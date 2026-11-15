//! Typed definition composition. Lists replace; object fields merge in explicit patches.
use crate::LoadContext;
use block_junk_mod_api::ModManifest;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{Arc, Mutex, atomic::Ordering};

#[derive(Clone, Debug)]
pub struct Source {
    pub mod_id: String,
    pub stage: String,
}

impl Source {
    pub(crate) fn capture(lua: &Lua, mod_id: &str, fallback: &str) -> Self {
        let stage = (0..16)
            .find_map(|level| {
                lua.inspect_stack(level, |debug| {
                    let source = debug.source();
                    if source.what == "C" {
                        return None;
                    }
                    source
                        .source
                        .map(|source| format!("{}:{}", source, debug.current_line().unwrap_or(0)))
                })
                .flatten()
            })
            .unwrap_or_else(|| fallback.into());
        Self {
            mod_id: mod_id.into(),
            stage,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FieldSource {
    pub source: Source,
    pub defaulted: bool,
    pub derived_from: Vec<String>,
}

pub(crate) fn validate_input(value: &Value) -> mlua::Result<()> {
    fn visit(
        value: &Value,
        depth: usize,
        remaining: &mut usize,
        active: &mut std::collections::BTreeSet<usize>,
        path: &str,
    ) -> mlua::Result<()> {
        if depth == 0 || *remaining == 0 {
            return Err(mlua::Error::external(
                "definition nesting or element budget exceeded",
            ));
        }
        *remaining -= 1;
        match value {
            Value::Number(number) if !number.is_finite() => {
                return Err(mlua::Error::external(format!(
                    "{path}: definition numbers must be finite"
                )));
            }
            Value::Table(table) => {
                if table.metatable().is_some() {
                    return Err(mlua::Error::external("data tables cannot have metatables"));
                }
                let pointer = table.to_pointer() as usize;
                if !active.insert(pointer) {
                    return Err(mlua::Error::external("cyclic definition table"));
                }
                for entry in table.clone().pairs::<Value, Value>() {
                    let (key, value) = entry?;
                    visit(&key, depth - 1, remaining, active, path)?;
                    let label = match &key {
                        Value::String(key) => {
                            key.to_string_lossy().chars().take(128).collect::<String>()
                        }
                        Value::Integer(key) => key.to_string(),
                        _ => "?".into(),
                    };
                    visit(
                        &value,
                        depth - 1,
                        remaining,
                        active,
                        &format!("{path}/{label}"),
                    )?;
                }
                active.remove(&pointer);
            }
            _ => {}
        }
        Ok(())
    }
    visit(value, 32, &mut 65536, &mut Default::default(), "$")
}

// Lua numbers are doubles; a finite input may overflow a definition's f32 field.
// Validate the actual decoded type before JSON freezing can turn infinity into null.
fn validate_decoded<T: Serialize>(lua: &Lua, value: &T, key: &str) -> mlua::Result<()> {
    let value = lua.to_value_with(
        value,
        mlua::SerializeOptions::default().set_array_metatable(false),
    )?;
    validate_input(&value).map_err(|error| mlua::Error::external(format!("{key}: {error}")))
}

fn record_fields(
    output: &mut std::collections::BTreeMap<String, Vec<FieldSource>>,
    path: &str,
    full: &serde_json::Value,
    supplied: Option<&serde_json::Value>,
    source: &Source,
    replace: bool,
) {
    if supplied.is_none() && !replace {
        return;
    }
    match full {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                record_fields(
                    output,
                    &format!("{path}/{key}"),
                    value,
                    supplied.and_then(|input| input.get(key)),
                    source,
                    replace,
                );
            }
        }
        _ => output.entry(path.into()).or_default().push(FieldSource {
            source: source.clone(),
            defaulted: supplied.is_none(),
            derived_from: Vec::new(),
        }),
    }
}

pub(crate) fn install(lua: &Lua, ctx: &LoadContext, manifest: &ModManifest) -> mlua::Result<()> {
    register(lua, ctx, manifest, "movement", &ctx.pending_movement)?;
    register(lua, ctx, manifest, "settlements", &ctx.pending_settlements)?;
    register(
        lua,
        ctx,
        manifest,
        "capabilities",
        &ctx.pending_capabilities,
    )?;
    register(lua, ctx, manifest, "worlds", &ctx.pending_worlds)?;
    register(
        lua,
        ctx,
        manifest,
        "world_upgrades",
        &ctx.pending_world_upgrades,
    )?;
    register(lua, ctx, manifest, "blocks", &ctx.pending_blocks)?;
    register(lua, ctx, manifest, "items", &ctx.pending_items)?;
    register(lua, ctx, manifest, "rooms", &ctx.pending_rooms)?;
    register(lua, ctx, manifest, "recipes", &ctx.pending_recipes)?;
    register(lua, ctx, manifest, "needs", &ctx.pending_needs)?;
    register(lua, ctx, manifest, "npcs", &ctx.pending_npc_kinds)?;
    register(lua, ctx, manifest, "animations", &ctx.pending_animations)?;
    setting(
        lua,
        ctx,
        manifest,
        "npcs",
        "work_defaults",
        &ctx.pending_work_defaults,
    )?;
    setting(
        lua,
        ctx,
        manifest,
        "civilization",
        "params",
        &ctx.pending_civilization_params,
    )?;
    setting(
        lua,
        ctx,
        manifest,
        "blocks",
        "material_drop_multiplier",
        &ctx.pending_material_drop_multiplier,
    )?;
    let engine: Table = lua.globals().get("engine")?;
    let planners: Value = engine.get(crate::NPC_PLANNERS_TABLE)?;
    if matches!(planners, Value::Table(_)) {
        let table: Table = engine.get("npcs")?;
        for replace in [false, true] {
            let provenance = ctx.provenance.clone();
            let owners = ctx.callback_owners.clone();
            let phase = ctx.phase.clone();
            let manifest = manifest.clone();
            table.set(
                if replace {
                    "replace_planner"
                } else {
                    "set_planner"
                },
                lua.create_function(move |lua, (kind, callback): (String, mlua::Function)| {
                    if phase.load(Ordering::Relaxed) != 2 {
                        return Err(mlua::Error::external(
                            "planners register only in events.lua",
                        ));
                    }
                    let key = format!("planner/{kind}");
                    let mut sources = provenance.lock().unwrap();
                    let history = sources.entry(key.clone()).or_default();
                    authorize(history, &manifest, &key, replace)?;
                    let engine: Table = lua.globals().get("engine")?;
                    let planners: Table = engine.get(crate::NPC_PLANNERS_TABLE)?;
                    planners.set(kind, callback)?;
                    owners.lock().unwrap().insert(key, manifest.id.clone());
                    history.push(Source::capture(lua, &manifest.id, "events.lua"));
                    Ok(())
                })?,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn authorize(
    history: &[Source],
    manifest: &ModManifest,
    key: &str,
    replace: bool,
) -> mlua::Result<()> {
    match history.last() {
        Some(previous)
            if !replace
                || !(manifest.id == previous.mod_id
                    || manifest.dependencies.contains_key(&previous.mod_id)
                    || manifest
                        .optional_dependencies
                        .contains_key(&previous.mod_id)
                    || manifest.overrides.contains(&previous.mod_id)) =>
        {
            Err(mlua::Error::external(format!(
                "{key}: conflicting writes by {} ({}) and {}; replacement requires an explicit dependency/override",
                previous.mod_id, previous.stage, manifest.id
            )))
        }
        None if replace => Err(mlua::Error::external(format!(
            "{}: replacement target {key} does not exist",
            manifest.id
        ))),
        _ => Ok(()),
    }
}

fn setting<T: Serialize + DeserializeOwned + Send + 'static>(
    lua: &Lua,
    ctx: &LoadContext,
    manifest: &ModManifest,
    category: &str,
    name: &str,
    slot: &Arc<Mutex<Option<T>>>,
) -> mlua::Result<()> {
    let engine: Table = lua.globals().get("engine")?;
    let table: Table = engine.get(category)?;
    for replace in [false, true] {
        let key = format!("{category}/{name}");
        let sources = ctx.provenance.clone();
        let phase = ctx.phase.clone();
        let manifest = manifest.clone();
        let slot = slot.clone();
        let frozen = ctx.frozen_definitions.clone();
        let fields = ctx.field_provenance.clone();
        table.set(
            format!("{}_{name}", if replace { "replace" } else { "set" }),
            lua.create_function(move |lua, value: Value| {
                if phase.load(Ordering::Relaxed) != u8::from(replace) {
                    return Err(mlua::Error::external(
                        "settings register in data.lua and replace in patches.lua",
                    ));
                }
                validate_input(&value)?;
                let def: T = lua.from_value(value.clone())?;
                validate_decoded(lua, &def, &key)?;
                let supplied: serde_json::Value = lua.from_value(value.clone())?;
                let full = serde_json::to_value(&def).map_err(mlua::Error::external)?;
                if key == "blocks/material_drop_multiplier" {
                    let multiplier: f32 = lua.from_value(value)?;
                    if !multiplier.is_finite() || multiplier < 0.0 {
                        return Err(mlua::Error::external(
                            "material drop multiplier must be finite and nonnegative",
                        ));
                    }
                }
                let mut sources = sources.lock().unwrap();
                let history = sources.entry(key.clone()).or_default();
                authorize(history, &manifest, &key, replace)?;
                *slot.lock().unwrap() = Some(def);
                let source = Source::capture(
                    lua,
                    &manifest.id,
                    if replace { "patches.lua" } else { "data.lua" },
                );
                record_fields(
                    &mut fields.lock().unwrap(),
                    &key,
                    &full,
                    Some(&supplied),
                    &source,
                    true,
                );
                frozen.lock().unwrap().insert(key.clone(), full);
                history.push(source);
                Ok(())
            })?,
        )?;
    }
    Ok(())
}

fn id<T: Serialize>(def: &T) -> mlua::Result<String> {
    let json = serde_json::to_value(def).map_err(mlua::Error::external)?;
    json.get("id")
        .and_then(|id| id.as_str())
        .map(str::to_owned)
        .ok_or_else(|| mlua::Error::external("definition requires a string id"))
}

fn register<T: Serialize + DeserializeOwned + Send + 'static>(
    lua: &Lua,
    ctx: &LoadContext,
    manifest: &ModManifest,
    category: &'static str,
    definitions: &Arc<Mutex<Vec<T>>>,
) -> mlua::Result<()> {
    let engine: Table = lua.globals().get("engine")?;
    let table: Table = engine.get(category)?;
    let frozen = ctx.frozen_definitions.clone();
    table.set(
        "get",
        lua.create_function(move |lua, id: String| {
            let definitions = frozen.lock().unwrap();
            match definitions.get(&format!("{category}/{id}")) {
                Some(def) => lua.to_value_with(def, crate::lua_event_options()),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    for operation in ["register", "replace", "patch", "patch_list"] {
        let definitions = definitions.clone();
        let provenance = ctx.provenance.clone();
        let fields = ctx.field_provenance.clone();
        let frozen = ctx.frozen_definitions.clone();
        let phase = ctx.phase.clone();
        let manifest = manifest.clone();
        table.set(operation, lua.create_function(move |lua, value: Value| {
            validate_input(&value)?;
            let expected = if operation == "register" { 0 } else { 1 };
            if phase.load(Ordering::Relaxed) != expected {
                return Err(mlua::Error::external(format!("{category}.{operation} is only available in {}", if expected == 0 { "data.lua" } else { "patches.lua" })));
            }
            let supplied: serde_json::Value = lua.from_value(value.clone())?;
            let key = supplied.get("id").and_then(|v| v.as_str()).ok_or_else(|| mlua::Error::external("definition requires id"))?;
            let key = format!("{category}/{key}");
            let mut sources = provenance.lock().unwrap();
            let history = sources.entry(key.clone()).or_default();
            if let Some(previous) = history.last() {
                if operation == "register" || !(manifest.id == previous.mod_id || manifest.dependencies.contains_key(&previous.mod_id) || manifest.optional_dependencies.contains_key(&previous.mod_id) || manifest.overrides.contains(&previous.mod_id)) {
                    return Err(mlua::Error::external(format!("{key}: conflicting writes by {} ({}) and {} ({operation}); replacement requires an explicit dependency/override", previous.mod_id, previous.stage, manifest.id)));
                }
            } else if operation != "register" {
                return Err(mlua::Error::external(format!("{}: {operation} target {key} does not exist", manifest.id)));
            }
            let mut defs = definitions.lock().unwrap();
            let target = supplied["id"].as_str().unwrap();
            let position = defs.iter().position(|d| id(d).is_ok_and(|s| s == target));
            let def: T = if operation == "patch" || operation == "patch_list" {
                let mut json = serde_json::to_value(&defs[position.expect("provenance owns definition")]).map_err(mlua::Error::external)?;
                if operation == "patch_list" {
                    let patch: crate::list_patch::ListPatch = lua.from_value(value)?;
                    crate::list_patch::apply(&mut json, &patch).map_err(mlua::Error::external)?;
                } else {
                    merge(&mut json, supplied.clone());
                }
                serde_json::from_value(json).map_err(mlua::Error::external)?
            } else { lua.from_value(value)? };
            validate_decoded(lua,&def,&key)?;
            let full = serde_json::to_value(&def).map_err(mlua::Error::external)?;
            let source = Source::capture(lua,&manifest.id,if expected == 0 {"data.lua"} else {"patches.lua"});
            if let Some(position) = position { defs[position] = def; } else { defs.push(def); }
            if operation == "patch_list" {
                fields.lock().unwrap().entry(format!("{key}{}", supplied["field"].as_str().unwrap_or(""))).or_default().push(FieldSource { source:source.clone(),defaulted:false,derived_from:Vec::new() });
            } else { record_fields(&mut fields.lock().unwrap(), &key, &full, Some(&supplied), &source, operation != "patch"); }
            history.push(source);
            frozen.lock().unwrap().insert(key, full);
            Ok(())
        })?)?;
    }
    Ok(())
}

fn merge(target: &mut serde_json::Value, patch: serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(old) => merge(old, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target @ serde_json::Value::Array(_), serde_json::Value::Object(value))
            if value.is_empty() =>
        {
            *target = serde_json::Value::Array(Vec::new())
        }
        (target, value) => *target = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_replace_instead_of_merging_by_position() {
        let mut value = serde_json::json!({"inputs":[1,2,3],"nested":{"a":1,"b":2}});
        merge(
            &mut value,
            serde_json::json!({"inputs":[9],"nested":{"b":3}}),
        );
        assert_eq!(
            value,
            serde_json::json!({"inputs":[9],"nested":{"a":1,"b":3}})
        );
    }
}
