use bevy::ecs::entity::{EntityMapper, MapEntities};
use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockId, Cardinal};
use serde::{Deserialize, Serialize};

use crate::blocks::BlockSlot;
use crate::voxel::EntityEntry;

pub const CHUNK_SIZE: u32 = 32;
pub const CHUNK_PADDED: u32 = CHUNK_SIZE + 2;

/// Stable identifier for a chunk in the world grid. Both client and server
/// key their `ChunkMap` by this — see the networking-design skill for why
/// we avoid `Entity` in cross-side messages.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkCoord(pub IVec3);

/// Client → server: a place-or-break request. Server → client (after
/// validation): the applied edit broadcast to everyone in AoI.
///
/// On a request:
///   - `slot != EMPTY` → place this block at `anchor`, rotated by `orientation`.
///     The server expands the def's footprint, validates every footprint
///     cell is empty (and chunks loaded), and applies atomically.
///   - `slot == EMPTY` → break. `anchor` is the cell the player clicked,
///     which may be any cell of a multi-cell entity; the server resolves
///     to the entity's anchor via the chunk sidecar before clearing.
///
/// On a broadcast:
///   - `slot != EMPTY` → a place was applied; `anchor` is authoritative
///     and `orientation` is the placed orientation.
///   - `slot == EMPTY` → a break was applied; `anchor` is the resolved
///     anchor of whatever was removed (single-cell or entity), and
///     `orientation` is the removed entity's orientation. Recipients use
///     this to rotate the def's footprint and clear all of its cells.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlockEdit {
    pub anchor: IVec3,
    pub slot: BlockSlot,
    pub orientation: Cardinal,
}

/// Server-internal local-bus event, NOT a wire message. Emitted once per
/// world cell whose slot changed. Subscribers (room dirty-marking, mod
/// scripting hooks) react cell-by-cell without needing to know about
/// block-entity footprints.
#[derive(Message, Clone, Copy, Debug)]
pub struct CellEdit {
    pub world: IVec3,
    pub slot: BlockSlot,
}

/// Server → client on connect: the slot ↔ id table the server is using.
/// Client validates against its own registry; mismatched slot/id pairs
/// indicate a divergent mod set and the connection is rejected.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct BlockManifest {
    /// Slot index = position in this Vec. Slot 0 is always `vanilla:empty`.
    pub slots: Vec<BlockId>,
}

/// Server → client only: tells a client what to put in a chunk it just
/// entered AoI of. Two payload variants — see `ChunkData`. Subsequent
/// changes arrive as `BlockEdit` broadcasts; this message fires once
/// per (chunk, client) pair on AoI entry.
///
/// Unedited chunks travel with `entities` empty — terrain has no
/// block-entities. Edited chunks ship the sidecar so anchors/ghosts
/// arrive atomically with the slot grid.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ChunkSnapshot {
    pub coord: ChunkCoord,
    pub data: ChunkData,
    #[serde(default)]
    pub entities: Vec<EntityEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChunkData {
    /// The chunk has never been edited. The client generates it locally
    /// from the deterministic terrain function (`Chunk::from_terrain`).
    /// ~13 B on the wire.
    Procedural,
    /// The chunk has been edited; the client must use these blocks rather
    /// than regenerating. ~64 KB on the wire (RLE later).
    Edited(Vec<BlockSlot>),
}

/// Client → server: where this client's avatar is and which way they're
/// facing. Translation drives AoI; yaw drives the body orientation other
/// clients see. Sent at ≈10 Hz; server tick interpolates between updates.
/// Pitch isn't included — the visible avatar is a single block-shape with
/// no separate head, so head pitch buys nothing on the wire today.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlayerPose {
    pub translation: Vec3,
    /// Body yaw in radians (rotation around +Y).
    pub yaw: f32,
}

/// Marker component on the server-side player-avatar entity. Replicated to
/// every *other* client so they get a visible body for that player. Paired
/// with `AvatarPose`, which carries the per-tick state.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar;

