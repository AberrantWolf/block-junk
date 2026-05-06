use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use lightyear::prelude::*;

use crate::protocol::{BlockEdit, ChunkCoord, ChunkSnapshot, GameSet, WorldChannel};
use crate::voxel::Chunk;

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        app.init_resource::<ChunkMap>();
        // Local Bevy bus for server-internal observers (scripting, building
        // detection, etc.). Not what crosses the wire — that's lightyear's
        // MessageSender/Receiver. Server-only.
        app.add_message::<BlockEdit>();
        app.add_systems(Startup, spawn_world);
        app.add_systems(Update, receive_block_edits.in_set(GameSet::Simulation));
        app.add_observer(send_initial_chunks_to_new_client);
    }
}

/// Maps chunk coords to their entity in this world. Server-authoritative.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

fn spawn_world(mut commands: Commands, mut map: ResMut<ChunkMap>) {
    let coord = ChunkCoord(IVec3::ZERO);
    let entity = commands
        .spawn((
            Chunk::new_sphere(),
            coord,
            Name::new("chunk(0,0,0)"),
            Transform::default(),
        ))
        .id();
    map.0.insert(coord, entity);
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

