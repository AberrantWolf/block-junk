use core::time::Duration;
use std::time::Instant;

use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::*;

use lightyear::input::native::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot, TerrainSlots};
use crate::collision::{Aabb, WorldCollision};
use crate::menu::{ServerSaveConfig, ServerSaveRequestFlag, ServerShutdownFlag};
use crate::physics::{
    EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step, soft_separate_actors,
};
use crate::plans::Plans;
use crate::protocol::{
    Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, BlockManifest,
    CHUNK_PADDED, Carrying, CellEdit, ChunkCoord, ChunkData, ChunkSnapshot, ChunkUnload,
    DepositRequest, DropRequest, DropToolRequest, EquippedTool, GameSet,
    MovementIntent, MovementMode,
    NpcAnimOverride, NpcDetails, PickupRequest, PlanEdit, PlanKind,
    RequestNpcDetails, WorldChannel, WorldClock, WorldClockSync, WorldItem,
};
use crate::items::{ItemRegistry, PLAYER_CARRY_CAPACITY};
use crate::npc::{Brain, Goal, Needs, Npc, NpcId, NpcKind, NpcPath, NpcWorkCompleted};
use crate::rooms::{DetectionDirty, RoomEventMsg, RoomMap, mark_dirty_from_edits, process_dirty};
use crate::craft_stations::{CraftOrder, CraftStations, StationState};
use crate::save::{SAVE_VERSION, SaveFile, SavedCarry, SavedChunk, SavedCraftOrder, SavedMaterialEntry, SavedNpc, SavedPlanState, SavedStationItem, SavedStationState, SavedTool, SavedWorldItem, read_save, write_save};
use crate::voxel::{
    Chunk, ChunkEntities, ChunkMap, EntryKind, chunk_local_to_world, chunk_world_transform,
    world_to_chunk,
};

/// Marker on chunks whose state has diverged from the deterministic terrain
/// function. Server uses it to decide whether to ship the bytes or just
/// tell the client "regenerate locally" on AoI entry.
#[derive(Component)]
pub struct ChunkEdited;

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        app.add_plugins(crate::interactables::InteractableIndexPlugin);
        app.add_plugins(crate::debug::DebugServerPlugin);
        app.add_plugins(crate::npc::NpcServerPlugin);
        app.add_plugins(crate::plans::PlansServerPlugin);
        app.add_plugins(crate::plan_claims::PlanClaimsPlugin);
        app.add_plugins(crate::haul::HaulPlugin);
        app.add_plugins(crate::craft_stations::CraftStationsServerPlugin);
        // ServerScriptingPlugin inserts BlockRegistry; resolve well-known
        // terrain slots from it once so chunk gen doesn't hash strings.
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(terrain_slots);
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientAvatars>();
        app.init_resource::<ClientChunks>();
        app.init_resource::<PendingChunks>();
        app.init_resource::<PendingSpawnPose>();
        app.init_resource::<PendingSpawnCarry>();
        app.init_resource::<PendingSpawnTool>();
        app.init_resource::<RoomMap>();
        app.init_resource::<DetectionDirty>();
        // World clock. Start at 0.25 (sunrise) so a fresh session begins
        // with the world lit and gives players a few minutes before the
        // first sleep-driven NPC behaviour kicks in. Save persistence is
        // future work; today every load lands here.
        app.insert_resource(WorldClock {
            day: 0,
            time_of_day: 0.25,
        });
        app.init_resource::<ClockSyncCooldown>();
        // Local Bevy bus for server-internal observers (scripting, building
        // detection, etc.). Not what crosses the wire — that's lightyear's
        // MessageSender/Receiver. Server-only.
        //
        // CellEdit is the per-cell shape; an incoming wire `BlockEdit`
        // (anchor + slot + orientation) gets expanded into one CellEdit
        // per footprint cell so existing per-cell consumers don't need to
        // know about block-entity footprints.
        app.add_message::<CellEdit>();
        app.add_message::<RoomEventMsg>();
        // Two chained groups in Simulation. Splitting into two `add_systems`
        // calls works around a Bevy 0.18 trait-resolution wall on chained
        // tuples beyond ~5 systems. The room group reads chunks updated by
        // `receive_block_edits`, so its order is "after edits"; the AoI
        // group is independent.
        app.add_systems(
            Update,
            (receive_block_edits, mark_dirty_from_edits, process_dirty)
                .chain()
                .in_set(GameSet::Simulation),
        );
        // NPC work-completion adapter: translates the brain's local-bus
        // `NpcWorkCompleted` events into the same `apply_block_edit`
        // path that handles client `BlockEdit` messages — so the world
        // mutation, the broadcast, and the plan auto-clear all run
        // through one code path.
        //
        // `auto_clear_stale_plans` listens to the `CellEdit` bus that
        // `apply_block_edit` writes per cell change and clears any
        // matching plan tag, then broadcasts a `PlanEdit{None}` so
        // client mirrors drop the now-stale outline.
        app.add_systems(
            Update,
            (
                apply_npc_work,
                auto_clear_stale_plans,
                spawn_drops_on_destroy,
                push_actors_out_of_new_blocks,
            )
                .chain()
                .after(receive_block_edits)
                .in_set(GameSet::Simulation),
        );
        app.add_systems(
            Update,
            receive_npc_inspection_requests.in_set(GameSet::Simulation),
        );
        app.add_systems(
            Update,
            (
                receive_pickup_requests,
                receive_drop_requests,
                receive_drop_tool_requests,
                receive_deposit_requests,
            )
                .in_set(GameSet::Simulation),
        );
        app.add_systems(
            Update,
            (poll_chunk_gen, update_aoi)
                .chain()
                .in_set(GameSet::Simulation),
        );
        // Server-authoritative player simulation: read replicated inputs,
        // run the same controller the predicted client runs, write the
        // authoritative AvatarPose back. Lightyear's prediction layer
        // compares this against the client's predicted state and replays
        // unacked inputs on disagreement.
        app.add_systems(FixedUpdate, server_player_step);
        // Soft actor separation runs after both physics systems have
        // moved everyone for this tick. The pairwise push then nudges
        // any overlapping actors apart 50/50 — gentle pushing instead
        // of hard contact-stop. Also runs on the first tick after
        // load and incidentally separates pre-stacked NPCs.
        app.add_systems(
            FixedUpdate,
            soft_separate_actors
                .after(server_player_step)
                .after(crate::npc::npc_physics_step),
        );
        // Block-stuck NPCs from a save (or any load-time edge case
        // where an actor is inside a solid cell) get one pushout
        // attempt on the first Update tick after chunks have flushed
        // in from `load_from_save`.
        app.add_systems(
            Update,
            rescue_embedded_actors_after_load.in_set(GameSet::Simulation),
        );
        app.add_systems(FixedUpdate, tick_world_clock);
        app.add_systems(Update, broadcast_world_clock);
        // Save/load wiring. `load_from_save` runs before any other Startup
        // system that touches ChunkMap so loaded chunks beat AoI's
        // procedural fallback. `save_then_shutdown` polls the shutdown flag
        // every tick — when it fires, writes the world and exits the App.
        app.add_systems(Startup, load_from_save);
        app.add_systems(Update, (save_then_shutdown, save_on_request));
        app.add_observer(install_replication_sender);
        app.add_observer(register_new_client);
        app.add_observer(forget_disconnected_client);
    }
}

/// Held on the server App after a successful load. Consumed once by the
/// first `register_new_client` invocation, so the client that triggered
/// the load lands back where they left off. Subsequent connections (in a
/// multi-host scenario) get the default spawn. Per-player persistence
/// requires a stable client identity we don't have yet — tracked as
/// follow-up.
#[derive(Resource, Default)]
pub struct PendingSpawnPose(pub Option<AvatarPose>);

/// Companion to [`PendingSpawnPose`] for the player's carry stack.
/// Consumed at the same point so the reloading player lands with the
/// same item in hand they had at save.
#[derive(Resource, Default)]
pub struct PendingSpawnCarry(pub Option<Carrying>);

/// Same shape as [`PendingSpawnCarry`] for the player's tool slot.
/// Hand-off lane between save-load (or the starter-loadout fallback)
/// and the avatar spawn observer. `None` ⇒ "use the starter axe if
/// the item is registered, otherwise spawn empty-handed."
#[derive(Resource, Default)]
pub struct PendingSpawnTool(pub Option<EquippedTool>);

/// Item id the engine equips on a freshly-spawned player when no save
/// override is present. One hardcode — easy to lift to mod data when
/// a starter-loadout system needs more than one item. Mod-side
/// equivalent isn't worth the surface until there's a second item.
const STARTER_TOOL_ID: &str = "vanilla:axe";

