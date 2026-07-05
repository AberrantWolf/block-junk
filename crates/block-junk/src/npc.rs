//! NPCs — server-authoritative actors driven by a two-layer brain.
//!
//! **Layer 1 (this file, native Rust)** runs every fixed tick: decay
//! needs, advance the current goal, and hand execution to the
//! kinematic mover (`npc_mover.rs`), which walks the validated cell
//! path and reports back through the goal's `edge` cursor and
//! `blocked` flag.
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

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::haul::{HaulStore, HaulTarget};
use crate::interactables::{InteractableIndex, InteractionClaims};
use crate::items::ItemSlot;
use crate::npc_registry::{NeedRegistry, NpcKindRegistry, WorkDefaultsRes};
use crate::pathfinding::{
    NAV_BODY_HALF_EXTENT, NAV_PASSABLE_COST_MULT, Walkability, corridor_clear, find_path,
    nearest_standable_below, smooth_path, standable,
};
use crate::physics::{EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, standing_pose_translation};
use crate::plan_claims::PlanClaims;
use crate::plans::Plans;
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, Carrying, CellEdit, EquippedTool,
    KinematicLock, MovementMode, NpcAnimOverride, PlanEdit, PlanKind,
    StateSyncChannel, WorldClock, WorldItem,
};
use crate::rooms::RoomMap;
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

/// Server-only marker: a [`CellEdit`] landed inside this NPC's live
/// `MoveTo` path envelope. The brain re-validates the remaining path on
/// its next tick — repathing in place when the route broke, abandoning
/// (with the usual claim/memo cleanup) only when no route remains.
///
/// Set by [`mark_paths_dirty_on_cell_edit`] in `Update`, consumed in
/// the `FixedUpdate` brain tick. A component rather than a Message on
/// purpose: a `MessageReader` polled from `FixedUpdate` misses messages
/// on frames with no fixed tick, a marker can't be lost.
#[derive(Component)]
pub(crate) struct PathDirty;

/// Stable identifier for an NPC across save/load. Distinct from Bevy
/// `Entity` because Entity values aren't preserved across reboots.
/// Allocated server-side from the monotonic [`NpcIdAllocator`] (every
/// runtime spawn path must use it) and exposed to mods
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

/// Per-NPC rolled stat values, keyed by
/// [`NpcStatDef`](block_junk_mod_api::npcs::NpcStatDef) id. Rolled once
/// at spawn from the NPC's persisted rng, fixed for life, saved with
/// the NPC, and mirrored into `NpcSnapshot.stats` for the Lua planner.
/// Never decays — parallel to [`Needs`] in storage shape only.
#[derive(Component, Clone, Debug, Default)]
pub struct NpcStats(pub HashMap<String, f32>);

/// Roll every stat a kind declares, in registration order (declaration
/// order is what makes the roll deterministic for a given rng state).
pub fn roll_stats(
    defs: &[block_junk_mod_api::npcs::NpcStatDef],
    rng: &mut u64,
) -> HashMap<String, f32> {
    defs.iter()
        .map(|def| {
            let value = def.min + rand_unit(rng) * (def.max - def.min);
            (def.id.clone(), value)
        })
        .collect()
}

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
#[allow(
    dead_code,
    reason = "field is read by debug HUD (future) and shows up in logs today"
)]
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
    /// Walk a precomputed A* path of foot cells, executed by the
    /// kinematic mover (`npc_mover_step`), then on successful arrival
    /// run `on_arrive`. `edge` is the mover's cursor: the index of the
    /// waypoint currently being departed (edge `e` traverses
    /// `path[e] -> path[e+1]`); the cursor sitting on the last waypoint
    /// IS the arrival condition — the mover lands bodies exactly, so
    /// there is no arrive radius and no settle check. A one-waypoint
    /// path is an instant arrival by construction.
    ///
    /// `blocked` is the single execution-failure channel: the mover
    /// sets it when an edge fails its per-tick oracle re-check or a
    /// fall lands the body somewhere off-path, and the brain answers
    /// by repathing in place (claims kept — the target didn't become
    /// unreachable just because the route changed) or abandoning
    /// through the usual cleanup when no route remains. Abandonment
    /// skips `on_arrive` — only a clean arrival fires it.
    MoveTo {
        path: Vec<IVec3>,
        edge: usize,
        blocked: bool,
        deadline_secs: f32,
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
    /// Lying on the ground sleeping — the planner's no-bed fallback
    /// ([`PlannerGoal::SleepGround`]). Restores `need` continuously at
    /// `restore_per_sec` while counting down, so an interruption keeps
    /// whatever was already slept. Ends early once the need is fully
    /// restored. Not preempt-eligible (it IS the survival response),
    /// holds no claims, and doesn't lock the body — like `Resting`,
    /// just horizontal.
    SleepingGround {
        remaining_secs: f32,
        need: String,
        restore_per_sec: f32,
        /// Clip id for the client override (vanilla: the lie idle).
        animation: Option<String>,
    },
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
    /// Parked at a craft station, registered as the active worker
    /// while the server's `tick_station_work` drives the order to
    /// completion. Phase 6c-A. Entered only via a successful arrival
    /// on [`ArrivalAction::WorkStation`].
    ///
    /// Unlike [`Goal::Working`] there's no NPC-side timer — the
    /// progress lives on the station's `active_work` field. The brain
    /// holds position with zero `MovementIntent` and checks each tick
    /// whether work remains (active_work present OR a queued order
    /// the inventory satisfies). When neither is true the brain
    /// returns to Idle, unregisters from `ActiveWorkers`, and releases
    /// its `CraftAssignment` for the next NPC to take.
    ///
    /// Order-to-order continuation happens server-side: when
    /// `tick_station_work` completes one order and the next queued
    /// order's inputs are still satisfied, it auto-starts the next
    /// `active_work` so the NPC keeps crafting without re-routing.
    CraftingAtStation { station_cell: IVec3 },
}

impl Goal {
    /// The one way to build a [`Goal::MoveTo`]: mover cursor at the
    /// first edge, nothing blocked yet. Centralized so the variant's
    /// bookkeeping fields can change shape without touching every
    /// planner dispatch site.
    fn move_to(
        path: Vec<IVec3>,
        deadline_secs: f32,
        on_arrive: ArrivalAction,
        snap: Option<UseSlotSnap>,
    ) -> Self {
        Goal::MoveTo {
            path,
            edge: 0,
            blocked: false,
            deadline_secs,
            on_arrive,
            snap,
        }
    }
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
    DepositAtPlan { plan_cell: IVec3 },
    /// Prereq leg of a haul cycle: arrive at a reserved tool item
    /// and equip it. Phase 5b — created by the scheduler when an NPC
    /// is assigned to a plan whose `work_action.required_tool`
    /// doesn't match the NPC's current `EquippedTool`. On arrival:
    /// validate the WorldItem still matches the expected slot,
    /// swap into the tool slot with the displaced tool (if any)
    /// dropping at the picked-up item's old position, clear the
    /// assignment's `pending_tool`, then continue with the next
    /// leg (material fetch).
    PickupTool {
        item_entity: Entity,
        item_slot: ItemSlot,
    },
    /// Arrive at a craft station and begin work. Phase 6c-A — created
    /// by the craft scheduler when an NPC has the right tool (if
    /// any) and the station has a queued order whose inputs the
    /// inventory satisfies. On arrival: find the first satisfiable
    /// order, consume its inputs, create `active_work`, register the
    /// NPC entity in `ActiveWorkers`, transition the brain to
    /// [`Goal::CraftingAtStation`].
    ///
    /// If by arrival no order is still satisfiable (player Cancel,
    /// another worker stole the inputs), release the
    /// `CraftAssignment` and return to Idle silently.
    WorkStation { station_cell: IVec3 },
    /// Final leg of a station-haul cycle: arrive at a craft station
    /// and drain the NPC's full carry stack into the station's
    /// `inventory`. Phase 6c (station haul) — issued by the haul
    /// scheduler when a station's queued orders have unmet inputs and
    /// the NPC is carrying a matching item. Mirrors `DepositAtPlan`
    /// in shape; the only difference is the deposit target
    /// (CraftStations vs Plans) and the broadcast (StationUpdate vs
    /// PlanEdit).
    ///
    /// If by arrival the station is gone (block destroyed) or no
    /// longer wants the carry's item kind, the haul releases without
    /// depositing — same convention as DepositAtPlan.
    DepositAtStation { station_cell: IVec3 },
}

/// Native-side brain state. Holds the current goal + a tiny PRNG seed
/// for reproducible target selection.
#[derive(Component, Clone, Debug)]
pub struct Brain {
    pub goal: Goal,
    /// splitmix-seeded PRNG state. Per-NPC so two NPCs spawned the same
    /// tick don't pick identical wander targets.
    pub rng: u64,
    /// Civilization cluster this NPC calls home. NPCs without a claim
    /// (`None`) wander freely; claimed NPCs sample wander targets inside
    /// the cluster's inflated bbox so they don't drift into the
    /// wilderness. Provisional claim mechanic: an NPC claims whatever
    /// cluster contains the cell it just slept in. Claims aren't saved
    /// across world reloads — `ClusterId` isn't stable across restarts;
    /// the NPC re-claims on next sleep.
    pub home_cluster: Option<crate::civilization::ClusterId>,
    /// Seconds until the next survival preempt may fire. Set after
    /// every preempt so an *unsatisfiable* critical need (exhausted at
    /// noon when sleep is night-gated; starving with no food in range)
    /// can't thrash abort→plan→claim→A*→abort at full tick rate — the
    /// NPC works in [`PREEMPT_RETRY_COOLDOWN_SECS`] chunks until the
    /// need becomes satisfiable. Runtime-only; not persisted.
    pub preempt_cooldown_secs: f32,
}

/// Minimum spacing between survival preempts for one NPC. See
/// [`Brain::preempt_cooldown_secs`].
const PREEMPT_RETRY_COOLDOWN_SECS: f32 = 5.0;

/// How long a haul target whose last leg failed pathfinding stays
/// memoized as unreachable before the scheduler may retry it. Without
/// the memo, a nearby item in a pit (or a walled-off plan) pins its
/// nearest hauler in a deterministic assign→path-fail→release loop
/// forever — the NPC never hauls anything else.
const HAUL_UNREACHABLE_RETRY_SECS: f32 = 30.0;

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
        app.init_resource::<NpcIdAllocator>();
        app.init_resource::<SmokeClusterSpawned>();
        // Spawn deferred to first client connect rather than Startup —
        // chunks aren't loaded until a client's AoI requests them, and
        // an NPC spawned into an empty world falls past unloaded chunks
        // forever (no candidates to collide against).
        app.add_observer(spawn_initial_npc_on_first_connect);
        // Lifecycle backstop: ANY NPC despawn (death, debug removal, a
        // mod) releases every claim/booking/reservation keyed by its
        // id. Without this a removed NPC's bed/plan/station claims and
        // item reservations would lock permanently — every claim table
        // assumes this observer exists.
        app.add_observer(release_claims_on_npc_despawn);
        // Local-bus message: brain emits on Working timer completion;
        // server-side consumer (in server.rs) applies the underlying
        // BlockEdit + clears the plan tag. Splits these concerns so the
        // brain tick stays under the SystemParam cap.
        app.add_message::<NpcWorkCompleted>();
        // Brain → mover order matters: the brain commits/advances goals
        // this tick, the kinematic mover executes them, and the brain
        // reads the results (edge cursor, blocked flag) next tick. Both
        // run in FixedUpdate alongside the player physics so all actors
        // advance together.
        app.add_systems(
            FixedUpdate,
            (npc_brain_tick, crate::npc_mover::npc_mover_step).chain(),
        );
        // Activity is derived from Goal and replicated to drive client
        // animation. Updates after the brain tick so the broadcast
        // reflects the just-decided goal.
        app.add_systems(FixedUpdate, refresh_npc_activity.after(npc_brain_tick));
        // Settlement-arc S1 diagnostics — read-only census; Update is
        // fine (it samples, it doesn't steer).
        app.add_systems(Update, npc_census);
    }
}

/// Seconds between census log lines.
const NPC_CENSUS_INTERVAL_SECS: f32 = 5.0;
/// A `MoveTo` NPC that displaced less than this since the last census
/// reads as "not actually moving."
const NPC_CENSUS_MOVE_EPSILON_M: f32 = 0.25;

