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
    /// How much room one unit of this item takes in a pile or (later) a
    /// container. Drives pile capacity: a pile holds up to
    /// [`PILE_CAPACITY_BULK`] worth. Berries 1, planks/chunks 2, logs 3,
    /// tools 6. Defaults to 1.
    #[serde(default = "default_bulk")]
    pub bulk: u32,
    /// Collective-pile behavior. `None` ⇒ this item never merges into a
    /// stack — loose units only (the NPC tidy job leaves it alone).
    /// `Some` ⇒ the tidy job may gather units into a single storage-cell
    /// pile, capped and rendered per this config. A pile of `count` units
    /// is one [`WorldItem`] entity with that count.
    #[serde(default)]
    pub pile: Option<PileConfig>,
    /// Animation id (matching a registered [`crate::animations::AnimationDef`])
    /// played while an NPC *works* with this item equipped as a tool —
    /// e.g. an axe sets `"vanilla:chopping"`, a pickaxe `"vanilla:pickaxing"`.
    /// `None` ⇒ fall back to the NPC kind's default work clip. Only
    /// consulted for the item in the carrier's `EquippedTool` slot.
    #[serde(default)]
    pub work_animation: Option<String>,
    /// Local pose applied to this item's mesh when it's held in an NPC's
    /// hand (an equipped tool in the right hand, a carried unit in the
    /// left). `None` ⇒ identity, which suits meshes authored for the hand
    /// socket (KayKit tools). World-scale resource meshes — a 1.35 m log —
    /// set a `scale` here so they sit in the fist instead of dwarfing the
    /// carrier. Does not affect the loose [`WorldItem`]/pile render.
    #[serde(default)]
    pub hold_transform: Option<HoldTransform>,
}

/// Local transform for an item's mesh while held in an NPC's hand. All
/// fields optional in Lua: `hold_transform = { scale = 0.35 }` is the
/// common case (shrink a bulky resource to hand size).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct HoldTransform {
    /// Offset from the hand socket, in metres (local to the socket).
    #[serde(default)]
    pub pos: [f32; 3],
    /// Rotation as XYZ Euler angles in **degrees** (author-friendly; the
    /// render crate converts to radians).
    #[serde(default)]
    pub rot_deg: [f32; 3],
    /// Uniform scale. Defaults to 1.0 (no scaling).
    #[serde(default = "default_hold_scale")]
    pub scale: f32,
}

fn default_hold_scale() -> f32 {
    1.0
}

/// Per-item pile tuning. All fields optional in Lua — an item that just
/// wants default capacity and a base-mesh-only look can write `pile = {}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PileConfig {
    /// Max units in one pile. `None` ⇒ derive from `bulk` against
    /// [`PILE_CAPACITY_BULK`]. An explicit value wins (clamped ≥ 1).
    #[serde(default)]
    pub capacity: Option<u32>,
    /// Ascending count thresholds → mesh. The highest tier whose `min`
    /// is ≤ the pile's count picks the mesh; below the lowest `min` (i.e.
    /// a single unit) the base [`ItemDef::mesh`] renders — "a single unit
    /// looks like a single item, a stack looks like a stack." Empty ⇒ the
    /// pile always uses the base mesh regardless of count.
    #[serde(default)]
    pub tiers: Vec<PileTier>,
}

/// One count→mesh step in a [`PileConfig`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PileTier {
    /// Minimum unit count at which this mesh applies.
    pub min: u32,
    /// glTF/scene path, same `mods://` convention as [`ItemDef::mesh`].
    pub mesh: String,
}

/// Default total bulk a single pile holds. `capacity_units =
/// PILE_CAPACITY_BULK / bulk`, so logs (bulk 3) pile 4-high, planks
/// (bulk 2) 6-high, berries (bulk 1) 12-high. Engine tuning default;
/// a `PileConfig.capacity` overrides per item.
pub const PILE_CAPACITY_BULK: u32 = 12;

impl ItemDef {
    /// Whether the tidy job may merge this item into a pile (needs a
    /// `pile` config and room for more than one unit).
    pub fn is_pileable(&self) -> bool {
        self.pile.is_some() && self.pile_capacity() > 1
    }

    /// Units this item stacks to in one pile (≥ 1). Non-pile items → 1.
    pub fn pile_capacity(&self) -> u32 {
        match &self.pile {
            None => 1,
            Some(p) => p
                .capacity
                .unwrap_or_else(|| PILE_CAPACITY_BULK / self.bulk.max(1))
                .max(1),
        }
    }

    /// Mesh path to render for a stack of `count` units. Picks the
    /// highest matching pile tier, else the base mesh.
    pub fn pile_mesh(&self, count: u32) -> &str {
        if let Some(p) = &self.pile {
            let mut chosen: Option<&PileTier> = None;
            for t in &p.tiers {
                if count >= t.min && chosen.map(|c| t.min >= c.min).unwrap_or(true) {
                    chosen = Some(t);
                }
            }
            if let Some(t) = chosen {
                return &t.mesh;
            }
        }
        &self.mesh
    }

    /// In-hand local pose components: `(translation, rotation_xyz_radians,
    /// uniform_scale)`. Identity when no `hold_transform` is set. Returned
    /// as plain arrays so this bevy-free crate needn't know about `Transform`
    /// — the render crate assembles it.
    pub fn hold_pose(&self) -> ([f32; 3], [f32; 3], f32) {
        match &self.hold_transform {
            None => ([0.0; 3], [0.0; 3], 1.0),
            Some(h) => (
                h.pos,
                [
                    h.rot_deg[0].to_radians(),
                    h.rot_deg[1].to_radians(),
                    h.rot_deg[2].to_radians(),
                ],
                h.scale,
            ),
        }
    }
}

fn default_color() -> [f32; 3] {
    [0.7, 0.7, 0.7]
}

fn default_bulk() -> u32 {
    1
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
