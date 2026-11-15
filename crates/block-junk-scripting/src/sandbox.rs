//! Host-owned coroutine execution: exhaustion yields to Rust, so pcall cannot swallow it.
use block_junk_mod_api::ModManifest;
use mlua::{
    ChunkMode, FromLuaMulti, Function, HookTriggers, IntoLuaMulti, Lua, LuaOptions, StdLib,
    ThreadStatus, VmState,
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug)]
pub struct ScriptLimits {
    pub memory_bytes: usize,
    pub instructions_per_call: u64,
}
impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 32 * 1024 * 1024,
            instructions_per_call: 1_000_000,
        }
    }
}

pub(crate) fn create(
    dir: &Path,
    manifest: &ModManifest,
    limits: ScriptLimits,
) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(limits.memory_bytes)?;
    lua.set_app_data(limits);
    for name in ["dofile", "loadfile", "load", "collectgarbage"] {
        lua.globals().set(name, mlua::Value::Nil)?;
    }
    let root = dir.canonicalize().map_err(mlua::Error::external)?;
    let modules = lua.create_table()?;
    for resource in &manifest.resources {
        let relative = Path::new(resource);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(mlua::Error::external(format!(
                "invalid declared resource {resource:?}"
            )));
        }
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(mlua::Error::external)?;
        if !path.starts_with(&root) {
            return Err(mlua::Error::external(format!(
                "resource {resource:?} escapes mod directory"
            )));
        }
        if let Some(module) = resource.strip_suffix(".lua") {
            let source = std::fs::read_to_string(&path).map_err(mlua::Error::external)?;
            let function = lua
                .load(source)
                .set_mode(ChunkMode::Text)
                .set_name(format!("{}/{resource}", manifest.id))
                .into_function()?;
            modules.set(module.replace('/', "."), function)?;
        }
    }
    // Lua implementation keeps imports inside the host-owned coroutine, including its budget.
    let factory: Function = lua.load("return function(modules) local loaded, loading = {}, {}; return function(name) if loaded[name] ~= nil then return loaded[name] end; local f = modules[name]; if not f then error('undeclared module: '..tostring(name)) end; if loading[name] then error('cyclic import: '..name) end; loading[name]=true; local result=f(); loading[name]=nil; if result==nil then result=true end; loaded[name]=result; return result end end").eval()?;
    let require: Function = factory.call(modules)?;
    lua.globals().set("require", require)?;
    Ok(lua)
}

pub(crate) fn call<R: FromLuaMulti>(
    lua: &Lua,
    function: Function,
    args: impl IntoLuaMulti,
) -> mlua::Result<R> {
    let limit = lua
        .app_data_ref::<ScriptLimits>()
        .map(|l| l.instructions_per_call)
        .unwrap_or(ScriptLimits::default().instructions_per_call);
    let remaining = Arc::new(AtomicU64::new(limit.div_ceil(1000).max(1)));
    let thread = lua.create_thread(function)?;
    thread.set_hook(
        HookTriggers::new().every_nth_instruction(1000),
        move |_, _| {
            if remaining.fetch_sub(1, Ordering::Relaxed) <= 1 {
                Ok(VmState::Yield)
            } else {
                Ok(VmState::Continue)
            }
        },
    )?;
    let queue = lua
        .app_data_ref::<Arc<std::sync::Mutex<Vec<block_junk_mod_api::ui::UiToast>>>>()
        .map(|queue| queue.clone());
    let checkpoint = queue.as_ref().map(|queue| queue.lock().unwrap().len());
    let invocation = lua
        .app_data_ref::<crate::commands::Invocation>()
        .map(|i| i.clone());
    let command_checkpoint = invocation
        .as_ref()
        .map(|i| i.pending.lock().unwrap().len())
        .unwrap_or(0);
    let results_checkpoint = invocation
        .as_ref()
        .map(|invocation| *invocation.results_read.lock().unwrap())
        .unwrap_or(0);
    let result = (|| {
        let result = thread.resume::<mlua::MultiValue>(args)?;
        if thread.status() != ThreadStatus::Finished {
            return Err(mlua::Error::external(format!(
                "script exceeded instruction budget ({limit})"
            )));
        }
        let result = R::from_lua_multi(result, lua)?;
        if let Some(invocation) = &invocation {
            invocation.publish(command_checkpoint)?;
        }
        Ok(result)
    })();
    if result.is_err()
        && let Some(invocation) = invocation
    {
        *invocation.results_read.lock().unwrap() = results_checkpoint;
        invocation
            .pending
            .lock()
            .unwrap()
            .truncate(command_checkpoint);
    }
    if result.is_err()
        && let (Some(queue), Some(checkpoint)) = (queue, checkpoint)
    {
        queue.lock().unwrap().truncate(checkpoint);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protected_calls_cannot_catch_budget_exhaustion() {
        let lua = Lua::new();
        lua.set_app_data(ScriptLimits {
            instructions_per_call: 10_000,
            ..Default::default()
        });
        for script in [
            "while true do end",
            "while true do pcall(function() while true do end end) end",
            "local x=0; while true do x=x+1 end",
        ] {
            let function = lua.load(script).into_function().unwrap();
            assert!(
                call::<()>(&lua, function, ())
                    .unwrap_err()
                    .to_string()
                    .contains("instruction budget")
            );
        }
        assert_eq!(
            call::<u32>(&lua, lua.load("return 42").into_function().unwrap(), ()).unwrap(),
            42
        );
    }
}
