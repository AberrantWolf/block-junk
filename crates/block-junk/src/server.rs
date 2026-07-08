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
use crate::craft_stations::{ActiveWork, CraftOrder, CraftStations, StationState};
use crate::items::{ItemRegistry, PLAYER_CARRY_CAPACITY};
use crate::menu::{
    SAVE_RESULT_FAILED, SAVE_RESULT_OK, ServerSaveConfig, ServerSaveRequestFlag,
    ServerSaveResultFlag, ServerShutdownFlag,
};
use crate::npc::{Brain, Goal, Needs, Npc, NpcId, NpcKind, NpcPath, NpcStats, NpcWorkCompleted};
use crate::physics::{
    EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step, soft_separate_actors,
};
use crate::plans::Plans;
use crate::protocol::{
    ActionRejected, Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit,
    ModSetManifest, CHUNK_PADDED, Carrying, CellEdit, ChunkChannel, ChunkCoord, ChunkData,
    ChunkSnapshot, ChunkUnload, DepositRequest, DropRequest, DropToolRequest, EquippedTool,
    GameSet, INTERACT_REACH, MovementIntent, MovementMode, NpcAnimOverride, NpcDetails,
    PeriodicSyncChannel, PickupRequest, PlanEdit, PlanKind, REACH_SLACK, RejectReason,
    RequestNpcDetails, StateSyncChannel, WorldChannel, WorldClock, WorldClockSync, WorldItem,
    WorldToast,
};
use crate::rooms::{DetectionDirty, RoomEventMsg, RoomMap, mark_dirty_from_edits, process_dirty};
use crate::save::{
    SAVE_VERSION, SaveError, SaveFile, SavedActiveWork, SavedCarry, SavedChunk,
    SavedContainerState, SavedCraftOrder, SavedMaterialEntry, SavedNpc, SavedPlanState,
    SavedPlayer, SavedStationItem, SavedStationState, SavedTool, SavedWorldItem,
    UNCLAIMED_PLAYER_ID, read_save, remap_block_slots, write_save,
};
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
        app.add_plugins(crate::storage::StorageServerPlugin);
        app.add_plugins(crate::room_sync::RoomSyncServerPlugin);
        app.add_plugins(crate::plan_claims::PlanClaimsPlugin);
        app.add_plugins(crate::haul::HaulPlugin);
        app.add_plugins(crate::craft_stations::CraftStationsServerPlugin);
        app.add_plugins(crate::containers::ContainersServerPlugin);
        app.add_plugins(crate::regrow::RegrowServerPlugin);
        // ServerScriptingPlugin inserts BlockRegistry; resolve well-known
        // terrain slots from it once so chunk gen doesn't hash strings.
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(terrain_slots);
        // lightyear 0.28: the replication send rate is app-wide, not
        // per-sender. Overrides the Default (send every tick) the
        // lightyear plugins insert.
        app.insert_resource(ReplicationMetadata::new(REPLICATION_INTERVAL));
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientAvatars>();
        app.init_resource::<ClientChunks>();
        app.init_resource::<PendingChunks>();
        app.init_resource::<PlayerStates>();
        app.init_resource::<SaveWriteGuard>();
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
                settle_items_on_cell_edit,
                push_actors_out_of_new_blocks,
            )
                .chain()
                .after(receive_block_edits)
                .in_set(GameSet::Simulation),
        );
        // S4 bush-eat transform. Its own add_systems call — the tuple
        // above is at the trait-resolution limit. Emits CellEdit/
        // BlockEdit like apply_npc_work; no drops, so ordering vs
        // spawn_drops_on_destroy is immaterial, only "after edits."
        app.add_systems(
            Update,
            apply_npc_consumption
                .after(receive_block_edits)
                .in_set(GameSet::Simulation),
        );
        // Station + container teardown ride the same CellEdit bus.
        // Separate add_systems call: the chained tuple above sits at
        // the Bevy 0.18 trait-resolution limit already.
        app.add_systems(
            Update,
            (
                crate::craft_stations::clear_destroyed_stations,
                crate::containers::clear_destroyed_containers,
            )
                .after(receive_block_edits)
                .in_set(GameSet::Simulation),
        );
        // Live-path invalidation rides the CellEdit bus too: flags NPCs
        // whose MoveTo path envelope an edit touched (`PathDirty`); the
        // FixedUpdate brain tick re-validates and repaths. Must stay in
        // Update — a MessageReader polled from FixedUpdate drops
        // messages on frames without a fixed tick.
        app.add_systems(
            Update,
            crate::npc::mark_paths_dirty_on_cell_edit
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
        // Soft actor separation (players only — kinematic NPCs ghost
        // through) runs after the player step has moved everyone for
        // this tick. The pairwise push nudges overlapping players
        // apart 50/50 — gentle pushing instead of hard contact-stop.
        app.add_systems(
            FixedUpdate,
            soft_separate_actors.after(server_player_step),
        );
        // Block-stuck NPCs from a save (or any load-time edge case
        // where an actor is inside a solid cell) get one pushout
        // attempt on the first Update tick after chunks have flushed
        // in from `load_from_save`.
        app.add_systems(
            Update,
            rescue_embedded_actors_after_load.in_set(GameSet::Simulation),
        );
        // Same load-time edge case for loose items: settle any restored
        // from a save onto solid ground once chunks have flushed in, so
        // a saved item over since-edited terrain doesn't hang in the air.
        app.add_systems(
            Update,
            settle_loaded_items_after_load.in_set(GameSet::Simulation),
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

/// One player's persisted state in runtime types (slots resolved).
#[derive(Clone, Debug)]
pub struct PlayerState {
    pub pose: AvatarPose,
    pub carry: Carrying,
    pub tool: EquippedTool,
}

/// Per-client-id player persistence, server-side. Filled from the save
/// on load and by disconnects during the session; an entry is *removed*
/// when its player connects (their live avatar is authoritative until
/// they disconnect again — the save assembler reads connected players
/// straight from their avatars). The [`save::UNCLAIMED_PLAYER_ID`]
/// entry is v12-migrated legacy state; the first client id connecting
/// without an entry of its own claims it.
#[derive(Resource, Default)]
pub struct PlayerStates(pub HashMap<u64, PlayerState>);

/// The u64 the wire actually authenticated. `None` for peer kinds our
/// transports never produce — callers should skip persistence for
/// those rather than corrupt the table.
fn client_id_u64(remote: &RemoteId) -> Option<u64> {
    match remote.0 {
        PeerId::Netcode(id) => Some(id),
        other => {
            warn!(?other, "connection has a non-netcode peer id; not persisting it");
            None
        }
    }
}

/// Set by `load_from_save` when a load was attempted and failed (corrupt
/// blob, version mismatch, missing block ids). The session then runs on
/// an empty world — and every save path refuses to write to the
/// configured name, so the original file is never clobbered by the
/// accidental fresh world. A *missing* save (hosting a new world) does
/// NOT set this; there's nothing to protect.
#[derive(Resource, Default)]
pub struct SaveWriteGuard {
    pub reason: Option<String>,
}


/// Server App Startup: if `ServerSaveConfig::load_existing`, read the save
/// file and pre-populate `ChunkMap` with the persisted edited chunks. They
/// land with the `ChunkEdited` marker so subsequent AoI sends ship the
/// bytes rather than the procedural shortcut. Procedural chunks aren't
/// persisted (`Chunk::from_terrain` regenerates them on demand).
///
/// A load failure does NOT abort startup — we log and continue with an
/// empty world. Better than an unbootable session if a save is corrupt.
/// But the failure arms [`SaveWriteGuard`], so nothing this session
/// writes back over the file that failed to load — the empty world is
/// throwaway, the original save is not.
#[allow(
    clippy::too_many_arguments,
    reason = "load_from_save touches every persisted system"
)]
fn load_from_save(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut player_states: ResMut<PlayerStates>,
    mut dirty: ResMut<DetectionDirty>,
    mut clock: ResMut<WorldClock>,
    mut plans: ResMut<Plans>,
    mut stations: ResMut<CraftStations>,
    mut containers: ResMut<crate::containers::Containers>,
    mut storage_zones: ResMut<crate::storage::StorageZones>,
    mut guard: ResMut<SaveWriteGuard>,
    mut npc_ids: ResMut<crate::npc::NpcIdAllocator>,
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
    let fail = |e: &SaveError, guard: &mut SaveWriteGuard| {
        error!(
            "load save {name:?} failed: {e}; continuing with an EMPTY world. \
             Saving to {name:?} is disabled for this session so the \
             original file stays intact."
        );
        guard.reason = Some(e.to_string());
    };
    let mut save = match read_save(name) {
        Ok(s) => s,
        Err(e @ SaveError::NotFound { .. }) => {
            // Hosting a fresh world under a new name — nothing to
            // protect, saving stays enabled.
            info!("no existing save ({e}); starting fresh");
            return;
        }
        Err(e) => {
            fail(&e, &mut guard);
            return;
        }
    };
    // Saved chunk grids + Build plans store raw slots; rewrite them
    // through the save's own slot table into the live registry before
    // anything downstream reads a slot. Refuses (load fails, guard
    // arms) if the save references a block id that's no longer
    // registered — loading would silently transmute those cells.
    let lookup = |id: &str| block_registry.slot_of(&block_junk_mod_api::blocks::BlockId::new(id));
    if let Err(e) = remap_block_slots(&mut save, lookup) {
        fail(&e, &mut guard);
        return;
    }
    info!(
        "loading {} edited chunks + {} NPCs + {} plans + {} world items from save {name:?}",
        save.edited_chunks.len(),
        save.npcs.len(),
        save.plans.len(),
        save.world_items.len(),
    );
    // Storage zones are plain coords — no slot remap, straight restore.
    if !save.storage_cells.is_empty() {
        storage_zones.replace_all(save.storage_cells.iter().copied());
    }
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
                let active_work = saved.active_work.map(|aw| ActiveWork {
                    recipe_id: aw.recipe_id,
                    total_secs: aw.total_secs,
                    elapsed_secs: aw.elapsed_secs,
                });
                (
                    cell,
                    StationState {
                        orders,
                        inventory,
                        active_work,
                    },
                )
            })
            .collect();
        stations.replace_all(restored);
    }
    // Restore container stock (v17). Same id→slot resolution as
    // station inventories; unknown ids drop just that entry.
    if !save.containers.is_empty() {
        let restored: Vec<(IVec3, crate::containers::ContainerState)> = save
            .containers
            .into_iter()
            .map(|(cell, saved)| {
                let mut state = crate::containers::ContainerState::default();
                for entry in saved.inventory {
                    let id = block_junk_mod_api::items::ItemId::new(entry.item_id.clone());
                    match item_registry.slot_of(&id) {
                        Some(slot) => state.deposit(slot, entry.count),
                        None => warn!(
                            cell = ?cell.to_array(),
                            item = %entry.item_id,
                            "saved container stock references unknown item id; dropping entry",
                        ),
                    }
                }
                (cell, state)
            })
            .collect();
        containers.replace_all(restored);
    }
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
    // Saved padding is maintained at runtime, so chunks from a healthy
    // save are already mutually consistent — this refresh is a no-op
    // there. It exists to heal saves written before padding write-
    // through landed (stale mirrors of edited neighbours baked into
    // the file). Padding facing *procedural* chunks needs no fixup:
    // a never-edited neighbour matches terrain, which is what
    // generation put in the padding.
    let originals: HashMap<ChunkCoord, Chunk> = save
        .edited_chunks
        .iter()
        .map(|sc| (sc.coord, sc.chunk.clone()))
        .collect();
    for sc in &mut save.edited_chunks {
        sc.chunk.refresh_padding(sc.coord, |world| {
            let (ncoord, nlocal) = world_to_chunk(world);
            originals.get(&ncoord).map(|c| c.get(nlocal))
        });
    }
    drop(originals);

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
        // Keep the runtime allocator ahead of every persisted id so a
        // post-load spawn can't collide (claim tables key on NpcId).
        npc_ids.reserve_through(npc.id);
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
                count: saved.count.max(1),
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
    // Players table: resolve each entry's item ids → slots (unknown ids
    // — a mod removed between sessions — degrade that field to empty
    // with a warning, not the whole entry). Entries sit here until
    // their client id connects; `register_new_client` consumes them.
    for saved in save.players {
        let state = resolve_saved_player(&saved, &item_registry);
        player_states.0.insert(saved.client_id, state);
    }
    if !player_states.0.is_empty() {
        info!(
            "restored {} player state entr{} from save",
            player_states.0.len(),
            if player_states.0.len() == 1 { "y" } else { "ies" },
        );
    }
}

