//! Lua scripting host for block-junk.
//!
//! Each mod gets its own [`mlua::Lua`] state per side — no shared globals
//! between mods, and no shared runtime state between client and server (each
//! side spins up its own [`ModRegistry`]). When a hook callback errors, the
//! mod is **disabled for the rest of the session** and a loud `error!` is
//! logged. The engine never silently continues with corrupt mod state.

mod callbacks;
pub mod commands;
mod composition;
mod definitions;
pub mod events;
mod list_patch;
pub mod resources;
mod sandbox;
pub mod state;
pub use sandbox::ScriptLimits;

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
use mlua::{Lua, LuaSerdeExt, SerializeOptions, Table, Value};
use thiserror::Error;
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("content composition: {0}")]
    Composition(String),
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
    pub commands: commands::CommandOutbox,
    frozen_definitions: Arc<Mutex<std::collections::BTreeMap<String, serde_json::Value>>>,
    pub mod_state: state::StateStore,
    state_schemas: Arc<Mutex<std::collections::BTreeMap<String, state::StateSchema>>>,
    pub field_provenance:
        Arc<Mutex<std::collections::BTreeMap<String, Vec<definitions::FieldSource>>>>,
    pub pending_movement: Arc<Mutex<Vec<block_junk_mod_api::architecture::MovementProfile>>>,
    pub pending_settlements:
        Arc<Mutex<Vec<block_junk_mod_api::architecture::SettlementDefinition>>>,
    pub pending_capabilities:
        Arc<Mutex<Vec<block_junk_mod_api::capabilities::CapabilityDefinition>>>,
    pub pending_worlds: Arc<Mutex<Vec<block_junk_mod_api::worlds::WorldPreset>>>,
    pub pending_world_upgrades: Arc<Mutex<Vec<block_junk_mod_api::upgrades::WorldUpgrade>>>,
    callback_owners: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
    pub provenance: Arc<Mutex<std::collections::BTreeMap<String, Vec<definitions::Source>>>>,
    phase: Arc<std::sync::atomic::AtomicU8>,
    pub pending_blocks: Arc<Mutex<Vec<BlockDef>>>,
    pub pending_items: Arc<Mutex<Vec<ItemDef>>>,
    pub pending_rooms: Arc<Mutex<Vec<RoomPattern>>>,
    pub pending_npc_kinds: Arc<Mutex<Vec<NpcKindDef>>>,
    pub pending_needs: Arc<Mutex<Vec<NeedDef>>>,
    pub pending_animations: Arc<Mutex<Vec<AnimationDef>>>,
    pub pending_recipes: Arc<Mutex<Vec<RecipeDef>>>,
    /// Composed work-action defaults; replacement requires explicit ownership.
    pub pending_work_defaults: Arc<Mutex<Option<WorkDefaults>>>,
    /// Composed civilization-cluster parameters.
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

    /// Publish engine-resolved values after validation (for example implicit drops).
    pub fn publish_definition<T: serde::Serialize>(&self, category: &str, id: &str, value: &T) {
        self.frozen_definitions.lock().unwrap().insert(
            format!("{category}/{id}"),
            serde_json::to_value(value).expect("validated definition serializes"),
        );
    }

    pub fn record_derivation(&self, field: String, inputs: Vec<String>, operation: &str) {
        self.field_provenance
            .lock()
            .unwrap()
            .entry(field)
            .or_default()
            .push(definitions::FieldSource {
                source: definitions::Source {
                    mod_id: "engine".into(),
                    stage: operation.into(),
                },
                defaulted: false,
                derived_from: inputs,
            });
    }

    pub fn explain_validation(&self, error: &str) -> String {
        let sources = self.provenance.lock().unwrap();
        let mut details = Vec::new();
        for (definition, history) in sources.iter() {
            let Some((_, id)) = definition.split_once('/') else {
                continue;
            };
            if !error.contains(id) {
                continue;
            }
            if let Some(source) = history.last() {
                details.push(format!(
                    "{definition}: {} / {}",
                    source.mod_id, source.stage
                ));
            }
            for (path, fields) in self
                .field_provenance
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path.starts_with(&format!("{definition}/")))
            {
                if let Some(field) = path.rsplit('/').next()
                    && error.contains(field)
                    && let Some(source) = fields.last()
                {
                    details.push(format!(
                        "{path}: {} / {}{}",
                        source.source.mod_id,
                        source.source.stage,
                        if source.defaulted { " (default)" } else { "" }
                    ));
                    for input in &source.derived_from {
                        let owner = sources
                            .iter()
                            .filter(|(key, _)| input.starts_with(key.as_str()))
                            .max_by_key(|(key, _)| key.len())
                            .and_then(|(_, history)| history.last());
                        details.push(match owner {
                            Some(owner) => {
                                format!("derived from {input}: {} / {}", owner.mod_id, owner.stage)
                            }
                            None => format!("derived from {input}: engine default"),
                        });
                    }
                }
            }
            if details.len() >= 8 {
                break;
            }
        }
        if details.is_empty() {
            error.into()
        } else {
            format!("{error}\n{}", details.join("\n"))
        }
    }

    /// Canonical post-composition content, including policy data formerly absent
    /// from the network agreement. Draining startup buffers does not alter this hash.
    pub fn definition_snapshot(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        self.frozen_definitions.lock().unwrap().clone()
    }

    pub fn fingerprint(&self) -> u64 {
        let definitions = self.frozen_definitions.lock().unwrap();
        let setting = |key: &str, default: serde_json::Value| {
            definitions.get(key).cloned().unwrap_or(default)
        };
        let mut value = serde_json::json!({
            "api": API_VERSION,
            "state_schemas": &*self.state_schemas.lock().unwrap(),
            "definitions": &*definitions,
            "work_defaults": setting("npcs/work_defaults",serde_json::to_value(WorkDefaults::default()).unwrap()),
            "civilization": setting("civilization/params",serde_json::to_value(CivilizationParams::default()).unwrap()),
            "drop_multiplier": setting("blocks/material_drop_multiplier",serde_json::json!(1.0)),
        });
        value.sort_all_objects();
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in serde_json::to_vec(&value).expect("content serializes") {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
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

    /// Drain pending needs; runtime queries use the frozen definition snapshot.
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
    pub directory: PathBuf,
    pub name: String,
    pub manifest: ModManifest,
    pub lua: Lua,
    pub disabled: bool,
}

pub struct ModRegistry {
    events: events::EventQueue,
    failures: std::collections::VecDeque<events::BehaviorFailure>,
    pub commands: commands::CommandOutbox,
    pub mod_state: state::StateStore,
    state_schemas: Arc<Mutex<std::collections::BTreeMap<String, state::StateSchema>>>,
    pub source_fingerprint: u64,
    callback_owners: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
    phase: Arc<std::sync::atomic::AtomicU8>,
    runtime_started: bool,
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
        Self::load_dir_with_limits(side, mods_dir, ctx, ScriptLimits::default())
    }

    pub fn load_dir_with_limits(
        side: Side,
        mods_dir: &Path,
        ctx: &LoadContext,
        limits: ScriptLimits,
    ) -> Result<Self, LoadError> {
        let mut registry = Self::load_composed(side, mods_dir, ctx, limits)?;
        registry.activate_runtime()?;
        Ok(registry)
    }

    /// Engine entry point: compose definitions, then validate engine references,
    /// then call activate_runtime. No events script runs before that validation.
    pub fn load_composed(
        side: Side,
        mods_dir: &Path,
        ctx: &LoadContext,
        limits: ScriptLimits,
    ) -> Result<Self, LoadError> {
        if limits.memory_bytes == 0 || limits.instructions_per_call == 0 {
            return Err(LoadError::Composition(
                "script budgets must be positive".into(),
            ));
        }
        let mut mods = Vec::new();
        if !mods_dir.exists() {
            info!("no mods directory at {}", mods_dir.display());
            return Ok(Self {
                events: Default::default(),
                failures: Default::default(),
                commands: ctx.commands.clone(),
                mod_state: ctx.mod_state.clone(),
                state_schemas: ctx.state_schemas.clone(),
                source_fingerprint: 0,
                side,
                mods,
                callback_owners: ctx.callback_owners.clone(),
                phase: ctx.phase.clone(),
                runtime_started: false,
            });
        }

        let discovered = composition::discover(mods_dir)?;
        let source_fingerprint = composition::fingerprint_sources(&discovered)?;
        ctx.phase.store(0, std::sync::atomic::Ordering::Relaxed);
        for (dir, manifest) in &discovered {
            mods.push(load_mod(side, dir, manifest.clone(), ctx, limits)?);
        }
        ctx.phase.store(1, std::sync::atomic::Ordering::Relaxed);
        for ((dir, _), m) in discovered.iter().zip(&mods) {
            if dir.join("patches.lua").exists() {
                run_script(&m.lua, &m.name, side, dir, "patches.lua")?;
            }
        }
        ctx.phase.store(3, std::sync::atomic::Ordering::Relaxed);
        info!("[{}] loaded {} mod(s)", side.as_str(), mods.len());
        Ok(Self {
            events: Default::default(),
            failures: Default::default(),
            commands: ctx.commands.clone(),
            mod_state: ctx.mod_state.clone(),
            state_schemas: ctx.state_schemas.clone(),
            source_fingerprint,
            side,
            mods,
            callback_owners: ctx.callback_owners.clone(),
            phase: ctx.phase.clone(),
            runtime_started: false,
        })
    }

    pub fn activate_runtime(&mut self) -> Result<(), LoadError> {
        if self.runtime_started {
            return Ok(());
        }
        self.runtime_started = true;
        self.phase.store(2, std::sync::atomic::Ordering::Relaxed);
        let result = (|| {
            for m in &self.mods {
                if self.side == Side::Server && m.directory.join(EVENTS_SCRIPT).exists() {
                    run_script(&m.lua, &m.name, self.side, &m.directory, EVENTS_SCRIPT)?;
                }
            }
            self.validate_restored_subscriptions()
                .map_err(LoadError::Composition)?;
            Ok(())
        })();
        self.phase.store(3, std::sync::atomic::Ordering::Relaxed);
        if result.is_err() {
            for m in &mut self.mods {
                m.disabled = true;
            }
        }
        result
    }

    fn dispatch_subscription<T: serde::Serialize>(
        &mut self,
        event: &str,
        value: &T,
    ) -> Vec<String> {
        let prefix = format!("subscription/{event}/");
        let subscriptions: Vec<_> = self
            .callback_owners
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, owner)| (key.clone(), owner.clone()))
            .collect();
        let mut lines = Vec::new();
        for (key, owner) in subscriptions {
            let Some(m) = self.mods.iter_mut().find(|m| m.manifest.id == owner) else {
                continue;
            };
            if m.disabled {
                continue;
            }
            let result = m
                .lua
                .to_value_with(value, lua_event_options())
                .and_then(|value| callbacks::call(&m.lua, &key, value));
            match result {
                Ok(Value::String(text)) if event == "inspect" => match text.to_str() {
                    Ok(text) => lines.push(text.to_owned()),
                    Err(_) => m.disabled = true,
                },
                Ok(Value::Nil) => {}
                Ok(_) if event != "inspect" => {}
                Ok(_) => {
                    error!(mod_name=%owner, "inspect callback returned neither string nor nil");
                    m.disabled = true;
                }
                Err(error) => {
                    error!(mod_name=%owner, %error, "disabling failed subscription owner");
                    m.disabled = true;
                }
            }
            if m.disabled {
                self.failures.push_back(events::BehaviorFailure {
                    owner,
                    behavior: key,
                    cause: None,
                    reason: "inspection subscription failed; callback commands were discarded"
                        .into(),
                });
            }
        }
        lines
    }

    pub fn dispatch_block_placed(
        &mut self,
        event: BlockPlacedEvent,
    ) -> Result<events::EventCause, String> {
        debug_assert_eq!(self.side, Side::Server);
        self.enqueue_event("block_placed", &event, None)
    }

    pub fn call_inspect(&mut self, target: &InspectTarget) -> Vec<String> {
        self.dispatch_subscription("inspect", target)
    }

    pub fn dispatch_room_event(&mut self, event: &RoomEvent) -> Result<events::EventCause, String> {
        debug_assert_eq!(self.side, Side::Server);
        self.enqueue_event("room_event", event, None)
    }

    /// Server-only: ask whichever active mod registered a planner for
    /// `kind` what the NPC should do next. The last explicitly authorized
    /// replacement owns the callback; composition rejects conflicting writers.
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
    pub fn planner_owner(&self, kind: &str) -> Option<String> {
        self.callback_owners
            .lock()
            .unwrap()
            .get(&format!("planner/{kind}"))
            .cloned()
    }

    pub fn call_planner(
        &self,
        kind: &NpcKindId,
        snapshot: &NpcSnapshot,
    ) -> Result<Option<PlannerGoal>, PlannerError> {
        debug_assert_eq!(self.side, Side::Server);
        let owner = self
            .callback_owners
            .lock()
            .unwrap()
            .get(&format!("planner/{}", kind.as_str()))
            .cloned();
        let Some(owner) = owner else {
            return Ok(None);
        };
        let Some(m) = self.mods.iter().find(|m| m.manifest.id == owner) else {
            return Err(PlannerError {
                mod_name: owner,
                kind: kind.clone(),
                source: mlua::Error::external("planner owner is unavailable"),
            });
        };
        if m.disabled {
            return Err(PlannerError {
                mod_name: owner,
                kind: kind.clone(),
                source: mlua::Error::external("planner owner is disabled"),
            });
        }
        call_npc_planner(&m.lua, kind, snapshot).map_err(|source| PlannerError {
            mod_name: owner,
            kind: kind.clone(),
            source,
        })
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

fn load_mod(
    side: Side,
    dir: &Path,
    manifest: ModManifest,
    ctx: &LoadContext,
    limits: ScriptLimits,
) -> Result<LoadedMod, LoadError> {
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
    if !has_data && !has_events && !dir.join("patches.lua").exists() {
        return Err(LoadError::NoScripts {
            name: manifest.name.clone(),
        });
    }

    let lua = sandbox::create(dir, &manifest, limits).map_err(|source| LoadError::Lua {
        name: manifest.name.clone(),
        side: side.as_str(),
        source,
    })?;
    install_engine_table(&lua, side, ctx).map_err(|source| LoadError::Lua {
        name: manifest.name.clone(),
        side: side.as_str(),
        source,
    })?;

    definitions::install(&lua, ctx, &manifest)
        .and_then(|_| callbacks::install(&lua, ctx, &manifest))
        .and_then(|_| state::install(&lua, ctx, &manifest.id))
        .and_then(|_| commands::install(&lua, side, ctx, &manifest.id))
        .map_err(|source| LoadError::Lua {
            name: manifest.name.clone(),
            side: side.as_str(),
            source,
        })?;

    // Registry orchestration runs every data script before any patch or runtime hook.
    if has_data {
        run_script(&lua, &manifest.name, side, dir, DATA_SCRIPT)?;
    }

    info!(name = %manifest.name, version = %manifest.version, side = side.as_str(), "loaded mod");
    Ok(LoadedMod {
        directory: dir.to_owned(),
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
        .set_mode(mlua::ChunkMode::Text)
        .into_function()
        .and_then(|function| sandbox::call::<()>(lua, function, ()))
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
    engine.set("worlds", lua.create_table()?)?;
    engine.set("world_upgrades", lua.create_table()?)?;
    engine.set("movement", lua.create_table()?)?;
    engine.set("settlements", lua.create_table()?)?;
    engine.set("capabilities", lua.create_table()?)?;

    // engine.blocks.register(def) — both sides; the same shared.lua runs in
    // both Lua states, so each side accumulates an identical block list and
    // ends up with matching slots.
    let blocks_table = lua.create_table()?;

    // engine.blocks.set_material_drop_multiplier(x) — both sides, so the
    // resolved drop lists agree across the wire. Scales `materials` when a
    // block leaves `drops` unspecified (see BlockDef::resolve_drops).
    // Single-writer, like set_work_defaults: a second caller fails loudly
    // instead of the winner depending on mod load order.
    engine.set("blocks", blocks_table)?;

    // engine.items.register(def) — both sides accumulate identical item
    // sets so slot ordering matches across the wire. Items are world-
    // physical things actors can carry; see the `items` module in the
    // mod-API crate for the def shape.
    let items_table = lua.create_table()?;
    engine.set("items", items_table)?;

    // engine.rooms.register(pattern) — both sides accumulate the same set
    // since shared.lua runs in both Lua states. The engine builds parallel
    // RoomPatternRegistries that agree.
    let rooms_table = lua.create_table()?;
    engine.set("rooms", rooms_table)?;

    // engine.needs.register(def) — both sides accumulate; only the server
    // currently consults the need registry (needs decay during the brain
    // tick), but registering on both sides keeps slot ordering identical
    // so a future client-side debug HUD reads the same indices.
    let needs_table = lua.create_table()?;
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
    engine.set("animations", animations_table)?;

    // engine.recipes.register(def) — both sides accumulate. Recipes
    // are display data on the client (crafting UI, station inspect
    // panel) and execution data on the server (crafting handler).
    // Slot ordering matches across sides so a future wire-friendly
    // RecipeSlot type drops in cleanly.
    let recipes_table = lua.create_table()?;
    engine.set("recipes", recipes_table)?;

    // engine.npcs.register(def) — both sides accumulate. set_planner is
    // server-only because planners run against authoritative state.
    let npcs_table = lua.create_table()?;

    // Composed work defaults are validated against needs by the engine.

    if side == Side::Server {
        // Pre-create the planners table so set_planner's writes never
        // hit a nil parent. Mods that don't call set_planner just leave
        // it empty; dispatch checks before invoking.
        let planners = lua.create_table()?;
        engine.set(NPC_PLANNERS_TABLE, planners)?;
    }

    engine.set("npcs", npcs_table)?;

    // Civilization settings use the same explicit composition ownership.
    let civ_table = lua.create_table()?;
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
    lua.set_app_data(toast_queue.clone());
    let phase = ctx.phase.clone();
    let push_toast = lua.create_function(move |lua, (pos, text): (Value, String)| {
        if phase.load(std::sync::atomic::Ordering::Relaxed) == 4 {
            return Err(mlua::Error::external(
                "world effects are forbidden during state migration",
            ));
        }
        let pos: BlockPos = lua.from_value(pos)?;
        let mut queue = toast_queue.lock().unwrap();
        if queue.len() >= 256 || text.len() > 4096 {
            return Err(mlua::Error::external("mod toast command budget exceeded"));
        }
        queue.push(UiToast { pos, text });
        Ok(())
    })?;
    ui_table.set("toast", push_toast)?;
    engine.set("ui", ui_table)?;

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

/// Look up a planner for `kind` in this Lua state. Returns:
/// - `Ok(Some(goal))` — planner present and returned a parseable goal.
/// - `Ok(None)` — no planner registered for this kind in this state.
/// - `Err(_)` — planner was present but errored (call failed, returned
///   non-table, or returned something that doesn't parse as a
///   [`PlannerGoal`]).
struct DecodedGoal(PlannerGoal);
impl mlua::FromLua for DecodedGoal {
    fn from_lua(value: Value, lua: &Lua) -> mlua::Result<Self> {
        definitions::validate_input(&value)?;
        lua.from_value(value).map(Self)
    }
}

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
    commands::cause(lua, None);
    commands::behavior(lua, &format!("planner/{}", kind.as_str()));
    let snapshot_value = lua.to_value_with(snapshot, lua_event_options())?;
    let goal: DecodedGoal = sandbox::call(lua, handler, snapshot_value)?;
    Ok(Some(goal.0))
}

/// Evaluate a standalone data resource with the production callback budgets.
/// Returned tables are plain, acyclic data: parsing cannot trigger unbounded
/// metamethod execution after the host coroutine has finished.
pub fn evaluate_data(source: &str, name: &str) -> mlua::Result<(Lua, Value)> {
    let lua = Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8,
        mlua::LuaOptions::default(),
    )?;
    let limits = ScriptLimits::default();
    lua.set_memory_limit(limits.memory_bytes)?;
    lua.set_app_data(limits);
    for name in ["dofile", "loadfile", "load", "collectgarbage"] {
        lua.globals().set(name, Value::Nil)?;
    }
    let function = lua
        .load(source)
        .set_name(name)
        .set_mode(mlua::ChunkMode::Text)
        .into_function()?;
    let value: Value = sandbox::call(&lua, function, ())?;
    definitions::validate_input(&value)?;
    Ok((lua, value))
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
        let manifest =
            toml::from_str("id='test'\nname='test'\nversion='1.0.0'\napi_version='0.6.0'").unwrap();
        definitions::install(&lua, &ctx, &manifest).unwrap();
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

    /// The S4 forage-transition knobs survive the Lua→serde boundary:
    /// `depleted_block` (what a harvested/eaten block becomes) and the
    /// `regrow` table (`into` + `after_secs`). Omitted ⇒ both None.
    #[test]
    fn lua_forage_transition_fields_deserialize() {
        let (lua, ctx) = engine_fixture();
        register_test_block(
            &lua,
            "depleted_block = 'test:bare', regrow = { into = 'test:ripe', after_secs = 90.0 },",
        );
        let blocks = ctx.take_blocks();
        assert_eq!(
            blocks[0].depleted_block.as_ref().map(|b| b.as_str()),
            Some("test:bare"),
            "depleted_block must parse to the named block id"
        );
        let regrow = blocks[0].regrow.as_ref().expect("regrow table parses");
        assert_eq!(regrow.into.as_str(), "test:ripe");
        assert_eq!(regrow.after_secs, 90.0);

        // Omitted ⇒ both None (the non-forage default for every block).
        let (lua, ctx) = engine_fixture();
        register_test_block(&lua, "");
        let blocks = ctx.take_blocks();
        assert!(blocks[0].depleted_block.is_none());
        assert!(blocks[0].regrow.is_none());
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
        assert_eq!(
            def.pile_mesh(3),
            "mods://test/small.gltf",
            "2-3 = small tier"
        );
        assert_eq!(
            def.pile_mesh(9),
            "mods://test/large.gltf",
            "4+ = large tier"
        );

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

    /// The A1 `hold_transform` knob survives Lua→serde and `hold_pose`
    /// converts degrees→radians. An item without it poses at identity.
    #[test]
    fn item_hold_transform_deserializes() {
        let (lua, ctx) = engine_fixture();
        lua.load(
            "engine.items.register {
                 id = 'test:log',
                 display_name = 'Log',
                 mesh = 'mods://test/log.gltf',
                 hold_transform = { scale = 0.35, rot_deg = { 90, 0, 0 } },
             }
             engine.items.register {
                 id = 'test:plain',
                 display_name = 'Plain',
                 mesh = 'mods://test/plain.gltf',
             }",
        )
        .exec()
        .expect("register hold items");
        let items = ctx.take_items();

        let (pos, rot, scale) = items[0].hold_pose();
        assert_eq!(pos, [0.0, 0.0, 0.0], "pos defaults to origin");
        assert!((scale - 0.35).abs() < 1e-6, "scale carries through");
        assert!(
            (rot[0] - core::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "90 deg → π/2 rad"
        );

        let (pos, rot, scale) = items[1].hold_pose();
        assert_eq!(
            (pos, rot, scale),
            ([0.0; 3], [0.0; 3], 1.0),
            "no knob ⇒ identity"
        );
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
