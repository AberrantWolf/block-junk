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

/// Marker component for "thing with a body that can move and interact" —
/// the shared DNA between player avatars and NPCs. Carries the same
/// physics state (`AvatarPose`, `AvatarVelocity`, `AvatarOnGround`,
/// `MovementMode`) and consumes the same `MovementIntent` regardless of
/// whether the intent comes from a connected client or a brain.
///
/// Replicated so the client side can render and (future) interact with
/// any actor uniformly. Specialised markers like `Avatar` (player) and
/// `Npc` (mob) ride alongside to disambiguate when needed — "give every
/// actor a name tag" wants `Actor`, "attach a camera to my own avatar"
/// wants `Avatar` + `Predicted`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Actor;

/// Marker component on the server-side player-avatar entity. Replicated to
/// every client so they can render a body (or, on the owner side, attach a
/// camera). Paired with the predicted state components below. Coexists
/// with `Actor` — every Avatar is also an Actor, but not every Actor is
/// an Avatar (NPCs are Actors without `Avatar`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar;

/// Coarse "what is this NPC doing right now" — derived server-side
/// from [`crate::npc::Brain::goal`] and replicated to drive client
/// animation selection. Walking is decided client-side from velocity
/// (hysteresis on motion onset/offset), so this enum only carries the
/// states the client *can't* infer from pose: stationary work-flavoured
/// actions. Consuming and Resting both render as Idle today since we
/// don't have dedicated clips for them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect)]
pub enum NpcActivity {
    #[default]
    Idle,
    Working,
    Sleeping,
}

/// Per-avatar movement mode. Server-authoritative — the server decides
/// when a creative-mode toggle is allowed; today the request is granted
/// unconditionally. Replicated + predicted so the owner client stays in
/// sync without needing to wait a round-trip after pressing F1.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect)]
pub enum MovementMode {
    /// Walking: gravity, jump, AABB collision against the world.
    #[default]
    Walk,
    /// Creative-mode flight: 6-dof, no gravity, no collision.
    Fly,
}

/// Horizontal + vertical velocity. Predicted state — needs to roll back
/// with the rest of the simulation so the owner restarts from the
/// authoritative velocity after a server correction. Not interpolated:
/// remote viewers don't need it (they render position only).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarVelocity(pub Vec3);

/// True if the controller's last sweep ended on a downward Y contact.
/// Read by the controller to gate jumps and ground friction. Predicted
/// state for the same reason as `AvatarVelocity`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct AvatarOnGround(pub bool);

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

/// What a player has tagged a cell to become. Lives in the shared [`Plans`]
/// resource (server-authoritative, mirrored on each client). Tagged cells
/// aren't world state — they're work orders for NPCs to consume in a
/// future phase. The world block at the cell is untouched until the work
/// completes (Phase 6).
///
/// `Remove` means "I want whatever is here to be gone." `Build` carries
/// the slot + orientation so an NPC working the plan knows what to
/// construct and how to rotate it. Multi-cell footprints are recorded
/// at the anchor cell only — the NPC expands the footprint at work time
/// against the live registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanKind {
    Remove,
    Build {
        slot: BlockSlot,
        orientation: Cardinal,
    },
}

/// Client → server: tag (`kind = Some`) or untag (`kind = None`) a cell.
/// Server → client: the canonical applied edit, broadcast to everyone in
/// the world. Same bidirectional shape as [`BlockEdit`] for symmetry.
///
/// Server validation:
///   - `Some(Remove)` rejected if the cell is currently empty.
///   - `Some(Build {..})` rejected if the cell is currently solid.
///   - `None` succeeds even if no tag exists (idempotent untag).
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlanEdit {
    pub cell: IVec3,
    pub kind: Option<PlanKind>,
}

/// Server → client on connect: the current state of the [`Plans`] map.
/// Sparse — only tagged cells. Tagged-add cells with stale orientations
/// from an older save are not migrated; the snapshot is whatever the
/// server resource currently holds.
#[derive(Message, Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanFullSync {
    pub entries: Vec<(IVec3, PlanKind)>,
}