/// Server App Startup: if `ServerSaveConfig::load_existing`, read the save
/// file and pre-populate `ChunkMap` with the persisted edited chunks. They
/// land with the `ChunkEdited` marker so subsequent AoI sends ship the
/// bytes rather than the procedural shortcut. Procedural chunks aren't
/// persisted (`Chunk::from_terrain` regenerates them on demand).
///
/// A load failure does NOT abort startup — we log and continue with an
/// empty world. Better than an unbootable session if a save is corrupt.
#[allow(clippy::too_many_arguments, reason = "load_from_save touches every persisted system")]
fn load_from_save(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut pending_pose: ResMut<PendingSpawnPose>,
    mut pending_carry: ResMut<PendingSpawnCarry>,
    mut pending_tool: ResMut<PendingSpawnTool>,
    mut dirty: ResMut<DetectionDirty>,
    mut clock: ResMut<WorldClock>,
    mut plans: ResMut<Plans>,
    mut stations: ResMut<CraftStations>,
    config: Option<Res<ServerSaveConfig>>,
    block_registry: Res<BlockRegistry>,
    item_registry: Res<ItemRegistry>,
    kind_registry: Res<crate::npc_registry::NpcKindRegistry>,
) {
    let Some(config) = config else {
        return;
    };
    if !config.load_existing {
        return;
    }
    let Some(name) = config.save_name.as_deref() else {
        return;
    };
    let save = match read_save(name) {
        Ok(s) => s,
        Err(e) => {
            error!("load save {name:?} failed: {e}; continuing with empty world");
            return;
        }
    };
    info!(
        "loading {} edited chunks + {} NPCs + {} plans + {} world items from save {name:?}",
        save.edited_chunks.len(),
        save.npcs.len(),
        save.plans.len(),
        save.world_items.len(),
    );
    // Restore the plan map before chunk spawn so `auto_clear_stale_plans`
    // running on the load's CellEdits doesn't see a partial state.
    // Convert from on-disk SavedPlanState (item ids as strings) to
    // engine PlanState (item slots) via the live item registry;
    // entries naming an item the current registry doesn't know about
    // are skipped with a warning rather than blocking the load.
    if !save.plans.is_empty() {
        let restored: Vec<(IVec3, crate::protocol::PlanState)> = save
            .plans
            .into_iter()
            .map(|(cell, saved)| {
                let materials = saved
                    .materials
                    .into_iter()
                    .filter_map(|m| {
                        let id = block_junk_mod_api::items::ItemId::new(m.item_id.clone());
                        match item_registry.slot_of(&id) {
                            Some(slot) => Some(crate::protocol::MaterialEntry {
                                item: slot,
                                needed: m.needed,
                                present: m.present,
                            }),
                            None => {
                                warn!(
                                    item = %m.item_id,
                                    "saved plan materials reference unknown item id; dropping entry",
                                );
                                None
                            }
                        }
                    })
                    .collect();
                (
                    cell,
                    crate::protocol::PlanState::new(saved.kind, materials),
                )
            })
            .collect();
        plans.replace_all(restored);
    }
    // Restore craft-station state. Each station's inventory items
    // resolve through the item registry; missing ids (mod removed)
    // log + drop just that inventory entry rather than blocking the
    // whole load. Orders with unknown recipe ids are kept (the
    // craft modal renders "(unknown recipe)" + Cancel works) since
    // the player may want to clear them by hand.
    if !save.craft_stations.is_empty() {
        let restored: Vec<(IVec3, StationState)> = save
            .craft_stations
            .into_iter()
            .map(|(cell, saved)| {
                let orders = saved
                    .orders
                    .into_iter()
                    .map(|o| CraftOrder {
                        recipe_id: o.recipe_id,
                        total: o.total,
                        completed: o.completed,
                    })
                    .collect();
                let mut inventory = std::collections::HashMap::new();
                for entry in saved.inventory {
                    let id = block_junk_mod_api::items::ItemId::new(entry.item_id.clone());
                    match item_registry.slot_of(&id) {
                        Some(slot) => {
                            *inventory.entry(slot).or_insert(0) += entry.count;
                        }
                        None => warn!(
                            cell = ?cell.to_array(),
                            item = %entry.item_id,
                            "saved station inventory references unknown item id; dropping entry",
                        ),
                    }
                }
                (cell, StationState { orders, inventory })
            })
            .collect();
        stations.replace_all(restored);
    }
    pending_pose.0 = save.last_player_pose;
    // Restore the world clock if the save carries one. Saves predating
    // v4 don't (Option::None); fall back to the resource's default
    // sunrise position rather than zeroing it to midnight.
    if let Some(saved_clock) = save.world_clock {
        *clock = saved_clock;
        info!(
            day = clock.day,
            time_of_day = clock.time_of_day,
            "restored world clock from save",
        );
    }
    // Mark every room-bounding cell in loaded chunks dirty for room
    // detection. RoomMap is runtime-only state (not persisted — RoomIds
    // aren't stable across restarts per the design memo), so without
    // priming the dirty queue here, registered rooms from before the
    // save would only re-detect after the player edited a block. Use
    // the moment-of-load timestamp so the existing DEBOUNCE window
    // applies and the first `process_dirty` tick after Startup runs
    // the detection.
    let now = Instant::now();
    let mut dirty_marked = 0usize;
    for SavedChunk {
        coord,
        chunk,
        entities,
    } in save.edited_chunks
    {
        // Interior cells run [1, CHUNK_PADDED - 1) in chunk-local space.
        // `chunk_local_to_world` converts to the unpadded world cell.
        for x in 1..(CHUNK_PADDED as i32 - 1) {
            for y in 1..(CHUNK_PADDED as i32 - 1) {
                for z in 1..(CHUNK_PADDED as i32 - 1) {
                    let local = IVec3::new(x, y, z);
                    let slot = chunk.get(local);
                    if slot.is_empty() {
                        continue;
                    }
                    if !block_registry.def(slot).flags.room_boundary {
                        continue;
                    }
                    dirty.push(chunk_local_to_world(coord, local), now);
                    dirty_marked += 1;
                }
            }
        }
        let entity = commands
            .spawn((
                chunk,
                coord,
                entities,
                ChunkEdited,
                Name::new(format!("chunk{:?}", coord.0.to_array())),
                chunk_world_transform(coord),
            ))
            .id();
        chunk_map.0.insert(coord, entity);
    }
    if dirty_marked > 0 {
        info!("primed {dirty_marked} room-bounding cells for re-detection after load");
    }
    for npc in save.npcs {
        spawn_loaded_npc(&mut commands, npc, &kind_registry, &item_registry);
    }
    // Loose items in the world. Resolve item ids → slots through the
    // current registry. An item id missing from the registry (mod
    // removed / renamed between sessions) gets logged and skipped —
    // the rest of the world still loads.
    let mut loaded_items = 0usize;
    for saved in save.world_items {
        let id = block_junk_mod_api::items::ItemId::new(saved.item_id.clone());
        let Some(slot) = item_registry.slot_of(&id) else {
            warn!(
                item = %saved.item_id,
                "saved world item references unknown item id; skipping",
            );
            continue;
        };
        let translation = saved.translation;
        commands.spawn((
            WorldItem {
                item: slot,
                translation,
            },
            Transform::from_translation(translation),
            GlobalTransform::default(),
            Replicate::to_clients(NetworkTarget::All),
            Name::new(format!("WorldItem(loaded:{})", id)),
        ));
        loaded_items += 1;
    }
    if loaded_items > 0 {
        info!("spawned {loaded_items} loose world items from save");
    }
    // Carry: resolve item id → slot, hand off via PendingSpawnCarry so
    // register_new_client can apply it to the spawning avatar.
    if let Some(saved_carry) = save.last_player_carry {
        let id = block_junk_mod_api::items::ItemId::new(saved_carry.item_id.clone());
        match item_registry.slot_of(&id) {
            Some(slot) => {
                pending_carry.0 = Some(Carrying {
                    item: Some(slot),
                    count: saved_carry.count,
                });
                info!(
                    item = %saved_carry.item_id,
                    count = saved_carry.count,
                    "restored player carry from save",
                );
            }
            None => warn!(
                item = %saved_carry.item_id,
                "saved player carry references unknown item id; spawning empty-handed",
            ),
        }
    }
    // Tool: same lookup-or-warn pattern as carry. Distinct from the
    // carry restore in one way — we ALWAYS set Pending to Some, even
    // for a save that recorded an empty tool slot. The
    // `register_new_client` starter-axe fallback only fires when
    // Pending was left untouched (no save loaded); reaching this
    // branch means a save loaded, so its intent (whether empty or a
    // specific tool) is what should land.
    let pending = match save.last_player_tool {
        Some(saved_tool) => {
            let id = block_junk_mod_api::items::ItemId::new(saved_tool.item_id.clone());
            match item_registry.slot_of(&id) {
                Some(slot) => {
                    info!(
                        item = %saved_tool.item_id,
                        "restored player tool from save",
                    );
                    EquippedTool { item: Some(slot) }
                }
                None => {
                    warn!(
                        item = %saved_tool.item_id,
                        "saved player tool references unknown item id; spawning empty-handed",
                    );
                    EquippedTool::default()
                }
            }
        }
        None => EquippedTool::default(),
    };
    pending_tool.0 = Some(pending);
}

/// Spawn an NPC entity restored from a save. Mirrors the cluster-spawn
/// observer in `npc.rs` except for the inputs: pose / mode / needs /
/// rng come from the save, transient state (velocity, on-ground, goal,
/// path overlay) defaults so the brain resumes from Idle and the
/// planner picks a fresh action on the first post-load tick.
///
/// Backfills any needs the kind registry declares but the save doesn't
/// carry — saves from before a mod added a new need would otherwise
/// leave that NPC permanently missing the entry, and the planner would
/// read it as `nil` forever. The save's value wins on collision (the
/// saved decay is the authoritative state for needs that already
/// existed), only-in-registry needs get the registry's default.
fn spawn_loaded_npc(
    commands: &mut Commands,
    npc: SavedNpc,
    kind_registry: &crate::npc_registry::NpcKindRegistry,
    item_registry: &ItemRegistry,
) {
    let mut needs = npc.needs;
    if let Some(def) = kind_registry.get(&npc.kind) {
        for (need_id, default_value) in &def.default_needs {
            needs.entry(need_id.clone()).or_insert(*default_value);
        }
    }
    // Reconstruct the carry stack. Missing item ids (mod uninstalled
    // between sessions) drop the carry silently — same degradation
    // pattern `load_from_save` uses for world items.
    let carry = npc
        .carrying
        .as_ref()
        .and_then(|sc| {
            let id = block_junk_mod_api::items::ItemId::new(sc.item_id.clone());
            match item_registry.slot_of(&id) {
                Some(slot) => Some(Carrying {
                    item: Some(slot),
                    count: sc.count,
                }),
                None => {
                    warn!(
                        npc = npc.id,
                        item = %sc.item_id,
                        "saved NPC carry references unknown item id; spawning empty-handed",
                    );
                    None
                }
            }
        })
        .unwrap_or_default();
    // Tool slot: same lookup-or-drop-with-warning pattern. NPCs don't
    // get a starter-loadout fallback (only players do via
    // STARTER_TOOL_ID); a missing saved id just lands them with no
    // tool.
    let tool = npc
        .tool
        .as_ref()
        .and_then(|st| {
            let id = block_junk_mod_api::items::ItemId::new(st.item_id.clone());
            match item_registry.slot_of(&id) {
                Some(slot) => Some(EquippedTool { item: Some(slot) }),
                None => {
                    warn!(
                        npc = npc.id,
                        item = %st.item_id,
                        "saved NPC tool references unknown item id; spawning toolless",
                    );
                    None
                }
            }
        })
        .unwrap_or_default();
    // Nested tuple: same 15-element Bundle workaround as the spawn-
    // cluster path. Identity/brain group + per-frame state + lightyear.
    commands.spawn((
        (
            Actor,
            Npc,
            NpcId(npc.id),
            NpcKind(npc.kind),
            Needs(needs),
            Brain {
                goal: Goal::Idle,
                rng: npc.rng,
            },
            carry,
            tool,
        ),
        npc.pose,
        AvatarVelocity::default(),
        AvatarOnGround::default(),
        npc.movement_mode,
        MovementIntent::default(),
        NpcPath::default(),
        NpcAnimOverride::default(),
        Replicate::to_clients(NetworkTarget::All),
        InterpolationTarget::to_clients(NetworkTarget::All),
        Name::new(format!("npc:{}", npc.id)),
    ));
}

