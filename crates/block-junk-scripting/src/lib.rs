//! Lua scripting host for block-junk.
//!
//! Each mod gets its own [`mlua::Lua`] state per side — no shared globals
//! between mods, and no shared runtime state between client and server (each
//! side spins up its own [`ModRegistry`]). When a hook callback errors, the
//! mod is **disabled for the rest of the session** and a loud `error!` is
//! logged. The engine never silently continues with corrupt mod state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use block_junk_mod_api::{
    API_VERSION, ApiVersion, ModManifest, Side,
    animations::AnimationDef,
    blocks::BlockDef,
    civilization::CivilizationParams,
    items::ItemDef,
    npcs::{NeedDef, NpcKindDef, NpcKindId, NpcSnapshot, PlannerGoal, WorkDefaults},
    recipes::RecipeDef,
    rooms::{RoomEvent, RoomPattern},
    server::BlockPlacedEvent,
    shared::BlockPos,
    ui::{InspectTarget, UiToast},
};
use mlua::{Function, Lua, LuaSerdeExt, SerializeOptions, Table, Value};
use thiserror::Error;
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing manifest at {path}: {source}")]
    Manifest {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("mod {name} declares api_version={target}: {reason}")]
    ApiVersion {
        name: String,
        target: String,
        reason: String,
    },
    #[error("mod {name} has no entry script; create data.lua or events.lua in its directory")]
    NoScripts { name: String },
    #[error("lua error in mod {name} ({side}): {source}")]
    Lua {
        name: String,
        side: &'static str,
        #[source]
        source: mlua::Error,
    },
}

/// Slot in the per-mod `engine` table where a registered hook callback lives.
const BLOCK_PLACED_SLOT: &str = "_block_placed_handler";
const ROOM_EVENT_SLOT: &str = "_room_event_handler";
const INSPECT_SLOT: &str = "_inspect_handler";
/// Sub-table on `engine` holding per-kind NPC planner callbacks. Unlike
/// the single-slot hooks above, planners are keyed by NPC kind id so one
/// mod can plan for multiple kinds it has registered.
const NPC_PLANNERS_TABLE: &str = "_npc_planners";

/// Shared state passed to [`ModRegistry::load_dir`]. Each registration call
/// from a mod's Lua state appends into one of these buffers; the engine
/// drains them after load to build its real registries (BlockRegistry, etc.).
///
/// One context is built per side (server / client). Both sides run each
/// mod's `data.lua`, so the same blocks register into both — the engine
/// builds two parallel registries that agree slot-for-slot.
#[derive(Clone, Default)]
pub struct LoadContext {
    pub pending_blocks: Arc<Mutex<Vec<BlockDef>>>,
    pub pending_items: Arc<Mutex<Vec<ItemDef>>>,
    pub pending_rooms: Arc<Mutex<Vec<RoomPattern>>>,
    pub pending_npc_kinds: Arc<Mutex<Vec<NpcKindDef>>>,
    pub pending_needs: Arc<Mutex<Vec<NeedDef>>>,
    pub pending_animations: Arc<Mutex<Vec<AnimationDef>>>,
    pub pending_recipes: Arc<Mutex<Vec<RecipeDef>>>,
    /// At most one set of work-action defaults across all mods. Loud
    /// failure on second call so two mods can't silently overwrite each
    /// other's balance — matches the "never silently degrade" rule.
    pub pending_work_defaults: Arc<Mutex<Option<WorkDefaults>>>,
    /// At most one set of civilization-cluster params. Same single-writer
    /// rule as work-defaults — two mods can't silently disagree on how
    /// rooms group into clusters.
    pub pending_civilization_params: Arc<Mutex<Option<CivilizationParams>>>,
    /// At most one material-drop multiplier across all mods (same
    /// single-writer rule). Scales `materials` counts when a block
    /// leaves `drops` unspecified; the engine applies it once after
    /// load via `BlockDef::resolve_drops`. `None` ⇒ 1.0.
    pub pending_material_drop_multiplier: Arc<Mutex<Option<f32>>>,
    /// Unlike the `pending_*` buffers above (drained once after load),
    /// this is a **runtime** queue: `engine.ui.toast` pushes into it for
    /// the lifetime of the session and the engine drains it every tick.
    /// The engine keeps a clone of the Arc (see [`Self::ui_toast_queue`]).
    pub ui_toasts: Arc<Mutex<Vec<UiToast>>>,
}

