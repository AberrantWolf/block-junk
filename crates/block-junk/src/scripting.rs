//! Engine-side glue between Bevy and the mod scripting host.
//!
//! Each side (client / server) loads its own [`ModRegistry`] from `./mods/`.
//! The two registries hold separate Lua states even when the engine runs in
//! host mode, mirroring the eventual networked split.

use std::path::PathBuf;

use bevy::prelude::*;
use block_junk_mod_api::Side;
use block_junk_mod_api::server::BlockPlacedEvent;
use block_junk_mod_api::shared::BlockPos;
use block_junk_scripting::{LoadContext, ModRegistry, warn_if_empty};

use crate::blocks::{BlockRegistry, WorldSlots};
use crate::protocol::{BlockEdit, GameSet};
use crate::rooms::RoomPatternRegistry;

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
        let LoadResult {
            mods,
            blocks,
            slots,
            rooms,
        } = load_side(Side::Server);
        app.insert_resource(ServerMods(mods));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(rooms);
        app.add_systems(
            Update,
            dispatch_block_placed.in_set(GameSet::PostSimulation),
        );
    }
}

pub struct ClientScriptingPlugin;

impl Plugin for ClientScriptingPlugin {
    fn build(&self, app: &mut App) {
        let LoadResult {
            mods,
            blocks,
            slots,
            rooms,
        } = load_side(Side::Client);
        app.insert_resource(ClientMods(mods));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(rooms);
        // No client-only hooks yet — the registry is in place so adding one
        // is a single-system addition rather than a wiring change.
    }
}

struct LoadResult {
    mods: ModRegistry,
    blocks: BlockRegistry,
    slots: WorldSlots,
    rooms: RoomPatternRegistry,
}

/// Run mod loading for one side, then build the resulting registries.
/// Panics on any failure — there's no degraded mode that's safe to boot
/// into when content is misconfigured.
fn load_side(side: Side) -> LoadResult {
    let ctx = LoadContext::new();
    let mods = match ModRegistry::load_dir(side, &PathBuf::from(MODS_DIR), &ctx) {
        Ok(r) => r,
        Err(e) => panic!("{} mod load failed: {e}", side.as_str()),
    };
    warn_if_empty(&mods);
    let (blocks, slots) = match BlockRegistry::build(ctx.take_blocks()) {
        Ok(pair) => pair,
        Err(e) => panic!("{} block registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] block registry: {} block(s)",
        side.as_str(),
        blocks.slot_count()
    );
    let rooms = match RoomPatternRegistry::build(ctx.take_rooms()) {
        Ok(r) => r,
        Err(e) => panic!("{} room pattern registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] room registry: {} pattern(s)",
        side.as_str(),
        rooms.pattern_count()
    );
    LoadResult {
        mods,
        blocks,
        slots,
        rooms,
    }
}

fn dispatch_block_placed(
    mut reader: MessageReader<BlockEdit>,
    mut mods: ResMut<ServerMods>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        let event = BlockPlacedEvent {
            pos: BlockPos {
                x: edit.pos.x,
                y: edit.pos.y,
                z: edit.pos.z,
            },
            block: registry.id_of(edit.block).clone(),
        };
        mods.0.dispatch_block_placed(event);
    }
}