/// Polled each tick. When the client flips the save-request atomic, write
/// the world to disk and clear the flag. Unlike `save_then_shutdown` this
/// is multi-shot (the user might "Save Now" several times per session) so
/// no Local guard.
#[allow(clippy::too_many_arguments, reason = "save_on_request touches every persisted system")]
fn save_on_request(
    flag: Option<Res<ServerSaveRequestFlag>>,
    config: Option<Res<ServerSaveConfig>>,
    clock: Res<WorldClock>,
    plans: Res<Plans>,
    stations: Res<CraftStations>,
    chunks: Query<(&ChunkCoord, &Chunk, &ChunkEntities), With<ChunkEdited>>,
    avatars: Query<(&AvatarPose, &Carrying, &EquippedTool), With<Avatar>>,
    npcs: Query<
        (
            &NpcId,
            &NpcKind,
            &AvatarPose,
            &MovementMode,
            &Needs,
            &Brain,
            &Carrying,
            &EquippedTool,
        ),
        With<Npc>,
    >,
    world_items: Query<&WorldItem>,
    item_registry: Res<ItemRegistry>,
) {
    let Some(flag) = flag else {
        return;
    };
    if !flag.0.swap(false, core::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(config) = config else {
        return;
    };
    let Some(name) = &config.save_name else {
        return;
    };
    let edited: Vec<SavedChunk> = chunks
        .iter()
        .map(|(coord, ch, ce)| SavedChunk {
            coord: *coord,
            chunk: ch.clone(),
            entities: ce.clone(),
        })
        .collect();
    let saved_npcs = collect_saved_npcs(&npcs, &item_registry);
    let chunk_count = edited.len();
    let npc_count = saved_npcs.len();
    let saved_plans = convert_saved_plans(&plans, &item_registry);
    let plan_count = saved_plans.len();
    let saved_items = collect_saved_world_items(&world_items, &item_registry);
    let item_count = saved_items.len();
    let saved_stations = convert_saved_stations(&stations, &item_registry);
    let station_count = saved_stations.len();
    let (saved_pose, saved_carry, saved_player_tool) =
        first_avatar_state(&avatars, &item_registry);
    let save = SaveFile {
        version: SAVE_VERSION,
        edited_chunks: edited,
        last_player_pose: saved_pose,
        npcs: saved_npcs,
        world_clock: Some(*clock),
        plans: saved_plans,
        world_items: saved_items,
        last_player_carry: saved_carry,
        last_player_tool: saved_player_tool,
        craft_stations: saved_stations,
    };
    match write_save(name, &save) {
        Ok(()) => info!(
            "save-on-request: wrote {chunk_count} chunks + {npc_count} NPCs + {plan_count} plans + {item_count} items + {station_count} stations to {name:?}",
        ),
        Err(e) => error!("save-on-request to {name:?} failed: {e}"),
    }
}

/// Convert the engine-side `CraftStations` snapshot to the on-disk
/// shape. Item slots are resolved back to their stable ids for the
/// same reason `convert_saved_plans` does — slot ordering can shift
/// between sessions if the mod set changes.
fn convert_saved_stations(
    stations: &CraftStations,
    item_registry: &ItemRegistry,
) -> Vec<(IVec3, SavedStationState)> {
    stations
        .iter()
        .map(|(cell, state)| {
            let orders = state
                .orders
                .iter()
                .map(|o| SavedCraftOrder {
                    recipe_id: o.recipe_id.clone(),
                    total: o.total,
                    completed: o.completed,
                })
                .collect();
            let inventory = state
                .inventory
                .iter()
                .map(|(slot, count)| SavedStationItem {
                    item_id: item_registry.id_of(*slot).to_string(),
                    count: *count,
                })
                .collect();
            (
                *cell,
                SavedStationState { orders, inventory },
            )
        })
        .collect()
}

/// Convert the engine-side `Plans` snapshot to the on-disk shape.
/// Item slots are resolved back to their stable [`ItemId`] strings so
/// the save survives a registry rebuild that changes slot ordering.
fn convert_saved_plans(
    plans: &Plans,
    item_registry: &ItemRegistry,
) -> Vec<(IVec3, SavedPlanState)> {
    plans
        .snapshot()
        .into_iter()
        .map(|(cell, state)| {
            let materials = state
                .materials
                .into_iter()
                .map(|m| SavedMaterialEntry {
                    item_id: item_registry.id_of(m.item).to_string(),
                    needed: m.needed,
                    present: m.present,
                })
                .collect();
            (
                cell,
                SavedPlanState {
                    kind: state.kind,
                    materials,
                },
            )
        })
        .collect()
}

/// Snapshot every loose `WorldItem`. Converts the engine slot back to
/// the stable [`ItemId`] string so the save survives a registry
/// rebuild that changes slot ordering.
fn collect_saved_world_items(
    items: &Query<&WorldItem>,
    item_registry: &ItemRegistry,
) -> Vec<SavedWorldItem> {
    items
        .iter()
        .map(|wi| SavedWorldItem {
            item_id: item_registry.id_of(wi.item).to_string(),
            translation: wi.translation,
        })
        .collect()
}

/// Pull the first connected avatar's pose + (non-empty) carry + tool,
/// mirroring the "first reconnect wins" convention `last_player_pose`
/// already uses. Each of carry/tool serialises as `None` when its slot
/// is empty.
fn first_avatar_state(
    avatars: &Query<(&AvatarPose, &Carrying, &EquippedTool), With<Avatar>>,
    item_registry: &ItemRegistry,
) -> (Option<AvatarPose>, Option<SavedCarry>, Option<SavedTool>) {
    let Some((pose, carry, tool)) = avatars.iter().next() else {
        return (None, None, None);
    };
    let saved_carry = match (carry.item, carry.count) {
        (Some(slot), count) if count > 0 => Some(SavedCarry {
            item_id: item_registry.id_of(slot).to_string(),
            count,
        }),
        _ => None,
    };
    let saved_tool = tool.item.map(|slot| SavedTool {
        item_id: item_registry.id_of(slot).to_string(),
    });
    (Some(*pose), saved_carry, saved_tool)
}

/// Snapshot every NPC's persistent state. `BrainDisabled` NPCs are
/// included — the marker is treated as a runtime recovery state, not
/// persisted, so reloading gives the planner a fresh chance. A
/// consistently broken planner will re-disable each NPC on its first
/// tick after load (and log loudly each time).
fn collect_saved_npcs(
    npcs: &Query<
        (
            &NpcId,
            &NpcKind,
            &AvatarPose,
            &MovementMode,
            &Needs,
            &Brain,
            &Carrying,
            &EquippedTool,
        ),
        With<Npc>,
    >,
    item_registry: &ItemRegistry,
) -> Vec<SavedNpc> {
    npcs.iter()
        .map(|(id, kind, pose, mode, needs, brain, carry, tool)| {
            let carrying = match (carry.item, carry.count) {
                (Some(slot), count) if count > 0 => Some(SavedCarry {
                    item_id: item_registry.id_of(slot).to_string(),
                    count,
                }),
                _ => None,
            };
            let saved_tool = tool.item.map(|slot| SavedTool {
                item_id: item_registry.id_of(slot).to_string(),
            });
            SavedNpc {
                id: id.0,
                kind: kind.0.clone(),
                pose: *pose,
                movement_mode: *mode,
                needs: needs.0.clone(),
                rng: brain.rng,
                carrying,
                tool: saved_tool,
            }
        })
        .collect()
}

/// Drives the server App's shutdown lifecycle. Each tick:
///   1. If the shutdown flag isn't set, do nothing.
///   2. Once it's set: collect every chunk with `ChunkEdited`, serialize
///      to the configured save path (unless save is disabled), then emit
///      `AppExit`.
///
/// The `Local<bool>` guards against running the save loop more than once
/// per session; the runner won't actually exit until the next tick reads
/// the AppExit message.
#[allow(clippy::too_many_arguments, reason = "save_then_shutdown touches every persisted system")]
fn save_then_shutdown(
    flag: Option<Res<ServerShutdownFlag>>,
    config: Option<Res<ServerSaveConfig>>,
    clock: Res<WorldClock>,
    plans: Res<Plans>,
    stations: Res<CraftStations>,
    chunks: Query<(&ChunkCoord, &Chunk, &ChunkEntities), With<ChunkEdited>>,
    avatars: Query<(&AvatarPose, &Carrying, &EquippedTool), With<Avatar>>,
    npcs: Query<
        (
            &NpcId,
            &NpcKind,
            &AvatarPose,
            &MovementMode,
            &Needs,
            &Brain,
            &Carrying,
            &EquippedTool,
        ),
        With<Npc>,
    >,
    world_items: Query<&WorldItem>,
    item_registry: Res<ItemRegistry>,
    mut exit: MessageWriter<AppExit>,
    mut handled: Local<bool>,
) {
    if *handled {
        return;
    }
    let Some(flag) = flag else {
        return;
    };
    if !flag.0.load(core::sync::atomic::Ordering::SeqCst) {
        return;
    }
    *handled = true;

    if let Some(config) = config {
        match (&config.save_name, config.no_save_on_exit) {
            (Some(name), false) => {
                let edited: Vec<SavedChunk> = chunks
                    .iter()
                    .map(|(coord, ch, ce)| SavedChunk {
                        coord: *coord,
                        chunk: ch.clone(),
                        entities: ce.clone(),
                    })
                    .collect();
                let saved_npcs = collect_saved_npcs(&npcs, &item_registry);
                let saved_plans = convert_saved_plans(&plans, &item_registry);
                let saved_items = collect_saved_world_items(&world_items, &item_registry);
                let saved_stations = convert_saved_stations(&stations, &item_registry);
                let (saved_pose, saved_carry, saved_player_tool) =
                    first_avatar_state(&avatars, &item_registry);
                let chunk_count = edited.len();
                let npc_count = saved_npcs.len();
                let plan_count = saved_plans.len();
                let item_count = saved_items.len();
                let station_count = saved_stations.len();
                let save = SaveFile {
                    version: SAVE_VERSION,
                    edited_chunks: edited,
                    last_player_pose: saved_pose,
                    npcs: saved_npcs,
                    world_clock: Some(*clock),
                    plans: saved_plans,
                    world_items: saved_items,
                    last_player_carry: saved_carry,
                    last_player_tool: saved_player_tool,
                    craft_stations: saved_stations,
                };
                match write_save(name, &save) {
                    Ok(()) => info!(
                        "saved {chunk_count} chunks + {npc_count} NPCs + {plan_count} plans + {item_count} items + {station_count} stations to {name:?}",
                    ),
                    Err(e) => error!("save to {name:?} failed: {e}"),
                }
            }
            (Some(name), true) => {
                info!("DebugNoSaveOnExit set; skipping save to {name:?}");
            }
            (None, _) => {}
        }
    }

    exit.write(AppExit::Success);
}

/// Connection entity → avatar entity. The avatar carries the authoritative
/// `Transform` (driven by incoming `PlayerPosition` messages) and is the
/// thing replicated to other clients. Both `track_client_positions` and
/// `update_aoi` look up positions through this map.
#[derive(Resource, Default)]
pub struct ClientAvatars(pub HashMap<Entity, Entity>);

/// Chunks currently believed to be loaded on each client. Used by `update_aoi`
/// to compute deltas (which snapshots/unloads to send each tick).
#[derive(Resource, Default)]
pub struct ClientChunks(pub HashMap<Entity, HashSet<ChunkCoord>>);

/// Chunks whose generation is currently in flight on a worker thread.
/// `update_aoi` skips coords already in here so we don't queue duplicate
/// generations; `poll_chunk_gen` drains them as they complete.
#[derive(Resource, Default)]
pub struct PendingChunks(pub HashMap<ChunkCoord, Task<Chunk>>);

const AOI_RADIUS_XZ: i32 = 2;
const AOI_RADIUS_Y: i32 = 1;

/// How often the server pushes replication updates to each client. 20 Hz is
/// twice the player-position ingest rate (10 Hz), so we never sit on a fresh
/// position for more than half a tick. At ~12 B/Vec3 this stays well inside
/// the 40 kbps/player budget even with a handful of co-located avatars.
const REPLICATION_INTERVAL: Duration = Duration::from_millis(50);

/// Each connection entity needs a `ReplicationSender` before any `Replicate`d
/// component on a server-side entity can be pushed to it. Insert as soon as
/// the link appears (before the netcode handshake completes) so the sender is
/// ready by the time we spawn an avatar in the `Connected` observer.
fn install_replication_sender(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(ReplicationSender::new(
        REPLICATION_INTERVAL,
        SendUpdatesMode::SinceLastAck,
        false,
    ));
}

/// On client connect: spawn an avatar entity carrying the authoritative
/// Transform for this player, replicated to every *other* connected client.
/// We exclude the owner so their own camera Transform isn't periodically
/// overwritten with a stale server copy of itself.
///
/// The avatar starts at the origin so AoI can begin streaming chunks before
/// the first `PlayerPosition` message lands; without that the new client
/// sees nothing for ~100 ms.
fn register_new_client(
    trigger: On<Add, Connected>,
    remote_ids: Query<&RemoteId>,
    mut commands: Commands,
    mut avatars: ResMut<ClientAvatars>,
    mut sent: ResMut<ClientChunks>,
    mut pending_pose: ResMut<PendingSpawnPose>,
    mut pending_carry: ResMut<PendingSpawnCarry>,
    mut pending_tool: ResMut<PendingSpawnTool>,
    registry: Res<BlockRegistry>,
    item_registry: Res<ItemRegistry>,
    mut manifests: Query<&mut MessageSender<BlockManifest>>,
) {
    let connection = trigger.entity;
    let Ok(remote) = remote_ids.get(connection) else {
        warn!("Connected fired with no RemoteId on entity {connection:?}");
        return;
    };
    // Replicated to ALL clients (owner included), with the targets
    // splitting prediction (owner rolls back on disagreement) from
    // interpolation (everyone else lerps between server samples). The
    // owner's client gets `Predicted` on its copy; remote clients get
    // `Interpolated`. ControlledBy ties the entity back to its
    // connection so input replication knows where to deliver the inputs.
    // Spawn position: if the save provided a persisted pose, consume it
    // (one-shot — see `PendingSpawnPose`). Otherwise spawn above the
    // sine-wave terrain (peaks ~y=16) so the first physics tick lands
    // the player on the surface rather than inside it. Eye height =
    // AvatarPose.translation by convention.
    let spawn_pose = pending_pose.0.take().unwrap_or(AvatarPose {
        translation: Vec3::new(0.0, 32.0, 60.0),
        yaw: 0.0,
    });
    // Restore the saved carry stack if one was provided (single-player
    // "first reconnect wins" convention, same as `PendingSpawnPose`).
    // Otherwise start empty-handed.
    let spawn_carry = pending_carry.0.take().unwrap_or_default();
    // Tool slot: save override wins; if unset, hand the player a
    // starter axe by looking up STARTER_TOOL_ID in the item registry.
    // Missing tool id (mod removed) → empty slot + a warning, same
    // degradation as the carry path.
    let spawn_tool = pending_tool.0.take().unwrap_or_else(|| {
        let id = block_junk_mod_api::items::ItemId::new(STARTER_TOOL_ID);
        match item_registry.slot_of(&id) {
            Some(slot) => EquippedTool { item: Some(slot) },
            None => {
                warn!(
                    starter = STARTER_TOOL_ID,
                    "starter tool id missing from item registry; spawning tool slot empty",
                );
                EquippedTool::default()
            }
        }
    });
    let avatar = commands
        .spawn((
            Actor,
            Avatar,
            spawn_pose,
            AvatarVelocity::default(),
            AvatarOnGround::default(),
            MovementMode::default(),
            spawn_carry,
            spawn_tool,
            ActionState::<MovementIntent>::default(),
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::Single(remote.0)),
            InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(remote.0)),
            ControlledBy {
                owner: connection,
                lifetime: Default::default(),
            },
            Name::new(format!("avatar:{}", remote.0)),
        ))
        .id();
    avatars.0.insert(connection, avatar);
    sent.0.entry(connection).or_default();

    // Send the slot table once so the client can sanity-check it against
    // its own. Mismatches indicate a divergent mod set; logged client-side.
    if let Ok(mut sender) = manifests.get_mut(connection) {
        let manifest = BlockManifest {
            slots: registry.iter().map(|(_, def)| def.id.clone()).collect(),
        };
        sender.send::<WorldChannel>(manifest);
    }
}

