//! Mask + ramp registry types referenced by per-block layer stacks.
//!
//! A block's surface appearance is a base color (from the `color` + `pattern`
//! pair on [`BlockDef`](crate::blocks::BlockDef)) plus a stack of **layers**.
//! Each layer references a tileable grayscale **mask** and a 1D color
//! **ramp** by stable id; the engine bakes both into texture-array atlases
//! once at boot and composites them per-fragment.
//!
//! Mods register masks and ramps with the same shape as blocks: a stable
//! `"namespace:id"` string, slot order = registration order, vanilla
//! ships its own set and other mods extend it. Block defs then reference
//! those ids inside `BlockDef.layers`.
//!
//! v1 ships **procedural** masks and **color-stop** ramps only. Image-
//! based mask sources and gradient-image ramps are reserved for later —
//! the existing variants are non-exhaustive so adding them is additive.
//!
//! Authoring example (Lua):
//!
//! ```lua
//! engine.masks.register {
//!     id = "vanilla:bubbles_large",
//!     source = { kind = "worley", cells = 4 },
//! }
//! engine.ramps.register {
//!     id = "vanilla:stone_grey",
//!     stops = {
//!         { 0.32, 0.32, 0.34 },
//!         { 0.58, 0.58, 0.60 },
//!     },
//! }
//! engine.blocks.register {
//!     id = "vanilla:grass",
//!     color = { 0.36, 0.62, 0.30 },
//!     layers = {
//!         { mask = "vanilla:bubbles_large", ramp = "vanilla:stone_grey",
//!           scale = 2.0, threshold = 0.62, softness = 0.05 },
//!     },
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Stable string identifier for a registered mask, "namespace:name" by
/// convention. Same equality / interning model as [`BlockId`](crate::blocks::BlockId).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MaskId(pub String);

impl MaskId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for MaskId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for MaskId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for MaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable string identifier for a registered ramp.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RampId(pub String);

impl RampId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RampId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for RampId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for RampId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the engine bakes the named mask layer at boot. Tagged on the
/// `kind` field so Lua-side authoring reads naturally:
/// `source = { kind = "worley", cells = 4 }`. Non-exhaustive so future
/// variants (image sources, value-noise, etc.) can be added without a
/// breaking bump.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MaskSource {
    /// Tileable Worley noise: 1.0 at the cell-point centres, fading to
    /// 0.0 by ~60% of the cell pitch. `cells` is the per-side cell
    /// count of the source tile — larger = smaller bubbles per tile.
    /// Must be `>= 1`.
    Worley { cells: u32 },
}

/// One mask entry registered by a mod. Slot index is assigned in
/// registration order by the engine and isn't visible from Lua.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaskDef {
    pub id: MaskId,
    pub source: MaskSource,
}

/// One color ramp. Stops are evenly distributed along U and linearly
/// interpolated between; the shader samples `vec2(mask, 0.5)` against
/// this strip so the ramp paints depth/shading within the masked area.
/// Must have at least 2 stops (a single-color "ramp" is two identical
/// stops; the dual-stop requirement is to keep the data shape uniform
/// and reject likely-typo zero/one-stop tables).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RampDef {
    pub id: RampId,
    pub stops: Vec<[f32; 3]>,
}

/// One mask+ramp layer in a block's texture stack. The engine resolves
/// `mask` / `ramp` to slot indices at boot (referenced ids that aren't
/// registered are a load-time error). Layers composite top-to-bottom:
/// layer `i+1` paints over layer `i`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerDef {
    pub mask: MaskId,
    pub ramp: RampId,
    /// World-units per mask repeat. Larger = bigger features.
    pub scale: f32,
    /// Smoothstep midpoint applied to the raw mask value to derive blend
    /// coverage. 0.5 keeps the mask's natural shapes; raising shrinks
    /// the masked region, lowering grows it.
    pub threshold: f32,
    /// Smoothstep half-width. 0 = crisp cartoon edge; 0.2 = soft.
    pub softness: f32,
}
