//! Engine-side glue between Bevy and the mod scripting host.
//!
//! Each side (client / server) loads its own [`ModRegistry`] from `./mods/`.
//! The two registries hold separate Lua states even when the engine runs in
//! host mode, mirroring the eventual networked split.

use std::path::PathBuf;

use bevy::prelude::*;
use block_junk_mod_api::server::BlockPlacedEvent;
use block_junk_mod_api::shared::{BlockKind, BlockPos};
use block_junk_mod_api::Side;
use block_junk_scripting::{ModRegistry, warn_if_empty};

use crate::protocol::{Block, BlockEdit, GameSet};

const MODS_DIR: &str = "./mods";

/// Wrapper resources so server and client registries live as distinct
/// types in the ECS even when both run in the same process.
#[derive(Resource)]
pub struct ServerMods(pub ModRegistry);

#[derive(Resource)]
#[allow(dead_code, reason = "field is read once we add the first client-side hook")]
pub struct ClientMods(pub ModRegistry);

pub struct ServerScriptingPlugin;

impl Plugin for ServerScriptingPlugin {
    fn build(&self, app: &mut App) {
        let registry = match ModRegistry::load_dir(Side::Server, &PathBuf::from(MODS_DIR)) {
            Ok(r) => r,
            Err(e) => panic!("server mod load failed: {e}"),
        };
        warn_if_empty(&registry);
        app.insert_resource(ServerMods(registry));
        app.add_systems(
            Update,
            dispatch_block_placed.in_set(GameSet::PostSimulation),
        );
    }
}

pub struct ClientScriptingPlugin;

impl Plugin for ClientScriptingPlugin {
    fn build(&self, app: &mut App) {
        let registry = match ModRegistry::load_dir(Side::Client, &PathBuf::from(MODS_DIR)) {
            Ok(r) => r,
            Err(e) => panic!("client mod load failed: {e}"),
        };
        warn_if_empty(&registry);
        app.insert_resource(ClientMods(registry));
        // No client-only hooks yet — the registry is in place so adding one
        // is a single-system addition rather than a wiring change.
    }
}

fn dispatch_block_placed(mut reader: MessageReader<BlockEdit>, mut mods: ResMut<ServerMods>) {
    for edit in reader.read() {
        let event = BlockPlacedEvent {
            pos: BlockPos {
                x: edit.pos.x,
                y: edit.pos.y,
                z: edit.pos.z,
            },
            block: match edit.block {
                Block::Empty => BlockKind::Empty,
                Block::Solid => BlockKind::Solid,
            },
        };
        mods.0.dispatch_block_placed(event);
    }
}
