//! Discovery and dependency validation happen before any Lua execution.
use crate::LoadError;
use block_junk_mod_api::ModManifest;
use semver::{Version, VersionReq};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

type Discovered = BTreeMap<String, (PathBuf, ModManifest)>;

pub(crate) fn fingerprint_sources(mods: &[(PathBuf, ModManifest)]) -> Result<u64, LoadError> {
    use std::io::Read;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut bytes = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (dir, manifest) in mods {
        bytes(&(manifest.id.len() as u64).to_le_bytes());
        bytes(manifest.id.as_bytes());
        let root = dir.canonicalize().map_err(|source| LoadError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut resources: BTreeSet<String> = manifest.resources.iter().cloned().collect();
        for script in [
            "manifest.toml",
            "data.lua",
            "patches.lua",
            "events.lua",
            "textures.lua",
        ] {
            if dir.join(script).exists() {
                resources.insert(script.into());
            }
        }
        for resource in resources {
            let path = dir
                .join(&resource)
                .canonicalize()
                .map_err(|source| LoadError::Io {
                    path: dir.join(&resource),
                    source,
                })?;
            if !path.starts_with(&root) {
                return Err(LoadError::Composition(format!(
                    "mod {} resource {resource} escapes its directory",
                    manifest.id
                )));
            }
            let mut file = fs::File::open(&path).map_err(|source| LoadError::Io {
                path: path.clone(),
                source,
            })?;
            let len = file
                .metadata()
                .map_err(|source| LoadError::Io {
                    path: path.clone(),
                    source,
                })?
                .len();
            bytes(&(resource.len() as u64).to_le_bytes());
            bytes(resource.as_bytes());
            bytes(&len.to_le_bytes());
            let mut buffer = [0u8; 8192];
            loop {
                let count = file.read(&mut buffer).map_err(|source| LoadError::Io {
                    path: path.clone(),
                    source,
                })?;
                if count == 0 {
                    break;
                }
                bytes(&buffer[..count]);
            }
        }
    }
    Ok(hash)
}

pub(crate) fn discover(root: &Path) -> Result<Vec<(PathBuf, ModManifest)>, LoadError> {
    let mut found = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|source| LoadError::Io {
        path: root.into(),
        source,
    })? {
        let dir = entry
            .map_err(|source| LoadError::Io {
                path: root.into(),
                source,
            })?
            .path();
        let path = dir.join("manifest.toml");
        if !dir.is_dir() || !path.exists() {
            continue;
        }
        let source = fs::read_to_string(&path).map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        let manifest: ModManifest =
            toml::from_str(&source).map_err(|source| LoadError::Manifest {
                path: path.clone(),
                source,
            })?;
        if manifest.id.is_empty()
            || !manifest
                .id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        {
            return Err(LoadError::Composition(format!(
                "{}: invalid stable mod id {:?}",
                path.display(),
                manifest.id
            )));
        }
        if let Some((previous, _)) = found.get(&manifest.id) {
            return Err(LoadError::Composition(format!(
                "duplicate mod {} in {:?} and {:?}",
                manifest.id, previous, dir
            )));
        }
        found.insert(manifest.id.clone(), (dir, manifest));
    }
    let order = resolve(&found)?;
    Ok(order
        .into_iter()
        .map(|id| found.remove(&id).unwrap())
        .collect())
}