/// Server-authoritative simulation tick. Reads each avatar's current
/// `ActionState<MovementIntent>` (filled by lightyear's input replication)
/// and runs the same controller the predicted client runs. The resulting
/// AvatarPose is what gets replicated back; the predicted client compares
/// against it and rolls back on disagreement.
/// Real seconds between `WorldClockSync` broadcasts. The client
/// extrapolates locally between syncs (via `WorldClock::advance` in its
/// own `Update`), so 1 Hz is plenty to keep drift bounded — at
/// `DAY_LENGTH_SECS = 600` a one-second drift is one part in 600 of the
/// day cycle, well below visible.
const CLOCK_SYNC_INTERVAL_SECS: f32 = 1.0;

/// Countdown until the next clock sync. Wraps `f32` rather than a Bevy
/// `Timer` because the only use is "decrement, fire-when-zero, reset" —
/// the timer API's repeating/just-finished bookkeeping is overkill.
#[derive(Resource, Default)]
pub struct ClockSyncCooldown(pub f32);

/// Advance the world clock one fixed tick. Single-source-of-truth for
/// time-of-day; the snapshot builder and the replication broadcaster
/// both read this resource.
fn tick_world_clock(time: Res<Time>, mut clock: ResMut<WorldClock>) {
    clock.advance(time.delta_secs());
}

/// Periodic clock broadcast. Sends `WorldClockSync` to every connected
/// client once every `CLOCK_SYNC_INTERVAL_SECS` real seconds. Also fires
/// the first sync on the cooldown's initial tick after spawn, so a
/// freshly-connected client snaps within ~1 s of join rather than
/// waiting for the cooldown to first roll over.
fn broadcast_world_clock(
    time: Res<Time>,
    clock: Res<WorldClock>,
    mut cooldown: ResMut<ClockSyncCooldown>,
    mut senders: Query<&mut MessageSender<WorldClockSync>>,
) {
    cooldown.0 -= time.delta_secs();
    if cooldown.0 > 0.0 {
        return;
    }
    cooldown.0 = CLOCK_SYNC_INTERVAL_SECS;
    let msg = WorldClockSync {
        day: clock.day,
        time_of_day: clock.time_of_day,
    };
    for mut sender in senders.iter_mut() {
        sender.send::<WorldChannel>(msg);
    }
}

fn server_player_step(
    time: Res<Time>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut avatars: Query<(
        &mut AvatarPose,
        &mut AvatarVelocity,
        &mut AvatarOnGround,
        &mut MovementMode,
        &ActionState<MovementIntent>,
    )>,
) {
    let dt = time.delta_secs();
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (mut pose, mut vel, mut on_ground, mut mode, input) in avatars.iter_mut() {
        // Belt-and-braces against the controller starting embedded
        // in a freshly-solid cell. Mirrors the client-side guard in
        // `client_player_step`; same helper. The `Update`-scheduled
        // `push_actors_out_of_new_blocks` handles the common case
        // synchronously with the edit, but a save-load + edit during
        // an in-flight tick can land the controller here with the
        // body already inside the new geometry.
        let rescue = crate::physics::rescue_embedded_actor(&mut pose.translation, &world);
        if rescue != Vec3::ZERO {
            vel.0.x = 0.0;
            vel.0.z = 0.0;
        }
        apply_walk_step(&mut pose, &mut vel, &mut on_ground, &mut mode, &input.0, dt, &world);
    }
}

fn forget_disconnected_client(
    trigger: On<Remove, ClientOf>,
    mut commands: Commands,
    mut avatars: ResMut<ClientAvatars>,
    mut sent: ResMut<ClientChunks>,
) {
    if let Some(avatar) = avatars.0.remove(&trigger.entity) {
        commands.entity(avatar).despawn();
    }
    sent.0.remove(&trigger.entity);
}

/// Drains completed chunk-generation tasks off the AsyncComputeTaskPool,
/// installing the resulting chunks into the world. Runs before `update_aoi`
/// so newly-completed chunks are available to send this same tick.
fn poll_chunk_gen(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut pending: ResMut<PendingChunks>,
) {
    let mut completed: Vec<(ChunkCoord, Chunk)> = Vec::new();
    pending.0.retain(|coord, task| {
        if let Some(chunk) = block_on(poll_once(&mut *task)) {
            completed.push((*coord, chunk));
            false
        } else {
            true
        }
    });
    for (coord, chunk) in completed {
        let entity = commands
            .spawn((
                chunk,
                coord,
                ChunkEntities::default(),
                Name::new(format!("chunk{:?}", coord.0.to_array())),
                chunk_world_transform(coord),
            ))
            .id();
        chunk_map.0.insert(coord, entity);
    }
}

