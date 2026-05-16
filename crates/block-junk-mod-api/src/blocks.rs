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
    /// Base colour for the block's procedurally-generated 16×16 texture and
    /// the hotbar icon. Ignored when `mesh` is `Some` (mesh blocks bring
    /// their own materials). RGB only — alpha is added at the render call
    /// site.
    pub color: [f32; 3],
    /// Procedural pattern applied over `color` when generating the block's
    /// texture and hotbar icon. See the `Pattern` enum in
    /// `block_textures.rs` for the recognised values. `None` defaults to
    /// `"noise"` (a subtle per-pixel jitter) — a uniform colour reads as
    /// flat, which is the look this whole field exists to fix.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Optional asset path for a non-cube visual. When set, the client
    /// renders this block as a separate ECS entity loaded from the given
    /// glTF (or scene) path, instead of baking cube faces into the chunk
    /// mesh. Use the `mods://` asset source — e.g.
    /// `"mods://vanilla/models/bed.glb"`. Server ignores this field.
    #[serde(default)]
    pub mesh: Option<String>,
    /// Cell offsets occupied by this block in default ([`Cardinal::East`])
    /// orientation. The anchor cell is `[0,0,0]`; additional cells extend
    /// into +X (east) and may use +Y (tall fixtures) or +Z (wide).
    /// Defaults to a single-cell footprint when the field is omitted, so
    /// existing single-cell blocks register unchanged.
    #[serde(default = "default_footprint")]
    pub footprint: Vec<[i32; 3]>,
    /// Tight model-space bounding box used by the raycast to skip past
    /// block-entities whose mesh doesn't fill its footprint cells. Local
    /// origin is the bottom-centre of the anchor cell (matching the
    /// modeling guide); the AABB is in default orientation. `None` means
    /// "use the union of cube cells covered by `footprint`," which is the
    /// safe fallback for cube-shaped blocks.
    #[serde(default)]
    pub entity_aabb: Option<EntityAabb>,
    /// Marks this block as something NPCs can consume to satisfy a
    /// need. Present ⇒ an NPC whose planner picks a Consume goal on
    /// this cell pathfinds to a standable neighbour, stands still for
    /// `duration_secs`, and on completion subtracts `restores` from
    /// the named need (clamped at 0). The engine validates at boot
    /// that `need` refers to a registered need id. `None` (the
    /// default) ⇒ NPCs never consider this block for a Consume goal.
    #[serde(default)]
    pub consumable: Option<Consumable>,
    /// Marks this block as a sleeper (a bed, a bedroll, a sarcophagus —
    /// anything one NPC can use to satisfy a need over a long stretch
    /// of time). Mechanically similar to `consumable` but with two
    /// extra rules: (a) only one NPC can claim a given sleeper at a
    /// time (the engine maintains a per-anchor claim table) and
    /// (b) duration is allowed to be much longer (sleep is intended
    /// to feel like minutes, not seconds). The engine validates at
    /// boot that `need` refers to a registered need id, same as
    /// `consumable`. `None` ⇒ NPCs never consider this block for sleep.
    #[serde(default)]
    pub sleeper: Option<Sleeper>,
}

/// Block-level "interacting with this satisfies a need" declaration.
/// Deliberately kept generic — a food basket, a healing fountain, a
/// spell scroll, a workbench, and an altar are all the same shape from
/// the engine's perspective: walk there, stand still for some time,
/// decrement a named need. The mod owns what the action *means*; the
/// engine just executes the path-arrive-pause-restore loop.
///
/// Consumption is **non-destructive** in this slice — the block stays
/// after an NPC consumes from it. Depletion / regrowth (a basket
/// emptying, a scroll being used up) is a future concern; today every
/// consumable block is a permanent attraction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Consumable {
    /// Id of the need this consumption reduces. Cross-validated against
    /// the [`NeedDef`](crate::npcs::NeedDef) registry at boot — a
    /// consumable referencing an unregistered need is a load error.
    /// Plain `String` (not `NeedId`) matches the existing
    /// `default_needs` convention on `NpcKindDef`.
    pub need: String,
    /// How much deficit is subtracted from the named need on completion.
    /// Need values run 0.0 (fully satisfied) → 1.0 (critical), so a
    /// larger `restores` is a more potent consumption. Clamped at 0.0
    /// after the subtraction. Sensible range is roughly `[0.1, 1.0]`.
    pub restores: f32,
    /// How long the NPC stands still at the cell before the `restores`
    /// amount is applied. Gives the action visible weight (NPC clearly
    /// *interacted*, didn't just touch and turn). Engine clamps to a
    /// sane bound so a misbehaving mod can't park an NPC indefinitely.
    pub duration_secs: f32,
}

