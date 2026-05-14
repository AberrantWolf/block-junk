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
    blocks::BlockDef,
    rooms::{RoomEvent, RoomPattern},
    server::BlockPlacedEvent,
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
    pub pending_rooms: Arc<Mutex<Vec<RoomPattern>>>,
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

    /// Drain the accumulated room patterns.
    pub fn take_rooms(&self) -> Vec<RoomPattern> {
        std::mem::take(&mut *self.pending_rooms.lock().unwrap())
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

    let target = ApiVersion::parse(&manifest.api_version).map_err(|reason| {
        LoadError::ApiVersion {
            name: manifest.name.clone(),
            target: manifest.api_version.clone(),
            reason: reason.to_owned(),
        }
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
    engine.set("blocks", blocks_table)?;

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

fn call_room_event(lua: &Lua, event: &RoomEvent) -> Result<(), mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let handler: Value = engine.get(ROOM_EVENT_SLOT)?;
    let Value::Function(handler) = handler else {
        return Ok(());
    };
    let event_value = lua.to_value_with(event, lua_event_options())?;
    handler.call::<()>(event_value)
}