/// Streaming. For each client, computes the chunk set in their AoI and
/// diffs against what they have. Chunks newly in AoI:
///   - if generated already, snapshot is sent immediately
///   - else if a generation task is in flight, skipped (will land later)
///   - else a fresh task is queued on `AsyncComputeTaskPool`
/// Chunks no longer in AoI: a `ChunkUnload` is sent.
///
/// Master chunk records in `ChunkMap` are NOT evicted when no client needs
/// them — that's deferred to a later stage with the "edited?" tracking, so
/// we don't lose player edits when the last viewer wanders off.
fn update_aoi(
    chunk_map: Res<ChunkMap>,
    chunks: Query<(&Chunk, &ChunkEntities, Has<ChunkEdited>)>,
    mut pending: ResMut<PendingChunks>,
    avatars: Res<ClientAvatars>,
    poses: Query<&AvatarPose>,
    mut sent: ResMut<ClientChunks>,
    mut snapshots: Query<&mut MessageSender<ChunkSnapshot>>,
    mut unloads: Query<&mut MessageSender<ChunkUnload>>,
    terrain_slots: Res<TerrainSlots>,
) {
    for (&client_entity, &avatar_entity) in avatars.0.iter() {
        let Ok(avatar_pose) = poses.get(avatar_entity) else {
            continue;
        };
        let player_chunk = world_to_chunk_coord(avatar_pose.translation);
        let desired = aoi_around(player_chunk);
        let current = sent.0.entry(client_entity).or_default();

        let candidates: Vec<ChunkCoord> = desired.difference(current).copied().collect();
        let removed: Vec<ChunkCoord> = current.difference(&desired).copied().collect();

        for coord in &candidates {
            // Resolve the chunk's wire payload. Three states:
            //   - server has the chunk, edited: send the bytes + sidecar
            //   - server has the chunk, never edited: send Procedural (tiny)
            //   - server doesn't have the chunk yet: queue async gen and skip
            let payload: Option<(ChunkData, Vec<crate::voxel::EntityEntry>)> =
                if let Some(&entity) = chunk_map.0.get(coord) {
                    chunks.get(entity).ok().map(|(chunk, entities, edited)| {
                        let data = if edited {
                            ChunkData::Edited(chunk.blocks.clone())
                        } else {
                            ChunkData::Procedural
                        };
                        // Procedural chunks have no entities by construction, but
                        // ship the sidecar regardless — empty in that case, so
                        // the wire cost is one varint.
                        (data, entities.entries.clone())
                    })
                } else {
                    if !pending.0.contains_key(coord) {
                        let coord_for_task = *coord;
                        let slots = *terrain_slots;
                        let task = AsyncComputeTaskPool::get()
                            .spawn(async move { Chunk::from_terrain(coord_for_task, &slots) });
                        pending.0.insert(*coord, task);
                    }
                    None
                };

            let Some((data, entities)) = payload else {
                continue; // still generating; try again next tick
            };
            if let Ok(mut sender) = snapshots.get_mut(client_entity) {
                sender.send::<WorldChannel>(ChunkSnapshot {
                    coord: *coord,
                    data,
                    entities,
                });
                current.insert(*coord);
            }
        }

        for coord in &removed {
            if let Ok(mut sender) = unloads.get_mut(client_entity) {
                sender.send::<WorldChannel>(ChunkUnload { coord: *coord });
            }
            current.remove(coord);
        }
    }
}

fn world_to_chunk_coord(pos: Vec3) -> ChunkCoord {
    let size = crate::protocol::CHUNK_SIZE as f32;
    ChunkCoord(IVec3::new(
        (pos.x / size).floor() as i32,
        (pos.y / size).floor() as i32,
        (pos.z / size).floor() as i32,
    ))
}

fn aoi_around(centre: ChunkCoord) -> HashSet<ChunkCoord> {
    let mut set = HashSet::with_capacity(
        ((2 * AOI_RADIUS_XZ + 1).pow(2) * (2 * AOI_RADIUS_Y + 1)) as usize,
    );
    for cy in -AOI_RADIUS_Y..=AOI_RADIUS_Y {
        for cz in -AOI_RADIUS_XZ..=AOI_RADIUS_XZ {
            for cx in -AOI_RADIUS_XZ..=AOI_RADIUS_XZ {
                set.insert(ChunkCoord(centre.0 + IVec3::new(cx, cy, cz)));
            }
        }
    }
    set
}

fn receive_block_edits(
    mut commands: Commands,
    mut receivers: Query<&mut MessageReceiver<BlockEdit>>,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut bus: MessageWriter<CellEdit>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for mut receiver in receivers.iter_mut() {
        let edits: Vec<BlockEdit> = receiver.receive().collect();
        for edit in edits {
            apply_block_edit(
                edit,
                &mut commands,
                &mut chunks,
                &map,
                &registry,
                server,
                &mut broadcast,
                &mut bus,
            );
        }
    }
}

/// Validate + apply a single client request, then broadcast the canonical
/// applied event. On a place: expand the footprint, check every cell is
/// empty, write all cells + sidecar entries. On a break: resolve the
/// clicked cell to its entity's anchor (single-cell breaks resolve
/// trivially), clear all footprint cells + sidecar entries.
///
/// `pub(crate)` so debug helpers can synthesize a place/break without
/// going back through the wire — the server can't deliver a BlockEdit
/// to its own `MessageReceiver`, but it can call this directly with
/// the same effect.
pub(crate) fn apply_block_edit(
    edit: BlockEdit,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    registry: &BlockRegistry,
    server: &Server,
    broadcast: &mut ServerMultiMessageSender,
    bus: &mut MessageWriter<CellEdit>,
) {
    if edit.slot.is_empty() {
        apply_break(edit, commands, chunks, map, registry, server, broadcast, bus);
    } else {
        apply_place(edit, commands, chunks, map, registry, server, broadcast, bus);
    }
}

/// Place path. Resolves the rotated footprint, validates every cell is
/// empty (and its chunk is loaded), writes the slot to each cell, adds an
/// `Anchor` entry at the anchor cell + `Ghost` entries at every other
/// footprint cell. Cross-chunk footprints are handled naturally — each
/// affected chunk gets the cells that fall inside it.
fn apply_place(
    edit: BlockEdit,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    registry: &BlockRegistry,
    server: &Server,
    broadcast: &mut ServerMultiMessageSender,
    bus: &mut MessageWriter<CellEdit>,
) {
    let def = registry.def(edit.slot);
    let cells = world_footprint(edit.anchor, &def.footprint, edit.orientation);
    if cells.is_empty() {
        return;
    }

    // Group cells by their owning chunk + verify each chunk is loaded.
    let mut cells_by_chunk: HashMap<ChunkCoord, Vec<(IVec3, IVec3)>> = HashMap::default();
    for cell in &cells {
        let (coord, local) = world_to_chunk(*cell);
        cells_by_chunk.entry(coord).or_default().push((*cell, local));
    }
    for coord in cells_by_chunk.keys() {
        if !map.0.contains_key(coord) {
            // A footprint cell falls in a chunk the server hasn't
            // generated yet. Reject the placement; the client retries
            // when AoI brings the chunk online. Loud log so this surfaces
            // if it happens often in practice (suggests the placement UX
            // is letting players aim past their AoI).
            warn!(
                anchor = ?edit.anchor,
                slot = %def.id,
                missing_chunk = ?coord,
                "rejecting cross-chunk place: chunk not loaded server-side",
            );
            return;
        }
    }

    // Validation pass: every footprint cell must currently be empty.
    // Split borrow trick — we can't both `.iter` and `.get_mut` the same
    // query, so do validation against the *immutable* view via a per-
    // chunk lookup that reborrows the query each time.
    for (coord, cells_in_chunk) in &cells_by_chunk {
        let chunk_entity = map.0[coord];
        let Ok((chunk, _)) = chunks.get(chunk_entity) else {
            return;
        };
        for &(_world, local) in cells_in_chunk {
            if !chunk.get(local).is_empty() {
                info!(
                    anchor = ?edit.anchor,
                    slot = %def.id,
                    blocked = ?_world,
                    "rejecting place: footprint cell already occupied",
                );
                return;
            }
        }
    }

    // Sidecar entries describe block-entity geometry — anchors track the
    // entity's orientation, ghosts point footprint cells back at their
    // anchor. Plain cube blocks need none of that: the slot grid alone
    // tells the full story. So only mesh blocks get sidecar entries.
    let needs_sidecar = def.mesh.is_some();

    // Apply pass. One chunk at a time so the borrow scope is clean.
    for (coord, cells_in_chunk) in &cells_by_chunk {
        let chunk_entity = map.0[coord];
        let Ok((mut chunk, mut entities)) = chunks.get_mut(chunk_entity) else {
            continue;
        };
        for &(world, local) in cells_in_chunk {
            // is_empty was checked above; set should always succeed.
            // Padding cells aren't part of `world_to_chunk`'s output for
            // interior coords, so set() returns true on the real edits.
            chunk.set(local, edit.slot);
            if needs_sidecar {
                let kind = if world == edit.anchor {
                    EntryKind::Anchor {
                        orientation: edit.orientation,
                    }
                } else {
                    EntryKind::Ghost {
                        anchor: edit.anchor,
                    }
                };
                entities.insert(world, kind);
            }
            bus.write(CellEdit {
                world,
                slot: edit.slot,
                // Place validation rejects any non-empty footprint cell,
                // so the prior occupant is always EMPTY.
                prev_slot: BlockSlot::EMPTY,
            });
        }
        commands.entity(chunk_entity).insert(ChunkEdited);
    }

    if let Err(err) = broadcast.send::<BlockEdit, WorldChannel>(&edit, server, &NetworkTarget::All)
    {
        warn!("BlockEdit broadcast failed: {err}");
    }
}

