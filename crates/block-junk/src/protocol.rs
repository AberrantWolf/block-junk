use bevy::ecs::entity::{EntityMapper, MapEntities};
use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockId, Cardinal};
use serde::{Deserialize, Serialize};

use crate::blocks::BlockSlot;
use crate::items::ItemSlot;
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
/// world cell whose slot changed. Subscribers (room dirty-marking, drop
/// spawning, mod scripting hooks) react cell-by-cell without needing to
/// know about block-entity footprints.
///
/// `slot` is the *new* slot at this cell after the edit; `prev_slot` is
/// what occupied it before. For a place: `prev_slot == EMPTY` by
/// construction (the edit is rejected if the cell was occupied). For a
/// break: `prev_slot` is whatever was destroyed, which is what drops /
/// post-destroy effects need to look up in the registry.
#[derive(Message, Clone, Copy, Debug)]
pub struct CellEdit {
    pub world: IVec3,
    pub slot: BlockSlot,
    pub prev_slot: BlockSlot,
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

/// Marker for "this actor's body is currently being driven by a
/// dedicated use-slot, not by the physics tick." Inserted when an NPC
/// enters a goal that snaps them onto a block's [`UseSlot`] (sleeping
/// in a bed, eventually sitting in a chair, striking at a forge);
/// removed on goal exit. While present:
///
/// - The NPC physics step skips them (no gravity, no walk_step sweep,
///   so the snapped pose translation isn't pulled back to the floor or
///   nudged by the AABB sweep).
/// - The server-side soft-actor-separation pass skips them (a
///   passer-by who shoulder-bumps the bed shouldn't slide the
///   sleeping body off it).
///
/// Server-only state — not replicated. Clients infer the equivalent
/// "don't touch this body" behaviour from their existing filters
/// (the client soft-separate pass only mutates `Predicted` actors,
/// and interpolated NPCs were never client-pushable in the first
/// place). When a player ever enters a use-slot (chair, vehicle) we
/// will need to flip this to a replicated marker so the predicted
/// owner skips physics on their end too.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct KinematicLock;

/// Marker component on the server-side player-avatar entity. Replicated to
/// every client so they can render a body (or, on the owner side, attach a
/// camera). Paired with the predicted state components below. Coexists
/// with `Actor` — every Avatar is also an Actor, but not every Actor is
/// an Avatar (NPCs are Actors without `Avatar`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Avatar;

/// Server-set animation clip override for an NPC. Replicated so every
/// client renders the same clip.
///
/// `None` ⇒ no override: the client picks idle vs walk via velocity
/// hysteresis against the NPC kind's default clips. This is the
/// common case — every NPC sits in this state outside of explicit
/// stationary actions.
///
/// `Some(id)` ⇒ play this clip until cleared. Server sets this on
/// transitions into stationary states (Working, Interacting with a
/// use-slot animation) and clears it on transitions back out.
/// `id` is an [`AnimationId`](block_junk_mod_api::animations::AnimationId)
/// the client resolves through its cached registry to an
/// `AnimationNodeIndex`.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect)]
pub struct NpcAnimOverride(pub Option<String>);

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

/// What an `Actor` (player or NPC) is currently carrying. The whole
/// inventory: a single stack of one item kind, or nothing. See the
/// `ephemeral-single-stack-carry` memory for why this is intentionally
/// minimal — the constraint is the gameplay.
///
/// Invariant: `count == 0` ⇒ `item == None`. The `pickup` /
/// `drop_all` helpers maintain it; direct field writes shouldn't
/// violate it.
///
/// Server-authoritative; replicated to all clients without prediction.
/// The owner reads their own carry off their `Predicted` avatar copy,
/// HUD lag = one server round-trip. Remote players' carry isn't
/// rendered yet — float-above-head visualisation lands when NPC
/// haul-state visuals do (Phase 4).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Carrying {
    pub item: Option<ItemSlot>,
    pub count: u32,
}

impl Carrying {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.item.is_none()
    }

    /// Whether `item` can be added to this stack given the carry cap.
    /// Empty hand always accepts a single unit; a partial matching stack
    /// accepts up to `cap - count` more.
    pub fn can_accept(&self, item: ItemSlot, cap: u32) -> bool {
        if self.is_empty() {
            cap > 0
        } else {
            self.item == Some(item) && self.count < cap
        }
    }

    /// Add one unit of `item`. Returns `true` on success, `false` if the
    /// add would violate the cap or mix item types.
    pub fn pickup_one(&mut self, item: ItemSlot, cap: u32) -> bool {
        if !self.can_accept(item, cap) {
            return false;
        }
        if self.is_empty() {
            self.item = Some(item);
            self.count = 1;
        } else {
            self.count += 1;
        }
        true
    }

    /// Empty the stack. Returns what was being held (if anything) so the
    /// caller can spawn the corresponding `WorldItem`s.
    pub fn drop_all(&mut self) -> Option<(ItemSlot, u32)> {
        let item = self.item.take()?;
        let count = self.count;
        self.count = 0;
        if count == 0 { None } else { Some((item, count)) }
    }
}

/// A loose item sitting in the world — what a destroyed block leaves
/// behind, what an actor sets down when they drop their carry stack,
/// and (Phase 4) what an NPC walks past and picks up to deliver to a
/// plan. Server-authoritative entity; replicated to every client.
///
/// `item` is the registry slot (compact wire format, like `BlockSlot`
/// for chunk storage). `translation` is the entity's world position at
/// spawn — items don't move in Phase 1, so this is set once on the
/// server and never updated, but lightyear still re-syncs on initial
/// replicate and on any future server-side mutation. Yaw is omitted
/// for now (items are tumbled visually with a per-entity random offset
/// derived from spawn position; no facing direction to track).
///
/// 14 bytes on the wire (Vec3 + u16). Stacks of 5 dropped from one
/// destroyed block are 5 entities = 70 B/spawn. Profile if drop rates
/// climb; merging stacks into one entity with a count is the obvious
/// next step.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldItem {
    pub item: ItemSlot,
    pub translation: Vec3,
}