impl LoadContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain the accumulated block defs. Called by the engine after
    /// `load_dir` returns so it can build the real `BlockRegistry`.
    pub fn take_blocks(&self) -> Vec<BlockDef> {
        std::mem::take(&mut *self.pending_blocks.lock().unwrap())
    }

    /// Drain the accumulated item defs.
    pub fn take_items(&self) -> Vec<ItemDef> {
        std::mem::take(&mut *self.pending_items.lock().unwrap())
    }

    /// Drain the accumulated room patterns.
    pub fn take_rooms(&self) -> Vec<RoomPattern> {
        std::mem::take(&mut *self.pending_rooms.lock().unwrap())
    }

    /// Drain the accumulated NPC kind defs.
    pub fn take_npc_kinds(&self) -> Vec<NpcKindDef> {
        std::mem::take(&mut *self.pending_npc_kinds.lock().unwrap())
    }

    /// Drain the accumulated need defs.
    pub fn take_needs(&self) -> Vec<NeedDef> {
        std::mem::take(&mut *self.pending_needs.lock().unwrap())
    }

    /// Drain the accumulated animation defs.
    pub fn take_animations(&self) -> Vec<AnimationDef> {
        std::mem::take(&mut *self.pending_animations.lock().unwrap())
    }

    /// Drain the accumulated recipe defs.
    pub fn take_recipes(&self) -> Vec<RecipeDef> {
        std::mem::take(&mut *self.pending_recipes.lock().unwrap())
    }

    /// Take the optional work-action defaults. `None` ⇒ no mod called
    /// `engine.npcs.set_work_defaults`; the engine uses
    /// [`WorkDefaults::default`].
    pub fn take_work_defaults(&self) -> Option<WorkDefaults> {
        std::mem::take(&mut *self.pending_work_defaults.lock().unwrap())
    }

    /// Take the optional civilization-cluster params. `None` ⇒ no mod
    /// called `engine.civilization.set_params`; the engine uses
    /// [`CivilizationParams::default`].
    pub fn take_civilization_params(&self) -> Option<CivilizationParams> {
        std::mem::take(&mut *self.pending_civilization_params.lock().unwrap())
    }

    /// Take the optional material-drop multiplier. `None` ⇒ no mod
    /// called `engine.blocks.set_material_drop_multiplier`; the engine
    /// uses 1.0 (unspecified drops refund build cost exactly).
    pub fn take_material_drop_multiplier(&self) -> Option<f32> {
        std::mem::take(&mut *self.pending_material_drop_multiplier.lock().unwrap())
    }

    /// Clone the live `engine.ui.toast` queue handle. The engine stores
    /// this in a resource and drains it every tick — pushes from Lua at
    /// any later point land in the same queue.
    pub fn ui_toast_queue(&self) -> Arc<Mutex<Vec<UiToast>>> {
        self.ui_toasts.clone()
    }
}

pub struct LoadedMod {
    pub name: String,
    pub manifest: ModManifest,
    pub lua: Lua,
    pub disabled: bool,
}

pub struct ModRegistry {
    pub side: Side,
    pub mods: Vec<LoadedMod>,
}

