use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use lightyear::prelude::server::ClientOf;
use lightyear::prelude::*;

use crate::protocol::{
    BlockEdit, ChunkCoord, ChunkSnapshot, GameSet, PlayerPosition, WorldChannel,
};
use crate::voxel::Chunk;

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        app.init_resource::<ChunkMap>();
        app.init_resource::<ClientPositions>();
        // Local Bevy bus for server-internal observers (scripting, building
        // detection, etc.). Not what crosses the wire — that's lightyear's
        // MessageSender/Receiver. Server-only.
        app.add_message::<BlockEdit>();
        app.add_systems(Startup, spawn_world);
        app.add_systems(
            Update,
            (receive_block_edits, track_client_positions).in_set(GameSet::Simulation),
        );
        app.add_observer(send_initial_chunks_to_new_client);
        app.add_observer(forget_disconnected_client);
    }
}

/// Latest known position for each connected client, keyed by the connection
/// entity (the `ClientOf` link entity). Filled by `track_client_positions`,
/// consumed by AoI streaming (Stage C).
#[derive(Resource, Default)]
pub struct ClientPositions(pub HashMap<Entity, Vec3>);

/// Maps chunk coords to their entity in this world. Server-authoritative.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

/// Initial world: a small grid of chunks generated from the terrain
/// function. This is the staged-A "no AoI yet" world; later commits add
/// streaming so the world is effectively unbounded but only nearby chunks
/// exist on the server at any time.
const INITIAL_RADIUS_XZ: i32 = 2;
const INITIAL_RADIUS_Y: i32 = 1;

fn spawn_world(mut commands: Commands, mut map: ResMut<ChunkMap>) {
    use crate::voxel::chunk_world_transform;

    for cy in -INITIAL_RADIUS_Y..=INITIAL_RADIUS_Y {
        for cz in -INITIAL_RADIUS_XZ..=INITIAL_RADIUS_XZ {
            for cx in -INITIAL_RADIUS_XZ..=INITIAL_RADIUS_XZ {
                let coord = ChunkCoord(IVec3::new(cx, cy, cz));
                let entity = commands
                    .spawn((
                        Chunk::from_terrain(coord),
                        coord,
                        Name::new(format!("chunk{:?}", coord.0.to_array())),
                        chunk_world_transform(coord),
                    ))
                    .id();
                map.0.insert(coord, entity);
            }
        }
    }
    let total = (2 * INITIAL_RADIUS_XZ + 1).pow(2) * (2 * INITIAL_RADIUS_Y + 1);
    info!("world spawned: {total} chunks");
}

/// When a client connects, hand them a snapshot of every chunk we have so
/// they can build their local copy. After this, the client stays in sync
/// via BlockEdit broadcasts (event-sourcing).
fn send_initial_chunks_to_new_client(
    trigger: On<Add, Connected>,
    chunks: Query<(&Chunk, &ChunkCoord)>,
    mut sender: Query<&mut MessageSender<ChunkSnapshot>>,
) {
    let Ok(mut sender) = sender.get_mut(trigger.entity) else {
        return;
    };
    for (chunk, coord) in chunks.iter() {
        sender.send::<WorldChannel>(ChunkSnapshot {
            coord: *coord,
            blocks: chunk.blocks.clone(),
        });
        info!("sent ChunkSnapshot {:?} to {:?}", coord.0, trigger.entity);
    }
}

/// Receive client edit requests, validate, apply to the world, broadcast
/// the applied edit to all clients. Also re-emit on the local Bevy bus so
/// scripting / other server-side systems hear it.
///
/// "Validation" is currently just "the chunk exists and Chunk::set accepts
/// the edit"; later we add ownership checks, anti-cheat, etc.
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

/// Drop the position record for a client when its connection ends. Without
/// this the map would grow unbounded over a long server lifetime.
fn forget_disconnected_client(
    trigger: On<Remove, ClientOf>,
    mut positions: ResMut<ClientPositions>,
) {
    positions.0.remove(&trigger.entity);
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

