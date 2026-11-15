//! Frozen movement and settlement rules shared by simulation and inspection.
use bevy::prelude::Resource;
use block_junk_mod_api::architecture::{MovementProfile, SettlementDefinition};
use std::collections::BTreeMap;

#[derive(Resource, Clone, Debug, Default)]
pub struct WorldRules {
    pub capabilities: BTreeMap<String, block_junk_mod_api::capabilities::CapabilityDefinition>,
    pub movement: BTreeMap<String, MovementProfile>,
    pub settlements: BTreeMap<String, SettlementDefinition>,
}