/// On-disk player entry → runtime types. Carry/tool item ids that no
/// longer resolve (mod uninstalled between sessions) degrade to empty
/// with a warning — same policy as world items and NPC carry.
fn resolve_saved_player(saved: &SavedPlayer, item_registry: &ItemRegistry) -> PlayerState {
    let carry = match &saved.carry {
        Some(sc) => {
            let id = block_junk_mod_api::items::ItemId::new(sc.item_id.clone());
            match item_registry.slot_of(&id) {
                Some(slot) => Carrying {
                    item: Some(slot),
                    count: sc.count,
                },
                None => {
                    warn!(
                        client_id = saved.client_id,
                        item = %sc.item_id,
                        "saved player carry references unknown item id; restoring empty-handed",
                    );
                    Carrying::default()
                }
            }
        }
        None => Carrying::default(),
    };
    let tool = match &saved.tool {
        Some(st) => {
            let id = block_junk_mod_api::items::ItemId::new(st.item_id.clone());
            match item_registry.slot_of(&id) {
                Some(slot) => EquippedTool { item: Some(slot) },
                None => {
                    warn!(
                        client_id = saved.client_id,
                        item = %st.item_id,
                        "saved player tool references unknown item id; restoring empty slot",
                    );
                    EquippedTool::default()
                }
            }
        }
        None => EquippedTool::default(),
    };
    PlayerState {
        pose: saved.pose,
        carry,
        tool,
    }
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
    // Stats: saved values win; anything the registry declares that the
    // save lacks (pre-stats save, or a mod added a stat since) is
    // rolled now from the persisted rng — mirrors the needs merge
    // above. The advanced rng goes into Brain so the roll isn't
    // repeatable, and the rolled value persists on the next save.
    let mut rng = npc.rng;
    let mut stats = npc.stats;
    if let Some(def) = kind_registry.get(&npc.kind) {
        for stat in &def.stats {
            stats.entry(stat.id.clone()).or_insert_with(|| {
                stat.min + crate::npc::rand_unit(&mut rng) * (stat.max - stat.min)
            });
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
            NpcStats(stats),
            Brain {
                goal: Goal::Idle,
                rng,
                home_cluster: None,
                preempt_cooldown_secs: 0.0,
            },
            carry,
            tool,
        ),
        npc.pose,
        AvatarVelocity::default(),
        AvatarOnGround::default(),
        npc.movement_mode,
        crate::npc_mover::NavMover::default(),
        NpcPath::default(),
        NpcAnimOverride::default(),
        Replicate::to_clients(NetworkTarget::All),
        InterpolationTarget::to_clients(NetworkTarget::All),
        Name::new(format!("npc:{}", npc.id)),
    ));
}

