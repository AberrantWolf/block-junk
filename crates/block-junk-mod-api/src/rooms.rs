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
    /// Min/max interior height of the room in layers.
    ///
    /// For roofed regions (see [`RoomSignature::roof_fraction`]) this is
    /// the *median clear headroom* under the ceiling across roofed floor
    /// columns — median, not min, so one low doorway beam or a single
    /// vaulted shaft doesn't swing the room's classification. For
    /// unroofed regions it's the minimum wall run height along the
    /// external perimeter (a walled yard with 1-high walls reads 1).
    ///
    /// (Pre-2026-07 this was a single bottom-up "every perimeter cell
    /// solid per layer" walk, which meant any wall opening — the air gap
    /// above a door block, a window — capped the height at that layer
    /// and usually voided the roof too.)
    EnclosureHeight {
        #[serde(default)]
        min: Option<u32>,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Whether the region must read as roofed (volumetric). Roofing is
    /// per-column: `has_roof` is true when at least
    /// `HAS_ROOF_MIN_FRACTION` (0.85) of floor columns find a ceiling
    /// block overhead — so pitched roofs, skylights, and chimney holes
    /// don't void it. `required = false` matches only *mostly open*
    /// regions; for finer control use [`Constraint::RoofFraction`].
    /// Omit for "any".
    HasRoof { required: bool },
    /// Min/max fraction (0..=1) of floor columns that find a ceiling
    /// block overhead. `{ max = 0.5 }` reads "at most half covered" —
    /// the walled-yard shape; `{ min = 0.85 }` is equivalent to
    /// `HasRoof { required = true }`.
    RoofFraction {
        #[serde(default)]
        min: Option<f32>,
        #[serde(default)]
        max: Option<f32>,
    },
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
    /// Required count of access points in the room's wall ring
    /// (volumetric only): `walkable_boundary` blocks (door blocks) plus
    /// *virtual doorways* — 1-wide wall openings with walkable headroom
    /// that the detector treats as boundaries instead of leaks. Use with
    /// `min = 1` to require an explicit access point — keeps the detector
    /// from registering accidental enclosures players never intended
    /// (a hole dug in terrain, a divot under a tree).
    DoorCount {
        #[serde(default)]
        min: u32,
        #[serde(default)]
        max: Option<u32>,
    },
    /// Required count of tag-`a` placements sitting directly next to a
    /// tag-`b` block: `{ kind = "adjacent_pair", a = "vanilla:seat",
    /// b = "vanilla:table", min = 2 }` reads "at least two seats at a
    /// table". Adjacency is horizontal-orthogonal at the same layer
    /// (the 4 cardinal neighbours — furniture standing side by side);
    /// multi-cell placements are normalised by footprint size exactly
    /// like [`Constraint::TagCount`], so a 2-cell table flanked on both
    /// cells still counts each touching seat once. The pair is ordered:
    /// the *count* is of `a` placements (each counted once no matter how
    /// many `b` cells it touches). Avoid `a == b` with multi-cell blocks
    /// — a placement's own cells are adjacent to each other and would
    /// self-count.
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

/// Count of tag-`a` placements horizontally adjacent to a tag-`b` cell
/// within a region — the precomputed data behind
/// [`Constraint::AdjacentPair`]. Ordered: `(seat, table)` counts seats
/// touching tables, `(table, seat)` counts tables touching seats.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdjacentPairCount {
    pub a: TagId,
    pub b: TagId,
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
    /// Subset of floor cells where the player has player-height vertical
    /// clearance (the cell directly above is air or in-cell-traversable).
    /// Cells with head-height obstructions stay in the floor set (so the
    /// room's enclosure is preserved) but don't count toward this. The
    /// `FloorArea` constraint reads from this when it's `Some` — patterns
    /// asking "minimum room size" want walkable size, not geometric.
    #[serde(default)]
    pub walkable_count: Option<u32>,
    /// Interior height in layers: median clear headroom under the
    /// ceiling (roofed regions) or minimum external wall run height
    /// (unroofed). See [`Constraint::EnclosureHeight`] for the full
    /// semantics. Walled yard with 1-high walls = 1; house with a
    /// ceiling two layers above the floor = 2.
    #[serde(default)]
    pub enclosure_height: Option<u32>,
    /// `Some(roof_fraction >= 0.85)` — see [`Constraint::HasRoof`].
    #[serde(default)]
    pub has_roof: Option<bool>,
    /// Fraction (0..=1) of floor columns with a ceiling block overhead
    /// within the roof scan cap. Pitched roofs count per column.
    #[serde(default)]
    pub roof_fraction: Option<f32>,
    #[serde(default)]
    pub door_count: Option<u32>,
    #[serde(default)]
    pub floor_composition: Option<FloorComposition>,

    /// Tag occurrence counts. For volumetric: tags on blocks INSIDE the air
    /// volume (furniture). For connective: tags on the component's blocks.
    #[serde(default)]
    pub tag_counts: Vec<TagCount>,

    /// Ordered tag-adjacency counts between tagged blocks in the region
    /// (see [`AdjacentPairCount`]). Only pairs with a nonzero count are
    /// listed. Same interior scan as `tag_counts`.
    #[serde(default)]
    pub adjacent_pairs: Vec<AdjacentPairCount>,
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
