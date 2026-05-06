use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::*;

use crate::protocol::{
    BlockEdit, ChunkCoord, ChunkSnapshot, ChunkUnload, GameSet, PlayerPosition, WorldChannel,
};
use crate::voxel::{Chunk, chunk_world_transform};

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientPositions>();
        app.init_resource::<ClientChunks>();
        app.init_resource::<PendingChunks>();
        // Local Bevy bus for server-internal observers (scripting, building
        // detection, etc.). Not what crosses the wire — that's lightyear's
        // MessageSender/Receiver. Server-only.
        app.add_message::<BlockEdit>();
        app.add_systems(
            Update,
            (
                receive_block_edits,
                track_client_positions,
                poll_chunk_gen,
                update_aoi,
            )
                .chain()
                .in_set(GameSet::Simulation),
        );
        app.add_observer(register_new_client);
        app.add_observer(forget_disconnected_client);
    }
}

/// Latest known position for each connected client, keyed by the connection
/// (`ClientOf`) entity. Filled by `track_client_positions`, read by `update_aoi`.
#[derive(Resource, Default)]
pub struct ClientPositions(pub HashMap<Entity, Vec3>);

/// Chunks currently believed to be loaded on each client. Used by `update_aoi`
/// to compute deltas (which snapshots/unloads to send each tick).
#[derive(Resource, Default)]
pub struct ClientChunks(pub HashMap<Entity, HashSet<ChunkCoord>>);

/// Authoritative map of chunk coords to their entity in this world.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

/// Chunks whose generation is currently in flight on a worker thread.
/// `update_aoi` skips coords already in here so we don't queue duplicate
/// generations; `poll_chunk_gen` drains them as they complete.
#[derive(Resource, Default)]
pub struct PendingChunks(pub HashMap<ChunkCoord, Task<Chunk>>);

const AOI_RADIUS_XZ: i32 = 2;
const AOI_RADIUS_Y: i32 = 1;

/// On client connect: stash a default position so AoI starts streaming
/// chunks before the first PlayerPosition message lands. Without this the
/// new client sees nothing until ~100 ms post-connect.
fn register_new_client(
    trigger: On<Add, Connected>,
    mut positions: ResMut<ClientPositions>,
    mut sent: ResMut<ClientChunks>,
) {
    positions.0.entry(trigger.entity).or_insert(Vec3::ZERO);
    sent.0.entry(trigger.entity).or_default();
}

fn track_client_positions(
    mut receivers: Query<(Entity, &mut MessageReceiver<PlayerPosition>)>,
    mut positions: ResMut<ClientPositions>,
) {
    for (entity, mut receiver) in receivers.iter_mut() {
        for msg in receiver.receive() {
            positions.0.insert(entity, msg.0);
        }
    }
}

fn forget_disconnected_client(
    trigger: On<Remove, ClientOf>,
    mut positions: ResMut<ClientPositions>,
    mut sent: ResMut<ClientChunks>,
) {
    positions.0.remove(&trigger.entity);
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
    chunks: Query<&Chunk>,
    mut pending: ResMut<PendingChunks>,
    positions: Res<ClientPositions>,
    mut sent: ResMut<ClientChunks>,
    mut snapshots: Query<&mut MessageSender<ChunkSnapshot>>,
    mut unloads: Query<&mut MessageSender<ChunkUnload>>,
) {
    for (&client_entity, pos) in positions.0.iter() {
        let player_chunk = world_to_chunk_coord(*pos);
        let desired = aoi_around(player_chunk);
        let current = sent.0.entry(client_entity).or_default();

        let candidates: Vec<ChunkCoord> = desired.difference(current).copied().collect();
        let removed: Vec<ChunkCoord> = current.difference(&desired).copied().collect();

        for coord in &candidates {
            let blocks: Option<Vec<crate::protocol::Block>> = if let Some(&entity) =
                chunk_map.0.get(coord)
            {
                chunks.get(entity).ok().map(|c| c.blocks.clone())
            } else {
                if !pending.0.contains_key(coord) {
                    let coord_for_task = *coord;
                    let task = AsyncComputeTaskPool::get()
                        .spawn(async move { Chunk::from_terrain(coord_for_task) });
                    pending.0.insert(*coord, task);
                }
                None
            };

            let Some(blocks) = blocks else {
                continue; // still generating; try again next tick
            };
            if let Ok(mut sender) = snapshots.get_mut(client_entity) {
                sender.send::<WorldChannel>(ChunkSnapshot {
                    coord: *coord,
                    blocks,
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
    mut receivers: Query<&mut MessageReceiver<BlockEdit>>,
    mut chunks: Query<&mut Chunk>,
    map: Res<ChunkMap>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut bus: MessageWriter<BlockEdit>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for mut receiver in receivers.iter_mut() {
        for edit in receiver.receive() {
            let Some(&entity) = map.0.get(&edit.coord) else {
                continue;
            };
            let Ok(mut chunk) = chunks.get_mut(entity) else {
                continue;
            };
            if !chunk.set(edit.pos, edit.block) {
                continue;
            }

            // Broadcast the applied edit to every connected client.
            if let Err(err) =
                broadcast.send::<BlockEdit, WorldChannel>(&edit, server, &NetworkTarget::All)
            {
                warn!("BlockEdit broadcast failed: {err}");
            }

            // Local bus so scripting/other server-side hooks see it.
            bus.write(edit);
        }
    }
}