fn resolve(found: &Discovered) -> Result<Vec<String>, LoadError> {
    let fail = |message| LoadError::Composition(message);
    let versions: BTreeMap<_, _> = found
        .iter()
        .map(|(id, (_, m))| {
            Version::parse(&m.version)
                .map(|v| (id.clone(), v))
                .map_err(|e| fail(format!("mod {id}: invalid version {:?}: {e}", m.version)))
        })
        .collect::<Result<_, _>>()?;
    let mut edges = BTreeMap::<String, BTreeSet<String>>::new();
    for (id, (_, m)) in found {
        let mut before = BTreeSet::new();
        for (required, dependencies) in [(true, &m.dependencies), (false, &m.optional_dependencies)]
        {
            for (dependency, constraint) in dependencies {
                let requirement = VersionReq::parse(constraint).map_err(|e| {
                    fail(format!(
                        "mod {id}: invalid constraint for {dependency}: {e}"
                    ))
                })?;
                match versions.get(dependency) {
                    None if required => {
                        return Err(fail(format!(
                            "mod {id} requires missing mod {dependency} ({constraint})"
                        )));
                    }
                    None => {}
                    Some(version) => {
                        if !requirement.matches(version) {
                            return Err(fail(format!(
                                "mod {id} requires {dependency} {constraint}, installed {version}"
                            )));
                        }
                        before.insert(dependency.clone());
                    }
                }
            }
        }
        for (other, constraint) in &m.incompatibilities {
            let requirement = VersionReq::parse(constraint).map_err(|e| {
                fail(format!(
                    "mod {id}: invalid incompatibility with {other}: {e}"
                ))
            })?;
            if versions.get(other).is_some_and(|v| requirement.matches(v)) {
                return Err(fail(format!(
                    "mod {id} is incompatible with {other} {constraint}"
                )));
            }
        }
        for other in &m.overrides {
            if !found.contains_key(other) {
                return Err(fail(format!("mod {id} overrides missing mod {other}")));
            }
            before.insert(other.clone());
        }
        edges.insert(id.clone(), before);
    }
    let mut result = Vec::new();
    while !edges.is_empty() {
        let Some(next) = edges
            .iter()
            .find(|(_, before)| before.is_empty())
            .map(|(id, _)| id.clone())
        else {
            return Err(fail(format!(
                "dependency/override cycle among {}",
                edges.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        };
        edges.remove(&next);
        for before in edges.values_mut() {
            before.remove(&next);
        }
        result.push(next);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "block-junk-mods-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn add(
            &self,
            directory: &str,
            id: &str,
            extra: &str,
            data: &str,
            patches: &str,
            events: &str,
        ) {
            let dir = self.0.join(directory);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("manifest.toml"),
                format!("id='{id}'\nname='{id}'\nversion='1.2.0'\napi_version='0.6.0'\n{extra}"),
            )
            .unwrap();
            for (name, script) in [
                ("data.lua", data),
                ("patches.lua", patches),
                ("events.lua", events),
            ] {
                fs::write(dir.join(name), script).unwrap();
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn real_loader_composes_then_freezes_and_replaces_callbacks() {
        let f = Fixture::new();
        f.add("zzz", "base", "", "engine.items.register {id='base:item',display_name='Original',mesh='test.glb'}\nengine.blocks.set_material_drop_multiplier(1)", "", "engine.npcs.set_planner('base:worker', function() return {kind='idle'} end)");
        f.add("aaa", "addon", "[dependencies]\nbase='^1'", "", "engine.items.patch {id='base:item', display_name='Changed'}\nengine.blocks.replace_material_drop_multiplier(0.5)", "engine.npcs.replace_planner('base:worker', function() return {kind='rest', duration_secs=4} end)");
        let ctx = crate::LoadContext::new();
        let mut registry = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &f.0,
            &ctx,
            crate::ScriptLimits::default(),
        )
        .unwrap();
        assert!(
            !ctx.provenance
                .lock()
                .unwrap()
                .contains_key("planner/base:worker")
        );
        registry.activate_runtime().unwrap();
        registry.activate_runtime().unwrap();
        let snapshot = serde_json::from_value(
            serde_json::json!({"id":1,"kind":"base:worker","foot":{"x":0,"y":0,"z":0},"needs":{}}),
        )
        .unwrap();
        assert!(matches!(
            registry
                .call_planner(&"base:worker".into(), &snapshot)
                .unwrap(),
            Some(block_junk_mod_api::npcs::PlannerGoal::Rest { duration_secs: 4.0 })
        ));
        assert_eq!(
            registry
                .mods
                .iter()
                .map(|m| m.manifest.id.as_str())
                .collect::<Vec<_>>(),
            ["base", "addon"]
        );
        assert_eq!(ctx.pending_items.lock().unwrap()[0].display_name, "Changed");
        assert_eq!(ctx.take_material_drop_multiplier(), Some(0.5));
        assert_eq!(ctx.provenance.lock().unwrap()["items/base:item"].len(), 2);
        assert_eq!(
            ctx.provenance.lock().unwrap()["planner/base:worker"]
                .last()
                .unwrap()
                .mod_id,
            "addon"
        );
        assert!(
            registry.mods[1]
                .lua
                .load("engine.items.patch {id='base:item', display_name='Late'}")
                .exec()
                .is_err()
        );
        assert_eq!(ctx.pending_items.lock().unwrap()[0].display_name, "Changed");
        registry.mods[1].disabled = true;
        let error = registry
            .call_planner(&"base:worker".into(), &snapshot)
            .unwrap_err();
        assert!(error.to_string().contains("disabled"));
    }

    #[test]
    fn unrelated_patch_reports_both_owners() {
        let f = Fixture::new();
        f.add(
            "one",
            "base",
            "",
            "engine.items.register {id='base:item',display_name='Original',mesh='test.glb'}",
            "",
            "",
        );
        f.add(
            "two",
            "addon",
            "",
            "",
            "engine.items.patch {id='base:item', display_name='Changed'}",
            "",
        );
        let ctx = crate::LoadContext::new();
        let error = crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &f.0, &ctx)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("base")
                && error.contains("addon")
                && error.contains("conflicting writes"),
            "{error}"
        );
        assert_eq!(
            ctx.pending_items.lock().unwrap()[0].display_name,
            "Original"
        );
    }

    #[test]
    fn imported_definition_provenance_identifies_actual_script_and_line() {
        let fixture = Fixture::new();
        fixture.add(
            "base",
            "base",
            "resources=['helper.lua']",
            "require('helper')",
            "",
            "",
        );
        fs::write(fixture.0.join("base/helper.lua"),"\nengine.items.register {id='base:imported',display_name='Imported',mesh='fixture.gltf'}").unwrap();
        let ctx = crate::LoadContext::new();
        crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &ctx,
            Default::default(),
        )
        .unwrap();
        let provenance = ctx.provenance.lock().unwrap();
        assert!(
            provenance["items/base:imported"][0]
                .stage
                .contains("base/helper.lua:2"),
            "{:?}",
            provenance["items/base:imported"]
        );
    }

    #[test]
    fn finite_lua_numbers_that_overflow_decoded_fields_reject_before_freeze() {
        for patch in [false, true] {
            let fixture = Fixture::new();
            let valid = "engine.blocks.register {id='base:bench',display_name='Bench',flags={},color={1,1,1},interactable={duration_secs=1}}";
            let invalid = "engine.blocks.register {id='base:bench',display_name='Bench',flags={},color={1,1,1},interactable={duration_secs=1e100}}";
            fixture.add(
                "base",
                "base",
                "",
                if patch { valid } else { invalid },
                if patch {
                    "engine.blocks.patch {id='base:bench',interactable={duration_secs=1e100}}"
                } else {
                    ""
                },
                "",
            );
            let context = crate::LoadContext::new();
            let error = crate::ModRegistry::load_dir(
                block_junk_mod_api::Side::Server,
                &fixture.0,
                &context,
            )
            .err()
            .unwrap()
            .to_string();
            for part in ["base:bench", "interactable/duration_secs", "finite"] {
                assert!(error.contains(part), "{error}");
            }
            if patch {
                assert_eq!(
                    context.pending_blocks.lock().unwrap()[0]
                        .interactable
                        .as_ref()
                        .unwrap()
                        .duration_secs,
                    1.
                );
            } else {
                assert!(context.pending_blocks.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn event_chain_breadth_cannot_reset_budget_by_draining_queue() {
        let fixture = Fixture::new();
        fixture.add(
            "base",
            "base",
            "",
            "",
            "",
            "engine.on_block_placed(function() end)",
        );
        let ctx = crate::LoadContext::new();
        let mut registry =
            crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &fixture.0, &ctx)
                .unwrap();
        let root = registry.enqueue_event("block_placed", &(), None).unwrap();
        registry.advance_events();
        for _ in 1..crate::events::MAX_CHAIN_WORK / 2 {
            registry
                .enqueue_event("block_placed", &(), Some(root))
                .unwrap();
            registry.advance_events();
        }
        assert_eq!(registry.pending_event_callbacks(), 0);
        let error = registry
            .enqueue_event("block_placed", &(), Some(root))
            .unwrap_err();
        assert!(error.contains("work budget"), "{error}");
    }

    #[test]
    fn delayed_jobs_pin_causal_budgets_and_pending_callbacks_round_trip() {
        let fixture = Fixture::new();
        fixture.add("base","base","","engine.state.register(1,{})","","engine.on_block_placed(function() engine.commands.push {kind='set_state',scope='world',expected_revision=0,json='{}'} end)");
        let context = crate::LoadContext::new();
        let mut registry =
            crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &fixture.0, &context)
                .unwrap();
        let root = registry
            .enqueue_event("block_placed", &serde_json::json!({}), None)
            .unwrap();
        registry.pin_cause(root);
        registry.advance_events();
        registry.commands.batches.lock().unwrap().clear();
        for _ in 0..crate::events::MAX_RETAINED_CAUSES + 5 {
            registry.enqueue_event("unused", &(), None).unwrap();
        }
        let child = registry
            .enqueue_event("block_placed", &serde_json::json!({}), Some(root))
            .unwrap();
        assert_eq!(child.root, root.root);
        let saved = registry.saved_events();
        saved.validate().unwrap();
        let context = crate::LoadContext::new();
        let mut restored = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &context,
            Default::default(),
        )
        .unwrap();
        restored.restore_events(&saved).unwrap();
        restored.activate_runtime().unwrap();
        assert!(restored.commands.batches.lock().unwrap().is_empty());
        restored.advance_events();
        assert_eq!(
            restored
                .commands
                .batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .cause,
            Some(child)
        );
        assert_eq!(restored.pending_event_callbacks(), 0);
        for _ in 2..crate::events::MAX_CHAIN_WORK / 2 {
            restored
                .enqueue_event("block_placed", &serde_json::json!({}), Some(root))
                .unwrap();
            restored.advance_events();
            restored.commands.batches.lock().unwrap().clear();
        }
        assert!(
            restored
                .enqueue_event("block_placed", &serde_json::json!({}), Some(root))
                .unwrap_err()
                .contains("work budget")
        );
    }

    #[test]
    fn events_are_delayed_bounded_and_commands_retain_causality() {
        let fixture = Fixture::new();
        fixture.add("base","base","","engine.state.register(1,{})","", "engine.on_block_placed(function() engine.commands.push {kind='set_state',scope='world',expected_revision=0,json='{}'} end)");
        let ctx = crate::LoadContext::new();
        let mut registry =
            crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &fixture.0, &ctx)
                .unwrap();
        let cause = registry
            .enqueue_event("block_placed", &serde_json::json!({}), None)
            .unwrap();
        assert!(registry.commands.batches.lock().unwrap().is_empty());
        registry.advance_events();
        assert_eq!(
            registry
                .commands
                .batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .cause,
            Some(cause)
        );
        let child = registry
            .enqueue_event("block_placed", &serde_json::json!({}), Some(cause))
            .unwrap();
        assert_eq!(child.root, cause.root);
        assert_eq!(child.depth, 1);
        assert!(registry.commands.batches.lock().unwrap().is_empty());
        registry.advance_events();
        assert_eq!(
            registry
                .commands
                .batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .cause,
            Some(child)
        );
        assert!(
            registry
                .enqueue_event(
                    "block_placed",
                    &serde_json::json!({}),
                    Some(crate::events::EventCause {
                        root: cause.root,
                        depth: crate::events::MAX_CHAIN_DEPTH
                    })
                )
                .is_err()
        );
        for _ in 0..20 {
            registry
                .enqueue_event("block_placed", &serde_json::json!({}), None)
                .unwrap();
        }
        registry.advance_events();
        assert_eq!(
            registry.pending_event_callbacks(),
            20 - crate::events::CALLBACKS_PER_TICK
        );
        assert_eq!(
            registry.commands.batches.lock().unwrap().len(),
            crate::events::CALLBACKS_PER_TICK
        );
        while registry
            .enqueue_event("block_placed", &serde_json::json!({}), None)
            .is_ok()
        {}
        let pending = registry.pending_event_callbacks();
        assert_eq!(
            pending + registry.commands.batches.lock().unwrap().len(),
            crate::events::MAX_RETAINED_CAUSES
        );
        assert!(
            registry
                .enqueue_event("block_placed", &serde_json::json!({}), None)
                .is_err()
        );
        assert_eq!(registry.pending_event_callbacks(), pending);
        registry.commands.batches.lock().unwrap().clear();
        while registry
            .enqueue_event("block_placed", &serde_json::json!({}), None)
            .is_ok()
        {}
        assert_eq!(
            registry.pending_event_callbacks(),
            crate::events::MAX_PENDING_CALLBACKS
        );
    }

    #[test]
    fn runaway_callback_discards_queued_effects() {
        let f = Fixture::new();
        f.add("one", "base", "", "", "", "engine.on_block_placed(function() engine.ui.toast({x=0,y=0,z=0}, 'must disappear'); while true do pcall(function() while true do end end) end end)");
        let ctx = crate::LoadContext::new();
        let mut registry = crate::ModRegistry::load_dir_with_limits(
            block_junk_mod_api::Side::Server,
            &f.0,
            &ctx,
            crate::ScriptLimits {
                instructions_per_call: 10_000,
                ..Default::default()
            },
        )
        .unwrap();
        registry
            .dispatch_block_placed(block_junk_mod_api::server::BlockPlacedEvent {
                pos: block_junk_mod_api::shared::BlockPos { x: 0, y: 0, z: 0 },
                block: "base:test".into(),
            })
            .unwrap();
        registry.advance_events();
        assert!(registry.mods[0].disabled);
        assert!(ctx.ui_toasts.lock().unwrap().is_empty());
    }

    #[test]
    fn imports_are_declared_and_memory_is_bounded() {
        let f = Fixture::new();
        f.add("one", "base", "resources=['helper.lua']", "assert(require('helper') == 42); assert(os == nil and io == nil and package == nil and debug == nil and coroutine == nil)", "", "");
        fs::write(f.0.join("one/helper.lua"), "return 42").unwrap();
        let ctx = crate::LoadContext::new();
        let registry =
            crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &f.0, &ctx).unwrap();
        assert!(
            registry.mods[0]
                .lua
                .load("require('not_declared')")
                .exec()
                .unwrap_err()
                .to_string()
                .contains("undeclared")
        );
        let function = registry.mods[0]
            .lua
            .load("return string.rep('x', 64000000)")
            .into_function()
            .unwrap();
        assert!(crate::sandbox::call::<String>(&registry.mods[0].lua, function, ()).is_err());
    }

    #[test]
    fn content_fingerprint_includes_balance_and_agrees_between_sides() {
        let f = Fixture::new();
        f.add(
            "one",
            "base",
            "",
            "engine.blocks.set_material_drop_multiplier(1)",
            "",
            "",
        );
        let server = crate::LoadContext::new();
        let client = crate::LoadContext::new();
        crate::ModRegistry::load_dir(block_junk_mod_api::Side::Server, &f.0, &server).unwrap();
        crate::ModRegistry::load_dir(block_junk_mod_api::Side::Client, &f.0, &client).unwrap();
        assert_eq!(server.fingerprint(), client.fingerprint());
        let before = client.fingerprint();
        client.take_material_drop_multiplier();
        client.take_blocks();
        assert_eq!(before, client.fingerprint());
        client.publish_definition("blocks", "material_drop_multiplier", &2.0);
        assert_ne!(server.fingerprint(), client.fingerprint());
    }

    #[test]
    fn callback_command_batches_rollback_errors_and_invalid_return_values() {
        let fixture = Fixture::new();
        fixture.add(
            "base",
            "base",
            "",
            "engine.state.register(1, {})",
            "",
            r#"
            engine.npcs.set_planner('base:worker', function()
                engine.commands.push {kind='set_state',scope='world',expected_revision=0,json='{}'}
                return {kind='not_a_goal'}
            end)
        "#,
        );
        let ctx = crate::LoadContext::new();
        let mut registry = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &ctx,
            Default::default(),
        )
        .unwrap();
        registry.activate_runtime().unwrap();
        let snapshot = serde_json::from_value(
            serde_json::json!({"id":1,"kind":"base:worker","foot":{"x":0,"y":0,"z":0},"needs":{}}),
        )
        .unwrap();
        assert!(
            registry
                .call_planner(&"base:worker".into(), &snapshot)
                .is_err()
        );
        assert!(registry.commands.batches.lock().unwrap().is_empty());
        let lua = &registry.mods[0].lua;
        for suffix in [
            "error('failed')",
            "while true do end",
            "for i=1,65 do engine.commands.push {kind='cancel_project',project_id=1,revision=1} end",
        ] {
            let function = lua.load(format!("engine.commands.push {{kind='cancel_project',project_id=1,revision=1}}; {suffix}")).into_function().unwrap();
            assert!(crate::sandbox::call::<()>(lua, function, ()).is_err());
            assert!(registry.commands.batches.lock().unwrap().is_empty());
        }
        let function = lua
            .load("engine.commands.push {kind='cancel_project',project_id=1,revision=1}")
            .into_function()
            .unwrap();
        crate::sandbox::call::<()>(lua, function, ()).unwrap();
        assert_eq!(registry.commands.batches.lock().unwrap().len(), 1);
        assert_eq!(
            registry.commands.batches.lock().unwrap()[0].commands.len(),
            1
        );
        assert!(registry.saved_state().is_empty());
    }

    #[test]
    fn state_transaction_rejects_stale_writes_without_partial_mutation() {
        let fixture = Fixture::new();
        fixture.add("base", "base", "", "engine.state.register(1, {})", "", "");
        let ctx = crate::LoadContext::new();
        let registry = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &ctx,
            Default::default(),
        )
        .unwrap();
        use crate::state::StateScope::World;
        assert!(
            registry
                .state_transaction("base", |tx| {
                    tx.set(World, 0, "{\"count\":1}")?;
                    tx.set(World, 0, "{\"count\":2}")
                })
                .is_err()
        );
        assert!(registry.saved_state().is_empty());
        registry
            .state_transaction("base", |tx| tx.set(World, 0, "{\"count\":3}"))
            .unwrap();
        assert_eq!(registry.saved_state()[0].revision, 1);
        assert_eq!(registry.saved_state()[0].json, "{\"count\":3}");
    }

    #[test]
    fn state_migrations_are_atomic_and_precede_runtime_hooks() {
        let fixture = Fixture::new();
        fixture.add("base", "base", "", "engine.state.register(2, {count=0}); engine.state.migrate(1, function(old) return {count=old.count+1} end)", "", "assert(engine.state.get('world').count == 5)");
        let ctx = crate::LoadContext::new();
        let mut registry = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &ctx,
            Default::default(),
        )
        .unwrap();
        let record = crate::state::SavedModState {
            revision: 1,
            mod_id: "base".into(),
            scope: crate::state::StateScope::World,
            schema: 1,
            json: "{\"count\":4}".into(),
        };
        let mut missing = record.clone();
        missing.mod_id = "missing".into();
        assert!(registry.restore_state(&[record.clone(), missing]).is_err());
        assert!(registry.saved_state().is_empty());
        registry.restore_state(&[record]).unwrap();
        assert_eq!(registry.saved_state()[0].schema, 2);
        registry.activate_runtime().unwrap();
        assert!(
            registry.restore_state(&[]).is_err(),
            "cannot replace durable state after hooks begin"
        );
    }

    #[test]
    fn migration_cannot_emit_world_effects() {
        let fixture = Fixture::new();
        fixture.add("base", "base", "", "engine.state.register(2, {}); engine.state.migrate(1, function(old) engine.ui.toast({x=0,y=0,z=0}, 'invalid'); return old end)", "", "");
        let ctx = crate::LoadContext::new();
        let mut registry = crate::ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            &fixture.0,
            &ctx,
            Default::default(),
        )
        .unwrap();
        let record = crate::state::SavedModState {
            revision: 1,
            mod_id: "base".into(),
            scope: crate::state::StateScope::World,
            schema: 1,
            json: "{}".into(),
        };
        assert!(
            registry
                .restore_state(&[record])
                .unwrap_err()
                .contains("forbidden")
        );
        assert!(registry.saved_state().is_empty());
        assert!(ctx.ui_toasts.lock().unwrap().is_empty());
    }

    #[test]
    fn stable_subscription_replacement_disables_without_fallback() {
        let fixture = Fixture::new();
        fixture.add(
            "base",
            "base",
            "",
            "engine.events.subscribe('inspect', 'base:detail', function() return 'base' end)",
            "",
            "",
        );
        fixture.add(
            "addon",
            "addon",
            "[dependencies]\nbase='^1'",
            "engine.events.replace('inspect', 'base:detail', function() return 'addon' end)",
            "",
            "",
        );
        let ctx = crate::LoadContext::new();
        let mut registry =
            crate::ModRegistry::load_dir(block_junk_mod_api::Side::Client, &fixture.0, &ctx)
                .unwrap();
        let target = serde_json::from_value(
            serde_json::json!({"kind":"block","id":"base:block","pos":{"x":0,"y":0,"z":0}}),
        )
        .unwrap();
        assert_eq!(registry.call_inspect(&target), ["addon"]);
        registry
            .mods
            .iter_mut()
            .find(|m| m.manifest.id == "addon")
            .unwrap()
            .disabled = true;
        assert!(registry.call_inspect(&target).is_empty());
    }
    fn fixture(entries: &[(&str, &str)]) -> Discovered {
        entries
            .iter()
            .map(|(id, extra)| {
                let m = toml::from_str(&format!(
                    "id='{id}'\nname='{id}'\nversion='1.2.0'\napi_version='0.6.0'\n{extra}"
                ))
                .unwrap();
                ((*id).into(), (PathBuf::from(format!("arbitrary/{id}")), m))
            })
            .collect()
    }
    #[test]
    fn dependency_order_uses_ids_and_constraints() {
        let f = fixture(&[
            ("z", ""),
            ("a", "[dependencies]\nz='^1.0'"),
            ("b", "[optional_dependencies]\nabsent='*'"),
        ]);
        assert_eq!(resolve(&f).unwrap(), ["b", "z", "a"]);
    }
    #[test]
    fn invalid_graphs_fail_before_execution() {
        for extra in [
            "[dependencies]\nmissing='*'",
            "[dependencies]\nz='^2'",
            "[optional_dependencies]\nz='^2'",
            "[incompatibilities]\nz='^1'",
            "overrides=['missing']",
            "[dependencies]\na='*'",
        ] {
            assert!(
                resolve(&fixture(&[("a", extra), ("z", "")])).is_err(),
                "{extra}"
            );
        }
        assert!(
            resolve(&fixture(&[
                ("a", "overrides=['z']"),
                ("z", "overrides=['a']")
            ]))
            .is_err()
        );
    }
    #[test]
    fn override_edge_is_explicit_and_deterministic() {
        assert_eq!(
            resolve(&fixture(&[("a", "overrides=['z']"), ("z", "")])).unwrap(),
            ["z", "a"]
        );
    }
}
