//! Block registry types — what mods register and what the engine stores.
//!
//! [`BlockId`] is the stable string identifier mods use everywhere. The
//! engine interns these to a compact numeric handle (`BlockSlot`) for the
//! wire format and per-cell chunk storage; that handle is engine-internal
//! and not visible to mods.

use serde::{Deserialize, Serialize};

use crate::textures::LayerDef;

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
    /// Optional mask+ramp layer stack composited over the base texture
    /// in the chunk fragment shader. Each entry references a previously
    /// registered mask and ramp by id; the engine resolves them to slot
    /// indices at boot. See [`LayerDef`] for the per-layer parameters
    /// and the `textures` module docs for the authoring model.
    #[serde(default)]
    pub layers: Vec<LayerDef>,
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
    /// Marks this block as an NPC-interactable: present ⇒ planners
    /// can target this block with [`PlannerGoal::Interact`], the
    /// engine paths to it (snapping to [`use_slot`] if one is
    /// declared, otherwise any standable neighbour), holds the NPC
    /// at the target for `duration_secs`, and on completion applies
    /// the optional `need_restore`. `exclusive` controls whether
    /// one NPC at a time holds a claim (beds: yes; berry baskets:
    /// no). Mods add new actions — sleep, eat, enchant, smelt — by
    /// authoring this metadata on a block; the engine doesn't carry
    /// any per-action code paths. `None` ⇒ NPCs ignore the block
    /// for planner purposes.
    #[serde(default)]
    pub interactable: Option<Interactable>,
    /// Optional dedicated "use slot" — anchor-relative pose and yaw an
    /// NPC snaps to when actively using this block, plus the set of
    /// standable cells from which the action may begin. Present ⇒ the
    /// brain paths to one of `approach`, then on arrival sets
    /// `pose.translation = anchor + Cardinal::rotate(slot.pose)` and
    /// `pose.yaw = orientation.yaw() + slot.yaw`, marks the NPC
    /// kinematic for the duration of the action, and stops trying to
    /// derive the body's position from generic standable-neighbour
    /// search. Absent ⇒ existing "stand at any standable neighbour"
    /// behaviour applies (fruit basket today). Mods opt blocks in
    /// when the visible action *requires* exact positioning (sleeping
    /// in a bed, sitting in a chair, striking at a forge); blocks
    /// that read naturally from any side leave it as `None`.
    #[serde(default)]
    pub use_slot: Option<UseSlot>,
}

/// Block-level "this is something NPCs can use" declaration. One
/// schema for every action the engine knows how to drive: eat, sleep,
/// enchant, smelt, sit. The engine never branches on the *kind* of
/// interaction — it just runs the same path-arrive-pause-restore
/// pipeline, reads this metadata to decide duration / need delta /
/// claim semantics, and trusts the mod to provide visually distinct
/// blocks + assets. Adding a new action ("read tome") is purely a
/// data change.
///
/// **Non-destructive** for now — the block stays after use. Depletion
/// / regrowth (basket emptying, scroll consumed) is a follow-up.
///
/// Pairs with [`UseSlot`] for body positioning. The two are
/// independent: an `Interactable` without a slot reads as "stand at
/// any standable neighbour and wait" (berry basket); an
/// `Interactable` with a slot reads as "snap the body onto the
/// authored pose for the duration" (bed). A pure-positional
/// interaction (sit in a chair, no need restore) sets `need_restore
/// = None` and still gets the snap + lock + duration behaviour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interactable {
    /// Optional need change applied when the interaction completes.
    /// `None` ⇒ purely positional (the NPC stood / sat for
    /// `duration_secs` and nothing else happened, which is the
    /// "sit in chair" case). `Some` ⇒ the named need is reduced by
    /// `restores`, clamped at 0. The engine validates at boot that
    /// `need_restore.need` is a registered [`NeedDef`](crate::npcs::NeedDef).
    #[serde(default)]
    pub need_restore: Option<NeedRestore>,
    /// How long the NPC stays at the block before completion fires.
    /// Engine clamps to a sane upper bound so a misbehaving mod
    /// can't park an NPC for an hour. The lower bound is 0.1 s for
    /// non-exclusive blocks (short eats look glitchy below that)
    /// and 1.0 s for exclusive blocks (sleep is intended to feel
    /// like seconds, not a teleport).
    pub duration_secs: f32,
    /// Whether the engine enforces single-user exclusivity through
    /// a per-anchor claim. `true` ⇒ a bed-style claim: one NPC at
    /// a time, contention rejected at the brain's commit step.
    /// `false` ⇒ unbounded concurrent use (food on a shelf, water
    /// at a well, a public altar). Defaults to `false` since the
    /// majority of interactables are non-exclusive.
    #[serde(default)]
    pub exclusive: bool,
}

