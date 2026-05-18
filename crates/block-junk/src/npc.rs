//! NPCs — server-authoritative actors driven by a two-layer brain.
//!
//! **Layer 1 (this file, native Rust)** runs every fixed tick: decay
//! needs, advance the current goal, write a [`MovementIntent`], step
//! physics through the same `apply_walk_step` players use.
//!
//! **Layer 2 (Lua planner)** runs only when the engine asks: when an
//! NPC's goal completes (the brain enters [`Goal::Idle`]). The planner
//! is a mod-registered callback keyed by [`NpcKind`]; the engine sends
//! it an [`NpcSnapshot`] and the planner returns a [`PlannerGoal`] that
//! the engine knows how to execute. Planners can choose between Wander,
//! Rest, or Idle (defer to the next tick) but cannot invent new actions
//! without engine support.
//!
//! **Per-NPC error isolation**: if the planner errors for one NPC, the
//! engine attaches a [`BrainDisabled`] marker to that single entity and
//! keeps running every other NPC + mod. This is stricter than the
//! whole-mod disable used for declarative hooks — a buggy planner can
//! reasonably be called many times before being trusted again, and we
//! don't want one bad NPC kind to silence its entire mod.
//!
//! **Native fallback**: if no mod registered a planner for a kind, the
//! engine drives it with the same Wander loop the project ran before
//! the planner surface landed. Lets the engine boot + smoke-test even
//! when no mods load.

use bevy::prelude::*;
use block_junk_mod_api::blocks::{Cardinal, Interactable, NeedRestore, UseSlot};
use block_junk_mod_api::npcs::{
    NearbyInteraction, NearbyPlan, NearbyRoom, NpcKindId, NpcSnapshot, PendingAssignment,
    PlanKindHint, PlannerGoal,
};
use block_junk_mod_api::shared::BlockPos;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blocks::BlockRegistry;
use crate::collision::WorldCollision;
use crate::haul::{HaulAssignments, WorldItemReservations, release_haul_for};
use crate::interactables::{InteractableIndex, InteractionClaims};
use crate::items::ItemSlot;
use crate::npc_registry::{NeedRegistry, NpcKindRegistry, WorkDefaultsRes};
use crate::pathfinding::{Walkability, find_path, nearest_standable_below, smooth_path, standable};
use crate::rooms::RoomMap;
use crate::physics::{
    EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step, standing_pose_translation,
};
use crate::plan_claims::PlanClaims;
use crate::plans::Plans;
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, Carrying, KinematicLock, MovementIntent,
    MovementMode, NpcAnimOverride, PlanEdit, PlanKind, WorldChannel, WorldClock, WorldItem,
};
use crate::scripting::ServerMods;
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind, world_to_chunk};

/// Replicated marker — "this entity is an NPC, not a player avatar."
/// Lets clients render it differently and lets server systems narrow
/// queries to AI-controlled actors. Sibling to [`crate::protocol::Avatar`];
/// both ride alongside the shared [`Actor`] marker.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Npc;

/// Replicated debug aid: the NPC's current A* path as a sequence of
/// foot cells. Empty while Idle. Updated only on goal transitions
/// (entering a new Wander, falling back to Idle), so replication churn
/// is one packet per planner decision, not per tick. Clients render
/// this with gizmos when the debug-path overlay is enabled.
///
/// Will probably move behind a per-client opt-in / dev-build feature
/// once there are dozens of NPCs and the bandwidth matters.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NpcPath(pub Vec<IVec3>);

/// Stable identifier for an NPC across save/load. Distinct from Bevy
/// `Entity` because Entity values aren't preserved across reboots.
/// Allocated server-side from a monotonic counter and exposed to mods
/// in [`NpcSnapshot::id`]. Replicated so the client can refer to a
/// specific NPC across the wire — needed for inspection requests
/// (the client raycasts an entity, looks up the NpcId, and sends it
/// to the server in a RequestNpcDetails).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NpcId(pub u64);

/// Mod-namespaced kind, e.g. `vanilla:wanderer`. Selects which planner
/// the engine calls on goal completion and which need table the spawn
/// path initialises from the [`NpcKindRegistry`]. Replicated to every
/// client so the client-side animation driver can look up the kind's
/// default idle / walk / work clips in its local [`AnimationRegistry`].
#[derive(
    Component, Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Reflect,
)]
pub struct NpcKind(pub String);

/// Floating-point need state. 0.0 = fully satisfied, 1.0 = critical.
/// Keyed by need id (matches the [`NeedDef`] declared by mods); the
/// engine never reads any individual need by name — it just decays every
/// entry by the registry-supplied rate and hands the full table to the
/// planner.
///
/// Per the design memo, needs are registered in the `vanilla` mod, not
/// the engine, so the engine carries no knowledge of "hunger" vs
/// "sleep." A kind that hasn't subscribed to any needs has an empty map
/// and decays nothing — the native-fallback smoke-test NPC works fine
/// in that state.
#[derive(Component, Clone, Debug, Default)]
pub struct Needs(pub HashMap<String, f32>);

/// Per-NPC marker indicating its planner has errored and shouldn't run
/// again this session. The brain tick filters this out via
/// `Without<BrainDisabled>` so the entity still exists (renderable,
/// physics still steps if we ever add it back), but no new goals are
/// chosen and the existing intent is the empty default — the NPC stands
/// still.
///
/// Distinct from the whole-mod disable applied to declarative hooks:
/// one bad NPC kind shouldn't silence its entire mod, and a buggy
/// planner that errors per-NPC will accumulate disabled NPCs visibly,
/// each one logged on the way out.
#[derive(Component, Clone, Debug)]
#[allow(dead_code, reason = "field is read by debug HUD (future) and shows up in logs today")]
pub struct BrainDisabled {
    pub reason: String,
}

/// Current goal the brain is executing. Variants combine the abstract
/// planner-supplied action with the engine-only bookkeeping needed to
/// drive it (current path + progress, remaining timer, stuck detector).
///
/// `MoveTo` is the single path-following primitive — Wander, Goto, and
/// Consume from [`PlannerGoal`] all reduce to "walk along this path,
/// then optionally do something on arrival." Multiple path-driven
/// planner actions share one variant rather than each adding a parallel
/// pile of (path, progress, deadline, stuck) fields.
#[derive(Clone, Debug)]
pub enum Goal {
    /// No active goal. Entering this state triggers a planner call on
    /// the next brain tick; newly-spawned NPCs start here, and any
    /// completed action drops back here so the planner picks what's
    /// next.
    Idle,
    /// Walk a precomputed A* path of foot cells using pure-pursuit
    /// steering, then on successful arrival run `on_arrive`. `progress`
    /// is the NPC's monotonically-increasing arc length along the path
    /// — recomputed each tick by projecting the pose onto the path
    /// (never backwards), then offset by `LOOKAHEAD_DIST` to produce
    /// the actual aim point. Steering toward an always-ahead carrot
    /// (vs. the current waypoint) stops the NPC from circling a point
    /// it's trying to reach faster than its turn rate allows. `last_pos`
    /// + `stuck_secs` detect a path that's become impossible (player
    /// dug in front of us, NPC wedged on a corner) and force a replan
    /// via abandonment. Abandonment skips `on_arrive` — only a clean
    /// arrival fires it.
    MoveTo {
        path: Vec<IVec3>,
        progress: f32,
        deadline_secs: f32,
        last_pos: Vec3,
        stuck_secs: f32,
        on_arrive: ArrivalAction,
        /// Optional snap-on-arrival. Independent of [`ArrivalAction`]:
        /// any goal whose destination block carries a
        /// [`UseSlot`](block_junk_mod_api::blocks::UseSlot) populates
        /// this with the pre-computed world-space pose. On arrival the
        /// engine teleports the body onto that pose, sets pose.yaw to
        /// the slot's stored yaw, and inserts [`KinematicLock`] so the
        /// physics tick + soft-actor-separation pass leave the body
        /// alone for the duration of the follow-on action. `None`
        /// means "no special positioning" — the NPC lands wherever the
        /// path's last cell led them, and any action-specific behaviour
        /// (yaw aiming, etc.) takes over from there. The planner
        /// resolves slot data once at goal commit so arrival doesn't
        /// need to re-read the block def.
        snap: Option<UseSlotSnap>,
    },
    /// Stand still for a while. Duration is whatever the planner
    /// returned in [`PlannerGoal::Rest`], clamped to
    /// `[MIN_REST_SECS, MAX_REST_SECS]` so a misbehaving mod can't
    /// freeze an NPC indefinitely or churn the planner at 60 Hz.
    Resting { remaining_secs: f32 },
    /// Standing at an interactable cell, counting down to completion.
    /// One state covers every variant the engine used to have a
    /// separate Goal for (Consuming, Sleeping, and the future
    /// Enchanting / Smelting / etc.) — the block def's
    /// [`Interactable`](block_junk_mod_api::blocks::Interactable)
    /// supplies the action-specific tuning (need to decrement,
    /// duration, exclusivity).
    ///
    /// Entered only via a successful arrival on a `MoveTo` with
    /// `ArrivalAction::Interact`. `need_restore` is captured at goal
    /// creation so a planner that mid-action decides "actually you
    /// should be eating *that* food" can't retroactively change what
    /// the NPC is doing now — the captured snapshot wins. `target_cell`
    /// is the interactable block itself (not the stand cell); the
    /// brain uses it to rotate the body toward whatever the NPC is
    /// interacting with for non-snap interactions. `anchor_cell` is
    /// the claim key for `exclusive` interactables — released on any
    /// path out of [`Goal::Interacting`] including stuck-abandon or
    /// planner override; ignored when the block is non-exclusive.
    Interacting {
        remaining_secs: f32,
        need_restore: Option<NeedRestore>,
        target_cell: IVec3,
        anchor_cell: IVec3,
        exclusive: bool,
        /// Animation override captured from the block's
        /// [`UseSlot::animation`](block_junk_mod_api::blocks::UseSlot::animation)
        /// at goal-commit time. `Some` when the slot author named a
        /// clip — the per-tick activity refresh writes it through to
        /// [`NpcAnimOverride`](crate::protocol::NpcAnimOverride). `None`
        /// when the slot didn't override (or the block had no slot
        /// at all) — animation falls back to the kind defaults
        /// (idle / walk) via the client's velocity hysteresis.
        animation: Option<String>,
    },
    /// Working a player-tagged plan at `target_cell` until the timer
    /// expires, at which point the engine applies the world mutation
    /// captured in `plan_kind`, clears the tag, releases the claim,
    /// and the brain applies the captured `need_restore` (if any).
    /// Entered only via a successful arrival on a [`ArrivalAction::Work`].
    ///
    /// `plan_kind` is snapshot-at-goal-commit-time so a player who
    /// re-tags the cell mid-traversal can't redirect what the NPC
    /// builds — they get to cancel the plan, but not silently swap it.
    /// `need_restore` is likewise captured at commit — the block
    /// being placed (Build) or removed (Remove) at *that* moment
    /// determined the payoff; the mid-action picture doesn't matter.
    Working {
        remaining_secs: f32,
        target_cell: IVec3,
        plan_kind: PlanKind,
        need_restore: Option<NeedRestore>,
    },
}

/// Pre-computed pose-snap for a
/// [`UseSlot`](block_junk_mod_api::blocks::UseSlot) interaction. Built
/// once at goal-commit (when the brain knows the anchor + orientation
/// + slot data) and carried on [`Goal::MoveTo`] so the arrival handler
/// doesn't need to re-resolve the block def. Action-agnostic: any
/// goal that lands the NPC at a slot-bearing block populates this the
/// same way, the arrival applies it uniformly, and the follow-on
/// [`ArrivalAction`] decides what the NPC *does* once snapped (sleep,
/// consume, work).
///
/// `translation` is world-space (anchor cell origin + rotated slot
/// pose); `yaw` is body yaw in radians (same convention pose.yaw uses,
/// already including the block's [`Cardinal::yaw`](block_junk_mod_api::blocks::Cardinal::yaw) + the slot's
/// authored yaw offset).
#[derive(Clone, Copy, Debug)]
pub struct UseSlotSnap {
    pub translation: Vec3,
    pub yaw: f32,
}

/// What the engine does after the NPC arrives at the end of a
/// [`Goal::MoveTo`] path. `None` means "just stop, drop to Idle, let
/// the planner pick the next thing." `Consume` triggers a transition
/// into [`Goal::Consuming`] which applies the need restoration on
/// completion.
///
/// Extending this enum is how we add new arrival-side primitives
/// (sleep on a bed, work at a workbench, etc.) — each gets its own
/// follow-on `Goal` variant the same way Consume does.
#[derive(Clone, Debug)]
pub enum ArrivalAction {
    /// Just stop on arrival. Used by `PlannerGoal::Wander` and
    /// `PlannerGoal::Goto`, which describe motion without a follow-on.
    None,
    /// Begin a stand-still interaction at the target block. Captured
    /// values mirror the block's
    /// [`Interactable`](block_junk_mod_api::blocks::Interactable)
    /// metadata at goal-commit time. `anchor_cell` is the claim key
    /// the brain reserved for `exclusive` blocks (ignored when not
    /// exclusive); the brain must release the claim on any path out
    /// of the resulting [`Goal::Interacting`]. If the block has been
    /// broken, replaced, or claim-stolen by arrival, the action
    /// degrades silently to "stand briefly, then idle."
    Interact {
        need_restore: Option<NeedRestore>,
        duration_secs: f32,
        target_cell: IVec3,
        anchor_cell: IVec3,
        exclusive: bool,
        /// Carries the slot's animation override through to the
        /// `Goal::Interacting` that the arrival transition creates.
        /// See [`Goal::Interacting::animation`].
        animation: Option<String>,
    },
    /// Begin a work action at the plan target cell. Carries the snapshot
    /// of the `PlanKind` at the moment the goal was committed so a
    /// mid-traversal tag swap (player edits the tag while NPC is en
    /// route) doesn't redirect the work — the player gets to cancel
    /// but not silently re-aim. `need_restore` and `duration_secs`
    /// were resolved from the target block's
    /// [`WorkAction`](block_junk_mod_api::blocks::WorkAction) (or the
    /// engine-wide [`WorkDefaults`](block_junk_mod_api::npcs::WorkDefaults))
    /// at goal commit; arrival doesn't re-read the block def.
    Work {
        duration_secs: f32,
        target_cell: IVec3,
        plan_kind: PlanKind,
        need_restore: Option<NeedRestore>,
    },
    /// One leg of a haul cycle: arrive at a `WorldItem`, pick it up,
    /// then either keep collecting or walk to the plan to deposit.
    /// `item_entity` is the specific loose item the scheduler reserved
    /// for this NPC; `item_slot` is cached so the brain can validate
    /// the item didn't change kinds between reservation and arrival
    /// (e.g. it was picked up by a player and another loose item
    /// drifted into the slot). `plan_cell` is the eventual delivery
    /// target — needed at arrival time so the brain can plan the next
    /// leg without consulting [`HaulAssignments`] (it consults it too,
    /// but the cached plan cell lets us short-circuit when the
    /// assignment is gone).
    ///
    /// If on arrival the item entity is missing, the WorldItem kind
    /// no longer matches, or the NPC's carry can't accept it, the
    /// haul is released and the NPC drops back to Idle for the
    /// scheduler to reassign.
    PickupForPlan {
        item_entity: Entity,
        item_slot: ItemSlot,
        plan_cell: IVec3,
    },
    /// Final leg of a haul cycle: arrive at the plan and deposit the
    /// NPC's full carry stack into the plan's materials. Reads the
    /// NPC's [`Carrying`](crate::protocol::Carrying), calls
    /// [`Plans::deposit`], broadcasts a [`PlanEdit`] so client
    /// mirrors update, and clears the carry.
    ///
    /// If on arrival the plan is gone or no longer a Build plan, the
    /// haul releases without depositing (carry stays on the NPC; the
    /// scheduler will pick it up or, in degenerate cases, the carry
    /// just sits there until something else does — there's no
    /// auto-drop, since spilling a NPC's stack mid-air would be
    /// surprising).
    DepositAtPlan {
        plan_cell: IVec3,
    },
}

