//! Requirement-list regressions at the finalized registry boundary.

use crate::items::{ItemBootstrapError, ItemRegistry};
use crate::recipes::{RecipeBootstrapError, RecipeRegistry};
use block_junk_mod_api::{blocks::BlockDef, items::ItemDef, recipes::RecipeDef};

fn items() -> ItemRegistry {
    let defs: Vec<ItemDef> = serde_json::from_value(serde_json::json!([
        {"id": "test:log", "display_name": "Log", "mesh": "unused.gltf"},
        {"id": "test:stone", "display_name": "Stone", "mesh": "unused.gltf"}
    ]))
    .unwrap();
    ItemRegistry::build(defs).unwrap()
}

fn recipe(id: &str, inputs: serde_json::Value) -> RecipeDef {
    serde_json::from_value(serde_json::json!({
        "id": id, "display_name": "Fixture", "inputs": inputs,
        "output": {"item": "test:stone", "count": 1},
        "duration_secs": 2.0, "station": "test:bench"
    }))
    .unwrap()
}

fn block(id: &str, materials: serde_json::Value) -> BlockDef {
    serde_json::from_value(serde_json::json!({
        "id": id, "display_name": "Fixture", "flags": {},
        "color": [0.5, 0.5, 0.5], "materials": materials, "drops": []
    }))
    .unwrap()
}

#[test]
fn named_facilities_validate_behavior_and_physical_slot_identity() {
    use block_junk_mod_api::architecture::CompiledArchitecture;
    let value = serde_json::json!({
        "id":"test:seat","display_name":"Seat","flags":{},"color":[1,1,1],
        "facility_actions":{"sit":{"kind":"timed","duration_secs":2}},
        "facility_slots":[{"key":"seat","pose":[0,0.5,0],"yaw":0,"approaches":[[1,0,0]],"exclusive":true,"interactions":["sit"]}]
    });
    let def: BlockDef = serde_json::from_value(value.clone()).unwrap();
    let variants = CompiledArchitecture::compile(&def).unwrap();
    assert!(
        variants
            .iter()
            .all(|variant| variant.slots.len() == 1 && variant.structure.solid_cells.is_empty())
    );
    let mut invalid = value.clone();
    invalid["facility_slots"][0]["interactions"] = serde_json::json!(["typo"]);
    assert!(
        CompiledArchitecture::compile(&serde_json::from_value(invalid).unwrap())
            .unwrap_err()
            .contains("authored actions")
    );
    let mut invalid = def.clone();
    let mut duplicate = invalid.facility_slots[0].clone();
    duplicate.key = "second_person".into();
    invalid.facility_slots.push(duplicate);
    assert!(
        CompiledArchitecture::compile(&invalid)
            .unwrap_err()
            .contains("exclusive use pose")
    );
    let mut invalid = value;
    invalid["facility_actions"]["sit"]["consumption"] =
        serde_json::json!({"kind":"stock","count":1});
    assert!(
        CompiledArchitecture::compile(&serde_json::from_value(invalid).unwrap())
            .unwrap_err()
            .contains("container")
    );
}

#[test]
fn air_is_engine_owned_even_without_any_mod_content() {
    let (registry, slots) = crate::blocks::BlockRegistry::build(Vec::new()).unwrap();
    assert_eq!(
        registry.id_of(crate::blocks::BlockSlot::EMPTY).as_str(),
        "engine:air"
    );
    assert_eq!(slots.slots.len(), 1);
    let error =
        crate::blocks::BlockRegistry::build(vec![block("engine:air", serde_json::json!([]))])
            .err()
            .unwrap();
    assert!(matches!(
        error,
        crate::blocks::BootstrapError::ReservedId(_)
    ));
}

#[test]
fn duplicate_recipe_ingredients_fail_with_definition_and_entry_locations() {
    let recipe = recipe(
        "test:bad",
        serde_json::json!([
            {"item": "test:log", "count": 2},
            {"item": "test:stone", "count": 1},
            {"item": "test:log", "count": 3}
        ]),
    );
    let error = RecipeRegistry::build(vec![recipe], &items())
        .err()
        .expect("duplicate must fail");
    assert!(
        matches!(&error, RecipeBootstrapError::DuplicateInput { recipe, item, first: 1, duplicate: 3 }
        if recipe.as_str() == "test:bad" && item.as_str() == "test:log")
    );
    let message = error.to_string();
    for expected in ["test:bad", "test:log", "inputs[1]", "inputs[3]"] {
        assert!(message.contains(expected), "{message}");
    }
}