/// Polled each tick. When the client flips the save-request atomic, write
/// the world to disk, publish the outcome, then clear the flag — in that
/// order, because the client treats the flag dropping as "the outcome is
/// readable". Clearing first (the old `swap`) made every refusal and
/// write error render as "Saved ✓" in the pause menu. Unlike
/// `save_then_shutdown` this is multi-shot (the user might "Save Now"
/// several times per session) so no Local guard.
fn save_on_request(
    flag: Option<Res<ServerSaveRequestFlag>>,
    result: Option<Res<ServerSaveResultFlag>>,
    config: Option<Res<ServerSaveConfig>>,
    ctx: SaveCtx,
) {
    use core::sync::atomic::Ordering;
    let Some(flag) = flag else {
        return;
    };
    if !flag.0.load(Ordering::SeqCst) {
        return;
    }
    let ok = 'save: {
        let Some(config) = config else {
            error!("save requested but no ServerSaveConfig; nothing written");
            break 'save false;
        };
        let Some(name) = &config.save_name else {
            error!("save requested but no save name configured; nothing written");
            break 'save false;
        };
        if let Some(reason) = &ctx.guard.reason {
            error!(
                "NOT saving to {name:?}: this session's load failed ({reason}); \
                 the original save file is preserved untouched"
            );
            break 'save false;
        }
        let save = assemble_save_file(&ctx);
        write_save_logged(name, &save)
    };
    if let Some(result) = result {
        result.0.store(
            if ok {
                SAVE_RESULT_OK
            } else {
                SAVE_RESULT_FAILED
            },
            Ordering::SeqCst,
        );
    }
    // A request that arrived mid-save is satisfied by the write that just
    // finished; clearing unconditionally can't lose meaningful work.
    flag.0.store(false, Ordering::SeqCst);
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
            let active_work = state.active_work.as_ref().map(|aw| SavedActiveWork {
                recipe_id: aw.recipe_id.clone(),
                total_secs: aw.total_secs,
                elapsed_secs: aw.elapsed_secs,
            });
            (
                *cell,
                SavedStationState {
                    orders,
                    inventory,
                    active_work,
                },
            )
        })
        .collect()
}

/// Convert the engine-side `Containers` snapshot to the on-disk
/// shape. Stock entries sort by item id for deterministic bytes (the
/// backing HashMap iterates in arbitrary order).
fn convert_saved_containers(
    containers: &crate::containers::Containers,
    item_registry: &ItemRegistry,
) -> Vec<(IVec3, SavedContainerState)> {
    let mut out: Vec<(IVec3, SavedContainerState)> = containers
        .iter()
        .map(|(cell, state)| {
            let mut inventory: Vec<SavedStationItem> = state
                .inventory
                .iter()
                .map(|(slot, count)| SavedStationItem {
                    item_id: item_registry.id_of(*slot).to_string(),
                    count: *count,
                })
                .collect();
            inventory.sort_by(|a, b| a.item_id.cmp(&b.item_id));
            (*cell, SavedContainerState { inventory })
        })
        .collect();
    out.sort_by_key(|(c, _)| (c.x, c.y, c.z));
    out
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
            count: wi.count,
        })
        .collect()
}

/// Shrink a stack after some units were withdrawn: despawn the old
/// entity and, if any units remain, respawn a fresh `WorldItem` at the
/// same spot with the remaining count. This keeps `WorldItem.count`
/// immutable for a live entity (see the WorldItem docs) — the client's
/// tier mesh is always chosen fresh at spawn, never swapped in place —
/// and it naturally invalidates any stale NPC reservation on the old
/// entity id. `remainder == 0` is a plain despawn.
pub fn shrink_or_despawn_stack(
    commands: &mut Commands,
    entity: Entity,
    item: crate::items::ItemSlot,
    translation: Vec3,
    remainder: u32,
) {
    commands.entity(entity).despawn();
    if remainder > 0 {
        commands.spawn((
            WorldItem {
                item,
                translation,
                count: remainder,
            },
            Transform::from_translation(translation),
            GlobalTransform::default(),
            Replicate::to_clients(NetworkTarget::All),
            Name::new(format!("WorldItem(remainder:{})", item.0)),
        ));
    }
}

/// Carry stack → on-disk shape. Empty (or zero-count) serialises as
/// `None`, matching the load path's `Option` semantics.
fn saved_carry_of(carry: &Carrying, item_registry: &ItemRegistry) -> Option<SavedCarry> {
    match (carry.item, carry.count) {
        (Some(slot), count) if count > 0 => Some(SavedCarry {
            item_id: item_registry.id_of(slot).to_string(),
            count,
        }),
        _ => None,
    }
}

/// Tool slot → on-disk shape. Empty slot serialises as `None`.
fn saved_tool_of(tool: &EquippedTool, item_registry: &ItemRegistry) -> Option<SavedTool> {
    tool.item.map(|slot| SavedTool {
        item_id: item_registry.id_of(slot).to_string(),
    })
}

