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
    ActionRejected, Actor, AuthenticatedPlayer, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity,
    BlockEdit, BlockWorkIntent, BlockWorkTarget, Carrying, CellEdit, ChunkCoord, ChunkData,
    ClientReady, DepositRequest, DropRequest, DropToolRequest, EquippedTool, GameReady, GameSet,
    INTERACT_REACH, MovementIntent, MovementMode, NpcAnimOverride, NpcDetails, PeriodicSyncChannel,
    PickupRequest, PlanKind, REACH_SLACK, RejectReason, RequestNpcDetails, ServerHello,
    TerrainEditRecord, WorldChannel, WorldClock, WorldClockSync, WorldItem,
};
use crate::recipes::RecipeRegistry;
use crate::save::{
    SAVE_VERSION, SaveError, SaveFile, SavedActiveWork, SavedCarry, SavedChunk,
    SavedContainerState, SavedCraftOrder, SavedMaterialEntry, SavedNpc, SavedPlanState,
    SavedPlayer, SavedStationItem, SavedStationState, SavedTool, SavedWorldItem, read_save,
    remap_block_slots, write_save,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, chunk_world_transform, world_to_chunk};

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
        app.add_plugins(crate::placement_network::PlacementServerPlugin);
        app.add_plugins(crate::plan_claims::PlanClaimsPlugin);
        app.add_plugins(crate::haul::HaulPlugin);
        app.add_plugins(crate::craft_stations::CraftStationsServerPlugin);
        app.add_plugins(crate::containers::ContainersServerPlugin);
        app.add_plugins(crate::regrow::RegrowServerPlugin);
        // ServerScriptingPlugin inserts BlockRegistry; resolve well-known
        // terrain slots from it once so chunk gen doesn't hash strings.
        let terrain_slots = TerrainSlots::from_registry(
            app.world().resource::<BlockRegistry>(),
            &app.world().resource::<crate::scripting::SelectedWorld>().0,
        );
        app.insert_resource(terrain_slots);
        // lightyear 0.28: the replication send rate is app-wide, not
        // per-sender. Overrides the Default (send every tick) the
        // lightyear plugins insert.
        app.insert_resource(ReplicationMetadata::new(REPLICATION_INTERVAL));
        app.init_resource::<ChunkMap>();
        app.init_resource::<crate::placements::Placements>();
        app.init_resource::<crate::facility_interactions::FacilityInteractions>();
        crate::spatial::init_session_resource::<crate::item_placement::ItemPlacementQueue>(app);
        app.add_systems(PostUpdate, crate::item_placement::deliver);
        app.init_resource::<ClientAvatars>();
        app.init_resource::<SpatialSubscriptions>();
        app.init_resource::<SpatialVisibilityState>();
        app.init_resource::<PendingChunks>();
        app.init_resource::<ActiveSimulationChunks>();
        app.init_resource::<crate::spatial_geometry::StructuralSummaries>();
        app.init_resource::<crate::space_identity::SpaceIdentities>();
        app.init_resource::<crate::spatial_events::SpatialJournal>();
        app.init_resource::<crate::capabilities::Capabilities>()
            .init_resource::<crate::capabilities::CapabilityWork>()
            .init_resource::<crate::capabilities::CapabilityDependencies>();
        app.init_resource::<crate::facility_access::FacilityAccess>()
            .init_resource::<crate::facility_access::AccessWork>()
            .init_resource::<crate::facility_access::FacilityDependencies>();
        app.init_resource::<crate::settlements::Settlements>()
            .init_resource::<crate::settlements::SettlementWork>()
            .init_resource::<crate::settlements::SettlementDependencies>();
        app.init_resource::<crate::live_spaces::SpatialDependencies>()
            .init_resource::<crate::live_spaces::RoofEnvelope>()
            .init_resource::<crate::live_spaces::SpatialComputation>()
            .init_resource::<crate::live_spaces::PublishedSpaces>();
        app.add_systems(
            PostUpdate,
            (
                crate::live_spaces::track_roof_envelope,
                crate::spatial_geometry::refresh_structural_summaries,
                crate::spatial_geometry::materialize_facilities,
                crate::live_spaces::compute_spaces,
                crate::settlements::refresh,
                crate::facility_access::refresh,
                crate::capabilities::refresh,
                crate::live_spaces::request_dependency_chunks,
            )
                .chain(),
        );
        app.init_resource::<PlayerStates>();
        app.init_resource::<BlockWorkLeases>();
        app.init_resource::<RequestBudgets>();
        app.init_resource::<SnapshotBudgets>();
        app.init_resource::<crate::space_network::SpaceSenders>();
        app.init_resource::<crate::capability_network::CapabilitySenders>();
        app.add_systems(
            PostUpdate,
            (
                crate::space_network::publish,
                crate::capability_network::publish,
            )
                .after(crate::capabilities::refresh),
        );
        app.init_resource::<SaveWriteGuard>();
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
        app.add_systems(
            Update,
            flush_mod_terrain_effects.in_set(GameSet::PostSimulation),
        );
        app.add_systems(Update, receive_block_edits.in_set(GameSet::Simulation));
        // NPC work-completion adapter: translates the brain's local-bus
        // `NpcWorkCompleted` events into the same `apply_block_edit`
        // path that handles client `BlockEdit` messages — so the world
        // mutation, the broadcast, and the plan auto-clear all run
        // through one code path.
        //
        // `auto_clear_stale_plans` listens to the `CellEdit` bus that
        // `apply_block_edit` writes per cell change and clears any
        // matching plan tag; the spatial store journals the removal.
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
            apply_facility_completion
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
                crate::portal_service::receive,
            )
                .in_set(GameSet::Simulation),
        );
        app.add_systems(
            Update,
            (
                request_npc_chunks,
                poll_chunk_gen,
                update_aoi,
                refresh_spatial_scopes,
                update_spatial_visibility,
            )
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
        app.add_systems(FixedUpdate, soft_separate_actors.after(server_player_step));
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
        app.add_systems(Startup, load_from_save.in_set(WorldLoad));
        app.add_systems(Last, (save_then_shutdown, save_on_request));
        app.add_systems(Update, (verify_client_ready, expire_ready_challenges));
        app.add_observer(send_server_hello);
        app.add_observer(install_replication_sender);
        app.add_observer(register_new_client);
        app.add_observer(forget_disconnected_client);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RequestClass {
    Ordinary,
    BatchCells(usize),
    NpcInspection,
}

#[derive(Resource, Default)]
pub(crate) struct RequestBudgets(HashMap<Entity, PeerRequestBudget>);

#[derive(Resource, Default)]
pub(crate) struct SnapshotBudgets(HashMap<Entity, SnapshotBudget>);
impl SnapshotBudgets {
    pub(crate) fn spend(&mut self, connection: Entity, bytes: usize) -> bool {
        let budget = self.0.entry(connection).or_default();
        let now = Instant::now();
        budget.bytes = (budget.bytes
            + now.duration_since(budget.updated_at).as_secs_f32()
                * crate::spatial::SPATIAL_BYTES_PER_SECOND as f32)
            .min(crate::spatial::SPATIAL_BURST_BYTES as f32);
        budget.updated_at = now;
        if bytes as f32 > budget.bytes {
            return false;
        }
        budget.bytes -= bytes as f32;
        true
    }
}

struct SnapshotBudget {
    updated_at: Instant,
    bytes: f32,
    pending: Option<PendingChunkTransmission>,
}

struct PendingChunkTransmission {
    coord: ChunkCoord,
    messages: Vec<(crate::protocol::SpatialMessage, usize)>,
    next: usize,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        Self {
            updated_at: Instant::now(),
            bytes: crate::spatial::SPATIAL_BURST_BYTES as f32,
            pending: None,
        }
    }
}

struct PeerRequestBudget {
    updated_at: Instant,
    ordinary: f32,
    batch_cells: f32,
    npc_inspection: f32,
    malformed: std::collections::VecDeque<Instant>,
}

impl Default for PeerRequestBudget {
    fn default() -> Self {
        Self {
            updated_at: Instant::now(),
            ordinary: 120.0,
            batch_cells: 8192.0,
            npc_inspection: 10.0,
            malformed: std::collections::VecDeque::new(),
        }
    }
}

/// Shared C2S mutation boundary. It resolves readiness/authenticated
/// identity and charges the request's central per-peer token bucket before
/// a feature handler examines world state.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct ValidatedRequestContext<'w, 's> {
    authenticated: Query<'w, 's, &'static AuthenticatedPlayer, With<GameReady>>,
    budgets: ResMut<'w, RequestBudgets>,
    subscriptions: Res<'w, SpatialSubscriptions>,
}

impl ValidatedRequestContext<'_, '_> {
    pub(crate) fn authorize(
        &mut self,
        connection: Entity,
        class: RequestClass,
    ) -> Option<AuthenticatedPlayer> {
        let auth = *self.authenticated.get(connection).ok()?;
        let budget = self.budgets.0.entry(connection).or_default();
        let now = Instant::now();
        let elapsed = now.duration_since(budget.updated_at).as_secs_f32();
        budget.updated_at = now;
        budget.ordinary = (budget.ordinary + elapsed * 60.0).min(120.0);
        budget.batch_cells = (budget.batch_cells + elapsed * 8192.0).min(8192.0);
        budget.npc_inspection = (budget.npc_inspection + elapsed * 10.0).min(10.0);
        let tokens = match class {
            RequestClass::Ordinary => &mut budget.ordinary,
            RequestClass::BatchCells(_) => &mut budget.batch_cells,
            RequestClass::NpcInspection => &mut budget.npc_inspection,
        };
        let cost = match class {
            RequestClass::BatchCells(cells) => cells as f32,
            _ => 1.0,
        };
        if *tokens < cost {
            return None;
        }
        *tokens -= cost;
        Some(auth)
    }