/// Native-side brain state. Holds the current goal + a tiny PRNG seed
/// for reproducible target selection.
#[derive(Component, Clone, Debug)]
pub struct Brain {
    pub goal: Goal,
    /// splitmix-seeded PRNG state. Per-NPC so two NPCs spawned the same
    /// tick don't pick identical wander targets.
    pub rng: u64,
}

/// Default wander radius for the native fallback path (no planner
/// registered for this NPC's kind). The Lua planner provides its own
/// radius and may pick a larger one.
const FALLBACK_WANDER_RADIUS_CELLS: i32 = 12;
const FALLBACK_WANDER_TIMEOUT_SECS: f32 = 12.0;
/// Bounds on planner-supplied goal parameters. A buggy planner that
/// returns absurd numbers can't park an NPC for an hour or send the
/// pathfinder on a multi-chunk search — values are clamped at the
/// engine boundary before being committed to the live goal.
const MAX_WANDER_RADIUS_CELLS: i32 = 64;
const MAX_WANDER_TIMEOUT_SECS: f32 = 60.0;
const MAX_GOTO_TIMEOUT_SECS: f32 = 120.0;
/// Max walk-timeout the brain accepts for an Interact goal. Same
/// magnitude as the goto/work timeouts — past two minutes, an NPC
/// trying to reach an interactable should abandon and let the
/// planner pick again rather than chase it forever.
const MAX_INTERACT_TIMEOUT_SECS: f32 = 120.0;
const MIN_REST_SECS: f32 = 0.5;
const MAX_REST_SECS: f32 = 60.0;
/// Interaction duration is read from the block's
/// [`Interactable::duration_secs`](block_junk_mod_api::blocks::Interactable::duration_secs).
/// The brain clamps to `[0.1, 120.0]` so a typo or a misbehaving mod
/// can't park an NPC for an hour or strobe through the action in
/// one tick. The registry validator enforces the lower bound at
/// boot too (≥ 1.0 for exclusive blocks, ≥ 0.1 otherwise).
const MIN_INTERACT_DURATION_SECS: f32 = 0.1;
const MAX_INTERACT_DURATION_SECS: f32 = 120.0;
/// Plan-pickup scan radius (Manhattan via the Chebyshev pre-filter).
/// Same magnitude as sleepers/consumables — plans on the far side of
/// the map shouldn't keep luring a villager from local tasks.
const SNAPSHOT_PLAN_RADIUS_CELLS: i32 = 48;
const SNAPSHOT_PLAN_LIMIT: usize = 8;
/// Maximum walk-deadline for a WorkPlan goal. Same magnitude as the
/// other A*-driven goals — past two minutes the NPC abandons and lets
/// the planner pick again. The actual *work* duration (how long the
/// NPC stands at the cell) is per-block via
/// [`BlockDef::work_action`](block_junk_mod_api::blocks::BlockDef::work_action)
/// with [`WorkDefaults`](block_junk_mod_api::npcs::WorkDefaults) as the
/// fallback — no engine constant needed.
const MAX_WORK_TIMEOUT_SECS: f32 = 120.0;
/// How many nearby matched rooms to include in each planner snapshot.
/// Cap exists so a world with hundreds of registered rooms doesn't
/// blow up the per-call serialization cost; 8 is enough headroom for
/// a planner to pick between "nearest of each kind" without flooding
/// the table.
const SNAPSHOT_ROOM_LIMIT: usize = 8;
/// Same idea as `SNAPSHOT_ROOM_LIMIT` for the unified interactions
/// array. 8 is plenty for "nearest of each need" picks in early-game;
/// the planner only sees the closest entries so a player who places
/// hundreds of food blocks or beds doesn't blow up the per-call cost.
const SNAPSHOT_INTERACTION_LIMIT: usize = 8;
/// Chebyshev radius the snapshot builder scans for interactables.
/// Past this the NPC won't see a food block or a bed at all — it'll
/// wander toward rooms or random targets until it bumps into one.
/// 48 cells ≈ 3 chunks at CHUNK_SIZE = 16, big enough to cover a
/// small settlement.
const SNAPSHOT_INTERACTION_RADIUS_CELLS: i32 = 48;
/// How far ahead of the closest-projection point on the path the NPC
/// aims. Bigger ⇒ smoother turns, more corner-cutting; smaller ⇒
/// hugs the path tighter, may oscillate. 0.5 m ≈ half a cell — the
/// NPC always steers toward something half a step further along the
/// path than where they actually are.
const LOOKAHEAD_DIST: f32 = 0.5;
/// Distance (XZ) from the NPC to the path's final waypoint that
/// counts as "arrived." Must be wider than the NPC's turn radius
/// (v/ω ≈ 0.83 m at 5 m/s walk / 6 rad/s turn) — otherwise the NPC
/// can orbit the final waypoint forever, unable to tighten enough
/// to land inside the radius. 1.2 m gives some headroom over that
/// while still feeling like "arrived at the spot."
const PATH_ARRIVE_RADIUS: f32 = 1.2;
/// How far ahead of `progress` we look for a step-up before holding
/// the jump button. The walk-controller only converts `jump=true`
/// into a jump impulse on the rising edge of `on_ground`, so holding
/// across the approach is harmless and lets the impulse fire the
/// frame the NPC's foot reaches the new floor (vs. requiring exact
/// timing). 1 m ≈ one cell of approach — long enough that the
/// vertical impulse has time to clear the obstacle horizontally.
const JUMP_TRIGGER_DIST: f32 = 1.0;
/// Movement under this distance/tick counts as "not moving." At 60 Hz
/// the walk speed of 5 m/s produces ~0.083 m/tick, so 0.02 m only
/// triggers when the NPC genuinely isn't progressing.
const STUCK_MOVE_THRESHOLD: f32 = 0.02;
/// After this much continuous time without movement, abandon the goal
/// and let Idle pick a new path. Long enough to absorb the half-second
/// it takes to turn 180° but short enough that wedged NPCs unwedge
/// before the player notices.
const STUCK_REPLAN_SECS: f32 = 1.5;

/// Maximum yaw rotation per tick for NPC steering, radians/sec. ~344°/s
/// — fast enough that the body doesn't lag visibly behind the chosen
/// direction, slow enough that you can see the turn.
const NPC_TURN_RATE: f32 = 6.0;

/// How many wander-target attempts the planner makes per Idle-resolve
/// before giving up for this tick. Some attempts will hit unloaded
/// chunks (`is_solid → true`) or unreachable regions; one retry per
/// tick is too few, ten is wasteful.
const MAX_WANDER_ATTEMPTS: usize = 6;
/// How far below the NPC's height we look for the ground at a candidate
/// XZ. Enough to cover one chunk's vertical span.
const WANDER_DROP_BUDGET: i32 = 16;

/// A* budgets. `NODE_BUDGET` is the hard CPU ceiling for a single
/// search — at ~2000 nodes per call the worst case is a single-digit
/// millisecond hitch, and the wander layer retries next tick anyway.
/// `PATH_BUDGET` is the allowed g-score (≈ step count for unit costs);
/// 64 covers a meaningful radius without letting one NPC spend its
/// whole budget on a 200-step trek.
const ASTAR_NODE_BUDGET: usize = 2000;
const ASTAR_PATH_BUDGET: usize = 64;

/// Chebyshev search radius used by [`rescue_to_nearby_standable`]
/// when the brain detects an NPC whose pose isn't standable at
/// planner entry. 2 cells covers "fell one cell off a ledge" and
/// "slid against a wall onto an unsupported corner" without giving
/// the rescue licence to teleport the NPC across the room — a wider
/// radius would hide the underlying bug instead of surfacing it.
const RESCUE_RADIUS_CELLS: i32 = 2;

pub struct NpcServerPlugin;

impl Plugin for NpcServerPlugin {
    fn build(&self, app: &mut App) {
        // Spawn deferred to first client connect rather than Startup —
        // chunks aren't loaded until a client's AoI requests them, and
        // an NPC spawned into an empty world falls past unloaded chunks
        // forever (no candidates to collide against).
        app.add_observer(spawn_initial_npc_on_first_connect);
        // Local-bus message: brain emits on Working timer completion;
        // server-side consumer (in server.rs) applies the underlying
        // BlockEdit + clears the plan tag. Splits these concerns so the
        // brain tick stays under the SystemParam cap.
        app.add_message::<NpcWorkCompleted>();
        // Brain → physics order matters: physics consumes the intent
        // the brain writes this tick. Both run in FixedUpdate alongside
        // the player physics so all actors advance together.
        app.add_systems(FixedUpdate, (npc_brain_tick, npc_physics_step).chain());
        // Activity is derived from Goal and replicated to drive client
        // animation. Updates after the brain tick so the broadcast
        // reflects the just-decided goal.
        app.add_systems(FixedUpdate, refresh_npc_activity.after(npc_brain_tick));
    }
}

/// Map [`Brain::goal`] onto the replicated [`NpcAnimOverride`]. The
/// client uses this to pick a clip; when the override is `None`, the
/// client falls back to velocity-based idle/walk hysteresis against
/// the NPC kind's defaults.
///
/// - `Goal::Interacting` with a slot-supplied animation ⇒ override
///   to that clip (sleep in the bed → "vanilla:lie_idle"; sit in the
///   chair → "mymod:sit_idle"; etc.).
/// - `Goal::Working` ⇒ override to the NPC kind's `animations.work`.
/// - Everything else ⇒ clear the override.
///
/// `set_if_neq` keeps the replication channel quiet between goal
/// transitions; the override doesn't change every tick.
fn refresh_npc_activity(
    kinds: Res<NpcKindRegistry>,
    mut npcs: Query<(&Brain, &NpcKind, &mut NpcAnimOverride), With<Npc>>,
) {
    for (brain, kind, mut override_) in npcs.iter_mut() {
        let next = match &brain.goal {
            Goal::Interacting { animation, .. } => NpcAnimOverride(animation.clone()),
            Goal::Working { .. } => NpcAnimOverride(
                kinds.get(&kind.0).map(|k| k.animations.work.clone()),
            ),
            _ => NpcAnimOverride(None),
        };
        override_.set_if_neq(next);
    }
}

/// Brain → server-bus message. Emitted in `npc_brain_tick` when a
/// `Goal::Working` timer expires; consumed in `server::apply_npc_work`
/// which translates `plan_kind` into the matching `BlockEdit` and
/// runs it through `apply_block_edit` so the world mutation, the
/// broadcast, and the plan-tag auto-clear all happen through the
/// same code path that handles player edits.
#[derive(Message, Clone, Copy, Debug)]
pub struct NpcWorkCompleted {
    pub cell: IVec3,
    pub plan_kind: PlanKind,
}

