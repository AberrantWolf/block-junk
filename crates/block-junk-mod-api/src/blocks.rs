//! Block registry types — what mods register and what the engine stores.
//!
//! [`BlockId`] is the stable string identifier mods use everywhere. The
//! engine interns these to a compact numeric handle (`BlockSlot`) for the
//! wire format and per-cell chunk storage; that handle is engine-internal
//! and not visible to mods.

use serde::{Deserialize, Serialize};

/// Stable string identifier for a block kind, "namespace:name" by convention.
/// The namespace matches the mod that registered the block ("vanilla",
/// "mymod"). Equality is byte-exact on the full string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(pub String);

impl BlockId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for BlockId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for BlockId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for BlockId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Free-form tag, "namespace:name" by convention. Mods declare and consume
/// these to opt blocks into higher-level systems (room patterns, NPC AI).
/// The engine matches tags by id-equality only — it has no built-in meaning.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TagId(pub String);

impl TagId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TagId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for TagId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl core::fmt::Display for TagId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Engine-meaningful block properties. Read in hot loops (meshing, room
/// detection, raycast), so they're plain booleans rather than tag lookups.
/// Mod-meaningful properties live in [`BlockDef::tags`] instead.
///
/// `serde(default)` so a Lua table only has to list flags that differ
/// from `false`; omitted fields stay default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlockFlags {
    /// Collides with the player and occludes neighbour mesh faces.
    pub solid: bool,
    /// Volumetric room flood-fill stops at this block. Walls, doors, glass.
    pub room_boundary: bool,
    /// A boundary the player or an NPC can pass through (door, open gate).
    /// Implies `room_boundary`.
    pub walkable_boundary: bool,
    /// The cell directly ABOVE a block with this flag is a valid floor cell.
    /// Solid ground has this; water has this (you stand on the surface).
    pub support_below: bool,
    /// A cell that *contains* a block with this flag is itself a valid floor
    /// cell, regardless of what's below it. Ladders, rails.
    pub support_in_cell: bool,
    /// Appears in the player's hotbar / placement UI. Empty is `false`; most
    /// other vanilla blocks default to `true`.
    pub placeable: bool,
}

/// Full registered block definition. The engine holds one per [`BlockId`];
/// mods construct these and pass them to the engine's block-registration API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockDef {
    pub id: BlockId,
    pub display_name: String,
    pub flags: BlockFlags,
    /// Free-form tags. Engine-opaque; only matched by id-equality. See
    /// [`TagId`] for the namespace convention.
    #[serde(default)]
    pub tags: Vec<TagId>,
    /// Per-vertex tint for voxel-meshed blocks; also the swatch colour in
    /// the hotbar UI. Ignored when `mesh` is `Some`. RGB only — alpha is
    /// added at the render call site.
    pub color: [f32; 3],
    /// Optional asset path for a non-cube visual. When set, the client
    /// renders this block as a separate ECS entity loaded from the given
    /// glTF (or scene) path, instead of baking cube faces into the chunk
    /// mesh. Use the `mods://` asset source — e.g.
    /// `"mods://vanilla/models/bed.glb"`. Server ignores this field.
    #[serde(default)]
    pub mesh: Option<String>,
}