/// Bulk version of [`PlanEdit`]. All cells in `cells` are tagged with
/// the same `kind` (or cleared if `kind` is `None`). Server validates
/// each cell against the same rules as `PlanEdit` and drops the ones
/// that fail — partial application is OK; the user sees the diff in
/// the broadcast that comes back. Bidirectional shape mirrors
/// [`PlanEdit`].
///
/// Plan rectangles can get large; the client caps the per-message cell
/// count at [`PLAN_EDIT_BATCH_MAX`] and splits bigger selections into
/// multiple messages.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct PlanEditBatch {
    pub kind: Option<PlanKind>,
    pub cells: Vec<IVec3>,
}

/// Max cells per [`PlanEditBatch`] message. Chosen to keep a single
/// message comfortably under the lightyear reliable-channel fragment
/// budget; 4096 IVec3 cells = ~48 KB raw. A 64×64 face drag is 4096
/// cells — at that size we split into two messages.
pub const PLAN_EDIT_BATCH_MAX: usize = 4096;

/// Channel marker. One ordered-reliable channel for all world events
/// (BlockEdit, ChunkSnapshot, building events…). Future work may split
/// priorities; for now KISS.
pub struct WorldChannel;

/// Per-tick movement intent. The unified input vocabulary for *anything*
/// with a body — players via lightyear's input pipeline (`input_native`,
/// sequence-numbered redundancy so a dropped UDP packet doesn't drop a
/// tick), NPCs via their brain writing the component directly. Both server
/// (authority) and the owning client (prediction) run the same controller
/// against this in `FixedUpdate`.
///
/// Wishdir is encoded as three i8s (-1/0/+1 per axis) — fits in 3 bytes
/// where a Vec3 would take 12. `dyaw` is the *change* in yaw since the
/// last tick (radians), not the absolute yaw — the actor's pose owns the
/// running yaw, the source (player input or brain steering) just reports
/// motion. Pitch isn't here yet (no head/torso split, the avatar is a
/// single yaw-rotated cuboid).
///
/// `Default` means "no movement, no rotation this tick." The lightyear
/// input buffer treats a missing per-tick input as "use the previous
/// one"; with delta-yaw a duplicated tick over-rotates, but the buffer
/// only duplicates when packets drop entirely, which is rare and a 10 ms
/// slice of mouse motion at the wrist isn't catastrophic.
///
/// Field set is the union of "player keys" and "NPC brain output." NPCs
/// leave `toggle_mode` alone (no fly mode for them) and use `wishdir[0]`
/// + `wishdir[2]` only — the y axis is for player fly mode.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct MovementIntent {
    /// Per-axis -1, 0, or +1. X is strafe (right/left), Y is fly up/down,
    /// Z is forward/back. Controller interprets these per `MovementMode`.
    pub wishdir: [i8; 3],
    /// Held this tick — jump in walk mode, ascend in fly mode (redundant
    /// with `wishdir.y` but kept separate so the controller can tell
    /// "jump just-pressed" from "fly-up held").
    pub jump: bool,
    /// Just-pressed this tick — server flips `MovementMode` on the actor.
    /// Player only (NPCs don't toggle fly). Later gated on creative-mode
    /// permissions.
    pub toggle_mode: bool,
    /// "Use the thing in front of me" this tick. Players will get this
    /// from a key (E or similar); NPC brains set it when their goal
    /// requires interacting with a block-entity (use bed, open door).
    /// Currently inert — the controller doesn't act on it yet.
    pub interact: bool,
    /// Yaw delta in radians since the last tick (accumulated mouse motion
    /// for players, brain steering delta for NPCs). The controller does
    /// `pose.yaw += dyaw` — pose.yaw is the truth, and a default
    /// intent naturally leaves the pose alone.
    pub dyaw: f32,
}

