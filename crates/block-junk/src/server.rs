use bevy::prelude::*;

use crate::protocol::{BlockEdit, GameSet};
use crate::voxel::Chunk;

pub struct ServerPlugin;

impl Plugin for ServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::scripting::ServerScriptingPlugin);
        app.add_systems(Startup, spawn_world)
            .add_systems(Update, apply_block_edits.in_set(GameSet::Simulation));
        // TODO: wire lightyear server, replicate chunks + accept BlockEdit messages over the wire.
    }
}

fn spawn_world(mut commands: Commands) {
    commands.spawn((
        Chunk::new_sphere(),
        Name::new("test_chunk"),
        Transform::default(),
    ));
}

fn apply_block_edits(mut reader: MessageReader<BlockEdit>, mut chunks: Query<&mut Chunk>) {
    for edit in reader.read() {
        if let Ok(mut chunk) = chunks.get_mut(edit.chunk) {
            chunk.set(edit.pos, edit.block);
        }
    }
}