/// Block-level "an NPC can sleep here to satisfy a need" declaration.
/// Same shape as [`Consumable`] but a single sleeper accommodates one
/// NPC at a time — the engine reserves the anchor cell for whichever
/// NPC committed first and rejects competing claims until release.
/// Beds today, larger fixtures (a campfire ring, a guest hall pad)
/// later; the engine doesn't care what the asset is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sleeper {
    /// Id of the need this sleep reduces. Cross-validated against the
    /// [`NeedDef`](crate::npcs::NeedDef) registry at boot — a sleeper
    /// referencing an unregistered need is a load error.
    pub need: String,
    /// How much deficit is subtracted from the named need on completion.
    /// Need values run 0.0 (fully satisfied) → 1.0 (critical), so a
    /// larger `restores` is more refreshing. Clamped at 0.0 after the
    /// subtraction. A whole night might restore 0.8–1.0; a nap less.
    pub restores: f32,
    /// How long the NPC stands at the sleeper before the `restores`
    /// amount is applied. Engine clamps to a sane upper bound so a
    /// misbehaving mod can't park an NPC for an hour, but the bound
    /// is much larger than the consumable bound — sleep is allowed to
    /// feel like minutes.
    pub duration_secs: f32,
}

/// Default footprint helper for serde. A single cell at the anchor.
fn default_footprint() -> Vec<[i32; 3]> {
    vec![[0, 0, 0]]
}

/// Local-space AABB of a block-entity model in default orientation.
///
/// Coordinates are in the model frame: `(0, 0, 0)` is the bottom-centre of
/// the anchor cell (the same origin convention the modeling guide tells
/// authors to model against). Y is up; +X is the default-orientation
/// "extends" direction. The box is inclusive on both ends — points on a
/// face count as inside.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntityAabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// Cardinal placement orientation for a block entity. East is the default
/// — that's the direction a multi-cell footprint extends in unrotated
/// model space, per the modeling guide. Rotations are 90° steps around +Y.
///
/// Stored as a single byte on disk and on the wire (one entry per anchored
/// block-entity in a chunk's sidecar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Cardinal {
    #[default]
    East = 0,
    North = 1,
    West = 2,
    South = 3,
}

impl Cardinal {
    pub const ALL: [Cardinal; 4] = [
        Cardinal::East,
        Cardinal::North,
        Cardinal::West,
        Cardinal::South,
    ];

    /// Snap a yaw (radians, body rotation around +Y) to the nearest
    /// cardinal. Bevy convention: yaw=0 looks toward -Z (north), so a
    /// player *facing* north should place a bed extending into their
    /// forward direction… but the orientation convention here is which way
    /// the entity *itself* extends from its anchor. We pick orientation to
    /// match the player's facing: facing east → entity extends east, etc.
    pub fn from_yaw_facing(yaw: f32) -> Self {
        let two_pi = std::f32::consts::TAU;
        let frac = std::f32::consts::FRAC_PI_2;
        let normalised = yaw.rem_euclid(two_pi);
        // Bevy yaw=0 → forward is -Z (north), yaw=PI/2 → forward is -X
        // (west), etc. Map yaw to the cardinal whose extends-direction
        // matches the forward vector.
        // forward = (sin(-yaw), 0, -cos(yaw)) for yaw applied as
        // Quat::from_rotation_y(yaw) on -Z. At yaw=0: (0,0,-1)=N.
        // At yaw=-PI/2: (1,0,0)=E. So:
        //   yaw ≈ -PI/2 → East
        //   yaw ≈ 0     → North
        //   yaw ≈ +PI/2 → West
        //   yaw ≈ ±PI   → South
        // Quantise: ((yaw + PI/4) / (PI/2)) floored, mod 4, indexed into
        // [North, West, South, East].
        let bucket = ((normalised + frac * 0.5) / frac).floor() as i32 & 3;
        match bucket {
            0 => Cardinal::North,
            1 => Cardinal::West,
            2 => Cardinal::South,
            _ => Cardinal::East,
        }
    }

    /// Rotate by 90° steps. `+1` = one step counter-clockwise viewed from
    /// above (E → N → W → S → E). Used by the rotate-during-placement
    /// hotkey so the user can override the auto-snapped orientation.
    pub fn rotated(self, steps: i32) -> Self {
        let i = ((self as i32) + steps).rem_euclid(4) as u8;
        match i {
            0 => Cardinal::East,
            1 => Cardinal::North,
            2 => Cardinal::West,
            _ => Cardinal::South,
        }
    }

    /// Yaw in radians for rendering: rotate the model's `SceneRoot` by
    /// `Quat::from_rotation_y(self.yaw())`. Composes with the modeling
    /// guide's default-east convention so rotating produces the right
    /// visual orientation in world space.
    pub fn yaw(self) -> f32 {
        let frac = std::f32::consts::FRAC_PI_2;
        match self {
            Cardinal::East => 0.0,
            Cardinal::North => frac,
            Cardinal::West => std::f32::consts::PI,
            Cardinal::South => -frac,
        }
    }

    /// Rotate a default-orientation cell offset to this orientation. Y is
    /// preserved; the rotation is in the X/Z plane around the anchor cell.
    pub fn rotate_offset(self, offset: [i32; 3]) -> [i32; 3] {
        let [x, y, z] = offset;
        match self {
            // Derived from the standard right-handed Y-up rotation matrix
            // R_y(θ): (x, z) ↦ (x cos θ + z sin θ, -x sin θ + z cos θ).
            Cardinal::East => [x, y, z],
            Cardinal::North => [z, y, -x],
            Cardinal::West => [-x, y, -z],
            Cardinal::South => [-z, y, x],
        }
    }