/// Break path. Resolves the clicked cell to its entity's anchor (single-
/// cell blocks resolve to themselves with default orientation; multi-
/// cell entities walk the chunk sidecar). Clears every footprint cell
/// in the affected chunks + drops the entries.
fn apply_break(
    edit: BlockEdit,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    registry: &BlockRegistry,
    server: &Server,
    broadcast: &mut ServerMultiMessageSender,
    bus: &mut MessageWriter<CellEdit>,
) {
    let click_cell = edit.anchor;
    let (click_coord, click_local) = world_to_chunk(click_cell);
    let Some(&click_entity) = map.0.get(&click_coord) else {
        return;
    };

    // Resolve clicked cell → anchor cell + slot + orientation.
    let (anchor, slot, orientation) = {
        let Ok((chunk, entities)) = chunks.get(click_entity) else {
            return;
        };
        let click_slot = chunk.get(click_local);
        if click_slot.is_empty() {
            return;
        }
        match entities.get(click_cell) {
            Some(EntryKind::Anchor { orientation }) => (click_cell, click_slot, orientation),
            Some(EntryKind::Ghost { anchor }) => {
                // Anchor lives in the same or another chunk. Look it up.
                let (anchor_coord, anchor_local) = world_to_chunk(anchor);
                let Some(&anchor_entity) = map.0.get(&anchor_coord) else {
                    warn!(
                        clicked = ?click_cell,
                        anchor = ?anchor,
                        "ghost cell points at unloaded anchor chunk; ignoring break",
                    );
                    return;
                };
                let Ok((anchor_chunk, anchor_entities)) = chunks.get(anchor_entity) else {
                    return;
                };
                let anchor_slot = anchor_chunk.get(anchor_local);
                let orientation = match anchor_entities.get(anchor) {
                    Some(EntryKind::Anchor { orientation }) => orientation,
                    _ => {
                        // Sidecar inconsistency — anchor entry missing or
                        // a ghost. Loud log; bail without mutating.
                        error!(
                            clicked = ?click_cell,
                            anchor = ?anchor,
                            "ghost->anchor resolution failed; sidecar inconsistent",
                        );
                        return;
                    }
                };
                (anchor, anchor_slot, orientation)
            }
            None => {
                // No sidecar entry: a plain single-cell block. Resolve
                // trivially.
                (click_cell, click_slot, Cardinal::default())
            }
        }
    };

    // Compute footprint cells from the resolved entity.
    let def = registry.def(slot);
    let cells = world_footprint(anchor, &def.footprint, orientation);
    let mut cells_by_chunk: HashMap<ChunkCoord, Vec<(IVec3, IVec3)>> = HashMap::default();
    for cell in &cells {
        let (coord, local) = world_to_chunk(*cell);
        cells_by_chunk.entry(coord).or_default().push((*cell, local));
    }

    // Apply: clear each cell + drop the entry.
    for (coord, cells_in_chunk) in &cells_by_chunk {
        let Some(&chunk_entity) = map.0.get(coord) else {
            continue;
        };
        let Ok((mut chunk, mut entities)) = chunks.get_mut(chunk_entity) else {
            continue;
        };
        for &(world, local) in cells_in_chunk {
            chunk.set(local, BlockSlot::EMPTY);
            entities.remove(world);
            bus.write(CellEdit {
                world,
                slot: BlockSlot::EMPTY,
                // The full footprint shares the same source block — its
                // slot was captured at resolution above. Subscribers
                // (drops, sidecar cleanup) read this to learn *what*
                // was destroyed without re-querying the now-empty cell.
                prev_slot: slot,
            });
        }
        commands.entity(chunk_entity).insert(ChunkEdited);
    }

    // Broadcast the canonical applied break with the resolved anchor +
    // orientation, so other clients can compute the footprint themselves.
    let applied = BlockEdit {
        anchor,
        slot: BlockSlot::EMPTY,
        orientation,
    };
    if let Err(err) =
        broadcast.send::<BlockEdit, WorldChannel>(&applied, server, &NetworkTarget::All)
    {
        warn!("BlockEdit broadcast failed: {err}");
    }
}

/// Resolve a default-orientation footprint into world cells. Same shape
/// as the client-side helper — pulled into the server module so we don't
/// reach across the client/server split.
fn world_footprint(anchor: IVec3, def_footprint: &[[i32; 3]], orientation: Cardinal) -> Vec<IVec3> {
    def_footprint
        .iter()
        .map(|&offset| anchor + IVec3::from_array(orientation.rotate_offset(offset)))
        .collect()
}

/// NPC work consumer. Translates the brain's `NpcWorkCompleted` events
/// into `BlockEdit`s and feeds them through the same `apply_block_edit`
/// path that handles client requests — so the world mutation, the
/// broadcast, and the plan auto-clear all funnel through one code path.
#[allow(clippy::too_many_arguments, reason = "block-edit application spans many subsystems")]
fn apply_npc_work(
    mut reader: MessageReader<NpcWorkCompleted>,
    mut commands: Commands,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    servers: Query<&Server>,
    mut broadcast: ServerMultiMessageSender,
    mut bus: MessageWriter<CellEdit>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for completion in reader.read() {
        let edit = match completion.plan_kind {
            PlanKind::Remove => BlockEdit {
                anchor: completion.cell,
                slot: BlockSlot::EMPTY,
                orientation: Cardinal::default(),
            },
            PlanKind::Build { slot, orientation } => BlockEdit {
                anchor: completion.cell,
                slot,
                orientation,
            },
        };
        apply_block_edit(
            edit,
            &mut commands,
            &mut chunks,
            &map,
            &registry,
            server,
            &mut broadcast,
            &mut bus,
        );
    }
}

/// Server side of the NPC inspection RPC. Iterates each connection's
/// `RequestNpcDetails` queue, looks up the named NPC's state, and
/// sends a single `NpcDetails` reply over the requesting connection.
/// Targeted (per-connection sender) — other clients don't see this
/// traffic.
fn receive_npc_inspection_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<RequestNpcDetails>)>,
    npcs: Query<(&NpcId, &NpcKind, &Needs, &Brain, &AvatarPose), With<Npc>>,
    mut senders: Query<&mut MessageSender<NpcDetails>>,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<RequestNpcDetails> = receiver.receive().collect();
        for req in requests {
            let Some((id, kind, needs, brain, _pose)) = npcs
                .iter()
                .find(|(id, _, _, _, _)| id.0 == req.npc_id)
            else {
                // NPC despawned between client raycast and server
                // receive. Silently drop — the requester will time
                // out and the panel will close on its own.
                continue;
            };
            let (current_goal, target_cell) = summarize_goal(&brain.goal);
            let details = NpcDetails {
                npc_id: id.0,
                kind: kind.0.clone(),
                needs: needs.0.clone(),
                current_goal,
                target_cell,
            };
            if let Ok(mut sender) = senders.get_mut(connection) {
                sender.send::<WorldChannel>(details);
            }
        }
    }
}

/// Tolerance for the fuzzy spatial match in `receive_pickup_requests`.
/// 0.5 m is wider than any plausible client-server clock drift on the
/// item's position (items don't move; the only "drift" is sub-tick
/// scheduling) but tight enough that a click won't grab a neighbouring
/// pile by accident.
const PICKUP_MATCH_RADIUS: f32 = 0.5;

/// Anti-cheat distance from player eye to the pickup target. Generous —
/// a real reach gate based on the avatar's actual cursor raycast lives
/// client-side; this just rejects gross outliers.
const PICKUP_PLAYER_REACH: f32 = 12.0;

/// Apply a client's pickup request. Per request: find the player's
/// avatar, find the closest `WorldItem` to the requested translation,
/// then route the pickup based on item kind:
///   - tool (item def has non-empty `tool_tags`): goes into the
///     player's `EquippedTool` slot. If the slot is full, the
///     displaced tool drops as a fresh `WorldItem` at the player's
///     feet (swap semantics — picking up always succeeds).
///   - resource: goes into `Carrying`. Capacity / kind-mismatch
///     refusals are silent no-ops.
///
/// Carry + tool replication broadcasts new state back to the owner;
/// HUD picks it up next frame.
fn receive_pickup_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<PickupRequest>)>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut Carrying, &mut EquippedTool), With<Avatar>>,
    world_items: Query<(Entity, &WorldItem)>,
    item_registry: Res<ItemRegistry>,
    mut commands: Commands,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<PickupRequest> = receiver.receive().collect();
        for req in requests {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok((pose, mut carry, mut tool)) = players.get_mut(avatar) else {
                continue;
            };
            // Anti-cheat reach. Computed against eye position to match
            // how the client raycast measures distance.
            if (pose.translation - req.target).length() > PICKUP_PLAYER_REACH {
                continue;
            }
            // Closest WorldItem within the match radius.
            let mut best: Option<(Entity, crate::items::ItemSlot, f32)> = None;
            for (entity, wi) in world_items.iter() {
                let d = (wi.translation - req.target).length();
                if d > PICKUP_MATCH_RADIUS {
                    continue;
                }
                if best.map(|(_, _, bd)| d < bd).unwrap_or(true) {
                    best = Some((entity, wi.item, d));
                }
            }
            let Some((entity, item_slot, _)) = best else {
                continue;
            };
            let is_tool = !item_registry.def(item_slot).tool_tags.is_empty();
            if is_tool {
                // Swap into the tool slot. Drop the displaced tool
                // (if any) where the picked-up item *was* —
                // `req.target` is the client's click position, which
                // is within `PICKUP_MATCH_RADIUS` of the item we
                // matched. In-place swap reads as "I traded my axe
                // for the hammer that was here," much clearer than
                // the displaced tool landing at the player's feet
                // (potentially inside the body collider or behind
                // them).
                let displaced = tool.item.replace(item_slot);
                commands.entity(entity).despawn();
                info!(
                    new_tool = item_slot.0,
                    displaced = ?displaced.map(|s| s.0),
                    "tool pickup swap",
                );
                if let Some(prev_slot) = displaced
                    && prev_slot != item_slot
                {
                    commands.spawn((
                        WorldItem {
                            item: prev_slot,
                            translation: req.target,
                        },
                        Transform::from_translation(req.target),
                        GlobalTransform::default(),
                        Replicate::to_clients(NetworkTarget::All),
                        Name::new(format!("WorldItem(tool_swap:{})", prev_slot.0)),
                    ));
                }
            } else {
                if !carry.pickup_one(item_slot, PLAYER_CARRY_CAPACITY) {
                    // Carry full or holding a different item. Silent
                    // no-op; the player keeps their stack and the
                    // world item stays in the world.
                    continue;
                }
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Apply a client's drop request. Clears the player's `Carrying` and
/// spawns N `WorldItem` entities (one per unit in the dropped stack).
/// Items land one tile ahead of the player when that cell is standable
/// (so the player can see what they just dropped), else at the player's
/// feet (sliding off a cliff edge or facing a wall both degrade to
/// "right here"). A tight per-unit fan jitter keeps a stack from
/// z-fighting at the same point.
fn receive_drop_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DropRequest>)>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut Carrying), With<Avatar>>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mut commands: Commands,
) {
    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &block_registry,
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let request_count = receiver.receive().count();
        if request_count == 0 {
            continue;
        }
        let Some(&avatar) = avatars.0.get(&connection) else {
            continue;
        };
        let Ok((pose, mut carry)) = players.get_mut(avatar) else {
            continue;
        };
        let Some((item, count)) = carry.drop_all() else {
            continue;
        };
        let centre = drop_target_position(pose, &world);
        // Tight ring (0.08 m) so a 5-stack reads as "here," not "spread
        // across half a tile."
        for unit in 0..count {
            let angle = (unit as f32) * std::f32::consts::TAU / count.max(1) as f32;
            let offset = Vec3::new(angle.cos() * 0.08, 0.0, angle.sin() * 0.08);
            let translation = centre + offset;
            commands.spawn((
                WorldItem {
                    item,
                    translation,
                },
                Transform::from_translation(translation),
                GlobalTransform::default(),
                Replicate::to_clients(NetworkTarget::All),
                Name::new(format!("WorldItem(dropped:{})", item.0)),
            ));
        }
    }
}

