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
use crate::collision::WorldCollision;
use crate::menu::{ServerSaveConfig, ServerSaveRequestFlag, ServerShutdownFlag};
use crate::physics::apply_walk_step;
use crate::protocol::{
    Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, BlockManifest,
    CHUNK_PADDED, CellEdit, ChunkCoord, ChunkData, ChunkSnapshot, ChunkUnload, GameSet,
    MovementIntent, MovementMode, WorldChannel, WorldClock, WorldClockSync,
};
use crate::npc::{Brain, Goal, Needs, Npc, NpcId, NpcKind, NpcPath};
use crate::rooms::{DetectionDirty, RoomEventMsg, RoomMap, mark_dirty_from_edits, process_dirty};
use crate::save::{SAVE_VERSION, SaveFile, SavedChunk, SavedNpc, read_save, write_save};
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
        app.add_plugins(crate::consumables::ConsumableIndexPlugin);
        app.add_plugins(crate::sleepers::SleeperIndexPlugin);
        app.add_plugins(crate::debug::DebugServerPlugin);
        app.add_plugins(crate::npc::NpcServerPlugin);
        app.add_plugins(crate::plans::PlansServerPlugin);
        // ServerScriptingPlugin inserts BlockRegistry; resolve well-known
        // terrain slots from it once so chunk gen doesn't hash strings.
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(terrain_slots);
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientAvatars>();
        app.init_resource::<ClientChunks>();
        app.init_resource::<PendingChunks>();
        app.init_resource::<PendingSpawnPose>();
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

/// Server App Startup: if `ServerSaveConfig::load_existing`, read the save
/// file and pre-populate `ChunkMap` with the persisted edited chunks. They
/// land with the `ChunkEdited` marker so subsequent AoI sends ship the
/// bytes rather than the procedural shortcut. Procedural chunks aren't
/// persisted (`Chunk::from_terrain` regenerates them on demand).
///
/// A load failure does NOT abort startup — we log and continue with an
/// empty world. Better than an unbootable session if a save is corrupt.
fn load_from_save(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut pending_pose: ResMut<PendingSpawnPose>,
    mut dirty: ResMut<DetectionDirty>,
    mut clock: ResMut<WorldClock>,
    config: Option<Res<ServerSaveConfig>>,
    block_registry: Res<BlockRegistry>,
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
        "loading {} edited chunks + {} NPCs from save {name:?}",
        save.edited_chunks.len(),
        save.npcs.len()
    );
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
        spawn_loaded_npc(&mut commands, npc, &kind_registry);
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
) {
    let mut needs = npc.needs;
    if let Some(def) = kind_registry.get(&npc.kind) {
        for (need_id, default_value) in &def.default_needs {
            needs.entry(need_id.clone()).or_insert(*default_value);
        }
    }
    commands.spawn((
        Actor,
        Npc,
        NpcId(npc.id),
        NpcKind(npc.kind),
        Needs(needs),
        Brain {
            goal: Goal::Idle,
            rng: npc.rng,
        },
        npc.pose,
        AvatarVelocity::default(),
        AvatarOnGround::default(),
        npc.movement_mode,
        MovementIntent::default(),
        NpcPath::default(),
        Replicate::to_clients(NetworkTarget::All),
        InterpolationTarget::to_clients(NetworkTarget::All),
        Name::new(format!("npc:{}", npc.id)),
    ));
}

/// Polled each tick. When the client flips the save-request atomic, write
/// the world to disk and clear the flag. Unlike `save_then_shutdown` this
/// is multi-shot (the user might "Save Now" several times per session) so
/// no Local guard.
fn save_on_request(
    flag: Option<Res<ServerSaveRequestFlag>>,
    config: Option<Res<ServerSaveConfig>>,
    clock: Res<WorldClock>,
    chunks: Query<(&ChunkCoord, &Chunk, &ChunkEntities), With<ChunkEdited>>,
    avatars: Query<&AvatarPose, With<Avatar>>,
    npcs: Query<(&NpcId, &NpcKind, &AvatarPose, &MovementMode, &Needs, &Brain), With<Npc>>,
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
    let saved_npcs = collect_saved_npcs(&npcs);
    let chunk_count = edited.len();
    let npc_count = saved_npcs.len();
    let save = SaveFile {
        version: SAVE_VERSION,
        edited_chunks: edited,
        last_player_pose: avatars.iter().next().copied(),
        npcs: saved_npcs,
        world_clock: Some(*clock),
    };
    match write_save(name, &save) {
        Ok(()) => info!("save-on-request: wrote {chunk_count} chunks + {npc_count} NPCs to {name:?}"),
        Err(e) => error!("save-on-request to {name:?} failed: {e}"),
    }
}

/// Snapshot every NPC's persistent state. `BrainDisabled` NPCs are
/// included — the marker is treated as a runtime recovery state, not
/// persisted, so reloading gives the planner a fresh chance. A
/// consistently broken planner will re-disable each NPC on its first
/// tick after load (and log loudly each time).
fn collect_saved_npcs(
    npcs: &Query<(&NpcId, &NpcKind, &AvatarPose, &MovementMode, &Needs, &Brain), With<Npc>>,
) -> Vec<SavedNpc> {
    npcs.iter()
        .map(|(id, kind, pose, mode, needs, brain)| SavedNpc {
            id: id.0,
            kind: kind.0.clone(),
            pose: *pose,
            movement_mode: *mode,
            needs: needs.0.clone(),
            rng: brain.rng,
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
    clock: Res<WorldClock>,
    chunks: Query<(&ChunkCoord, &Chunk, &ChunkEntities), With<ChunkEdited>>,
    avatars: Query<&AvatarPose, With<Avatar>>,
    npcs: Query<(&NpcId, &NpcKind, &AvatarPose, &MovementMode, &Needs, &Brain), With<Npc>>,
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
                let saved_npcs = collect_saved_npcs(&npcs);
                let chunk_count = edited.len();
                let npc_count = saved_npcs.len();
                let save = SaveFile {
                    version: SAVE_VERSION,
                    edited_chunks: edited,
                    last_player_pose: avatars.iter().next().copied(),
                    npcs: saved_npcs,
                    world_clock: Some(*clock),
                };
                match write_save(name, &save) {
                    Ok(()) => info!("saved {chunk_count} chunks + {npc_count} NPCs to {name:?}"),
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
    registry: Res<BlockRegistry>,
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
    let avatar = commands
        .spawn((
            Actor,
            Avatar,
            spawn_pose,
            AvatarVelocity::default(),
            AvatarOnGround::default(),
            MovementMode::default(),
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
fn apply_block_edit(
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
