//! Room pattern registry. Mods declare what counts as a "bedroom" or
//! "crossroads"; the engine's room detector populates a [`RoomSignature`]
//! per detected region and matches it against the registered patterns.
//!
//! Two pattern domains:
//!
//! - **Volumetric** — an enclosed air region with a floor plane. Bounded by
//!   `room_boundary` blocks. Floor cells require player-reachable support
//!   (solid/water below or ladder/rail in-cell) and adequate headroom.
//! - **Connective** — a connected component of structure-tagged blocks, no
//!   enclosure required. Crossroads, signpost clusters, gravestone rows.
//!
//! Patterns form an inheritance tree via [`RoomPattern::parent`]; matching
//! finds the deepest node whose constraints all pass and whose ancestors
//! also pass. Ties at the same depth break by [`RoomPattern::priority`]
//! then by registration order.

use serde::{Deserialize, Serialize};

use crate::blocks::TagId;
use crate::shared::BlockPos;

/// Stable string identifier, "namespace:name" by convention.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomPatternId(pub String);

impl RoomPatternId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RoomPatternId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for RoomPatternId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for RoomPatternId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which detection domain a pattern lives in. A pattern's domain must
/// match its parent's — a volumetric child can't extend a connective root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatternDomain {
    Volumetric,
    Connective,
}

/// What kind of floor support is being measured by [`Constraint::FloorFraction`].
/// Cells are categorised once per signature, then summed by fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorKind {
    /// Cell below the floor cell is a solid block.
    Solid,
    /// Cell below the floor cell is water (player stands on the surface).
    WaterBelow,
    /// The floor cell itself contains a `support_in_cell` block (ladder, rail).
    SupportInCell,
}

/// Predicate constraints. Each variant reads exactly one field of the
/// computed [`RoomSignature`], so evaluation is a flat match per item.
///
/// Mods declare these as Lua tables tagged by `kind`:
/// ```lua
/// { kind = "volume", min = 8, max = 50 }
/// { kind = "floor_fraction", surface = "solid", min = 0.8 }
/// { kind = "tag_count", tag = "vanilla:bed", min = 1 }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Constraint {
    /// Total air-cell volume of the region (volumetric only).
    Volume {
        #[serde(default)]
        min: Option<u32>,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Floor-cell count of the region (volumetric only).
    FloorArea {
        #[serde(default)]
        min: Option<u32>,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Min/max headroom in cells above the floor plane (volumetric only).
    Headroom {
        #[serde(default)]
        min: Option<u32>,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Whether the region must have a contiguous block ceiling (volumetric).
    /// `false` matches "open to sky" enclosures (walled yards). Omit for "any".
    HasRoof { required: bool },
    /// Fraction of floor cells supported by the named [`FloorKind`].
    /// Sum of all kinds is ≤ 1.0.
    FloorFraction { surface: FloorKind, min: f32 },
    /// Required tag occurrence count. For volumetric: tags on blocks INSIDE
    /// the air volume (furniture, decor). For connective: tags on blocks
    /// IN the component itself.
    TagCount {
        tag: TagId,
        #[serde(default)]
        min: u32,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Fraction of in-region cells carrying the tag. Pair with TagCount
    /// for "characterized by X AND requires at least one X".
    TagFraction { tag: TagId, min: f32 },
    /// Component cell count (connective only).
    ComponentSize {
        #[serde(default)]
        min: Option<u32>,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Number of `(a, b)` adjacency pairs in the component (connective only).
    /// Crossroads-style: `{ a = "vanilla:road", b = "vanilla:sign", min = 2 }`.
    AdjacentPair { a: TagId, b: TagId, min: u32 },
}

/// A registered pattern. Constraints are *additive* with the parent — at
/// match time, an ancestor's constraints must pass before a descendant's
/// are evaluated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomPattern {
    pub id: RoomPatternId,
    pub display_name: String,
    /// Parent in the inheritance tree. Domain must agree.
    #[serde(default)]
    pub parent: Option<RoomPatternId>,
    pub domain: PatternDomain,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    /// Tie-break for sibling matches at the same tree depth. Higher wins;
    /// ties break on registration order.
    #[serde(default)]
    pub priority: i32,
}

/// A single tag occurrence count within a region.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: TagId,
    pub count: u32,
}

/// Fraction breakdown of floor support by [`FloorKind`]. Sums to ≤ 1.0;
/// the remainder is "other" (only reachable once new support kinds land).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FloorComposition {
    pub solid: f32,
    pub water_below: f32,
    pub support_in_cell: f32,
}

/// Inclusive integer-cell axis-aligned bounding box.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BBox {
    pub min: BlockPos,
    pub max: BlockPos,
}

/// Per-region computed properties. The detector populates one per dirty
/// region; the matcher walks the pattern tree using only this — no chunk
/// access needed at match time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomSignature {
    pub domain: PatternDomain,
    pub bbox: BBox,
    /// Floor cells (volumetric) or component cells (connective).
    pub cell_count: u32,

    // Volumetric-only fields. `None` for connective signatures.
    #[serde(default)]
    pub volume: Option<u32>,
    #[serde(default)]
    pub min_headroom: Option<u32>,
    #[serde(default)]
    pub max_headroom: Option<u32>,
    #[serde(default)]
    pub has_roof: Option<bool>,
    #[serde(default)]
    pub door_count: Option<u32>,
    #[serde(default)]
    pub floor_composition: Option<FloorComposition>,

    /// Tag occurrence counts. For volumetric: tags on blocks INSIDE the air
    /// volume (furniture). For connective: tags on the component's blocks.
    #[serde(default)]
    pub tag_counts: Vec<TagCount>,
}

/// Stable session-scoped room handle. Issued by the detector; lifetime
/// tied to the region's existence. A wall break that destroys two rooms
/// and creates one merged room consumes two old ids and issues a new one.
/// Not persisted (today) — RoomIds are not stable across server restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomId(pub u32);

/// Server-side hook event. Mods subscribe via `engine.on_room_event` to
/// react — spawn an NPC, log to a journal, fire a sound.
///
/// `Created`/`Destroyed` always fire on appearance/disappearance.
/// `Changed` fires when an existing region (same floor footprint) gains
/// or loses a pattern match — e.g. a furniture block lands inside,
/// deepening the matched type from `enclosed_space` to `small_house`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoomEvent {
    Created {
        room: RoomId,
        /// Deepest matching pattern, or `None` if the region passes no
        /// registered pattern's constraints (still a valid `RoomId` —
        /// the engine tracks unmatched regions so a later edit can turn
        /// one into a match without re-creating the room handle).
        pattern: Option<RoomPatternId>,
        signature: RoomSignature,
    },
    Changed {
        room: RoomId,
        from: Option<RoomPatternId>,
        to: Option<RoomPatternId>,
        signature: RoomSignature,
    },
    Destroyed {
        room: RoomId,
    },
}
