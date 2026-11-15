//! Declarative save transformations, executed against a staged save before hooks.
//! Exact source and destination contracts prevent an old migration from silently
//! approving a later, unrelated definition change.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldUpgrade {
    pub id: String,
    #[serde(default)]
    pub blocks: Vec<BlockUpgrade>,
    #[serde(default)]
    pub movement: Vec<MovementUpgrade>,
    #[serde(default)]
    pub generation: Option<GenerationUpgrade>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockUpgrade {
    pub block: crate::blocks::BlockId,
    pub from_contract: String,
    pub to_contract: String,
    pub footprint: FootprintUpgrade,
    #[serde(default)]
    pub slot_renames: BTreeMap<String, String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FootprintUpgrade {
    /// Existing cells remain valid only when the footprint is unchanged.
    Preserve,
    /// Replace the old occupied cells at the same anchor, rejecting collisions.
    RebuildAtAnchor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementUpgrade {
    pub profile: String,
    pub from_contract: String,
    pub to_contract: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationUpgrade {
    pub preset: String,
    pub from_contract: String,
    pub to_contract: String,
    pub terrain_policy: TerrainUpgrade,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerrainUpgrade {
    /// Keep edited chunks and materialize the old terrain around retained actors
    /// and placements. Other procedural chunks use the new generation contract.
    PreserveRetainedRegions,
}