/// Latches on the first `Connected` so subsequent reconnects don't
/// re-spawn the smoke-test cluster. `AtomicBool` rather than
/// `Local<bool>` because observers don't take `Local`.
static SMOKE_TEST_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Smoke-test cluster — a small ring of NPCs near the player's default
/// landing spot (player spawn = (0, 32, 60)). Each is offset by a few
/// metres so they don't all stack on one cell and so a player can
/// visibly tell them apart at a glance.
///
/// The cluster is small on purpose: planner state per NPC is keyed by
/// `NpcId` in Lua, so a multi-NPC smoke test is what validates that
/// per-id state actually isolates. One NPC can't reveal the bug
/// "everyone shares the same alternation state."
///
/// Replicated to all clients with interpolation (no client predicts
/// NPCs — there's no per-client "owner" of one).
///
/// Default needs come from the [`NpcKindRegistry`] entry for
/// `vanilla:wanderer`; if no mod registered that kind we still spawn
/// (with an empty need map) and let the native fallback drive the
/// brain. That's what the design memo's "trivial native NPC for engine
/// smoke tests" is — same entity, the brain just falls back to native
/// logic when there's no Lua planner.
fn spawn_initial_npc_on_first_connect(
    _: On<Add, Connected>,
    mut commands: Commands,
    kinds: Res<NpcKindRegistry>,
    existing: Query<(), With<Npc>>,
) {
    if SMOKE_TEST_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    // If a save was loaded at startup, NPCs already exist with their
    // persisted ids — spawning the smoke-test cluster on top would
    // duplicate ids (the cluster hardcodes 1..=4 and a freshly-saved
    // world has those same ids). Skip silently; the atomic above still
    // latches so subsequent reconnects don't re-attempt.
    if !existing.is_empty() {
        info!("NPCs already present (loaded from save); skipping smoke-test cluster spawn");
        return;
    }
    let kind_id = "vanilla:wanderer";
    let default_needs = match kinds.get(kind_id) {
        Some(def) => def.default_needs.clone(),
        None => {
            warn!(
                kind = kind_id,
                "no NPC kind registered; spawning with empty needs (native fallback brain)"
            );
            HashMap::new()
        }
    };
    // Offset positions east + south of the player spawn (0, 32, 60).
    // Y picks one cell above so the controller settles them onto the
    // floor on the first physics step. Small XZ spread keeps them all
    // visible in the player's initial frame without overlapping.
    let cluster = [
        Vec3::new(4.0, 32.0, 60.0),
        Vec3::new(6.0, 32.0, 62.0),
        Vec3::new(2.0, 32.0, 62.0),
        Vec3::new(4.0, 32.0, 64.0),
    ];
    for (i, translation) in cluster.into_iter().enumerate() {
        let id: u64 = (i + 1) as u64;
        // Nested tuples work around Bevy's 15-element Bundle cap. Two
        // groups: identity/brain (cheap markers + structured state) and
        // physics + replication (per-frame state + lightyear).
        commands.spawn((
            (
                Actor,
                Npc,
                NpcId(id),
                NpcKind(kind_id.into()),
                Needs(default_needs.clone()),
                Brain {
                    goal: Goal::Idle,
                    rng: 0xDEAD_BEEF_CAFE_F00D ^ id,
                },
                crate::protocol::Carrying::default(),
            ),
            AvatarPose {
                translation,
                yaw: 0.0,
            },
            AvatarVelocity::default(),
            AvatarOnGround::default(),
            MovementMode::Walk,
            MovementIntent::default(),
            NpcPath::default(),
            NpcAnimOverride::default(),
            Replicate::to_clients(NetworkTarget::All),
            InterpolationTarget::to_clients(NetworkTarget::All),
            Name::new(format!("npc:{id}")),
        ));
    }
    info!(kind = kind_id, count = cluster.len(), "spawned smoke-test NPC cluster");
}

/// Adapter that lets pathfinding query the live world. Treats unloaded
/// chunks as solid so the search doesn't commit to a path through
/// territory whose contents we don't know.
struct WorldWalk<'q, 'w, 's> {
    chunks: &'q Query<'w, 's, &'static Chunk>,
    chunk_map: &'q ChunkMap,
    registry: &'q BlockRegistry,
}

impl<'q, 'w, 's> Walkability for WorldWalk<'q, 'w, 's> {
    fn is_solid(&self, cell: IVec3) -> bool {
        let (coord, local) = world_to_chunk(cell);
        let Some(&entity) = self.chunk_map.0.get(&coord) else {
            return true;
        };
        let Ok(chunk) = self.chunks.get(entity) else {
            return true;
        };
        let slot = chunk.get(local);
        if slot.is_empty() {
            return false;
        }
        // Doors / open gates: solid for room detection (the flood-fill
        // wants them as walls so the room is bounded) but pathing
        // treats them as passable so NPCs walk through rather than
        // climb over. Matches the collision rule in `WorldCollision` —
        // both controllers see the same passable cell.
        !self.registry.def(slot).flags.walkable_boundary
    }
    // cost() default 1.0 — future road tags hook in here without
    // changing the algorithm.
}

/// Per fixed-tick brain. Four phases per NPC:
///   1. Decay every need by its registry-defined rate.
///   2. Advance the active goal (timer countdown for Resting; pose-
///      projection + stuck detection for Wander).
///   3. If the goal completed, drop to Idle. If we're now Idle, ask
///      the Lua planner (or native fallback) for a new goal. Planner
///      errors disable just this one NPC's brain.
///   4. Steer the [`MovementIntent`] toward the current waypoint
///      (Wander only — Idle and Resting both clear intent).
/// SystemParam bundle for plan + haul resources that the brain tick
/// reaches for in phase 3. Folded into one slot because the brain tick
/// is already at the Bevy 0.18 16-SystemParam ceiling — every loose
/// `Res`/`Query` we'd otherwise add against this fn would trip the
/// trait-impl limit. Group is "stuff the haul scheduler and arrival
/// handlers share, plus the plan-claim state they coexist with."
#[derive(bevy::ecs::system::SystemParam)]
struct HaulCtx<'w, 's> {
    plans: ResMut<'w, Plans>,
    plan_claims: ResMut<'w, PlanClaims>,
    assignments: ResMut<'w, HaulAssignments>,
    reservations: ResMut<'w, WorldItemReservations>,
    broadcast: ServerMultiMessageSender<'w, 's>,
    servers: Query<'w, 's, &'static Server>,
    world_items: Query<'w, 's, (Entity, &'static WorldItem)>,
    kind_registry: Res<'w, NpcKindRegistry>,
}

#[allow(clippy::too_many_arguments, reason = "brain tick spans many subsystems")]
fn npc_brain_tick(
    time: Res<Time>,
    chunks: Query<&'static Chunk>,
    chunk_entities_q: Query<&'static ChunkEntities>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mods: Res<ServerMods>,
    need_registry: Res<NeedRegistry>,
    work_defaults: Res<WorkDefaultsRes>,
    room_map: Res<RoomMap>,
    interactable_index: Res<InteractableIndex>,
    mut interaction_claims: ResMut<InteractionClaims>,
    mut haul: HaulCtx,
    world_clock: Res<WorldClock>,
    mut commands: Commands,
    mut npcs: Query<
        (
            Entity,
            &NpcId,
            &mut AvatarPose,
            &mut Needs,
            &mut Brain,
            &mut MovementIntent,
            &mut NpcPath,
            &mut Carrying,
            &NpcKind,
            Has<KinematicLock>,
        ),
        (With<Npc>, Without<BrainDisabled>),
    >,
) {
    let dt = time.delta_secs();
    let world = WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &block_registry,
    };

    let server = haul.servers.single().ok();

