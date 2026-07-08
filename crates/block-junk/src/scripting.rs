//! Engine-side glue between Bevy and the mod scripting host.
//!
//! Each side (client / server) loads its own [`ModRegistry`] from `./mods/`.
//! The two registries hold separate Lua states even when the engine runs in
//! host mode, mirroring the eventual networked split.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use block_junk_mod_api::Side;
use block_junk_mod_api::server::BlockPlacedEvent;
use block_junk_mod_api::shared::BlockPos;
use block_junk_mod_api::ui::UiToast;
use block_junk_scripting::{LoadContext, ModRegistry, warn_if_empty};
use lightyear::prelude::{NetworkTarget, Server, ServerMultiMessageSender};

use crate::block_textures::TextureRegistry;
use crate::blocks::{BlockRegistry, WorldSlots};
use crate::items::ItemRegistry;
use crate::npc_registry::{AnimationRegistry, NeedRegistry, NpcKindRegistry, WorkDefaultsRes};
use crate::protocol::{CellEdit, GameSet};
use crate::recipes::RecipeRegistry;
use crate::rooms::{RoomEventMsg, RoomPatternRegistry};

const MODS_DIR: &str = "./mods";

/// Wrapper resources so server and client registries live as distinct
/// types in the ECS even when both run in the same process.
#[derive(Resource)]
pub struct ServerMods(pub ModRegistry);

#[derive(Resource)]
pub struct ClientMods(pub ModRegistry);

/// Live handle to this side's `engine.ui.toast` queue. Lua pushes into
/// it at any point during the session; one drain system per side
/// empties it every tick (server → broadcast, client → local toasts).
#[derive(Resource)]
pub struct ModToastQueue(pub Arc<Mutex<Vec<UiToast>>>);

/// Hard cap on a single mod toast's length. Toasts are glanceable
/// worldspace chips, not dialogue boxes — and the server broadcasts
/// the string to everyone, so unbounded text is also a wire concern.
const TOAST_TEXT_MAX: usize = 120;

/// Drain this side's mod-toast queue, normalising text length. Shared
/// by both drain systems.
fn drain_toast_queue(queue: &ModToastQueue) -> Vec<crate::protocol::WorldToast> {
    let mut drained = queue.0.lock().unwrap();
    drained
        .drain(..)
        .map(|t| {
            let mut text = t.text;
            if text.len() > TOAST_TEXT_MAX {
                let mut end = TOAST_TEXT_MAX;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push('…');
            }
            crate::protocol::WorldToast {
                cell: bevy::math::IVec3::new(t.pos.x, t.pos.y, t.pos.z),
                text,
            }
        })
        .collect()
}

pub struct ServerScriptingPlugin;

impl Plugin for ServerScriptingPlugin {
    fn build(&self, app: &mut App) {
        let LoadResult {
            mods,
            blocks,
            slots,
            items,
            recipes,
            rooms,
            needs,
            npc_kinds,
            textures,
            animations,
            work_defaults,
            civilization_params,
            ui_toasts,
        } = load_side(Side::Server);
        app.insert_resource(ServerMods(mods));
        app.insert_resource(ModToastQueue(ui_toasts));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(items);
        app.insert_resource(recipes);
        app.insert_resource(rooms);
        app.insert_resource(needs);
        app.insert_resource(npc_kinds);
        app.insert_resource(textures);
        app.insert_resource(animations);
        app.insert_resource(work_defaults);
        app.insert_resource(civilization_params);
        app.add_systems(
            Update,
            (
                dispatch_block_placed,
                dispatch_room_events,
                broadcast_mod_toasts,
            )
                .in_set(GameSet::PostSimulation),
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
            items,
            recipes,
            rooms,
            needs,
            npc_kinds,
            textures,
            animations,
            work_defaults,
            civilization_params: _,
            ui_toasts,
        } = load_side(Side::Client);
        app.insert_resource(ClientMods(mods));
        app.insert_resource(ModToastQueue(ui_toasts));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(items);
        app.insert_resource(recipes);
        app.insert_resource(rooms);
        app.insert_resource(needs);
        app.insert_resource(npc_kinds);
        app.insert_resource(textures);
        app.insert_resource(animations);
        app.insert_resource(work_defaults);
        // Client-side mod toasts render locally — no round-trip. The
        // inspect-panel hook (`engine.ui.on_inspect`) is pulled, not
        // pushed: `inspect_panel::refresh_inspect_panel` calls into
        // `ClientMods` directly.
        app.add_systems(
            Update,
            drain_client_mod_toasts.in_set(GameSet::PostSimulation),
        );
    }
}