    /// Record malformed input. Returns true at three strikes inside the
    /// ten-second window, at which point the caller disconnects the peer.
    pub(crate) fn malformed(&mut self, connection: Entity) -> bool {
        let now = Instant::now();
        let strikes = &mut self.budgets.0.entry(connection).or_default().malformed;
        while strikes
            .front()
            .is_some_and(|strike| now.duration_since(*strike) > Duration::from_secs(10))
        {
            strikes.pop_front();
        }
        strikes.push_back(now);
        strikes.len() >= 3
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
/// straight from their avatars).
#[derive(Resource, Default)]
pub struct PlayerStates(pub HashMap<u64, PlayerState>);

/// The u64 the wire actually authenticated. `None` for peer kinds our
/// transports never produce — callers should skip persistence for
/// those rather than corrupt the table.
fn client_id_u64(remote: &RemoteId) -> Option<u64> {
    match remote.0 {
        PeerId::Netcode(id) => Some(id),
        other => {
            warn!(
                ?other,
                "connection has a non-netcode peer id; not persisting it"
            );
            None
        }
    }
}

/// Set by `load_from_save` when a load was attempted and failed (corrupt
/// blob, version mismatch, missing block ids). The session then runs on
/// runtime startup — and every save path refuses to write to the
/// configured name, so the original file is never clobbered by the
/// accidental fresh world. A *missing* save (hosting a new world) does
/// NOT set this; there's nothing to protect.
#[derive(Resource, Default)]
pub(crate) struct SaveWriteGuard {
    pub reason: Option<String>,
}

/// Server App Startup: if `ServerSaveConfig::load_existing`, read the save
/// file and pre-populate `ChunkMap` with the persisted edited chunks. They
/// land with the `ChunkEdited` marker so subsequent AoI sends ship the
/// bytes rather than the procedural shortcut. Procedural chunks aren't
/// persisted (`Chunk::from_terrain` regenerates them on demand).
///
/// A load failure arms [`SaveWriteGuard`]. Runtime activation then aborts
/// startup before hooks or population bootstrap; the original save is untouched.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WorldLoad;

#[derive(bevy::ecs::system::SystemParam)]
struct SaveRegistries<'w> {
    craft_bookings: ResMut<'w, crate::craft_stations::CraftBookings>,
    interactions: ResMut<'w, crate::facility_interactions::FacilityInteractions>,
    spaces: ResMut<'w, crate::space_identity::SpaceIdentities>,
    spatial_events: ResMut<'w, crate::spatial_events::SpatialJournal>,
    published_spaces: ResMut<'w, crate::live_spaces::PublishedSpaces>,
    craft_order_ids: ResMut<'w, crate::craft_stations::CraftOrderIds>,
    pending_items: ResMut<'w, crate::item_placement::ItemPlacementQueue>,
    placements: ResMut<'w, crate::placements::Placements>,
    mods: ResMut<'w, crate::scripting::ServerMods>,
    preset: Res<'w, crate::scripting::SelectedWorld>,
    contracts: Res<'w, crate::content_contracts::ContentContracts>,
    population: ResMut<'w, crate::npc::PopulationInitialized>,
    blocks: Res<'w, BlockRegistry>,
    items: Res<'w, ItemRegistry>,
    kinds: Res<'w, crate::npc_registry::NpcKindRegistry>,
    needs: Res<'w, crate::npc_registry::NeedRegistry>,
    recipes: Res<'w, RecipeRegistry>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "load_from_save applies one fully preflighted transaction across persisted resources"
)]
fn load_from_save(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut player_states: ResMut<PlayerStates>,
    mut clock: ResMut<WorldClock>,
    mut plans: ResMut<Plans>,
    mut construction: ResMut<crate::construction::ConstructionLedger>,
    mut stations: ResMut<CraftStations>,
    mut containers: ResMut<crate::containers::Containers>,
    mut storage_zones: ResMut<crate::storage::StorageZones>,
    mut guard: ResMut<SaveWriteGuard>,
    mut npc_ids: ResMut<crate::npc::NpcIdAllocator>,
    config: Option<Res<ServerSaveConfig>>,
    mut registries: SaveRegistries,
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
            "load save {name:?} failed: {e}; aborting runtime startup. \
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
    let Some(_saved_world) = save.world.as_ref() else {
        fail(
            &SaveError::Corrupt {
                name: name.into(),
                reason: "missing world preset".into(),
            },
            &mut guard,
        );
        return;
    };
    save = match crate::save_upgrade::migrate(
        &save,
        &registries.contracts,
        &registries.blocks,
        &registries.preset.0,
    ) {
        Ok(staged) => staged,
        Err(reason) => {
            fail(
                &SaveError::Corrupt {
                    name: name.into(),
                    reason,
                },
                &mut guard,
            );
            return;
        }
    };
    // Saved chunk grids + Build plans store raw slots; rewrite them
    // through the save's own slot table into the live registry before
    // anything downstream reads a slot. Refuses (load fails, guard
    // arms) if the save references a block id that's no longer
    // registered — loading would silently transmute those cells.
    let lookup = |id: &str| {
        registries
            .blocks
            .slot_of(&block_junk_mod_api::blocks::BlockId::new(id))
    };
    if let Err(e) = remap_block_slots(&mut save, lookup) {
        fail(&e, &mut guard);
        return;
    }
    if let Err(e) = validate_runtime_save_references(
        name,
        &save,
        &registries.blocks,
        &registries.items,
        &registries.recipes,
        &registries.kinds,
        &registries.needs,
    ) {
        fail(&e, &mut guard);
        return;
    }
    if let Err(reason) = crate::action_jobs::validate_owners(&save, &registries.mods.0)
        .and_then(|()| {
            registries
                .mods
                .0
                .restore_events(&save.world.as_ref().expect("validated world").events)
        })
        .and_then(|()| registries.mods.0.restore_state(&save.mod_state))
    {
        fail(
            &SaveError::Corrupt {
                name: name.into(),
                reason,
            },
            &mut guard,
        );
        return;
    }
    let mut placements = save
        .world
        .as_ref()
        .expect("validated world")
        .placements
        .clone();
    placements
        .rebuild(&registries.blocks)
        .expect("preflighted placements");
    *registries.placements = placements;
    *registries.craft_order_ids = save.world.as_ref().unwrap().craft_order_ids.clone();
    *npc_ids = save.world.as_ref().unwrap().npc_ids.clone();
    registries.craft_bookings.jobs = save.world.as_ref().unwrap().craft_jobs.clone();
    registries.craft_bookings.jobs.suspend_for_load();
    *registries.interactions = save.world.as_ref().unwrap().interactions.clone();
    *registries.spaces = save.world.as_ref().unwrap().spaces.clone();
    *registries.spatial_events = save.world.as_ref().unwrap().spatial_events.clone();
    registries.published_spaces.revision = registries.spatial_events.revision;
    registries.interactions.suspend_for_load();
    *registries.pending_items = save.world.as_ref().unwrap().pending_items.clone();
    registries.population.0 = save.world.as_ref().unwrap().population_initialized;
    let item_registry = &registries.items;
    let kind_registry = &registries.kinds;
    info!(
        "loading {} edited chunks + {} NPCs + {} plans + {} world items from save {name:?}",
        save.edited_chunks.len(),
        save.npcs.len(),
        save.plans.len(),
        save.world_items.len(),
    );
    // Storage zones are plain coords — no slot remap, straight restore.
    if !save.storage_cells.is_empty() {
        storage_zones.restore_all(save.storage_cells.iter().copied());
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
                    crate::protocol::PlanState {
                        kind: saved.kind, materials, project_id: saved.project_id,
                        revision: saved.revision, footprint: saved.footprint, progress_secs: saved.progress_secs,
                    },
                )
            })
            .collect();
        plans.restore_all(restored);
    }
    construction.jobs = save.construction.jobs.clone();
    construction.jobs.suspend_for_load();
    construction.next_project_id = save.construction.next_project_id;
    construction.pending_refunds = save
        .construction
        .pending_refunds
        .iter()
        .map(|refund| crate::construction::PendingRefund {
            project_id: refund.project_id,
            origin: refund.origin,
            items: refund
                .items
                .iter()
                .map(|item| {
                    (
                        item_registry
                            .slot_of(&block_junk_mod_api::items::ItemId::new(
                                item.item_id.clone(),
                            ))
                            .expect("preflighted refund item"),
                        item.count,
                    )
                })
                .collect(),
        })
        .collect();
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
                        id: o.id,
                        revision: o.revision,
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
                    order_id: aw.order_id,
                    committed_inputs: aw
                        .committed_inputs
                        .into_iter()
                        .map(|entry| {
                            (
                                item_registry
                                    .slot_of(&block_junk_mod_api::items::ItemId::new(entry.item_id))
                                    .expect("validated saved commitment"),
                                entry.count,
                            )
                        })
                        .collect(),
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
        stations.restore_all(restored);
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
                        Some(slot) => {
                            debug_assert!(state.deposit(slot, entry.count));
                        }
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
        containers.restore_all(restored);
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

    for SavedChunk {
        coord,
        chunk,
        entities: _,
    } in save.edited_chunks
    {
        let entities = registries.placements.sidecar_for(coord, &registries.blocks);
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
    for npc in save.npcs {
        // Keep the runtime allocator ahead of every persisted id so a
        // post-load spawn can't collide (claim tables key on NpcId).

        let exit = save
            .world
            .as_ref()
            .unwrap()
            .interactions
            .loaded_occupancy(npc.id);
        let entity = spawn_loaded_npc(&mut commands, npc, kind_registry, item_registry);
        if let Some(exit) = exit {
            commands
                .entity(entity)
                .insert((exit, crate::protocol::KinematicLock));
        }
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
            crate::spatial::ordinary_spatial_replica(crate::spatial::SpatialScope::Point(
                translation,
            )),
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
        let state = resolve_saved_player(&saved, item_registry);
        player_states.0.insert(saved.client_id, state);
    }
    if !player_states.0.is_empty() {
        info!(
            "restored {} player state entr{} from save",
            player_states.0.len(),
            if player_states.0.len() == 1 {
                "y"
            } else {
                "ies"
            },
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "save preflight spans every registry-backed persisted subsystem"
)]
pub(crate) fn validate_runtime_save_references(
    name: &str,
    save: &SaveFile,
    blocks: &BlockRegistry,
    items: &ItemRegistry,
    recipes: &RecipeRegistry,
    kinds: &crate::npc_registry::NpcKindRegistry,
    needs: &crate::npc_registry::NeedRegistry,
) -> Result<(), SaveError> {
    let corrupt = |reason: String| SaveError::Corrupt {
        name: name.to_owned(),
        reason,
    };
    let item_slot = |id: &str| {
        items
            .slot_of(&block_junk_mod_api::items::ItemId::new(id))
            .ok_or_else(|| corrupt(format!("unknown item id {id:?}")))
    };
    let recipe = |id: &str| {
        let slot = recipes
            .slot_of(&block_junk_mod_api::recipes::RecipeId::new(id))
            .ok_or_else(|| corrupt(format!("unknown recipe id {id:?}")))?;
        recipes
            .try_id_of(slot)
            .filter(|resolved| resolved.as_str() == id)
            .ok_or_else(|| corrupt(format!("recipe registry mismatch for {id:?}")))?;
        recipes
            .try_def(slot)
            .ok_or_else(|| corrupt(format!("invalid recipe slot {} for {id:?}", slot.0)))
    };

    if let Some(world) = &save.world {
        world.pending_items.validate(items).map_err(&corrupt)?;
        world
            .spatial_events
            .validate(&world.events, &world.spaces)
            .map_err(&corrupt)?;
        world
            .placements
            .validate_saved(save, blocks, &world.preset)
            .map_err(corrupt)?;
    }
    for state in &save.mod_state {
        match &state.scope {
            block_junk_scripting::state::StateScope::World => {}
            block_junk_scripting::state::StateScope::Space(id)
                if save.world.as_ref().is_some_and(|world| {
                    world
                        .spaces
                        .records
                        .get(&crate::space_identity::SpaceId(*id))
                        .is_some_and(|record| !record.resolved_state_owners.contains(&state.mod_id))
                }) => {}
            block_junk_scripting::state::StateScope::Placement(id)
                if save
                    .world
                    .as_ref()
                    .is_some_and(|world| world.placements.contains(*id)) => {}
            block_junk_scripting::state::StateScope::Npc(id)
                if save.npcs.iter().any(|npc| npc.id == *id)
                    || save
                        .world
                        .as_ref()
                        .is_some_and(|world| world.npc_ids.retired.contains(id)) => {}
            _ => {
                return Err(corrupt(format!(
                    "mod {} state references missing scope {:?}",
                    state.mod_id, state.scope
                )));
            }
        }
    }

    for player in &save.players {
        if let Some(carry) = &player.carry {
            item_slot(&carry.item_id)?;
            if carry.count == 0 {
                return Err(corrupt(format!(
                    "player {} has a zero-count carry",
                    player.client_id
                )));
            }
        }
        if let Some(tool) = &player.tool {
            item_slot(&tool.item_id)?;
        }
    }
    for npc in &save.npcs {
        if let Some(id) = npc.home_facility
            && !save
                .world
                .as_ref()
                .is_some_and(|world| world.placements.contains(id))
        {
            return Err(corrupt(format!(
                "NPC {} has an unknown home facility {id}",
                npc.id
            )));
        }
        if kinds.get(&npc.kind).is_none() {
            return Err(corrupt(format!(
                "NPC {} has unknown kind {:?}",
                npc.id, npc.kind
            )));
        }
        for need in npc.needs.keys() {
            if !needs.contains(need) {
                return Err(corrupt(format!("NPC {} has unknown need {need:?}", npc.id)));
            }
        }
        if let Some(carry) = &npc.carrying {
            item_slot(&carry.item_id)?;
            if carry.count == 0 {
                return Err(corrupt(format!("NPC {} has a zero-count carry", npc.id)));
            }
        }
        if let Some(tool) = &npc.tool {
            item_slot(&tool.item_id)?;
        }
    }
    for (cell, plan) in &save.plans {
        if let PlanKind::Build { slot, orientation } = plan.kind {
            let def = blocks
                .try_def(slot)
                .ok_or_else(|| corrupt(format!("plan has invalid block slot {}", slot.0)))?;
            let mut expected = world_footprint(*cell, &def.footprint, orientation);
            let mut saved = plan.footprint.clone();
            expected.sort_by_key(|c| (c.x, c.y, c.z));
            saved.sort_by_key(|c| (c.x, c.y, c.z));
            if saved != expected {
                return Err(corrupt(format!(
                    "saved project footprint at {cell:?} differs from current content"
                )));
            }
        }
        for material in &plan.materials {
            item_slot(&material.item_id)?;
        }
    }
    for world_item in &save.world_items {
        item_slot(&world_item.item_id)?;
    }

    let saved_slot_at = |cell: IVec3| {
        let (coord, local) = world_to_chunk(cell);
        save.edited_chunks
            .iter()
            .find(|chunk| chunk.coord == coord)
            .map(|chunk| chunk.chunk.get(local))
    };
    for refund in &save.construction.pending_refunds {
        for item in &refund.items {
            item_slot(&item.item_id)?;
        }
    }
    let mut craft_ids = std::collections::HashSet::new();
    let next_craft_id = save
        .world
        .as_ref()
        .map_or(1, |world| world.craft_order_ids.next);
    if next_craft_id == 0 {
        return Err(corrupt("invalid craft identity allocator".into()));
    }
    for (cell, station) in &save.craft_stations {
        let slot = saved_slot_at(*cell)
            .ok_or_else(|| corrupt(format!("station state at {cell:?} has no saved chunk")))?;
        let station_def = blocks.try_def(slot).ok_or_else(|| {
            corrupt(format!(
                "station at {cell:?} has invalid block slot {}",
                slot.0
            ))
        })?;
        let station_tag = station_def.station_tag.as_ref().ok_or_else(|| {
            corrupt(format!(
                "station state at {cell:?} is not on a station block"
            ))
        })?;
        for order in &station.orders {
            let def = recipe(&order.recipe_id)?;
            if order.id == 0
                || order.id >= next_craft_id
                || !craft_ids.insert(order.id)
                || order.revision != u64::from(order.completed) + 1
                || &def.station != station_tag
                || order.total == 0
                || order.total > crate::craft_stations::MAX_ORDER_QUANTITY
                || order.completed >= order.total
            {
                return Err(corrupt(format!("invalid craft order at {cell:?}")));
            }
        }
        if let Some(work) = &station.active_work {
            for entry in &work.committed_inputs {
                item_slot(&entry.item_id)?;
            }
            let def = recipe(&work.recipe_id)?;
            if !station
                .orders
                .iter()
                .any(|order| order.id == work.order_id && order.recipe_id == work.recipe_id)
                || &def.station != station_tag
                || !work.total_secs.is_finite()
                || work.total_secs <= 0.0
                || !work.elapsed_secs.is_finite()
                || work.elapsed_secs < 0.0
                || work.elapsed_secs > work.total_secs
            {
                return Err(corrupt(format!("invalid active craft work at {cell:?}")));
            }
        }
        for entry in &station.inventory {
            item_slot(&entry.item_id)?;
        }
    }
    for (cell, container) in &save.containers {
        let slot = saved_slot_at(*cell)
            .ok_or_else(|| corrupt(format!("container state at {cell:?} has no saved chunk")))?;
        let def = blocks.try_def(slot).ok_or_else(|| {
            corrupt(format!(
                "container at {cell:?} has invalid block slot {}",
                slot.0
            ))
        })?;
        if def.container.is_none() {
            return Err(corrupt(format!(
                "container state at {cell:?} is not on a container block"
            )));
        }
        let mut total = 0_u32;
        for entry in &container.inventory {
            item_slot(&entry.item_id)?;
            total = total
                .checked_add(entry.count)
                .ok_or_else(|| corrupt(format!("container inventory overflows at {cell:?}")))?;
        }
    }
    Ok(())
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
) -> Entity {
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
    let mut policy_results = npc.policy_results;
    if let Some((candidate_id, revision)) = npc.active_policy_job {
        policy_results.push(block_junk_mod_api::npcs::PolicyJobResult {
            candidate_id,
            revision,
            state: block_junk_mod_api::npcs::PolicyJobState::Interrupted,
            reason: Some("session reloaded; transient reservation must be revalidated".into()),
        });
    }
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
    commands
        .spawn((
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
                    home_facility: npc
                        .home_facility
                        .map(block_junk_mod_api::architecture::PlacementId),
                    preempt_cooldown_secs: 0.0,
                    policy_results,
                    active_policy_job: None,
                    awaiting_construction: None,
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
            crate::spatial::interpolated_spatial_replica(crate::spatial::SpatialScope::Point(
                npc.pose.translation,
            )),
            Name::new(format!("npc:{}", npc.id)),
        ))
        .id()
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
pub(crate) fn convert_saved_stations(
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
                    id: o.id,
                    revision: o.revision,
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
                order_id: aw.order_id,
                committed_inputs: aw
                    .committed_inputs
                    .iter()
                    .map(|&(slot, count)| SavedStationItem {
                        item_id: item_registry.id_of(slot).to_string(),
                        count,
                    })
                    .collect(),
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
                    project_id: state.project_id,
                    revision: state.revision,
                    footprint: state.footprint,
                    progress_secs: state.progress_secs,
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
            crate::spatial::ordinary_spatial_replica(crate::spatial::SpatialScope::Point(
                translation,
            )),
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
        .map(
            |(id, kind, pose, mode, needs, brain, carry, tool, stats)| SavedNpc {
                home_facility: brain.home_facility.map(|id| id.0),
                id: id.0,
                kind: kind.0.clone(),
                pose: *pose,
                movement_mode: *mode,
                needs: needs.0.clone(),
                rng: brain.rng,
                carrying: saved_carry_of(carry, item_registry),
                tool: saved_tool_of(tool, item_registry),
                stats: stats.0.clone(),
                policy_results: brain.policy_results.clone(),
                active_policy_job: brain.active_policy_job.clone(),
            },
        )
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
/// the two save systems stay under Bevy 0.19's 16-param ceiling (same
/// pattern as `HaulCtx` in npc.rs) and can't drift on what gets
/// persisted.
#[derive(bevy::ecs::system::SystemParam)]
pub struct SaveCtx<'w, 's> {
    craft_bookings: Res<'w, crate::craft_stations::CraftBookings>,
    interactions: Res<'w, crate::facility_interactions::FacilityInteractions>,
    spaces: Res<'w, crate::space_identity::SpaceIdentities>,
    spatial_events: Res<'w, crate::spatial_events::SpatialJournal>,
    npc_ids: Res<'w, crate::npc::NpcIdAllocator>,
    craft_order_ids: Res<'w, crate::craft_stations::CraftOrderIds>,
    pending_items: Res<'w, crate::item_placement::ItemPlacementQueue>,
    placements: Res<'w, crate::placements::Placements>,
    mods: Res<'w, crate::scripting::ServerMods>,
    preset: Res<'w, crate::scripting::SelectedWorld>,
    contracts: Res<'w, crate::content_contracts::ContentContracts>,
    population: Res<'w, crate::npc::PopulationInitialized>,
    construction: Res<'w, crate::construction::ConstructionLedger>,
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
        (
            &'static AvatarPose,
            &'static Carrying,
            &'static EquippedTool,
        ),
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
        mod_state: ctx.mods.0.saved_state(),
        world: Some(crate::save::SavedWorld {
            spaces: ctx.spaces.clone(),
            spatial_events: ctx.spatial_events.clone(),
            events: ctx.mods.0.saved_events(),
            craft_jobs: ctx.craft_bookings.jobs.clone(),
            interactions: ctx.interactions.clone(),
            npc_ids: ctx.npc_ids.clone(),
            craft_order_ids: ctx.craft_order_ids.clone(),
            pending_items: ctx.pending_items.clone(),
            placements: ctx.placements.clone(),
            contracts: ctx.contracts.clone(),
            preset: ctx.preset.0.clone(),
            population_initialized: ctx.population.0,
        }),
        construction: crate::save::SavedConstruction {
            jobs: ctx.construction.jobs.clone(),
            next_project_id: ctx.construction.next_project_id,
            pending_refunds: ctx
                .construction
                .pending_refunds
                .iter()
                .map(|refund| crate::save::SavedRefund {
                    project_id: refund.project_id,
                    origin: refund.origin,
                    items: refund
                        .items
                        .iter()
                        .map(|&(item, count)| SavedStationItem {
                            item_id: ctx.item_registry.id_of(item).to_string(),
                            count,
                        })
                        .collect(),
                })
                .collect(),
        },
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
pub struct SpatialSubscriptions {
    client_to_chunks: HashMap<Entity, HashSet<ChunkCoord>>,
    chunk_to_clients: HashMap<ChunkCoord, HashSet<PeerId>>,
    client_to_peer: HashMap<Entity, PeerId>,
}

#[derive(Resource, Default)]
struct SpatialVisibilityState(HashSet<(Entity, Entity)>);

impl SpatialSubscriptions {
    pub(crate) fn chunks_for(&self, connection: Entity) -> impl Iterator<Item = ChunkCoord> + '_ {
        self.client_to_chunks
            .get(&connection)
            .into_iter()
            .flatten()
            .copied()
    }
    pub(crate) fn requested_chunks(&self) -> impl Iterator<Item = ChunkCoord> + '_ {
        self.chunk_to_clients.keys().copied()
    }
    pub(crate) fn target_for_chunks(
        &self,
        chunks: impl IntoIterator<Item = ChunkCoord>,
    ) -> NetworkTarget {
        let mut peers: HashSet<PeerId> = HashSet::default();
        for chunk in chunks {
            if let Some(subscribers) = self.chunk_to_clients.get(&chunk) {
                peers.extend(subscribers.iter().copied());
            }
        }
        NetworkTarget::Only(peers.into_iter().collect())
    }
    pub(crate) fn target_for_cells(&self, cells: impl IntoIterator<Item = IVec3>) -> NetworkTarget {
        self.target_for_chunks(cells.into_iter().map(|cell| world_to_chunk(cell).0))
    }
    fn remove_client(&mut self, connection: Entity) {
        let peer = self.client_to_peer.remove(&connection);
        if let Some(chunks) = self.client_to_chunks.remove(&connection) {
            for coord in chunks {
                if let Some(clients) = self.chunk_to_clients.get_mut(&coord) {
                    if let Some(peer) = peer {
                        clients.remove(&peer);
                    }
                    if clients.is_empty() {
                        self.chunk_to_clients.remove(&coord);
                    }
                }
            }
        }
    }
}

/// Chunks whose generation is currently in flight on a worker thread.
/// `update_aoi` skips coords already in here so we don't queue duplicate
/// generations; `poll_chunk_gen` drains them as they complete.
#[derive(Resource, Default)]
pub struct PendingChunks(pub HashMap<ChunkCoord, Task<Chunk>>);

/// Simulation demand is independent of connected viewers. This contains only
/// active actor neighborhoods, never a census of generated terrain.
#[derive(Resource, Default)]
pub(crate) struct ActiveSimulationChunks(pub HashSet<ChunkCoord>);

fn request_npc_chunks(
    actors: Query<&AvatarPose, With<crate::npc::Npc>>,
    mut demand: ResMut<ActiveSimulationChunks>,
    map: Res<ChunkMap>,
    mut pending: ResMut<PendingChunks>,
    slots: Res<TerrainSlots>,
) {
    let mut requested = std::collections::BTreeMap::<[i32; 3], i32>::new();
    for pose in &actors {
        if !pose.translation.is_finite() {
            continue;
        }
        let center = world_to_chunk_coord(pose.translation);
        for x in -1..=1 {
            for y in -1..=1 {
                for z in -1..=1 {
                    let distance = x * x + y * y + z * z;
                    let coord = (center.0 + IVec3::new(x, y, z)).to_array();
                    requested
                        .entry(coord)
                        .and_modify(|old| *old = (*old).min(distance))
                        .or_insert(distance);
                }
            }
        }
    }
    demand.0 = requested
        .keys()
        .map(|coord| ChunkCoord(IVec3::from_array(*coord)))
        .collect();
    let mut requested: Vec<_> = requested.into_iter().collect();
    requested.sort_by_key(|(coord, distance)| (*distance, *coord));
    let mut remaining = 4usize;
    for (coord, _) in requested {
        if remaining == 0 || pending.0.len() >= 32 {
            break;
        }
        let coord = ChunkCoord(IVec3::from_array(coord));
        if map.0.contains_key(&coord) || pending.0.contains_key(&coord) {
            continue;
        }
        let slots = *slots;
        pending.0.insert(
            coord,
            AsyncComputeTaskPool::get().spawn(async move { Chunk::from_terrain(coord, &slots) }),
        );
        remaining -= 1;
    }
}

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
fn install_replication_sender(trigger: On<Add, GameReady>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(ReplicationSender);
}

const READY_CHALLENGE_TTL: Duration = Duration::from_secs(10);

#[derive(Component)]
struct PendingReadyChallenge {
    nonce: [u8; 32],
    manifest_hash: [u8; 32],
    spatial_registry_fingerprint: [u8; 32],
    issued_at: Instant,
}

#[derive(bevy::ecs::system::SystemParam)]
struct HelloContext<'w, 's> {
    preset: Res<'w, crate::scripting::SelectedWorld>,
    content: Res<'w, crate::scripting::ContentFingerprint>,
    blocks: Res<'w, BlockRegistry>,
    items: Res<'w, ItemRegistry>,
    recipes: Res<'w, crate::recipes::RecipeRegistry>,
    npc_kinds: Res<'w, crate::npc_registry::NpcKindRegistry>,
    rules: Res<'w, crate::world_rules::WorldRules>,
    spatial_registry: Res<'w, crate::spatial::SpatialDatasetRegistry>,
    senders: Query<'w, 's, &'static mut MessageSender<ServerHello>>,
}

/// A raw transport connection receives exactly one compatibility/identity
/// challenge. All gameplay bootstrap observes `GameReady` instead.
fn send_server_hello(trigger: On<Add, Connected>, mut commands: Commands, context: HelloContext) {
    let connection = trigger.entity;
    let HelloContext {
        preset,
        content,
        blocks,
        items,
        recipes,
        npc_kinds,
        rules,
        spatial_registry,
        mut senders,
    } = context;
    let manifest =
        crate::modset::local_manifest(&blocks, &items, &recipes, &npc_kinds, &rules, &content);
    let hash = crate::protocol::manifest_hash(&manifest);
    let spatial_registry_fingerprint = spatial_registry.fingerprint();
    let mut nonce = [0u8; 32];
    if let Err(error) = getrandom::fill(&mut nonce) {
        error!("cannot create client-ready challenge: {error}");
        commands.trigger(Disconnect { entity: connection });
        return;
    }
    let Ok(mut sender) = senders.get_mut(connection) else {
        warn!("connected peer has no ServerHello sender");
        commands.trigger(Disconnect { entity: connection });
        return;
    };
    sender.send::<WorldChannel>(ServerHello {
        world_preset: preset.0.clone(),
        protocol_id: crate::network::NETCODE_PROTOCOL_ID,
        connection_nonce: nonce,
        manifest_hash: hash,
        spatial_registry_fingerprint,
        manifest,
    });
    commands.entity(connection).insert(PendingReadyChallenge {
        nonce,
        manifest_hash: hash,
        spatial_registry_fingerprint,
        issued_at: Instant::now(),
    });
}

fn verify_client_ready(
    mut commands: Commands,
    credentials: Res<crate::network::ServerCredentials>,
    mut connections: Query<(
        Entity,
        &RemoteId,
        &PendingReadyChallenge,
        &mut MessageReceiver<ClientReady>,
    )>,
) {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    for (connection, remote, challenge, mut receiver) in connections.iter_mut() {
        let messages: Vec<ClientReady> = receiver.receive().collect();
        if let Some(message) = messages.into_iter().next() {
            let valid = (|| {
                if challenge.issued_at.elapsed() > READY_CHALLENGE_TTL {
                    return false;
                }
                let PeerId::Netcode(remote_id) = remote.0 else {
                    return false;
                };
                let player_id = crate::identity::player_id_from_public_key(&message.public_key);
                if player_id != remote_id || message.signature.len() != 64 {
                    return false;
                }
                let Ok(key) = VerifyingKey::from_bytes(&message.public_key) else {
                    return false;
                };
                let Ok(signature_bytes) = <[u8; 64]>::try_from(&message.signature[..]) else {
                    return false;
                };
                let signature = Signature::from_bytes(&signature_bytes);
                let payload = crate::protocol::ready_payload(
                    crate::network::NETCODE_PROTOCOL_ID,
                    &challenge.nonce,
                    &challenge.manifest_hash,
                    &challenge.spatial_registry_fingerprint,
                );
                key.verify(&payload, &signature).is_ok()
            })();

            if !valid {
                warn!(
                    ?connection,
                    "rejecting invalid or replayed client-ready proof"
                );
                commands.trigger(Disconnect { entity: connection });
                continue;
            }
            let player_id = crate::identity::player_id_from_public_key(&message.public_key);
            let administrator = credentials.administrators.contains(&message.public_key);
            commands
                .entity(connection)
                .remove::<PendingReadyChallenge>();
            commands.entity(connection).insert((
                AuthenticatedPlayer {
                    player_id,
                    public_key: message.public_key,
                    administrator,
                },
                GameReady,
            ));
            info!(player_id, "client identity and content manifest verified");
        }
    }
}

fn expire_ready_challenges(
    mut commands: Commands,
    challenges: Query<(Entity, &PendingReadyChallenge)>,
) {
    for (connection, challenge) in challenges.iter() {
        if challenge.issued_at.elapsed() > READY_CHALLENGE_TTL {
            warn!(?connection, "client-ready challenge expired");
            commands.trigger(Disconnect { entity: connection });
        }
    }
}

/// On client connect: spawn an avatar entity carrying the authoritative
/// Transform for this player, replicated to every *other* connected client.
/// We exclude the owner so their own camera Transform isn't periodically
/// overwritten with a stale server copy of itself.
///
/// The avatar starts at the origin so AoI can begin streaming chunks before
/// the first `PlayerPosition` message lands; without that the new client
/// sees nothing for ~100 ms.
#[derive(bevy::ecs::system::SystemParam)]
struct ClientRegistration<'w, 's> {
    preset: Res<'w, crate::scripting::SelectedWorld>,
    remote_ids: Query<'w, 's, &'static RemoteId>,
    avatars: ResMut<'w, ClientAvatars>,
    sent: ResMut<'w, SpatialSubscriptions>,
    player_states: ResMut<'w, PlayerStates>,
}

fn register_new_client(
    trigger: On<Add, GameReady>,
    mut commands: Commands,
    registration: ClientRegistration,
) {
    let ClientRegistration {
        preset,
        remote_ids,
        mut avatars,
        mut sent,
        mut player_states,
    } = registration;
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
    //   2. Fresh defaults: spawn above the sine-wave terrain (peaks
    //      ~y=16) so the first physics tick lands the player on the
    //      surface, empty carry, starter axe. Eye height =
    //      AvatarPose.translation by convention.
    // Entries are removed on claim: while connected, the live avatar is
    // authoritative and the disconnect observer writes it back.
    let persisted = client_id_u64(remote).and_then(|id| player_states.0.remove(&id));
    let (spawn_pose, spawn_carry, spawn_tool) = match persisted {
        Some(state) => (state.pose, state.carry, state.tool),
        None => {
            // Brand-new identity spawns empty-handed: the day-one loop
            // is bare-hands chopping (soft tool gates) into workbench
            // tool crafting, not a granted starter axe.
            (
                AvatarPose {
                    translation: Vec3::from_array(preset.0.player_spawn),
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
            crate::spatial::predicted_owner_avatar(
                crate::spatial::SpatialScope::Point(spawn_pose.translation),
                remote.0,
            ),
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
    sent.client_to_chunks.entry(connection).or_default();
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

#[allow(
    clippy::too_many_arguments,
    reason = "disconnect cleanup atomically releases identity, avatar, subscriptions, budgets, and work"
)]
fn forget_disconnected_client(
    trigger: On<Remove, ClientOf>,
    mut commands: Commands,
    mut avatars: ResMut<ClientAvatars>,
    mut sent: ResMut<SpatialSubscriptions>,
    mut work_leases: ResMut<BlockWorkLeases>,
    mut plan_claims: ResMut<crate::plan_claims::PlanClaims>,
    mut snapshot_budgets: ResMut<SnapshotBudgets>,
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
                            crate::spatial::ordinary_spatial_replica(
                                crate::spatial::SpatialScope::Point(translation),
                            ),
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
    sent.remove_client(trigger.entity);
    if let Some(pending_coord) = snapshot_budgets
        .0
        .remove(&trigger.entity)
        .and_then(|budget| budget.pending.map(|pending| pending.coord))
        && let Ok(remote) = remote_ids.get(trigger.entity)
    {
        remove_spatial_subscriber(&mut sent, trigger.entity, remote.0, pending_coord);
    }
    work_leases.release(trigger.entity, &mut plan_claims);
}

/// Drains completed chunk-generation tasks off the AsyncComputeTaskPool,
/// installing the resulting chunks into the world. Runs before `update_aoi`
/// so newly-completed chunks are available to send this same tick.
fn poll_chunk_gen(
    blocks: Res<BlockRegistry>,
    placements: Res<crate::placements::Placements>,
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
                placements.sidecar_for(coord, &blocks),
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
///
/// Chunks no longer in AoI receive an ordered `LeaveChunk` fact.
///
/// Master chunk records in `ChunkMap` are NOT evicted when no client needs
/// them — that's deferred to a later stage with the "edited?" tracking, so
/// we don't lose player edits when the last viewer wanders off.
#[derive(bevy::ecs::system::SystemParam)]
struct AoiContext<'w, 's> {
    chunk_map: Res<'w, ChunkMap>,
    chunks: Query<'w, 's, (&'static Chunk, &'static ChunkEntities, Has<ChunkEdited>)>,
    pending: ResMut<'w, PendingChunks>,
    avatars: Res<'w, ClientAvatars>,
    poses: Query<'w, 's, &'static AvatarPose>,
    sent: ResMut<'w, SpatialSubscriptions>,
    remote_ids: Query<'w, 's, &'static RemoteId>,
    spatial: Query<'w, 's, &'static mut MessageSender<crate::protocol::SpatialMessage>>,
    terrain_slots: Res<'w, TerrainSlots>,
    budgets: ResMut<'w, SnapshotBudgets>,
    snapshots: Res<'w, crate::spatial::SpatialSnapshotCache>,
}

fn update_aoi(context: AoiContext) {
    let AoiContext {
        chunk_map,
        chunks,
        mut pending,
        avatars,
        poses,
        mut sent,
        mut spatial,
        terrain_slots,
        remote_ids,
        mut budgets,
        snapshots,
    } = context;
    for (&client_entity, &avatar_entity) in avatars.0.iter() {
        let Ok(avatar_pose) = poses.get(avatar_entity) else {
            continue;
        };
        let player_chunk = world_to_chunk_coord(avatar_pose.translation);
        let desired = aoi_around(player_chunk);
        let Ok(remote) = remote_ids.get(client_entity) else {
            continue;
        };
        sent.client_to_peer.insert(client_entity, remote.0);
        let (mut candidates, removed) = {
            let current = sent.client_to_chunks.entry(client_entity).or_default();
            (
                desired.difference(current).copied().collect::<Vec<_>>(),
                current.difference(&desired).copied().collect::<Vec<_>>(),
            )
        };
        candidates.sort_by_key(|coord| (coord.0 - player_chunk.0).length_squared());

        let budget = budgets.0.entry(client_entity).or_default();
        let now = Instant::now();
        let elapsed = now.duration_since(budget.updated_at).as_secs_f32();
        budget.updated_at = now;
        budget.bytes = (budget.bytes + elapsed * crate::spatial::SPATIAL_BYTES_PER_SECOND as f32)
            .min(crate::spatial::SPATIAL_BURST_BYTES as f32);

        // Leaving an already-committed chunk is independent of a staged entry.
        // Process it first so a large incoming snapshot can never delay unload.
        for coord in &removed {
            if let Ok(mut sender) = spatial.get_mut(client_entity) {
                sender.send::<crate::protocol::SpatialChannel>(
                    crate::protocol::SpatialMessage::LeaveChunk { coord: *coord },
                );
            }
            remove_spatial_subscriber(&mut sent, client_entity, remote.0, *coord);
        }

        if budget
            .pending
            .as_ref()
            .is_some_and(|outgoing| !desired.contains(&outgoing.coord))
        {
            let coord = budget.pending.take().expect("pending was checked").coord;
            if let Ok(mut sender) = spatial.get_mut(client_entity) {
                sender.send::<crate::protocol::SpatialChannel>(
                    crate::protocol::SpatialMessage::LeaveChunk { coord },
                );
            }
            remove_spatial_subscriber(&mut sent, client_entity, remote.0, coord);
        }
        if budget.pending.is_some() {
            transmit_pending_chunk(budget, client_entity, remote.0, &mut spatial, &mut sent);
            if budget.pending.is_some() {
                continue;
            }
        }
        candidates.retain(|coord| {
            !sent
                .client_to_chunks
                .get(&client_entity)
                .is_some_and(|current| current.contains(coord))
        });

        for coord in &candidates {
            // Resolve the chunk's wire payload. Three states:
            //   - server has the chunk, edited: send the bytes + sidecar
            //   - server has the chunk, never edited: send Procedural (tiny)
            //   - server doesn't have the chunk yet: queue async gen and skip
            let payload: Option<(ChunkData, Vec<crate::voxel::EntityEntry>, u64)> =
                if let Some(&entity) = chunk_map.0.get(coord) {
                    chunks.get(entity).ok().map(|(chunk, entities, edited)| {
                        let data = if edited {
                            encode_chunk_data(&chunk.blocks)
                        } else {
                            ChunkData::Procedural
                        };
                        // Procedural chunks have no entities by construction, but
                        // ship the sidecar regardless — empty in that case, so
                        // the wire cost is one varint.
                        (data, entities.entries.clone(), chunk.revision)
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

            let Some((data, entities, revision)) = payload else {
                continue; // still generating; try again next tick
            };
            let transaction = spatial_transaction_id(client_entity, *coord);
            let mut messages = vec![crate::protocol::SpatialMessage::BeginChunk {
                revision,
                transaction,
                coord: *coord,
                terrain: data,
                entities: crate::protocol::BoundedVec::new(entities)
                    .expect("a chunk sidecar cannot exceed the padded cell count"),
            }];
            append_partition_pages(&snapshots, *coord, transaction, &mut messages);
            messages.push(crate::protocol::SpatialMessage::CommitChunk {
                transaction,
                coord: *coord,
            });
            let Some(messages) = messages
                .into_iter()
                .map(|message| {
                    let size = bincode::serde::encode_to_vec(&message, bincode::config::standard())
                        .ok()?
                        .len();
                    // The token bucket can never hold more than one burst. A
                    // message above that ceiling would otherwise remain at the
                    // head of the transaction forever.
                    (size <= crate::spatial::SPATIAL_BURST_BYTES).then_some((message, size))
                })
                .collect::<Option<Vec<_>>>()
            else {
                warn!(coord = ?coord.0, "spatial transaction contains an oversized message");
                continue;
            };
            budget.pending = Some(PendingChunkTransmission {
                coord: *coord,
                messages,
                next: 0,
            });
            transmit_pending_chunk(budget, client_entity, remote.0, &mut spatial, &mut sent);
            if budget.pending.is_some() {
                break;
            }
        }
    }
}

fn transmit_pending_chunk(
    budget: &mut SnapshotBudget,
    connection: Entity,
    peer: PeerId,
    senders: &mut Query<&mut MessageSender<crate::protocol::SpatialMessage>>,
    subscriptions: &mut SpatialSubscriptions,
) {
    let Some(outgoing) = budget.pending.as_mut() else {
        return;
    };
    let Ok(mut sender) = senders.get_mut(connection) else {
        return;
    };
    while let Some((message, size)) = outgoing.messages.get(outgoing.next) {
        if *size as f32 > budget.bytes {
            break;
        }
        let began = matches!(message, crate::protocol::SpatialMessage::BeginChunk { .. });
        let committed = matches!(message, crate::protocol::SpatialMessage::CommitChunk { .. });
        sender.send::<crate::protocol::SpatialChannel>(message.clone());
        budget.bytes -= *size as f32;
        outgoing.next += 1;
        if began {
            subscriptions
                .chunk_to_clients
                .entry(outgoing.coord)
                .or_default()
                .insert(peer);
        }
        if committed {
            subscriptions
                .client_to_chunks
                .entry(connection)
                .or_default()
                .insert(outgoing.coord);
        }
    }
    if outgoing.next == outgoing.messages.len() {
        budget.pending = None;
    }
}

fn remove_spatial_subscriber(
    subscriptions: &mut SpatialSubscriptions,
    connection: Entity,
    peer: PeerId,
    coord: ChunkCoord,
) {
    if let Some(current) = subscriptions.client_to_chunks.get_mut(&connection) {
        current.remove(&coord);
    }
    if let Some(clients) = subscriptions.chunk_to_clients.get_mut(&coord) {
        clients.remove(&peer);
        if clients.is_empty() {
            subscriptions.chunk_to_clients.remove(&coord);
        }
    }
}

fn spatial_transaction_id(connection: Entity, coord: ChunkCoord) -> u64 {
    let bits = connection.to_bits();
    let c = coord.0;
    bits.rotate_left(17)
        ^ (c.x as u32 as u64)
        ^ ((c.y as u32 as u64) << 21)
        ^ ((c.z as u32 as u64) << 42)
}

fn append_partition_pages(
    snapshots: &crate::spatial::SpatialSnapshotCache,
    coord: ChunkCoord,
    transaction: u64,
    messages: &mut Vec<crate::protocol::SpatialMessage>,
) {
    messages.extend(snapshots.pages(coord).filter_map(|page| {
        crate::protocol::BoundedVec::new(page.payload)
            .ok()
            .map(|payload| crate::protocol::SpatialMessage::PartitionPage {
                transaction,
                dataset: page.dataset,
                schema_fingerprint: page.schema_fingerprint,
                payload,
            })
    }));
}

fn encode_chunk_data(blocks: &[BlockSlot]) -> ChunkData {
    let mut runs: Vec<crate::protocol::BlockRun> = Vec::new();
    for &slot in blocks {
        if let Some(last) = runs.last_mut()
            && last.slot == slot
        {
            last.count += 1;
        } else {
            runs.push(crate::protocol::BlockRun { slot, count: 1 });
        }
    }
    let raw = crate::protocol::BoundedVec::new(blocks.to_vec())
        .expect("server chunks always have the exact padded cell count");
    let rle = crate::protocol::BoundedVec::new(runs)
        .expect("RLE cannot have more runs than source cells");
    let raw_size = bincode::serde::encode_to_vec(&raw, bincode::config::standard())
        .map_or(usize::MAX, |bytes| bytes.len());
    let rle_size = bincode::serde::encode_to_vec(&rle, bincode::config::standard())
        .map_or(usize::MAX, |bytes| bytes.len());
    if rle_size < raw_size {
        ChunkData::Rle(rle)
    } else {
        ChunkData::Raw(raw)
    }
}

fn update_spatial_visibility(
    mut commands: Commands,
    subscriptions: Res<SpatialSubscriptions>,
    replicas: Query<(Entity, &crate::spatial::SpatialScope), With<crate::spatial::SpatialReplica>>,
    mut state: ResMut<SpatialVisibilityState>,
) {
    let mut desired = HashSet::default();
    for (&connection, chunks) in &subscriptions.client_to_chunks {
        for (entity, scope) in &replicas {
            if scope.intersects(chunks) {
                desired.insert((entity, connection));
            }
        }
    }
    for &(entity, connection) in desired.difference(&state.0) {
        crate::spatial::gain_replica_visibility(&mut commands, entity, connection);
    }
    for &(entity, connection) in state.0.difference(&desired) {
        crate::spatial::lose_replica_visibility(&mut commands, entity, connection);
    }
    state.0 = desired;
}

type ActorScopeQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static AvatarPose,
        &'static mut crate::spatial::SpatialScope,
    ),
    (
        Changed<AvatarPose>,
        With<Actor>,
        Without<WorldItem>,
        Without<crate::civilization::ClusterBboxReplica>,
    ),
>;
type ItemScopeQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static WorldItem,
        &'static mut crate::spatial::SpatialScope,
    ),
    (
        Changed<WorldItem>,
        Without<Actor>,
        Without<crate::civilization::ClusterBboxReplica>,
    ),
>;
type ClusterScopeQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static crate::civilization::ClusterBboxReplica,
        &'static mut crate::spatial::SpatialScope,
    ),
    (
        Changed<crate::civilization::ClusterBboxReplica>,
        Without<Actor>,
        Without<WorldItem>,
    ),
>;

fn refresh_spatial_scopes(
    mut actors: ActorScopeQuery,
    mut items: ItemScopeQuery,
    mut clusters: ClusterScopeQuery,
) {
    for (pose, mut scope) in &mut actors {
        *scope = crate::spatial::SpatialScope::Point(pose.translation);
    }
    for (item, mut scope) in &mut items {
        *scope = crate::spatial::SpatialScope::Point(item.translation);
    }
    for (bounds, mut scope) in &mut clusters {
        *scope = crate::spatial::SpatialScope::Bounds {
            min: bounds.min.as_vec3(),
            max: (bounds.max + IVec3::ONE).as_vec3(),
        };
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
    time: Res<Time>,
    mut receivers: Query<(Entity, &mut MessageReceiver<BlockWorkIntent>), With<GameReady>>,
    mut leases: ResMut<BlockWorkLeases>,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
    mut project: ProjectExecution,
    avatars: Res<ClientAvatars>,
    actors: Query<(&AvatarPose, &EquippedTool), With<Avatar>>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut bus: MessageWriter<CellEdit>,
    mut validation: ValidatedRequestContext,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        for intent in receiver.receive() {
            let Some(auth) = validation.authorize(connection, RequestClass::Ordinary) else {
                continue;
            };
            let previous = leases.0.get(&connection).cloned();
            if previous
                .as_ref()
                .is_some_and(|lease| intent.sequence <= lease.sequence)
            {
                continue;
            }
            match intent.target {
                Some(target) => {
                    let project_token = match target {
                        BlockWorkTarget::Plan { cell } => {
                            project.plans.get(cell).map(|p| p.token())
                        }
                        BlockWorkTarget::Break { .. } => None,
                    };
                    let progress = previous
                        .as_ref()
                        .filter(|lease| {
                            lease.target == target && lease.project_token == project_token
                        })
                        .map_or(0.0, |lease| lease.progress_secs);
                    let reservation = previous
                        .as_ref()
                        .filter(|lease| {
                            lease.target == target && lease.project_token == project_token
                        })
                        .and_then(|lease| lease.reservation.clone());
                    if reservation.is_none() {
                        leases.release(connection, &mut project.claims);
                    }
                    leases.0.insert(
                        connection,
                        BlockWorkLease {
                            owner: block_junk_mod_api::authority::ReservationOwner::Player {
                                id: auth.player_id,
                            },
                            reservation,
                            project_token,
                            sequence: intent.sequence,
                            target,
                            refreshed_at: Instant::now(),
                            progress_secs: progress,
                        },
                    );
                }
                None => {
                    leases.release(connection, &mut project.claims);
                }
            }
        }
    }

    let mut completed = Vec::new();
    let mut invalid = Vec::new();
    let mut worked_projects = std::collections::HashSet::new();
    let mut connections: Vec<_> = leases
        .0
        .iter()
        .map(|(connection, lease)| (*connection, lease.owner.clone()))
        .collect();
    connections.sort_by(|a, b| a.1.cmp(&b.1));
    for (connection, _) in connections {
        let lease = leases.0.get_mut(&connection).expect("collected work lease");
        if lease.refreshed_at.elapsed() > Duration::from_millis(500) {
            invalid.push(connection);
            continue;
        }
        let Some(&avatar) = avatars.0.get(&connection) else {
            invalid.push(connection);
            continue;
        };
        let Ok((pose, tool)) = actors.get(avatar) else {
            invalid.push(connection);
            continue;
        };
        let (cell, edit, work_slot, verb) = match lease.target {
            BlockWorkTarget::Plan { cell } => {
                let Some(state) = project.plans.get(cell) else {
                    invalid.push(connection);
                    continue;
                };
                if !state.is_satisfied()
                    || Some(state.token()) != lease.project_token
                    || lease
                        .reservation
                        .as_ref()
                        .is_some_and(|reservation| !project.claims.matches(cell, reservation))
                {
                    invalid.push(connection);
                    continue;
                }
                match state.kind {
                    PlanKind::Build { slot, orientation } => (
                        cell,
                        BlockEdit {
                            anchor: cell,
                            slot,
                            orientation,
                        },
                        slot,
                        block_junk_mod_api::blocks::WorkVerb::Build,
                    ),
                    PlanKind::Remove => {
                        let Some(slot) = server_slot_at(cell, &chunks, &map) else {
                            invalid.push(connection);
                            continue;
                        };
                        (
                            cell,
                            BlockEdit {
                                anchor: cell,
                                slot: BlockSlot::EMPTY,
                                orientation: Cardinal::default(),
                            },
                            slot,
                            block_junk_mod_api::blocks::WorkVerb::Destroy,
                        )
                    }
                }
            }
            BlockWorkTarget::Break { cell } => {
                let Some(slot) = server_slot_at(cell, &chunks, &map) else {
                    invalid.push(connection);
                    continue;
                };
                (
                    cell,
                    BlockEdit {
                        anchor: cell,
                        slot: BlockSlot::EMPTY,
                        orientation: Cardinal::default(),
                    },
                    slot,
                    block_junk_mod_api::blocks::WorkVerb::Destroy,
                )
            }
        };
        if !within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), INTERACT_REACH) {
            send_rejection(&mut rejections, connection, cell, RejectReason::OutOfReach);
            invalid.push(connection);
            continue;
        }
        let Some(def) = registry.try_def(work_slot) else {
            invalid.push(connection);
            continue;
        };
        let has_tool = def
            .work_action
            .as_ref()
            .and_then(|work| work.tool_for(verb))
            .is_none_or(|tag| items.tool_has_tag(tool.item, tag));
        let multiplier = def
            .work_action
            .as_ref()
            .and_then(|work| work.duration_multiplier(verb, has_tool))
            .or_else(|| def.work_action.is_none().then_some(1.0));
        let Some(multiplier) = multiplier else {
            invalid.push(connection);
            continue;
        };
        let duration = def
            .work_action
            .as_ref()
            .map_or(crate::client::PLAYER_ACTION_DURATION_SECS, |work| {
                work.duration_secs
            })
            * multiplier;
        if let BlockWorkTarget::Plan { cell } = lease.target {
            if lease.reservation.is_none() {
                let Some(reservation) = project.claims.claim_owned(cell, lease.owner.clone())
                else {
                    invalid.push(connection);
                    continue;
                };
                lease.reservation = Some(reservation);
            }
            if !worked_projects.insert(cell) {
                continue;
            }
            lease.progress_secs = project
                .plans
                .edit(&cell, 0, |p| {
                    p.progress_secs = (p.progress_secs + time.delta_secs()).min(duration);
                    p.progress_secs
                })
                .unwrap_or(0.0);
        } else {
            lease.progress_secs += time.delta_secs();
        }
        if lease.progress_secs >= duration {
            completed.push((connection, edit, lease.project_token));
        }
    }
    for connection in invalid {
        leases.release(connection, &mut project.claims);
    }
    for (connection, edit, token) in completed {
        leases.release(connection, &mut project.claims);
        let mut commit = || {
            apply_block_edit(
                edit,
                &mut commands,
                &mut chunks,
                BlockEditWorld::new(
                    &map,
                    &registry,
                    &mut project.placements,
                    &mut project.facilities,
                ),
                server,
                &mut broadcast,
                &mut bus,
                &validation.subscriptions,
            )
        };
        if let Some(token) = token {
            crate::construction::complete(&mut project.plans, edit.anchor, token, commit);
        } else {
            commit();
        }
    }
}

#[derive(Resource, Default)]
struct BlockWorkLeases(HashMap<Entity, BlockWorkLease>);

#[derive(Clone)]
struct BlockWorkLease {
    owner: block_junk_mod_api::authority::ReservationOwner,
    reservation: Option<crate::reservations::Lease>,
    project_token: Option<(u64, u64)>,
    sequence: u64,
    target: BlockWorkTarget,
    refreshed_at: Instant,
    progress_secs: f32,
}

impl BlockWorkLeases {
    fn release(&mut self, connection: Entity, claims: &mut crate::plan_claims::PlanClaims) {
        if let Some(lease) = self.0.remove(&connection)
            && let BlockWorkTarget::Plan { cell } = lease.target
            && let Some(reservation) = lease.reservation
        {
            claims.release_owned(cell, &reservation);
        }
    }
}

fn server_slot_at(
    cell: IVec3,
    chunks: &Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
) -> Option<BlockSlot> {
    let (coord, local) = world_to_chunk(cell);
    let entity = *map.0.get(&coord)?;
    let (chunk, _) = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    (!slot.is_empty()).then_some(slot)
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
pub(crate) struct BlockEditWorld<'a, 'w> {
    cause: Option<block_junk_scripting::events::EventCause>,
    facilities: &'a mut crate::facility_inventory::FacilityService<'w>,
    map: &'a ChunkMap,
    registry: &'a BlockRegistry,
    placements: &'a mut crate::placements::Placements,
}

impl<'a, 'w> BlockEditWorld<'a, 'w> {
    fn caused_by(mut self, cause: Option<block_junk_scripting::events::EventCause>) -> Self {
        self.cause = cause;
        self
    }
    pub(crate) fn new(
        map: &'a ChunkMap,
        registry: &'a BlockRegistry,
        placements: &'a mut crate::placements::Placements,
        facilities: &'a mut crate::facility_inventory::FacilityService<'w>,
    ) -> Self {
        Self {
            cause: None,
            map,
            registry,
            placements,
            facilities,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared authoritative edit transaction"
)]
pub(crate) fn apply_block_edit(
    edit: BlockEdit,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    world: BlockEditWorld<'_, '_>,
    server: &Server,
    broadcast: &mut ServerMultiMessageSender,
    bus: &mut MessageWriter<CellEdit>,
    subscriptions: &SpatialSubscriptions,
) -> bool {
    let mut transaction = crate::terrain_transaction::TerrainTransaction::default();
    let read = |cell| {
        let (coord, local) = world_to_chunk(cell);
        let entity = *world.map.0.get(&coord)?;
        let (chunk, sidecar) = chunks.get(entity).ok()?;
        Some((chunk.get(local), sidecar.get(cell), chunk.revision))
    };
    if transaction.stage(edit, world.registry, &read).is_err() {
        return false;
    }
    let mut placements = world.placements.clone();
    if placements
        .apply_effects(&transaction.effects, world.registry)
        .is_err()
    {
        return false;
    }
    let Ok(change) = world
        .facilities
        .spatial_events
        .prepare(transaction.changes().map(|(cell, _, _)| cell), world.cause)
    else {
        return false;
    };
    world.facilities.spatial_events.commit_change(change);
    world
        .facilities
        .retire(&transaction.effects, world.registry);
    *world.placements = placements;
    commit_terrain_transaction(
        transaction,
        commands,
        chunks,
        world.map,
        server,
        broadcast,
        bus,
        subscriptions,
        world.cause,
    );
    true
}

/// All fallible geometry work has finished; exclusive chunk access prevents
/// changes between validation and commit. Effects publish only after all writes.
#[allow(
    clippy::too_many_arguments,
    reason = "one committed terrain transaction spans geometry and ordered spatial publication"
)]
pub(crate) fn commit_terrain_transaction(
    transaction: crate::terrain_transaction::TerrainTransaction,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    server: &Server,
    broadcast: &mut ServerMultiMessageSender,
    bus: &mut MessageWriter<CellEdit>,
    subscriptions: &SpatialSubscriptions,
    cause: Option<block_junk_scripting::events::EventCause>,
) {
    let effects = commit_terrain_geometry(transaction, commands, chunks, map, bus, cause, true);
    for applied in effects {
        let target = subscriptions.target_for_cells(applied.cells.iter().map(|c| c.world));
        if let Err(error) = send_spatial_terrain_delta(broadcast, server, &target, &applied) {
            warn!(%error,"terrain spatial delta failed");
        }
    }
}

pub(crate) fn commit_terrain_geometry(
    transaction: crate::terrain_transaction::TerrainTransaction,
    commands: &mut Commands,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    bus: &mut MessageWriter<CellEdit>,
    cause: Option<block_junk_scripting::events::EventCause>,
    yield_drops: bool,
) -> Vec<TerrainEditRecord> {
    let mut mirrors = Vec::new();
    for (cell, slot, entry) in transaction.changes() {
        let (coord, local) = world_to_chunk(cell);
        let entity = map.0[&coord];
        let (mut chunk, mut sidecar) = chunks
            .get_mut(entity)
            .expect("exclusive validated terrain chunk");
        chunk.apply_committed_cell(local, slot);
        if let Some(entry) = entry {
            sidecar.insert(cell, entry);
        } else {
            sidecar.remove(cell);
        }
        commands.entity(entity).insert(ChunkEdited);
        mirrors.push((cell, slot));
    }
    crate::voxel::apply_padding_mirrors(&mirrors, map, chunks);
    for (coord, revision) in &transaction.revisions {
        let entity = map.0[&ChunkCoord(IVec3::from_array(*coord))];
        chunks
            .get_mut(entity)
            .expect("validated revision chunk")
            .0
            .revision = *revision;
    }
    for (effect_index, applied) in transaction.effects.iter().enumerate() {
        for cell in applied.cells.iter() {
            bus.write(CellEdit {
                yield_drops: yield_drops && transaction.yields_drops(effect_index),
                cause,
                world: cell.world,
                slot: cell.slot,
                prev_slot: applied.old_slot,
                is_anchor: cell.world == applied.anchor,
            });
        }
    }
    transaction.effects
}

#[derive(Resource, Default)]
pub(crate) struct PendingTerrainEffects(pub Vec<TerrainEditRecord>);

fn flush_mod_terrain_effects(
    mut queue: ResMut<PendingTerrainEffects>,
    servers: Query<&Server>,
    mut broadcast: ServerMultiMessageSender,
    subscriptions: Res<SpatialSubscriptions>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for applied in queue.0.drain(..) {
        let target = subscriptions.target_for_cells(applied.cells.iter().map(|c| c.world));
        if let Err(error) = send_spatial_terrain_delta(&mut broadcast, server, &target, &applied) {
            warn!(%error,"terrain spatial delta failed");
        }
    }
}

fn send_spatial_terrain_delta(
    broadcast: &mut ServerMultiMessageSender,
    server: &Server,
    target: &NetworkTarget,
    edit: &TerrainEditRecord,
) -> Result<(), String> {
    let bytes = bincode::serde::encode_to_vec(edit, bincode::config::standard())
        .expect("bounded terrain edit serializes");
    let payload =
        crate::protocol::BoundedVec::new(bytes).expect("bounded terrain edit fits a spatial page");
    broadcast
        .send::<crate::protocol::SpatialMessage, crate::protocol::SpatialChannel>(
            &crate::protocol::SpatialMessage::Delta {
                dataset: crate::spatial::TERRAIN_DATASET_ID,
                schema_fingerprint: crate::spatial::TERRAIN_SCHEMA_FINGERPRINT,
                payload,
            },
            server,
            target,
        )
        .map_err(|error| error.to_string())
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

type ConstructionWorkers<'w, 's> = Query<
    'w,
    's,
    (
        &'static crate::npc::NpcId,
        &'static AvatarPose,
        &'static EquippedTool,
        Option<&'static mut Brain>,
        Option<&'static mut Needs>,
    ),
    With<crate::npc::Npc>,
>;

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
    mut project: ProjectExecution,
    mut workers: ConstructionWorkers,
    items: Res<ItemRegistry>,
    servers: Query<&Server>,
    mut broadcast: ServerMultiMessageSender,
    mut bus: MessageWriter<CellEdit>,
    subscriptions: Res<SpatialSubscriptions>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for completion in reader.read() {
        let result: Result<(), &str> = (|| {
            // Authoritative half of "cancel cancels": the brain re-checks
            // before emitting, but a plan edit can land between that check
            // and this apply. Never mutate the world for a plan that no
            // longer exists as completed.
            let holder = project.claims.holder(completion.cell);
            if holder != Some(completion.worker)
                || project
                    .plans
                    .get(completion.cell)
                    .is_none_or(|p| p.token() != completion.project_token)
            {
                info!(
                    cell = ?completion.cell.to_array(),
                    "npc work completion against missing/changed plan; skipping edit",
                );
                return Err("project or worker authority changed");
            }
            project.claims.release(completion.cell, completion.worker);
            let Some((_, pose, tool, _, _)) =
                workers.iter().find(|(id, ..)| **id == completion.worker)
            else {
                return Err("construction actor missing");
            };
            if !within_reach(
                pose,
                completion.cell.as_vec3() + Vec3::splat(0.5),
                INTERACT_REACH,
            ) {
                return Err("construction actor out of reach");
            }
            let (slot, verb) = match completion.plan_kind {
                PlanKind::Build { slot, .. } => (slot, block_junk_mod_api::blocks::WorkVerb::Build),
                PlanKind::Remove => {
                    let Some(slot) = server_slot_at(completion.cell, &chunks, &map) else {
                        return Err("construction terrain unavailable");
                    };
                    (slot, block_junk_mod_api::blocks::WorkVerb::Destroy)
                }
            };
            let Some(def) = registry.try_def(slot) else {
                return Err("construction definition missing");
            };
            if let Some(work) = &def.work_action {
                let has_tool = work
                    .tool_for(verb)
                    .is_none_or(|tag| items.tool_has_tag(tool.item, tag));
                if work.duration_multiplier(verb, has_tool).is_none() {
                    return Err("construction tool requirement no longer met");
                }
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
            let cause = project
                .ledger
                .jobs
                .records
                .values()
                .find(|job| {
                    !job.state.terminal()
                        && job.target
                            == (block_junk_mod_api::authority::JobTarget::Construction {
                                project_id: completion.project_token.0,
                                project_revision: completion.project_token.1,
                            })
                })
                .and_then(|job| job.cause);
            let completed = crate::construction::complete(
                &mut project.plans,
                completion.cell,
                completion.project_token,
                || {
                    apply_block_edit(
                        edit,
                        &mut commands,
                        &mut chunks,
                        BlockEditWorld::new(
                            &map,
                            &registry,
                            &mut project.placements,
                            &mut project.facilities,
                        )
                        .caused_by(cause),
                        server,
                        &mut broadcast,
                        &mut bus,
                        &subscriptions,
                    )
                },
            );
            if completed {
                Ok(())
            } else {
                Err("authoritative completion rejected")
            }
        })();
        if project.ledger.jobs.records.values().any(|job| {
            !job.state.terminal()
                && job.actor
                    == block_junk_mod_api::authority::ReservationOwner::Npc {
                        id: completion.worker.0,
                    }
                && job.target
                    == block_junk_mod_api::authority::JobTarget::Construction {
                        project_id: completion.project_token.0,
                        project_revision: completion.project_token.1,
                    }
        }) {
            project.ledger.finish_project(
                completion.project_token,
                result.is_ok(),
                result.err().unwrap_or(""),
            );
        }
        if let Some((_, _, _, brain, needs)) = workers
            .iter_mut()
            .find(|(id, ..)| **id == completion.worker)
        {
            if result.is_ok()
                && let Some(mut needs) = needs
                && let Some(restore) = &completion.need_restore
                && let Some(value) = needs.0.get_mut(&restore.need)
            {
                *value = (*value - restore.restores).max(0.0);
            }
            let Some(mut brain) = brain else {
                continue;
            };
            if brain.awaiting_construction == Some(completion.project_token) {
                brain.awaiting_construction = None;
            }
            if brain
                .active_policy_job
                .as_ref()
                .is_some_and(|(id, revision)| {
                    id == &format!("construction:{}", completion.project_token.0)
                        && *revision == completion.project_token.1
                })
            {
                crate::npc::finish_policy_job(
                    &mut brain,
                    if result.is_ok() {
                        block_junk_mod_api::npcs::PolicyJobState::Completed
                    } else {
                        block_junk_mod_api::npcs::PolicyJobState::Failed
                    },
                    result.err(),
                );
            }
        }
    }
}

/// NPC bush-eat consumer (S4 forage). Translates each
/// A committed facility action can request an in-place swap of the block for
/// its `depleted_block` — *without* spawning drops (the villager ate
/// the berries; harvesting the same bush drops them instead).
///
/// Geometry, escrow and the actor's need change commit together. Failed or stale
/// completions leave the original resource intact and grant no free need recovery.
#[derive(bevy::ecs::system::SystemParam)]
struct NpcConsumptionWorld<'w, 's> {
    homes: Query<'w, 's, (&'static NpcId, &'static mut crate::npc::Brain)>,
    facilities: crate::facility_inventory::FacilityService<'w>,
    chunks: Query<'w, 's, (&'static mut Chunk, &'static mut ChunkEntities)>,
    map: Res<'w, ChunkMap>,
    registry: Res<'w, BlockRegistry>,
    placements: ResMut<'w, crate::placements::Placements>,
    actors: Query<'w, 's, (&'static NpcId, &'static mut Needs, &'static AvatarPose)>,
    construction: crate::construction::ConstructionService<'w>,
    effects: ResMut<'w, PendingTerrainEffects>,
}

#[cfg(test)]
pub(crate) fn install_interaction_fixture(app: &mut App) {
    app.init_resource::<crate::facility_access::FacilityAccess>()
        .add_message::<crate::facility_interactions::InteractionCompleted>()
        .add_systems(
            PreUpdate,
            (
                crate::facility_interactions::reconcile_exits,
                crate::facility_interactions::activate,
            )
                .chain(),
        )
        .add_systems(
            Update,
            (
                crate::facility_interactions::advance,
                apply_facility_completion,
            )
                .chain(),
        )
        .add_systems(PostUpdate, crate::facility_interactions::reconcile_exits);
}

fn apply_facility_completion(
    mut reader: MessageReader<crate::facility_interactions::InteractionCompleted>,
    mut interactions: ResMut<crate::facility_interactions::FacilityInteractions>,
    mut claims: ResMut<crate::interactables::InteractionClaims>,
    mut world: NpcConsumptionWorld,
    mut commands: Commands,
    mut bus: MessageWriter<CellEdit>,
) {
    use block_junk_mod_api::{
        architecture::{FacilityAction, FacilityConsumption},
        authority::{JobState, JobTarget, ReservationOwner},
    };
    for message in reader.read() {
        if !interactions
            .jobs
            .records
            .get(&message.job_id)
            .is_some_and(|job| {
                job.actor
                    == (ReservationOwner::Npc {
                        id: message.actor.0,
                    })
                    && job.state == JobState::Working
                    && interactions
                        .progress
                        .get(&job.id)
                        .is_some_and(|p| p.remaining_secs == 0.)
            })
        {
            continue;
        }
        let outcome = (|| -> Result<(), String> {
            let job = interactions
                .jobs
                .records
                .get(&message.job_id)
                .ok_or("interaction job missing")?
                .clone();
            if job.actor
                != (ReservationOwner::Npc {
                    id: message.actor.0,
                })
                || job.state != JobState::Working
                || !interactions
                    .progress
                    .get(&job.id)
                    .is_some_and(|p| p.remaining_secs == 0.)
            {
                return Err("interaction not ready to complete".into());
            }
            let (use_slot, action) = interactions.validate_target(
                job.id,
                &world.placements,
                &world.registry,
                &claims,
            )?;
            let FacilityAction::Timed {
                need_restore,
                consumption,
                ..
            } = action.clone()
            else {
                return Err("wrong timed action".into());
            };
            let JobTarget::Interaction { slot, .. } = &job.target else {
                return Err("wrong job target".into());
            };
            let placement = &world.placements.records[&slot.placement];
            let anchor = IVec3::from_array(placement.anchor);
            let (_, mut needs, pose) = world
                .actors
                .iter_mut()
                .find(|(id, _, _)| **id == message.actor)
                .ok_or("interaction actor missing")?;
            let approach = IVec3::from_array(interactions.progress[&job.id].approach);
            if !crate::facility_interactions::valid_actor_pose(use_slot, anchor, approach, pose) {
                return Err("interaction actor left its authored use position".into());
            }
            if need_restore
                .as_ref()
                .is_some_and(|restore| !needs.0.contains_key(&restore.need))
            {
                return Err("actor does not have the requested need".into());
            }
            let mut facilities = crate::facility_inventory::FacilityTransaction::default();
            let mut terrain = crate::terrain_transaction::TerrainTransaction::default();
            if let Some(consumption) = consumption {
                match consumption {
                    FacilityConsumption::Stock { count, tag } => {
                        facilities.consume_stock(anchor, count, tag.as_ref(), &world.facilities)?
                    }
                    FacilityConsumption::Deplete => {
                        let base = |cell| {
                            let (coord, local) = world_to_chunk(cell);
                            let entity = world.map.0.get(&coord)?;
                            let (chunk, sidecar) = world.chunks.get(*entity).ok()?;
                            Some((chunk.get(local), sidecar.get(cell), chunk.revision))
                        };
                        terrain.stage(
                            BlockEdit {
                                anchor,
                                slot: BlockSlot::EMPTY,
                                orientation: Cardinal::default(),
                            },
                            &world.registry,
                            &base,
                        )?;
                    }
                }
            }
            if world.effects.0.len() + terrain.effects.len() > 4096 {
                return Err("terrain publication queue full".into());
            }
            let mut placements = world.placements.clone();
            placements.apply_effects(&terrain.effects, &world.registry)?;
            let mut construction = world.construction.stage();
            construction.resolve_terrain(terrain.changes().map(|(cell, _, _)| cell));
            let spatial_change = world
                .facilities
                .spatial_events
                .prepare(terrain.changes().map(|(cell, _, _)| cell), job.cause)?;
            facilities.retire(&terrain.effects, &world.registry, &world.facilities);
            world
                .facilities
                .spatial_events
                .commit_change(spatial_change);
            world.construction.commit(construction);
            world.facilities.commit(facilities);
            *world.placements = placements;
            let effects = commit_terrain_geometry(
                terrain,
                &mut commands,
                &mut world.chunks,
                &world.map,
                &mut bus,
                job.cause,
                false,
            );
            world.effects.0.extend(effects);
            if need_restore
                .as_ref()
                .is_some_and(|restore| restore.need == "sleep")
                && world
                    .placements
                    .records
                    .get(&slot.placement)
                    .is_some_and(|record| record.active)
                && let Some((_, mut brain)) =
                    world.homes.iter_mut().find(|(id, _)| **id == message.actor)
            {
                brain.home_facility = Some(slot.placement);
            }
            if let Some(restore) = need_restore {
                let deficit = needs.0.get_mut(&restore.need).expect("validated need");
                *deficit = (*deficit - restore.restores).max(0.);
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                interactions.finish(message.job_id, JobState::Completed, None, &mut claims);
            }
            Err(reason) => {
                interactions.finish(message.job_id, JobState::Failed, Some(reason), &mut claims);
            }
        }
    }
}

/// Server side of the NPC inspection RPC. Iterates each connection's
/// `RequestNpcDetails` queue, looks up the named NPC's state, and
/// sends a single `NpcDetails` reply over the requesting connection.
/// Targeted (per-connection sender) — other clients don't see this
/// traffic.
type NpcInspectionData<'a> = (
    &'a NpcId,
    &'a NpcKind,
    &'a Needs,
    &'a NpcStats,
    &'a Brain,
    &'a AvatarPose,
);

fn receive_npc_inspection_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<RequestNpcDetails>), With<GameReady>>,
    npcs: Query<NpcInspectionData, With<Npc>>,
    npc_index: Res<crate::npc::NpcEntityIndex>,
    mut senders: Query<&mut MessageSender<NpcDetails>>,
    mut validation: ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<RequestNpcDetails> = receiver.receive().collect();
        for req in requests {
            if validation
                .authorize(connection, RequestClass::NpcInspection)
                .is_none()
            {
                continue;
            }
            let Some(&npc_entity) = npc_index.by_id.get(&NpcId(req.npc_id)) else {
                continue;
            };
            let Ok((id, kind, needs, stats, brain, _pose)) = npcs.get(npc_entity) else {
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
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins validated gameplay state"
)]
fn receive_pickup_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<PickupRequest>), With<GameReady>>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut Carrying, &mut EquippedTool), With<Avatar>>,
    world_items: Query<(Entity, &WorldItem)>,
    item_registry: Res<ItemRegistry>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut commands: Commands,
    mut validation: ValidatedRequestContext,
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
            if validation
                .authorize(connection, RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
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
                let Some((remaining, displaced)) =
                    crate::item_placement::exchange_tool(&mut tool, item_slot, item_count)
                else {
                    continue;
                };
                claimed.insert(entity);
                shrink_or_despawn_stack(
                    &mut commands,
                    entity,
                    item_slot,
                    item_translation,
                    remaining,
                );
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
                        crate::spatial::ordinary_spatial_replica(
                            crate::spatial::SpatialScope::Point(req.target),
                        ),
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

/// Transfer the carried stack to durable item delivery. A blocked destination
/// retains the obligation, and a large stack costs one queued delivery.
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins validated gameplay state"
)]
fn receive_drop_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DropRequest>), With<GameReady>>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut Carrying), With<Avatar>>,
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mut pending_items: ResMut<crate::item_placement::ItemPlacementQueue>,
    item_registry: Res<ItemRegistry>,
    mut validation: ValidatedRequestContext,
) {
    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        sidecars: &sidecars,
        chunk_map: &chunk_map,
        registry: &block_registry,
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let request_count = receiver
            .receive()
            .filter(|_| {
                validation
                    .authorize(connection, RequestClass::Ordinary)
                    .is_some()
            })
            .count();
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
        pending_items.enqueue(
            centre.floor().as_ivec3(),
            item_registry.def(item).id.clone(),
            count,
        );
    }
}