/// Apply a client's tool-drop request. Takes the equipped tool out
/// of the player's `EquippedTool` and spawns a `WorldItem` at the
/// `drop_target_position` (in front of the player, fall back to
/// feet). No-op when the tool slot is empty. Mirrors
/// `receive_drop_requests` (carry) — the only differences are the
/// component touched and the single-unit drop (tools never stack).
fn receive_drop_tool_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DropToolRequest>)>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut EquippedTool), With<Avatar>>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mut commands: Commands,
) {
    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &block_registry,
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let request_count = receiver.receive().count();
        if request_count == 0 {
            continue;
        }
        let Some(&avatar) = avatars.0.get(&connection) else {
            continue;
        };
        let Ok((pose, mut tool)) = players.get_mut(avatar) else {
            continue;
        };
        let Some(slot) = tool.item.take() else {
            continue;
        };
        let target = drop_target_position(pose, &world);
        commands.spawn((
            WorldItem {
                item: slot,
                translation: target,
            },
            Transform::from_translation(target),
            GlobalTransform::default(),
            Replicate::to_clients(NetworkTarget::All),
            Name::new(format!("WorldItem(tool_dropped:{})", slot.0)),
        ));
    }
}

/// Compute where a dropped stack should land relative to `pose`.
/// Snaps the player's yaw to the dominant cardinal direction and
/// looks one tile ahead; if that cell is standable (foot empty, head
/// empty, supporting cell solid) the items drop on top of it. Else
/// fall back to the player's actual foot position. Items always
/// spawn slightly above the floor so visual meshes don't sink in.
fn drop_target_position(
    pose: &AvatarPose,
    world: &crate::npc::WorldWalk,
) -> Vec3 {
    let foot_pos = pose.translation
        - Vec3::new(0.0, EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y, 0.0);
    let foot_cell = IVec3::new(
        foot_pos.x.floor() as i32,
        foot_pos.y.floor() as i32,
        foot_pos.z.floor() as i32,
    );
    // Engine convention: yaw=0 → -Z (matches `apply_walk_step` /
    // `aim_yaw_step`). Snap to whichever axis the forward vector
    // dominates so the drop reads as "the way I'm facing" rather
    // than at some diagonal between two cells.
    let forward = Vec3::new(-pose.yaw.sin(), 0.0, -pose.yaw.cos());
    let cardinal = if forward.x.abs() > forward.z.abs() {
        IVec3::new(forward.x.signum() as i32, 0, 0)
    } else {
        IVec3::new(0, 0, forward.z.signum() as i32)
    };
    let forward_cell = foot_cell + cardinal;
    if crate::pathfinding::standable(world, forward_cell) {
        Vec3::new(
            forward_cell.x as f32 + 0.5,
            forward_cell.y as f32 + 0.05,
            forward_cell.z as f32 + 0.5,
        )
    } else {
        foot_pos + Vec3::new(0.0, 0.05, 0.0)
    }
}

/// Apply a client's deposit request: drop carry units into a Build
/// plan's `materials_present`. Per request: locate the player, read
/// their `Carrying`, compute how many units the targeted plan still
/// needs of that item, decrement the carry by that amount, increment
/// the plan, then broadcast the updated `PlanEdit` so every client's
/// `Plans` mirror sees the new materials. Silent no-op on:
///   - empty carry
///   - no plan at `cell` (was untagged between client click and server receive)
///   - plan isn't Build (Remove plans don't accept materials)
///   - plan doesn't need this item kind, or is already full.
fn receive_deposit_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DepositRequest>)>,
    avatars: Res<ClientAvatars>,
    mut players: Query<&mut Carrying, With<Avatar>>,
    mut plans: ResMut<Plans>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<DepositRequest> = receiver.receive().collect();
        for req in requests {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(mut carry) = players.get_mut(avatar) else {
                continue;
            };
            // Empty carry → nothing to deposit.
            let (carry_item, carry_count) = match (carry.item, carry.count) {
                (Some(slot), c) if c > 0 => (slot, c),
                _ => continue,
            };
            // Plan must exist + accept this item kind.
            let accepted = plans.deposit(req.cell, carry_item, carry_count);
            if accepted == 0 {
                continue;
            }
            carry.count = carry_count - accepted;
            if carry.count == 0 {
                carry.item = None;
            }
            // Broadcast the updated plan state so client mirrors learn
            // the new materials.present + outline re-renders.
            let updated_state = plans.get(req.cell).cloned();
            if let Some(state) = updated_state {
                let reply = PlanEdit {
                    cell: req.cell,
                    kind: Some(state.kind),
                    materials: state.materials,
                };
                if let Err(err) =
                    broadcast.send::<PlanEdit, WorldChannel>(&reply, server, &NetworkTarget::All)
                {
                    warn!("deposit PlanEdit broadcast failed: {err}");
                }
            }
        }
    }
}

/// Convert the engine-side [`Goal`] into a human-readable summary +
/// the cell the goal is targeted at (if any). Used in the inspection
/// RPC reply. Includes the remaining timer so the panel re-fetched
/// mid-action visibly counts down.
fn summarize_goal(goal: &Goal) -> (String, Option<IVec3>) {
    match goal {
        Goal::Idle => ("idle".into(), None),
        Goal::Resting { remaining_secs } => {
            (format!("resting ({remaining_secs:.1}s)"), None)
        }
        Goal::MoveTo { path, .. } => {
            let target = path.last().copied();
            (format!("moving ({} cells)", path.len()), target)
        }
        Goal::Interacting {
            remaining_secs,
            need_restore,
            target_cell,
            ..
        } => {
            let label = match need_restore {
                Some(nr) => format!("interacting ({}, {:.1}s)", nr.need, remaining_secs),
                None => format!("interacting ({remaining_secs:.1}s)"),
            };
            (label, Some(*target_cell))
        }
        Goal::Working {
            remaining_secs,
            target_cell,
            plan_kind,
            ..
        } => {
            let verb = match plan_kind {
                PlanKind::Remove => "removing",
                PlanKind::Build { .. } => "building",
            };
            (
                format!("{verb} ({remaining_secs:.1}s)"),
                Some(*target_cell),
            )
        }
    }
}

/// Auto-clear plan tags whose underlying world state no longer matches
/// the plan's intent. A Remove tag whose cell becomes empty (because
/// the player destroyed it themselves, or an NPC finished the job) is
/// stale; same for a Build tag whose cell becomes solid. Listens to
/// the per-cell `CellEdit` bus that `apply_block_edit` writes so both
/// player-driven and NPC-driven mutations trigger the cleanup.
///
/// Broadcasts a `PlanEdit { kind: None }` per cleared tag so client
/// mirrors drop their outline at the same moment the cell changes.
fn auto_clear_stale_plans(
    mut reader: MessageReader<CellEdit>,
    mut plans: ResMut<Plans>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for edit in reader.read() {
        let stale = match plans.get(edit.world).map(|s| s.kind) {
            Some(PlanKind::Remove) => edit.slot.is_empty(),
            Some(PlanKind::Build { .. }) => !edit.slot.is_empty(),
            None => false,
        };
        if !stale {
            continue;
        }
        plans.clear(edit.world);
        let msg = PlanEdit {
            cell: edit.world,
            kind: None,
            materials: Vec::new(),
        };
        if let Err(err) = broadcast.send::<PlanEdit, WorldChannel>(
            &msg,
            server,
            &NetworkTarget::All,
        ) {
            warn!("auto-clear PlanEdit broadcast failed: {err}");
        }
    }
}

/// Spawn drop items when a block is destroyed. Reads the same CellEdit
/// bus as `auto_clear_stale_plans`; for every edit whose `prev_slot` is
/// non-empty and whose `slot` is empty (i.e. a destroy), looks up the
/// destroyed block's `BlockDef.drops`, and for each entry spawns
/// `count` `WorldItem` entities at the destroyed cell's centre with a
/// small per-unit XZ jitter so a multi-item pile doesn't z-fight.
///
/// Server-authoritative spawn with `Replicate::to_clients(All)` — every
/// client gets the new entity in their next replication tick, and the
/// client-side observer attaches the visible glTF scene. Multi-cell
/// block destroys emit one CellEdit per footprint cell, all with the
/// same `prev_slot`; we deliberately spawn drops per cell so a 2-cell
/// bed dropping `{count=1}` of wood lands two logs (one per cell)
/// rather than one — mod authors who want exact totals set counts
/// against the number of cells the block occupies, or future stack
/// merging dedupes piles.
fn spawn_drops_on_destroy(
    mut reader: MessageReader<CellEdit>,
    mut commands: Commands,
    blocks: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
) {
    use block_junk_mod_api::items::ItemId;

    // Small deterministic-per-spawn hash for the jitter offset. Doesn't
    // need to be reproducible across sessions, just unique enough that
    // siblings don't perfectly overlap — pile reads as a heap.
    fn jitter(cell: IVec3, unit_index: u32) -> Vec3 {
        let h = (cell.x as i64)
            .wrapping_mul(73_856_093)
            .wrapping_add((cell.y as i64).wrapping_mul(19_349_663))
            .wrapping_add((cell.z as i64).wrapping_mul(83_492_791))
            .wrapping_add(unit_index as i64 * 2_654_435_761) as u64;
        let fx = ((h & 0xFFFF) as f32 / 65535.0 - 0.5) * 0.4;
        let fz = (((h >> 16) & 0xFFFF) as f32 / 65535.0 - 0.5) * 0.4;
        Vec3::new(fx, 0.0, fz)
    }

    for edit in reader.read() {
        if !edit.slot.is_empty() || edit.prev_slot.is_empty() {
            continue;
        }
        let def = blocks.def(edit.prev_slot);
        if def.drops.is_empty() {
            continue;
        }
        // Cell centre + a tiny lift off the floor so the mesh isn't
        // bisected by the next-block-down's top face.
        let centre = edit.world.as_vec3() + Vec3::new(0.5, 0.05, 0.5);
        for drop in &def.drops {
            let item_id: &ItemId = &drop.item;
            // boot validation guarantees this resolves; failing here
            // would be an engine bug.
            let Some(slot) = items.slot_of(item_id) else {
                error!(
                    block = %def.id,
                    item = %item_id,
                    "drops references item missing from registry after boot; skipping",
                );
                continue;
            };
            for unit in 0..drop.count {
                let translation = centre + jitter(edit.world, unit);
                commands.spawn((
                    WorldItem {
                        item: slot,
                        translation,
                    },
                    Transform::from_translation(translation),
                    GlobalTransform::default(),
                    Replicate::to_clients(NetworkTarget::All),
                    Name::new(format!("WorldItem({})", item_id)),
                ));
            }
        }
    }
}

