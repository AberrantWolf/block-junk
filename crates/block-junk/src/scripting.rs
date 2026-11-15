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
use lightyear::prelude::{Server, ServerMultiMessageSender};

use crate::block_textures::TextureRegistry;
use crate::blocks::{BlockRegistry, WorldSlots};
use crate::items::ItemRegistry;
use crate::npc_registry::{AnimationRegistry, NeedRegistry, NpcKindRegistry, WorkDefaultsRes};
use crate::protocol::{CellEdit, GameSet};
use crate::recipes::RecipeRegistry;

const MODS_DIR: &str = "./mods";

/// Wrapper resources so server and client registries live as distinct
/// types in the ECS even when both run in the same process.
#[derive(Resource)]
pub struct ServerMods(pub ModRegistry);

#[derive(Resource)]
pub struct ClientMods(pub ModRegistry);

#[derive(Resource)]
pub struct ContentFingerprint(pub u64);

#[derive(Resource, Clone)]
pub struct SelectedWorld(pub block_junk_mod_api::worlds::WorldPreset);

#[derive(Resource, Clone)]
pub struct WorldCatalog {
    pub presets: Vec<block_junk_mod_api::worlds::WorldPreset>,
    pub base_fingerprint: u64,
}
impl WorldCatalog {
    pub fn fingerprint(
        &self,
        preset: &block_junk_mod_api::worlds::WorldPreset,
    ) -> ContentFingerprint {
        let mut hash = self.base_fingerprint;
        for byte in serde_json::to_vec(preset).expect("validated world preset") {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        ContentFingerprint(hash)
    }
}
#[derive(Resource, Default)]
pub struct WorldSelection(pub Option<String>);

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
fn drain_toast_queue(queue: &ModToastQueue) -> Vec<(IVec3, String)> {
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
            (bevy::math::IVec3::new(t.pos.x, t.pos.y, t.pos.z), text)
        })
        .collect()
}

pub struct ServerScriptingPlugin;

impl Plugin for ServerScriptingPlugin {
    fn build(&self, app: &mut App) {
        let LoadResult {
            mods,
            fingerprint,
            preset,
            catalog,
            contracts,
            blocks,
            slots,
            items,
            recipes,
            needs,
            npc_kinds,
            textures,
            animations,
            work_defaults,
            civilization_params,
            world_rules,
            ui_toasts,
        } = {
            let saved = app.world().get_resource::<crate::menu::ServerSaveConfig>();
            let preset = saved.and_then(|config| {
                if config.preset.is_some() {
                    config.preset.clone()
                } else if config.load_existing {
                    config.save_name.as_deref().map(|name| {
                        crate::save::read_save(name)
                            .unwrap_or_else(|e| panic!("cannot open saved world: {e}"))
                            .world
                            .expect("saved world preset required")
                            .preset
                            .id
                    })
                } else {
                    config.preset.clone()
                }
            });
            load_side_selected(Side::Server, preset.as_deref())
        };
        app.insert_resource(ServerMods(mods));
        app.init_resource::<crate::spatial_events::SpatialJournal>();
        app.add_systems(
            PostUpdate,
            crate::spatial_events::dispatch.after(crate::live_spaces::compute_spaces),
        );
        app.init_resource::<ModBehaviorFailures>();
        app.init_resource::<crate::server::PendingTerrainEffects>();
        app.add_systems(
            First,
            (
                advance_mod_events,
                cleanup_failed_behaviors,
                crate::mod_commands::execute_batches,
                retain_job_causes,
            )
                .chain(),
        );
        app.add_systems(
            Startup,
            activate_server_runtime
                .after(crate::server::WorldLoad)
                .in_set(WorldHooks),
        );
        app.insert_resource(contracts);
        app.insert_resource(world_rules);
        app.insert_resource(fingerprint);
        app.insert_resource(WorldSelection(
            crate::content_launch()
                .preset
                .clone()
                .or_else(|| (catalog.presets.len() == 1).then(|| preset.0.id.clone())),
        ));
        app.insert_resource(catalog);
        app.insert_resource(preset);
        app.insert_resource(ModToastQueue(ui_toasts));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(items);
        app.insert_resource(recipes);
        app.insert_resource(needs);
        app.insert_resource(npc_kinds);
        app.insert_resource(textures);
        app.insert_resource(animations);
        app.insert_resource(work_defaults);
        app.insert_resource(civilization_params);
        app.add_systems(
            Update,
            (dispatch_block_placed, broadcast_mod_toasts).in_set(GameSet::PostSimulation),
        );
    }
}

pub struct ClientScriptingPlugin;