    for (
        entity,
        npc_id,
        mut pose,
        mut needs,
        mut brain,
        mut intent,
        mut npc_path,
        mut carrying,
        kind,
        is_locked,
    ) in npcs.iter_mut()
    {
        // Phase 1: decay every subscribed need by its registry-defined
        // rate. Unknown ids decay at 0 (the rate-lookup returns 0) so
        // an NPC carrying a stale need from before a mod reload won't
        // crash — it just freezes that value.
        for (id, value) in needs.0.iter_mut() {
            let decay = need_registry.decay_per_sec(id);
            *value = (*value + decay * dt).clamp(0.0, 1.0);
        }

        // Phase 2: advance the active goal. MoveTo can finish in one
        // of two ways — `arrived` (reached the path's final waypoint;
        // run `on_arrive`) or `abandoned` (timed out or stuck; drop
        // straight to Idle with no follow-on). Consuming and Resting
        // both complete by timer expiry.
        // (action_to_run, optional pose-snap captured from MoveTo).
        // Snap lives on Goal::MoveTo (independent of the action), so
        // we capture it here when the path arrives and apply it before
        // dispatching to the action-specific transition below.
        let mut move_arrived: Option<(ArrivalAction, Option<UseSlotSnap>)> = None;
        let mut move_abandoned = false;
        let mut rest_done = false;
        let mut interact_completed = false;
        let mut work_done = false;
        match &mut brain.goal {
            Goal::Idle => {}
            Goal::Resting { remaining_secs } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    rest_done = true;
                }
            }
            Goal::Interacting { remaining_secs, .. } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    interact_completed = true;
                }
            }
            Goal::Working { remaining_secs, .. } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    work_done = true;
                }
            }
            Goal::MoveTo {
                path,
                progress,
                deadline_secs,
                last_pos,
                stuck_secs,
                on_arrive,
                snap,
            } => {
                *deadline_secs -= dt;
                let moved = (pose.translation - *last_pos).length();
                if moved < STUCK_MOVE_THRESHOLD {
                    *stuck_secs += dt;
                } else {
                    *stuck_secs = 0.0;
                }
                *last_pos = pose.translation;

                let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
                let new_progress = closest_progress_after(path, pose_xz, *progress);
                if new_progress > *progress {
                    *progress = new_progress;
                }

                // PATH_ARRIVE_RADIUS is wider than the NPC's turn
                // radius so this fires reliably; otherwise an NPC
                // could orbit its target forever. The feet-Y match
                // prevents a step-up path (sleep onto a bed, climb
                // onto a podium) from firing arrival before the NPC
                // has actually settled on the destination cell: XZ-
                // only would let it fire 1 cell short, and an integer
                // cell-Y check would still spuriously fire at jump
                // apex when the cell floor maps inside the destination
                // cell but the NPC is mid-air. Use settled feet (≈ the
                // cell's bottom face) instead.
                let last_cell = *path.last().expect("path non-empty");
                let end_xz = waypoint_xz(last_cell);
                let dist_to_end = (pose_xz - end_xz).length();
                let feet_y = pose.translation.y - EYE_OFFSET_FROM_CENTRE - PLAYER_HALF_EXTENTS.y;
                let foot_y_settled = (feet_y - last_cell.y as f32).abs() < 0.1;
                if dist_to_end < PATH_ARRIVE_RADIUS && foot_y_settled {
                    move_arrived = Some((on_arrive.clone(), *snap));
                } else if *deadline_secs <= 0.0 || *stuck_secs > STUCK_REPLAN_SECS {
                    move_abandoned = true;
                }
            }
        }

        // Phase 3: transition. A successful arrival branches on
        // `on_arrive` — None drops to Idle, Consume kicks off the
        // stand-still consumption timer. Abandonment and Rest go
        // straight to Idle. Consuming's expiry applies the
        // restoration after re-validating the block (mods may have
        // changed it mid-action).
        if let Some((action, snap)) = move_arrived {
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
            *intent = MovementIntent::default();
            // Apply pose snap + kinematic lock uniformly *before* the
            // action dispatch. The snap is action-agnostic — any goal
            // whose target block had a `use_slot` populated this. The
            // follow-on action just decides what the locked body
            // does (sleep/consume/work).
            if let Some(s) = snap {
                pose.translation = s.translation;
                pose.yaw = s.yaw;
                commands.entity(entity).insert(KinematicLock);
            }
            match action {
                ArrivalAction::None => {
                    brain.goal = Goal::Idle;
                }
                ArrivalAction::Interact {
                    need_restore,
                    duration_secs,
                    target_cell,
                    anchor_cell,
                    exclusive,
                    animation,
                } => {
                    brain.goal = Goal::Interacting {
                        remaining_secs: duration_secs,
                        need_restore,
                        target_cell,
                        anchor_cell,
                        exclusive,
                        animation,
                    };
                }
                ArrivalAction::Work {
                    duration_secs,
                    target_cell,
                    plan_kind,
                    need_restore,
                } => {
                    brain.goal = Goal::Working {
                        remaining_secs: duration_secs,
                        target_cell,
                        plan_kind,
                        need_restore,
                    };
                }
                ArrivalAction::PickupForPlan {
                    item_entity,
                    item_slot,
                    plan_cell,
                } => {
                    // Validate the reserved item is still where we
                    // left it and still matches the slot the scheduler
                    // queued. A despawned entity (player grabbed it,
                    // or some future cleanup removed it) or a slot
                    // mismatch (degenerate edge — items can't currently
                    // change kinds, but defensive) both release the
                    // haul.
                    let item_ok = haul
                        .world_items
                        .get(item_entity)
                        .map(|(_, wi)| wi.item == item_slot)
                        .unwrap_or(false);
                    let cap = haul
                        .kind_registry
                        .get(&kind.0)
                        .map(|d| d.carry_capacity)
                        .unwrap_or(DEFAULT_NPC_CARRY_CAPACITY);
                    if !item_ok || !carrying.can_accept(item_slot, cap) {
                        if !item_ok {
                            info!(
                                npc = npc_id.0,
                                item = ?item_entity,
                                "haul pickup: item gone or kind mismatch; releasing assignment",
                            );
                        } else {
                            info!(
                                npc = npc_id.0,
                                "haul pickup: carry can't accept the reserved item; releasing assignment",
                            );
                        }
                        release_haul_for(
                            *npc_id,
                            &mut haul.assignments,
                            &mut haul.reservations,
                        );
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    // Commit: increment carry, despawn the world item,
                    // free the reservation, drop the entry from the
                    // assignment queue. Carry::pickup_one returns false
                    // only if can_accept was false; we just checked, so
                    // the unwrap_or path is unreachable in practice.
                    let added = carrying.pickup_one(item_slot, cap);
                    debug_assert!(added, "pickup_one rejected after can_accept said yes");
                    commands.entity(item_entity).despawn();
                    haul.reservations.release(item_entity, *npc_id);
                    if let Some(assignment) = haul.assignments.get_mut(*npc_id) {
                        assignment.queue.retain(|r| r.entity != item_entity);
                    }
                    // Plan next leg from the (now updated) assignment.
                    let next_goal = haul
                        .assignments
                        .get(*npc_id)
                        .and_then(|a| {
                            pick_next_haul_leg(
                                &pose,
                                a.plan_cell,
                                &carrying,
                                cap,
                                &a.queue,
                                &haul.plans,
                                &world,
                            )
                            .map(Some)
                            .unwrap_or(Some(None))
                        })
                        .flatten();
                    match next_goal {
                        Some(goal) => {
                            if let Goal::MoveTo { path, .. } = &goal {
                                npc_path.set_if_neq(NpcPath(path.clone()));
                            }
                            brain.goal = goal;
                        }
                        None => {
                            // No more legs; release haul cleanly. May
                            // also be the "path failed" branch (Err)
                            // collapsed to Idle — both end the same
                            // way.
                            release_haul_for(
                                *npc_id,
                                &mut haul.assignments,
                                &mut haul.reservations,
                            );
                            brain.goal = Goal::Idle;
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                        }
                    }
                    let _ = plan_cell; // captured for diagnostics; unused after pickup
                }
                ArrivalAction::DepositAtPlan { plan_cell } => {
                    // Validate plan still exists + is a Build plan.
                    // Remove plans don't accept materials; if the tag
                    // was switched or cleared we release without
                    // dropping carry — the NPC keeps the stack for the
                    // next assignment (or a player Q-drop).
                    let plan_kind = haul.plans.kind(plan_cell);
                    let accepts = matches!(plan_kind, Some(PlanKind::Build { .. }));
                    if !accepts {
                        info!(
                            npc = npc_id.0,
                            cell = ?plan_cell.to_array(),
                            "haul deposit: plan gone or not a build plan; releasing assignment",
                        );
                        release_haul_for(
                            *npc_id,
                            &mut haul.assignments,
                            &mut haul.reservations,
                        );
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    // Deposit whatever we're carrying. Plans::deposit
                    // returns 0 if the plan doesn't want this kind
                    // (mismatched assignment — shouldn't happen but
                    // doesn't deserve a panic).
                    let (carry_item, carry_count) = match (carrying.item, carrying.count) {
                        (Some(slot), c) if c > 0 => (slot, c),
                        _ => {
                            // Carry empty at deposit — degenerate but
                            // recoverable. Release haul, idle, let the
                            // scheduler try again.
                            release_haul_for(
                                *npc_id,
                                &mut haul.assignments,
                                &mut haul.reservations,
                            );
                            brain.goal = Goal::Idle;
                            continue;
                        }
                    };
                    let accepted = haul.plans.deposit(plan_cell, carry_item, carry_count);
                    if accepted > 0 {
                        carrying.count = carry_count - accepted;
                        if carrying.count == 0 {
                            carrying.item = None;
                        }
                        info!(
                            npc = npc_id.0,
                            cell = ?plan_cell.to_array(),
                            accepted,
                            "haul deposit complete",
                        );
                        if let (Some(server), Some(state)) =
                            (server, haul.plans.get(plan_cell).cloned())
                        {
                            let reply = PlanEdit {
                                cell: plan_cell,
                                kind: Some(state.kind),
                                materials: state.materials,
                            };
                            if let Err(err) = haul.broadcast.send::<PlanEdit, WorldChannel>(
                                &reply,
                                server,
                                &NetworkTarget::All,
                            ) {
                                warn!("haul deposit PlanEdit broadcast failed: {err}");
                            }
                        }
                    }
                    // Decide what's next. After a deposit the queue
                    // may still have items (multi-trip haul); a
                    // plan-satisfied state ends the assignment.
                    let cap = haul
                        .kind_registry
                        .get(&kind.0)
                        .map(|d| d.carry_capacity)
                        .unwrap_or(DEFAULT_NPC_CARRY_CAPACITY);
                    let next_goal = haul
                        .assignments
                        .get(*npc_id)
                        .and_then(|a| {
                            pick_next_haul_leg(
                                &pose,
                                a.plan_cell,
                                &carrying,
                                cap,
                                &a.queue,
                                &haul.plans,
                                &world,
                            )
                            .map(Some)
                            .unwrap_or(Some(None))
                        })
                        .flatten();
                    match next_goal {
                        Some(goal) => {
                            if let Goal::MoveTo { path, .. } = &goal {
                                npc_path.set_if_neq(NpcPath(path.clone()));
                            }
                            brain.goal = goal;
                        }
                        None => {
                            release_haul_for(
                                *npc_id,
                                &mut haul.assignments,
                                &mut haul.reservations,
                            );
                            brain.goal = Goal::Idle;
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                        }
                    }
                }
            }
        }
        // Abandonment of a MoveTo with an action that reserved a
        // claim needs to release it — the brain took the claim at
        // goal commit and a stuck/timeout abandon never reaches
        // the arrival branch that would otherwise own the release.
        if move_abandoned {
            if let Goal::MoveTo { on_arrive, .. } = &brain.goal {
                match on_arrive {
                    ArrivalAction::Interact {
                        anchor_cell,
                        exclusive: true,
                        ..
                    } => {
                        interaction_claims.release(*anchor_cell, *npc_id);
                    }
                    ArrivalAction::Work { target_cell, .. } => {
                        haul.plan_claims.release(*target_cell, *npc_id);
                    }
                    ArrivalAction::PickupForPlan { .. }
                    | ArrivalAction::DepositAtPlan { .. } => {
                        // Stuck or timed out mid-haul: free the entire
                        // assignment + every reservation it holds. The
                        // scheduler will repick next tick.
                        release_haul_for(
                            *npc_id,
                            &mut haul.assignments,
                            &mut haul.reservations,
                        );
                    }
                    _ => {}
                }
            }
        }
        if move_abandoned || rest_done {
            brain.goal = Goal::Idle;
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
        }
        // Target cell of whichever interaction just finished. Set by
        // the matching per-action branch; consumed by the generic
        // post-interaction block (eject + kinematic unlock + Idle).
        let mut interact_done: Option<IVec3> = None;
        if interact_completed {
            // Generic completion path for every interactable. Apply
            // the captured need delta if any, release the claim if
            // the block was exclusive. Captured values come from the
            // planner snapshot — if the block was broken or replaced
            // between commit and completion, "did the action against
            // a stale snapshot" is the consistent outcome at every
            // upstream layer, and we still credit the NPC for
            // sticking it out.
            if let Goal::Interacting {
                need_restore,
                anchor_cell,
                target_cell,
                exclusive,
                ..
            } = &brain.goal
            {
                if let Some(nr) = need_restore {
                    if let Some(value) = needs.0.get_mut(&nr.need) {
                        *value = (*value - nr.restores).max(0.0);
                        info!(
                            npc = npc_id.0,
                            need = %nr.need,
                            restored = nr.restores,
                            remaining_deficit = *value,
                            "interaction complete",
                        );
                    } else {
                        warn!(
                            npc = npc_id.0,
                            need = %nr.need,
                            "interaction complete but NPC has no entry for need; ignoring",
                        );
                    }
                } else {
                    info!(
                        npc = npc_id.0,
                        target = ?target_cell.to_array(),
                        "interaction complete (no need change)",
                    );
                }
                if *exclusive {
                    interaction_claims.release(*anchor_cell, *npc_id);
                }
                interact_done = Some(*target_cell);
            }
        }
        if work_done {
            // Action-specific completion: apply restore, release
            // the plan claim, emit the world-mutation message.
            // Generic post-action handling runs below. `need_restore`
            // was captured at goal commit from the targeted block's
            // `work_action` or the engine-wide `WorkDefaults` — a
            // mod that wants per-block payoff scales it from there.
            if let Goal::Working {
                target_cell,
                plan_kind,
                need_restore,
                ..
            } = &brain.goal
            {
                if let Some(nr) = need_restore {
                    if let Some(value) = needs.0.get_mut(&nr.need) {
                        *value = (*value - nr.restores).max(0.0);
                        info!(
                            npc = npc_id.0,
                            cell = ?target_cell.to_array(),
                            kind = ?plan_kind,
                            need = %nr.need,
                            restored = nr.restores,
                            remaining_deficit = *value,
                            "work complete",
                        );
                    } else {
                        warn!(
                            npc = npc_id.0,
                            need = %nr.need,
                            "work complete but NPC has no entry for need; ignoring",
                        );
                    }
                } else {
                    info!(
                        npc = npc_id.0,
                        cell = ?target_cell.to_array(),
                        kind = ?plan_kind,
                        "work complete (no need change)",
                    );
                }
                commands.write_message(NpcWorkCompleted {
                    cell: *target_cell,
                    plan_kind: *plan_kind,
                });
                haul.plan_claims.release(*target_cell, *npc_id);
                interact_done = Some(*target_cell);
            }
        }
        // Generic post-interaction cleanup. Eject + unlock happen
        // only if the NPC was actually [`KinematicLock`]ked into a
        // slot — otherwise their pose is wherever the regular
        // physics tick left them (e.g. standing at a slotless
        // berry basket), so there's nothing to recover from. When
        // we were locked, the body is sitting at the snap pose
        // inside the block; the eject walks the block's authored
        // approach cells (NPC leaves the way they came in), then
        // "on top of the anchor" as a universal fallback, before
        // dropping the lock so physics resumes from a standable
        // position rather than an embedded one.
        if let Some(target_cell) = interact_done {
            if is_locked {
                let slot = slot_at_cell(target_cell, &chunks, &chunk_map, &block_registry);
                let (anchor_cell, orientation) = resolve_anchor_with_orientation(
                    target_cell,
                    &chunk_entities_q,
                    &chunk_map,
                );
                if !try_eject_to_cells(
                    &mut pose,
                    eject_candidates_for_slot(slot.as_ref(), anchor_cell, orientation),
                    &world,
                ) {
                    warn!(
                        npc = npc_id.0,
                        anchor = ?anchor_cell.to_array(),
                        "post-interaction eject: no standable approach or fallback; NPC may be embedded",
                    );
                }
                commands.entity(entity).remove::<KinematicLock>();
            }
            brain.goal = Goal::Idle;
        }
        if matches!(brain.goal, Goal::Idle) {
            // Self-rescue: if the NPC's pose isn't standable, A* will
            // bail on every goal the planner picks and we'll loop
            // forever emitting `reason=start_unstandable`. This
            // typically means a failed post-interaction eject, a
            // mid-sleep build dropping a block on the NPC, or
            // soft-actor-separation sliding them onto an unsupported
            // corner. Pop them to the nearest standable cell within
            // a tight radius *before* the planner runs, so the
            // snapshot's foot reflects the rescued position. Locked
            // NPCs are excluded — KinematicLock is the engine's
            // promise that the body is intentionally sitting where
            // a slot snap put it (inside the bed mesh, on a chair),
            // and rescuing them would yank them out of a valid use.
            if !is_locked && pose_to_standable_foot(&pose, &world).is_none() {
                match rescue_to_nearby_standable(&mut pose, &world, RESCUE_RADIUS_CELLS) {
                    Some(cell) => {
                        warn!(
                            npc = npc_id.0,
                            rescue_to = ?cell.to_array(),
                            "rescued NPC from non-standable pose at planner entry",
                        );
                    }
                    None => {
                        warn!(
                            npc = npc_id.0,
                            pose = ?pose.translation.to_array(),
                            radius = RESCUE_RADIUS_CELLS,
                            "no standable cell within rescue radius; parking briefly",
                        );
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                }
            }
            // Per-NPC haul matchmaker runs here (NOT a standalone
            // system) because the brain tick is monolithic — an NPC
            // transitions Idle → next-goal in one iteration, so a
            // standalone scheduler in Update would never observe
            // Goal::Idle. Calling per-NPC at the Idle moment is the
            // only place where the scheduler can catch an unassigned
            // NPC. The call is cheap (one O(items) index build + one
            // O(plans) scan), and only NPCs without an existing
            // assignment + empty carry get scored.
            if !haul.assignments.contains(*npc_id) && carrying.is_empty() {
                crate::haul::try_schedule_haul_for_npc(
                    *npc_id,
                    &kind.0,
                    pose.translation,
                    carrying.is_empty(),
                    &haul.kind_registry,
                    &haul.plans,
                    &haul.world_items,
                    &mut haul.assignments,
                    &mut haul.reservations,
                );
            }
            // Engine-driven haul takes priority over the Lua planner.
            // If the scheduler (above, or a previous tick) queued an
            // assignment for this NPC, plan the first leg directly
            // and skip the planner call. The planner sees
            // `pending_assignments` in its snapshot when it is called
            // for a *different* NPC, but never gets to overrule an
            // active haul.
            if haul.assignments.contains(*npc_id) {
                let cap = haul
                    .kind_registry
                    .get(&kind.0)
                    .map(|d| d.carry_capacity)
                    .unwrap_or(DEFAULT_NPC_CARRY_CAPACITY);
                let next_goal = haul
                    .assignments
                    .get(*npc_id)
                    .and_then(|a| {
                        pick_next_haul_leg(
                            &pose,
                            a.plan_cell,
                            &carrying,
                            cap,
                            &a.queue,
                            &haul.plans,
                            &world,
                        )
                        .map(Some)
                        .unwrap_or(Some(None))
                    })
                    .flatten();
                match next_goal {
                    Some(goal) => {
                        if let Goal::MoveTo { path, .. } = &goal {
                            npc_path.set_if_neq(NpcPath(path.clone()));
                        }
                        brain.goal = goal;
                        *intent = MovementIntent::default();
                        continue;
                    }
                    None => {
                        // Assignment was empty or pathfinding to the
                        // first leg failed; release and fall through
                        // to the planner so the NPC doesn't burn a
                        // tick doing nothing.
                        release_haul_for(
                            *npc_id,
                            &mut haul.assignments,
                            &mut haul.reservations,
                        );
                    }
                }
            }
            let kind_id = NpcKindId(kind.0.clone());
            let snapshot = build_snapshot(
                *npc_id,
                &kind_id,
                &pose,
                &needs,
                &room_map,
                &interactable_index,
                &interaction_claims,
                &haul.plans,
                &haul.plan_claims,
                &haul.assignments,
                &chunks,
                &chunk_entities_q,
                &chunk_map,
                &block_registry,
                &work_defaults.0,
                *world_clock,
            );
            // One-line per-NPC trace at every planner call so a
            // session log shows what each NPC saw on each decision.
            // `?need_hunger` / `?need_sleep` use Option formatting so
            // a missing need shows up explicitly as None.
            let need_hunger = snapshot.needs.get("hunger").copied();
            let need_sleep = snapshot.needs.get("sleep").copied();
            info!(
                npc = npc_id.0,
                is_night = snapshot.is_night,
                hunger = ?need_hunger,
                sleep = ?need_sleep,
                nearby_interactions = snapshot.nearby_interactions.len(),
                "planner snapshot",
            );
            let planner_goal = match mods.0.call_planner(&kind_id, &snapshot) {
                Ok(Some(g)) => g,
                Ok(None) => native_fallback_goal(),
                Err(e) => {
                    error!(
                        entity = ?entity,
                        kind = %kind.0,
                        error = %e,
                        "planner errored; disabling this NPC's brain"
                    );
                    // Release any held claims — a disabled brain
                    // shouldn't lock a bed or plan for the rest of
                    // the session. Drop the kinematic lock too so a
                    // disabled NPC isn't frozen mid-action.
                    interaction_claims.release_all_for(*npc_id);
                    haul.plan_claims.release_all_for(*npc_id);
                    release_haul_for(
                        *npc_id,
                        &mut haul.assignments,
                        &mut haul.reservations,
                    );
                    commands
                        .entity(entity)
                        .remove::<KinematicLock>()
                        .insert(BrainDisabled {
                            reason: e.to_string(),
                        });
                    *intent = MovementIntent::default();
                    continue;
                }
            };
            // Convert the planner's surface form into a live engine
            // Goal. Wander triggers an A* pick here; Rest/Idle just
            // arm a timer. Clamps protect against a misbehaving
            // planner returning absurd values.
            match planner_goal {
                PlannerGoal::Idle => {
                    // "Ask me again soon", not "ask me every tick" —
                    // arm a minimum rest so the planner-call cadence
                    // stays in seconds, not frames.
                    brain.goal = Goal::Resting {
                        remaining_secs: MIN_REST_SECS,
                    };
                    if !npc_path.0.is_empty() {
                        npc_path.0.clear();
                    }
                    *intent = MovementIntent::default();
                }
                PlannerGoal::Rest { duration_secs } => {
                    brain.goal = Goal::Resting {
                        remaining_secs: duration_secs.clamp(MIN_REST_SECS, MAX_REST_SECS),
                    };
                    if !npc_path.0.is_empty() {
                        npc_path.0.clear();
                    }
                    *intent = MovementIntent::default();
                }
                PlannerGoal::Wander {
                    radius_cells,
                    timeout_secs,
                } => {
                    let radius = radius_cells.clamp(1, MAX_WANDER_RADIUS_CELLS);
                    let timeout = timeout_secs.clamp(1.0, MAX_WANDER_TIMEOUT_SECS);
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    match pick_wander_path(foot, radius, &mut brain.rng, &world) {
                        Some(path) => {
                            // set_if_neq keeps the wire quiet on the
                            // rare repeat path; planner-driven calls
                            // are several seconds apart so it triggers
                            // basically every time, but the guard is
                            // free if it doesn't.
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal = Goal::MoveTo {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                                on_arrive: ArrivalAction::None,
                                snap: None,
                            };
                        }
                        None => {
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                standable = standable(&world, foot),
                                "wander failed: every attempt unreachable, parking briefly"
                            );
                            // No reachable target this slice — park
                            // briefly so we don't churn the planner
                            // every tick.
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                        }
                    }
                }
                PlannerGoal::Goto { cell, timeout_secs } => {
                    // Same engine primitive as Wander once we have a
                    // path — the only difference is target selection
                    // (planner-supplied vs random within radius).
                    let timeout = timeout_secs.clamp(1.0, MAX_GOTO_TIMEOUT_SECS);
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    let planner_target = IVec3::new(cell.x, cell.y, cell.z);
                    // If the planner picked the floor anchor of a known
                    // room, jitter to *any* floor cell of that room.
                    // Stops every villager visiting the same building
                    // from converging on one tile and tripping the new
                    // actor-vs-actor collision into a stampede. For
                    // out-of-room Gotos (raw waypoint cells) the helper
                    // returns None and the planner cell is used as-is.
                    let target = room_map
                        .random_floor_cell_in_same_room(planner_target, rand_unit(&mut brain.rng))
                        .unwrap_or(planner_target);
                    // Already at the target: the planner picked a cell
                    // the NPC's already standing on (typically the
                    // anchor of the room they're currently in). Drop
                    // to Idle so the planner re-picks next tick — it
                    // already set `last_action = "visit"` before
                    // returning this Goto, so the next call cycles to
                    // rest naturally.
                    if target == foot {
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        brain.goal = Goal::Idle;
                        *intent = MovementIntent::default();
                        continue;
                    }
                    let path = find_path(
                        foot,
                        target,
                        &world,
                        ASTAR_NODE_BUDGET,
                        ASTAR_PATH_BUDGET,
                    )
                    .map(|raw| smooth_path(raw, &world))
                    .filter(|p| p.len() >= 2);
                    match path {
                        Some(path) => {
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal = Goal::MoveTo {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                                on_arrive: ArrivalAction::None,
                                snap: None,
                            };
                        }
                        None => {
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target.to_array(),
                                standable_start = standable(&world, foot),
                                standable_target = standable(&world, target),
                                "goto failed: no A* path, parking briefly"
                            );
                            // Target unreachable from here (no path or
                            // path too short). Park briefly; the
                            // planner can pick something else on the
                            // next call.
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                        }
                    }
                }
                PlannerGoal::Interact { cell, timeout_secs } => {
                    let timeout = timeout_secs.clamp(1.0, MAX_INTERACT_TIMEOUT_SECS);
                    let target_cell = IVec3::new(cell.x, cell.y, cell.z);
                    // Re-resolve the block on current world state +
                    // pull its `interactable` and optional `use_slot`.
                    // Action-agnostic: the engine doesn't ask whether
                    // the block "is a bed" vs "is a basket," only what
                    // the def's metadata says about claim semantics,
                    // duration, and need delta.
                    let Some((interactable, slot, def_id)) = interactable_with_slot_at_cell(
                        target_cell,
                        &chunks,
                        &chunk_map,
                        &block_registry,
                    ) else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "interact target no longer interactable; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    };
                    // Anchor is the claim key for exclusive blocks
                    // (multi-cell entities contend on one slot).
                    // Orientation rotates both `use_slot.approach`
                    // and `use_slot.pose` into world space.
                    let (anchor_cell, orientation) = resolve_anchor_with_orientation(
                        target_cell,
                        &chunk_entities_q,
                        &chunk_map,
                    );
                    // Atomic claim — only attempted for exclusive
                    // blocks. Non-exclusive interactions (food on a
                    // shelf, water at a well) don't contend; a
                    // queue of NPCs can use them in parallel from
                    // different cells.
                    if interactable.exclusive
                        && !interaction_claims.try_claim(anchor_cell, *npc_id)
                    {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            anchor = ?anchor_cell.to_array(),
                            block = %def_id,
                            "exclusive interact target taken by another NPC; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    // Resolve approach cell + (optional) snap from the
                    // use_slot. Slot-bearing blocks land the NPC on
                    // a precise pose with KinematicLock applied;
                    // slotless blocks fall back to any standable
                    // cardinal neighbour with no snap and no lock —
                    // works for any-angle interactions like a fruit
                    // basket or a water well.
                    let (stand_cell, snap) = match resolve_use_slot_target(
                        slot.as_ref(),
                        anchor_cell,
                        orientation,
                        target_cell,
                        foot,
                        &world,
                    ) {
                        Some(pair) => pair,
                        None => {
                            info!(
                                npc = npc_id.0,
                                target = ?target_cell.to_array(),
                                block = %def_id,
                                "no standable approach for interactable; releasing claim and parking briefly",
                            );
                            if interactable.exclusive {
                                interaction_claims.release(anchor_cell, *npc_id);
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                            continue;
                        }
                    };
                    let duration = interactable
                        .duration_secs
                        .clamp(MIN_INTERACT_DURATION_SECS, MAX_INTERACT_DURATION_SECS);
                    let need_restore = interactable.need_restore.clone();
                    let exclusive = interactable.exclusive;
                    // Capture the slot's animation override (if any)
                    // at goal-commit time. Carried through to
                    // Goal::Interacting so the per-tick activity
                    // refresh doesn't have to re-look-up the block
                    // def to drive the client's anim override.
                    let animation = slot.as_ref().and_then(|s| s.animation.clone());
                    if stand_cell == foot {
                        // Already standing on the approach cell —
                        // apply the snap in place and enter
                        // Interacting. Same arrival semantics as
                        // the MoveTo path, just without the path.
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        if let Some(s) = snap {
                            pose.translation = s.translation;
                            pose.yaw = s.yaw;
                            commands.entity(entity).insert(KinematicLock);
                        }
                        brain.goal = Goal::Interacting {
                            remaining_secs: duration,
                            need_restore,
                            target_cell,
                            anchor_cell,
                            exclusive,
                            animation,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                    let path = find_path(
                        foot,
                        stand_cell,
                        &world,
                        ASTAR_NODE_BUDGET,
                        ASTAR_PATH_BUDGET,
                    )
                    .map(|raw| smooth_path(raw, &world))
                    .filter(|p| p.len() >= 2);
                    match path {
                        Some(path) => {
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal = Goal::MoveTo {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                                on_arrive: ArrivalAction::Interact {
                                    need_restore,
                                    duration_secs: duration,
                                    target_cell,
                                    anchor_cell,
                                    exclusive,
                                    animation,
                                },
                                snap,
                            };
                        }
                        None => {
                            // Classify the A* miss: `find_path` bails
                            // up front when either endpoint isn't
                            // standable, otherwise it ran out of
                            // budget or found no path. The labels are
                            // what makes the next stuck-NPC report
                            // diagnosable from logs alone — "embedded
                            // start" is very different from "target
                            // walled off."
                            let reason = if !standable(&world, foot) {
                                "start_unstandable"
                            } else if !standable(&world, stand_cell) {
                                "stand_unstandable"
                            } else {
                                "unreachable"
                            };
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target_cell.to_array(),
                                stand = ?stand_cell.to_array(),
                                reason,
                                "interact failed: no A* path to approach cell, releasing claim and parking briefly"
                            );
                            if exclusive {
                                interaction_claims.release(anchor_cell, *npc_id);
                            }
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                        }
                    }
                }
                PlannerGoal::WorkPlan { cell, timeout_secs } => {
                    let timeout = timeout_secs.clamp(1.0, MAX_WORK_TIMEOUT_SECS);
                    let target_cell = IVec3::new(cell.x, cell.y, cell.z);
                    // Re-resolve against the authoritative `Plans`. The
                    // planner saw a snapshot — the tag may have been
                    // cancelled or auto-cleared by the time we commit.
                    // We need just the kind for the work pipeline; the
                    // materials gate is checked when collecting nearby
                    // plans for the snapshot (filtered out if pending).
                    let Some(plan_kind) = haul.plans.kind(target_cell) else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "work target no longer tagged; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    };
                    // Atomic claim. Lost-race → brief rest; planner re-picks.
                    if !haul.plan_claims.try_claim(target_cell, *npc_id) {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "work target claimed by another NPC; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                    // Capture work-action knobs (need + magnitude + duration)
                    // *once* at goal commit. Build plans consult the block
                    // being placed (in the plan slot); Remove plans consult
                    // the live block at the cell. Either falls back to the
                    // engine-wide WorkDefaults when the block has no
                    // `work_action`. A subsequent re-tag or re-block can't
                    // retroactively change what this NPC was rewarded for.
                    let (work_duration_secs, work_need_restore) = resolve_work_action(
                        plan_kind,
                        target_cell,
                        &chunks,
                        &chunk_map,
                        &block_registry,
                        &work_defaults.0,
                    );
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    let Some(stand_cell) =
                        nearest_standable_neighbor(target_cell, foot, &world)
                    else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "no standable neighbour of plan target; releasing claim and parking briefly",
                        );
                        haul.plan_claims.release(target_cell, *npc_id);
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    };
                    if stand_cell == foot {
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        brain.goal = Goal::Working {
                            remaining_secs: work_duration_secs,
                            target_cell,
                            plan_kind,
                            need_restore: work_need_restore,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                    let path = find_path(
                        foot,
                        stand_cell,
                        &world,
                        ASTAR_NODE_BUDGET,
                        ASTAR_PATH_BUDGET,
                    )
                    .map(|raw| smooth_path(raw, &world))
                    .filter(|p| p.len() >= 2);
                    match path {
                        Some(path) => {
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal = Goal::MoveTo {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                                on_arrive: ArrivalAction::Work {
                                    duration_secs: work_duration_secs,
                                    target_cell,
                                    plan_kind,
                                    need_restore: work_need_restore,
                                },
                                snap: None,
                            };
                        }
                        None => {
                            let reason = if !standable(&world, foot) {
                                "start_unstandable"
                            } else if !standable(&world, stand_cell) {
                                "stand_unstandable"
                            } else {
                                "unreachable"
                            };
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target_cell.to_array(),
                                stand = ?stand_cell.to_array(),
                                reason,
                                "work failed: no A* path to standable neighbour, releasing claim and parking briefly"
                            );
                            haul.plan_claims.release(target_cell, *npc_id);
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                        }
                    }
                }
            }
        }

        // Phase 4: steering. MoveTo drives forward motion + turning
        // (pure-pursuit along the path). Consuming rotates the body
        // toward the target cell without forward motion — full speed
        // would orbit a target the NPC is already adjacent to.
        // Idle and Resting clear intent (default = no motion).
        let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
        match &brain.goal {
            Goal::MoveTo {
                path, progress, ..
            } => {
                // Pure-pursuit aim: LOOKAHEAD_DIST ahead of the closest
                // projection along the path. The `forward` in
                // `apply_walk_step` is `(-sin(yaw), 0, -cos(yaw))`, so
                // the yaw pointing toward `(dx, 0, dz)` is
                // `atan2(-dx, -dz)`.
                let aim = lookahead_point(path, *progress, LOOKAHEAD_DIST);
                let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, aim, dt) else {
                    *intent = MovementIntent::default();
                    continue;
                };
                let foot_y = pose_to_foot_cell(&pose).y;
                let jump = step_up_imminent(path, *progress, foot_y);
                *intent = MovementIntent {
                    wishdir: [0, 0, -1],
                    jump,
                    toggle_mode: false,
                    interact: false,
                    dyaw,
                };
            }
            Goal::Working { target_cell, .. } => {
                let aim = waypoint_xz(*target_cell);
                let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, aim, dt) else {
                    *intent = MovementIntent::default();
                    continue;
                };
                *intent = MovementIntent {
                    wishdir: [0, 0, 0],
                    jump: false,
                    toggle_mode: false,
                    interact: false,
                    dyaw,
                };
            }
            Goal::Interacting { target_cell, .. } => {
                // Locked interactions (snapped onto a use_slot pose)
                // freeze yaw — the snap already chose the right
                // direction and aiming at the target cell would
                // drift the yaw away when the snap landed the NPC
                // off the target cell's centre. Unlocked
                // interactions (consume-pattern stand-and-wait)
                // face the target so the body visibly engages
                // with the block.
                if is_locked {
                    *intent = MovementIntent::default();
                } else {
                    let aim = waypoint_xz(*target_cell);
                    let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, aim, dt) else {
                        *intent = MovementIntent::default();
                        continue;
                    };
                    *intent = MovementIntent {
                        wishdir: [0, 0, 0],
                        jump: false,
                        toggle_mode: false,
                        interact: false,
                        dyaw,
                    };
                }
            }
            Goal::Idle | Goal::Resting { .. } => {
                *intent = MovementIntent::default();
            }
        }
    }
}