/// Push any actor (player or NPC) out of a cell that just became solid.
///
/// Observed when an NPC finishes a Build plan while their head cell is
/// the build target — `PATH_ARRIVE_RADIUS` is wider than the body, and
/// the standable-neighbour picker only checks the foot's cell, so a
/// body straddling target_cell vertically is possible. After the
/// block lands, the body is embedded.
///
/// Mechanism: listens to the same `CellEdit` bus as
/// `auto_clear_stale_plans`. For each cell that became blocking (solid +
/// !walkable_boundary), find every actor whose AABB overlaps and pick
/// the smallest axis-aligned push **whose destination is itself clear of
/// other solids** — earlier versions picked the unconditionally-smallest
/// push and could shove an actor sideways into an adjacent wall, leaving
/// them embedded with no further `CellEdit` to trigger another rescue.
/// Tiny `PUSH_EPS` clears the face cleanly so the next collision sweep
/// doesn't re-detect overlap.
///
/// General by design: also fixes the case where a player Build-mode
/// places a block on a tile their predicted owner avatar happens to
/// straddle, and any future case (explosions, falling-block sim, etc.)
/// where a cell goes from empty to solid under an actor.
fn push_actors_out_of_new_blocks(
    mut reader: MessageReader<CellEdit>,
    registry: Res<BlockRegistry>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    mut actors: Query<&mut AvatarPose>,
) {
    /// Microscopic gap left between the actor's face and the cell's
    /// face after a push. Without it, the next sweep finds them
    /// exactly touching, classifies that as overlap, and re-pushes.
    const PUSH_EPS: f32 = 1e-3;

    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };

    for edit in reader.read() {
        if edit.slot.is_empty() {
            continue;
        }
        let def = registry.def(edit.slot);
        if !def.flags.solid || def.flags.walkable_boundary {
            continue;
        }
        let cell = edit.world;
        let cell_min = cell.as_vec3();
        let cell_max = cell_min + Vec3::ONE;
        for mut pose in actors.iter_mut() {
            let centre = pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
            let aabb_min = centre - PLAYER_HALF_EXTENTS;
            let aabb_max = centre + PLAYER_HALF_EXTENTS;
            if aabb_max.x <= cell_min.x || aabb_min.x >= cell_max.x {
                continue;
            }
            if aabb_max.y <= cell_min.y || aabb_min.y >= cell_max.y {
                continue;
            }
            if aabb_max.z <= cell_min.z || aabb_min.z >= cell_max.z {
                continue;
            }
            // Per-face escape distance. Each is the signed delta that
            // would just clear the actor's relevant face past the cell
            // face on the same axis.
            let mut candidates = [
                Vec3::new(cell_min.x - aabb_max.x - PUSH_EPS, 0.0, 0.0),
                Vec3::new(cell_max.x - aabb_min.x + PUSH_EPS, 0.0, 0.0),
                Vec3::new(0.0, cell_min.y - aabb_max.y - PUSH_EPS, 0.0),
                Vec3::new(0.0, cell_max.y - aabb_min.y + PUSH_EPS, 0.0),
                Vec3::new(0.0, 0.0, cell_min.z - aabb_max.z - PUSH_EPS),
                Vec3::new(0.0, 0.0, cell_max.z - aabb_min.z + PUSH_EPS),
            ];
            // Sort smallest-first, then take the first push that lands
            // the actor in a region clear of all solids. The unfiltered-
            // smallest pick was the bug — it could shove the actor into
            // an adjacent wall and the second-embedment had no CellEdit
            // to re-trigger a rescue.
            candidates.sort_by(|a, b| {
                a.length_squared()
                    .partial_cmp(&b.length_squared())
                    .unwrap_or(core::cmp::Ordering::Equal)
            });
            let chosen = candidates.iter().copied().find(|push| {
                let new_min = aabb_min + *push;
                let new_max = aabb_max + *push;
                let region = Aabb::from_min_max(new_min, new_max);
                let solids = world.candidates(region);
                !solids.iter().any(|s| {
                    new_max.x > s.min.x
                        && new_min.x < s.max.x
                        && new_max.y > s.min.y
                        && new_min.y < s.max.y
                        && new_max.z > s.min.z
                        && new_min.z < s.max.z
                })
            });
            match chosen {
                Some(push) => {
                    pose.translation += push;
                    info!(
                        cell = ?cell.to_array(),
                        push = ?push.to_array(),
                        "pushed actor out of newly-solid cell",
                    );
                }
                None => {
                    // Sealed pocket — every escape direction is also
                    // solid. Better to leave the actor in place and
                    // surface the situation than teleport blindly.
                    warn!(
                        cell = ?cell.to_array(),
                        actor_centre = ?centre.to_array(),
                        "no clear push direction — actor remains embedded; pathfinding will fail",
                    );
                }
            }
        }
    }
}

/// One-shot rescue for actors that load already embedded in a solid
/// cell. `load_from_save` spawns chunks via `Commands` without driving
/// the `CellEdit` bus, so `push_actors_out_of_new_blocks` (which is
/// edit-driven) never fires for them — an NPC that the world was saved
/// inside what's now a wall would otherwise stay stuck.
///
/// Per-actor: probe the body AABB against the current world, and if it
/// overlaps any solid, run the same smallest-clearing-push selection
/// `push_actors_out_of_new_blocks` uses. We try a few iterations so an
/// actor wedged into a corner can hop out face-by-face.
///
/// The `Local<bool>` gates this to one execution; the chunk-map guard
/// defers the run until `load_from_save`'s spawned chunks are flushed
/// into the ECS (otherwise the world looks empty and every actor is
/// "trivially clear"). On a fresh world with no save and no chunks yet,
/// the system parks at the guard and runs once chunks appear via the
/// AoI procedural fallback — also harmless, no actors will be in a
/// solid then either.
fn rescue_embedded_actors_after_load(
    mut ran: Local<bool>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut actors: Query<(Entity, &mut AvatarPose), With<Actor>>,
) {
    if *ran {
        return;
    }
    if chunk_map.0.is_empty() {
        return;
    }
    *ran = true;

    const PUSH_EPS: f32 = 1e-3;
    const MAX_ITERS: usize = 4;
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (entity, mut pose) in actors.iter_mut() {
        let mut iter = 0;
        loop {
            if iter >= MAX_ITERS {
                warn!(
                    entity = ?entity,
                    centre = ?(pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE).to_array(),
                    "stuck-on-load actor still embedded after {MAX_ITERS} pushout iterations",
                );
                break;
            }
            iter += 1;
            let centre = pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
            let aabb_min = centre - PLAYER_HALF_EXTENTS;
            let aabb_max = centre + PLAYER_HALF_EXTENTS;
            let probe = Aabb::from_min_max(aabb_min, aabb_max);
            let solids = world.candidates(probe);
            let overlap = solids.iter().find(|s| {
                aabb_max.x > s.min.x
                    && aabb_min.x < s.max.x
                    && aabb_max.y > s.min.y
                    && aabb_min.y < s.max.y
                    && aabb_max.z > s.min.z
                    && aabb_min.z < s.max.z
            });
            let Some(s) = overlap else {
                break;
            };
            // Same smallest-clearing-push selection as
            // `push_actors_out_of_new_blocks`. Picking the
            // unconditionally smallest delta could shove the actor
            // into an adjacent solid; iterate against the full
            // candidate set so a corner case yields a corner-escape.
            let mut candidates = [
                Vec3::new(s.min.x - aabb_max.x - PUSH_EPS, 0.0, 0.0),
                Vec3::new(s.max.x - aabb_min.x + PUSH_EPS, 0.0, 0.0),
                Vec3::new(0.0, s.min.y - aabb_max.y - PUSH_EPS, 0.0),
                Vec3::new(0.0, s.max.y - aabb_min.y + PUSH_EPS, 0.0),
                Vec3::new(0.0, 0.0, s.min.z - aabb_max.z - PUSH_EPS),
                Vec3::new(0.0, 0.0, s.max.z - aabb_min.z + PUSH_EPS),
            ];
            candidates.sort_by(|a, b| {
                a.length_squared()
                    .partial_cmp(&b.length_squared())
                    .unwrap_or(core::cmp::Ordering::Equal)
            });
            let chosen = candidates.iter().copied().find(|push| {
                let new_min = aabb_min + *push;
                let new_max = aabb_max + *push;
                let region = Aabb::from_min_max(new_min, new_max);
                let solids = world.candidates(region);
                !solids.iter().any(|s| {
                    new_max.x > s.min.x
                        && new_min.x < s.max.x
                        && new_max.y > s.min.y
                        && new_min.y < s.max.y
                        && new_max.z > s.min.z
                        && new_min.z < s.max.z
                })
            });
            match chosen {
                Some(push) => {
                    pose.translation += push;
                    info!(
                        entity = ?entity,
                        push = ?push.to_array(),
                        "rescued stuck-on-load actor",
                    );
                }
                None => {
                    warn!(
                        entity = ?entity,
                        centre = ?centre.to_array(),
                        "stuck-on-load actor: every push direction lands in another solid",
                    );
                    break;
                }
            }
        }
    }
}
