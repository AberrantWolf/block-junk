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
use crate::npc_registry::{NeedRegistry, NpcKindRegistry};
use crate::protocol::{CellEdit, GameSet};
use crate::rooms::{RoomEventMsg, RoomPatternRegistry};

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
            needs,
            npc_kinds,
        } = load_side(Side::Server);
        app.insert_resource(ServerMods(mods));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(rooms);
        app.insert_resource(needs);
        app.insert_resource(npc_kinds);
        app.add_systems(
            Update,
            (dispatch_block_placed, dispatch_room_events).in_set(GameSet::PostSimulation),
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
            needs,
            npc_kinds,
        } = load_side(Side::Client);
        app.insert_resource(ClientMods(mods));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(rooms);
        app.insert_resource(needs);
        app.insert_resource(npc_kinds);
        // No client-only hooks yet — the registry is in place so adding one
        // is a single-system addition rather than a wiring change.
    }
}

struct LoadResult {
    mods: ModRegistry,
    blocks: BlockRegistry,
    slots: WorldSlots,
    rooms: RoomPatternRegistry,
    needs: NeedRegistry,
    npc_kinds: NpcKindRegistry,
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
    // Needs must be built before npc kinds so kind→need cross-validation
    // can run inside `NpcKindRegistry::build`.
    let needs = match NeedRegistry::build(ctx.take_needs()) {
        Ok(r) => r,
        Err(e) => panic!("{} need registry build failed: {e}", side.as_str()),
    };
    info!("[{}] need registry: {} need(s)", side.as_str(), needs.need_count());
    // Consumable blocks reference need ids; the need registry has to
    // exist before we can validate them. Failing here at boot beats
    // discovering "this food doesn't satisfy anything" the first time
    // an NPC tries to eat it.
    if let Err(e) = blocks.validate_consumables(&needs) {
        panic!("{} consumable validation failed: {e}", side.as_str());
    }
    let npc_kinds = match NpcKindRegistry::build(ctx.take_npc_kinds(), &needs) {
        Ok(r) => r,
        Err(e) => panic!("{} npc kind registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] npc kind registry: {} kind(s)",
        side.as_str(),
        npc_kinds.kind_count()
    );
    LoadResult {
        mods,
        blocks,
        slots,
        rooms,
        needs,
        npc_kinds,
    }
}

fn dispatch_block_placed(
    mut reader: MessageReader<CellEdit>,
    mut mods: ResMut<ServerMods>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        let event = BlockPlacedEvent {
            pos: BlockPos {
                x: edit.world.x,
                y: edit.world.y,
                z: edit.world.z,
            },
            block: registry.id_of(edit.slot).clone(),
        };
        mods.0.dispatch_block_placed(event);
    }
}

fn dispatch_room_events(
    mut reader: MessageReader<RoomEventMsg>,
    mut mods: ResMut<ServerMods>,
) {
    for msg in reader.read() {
        mods.0.dispatch_room_event(&msg.0);
    }
}
