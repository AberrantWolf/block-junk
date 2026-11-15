//! Saved compatibility is about referenced contracts, not an exact content lock.
use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, Resource, Serialize, Deserialize)]
pub(crate) struct ContentContracts {
    #[serde(skip)]
    pub upgrades: Vec<block_junk_mod_api::upgrades::WorldUpgrade>,
    pub blocks: BTreeMap<String, String>,
    pub movement: BTreeMap<String, String>,
}
impl ContentContracts {
    pub fn compile(
        blocks: &crate::blocks::BlockRegistry,
        profiles: &[block_junk_mod_api::architecture::MovementProfile],
    ) -> Self {
        let blocks = blocks.iter().map(|(_,def)| {
            // Balance, names, visuals and planner code deliberately do not enter
            // this contract. Legacy movement fields remain until M4/M6 replace them.
            let value = serde_json::json!({
                "footprint":def.footprint,"structure":def.structure,"slots":def.facility_slots,
                "aabb":def.entity_aabb,"solid":def.flags.solid,"nav_passable":def.flags.nav_passable,
                "support_below":def.flags.support_below,"support_in_cell":def.flags.support_in_cell,
                "room_boundary":def.flags.room_boundary,"walkable_boundary":def.flags.walkable_boundary,
                "legacy_slot":def.use_slot.as_ref().map(|s|serde_json::json!({"pose":s.pose,"yaw":s.yaw,"approach":s.approach})),
            });
            (def.id.as_str().to_owned(),value.to_string())
        }).collect();
        let movement = profiles
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    serde_json::to_string(p).expect("validated movement profile"),
                )
            })
            .collect();
        Self {
            blocks,
            movement,
            upgrades: Vec::new(),
        }
    }

    pub fn validate_save(
        &self,
        save: &crate::save::SaveFile,
        preset: &block_junk_mod_api::worlds::WorldPreset,
    ) -> Result<(), String> {
        let world = save
            .world
            .as_ref()
            .ok_or("save has no resolved world preset")?;
        if world.preset.id != preset.id
            || world.preset.terrain != preset.terrain
            || world.preset.generation != preset.generation
        {
            return Err(format!(
                "preset {} terrain/generation changed; a compatible world migration is required",
                world.preset.id
            ));
        }
        if world.preset.movement_profile != preset.movement_profile
            || world.contracts.movement.get(&preset.movement_profile)
                != self.movement.get(&preset.movement_profile)
        {
            return Err(format!(
                "movement profile {} changed; compatible migration required",
                preset.movement_profile
            ));
        }
        let mut referenced = BTreeSet::new();
        for record in world.placements.records.values() {
            referenced.insert(record.definition.as_str().to_owned());
        }
        for (_, id) in world.preset.terrain.entries() {
            referenced.insert(id.as_str().to_owned());
        }
        let mut slot = |slot: crate::blocks::BlockSlot| -> Result<(), String> {
            referenced.insert(
                save.block_slots
                    .get(slot.0 as usize)
                    .ok_or("saved block slot out of range")?
                    .clone(),
            );
            Ok(())
        };
        for chunk in &save.edited_chunks {
            for block in &chunk.chunk.blocks {
                slot(*block)?;
            }
        }
        for (_, project) in &save.plans {
            if let crate::protocol::PlanKind::Build { slot: block, .. } = project.kind {
                slot(block)?;
            }
        }
        for id in referenced {
            let saved = world
                .contracts
                .blocks
                .get(&id)
                .ok_or_else(|| format!("missing saved structural contract for {id}"))?;
            let current = self
                .blocks
                .get(&id)
                .ok_or_else(|| format!("missing referenced block {id}"))?;
            if saved != current {
                return Err(format!(
                    "block {id} placement geometry, use slots or movement contract changed; compatible migration required"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn contract_digest(contract: &str) -> String {
    blake3::hash(contract.as_bytes()).to_hex().to_string()
}
pub(crate) fn generation_contract(preset: &block_junk_mod_api::worlds::WorldPreset) -> String {
    serde_json::json!({"id":preset.id,"terrain":preset.terrain,"generation":preset.generation})
        .to_string()
}
impl ContentContracts {
    pub fn with_upgrades(
        mut self,
        upgrades: Vec<block_junk_mod_api::upgrades::WorldUpgrade>,
        presets: &[block_junk_mod_api::worlds::WorldPreset],
    ) -> Result<Self, String> {
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        let mut sources = BTreeMap::new();
        for upgrade in &upgrades {
            if upgrade.blocks.is_empty()
                && upgrade.movement.is_empty()
                && upgrade.generation.is_none()
            {
                return Err(format!("world_upgrades/{}: empty upgrade", upgrade.id));
            }
            if upgrade.blocks.len() + upgrade.movement.len() > 4096 {
                return Err(format!(
                    "world_upgrades/{}: too many contract changes",
                    upgrade.id
                ));
            }
            let mut check = |kind: &str,
                             id: &str,
                             from: &str,
                             to: &str,
                             current: &str|
             -> Result<(), String> {
                if !digest(from) || !digest(to) || from == to || contract_digest(current) != to {
                    return Err(format!(
                        "world_upgrades/{}: {kind}.{id} requires distinct source/destination BLAKE3 contracts; destination must be {}",
                        upgrade.id,
                        contract_digest(current)
                    ));
                }
                if let Some(other) =
                    sources.insert(format!("{kind}/{id}/{from}"), upgrade.id.clone())
                {
                    return Err(format!(
                        "world_upgrades/{} conflicts with {other} for {kind}.{id} from {from}",
                        upgrade.id
                    ));
                }
                Ok(())
            };
            for change in &upgrade.blocks {
                let current = self.blocks.get(change.block.as_str()).ok_or_else(|| {
                    format!(
                        "world_upgrades/{}: missing block {}",
                        upgrade.id, change.block
                    )
                })?;
                check(
                    "blocks",
                    change.block.as_str(),
                    &change.from_contract,
                    &change.to_contract,
                    current,
                )?;
                let definition: serde_json::Value =
                    serde_json::from_str(current).expect("compiled contract JSON");
                let slots = definition["slots"].as_array().expect("compiled slots");
                let mut targets = BTreeSet::new();
                for (old, new) in &change.slot_renames {
                    if old.is_empty()
                        || old.len() > 128
                        || old == new
                        || !slots.iter().any(|slot| slot["key"].as_str() == Some(new))
                        || !targets.insert(new)
                    {
                        return Err(format!(
                            "world_upgrades/{}: invalid or ambiguous slot rename {}.{old} -> {new}",
                            upgrade.id, change.block
                        ));
                    }
                }
            }
            for change in &upgrade.movement {
                let current = self.movement.get(&change.profile).ok_or_else(|| {
                    format!(
                        "world_upgrades/{}: missing movement profile {}",
                        upgrade.id, change.profile
                    )
                })?;
                check(
                    "movement",
                    &change.profile,
                    &change.from_contract,
                    &change.to_contract,
                    current,
                )?;
            }
            if let Some(change) = &upgrade.generation {
                let preset = presets
                    .iter()
                    .find(|preset| preset.id == change.preset)
                    .ok_or_else(|| {
                        format!(
                            "world_upgrades/{}: missing preset {}",
                            upgrade.id, change.preset
                        )
                    })?;
                check(
                    "generation",
                    &change.preset,
                    &change.from_contract,
                    &change.to_contract,
                    &generation_contract(preset),
                )?;
            }
        }
        self.upgrades = upgrades;
        Ok(self)
    }
    pub fn inspection(
        &self,
        preset: &block_junk_mod_api::worlds::WorldPreset,
    ) -> serde_json::Value {
        serde_json::json!({"blocks":self.blocks.iter().map(|(id,value)|(id,contract_digest(value))).collect::<BTreeMap<_,_>>(),"movement":self.movement.iter().map(|(id,value)|(id,contract_digest(value))).collect::<BTreeMap<_,_>>(),"generation":{&preset.id:contract_digest(&generation_contract(preset))}})
    }
}
