use core::time::Duration;

use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot, TerrainSlots};
use crate::protocol::{
    Avatar, AvatarPose, BlockEdit, BlockManifest, CellEdit, ChunkCoord, ChunkData, ChunkSnapshot,
    ChunkUnload, GameSet, PlayerPose, WorldChannel,
};
use crate::rooms::{DetectionDirty, RoomEventMsg, RoomMap, mark_dirty_from_edits, process_dirty};
use crate::voxel::{Chunk, ChunkEntities, EntryKind, chunk_world_transform, world_to_chunk};

/// Marker on chunks whose state has diverged from the deterministic terrain
/// function. Server uses it to decide whether to ship the bytes or just
/// tell the client "regenerate locally" on AoI entry.
#[derive(Component)]
pub struct ChunkEdited;

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        // ServerScriptingPlugin inserts BlockRegistry; resolve well-known
        // terrain slots from it once so chunk gen doesn't hash strings.
        let terrain_slots = TerrainSlots::from_registry(app.world().resource::<BlockRegistry>());
        app.insert_resource(terrain_slots);
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientAvatars>();
        app.init_resource::<ClientChunks>();
        app.init_resource::<PendingChunks>();
        app.init_resource::<RoomMap>();
        app.init_resource::<DetectionDirty>();
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
            (track_client_positions, poll_chunk_gen, update_aoi)
                .chain()
                .in_set(GameSet::Simulation),
        );
        app.add_observer(install_replication_sender);
        app.add_observer(register_new_client);
        app.add_observer(forget_disconnected_client);
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
    registry: Res<BlockRegistry>,
    mut manifests: Query<&mut MessageSender<BlockManifest>>,
) {
    let connection = trigger.entity;
    let Ok(remote) = remote_ids.get(connection) else {
        warn!("Connected fired with no RemoteId on entity {connection:?}");
        return;
    };
    let avatar = commands
        .spawn((
            Avatar,
            AvatarPose::default(),
            Replicate::to_clients(NetworkTarget::AllExceptSingle(remote.0)),
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

fn track_client_positions(
    mut receivers: Query<(Entity, &mut MessageReceiver<PlayerPose>)>,
    avatars: Res<ClientAvatars>,
    mut poses: Query<&mut AvatarPose>,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        // Drain everything on this receiver, but only the latest pose
        // matters — older ones get superseded before AoI runs anyway.
        let mut latest: Option<PlayerPose> = None;
        for msg in receiver.receive() {
            latest = Some(msg);
        }
        let Some(pose) = latest else { continue };
        let Some(&avatar) = avatars.0.get(&connection) else {
            continue;
        };
        if let Ok(mut p) = poses.get_mut(avatar) {
            p.translation = pose.translation;
            p.yaw = pose.yaw;
        }
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