impl MapEntities for MovementIntent {
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

/// One in-game day in real seconds. Picked short enough that a session
/// always sees several day/night transitions, long enough that the
/// transition itself doesn't feel like a flicker. Tune to taste.
pub const DAY_LENGTH_SECS: f32 = 600.0;

/// Server-authoritative day clock. `time_of_day` is the fraction of the
/// current day elapsed: 0.0 = midnight, 0.25 = sunrise, 0.5 = noon,
/// 0.75 = sunset. `day` counts completed days since session start.
/// Lives as a `Resource` on both sides — server ticks it forward,
/// client snaps it from `WorldClockSync` messages and locally
/// extrapolates between syncs so the sun doesn't visibly tick once
/// a second. Persisted in the save file (`SaveFile::world_clock`)
/// so a reload picks up where the world left off.
#[derive(Resource, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorldClock {
    pub day: u32,
    pub time_of_day: f32,
}

impl WorldClock {
    /// True during the half of the day when the sun is below the horizon.
    /// Mirrors the sun-rotation math in the client lighting system.
    pub fn is_night(self) -> bool {
        // Sun-up window is sunrise (0.25) → sunset (0.75). Anything else
        // is night. Phrasing in terms of "below 0.25 or above 0.75" keeps
        // the planner snapshot and the visuals reading the same truth.
        self.time_of_day < 0.25 || self.time_of_day >= 0.75
    }

    /// Advance the clock by `dt` real-time seconds, scaled by
    /// `DAY_LENGTH_SECS`. Server uses this every fixed tick; client uses
    /// it during render frames to extrapolate between sync messages.
    pub fn advance(&mut self, dt: f32) {
        self.time_of_day += dt / DAY_LENGTH_SECS;
        while self.time_of_day >= 1.0 {
            self.time_of_day -= 1.0;
            self.day = self.day.wrapping_add(1);
        }
    }
}

/// Server → client periodic sync of the world clock. Tiny (5 bytes)
/// and sent at low cadence — the client extrapolates locally between
/// messages. Lives on `WorldChannel` so it benefits from ordered-
/// reliable delivery (we don't want clocks to skip backwards from
/// out-of-order delivery; ordering pins each sync as monotonically
/// progressing).
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorldClockSync {
    pub day: u32,
    pub time_of_day: f32,
}

/// Client → server: debug request to fast-forward the world by
/// `secs` real-time seconds. Intended for the dev debug panel only;
/// no permission check today (see the wider note on debug messages).
///
/// "Fast-forward" means two things atomically: the [`WorldClock`]
/// rolls forward by `secs / DAY_LENGTH_SECS` (wrapping into the
/// next day), and every NPC's needs decay by `decay_per_sec * secs`
/// — i.e. the world experiences `secs` of time without the player
/// having to wait. Negative values are clamped to 0 (going backward
/// is weird — the existing-deficits don't ungrow; if you want to
/// rewind the clock, just keep advancing through the next cycle).
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DebugAdvanceTime {
    pub secs: f32,
}

/// Client → server: debug request to bump every NPC's value for the
/// named need by `delta` (typically positive — "make everyone more
/// tired" or "more hungry" to trigger behaviour without waiting
/// minutes for natural decay). Server clamps the resulting per-NPC
/// value to [0, 1] so a runaway delta can't break the math, and
/// silently ignores `need` ids the registry doesn't know about
/// (e.g. a typo from the UI).
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct DebugBumpNeed {
    pub need: String,
    pub delta: f32,
}

/// Client → server: ask the server to dump the current authoritative
/// state of one NPC so the requesting client's inspection panel can
/// show needs, current goal, and goal target. `npc_id` mirrors
/// [`crate::npc::NpcId`]'s inner u64 — kept as a plain u64 in the
/// protocol so `protocol.rs` doesn't have to import `npc.rs` and
/// invert the existing dependency.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RequestNpcDetails {
    pub npc_id: u64,
}

/// Server → client: targeted reply to a [`RequestNpcDetails`]. Sent on
/// `WorldChannel` to the requesting connection only — clients don't
/// see each other's inspection traffic. `current_goal` is a
/// pre-formatted human string ("sleeping (12.4s)", "moving to 14
/// cells, on_arrive: work") so the client UI doesn't need to mirror
/// the engine's full Goal enum to render it.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct NpcDetails {
    pub npc_id: u64,
    pub kind: String,
    pub needs: std::collections::HashMap<String, f32>,
    pub current_goal: String,
    pub target_cell: Option<IVec3>,
}