impl Plugin for ClientScriptingPlugin {
    fn build(&self, app: &mut App) {
        let LoadResult {
            mods,
            fingerprint,
            preset,
            catalog,
            contracts,
            blocks,
            slots,
            items,
            recipes,
            needs,
            npc_kinds,
            textures,
            animations,
            work_defaults,
            civilization_params: _,
            world_rules,
            ui_toasts,
        } = load_side(Side::Client);
        app.insert_resource(ClientMods(mods));
        app.insert_resource(contracts);
        app.insert_resource(world_rules);
        app.insert_resource(fingerprint);
        app.insert_resource(WorldSelection(
            crate::content_launch()
                .preset
                .clone()
                .or_else(|| (catalog.presets.len() == 1).then(|| preset.0.id.clone())),
        ));
        app.insert_resource(catalog);
        app.insert_resource(preset);
        app.insert_resource(ModToastQueue(ui_toasts));
        app.insert_resource(blocks);
        app.insert_resource(slots);
        app.insert_resource(items);
        app.insert_resource(recipes);
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

/// Server-side mod toasts use the ordered spatial lane and only target
/// subscribers to the anchor chunk.
fn broadcast_mod_toasts(
    queue: Res<ModToastQueue>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    subscriptions: Res<crate::server::SpatialSubscriptions>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (cell, text) in drain_toast_queue(&queue) {
        let Ok(text) = crate::protocol::BoundedString::<256>::new(text) else {
            continue;
        };
        let target = subscriptions.target_for_chunks([crate::voxel::world_to_chunk(cell).0]);
        let message = crate::protocol::SpatialMessage::Toast { cell, text };
        if let Err(err) = broadcast
            .send::<crate::protocol::SpatialMessage, crate::protocol::SpatialChannel>(
                &message, server, &target,
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
    for (cell, text) in drain_toast_queue(&queue) {
        pending.push(crate::worldspace_toast::SpawnToast { cell, text });
    }
}

pub(crate) struct LoadResult {
    pub(crate) mods: ModRegistry,
    fingerprint: ContentFingerprint,
    pub(crate) preset: SelectedWorld,
    catalog: WorldCatalog,
    contracts: crate::content_contracts::ContentContracts,
    pub(crate) blocks: BlockRegistry,
    slots: WorldSlots,
    items: ItemRegistry,
    recipes: RecipeRegistry,
    needs: NeedRegistry,
    npc_kinds: NpcKindRegistry,
    textures: TextureRegistry,
    animations: AnimationRegistry,
    work_defaults: WorkDefaultsRes,
    civilization_params: crate::civilization::CivilizationParamsRes,
    world_rules: crate::world_rules::WorldRules,
    /// Live `engine.ui.toast` queue handle (runtime, not load-time).
    ui_toasts: Arc<Mutex<Vec<UiToast>>>,
}

/// Run mod loading for one side, then build the resulting registries.
/// Panics on any failure — there's no degraded mode that's safe to boot
/// into when content is misconfigured.
fn load_side(side: Side) -> LoadResult {
    load_side_selected(side, None)
}

fn load_side_selected(side: Side, selected: Option<&str>) -> LoadResult {
    let mods_dir = crate::content_launch()
        .mods_dir
        .clone()
        .or_else(|| std::env::var_os("BLOCK_JUNK_MODS_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(MODS_DIR));
    let preset = crate::content_launch()
        .preset
        .clone()
        .or_else(|| std::env::var("BLOCK_JUNK_WORLD_PRESET").ok());
    load_content(side, &mods_dir, selected.or(preset.as_deref()))
        .unwrap_or_else(|error| panic!("content validation failed: {error}"))
}

pub(crate) fn load_content(
    side: Side,
    mods_dir: &std::path::Path,
    selected: Option<&str>,
) -> Result<LoadResult, String> {
    let ctx = LoadContext::new();
    compile_content(side, mods_dir, selected, &ctx).map_err(|error| ctx.explain_validation(&error))
}

fn compile_content(
    side: Side,
    mods_dir: &std::path::Path,
    selected: Option<&str>,
    ctx: &LoadContext,
) -> Result<LoadResult, String> {
    let mods = match ModRegistry::load_composed(
        side,
        mods_dir,
        ctx,
        block_junk_scripting::ScriptLimits::default(),
    ) {
        Ok(r) => r,
        Err(e) => return Err(format!("{} mod load failed: {e}", side.as_str())),
    };
    warn_if_empty(&mods);
    let mut pending_blocks = ctx.take_blocks();
    // Resolve the drops contract before anything reads the defs:
    // unspecified drops default to materials × the (mod-configurable)
    // multiplier. Must happen after ALL mods have run — the multiplier
    // may be set by a later mod than the blocks it applies to.
    let drop_multiplier = ctx.take_material_drop_multiplier().unwrap_or(1.0);
    for def in &mut pending_blocks {
        if def.drops.is_none() {
            ctx.record_derivation(
                format!("blocks/{}/drops", def.id.as_str()),
                vec![
                    format!("blocks/{}/materials", def.id.as_str()),
                    "blocks/material_drop_multiplier".into(),
                ],
                "resolve material-derived drops",
            );
        }
        def.resolve_drops(drop_multiplier)?;
        ctx.publish_definition("blocks", def.id.as_str(), def);
    }
    let (blocks, slots) = match BlockRegistry::build(pending_blocks.clone()) {
        Ok(pair) => pair,
        Err(e) => {
            return Err(format!(
                "{} block registry build failed: {e}",
                side.as_str()
            ));
        }
    };
    info!(
        "[{}] block registry: {} block(s)",
        side.as_str(),
        blocks.slot_count()
    );
    let items = match ItemRegistry::build(ctx.take_items()) {
        Ok(r) => r,
        Err(e) => return Err(format!("{} item registry build failed: {e}", side.as_str())),
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
        return Err(format!(
            "{} block drops validation failed: {e}",
            side.as_str()
        ));
    }
    // Recipes reference item ids (inputs + output) so the item
    // registry has to exist first. Validates duration bounds + id
    // resolution inside `build`; station-tag reachability is
    // checked against the block list below.
    let recipes = match RecipeRegistry::build(ctx.take_recipes(), &items) {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "{} recipe registry build failed: {e}",
                side.as_str()
            ));
        }
    };
    info!(
        "[{}] recipe registry: {} recipe(s)",
        side.as_str(),
        recipes.slot_count()
    );
    if let Err(e) = recipes.validate_against_blocks(&pending_blocks) {
        return Err(format!(
            "{} recipe station validation failed: {e}",
            side.as_str()
        ));
    }
    if !ctx.take_rooms().is_empty() {
        return Err("engine.rooms definitions are obsolete; use additive capabilities".into());
    }
    // Needs must be built before npc kinds so kind→need cross-validation
    // can run inside `NpcKindRegistry::build`.
    let needs = match NeedRegistry::build(ctx.take_needs()) {
        Ok(r) => r,
        Err(e) => return Err(format!("{} need registry build failed: {e}", side.as_str())),
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
        Err(e) => return Err(format!("{} work defaults build failed: {e}", side.as_str())),
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
    blocks.validate_facility_actions(&needs)?;
    blocks.validate_work_duration_products(&work_defaults.0)?;
    if let Err(e) = blocks.validate_interactables(&needs) {
        return Err(format!(
            "{} interactable validation failed: {e}",
            side.as_str()
        ));
    }
    if let Err(e) = blocks.validate_work_actions(&needs) {
        return Err(format!(
            "{} work action validation failed: {e}",
            side.as_str()
        ));
    }
    if let Err(e) = blocks.validate_use_slots() {
        return Err(format!("{} use_slot validation failed: {e}", side.as_str()));
    }
    // S4 forage loop: depleted_block / regrow.into must resolve, or a
    // harvest/regrow would panic mid-game looking up a dangling id.
    if let Err(e) = blocks.validate_transitions() {
        return Err(format!(
            "{} block transition validation failed: {e}",
            side.as_str()
        ));
    }
    // Animations need to exist before kinds + use-slots can reference
    // them. Build them before the cross-validators so the failure
    // mode is "unknown animation id," not "kind/use-slot references
    // something the registry hasn't built yet."
    let animations = match AnimationRegistry::build(ctx.take_animations()) {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "{} animation registry build failed: {e}",
                side.as_str()
            ));
        }
    };
    info!(
        "[{}] animation registry: {} clip(s)",
        side.as_str(),
        animations.len()
    );
    if let Err(e) = blocks.validate_use_slot_animations(&animations) {
        return Err(format!(
            "{} use_slot animation validation failed: {e}",
            side.as_str()
        ));
    }
    let npc_kinds = match NpcKindRegistry::build(ctx.take_npc_kinds(), &needs, &animations) {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "{} npc kind registry build failed: {e}",
                side.as_str()
            ));
        }
    };
    info!(
        "[{}] npc kind registry: {} kind(s)",
        side.as_str(),
        npc_kinds.kind_count()
    );
    // Procedural texture docs (per-mod textures.lua, pure data — not
    // run through the mod sandbox). Both sides parse + validate so a
    // broken file fails the headless server too; only the client bakes.
    let textures =
        match TextureRegistry::load_ordered(mods.mods.iter().map(|m| m.directory.as_path())) {
            Ok(r) => r,
            Err(e) => return Err(format!("{} texture load failed: {e}", side.as_str())),
        };
    info!(
        "[{}] texture registry: {} texture(s)",
        side.as_str(),
        textures.texture_count(),
    );
    if let Err(e) = blocks.validate_textures(&textures) {
        return Err(format!(
            "{} block texture validation failed: {e}",
            side.as_str()
        ));
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
    let worlds = ctx.pending_worlds.lock().unwrap();
    for candidate in worlds.iter() {
        candidate.validate()?;
        for (role, id) in candidate.terrain.entries() {
            let slot = blocks.slot_of(id).ok_or_else(|| {
                format!("preset {} terrain.{role}: unknown block {id}", candidate.id)
            })?;
            if blocks.def(slot).footprint != [[0, 0, 0]] {
                return Err(format!(
                    "preset {} terrain.{role}: generator requires a single-cell block, got {id}",
                    candidate.id
                ));
            }
        }
        for entry in &candidate.population {
            if npc_kinds.get(&entry.kind).is_none() {
                return Err(format!(
                    "preset {} population: unknown NPC kind {}",
                    candidate.id, entry.kind
                ));
            }
        }
        if !ctx
            .pending_movement
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.id == candidate.movement_profile)
            || !ctx
                .pending_settlements
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.id == candidate.settlement)
        {
            return Err(format!(
                "preset {}: unknown movement profile or settlement definition",
                candidate.id
            ));
        }
    }
    let preset = match selected {
        Some(id) => worlds
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("unknown world preset {id}"))?,
        None if worlds.len() == 1 || (side == Side::Client && !worlds.is_empty()) => &worlds[0],
        None => {
            return Err(format!(
                "select a world preset; available: {}",
                worlds
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    .clone();
    preset.validate()?;
    let movement = ctx.pending_movement.lock().unwrap();
    for profile in movement.iter() {
        if !profile.height.is_finite()
            || !(0.1..=8.).contains(&profile.height)
            || !profile.radius.is_finite()
            || !(0.05..=4.).contains(&profile.radius)
            || profile.step_up > 8
            || profile.max_drop > 64
        {
            return Err(format!(
                "movement {}: invalid body or traversal limits",
                profile.id
            ));
        }
    }
    if !movement.iter().any(|p| p.id == preset.movement_profile) {
        return Err(format!(
            "preset {}: unknown movement profile {}",
            preset.id, preset.movement_profile
        ));
    }
    for (_, kind) in npc_kinds.iter() {
        if let Some(profile) = &kind.movement_profile
            && !movement.iter().any(|candidate| &candidate.id == profile)
        {
            return Err(format!(
                "npc kind {} references unknown movement profile {profile}",
                kind.id
            ));
        }
    }
    let settlements = ctx.pending_settlements.lock().unwrap();
    for definition in settlements.iter() {
        if definition.outdoor_link_range > 1024
            || !movement.iter().any(|p| p.id == definition.movement_profile)
        {
            return Err(format!(
                "settlement {}: invalid link range or movement profile",
                definition.id
            ));
        }
    }
    if !settlements.iter().any(|p| p.id == preset.settlement) {
        return Err(format!(
            "preset {}: unknown settlement definition {}",
            preset.id, preset.settlement
        ));
    }
    info!(
        "compiled {} block architecture variants",
        blocks.architecture.len() * 4
    );
    for (_, def) in blocks.iter() {
        ctx.publish_definition("blocks", def.id.as_str(), def);
    }
    let capabilities = ctx.pending_capabilities.lock().unwrap();
    let mut capability_references =
        block_junk_mod_api::capabilities::CapabilityReferences::default();
    for (_, def) in blocks.iter() {
        capability_references
            .tags
            .extend(def.tags.iter().map(|tag| tag.as_str().to_owned()));
        capability_references.interactions.extend(
            def.facility_slots
                .iter()
                .flat_map(|slot| slot.interactions.iter().cloned()),
        );
        capability_references
            .environmental_attributes
            .extend(def.environmental_attributes.keys().cloned());
    }
    for capability in capabilities.iter() {
        capability
            .validate(&capability_references)
            .map_err(|error| {
                ctx.explain_validation(&format!("capability {}: {error}", capability.id))
            })?;
        ctx.publish_definition("capabilities", &capability.id, capability);
    }
    let asset_fingerprint = crate::mod_assets::validate(
        &block_junk_scripting::resources::ResourceRoots::from_registry(&mods),
        &ctx.definition_snapshot(),
    )?;
    let preset = SelectedWorld(preset);
    let mut selected_hash = ctx.fingerprint();
    for byte in mods
        .source_fingerprint
        .to_le_bytes()
        .into_iter()
        .chain(asset_fingerprint)
    {
        selected_hash ^= u64::from(byte);
        selected_hash = selected_hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let catalog = WorldCatalog {
        presets: worlds.clone(),
        base_fingerprint: selected_hash,
    };
    let fingerprint = catalog.fingerprint(&preset.0);
    let contracts = crate::content_contracts::ContentContracts::compile(&blocks, &movement)
        .with_upgrades(
            ctx.pending_world_upgrades.lock().unwrap().clone(),
            &catalog.presets,
        )?;
    Ok(LoadResult {
        contracts,
        mods,
        fingerprint,
        preset,
        catalog,
        blocks,
        slots,
        items,
        recipes,
        needs,
        npc_kinds,
        textures,
        animations,
        work_defaults,
        civilization_params,
        world_rules: crate::world_rules::WorldRules {
            capabilities: capabilities
                .iter()
                .map(|rule| (rule.id.clone(), rule.clone()))
                .collect(),
            movement: movement
                .iter()
                .map(|rule| (rule.id.clone(), rule.clone()))
                .collect(),
            settlements: settlements
                .iter()
                .map(|rule| (rule.id.clone(), rule.clone()))
                .collect(),
        },
        ui_toasts: ctx.ui_toast_queue(),
    })
}

pub(crate) fn validate_mods(args: &[String]) -> i32 {
    let mut root = PathBuf::from(MODS_DIR);
    let mut preset = None;
    let mut save = None;
    let mut rules_out = None;
    let json = args.iter().any(|a| a == "--json");
    let result = (|| -> Result<serde_json::Value, String> {
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--mods-dir" => {
                    root = PathBuf::from(args.next().ok_or("--mods-dir requires a directory")?)
                }
                "--preset" => preset = Some(args.next().ok_or("--preset requires an id")?.clone()),
                "--save" => save = Some(args.next().ok_or("--save requires a save name")?.clone()),
                "--rules-out" => {
                    rules_out = Some(PathBuf::from(
                        args.next().ok_or("--rules-out requires a path")?,
                    ))
                }
                "--json" => {}
                other => return Err(format!("unknown validation argument {other}")),
            }
        }
        let mut content = load_content(Side::Server, &root, preset.as_deref())?;
        if let Some(name) = save {
            let mut saved = crate::save::read_save(&name).map_err(|e| e.to_string())?;
            saved = crate::save_upgrade::migrate(
                &saved,
                &content.contracts,
                &content.blocks,
                &content.preset.0,
            )?;
            crate::save::remap_block_slots(&mut saved, |id| content.blocks.slot_of(&id.into()))
                .map_err(|e| e.to_string())?;
            crate::server::validate_runtime_save_references(
                &name,
                &saved,
                &content.blocks,
                &content.items,
                &content.recipes,
                &content.npc_kinds,
                &content.needs,
            )
            .map_err(|e| e.to_string())?;
            crate::action_jobs::validate_owners(&saved, &content.mods)?;
            content
                .mods
                .restore_events(&saved.world.as_ref().ok_or("save has no world")?.events)?;
            content.mods.restore_state(&saved.mod_state)?;
        }
        content.mods.activate_runtime().map_err(|e| e.to_string())?;
        if let Some(path) = rules_out {
            std::fs::write(
                path,
                capability_reference(&content.world_rules.capabilities),
            )
            .map_err(|error| format!("cannot write capability reference: {error}"))?;
        }
        Ok(
            serde_json::json!({"valid":true,"contracts":content.contracts.inspection(&content.preset.0),"preset":content.preset.0.id,"mods":content.mods.mods.iter().map(|m| &m.manifest.id).collect::<Vec<_>>(),"blocks":content.blocks.slot_count(),"items":content.items.slot_count(),"fingerprint":content.fingerprint.0}),
        )
    })();
    match result {
        Ok(report) => {
            if json {
                println!("{report}");
            } else {
                println!("Content valid: {report}");
            }
            0
        }
        Err(error) => {
            if json {
                println!("{}", serde_json::json!({"valid":false,"error":error}));
            } else {
                eprintln!("Content invalid: {error}");
            }
            1
        }
    }
}