/// Settlement-arc S1 instrumentation: periodically bucket NPCs by goal
/// variant and count movers that aren't moving. The 2026-05-18 playtest
/// ended with "most of my dudes ended up stuck" and no way to tell WHICH
/// failure mode accumulated (embedded body, impossible path, disabled
/// brain, corner-fight) — this log identifies the growing bucket so the
/// fix targets the right cause. Read-only; safe to leave on.
fn npc_census(
    time: Res<Time>,
    mut last_run: Local<f32>,
    mut last_positions: Local<HashMap<u64, Vec3>>,
    npcs: Query<(&NpcId, &Brain, &AvatarPose, Has<BrainDisabled>), With<Npc>>,
) {
    let now = time.elapsed_secs();
    if now - *last_run < NPC_CENSUS_INTERVAL_SECS {
        return;
    }
    *last_run = now;
    if npcs.is_empty() {
        return;
    }
    let (mut idle, mut moving, mut resting, mut interacting, mut working, mut crafting) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut disabled = 0u32;
    let mut movers_not_moving = 0u32;
    let mut movers_blocked = 0u32;
    let mut positions: HashMap<u64, Vec3> = HashMap::with_capacity(last_positions.len());
    for (id, brain, pose, is_disabled) in &npcs {
        if is_disabled {
            disabled += 1;
        }
        let displaced = last_positions
            .get(&id.0)
            .map(|prev| prev.distance(pose.translation));
        positions.insert(id.0, pose.translation);
        match &brain.goal {
            Goal::Idle => idle += 1,
            Goal::MoveTo { blocked, .. } => {
                moving += 1;
                if *blocked {
                    movers_blocked += 1;
                }
                if displaced.is_some_and(|d| d < NPC_CENSUS_MOVE_EPSILON_M) {
                    movers_not_moving += 1;
                }
            }
            Goal::Resting { .. } | Goal::SleepingGround { .. } => resting += 1,
            Goal::Interacting { .. } => interacting += 1,
            Goal::Working { .. } => working += 1,
            Goal::CraftingAtStation { .. } => crafting += 1,
        }
    }
    *last_positions = positions;
    info!(
        "npc census: {} | idle={idle} move={moving} rest={resting} interact={interacting} \
         work={working} craft={crafting} | disabled={disabled} \
         movers_not_moving={movers_not_moving} movers_blocked={movers_blocked}",
        npcs.iter().count(),
    );
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
            Goal::SleepingGround { animation, .. } => NpcAnimOverride(animation.clone()),
            Goal::Working { .. } => {
                NpcAnimOverride(kinds.get(&kind.0).map(|k| k.animations.work.clone()))
            }
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

/// Observer: an NPC entity is despawning — release everything keyed by
/// its id or entity across the four claim tables. Components are still
/// readable inside a `Remove` observer, so the id lookup works.
fn release_claims_on_npc_despawn(
    trigger: On<Remove, Npc>,
    ids: Query<&NpcId>,
    mut plan_claims: ResMut<crate::plan_claims::PlanClaims>,
    mut interaction_claims: ResMut<crate::interactables::InteractionClaims>,
    mut haul_store: ResMut<crate::haul::HaulStore>,
    mut bookings: ResMut<crate::craft_stations::CraftBookings>,
) {
    let Ok(&id) = ids.get(trigger.entity) else {
        return;
    };
    plan_claims.release_all_for(id);
    interaction_claims.release_all_for(id);
    haul_store.release_for_npc(id);
    bookings.release_npc_booking(id, trigger.entity);
    info!(npc = id.0, "npc despawned; released all claims");
}

/// Server-side monotonic [`NpcId`] source. EVERY runtime NPC spawn must
/// allocate from here — ids key the claim tables (plans, interactions,
/// hauls, craft bookings), so two NPCs sharing an id silently share
/// claims. `load_from_save` bumps the counter past the highest saved id
/// before any runtime spawn can fire.
#[derive(Resource)]
pub struct NpcIdAllocator {
    next: u64,
}

impl Default for NpcIdAllocator {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl NpcIdAllocator {
    pub fn allocate(&mut self) -> NpcId {
        let id = NpcId(self.next);
        self.next += 1;
        id
    }

    /// Bump the counter past an id already in use (saved NPCs keep
    /// their persisted ids). Idempotent; safe to call per loaded NPC.
    pub fn reserve_through(&mut self, used: u64) {
        self.next = self.next.max(used + 1);
    }
}

/// Once-per-session latch for the smoke-test cluster. A `Resource` (not
/// a process-wide static) so a future in-process server restart starts
/// fresh with its own App state.
#[derive(Resource, Default)]
struct SmokeClusterSpawned(bool);

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
    mut spawned: ResMut<SmokeClusterSpawned>,
    mut allocator: ResMut<NpcIdAllocator>,
) {
    if std::mem::replace(&mut spawned.0, true) {
        return;
    }
    // If a save was loaded at startup, NPCs already exist with their
    // persisted ids — the cluster would just pile more bodies onto the
    // spawn point. Skip silently; the latch above still holds so
    // subsequent reconnects don't re-attempt.
    if !existing.is_empty() {
        info!("NPCs already present (loaded from save); skipping smoke-test cluster spawn");
        return;
    }
    let kind_id = "vanilla:wanderer";
    let (default_needs, stat_defs) = match kinds.get(kind_id) {
        Some(def) => (def.default_needs.clone(), def.stats.clone()),
        None => {
            warn!(
                kind = kind_id,
                "no NPC kind registered; spawning with empty needs (native fallback brain)"
            );
            (HashMap::new(), Vec::new())
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
    for translation in cluster.into_iter() {
        let id = allocator.allocate();
        // Roll stats from the same rng the brain keeps: the roll
        // advances the state, and the advanced state goes into Brain so
        // save/load reproduces neither the roll nor the wander stream.
        let mut rng = 0xDEAD_BEEF_CAFE_F00D ^ id.0;
        let stats = roll_stats(&stat_defs, &mut rng);
        // Nested tuples work around Bevy's 15-element Bundle cap. Two
        // groups: identity/brain (cheap markers + structured state) and
        // physics + replication (per-frame state + lightyear).
        commands.spawn((
            (
                Actor,
                Npc,
                id,
                NpcKind(kind_id.into()),
                Needs(default_needs.clone()),
                NpcStats(stats),
                Brain {
                    goal: Goal::Idle,
                    rng,
                    home_cluster: None,
                    preempt_cooldown_secs: 0.0,
                },
                crate::protocol::Carrying::default(),
                crate::protocol::EquippedTool::default(),
            ),
            AvatarPose {
                translation,
                yaw: 0.0,
            },
            AvatarVelocity::default(),
            AvatarOnGround::default(),
            MovementMode::Walk,
            crate::npc_mover::NavMover::default(),
            NpcPath::default(),
            NpcAnimOverride::default(),
            Replicate::to_clients(NetworkTarget::All),
            InterpolationTarget::to_clients(NetworkTarget::All),
            Name::new(format!("npc:{}", id.0)),
        ));
    }
    info!(
        kind = kind_id,
        count = cluster.len(),
        "spawned smoke-test NPC cluster"
    );
}

/// Adapter that lets pathfinding query the live world. Treats unloaded
/// chunks as solid so the search doesn't commit to a path through
/// territory whose contents we don't know.
///
/// Exposed `pub(crate)` so other server systems (drop placement, etc.)
/// can reuse the same standable check the brain uses — keeps "where
/// does a body fit" logic in one place.
pub(crate) struct WorldWalk<'q, 'w, 's> {
    pub(crate) chunks: &'q Query<'w, 's, &'static Chunk>,
    pub(crate) chunk_map: &'q ChunkMap,
    pub(crate) registry: &'q BlockRegistry,
}

impl<'q, 'w, 's> WorldWalk<'q, 'w, 's> {
    /// Slot occupying `cell`, or `None` when the owning chunk isn't
    /// loaded. Every `Walkability` answer funnels through this so the
    /// unloaded-chunk rule stays in one place.
    fn slot_at(&self, cell: IVec3) -> Option<BlockSlot> {
        let (coord, local) = world_to_chunk(cell);
        let &entity = self.chunk_map.0.get(&coord)?;
        let chunk = self.chunks.get(entity).ok()?;
        Some(chunk.get(local))
    }
}

impl<'q, 'w, 's> Walkability for WorldWalk<'q, 'w, 's> {
    fn is_solid(&self, cell: IVec3) -> bool {
        let Some(slot) = self.slot_at(cell) else {
            // Unloaded chunk: solid, so the search doesn't commit to
            // paths through unknown territory.
            return true;
        };
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

    fn blocks_body(&self, cell: IVec3) -> bool {
        let Some(slot) = self.slot_at(cell) else {
            return true;
        };
        if slot.is_empty() {
            return false;
        }
        // Nav-passable furniture (beds): the body may occupy the cell,
        // at a cost — see `cost` below. `is_solid` stays true for these
        // cells, so they still support standing on top and stop items.
        let flags = &self.registry.def(slot).flags;
        !flags.walkable_boundary && !flags.nav_passable
    }

    fn cost(&self, cell: IVec3) -> f32 {
        // Occupied nav-passable cells cost extra so A* prefers any
        // reasonable aisle over cutting through furniture. Future road
        // tags hook in here without changing the algorithm.
        match self.slot_at(cell) {
            Some(slot) if !slot.is_empty() && self.registry.def(slot).flags.nav_passable => {
                NAV_PASSABLE_COST_MULT
            }
            _ => 1.0,
        }
    }
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
/// SystemParam bundle for the craft-station scheduler + arrival
/// handlers (Phase 6c-A). Folded into one slot to keep the brain tick
/// under Bevy 0.18's 16-SystemParam ceiling — same reason
/// [`HaulCtx`] is bundled. Group is "everything the craft scheduler
/// and the WorkStation arrival need to mutate or read."
#[derive(bevy::ecs::system::SystemParam)]
struct CraftCtx<'w> {
    stations: ResMut<'w, crate::craft_stations::CraftStations>,
    bookings: ResMut<'w, crate::craft_stations::CraftBookings>,
    recipes: Res<'w, crate::recipes::RecipeRegistry>,
}

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
    store: ResMut<'w, HaulStore>,
    broadcast: ServerMultiMessageSender<'w, 's>,
    servers: Query<'w, 's, &'static Server>,
    world_items: Query<'w, 's, (Entity, &'static WorldItem)>,
    kind_registry: Res<'w, NpcKindRegistry>,
    item_registry: Res<'w, crate::items::ItemRegistry>,
}

/// Read-only world-anchor bundle: room registry plus civilization
/// clusters and their tuning params. Folded so we can swap out the
/// previous top-level `room_map` slot for one bundle slot that carries
/// civ too, without busting the SystemParam ceiling.
#[derive(bevy::ecs::system::SystemParam)]
struct WorldAnchorsCtx<'w> {
    rooms: Res<'w, RoomMap>,
    civilization: Res<'w, crate::civilization::Civilization>,
    civ_params: Res<'w, crate::civilization::CivilizationParamsRes>,
}

/// Read-only world state the brain tick needs for pathing, reachability,
/// and validating that targets still exist. Kept together so gameplay
/// additions can grow this context without pushing `npc_brain_tick` back
/// toward Bevy's 16-SystemParam ceiling.
#[derive(bevy::ecs::system::SystemParam)]
struct BrainWorldCtx<'w, 's> {
    time: Res<'w, Time>,
    chunks: Query<'w, 's, &'static Chunk>,
    chunk_entities: Query<'w, 's, &'static ChunkEntities>,
    chunk_map: Res<'w, ChunkMap>,
    block_registry: Res<'w, BlockRegistry>,
    clock: Res<'w, WorldClock>,
}

impl<'w, 's> BrainWorldCtx<'w, 's> {
    fn walk<'a>(&'a self) -> WorldWalk<'a, 'w, 's> {
        WorldWalk {
            chunks: &self.chunks,
            chunk_map: &self.chunk_map,
            registry: &self.block_registry,
        }
    }
}

type BrainNpcQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static NpcId,
        &'static mut AvatarPose,
        &'static mut Needs,
        &'static mut Brain,
        &'static mut NpcPath,
        &'static mut Carrying,
        &'static mut EquippedTool,
        &'static NpcKind,
        &'static NpcStats,
        Has<KinematicLock>,
        Has<PathDirty>,
    ),
    (With<Npc>, Without<BrainDisabled>),
>;

type CleanPathNpcQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static Brain), (With<Npc>, Without<PathDirty>)>;

/// `Update`-schedule consumer of the [`CellEdit`] bus: flag every NPC
/// whose live `MoveTo` path envelope contains an edited cell with
/// [`PathDirty`]. Deliberately coarse — a cell-AABB test, no oracle
/// calls — because the precise re-validation runs in the brain tick
/// where the `WorldWalk` oracle already exists. Runs after
/// `receive_block_edits` like the other bus consumers in `server.rs`.
pub(crate) fn mark_paths_dirty_on_cell_edit(
    mut reader: MessageReader<CellEdit>,
    npcs: CleanPathNpcQuery,
    mut commands: Commands,
) {
    let edited: Vec<IVec3> = reader.read().map(|edit| edit.world).collect();
    if edited.is_empty() {
        return;
    }
    for (entity, brain) in npcs.iter() {
        let Goal::MoveTo { path, .. } = &brain.goal else {
            continue;
        };
        if path_envelope_hit(path, &edited) {
            commands.entity(entity).insert(PathDirty);
        }
    }
}