/// Apply a client's tool-drop request. Takes the equipped tool out
/// of the player's `EquippedTool` and spawns a `WorldItem` at the
/// `drop_target_position` (in front of the player, fall back to
/// feet). No-op when the tool slot is empty. Mirrors
/// `receive_drop_requests` (carry) — the only differences are the
/// component touched and the single-unit drop (tools never stack).
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins validated gameplay state"
)]
fn receive_drop_tool_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DropToolRequest>), With<GameReady>>,
    avatars: Res<ClientAvatars>,
    mut players: Query<(&AvatarPose, &mut EquippedTool), With<Avatar>>,
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mut pending_items: ResMut<crate::item_placement::ItemPlacementQueue>,
    item_registry: Res<ItemRegistry>,
    mut validation: ValidatedRequestContext,
) {
    let world = crate::npc::WorldWalk {
        chunks: &chunks,
        sidecars: &sidecars,
        chunk_map: &chunk_map,
        registry: &block_registry,
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let request_count = receiver
            .receive()
            .filter(|_| {
                validation
                    .authorize(connection, RequestClass::Ordinary)
                    .is_some()
            })
            .count();
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
        pending_items.enqueue(
            target.floor().as_ivec3(),
            item_registry.def(slot).id.clone(),
            1,
        );
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
/// the plan; the spatial store journals the new materials. Silent no-op on:
///   - empty carry
///   - no plan at `cell` (was untagged between client click and server receive)
///   - plan isn't Build (Remove plans don't accept materials)
///   - plan doesn't need this item kind, or is already full.
#[derive(bevy::ecs::system::SystemParam)]
struct DepositContext<'w, 's> {
    avatars: Res<'w, ClientAvatars>,
    players: Query<'w, 's, (&'static AvatarPose, &'static mut Carrying), With<Avatar>>,
    plans: ResMut<'w, Plans>,
    item_registry: Res<'w, ItemRegistry>,
    rejections: Query<'w, 's, &'static mut MessageSender<ActionRejected>>,
    toast_senders: Query<'w, 's, &'static mut MessageSender<crate::protocol::SpatialMessage>>,
}

fn receive_deposit_requests(
    mut receivers: Query<(Entity, &mut MessageReceiver<DepositRequest>), With<GameReady>>,
    context: DepositContext,
    mut validation: ValidatedRequestContext,
) {
    let DepositContext {
        avatars,
        mut players,
        mut plans,
        item_registry,
        mut rejections,
        mut toast_senders,
    } = context;
    for (connection, mut receiver) in receivers.iter_mut() {
        let requests: Vec<DepositRequest> = receiver.receive().collect();
        for req in requests {
            if validation
                .authorize(connection, RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
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
            let Some(token) = plans.get(req.cell).map(|project| project.token()) else {
                continue;
            };
            let accepted = crate::construction::transfer_carried_materials(
                &mut plans,
                req.cell,
                token,
                &mut carry,
                carry_count,
            );
            if accepted == 0 {
                continue;
            }
            if let Some(state) = plans.get(req.cell) {
                // Targeted deposit receipt — the depositor sees
                // "Wood Log 2/4" at the plan cell (there's no audio
                // yet, and the ghost re-tint alone is easy to miss).
                if let Some(entry) = state.materials.iter().find(|m| m.item == carry_item)
                    && let Ok(mut sender) = toast_senders.get_mut(connection)
                {
                    let text = format!(
                        "{} {}/{}",
                        item_registry.def(entry.item).display_name,
                        entry.present,
                        entry.needed
                    );
                    if let Ok(text) = crate::protocol::BoundedString::<256>::new(text) {
                        sender.send::<crate::protocol::SpatialChannel>(
                            crate::protocol::SpatialMessage::Toast {
                                cell: req.cell,
                                text,
                            },
                        );
                    }
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
        Goal::SleepingGround { remaining_secs, .. } => (
            format!("sleeping on the ground ({remaining_secs:.1}s)"),
            None,
        ),
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
    mut construction: crate::construction::ConstructionService,
) {
    for edit in reader.read() {
        let affected = construction
            .iter()
            .find(|(anchor, state)| {
                (**anchor == edit.world || state.footprint.contains(&edit.world))
                    && match state.kind {
                        PlanKind::Remove => edit.slot.is_empty(),
                        PlanKind::Build { .. } => !edit.slot.is_empty(),
                    }
            })
            .map(|(anchor, _)| *anchor);
        if let Some(anchor) = affected {
            construction.cancel(anchor);
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

/// Record one durable stack per drop entry and destroyed placement. Delivery is
/// bounded and retries blocked/unloaded destinations instead of burying material.
fn spawn_drops_on_destroy(
    mut reader: MessageReader<CellEdit>,
    mut pending: ResMut<crate::item_placement::ItemPlacementQueue>,
    blocks: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        if !edit.yield_drops
            || !edit.slot.is_empty()
            || edit.prev_slot.is_empty()
            || !edit.is_anchor
        {
            continue;
        }
        for drop in blocks.def(edit.prev_slot).resolved_drops() {
            pending.enqueue(edit.world, drop.item.clone(), drop.count);
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
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
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
        sidecars: &sidecars,
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
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
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
        sidecars: &sidecars,
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

#[derive(bevy::ecs::system::SystemParam)]
struct ProjectExecution<'w> {
    ledger: ResMut<'w, crate::construction::ConstructionLedger>,
    facilities: crate::facility_inventory::FacilityService<'w>,
    plans: ResMut<'w, Plans>,
    claims: ResMut<'w, crate::plan_claims::PlanClaims>,
    placements: ResMut<'w, crate::placements::Placements>,
}

#[cfg(test)]
#[path = "construction_udp.rs"]
mod construction_udp;

#[cfg(test)]
#[path = "foundation_udp.rs"]
mod foundation_udp;

#[cfg(test)]
mod simulation_demand_tests {
    use super::*;
    #[test]
    fn npc_neighborhoods_generate_without_players_and_ignore_unrequested_caves() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        crate::scripting::install_independent_fixture(&mut app, block_junk_mod_api::Side::Server);
        let slots = *app.world().resource::<TerrainSlots>();
        app.init_resource::<ChunkMap>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<PendingChunks>()
            .init_resource::<ActiveSimulationChunks>()
            .add_systems(Update, (request_npc_chunks, poll_chunk_gen).chain());
        let npc = app
            .world_mut()
            .spawn((
                Npc,
                AvatarPose {
                    translation: Vec3::new(96., 12., 96.),
                    yaw: 0.,
                },
            ))
            .id();
        let unrelated = ChunkCoord(IVec3::splat(100));
        let entity = app
            .world_mut()
            .spawn((
                unrelated,
                Chunk::from_terrain(unrelated, &slots),
                ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(unrelated, entity);
        app.update();
        let demand = &app.world().resource::<ActiveSimulationChunks>().0;
        assert_eq!(demand.len(), 27);
        assert!(!demand.contains(&unrelated));
        assert!(app.world().resource::<PendingChunks>().0.len() <= 4);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            app.update();
            let map = &app.world().resource::<ChunkMap>().0;
            if app
                .world()
                .resource::<ActiveSimulationChunks>()
                .0
                .iter()
                .all(|coord| map.contains_key(coord))
            {
                break;
            }
            assert!(Instant::now() < deadline, "NPC geometry failed to converge");
            std::thread::yield_now();
        }
        assert_eq!(app.world().resource::<ChunkMap>().0.len(), 28);
        app.world_mut().despawn(npc);
        app.update();
        assert!(
            app.world()
                .resource::<ActiveSimulationChunks>()
                .0
                .is_empty()
        );
        assert!(app.world().resource::<PendingChunks>().0.is_empty());
    }
}

#[cfg(test)]
mod consumption_transaction_tests {
    use super::*;
    fn complete_intent(
        world: &mut World,
        cell: IVec3,
        actor: u64,
    ) -> crate::facility_interactions::InteractionCompleted {
        let mut jobs = world
            .remove_resource::<crate::facility_interactions::FacilityInteractions>()
            .unwrap();
        let mut claims = world
            .remove_resource::<crate::interactables::InteractionClaims>()
            .unwrap();
        let placement = world
            .resource::<crate::placements::Placements>()
            .at(cell)
            .unwrap();
        let owner = block_junk_mod_api::authority::ReservationOwner::Npc { id: actor };
        let job = jobs
            .prepare(
                crate::facility_interactions::InteractionRequest {
                    placement,
                    slot_key: "fruit",
                    action: "eat",
                    actor: owner.clone(),
                    controller: owner,
                    cause: None,
                    approach: cell + IVec3::Z,
                },
                world.resource::<BlockRegistry>(),
                &mut claims,
            )
            .unwrap();
        jobs.working(job.id);
        jobs.progress.get_mut(&job.id).unwrap().remaining_secs = 0.;
        world.insert_resource(jobs);
        world.insert_resource(claims);
        crate::facility_interactions::InteractionCompleted {
            job_id: job.id,
            actor: NpcId(actor),
        }
    }
    #[test]
    fn competing_uses_of_last_stock_restore_only_the_actor_that_consumes_it() {
        let definitions=serde_json::from_value(serde_json::json!([{"id":"test:basket","display_name":"Basket","flags":{},"color":[1,1,1],"container":{"capacity_bulk":16,"accepts":[]},"facility_slots":[{"key":"fruit","yaw":0,"approaches":[[0,0,1]],"exclusive":false,"interactions":["eat"]}],"facility_actions":{"eat":{"kind":"timed","duration_secs":1,"need_restore":{"need":"hunger","restores":0.5},"consumption":{"kind":"stock","count":1,"tag":"test:food"}}}}])).unwrap();
        let registry = BlockRegistry::build(definitions).unwrap().0;
        let basket = registry.require("test:basket");
        let items=crate::items::ItemRegistry::build(serde_json::from_value(serde_json::json!([{"id":"test:fruit","display_name":"Fruit","mesh":"mods://test/fruit.gltf","tags":["test:food"]}])).unwrap()).unwrap();
        let fruit = items.slot_of(&"test:fruit".into()).unwrap();
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins)
            .insert_resource(registry)
            .insert_resource(items)
            .init_resource::<ChunkMap>()
            .init_resource::<Plans>()
            .init_resource::<crate::construction::ConstructionLedger>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::haul::HaulStore>()
            .init_resource::<PendingTerrainEffects>()
            .add_message::<CellEdit>()
            .add_message::<crate::facility_interactions::InteractionCompleted>()
            .add_systems(Update, apply_facility_completion);
        let cell = IVec3::new(4, 2, 4);
        let mut chunk = Chunk {
            revision: 0,
            blocks: vec![BlockSlot::EMPTY; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
        };
        chunk.set(world_to_chunk(cell).1, basket);
        let mut placements = crate::placements::Placements::default();
        placements
            .index_existing(
                cell,
                basket,
                Cardinal::East,
                app.world().resource::<BlockRegistry>(),
                |at| Some(chunk.get(world_to_chunk(at).1)),
            )
            .unwrap();
        app.insert_resource(placements);
        let entity = app
            .world_mut()
            .spawn((chunk, ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        let actors: Vec<_> = (1..=2)
            .map(|id| {
                app.world_mut()
                    .spawn((
                        NpcId(id),
                        Needs(std::collections::HashMap::from_iter([(
                            "hunger".into(),
                            1.,
                        )])),
                        AvatarPose {
                            translation: crate::physics::standing_pose_translation(cell + IVec3::Z),
                            yaw: 0.,
                        },
                    ))
                    .id()
            })
            .collect();
        app.world_mut()
            .resource_mut::<crate::containers::Containers>()
            .edit_or_insert(cell, |state| {
                state.inventory.insert(fruit, 1);
            });
        let first = complete_intent(app.world_mut(), cell, 1);
        let second = complete_intent(app.world_mut(), cell, 2);
        app.world_mut().write_message(first.clone());
        app.world_mut().write_message(second);
        app.world_mut().write_message(first);
        app.update();
        assert_eq!(
            app.world().get::<Needs>(actors[0]).unwrap().0["hunger"],
            0.5
        );
        assert_eq!(app.world().get::<Needs>(actors[1]).unwrap().0["hunger"], 1.);
        assert_eq!(
            app.world()
                .resource::<crate::containers::Containers>()
                .get(cell)
                .map_or(0, |state| state.available(fruit)),
            0
        );
        let jobs = &app
            .world()
            .resource::<crate::facility_interactions::FacilityInteractions>()
            .jobs;
        assert_eq!(
            jobs.records[&1].state,
            block_junk_mod_api::authority::JobState::Completed
        );
        assert_eq!(
            jobs.records[&2].state,
            block_junk_mod_api::authority::JobState::Failed
        );
        assert!(app.world().resource::<PendingTerrainEffects>().0.is_empty());
    }

    #[test]
    fn consuming_a_resource_commits_need_and_geometry_once_or_neither() {
        let definitions = serde_json::from_value(serde_json::json!([
            {"id":"test:crop","display_name":"Crop","flags":{},"color":[1,1,1],"depleted_block":"test:stalk","facility_slots":[{"key":"fruit","yaw":0,"approaches":[[0,0,1]],"exclusive":false,"interactions":["eat"]}],"facility_actions":{"eat":{"kind":"timed","duration_secs":1,"need_restore":{"need":"hunger","restores":0.5},"consumption":{"kind":"deplete"}}}},
            {"id":"test:stalk","display_name":"Stalk","flags":{},"color":[1,1,1],"footprint":[[0,0,0],[1,0,0]]}
        ])).unwrap();
        let registry = BlockRegistry::build(definitions).unwrap().0;
        let crop = registry.require("test:crop");
        let stalk = registry.require("test:stalk");
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins)
            .insert_resource(registry)
            .init_resource::<ChunkMap>()
            .init_resource::<Plans>()
            .init_resource::<crate::construction::ConstructionLedger>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::haul::HaulStore>()
            .init_resource::<PendingTerrainEffects>()
            .add_message::<CellEdit>()
            .init_resource::<crate::interactables::InteractionClaims>()
            .add_message::<crate::facility_interactions::InteractionCompleted>()
            .add_systems(Update, apply_facility_completion);
        let cell = IVec3::new(4, 2, 4);
        let obstruction = cell + IVec3::from_array(Cardinal::default().rotate_offset([1, 0, 0]));
        let mut chunk = Chunk {
            revision: 0,
            blocks: vec![BlockSlot::EMPTY; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
        };
        chunk.set(world_to_chunk(cell).1, crop);
        chunk.set(world_to_chunk(obstruction).1, crop);
        let entity = app
            .world_mut()
            .spawn((chunk, ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        let actor = app
            .world_mut()
            .spawn((
                NpcId(9),
                Needs(std::collections::HashMap::from_iter([(
                    "hunger".into(),
                    1.,
                )])),
                AvatarPose {
                    translation: crate::physics::standing_pose_translation(cell + IVec3::Z),
                    yaw: 0.,
                },
            ))
            .id();
        let mut placements = crate::placements::Placements::default();
        placements
            .index_existing(
                cell,
                crop,
                Cardinal::default(),
                app.world().resource::<BlockRegistry>(),
                |at| {
                    Some(
                        app.world()
                            .get::<Chunk>(entity)
                            .unwrap()
                            .get(world_to_chunk(at).1),
                    )
                },
            )
            .unwrap();
        app.insert_resource(placements);
        let first = complete_intent(app.world_mut(), cell, 9);
        app.world_mut().write_message(first);
        app.update();
        assert_eq!(app.world().get::<Needs>(actor).unwrap().0["hunger"], 1.);
        assert_eq!(
            app.world()
                .get::<Chunk>(entity)
                .unwrap()
                .get(world_to_chunk(cell).1),
            crop
        );
        assert!(app.world().resource::<PendingTerrainEffects>().0.is_empty());
        app.world_mut()
            .get_mut::<Chunk>(entity)
            .unwrap()
            .set(world_to_chunk(obstruction).1, BlockSlot::EMPTY);
        // Two competing completions against one revision cannot eat twice.
        let second = complete_intent(app.world_mut(), cell, 9);
        app.world_mut().write_message(second.clone());
        app.world_mut().write_message(second);
        app.update();
        assert_eq!(app.world().get::<Needs>(actor).unwrap().0["hunger"], 0.5);
        let chunk = app.world().get::<Chunk>(entity).unwrap();
        assert_eq!(chunk.get(world_to_chunk(cell).1), stalk);
        assert_eq!(chunk.get(world_to_chunk(obstruction).1), stalk);
        assert_eq!(app.world().resource::<PendingTerrainEffects>().0.len(), 2);
        let messages = app.world().resource::<Messages<CellEdit>>();
        let mut cursor = messages.get_cursor();
        assert!(cursor.read(messages).all(|edit| !edit.yield_drops));
    }
}