impl ModRegistry {
    /// Load every immediate subdirectory of `mods_dir` that contains a
    /// `manifest.toml`. Each mod gets its own Lua state with the `engine`
    /// table appropriate for `side`. Mods that don't declare a script for
    /// `side` (or `shared`) are still listed as loaded — they just have no
    /// hooks registered, which is valid.
    pub fn load_dir(side: Side, mods_dir: &Path, ctx: &LoadContext) -> Result<Self, LoadError> {
        let mut mods = Vec::new();
        if !mods_dir.exists() {
            info!("no mods directory at {}", mods_dir.display());
            return Ok(Self { side, mods });
        }

        let entries = fs::read_dir(mods_dir).map_err(|e| LoadError::Io {
            path: mods_dir.to_owned(),
            source: e,
        })?;

        // Mod load order has to be stable so block-slot assignment is
        // deterministic across runs. Rule: `vanilla` first if present,
        // then everything else by directory name. Once we have an explicit
        // `load_after` field in the manifest, this becomes a topological
        // sort with the same vanilla-first behaviour falling out naturally.
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| LoadError::Io {
                path: mods_dir.to_owned(),
                source: e,
            })?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if !dir.join("manifest.toml").exists() {
                continue;
            }
            dirs.push(dir);
        }
        dirs.sort_by(|a, b| {
            let a_van = a.file_name().and_then(|n| n.to_str()) == Some("vanilla");
            let b_van = b.file_name().and_then(|n| n.to_str()) == Some("vanilla");
            b_van.cmp(&a_van).then_with(|| a.cmp(b))
        });
        for dir in dirs {
            mods.push(load_mod(side, &dir, ctx)?);
        }

        info!("[{}] loaded {} mod(s)", side.as_str(), mods.len());
        Ok(Self { side, mods })
    }

    /// Server-only: dispatch the after-place event to every active mod.
    /// Mods that error out are disabled for the rest of the session.
    pub fn dispatch_block_placed(&mut self, event: BlockPlacedEvent) {
        debug_assert_eq!(self.side, Side::Server);
        for m in &mut self.mods {
            if m.disabled {
                continue;
            }
            if let Err(e) = call_block_placed(&m.lua, &event) {
                error!(mod_name = %m.name, error = %e, "disabling mod after callback error");
                m.disabled = true;
            }
        }
    }

    /// Client-side: collect inspect-panel contributions for the hovered
    /// target. Each active mod's `engine.ui.on_inspect` callback may
    /// return one string (a line/block of text appended to the panel)
    /// or nil. A callback that errors disables its mod for the session,
    /// same policy as the server-side hooks.
    pub fn call_inspect(&mut self, target: &InspectTarget) -> Vec<String> {
        let mut lines = Vec::new();
        for m in &mut self.mods {
            if m.disabled {
                continue;
            }
            match call_inspect_hook(&m.lua, target) {
                Ok(Some(text)) => lines.push(text),
                Ok(None) => {}
                Err(e) => {
                    error!(mod_name = %m.name, error = %e, "disabling mod after callback error");
                    m.disabled = true;
                }
            }
        }
        lines
    }

    /// Server-only: dispatch a room-detector event to every active mod.
    pub fn dispatch_room_event(&mut self, event: &RoomEvent) {
        debug_assert_eq!(self.side, Side::Server);
        for m in &mut self.mods {
            if m.disabled {
                continue;
            }
            if let Err(e) = call_room_event(&m.lua, event) {
                error!(mod_name = %m.name, error = %e, "disabling mod after callback error");
                m.disabled = true;
            }
        }
    }

    /// Server-only: ask whichever active mod registered a planner for
    /// `kind` what the NPC should do next. Walks the mods in load order
    /// — the first one with a matching planner wins; duplicate
    /// registrations from a later mod aren't visible from here.
    ///
    /// Returns:
    /// - `Ok(Some(goal))` — planner returned a goal.
    /// - `Ok(None)` — no active mod has a planner for this kind. The
    ///   caller falls back to a native default (or stays Idle).
    /// - `Err(_)` — the matching planner errored. Per the design memo
    ///   this is a **per-NPC** problem: callers attach a "this NPC's
    ///   brain is disabled" marker to that one entity rather than
    ///   disabling the whole mod. A buggy planner that crashes for
    ///   every NPC will silence its NPCs one-by-one and log loudly
    ///   each time, but other mods + other kinds keep working.
    pub fn call_planner(
        &self,
        kind: &NpcKindId,
        snapshot: &NpcSnapshot,
    ) -> Result<Option<PlannerGoal>, PlannerError> {
        debug_assert_eq!(self.side, Side::Server);
        for m in &self.mods {
            if m.disabled {
                continue;
            }
            match call_npc_planner(&m.lua, kind, snapshot) {
                Ok(Some(goal)) => return Ok(Some(goal)),
                Ok(None) => continue,
                Err(source) => {
                    return Err(PlannerError {
                        mod_name: m.name.clone(),
                        kind: kind.clone(),
                        source,
                    });
                }
            }
        }
        Ok(None)
    }
}

/// Per-NPC planner failure. Carries enough context to log which mod +
/// kind misbehaved; the engine's recovery is to mark the offending NPC
/// brain-disabled and continue.
#[derive(Debug, Error)]
#[error("planner for {kind} in mod {mod_name} errored: {source}")]
pub struct PlannerError {
    pub mod_name: String,
    pub kind: NpcKindId,
    #[source]
    pub source: mlua::Error,
}

pub fn warn_if_empty(registry: &ModRegistry) {
    if registry.mods.is_empty() {
        warn!(
            "[{}] no mods loaded; place a directory with manifest.toml under ./mods",
            registry.side.as_str()
        );
    }
}