/// True if any edited cell lands inside the path's cell AABB inflated
/// by ±1 in XZ (smoothed corridors and body width stay within one cell
/// of the waypoint bounding box) and by [-1, +2] in Y (support cells
/// below; head and step-up clearance above).
fn path_envelope_hit(path: &[IVec3], edited: &[IVec3]) -> bool {
    let Some(&first) = path.first() else {
        return false;
    };
    let (mut lo, mut hi) = (first, first);
    for &cell in path {
        lo = lo.min(cell);
        hi = hi.max(cell);
    }
    let lo = lo - IVec3::new(1, 1, 1);
    let hi = hi + IVec3::new(1, 2, 1);
    edited
        .iter()
        .any(|edit| edit.cmpge(lo).all() && edit.cmple(hi).all())
}

/// Goals the preempt check considers abortable. Goals the Lua planner
/// itself picks under critical need (Interact, Wander, Goto, Rest) are
/// excluded — preempting Interact would yank the NPC off the action
/// addressing the need, and preempting Wander/Goto/Rest would thrash
/// the planner once per tick when no survival target is in range.
///
/// Pure for testability; the brain tick consults it and only then
/// reaches for the side-effecting helpers below.
pub(crate) fn preempt_eligible(goal: &Goal) -> bool {
    match goal {
        Goal::Working { .. } => true,
        Goal::CraftingAtStation { .. } => true,
        Goal::MoveTo { on_arrive, .. } => matches!(
            on_arrive,
            ArrivalAction::Work { .. }
                | ArrivalAction::PickupForPlan { .. }
                | ArrivalAction::DepositAtPlan { .. }
                | ArrivalAction::DepositAtStation { .. }
                | ArrivalAction::PickupTool { .. }
                | ArrivalAction::WorkStation { .. }
        ),
        _ => false,
    }
}

/// Release every claim, reservation, and assignment the NPC was
/// holding for this goal. Pure side-effect on the four resource maps
/// — no Bevy Commands, no world-state mutation — so it round-trips
/// in unit tests without booting the full brain tick.
///
/// Coverage parallels [`preempt_current_goal`]:
/// - `MoveTo { Interact { exclusive: true } }` / `Interacting { exclusive: true }` ⇒ release interaction claim
/// - `MoveTo { Work }` / `Working` ⇒ release plan claim
/// - `MoveTo { Pickup* / Deposit* }` ⇒ release haul assignment + all its reservations
/// - Idle / Resting / non-exclusive interactions ⇒ no-op
pub(crate) fn preempt_release_holds(
    npc_id: NpcId,
    goal: &Goal,
    plan_claims: &mut PlanClaims,
    interaction_claims: &mut InteractionClaims,
    haul_store: &mut HaulStore,
) {
    match goal {
        Goal::Idle | Goal::Resting { .. } | Goal::SleepingGround { .. } => {}
        Goal::MoveTo { on_arrive, .. } => match on_arrive {
            ArrivalAction::Interact {
                anchor_cell,
                exclusive: true,
                ..
            } => {
                interaction_claims.release(*anchor_cell, npc_id);
            }
            ArrivalAction::Work { target_cell, .. } => {
                plan_claims.release(*target_cell, npc_id);
            }
            ArrivalAction::PickupForPlan { .. }
            | ArrivalAction::DepositAtPlan { .. }
            | ArrivalAction::DepositAtStation { .. }
            | ArrivalAction::PickupTool { .. } => {
                haul_store.release_for_npc(npc_id);
            }
            ArrivalAction::WorkStation { .. } => {
                // CraftAssignment release handled by the calling
                // brain-tick site (it has access to CraftAssignments
                // + ActiveWorkers, neither of which lives in this
                // pure helper's signature). See preempt-craft task.
            }
            _ => {}
        },
        Goal::Interacting {
            anchor_cell,
            exclusive: true,
            ..
        } => {
            interaction_claims.release(*anchor_cell, npc_id);
        }
        Goal::Interacting { .. } => {}
        Goal::Working { target_cell, .. } => {
            plan_claims.release(*target_cell, npc_id);
        }
        Goal::CraftingAtStation { .. } => {
            // CraftAssignment + ActiveWorkers release handled by the
            // brain-tick caller. Same reason as ArrivalAction::WorkStation
            // above — those resources aren't in this helper's scope.
        }
    }
}

/// Cleanly abort whatever the NPC is doing right now and drop them to
/// `Goal::Idle`. Used by the preempt check when a need crosses its
/// `preempt_threshold` — the next planner call (same tick, since we're
/// Idle) routes the NPC to Rest / Interact based on the high need
/// value.
///
/// Delegates claim/reservation release to [`preempt_release_holds`],
/// then handles the side-effects that need a Bevy context: ejecting
/// a `KinematicLock`-ed `Interacting` body to a standable cell and
/// dropping the lock so physics resumes from a sensible position.
///
/// No need-restore is applied — preempt is the "I gave up before
/// finishing" path; only completion credits the need. Path is cleared
/// so the next goal commits cleanly.
///
/// Phase 6c will add a `Goal::CraftingAtStation` arm that refunds
/// consumed recipe inputs back to the station's inventory.
#[allow(
    clippy::too_many_arguments,
    reason = "bundles cleanup-target refs from the brain tick's hot loop"
)]
fn preempt_current_goal(
    npc_id: NpcId,
    entity: Entity,
    is_locked: bool,
    brain: &mut Brain,
    npc_path: &mut NpcPath,
    pose: &mut AvatarPose,
    plan_claims: &mut PlanClaims,
    interaction_claims: &mut InteractionClaims,
    haul_store: &mut HaulStore,
    commands: &mut Commands,
    world: &WorldWalk,
    chunks: &Query<&'static Chunk>,
    chunk_entities_q: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
) {
    if matches!(brain.goal, Goal::Idle) {
        return;
    }
    preempt_release_holds(
        npc_id,
        &brain.goal,
        plan_claims,
        interaction_claims,
        haul_store,
    );
    // Eject + unlock only applies when the body is currently snapped
    // into a use-slot mid-interaction. Working / MoveTo / Resting
    // never set KinematicLock, so this branch is effectively
    // Interacting-only — but keying off `is_locked` rather than the
    // Goal variant keeps the helper symmetric with the post-completion
    // eject site below.
    if is_locked {
        if let Goal::Interacting { target_cell, .. } = &brain.goal {
            let slot = slot_at_cell(*target_cell, chunks, chunk_map, block_registry);
            let (anchor, orientation) =
                resolve_anchor_with_orientation(*target_cell, chunk_entities_q, chunk_map);
            if !try_eject_to_cells(
                pose,
                eject_candidates_for_slot(slot.as_ref(), anchor, orientation),
                world,
            ) {
                warn!(
                    npc = npc_id.0,
                    anchor = ?anchor.to_array(),
                    "preempt eject: no standable approach; NPC may be embedded",
                );
            }
            commands.entity(entity).remove::<KinematicLock>();
        }
    }
    npc_path.0.clear();
    brain.goal = Goal::Idle;
}