    /// Rotate a default-orientation model-space point. Same X/Z rotation as
    /// `rotate_offset` but in floats — used for AABB ray tests.
    pub fn rotate_point(self, p: [f32; 3]) -> [f32; 3] {
        let [x, y, z] = p;
        match self {
            Cardinal::East => [x, y, z],
            Cardinal::North => [z, y, -x],
            Cardinal::West => [-x, y, -z],
            Cardinal::South => [-z, y, x],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default-east extends +X by 1; rotating to North should put the
    /// extension at -Z (north of anchor). Other cardinals symmetric.
    #[test]
    fn cardinal_rotate_offset_extends_correctly() {
        let head = [1, 0, 0]; // bed head in default east orientation
        assert_eq!(Cardinal::East.rotate_offset(head), [1, 0, 0]);
        assert_eq!(Cardinal::North.rotate_offset(head), [0, 0, -1]);
        assert_eq!(Cardinal::West.rotate_offset(head), [-1, 0, 0]);
        assert_eq!(Cardinal::South.rotate_offset(head), [0, 0, 1]);
    }

    /// Y is preserved by all cardinal rotations.
    #[test]
    fn cardinal_rotate_offset_preserves_y() {
        for c in Cardinal::ALL {
            assert_eq!(c.rotate_offset([0, 5, 0])[1], 5);
            assert_eq!(c.rotate_offset([2, -3, 4])[1], -3);
        }
    }

    /// Two CCW steps from East lands on West.
    #[test]
    fn cardinal_rotated_steps() {
        assert_eq!(Cardinal::East.rotated(1), Cardinal::North);
        assert_eq!(Cardinal::East.rotated(2), Cardinal::West);
        assert_eq!(Cardinal::East.rotated(-1), Cardinal::South);
        assert_eq!(Cardinal::East.rotated(4), Cardinal::East);
    }

    /// Bevy yaw=0 → camera forward is -Z (north); facing-derived
    /// orientation should then place an entity extending North.
    #[test]
    fn from_yaw_facing_picks_natural_cardinal() {
        let f = std::f32::consts::FRAC_PI_2;
        assert_eq!(Cardinal::from_yaw_facing(0.0), Cardinal::North);
        assert_eq!(Cardinal::from_yaw_facing(f), Cardinal::West);
        assert_eq!(Cardinal::from_yaw_facing(-f), Cardinal::East);
        assert_eq!(
            Cardinal::from_yaw_facing(std::f32::consts::PI),
            Cardinal::South,
        );
    }

    /// AABB rotation flips min/max as expected. A bed-shaped AABB
    /// rotated North should have its X extent become -Z extent.
    #[test]
    fn entity_aabb_rotation() {
        let bed = EntityAabb {
            min: [-0.5, 0.0, -0.5],
            max: [1.5, 0.5, 0.5],
        };
        let rotated = bed.rotated(Cardinal::North);
        // Original X spanned [-0.5, 1.5]; under R_y(+90°) the X axis
        // rotates into -Z, so the new Z bounds should be [-1.5, 0.5].
        assert_eq!(rotated.min[2], -1.5);
        assert_eq!(rotated.max[2], 0.5);
        // Y is preserved.
        assert_eq!(rotated.min[1], 0.0);
        assert_eq!(rotated.max[1], 0.5);
    }
}

impl EntityAabb {
    /// Default-orientation AABB derived from a footprint. Each cell covers
    /// `[cx - 0.5, cx + 0.5] × [cy, cy + 1] × [cz - 0.5, cz + 0.5]` in model
    /// space (origin at anchor's bottom-centre). The cube-cell union is the
    /// safe fallback when a `BlockDef` doesn't declare a tighter box.
    pub fn cube_union(footprint: &[[i32; 3]]) -> Self {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for &[cx, cy, cz] in footprint {
            let lo = [cx as f32 - 0.5, cy as f32, cz as f32 - 0.5];
            let hi = [cx as f32 + 0.5, cy as f32 + 1.0, cz as f32 + 0.5];
            for axis in 0..3 {
                if lo[axis] < min[axis] {
                    min[axis] = lo[axis];
                }
                if hi[axis] > max[axis] {
                    max[axis] = hi[axis];
                }
            }
        }
        Self { min, max }
    }

    /// AABB after rotating by `orientation`. Because rotations are 90°
    /// multiples the result is still axis-aligned; we just rotate the
    /// corners and recompute min/max.
    pub fn rotated(self, orientation: Cardinal) -> Self {
        let mins = orientation.rotate_point(self.min);
        let maxs = orientation.rotate_point(self.max);
        Self {
            min: [
                mins[0].min(maxs[0]),
                mins[1].min(maxs[1]),
                mins[2].min(maxs[2]),
            ],
            max: [
                mins[0].max(maxs[0]),
                mins[1].max(maxs[1]),
                mins[2].max(maxs[2]),
            ],
        }
    }
}