/// Server side: drained `engine.ui.toast` calls become [`WorldToast`]
/// broadcasts. Sparse traffic — mods toast on events, not per tick.
fn broadcast_mod_toasts(
    queue: Res<ModToastQueue>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for toast in drain_toast_queue(&queue) {
        if let Err(err) = broadcast
            .send::<crate::protocol::WorldToast, crate::protocol::WorldChannel>(
                &toast,
                server,
                &NetworkTarget::All,
            )
        {
            warn!("mod toast broadcast failed: {err}");
        }
    }
}

/// Client side: drained `engine.ui.toast` calls go straight to the
/// local worldspace-toast queue.
fn drain_client_mod_toasts(
    queue: Res<ModToastQueue>,
    mut pending: ResMut<crate::worldspace_toast::PendingToasts>,
) {
    for toast in drain_toast_queue(&queue) {
        pending.push(crate::worldspace_toast::SpawnToast {
            cell: toast.cell,
            text: toast.text,
        });
    }
}

struct LoadResult {
    mods: ModRegistry,
    blocks: BlockRegistry,
    slots: WorldSlots,
    items: ItemRegistry,
    recipes: RecipeRegistry,
    rooms: RoomPatternRegistry,
    needs: NeedRegistry,
    npc_kinds: NpcKindRegistry,
    textures: TextureRegistry,
    animations: AnimationRegistry,
    work_defaults: WorkDefaultsRes,
    civilization_params: crate::civilization::CivilizationParamsRes,
    /// Live `engine.ui.toast` queue handle (runtime, not load-time).
    ui_toasts: Arc<Mutex<Vec<UiToast>>>,
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
    let mut pending_blocks = ctx.take_blocks();
    // Resolve the drops contract before anything reads the defs:
    // unspecified drops default to materials × the (mod-configurable)
    // multiplier. Must happen after ALL mods have run — the multiplier
    // may be set by a later mod than the blocks it applies to.
    let drop_multiplier = ctx.take_material_drop_multiplier().unwrap_or(1.0);
    for def in &mut pending_blocks {
        def.resolve_drops(drop_multiplier);
    }
    let (blocks, slots) = match BlockRegistry::build(pending_blocks.clone()) {
        Ok(pair) => pair,
        Err(e) => panic!("{} block registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] block registry: {} block(s)",
        side.as_str(),
        blocks.slot_count()
    );
    let items = match ItemRegistry::build(ctx.take_items()) {
        Ok(r) => r,
        Err(e) => panic!("{} item registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] item registry: {} item(s)",
        side.as_str(),
        items.slot_count()
    );
    // Cross-validate block drops against the item registry. The drops
    // list is engine-opaque until item ids resolve, so a typo here
    // would silently spawn nothing at runtime — fail loud at boot.
    if let Err(e) = items.validate_block_drops(&pending_blocks) {
        panic!("{} block drops validation failed: {e}", side.as_str());
    }
    // Recipes reference item ids (inputs + output) so the item
    // registry has to exist first. Validates duration bounds + id
    // resolution inside `build`; station-tag reachability is
    // checked against the block list below.
    let recipes = match RecipeRegistry::build(ctx.take_recipes(), &items) {
        Ok(r) => r,
        Err(e) => panic!("{} recipe registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] recipe registry: {} recipe(s)",
        side.as_str(),
        recipes.slot_count()
    );
    if let Err(e) = recipes.validate_against_blocks(&pending_blocks) {
        panic!("{} recipe station validation failed: {e}", side.as_str());
    }
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
    info!(
        "[{}] need registry: {} need(s)",
        side.as_str(),
        needs.need_count()
    );
    // Work-action defaults reference a need id, so the need registry
    // must exist first. Either side might consult these (the snapshot
    // builder runs server-side today but the resource is mirrored).
    let work_defaults = match WorkDefaultsRes::build(ctx.take_work_defaults(), &needs) {
        Ok(r) => r,
        Err(e) => panic!("{} work defaults build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] work defaults: need={:?} duration={}s",
        side.as_str(),
        work_defaults.0.need_restore.as_ref().map(|nr| &nr.need),
        work_defaults.0.duration_secs,
    );
    // Consumable blocks reference need ids; the need registry has to
    // exist before we can validate them. Failing here at boot beats
    // discovering "this food doesn't satisfy anything" the first time
    // an NPC tries to eat it.
    if let Err(e) = blocks.validate_interactables(&needs) {
        panic!("{} interactable validation failed: {e}", side.as_str());
    }
    if let Err(e) = blocks.validate_work_actions(&needs) {
        panic!("{} work action validation failed: {e}", side.as_str());
    }
    if let Err(e) = blocks.validate_use_slots() {
        panic!("{} use_slot validation failed: {e}", side.as_str());
    }
    // S4 forage loop: depleted_block / regrow.into must resolve, or a
    // harvest/regrow would panic mid-game looking up a dangling id.
    if let Err(e) = blocks.validate_transitions() {
        panic!("{} block transition validation failed: {e}", side.as_str());
    }
    // Animations need to exist before kinds + use-slots can reference
    // them. Build them before the cross-validators so the failure
    // mode is "unknown animation id," not "kind/use-slot references
    // something the registry hasn't built yet."
    let animations = match AnimationRegistry::build(ctx.take_animations()) {
        Ok(r) => r,
        Err(e) => panic!("{} animation registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] animation registry: {} clip(s)",
        side.as_str(),
        animations.len()
    );
    if let Err(e) = blocks.validate_use_slot_animations(&animations) {
        panic!(
            "{} use_slot animation validation failed: {e}",
            side.as_str()
        );
    }
    let npc_kinds = match NpcKindRegistry::build(ctx.take_npc_kinds(), &needs, &animations) {
        Ok(r) => r,
        Err(e) => panic!("{} npc kind registry build failed: {e}", side.as_str()),
    };
    info!(
        "[{}] npc kind registry: {} kind(s)",
        side.as_str(),
        npc_kinds.kind_count()
    );
    // Procedural texture docs (per-mod textures.lua, pure data — not
    // run through the mod sandbox). Both sides parse + validate so a
    // broken file fails the headless server too; only the client bakes.
    let textures = match TextureRegistry::load_from_mods_dir(&PathBuf::from(MODS_DIR)) {
        Ok(r) => r,
        Err(e) => panic!("{} texture load failed: {e}", side.as_str()),
    };
    info!(
        "[{}] texture registry: {} texture(s)",
        side.as_str(),
        textures.texture_count(),
    );
    if let Err(e) = blocks.validate_textures(&textures) {
        panic!("{} block texture validation failed: {e}", side.as_str());
    }
    // Civilization-cluster params. Lua-supplied via
    // `engine.civilization.set_params`; default if no mod set them.
    let civilization_params = crate::civilization::CivilizationParamsRes(
        ctx.take_civilization_params().unwrap_or_default(),
    );
    info!(
        "[{}] civilization params: max_room_distance={} buffer={}",
        side.as_str(),
        civilization_params.0.max_room_distance_cells,
        civilization_params.0.buffer_cells,
    );
    LoadResult {
        mods,
        blocks,
        slots,
        items,
        recipes,
        rooms,
        needs,
        npc_kinds,
        textures,
        animations,
        work_defaults,
        civilization_params,
        ui_toasts: ctx.ui_toast_queue(),
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

fn dispatch_room_events(mut reader: MessageReader<RoomEventMsg>, mut mods: ResMut<ServerMods>) {
    for msg in reader.read() {
        mods.0.dispatch_room_event(&msg.0);
    }
}