/// Every player the save should remember: connected clients read live
/// from their avatars (via `ClientAvatars` + each connection's
/// `RemoteId`), everyone else from the offline [`PlayerStates`] table
/// (disconnects this session + not-yet-claimed loaded entries — the two
/// sets are disjoint because connecting removes the table entry).
/// Sorted by id so identical state produces identical bytes.
fn collect_saved_players(ctx: &SaveCtx) -> Vec<SavedPlayer> {
    let mut players: Vec<SavedPlayer> = Vec::new();
    for (conn, avatar) in ctx.client_avatars.0.iter() {
        let Some(id) = ctx.remote_ids.get(*conn).ok().and_then(client_id_u64) else {
            continue;
        };
        let Ok((pose, carry, tool)) = ctx.avatar_states.get(*avatar) else {
            continue;
        };
        players.push(SavedPlayer {
            client_id: id,
            pose: *pose,
            carry: saved_carry_of(carry, &ctx.item_registry),
            tool: saved_tool_of(tool, &ctx.item_registry),
        });
    }
    for (id, state) in ctx.player_states.0.iter() {
        players.push(SavedPlayer {
            client_id: *id,
            pose: state.pose,
            carry: saved_carry_of(&state.carry, &ctx.item_registry),
            tool: saved_tool_of(&state.tool, &ctx.item_registry),
        });
    }
    players.sort_by_key(|p| p.client_id);
    players
}