/// Fixed entry-point filenames. Mods drop a `data.lua` (declarative
/// registrations, runs on both sides) and/or an `events.lua` (runtime
/// callback registrations, server only). At least one must be present.
const DATA_SCRIPT: &str = "data.lua";
const EVENTS_SCRIPT: &str = "events.lua";

fn load_mod(side: Side, dir: &Path, ctx: &LoadContext) -> Result<LoadedMod, LoadError> {
    let manifest_path = dir.join("manifest.toml");
    let manifest_str = fs::read_to_string(&manifest_path).map_err(|e| LoadError::Io {
        path: manifest_path.clone(),
        source: e,
    })?;
    let manifest: ModManifest = toml::from_str(&manifest_str).map_err(|e| LoadError::Manifest {
        path: manifest_path,
        source: e,
    })?;

    let target =
        ApiVersion::parse(&manifest.api_version).map_err(|reason| LoadError::ApiVersion {
            name: manifest.name.clone(),
            target: manifest.api_version.clone(),
            reason: reason.to_owned(),
        })?;
    if !ApiVersion::is_compatible_with(target, API_VERSION) {
        return Err(LoadError::ApiVersion {
            name: manifest.name.clone(),
            target: manifest.api_version.clone(),
            reason: format!("host API is {API_VERSION}"),
        });
    }

    let has_data = dir.join(DATA_SCRIPT).exists();
    // `events.lua` only runs on the server, but its presence still counts
    // as a valid script for the no-scripts check on either side — an
    // events-only mod (no declarative content) is legitimate.
    let has_events = dir.join(EVENTS_SCRIPT).exists();
    if !has_data && !has_events {
        return Err(LoadError::NoScripts {
            name: manifest.name.clone(),
        });
    }

    let lua = Lua::new();
    install_engine_table(&lua, side, ctx).map_err(|source| LoadError::Lua {
        name: manifest.name.clone(),
        side: side.as_str(),
        source,
    })?;

    // data.lua runs first so events.lua can reference helpers/constants
    // it defines. Server-only events.lua follows; on the client we stop
    // after data.lua because no client-side hooks exist yet.
    if has_data {
        run_script(&lua, &manifest.name, side, dir, DATA_SCRIPT)?;
    }
    if has_events && side == Side::Server {
        run_script(&lua, &manifest.name, side, dir, EVENTS_SCRIPT)?;
    }

    info!(name = %manifest.name, version = %manifest.version, side = side.as_str(), "loaded mod");
    Ok(LoadedMod {
        name: manifest.name.clone(),
        manifest,
        lua,
        disabled: false,
    })
}

fn run_script(
    lua: &Lua,
    mod_name: &str,
    side: Side,
    dir: &Path,
    relative: &str,
) -> Result<(), LoadError> {
    let path = dir.join(relative);
    let src = fs::read_to_string(&path).map_err(|e| LoadError::Io {
        path: path.clone(),
        source: e,
    })?;
    let chunk_name = format!("{mod_name}/{relative}");
    lua.load(&src)
        .set_name(chunk_name)
        .exec()
        .map_err(|source| LoadError::Lua {
            name: mod_name.to_owned(),
            side: side.as_str(),
            source,
        })
}

