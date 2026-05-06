//! Lua scripting host for block-junk.
//!
//! Each mod gets its own [`mlua::Lua`] state per side — no shared globals
//! between mods, and no shared runtime state between client and server (each
//! side spins up its own [`ModRegistry`]). When a hook callback errors, the
//! mod is **disabled for the rest of the session** and a loud `error!` is
//! logged. The engine never silently continues with corrupt mod state.

use std::fs;
use std::path::{Path, PathBuf};

use block_junk_mod_api::{API_VERSION, ApiVersion, ModManifest, Side, server::BlockPlacedEvent};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
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
    #[error("mod {name} declares no scripts; need at least one of shared/client/server")]
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
    pub fn load_dir(side: Side, mods_dir: &Path) -> Result<Self, LoadError> {
        let mut mods = Vec::new();
        if !mods_dir.exists() {
            info!("no mods directory at {}", mods_dir.display());
            return Ok(Self { side, mods });
        }

        let entries = fs::read_dir(mods_dir).map_err(|e| LoadError::Io {
            path: mods_dir.to_owned(),
            source: e,
        })?;

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
            mods.push(load_mod(side, &dir)?);
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
            if let Err(e) = call_block_placed(&m.lua, event) {
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

fn load_mod(side: Side, dir: &Path) -> Result<LoadedMod, LoadError> {
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
    if manifest.scripts.is_empty() {
        return Err(LoadError::NoScripts {
            name: manifest.name.clone(),
        });
    }

    let lua = Lua::new();
    install_engine_table(&lua, side).map_err(|source| LoadError::Lua {
        name: manifest.name.clone(),
        side: side.as_str(),
        source,
    })?;

    // Load shared first (so side scripts can use what shared defined),
    // then the side-specific script.
    if let Some(rel) = manifest.scripts.shared.as_deref() {
        run_script(&lua, &manifest.name, side, dir, rel)?;
    }
    if let Some(rel) = manifest.scripts.for_side(side) {
        run_script(&lua, &manifest.name, side, dir, rel)?;
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
fn install_engine_table(lua: &Lua, side: Side) -> Result<(), mlua::Error> {
    let engine = lua.create_table()?;
    engine.set("side", side.as_str())?;

    if side == Side::Server {
        let register = lua.create_function(|lua, callback: Function| {
            let engine: Table = lua.globals().get("engine")?;
            engine.set(BLOCK_PLACED_SLOT, callback)?;
            Ok(())
        })?;
        engine.set("on_block_placed", register)?;
    }

    lua.globals().set("engine", engine)?;
    Ok(())
}

fn call_block_placed(lua: &Lua, event: BlockPlacedEvent) -> Result<(), mlua::Error> {
    let engine: Table = lua.globals().get("engine")?;
    let handler: Value = engine.get(BLOCK_PLACED_SLOT)?;
    let Value::Function(handler) = handler else {
        return Ok(());
    };
    let event_value = lua.to_value(&event)?;
    handler.call::<()>(event_value)
}