/// Compute the per-tick yaw step that rotates `current_yaw` toward
/// whichever yaw points from `pose_xz` to `aim`. Clamped to
/// `NPC_TURN_RATE * dt` so a 180° flip doesn't snap. Returns `None`
/// when `aim` is on top of `pose_xz` (no direction to face).
fn aim_yaw_step(pose_xz: Vec2, current_yaw: f32, aim: Vec2, dt: f32) -> Option<f32> {
    let dx = aim.x - pose_xz.x;
    let dz = aim.y - pose_xz.y;
    if dx * dx + dz * dz < f32::EPSILON {
        return None;
    }
    let desired_yaw = (-dx).atan2(-dz);
    let mut delta = (desired_yaw - current_yaw) % core::f32::consts::TAU;
    if delta > core::f32::consts::PI {
        delta -= core::f32::consts::TAU;
    } else if delta < -core::f32::consts::PI {
        delta += core::f32::consts::TAU;
    }
    Some(delta.clamp(-NPC_TURN_RATE * dt, NPC_TURN_RATE * dt))
}

/// Try a few random XZ targets within `radius_cells` of `foot`; project
/// each onto the surface via `nearest_standable_below`; run A*. Return
/// the first path that has at least one step. `None` if every attempt
/// fails (caller stays Idle and retries next tick).
fn pick_wander_path<W: Walkability>(
    foot: IVec3,
    radius_cells: i32,
    rng: &mut u64,
    world: &W,
) -> Option<Vec<IVec3>> {
    let radius = radius_cells.max(1) as f32;
    for _ in 0..MAX_WANDER_ATTEMPTS {
        let dx = (rand_unit(rng) * 2.0 - 1.0) * radius;
        let dz = (rand_unit(rng) * 2.0 - 1.0) * radius;
        // Probe from a few cells above the NPC's foot Y — if the
        // candidate is on a small hill we still see its top.
        let probe = foot + IVec3::new(dx as i32, 4, dz as i32);
        let Some(target) = nearest_standable_below(world, probe, WANDER_DROP_BUDGET) else {
            continue;
        };
        // `let-else` (not `?`) so a single A* failure doesn't
        // short-circuit the whole loop — the NPC needs to keep
        // trying other random targets if the first one happens to
        // be unreachable in the budget.
        let Some(raw) = find_path(foot, target, world, ASTAR_NODE_BUDGET, ASTAR_PATH_BUDGET) else {
            continue;
        };
        // Smooth before returning: 4-directional A* output stair-
        // steps through diagonals, which makes the NPC visibly
        // wobble as pure-pursuit chases each kink. String-pulling
        // collapses redundant cells while preserving step-ups.
        let path = smooth_path(raw, world);
        if path.len() >= 2 {
            return Some(path);
        }
    }
    None
}