/// Snapshot every NPC's persistent state. `BrainDisabled` NPCs are
/// included — the marker is treated as a runtime recovery state, not
/// persisted, so reloading gives the planner a fresh chance. A
/// consistently broken planner will re-disable each NPC on its first
/// tick after load (and log loudly each time).
fn collect_saved_npcs(npcs: &SavedNpcQuery, item_registry: &ItemRegistry) -> Vec<SavedNpc> {
    npcs.iter()
        .map(|(id, kind, pose, mode, needs, brain, carry, tool, stats)| SavedNpc {
            id: id.0,
            kind: kind.0.clone(),
            pose: *pose,
            movement_mode: *mode,
            needs: needs.0.clone(),
            rng: brain.rng,
            carrying: saved_carry_of(carry, item_registry),
            tool: saved_tool_of(tool, item_registry),
            stats: stats.0.clone(),
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
fn save_then_shutdown(
    flag: Option<Res<ServerShutdownFlag>>,
    config: Option<Res<ServerSaveConfig>>,
    ctx: SaveCtx,
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
                if let Some(reason) = &ctx.guard.reason {
                    error!(
                        "NOT saving to {name:?}: this session's load failed ({reason}); \
                         the original save file is preserved untouched"
                    );
                } else {
                    let save = assemble_save_file(&ctx);
                    write_save_logged(name, &save);
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

/// Every component the save format captures per NPC. One alias so the
/// two save systems and `collect_saved_npcs` can't drift.
type SavedNpcQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static NpcId,
        &'static NpcKind,
        &'static AvatarPose,
        &'static MovementMode,
        &'static Needs,
        &'static Brain,
        &'static Carrying,
        &'static EquippedTool,
        &'static NpcStats,
    ),
    With<Npc>,
>;

/// Everything the save assembler reads, bundled into one SystemParam so
/// the two save systems stay under Bevy 0.18's 16-param ceiling (same
/// pattern as `HaulCtx` in npc.rs) and can't drift on what gets
/// persisted.
#[derive(bevy::ecs::system::SystemParam)]
pub struct SaveCtx<'w, 's> {
    clock: Res<'w, WorldClock>,
    plans: Res<'w, Plans>,
    stations: Res<'w, CraftStations>,
    containers: Res<'w, crate::containers::Containers>,
    storage_zones: Res<'w, crate::storage::StorageZones>,
    chunks: Query<
        'w,
        's,
        (&'static ChunkCoord, &'static Chunk, &'static ChunkEntities),
        With<ChunkEdited>,
    >,
    npcs: SavedNpcQuery<'w, 's>,
    world_items: Query<'w, 's, &'static WorldItem>,
    item_registry: Res<'w, ItemRegistry>,
    block_registry: Res<'w, BlockRegistry>,
    client_avatars: Res<'w, ClientAvatars>,
    remote_ids: Query<'w, 's, &'static RemoteId>,
    avatar_states: Query<
        'w,
        's,
        (&'static AvatarPose, &'static Carrying, &'static EquippedTool),
        With<Avatar>,
    >,
    player_states: Res<'w, PlayerStates>,
    guard: Res<'w, SaveWriteGuard>,
}

/// Snapshot every persisted system into a `SaveFile`. Shared by the
/// quit-save and Save Now paths so the two can't drift on what gets
/// persisted.
fn assemble_save_file(ctx: &SaveCtx) -> SaveFile {
    let edited: Vec<SavedChunk> = ctx
        .chunks
        .iter()
        .map(|(coord, ch, ce)| SavedChunk {
            coord: *coord,
            chunk: ch.clone(),
            entities: ce.clone(),
        })
        .collect();
    SaveFile {
        version: SAVE_VERSION,
        block_slots: ctx.block_registry.slot_table(),
        edited_chunks: edited,
        players: collect_saved_players(ctx),
        npcs: collect_saved_npcs(&ctx.npcs, &ctx.item_registry),
        world_clock: Some(*ctx.clock),
        plans: convert_saved_plans(&ctx.plans, &ctx.item_registry),
        world_items: collect_saved_world_items(&ctx.world_items, &ctx.item_registry),
        craft_stations: convert_saved_stations(&ctx.stations, &ctx.item_registry),
        storage_cells: {
            // Sorted for deterministic bytes — HashSet iteration order
            // would churn the save blob on every write.
            let mut cells = ctx.storage_zones.snapshot();
            cells.sort_by_key(|c| (c.x, c.y, c.z));
            cells
        },
        containers: convert_saved_containers(&ctx.containers, &ctx.item_registry),
    }
}

/// `write_save` + the standard outcome log line. Returns whether the
/// file actually reached disk so callers can report honestly.
fn write_save_logged(name: &str, save: &SaveFile) -> bool {
    match write_save(name, save) {
        Ok(()) => {
            info!(
                "saved {} chunks + {} NPCs + {} plans + {} items + {} stations + {} player(s) to {name:?}",
                save.edited_chunks.len(),
                save.npcs.len(),
                save.plans.len(),
                save.world_items.len(),
                save.craft_stations.len(),
                save.players.len(),
            );
            true
        }
        Err(e) => {
            error!("save to {name:?} failed: {e}");
            false
        }
    }
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
///
/// Since lightyear 0.28 the sender is a plain marker; the send interval
/// lives in the app-wide `ReplicationMetadata` resource instead.
fn install_replication_sender(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(ReplicationSender);
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
    mut player_states: ResMut<PlayerStates>,
    registry: Res<BlockRegistry>,
    item_registry: Res<ItemRegistry>,
    recipe_registry: Res<crate::recipes::RecipeRegistry>,
    npc_kind_registry: Res<crate::npc_registry::NpcKindRegistry>,
    room_pattern_registry: Res<crate::rooms::RoomPatternRegistry>,
    mut manifests: Query<&mut MessageSender<ModSetManifest>>,
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
    //
    // Spawn state, in priority order:
    //   1. This client id's own persisted entry (returning player).
    //   2. The save's unclaimed pre-identity entry, if any (first
    //      claimant wins — the v12 single-slot convention).
    //   3. Fresh defaults: spawn above the sine-wave terrain (peaks
    //      ~y=16) so the first physics tick lands the player on the
    //      surface, empty carry, starter axe. Eye height =
    //      AvatarPose.translation by convention.
    // Entries are removed on claim: while connected, the live avatar is
    // authoritative and the disconnect observer writes it back.
    let persisted = client_id_u64(remote).and_then(|id| {
        player_states.0.remove(&id).or_else(|| {
            let legacy = player_states.0.remove(&UNCLAIMED_PLAYER_ID);
            if legacy.is_some() {
                info!(
                    client_id = id,
                    "claimed the save's pre-identity player state"
                );
            }
            legacy
        })
    });
    let (spawn_pose, spawn_carry, spawn_tool) = match persisted {
        Some(state) => (state.pose, state.carry, state.tool),
        None => {
            // Brand-new identity spawns empty-handed: the day-one loop
            // is bare-hands chopping (soft tool gates) into workbench
            // tool crafting, not a granted starter axe.
            (
                AvatarPose {
                    translation: Vec3::new(0.0, 32.0, 60.0),
                    yaw: 0.0,
                },
                Carrying::default(),
                EquippedTool::default(),
            )
        }
    };
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
                // Persistent: lightyear's SessionBased default despawns
                // the avatar on disconnect BEFORE our On<Remove,
                // ClientOf> observer can read its pose/carry/tool to
                // bank them — the state would silently vanish. We own
                // the despawn in `forget_disconnected_client` instead.
                lifetime: lightyear::prelude::Lifetime::Persistent,
            },
            Name::new(format!("avatar:{}", remote.0)),
        ))
        .id();
    avatars.0.insert(connection, avatar);
    sent.0.entry(connection).or_default();

    // Send the mod-set fingerprint once so the client can validate its
    // registries against ours. The client refuses the session on any
    // disagreement — see `receive_modset_manifest` in client.rs.
    if let Ok(mut sender) = manifests.get_mut(connection) {
        let manifest = crate::modset::local_manifest(
            &registry,
            &item_registry,
            &recipe_registry,
            &npc_kind_registry,
            &room_pattern_registry,
        );
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
        sender.send::<PeriodicSyncChannel>(msg);
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
        apply_walk_step(
            &mut pose,
            &mut vel,
            &mut on_ground,
            &mut mode,
            &input.0,
            dt,
            &world,
        );
    }
}

fn forget_disconnected_client(
    trigger: On<Remove, ClientOf>,
    mut commands: Commands,
    mut avatars: ResMut<ClientAvatars>,
    mut sent: ResMut<ClientChunks>,
    mut player_states: ResMut<PlayerStates>,
    remote_ids: Query<&RemoteId>,
    states: Query<(&AvatarPose, &Carrying, &EquippedTool), With<Avatar>>,
) {
    if let Some(avatar) = avatars.0.remove(&trigger.entity) {
        // Bank the departing player's pose + carry + tool under their
        // client id so the next save persists it and a reconnect
        // restores it. (Pre-identity, this spilled the items as world
        // drops instead; now they stay "in the player's hands.")
        if let Ok((pose, carry, tool)) = states.get(avatar) {
            match remote_ids.get(trigger.entity).ok().and_then(client_id_u64) {
                Some(id) => {
                    player_states.0.insert(
                        id,
                        PlayerState {
                            pose: *pose,
                            carry: *carry,
                            tool: *tool,
                        },
                    );
                    info!(client_id = id, "stored disconnecting player's state");
                }
                None => {
                    // No stable identity to bank under (peer kind our
                    // transports shouldn't produce). Spill the items so
                    // they at least survive in the world — never
                    // silently destroy resources.
                    let cell = pose.translation.floor().as_ivec3();
                    let base = pose.translation + Vec3::new(0.0, 0.05, 0.0);
                    let mut units: Vec<crate::items::ItemSlot> = Vec::new();
                    if let Some(item) = carry.item {
                        units.extend(std::iter::repeat_n(item, carry.count as usize));
                    }
                    units.extend(tool.item);
                    for (i, slot) in units.iter().enumerate() {
                        let translation = base + crate::items::drop_jitter(cell, i as u32);
                        commands.spawn((
                            WorldItem {
                                item: *slot,
                                translation,
                                count: 1,
                            },
                            Transform::from_translation(translation),
                            GlobalTransform::default(),
                            Replicate::to_clients(NetworkTarget::All),
                            Name::new(format!("WorldItem(disconnect:{})", slot.0)),
                        ));
                    }
                    if !units.is_empty() {
                        warn!(
                            count = units.len(),
                            at = ?cell.to_array(),
                            "disconnect without stable id: dropped carried items into the world",
                        );
                    }
                }
            }
        }
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
    edited: Query<&Chunk, With<ChunkEdited>>,
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
    for (coord, mut chunk) in completed {
        // Guard against clobbering a live chunk. `update_aoi` only
        // queues coords absent from `ChunkMap`, so today this can't
        // fire — but if the queueing logic ever races a save-load (or
        // future eviction re-queues a coord that came back), the
        // unconditional insert below would replace an *edited* chunk
        // with fresh terrain and silently drop the player's edits
        // (the spawn also lacks `ChunkEdited`, so the loss would even
        // survive the next save). Prefer dropping the generated bytes
        // — regeneration is cheap, edits are not.
        if let Some(&existing) = chunk_map.0.get(&coord) {
            warn!(
                coord = ?coord.0.to_array(),
                ?existing,
                "chunk gen completed for an already-live chunk; discarding generated copy",
            );
            continue;
        }
        // The gen task only knows the terrain function. If a neighbour
        // chunk has been *edited* at the shared border, this chunk's
        // terrain-derived padding is already stale — pull the real
        // values before the chunk goes live. Unedited neighbours match
        // terrain by definition, so only edited ones are consulted.
        chunk.refresh_padding(coord, |world| {
            let (ncoord, nlocal) = world_to_chunk(world);
            chunk_map
                .0
                .get(&ncoord)
                .and_then(|&e| edited.get(e).ok())
                .map(|c| c.get(nlocal))
        });
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
                sender.send::<ChunkChannel>(ChunkSnapshot {
                    coord: *coord,
                    data,
                    entities,
                });
                current.insert(*coord);
            }
        }

        for coord in &removed {
            if let Ok(mut sender) = unloads.get_mut(client_entity) {
                sender.send::<ChunkChannel>(ChunkUnload { coord: *coord });
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
    let mut set =
        HashSet::with_capacity(((2 * AOI_RADIUS_XZ + 1).pow(2) * (2 * AOI_RADIUS_Y + 1)) as usize);
    for cy in -AOI_RADIUS_Y..=AOI_RADIUS_Y {
        for cz in -AOI_RADIUS_XZ..=AOI_RADIUS_XZ {
            for cx in -AOI_RADIUS_XZ..=AOI_RADIUS_XZ {
                set.insert(ChunkCoord(centre.0 + IVec3::new(cx, cy, cz)));
            }
        }
    }
    set
}

/// Reply to one client that its request was received and refused. No-op
/// if the connection has no sender (mid-disconnect) — the request was
/// already dropped, this is just the courtesy note feeding the client's
/// rejection toast.
pub(crate) fn send_rejection(
    senders: &mut Query<&mut MessageSender<ActionRejected>>,
    client: Entity,
    cell: IVec3,
    reason: RejectReason,
) {
    if let Ok(mut sender) = senders.get_mut(client) {
        sender.send::<WorldChannel>(ActionRejected { cell, reason });
    }
}

/// Server-side reach gate, shared by every mutating request handler.
/// `target` is the point being acted on (cell centre for blocks). The
/// slack absorbs the camera-eye vs avatar-pose measurement difference
/// so a click the client's exact-reach raycast accepted isn't refused.
pub(crate) fn within_reach(pose: &AvatarPose, target: Vec3, reach: f32) -> bool {
    (pose.translation - target).length() <= reach + REACH_SLACK
}

#[allow(
    clippy::too_many_arguments,
    reason = "edit pipeline + reach gate + rejection reply"
)]
fn receive_block_edits(
    mut commands: Commands,
    mut receivers: Query<(Entity, &mut MessageReceiver<BlockEdit>)>,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    avatars: Res<ClientAvatars>,
    poses: Query<&AvatarPose, With<Avatar>>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut bus: MessageWriter<CellEdit>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let edits: Vec<BlockEdit> = receiver.receive().collect();
        for edit in edits {
            // Reach gate — the server half of the INTERACT_REACH
            // contract. Mining/placing was the one mutating verb with
            // no server-side validation at all.
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let centre = edit.anchor.as_vec3() + Vec3::splat(0.5);
            if !within_reach(pose, centre, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    edit.anchor,
                    RejectReason::OutOfReach,
                );
                continue;
            }
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
        apply_break(
            edit, commands, chunks, map, registry, server, broadcast, bus,
        );
    } else {
        apply_place(
            edit, commands, chunks, map, registry, server, broadcast, bus,
        );
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
        cells_by_chunk
            .entry(coord)
            .or_default()
            .push((*cell, local));
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
                is_anchor: world == edit.anchor,
            });
        }
        commands.entity(chunk_entity).insert(ChunkEdited);
    }
    // Echo border cells into loaded neighbours' padding rings (own pass
    // — the per-chunk loop above holds a mutable borrow per chunk, and a
    // cell's mirrors live in *other* chunks, possibly ones in this very
    // footprint).
    let mirror_cells: Vec<(IVec3, BlockSlot)> =
        cells.iter().map(|&world| (world, edit.slot)).collect();
    crate::voxel::apply_padding_mirrors(&mirror_cells, map, chunks);

    // ChunkChannel, not WorldChannel: the client drops edits for chunks it
    // hasn't loaded, trusting the eventual ChunkSnapshot to carry the final
    // state. That only holds if snapshot and edit share one ordered stream —
    // on separate channels a small edit can overtake a fragmented snapshot
    // cut *after* it and be lost until the chunk re-enters AoI.
    if let Err(err) = broadcast.send::<BlockEdit, ChunkChannel>(&edit, server, &NetworkTarget::All)
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
        cells_by_chunk
            .entry(coord)
            .or_default()
            .push((*cell, local));
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
                is_anchor: world == anchor,
            });
        }
        commands.entity(chunk_entity).insert(ChunkEdited);
    }
    // Echo the cleared cells into loaded neighbours' padding rings
    // (separate pass for the same borrow reason as the place path).
    let mirror_cells: Vec<(IVec3, BlockSlot)> = cells
        .iter()
        .map(|&world| (world, BlockSlot::EMPTY))
        .collect();
    crate::voxel::apply_padding_mirrors(&mirror_cells, map, chunks);

    // Broadcast the canonical applied break with the resolved anchor +
    // orientation, so other clients can compute the footprint themselves.
    // ChunkChannel for snapshot/edit ordering — see apply_place.
    let applied = BlockEdit {
        anchor,
        slot: BlockSlot::EMPTY,
        orientation,
    };
    if let Err(err) =
        broadcast.send::<BlockEdit, ChunkChannel>(&applied, server, &NetworkTarget::All)
    {
        warn!("BlockEdit broadcast failed: {err}");
    }

    // S4 forage harvest-transform: a resource block with `depleted_block`
    // (a ripe berry bush) doesn't leave bare air when harvested — it
    // leaves its depleted form behind (bare bush), which the regrow
    // system then grows back. The clear above already emitted the
    // destroy `CellEdit` that spawned the drops (berries) off `slot`;
    // now stamp the depleted block into the just-emptied anchor cell.
    // Single-cell only: we place the depleted block's own footprint at
    // the anchor, so a multi-cell resource block would only restore its
    // anchor cell — fine for bushes, revisit if a big harvestable lands.
    if let Some(depleted_id) = registry.def(slot).depleted_block.clone()
        && let Some(depleted_slot) = registry.slot_of(&depleted_id)
    {
        apply_place(
            BlockEdit {
                anchor,
                slot: depleted_slot,
                orientation: Cardinal::default(),
            },
            commands, chunks, map, registry, server, broadcast, bus,
        );
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
#[allow(
    clippy::too_many_arguments,
    reason = "block-edit application spans many subsystems"
)]
fn apply_npc_work(
    mut reader: MessageReader<NpcWorkCompleted>,
    mut commands: Commands,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    plans: Res<Plans>,
    servers: Query<&Server>,
    mut broadcast: ServerMultiMessageSender,
    mut bus: MessageWriter<CellEdit>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for completion in reader.read() {
        // Authoritative half of "cancel cancels": the brain re-checks
        // before emitting, but a plan edit can land between that check
        // and this apply. Never mutate the world for a plan that no
        // longer exists as completed.
        if plans.kind(completion.cell) != Some(completion.plan_kind) {
            info!(
                cell = ?completion.cell.to_array(),
                "npc work completion against missing/changed plan; skipping edit",
            );
            continue;
        }
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

/// NPC bush-eat consumer (S4 forage). Translates each
/// [`NpcConsumedInteractable`] into an in-place swap of the block for
/// its `depleted_block` — *without* spawning drops (the villager ate
/// the berries; harvesting the same bush drops them instead).
///
/// The swap is done as a silent clear (no `CellEdit`, so
/// `spawn_drops_on_destroy` never fires) followed by an `apply_place` of
/// the depleted block, which emits the `CellEdit`/`BlockEdit` that
/// updates the interactable index, arms the regrow timer, and reaches
/// clients. Single-cell only — bushes have no footprint or sidecar.
fn apply_npc_consumption(
    mut reader: MessageReader<crate::npc::NpcConsumedInteractable>,
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
    for msg in reader.read() {
        let (coord, local) = world_to_chunk(msg.cell);
        let Some(&entity) = map.0.get(&coord) else {
            continue;
        };
        // Resolve the depleted target from whatever block is currently
        // there — it may have been mined/regrown between eat and now.
        let depleted_slot = {
            let Ok((chunk, _)) = chunks.get(entity) else {
                continue;
            };
            let slot = chunk.get(local);
            if slot.is_empty() {
                continue;
            }
            let Some(depleted_id) = registry.def(slot).depleted_block.clone() else {
                continue;
            };
            match registry.slot_of(&depleted_id) {
                Some(s) => s,
                None => continue,
            }
        };
        // Silently empty the cell (no CellEdit ⇒ no drops), then place
        // the depleted block through the shared path.
        {
            let Ok((mut chunk, _)) = chunks.get_mut(entity) else {
                continue;
            };
            chunk.set(local, BlockSlot::EMPTY);
        }
        apply_place(
            BlockEdit {
                anchor: msg.cell,
                slot: depleted_slot,
                orientation: Cardinal::default(),
            },
            &mut commands,
            &mut chunks,
            &map,
            &registry,
            server,
            &mut broadcast,
            &mut bus,
        );
        info!(
            cell = ?msg.cell.to_array(),
            depleted = %registry.id_of(depleted_slot),
            "npc ate a self-consuming interactable; depleted it",
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
    npcs: Query<(&NpcId, &NpcKind, &Needs, &NpcStats, &Brain, &AvatarPose), With<Npc>>,
    mut senders: Query<&mut MessageSender<NpcDetails>>,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<RequestNpcDetails> = receiver.receive().collect();
        for req in requests {
            let Some((id, kind, needs, stats, brain, _pose)) =
                npcs.iter().find(|(id, _, _, _, _, _)| id.0 == req.npc_id)
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
                stats: stats.0.clone(),
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
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut commands: Commands,
) {
    // Despawns are deferred Commands, so the world_items query stays
    // stale for the whole system run — two requests matching the same
    // item in one tick (two players clicking one pile, or a double
    // click) would both "succeed" and duplicate it. Track what this
    // run already claimed.
    let mut claimed: HashSet<Entity> = HashSet::default();
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<PickupRequest> = receiver.receive().collect();
        for req in requests {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok((pose, mut carry, mut tool)) = players.get_mut(avatar) else {
                continue;
            };
            if !within_reach(pose, req.target, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.target.floor().as_ivec3(),
                    RejectReason::OutOfReach,
                );
                continue;
            }
            // Closest WorldItem within the match radius.
            let mut best: Option<(Entity, crate::items::ItemSlot, Vec3, u32, f32)> = None;
            for (entity, wi) in world_items.iter() {
                if claimed.contains(&entity) {
                    continue;
                }
                let d = (wi.translation - req.target).length();
                if d > PICKUP_MATCH_RADIUS {
                    continue;
                }
                if best.map(|(_, _, _, _, bd)| d < bd).unwrap_or(true) {
                    best = Some((entity, wi.item, wi.translation, wi.count, d));
                }
            }
            let Some((entity, item_slot, item_translation, item_count, _)) = best else {
                continue;
            };
            let is_tool = !item_registry.def(item_slot).tool_tags.is_empty();
            if is_tool {
                // Identical to the held tool → pure no-op. The swap
                // path would despawn the ground tool and then skip
                // respawning the "displaced" one (same slot), silently
                // destroying a tool.
                if tool.item == Some(item_slot) {
                    continue;
                }
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
                claimed.insert(entity);
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
                            count: 1,
                        },
                        Transform::from_translation(req.target),
                        GlobalTransform::default(),
                        Replicate::to_clients(NetworkTarget::All),
                        Name::new(format!("WorldItem(tool_swap:{})", prev_slot.0)),
                    ));
                }
            } else {
                // Withdraw as much of the (possibly multi-unit pile) as
                // fits in the carry stack. `pickup_many` returns 0 when
                // the hand is full or holds a different item — silent
                // no-op then, the pile stays put.
                let taken = carry.pickup_many(item_slot, item_count, PLAYER_CARRY_CAPACITY);
                if taken == 0 {
                    continue;
                }
                claimed.insert(entity);
                shrink_or_despawn_stack(
                    &mut commands,
                    entity,
                    item_slot,
                    item_translation,
                    item_count - taken,
                );
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
        // across half a tile." Each unit settles like a destroy-drop —
        // the fallback branch of `drop_target_position` is the flying
        // player's foot position, which can be any height above ground,
        // and an unsettled spawn there floats forever (no CellEdit ever
        // re-settles a cell nothing was built in).
        for unit in 0..count {
            let angle = (unit as f32) * std::f32::consts::TAU / count.max(1) as f32;
            let offset = Vec3::new(angle.cos() * 0.08, 0.0, angle.sin() * 0.08);
            let translation = settled_translation(&world, centre + offset);
            commands.spawn((
                WorldItem {
                    item,
                    translation,
                    count: 1,
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
        // Settled for the same reason as receive_drop_requests: the
        // foot-position fallback can be mid-air.
        let target = settled_translation(&world, drop_target_position(pose, &world));
        commands.spawn((
            WorldItem {
                item: slot,
                translation: target,
                count: 1,
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
fn drop_target_position(pose: &AvatarPose, world: &crate::npc::WorldWalk) -> Vec3 {
    let foot_pos =
        pose.translation - Vec3::new(0.0, EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y, 0.0);
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
    mut players: Query<(&AvatarPose, &mut Carrying), With<Avatar>>,
    mut plans: ResMut<Plans>,
    item_registry: Res<ItemRegistry>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut toast_senders: Query<&mut MessageSender<WorldToast>>,
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
            let Ok((pose, mut carry)) = players.get_mut(avatar) else {
                continue;
            };
            if !within_reach(pose, req.cell.as_vec3() + Vec3::splat(0.5), INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
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
                // Targeted deposit receipt — the depositor sees
                // "Wood Log 2/4" at the plan cell (there's no audio
                // yet, and the ghost re-tint alone is easy to miss).
                // Other clients learn the same from the PlanEdit
                // broadcast below.
                if let Some(entry) = state.materials.iter().find(|m| m.item == carry_item)
                    && let Ok(mut sender) = toast_senders.get_mut(connection)
                {
                    sender.send::<WorldChannel>(WorldToast {
                        cell: req.cell,
                        text: format!(
                            "{} {}/{}",
                            item_registry.def(entry.item).display_name,
                            entry.present,
                            entry.needed
                        ),
                    });
                }
                let reply = PlanEdit {
                    cell: req.cell,
                    kind: Some(state.kind),
                    materials: state.materials,
                };
                if let Err(err) = broadcast.send::<PlanEdit, StateSyncChannel>(
                    &reply,
                    server,
                    &NetworkTarget::All,
                ) {
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
        Goal::Resting { remaining_secs } => (format!("resting ({remaining_secs:.1}s)"), None),
        Goal::SleepingGround { remaining_secs, .. } => {
            (format!("sleeping on the ground ({remaining_secs:.1}s)"), None)
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
            (format!("{verb} ({remaining_secs:.1}s)"), Some(*target_cell))
        }
        Goal::CraftingAtStation { station_cell } => ("crafting".into(), Some(*station_cell)),
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
        if let Err(err) =
            broadcast.send::<PlanEdit, StateSyncChannel>(&msg, server, &NetworkTarget::All)
        {
            warn!("auto-clear PlanEdit broadcast failed: {err}");
        }
    }
}

/// Where a loose item at `translation` comes to rest against the live
/// world. Items move along Y only — XZ is preserved so a settling pile
/// stays put laterally and a falling drop reads as a vertical drop, not
/// a slide. The resting Y is quantized to the base of the owning empty
/// cell (`cell.y + ITEM_FLOOR_LIFT`), so `translation.floor()` always
/// recovers that cell and items never hang at a partial-block height.
///
/// All the safety lives in [`settle_item_cell`]: it rises out of a cell
/// that just became solid, falls to the first solid support, clamps
/// (never deletes) when there is none, and — because unloaded chunks
/// read as solid via [`WorldWalk`] — never drops an item through the
/// loaded/unloaded boundary and out of the world.
pub(crate) fn settled_translation(
    world: &impl crate::pathfinding::Walkability,
    translation: Vec3,
) -> Vec3 {
    use crate::items::{ITEM_FLOOR_LIFT, MAX_ITEM_DROP, MAX_ITEM_RISE};
    let from = translation.floor().as_ivec3();
    let cell = crate::pathfinding::settle_item_cell(world, from, MAX_ITEM_RISE, MAX_ITEM_DROP);
    Vec3::new(
        translation.x,
        cell.y as f32 + ITEM_FLOOR_LIFT,
        translation.z,
    )
}

/// Spawn drop items when a block is destroyed. Reads the same CellEdit
/// bus as `auto_clear_stale_plans`; for each destroyed *block* (the
/// destroy-edit with `is_anchor` set — exactly one per block, however
/// many footprint cells it covered), looks up the destroyed block's
/// `BlockDef.drops`, and for each entry spawns `count` `WorldItem`
/// entities at the anchor cell's centre with a small per-unit XZ jitter
/// so a multi-item pile doesn't z-fight.
///
/// `drops` is a per-block contract, like `materials`: a 2-cell bed with
/// `{wood, count=3}` yields 3 wood, exactly, whatever its footprint.
/// (An earlier version spawned the list once per footprint *cell*,
/// which minted resources against build cost and made non-divisible
/// totals unrepresentable — destruction is atomic per block, so cell
/// granularity corresponded to nothing.)
///
/// Each drop is then settled down to solid ground via
/// [`settled_translation`] — a block destroyed in mid-air (the floating
/// block above the player's reach, or the last block of a column) leaves
/// its drops resting on whatever is below instead of hanging where the
/// block used to be. Drops don't support each other, so a whole pile
/// settles onto the same floor cell.
///
/// Server-authoritative spawn with `Replicate::to_clients(All)` — every
/// client gets the new entity in their next replication tick, and the
/// client-side observer attaches the visible glTF scene.
fn spawn_drops_on_destroy(
    mut reader: MessageReader<CellEdit>,
    mut commands: Commands,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    blocks: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
) {
    use crate::items::drop_jitter as jitter;
    use block_junk_mod_api::items::ItemId;

    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &blocks,
    };

    for edit in reader.read() {
        if !edit.slot.is_empty() || edit.prev_slot.is_empty() || !edit.is_anchor {
            continue;
        }
        let def = blocks.def(edit.prev_slot);
        if def.resolved_drops().is_empty() {
            continue;
        }
        // Cell centre. The +0.5 XZ centres the item in the cell (jitter
        // then spreads a pile within it); Y here is unimportant because
        // `settled_translation` re-quantizes it to the resting cell's
        // base — we only need it to floor() back to `edit.world`.
        let centre = edit.world.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
        for drop in def.resolved_drops() {
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
                let translation = settled_translation(&world, centre + jitter(edit.world, unit));
                commands.spawn((
                    WorldItem {
                        item: slot,
                        translation,
                        count: 1,
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
/// the build target — the standable-neighbour picker only checks the
/// foot's cell, so a body straddling target_cell vertically is
/// possible. After the block lands, the body is embedded.
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

/// Re-settle loose items when the block under them is mined out or a
/// block is built into their cell. Reads the same `CellEdit` bus as
/// `spawn_drops_on_destroy`, so it sees the authoritative chunk state
/// *after* the edit is applied.
///
/// An item rests in an empty cell `C` sitting on a solid support `C - Y`.
/// Two edits can break that invariant: the support `C - Y` is destroyed
/// (item must fall), or `C` itself is filled (item must rise). Both map
/// to a small set of "touched" owning cells per edit — the edited cell
/// (filled) and the cell above it (support removed) — so we collect that
/// set, then make a single pass over loose items and re-settle any whose
/// owning cell (`translation.floor()`) was touched. `settled_translation`
/// handles fall, rise, and clamp uniformly, so re-settling an item that
/// doesn't actually need to move is a no-op; the `!=` guard then skips
/// the redundant component write so lightyear replicates only real moves.
///
/// Items don't support items and re-settling never emits a `CellEdit`,
/// so there is no cascade and no feedback loop — a tower of items above a
/// mined block all fall straight to the same exposed floor in one pass.
fn settle_items_on_cell_edit(
    mut reader: MessageReader<CellEdit>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut items: Query<&mut WorldItem>,
) {
    let mut touched: HashSet<IVec3> = HashSet::default();
    for edit in reader.read() {
        // The edited cell (a block may have filled an item's cell) and
        // the cell directly above it (whose support this edit changed).
        touched.insert(edit.world);
        touched.insert(edit.world + IVec3::Y);
    }
    if touched.is_empty() {
        return;
    }

    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for mut wi in items.iter_mut() {
        let owning = wi.translation.floor().as_ivec3();
        if !touched.contains(&owning) {
            continue;
        }
        let settled = settled_translation(&world, wi.translation);
        if settled != wi.translation {
            wi.translation = settled;
        }
    }
}

/// One-shot settle for loose items restored from a save. `load_from_save`
/// spawns world items via `Commands` without driving the `CellEdit` bus,
/// so `settle_items_on_cell_edit` never fires for them; an item saved at
/// a stale position (the world below it was edited in a prior session, or
/// the save predates the settle rules) would otherwise hang in the air.
///
/// Mirrors `rescue_embedded_actors_after_load`: the `Local<bool>` gates
/// it to a single run, and the chunk-map guard defers that run until
/// `load_from_save`'s spawned chunks have flushed into the ECS (otherwise
/// the world looks empty and every item would "settle" by clamping into
/// the void). On a fresh world with no saved items this settles nothing
/// and simply marks itself done.
fn settle_loaded_items_after_load(
    mut ran: Local<bool>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut items: Query<&mut WorldItem>,
) {
    if *ran {
        return;
    }
    if chunk_map.0.is_empty() {
        return;
    }
    *ran = true;

    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    let mut settled_count = 0usize;
    for mut wi in items.iter_mut() {
        let settled = settled_translation(&world, wi.translation);
        if settled != wi.translation {
            wi.translation = settled;
            settled_count += 1;
        }
    }
    if settled_count > 0 {
        info!("settled {settled_count} loaded world items onto solid ground");
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