fn capability_reference(
    rules: &std::collections::BTreeMap<
        String,
        block_junk_mod_api::capabilities::CapabilityDefinition,
    >,
) -> String {
    use block_junk_mod_api::capabilities::{CapabilityScope, Expression};
    use std::fmt::Write;
    fn expression(value: &Expression) -> String {
        match value {
            Expression::Number { value } => value.to_string(),
            Expression::Boolean { value } => value.to_string(),
            Expression::Observe { query } => query.label(),
            Expression::Compare {
                operation,
                left,
                right,
            } => format!("{} {operation:?} {}", expression(left), expression(right)),
            Expression::Calculate {
                operation,
                operands,
            } => format!(
                "{operation:?}({})",
                operands
                    .iter()
                    .map(expression)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expression::All { conditions } => format!(
                "all({})",
                conditions
                    .iter()
                    .map(expression)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expression::Any { conditions } => format!(
                "any({})",
                conditions
                    .iter()
                    .map(expression)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expression::Not { condition } => format!("not({})", expression(condition)),
        }
    }
    let mut out = String::from(
        "# Generated capability rule reference\n\nThis file is generated from the final validated content set.\n\n",
    );
    for rule in rules.values() {
        let _ = writeln!(out, "## {} (`{}`)\n", rule.display_name, rule.id);
        match &rule.scope {
            CapabilityScope::ArchitecturalSpace => out.push_str("Scope: architectural space.\n\n"),
            CapabilityScope::Outdoor {
                anchor_interaction,
                range,
                allow_portals,
            } => {
                let _ = writeln!(
                    out,
                    "Scope: outdoor service area anchored at `{anchor_interaction}`, range {range}, portal traversal {allow_portals}.\n"
                );
            }
        }
        if rule.requirements.is_empty() {
            out.push_str("Requirements: none.\n\n");
        } else {
            out.push_str("Requirements:\n\n");
            for requirement in &rule.requirements {
                let _ = writeln!(
                    out,
                    "- `{}`: {} — `{}`",
                    requirement.key,
                    requirement.description,
                    expression(&requirement.condition)
                );
            }
            out.push('\n');
        }
        if let Some(capacity) = &rule.capacity {
            let _ = writeln!(out, "Capacity: `{}`\n", expression(capacity));
        }
        if !rule.attributes.is_empty() {
            out.push_str("Attributes:\n\n");
            for (key, value) in &rule.attributes {
                let _ = writeln!(out, "- `{key}`: `{}`", expression(value));
            }
            out.push('\n');
        }
    }
    // The final section needs only its terminating newline.
    out.pop();
    out
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WorldHooks;

fn activate_server_runtime(
    mut mods: ResMut<ServerMods>,
    mut guard: ResMut<crate::server::SaveWriteGuard>,
    mut exit: MessageWriter<bevy::app::AppExit>,
) {
    if guard.reason.is_some() {
        exit.write(bevy::app::AppExit::error());
        return;
    }
    if let Err(error) = mods.0.activate_runtime() {
        guard.reason = Some(error.to_string());
        error!(%error, "cannot activate runtime hooks");
        exit.write(bevy::app::AppExit::error());
    }
}

#[derive(Resource, Default)]
pub(crate) struct ModBehaviorFailures(
    pub std::collections::VecDeque<block_junk_scripting::events::BehaviorFailure>,
);

fn cleanup_failed_behaviors(
    mut mods: ResMut<ServerMods>,
    mut construction: crate::construction::ConstructionService,
    mut bookings: ResMut<crate::craft_stations::CraftBookings>,
    mut interactions: crate::facility_interactions::InteractionService,
    actors: Query<(Entity, &crate::npc::NpcKind), Without<crate::npc::BrainDisabled>>,
    mut failures: ResMut<ModBehaviorFailures>,
    mut commands: Commands,
) {
    for failure in mods.0.take_behavior_failures() {
        construction.disable_mod_jobs(&failure.owner, &failure.reason);
        bookings.disable_mod_jobs(&failure.owner, &failure.reason);
        interactions.disable_mod(&failure.owner, &failure.reason);
        for (entity, kind) in &actors {
            if mods.0.planner_owner(&kind.0).as_deref() == Some(&failure.owner) {
                commands.entity(entity).insert(crate::npc::BrainDisabled {
                    reason: failure.reason.clone(),
                });
            }
        }
        error!(owner=%failure.owner,behavior=%failure.behavior,cause=?failure.cause,reason=%failure.reason,"mod behavior disabled; releasing owned NPC work");
        if failures.0.len() == 128 {
            failures.0.pop_front();
        }
        failures.0.push_back(failure);
    }
}

fn retain_job_causes(
    journal: Res<crate::spatial_events::SpatialJournal>,
    mut edits: MessageReader<CellEdit>,
    mods: Res<ServerMods>,
    construction: Res<crate::construction::ConstructionLedger>,
    crafting: Res<crate::craft_stations::CraftBookings>,
    interactions: Res<crate::facility_interactions::FacilityInteractions>,
) {
    mods.0.retain_job_causes(
        construction
            .jobs
            .records
            .values()
            .chain(crafting.jobs.records.values())
            .chain(interactions.jobs.records.values())
            .filter(|job| !job.observed)
            .filter_map(|job| job.cause)
            .chain(journal.causes())
            .chain(edits.read().filter_map(|edit| edit.cause)),
    );
}

fn advance_mod_events(mut mods: ResMut<ServerMods>) {
    mods.0.advance_events();
}

fn dispatch_block_placed(
    mut reader: MessageReader<CellEdit>,
    mut mods: ResMut<ServerMods>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        if edit.slot == edit.prev_slot {
            continue;
        }
        let event = BlockPlacedEvent {
            pos: BlockPos {
                x: edit.world.x,
                y: edit.world.y,
                z: edit.world.z,
            },
            block: registry.id_of(edit.slot).clone(),
        };
        if let Err(error) = mods.0.enqueue_event("block_placed", &event, edit.cause) {
            error!(%error,"mod event rejected");
        }
    }
}

#[cfg(test)]
pub(crate) fn install_command_fixture(app: &mut App, root: &std::path::Path) {
    crate::facility_inventory::install_fixture(app);
    let context = LoadContext::new();
    let registry = ModRegistry::load_dir(Side::Server, root, &context).unwrap();
    app.insert_resource(ServerMods(registry))
        .init_resource::<crate::containers::Containers>()
        .init_resource::<crate::craft_stations::CraftStations>()
        .init_resource::<crate::npc_registry::NpcKindRegistry>()
        .init_resource::<crate::server::PendingTerrainEffects>()
        .add_systems(
            First,
            (advance_mod_events, crate::mod_commands::execute_batches).chain(),
        )
        .add_systems(
            Update,
            dispatch_block_placed.in_set(crate::GameSet::PostSimulation),
        );
}

#[cfg(test)]
pub(crate) fn install_independent_fixture(app: &mut App, side: Side) {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mods/independent");
    let content = load_content(side, &root, None).unwrap();
    app.insert_resource(crate::blocks::TerrainSlots::from_registry(
        &content.blocks,
        &content.preset.0,
    ));
    app.insert_resource(content.world_rules)
        .insert_resource(content.blocks)
        .insert_resource(content.items)
        .insert_resource(content.recipes)
        .insert_resource(content.npc_kinds)
        .insert_resource(content.fingerprint)
        .insert_resource(content.catalog)
        .insert_resource(content.preset);
}

#[cfg(test)]
mod foundation_tests {
    use super::*;
    mod capability_tests;

    fn independent() -> LoadResult {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/mods/independent");
        load_content(Side::Server, &root, Some("foundation:plain")).unwrap()
    }

    fn command_app() -> App {
        command_app_with_content(independent())
    }

    fn command_app_with_content(mut content: LoadResult) -> App {
        content.mods.activate_runtime().unwrap();
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins)
            .insert_resource(ServerMods(content.mods))
            .insert_resource(content.world_rules)
            .insert_resource(content.preset)
            .insert_resource(content.blocks)
            .insert_resource(content.items)
            .insert_resource(content.npc_kinds)
            .insert_resource(content.recipes)
            .init_resource::<crate::containers::Containers>()
            .init_resource::<crate::craft_stations::CraftStations>()
            .init_resource::<crate::plans::Plans>()
            .init_resource::<crate::construction::ConstructionLedger>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::haul::HaulStore>()
            .init_resource::<crate::facility_access::FacilityAccess>()
            .init_resource::<crate::voxel::ChunkMap>()
            .add_message::<crate::protocol::CellEdit>()
            .init_resource::<crate::server::PendingTerrainEffects>()
            .add_systems(Update, crate::mod_commands::execute_batches);
        app
    }

    #[test]
    fn portal_batches_are_atomic_and_cannot_close_through_an_actor() {
        use block_junk_mod_api::{architecture::PlacementId, blocks::Cardinal};
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        app.insert_resource(crate::placements::portal_fixture_registry());
        let chunk = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![Command::EditTerrain {
                cell: [2, 2, 2],
                expected_revision: 0,
                block: Some("test:door".into()),
                orientation: Cardinal::North,
            }],
        );
        app.update();
        let close = || Command::SetPortal {
            placement_id: 1,
            revision: 1,
            key: "passage".into(),
            open: false,
        };
        send(
            &app,
            2,
            vec![
                close(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .records[&PlacementId(1)]
                .revision,
            1
        );
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(chunk)
                .unwrap()
                .revision,
            1
        );
        let actor = app
            .world_mut()
            .spawn((
                crate::protocol::Actor,
                crate::protocol::AvatarPose {
                    translation: crate::physics::standing_pose_translation(IVec3::splat(2)),
                    yaw: 0.,
                },
            ))
            .id();
        send(&app, 3, vec![close()]);
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .records[&PlacementId(1)]
                .revision,
            1
        );
        app.world_mut().despawn(actor);
        send(&app, 4, vec![close()]);
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .records[&PlacementId(1)]
                .revision,
            2
        );
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(chunk)
                .unwrap()
                .revision,
            2
        );
        assert!(matches!(
            app.world()
                .get::<crate::voxel::ChunkEntities>(chunk)
                .unwrap()
                .get(IVec3::splat(2)),
            Some(crate::voxel::EntryKind::Anchor {
                portal_open: Some(0),
                ..
            })
        ));
        send(&app, 5, vec![close()]);
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(chunk)
                .unwrap()
                .revision,
            2
        );
        send(
            &app,
            6,
            vec![Command::MovePlacement {
                placement_id: 1,
                revision: 2,
                anchor: [5, 2, 2],
                orientation: Cardinal::East,
            }],
        );
        app.update();
        assert!(matches!(
            app.world()
                .get::<crate::voxel::ChunkEntities>(chunk)
                .unwrap()
                .get(IVec3::new(5, 2, 2)),
            Some(crate::voxel::EntryKind::Anchor {
                portal_open: Some(0),
                ..
            })
        ));
    }

    #[test]
    fn live_settlement_links_follow_committed_obstructions_and_ignore_unrequested_chunks() {
        use crate::settlements::{SettlementDependencies, SettlementWork, Settlements};
        use block_junk_mod_api::blocks::Cardinal;
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        app.world_mut()
            .resource_mut::<crate::world_rules::WorldRules>()
            .settlements
            .get_mut("foundation:colony")
            .unwrap()
            .outdoor_link_range = 4;
        app.init_resource::<Settlements>()
            .init_resource::<SettlementWork>()
            .init_resource::<SettlementDependencies>()
            .init_resource::<crate::spatial_geometry::StructuralSummaries>()
            .init_resource::<crate::live_spaces::PublishedSpaces>()
            .init_resource::<crate::server::SpatialSubscriptions>()
            .init_resource::<crate::server::ActiveSimulationChunks>()
            .add_systems(
                PostUpdate,
                (
                    crate::spatial_geometry::refresh_structural_summaries,
                    crate::spatial_geometry::materialize_facilities,
                    crate::settlements::refresh,
                )
                    .chain(),
            );
        app.world_mut()
            .resource_mut::<SettlementWork>()
            .operation_budget = 127;
        let floor = app
            .world()
            .resource::<BlockRegistry>()
            .require("foundation:stone");
        let mut chunk = crate::voxel::Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        for x in 0..32 {
            for z in 0..32 {
                chunk.apply_committed_cell(
                    crate::voxel::world_to_chunk(IVec3::new(x, 0, z)).1,
                    floor,
                );
            }
        }
        let entity = app
            .world_mut()
            .spawn((chunk.clone(), crate::voxel::ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            [
                (12, "foundation:bench"),
                (16, "foundation:table"),
                (20, "foundation:bed"),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (x, block))| Command::EditTerrain {
                cell: [x, 1, 16],
                expected_revision: index as u64,
                block: Some(block.into()),
                orientation: Cardinal::North,
            })
            .collect(),
        );
        let settle = |app: &mut App| {
            for _ in 0..20_000 {
                app.update();
                assert!(
                    app.world()
                        .resource::<SettlementWork>()
                        .operations_last_tick
                        <= 127
                );
                if !app.world().resource::<Settlements>().pending {
                    return;
                }
            }
            panic!("finite live settlement work failed to converge");
        };
        settle(&mut app);
        assert_eq!(app.world().resource::<Settlements>().groups.len(), 1);
        let original = app.world().resource::<Settlements>().by_facility.clone();
        let barrier: Vec<_> = (10..=22).flat_map(|z| [[18, 1, z], [18, 2, z]]).collect();
        let revision = app
            .world()
            .get::<crate::voxel::Chunk>(entity)
            .unwrap()
            .revision;
        send(
            &app,
            2,
            barrier
                .iter()
                .enumerate()
                .map(|(index, cell)| Command::EditTerrain {
                    cell: *cell,
                    expected_revision: revision + index as u64,
                    block: Some("foundation:stone".into()),
                    orientation: Cardinal::North,
                })
                .collect(),
        );
        app.update();
        settle(&mut app);
        assert_eq!(app.world().resource::<Settlements>().groups.len(), 2);
        let revision = app
            .world()
            .get::<crate::voxel::Chunk>(entity)
            .unwrap()
            .revision;
        send(
            &app,
            3,
            barrier
                .iter()
                .enumerate()
                .map(|(index, cell)| Command::EditTerrain {
                    cell: *cell,
                    expected_revision: revision + index as u64,
                    block: None,
                    orientation: Cardinal::North,
                })
                .collect(),
        );
        app.update();
        settle(&mut app);
        assert_eq!(app.world().resource::<Settlements>().by_facility, original);
        let revision = app.world().resource::<Settlements>().revision;
        let far = crate::protocol::ChunkCoord(IVec3::new(100, 0, 100));
        let extra = app
            .world_mut()
            .spawn((chunk, crate::voxel::ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(far, extra);
        for _ in 0..4 {
            app.update();
            assert_eq!(
                app.world()
                    .resource::<SettlementWork>()
                    .operations_last_tick,
                0
            );
        }
        assert_eq!(app.world().resource::<Settlements>().revision, revision);
        assert!(
            !app.world()
                .resource::<SettlementDependencies>()
                .reads
                .contains(&far.0.to_array())
        );
        assert!(
            !app.world()
                .resource::<crate::spatial_geometry::StructuralSummaries>()
                .completed
                .contains_key(&far.0.to_array())
        );
        let bytes = bincode::serde::encode_to_vec(
            app.world().resource::<crate::placements::Placements>(),
            bincode::config::standard(),
        )
        .unwrap();
        let (mut restored, _): (crate::placements::Placements, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        restored
            .rebuild(app.world().resource::<BlockRegistry>())
            .unwrap();
        app.insert_resource(restored)
            .insert_resource(Settlements::default())
            .insert_resource(SettlementWork::default())
            .insert_resource(SettlementDependencies::default())
            .insert_resource(crate::spatial_geometry::StructuralSummaries::default());
        app.world_mut()
            .resource_mut::<SettlementWork>()
            .operation_budget = 127;
        settle(&mut app);
        assert_eq!(app.world().resource::<Settlements>().by_facility, original);
    }

    fn population_app(initialized: bool) -> App {
        let content = independent();
        let terrain =
            crate::blocks::TerrainSlots::from_registry(&content.blocks, &content.preset.0);
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins);
        app.insert_resource(content.preset)
            .insert_resource(content.npc_kinds)
            .insert_resource(terrain)
            .insert_resource(crate::npc::PopulationInitialized(initialized))
            .init_resource::<crate::npc::NpcIdAllocator>()
            .init_resource::<crate::voxel::ChunkMap>()
            .init_resource::<crate::server::SaveWriteGuard>()
            .add_systems(Startup, crate::npc::spawn_initial_population);
        app
    }

    #[test]
    fn retired_space_state_resolution_is_owned_and_atomic() {
        use crate::space_identity::{Membership, SpaceDraft, SpaceIdentities};
        use block_junk_scripting::{
            commands::{Command, CommandBatch},
            state::StateScope,
        };
        let mut app = command_app();
        let draft = |cells: Vec<[i32; 3]>| SpaceDraft {
            membership: Membership::from_cells(cells).unwrap(),
            content_fingerprint: [1; 32],
        };
        let (parents, _) = app
            .world_mut()
            .resource_mut::<SpaceIdentities>()
            .reconcile(&Default::default(), &[draft(vec![[1, 1, 1], [2, 1, 1]])])
            .unwrap();
        let parent = parents[0];
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![Command::SetState {
                scope: StateScope::Space(parent.0),
                expected_revision: 0,
                json: "{\"kept\":7}".into(),
            }],
        );
        app.update();
        let (children, _) = app
            .world_mut()
            .resource_mut::<SpaceIdentities>()
            .reconcile(
                &std::collections::BTreeSet::from([parent]),
                &[draft(vec![[1, 1, 1]]), draft(vec![[2, 1, 1]])],
            )
            .unwrap();
        send(
            &app,
            2,
            vec![
                Command::ResolveRetiredSpaceState {
                    space_id: parent.0,
                    state_revision: 1,
                },
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert!(
            !app.world().resource::<SpaceIdentities>().records[&parent]
                .resolved_state_owners
                .contains("foundation")
        );
        assert!(
            app.world()
                .resource::<ServerMods>()
                .0
                .mod_state
                .0
                .lock()
                .unwrap()
                .contains_key(&("foundation".into(), StateScope::Space(parent.0)))
        );
        assert!(children.iter().all(|id| {
            !app.world()
                .resource::<ServerMods>()
                .0
                .mod_state
                .0
                .lock()
                .unwrap()
                .contains_key(&("foundation".into(), StateScope::Space(id.0)))
        }));
        send(
            &app,
            3,
            vec![
                Command::ReadSpaceLineage {
                    space_id: parent.0,
                    revision: app.world().resource::<SpaceIdentities>().records[&parent].revision,
                    direction: block_junk_mod_api::architecture::LineageDirection::Successors,
                    after_id: 0,
                    limit: 1,
                },
                Command::SetState {
                    scope: StateScope::Space(children[0].0),
                    expected_revision: 0,
                    json: "{\"kept\":7}".into(),
                },
                Command::ResolveRetiredSpaceState {
                    space_id: parent.0,
                    state_revision: 1,
                },
            ],
        );
        app.update();
        assert!(
            app.world().resource::<SpaceIdentities>().records[&parent]
                .resolved_state_owners
                .contains("foundation")
        );
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .mod_state
                .0
                .lock()
                .unwrap()
                .contains_key(&("foundation".into(), StateScope::Space(parent.0)))
        );
        send(
            &app,
            4,
            vec![Command::SetState {
                scope: StateScope::Space(parent.0),
                expected_revision: 0,
                json: "{}".into(),
            }],
        );
        app.update();
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .results
                .lock()
                .unwrap()["foundation"]
                .back()
                .unwrap()
                .accepted
        );
    }

    #[test]
    fn oversized_query_results_reject_the_whole_batch_before_state_commits() {
        use block_junk_mod_api::authority::{JobState, JobTarget, ReservationOwner};
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        {
            let mut bookings = app
                .world_mut()
                .resource_mut::<crate::craft_stations::CraftBookings>();
            for order_id in 1..=64 {
                let id = bookings
                    .jobs
                    .start(
                        ReservationOwner::Npc { id: 1 },
                        ReservationOwner::Behavior {
                            mod_id: "foundation".into(),
                            registration_id: "planner".into(),
                        },
                        JobTarget::CraftOrder {
                            order_id,
                            order_revision: 1,
                        },
                    )
                    .unwrap();
                bookings
                    .jobs
                    .finish(id, JobState::Failed, Some("x".repeat(1024)));
            }
        }
        app.world()
            .resource::<ServerMods>()
            .0
            .commands
            .batches
            .lock()
            .unwrap()
            .push_back(CommandBatch {
                id: 1,
                mod_id: "foundation".into(),
                behavior: "read-results".into(),
                cause: None,
                commands: vec![
                    Command::SetState {
                        scope: block_junk_scripting::state::StateScope::World,
                        expected_revision: 0,
                        json: "{\"marker\":1}".into(),
                    },
                    Command::ReadCraftJobs {
                        after_id: 0,
                        limit: 64,
                    },
                    Command::ReadCraftJobs {
                        after_id: 0,
                        limit: 64,
                    },
                    Command::ReadCraftJobs {
                        after_id: 0,
                        limit: 64,
                    },
                ],
            });
        app.update();
        let mods = &app.world().resource::<ServerMods>().0;
        assert!(mods.saved_state().is_empty());
        let results = mods.commands.results.lock().unwrap();
        let result = results["foundation"].back().unwrap();
        assert!(!result.accepted);
        assert!(result.receipts.is_empty());
        assert!(
            result
                .reason
                .as_ref()
                .unwrap()
                .contains("result byte limit")
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .jobs
                .records
                .values()
                .all(|job| !job.observed)
        );
    }

    #[test]
    fn command_batch_rolls_back_state_and_construction_on_late_conflict() {
        use block_junk_scripting::{
            commands::{Command, CommandBatch},
            state::StateScope,
        };
        let mut app = command_app();
        let mut plans = crate::plans::Plans::default();
        let mut ledger = crate::construction::ConstructionLedger::default();
        let mut project = crate::protocol::PlanState::new(
            crate::protocol::PlanKind::Remove,
            vec![crate::protocol::MaterialEntry {
                item: crate::items::ItemSlot(0),
                needed: 3,
                present: 2,
            }],
        );
        project.footprint = vec![IVec3::ZERO];
        assert!(ledger.designate(&mut plans, IVec3::ZERO, project));
        let original = plans.get(IVec3::ZERO).unwrap().clone();
        app.insert_resource(plans).insert_resource(ledger);
        let item = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .id_of(crate::items::ItemSlot(0))
            .as_str()
            .to_owned();
        let carrier = app
            .world_mut()
            .spawn((
                crate::npc::NpcId(77),
                crate::protocol::Carrying {
                    item: Some(crate::items::ItemSlot(0)),
                    count: 3,
                },
                crate::protocol::AvatarPose {
                    translation: Vec3::ZERO,
                    yaw: 0.,
                },
            ))
            .id();
        let delivery = Command::DeliverProject {
            project_id: original.project_id,
            revision: original.revision,
            npc_id: 77,
            item,
            count: 1,
        };
        let commands = vec![
            delivery.clone(),
            Command::CancelProject {
                project_id: original.project_id,
                revision: original.revision,
            },
            Command::SetState {
                scope: StateScope::World,
                expected_revision: 0,
                json: "{\"completed\":1}".into(),
            },
            Command::CancelProject {
                project_id: 999,
                revision: 1,
            },
        ];
        app.world()
            .resource::<ServerMods>()
            .0
            .commands
            .batches
            .lock()
            .unwrap()
            .push_back(CommandBatch {
                cause: None,
                id: 1,
                mod_id: "foundation".into(),
                behavior: "test".into(),
                commands,
            });
        app.update();
        let mods = &app.world().resource::<ServerMods>().0;
        assert!(mods.saved_state().is_empty());
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(carrier)
                .unwrap()
                .count,
            3
        );
        assert_eq!(
            app.world()
                .resource::<crate::plans::Plans>()
                .get(IVec3::ZERO),
            Some(&original)
        );
        assert!(!mods.commands.results.lock().unwrap()["foundation"][0].accepted);
        assert!(
            app.world()
                .resource::<crate::construction::ConstructionLedger>()
                .pending_refunds
                .is_empty()
        );
        mods.commands
            .batches
            .lock()
            .unwrap()
            .push_back(CommandBatch {
                cause: None,
                id: 2,
                mod_id: "foundation".into(),
                behavior: "test".into(),
                commands: vec![Command::SetState {
                    scope: StateScope::World,
                    expected_revision: 0,
                    json: "{}".into(),
                }],
            });
        app.update();
        let mods = &app.world().resource::<ServerMods>().0;
        assert_eq!(mods.saved_state()[0].revision, 1);
        assert!(mods.commands.results.lock().unwrap()["foundation"][1].accepted);
        mods.commands
            .batches
            .lock()
            .unwrap()
            .push_back(CommandBatch {
                cause: None,
                id: 3,
                mod_id: "foundation".into(),
                behavior: "test".into(),
                commands: vec![delivery.clone()],
            });
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(carrier)
                .unwrap()
                .count,
            2
        );
        assert_eq!(
            app.world()
                .resource::<crate::plans::Plans>()
                .get(IVec3::ZERO)
                .unwrap()
                .materials[0]
                .present,
            3
        );
        let mods = &app.world().resource::<ServerMods>().0;
        assert!(mods.commands.results.lock().unwrap()["foundation"][2].accepted);
        mods.commands
            .batches
            .lock()
            .unwrap()
            .push_back(CommandBatch {
                cause: None,
                id: 4,
                mod_id: "foundation".into(),
                behavior: "test".into(),
                commands: vec![delivery],
            });
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(carrier)
                .unwrap()
                .count,
            2
        );
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .results
                .lock()
                .unwrap()["foundation"][3]
                .accepted
        );
    }

    #[test]
    fn scripted_named_interaction_and_spawn_roll_back_together_on_later_rejection() {
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        app.add_plugins((TransformPlugin, bevy::state::app::StatesPlugin));
        app.add_plugins(lightyear::prelude::server::ServerPlugins {
            tick_duration: std::time::Duration::from_secs_f64(1. / 60.),
        });
        app.add_plugins(crate::network::ProtocolPlugin);
        app.finish();
        app.cleanup();
        let bed = app
            .world()
            .resource::<crate::blocks::BlockRegistry>()
            .require("foundation:bed");
        let stone = app
            .world()
            .resource::<crate::blocks::BlockRegistry>()
            .require("foundation:stone");
        let cell = IVec3::new(4, 2, 4);
        let foot = cell + IVec3::X;
        let mut chunk = crate::voxel::Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        chunk.set(crate::voxel::world_to_chunk(cell).1, bed);
        chunk.set(crate::voxel::world_to_chunk(foot - IVec3::Y).1, stone);
        let mut placements = crate::placements::Placements::default();
        placements
            .index_existing(
                cell,
                bed,
                block_junk_mod_api::blocks::Cardinal::East,
                app.world().resource::<crate::blocks::BlockRegistry>(),
                |at| Some(chunk.get(crate::voxel::world_to_chunk(at).1)),
            )
            .unwrap();
        app.insert_resource(placements);
        app.world_mut()
            .resource_mut::<crate::facility_access::FacilityAccess>()
            .slots
            .insert(
                (
                    block_junk_mod_api::architecture::UseSlotId {
                        placement: block_junk_mod_api::architecture::PlacementId(1),
                        key: "primary".into(),
                    },
                    "foundation:humanoid".into(),
                ),
                crate::facility_access::SlotAccess {
                    id: block_junk_mod_api::architecture::UseSlotId {
                        placement: block_junk_mod_api::architecture::PlacementId(1),
                        key: "primary".into(),
                    },
                    placement_revision: 1,
                    profile: "foundation:humanoid".into(),
                    interactions: vec!["sleep".into()],
                    exclusive: true,
                    approaches: vec![crate::facility_access::ApproachAccess {
                        cell: foot.to_array(),
                        status: crate::facility_access::AccessStatus::Usable,
                        obstruction: None,
                        reason: "fixture authored approach".into(),
                    }],
                },
            );
        let entity = app
            .world_mut()
            .spawn((chunk, crate::voxel::ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        let spawn = Command::SpawnNpc {
            npc_kind: "foundation:settler".into(),
            position: [5.5, 3.7, 4.5],
        };
        let start = Command::StartInteraction {
            placement_id: 1,
            revision: 1,
            npc_id: 1,
            slot_key: "primary".into(),
            action: "sleep".into(),
            approach: foot.to_array(),
        };
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![
                spawn.clone(),
                start.clone(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(app.world().resource::<crate::npc::NpcIdAllocator>().next, 1);
        assert!(
            app.world()
                .resource::<crate::facility_interactions::FacilityInteractions>()
                .jobs
                .records
                .is_empty()
        );
        assert!(
            !app.world()
                .resource::<crate::interactables::InteractionClaims>()
                .is_taken_by_other(cell, crate::npc::NpcId(99))
        );
        send(&app, 2, vec![spawn, start.clone(), start]);
        app.update();
        let results = app
            .world()
            .resource::<ServerMods>()
            .0
            .commands
            .results
            .lock()
            .unwrap();
        assert!(!results["foundation"][0].accepted);
        assert!(
            results["foundation"][1].accepted,
            "{:?}",
            results["foundation"][1]
        );
        drop(results);
        let jobs = app
            .world()
            .resource::<crate::facility_interactions::FacilityInteractions>();
        assert_eq!(jobs.jobs.records.len(), 1);
        assert_eq!(
            jobs.jobs.records[&1].state,
            block_junk_mod_api::authority::JobState::Accepted
        );
        assert!(
            app.world()
                .resource::<crate::interactables::InteractionClaims>()
                .is_taken_by_other(cell, crate::npc::NpcId(99))
        );
        crate::server::install_interaction_fixture(&mut app);
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        for _ in 0..20 {
            app.update();
        }
        let jobs = app
            .world()
            .resource::<crate::facility_interactions::FacilityInteractions>();
        assert_eq!(
            jobs.jobs.records[&1].state,
            block_junk_mod_api::authority::JobState::Completed,
            "{:?}",
            jobs.jobs.records[&1]
        );
        assert!(
            !app.world()
                .resource::<crate::interactables::InteractionClaims>()
                .is_taken_by_other(cell, crate::npc::NpcId(99))
        );
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world()
                .resource::<crate::facility_interactions::FacilityInteractions>()
                .jobs
                .records[&1]
                .revision,
            3
        );
    }

    #[test]
    fn scripted_craft_jobs_commit_spawn_and_inputs_together_and_report_outcomes() {
        use block_junk_mod_api::authority::JobState;
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        app.add_plugins((TransformPlugin, bevy::state::app::StatesPlugin));
        app.add_plugins(lightyear::prelude::server::ServerPlugins {
            tick_duration: std::time::Duration::from_secs_f64(1. / 60.),
        });
        app.add_plugins(crate::network::ProtocolPlugin);
        app.init_resource::<crate::server::ClientAvatars>()
            .init_resource::<crate::server::RequestBudgets>()
            .init_resource::<crate::server::SpatialSubscriptions>();
        app.add_plugins(crate::craft_stations::CraftStationsServerPlugin);
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        app.finish();
        app.cleanup();
        let stone = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .slot_of(&"foundation:stone".into())
            .unwrap();
        let brick = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .slot_of(&"foundation:brick".into())
            .unwrap();
        let chunk = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![
                Command::EditTerrain {
                    cell: [2, 2, 2],
                    expected_revision: 0,
                    block: Some("foundation:bench".into()),
                    orientation: Default::default(),
                },
                Command::QueueCraft {
                    placement_id: 1,
                    revision: 1,
                    recipe: "foundation:brick".into(),
                    quantity: 1,
                },
            ],
        );
        app.update();
        let cell = IVec3::splat(2);
        app.world_mut()
            .resource_mut::<crate::craft_stations::CraftStations>()
            .edit_or_insert(cell, |state| assert!(state.deposit(stone, 1)));
        let spawn = Command::SpawnNpc {
            npc_kind: "foundation:settler".into(),
            position: [4., 2., 2.],
        };
        let start = Command::StartCraft {
            placement_id: 1,
            revision: 1,
            npc_id: 1,
            order_id: 1,
            order_revision: 1,
        };
        send(
            &app,
            2,
            vec![
                spawn.clone(),
                start.clone(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(app.world().resource::<crate::npc::NpcIdAllocator>().next, 1);
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .jobs
                .records
                .is_empty()
        );
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(cell)
                .unwrap()
                .inventory[&stone],
            1
        );
        send(&app, 3, vec![spawn, start.clone()]);
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .jobs
                .records[&1]
                .state,
            JobState::Accepted
        );
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(cell)
                .unwrap()
                .active_work
                .as_ref()
                .unwrap()
                .committed_inputs,
            vec![(stone, 1)]
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .jobs
                .records[&1]
                .state,
            JobState::Working
        );
        let saved = serde_json::to_vec(
            &app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .jobs,
        )
        .unwrap();
        let mut restored: crate::action_jobs::JobLedger = serde_json::from_slice(&saved).unwrap();
        restored.suspend_for_load();
        restored.validate().unwrap();
        assert_eq!(restored.records[&1].state, JobState::Interrupted);
        app.world_mut()
            .resource_mut::<crate::craft_stations::CraftBookings>()
            .disable_mod_jobs("foundation", "fixture callback failure");
        let paused = app
            .world()
            .resource::<crate::craft_stations::CraftStations>()
            .get(cell)
            .unwrap()
            .active_work
            .clone()
            .unwrap();
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(cell)
                .unwrap()
                .active_work
                .as_ref()
                .unwrap(),
            &paused
        );
        let actor = app
            .world_mut()
            .query_filtered::<Entity, With<crate::npc::Brain>>()
            .single(app.world())
            .unwrap();
        app.world_mut()
            .get_mut::<crate::npc::Brain>(actor)
            .unwrap()
            .active_policy_job = Some(("craft:2,2,2".into(), 1));
        send(&app, 4, vec![start]);
        app.update();
        for _ in 0..30 {
            app.update();
        }
        let bookings = app
            .world()
            .resource::<crate::craft_stations::CraftBookings>();
        assert_eq!(bookings.jobs.records[&1].state, JobState::Failed);
        assert_eq!(bookings.jobs.records[&2].state, JobState::Completed);
        assert!(!bookings.has_worker(cell));
        let brain = app.world().get::<crate::npc::Brain>(actor).unwrap();
        assert!(brain.active_policy_job.is_none());
        assert_eq!(brain.policy_results.len(), 1);
        assert_eq!(
            brain.policy_results[0].state,
            block_junk_mod_api::npcs::PolicyJobState::Completed
        );
        assert_eq!(
            app.world_mut()
                .query::<&crate::protocol::WorldItem>()
                .iter(app.world())
                .filter(|item| item.item == brick)
                .map(|item| item.count)
                .sum::<u32>(),
            1
        );
        send(
            &app,
            5,
            vec![
                Command::ReadCraftJob { job_id: 1 },
                Command::ReadCraftJob { job_id: 2 },
                Command::AcknowledgeCraftJob {
                    job_id: 1,
                    revision: 3,
                },
                Command::AcknowledgeCraftJob {
                    job_id: 2,
                    revision: 3,
                },
            ],
        );
        app.update();
        let bookings = app
            .world()
            .resource::<crate::craft_stations::CraftBookings>();
        assert!(bookings.jobs.records.values().all(|job| job.observed));
        bookings.jobs.validate().unwrap();
    }

    #[test]
    fn population_commands_retire_identity_and_preserve_materials_and_owned_state() {
        use block_junk_scripting::commands::{Command, CommandBatch};
        use block_junk_scripting::state::StateScope;
        let mut app = command_app();
        app.add_plugins((TransformPlugin, bevy::state::app::StatesPlugin));
        app.add_plugins(lightyear::prelude::server::ServerPlugins {
            tick_duration: std::time::Duration::from_secs_f64(1. / 60.),
        });
        app.add_plugins(crate::network::ProtocolPlugin);

        app.init_resource::<crate::interactables::InteractionClaims>()
            .add_observer(crate::npc::release_claims_on_npc_despawn);
        app.finish();
        app.cleanup();
        let stone = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .slot_of(&"foundation:stone".into())
            .unwrap();
        let chunk = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        let spawn = Command::SpawnNpc {
            npc_kind: "foundation:settler".into(),
            position: [2., 2., 2.],
        };
        let state = Command::SetState {
            scope: StateScope::Npc(1),
            expected_revision: 0,
            json: "{\"lived\":true}".into(),
        };
        send(
            &app,
            1,
            vec![
                spawn.clone(),
                state.clone(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(app.world().resource::<crate::npc::NpcIdAllocator>().next, 1);
        assert!(
            app.world()
                .resource::<ServerMods>()
                .0
                .saved_state()
                .is_empty()
        );
        assert_eq!(
            app.world_mut()
                .query::<&crate::npc::Npc>()
                .iter(app.world())
                .count(),
            0
        );
        send(&app, 2, vec![spawn.clone(), state]);
        app.update();
        let actor = app
            .world_mut()
            .query::<(Entity, &crate::npc::NpcId)>()
            .iter(app.world())
            .find(|(_, id)| id.0 == 1)
            .unwrap()
            .0;
        app.world_mut()
            .get_mut::<crate::protocol::Carrying>(actor)
            .unwrap()
            .item = Some(stone);
        app.world_mut()
            .get_mut::<crate::protocol::Carrying>(actor)
            .unwrap()
            .count = 3;
        app.world_mut()
            .get_mut::<crate::protocol::EquippedTool>(actor)
            .unwrap()
            .item = Some(stone);
        let cell = IVec3::splat(2);
        let mut bookings = app
            .world_mut()
            .resource_mut::<crate::craft_stations::CraftBookings>();
        assert!(bookings.try_book_npc(crate::npc::NpcId(1), cell));
        assert!(bookings.register_worker(
            cell,
            crate::craft_stations::WorkerAuthority::npc(actor, crate::npc::NpcId(1))
        ));
        let remove = Command::RemoveNpc {
            npc_id: 1,
            revision: 1,
        };
        send(
            &app,
            3,
            vec![
                remove.clone(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            3
        );
        assert!(
            app.world()
                .resource::<crate::item_placement::ItemPlacementQueue>()
                .0
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<crate::npc::NpcIdAllocator>()
                .retired
                .is_empty()
        );
        send(&app, 4, vec![remove.clone()]);
        app.update();
        assert!(app.world().get_entity(actor).is_err());
        assert!(
            app.world()
                .resource::<crate::npc::NpcIdAllocator>()
                .retired
                .contains(&1)
        );
        assert!(
            !app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .has_worker(cell)
        );
        assert!(
            !app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .contains_npc(crate::npc::NpcId(1))
        );
        assert_eq!(
            app.world()
                .resource::<crate::item_placement::ItemPlacementQueue>()
                .0
                .iter()
                .map(|item| item.count)
                .sum::<u32>(),
            4
        );
        assert_eq!(
            app.world().resource::<ServerMods>().0.saved_state()[0].scope,
            StateScope::Npc(1)
        );
        send(&app, 5, vec![remove]);
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::item_placement::ItemPlacementQueue>()
                .0
                .iter()
                .map(|item| item.count)
                .sum::<u32>(),
            4
        );
        send(&app, 6, vec![spawn]);
        app.update();
        let ids = app.world().resource::<crate::npc::NpcIdAllocator>();
        assert_eq!(ids.next, 3);
        let restored: crate::npc::NpcIdAllocator =
            serde_json::from_slice(&serde_json::to_vec(ids).unwrap()).unwrap();
        assert_eq!(restored.retired, ids.retired);
        assert!(
            app.world_mut()
                .query::<&crate::npc::NpcId>()
                .iter(app.world())
                .any(|id| id.0 == 2)
        );
    }

    #[test]
    fn facility_transfers_conserve_stock_and_carry_across_late_rejection() {
        use block_junk_scripting::commands::{
            Command, CommandBatch, InventoryBank, TransferDirection,
        };
        let mut app = command_app();
        let stone = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .slot_of(&"foundation:stone".into())
            .unwrap();
        let actor = app
            .world_mut()
            .spawn((
                crate::npc::NpcId(8),
                crate::npc::NpcKind("foundation:settler".into()),
                crate::protocol::Carrying {
                    item: Some(stone),
                    count: 3,
                },
                crate::protocol::AvatarPose {
                    translation: Vec3::splat(2.),
                    yaw: 0.,
                },
            ))
            .id();
        let chunk = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![Command::EditTerrain {
                cell: [2, 2, 2],
                expected_revision: 0,
                block: Some("foundation:bench".into()),
                orientation: Default::default(),
            }],
        );
        app.update();
        let transfer = |direction, count| Command::TransferFacility {
            placement_id: 1,
            revision: 1,
            npc_id: 8,
            item: "foundation:stone".into(),
            count,
            bank: InventoryBank::Crafting,
            direction,
        };
        send(
            &app,
            2,
            vec![
                transfer(TransferDirection::Deposit, 2),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            3
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .is_none()
        );
        send(&app, 3, vec![transfer(TransferDirection::Deposit, 2)]);
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .unwrap()
                .inventory[&stone],
            2
        );
        send(&app, 4, vec![transfer(TransferDirection::Withdraw, 3)]);
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .unwrap()
                .inventory[&stone],
            2
        );
        send(&app, 5, vec![transfer(TransferDirection::Withdraw, 2)]);
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            3
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .is_none()
        );
        let queue = Command::QueueCraft {
            placement_id: 1,
            revision: 1,
            recipe: "foundation:brick".into(),
            quantity: 1,
        };
        send(
            &app,
            9,
            vec![
                queue.clone(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftOrderIds>()
                .next,
            1
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .is_none()
        );
        send(&app, 10, vec![queue.clone(), queue]);
        app.update();
        let orders = &app
            .world()
            .resource::<crate::craft_stations::CraftStations>()
            .get(IVec3::splat(2))
            .unwrap()
            .orders;
        assert_eq!(
            orders.iter().map(|order| order.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        send(
            &app,
            11,
            vec![Command::CancelCraft {
                placement_id: 1,
                revision: 1,
                order_id: 2,
                order_revision: 1,
            }],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftOrderIds>()
                .next,
            3
        );
        send(&app, 6, vec![transfer(TransferDirection::Deposit, 2)]);
        app.update();
        app.world_mut()
            .resource_mut::<crate::craft_stations::CraftStations>()
            .edit_or_insert(IVec3::splat(2), |state| {
                assert!(state.begin_work(1, "foundation:brick".into(), 2., vec![(stone, 1)]));
            });
        let replacement = vec![
            Command::RemovePlacement {
                placement_id: 1,
                revision: 1,
            },
            Command::EditTerrain {
                cell: [2, 2, 2],
                expected_revision: 2,
                block: Some("foundation:bench".into()),
                orientation: Default::default(),
            },
            Command::TransferFacility {
                placement_id: 2,
                revision: 1,
                npc_id: 8,
                item: "foundation:stone".into(),
                count: 1,
                bank: InventoryBank::Crafting,
                direction: TransferDirection::Deposit,
            },
        ];
        let mut rejected = replacement.clone();
        rejected.push(Command::CancelProject {
            project_id: 999,
            revision: 1,
        });
        send(&app, 7, rejected);
        app.update();
        assert!(
            app.world()
                .resource::<crate::item_placement::ItemPlacementQueue>()
                .0
                .is_empty()
        );
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            1
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .unwrap()
                .active_work
                .is_some()
        );
        send(&app, 8, replacement);
        app.update();
        let results = app
            .world()
            .resource::<ServerMods>()
            .0
            .commands
            .results
            .lock()
            .unwrap();
        assert!(
            results["foundation"].back().unwrap().accepted,
            "{:?}",
            results["foundation"].back()
        );
        let state = app
            .world()
            .resource::<crate::craft_stations::CraftStations>()
            .get(IVec3::splat(2))
            .unwrap();
        assert_eq!(state.inventory[&stone], 1);
        assert!(state.active_work.is_none());
        assert_eq!(
            app.world()
                .get::<crate::protocol::Carrying>(actor)
                .unwrap()
                .count,
            0
        );
        let pending = app
            .world()
            .resource::<crate::item_placement::ItemPlacementQueue>();
        assert_eq!(
            pending
                .0
                .iter()
                .map(|entry| u64::from(entry.count))
                .sum::<u64>(),
            2
        );
        let saved: crate::item_placement::ItemPlacementQueue =
            serde_json::from_slice(&serde_json::to_vec(pending).unwrap()).unwrap();
        assert_eq!(&saved, pending);
    }

    #[test]
    fn relocating_facility_preserves_identity_stock_and_state_or_rolls_back() {
        use block_junk_mod_api::{architecture::PlacementId, blocks::Cardinal};
        use block_junk_scripting::commands::{Command, CommandBatch};
        let mut app = command_app();
        let chunk = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        send(
            &app,
            1,
            vec![Command::EditTerrain {
                cell: [2, 2, 2],
                expected_revision: 0,
                block: Some("foundation:bench".into()),
                orientation: Cardinal::East,
            }],
        );
        app.update();
        let stone = app
            .world()
            .resource::<crate::items::ItemRegistry>()
            .slot_of(&"foundation:stone".into())
            .unwrap();
        app.world_mut()
            .resource_mut::<crate::craft_stations::CraftStations>()
            .edit_or_insert(IVec3::splat(2), |station| {
                assert!(station.deposit(stone, 3));
            });
        app.world_mut()
            .resource_mut::<crate::placements::Placements>()
            .records
            .get_mut(&PlacementId(1))
            .unwrap()
            .authored_state = "{\"owner\":9}".into();
        let movement = |revision| Command::MovePlacement {
            placement_id: 1,
            revision,
            anchor: [5, 2, 2],
            orientation: Cardinal::North,
        };
        send(
            &app,
            2,
            vec![
                movement(1),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .records[&PlacementId(1)]
                .anchor,
            [2, 2, 2]
        );
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(chunk)
                .unwrap()
                .revision,
            1
        );
        let mut edits = app
            .world()
            .resource::<Messages<crate::protocol::CellEdit>>()
            .get_cursor_current();
        send(&app, 3, vec![movement(1)]);
        app.update();
        let record = &app
            .world()
            .resource::<crate::placements::Placements>()
            .records[&PlacementId(1)];
        assert_eq!(
            (record.anchor, record.orientation, record.revision),
            ([5, 2, 2], Cardinal::North, 2)
        );
        assert_eq!(record.authored_state, "{\"owner\":9}");
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .next_id,
            2
        );
        assert_eq!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::new(5, 2, 2))
                .unwrap()
                .inventory[&stone],
            3
        );
        assert!(
            app.world()
                .resource::<crate::craft_stations::CraftStations>()
                .get(IVec3::splat(2))
                .is_none()
        );
        assert!(
            edits
                .read(
                    app.world()
                        .resource::<Messages<crate::protocol::CellEdit>>()
                )
                .all(|edit| !edit.yield_drops)
        );
        let revision = app
            .world()
            .get::<crate::voxel::Chunk>(chunk)
            .unwrap()
            .revision;
        send(&app, 4, vec![movement(2)]); // Identical transform is an idempotent designation.
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(chunk)
                .unwrap()
                .revision,
            revision
        );
        send(&app, 5, vec![movement(1)]);
        app.update();
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .results
                .lock()
                .unwrap()["foundation"]
                .back()
                .unwrap()
                .accepted
        );
        let encoded = bincode::serde::encode_to_vec(
            app.world().resource::<crate::placements::Placements>(),
            bincode::config::standard(),
        )
        .unwrap();
        let (mut restored, _): (crate::placements::Placements, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        restored
            .rebuild(app.world().resource::<crate::blocks::BlockRegistry>())
            .unwrap();
        assert_eq!(restored.at(IVec3::new(5, 2, 2)).unwrap().id, PlacementId(1));
    }

    #[test]
    fn placement_state_and_geometry_share_atomic_identity_commit() {
        use block_junk_scripting::{
            commands::{Command, CommandBatch, CommandReceipt},
            state::StateScope,
        };
        let mut app = command_app();
        let entity = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                })
        };
        let place = |revision| Command::EditTerrain {
            cell: [2, 2, 2],
            expected_revision: revision,
            block: Some("foundation:bed".into()),
            orientation: Default::default(),
        };
        let set_state = || Command::SetState {
            scope: StateScope::Placement(1),
            expected_revision: 0,
            json: "{\"owner\":9}".into(),
        };
        send(
            &app,
            1,
            vec![
                place(0),
                set_state(),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .next_id,
            1
        );
        assert!(
            app.world()
                .resource::<ServerMods>()
                .0
                .saved_state()
                .is_empty()
        );
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(entity)
                .unwrap()
                .revision,
            0
        );
        send(
            &app,
            2,
            vec![
                place(0),
                set_state(),
                Command::ReadPlacement { cell: [2, 2, 2] },
            ],
        );
        app.update();
        let mods = &app.world().resource::<ServerMods>().0;
        assert_eq!(mods.saved_state()[0].scope, StateScope::Placement(1));
        assert!(
            matches!(mods.commands.results.lock().unwrap()["foundation"][1].receipts.last(),
            Some(CommandReceipt::Placement {placement_id:Some(1),revision:Some(1),use_slots,..}) if use_slots.len()==1 && use_slots[0].placement.0==1)
        );
        let json =
            serde_json::to_string(app.world().resource::<crate::placements::Placements>()).unwrap();
        let mut restored: crate::placements::Placements = serde_json::from_str(&json).unwrap();
        restored
            .rebuild(app.world().resource::<crate::blocks::BlockRegistry>())
            .unwrap();
        app.insert_resource(restored);
        send(
            &app,
            3,
            vec![
                Command::RemovePlacement {
                    placement_id: 1,
                    revision: 1,
                },
                place(2),
            ],
        );
        app.update();
        let placements = app.world().resource::<crate::placements::Placements>();
        assert_eq!(placements.at(IVec3::splat(2)).unwrap().id.0, 2);
        assert!(!placements.records[&block_junk_mod_api::architecture::PlacementId(1)].active);
        // The old owner's state stays on its retired identity, never copied.
        assert_eq!(
            app.world().resource::<ServerMods>().0.saved_state()[0].scope,
            StateScope::Placement(1)
        );
        send(
            &app,
            4,
            vec![Command::RemovePlacement {
                placement_id: 1,
                revision: 1,
            }],
        );
        app.update();
        assert_eq!(
            app.world()
                .resource::<crate::placements::Placements>()
                .at(IVec3::splat(2))
                .unwrap()
                .id
                .0,
            2
        );
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .results
                .lock()
                .unwrap()["foundation"][3]
                .accepted
        );
    }

    #[test]
    fn terrain_commands_commit_together_and_reject_stale_revisions() {
        use block_junk_scripting::commands::{Command, CommandBatch, CommandReceipt};
        let mut app = command_app();
        let entity = app
            .world_mut()
            .spawn((
                crate::voxel::Chunk {
                    revision: 0,
                    blocks: vec![
                        crate::blocks::BlockSlot::EMPTY;
                        (crate::protocol::CHUNK_PADDED as usize).pow(3)
                    ],
                },
                crate::voxel::ChunkEntities::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        let send = |app: &App, id, commands| {
            app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .push_back(CommandBatch {
                    id,
                    mod_id: "foundation".into(),
                    behavior: "test".into(),
                    cause: None,
                    commands,
                });
        };
        let edit = |cell, revision| Command::EditTerrain {
            cell,
            expected_revision: revision,
            block: Some("foundation:stone".into()),
            orientation: Default::default(),
        };
        send(
            &app,
            1,
            vec![
                edit([1, 1, 1], 0),
                Command::CancelProject {
                    project_id: 999,
                    revision: 1,
                },
            ],
        );
        app.update();
        let chunk = app.world().get::<crate::voxel::Chunk>(entity).unwrap();
        assert_eq!(chunk.revision, 0);
        assert!(chunk.get(IVec3::splat(2)).is_empty());
        assert!(
            app.world()
                .resource::<crate::server::PendingTerrainEffects>()
                .0
                .is_empty()
        );
        // A later unrelated edit must not cancel the project created after the
        // first edit in the same batch.
        send(
            &app,
            2,
            vec![
                edit([1, 1, 1], 0),
                Command::Designate {
                    cell: [1, 1, 1],
                    block: None,
                    orientation: Default::default(),
                },
                edit([2, 1, 1], 1),
                Command::ReadCell { cell: [1, 1, 1] },
            ],
        );
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::voxel::Chunk>(entity)
                .unwrap()
                .revision,
            2
        );
        assert!(
            app.world()
                .resource::<crate::plans::Plans>()
                .get(IVec3::ONE)
                .is_some()
        );
        let mods = &app.world().resource::<ServerMods>().0;
        let results = mods.commands.results.lock().unwrap();
        assert!(!results["foundation"][0].accepted);
        assert!(
            results["foundation"][1].accepted,
            "{:?}",
            results["foundation"][1]
        );
        assert!(matches!(
            results["foundation"][1].receipts.last(),
            Some(CommandReceipt::Cell { revision: 2, .. })
        ));
        drop(results);
        send(&app, 3, vec![edit([3, 1, 1], 1)]);
        app.update();
        assert!(
            app.world()
                .get::<crate::voxel::Chunk>(entity)
                .unwrap()
                .get(IVec3::new(4, 2, 2))
                .is_empty()
        );
        assert!(
            !app.world()
                .resource::<ServerMods>()
                .0
                .commands
                .results
                .lock()
                .unwrap()["foundation"][2]
                .accepted
        );
    }

    #[test]
    fn disabling_brain_releases_all_leases_without_consuming_escrow() {
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins)
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::interactables::InteractionClaims>()
            .init_resource::<crate::haul::HaulStore>()
            .init_resource::<crate::craft_stations::CraftBookings>()
            .init_resource::<crate::plans::Plans>()
            .init_resource::<crate::construction::ConstructionLedger>()
            .init_resource::<crate::placements::Placements>()
            .add_observer(crate::npc::release_claims_on_brain_disable);
        let mut supplied = crate::protocol::PlanState::new(
            crate::protocol::PlanKind::Remove,
            vec![crate::protocol::MaterialEntry {
                item: crate::items::ItemSlot(1),
                needed: 5,
                present: 3,
            }],
        );
        supplied.project_id = 1;
        supplied.revision = 1;
        supplied.footprint = vec![IVec3::ZERO];
        app.world_mut()
            .resource_mut::<crate::plans::Plans>()
            .upsert(IVec3::ZERO, supplied.clone(), 0);
        let id = crate::npc::NpcId(17);
        let entity = app
            .world_mut()
            .spawn((id, crate::protocol::KinematicLock))
            .id();
        assert!(
            app.world_mut()
                .resource_mut::<crate::plan_claims::PlanClaims>()
                .try_claim(IVec3::ZERO, id)
        );
        assert!(
            app.world_mut()
                .resource_mut::<crate::interactables::InteractionClaims>()
                .claim_slot(
                    IVec3::X,
                    block_junk_mod_api::architecture::UseSlotId {
                        placement: block_junk_mod_api::architecture::PlacementId(1),
                        key: "primary".into()
                    },
                    block_junk_mod_api::authority::ReservationOwner::Npc { id: id.0 }
                )
                .is_some()
        );
        let mut bookings = app
            .world_mut()
            .resource_mut::<crate::craft_stations::CraftBookings>();
        assert!(bookings.try_book_npc(id, IVec3::Z));
        assert!(bookings.register_worker(
            IVec3::Z,
            crate::craft_stations::WorkerAuthority::npc(entity, id)
        ));
        app.world_mut()
            .entity_mut(entity)
            .insert(crate::npc::BrainDisabled {
                reason: "fixture callback failure".into(),
            });
        app.update();
        assert!(
            app.world()
                .resource::<crate::plan_claims::PlanClaims>()
                .holder(IVec3::ZERO)
                .is_none()
        );
        assert!(
            app.world_mut()
                .resource_mut::<crate::interactables::InteractionClaims>()
                .claim_slot(
                    IVec3::X,
                    block_junk_mod_api::architecture::UseSlotId {
                        placement: block_junk_mod_api::architecture::PlacementId(1),
                        key: "primary".into()
                    },
                    block_junk_mod_api::authority::ReservationOwner::Npc { id: 18 }
                )
                .is_some()
        );
        assert!(
            !app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .contains_npc(id)
        );
        assert!(
            !app.world()
                .resource::<crate::craft_stations::CraftBookings>()
                .has_worker(IVec3::Z)
        );
        assert!(
            app.world()
                .get::<crate::protocol::KinematicLock>(entity)
                .is_none()
        );
        assert!(
            app.world()
                .resource::<crate::construction::ConstructionLedger>()
                .pending_refunds
                .is_empty()
        );
        assert_eq!(
            app.world()
                .resource::<crate::plans::Plans>()
                .get(IVec3::ZERO),
            Some(&supplied)
        );
    }

    #[test]
    fn independent_base_bootstraps_population_without_a_connection() {
        let mut app = population_app(false);
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&crate::npc::Npc>()
                .iter(app.world())
                .count(),
            1
        );
        assert!(
            !app.world()
                .resource::<crate::voxel::ChunkMap>()
                .0
                .is_empty()
        );
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&crate::npc::Npc>()
                .iter(app.world())
                .count(),
            1
        );
        assert!(
            app.world()
                .resource::<crate::npc::PopulationInitialized>()
                .0
        );
    }

    #[test]
    fn initialized_empty_population_does_not_reappear() {
        let mut app = population_app(true);
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&crate::npc::Npc>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[test]
    #[cfg(unix)]
    fn shipped_addon_replaces_vanilla_planner_in_production_pipeline() {
        struct Root(std::path::PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Root(
            std::env::temp_dir().join(format!("colony-addon-validation-{}", std::process::id())),
        );
        std::fs::create_dir(&root.0).unwrap();
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::os::unix::fs::symlink(repo.join("mods/vanilla"), root.0.join("vanilla")).unwrap();
        std::os::unix::fs::symlink(
            repo.join("examples/mods/addons/quiet_colony"),
            root.0.join("quiet_colony"),
        )
        .unwrap();
        let mut content = load_content(Side::Server, &root.0, Some("vanilla:colony")).unwrap();
        let recipe = content
            .recipes
            .slot_of(&"vanilla:planks_from_log".into())
            .unwrap();
        assert_eq!(content.recipes.try_def(recipe).unwrap().duration_secs, 2.0);
        let kind = content.npc_kinds.get("vanilla:wanderer").unwrap();
        assert!(
            kind.job_classes
                .contains(&block_junk_mod_api::npcs::JobClass::Crafting)
        );
        assert!(
            !kind
                .job_classes
                .contains(&block_junk_mod_api::npcs::JobClass::Hauling)
        );
        content.mods.activate_runtime().unwrap();
        let snapshot = serde_json::from_value(serde_json::json!({"id":1,"kind":"vanilla:wanderer","foot":{"x":0,"y":0,"z":0},"needs":{}})).unwrap();
        assert!(matches!(
            content
                .mods
                .call_planner(&"vanilla:wanderer".into(), &snapshot)
                .unwrap(),
            Some(block_junk_mod_api::npcs::PlannerGoal::Rest { duration_secs: 4.0 })
        ));
        let snapshot = serde_json::from_value(serde_json::json!({
            "id":1,"kind":"vanilla:wanderer","foot":{"x":0,"y":0,"z":0},
            "needs":{"sleep":0.8},
            "nearby_interactions":[{"cell":{"x":2,"y":1,"z":0},"need":"sleep","restores":0.4,"exclusive":true,"distance":2,"capabilities":["vanilla:sleeping"]}]
        })).unwrap();
        assert!(matches!(
            content
                .mods
                .call_planner(&"vanilla:wanderer".into(), &snapshot)
                .unwrap(),
            Some(block_junk_mod_api::npcs::PlannerGoal::Interact { .. })
        ));
        let snapshot = serde_json::from_value(serde_json::json!({
            "id":1,"kind":"vanilla:wanderer","foot":{"x":0,"y":0,"z":0},
            "needs":{"work":0.9},
            "job_candidates":[{"id":"craft:1,1,1","revision":7,"class":"crafting","target":{"x":1,"y":1,"z":1},"estimated_cost":3,"requirements":[],"capability":"vanilla:crafting","available":true}]
        })).unwrap();
        assert!(matches!(
            content
                .mods
                .call_planner(&"vanilla:wanderer".into(), &snapshot)
                .unwrap(),
            Some(block_junk_mod_api::npcs::PlannerGoal::TakeJob { revision: 7, .. })
        ));
        assert_eq!(
            content.mods.mods.last().unwrap().manifest.id,
            "quiet_colony"
        );
    }

    #[test]
    fn explicit_generation_upgrade_preserves_retained_neighborhood_and_validates_movement() {
        let content = independent();
        let world = crate::save::SavedWorld {
            spaces: Default::default(),
            spatial_events: Default::default(),
            events: Default::default(),
            craft_jobs: Default::default(),
            interactions: Default::default(),
            npc_ids: crate::npc::NpcIdAllocator {
                next: 2,
                retired: Default::default(),
            },
            craft_order_ids: Default::default(),
            pending_items: Default::default(),
            placements: Default::default(),
            contracts: content.contracts.clone(),
            preset: content.preset.0.clone(),
            population_initialized: true,
        };
        let mut save:crate::save::SaveFile=serde_json::from_value(serde_json::json!({"mod_state":[],"world":world,"construction":{"jobs":{"next":1,"records":{}},"next_project_id":1,"pending_refunds":[]},"version":crate::save::SAVE_VERSION,"edited_chunks":[],"block_slots":content.blocks.slot_table()})).unwrap();
        save.npcs.push(crate::save::SavedNpc {
            home_facility: None,
            id: 1,
            kind: "foundation:settler".into(),
            pose: crate::protocol::AvatarPose {
                translation: Vec3::new(3., 12., 3.),
                yaw: 0.,
            },
            movement_mode: Default::default(),
            needs: Default::default(),
            rng: 1,
            carrying: None,
            tool: None,
            stats: Default::default(),
            policy_results: Vec::new(),
            active_policy_job: None,
        });
        let mut preset = content.preset.0.clone();
        preset.id = "foundation:plain-v2".into();
        preset.generation.base_height += 1.;
        let mut contracts = content.contracts.clone();
        let mut movement: block_junk_mod_api::architecture::MovementProfile =
            serde_json::from_str(&contracts.movement[&preset.movement_profile]).unwrap();
        movement.height = 1.6;
        contracts.movement.insert(
            movement.id.clone(),
            serde_json::to_string(&movement).unwrap(),
        );
        let upgrade=serde_json::from_value(serde_json::json!({"id":"foundation:world-v2","movement":[{"profile":movement.id,"from_contract":crate::content_contracts::contract_digest(&content.contracts.movement[&preset.movement_profile]),"to_contract":crate::content_contracts::contract_digest(&contracts.movement[&preset.movement_profile])}],"generation":{"preset":preset.id,"from_contract":crate::content_contracts::contract_digest(&crate::content_contracts::generation_contract(&content.preset.0)),"to_contract":crate::content_contracts::contract_digest(&crate::content_contracts::generation_contract(&preset)),"terrain_policy":"preserve_retained_regions"}})).unwrap();
        let contracts = contracts
            .with_upgrades(vec![upgrade], &[preset.clone()])
            .unwrap();
        let migrated =
            crate::save_upgrade::migrate(&save, &contracts, &content.blocks, &preset).unwrap();
        assert_eq!(migrated.edited_chunks.len(), 27);
        assert_eq!(
            migrated.npcs[0].pose.translation,
            save.npcs[0].pose.translation
        );
        assert!(migrated.world.as_ref().unwrap().population_initialized);
        let origin = crate::protocol::ChunkCoord(IVec3::ZERO);
        let old = crate::voxel::Chunk::from_terrain(
            origin,
            &crate::blocks::TerrainSlots::from_registry(&content.blocks, &content.preset.0),
        );
        assert_eq!(
            migrated
                .edited_chunks
                .iter()
                .find(|chunk| chunk.coord == origin)
                .unwrap()
                .chunk
                .blocks,
            old.blocks
        );
        assert_eq!(
            migrated.world.as_ref().unwrap().preset.id,
            "foundation:plain-v2"
        );
        assert!(save.edited_chunks.is_empty());
        let bytes = bincode::serde::encode_to_vec(&migrated, bincode::config::standard()).unwrap();
        assert!(bytes.len() > 1024 * 1024);
        let (restored, _): (crate::save::SaveFile, usize) = bincode::serde::decode_from_slice(
            &bytes,
            bincode::config::standard().with_limit::<268_435_456>(),
        )
        .unwrap();
        contracts.validate_save(&restored, &preset).unwrap();
        let mut invalid = contracts.clone();
        invalid.upgrades.clear();
        assert!(crate::save_upgrade::migrate(&save, &invalid, &content.blocks, &preset).is_err());
    }

    #[test]
    fn structural_upgrade_preserves_ids_escrow_and_slot_progress_or_rejects_atomically() {
        use block_junk_mod_api::{
            authority::{JobState, JobTarget, ReservationOwner},
            blocks::Cardinal,
        };
        let content = independent();
        let bed = content.blocks.require("foundation:bed");
        let stone = content.blocks.require("foundation:stone");
        let anchor = IVec3::new(4, 2, 4);
        let approach = anchor + IVec3::X;
        let mut chunk = crate::voxel::Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        chunk.set(crate::voxel::world_to_chunk(anchor).1, bed);
        chunk.set(crate::voxel::world_to_chunk(approach - IVec3::Y).1, stone);
        let mut placements = crate::placements::Placements::default();
        placements
            .index_existing(anchor, bed, Cardinal::East, &content.blocks, |cell| {
                Some(chunk.get(crate::voxel::world_to_chunk(cell).1))
            })
            .unwrap();
        let mut sidecar = crate::voxel::ChunkEntities::default();
        sidecar.insert(
            anchor,
            crate::voxel::EntryKind::Anchor {
                orientation: Cardinal::East,
                portal_open: None,
            },
        );
        let placement = placements.at(anchor).unwrap();
        let mut interactions = crate::facility_interactions::FacilityInteractions::default();
        let mut claims = crate::interactables::InteractionClaims::default();
        let job = interactions
            .prepare(
                crate::facility_interactions::InteractionRequest {
                    placement,
                    slot_key: "primary",
                    action: "sleep",
                    actor: ReservationOwner::Npc { id: 1 },
                    controller: ReservationOwner::Npc { id: 1 },
                    cause: None,
                    approach,
                },
                &content.blocks,
                &mut claims,
            )
            .unwrap();
        interactions.working(job.id);
        interactions
            .progress
            .get_mut(&job.id)
            .unwrap()
            .remaining_secs = 0.4;
        let world = crate::save::SavedWorld {
            spaces: Default::default(),
            spatial_events: Default::default(),
            events: Default::default(),
            craft_jobs: Default::default(),
            interactions,
            npc_ids: crate::npc::NpcIdAllocator {
                next: 2,
                retired: Default::default(),
            },
            craft_order_ids: Default::default(),
            pending_items: Default::default(),
            placements,
            contracts: content.contracts.clone(),
            preset: content.preset.0.clone(),
            population_initialized: true,
        };
        let mut save:crate::save::SaveFile=serde_json::from_value(serde_json::json!({"mod_state":[],"world":world,"construction":{"jobs":{"next":1,"records":{}},"next_project_id":2,"pending_refunds":[]},"version":crate::save::SAVE_VERSION,"edited_chunks":[],"block_slots":content.blocks.slot_table()})).unwrap();
        save.edited_chunks.push(crate::save::SavedChunk {
            coord: crate::protocol::ChunkCoord(IVec3::ZERO),
            chunk,
            entities: sidecar,
        });
        save.npcs.push(crate::save::SavedNpc {
            home_facility: Some(1),
            id: 1,
            kind: "foundation:settler".into(),
            pose: crate::protocol::AvatarPose {
                translation: crate::physics::standing_pose_translation(approach),
                yaw: 0.,
            },
            movement_mode: Default::default(),
            needs: Default::default(),
            rng: 1,
            carrying: None,
            tool: None,
            stats: Default::default(),
            policy_results: Vec::new(),
            active_policy_job: None,
        });
        let project_anchor = IVec3::new(10, 2, 10);
        save.plans.push((
            project_anchor,
            crate::save::SavedPlanState {
                project_id: 1,
                revision: 1,
                footprint: vec![project_anchor],
                progress_secs: 0.2,
                kind: crate::protocol::PlanKind::Build {
                    slot: bed,
                    orientation: Cardinal::East,
                },
                materials: vec![crate::save::SavedMaterialEntry {
                    item_id: "foundation:stone".into(),
                    needed: 1,
                    present: 1,
                }],
            },
        ));
        let mut definitions: Vec<_> = content
            .blocks
            .iter()
            .filter(|(slot, _)| !slot.is_empty())
            .map(|(_, def)| def.clone())
            .collect();
        let definition = definitions
            .iter_mut()
            .find(|def| def.id.as_str() == "foundation:bed")
            .unwrap();
        definition.footprint = vec![[0, 0, 0], [0, 0, 1]];
        definition.facility_slots[0].key = "sleeping".into();
        definition.facility_slots[0].approaches = vec![[1, 0, 0], [-1, 0, 0]];
        let blocks = BlockRegistry::build(definitions).unwrap().0;
        let mut contracts = crate::content_contracts::ContentContracts::compile(&blocks, &[]);
        contracts.movement = content.contracts.movement.clone();
        assert!(
            crate::save_upgrade::migrate(&save, &contracts, &blocks, &content.preset.0)
                .err()
                .unwrap()
                .contains("compatible migration")
        );
        let upgrade=serde_json::from_value(serde_json::json!({"id":"foundation:bed-v2","blocks":[{"block":"foundation:bed","from_contract":crate::content_contracts::contract_digest(&content.contracts.blocks["foundation:bed"]),"to_contract":crate::content_contracts::contract_digest(&contracts.blocks["foundation:bed"]),"footprint":"rebuild_at_anchor","slot_renames":{"primary":"sleeping"}}]})).unwrap();
        let contracts = contracts
            .with_upgrades(vec![upgrade], std::slice::from_ref(&content.preset.0))
            .unwrap();
        let original = serde_json::to_vec(&save).unwrap();
        let upgraded =
            crate::save_upgrade::migrate(&save, &contracts, &blocks, &content.preset.0).unwrap();
        assert_eq!(serde_json::to_vec(&save).unwrap(), original);
        assert_eq!(upgraded.npcs[0].home_facility, Some(1));
        let world = upgraded.world.as_ref().unwrap();
        let record = &world.placements.records[&block_junk_mod_api::architecture::PlacementId(1)];
        assert_eq!(record.revision, 2);
        assert_eq!(record.anchor, anchor.to_array());
        assert!(
            matches!(&world.interactions.jobs.records[&job.id].target,JobTarget::Interaction{slot,placement_revision:2,..} if slot.key=="sleeping")
        );
        assert_eq!(
            world.interactions.jobs.records[&job.id].state,
            JobState::Interrupted
        );
        assert_eq!(world.interactions.progress[&job.id].remaining_secs, 0.4);
        assert_eq!(upgraded.plans[0].1.footprint.len(), 2);
        assert_eq!(upgraded.plans[0].1.materials[0].present, 1);
        assert_eq!(upgraded.plans[0].1.progress_secs, 0.2);
        crate::server::validate_runtime_save_references(
            "upgrade",
            &upgraded,
            &blocks,
            &content.items,
            &content.recipes,
            &content.npc_kinds,
            &content.needs,
        )
        .unwrap();
        crate::action_jobs::validate_owners(&upgraded, &content.mods).unwrap();
        let bytes = bincode::serde::encode_to_vec(&upgraded, bincode::config::standard()).unwrap();
        let (restored, _): (crate::save::SaveFile, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(restored.npcs[0].home_facility, Some(1));
        contracts
            .validate_save(&restored, &content.preset.0)
            .unwrap();
        #[cfg(unix)]
        {
            // Exercise the shipped declaration and the real disk/headless path,
            // including rejection without replacing the original save generation.
            struct Fixture {
                root: PathBuf,
                save_name: String,
            }
            impl Drop for Fixture {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.root);
                    let _ = crate::save::delete_save(&self.save_name);
                }
            }
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let fixture = Fixture {
                root: std::env::temp_dir().join(format!("colony-upgrade-{nonce}")),
                save_name: format!("upgrade-test-{nonce}"),
            };
            std::fs::create_dir(&fixture.root).unwrap();
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            for (name, path) in [
                ("foundation", "examples/mods/independent/foundation"),
                ("wide_bed", "examples/mods/addons/wide_bed"),
            ] {
                std::os::unix::fs::symlink(repo.join(path), fixture.root.join(name)).unwrap();
            }
            crate::save::write_save(&fixture.save_name, &save).unwrap();
            let before =
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap();
            let args = vec![
                "--mods-dir".into(),
                fixture.root.to_string_lossy().into_owned(),
                "--save".into(),
                fixture.save_name.clone(),
                "--json".into(),
            ];
            assert_eq!(validate_mods(&args), 0);
            assert_eq!(
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap(),
                before
            );
            let mut obstructed = save.clone();
            obstructed.edited_chunks[0]
                .chunk
                .set(crate::voxel::world_to_chunk(anchor + IVec3::Z).1, stone);
            crate::save::write_save(&fixture.save_name, &obstructed).unwrap();
            let before =
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap();
            assert_eq!(validate_mods(&args), 1);
            assert_eq!(
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap(),
                before
            );
            let fault = fixture.root.join("state_fault");
            std::fs::create_dir(&fault).unwrap();
            std::fs::write(fault.join("manifest.toml"), "id = \"state_fault\"\nname = \"Failure fixture\"\nversion = \"1.0.0\"\napi_version = \"0.6.0\"\n[dependencies]\nfoundation = \"^1.0\"\n").unwrap();
            std::fs::write(fault.join("data.lua"), "engine.state.register(2, {})\nengine.state.migrate(1, function(old) old.changed = true; error('deliberate migration failure') end)\n").unwrap();
            let mut failed_state = save.clone();
            failed_state
                .mod_state
                .push(block_junk_scripting::state::SavedModState {
                    mod_id: "state_fault".into(),
                    scope: block_junk_scripting::state::StateScope::World,
                    schema: 1,
                    revision: 4,
                    json: "{\"changed\":false}".into(),
                });
            crate::save::write_save(&fixture.save_name, &failed_state).unwrap();
            let before =
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap();
            assert_eq!(validate_mods(&args), 1);
            assert_eq!(
                serde_json::to_vec(&crate::save::read_save(&fixture.save_name).unwrap()).unwrap(),
                before
            );
        }
        save.edited_chunks[0]
            .chunk
            .set(crate::voxel::world_to_chunk(anchor + IVec3::Z).1, stone);
        let blocked = serde_json::to_vec(&save).unwrap();
        assert!(
            crate::save_upgrade::migrate(&save, &contracts, &blocks, &content.preset.0).is_err()
        );
        assert_eq!(serde_json::to_vec(&save).unwrap(), blocked);
    }

    #[test]
    fn saved_contracts_allow_balance_but_reject_referenced_geometry_changes() {
        let content = independent();
        let saved_world = crate::save::SavedWorld {
            spaces: Default::default(),
            spatial_events: Default::default(),
            events: Default::default(),
            craft_jobs: Default::default(),
            interactions: Default::default(),
            npc_ids: Default::default(),
            craft_order_ids: Default::default(),
            pending_items: Default::default(),
            placements: Default::default(),
            contracts: content.contracts.clone(),
            preset: content.preset.0.clone(),
            population_initialized: true,
        };
        let save: crate::save::SaveFile = serde_json::from_value(serde_json::json!({
            "mod_state":[],"world":saved_world,"construction":{"jobs":{"next":1,"records":{}},"next_project_id":1,"pending_refunds":[]},
            "version":crate::save::SAVE_VERSION,"edited_chunks":[],"block_slots":content.blocks.slot_table(),
        })).unwrap();
        content
            .contracts
            .validate_save(&save, &content.preset.0)
            .unwrap();
        let mut changed = content.contracts.clone();
        // Unreferenced furniture may be removed, just like unused slot-table IDs.
        changed.blocks.remove("foundation:bed");
        changed.validate_save(&save, &content.preset.0).unwrap();
        let mut defs: Vec<_> = content
            .blocks
            .iter()
            .filter(|(slot, _)| !slot.is_empty())
            .map(|(_, def)| def.clone())
            .collect();
        for def in &mut defs {
            def.display_name.push_str(" balanced");
            for ingredient in &mut def.materials {
                ingredient.count += 1;
            }
        }
        let (blocks, _) = BlockRegistry::build(defs).unwrap();
        let mut balance = crate::content_contracts::ContentContracts::compile(&blocks, &[]);
        balance.movement = content.contracts.movement.clone();
        balance.validate_save(&save, &content.preset.0).unwrap();
        balance.blocks.insert(
            content.preset.0.terrain.stone.as_str().into(),
            "changed geometry".into(),
        );
        assert!(
            balance
                .validate_save(&save, &content.preset.0)
                .unwrap_err()
                .contains("compatible migration")
        );
        let mut generation = content.preset.0.clone();
        generation.generation.seed += 1;
        assert!(
            content
                .contracts
                .validate_save(&save, &generation)
                .unwrap_err()
                .contains("generation")
        );
        let bytes = bincode::serde::encode_to_vec(&save, bincode::config::standard()).unwrap();
        let (restored, _): (crate::save::SaveFile, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        content
            .contracts
            .validate_save(&restored, &content.preset.0)
            .unwrap();
    }

    #[test]
    fn independent_content_and_configuration_agree_between_sides() {
        let server = independent();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/mods/independent");
        let client = load_content(Side::Client, &root, None).unwrap();
        assert_eq!(server.fingerprint.0, client.fingerprint.0);
        assert!(
            server
                .blocks
                .iter()
                .all(|(_, def)| !def.id.as_str().starts_with("vanilla:"))
        );
        assert!(server.mods.mods.iter().all(|m| m.manifest.id != "vanilla"));
        let saved = crate::save::SavedWorld {
            spaces: Default::default(),
            spatial_events: Default::default(),
            events: Default::default(),
            craft_jobs: Default::default(),
            interactions: Default::default(),
            npc_ids: Default::default(),
            craft_order_ids: Default::default(),
            pending_items: Default::default(),
            placements: Default::default(),
            contracts: server.contracts.clone(),
            preset: server.preset.0.clone(),
            population_initialized: true,
        };
        let bytes = bincode::serde::encode_to_vec(&saved, bincode::config::standard()).unwrap();
        let (restored, _): (crate::save::SavedWorld, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(restored.preset, server.preset.0);
        assert!(restored.population_initialized);
        assert!(load_content(Side::Client, &root, Some("missing:preset")).is_err());
    }
}