#[test]
fn duplicate_build_materials_fail_instead_of_creating_an_unsatisfiable_plan() {
    let block = block(
        "test:bad",
        serde_json::json!([
            {"item": "test:log", "count": 2},
            {"item": "test:stone", "count": 1},
            {"item": "test:log", "count": 2}
        ]),
    );
    let error = items().validate_block_drops(&[block]).unwrap_err();
    assert!(
        matches!(&error, ItemBootstrapError::DuplicateMaterial { block, item, first: 1, duplicate: 3 }
        if block == "test:bad" && item.as_str() == "test:log")
    );
    let message = error.to_string();
    assert!(message.contains("materials[1]") && message.contains("materials[3]"));
}

#[test]
fn distinct_definitions_can_require_the_same_items() {
    let requirements = serde_json::json!([
        {"item": "test:log", "count": 5}, {"item": "test:stone", "count": 2}
    ]);
    let items = items();
    let recipes = RecipeRegistry::build(
        vec![
            recipe("test:a", requirements.clone()),
            recipe("test:b", requirements.clone()),
        ],
        &items,
    )
    .unwrap();
    assert_eq!(recipes.slot_count(), 2);
    items
        .validate_block_drops(&[
            block("test:a", requirements.clone()),
            block("test:b", requirements),
        ])
        .unwrap();
}

#[test]
fn derived_material_counts_reject_overflow_without_publishing_saturated_drops() {
    let mut def = block(
        "test:wall",
        serde_json::json!([{"item":"test:stone","count":u32::MAX}]),
    );
    def.drops = None;
    assert!(def.resolve_drops(2.).unwrap_err().contains("stack limit"));
    assert!(def.drops.is_none());
    def.resolve_drops(1.).unwrap();
    assert_eq!(def.resolved_drops()[0].count, u32::MAX);
}

#[test]
fn need_rules_reject_invalid_thresholds_and_missing_cross_need_references() {
    use crate::npc_registry::NeedRegistry;
    let value = serde_json::json!({"id":"sleep","display_name":"Sleep","decay_per_sec":0.01,"preempt_threshold":0.8});
    let valid = serde_json::from_value(value.clone()).unwrap();
    NeedRegistry::build(vec![valid]).unwrap();
    let mut invalid = value.clone();
    invalid["preempt_threshold"] = serde_json::json!(1.1);
    assert!(
        NeedRegistry::build(vec![serde_json::from_value(invalid).unwrap()])
            .unwrap_err()
            .to_string()
            .contains("preempt_threshold")
    );
    let mut invalid = value;
    invalid["decay_boost"] = serde_json::json!({"need":"missing","above":0.5,"multiplier":2});
    let error = NeedRegistry::build(vec![serde_json::from_value(invalid).unwrap()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing") && error.contains("decay_boost"));
}

#[test]
fn empty_requirements_remain_valid_for_explicit_free_content() {
    let items = items();
    RecipeRegistry::build(vec![recipe("test:free", serde_json::json!([]))], &items).unwrap();
    items
        .validate_block_drops(&[block("test:free", serde_json::json!([]))])
        .unwrap();
}

#[test]
fn shipped_lua_content_passes_requirement_validation() {
    use block_junk_scripting::{LoadContext, ModRegistry};
    let context = LoadContext::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mods");
    ModRegistry::load_dir(block_junk_mod_api::Side::Server, &root, &context).unwrap();
    let mut blocks = context.take_blocks();
    let multiplier = context.take_material_drop_multiplier().unwrap_or(1.0);
    for block in &mut blocks {
        block.resolve_drops(multiplier).unwrap();
    }
    let items = ItemRegistry::build(context.take_items()).unwrap();
    items.validate_block_drops(&blocks).unwrap();
    let recipes = RecipeRegistry::build(context.take_recipes(), &items).unwrap();
    recipes.validate_against_blocks(&blocks).unwrap();
}