/// Planner stand-in for kinds that have no Lua planner registered. The
/// engine still has to drive the NPC, so it picks the simplest plausible
/// behavior — a wander at the default radius. Pairs with
/// [`apply_planner_goal`]'s "downgrade to a short rest on path failure"
/// branch so a fallback NPC in a wall-locked spot doesn't spin.
fn native_fallback_goal() -> PlannerGoal {
    PlannerGoal::Wander {
        radius_cells: FALLBACK_WANDER_RADIUS_CELLS,
        timeout_secs: FALLBACK_WANDER_TIMEOUT_SECS,
    }
}

/// Max walk-deadline for a single haul leg (pickup or deposit). Same
/// magnitude as the per-Goto/Work timeouts but a touch shorter — a
/// haul cycle is many legs in series, so spending two minutes on each
/// would let one wedged NPC tie up its assignment for the entire
/// session. 60 s leaves headroom for a cross-chunk walk while still
/// timing out promptly on a genuine wedge.
const HAUL_LEG_TIMEOUT_SECS: f32 = 60.0;

/// Default carry cap for any NPC kind that doesn't declare its own.
/// Mirrors [`block_junk_mod_api::npcs::default_carry_capacity`] so the
/// engine never reads a 0 cap when a kind is missing from the registry
/// (which would deadlock the scheduler — every reservation gates on
/// `Carrying::can_accept`).
const DEFAULT_NPC_CARRY_CAPACITY: u32 = 3;

/// Plan a Goal::MoveTo from `pose` to a standable neighbor of `target_cell`,
/// with `on_arrive` firing on arrival. Returns `None` when no standable
/// neighbor exists or no A* path reaches one — callers release the
/// haul + idle in that case. When the NPC is already on a standable
/// neighbor, returns a 1-cell path so the arrival check fires on the
/// next tick (the brain's arrival path-projection helpers tolerate
/// `path.len() == 1`).
fn plan_haul_move<W: Walkability>(
    pose: &AvatarPose,
    target_cell: IVec3,
    on_arrive: ArrivalAction,
    deadline_secs: f32,
    world: &W,
) -> Option<Goal> {
    let foot = pose_to_standable_foot(pose, world).unwrap_or_else(|| pose_to_foot_cell(pose));
    let stand_cell = nearest_standable_neighbor(target_cell, foot, world)?;
    let path = if stand_cell == foot {
        vec![foot]
    } else {
        find_path(foot, stand_cell, world, ASTAR_NODE_BUDGET, ASTAR_PATH_BUDGET)
            .map(|raw| smooth_path(raw, world))
            .filter(|p| p.len() >= 2)?
    };
    Some(Goal::MoveTo {
        path,
        progress: 0.0,
        deadline_secs,
        last_pos: pose.translation,
        stuck_secs: 0.0,
        on_arrive,
        snap: None,
    })
}

/// Pick the next leg of a haul cycle after a pickup or deposit
/// completes. Returns:
/// - `Ok(Some(goal))` — next MoveTo is queued; assignment continues.
/// - `Ok(None)` — assignment is naturally done (carry empty + queue
///   empty, or plan satisfied with nothing left to fetch); caller
///   releases the haul and drops to Idle. The scheduler picks again
///   next tick if the plan still needs more.
/// - `Err(())` — pathfinding failed for whichever destination was
///   next; caller releases the haul and parks briefly. Same recovery
///   as the existing WorkPlan path-failure branch.
fn pick_next_haul_leg<W: Walkability>(
    pose: &AvatarPose,
    plan_cell: IVec3,
    carrying: &Carrying,
    carry_cap: u32,
    assignment_queue: &[crate::haul::ReservedItem],
    plans: &Plans,
    world: &W,
) -> Result<Option<Goal>, ()> {
    let plan_remaining = matches!(plans.get(plan_cell), Some(s) if !s.is_satisfied());
    let carry_full = !carrying.is_empty() && carrying.count >= carry_cap;
    let queue_empty = assignment_queue.is_empty();
    // Walk to deposit if: carry has stuff AND (queue empty OR carry full
    // OR plan no longer needs more). The "plan no longer needs more"
    // path drops the leftover via deposit too — Plans::deposit rounds
    // accepted to remaining-need and the leftover stays on the NPC for
    // the next assignment.
    if !carrying.is_empty() && (queue_empty || carry_full || !plan_remaining) {
        return plan_haul_move(
            pose,
            plan_cell,
            ArrivalAction::DepositAtPlan { plan_cell },
            HAUL_LEG_TIMEOUT_SECS,
            world,
        )
        .map(Some)
        .ok_or(());
    }
    // Walk to the next reserved item if: carry has room AND queue has
    // items AND the plan still wants more. Pop happens at the *arrival*
    // (pickup) handler, not here — `pick_next_haul_leg` only reads.
    if !queue_empty && plan_remaining {
        let next = assignment_queue[0];
        return plan_haul_move(
            pose,
            pose_to_foot_cell_of(next.translation),
            ArrivalAction::PickupForPlan {
                item_entity: next.entity,
                item_slot: next.item,
                plan_cell,
            },
            HAUL_LEG_TIMEOUT_SECS,
            world,
        )
        .map(Some)
        .ok_or(());
    }
    // Carry empty + (queue empty or plan satisfied). The assignment has
    // run its course; release and idle. If the plan still needs more,
    // the scheduler will create a fresh assignment next tick.
    Ok(None)
}

/// Convert a loose-item world translation into the foot cell directly
/// under it — the cell the NPC pathfinds *to a neighbor of*. Items
/// land at the surface, so their translation's floor `y` is the
/// foot cell. Used in haul leg planning so callers don't have to
/// invent a target IVec3 from a Vec3 themselves.
fn pose_to_foot_cell_of(translation: Vec3) -> IVec3 {
    IVec3::new(
        translation.x.floor() as i32,
        translation.y.floor() as i32,
        translation.z.floor() as i32,
    )
}

/// Build the snapshot handed to a planner this tick. Clones the need
/// map (the planner's Lua state needs an independent copy to walk into
/// a Lua table) and collects the K nearest matched rooms — that's the
/// per-planner-call cost we accept to keep the brain tick cheap (only
/// fires on goal transitions, not every fixed tick).
///
/// The room list is sorted by Manhattan distance from `foot`. Manhattan
/// is cheap to compute server-side and ranks correctly for "nearer is
/// better"; a planner that needs euclidean can derive it from `foot` +
/// `anchor`.
#[allow(clippy::too_many_arguments, reason = "snapshot builder collates many subsystems")]
fn build_snapshot(
    id: NpcId,
    kind: &NpcKindId,
    pose: &AvatarPose,
    needs: &Needs,
    rooms: &RoomMap,
    interactables: &InteractableIndex,
    interaction_claims: &InteractionClaims,
    plans: &Plans,
    plan_claims: &PlanClaims,
    haul_assignments: &HaulAssignments,
    chunks: &Query<&Chunk>,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
    work_defaults: &block_junk_mod_api::npcs::WorkDefaults,
    world_clock: WorldClock,
) -> NpcSnapshot {
    let foot = pose_to_foot_cell(pose);
    let nearby_rooms = collect_nearby_rooms(rooms, foot, SNAPSHOT_ROOM_LIMIT);
    let nearby_interactions = collect_nearby_interactions(
        interactables,
        interaction_claims,
        block_registry,
        chunk_entities,
        chunk_map,
        id,
        foot,
        SNAPSHOT_INTERACTION_RADIUS_CELLS,
        SNAPSHOT_INTERACTION_LIMIT,
    );
    let nearby_plans = collect_nearby_plans(
        plans,
        plan_claims,
        id,
        foot,
        SNAPSHOT_PLAN_RADIUS_CELLS,
        SNAPSHOT_PLAN_LIMIT,
        chunks,
        chunk_map,
        block_registry,
        work_defaults,
    );
    // Engine-assigned haul work for *this* NPC. Today the engine
    // bypasses the planner whenever an assignment is live, so this
    // field arrives empty in every snapshot a planner actually sees —
    // populated only for the (currently unreachable) future where a
    // planner gets to weigh in even mid-haul. Wire it through anyway
    // so the surface is stable and the bypass becomes an enable/disable
    // knob rather than a shape change.
    let pending_assignments = haul_assignments
        .get(id)
        .map(|a| {
            vec![PendingAssignment {
                plan_cell: BlockPos {
                    x: a.plan_cell.x,
                    y: a.plan_cell.y,
                    z: a.plan_cell.z,
                },
                items_remaining: a.queue.len() as u32,
            }]
        })
        .unwrap_or_default();
    NpcSnapshot {
        id: id.0,
        kind: kind.clone(),
        foot: BlockPos {
            x: foot.x,
            y: foot.y,
            z: foot.z,
        },
        needs: needs.0.clone(),
        nearby_rooms,
        nearby_interactions,
        nearby_plans,
        is_night: world_clock.is_night(),
        pending_assignments,
    }
}

/// K nearest *unclaimed* plan cells within `radius_cells` (Manhattan)
/// of `foot`. Same shape as `collect_nearby_sleepers` — filter taken,
/// sort by distance, truncate to limit. The `kind` is mapped from the
/// full engine-side `PlanKind` to the simpler `PlanKindHint` exposed
/// to mods (which don't need slot + orientation to make the decision).
///
/// `need`/`restores` mirror what the brain would actually apply on
/// completion — resolved per plan from the targeted block's
/// `work_action` (Build: block being placed; Remove: live block at
/// cell) with `WorkDefaults` as the fallback. Planners use these to
/// pick the highest-payoff nearby plan when several are equidistant.
#[allow(clippy::too_many_arguments, reason = "snapshot collector mirrors live brain lookups")]
fn collect_nearby_plans(
    plans: &Plans,
    plan_claims: &PlanClaims,
    self_id: NpcId,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
    work_defaults: &block_junk_mod_api::npcs::WorkDefaults,
) -> Vec<NearbyPlan> {
    let mut out: Vec<NearbyPlan> = Vec::new();
    for (cell, state) in plans.iter() {
        let d = *cell - foot;
        if d.x.abs() > radius_cells || d.y.abs() > radius_cells || d.z.abs() > radius_cells {
            continue;
        }
        if plan_claims.is_taken_by_other(*cell, self_id) {
            continue;
        }
        // Phase-3 gate: NPCs can only commit to plans whose materials
        // are fully delivered. Pending-materials Build plans wait for
        // the player (or, post-Phase-4, the haul scheduler) to fill
        // them — the planner shouldn't even see them.
        if !state.is_satisfied() {
            continue;
        }
        let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
        let hint = match state.kind {
            PlanKind::Remove => PlanKindHint::Remove,
            PlanKind::Build { .. } => PlanKindHint::Build,
        };
        let (_duration, need_restore) = resolve_work_action(
            state.kind,
            *cell,
            chunks,
            chunk_map,
            block_registry,
            work_defaults,
        );
        let (need, restores) = match need_restore {
            Some(nr) => (Some(nr.need), nr.restores),
            None => (None, 0.0),
        };
        out.push(NearbyPlan {
            cell: BlockPos {
                x: cell.x,
                y: cell.y,
                z: cell.z,
            },
            kind: hint,
            need,
            restores,
            distance,
        });
    }
    out.sort_by_key(|p| p.distance);
    out.truncate(limit);
    out
}

/// Resolve a (possibly mid-bed) cell to its anchor cell via the chunk
/// sidecar. For single-cell sleepers there's no sidecar entry and the
/// cell itself is the anchor. For multi-cell ones we follow the
/// `EntryKind::Ghost` back to the anchor; sidecar inconsistency
/// (anchor isn't an Anchor) is treated as "use the cell as-is" — the
/// downstream `sleeper_at_cell` re-validates and will bail if the
/// resolution sent us somewhere wrong.
fn resolve_anchor_cell(
    cell: IVec3,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
) -> IVec3 {
    let (coord, _) = world_to_chunk(cell);
    let Some(&entity) = chunk_map.0.get(&coord) else {
        return cell;
    };
    let Ok(entries) = chunk_entities.get(entity) else {
        return cell;
    };
    match entries.get(cell) {
        Some(EntryKind::Anchor { .. }) | None => cell,
        Some(EntryKind::Ghost { anchor }) => anchor,
    }
}

