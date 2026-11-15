//! Namespaced durable state. Migrations operate on staged copies only.
use crate::{LoadContext, ModRegistry};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
};

pub const MAX_STATE_BYTES: usize = 64 * 1024;
pub const MAX_STATE_RECORDS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateScope {
    World,
    Npc(u64),
    Placement(u64),
    Space(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedModState {
    pub mod_id: String,
    pub scope: StateScope,
    pub schema: u32,
    pub revision: u64,
    /// JSON text keeps the save's binary codec independent of dynamic value types.
    pub json: String,
}
#[derive(Clone, Default)]
pub struct StateStore(pub Arc<Mutex<BTreeMap<(String, StateScope), SavedModState>>>);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StateSchema {
    version: u32,
    default: serde_json::Value,
}

pub(crate) fn install(lua: &Lua, ctx: &LoadContext, owner: &str) -> mlua::Result<()> {
    let engine: Table = lua.globals().get("engine")?;
    let table = lua.create_table()?;
    let migrations = lua.create_table()?;
    table.set("_migrations", migrations)?;
    let schemas = ctx.state_schemas.clone();
    let phase = ctx.phase.clone();
    let id = owner.to_string();
    table.set(
        "register",
        lua.create_function(move |lua, (version, default): (u32, Value)| {
            if phase.load(Ordering::Relaxed) != 0 || version == 0 {
                return Err(mlua::Error::external(
                    "state schema registers once in data.lua with a positive version",
                ));
            }
            crate::definitions::validate_input(&default)?;
            let default: serde_json::Value = lua.from_value(default)?;
            validate_value(&default).map_err(mlua::Error::external)?;
            let mut schemas = schemas.lock().unwrap();
            if schemas.contains_key(&id) {
                return Err(mlua::Error::external("duplicate state schema"));
            }
            schemas.insert(id.clone(), StateSchema { version, default });
            Ok(())
        })?,
    )?;
    let phase = ctx.phase.clone();
    table.set(
        "migrate",
        lua.create_function(move |lua, (from, callback): (u32, Function)| {
            if phase.load(Ordering::Relaxed) != 0 || from == 0 {
                return Err(mlua::Error::external(
                    "state migrations register in data.lua",
                ));
            }
            let engine: Table = lua.globals().get("engine")?;
            let state: Table = engine.get("state")?;
            let migrations: Table = state.get("_migrations")?;
            if migrations.contains_key(from)? {
                return Err(mlua::Error::external("duplicate migration source version"));
            }
            migrations.set(from, callback)
        })?,
    )?;
    let id = owner.to_string();
    let store = ctx.mod_state.clone();
    let schemas = ctx.state_schemas.clone();
    table.set(
        "get",
        lua.create_function(move |lua, scope: Value| {
            let scope: StateScope = lua.from_value(scope)?;
            let key = (id.clone(), scope);
            let json = if let Some(record) = store.0.lock().unwrap().get(&key) {
                serde_json::from_str(&record.json).map_err(mlua::Error::external)?
            } else {
                schemas
                    .lock()
                    .unwrap()
                    .get(&id)
                    .ok_or_else(|| mlua::Error::external("mod has no state schema"))?
                    .default
                    .clone()
            };
            lua.to_value(&json)
        })?,
    )?;
    let store = ctx.mod_state.clone();
    let id = owner.to_string();
    table.set(
        "revision",
        lua.create_function(move |lua, scope: Value| {
            let scope: StateScope = lua.from_value(scope)?;
            Ok(store
                .0
                .lock()
                .unwrap()
                .get(&(id.clone(), scope))
                .map_or(0, |r| r.revision))
        })?,
    )?;
    // Writes join the engine's authoritative command transaction; there is no direct setter.
    engine.set("state", table)?;
    Ok(())
}

pub(crate) fn validate_value(value: &serde_json::Value) -> Result<(), String> {
    if serde_json::to_vec(value).map_err(|e| e.to_string())?.len() > MAX_STATE_BYTES {
        return Err("mod state record exceeds 64 KiB".into());
    }
    fn depth(value: &serde_json::Value, remaining: usize) -> bool {
        if remaining == 0 {
            return false;
        }
        match value {
            serde_json::Value::Object(map) => map.values().all(|v| depth(v, remaining - 1)),
            serde_json::Value::Array(values) => values.iter().all(|v| depth(v, remaining - 1)),
            _ => true,
        }
    }
    if !depth(value, 32) {
        return Err("mod state nesting exceeds 32 levels".into());
    }
    Ok(())
}

impl ModRegistry {
    /// Preflight and migrate every record, then publish the new store atomically.
    pub fn restore_state(&mut self, records: &[SavedModState]) -> Result<(), String> {
        if self.runtime_started {
            return Err("restore mod state before runtime hooks".into());
        }
        if records.len() > MAX_STATE_RECORDS {
            return Err("too many mod state records".into());
        }
        let schemas = self.state_schemas.lock().unwrap().clone();
        let mut staged = BTreeMap::new();
        self.phase.store(4, Ordering::Relaxed);
        let result = (|| {
            for record in records {
                if record.json.len() > MAX_STATE_BYTES || record.schema == 0 || record.revision == 0
                {
                    return Err("invalid saved mod state size or schema".into());
                }
                let schema = schemas.get(&record.mod_id).ok_or_else(|| {
                    format!(
                        "saved state owner {} or its schema is missing",
                        record.mod_id
                    )
                })?;
                let owner = self
                    .mods
                    .iter()
                    .find(|m| m.manifest.id == record.mod_id)
                    .ok_or_else(|| format!("missing state owner {}", record.mod_id))?;
                if record.schema > schema.version {
                    return Err(format!(
                        "mod {} state schema {} is newer than supported {}",
                        record.mod_id, record.schema, schema.version
                    ));
                }
                let mut value: serde_json::Value =
                    serde_json::from_str(&record.json).map_err(|e| e.to_string())?;
                validate_value(&value)?;
                let engine: Table = owner
                    .lua
                    .globals()
                    .get("engine")
                    .map_err(|e| e.to_string())?;
                let state: Table = engine.get("state").map_err(|e| e.to_string())?;
                let migrations: Table = state.get("_migrations").map_err(|e| e.to_string())?;
                for version in record.schema..schema.version {
                    let function: Function = migrations.get(version).map_err(|_| {
                        format!(
                            "mod {} missing state migration {version} -> {}",
                            record.mod_id,
                            version + 1
                        )
                    })?;
                    let input = owner.lua.to_value(&value).map_err(|e| e.to_string())?;
                    let result: Value = crate::sandbox::call(&owner.lua, function, input)
                        .map_err(|e| format!("mod {} migration {version}: {e}", record.mod_id))?;
                    crate::definitions::validate_input(&result).map_err(|e| e.to_string())?;
                    value = owner.lua.from_value(result).map_err(|e| e.to_string())?;
                    validate_value(&value)?;
                }
                let key = (record.mod_id.clone(), record.scope.clone());
                if staged
                    .insert(
                        key,
                        SavedModState {
                            mod_id: record.mod_id.clone(),
                            scope: record.scope.clone(),
                            schema: schema.version,
                            revision: record.revision,
                            json: serde_json::to_string(&value).map_err(|e| e.to_string())?,
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate mod state scope".into());
                }
            }
            Ok(())
        })();
        self.phase.store(3, Ordering::Relaxed);
        result?;
        *self.mod_state.0.lock().unwrap() = staged;
        Ok(())
    }

    pub fn saved_state(&self) -> Vec<SavedModState> {
        self.mod_state.0.lock().unwrap().values().cloned().collect()
    }
}

/// A state transaction owns only its proposed records. Its commit is available
/// exclusively through the registry's lock-held transaction closure.
pub struct StateTransaction {
    records: BTreeMap<(String, StateScope), SavedModState>,
    schema: StateSchema,
    owner: String,
}
impl StateTransaction {
    pub fn remove(&mut self, scope: StateScope, expected_revision: u64) -> Result<(), String> {
        let key = (self.owner.clone(), scope);
        if self
            .records
            .get(&key)
            .is_none_or(|record| record.revision != expected_revision)
        {
            return Err("missing or stale mod state revision".into());
        }
        self.records.remove(&key);
        Ok(())
    }
    pub fn set(
        &mut self,
        scope: StateScope,
        expected_revision: u64,
        json: &str,
    ) -> Result<(), String> {
        if self.schema.version == 0 {
            return Err("mod has no state schema".into());
        }
        if json.len() > MAX_STATE_BYTES {
            return Err("state size limit exceeded".into());
        }
        let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        validate_value(&value)?;
        let key = (self.owner.clone(), scope.clone());
        let revision = self.records.get(&key).map_or(0, |r| r.revision);
        if revision != expected_revision {
            return Err("stale mod state revision".into());
        }
        let revision = revision.checked_add(1).ok_or("state revision exhausted")?;
        if !self.records.contains_key(&key) && self.records.len() >= MAX_STATE_RECORDS {
            return Err("state record limit exceeded".into());
        }
        self.records.insert(
            key,
            SavedModState {
                mod_id: self.owner.clone(),
                scope,
                schema: self.schema.version,
                revision,
                json: serde_json::to_string(&value).map_err(|e| e.to_string())?,
            },
        );
        Ok(())
    }
}
impl ModRegistry {
    /// The closure stages all fallible work before committing engine mutations.
    /// On success state publication cannot fail. The lock excludes other writes.
    pub fn state_transaction<R>(
        &self,
        owner: &str,
        action: impl FnOnce(&mut StateTransaction) -> Result<R, String>,
    ) -> Result<R, String> {
        let schema = self
            .state_schemas
            .lock()
            .unwrap()
            .get(owner)
            .cloned()
            .unwrap_or(StateSchema {
                version: 0,
                default: serde_json::Value::Null,
            });
        let mut store = self.mod_state.0.lock().unwrap();
        let mut transaction = StateTransaction {
            records: store.clone(),
            schema,
            owner: owner.into(),
        };
        let result = action(&mut transaction)?;
        *store = transaction.records;
        Ok(result)
    }
}