/// Build the per-mod `engine` table. Functions exposed here differ by side —
/// hooks that mutate world state only exist on the server, etc.
fn install_engine_table(lua: &Lua, side: Side, ctx: &LoadContext) -> Result<(), mlua::Error> {
    let engine = lua.create_table()?;
    engine.set("side", side.as_str())?;

    // engine.blocks.register(def) — both sides; the same shared.lua runs in
    // both Lua states, so each side accumulates an identical block list and
    // ends up with matching slots.
    let blocks_table = lua.create_table()?;
    let pending = ctx.pending_blocks.clone();
    let register_block = lua.create_function(move |lua, def_value: Value| {
        let def: BlockDef = lua.from_value(def_value)?;
        let mut buf = pending.lock().unwrap();
        if buf.iter().any(|d| d.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate block id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    blocks_table.set("register", register_block)?;

    // engine.blocks.set_material_drop_multiplier(x) — both sides, so the
    // resolved drop lists agree across the wire. Scales `materials` when a
    // block leaves `drops` unspecified (see BlockDef::resolve_drops).
    // Single-writer, like set_work_defaults: a second caller fails loudly
    // instead of the winner depending on mod load order.
    let pending_mult = ctx.pending_material_drop_multiplier.clone();
    let set_material_drop_multiplier = lua.create_function(move |_, mult: f32| {
        if !mult.is_finite() || mult < 0.0 {
            return Err(mlua::Error::external(format!(
                "material drop multiplier must be finite and >= 0, got {mult}"
            )));
        }
        let mut slot = pending_mult.lock().unwrap();
        if let Some(existing) = *slot {
            return Err(mlua::Error::external(format!(
                "material drop multiplier already set to {existing}; \
                 only one mod may call set_material_drop_multiplier"
            )));
        }
        *slot = Some(mult);
        Ok(())
    })?;
    blocks_table.set("set_material_drop_multiplier", set_material_drop_multiplier)?;
    engine.set("blocks", blocks_table)?;

    // engine.items.register(def) — both sides accumulate identical item
    // sets so slot ordering matches across the wire. Items are world-
    // physical things actors can carry; see the `items` module in the
    // mod-API crate for the def shape.
    let items_table = lua.create_table()?;
    let pending_items = ctx.pending_items.clone();
    let register_item = lua.create_function(move |lua, value: Value| {
        let def: ItemDef = lua.from_value(value)?;
        let mut buf = pending_items.lock().unwrap();
        if buf.iter().any(|i| i.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate item id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    items_table.set("register", register_item)?;
    engine.set("items", items_table)?;

    // engine.rooms.register(pattern) — both sides accumulate the same set
    // since shared.lua runs in both Lua states. The engine builds parallel
    // RoomPatternRegistries that agree.
    let rooms_table = lua.create_table()?;
    let pending_rooms = ctx.pending_rooms.clone();
    let register_room = lua.create_function(move |lua, value: Value| {
        let pattern: RoomPattern = lua.from_value(value)?;
        let mut buf = pending_rooms.lock().unwrap();
        if buf.iter().any(|p| p.id == pattern.id) {
            return Err(mlua::Error::external(format!(
                "duplicate room pattern id {}",
                pattern.id
            )));
        }
        buf.push(pattern);
        Ok(())
    })?;
    rooms_table.set("register", register_room)?;
    engine.set("rooms", rooms_table)?;

    // engine.needs.register(def) — both sides accumulate; only the server
    // currently consults the need registry (needs decay during the brain
    // tick), but registering on both sides keeps slot ordering identical
    // so a future client-side debug HUD reads the same indices.
    let needs_table = lua.create_table()?;
    let pending_needs = ctx.pending_needs.clone();
    let register_need = lua.create_function(move |lua, value: Value| {
        let def: NeedDef = lua.from_value(value)?;
        let mut buf = pending_needs.lock().unwrap();
        if buf.iter().any(|n| n.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate need id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    needs_table.set("register", register_need)?;
    // engine.needs.get(id) — read back a previously-registered need
    // (including urge_threshold + preempt_threshold) so the planner can
    // gate decisions on data-driven cutoffs instead of file-local
    // constants. Returns nil if `id` wasn't registered. Scoped to this
    // Lua state's accumulation buffer; cross-mod reads aren't supported
    // (mods get the full registry on the engine side only).
    let pending_needs_get = ctx.pending_needs.clone();
    let get_need = lua.create_function(move |lua, id: String| -> mlua::Result<Value> {
        let buf = pending_needs_get.lock().unwrap();
        match buf.iter().find(|n| n.id.as_str() == id) {
            Some(def) => {
                Ok(lua.to_value_with(def, SerializeOptions::new().serialize_none_to_null(false))?)
            }
            None => Ok(Value::Nil),
        }
    })?;
    needs_table.set("get", get_need)?;
    engine.set("needs", needs_table)?;

    // engine.animations.register(def) — both sides; the client loads
    // each registered clip's glTF asset at boot and builds a unified
    // AnimationGraph keyed by id, while the server only validates
    // that references from NpcKindDef + UseSlot resolve.
    let animations_table = lua.create_table()?;
    let pending_animations = ctx.pending_animations.clone();
    let register_animation = lua.create_function(move |lua, value: Value| {
        let def: AnimationDef = lua.from_value(value)?;
        let mut buf = pending_animations.lock().unwrap();
        if buf.iter().any(|a| a.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate animation id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    animations_table.set("register", register_animation)?;
    engine.set("animations", animations_table)?;

    // engine.recipes.register(def) — both sides accumulate. Recipes
    // are display data on the client (crafting UI, station inspect
    // panel) and execution data on the server (crafting handler).
    // Slot ordering matches across sides so a future wire-friendly
    // RecipeSlot type drops in cleanly.
    let recipes_table = lua.create_table()?;
    let pending_recipes = ctx.pending_recipes.clone();
    let register_recipe = lua.create_function(move |lua, value: Value| {
        let def: RecipeDef = lua.from_value(value)?;
        let mut buf = pending_recipes.lock().unwrap();
        if buf.iter().any(|r| r.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate recipe id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    recipes_table.set("register", register_recipe)?;
    engine.set("recipes", recipes_table)?;

    // engine.npcs.register(def) — both sides accumulate. set_planner is
    // server-only because planners run against authoritative state.
    let npcs_table = lua.create_table()?;
    let pending_npcs = ctx.pending_npc_kinds.clone();
    let register_npc = lua.create_function(move |lua, value: Value| {
        let def: NpcKindDef = lua.from_value(value)?;
        let mut buf = pending_npcs.lock().unwrap();
        if buf.iter().any(|n| n.id == def.id) {
            return Err(mlua::Error::external(format!(
                "duplicate npc kind id {}",
                def.id
            )));
        }
        buf.push(def);
        Ok(())
    })?;
    npcs_table.set("register", register_npc)?;

    // engine.npcs.set_work_defaults(def) — both sides accumulate; only
    // one mod may set this per session. The validator (engine-side, in
    // npc_registry.rs) cross-checks the need against the registered
    // NeedRegistry at boot. A nil call clears the slot; mods can use
    // this in shared.lua to express "I want the engine fallback."
    let pending_work_defaults = ctx.pending_work_defaults.clone();
    let set_work_defaults = lua.create_function(move |lua, value: Value| {
        let def: WorkDefaults = lua.from_value(value)?;
        let mut slot = pending_work_defaults.lock().unwrap();
        if slot.is_some() {
            return Err(mlua::Error::external(
                "engine.npcs.set_work_defaults called twice; only one mod may set work defaults",
            ));
        }
        *slot = Some(def);
        Ok(())
    })?;
    npcs_table.set("set_work_defaults", set_work_defaults)?;

    if side == Side::Server {
        // Pre-create the planners table so set_planner's writes never
        // hit a nil parent. Mods that don't call set_planner just leave
        // it empty; dispatch checks before invoking.
        let planners = lua.create_table()?;
        engine.set(NPC_PLANNERS_TABLE, planners)?;

        let set_planner = lua.create_function(|lua, (kind, callback): (String, Function)| {
            let engine: Table = lua.globals().get("engine")?;
            let planners: Table = engine.get(NPC_PLANNERS_TABLE)?;
            planners.set(kind, callback)?;
            Ok(())
        })?;
        npcs_table.set("set_planner", set_planner)?;
    }

    engine.set("npcs", npcs_table)?;

    // engine.civilization.set_params({ max_room_distance_cells = N,
    //                                  buffer_cells = M }) — same
    // single-writer rule as set_work_defaults; vanilla owns the slot,
    // a second caller errors loudly so two mods can't silently disagree.
    let civ_table = lua.create_table()?;
    let pending_civ_params = ctx.pending_civilization_params.clone();
    let set_civ_params = lua.create_function(move |lua, value: Value| {
        let def: CivilizationParams = lua.from_value(value)?;
        let mut slot = pending_civ_params.lock().unwrap();
        if slot.is_some() {
            return Err(mlua::Error::external(
                "engine.civilization.set_params called twice; only one mod may set civilization params",
            ));
        }
        *slot = Some(def);
        Ok(())
    })?;
    civ_table.set("set_params", set_civ_params)?;
    engine.set("civilization", civ_table)?;

    // engine.ui — the content-level UI seed (see mod-api's `ui` module
    // docs). Both sides expose the same two entries:
    //   - toast(pos, text): pushes into this side's runtime queue. On
    //     the server the engine broadcasts drained toasts to every
    //     client; on the client they render locally.
    //   - on_inspect(fn): stores a hover-inspect callback. Only the
    //     client ever calls it, but registering is side-agnostic so
    //     mods can do it from data.lua without an `engine.side` check.
    let ui_table = lua.create_table()?;
    let toast_queue = ctx.ui_toasts.clone();
    let push_toast = lua.create_function(move |lua, (pos, text): (Value, String)| {
        let pos: BlockPos = lua.from_value(pos)?;
        toast_queue.lock().unwrap().push(UiToast { pos, text });
        Ok(())
    })?;
    ui_table.set("toast", push_toast)?;
    let register_inspect = lua.create_function(|lua, callback: Function| {
        let engine: Table = lua.globals().get("engine")?;
        engine.set(INSPECT_SLOT, callback)?;
        Ok(())
    })?;
    ui_table.set("on_inspect", register_inspect)?;
    engine.set("ui", ui_table)?;

    if side == Side::Server {
        let register = lua.create_function(|lua, callback: Function| {
            let engine: Table = lua.globals().get("engine")?;
            engine.set(BLOCK_PLACED_SLOT, callback)?;
            Ok(())
        })?;
        engine.set("on_block_placed", register)?;

        let register_room = lua.create_function(|lua, callback: Function| {
            let engine: Table = lua.globals().get("engine")?;
            engine.set(ROOM_EVENT_SLOT, callback)?;
            Ok(())
        })?;
        engine.set("on_room_event", register_room)?;
    }

    lua.globals().set("engine", engine)?;
    Ok(())
}

/// Serialize options used for every event we hand to a Lua callback.
/// `serialize_none_to_null = false` makes `Option::None` arrive as Lua
/// `nil` rather than mlua's `null` lightuserdata, so handler code can
/// write the natural `event.field or "(none)"` instead of having to
/// reach into mlua to compare against the null sentinel.
fn lua_event_options() -> SerializeOptions {
    SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false)
}

fn call_block_placed(lua: &Lua, event: &BlockPlacedEvent) -> Result<(), mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let handler: Value = engine.get(BLOCK_PLACED_SLOT)?;
    let Value::Function(handler) = handler else {
        return Ok(());
    };
    let event_value = lua.to_value_with(event, lua_event_options())?;
    handler.call::<()>(event_value)
}

/// Invoke one mod's inspect hook. `Ok(None)` covers both "no handler
/// registered" and "handler returned nil"; any non-string non-nil
/// return is an error (the mod author's bug should be loud).
fn call_inspect_hook(lua: &Lua, target: &InspectTarget) -> Result<Option<String>, mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let handler: Value = engine.get(INSPECT_SLOT)?;
    let Value::Function(handler) = handler else {
        return Ok(None);
    };
    let target_value = lua.to_value_with(target, lua_event_options())?;
    let returned: Value = handler.call(target_value)?;
    match returned {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_owned())),
        other => Err(mlua::Error::external(format!(
            "engine.ui.on_inspect callback must return a string or nil, got {}",
            other.type_name()
        ))),
    }
}

fn call_room_event(lua: &Lua, event: &RoomEvent) -> Result<(), mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let handler: Value = engine.get(ROOM_EVENT_SLOT)?;
    let Value::Function(handler) = handler else {
        return Ok(());
    };
    let event_value = lua.to_value_with(event, lua_event_options())?;
    handler.call::<()>(event_value)
}

/// Look up a planner for `kind` in this Lua state. Returns:
/// - `Ok(Some(goal))` — planner present and returned a parseable goal.
/// - `Ok(None)` — no planner registered for this kind in this state.
/// - `Err(_)` — planner was present but errored (call failed, returned
///   non-table, or returned something that doesn't parse as a
///   [`PlannerGoal`]).
fn call_npc_planner(
    lua: &Lua,
    kind: &NpcKindId,
    snapshot: &NpcSnapshot,
) -> Result<Option<PlannerGoal>, mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let planners: Value = engine.get(NPC_PLANNERS_TABLE)?;
    let Value::Table(planners) = planners else {
        return Ok(None);
    };
    let handler: Value = planners.get(kind.as_str())?;
    let Value::Function(handler) = handler else {
        return Ok(None);
    };
    let snapshot_value = lua.to_value_with(snapshot, lua_event_options())?;
    let returned: Value = handler.call(snapshot_value)?;
    let goal: PlannerGoal = lua.from_value(returned)?;
    Ok(Some(goal))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Lua state with the engine table installed, plus the context its
    /// registrations land in — the same wiring `load_mod` gives real mods.
    fn engine_fixture() -> (Lua, LoadContext) {
        let ctx = LoadContext::new();
        let lua = Lua::new();
        install_engine_table(&lua, Side::Server, &ctx).expect("install engine table");
        (lua, ctx)
    }

    fn register_test_block(lua: &Lua, extra_fields: &str) {
        lua.load(format!(
            "engine.blocks.register {{
                 id = 'test:block',
                 display_name = 'Test',
                 flags = {{ solid = true }},
                 color = {{ 0.5, 0.5, 0.5 }},
                 {extra_fields}
             }}"
        ))
        .exec()
        .expect("register block");
    }

    /// The three drops spellings must survive the Lua→serde boundary
    /// distinctly: omitted = None (defaults to materials at resolve),
    /// `{}` = Some(empty) (yields nothing), a list = Some(list).
    #[test]
    fn lua_drops_spellings_map_to_option_shapes() {
        let (lua, ctx) = engine_fixture();
        register_test_block(&lua, "materials = { { item = 'test:log', count = 2 } },");
        let blocks = ctx.take_blocks();
        assert!(
            blocks[0].drops.is_none(),
            "omitted drops must parse as None"
        );

        let (lua, ctx) = engine_fixture();
        register_test_block(
            &lua,
            "materials = { { item = 'test:log', count = 2 } }, drops = {},",
        );
        let blocks = ctx.take_blocks();
        assert_eq!(
            blocks[0].drops.as_deref(),
            Some(&[][..]),
            "explicit empty table must parse as Some(empty), not None"
        );

        let (lua, ctx) = engine_fixture();
        register_test_block(&lua, "drops = { { item = 'test:log', count = 5 } },");
        let blocks = ctx.take_blocks();
        assert_eq!(blocks[0].drops.as_ref().map(Vec::len), Some(1));
        assert_eq!(blocks[0].drops.as_ref().unwrap()[0].count, 5);
    }

    /// The S2 pile knobs survive the Lua→serde boundary: `bulk`, the
    /// optional `pile` table with its `tiers` list, and the derived
    /// helpers (capacity from bulk, tier mesh by count). An item with no
    /// `pile` stays non-pileable on the base mesh.
    #[test]
    fn item_pile_config_deserializes() {
        let (lua, ctx) = engine_fixture();
        lua.load(
            "engine.items.register {
                 id = 'test:plank',
                 display_name = 'Plank',
                 mesh = 'mods://test/base.gltf',
                 bulk = 2,
                 pile = {
                     tiers = {
                         { min = 2, mesh = 'mods://test/small.gltf' },
                         { min = 4, mesh = 'mods://test/large.gltf' },
                     },
                 },
             }",
        )
        .exec()
        .expect("register pile item");
        let items = ctx.take_items();
        let def = &items[0];
        assert_eq!(def.bulk, 2);
        assert!(def.is_pileable());
        assert_eq!(def.pile_capacity(), 6, "12 bulk / 2 = 6 units");
        assert_eq!(def.pile_mesh(1), "mods://test/base.gltf", "single = base");
        assert_eq!(def.pile_mesh(3), "mods://test/small.gltf", "2-3 = small tier");
        assert_eq!(def.pile_mesh(9), "mods://test/large.gltf", "4+ = large tier");

        let (lua, ctx) = engine_fixture();
        lua.load(
            "engine.items.register {
                 id = 'test:axe',
                 display_name = 'Axe',
                 mesh = 'mods://test/axe.gltf',
             }",
        )
        .exec()
        .expect("register plain item");
        let items = ctx.take_items();
        assert_eq!(items[0].bulk, 1, "bulk defaults to 1");
        assert!(!items[0].is_pileable(), "no pile config ⇒ not pileable");
        assert_eq!(items[0].pile_mesh(5), "mods://test/axe.gltf");
    }

    /// Multiplier setter: value lands in the context; a second caller
    /// fails loudly (single-writer, like set_work_defaults).
    #[test]
    fn material_drop_multiplier_is_single_writer() {
        let (lua, ctx) = engine_fixture();
        lua.load("engine.blocks.set_material_drop_multiplier(0.5)")
            .exec()
            .expect("first set succeeds");
        assert!(
            lua.load("engine.blocks.set_material_drop_multiplier(2.0)")
                .exec()
                .is_err(),
            "second set must error"
        );
        assert_eq!(ctx.take_material_drop_multiplier(), Some(0.5));
    }

    /// NaN, infinity, and negatives are rejected at the API edge so a
    /// bad value can't silently zero (or explode) every default drop.
    #[test]
    fn material_drop_multiplier_rejects_garbage() {
        let (lua, ctx) = engine_fixture();
        for bad in ["-1.0", "0/0", "math.huge"] {
            assert!(
                lua.load(format!("engine.blocks.set_material_drop_multiplier({bad})"))
                    .exec()
                    .is_err(),
                "{bad} must be rejected"
            );
        }
        assert_eq!(ctx.take_material_drop_multiplier(), None);
    }
}
