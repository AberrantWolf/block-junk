//! Item registry types — what mods register and what the engine stores.
//!
//! Items are world-physical things that an actor can carry: dropped logs, ore
//! chunks, tools later on (Phase 5). Distinct from [`BlockId`](crate::blocks::BlockId)
//! both conceptually (items live in carry stacks and ECS entities at a
//! position; blocks live on the 1m grid) and in storage (separate registry,
//! separate slot space). A "wood log" item is not the same value as a "wood
//! block" — destroying one block may yield several items, or none.
//!
//! Mods register one [`ItemDef`] per kind and reference items by stable
//! [`ItemId`] strings everywhere a script touches them (in
//! [`BlockDef::drops`](crate::blocks::BlockDef::drops), in
//! recipes, in carry-cap validation). The engine interns these to an
//! engine-internal compact handle for the wire format.
//!
//! `tool_tags` is wired here in Phase 1 but unused until Phase 5 brings the
//! tool system online.

use serde::{Deserialize, Serialize};

use crate::blocks::TagId;

/// Stable string identifier for an item kind, "namespace:name" by
/// convention. Matches the namespacing rule used by
/// [`BlockId`](crate::blocks::BlockId) so referring to an item from a
/// block def or a recipe reads consistently.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId(pub String);

impl ItemId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ItemId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for ItemId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for ItemId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full registered item definition. The engine holds one per [`ItemId`].
///
/// Phase 1 captures the visual + carry-arithmetic essentials; `tool_tags`
/// is reserved for Phase 5 when tools start gating which work-actions
/// the carrier can perform. Mods may set it on day-one items so the
/// migration to tools doesn't require touching content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemDef {
    pub id: ItemId,
    pub display_name: String,
    /// glTF/scene path the client uses to render the loose
    /// [`WorldItem`] entity. Resolved through the same `mods://` asset
    /// source `BlockDef::mesh` uses — e.g.
    /// `"mods://vanilla/models/wood_log.gltf"`. Required: every item
    /// must render *as* something. Server boots ignore the field; the
    /// client validates the path resolves at startup.
    pub mesh: String,
    /// Optional fallback color for icons / inventory tints when the
    /// renderer can't draw the full mesh (e.g. tiny HUD chip). RGB
    /// only — alpha is added at the call site. Defaults to mid-grey.
    #[serde(default = "default_color")]
    pub color: [f32; 3],
    /// Free-form tags an item carries, looked up by the engine when a
    /// downstream system wants to filter by item *role* rather than by
    /// exact id — most prominently the Phase 5 tool gating, where a
    /// `BlockDef.work_action.required_tool` field will match against
    /// any item carrying that tag. Match is byte-exact on the tag id.
    /// `vec![]` for plain resources (logs, ore).
    #[serde(default)]
    pub tool_tags: Vec<TagId>,
}

fn default_color() -> [f32; 3] {
    [0.7, 0.7, 0.7]
}

/// A drop produced when a block is destroyed: which item, how many. Used
/// by [`BlockDef::drops`](crate::blocks::BlockDef::drops). One entry per
/// distinct item kind; a block that drops wood + bark + sap is three
/// entries with `count = 1` each (or whatever counts the mod picks).
///
/// `count = 0` is legal but pointless — engines may warn and skip it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemDrop {
    pub item: ItemId,
    /// How many [`WorldItem`] entities to spawn at the destroyed cell.
    /// One entity per unit in the MVP; a future stack-merge step can
    /// fold piles back to fewer entities without changing this shape.
    pub count: u32,
}