/// Need + magnitude pair for [`Interactable::need_restore`]. Split
/// from the parent so a pure-positional interaction (no need change)
/// is just `None` instead of having to encode "ignore these values."
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NeedRestore {
    /// Id of the need this interaction reduces. Cross-validated
    /// against the registered [`NeedDef`](crate::npcs::NeedDef)s at
    /// boot. Plain `String` matches the existing `default_needs`
    /// convention on `NpcKindDef`.
    pub need: String,
    /// How much deficit is subtracted from the named need on
    /// completion. Need values run 0.0 (fully satisfied) → 1.0
    /// (critical), so a larger `restores` is a more potent
    /// interaction. Clamped at 0 after the subtraction.
    pub restores: f32,
}

/// Dedicated "use slot" for an interactable block — bed, chair, forge,
/// bicycle. Tells the engine *exactly* where an NPC's body should sit
/// while the interaction is active, instead of trying to coerce the
/// generic walk/collide pipeline into producing that position.
///
/// **Coordinates** are in default-orientation model space, with the
/// origin at the anchor cell's bottom-centre — the same frame
/// [`EntityAabb`] uses, so authors can think about slot position the
/// same way they think about bounding boxes. The engine rotates
/// `pose` and each `approach` cell by the block's stored
/// [`Cardinal`] at runtime, so a single authored slot survives all
/// four placement rotations.
///
/// **Activation flow.** When the NPC's brain commits to using this
/// block, it pathfinds to whichever cell in `approach` (rotated +
/// anchor-offset) is closest and standable in the live world. On
/// arrival the brain *snaps* the NPC's pose translation to
/// `anchor_bottom_centre + Cardinal::rotate(pose)` and pose yaw to
/// `orientation.yaw() + yaw`, and inserts a kinematic-lock so the
/// physics tick and the soft-actor-separation pass leave the body
/// alone for the duration. On goal completion / abandonment the lock
/// is removed and the NPC re-enters the normal grounded simulation.
///
/// Blocks where any-angle interaction reads correctly (a basket of
/// berries) skip this entirely; their `use_slot` stays `None` and the
/// brain falls back to the existing nearest-standable-neighbour
/// behaviour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UseSlot {
    /// Where the body's **model origin** (the rig's "feet" point —
    /// the model y=0 plane the rig was authored around) lands when
    /// the action is active, in default-orientation anchor-space.
    /// XZ origin is the anchor cell's bottom-centre. The engine
    /// internally adds the standing eye-offset when writing
    /// `pose.translation`, so authors think in concrete terms —
    /// "the body's feet/base goes here" — instead of having to add
    /// 1.62 to every Y value to compensate for the eye-position
    /// pose convention.
    ///
    /// For a 2-cell bed where the body should lie centred on the
    /// mattress, use `(1.0, 1.0, 0.0)`: mid-bed in X (between the
    /// foot cell at x=0 and the head cell at x=1), Y=1.0 (top of the
    /// 1m-tall mattress), Z=0 (centre of the bed's width).
    pub pose: [f32; 3],
    /// Body yaw while the action is active, added to the block's
    /// [`Cardinal::yaw`]. `0.0` ⇒ body faces the block's
    /// extends-direction (`Cardinal::East` default = +X). Authors tune
    /// per-rig: if the lying animation extends the body opposite to
    /// the standing-forward axis, set `yaw = π` so the head still
    /// lands at the head end of the bed.
    pub yaw: f32,
    /// Standable cells from which the NPC may begin the action, in
    /// default-orientation, anchor-relative cell coords. The engine
    /// rotates each entry by the block's [`Cardinal`] and offsets by
    /// the anchor cell, then picks the closest reachable one to path
    /// to. Authors list every cell that "faces the right side" of the
    /// block — for a bed, the three cells around the foot plus the
    /// three cells around the head, omitting the cells the bed itself
    /// occupies. A block that's reachable from any side can list all
    /// 4–8 neighbours; a block that *must* be approached from one
    /// face (a forge facing into the wall) lists just that face.
    pub approach: Vec<[i32; 3]>,
    /// Optional registered [`AnimationId`](crate::animations::AnimationId)
    /// to play while the NPC is locked to this slot. `Some` ⇒ the
    /// client crossfades to this clip when the kinematic lock comes
    /// on and back to the NPC kind's idle / walk default when the
    /// lock comes off. `None` ⇒ no override, the NPC keeps playing
    /// whatever the kind defaults pick — useful when a slot exists
    /// only for positioning (sit-down without a custom clip).
    /// Validated at boot against the [`AnimationRegistry`].
    #[serde(default)]
    pub animation: Option<String>,
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