/// What a player has tagged a cell to become. Lives in the shared [`Plans`]
/// resource (server-authoritative, mirrored on each client). Tagged cells
/// aren't world state — they're work orders for NPCs to consume.
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

/// Full state of one tagged cell: what it should become *plus* the
/// progress of material delivery for Build plans. Remove plans have an
/// empty `materials` vec (nothing needs to be delivered to break a
/// block); Build plans carry one entry per item kind required, with
/// `present` rising toward `needed` as the player or NPCs deposit
/// resources.
///
/// Replicated to every client so each can render the right outline
/// colour (desaturated green when materials still pending, full green
/// when ready) and decide self-work eligibility locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanState {
    pub kind: PlanKind,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

impl PlanState {
    pub fn new(kind: PlanKind, materials: Vec<MaterialEntry>) -> Self {
        Self { kind, materials }
    }

    /// True when every material entry has its full count delivered, or
    /// when the plan needs no materials at all (every Remove plan).
    pub fn is_satisfied(&self) -> bool {
        self.materials.iter().all(|m| m.present >= m.needed)
    }

    /// How many more units of `item` can still be deposited before
    /// this plan is satisfied for that material. `0` means the plan
    /// doesn't accept this item kind (either not needed, or already
    /// fully delivered).
    pub fn remaining_for(&self, item: ItemSlot) -> u32 {
        self.materials
            .iter()
            .find(|m| m.item == item)
            .map(|m| m.needed.saturating_sub(m.present))
            .unwrap_or(0)
    }
}

/// One material requirement on a [`PlanState`]: which item, how many
/// needed in total, how many delivered so far. Capped at `needed` on
/// deposit so a deposit-too-big call doesn't overshoot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialEntry {
    pub item: ItemSlot,
    pub needed: u32,
    pub present: u32,
}

/// Client → server: tag (`kind = Some`) or untag (`kind = None`) a cell.
/// Server → client: the canonical applied edit, broadcast to everyone in
/// the world. Same bidirectional shape as [`BlockEdit`] for symmetry.
///
/// Server validation:
///   - `Some(Remove)` rejected if the cell is currently empty.
///   - `Some(Build {..})` rejected if the cell is currently solid.
///   - `None` succeeds even if no tag exists (idempotent untag).
///
/// `materials` is **server-set only**. Client requests leave it empty
/// (the field defaults via `serde(default)`); the server populates it
/// from [`BlockDef::materials`](block_junk_mod_api::blocks::BlockDef::materials)
/// on a Build tag and rebroadcasts. Subsequent deposits / fills also
/// fire `PlanEdit` broadcasts so clients see the updated `present`
/// counts in their mirrors without a separate message type.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct PlanEdit {
    pub cell: IVec3,
    pub kind: Option<PlanKind>,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

/// Server → client on connect: the current state of the [`Plans`] map.
/// Sparse — only tagged cells. Each entry carries the full PlanState
/// (kind + materials progress) so a fresh-connecting client renders
/// the right outline state immediately.
#[derive(Message, Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanFullSync {
    pub entries: Vec<(IVec3, PlanState)>,
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
///
/// `materials` is shared across the whole batch: every cell tagged
/// Build by this batch uses the same block (the request's `kind`
/// carries one slot), so the materials_needed list is uniform.
/// Server-set on broadcast, defaulted empty on client request.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct PlanEditBatch {
    pub kind: Option<PlanKind>,
    pub cells: Vec<IVec3>,
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

/// Client → server: deposit one or more units of the player's
/// [`Carrying`] stack into the Build plan at `cell`. Server reads the
/// requesting player's carry, looks up the plan's outstanding need
/// for the carried item, decrements the player's carry by that
/// amount, increments `materials.present` on the plan, and broadcasts
/// the updated state via `PlanEdit`.
///
/// Empty-handed clicks, mismatched item types, and plans that don't
/// need anything more silently no-op — same degradation pattern as
/// pickup.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub cell: IVec3,
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

/// Client → server: instantly fill the materials of the player's
/// nearest unsatisfied Build plan. Phase-4 testing prerequisite — lets
/// us verify NPC pickup of fully-materialled plans without hauling
/// each unit by hand. Server picks the nearest plan to the requesting
/// player's avatar; no-op if no pending plans exist within range.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DebugFillNearestPlan;

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

/// Client → server: pick up the loose item closest to `target` in world
/// space. The client raycast resolves which `WorldItem` is under the
/// cursor and sends its translation; the server does a fuzzy spatial
/// match (`PICKUP_MATCH_RADIUS`) to find the actual entity. Entity ids
/// don't cross the wire here — `WorldItem` doesn't carry a stable
/// network id, and a fuzzy translation match is enough since loose
/// items don't move between when the client clicks and when the
/// server receives.
///
/// Server validates: the player has carry capacity for the item kind,
/// the player isn't unreasonably far from `target`. Failure is
/// silent — the HUD just doesn't update.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PickupRequest {
    pub target: Vec3,
}

/// Client → server: drop the player's entire carry stack at their
/// feet. No payload — the server reads the player's Carrying to
/// know what to drop. No-op when the player is empty-handed.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DropRequest;

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