/// Like [`resolve_anchor_cell`] but also pulls the bed's stored
/// orientation. Falls back to `Cardinal::East` (the default placement)
/// when the cell has no entity entry — e.g. a 1-cell sleeper without a
/// sidecar entry, or a chunk that isn't loaded yet.
fn resolve_anchor_with_orientation(
    cell: IVec3,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
) -> (IVec3, Cardinal) {
    let (coord, _) = world_to_chunk(cell);
    let Some(&entity) = chunk_map.0.get(&coord) else {
        return (cell, Cardinal::default());
    };
    let Ok(entries) = chunk_entities.get(entity) else {
        return (cell, Cardinal::default());
    };
    match entries.get(cell) {
        Some(EntryKind::Anchor { orientation }) => (cell, orientation),
        Some(EntryKind::Ghost { anchor }) => {
            // Look up the anchor's own entry to read its orientation;
            // ghost entries don't carry one.
            let (a_coord, _) = world_to_chunk(anchor);
            let Some(&a_entity) = chunk_map.0.get(&a_coord) else {
                return (anchor, Cardinal::default());
            };
            let Ok(a_entries) = chunk_entities.get(a_entity) else {
                return (anchor, Cardinal::default());
            };
            let orientation = match a_entries.get(anchor) {
                Some(EntryKind::Anchor { orientation }) => orientation,
                _ => Cardinal::default(),
            };
            (anchor, orientation)
        }
        None => (cell, Cardinal::default()),
    }
}