#[allow(
    clippy::too_many_arguments,
    reason = "brain tick spans many subsystems"
)]
fn npc_brain_tick(
    world_ctx: BrainWorldCtx,
    mods: Res<ServerMods>,
    need_registry: Res<NeedRegistry>,
    work_defaults: Res<WorkDefaultsRes>,
    anchors: WorldAnchorsCtx,
    interactable_index: Res<InteractableIndex>,
    mut interaction_claims: ResMut<InteractionClaims>,
    mut haul: HaulCtx,
    mut craft: CraftCtx,
    mut commands: Commands,
    mut npcs: BrainNpcQuery,
) {
    let dt = world_ctx.time.delta_secs();
    let now_secs = world_ctx.time.elapsed_secs();
    let world = world_ctx.walk();
    let chunks = &world_ctx.chunks;
    let chunk_entities_q = &world_ctx.chunk_entities;
    let chunk_map = &world_ctx.chunk_map;
    let block_registry = &world_ctx.block_registry;
    let world_clock = *world_ctx.clock;

    let server = haul.servers.single().ok();

    for (
        entity,
        npc_id,
        mut pose,
        mut needs,
        mut brain,
        mut npc_path,
        mut carrying,
        mut equipped_tool,
        kind,
        stats,
        is_locked,
        path_dirty,
    ) in npcs.iter_mut()
    {
        // Phase 1: decay every subscribed need by its registry-defined
        // rate. Unknown ids decay at 0 (the rate-lookup returns 0) so
        // an NPC carrying a stale need from before a mod reload won't
        // crash — it just freezes that value.
        //
        // Two passes because of `decay_boost` (starvation coupling —
        // e.g. hunger past its trigger multiplies sleep's decay):
        // effective rates are computed against the pre-decay values
        // so map iteration order can't change the outcome. The alloc
        // is a few (String, f32) pairs per NPC per tick — noise next
        // to the registry lookups the old single pass already did.
        let effective_decay: Vec<(String, f32)> = needs
            .0
            .keys()
            .map(|id| {
                let mut decay = need_registry.decay_per_sec(id);
                if let Some(boost) = need_registry.decay_boost(id)
                    && needs.0.get(&boost.need).copied().unwrap_or(0.0) >= boost.above
                {
                    decay *= boost.multiplier;
                }
                (id.clone(), decay)
            })
            .collect();
        for (id, decay) in effective_decay {
            if let Some(value) = needs.0.get_mut(&id) {
                *value = (*value + decay * dt).clamp(0.0, 1.0);
            }
        }

        // Phase 1.5: preempt check. If any need crossed its data-defined
        // `preempt_threshold` AND the NPC is doing something a survival-
        // aware planner wouldn't pick under critical need (work / haul),
        // abort to Idle so the planner re-routes them this same tick. We
        // *don't* preempt goals the planner itself picks at urge (Rest,
        // Interact, Wander, Goto) — preempting an Interact would yank
        // the NPC off the very action that addresses the need, and
        // preempting Wander/Goto would thrash 60 Hz when no survival
        // interactable is in range. The mark-preempted flag suppresses
        // the haul scheduler at the Idle entry below so a freshly-
        // aborted hauler doesn't get instantly reassigned to another
        // haul before the planner gets a turn.
        let mut preempted_this_tick = false;
        brain.preempt_cooldown_secs = (brain.preempt_cooldown_secs - dt).max(0.0);
        if brain.preempt_cooldown_secs <= 0.0 && preempt_eligible(&brain.goal) {
            let crossed = needs.0.iter().find_map(|(id, value)| {
                need_registry
                    .preempt_threshold(id)
                    .filter(|threshold| *value >= *threshold)
                    .map(|threshold| (id.clone(), *value, threshold))
            });
            if let Some((need_id, value, threshold)) = crossed {
                info!(
                    npc = npc_id.0,
                    need = %need_id,
                    value = value,
                    threshold = threshold,
                    "preempt: aborting current goal for survival",
                );
                // Craft-specific release happens BEFORE the generic
                // preempt path mutates brain.goal — once goal is
                // overwritten to Idle, we can't read the
                // station_cell back out. The Active worker slot is
                // freed too so paused active_work doesn't trap the
                // station behind a no-longer-present worker.
                let craft_station_cell = match &brain.goal {
                    Goal::CraftingAtStation { station_cell } => Some(*station_cell),
                    Goal::MoveTo {
                        on_arrive: ArrivalAction::WorkStation { station_cell },
                        ..
                    } => Some(*station_cell),
                    _ => None,
                };
                preempt_current_goal(
                    *npc_id,
                    entity,
                    is_locked,
                    &mut brain,
                    &mut npc_path,
                    &mut pose,
                    &mut haul.plan_claims,
                    &mut interaction_claims,
                    &mut haul.store,
                    &mut commands,
                    &world,
                    chunks,
                    chunk_entities_q,
                    chunk_map,
                    block_registry,
                );
                if let Some(_cell) = craft_station_cell {
                    // Atomic cleanup: drops the NPC's booking AND
                    // releases the worker slot if it was this NPC's
                    // entity. Don't refund consumed inputs —
                    // active_work persists so any future worker
                    // (player or another NPC) can resume the
                    // in-progress craft. Preempt is a pause, not a
                    // cancel.
                    craft.bookings.release_npc_booking(*npc_id, entity);
                }
                preempted_this_tick = true;
                brain.preempt_cooldown_secs = PREEMPT_RETRY_COOLDOWN_SECS;
            }
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
        // Set by the CraftingAtStation arm when the brain detects
        // end-conditions (station out of work, lost worker slot, or
        // assignment cleared). Consumed below to release the
        // CraftAssignment + ActiveWorker entry and drop back to Idle.
        let mut crafting_done_at: Option<IVec3> = None;
        match &mut brain.goal {
            Goal::Idle => {}
            Goal::Resting { remaining_secs } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    rest_done = true;
                }
            }
            Goal::SleepingGround {
                remaining_secs,
                need,
                restore_per_sec,
                ..
            } => {
                *remaining_secs -= dt;
                // Continuous restore: fights the same decay the Phase-1
                // loop above just applied, so the *net* rate is
                // restore - decay. Fully rested (or timer up) → back to
                // Idle; the planner re-issues another slice if the NPC
                // is still tired, which is what lets a ground sleeper
                // notice a freshly-placed bed between naps.
                let mut fully_rested = false;
                if let Some(value) = needs.0.get_mut(need) {
                    *value = (*value - *restore_per_sec * dt).max(0.0);
                    fully_rested = *value <= 0.0;
                }
                if *remaining_secs <= 0.0 || fully_rested {
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
            Goal::CraftingAtStation { station_cell } => {
                // No NPC-side timer — the station's `active_work`
                // ticks server-side. End-condition: leave when I'm no
                // longer the registered worker (someone else took the
                // station, or I got unregistered via Cancel) OR the
                // station has nothing left to work on (`active_work`
                // is None AND no satisfiable queued order). Task #14's
                // auto-continue handles "active_work completes →
                // start next order" so a transient None state during
                // one tick is fine; a None state with nothing to
                // start is the real end.
                let station_cell_local = *station_cell;
                let still_me = craft.bookings.worker_at(station_cell_local) == Some(entity);
                let still_assigned =
                    craft.bookings.station_of_npc(*npc_id) == Some(station_cell_local);
                let work_in_progress = craft
                    .stations
                    .get(station_cell_local)
                    .and_then(|s| s.active_work.as_ref())
                    .is_some();
                if !still_me || !still_assigned || !work_in_progress {
                    crafting_done_at = Some(station_cell_local);
                }
            }
            Goal::MoveTo {
                path,
                edge,
                blocked,
                deadline_secs,
                on_arrive,
                snap,
            } => {
                // A world edit landed in this path's envelope since the
                // last tick. Re-validate the not-yet-walked portion:
                // still clear → keep walking; broken → same repath flow
                // as a mover block below.
                if path_dirty {
                    commands.entity(entity).remove::<PathDirty>();
                    if !remaining_path_valid(path, *edge, &world) {
                        *blocked = true;
                    }
                }

                // The single execution-failure channel: the mover (or
                // the re-validation above) flagged the path. Repath in
                // place — the *target* isn't unreachable just because a
                // cell en route changed, so claims/memos stay untouched
                // — and only when no route remains fall into the normal
                // abandon machinery, which memoizes and releases.
                if *blocked {
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    let goal_cell = *path.last().expect("path non-empty");
                    let repath =
                        find_path(foot, goal_cell, &world, ASTAR_NODE_BUDGET, ASTAR_PATH_BUDGET)
                            .map(|raw| smooth_path(raw, &world));
                    match repath {
                        Some(new_path) => {
                            info!(
                                npc = npc_id.0,
                                goal = ?goal_cell.to_array(),
                                "path blocked; repathed in place",
                            );
                            *path = new_path;
                            *edge = 0;
                            *blocked = false;
                            npc_path.set_if_neq(NpcPath(path.clone()));
                        }
                        None => {
                            info!(
                                npc = npc_id.0,
                                goal = ?goal_cell.to_array(),
                                "path blocked with no route left; abandoning",
                            );
                            move_abandoned = true;
                        }
                    }
                }

                // Arrival is exact: the mover's edge cursor sitting on
                // the last waypoint means the body was landed precisely
                // there (or the path was a synthetic one-cell instant
                // arrival). No radius, no settle check.
                *deadline_secs -= dt;
                if !move_abandoned {
                    if *edge + 1 >= path.len() {
                        move_arrived = Some((on_arrive.clone(), *snap));
                    } else if *deadline_secs <= 0.0 {
                        move_abandoned = true;
                    }
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
            // Apply pose snap + kinematic lock uniformly *before* the
            // action dispatch. The snap is action-agnostic — any goal
            // whose target block had a `use_slot` populated this. The
            // follow-on action just decides what the locked body
            // does (sleep/consume/work).
            // An Interact target may have been broken or replaced
            // while we walked over. Validate BEFORE the snap below —
            // the documented degrade is "stand briefly, then idle",
            // not "sleep mid-air where the bed used to be and still
            // collect the restore".
            if let ArrivalAction::Interact {
                target_cell,
                anchor_cell,
                exclusive,
                ..
            } = &action
            {
                let still_interactable = {
                    let (coord, local) = world_to_chunk(*target_cell);
                    chunk_map
                        .0
                        .get(&coord)
                        .and_then(|&e| chunks.get(e).ok())
                        .map(|chunk| chunk.get(local))
                        .filter(|slot| !slot.is_empty())
                        .map(|slot| block_registry.def(slot).interactable.is_some())
                        .unwrap_or(false)
                };
                if !still_interactable {
                    info!(
                        npc = npc_id.0,
                        cell = ?target_cell.to_array(),
                        "interact arrival: block gone or no longer interactable; abandoning",
                    );
                    if *exclusive {
                        interaction_claims.release(*anchor_cell, *npc_id);
                    }
                    brain.goal = Goal::Idle;
                    continue;
                }
            }
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
                    // The plan may have been cancelled (or retagged)
                    // while we walked over — the mod-api contract says
                    // a cancelled plan's work completes silently, so
                    // don't start the timer against a stale snapshot.
                    if haul.plans.kind(target_cell) != Some(plan_kind) {
                        info!(
                            npc = npc_id.0,
                            cell = ?target_cell.to_array(),
                            "work arrival: plan gone or changed; abandoning",
                        );
                        haul.plan_claims.release(target_cell, *npc_id);
                        brain.goal = Goal::Idle;
                        continue;
                    }
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
                        haul.store.release_for_npc(*npc_id);
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
                    haul.store.drop_queue_entry(*npc_id, item_entity);
                    // Plan next leg from the (now updated) assignment.
                    continue_haul_or_release(
                        *npc_id,
                        kind,
                        now_secs,
                        &pose,
                        &carrying,
                        &mut brain,
                        &mut npc_path,
                        &mut haul.store,
                        &haul.plans,
                        &haul.kind_registry,
                        &haul.item_registry,
                        &craft.stations,
                        &craft.recipes,
                        &world,
                    );
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
                        haul.store.release_for_npc(*npc_id);
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
                            haul.store.release_for_npc(*npc_id);
                            brain.goal = Goal::Idle;
                            continue;
                        }
                    };
                    let accepted = haul.plans.deposit(plan_cell, carry_item, carry_count);
                    if accepted == 0 {
                        // Plan no longer needs this kind (another
                        // hauler or the player filled it between
                        // assignment and arrival). Without this
                        // release, pick_next_haul_leg re-plans the
                        // same deposit leg forever — a silent 60 Hz
                        // livelock holding the assignment. Mirrors
                        // the `want == 0` guard on the station path.
                        info!(
                            npc = npc_id.0,
                            cell = ?plan_cell.to_array(),
                            kind = carry_item.0,
                            "haul deposit: plan has no demand for carry; releasing",
                        );
                        haul.store.release_for_npc(*npc_id);
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    {
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
                            if let Err(err) = haul.broadcast.send::<PlanEdit, StateSyncChannel>(
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
                    continue_haul_or_release(
                        *npc_id,
                        kind,
                        now_secs,
                        &pose,
                        &carrying,
                        &mut brain,
                        &mut npc_path,
                        &mut haul.store,
                        &haul.plans,
                        &haul.kind_registry,
                        &haul.item_registry,
                        &craft.stations,
                        &craft.recipes,
                        &world,
                    );
                }
                ArrivalAction::DepositAtStation { station_cell } => {
                    // Validate the cell is still a station block. A
                    // destroyed workbench between haul start and
                    // arrival collapses to "release haul, idle." The
                    // NPC keeps any leftover carry; the scheduler
                    // finds a different target (plan or station) on
                    // the next Idle entry.
                    let station_ok = crate::craft_stations::lookup_station_def(
                        station_cell,
                        chunks,
                        chunk_map,
                        block_registry,
                    )
                    .is_some();
                    if !station_ok {
                        info!(
                            npc = npc_id.0,
                            cell = ?station_cell.to_array(),
                            "haul deposit: station gone; releasing assignment",
                        );
                        haul.store.release_for_npc(*npc_id);
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    let (carry_item, carry_count) = match (carrying.item, carrying.count) {
                        (Some(slot), c) if c > 0 => (slot, c),
                        _ => {
                            haul.store.release_for_npc(*npc_id);
                            brain.goal = Goal::Idle;
                            continue;
                        }
                    };
                    // Cap deposit at the station's current demand for
                    // this kind, so an NPC arriving with a partial
                    // stack but a small unmet need doesn't over-supply
                    // and orphan the leftover. Unlike Plans::deposit
                    // (which caps internally), CraftStations::deposit
                    // is unbounded — players intentionally dump whole
                    // stacks via the modal — so the cap lives here at
                    // the NPC-haul layer.
                    let want = craft
                        .stations
                        .get(station_cell)
                        .map(|s| {
                            crate::haul::compute_station_demand(
                                s,
                                &craft.recipes,
                                &haul.item_registry,
                            )
                            .get(&carry_item)
                            .copied()
                            .unwrap_or(0)
                        })
                        .unwrap_or(0);
                    if want == 0 {
                        info!(
                            npc = npc_id.0,
                            cell = ?station_cell.to_array(),
                            kind = carry_item.0,
                            "haul deposit: station has no demand for carry; releasing",
                        );
                        haul.store.release_for_npc(*npc_id);
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    let accepted = want.min(carry_count);
                    let state_after = {
                        let state = craft.stations.get_or_insert(station_cell);
                        state.deposit(carry_item, accepted);
                        state.clone()
                    };
                    carrying.count = carry_count - accepted;
                    if carrying.count == 0 {
                        carrying.item = None;
                    }
                    info!(
                        npc = npc_id.0,
                        cell = ?station_cell.to_array(),
                        accepted,
                        "haul deposit (station) complete",
                    );
                    if let Some(server) = server {
                        crate::craft_stations::broadcast_station(
                            &mut haul.broadcast,
                            server,
                            station_cell,
                            Some(state_after),
                        );
                    }
                    // Pick next leg from the (possibly still active)
                    // assignment. Same pattern as DepositAtPlan.
                    continue_haul_or_release(
                        *npc_id,
                        kind,
                        now_secs,
                        &pose,
                        &carrying,
                        &mut brain,
                        &mut npc_path,
                        &mut haul.store,
                        &haul.plans,
                        &haul.kind_registry,
                        &haul.item_registry,
                        &craft.stations,
                        &craft.recipes,
                        &world,
                    );
                }
                ArrivalAction::PickupTool {
                    item_entity,
                    item_slot,
                } => {
                    // Validate the reserved tool item still exists +
                    // matches the kind we reserved. A despawn between
                    // reserve and arrival (player grabbed it, future
                    // cleanup) collapses to "release haul, idle." The
                    // scheduler will reassess next tick.
                    let item_ok = haul
                        .world_items
                        .get(item_entity)
                        .map(|(_, wi)| wi.item == item_slot)
                        .unwrap_or(false);
                    if !item_ok {
                        info!(
                            npc = npc_id.0,
                            item = ?item_entity,
                            "haul tool pickup: item gone or mismatch; releasing assignment",
                        );
                        haul.store.release_for_npc(*npc_id);
                        brain.goal = Goal::Idle;
                        continue;
                    }
                    // Capture old translation BEFORE despawn — drop
                    // the displaced tool there for an in-place swap.
                    let pickup_pos = haul
                        .world_items
                        .get(item_entity)
                        .map(|(_, wi)| wi.translation)
                        .unwrap_or(pose.translation);
                    // Swap into tool slot. Same logic as the player
                    // pickup path.
                    let displaced = equipped_tool.item.replace(item_slot);
                    commands.entity(item_entity).despawn();
                    info!(
                        npc = npc_id.0,
                        new_tool = item_slot.0,
                        displaced = ?displaced.map(|s| s.0),
                        "npc tool pickup swap",
                    );
                    if let Some(prev_slot) = displaced
                        && prev_slot != item_slot
                    {
                        commands.spawn((
                            WorldItem {
                                item: prev_slot,
                                translation: pickup_pos,
                            },
                            Transform::from_translation(pickup_pos),
                            GlobalTransform::default(),
                            Replicate::to_clients(NetworkTarget::All),
                            Name::new(format!("WorldItem(npc_tool_swap:{})", prev_slot.0)),
                        ));
                    }
                    // pending_tool field + its reservation are coupled
                    // — `clear_pending_tool` clears both atomically.
                    let _ = item_entity;
                    haul.store.clear_pending_tool(*npc_id);
                    // Continue with the next haul leg now that we're
                    // tooled up. If the queue is empty (assignment
                    // was tool-only) we naturally release + idle and
                    // the scheduler will repick.
                    continue_haul_or_release(
                        *npc_id,
                        kind,
                        now_secs,
                        &pose,
                        &carrying,
                        &mut brain,
                        &mut npc_path,
                        &mut haul.store,
                        &haul.plans,
                        &haul.kind_registry,
                        &haul.item_registry,
                        &craft.stations,
                        &craft.recipes,
                        &world,
                    );
                }
                ArrivalAction::WorkStation { station_cell } => {
                    // Resolve the station def + try to start the first
                    // queued order whose inputs the inventory still
                    // satisfies. If no order qualifies (player Cancel
                    // mid-walk, inventory drained by another worker),
                    // release the CraftAssignment and drop to Idle —
                    // the scheduler may re-pair us next tick or fall
                    // through to haul.
                    let station_def = crate::craft_stations::lookup_station_def(
                        station_cell,
                        chunks,
                        chunk_map,
                        block_registry,
                    );
                    // A worker-less in-progress craft is a resume: register as
                    // the worker and let the ticker continue from the saved
                    // elapsed_secs. The scheduler tool-gated us before sending
                    // us over. Fresh starts go through the shared
                    // first-satisfiable-order path.
                    let resumable = station_def.is_some()
                        && craft
                            .stations
                            .get(station_cell)
                            .map(|st| st.active_work.is_some())
                            .unwrap_or(false)
                        && craft.bookings.worker_at(station_cell).is_none();
                    let started = if resumable {
                        craft.bookings.register_worker(station_cell, entity);
                        info!(
                            npc = npc_id.0,
                            station = ?station_cell.to_array(),
                            "craft arrival: resuming paused in-progress work",
                        );
                        true
                    } else if let Some(station_def) = station_def {
                        crate::craft_stations::try_start_first_satisfiable_order(
                            station_cell,
                            entity,
                            equipped_tool.item,
                            &station_def,
                            &haul.item_registry,
                            &craft.recipes,
                            &mut craft.stations,
                            &mut craft.bookings,
                        )
                        .is_some()
                    } else {
                        false
                    };
                    if started {
                        if let Some(server) = server {
                            let snapshot = craft.stations.get(station_cell).cloned();
                            crate::craft_stations::broadcast_station(
                                &mut haul.broadcast,
                                server,
                                station_cell,
                                snapshot,
                            );
                        }
                        brain.goal = Goal::CraftingAtStation { station_cell };
                    } else {
                        info!(
                            npc = npc_id.0,
                            station = ?station_cell.to_array(),
                            "craft arrival: no satisfiable order; releasing booking",
                        );
                        craft.bookings.release_npc_booking(*npc_id, entity);
                        brain.goal = Goal::Idle;
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
                        exclusive,
                        ..
                    } => {
                        // Physically stuck en route despite a valid
                        // path (wedged on furniture, jammed in a
                        // doorway) — back off like the haul/work arms
                        // below, or a starving NPC re-picks the same
                        // blocked target every stuck-release forever.
                        interaction_claims.memo_unreachable(
                            *anchor_cell,
                            now_secs + HAUL_UNREACHABLE_RETRY_SECS,
                        );
                        if *exclusive {
                            interaction_claims.release(*anchor_cell, *npc_id);
                        }
                    }
                    ArrivalAction::Work { target_cell, .. } => {
                        // Physically stuck/timed out en route, even
                        // though A* found a path — memoize like the
                        // A*-failure branch does, or the planner
                        // re-picks the same plan next tick and the NPC
                        // ping-pongs against the same obstacle.
                        haul.store.memo_unreachable(
                            crate::haul::HaulTarget::Plan(*target_cell),
                            now_secs + HAUL_UNREACHABLE_RETRY_SECS,
                        );
                        haul.plan_claims.release(*target_cell, *npc_id);
                    }
                    ArrivalAction::PickupForPlan { .. }
                    | ArrivalAction::DepositAtPlan { .. }
                    | ArrivalAction::DepositAtStation { .. }
                    | ArrivalAction::PickupTool { .. } => {
                        // Blocked-with-no-route or timed out mid-haul:
                        // free the entire assignment + every
                        // reservation it holds. Memoize the
                        // assignment's target first — without the same
                        // backoff the A*-failure branch gets, the
                        // scheduler re-commits the identical target on
                        // every retry (the playtest's "haul assignment
                        // committed" spam).
                        if let Some(a) = haul.store.assignment_of(*npc_id) {
                            let target = a.target;
                            haul.store.memo_unreachable(
                                target,
                                now_secs + HAUL_UNREACHABLE_RETRY_SECS,
                            );
                        }
                        haul.store.release_for_npc(*npc_id);
                    }
                    ArrivalAction::WorkStation { .. } => {
                        // Stuck mid-walk-to-station: release the
                        // booking so another NPC (or this one next
                        // tick) can take it. No worker registration
                        // exists yet — arrival is what registers —
                        // but `release_npc_booking` handles both.
                        craft.bookings.release_npc_booking(*npc_id, entity);
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
            // Capture the bits we need before mutating brain.home_cluster
            // below — &brain.goal borrows the brain.
            let mut sleep_cell: Option<IVec3> = None;
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
                    // Provisional home-cluster claim: finishing a sleep
                    // anywhere claims the cluster that owns that cell.
                    // Match is by need id ("sleep"), not block kind, so
                    // any future sleep-restoring interactable counts.
                    if nr.need == "sleep" {
                        sleep_cell = Some(*target_cell);
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
            if let Some(cell) = sleep_cell
                && let Some(cluster) = anchors
                    .civilization
                    .cluster_containing_cell(cell, &anchors.rooms)
            {
                if brain.home_cluster != Some(cluster) {
                    info!(
                        npc = npc_id.0,
                        cluster = cluster.0,
                        cell = ?cell.to_array(),
                        "claimed home cluster on sleep",
                    );
                }
                brain.home_cluster = Some(cluster);
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
                // Cancelled mid-work → the effort still counts (the
                // need restore above already applied) but the world
                // mutation must NOT happen: "cancel cancels" is the
                // player-facing contract, and a Remove plan firing
                // after its tag was cleared destroys a block the
                // player decided to keep.
                if haul.plans.kind(*target_cell) == Some(*plan_kind) {
                    commands.write_message(NpcWorkCompleted {
                        cell: *target_cell,
                        plan_kind: *plan_kind,
                    });
                } else {
                    info!(
                        npc = npc_id.0,
                        cell = ?target_cell.to_array(),
                        "work complete but plan was cancelled; completing silently",
                    );
                }
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
                let slot = slot_at_cell(target_cell, chunks, chunk_map, block_registry);
                let (anchor_cell, orientation) =
                    resolve_anchor_with_orientation(target_cell, chunk_entities_q, chunk_map);
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
        // Crafting end-condition cleanup: release the booking + the
        // ActiveWorker slot, then drop to Idle. Triggered by the
        // CraftingAtStation arm when the station ran out of work or
        // the NPC's worker registration was cleared. Done here (not
        // inline in the match) so we don't double-borrow `craft`.
        if let Some(station_cell) = crafting_done_at {
            craft.bookings.release_npc_booking(*npc_id, entity);
            info!(
                npc = npc_id.0,
                station = ?station_cell.to_array(),
                "crafting complete or interrupted; releasing station",
            );
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
                        continue;
                    }
                }
            }
            // Phase 6c-A: craft scheduler runs BEFORE haul. "Skills
            // first, hauling fallback" — an NPC who can do useful work
            // at a nearby station (inventory satisfies an order, tool
            // matches) takes that work in preference to hauling
            // materials around. This delivers the user's natural-
            // fallout requirement: free NPCs do skilled work when
            // workable; only idle-skilled-NPCs fall back to hauling.
            if !preempted_this_tick
                && !craft.bookings.contains_npc(*npc_id)
                && !haul.store.has_assignment(*npc_id)
                && let Some(station_cell) = crate::craft_stations::try_schedule_craft_for_npc(
                    *npc_id,
                    pose.translation,
                    &equipped_tool,
                    &craft.stations,
                    &mut craft.bookings,
                    block_registry,
                    &craft.recipes,
                    &haul.item_registry,
                    chunks,
                    chunk_map,
                )
            {
                // Walk to a standable neighbour of the station block.
                // Station cells are solid (the workbench itself), so
                // the path target is one of the surrounding floor
                // cells. Same pattern as the WorkPlan dispatch below.
                let foot = pose_to_standable_foot(&pose, &world)
                    .unwrap_or_else(|| pose_to_foot_cell(&pose));
                let stand_cell = nearest_standable_neighbor(station_cell, foot, &world);
                let path = stand_cell.and_then(|stand| {
                    if stand == foot {
                        Some(vec![foot])
                    } else {
                        find_path(foot, stand, &world, ASTAR_NODE_BUDGET, ASTAR_PATH_BUDGET)
                            .map(|raw| smooth_path(raw, &world))
                            .filter(|p| p.len() >= 1)
                    }
                });
                match path {
                    Some(path) if path.len() >= 2 => {
                        npc_path.set_if_neq(NpcPath(path.clone()));
                        brain.goal = Goal::move_to(
                            path,
                            60.0,
                            ArrivalAction::WorkStation { station_cell },
                            None,
                        );
                        continue;
                    }
                    Some(_) => {
                        // Already adjacent to the station — skip the
                        // walk, jump straight to the arrival handler
                        // by synthesising a one-cell path. Same trick
                        // the WorkPlan path uses when foot == stand.
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        brain.goal = Goal::move_to(
                            vec![foot],
                            1.0,
                            ArrivalAction::WorkStation { station_cell },
                            None,
                        );
                        continue;
                    }
                    None => {
                        // No standable neighbour or A* failed.
                        // Release the booking; next tick may find a
                        // different station or fall through to haul.
                        info!(
                            npc = npc_id.0,
                            station = ?station_cell.to_array(),
                            "craft commit: no path to station; releasing booking",
                        );
                        craft.bookings.release_npc_booking(*npc_id, entity);
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
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
            // NPC. Cheap: one O(items) index build + one O(plans)
            // scan. Runs for *any* NPC without an existing
            // assignment, including ones with non-empty carry — the
            // matcher's deposit-only branch handles that case
            // (save/load mid-haul, hand-offs from future systems).
            //
            // Skipped on a tick where preempt fired: a hauler that just
            // peeled off for survival would otherwise be instantly
            // reassigned to another haul before the planner could
            // route them to food/sleep.
            if !preempted_this_tick && !haul.store.has_assignment(*npc_id) {
                crate::haul::try_schedule_haul_for_npc(
                    *npc_id,
                    &kind.0,
                    pose.translation,
                    &carrying,
                    &equipped_tool,
                    &haul.kind_registry,
                    &haul.plans,
                    &craft.stations,
                    block_registry,
                    &haul.item_registry,
                    &craft.recipes,
                    chunks,
                    chunk_map,
                    &haul.world_items,
                    &mut haul.store,
                    now_secs,
                );
            }
            // Engine-driven haul takes priority over the Lua planner.
            // If the scheduler (above, or a previous tick) queued an
            // assignment for this NPC, plan the first leg directly
            // and skip the planner call. The planner sees
            // `pending_assignments` in its snapshot when it is called
            // for a *different* NPC, but never gets to overrule an
            // active haul.
            if haul.store.has_assignment(*npc_id) {
                // Committed a leg → done deciding this tick. A clean
                // release (or path failure, which also memoizes the
                // target) falls through to the planner so the NPC
                // doesn't burn a tick doing nothing.
                if continue_haul_or_release(
                    *npc_id,
                    kind,
                    now_secs,
                    &pose,
                    &carrying,
                    &mut brain,
                    &mut npc_path,
                    &mut haul.store,
                    &haul.plans,
                    &haul.kind_registry,
                    &haul.item_registry,
                    &craft.stations,
                    &craft.recipes,
                    &world,
                ) {
                    continue;
                }
            }
            let kind_id = NpcKindId(kind.0.clone());
            let snapshot = build_snapshot(
                *npc_id,
                &kind_id,
                &pose,
                &needs,
                stats,
                &equipped_tool,
                &anchors.rooms,
                &interactable_index,
                &interaction_claims,
                &haul.plans,
                &haul.plan_claims,
                &haul.store,
                chunks,
                chunk_entities_q,
                chunk_map,
                block_registry,
                &haul.item_registry,
                &work_defaults.0,
                world_clock,
                now_secs,
            );
            // One-line per-NPC trace at every planner call so a
            // session log shows what each NPC saw on each decision.
            // Includes every need-id the snapshot carries (vanilla:
            // hunger/sleep/work; mods may add more) plus the counts of
            // every option pool the planner can pick from. The pairing
            // with the "planner committed" log below tells you *what*
            // the planner saw vs *what* it picked, which is enough to
            // diagnose "why didn't this NPC work that nearby plan?"
            // without instrumenting the Lua planner itself.
            let need_hunger = snapshot.needs.get("hunger").copied();
            let need_sleep = snapshot.needs.get("sleep").copied();
            let need_work = snapshot.needs.get("work").copied();
            info!(
                npc = npc_id.0,
                is_night = snapshot.is_night,
                hunger = ?need_hunger,
                sleep = ?need_sleep,
                work = ?need_work,
                nearby_plans = snapshot.nearby_plans.len(),
                nearby_interactions = snapshot.nearby_interactions.len(),
                nearby_rooms = snapshot.nearby_rooms.len(),
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
                    haul.store.release_for_npc(*npc_id);
                    commands
                        .entity(entity)
                        .remove::<KinematicLock>()
                        .insert(BrainDisabled {
                            reason: e.to_string(),
                        });
                    continue;
                }
            };
            // Single-line summary of what the planner *chose*, paired
            // with the "planner snapshot" log just above. Distinguishes
            // between the planner's primitive kinds (Idle/Rest/Wander/
            // Goto/Interact/WorkPlan) and includes the target cell where
            // applicable. With the snapshot's `nearby_plans` count, this
            // is enough to spot "NPC saw 2 plans but chose Interact" —
            // a clue that a survival branch (hunger/sleep) fired first.
            let chosen_summary: &'static str = match &planner_goal {
                PlannerGoal::Idle => "Idle",
                PlannerGoal::Rest { .. } => "Rest",
                PlannerGoal::Wander { .. } => "Wander",
                PlannerGoal::Goto { .. } => "Goto",
                PlannerGoal::Interact { .. } => "Interact",
                PlannerGoal::WorkPlan { .. } => "WorkPlan",
                PlannerGoal::SleepGround { .. } => "SleepGround",
            };
            let chosen_target: Option<BlockPos> = match &planner_goal {
                PlannerGoal::Goto { cell, .. }
                | PlannerGoal::Interact { cell, .. }
                | PlannerGoal::WorkPlan { cell, .. } => Some(*cell),
                _ => None,
            };
            info!(
                npc = npc_id.0,
                kind = chosen_summary,
                target = ?chosen_target,
                "planner committed",
            );
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
                }
                PlannerGoal::Rest { duration_secs } => {
                    brain.goal = Goal::Resting {
                        remaining_secs: duration_secs.clamp(MIN_REST_SECS, MAX_REST_SECS),
                    };
                    if !npc_path.0.is_empty() {
                        npc_path.0.clear();
                    }
                }
                PlannerGoal::SleepGround {
                    duration_secs,
                    restores,
                    need,
                    animation,
                } => {
                    // Same clamp window as Rest. The restore is spread
                    // over the *clamped* duration so a mod passing a
                    // huge `restores` with a tiny duration can't beat
                    // bed rates through the clamp.
                    let duration = duration_secs.clamp(MIN_REST_SECS, MAX_REST_SECS);
                    brain.goal = Goal::SleepingGround {
                        remaining_secs: duration,
                        need,
                        restore_per_sec: restores.clamp(0.0, 1.0) / duration,
                        animation,
                    };
                    if !npc_path.0.is_empty() {
                        npc_path.0.clear();
                    }
                }
                PlannerGoal::Wander {
                    radius_cells,
                    timeout_secs,
                } => {
                    let radius = radius_cells.clamp(1, MAX_WANDER_RADIUS_CELLS);
                    let timeout = timeout_secs.clamp(1.0, MAX_WANDER_TIMEOUT_SECS);
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    // Bounded-wander resolution. An NPC that claimed a
                    // home cluster samples wander targets inside that
                    // cluster's inflated bbox so they don't drift across
                    // the map. A dangling claim (cluster pruned after
                    // its last room was destroyed) lazy-clears here.
                    let home_bbox = match brain.home_cluster {
                        Some(id) => {
                            let inflated = anchors
                                .civilization
                                .cluster_bbox_inflated(id, anchors.civ_params.0.buffer_cells);
                            if inflated.is_none() {
                                brain.home_cluster = None;
                            }
                            inflated
                        }
                        None => None,
                    };
                    match pick_wander_path(foot, radius, home_bbox, &mut brain.rng, &world) {
                        Some(path) => {
                            // set_if_neq keeps the wire quiet on the
                            // rare repeat path; planner-driven calls
                            // are several seconds apart so it triggers
                            // basically every time, but the guard is
                            // free if it doesn't.
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal =
                                Goal::move_to(path, timeout, ArrivalAction::None, None);
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
                    let target = anchors
                        .rooms
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
                        continue;
                    }
                    let path =
                        find_path(foot, target, &world, ASTAR_NODE_BUDGET, ASTAR_PATH_BUDGET)
                            .map(|raw| smooth_path(raw, &world))
                            .filter(|p| p.len() >= 2);
                    match path {
                        Some(path) => {
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal =
                                Goal::move_to(path, timeout, ArrivalAction::None, None);
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
                        chunks,
                        chunk_map,
                        block_registry,
                    ) else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "interact target no longer interactable; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        continue;
                    };
                    // Anchor is the claim key for exclusive blocks
                    // (multi-cell entities contend on one slot).
                    // Orientation rotates both `use_slot.approach`
                    // and `use_slot.pose` into world space.
                    let (anchor_cell, orientation) =
                        resolve_anchor_with_orientation(target_cell, chunk_entities_q, chunk_map);
                    // Atomic claim — only attempted for exclusive
                    // blocks. Non-exclusive interactions (food on a
                    // shelf, water at a well) don't contend; a
                    // queue of NPCs can use them in parallel from
                    // different cells.
                    if interactable.exclusive && !interaction_claims.try_claim(anchor_cell, *npc_id)
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
                            brain.goal = Goal::move_to(
                                path,
                                timeout,
                                ArrivalAction::Interact {
                                    need_restore,
                                    duration_secs: duration,
                                    target_cell,
                                    anchor_cell,
                                    exclusive,
                                    animation,
                                },
                                snap,
                            );
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
                            // Same backoff the work/haul A*-miss arms
                            // use — no point re-offering this anchor
                            // to the planner every tick.
                            interaction_claims.memo_unreachable(
                                anchor_cell,
                                now_secs + HAUL_UNREACHABLE_RETRY_SECS,
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
                        continue;
                    }
                    // Phase-5b belt-and-braces: re-check the tool gate
                    // at commit. The snapshot filter already hid plans
                    // this NPC couldn't work, but the registry could
                    // have mutated between snapshot and commit (hot
                    // reload, future mod changes). Degrade silently
                    // to a brief rest — same pattern as the no-tag
                    // branch above.
                    let commit_verb = plan_kind.work_verb();
                    let commit_work_action = match plan_kind {
                        PlanKind::Build { slot, .. } => {
                            block_registry.def(slot).work_action.clone()
                        }
                        PlanKind::Remove => {
                            let (coord, local) = world_to_chunk(target_cell);
                            chunk_map
                                .0
                                .get(&coord)
                                .and_then(|&e| chunks.get(e).ok())
                                .and_then(|chunk| {
                                    let s = chunk.get(local);
                                    if s.is_empty() { None } else { Some(s) }
                                })
                                .and_then(|s| block_registry.def(s).work_action.clone())
                        }
                    };
                    // Soft gates resolve to a duration multiplier (1.0
                    // when tooled, >1 bare-handed); a hard gate with no
                    // matching tool resolves to None and releases as
                    // before.
                    let commit_tool_multiplier = match commit_work_action.as_ref() {
                        None => 1.0,
                        Some(work) => {
                            let has_tool = work.tool_for(commit_verb).is_none_or(|tag| {
                                haul.item_registry.tool_has_tag(equipped_tool.item, tag)
                            });
                            match work.duration_multiplier(commit_verb, has_tool) {
                                Some(mult) => mult,
                                None => {
                                    warn!(
                                        npc = npc_id.0,
                                        target = ?target_cell.to_array(),
                                        required = ?work.tool_for(commit_verb),
                                        "work commit: hard tool gate unsatisfied; releasing claim and resting",
                                    );
                                    haul.plan_claims.release(target_cell, *npc_id);
                                    brain.goal = Goal::Resting {
                                        remaining_secs: MIN_REST_SECS,
                                    };
                                    continue;
                                }
                            }
                        }
                    };
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
                        chunks,
                        chunk_map,
                        block_registry,
                        &work_defaults.0,
                    );
                    // Worn-down modifier (starvation): a needy worker
                    // is a slow worker. Composes with the tool gate —
                    // a starving bare-handed miner stacks both.
                    let worn_down_multiplier = work_defaults
                        .0
                        .worn_down
                        .as_ref()
                        .filter(|w| needs.0.get(&w.need).copied().unwrap_or(0.0) >= w.above)
                        .map(|w| w.multiplier)
                        .unwrap_or(1.0);
                    let work_duration_secs =
                        work_duration_secs * commit_tool_multiplier * worn_down_multiplier;
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    let Some(stand_cell) = nearest_standable_neighbor(target_cell, foot, &world)
                    else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "no standable neighbour of plan target; releasing claim and parking briefly",
                        );
                        // Memoize like the A*-failure branch below —
                        // without this the planner re-picks the same
                        // plan on the next idle entry and the NPC
                        // livelocks at ~2 Hz (2026-07-05 playtest: a
                        // high tag with no perch pinned an NPC while
                        // its needs decayed). Common case: the upper
                        // blocks of a tall Remove column — clearing
                        // the lower ones creates the perch, and the
                        // 30s retry naturally picks the plan back up.
                        haul.store.memo_unreachable(
                            HaulTarget::Plan(target_cell),
                            now_secs + HAUL_UNREACHABLE_RETRY_SECS,
                        );
                        haul.plan_claims.release(target_cell, *npc_id);
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
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
                            brain.goal = Goal::move_to(
                                path,
                                timeout,
                                ArrivalAction::Work {
                                    duration_secs: work_duration_secs,
                                    target_cell,
                                    plan_kind,
                                    need_restore: work_need_restore,
                                },
                                None,
                            );
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
                            // Memoize like the haul scheduler does, or the
                            // planner re-picks this exact plan on the next
                            // idle entry and the NPC livelocks at ~2 Hz on
                            // commit→path-fail→park (2026-07-03 playtest:
                            // a roof-level tag pinned two NPCs for good).
                            // `collect_nearby_plans` hides memoized cells,
                            // so the planner falls through to other work.
                            haul.store.memo_unreachable(
                                HaulTarget::Plan(target_cell),
                                now_secs + HAUL_UNREACHABLE_RETRY_SECS,
                            );
                            haul.plan_claims.release(target_cell, *npc_id);
                            if !npc_path.0.is_empty() {
                                npc_path.0.clear();
                            }
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                        }
                    }
                }
            }
        }

        // Phase 4: facing. Forward motion belongs to the kinematic
        // mover (`npc_mover_step`, chained after this system) — the
        // brain only turns *standing* bodies toward whatever they're
        // engaged with, applied straight to the pose since there is no
        // NPC MovementIntent anymore.
        let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
        let face_target = match &brain.goal {
            Goal::Working { target_cell, .. } => Some(*target_cell),
            // Locked interactions (snapped onto a use_slot pose)
            // freeze yaw — the snap already chose the right direction
            // and aiming at the target cell would drift the yaw away
            // when the snap landed the NPC off the target cell's
            // centre. Unlocked interactions (consume-pattern
            // stand-and-wait) face the target so the body visibly
            // engages with the block.
            Goal::Interacting { target_cell, .. } if !is_locked => Some(*target_cell),
            // Face the station while crafting; the body is parked.
            Goal::CraftingAtStation { station_cell } => Some(*station_cell),
            _ => None,
        };
        if let Some(cell) = face_target
            && let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, waypoint_xz(cell), dt)
        {
            pose.yaw = (pose.yaw + dyaw).rem_euclid(core::f32::consts::TAU);
        }
    }
}

/// Compute the per-tick yaw step that rotates `current_yaw` toward
/// whichever yaw points from `pose_xz` to `aim`. Clamped to
/// `NPC_TURN_RATE * dt` so a 180° flip doesn't snap. Returns `None`
/// when `aim` is on top of `pose_xz` (no direction to face).
pub(crate) fn aim_yaw_step(pose_xz: Vec2, current_yaw: f32, aim: Vec2, dt: f32) -> Option<f32> {
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

/// Try a few random XZ targets and run A* to the first that's
/// reachable. Sampling box:
///
/// - `home_bbox = None` — sample within `radius_cells` of `foot`
///   (legacy free-wander).
/// - `home_bbox = Some((min, max))` — sample uniformly inside the
///   inflated cluster bbox; `foot`/`radius_cells` are ignored for
///   target picking but still used as the A* start.
///
/// Returns the first smoothed path with at least one step. `None` if
/// every attempt fails (caller stays Idle and retries next tick).
fn pick_wander_path<W: Walkability>(
    foot: IVec3,
    radius_cells: i32,
    home_bbox: Option<(IVec3, IVec3)>,
    rng: &mut u64,
    world: &W,
) -> Option<Vec<IVec3>> {
    let radius = radius_cells.max(1) as f32;
    for _ in 0..MAX_WANDER_ATTEMPTS {
        // XZ candidate. The probe Y is the cluster's bbox top + 4 when
        // we're bounded (matches the home's vertical extent so a
        // multi-storey building's roof doesn't shadow the ground); the
        // foot's Y + 4 when we're free-wandering.
        let (tx, tz, probe_y) = if let Some((bmin, bmax)) = home_bbox {
            let span_x = (bmax.x - bmin.x).max(0) as f32;
            let span_z = (bmax.z - bmin.z).max(0) as f32;
            let x = bmin.x as f32 + rand_unit(rng) * span_x;
            let z = bmin.z as f32 + rand_unit(rng) * span_z;
            (x, z, bmax.y + 4)
        } else {
            let dx = (rand_unit(rng) * 2.0 - 1.0) * radius;
            let dz = (rand_unit(rng) * 2.0 - 1.0) * radius;
            (foot.x as f32 + dx, foot.z as f32 + dz, foot.y + 4)
        };
        let probe = IVec3::new(tx as i32, probe_y, tz as i32);
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
        find_path(
            foot,
            stand_cell,
            world,
            ASTAR_NODE_BUDGET,
            ASTAR_PATH_BUDGET,
        )
        .map(|raw| smooth_path(raw, world))
        .filter(|p| p.len() >= 2)?
    };
    Some(Goal::move_to(path, deadline_secs, on_arrive, None))
}

/// Continue the NPC's active haul assignment: pick the next leg and
/// commit it as the brain goal, or end the assignment (release + Idle).
/// Returns true when a new goal was committed — callers in the Idle
/// entry use that to skip the planner this tick.
///
/// One implementation for what used to be five hand-copied blocks (two
/// of which had drifted and grown bugs). Failure semantics live here:
/// a pathfinding failure memoizes the assignment's target as
/// unreachable for [`HAUL_UNREACHABLE_RETRY_SECS`] so the scheduler
/// doesn't immediately re-pair the same NPC to the same impossible
/// target.
#[allow(
    clippy::too_many_arguments,
    reason = "haul continuation spans plan + station ctx"
)]
fn continue_haul_or_release<W: Walkability>(
    npc_id: NpcId,
    kind: &NpcKind,
    now_secs: f32,
    pose: &AvatarPose,
    carrying: &Carrying,
    brain: &mut Brain,
    npc_path: &mut Mut<NpcPath>,
    store: &mut HaulStore,
    plans: &Plans,
    kind_registry: &NpcKindRegistry,
    item_registry: &crate::items::ItemRegistry,
    stations: &crate::craft_stations::CraftStations,
    recipes: &crate::recipes::RecipeRegistry,
    world: &W,
) -> bool {
    let cap = kind_registry
        .get(&kind.0)
        .map(|d| d.carry_capacity)
        .unwrap_or(DEFAULT_NPC_CARRY_CAPACITY);
    let leg = match store.assignment_of(npc_id) {
        None => Ok(None),
        Some(a) => pick_next_haul_leg(
            pose,
            a.target,
            carrying,
            cap,
            a.pending_tool.as_ref(),
            &a.queue,
            plans,
            stations,
            recipes,
            item_registry,
            world,
        )
        .map_err(|()| a.target),
    };
    match leg {
        Ok(Some(goal)) => {
            if let Goal::MoveTo { path, .. } = &goal {
                npc_path.set_if_neq(NpcPath(path.clone()));
            }
            brain.goal = goal;
            true
        }
        Ok(None) => {
            // Assignment naturally done (or already gone); clean release.
            store.release_for_npc(npc_id);
            brain.goal = Goal::Idle;
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
            false
        }
        Err(target) => {
            info!(
                npc = npc_id.0,
                target = ?target,
                retry_in = HAUL_UNREACHABLE_RETRY_SECS,
                "haul leg pathfinding failed; memoizing target as unreachable",
            );
            store.memo_unreachable(target, now_secs + HAUL_UNREACHABLE_RETRY_SECS);
            store.release_for_npc(npc_id);
            brain.goal = Goal::Idle;
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
            false
        }
    }
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
#[allow(
    clippy::too_many_arguments,
    reason = "haul leg picker spans plan + station ctx"
)]
fn pick_next_haul_leg<W: Walkability>(
    pose: &AvatarPose,
    target: crate::haul::HaulTarget,
    carrying: &Carrying,
    carry_cap: u32,
    pending_tool: Option<&crate::haul::ReservedItem>,
    assignment_queue: &[crate::haul::ReservedItem],
    plans: &Plans,
    stations: &crate::craft_stations::CraftStations,
    recipes: &crate::recipes::RecipeRegistry,
    item_registry: &crate::items::ItemRegistry,
    world: &W,
) -> Result<Option<Goal>, ()> {
    // Tool prereq comes first. Until the NPC has the right tool, no
    // amount of material hauling helps — work would be gated at the
    // plan. Scheduler reserved this tool atomically, so by the time
    // we read pending_tool here it's earmarked for this NPC. Station
    // targets never set pending_tool (recipe tool gates are enforced
    // by the *craft* scheduler, not the haul one).
    if let Some(tool) = pending_tool {
        return plan_haul_move(
            pose,
            pose_to_foot_cell_of(tool.translation),
            ArrivalAction::PickupTool {
                item_entity: tool.entity,
                item_slot: tool.item,
            },
            HAUL_LEG_TIMEOUT_SECS,
            world,
        )
        .map(Some)
        .ok_or(());
    }
    // "Target still wants more" check — plans use `is_satisfied`,
    // stations use `compute_station_demand`. Both: missing entry ⇒
    // target gone ⇒ no demand.
    let target_remaining = match target {
        HaulTarget::Plan(cell) => matches!(plans.get(cell), Some(s) if !s.is_satisfied()),
        HaulTarget::Station(cell) => stations
            .get(cell)
            .map(|s| !crate::haul::compute_station_demand(s, recipes, item_registry).is_empty())
            .unwrap_or(false),
    };
    let carry_full = !carrying.is_empty() && carrying.count >= carry_cap;
    let queue_empty = assignment_queue.is_empty();
    let deposit_arrival = match target {
        HaulTarget::Plan(cell) => ArrivalAction::DepositAtPlan { plan_cell: cell },
        HaulTarget::Station(cell) => ArrivalAction::DepositAtStation { station_cell: cell },
    };
    // Walk to deposit if: carry has stuff AND (queue empty OR carry full
    // OR target no longer needs more). The "no longer needs more"
    // path drops the leftover via deposit too — Plans::deposit /
    // CraftStations::deposit round to remaining-need; any leftover
    // stays on the NPC for the next assignment.
    if !carrying.is_empty() && (queue_empty || carry_full || !target_remaining) {
        return plan_haul_move(
            pose,
            target.cell(),
            deposit_arrival,
            HAUL_LEG_TIMEOUT_SECS,
            world,
        )
        .map(Some)
        .ok_or(());
    }
    // Walk to the next reserved item if: carry has room AND queue has
    // items AND the target still wants more. Pop happens at the
    // *arrival* (pickup) handler, not here — this fn only reads.
    if !queue_empty && target_remaining {
        let next = assignment_queue[0];
        // PickupForPlan's `plan_cell` field is the original target's
        // cell — for station targets it carries the station_cell, used
        // by the pickup arrival only for "where am I delivering this"
        // diagnostics. The actual deposit destination is re-derived
        // from `assignment.target` on the next leg.
        return plan_haul_move(
            pose,
            pose_to_foot_cell_of(next.translation),
            ArrivalAction::PickupForPlan {
                item_entity: next.entity,
                item_slot: next.item,
                plan_cell: target.cell(),
            },
            HAUL_LEG_TIMEOUT_SECS,
            world,
        )
        .map(Some)
        .ok_or(());
    }
    // Carry empty + (queue empty or target satisfied). The assignment
    // has run its course; release and idle. If the target still needs
    // more, the scheduler will create a fresh assignment next tick.
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
#[allow(
    clippy::too_many_arguments,
    reason = "snapshot builder collates many subsystems"
)]
fn build_snapshot(
    id: NpcId,
    kind: &NpcKindId,
    pose: &AvatarPose,
    needs: &Needs,
    stats: &NpcStats,
    equipped_tool: &EquippedTool,
    rooms: &RoomMap,
    interactables: &InteractableIndex,
    interaction_claims: &InteractionClaims,
    plans: &Plans,
    plan_claims: &PlanClaims,
    haul_store: &HaulStore,
    chunks: &Query<&Chunk>,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
    item_registry: &crate::items::ItemRegistry,
    work_defaults: &block_junk_mod_api::npcs::WorkDefaults,
    world_clock: WorldClock,
    now_secs: f32,
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
        now_secs,
    );
    let nearby_plans = collect_nearby_plans(
        plans,
        plan_claims,
        haul_store,
        now_secs,
        id,
        equipped_tool,
        foot,
        SNAPSHOT_PLAN_RADIUS_CELLS,
        SNAPSHOT_PLAN_LIMIT,
        chunks,
        chunk_map,
        block_registry,
        item_registry,
        work_defaults,
    );
    // Engine-assigned haul work for *this* NPC. Today the engine
    // bypasses the planner whenever an assignment is live, so this
    // field arrives empty in every snapshot a planner actually sees —
    // populated only for the (currently unreachable) future where a
    // planner gets to weigh in even mid-haul. Wire it through anyway
    // so the surface is stable and the bypass becomes an enable/disable
    // knob rather than a shape change.
    let pending_assignments = haul_store
        .assignment_of(id)
        .map(|a| {
            let cell = a.target.cell();
            vec![PendingAssignment {
                plan_cell: BlockPos {
                    x: cell.x,
                    y: cell.y,
                    z: cell.z,
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
        stats: stats.0.clone(),
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
#[allow(
    clippy::too_many_arguments,
    reason = "snapshot collector mirrors live brain lookups"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "flat filter pipeline over several registries"
)]
fn collect_nearby_plans(
    plans: &Plans,
    plan_claims: &PlanClaims,
    haul_store: &HaulStore,
    now_secs: f32,
    self_id: NpcId,
    equipped_tool: &EquippedTool,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
    item_registry: &crate::items::ItemRegistry,
    work_defaults: &block_junk_mod_api::npcs::WorkDefaults,
) -> Vec<NearbyPlan> {
    let mut out: Vec<NearbyPlan> = Vec::new();
    // Trace-level filter reasons: a plan that gets filtered before
    // reaching the planner is invisible at the info level (we only
    // log the surviving count). Run with
    // `RUST_LOG=block_junk::npc=trace` to see exactly which plans got
    // excluded and why ("too far", "claimed by someone else", "needs
    // materials", "wrong tool"). Tagged with both the NPC and the
    // plan cell so a grep tells you the whole picture.
    for (cell, state) in plans.iter() {
        let d = *cell - foot;
        let chebyshev = d.x.abs().max(d.y.abs()).max(d.z.abs());
        if chebyshev > radius_cells {
            trace!(
                npc = self_id.0,
                cell = ?cell.to_array(),
                chebyshev,
                radius_cells,
                "nearby plan filter: too far",
            );
            continue;
        }
        if plan_claims.is_taken_by_other(*cell, self_id) {
            trace!(
                npc = self_id.0,
                cell = ?cell.to_array(),
                "nearby plan filter: claimed by another npc",
            );
            continue;
        }
        // Memoized-unreachable gate: a plan whose stand cell recently
        // failed A* (from the planner's WorkPlan commit or the haul
        // scheduler) is hidden until the memo expires, so the planner
        // falls through to reachable work instead of livelocking on
        // commit→path-fail→park against the same cell.
        if haul_store.is_unreachable_peek(HaulTarget::Plan(*cell), now_secs) {
            trace!(
                npc = self_id.0,
                cell = ?cell.to_array(),
                "nearby plan filter: memoized unreachable",
            );
            continue;
        }
        // Phase-3 gate: NPCs can only commit to plans whose materials
        // are fully delivered. Pending-materials Build plans wait for
        // the player (or the haul scheduler) to fill them — the
        // planner shouldn't even see them.
        if !state.is_satisfied() {
            trace!(
                npc = self_id.0,
                cell = ?cell.to_array(),
                "nearby plan filter: materials not yet delivered",
            );
            continue;
        }
        // Phase-5b gate, softened: skip only plans whose tool gate is
        // *hard*-unsatisfied (duration_multiplier → None). Soft-gated
        // plans stay visible — the commit path stretches the work
        // duration instead, so a tool-less NPC grinds slowly rather
        // than ignoring the plan. Hard-gated Build plans still wait
        // for the haul scheduler's tool-fetch (or a crafted tool) to
        // make them workable.
        let block_slot = match state.kind {
            PlanKind::Build { slot, .. } => Some(slot),
            PlanKind::Remove => {
                let (coord, local) = world_to_chunk(*cell);
                chunk_map
                    .0
                    .get(&coord)
                    .and_then(|&e| chunks.get(e).ok())
                    .and_then(|chunk| {
                        let s = chunk.get(local);
                        if s.is_empty() { None } else { Some(s) }
                    })
            }
        };
        if let Some(slot) = block_slot
            && let Some(work) = &block_registry.def(slot).work_action
        {
            let verb = state.kind.work_verb();
            let has_tool = work
                .tool_for(verb)
                .is_none_or(|tag| item_registry.tool_has_tag(equipped_tool.item, tag));
            if work.duration_multiplier(verb, has_tool).is_none() {
                trace!(
                    npc = self_id.0,
                    cell = ?cell.to_array(),
                    required = ?work.tool_for(verb),
                    equipped = ?equipped_tool.item.map(|s| s.0),
                    "nearby plan filter: hard tool gate",
                );
                continue;
            }
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
) -> Option<(
    Interactable,
    Option<UseSlot>,
    block_junk_mod_api::blocks::BlockId,
)> {
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
            Some((
                stand,
                Some(compute_use_slot_snap(slot, anchor_cell, orientation)),
            ))
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
    let translation = model_origin + Vec3::Y * (EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y);
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
#[allow(
    clippy::too_many_arguments,
    reason = "merges per-cell + per-anchor lookups"
)]
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
    now_secs: f32,
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
        // Recently defeated an NPC's walk (stuck or no path) ⇒
        // excluded until the backoff expires, so the planner offers
        // the next-nearest alternative instead of the same dead end.
        if claims.is_unreachable(anchor, now_secs) {
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
/// (basket on a raised platform the NPC approaches from below), then
/// two cells down (target at head-height-plus-one — an NPC working a
/// wall row it can reach standing on the ground beside it; same arm's-
/// reach rationale as the overhead stance below). Within each Y, ties
/// broken by Manhattan distance from `from` so the resulting path
/// bends toward whichever side the NPC's already approaching from
/// rather than picking an arbitrary cardinal.
///
/// Last resort: the cell exactly two below the target — overhead work.
/// This is what lets an NPC standing on the floor of a 2-cell-high
/// interior place (or break) its ceiling instead of stair-stepping
/// scaffolds up the outside. Exactly two, not one: standing one below
/// puts the NPC's head *inside* the target cell, so a Build there
/// would materialise the block around their skull. Ordered dead last
/// so any side approach keeps winning when one exists.
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
    for dy in [0, 1, -1, -2] {
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
    let overhead = target - IVec3::new(0, 2, 0);
    if standable(world, overhead) {
        return Some(overhead);
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
pub(crate) fn pose_to_foot_cell(pose: &AvatarPose) -> IVec3 {
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
pub(crate) fn waypoint_xz(cell: IVec3) -> Vec2 {
    Vec2::new(cell.x as f32 + 0.5, cell.z as f32 + 0.5)
}

/// Re-validate the not-yet-walked portion of a smoothed path after a
/// world edit near it. Mirrors exactly what planning promised: every
/// remaining waypoint `standable`, every same-Y segment `corridor_clear`
/// at body width, and the `step_neighbours` clearance probes on
/// vertical kinks (climb head room on step-up, pass-through cell on
/// step-down). The already-walked prefix is skipped via the mover's
/// `from_edge` cursor so an edit behind the NPC doesn't force a
/// pointless repath.
fn remaining_path_valid<W: Walkability>(path: &[IVec3], from_edge: usize, world: &W) -> bool {
    let first = from_edge.min(path.len().saturating_sub(1));
    for i in first..path.len() {
        if !standable(world, path[i]) {
            return false;
        }
        let Some(&next) = path.get(i + 1) else {
            continue;
        };
        let cell = path[i];
        match next.y - cell.y {
            0 => {
                if !corridor_clear(cell, next, world, NAV_BODY_HALF_EXTENT) {
                    return false;
                }
            }
            // Step up: the cell two above the current foot must be
            // body-passable to climb through (same probe as
            // `step_neighbours`).
            1 => {
                if world.blocks_body(cell + IVec3::new(0, 2, 0)) {
                    return false;
                }
            }
            // Step down: the cell walked through on the way down (one
            // above the destination foot) must be body-passable.
            -1 => {
                if world.blocks_body(next + IVec3::Y) {
                    return false;
                }
            }
            // Anything else can't come from the planner; treat as
            // broken so the NPC repaths rather than walks it.
            _ => return false,
        }
    }
    true
}

/// splitmix64-style PRNG. Returns a uniform float in [0, 1). Quality
/// only has to fool human eyes scanning wander patterns.
pub(crate) fn rand_unit(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    let bits = (z ^ (z >> 31)) as u32;
    bits as f32 / (u32::MAX as f32 + 1.0)
}

#[cfg(test)]
mod tests {

    //! Pure-data tests for the preempt path. The full integration
    //! ("preempted NPC ends up Resting / Interacting next tick") needs
    //! a real brain-tick app and is covered by manual smoke. Here we
    //! pin down the decision + cleanup logic so a future refactor
    //! can't silently break it.

    use super::*;
    use crate::haul::{HaulAssignment, HaulStore, ReservedItem};
    use crate::interactables::InteractionClaims;
    use crate::items::ItemSlot;

    /// Minimal `Walkability` world for the path-invalidation helpers:
    /// explicit solid set, everything else air.
    struct TestGrid {
        solid: HashSet<IVec3>,
    }

    impl TestGrid {
        /// Floor plane at y=0 spanning x,z in [-10, 10].
        fn floored() -> Self {
            let mut solid = HashSet::new();
            for x in -10..=10 {
                for z in -10..=10 {
                    solid.insert(IVec3::new(x, 0, z));
                }
            }
            Self { solid }
        }
    }

    impl Walkability for TestGrid {
        fn is_solid(&self, cell: IVec3) -> bool {
            self.solid.contains(&cell)
        }
    }

    #[test]
    fn path_envelope_hit_covers_support_head_and_corridor() {
        let path = vec![IVec3::new(0, 1, 0), IVec3::new(4, 1, 0)];
        // Support cell below the path.
        assert!(path_envelope_hit(&path, &[IVec3::new(2, 0, 0)]));
        // Head / step-up clearance above (up to +2).
        assert!(path_envelope_hit(&path, &[IVec3::new(2, 3, 0)]));
        // XZ-adjacent cell a smoothed corridor could sweep.
        assert!(path_envelope_hit(&path, &[IVec3::new(2, 1, 1)]));
        // Outside the envelope: too high, too far along X.
        assert!(!path_envelope_hit(&path, &[IVec3::new(2, 4, 0)]));
        assert!(!path_envelope_hit(&path, &[IVec3::new(6, 1, 0)]));
        // No false positive from an empty edit list.
        assert!(!path_envelope_hit(&path, &[]));
    }

    #[test]
    fn remaining_path_valid_skips_walked_prefix() {
        let mut world = TestGrid::floored();
        let path = vec![IVec3::new(0, 1, 0), IVec3::new(4, 1, 0), IVec3::new(4, 1, 3)];
        assert!(remaining_path_valid(&path, 0, &world));

        // Wall dropped onto the FIRST segment breaks the path from the
        // start…
        world.solid.insert(IVec3::new(2, 1, 0));
        world.solid.insert(IVec3::new(2, 2, 0));
        assert!(!remaining_path_valid(&path, 0, &world));
        // …but an NPC already past that segment (first segment is 4
        // long) doesn't care.
        assert!(remaining_path_valid(&path, 1, &world));
    }

    #[test]
    fn remaining_path_valid_checks_support_removal() {
        let mut world = TestGrid::floored();
        let path = vec![IVec3::new(0, 1, 0), IVec3::new(4, 1, 0)];
        // Dig out the floor under a mid-path cell: the waypoint's
        // corridor loses support, so the path is invalid.
        world.solid.remove(&IVec3::new(2, 0, 0));
        assert!(!remaining_path_valid(&path, 0, &world));
    }
    use crate::plan_claims::PlanClaims;
    use bevy::prelude::{Entity, IVec3, Vec3};

    fn dummy_work_goal(cell: IVec3) -> Goal {
        Goal::Working {
            remaining_secs: 4.0,
            target_cell: cell,
            plan_kind: PlanKind::Remove,
            need_restore: None,
        }
    }

    fn dummy_pickup_movetomove(item_entity: Entity, plan_cell: IVec3) -> Goal {
        Goal::MoveTo {
            path: vec![IVec3::ZERO, plan_cell],
            edge: 0,
            blocked: false,
            deadline_secs: 30.0,
            on_arrive: ArrivalAction::PickupForPlan {
                item_entity,
                item_slot: ItemSlot(0),
                plan_cell,
            },
            snap: None,
        }
    }

    #[test]
    fn preempt_eligible_picks_work_and_haul_only() {
        let cell = IVec3::new(1, 2, 3);
        assert!(preempt_eligible(&dummy_work_goal(cell)));
        assert!(preempt_eligible(&dummy_pickup_movetomove(
            Entity::from_raw_u32(7).unwrap(),
            cell
        )));
        assert!(preempt_eligible(&Goal::MoveTo {
            path: vec![cell],
            edge: 0,
            blocked: false,
            deadline_secs: 30.0,
            on_arrive: ArrivalAction::Work {
                duration_secs: 4.0,
                target_cell: cell,
                plan_kind: PlanKind::Remove,
                need_restore: None,
            },
            snap: None,
        }));
        // Phase 6c-A: crafting at a station is preemptable (a tired
        // NPC mid-craft should drop the workbench and head for bed).
        // MoveTo with WorkStation arrival is too — we'd rather pivot
        // to survival than walk to the workbench just to be preempted
        // on arrival.
        assert!(preempt_eligible(&Goal::CraftingAtStation {
            station_cell: cell,
        }));
        assert!(preempt_eligible(&Goal::MoveTo {
            path: vec![cell],
            edge: 0,
            blocked: false,
            deadline_secs: 30.0,
            on_arrive: ArrivalAction::WorkStation { station_cell: cell },
            snap: None,
        }));

        // Idle / Resting / Interacting are off-limits to preempt — the
        // planner picks Rest/Interact when survival is critical, so
        // preempting them would yank the NPC off the very action that
        // addresses the need.
        assert!(!preempt_eligible(&Goal::Idle));
        assert!(!preempt_eligible(&Goal::Resting {
            remaining_secs: 1.0
        }));
        assert!(!preempt_eligible(&Goal::Interacting {
            remaining_secs: 1.0,
            need_restore: None,
            target_cell: cell,
            anchor_cell: cell,
            exclusive: true,
            animation: None,
        }));
        // MoveTo with no follow-on (Wander/Goto) and MoveTo→Interact
        // are both planner-picked under high need; neither should be
        // preempted.
        assert!(!preempt_eligible(&Goal::MoveTo {
            path: vec![cell],
            edge: 0,
            blocked: false,
            deadline_secs: 30.0,
            on_arrive: ArrivalAction::None,
            snap: None,
        }));
        assert!(!preempt_eligible(&Goal::MoveTo {
            path: vec![cell],
            edge: 0,
            blocked: false,
            deadline_secs: 30.0,
            on_arrive: ArrivalAction::Interact {
                need_restore: None,
                duration_secs: 4.0,
                target_cell: cell,
                anchor_cell: cell,
                exclusive: false,
                animation: None,
            },
            snap: None,
        }));
    }

    #[test]
    fn preempt_release_holds_releases_plan_claim_on_working() {
        let npc = NpcId(42);
        let cell = IVec3::new(5, 6, 7);
        let mut plan_claims = PlanClaims::default();
        let mut interaction_claims = InteractionClaims::default();
        let mut haul_store = HaulStore::default();
        assert!(plan_claims.try_claim(cell, npc));

        preempt_release_holds(
            npc,
            &dummy_work_goal(cell),
            &mut plan_claims,
            &mut interaction_claims,
            &mut haul_store,
        );

        // After release, another NPC can claim the same cell.
        let other = NpcId(43);
        assert!(plan_claims.try_claim(cell, other));
    }

    #[test]
    fn preempt_release_holds_drops_haul_assignment_and_reservations() {
        let npc = NpcId(7);
        let item_entity = Entity::from_raw_u32(101).unwrap();
        let plan_cell = IVec3::new(0, 0, 5);
        let mut plan_claims = PlanClaims::default();
        let mut interaction_claims = InteractionClaims::default();
        let mut haul_store = HaulStore::default();

        assert!(haul_store.try_reserve_internal(item_entity, npc));
        haul_store.commit_assignment(
            npc,
            HaulAssignment {
                target: crate::haul::HaulTarget::Plan(plan_cell),
                queue: vec![ReservedItem {
                    entity: item_entity,
                    item: ItemSlot(0),
                    translation: Vec3::ZERO,
                }],
                pending_tool: None,
            },
        );
        assert!(haul_store.has_assignment(npc));

        preempt_release_holds(
            npc,
            &dummy_pickup_movetomove(item_entity, plan_cell),
            &mut plan_claims,
            &mut interaction_claims,
            &mut haul_store,
        );

        assert!(!haul_store.has_assignment(npc));
        let other = NpcId(8);
        assert!(haul_store.try_reserve_internal(item_entity, other));
    }

    #[test]
    fn preempt_release_holds_noop_on_idle_and_resting() {
        let npc = NpcId(1);
        let cell = IVec3::new(1, 1, 1);
        let mut plan_claims = PlanClaims::default();
        let mut interaction_claims = InteractionClaims::default();
        let mut haul_store = HaulStore::default();
        assert!(plan_claims.try_claim(cell, npc));

        preempt_release_holds(
            npc,
            &Goal::Idle,
            &mut plan_claims,
            &mut interaction_claims,
            &mut haul_store,
        );
        preempt_release_holds(
            npc,
            &Goal::Resting {
                remaining_secs: 2.0,
            },
            &mut plan_claims,
            &mut interaction_claims,
            &mut haul_store,
        );

        // The unrelated plan claim survives — both no-ops left it
        // alone, so another NPC still can't take it.
        let other = NpcId(2);
        assert!(!plan_claims.try_claim(cell, other));
    }

    #[test]
    fn npc_id_allocator_is_monotonic_and_respects_loaded_ids() {
        let mut alloc = NpcIdAllocator::default();
        assert_eq!(alloc.allocate(), NpcId(1));
        assert_eq!(alloc.allocate(), NpcId(2));
        // Loading a save with ids up to 9 must push the counter past
        // them — and reserving something already-passed is a no-op.
        alloc.reserve_through(9);
        alloc.reserve_through(3);
        assert_eq!(alloc.allocate(), NpcId(10));
    }
}