/// Authoritative pose of an `Avatar`, written on the server from `PlayerPose`
/// ingests and replicated out as state. Sixteen bytes (Vec3 + yaw f32) vs
/// the forty a full `Transform` would cost — rotation+scale of the full
/// transform are dead weight when all we render is a yaw-rotated cuboid.
/// Quantize to i16/u16 fixed-point if avatar bandwidth ever shows up in
/// profiles; the precision needed (~cm of position, ~tenth of a degree of
/// yaw) fits comfortably.
///
/// Registered with `.add_prediction().add_linear_interpolation()` (see
/// network.rs) so the owner's copy is predicted-with-rollback and remote
/// copies are interpolated between server samples — `Ease` below defines
/// the lerp.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarPose {
    pub translation: Vec3,
    pub yaw: f32,
}

impl Ease for AvatarPose {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        FunctionCurve::new(Interval::UNIT, move |t| {
            // Yaw lerp via shortest arc: wrap the delta to [-π, π] before
            // scaling so a yaw going from +175° to -175° interpolates the
            // 10° short way, not the 350° long way around.
            let two_pi = std::f32::consts::TAU;
            let mut d = (end.yaw - start.yaw) % two_pi;
            if d > std::f32::consts::PI {
                d -= two_pi;
            } else if d < -std::f32::consts::PI {
                d += two_pi;
            }
            AvatarPose {
                translation: Vec3::lerp(start.translation, end.translation, t),
                yaw: start.yaw + d * t,
            }
        })
    }
}

/// Server → client: this chunk has left your AoI; despawn your local copy.
/// The server may still hold its data (we don't evict the master record yet),
/// but you don't need it anymore.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ChunkUnload {
    pub coord: ChunkCoord,
}

/// Channel marker. One ordered-reliable channel for all world events
/// (BlockEdit, ChunkSnapshot, building events…). Future work may split
/// priorities; for now KISS.
pub struct WorldChannel;

/// Per-tick player input. Replicated client→server by lightyear's input
/// pipeline (`input_native`), with sequence-numbered redundancy so a
/// dropped UDP packet doesn't drop a tick of input. Both server (authority)
/// and the owning client (prediction) run the same controller against
/// these inputs in `FixedUpdate`.
///
/// Wishdir is encoded as three i8s (-1/0/+1 per axis) — fits in 3 bytes
/// where a Vec3 would take 12. Yaw is sent every tick because the avatar's
/// body orientation tracks the camera; pitch isn't here yet (no head/torso
/// split, the avatar is a single yaw-rotated cuboid).
///
/// `Default` MUST mean "no keys held, last known yaw" so missing-input
/// vs no-input stays distinguishable. The buffer treats a missing
/// per-tick input as "use the previous one"; a `Default` value means
/// "the player explicitly pressed nothing this tick."
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct PlayerInput {
    /// Per-axis -1, 0, or +1. X is strafe (right/left), Y is fly up/down,
    /// Z is forward/back. Server interprets these per `MovementMode`.
    pub wishdir: [i8; 3],
    /// Held this tick — jump in walk mode, ascend in fly mode (redundant
    /// with `wishdir.y` but kept separate so the controller can tell
    /// "jump just-pressed" from "fly-up held").
    pub jump: bool,
    /// Just-pressed this tick — server flips `MovementMode` on the avatar.
    /// Later this gets gated on creative-mode permissions.
    pub toggle_mode: bool,
    /// Camera yaw in radians (rotation around +Y). Sets the avatar's
    /// body orientation; also drives the wishdir basis on the server's
    /// controller side.
    pub yaw: f32,
}

impl MapEntities for PlayerInput {
    fn map_entities<M: EntityMapper>(&mut self, _: &mut M) {}
}

/// Game-wide schedule ordering. Plugins assign their systems to one of these
/// sets so input → simulation → re-mesh runs in one frame in the right order,
/// even across plugin boundaries.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum GameSet {
    Input,
    Simulation,
    PostSimulation,
}