/// Resolve an interactable-bearing block at a cell, returning its
/// [`Interactable`] metadata, optional [`UseSlot`], and id (for log
/// lines). `None` when the cell is empty, the chunk isn't loaded,
/// or the def has no interactable metadata. Pulled into one lookup
/// so the planner commit path doesn't make three separate trips
/// through the same chunk + registry.
fn interactable_with_slot_at_cell(
    cell: IVec3,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<(Interactable, Option<UseSlot>, block_junk_mod_api::blocks::BlockId)> {
    let (coord, local) = world_to_chunk(cell);
    let entity = *chunk_map.0.get(&coord)?;
    let chunk = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    let def = registry.def(slot);
    let interactable = def.interactable.clone()?;
    Some((interactable, def.use_slot.clone(), def.id.clone()))
}

/// Pick a stand cell + (optional) snap for a goal whose target block
/// may carry a [`UseSlot`]. Two modes:
///
/// - **Slot present**: rotate each `slot.approach` cell offset by the
///   block's [`Cardinal`] and add the anchor cell to get a world-space
///   candidate. Among the candidates that are currently `standable`,
///   pick whichever has the smallest Manhattan distance to the NPC's
///   foot (ties broken by listing order — authors put the "preferred"
///   approach first if it matters). Compute the snap once from the
///   anchor + rotated `slot.pose` + `slot.yaw`; arrival just teleports
///   to it.
/// - **Slot absent**: fall back to the consume-pattern. Pick a
///   standable cardinal neighbour of `target_cell` closest to `foot`,
///   and return `snap = None`. The body lands at the path's last cell
///   with no pose snap and no kinematic lock — the legacy behaviour
///   for blocks that read naturally from any side (fruit basket).
///
/// Returns `None` only when neither a slot-approach nor a neighbour
/// stand cell is available — the goal should abandon and let the
/// planner pick something else.
fn resolve_use_slot_target<W: Walkability>(
    slot: Option<&UseSlot>,
    anchor_cell: IVec3,
    orientation: Cardinal,
    target_cell: IVec3,
    foot: IVec3,
    world: &W,
) -> Option<(IVec3, Option<UseSlotSnap>)> {
    match slot {
        Some(slot) => {
            let mut best: Option<(IVec3, i32)> = None;
            for off in &slot.approach {
                let rotated = orientation.rotate_offset(*off);
                let cand = anchor_cell + IVec3::new(rotated[0], rotated[1], rotated[2]);
                if !standable(world, cand) {
                    continue;
                }
                let d = cand - foot;
                let dist = d.x.abs() + d.y.abs() + d.z.abs();
                match best {
                    None => best = Some((cand, dist)),
                    Some((_, prev)) if dist < prev => best = Some((cand, dist)),
                    _ => {}
                }
            }
            let stand = best?.0;
            Some((stand, Some(compute_use_slot_snap(slot, anchor_cell, orientation))))
        }
        None => {
            let stand = nearest_standable_neighbor(target_cell, foot, world)?;
            Some((stand, None))
        }
    }
}

/// World-space pose snap implied by a slot at a given anchor +
/// orientation. The author writes `slot.pose` in default-orientation
/// model space (origin at the anchor cell's bottom-centre, +X = the
/// default extends direction), so converting to world is: rotate the
/// XZ components by the block's [`Cardinal`], shift Y by the anchor's
/// cell-origin Y, and shift XZ by the anchor cell's *centre* (the
/// model frame's origin is the bottom-*centre* of the anchor, not the
/// cell's min corner). Yaw is the block's cardinal yaw plus the
/// slot's authored offset — that's where "while using, orient this
/// way" lives.
/// Look up the optional [`UseSlot`] for whatever block lives at this
/// cell. Generic over interaction type — the slot is the same field
/// on the def regardless of whether the block is a sleeper,
/// consumable, or future workstation, so ejection can read it the
/// same way for all of them. Returns `None` for empty cells,
/// unloaded chunks, or defs without a slot.
fn slot_at_cell(
    cell: IVec3,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<UseSlot> {
    let (coord, local) = world_to_chunk(cell);
    let entity = *chunk_map.0.get(&coord)?;
    let chunk = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    registry.def(slot).use_slot.clone()
}

/// Resolve the work-action knobs (`duration_secs`, optional `need_restore`)
/// for a WorkPlan goal at commit time. Reads `block.work_action` from
/// the block being placed (Build) or the live block at the cell
/// (Remove), with [`WorkDefaults`] as the fallback when either the
/// block lookup misses or the block has no `work_action`.
///
/// Returns the **engine defaults** if the Remove cell is unloaded or
/// empty rather than failing — the brain still wants to commit a goal,
/// and the alternative (silent abort) hides the underlying issue at a
/// layer the planner can't react to.
fn resolve_work_action(
    plan_kind: PlanKind,
    target_cell: IVec3,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    defaults: &block_junk_mod_api::npcs::WorkDefaults,
) -> (f32, Option<NeedRestore>) {
    let block_action = match plan_kind {
        PlanKind::Build { slot, .. } => registry.def(slot).work_action.as_ref().cloned(),
        PlanKind::Remove => {
            let (coord, local) = world_to_chunk(target_cell);
            chunk_map
                .0
                .get(&coord)
                .and_then(|&entity| chunks.get(entity).ok())
                .map(|chunk| chunk.get(local))
                .filter(|slot| !slot.is_empty())
                .and_then(|slot| registry.def(slot).work_action.clone())
        }
    };
    match block_action {
        Some(w) => (w.duration_secs, w.need_restore),
        None => (defaults.duration_secs, defaults.need_restore.clone()),
    }
}

/// Teleport `pose` onto the first standable cell in `candidates`.
/// Pose is set to "standing at cell centre, feet on cell floor" —
/// the same eye-position math the spawn path and `walk_step` rest
/// at. Returns `true` on a successful eject (pose mutated), `false`
/// if every candidate was unstandable (pose unchanged; caller
/// surfaces this with a warning).
///
/// Used as the post-use eject — when a [`KinematicLock`] is about
/// to be released, the NPC's body is typically sitting inside the
/// block they just used (on the mattress, in the chair, atop the
/// forge), and the next physics tick alone won't pull them out.
/// Callers pass an ordered candidate list with the block's
/// `use_slot.approach` cells first (NPCs leave the way they came
/// in), then "above the AABB" fallbacks so a sealed-in NPC still
/// has somewhere to go.
fn try_eject_to_cells<W: Walkability>(
    pose: &mut AvatarPose,
    candidates: impl IntoIterator<Item = IVec3>,
    world: &W,
) -> bool {
    for cell in candidates {
        if !standable(world, cell) {
            continue;
        }
        pose.translation = standing_pose_translation(cell);
        return true;
    }
    false
}

/// World-space eject candidates for an actor leaving a use-slot
/// interaction. Order:
/// 1. Each `slot.approach` cell, rotated by `orientation` and
///    offset from `anchor_cell`. Author-listed order is preserved
///    — the first entry is the "preferred exit." NPCs going back
///    the way they came in feels right for most blocks.
/// 2. `anchor + Y` and `anchor + 2Y` as a last-resort "pop on top
///    of the AABB" fallback when every approach is now blocked
///    (a corral the NPC was sealed into mid-sleep).
///
/// Slot-less blocks skip step 1 and fall straight to step 2.
fn eject_candidates_for_slot(
    slot: Option<&UseSlot>,
    anchor_cell: IVec3,
    orientation: Cardinal,
) -> Vec<IVec3> {
    let mut out = Vec::new();
    if let Some(slot) = slot {
        for off in &slot.approach {
            let (rx, rz) = match orientation {
                Cardinal::East => (off[0], off[2]),
                Cardinal::North => (off[2], -off[0]),
                Cardinal::West => (-off[0], -off[2]),
                Cardinal::South => (-off[2], off[0]),
            };
            out.push(anchor_cell + IVec3::new(rx, off[1], rz));
        }
    }
    out.push(anchor_cell + IVec3::Y);
    out.push(anchor_cell + IVec3::Y * 2);
    out
}

fn compute_use_slot_snap(slot: &UseSlot, anchor_cell: IVec3, orientation: Cardinal) -> UseSlotSnap {
    // Float rotation matches Cardinal::rotate_offset's integer matrix
    // — fractional pose components (mid-cell, half-height) survive
    // the trip into world space.
    let (rx, rz) = match orientation {
        Cardinal::East => (slot.pose[0], slot.pose[2]),
        Cardinal::North => (slot.pose[2], -slot.pose[0]),
        Cardinal::West => (-slot.pose[0], -slot.pose[2]),
        Cardinal::South => (-slot.pose[2], slot.pose[0]),
    };
    let anchor_origin = anchor_cell.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
    // `slot.pose` puts the rig's model origin (its "feet" plane)
    // at this point. `pose.translation` is the eye position, so we
    // raise by the standing eye-offset to get the actual translation.
    // This is the symmetric of `attach_npc_visuals`'s `foot_offset`
    // — the child Transform shifts the model origin down by that
    // same amount, so the round trip leaves the body where the
    // author asked.
    let model_origin = anchor_origin + Vec3::new(rx, slot.pose[1], rz);
    let translation =
        model_origin + Vec3::Y * (EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y);
    let yaw = orientation.yaw() + slot.yaw;
    UseSlotSnap { translation, yaw }
}

/// K nearest interactable cells within `radius_cells` (Chebyshev) of
/// `foot`, one entry per *block* (collapsed by anchor so multi-cell
/// interactables don't appear twice). Each entry pulls its
/// `need_restore` and `exclusive` from the block's
/// [`Interactable`](block_junk_mod_api::blocks::Interactable) def via
/// the registry — the index only stores `(cell, BlockSlot)` to avoid
/// duplicating data that ultimately lives in the def.
///
/// **Already filtered**: exclusive interactables currently claimed
/// by a different NPC are excluded. Race-on-claim is still possible
/// (two planners tick the same instant) but resolved atomically at
/// the brain's `try_claim` step.
///
/// Distance is Manhattan (consistent with `nearby_rooms.distance`);
/// the radius filter uses Chebyshev because that matches how the
/// index's `iter_within` is bounded.
#[allow(clippy::too_many_arguments, reason = "merges per-cell + per-anchor lookups")]
fn collect_nearby_interactions(
    index: &InteractableIndex,
    claims: &InteractionClaims,
    block_registry: &BlockRegistry,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    self_id: NpcId,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
) -> Vec<NearbyInteraction> {
    let mut seen_anchors: HashSet<IVec3> = HashSet::new();
    let mut out: Vec<NearbyInteraction> = Vec::new();
    for (cell, slot) in index.iter_within(foot, radius_cells) {
        let Some(i) = block_registry.def(slot).interactable.as_ref() else {
            // Defensive: stale index entry whose def is no longer
            // interactable (would happen if a future mod-reload
            // changed metadata under us). Just skip.
            continue;
        };
        let anchor = resolve_anchor_cell(cell, chunk_entities, chunk_map);
        // Collapse multi-cell blocks to one entry. Without this a
        // 2-cell bed's foot + head both surface as separate
        // candidates and a planner that already routed to the
        // anchor still sees the "other" cell as available.
        if !seen_anchors.insert(anchor) {
            continue;
        }
        // Exclusive + taken by someone else ⇒ exclude. Non-
        // exclusive blocks ignore claims entirely (anyone may use
        // a water well at the same time).
        if i.exclusive && claims.is_taken_by_other(anchor, self_id) {
            continue;
        }
        let d = anchor - foot;
        let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
        let (need, restores) = match &i.need_restore {
            Some(nr) => (Some(nr.need.clone()), nr.restores),
            None => (None, 0.0),
        };
        out.push(NearbyInteraction {
            cell: BlockPos {
                x: anchor.x,
                y: anchor.y,
                z: anchor.z,
            },
            need,
            restores,
            exclusive: i.exclusive,
            distance,
        });
    }
    out.sort_by_key(|n| n.distance);
    out.truncate(limit);
    out
}


/// Find a standable cell adjacent to `target` that an NPC can stand on
/// while interacting with `target`. Consumables are typically solid
/// blocks the NPC can't stand *on*, so the brain pathfinds to one of
/// their neighbours instead.
///
/// Search order: same-Y cardinals first (the common case — basket on a
/// floor), then one cell up (basket on a low step), then one cell down
/// (basket on a raised platform the NPC approaches from below). Within
/// each Y, ties broken by Manhattan distance from `from` so the
/// resulting path bends toward whichever side the NPC's already
/// approaching from rather than picking an arbitrary cardinal.
fn nearest_standable_neighbor<W: Walkability>(
    target: IVec3,
    from: IVec3,
    world: &W,
) -> Option<IVec3> {
    let offsets = [
        IVec3::new(1, 0, 0),
        IVec3::new(-1, 0, 0),
        IVec3::new(0, 0, 1),
        IVec3::new(0, 0, -1),
    ];
    for dy in [0, 1, -1] {
        let mut best: Option<(IVec3, i32)> = None;
        for off in offsets {
            let cand = target + off + IVec3::new(0, dy, 0);
            if !standable(world, cand) {
                continue;
            }
            let d = cand - from;
            let dist = d.x.abs() + d.y.abs() + d.z.abs();
            match best {
                None => best = Some((cand, dist)),
                Some((_, prev)) if dist < prev => best = Some((cand, dist)),
                _ => {}
            }
        }
        if let Some((cell, _)) = best {
            return Some(cell);
        }
    }
    None
}

/// K nearest matched rooms by Manhattan distance from `foot`. Returns a
/// sorted Vec (closest first). Touches every matched room (typically a
/// handful in a small world), so it's `O(rooms)` per call — fine at
/// goal-transition cadence.
fn collect_nearby_rooms(rooms: &RoomMap, foot: IVec3, limit: usize) -> Vec<NearbyRoom> {
    let mut out: Vec<NearbyRoom> = rooms
        .iter_matched()
        .map(|(room_id, pattern, anchor)| {
            let d = anchor - foot;
            let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
            NearbyRoom {
                id: room_id.0,
                pattern: pattern.0.clone(),
                anchor: BlockPos {
                    x: anchor.x,
                    y: anchor.y,
                    z: anchor.z,
                },
                distance,
            }
        })
        .collect();
    out.sort_by_key(|r| r.distance);
    out.truncate(limit);
    out
}

/// The foot cell of an actor whose pose carries an eye-position
/// `translation`. Mirrors the player AABB derivation in
/// `apply_walk_step`: feet are `EYE_OFFSET_FROM_CENTRE + half-y`
/// below the eye.
///
/// This is the *literal* pose-floor cell. For pathfinding, prefer
/// [`pose_to_standable_foot`] — the AABB can straddle a cell boundary,
/// in which case the literal floor lands on an unsupported cell while
/// the actor is physically resting on an adjacent one.
///
/// **FP epsilon on Y.** Eject/rescue/walk-step all reconstruct
/// `pose.y = cell.y + EYE_OFFSET + HALF_Y`. The two added constants
/// don't have exact f32 representations, so `pose.y - EYE - HALF`
/// can drift below `cell.y` by ~5×10⁻⁷ at certain Y values (it
/// happens at `cell.y ∈ {1, 2, 7, 8, ...}` — anywhere the mantissa
/// rolls). Without a tolerance, `floor(7.9999995)` returns 7 and
/// the foot cell silently slips a cell below the actor's actual
/// resting cell, which then fails the standable check and traps
/// pathfinding in a loop. The 1×10⁻⁴ bias is far smaller than
/// any meaningful Y movement (1 cell = 1.0) but comfortably
/// larger than the worst-case FP drift.
fn pose_to_foot_cell(pose: &AvatarPose) -> IVec3 {
    const FOOT_Y_EPS: f32 = 1e-4;
    let feet_y = pose.translation.y - EYE_OFFSET_FROM_CENTRE - PLAYER_HALF_EXTENTS.y;
    IVec3::new(
        pose.translation.x.floor() as i32,
        (feet_y + FOOT_Y_EPS).floor() as i32,
        pose.translation.z.floor() as i32,
    )
}

/// Pick a standable foot cell beneath the NPC's body AABB. Returns
/// [`pose_to_foot_cell`] directly when that's already standable;
/// otherwise scans the (up to 4) cells the AABB's XZ extent actually
/// straddles for one that supports the actor.
///
/// **Why this exists.** Body half-extents are (0.3, _, 0.3), so the
/// AABB spans 0.6 m in XZ. When pose.x or pose.z lands near a cell
/// boundary the AABB overlaps two cells; the sweep can support the
/// actor from a block in the *adjacent* cell while their pose-floor
/// lands on a cell over a drop. The actor is physically fine — they're
/// edge-balanced — but pathfinding sees a non-standable start and
/// every plan fails, so the NPC freezes in place.
///
/// Returns `None` when none of the overlapped cells is standable (the
/// rare case of being truly mid-fall, embedded in a wall, or hovering
/// over genuinely-empty space). Callers treat that as a normal
/// "no path" outcome.
fn pose_to_standable_foot<W: Walkability>(pose: &AvatarPose, world: &W) -> Option<IVec3> {
    let nominal = pose_to_foot_cell(pose);
    if standable(world, nominal) {
        return Some(nominal);
    }
    let aabb_min_x = pose.translation.x - PLAYER_HALF_EXTENTS.x;
    let aabb_max_x = pose.translation.x + PLAYER_HALF_EXTENTS.x;
    let aabb_min_z = pose.translation.z - PLAYER_HALF_EXTENTS.z;
    let aabb_max_z = pose.translation.z + PLAYER_HALF_EXTENTS.z;
    let cx_lo = aabb_min_x.floor() as i32;
    // -ε so an AABB whose max sits exactly on an integer boundary doesn't
    // claim it overlaps the next cell (boundary-touching != overlap).
    let cx_hi = (aabb_max_x - 1e-4).floor() as i32;
    let cz_lo = aabb_min_z.floor() as i32;
    let cz_hi = (aabb_max_z - 1e-4).floor() as i32;
    for cx in cx_lo..=cx_hi {
        for cz in cz_lo..=cz_hi {
            if cx == nominal.x && cz == nominal.z {
                continue; // already checked above
            }
            let candidate = IVec3::new(cx, nominal.y, cz);
            if standable(world, candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Teleport `pose` to the nearest standable cell within
/// `max_radius_cells` (Chebyshev) of the literal foot cell. Returns
/// the target cell on success, `None` if every cell in the search
/// volume was unstandable (the actor is wedged in deep enough that
/// the rescue radius can't reach a valid floor — the caller should
/// park them and surface a warning).
///
/// **Why this exists.** The brain pathfinder bails when the NPC's
/// start cell isn't standable, and there are edge cases — a failed
/// post-interaction eject, a building dropped on the NPC mid-rest,
/// soft-actor-separation sliding them onto a non-support cell —
/// where the NPC ends up at a pose that fails both
/// [`pose_to_standable_foot`] and `standable(pose_to_foot_cell)`.
/// Without rescue, the planner loops forever picking the same goal,
/// pathfinder bails on the same unstandable start, and the warning
/// stream spams indefinitely. With rescue, the NPC pops to a
/// nearby valid cell and resumes normal planning.
///
/// **Skipped when already standable.** The first thing the function
/// does is call [`pose_to_standable_foot`] and return `None` (no
/// rescue needed) if the pose is fine. Callers can treat the
/// `Option<IVec3>` return as "did we have to move the NPC."
///
/// **Search order is Chebyshev rings, Manhattan tiebreak.** A cell
/// 1 step away is always preferred over a cell 2 steps away. Within
/// a ring the cell whose absolute integer-axis deltas sum smallest
/// (closer to the axis-aligned neighbours) wins. This biases the
/// rescue toward "drop the NPC straight down to the floor they're
/// hovering above" instead of "shove them sideways across the room."
fn rescue_to_nearby_standable<W: Walkability>(
    pose: &mut AvatarPose,
    world: &W,
    max_radius_cells: i32,
) -> Option<IVec3> {
    if pose_to_standable_foot(pose, world).is_some() {
        return None;
    }
    let centre = pose_to_foot_cell(pose);
    for d in 1..=max_radius_cells {
        let mut best: Option<(IVec3, i32)> = None;
        for dx in -d..=d {
            for dy in -d..=d {
                for dz in -d..=d {
                    let cheb = dx.abs().max(dy.abs()).max(dz.abs());
                    if cheb != d {
                        continue;
                    }
                    let cand = centre + IVec3::new(dx, dy, dz);
                    if !standable(world, cand) {
                        continue;
                    }
                    let manhattan = dx.abs() + dy.abs() + dz.abs();
                    match best {
                        None => best = Some((cand, manhattan)),
                        Some((_, prev)) if manhattan < prev => best = Some((cand, manhattan)),
                        _ => {}
                    }
                }
            }
        }
        if let Some((cell, _)) = best {
            pose.translation = standing_pose_translation(cell);
            return Some(cell);
        }
    }
    None
}

/// Horizontal centre of a foot cell — the 2D aim target for steering.
/// The brain ignores Y (the controller's gravity + step-up handles
/// vertical motion), so all path math lives in XZ.
fn waypoint_xz(cell: IVec3) -> Vec2 {
    Vec2::new(cell.x as f32 + 0.5, cell.z as f32 + 0.5)
}

/// Closest-point projection of `p` onto segment `a..b`. Returns
/// `(t, point)` with `t` clamped to `[0, 1]`.
fn closest_on_segment(p: Vec2, a: Vec2, b: Vec2) -> (f32, Vec2) {
    let ab = b - a;
    let len_sq = ab.length_squared();
    if len_sq < f32::EPSILON {
        return (0.0, a);
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    (t, a + ab * t)
}

/// Project `p` onto the segment of `path` that contains arc length
/// `min_progress` or comes after it; return the cumulative arc length
/// of the closest projection. Capping at `min_progress` is what makes
/// progress monotonic — without it the NPC briefly drifting onto an
/// earlier segment would yank progress backwards and the lookahead
/// would point the NPC at where it just came from.
///
/// If every segment falls before `min_progress`, returns the
/// path's total length (the NPC is past the end).
fn closest_progress_after(path: &[IVec3], p: Vec2, min_progress: f32) -> f32 {
    if path.len() < 2 {
        return 0.0;
    }
    let mut traversed = 0.0_f32;
    let mut best: Option<(f32, f32)> = None; // (progress, dist_sq)
    for w in path.windows(2) {
        let a = waypoint_xz(w[0]);
        let b = waypoint_xz(w[1]);
        let seg_len = (b - a).length();
        if min_progress > traversed + seg_len {
            traversed += seg_len;
            continue;
        }
        // Force the projection onto the part of the segment that's
        // at or after `min_progress`.
        let t_lo = if min_progress <= traversed {
            0.0
        } else {
            (min_progress - traversed) / seg_len
        };
        let (t_raw, _) = closest_on_segment(p, a, b);
        let t = t_raw.max(t_lo);
        let projected = a + (b - a) * t;
        let dist_sq = (p - projected).length_squared();
        let progress = traversed + t * seg_len;
        match best {
            None => best = Some((progress, dist_sq)),
            Some((_, prev)) if dist_sq < prev => best = Some((progress, dist_sq)),
            _ => {}
        }
        traversed += seg_len;
    }
    best.map(|(p, _)| p).unwrap_or(traversed)
}

/// True if the upcoming portion of `path` (within `JUMP_TRIGGER_DIST`
/// of `progress`) requires a step up from the NPC's current foot Y.
/// Lets the brain hold the jump button as the NPC approaches an
/// obstacle the path planner already decided to climb. Looks only at
/// segment ENDPOINTS — the planner's neighbour generator only
/// produces vertical changes at segment boundaries (a step-up cell
/// is the destination of one step), so the endpoint Y is what
/// matters.
fn step_up_imminent(path: &[IVec3], progress: f32, foot_y: i32) -> bool {
    if path.len() < 2 {
        return false;
    }
    let mut traversed = 0.0_f32;
    for w in path.windows(2) {
        let a = waypoint_xz(w[0]);
        let b = waypoint_xz(w[1]);
        let seg_len = (b - a).length();
        let segment_end = traversed + seg_len;
        // Skip segments already behind us.
        if segment_end <= progress {
            traversed = segment_end;
            continue;
        }
        // How far ahead of the NPC's progress this segment ends.
        let dist_to_end = segment_end - progress;
        if dist_to_end > JUMP_TRIGGER_DIST {
            // Next vertical change is too far away to act on yet.
            return false;
        }
        if w[1].y > foot_y {
            return true;
        }
        traversed = segment_end;
    }
    false
}

/// Walk `distance` further along `path` starting from arc length
/// `start_progress`; return the world XZ point at that lookahead. If
/// the lookahead runs off the end, returns the path's last waypoint.
fn lookahead_point(path: &[IVec3], start_progress: f32, distance: f32) -> Vec2 {
    if path.is_empty() {
        return Vec2::ZERO;
    }
    if path.len() == 1 {
        return waypoint_xz(path[0]);
    }
    let target = start_progress + distance;
    let mut traversed = 0.0_f32;
    for w in path.windows(2) {
        let a = waypoint_xz(w[0]);
        let b = waypoint_xz(w[1]);
        let seg_len = (b - a).length();
        if traversed + seg_len >= target {
            let t = ((target - traversed) / seg_len).clamp(0.0, 1.0);
            return a + (b - a) * t;
        }
        traversed += seg_len;
    }
    waypoint_xz(*path.last().expect("non-empty path"))
}

/// Run the same physics controller players use, against the brain-
/// written `MovementIntent`. Mirrors `server_player_step` in server.rs
/// modulo the source of the intent — NPC brain vs replicated player
/// input. Actor-vs-actor contact is handled post-physics by
/// `soft_separate_actors`, not in the sweep, so two actors can briefly
/// overlap then get pushed apart gently rather than hard-stopping at
/// contact.
pub(crate) fn npc_physics_step(
    time: Res<Time>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut npcs: Query<
        (
            &mut AvatarPose,
            &mut AvatarVelocity,
            &mut AvatarOnGround,
            &mut MovementMode,
            &MovementIntent,
        ),
        (With<Npc>, Without<KinematicLock>),
    >,
) {
    let dt = time.delta_secs();
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (mut pose, mut vel, mut on_ground, mut mode, intent) in npcs.iter_mut() {
        apply_walk_step(&mut pose, &mut vel, &mut on_ground, &mut mode, intent, dt, &world);
    }
}

/// splitmix64-style PRNG. Returns a uniform float in [0, 1). Quality
/// only has to fool human eyes scanning wander patterns.
fn rand_unit(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    let bits = (z ^ (z >> 31)) as u32;
    bits as f32 / (u32::MAX as f32 + 1.0)
}
