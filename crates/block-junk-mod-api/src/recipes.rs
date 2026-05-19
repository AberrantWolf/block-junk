//! Recipe registry types — what a mod registers via
//! `engine.recipes.register` and what the engine stores.
//!
//! A recipe is a "consume these items at this kind of station for this
//! long, get these items back" deal. The engine doesn't branch on
//! recipe kind: every recipe runs through one pipeline (await
//! interaction, consume inputs, wait `duration_secs`, spawn outputs).
//! Adding a new recipe is purely a data change.
//!
//! Crafting is gated by station type (`station: TagId`) and optionally
//! by tool tag (`required_tool: Option<TagId>`). Player-driven
//! crafting in Phase 6a evaluates these on every L-click; NPC
//! crafting (Phase 6b) routes through the same gates from the haul
//! scheduler.

use serde::{Deserialize, Serialize};

use crate::blocks::TagId;
use crate::items::ItemDrop;

/// Stable string identifier for a recipe, "namespace:name" by
/// convention. Mirrors [`crate::items::ItemId`] and
/// [`crate::blocks::BlockId`]; recipes referenced in future event
/// hooks (`on_task_complete`) name them with this id, not the
/// engine-internal slot.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecipeId(pub String);

impl RecipeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RecipeId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for RecipeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for RecipeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full registered recipe. Cross-validated at boot against the item
/// registry (every `inputs` + `output` ItemId resolves) and against
/// the block registry (at least one block carries `station_tag ==
/// recipe.station`, so the recipe is reachable).
///
/// `inputs` is a vec to allow multi-ingredient recipes later. Phase
/// 6a only exercises single-input recipes (1 wood_log → 1 wood_plank);
/// the engine's player-side handler walks `inputs` and rejects clicks
/// whose carry doesn't satisfy every entry. Future multi-input UX
/// will need a workbench-inventory model (stations buffer
/// ingredients across multiple deliveries) — out of scope for 6a.
///
/// `output` is single-entry for now. A future multi-output variant
/// would model byproducts (smelting iron → ingot + slag) — same
/// shape change with cascading downstream UX.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecipeDef {
    pub id: RecipeId,
    pub display_name: String,
    /// Items consumed from the crafter's stack per craft. Each entry
    /// names an [`crate::items::ItemId`] and a `count`. Empty inputs
    /// is legal (a free-action recipe — e.g. a "summon ambient
    /// sparkles" decorative crafting trigger) but unusual.
    #[serde(default)]
    pub inputs: Vec<ItemDrop>,
    /// What the recipe produces per craft. Spawned as
    /// [`crate::items::ItemId`] world items adjacent to the station.
    pub output: ItemDrop,
    /// How long the craft takes in seconds. Same kind of value as
    /// `WorkAction.duration_secs`; the engine clamps to a sane
    /// upper bound so a misbehaving mod can't park a crafter for
    /// an hour.
    pub duration_secs: f32,
    /// Station tag this recipe runs at. Cross-referenced at boot
    /// against [`crate::blocks::BlockDef::station_tag`] — a recipe
    /// with no matching station block is a typo, flagged loudly at
    /// load.
    pub station: TagId,
    /// Tool the crafter must hold to perform the recipe. Matches
    /// against [`crate::items::ItemDef::tool_tags`] the same way
    /// `WorkAction.required_tool` does. `None` ⇒ no tool required.
    #[serde(default)]
    pub required_tool: Option<TagId>,
}
