//! Stable subscription identity and explicit replacement ownership.
use crate::{
    LoadContext,
    definitions::{Source, authorize},
};
use block_junk_mod_api::ModManifest;
use mlua::{Function, Lua, Table};
use std::sync::atomic::Ordering;

pub(crate) const TABLE: &str = "_subscriptions";

pub(crate) fn install(lua: &Lua, ctx: &LoadContext, manifest: &ModManifest) -> mlua::Result<()> {
    let engine: Table = lua.globals().get("engine")?;
    let callbacks = lua.create_table()?;
    engine.set(TABLE, callbacks)?;
    let events = lua.create_table()?;
    for replace in [false, true] {
        let sources = ctx.provenance.clone();
        let owners = ctx.callback_owners.clone();
        let phase = ctx.phase.clone();
        let manifest = manifest.clone();
        let register = lua.create_function(
            move |lua, (event, id, handler): (String, String, Function)| {
                let stage = phase.load(Ordering::Relaxed);
                if !matches!(stage, 0 | 2) {
                    return Err(mlua::Error::external(
                        "subscriptions are frozen outside data/events loading",
                    ));
                }
                if !matches!(
                    event.as_str(),
                    "block_placed" | "room_event" | "space_event" | "inspect"
                ) || id.is_empty()
                    || id.len() > 128
                {
                    return Err(mlua::Error::external(
                        "invalid subscription event or stable id",
                    ));
                }
                let key = format!("subscription/{event}/{id}");
                let mut sources = sources.lock().unwrap();
                let history = sources.entry(key.clone()).or_default();
                authorize(history, &manifest, &key, replace)?;
                let engine: Table = lua.globals().get("engine")?;
                let callbacks: Table = engine.get(TABLE)?;
                callbacks.set(key.clone(), handler)?;
                owners.lock().unwrap().insert(key, manifest.id.clone());
                history.push(Source::capture(
                    lua,
                    &manifest.id,
                    if stage == 0 { "data.lua" } else { "events.lua" },
                ));
                Ok(())
            },
        )?;
        events.set(if replace { "replace" } else { "subscribe" }, register)?;
    }
    engine.set("events", events.clone())?;
    // Existing shorthand is one named subscription per mod, not a mutable single slot.
    let subscribe: Function = events.get("subscribe")?;
    for (event, field) in [
        ("block_placed", "on_block_placed"),
        ("room_event", "on_room_event"),
        ("space_event", "on_space_event"),
        ("inspect", "on_inspect"),
    ] {
        let subscribe = subscribe.clone();
        let id = format!("{}:default", manifest.id);
        let wrapper = lua.create_function(move |_, handler: Function| {
            subscribe.call::<()>((event, id.clone(), handler))
        })?;
        if event == "inspect" {
            let ui: Table = engine.get("ui")?;
            ui.set(field, wrapper)?;
        } else if matches!(ctx.phase.load(Ordering::Relaxed), 0 | 2) {
            engine.set(field, wrapper)?;
        }
    }
    Ok(())
}

pub(crate) fn call(lua: &Lua, key: &str, value: mlua::Value) -> mlua::Result<mlua::Value> {
    crate::commands::behavior(lua, key);
    let engine: Table = lua.globals().get("engine")?;
    let callbacks: Table = engine.get(TABLE)?;
    let function: Function = callbacks.get(key)?;
    if key.starts_with("subscription/inspect/") {
        let text: InspectText = crate::sandbox::call(lua, function, value)?;
        return Ok(text.0);
    }
    crate::sandbox::call(lua, function, value)
}

struct InspectText(mlua::Value);
impl mlua::FromLua for InspectText {
    fn from_lua(value: mlua::Value, _: &Lua) -> mlua::Result<Self> {
        match &value {
            mlua::Value::Nil => {}
            mlua::Value::String(s) => {
                s.to_str()?;
            }
            _ => {
                return Err(mlua::Error::external(
                    "inspect callback must return string or nil",
                ));
            }
        }
        Ok(Self(value))
    }
}
