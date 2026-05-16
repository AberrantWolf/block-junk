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
use block_junk_mod_api::blocks::{Cardinal, Consumable, Sleeper};
use block_junk_mod_api::npcs::{
    NearbyConsumable, NearbyPlan, NearbyRoom, NearbySleeper, NpcKindId, NpcSnapshot,
    PlanKindHint, PlannerGoal,
};
use block_junk_mod_api::shared::BlockPos;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blocks::BlockRegistry;
use crate::collision::WorldCollision;
use crate::consumables::ConsumableIndex;
use crate::npc_registry::{NeedRegistry, NpcKindRegistry};
use crate::pathfinding::{Walkability, find_path, nearest_standable_below, smooth_path, standable};
use crate::rooms::RoomMap;
use crate::physics::{EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step};
use crate::plan_claims::PlanClaims;
use crate::plans::Plans;
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, MovementIntent, MovementMode, NpcActivity,
    PlanKind, WorldClock,
};
use crate::scripting::ServerMods;
use crate::sleepers::{BedClaims, SleeperIndex};
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
/// path initialises from the [`NpcKindRegistry`].
#[derive(Component, Clone, Debug)]
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
    },
    /// Stand still for a while. Duration is whatever the planner
    /// returned in [`PlannerGoal::Rest`], clamped to
    /// `[MIN_REST_SECS, MAX_REST_SECS]` so a misbehaving mod can't
    /// freeze an NPC indefinitely or churn the planner at 60 Hz.
    Resting { remaining_secs: f32 },
    /// Standing at a consumable cell, counting down to the moment the
    /// `restores` deficit is subtracted from the named need. Entered
    /// only via a successful arrival on a `MoveTo` with
    /// `ArrivalAction::Consume`. Need + magnitude are captured at goal
    /// creation so a planner that, mid-action, decides "actually you
    /// should be eating *that* food" can't retroactively change what
    /// the NPC is doing now. `target_cell` is the consumable block
    /// itself — the brain uses it to rotate the body toward whatever
    /// the NPC is interacting with, without forward motion (forward
    /// motion would orbit, since the NPC's already adjacent).
    Consuming {
        remaining_secs: f32,
        need: String,
        restores: f32,
        target_cell: IVec3,
    },
    /// Standing at a sleeper cell with a held claim, counting down to
    /// the moment `restores` is subtracted from the named need. Entered
    /// only via a successful arrival on a `MoveTo` with
    /// `ArrivalAction::Sleep`. `anchor_cell` carries the claim key so
    /// the brain releases the right slot on transition out (including
    /// when the sleep is abandoned by stuck-detection or by a planner
    /// override). `target_cell` is whichever sleeper cell the planner
    /// originally picked (foot or head); it's what the brain rotates
    /// the body toward during the sleep so the visible action lines up
    /// with the bed orientation.
    Sleeping {
        remaining_secs: f32,
        need: String,
        restores: f32,
        target_cell: IVec3,
        anchor_cell: IVec3,
    },
    /// Working a player-tagged plan at `target_cell` until the timer
    /// expires, at which point the engine applies the world mutation
    /// captured in `plan_kind`, clears the tag, releases the claim,
    /// and the brain reduces the `work` need. Entered only via a
    /// successful arrival on a [`ArrivalAction::Work`].
    ///
    /// `plan_kind` is snapshot-at-goal-commit-time so a player who
    /// re-tags the cell mid-traversal can't redirect what the NPC
    /// builds — they get to cancel the plan, but not silently swap it.
    Working {
        remaining_secs: f32,
        target_cell: IVec3,
        plan_kind: PlanKind,
    },
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
    /// Begin a stand-still consume action at the target cell. The
    /// captured `need` / `restores` / `duration_secs` are the values
    /// the snapshot saw — taken from the block's
    /// [`Consumable`](Consumable) metadata at planner-call time. If the
    /// block has changed by arrival, the brain re-validates against the
    /// current cell before applying the restoration. `target_cell` is
    /// the consumable block itself (not the stand cell); the body is
    /// rotated toward this cell during [`Goal::Consuming`].
    Consume {
        need: String,
        restores: f32,
        duration_secs: f32,
        target_cell: IVec3,
    },
    /// Begin a sleep action at the target sleeper cell. Captured
    /// values mirror `Consume`; `anchor_cell` is the claim key the
    /// brain reserved at goal commit time and must release on any
    /// path out of [`Goal::Sleeping`]. If the brain reaches the
    /// arrival check and the bed has been broken or the claim was
    /// somehow lost mid-traversal, the action degrades silently to
    /// "stand briefly, then idle."
    Sleep {
        need: String,
        restores: f32,
        duration_secs: f32,
        target_cell: IVec3,
        anchor_cell: IVec3,
        /// Bed's stored placement orientation. Snapped onto the NPC's
        /// pose yaw at arrival so they end up aligned with the bed's
        /// long axis instead of facing whatever direction their last
        /// walk segment happened to leave them in.
        orientation: Cardinal,
    },
    /// Begin a work action at the plan target cell. Carries the snapshot
    /// of the `PlanKind` at the moment the goal was committed so a
    /// mid-traversal tag swap (player edits the tag while NPC is en
    /// route) doesn't redirect the work — the player gets to cancel
    /// but not silently re-aim.
    Work {
        duration_secs: f32,
        target_cell: IVec3,
        plan_kind: PlanKind,
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
const MAX_CONSUME_TIMEOUT_SECS: f32 = 120.0;
const MIN_REST_SECS: f32 = 0.5;
const MAX_REST_SECS: f32 = 60.0;
/// Per the consumable-duration validation in `BlockRegistry`, mods
/// can't register a duration shorter than 0.1 s; the brain enforces a
/// matching upper bound so a 5-minute ritual eat is the worst-case
/// frozen NPC. Both bounds are applied as a clamp at goal creation
/// so the running goal always carries a sane remaining_secs.
const MIN_CONSUME_DURATION_SECS: f32 = 0.1;
const MAX_CONSUME_DURATION_SECS: f32 = 30.0;
/// Sleep is allowed to last longer than a consumable action by
/// design — it's intended to feel like minutes, not seconds. Lower
/// bound mirrors the registry's `validate_sleepers` lower bound
/// (>= 1.0); upper bound caps the worst-case "NPC is parked here"
/// at a couple of game minutes so a misbehaving mod or a typo in a
/// duration field doesn't permanently freeze an NPC.
const MIN_SLEEP_DURATION_SECS: f32 = 1.0;
const MAX_SLEEP_DURATION_SECS: f32 = 120.0;
const MAX_SLEEP_TIMEOUT_SECS: f32 = 120.0;
/// How far the snapshot builder scans for sleepers. Same magnitude
/// as `SNAPSHOT_CONSUMABLE_RADIUS_CELLS` — a bed across the world
/// shouldn't influence a planner's pick.
const SNAPSHOT_SLEEPER_RADIUS_CELLS: i32 = 48;
const SNAPSHOT_SLEEPER_LIMIT: usize = 8;
/// Plan-pickup scan radius (Manhattan via the Chebyshev pre-filter).
/// Same magnitude as sleepers/consumables — plans on the far side of
/// the map shouldn't keep luring a villager from local tasks.
const SNAPSHOT_PLAN_RADIUS_CELLS: i32 = 48;
const SNAPSHOT_PLAN_LIMIT: usize = 8;
/// How long a single-cell work action takes the NPC. Picked so a
/// villager visibly stands at the target for a few seconds (matches
/// the player's own [`crate::client::PLAYER_ACTION_DURATION_SECS`] in
/// feel without being identical — NPCs are slower at hand-crafting).
/// Tune as work animations land.
const WORK_DURATION_SECS: f32 = 4.0;
const MAX_WORK_TIMEOUT_SECS: f32 = 120.0;
/// How much of the `work` need a completed action satisfies. Same
/// magnitude as a sleeper's `restores` floor — one job moves the
/// villager from "looking for purpose" back toward content.
const WORK_RESTORES: f32 = 0.35;
const WORK_NEED_ID: &str = "work";
/// How many nearby matched rooms to include in each planner snapshot.
/// Cap exists so a world with hundreds of registered rooms doesn't
/// blow up the per-call serialization cost; 8 is enough headroom for
/// a planner to pick between "nearest of each kind" without flooding
/// the table.
const SNAPSHOT_ROOM_LIMIT: usize = 8;
/// Same idea as `SNAPSHOT_ROOM_LIMIT` for the consumables array. 8 is
/// plenty for "nearest of each need" picks in early-game; the planner
/// only sees the closest entries so a player who places hundreds of
/// food blocks doesn't blow up the per-call cost.
const SNAPSHOT_CONSUMABLE_LIMIT: usize = 8;
/// Chebyshev radius the snapshot builder scans for consumables. Past
/// this the NPC won't see a food block at all — it'll wander toward
/// rooms or random targets until it bumps into one. 48 cells ≈ 3
/// chunks at CHUNK_SIZE = 16, big enough to cover a small settlement.
const SNAPSHOT_CONSUMABLE_RADIUS_CELLS: i32 = 48;
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

/// Map [`Brain::goal`] onto the coarse [`NpcActivity`] enum the client
/// reads for animation. Walking is decided client-side from velocity
/// (its hysteresis prevents strobing on stop/start), so MoveTo here
/// maps to `Idle` — the client takes over once velocity rises.
/// Resting and Consuming also map to `Idle` until we have dedicated
/// clips for them.
fn refresh_npc_activity(mut npcs: Query<(&Brain, &mut NpcActivity), With<Npc>>) {
    for (brain, mut activity) in npcs.iter_mut() {
        let next = match &brain.goal {
            Goal::Sleeping { .. } => NpcActivity::Sleeping,
            Goal::Working { .. } => NpcActivity::Working,
            _ => NpcActivity::Idle,
        };
        // `set_if_neq` keeps the replication channel quiet on the common
        // path (the activity changes once per goal transition, not per
        // tick).
        activity.set_if_neq(next);
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
            NpcActivity::default(),
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
#[allow(clippy::too_many_arguments, reason = "brain tick spans many subsystems")]
fn npc_brain_tick(
    time: Res<Time>,
    chunks: Query<&'static Chunk>,
    chunk_entities_q: Query<&'static ChunkEntities>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mods: Res<ServerMods>,
    need_registry: Res<NeedRegistry>,
    room_map: Res<RoomMap>,
    consumable_index: Res<ConsumableIndex>,
    sleeper_index: Res<SleeperIndex>,
    mut bed_claims: ResMut<BedClaims>,
    plans: Res<Plans>,
    mut plan_claims: ResMut<PlanClaims>,
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
            &NpcKind,
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

    for (entity, npc_id, mut pose, mut needs, mut brain, mut intent, mut npc_path, kind) in
        npcs.iter_mut()
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
        let mut move_arrived: Option<ArrivalAction> = None;
        let mut move_abandoned = false;
        let mut rest_done = false;
        let mut consume_done = false;
        let mut sleep_done = false;
        let mut work_done = false;
        match &mut brain.goal {
            Goal::Idle => {}
            Goal::Resting { remaining_secs } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    rest_done = true;
                }
            }
            Goal::Consuming { remaining_secs, .. } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    consume_done = true;
                }
            }
            Goal::Sleeping { remaining_secs, .. } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    sleep_done = true;
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
                // could orbit its target forever.
                let end_xz = waypoint_xz(*path.last().expect("path non-empty"));
                let dist_to_end = (pose_xz - end_xz).length();
                if dist_to_end < PATH_ARRIVE_RADIUS {
                    move_arrived = Some(on_arrive.clone());
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
        if let Some(action) = move_arrived {
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
            *intent = MovementIntent::default();
            match action {
                ArrivalAction::None => {
                    brain.goal = Goal::Idle;
                }
                ArrivalAction::Consume {
                    need,
                    restores,
                    duration_secs,
                    target_cell,
                } => {
                    brain.goal = Goal::Consuming {
                        remaining_secs: duration_secs,
                        need,
                        restores,
                        target_cell,
                    };
                }
                ArrivalAction::Sleep {
                    need,
                    restores,
                    duration_secs,
                    target_cell,
                    anchor_cell,
                    orientation,
                } => {
                    pose.yaw = sleep_yaw_for(orientation);
                    brain.goal = Goal::Sleeping {
                        remaining_secs: duration_secs,
                        need,
                        restores,
                        target_cell,
                        anchor_cell,
                    };
                }
                ArrivalAction::Work {
                    duration_secs,
                    target_cell,
                    plan_kind,
                } => {
                    brain.goal = Goal::Working {
                        remaining_secs: duration_secs,
                        target_cell,
                        plan_kind,
                    };
                }
            }
        }
        // Abandonment of a MoveTo whose ArrivalAction is Sleep or Work
        // needs to release the claim too — the brain reserved it at
        // goal commit time and a stuck/timeout abandon never reaches
        // arrival.
        if move_abandoned {
            if let Goal::MoveTo { on_arrive, .. } = &brain.goal {
                match on_arrive {
                    ArrivalAction::Sleep { anchor_cell, .. } => {
                        bed_claims.release(*anchor_cell, *npc_id);
                    }
                    ArrivalAction::Work { target_cell, .. } => {
                        plan_claims.release(*target_cell, *npc_id);
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
        if consume_done {
            // Re-resolve the consumable at the current goal's stored
            // need/restores: capture happened at planner-call time,
            // but the block may have been broken or replaced since.
            // We trust the captured values (the NPC saw them when
            // committing) and just decrement — if the block was
            // removed, the NPC still "ate the air" once, which is
            // the same as accepting the stale snapshot at all
            // upstream layers. Player can't exploit it because the
            // duration_secs hold them in place for the full action.
            if let Goal::Consuming { need, restores, .. } = &brain.goal {
                if let Some(value) = needs.0.get_mut(need) {
                    *value = (*value - *restores).max(0.0);
                    info!(
                        npc = npc_id.0,
                        need = %need,
                        restored = restores,
                        remaining_deficit = *value,
                        "consumption complete",
                    );
                } else {
                    warn!(
                        npc = npc_id.0,
                        need = %need,
                        "consumption complete but NPC has no entry for need; ignoring",
                    );
                }
            }
            brain.goal = Goal::Idle;
        }
        if sleep_done {
            // Same logic as consume_done plus releasing the per-bed
            // claim. Captured `need` / `restores` are trusted — if the
            // bed was destroyed mid-sleep, the NPC still wakes refreshed
            // (the action played out from their point of view).
            if let Goal::Sleeping {
                need,
                restores,
                anchor_cell,
                ..
            } = &brain.goal
            {
                if let Some(value) = needs.0.get_mut(need) {
                    *value = (*value - *restores).max(0.0);
                    info!(
                        npc = npc_id.0,
                        need = %need,
                        restored = restores,
                        remaining_deficit = *value,
                        "sleep complete",
                    );
                } else {
                    warn!(
                        npc = npc_id.0,
                        need = %need,
                        "sleep complete but NPC has no entry for need; ignoring",
                    );
                }
                bed_claims.release(*anchor_cell, *npc_id);
            }
            brain.goal = Goal::Idle;
        }
        if work_done {
            // Reduce `work` need, release the claim, and emit a local
            // message for the server-side consumer to apply the world
            // mutation (place or break) + clear the plan tag. Splitting
            // it across systems keeps the brain tick under the
            // SystemParam cap — the consumer needs the broadcast
            // sender + chunk-write params the brain doesn't carry.
            if let Goal::Working {
                target_cell,
                plan_kind,
                ..
            } = &brain.goal
            {
                if let Some(value) = needs.0.get_mut(WORK_NEED_ID) {
                    *value = (*value - WORK_RESTORES).max(0.0);
                    info!(
                        npc = npc_id.0,
                        cell = ?target_cell.to_array(),
                        kind = ?plan_kind,
                        restored = WORK_RESTORES,
                        remaining_deficit = *value,
                        "work complete",
                    );
                } else {
                    warn!(
                        npc = npc_id.0,
                        "work complete but NPC has no `work` need entry; ignoring",
                    );
                }
                commands.write_message(NpcWorkCompleted {
                    cell: *target_cell,
                    plan_kind: *plan_kind,
                });
                plan_claims.release(*target_cell, *npc_id);
            }
            brain.goal = Goal::Idle;
        }
        if matches!(brain.goal, Goal::Idle) {
            let kind_id = NpcKindId(kind.0.clone());
            let snapshot = build_snapshot(
                *npc_id,
                &kind_id,
                &pose,
                &needs,
                &room_map,
                &consumable_index,
                &sleeper_index,
                &bed_claims,
                &plans,
                &plan_claims,
                &chunk_entities_q,
                &chunk_map,
                &block_registry,
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
                nearby_consumables = snapshot.nearby_consumables.len(),
                nearby_sleepers = snapshot.nearby_sleepers.len(),
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
                    // the session.
                    bed_claims.release_all_for(*npc_id);
                    plan_claims.release_all_for(*npc_id);
                    commands.entity(entity).insert(BrainDisabled {
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
                    let target = IVec3::new(cell.x, cell.y, cell.z);
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
                PlannerGoal::Sleep { cell, timeout_secs } => {
                    let timeout = timeout_secs.clamp(1.0, MAX_SLEEP_TIMEOUT_SECS);
                    let target_cell = IVec3::new(cell.x, cell.y, cell.z);
                    // Re-resolve the sleeper on current world state. A
                    // planner that picked a bed seconds ago might now
                    // be looking at empty air or a non-bed replacement.
                    let sleeper = match sleeper_at_cell(
                        target_cell,
                        &chunks,
                        &chunk_map,
                        &block_registry,
                    ) {
                        Some(s) => s,
                        None => {
                            info!(
                                npc = npc_id.0,
                                target = ?target_cell.to_array(),
                                "sleep target no longer a sleeper; parking briefly",
                            );
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                            continue;
                        }
                    };
                    // Resolve the bed's anchor + its stored orientation.
                    // Anchor doubles as the claim key (foot and head of a
                    // multi-cell bed contend for the same slot); the
                    // orientation feeds the yaw snap on arrival so the
                    // sleeping NPC ends up aligned with the bed's long
                    // axis instead of pointing wherever the last walk
                    // segment left them.
                    let (anchor_cell, bed_orientation) = resolve_anchor_with_orientation(
                        target_cell,
                        &chunk_entities_q,
                        &chunk_map,
                    );
                    // Atomic claim. Failure means another NPC took the
                    // bed between snapshot construction and now — fall
                    // through to a brief rest, the planner will re-pick.
                    if !bed_claims.try_claim(anchor_cell, *npc_id) {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            anchor = ?anchor_cell.to_array(),
                            "sleep target claimed by another NPC; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    }
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    // Sleep stands the NPC *on* the bed (cell directly
                    // above the anchor), not next to it like a consumable.
                    // `support_below = true` on the bed makes the cell
                    // above standable for free; if it isn't (overhang /
                    // low ceiling), fall back to the cardinal-neighbour
                    // search so a placed-in-a-tight-spot bed still gives
                    // *some* sleep target rather than refusing to sleep.
                    let atop_anchor = anchor_cell + IVec3::Y;
                    let stand_cell = if standable(&world, atop_anchor) {
                        atop_anchor
                    } else if let Some(c) =
                        nearest_standable_neighbor(target_cell, foot, &world)
                    {
                        c
                    } else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "no standable cell atop sleeper or in its cardinal neighbours; releasing claim and parking briefly",
                        );
                        bed_claims.release(anchor_cell, *npc_id);
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    };
                    let duration = sleeper
                        .duration_secs
                        .clamp(MIN_SLEEP_DURATION_SECS, MAX_SLEEP_DURATION_SECS);
                    if stand_cell == foot {
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        pose.yaw = sleep_yaw_for(bed_orientation);
                        brain.goal = Goal::Sleeping {
                            remaining_secs: duration,
                            need: sleeper.need.clone(),
                            restores: sleeper.restores,
                            target_cell,
                            anchor_cell,
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
                                on_arrive: ArrivalAction::Sleep {
                                    need: sleeper.need.clone(),
                                    restores: sleeper.restores,
                                    duration_secs: duration,
                                    target_cell,
                                    anchor_cell,
                                    orientation: bed_orientation,
                                },
                            };
                        }
                        None => {
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target_cell.to_array(),
                                stand = ?stand_cell.to_array(),
                                "sleep failed: no A* path to standable neighbour, releasing claim and parking briefly"
                            );
                            bed_claims.release(anchor_cell, *npc_id);
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
                PlannerGoal::Consume { cell, timeout_secs } => {
                    let timeout = timeout_secs.clamp(1.0, MAX_CONSUME_TIMEOUT_SECS);
                    let target_cell = IVec3::new(cell.x, cell.y, cell.z);
                    // Re-resolve the consumable on the current world
                    // state (not the snapshot). Mods can in principle
                    // hand back any cell — we only path to it if it
                    // still has consumable metadata. Stale references
                    // become a no-op + brief rest, identical to a
                    // failed path.
                    let consumable = match consumable_at_cell(
                        target_cell,
                        &chunks,
                        &chunk_map,
                        &block_registry,
                    ) {
                        Some(c) => c,
                        None => {
                            info!(
                                npc = npc_id.0,
                                target = ?target_cell.to_array(),
                                "consume target no longer consumable; parking briefly",
                            );
                            brain.goal = Goal::Resting {
                                remaining_secs: MIN_REST_SECS,
                            };
                            *intent = MovementIntent::default();
                            continue;
                        }
                    };
                    let foot = pose_to_standable_foot(&pose, &world)
                        .unwrap_or_else(|| pose_to_foot_cell(&pose));
                    // Consumables are typically solid blocks, so the
                    // NPC's actual stand cell is one of their
                    // neighbours. Pick the standable neighbour
                    // closest to the NPC's current foot so the path
                    // bends toward the side the NPC's already
                    // approaching from.
                    let Some(stand_cell) = nearest_standable_neighbor(target_cell, foot, &world)
                    else {
                        info!(
                            npc = npc_id.0,
                            target = ?target_cell.to_array(),
                            "no standable neighbour of consumable; parking briefly",
                        );
                        brain.goal = Goal::Resting {
                            remaining_secs: MIN_REST_SECS,
                        };
                        *intent = MovementIntent::default();
                        continue;
                    };
                    let duration = consumable
                        .duration_secs
                        .clamp(MIN_CONSUME_DURATION_SECS, MAX_CONSUME_DURATION_SECS);
                    // Already standing where we'd path to: no MoveTo
                    // needed, fire the consumption timer immediately.
                    // Common when the planner re-targets the same
                    // basket on a subsequent tick and the NPC hasn't
                    // moved, or when the NPC happens to wander into the
                    // standable cell before the planner runs.
                    if stand_cell == foot {
                        if !npc_path.0.is_empty() {
                            npc_path.0.clear();
                        }
                        brain.goal = Goal::Consuming {
                            remaining_secs: duration,
                            need: consumable.need.clone(),
                            restores: consumable.restores,
                            target_cell,
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
                                on_arrive: ArrivalAction::Consume {
                                    need: consumable.need.clone(),
                                    restores: consumable.restores,
                                    duration_secs: duration,
                                    target_cell,
                                },
                            };
                        }
                        None => {
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target_cell.to_array(),
                                stand = ?stand_cell.to_array(),
                                "consume failed: no A* path to standable neighbour, parking briefly"
                            );
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
                    let Some(plan_kind) = plans.get(target_cell) else {
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
                    if !plan_claims.try_claim(target_cell, *npc_id) {
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
                        plan_claims.release(target_cell, *npc_id);
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
                            remaining_secs: WORK_DURATION_SECS,
                            target_cell,
                            plan_kind,
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
                                    duration_secs: WORK_DURATION_SECS,
                                    target_cell,
                                    plan_kind,
                                },
                            };
                        }
                        None => {
                            warn!(
                                npc = npc_id.0,
                                foot = ?foot.to_array(),
                                target = ?target_cell.to_array(),
                                stand = ?stand_cell.to_array(),
                                "work failed: no A* path to standable neighbour, releasing claim and parking briefly"
                            );
                            plan_claims.release(target_cell, *npc_id);
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
            Goal::Consuming { target_cell, .. }
            | Goal::Sleeping { target_cell, .. }
            | Goal::Working { target_cell, .. } => {
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
    consumables: &ConsumableIndex,
    sleepers: &SleeperIndex,
    bed_claims: &BedClaims,
    plans: &Plans,
    plan_claims: &PlanClaims,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    block_registry: &BlockRegistry,
    world_clock: WorldClock,
) -> NpcSnapshot {
    let foot = pose_to_foot_cell(pose);
    let nearby_rooms = collect_nearby_rooms(rooms, foot, SNAPSHOT_ROOM_LIMIT);
    let nearby_consumables = collect_nearby_consumables(
        consumables,
        block_registry,
        foot,
        SNAPSHOT_CONSUMABLE_RADIUS_CELLS,
        SNAPSHOT_CONSUMABLE_LIMIT,
    );
    let nearby_sleepers = collect_nearby_sleepers(
        sleepers,
        bed_claims,
        block_registry,
        chunk_entities,
        chunk_map,
        id,
        foot,
        SNAPSHOT_SLEEPER_RADIUS_CELLS,
        SNAPSHOT_SLEEPER_LIMIT,
    );
    let nearby_plans = collect_nearby_plans(
        plans,
        plan_claims,
        id,
        foot,
        SNAPSHOT_PLAN_RADIUS_CELLS,
        SNAPSHOT_PLAN_LIMIT,
    );
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
        nearby_consumables,
        nearby_sleepers,
        nearby_plans,
        is_night: world_clock.is_night(),
    }
}

/// K nearest *unclaimed* plan cells within `radius_cells` (Manhattan)
/// of `foot`. Same shape as `collect_nearby_sleepers` — filter taken,
/// sort by distance, truncate to limit. The `kind` is mapped from the
/// full engine-side `PlanKind` to the simpler `PlanKindHint` exposed
/// to mods (which don't need slot + orientation to make the decision).
fn collect_nearby_plans(
    plans: &Plans,
    plan_claims: &PlanClaims,
    self_id: NpcId,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
) -> Vec<NearbyPlan> {
    let mut out: Vec<NearbyPlan> = Vec::new();
    for (cell, kind) in plans.iter() {
        let d = *cell - foot;
        if d.x.abs() > radius_cells || d.y.abs() > radius_cells || d.z.abs() > radius_cells {
            continue;
        }
        if plan_claims.is_taken_by_other(*cell, self_id) {
            continue;
        }
        let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
        let hint = match kind {
            PlanKind::Remove => PlanKindHint::Remove,
            PlanKind::Build { .. } => PlanKindHint::Build,
        };
        out.push(NearbyPlan {
            cell: BlockPos {
                x: cell.x,
                y: cell.y,
                z: cell.z,
            },
            kind: hint,
            distance,
        });
    }
    out.sort_by_key(|p| p.distance);
    out.truncate(limit);
    out
}

/// K nearest *unclaimed* beds within `radius_cells` (Chebyshev) of
/// `foot`, one entry per bed (not per cell). Multi-cell beds appear
/// in [`SleeperIndex`] once per cell (foot + head); collapsing to one
/// entry per anchor is what makes the claim check correct — without
/// it, a claimed bed's *anchor* cell filters out as taken but its
/// non-anchor cells still look free, so planners route to the same
/// bed from a different cell and the brain's `try_claim` rejects
/// them in a loop.
///
/// The emitted entry uses the anchor cell as its target (the brain's
/// Sleep handler re-resolves it to the same anchor anyway) so the
/// planner-side pick already names the claim key.
fn collect_nearby_sleepers(
    index: &SleeperIndex,
    bed_claims: &BedClaims,
    block_registry: &BlockRegistry,
    chunk_entities: &Query<&'static ChunkEntities>,
    chunk_map: &ChunkMap,
    self_id: NpcId,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
) -> Vec<NearbySleeper> {
    let mut seen_anchors: HashSet<IVec3> = HashSet::new();
    let mut out: Vec<NearbySleeper> = Vec::new();
    for (cell, slot) in index.iter_within(foot, radius_cells) {
        let Some(s) = block_registry.def(slot).sleeper.as_ref() else {
            continue;
        };
        let anchor = resolve_anchor_cell(cell, chunk_entities, chunk_map);
        // Dedupe: a 2-cell bed's foot + head both index into the same
        // anchor. Only emit one entry per bed.
        if !seen_anchors.insert(anchor) {
            continue;
        }
        // Filter beds claimed by *other* NPCs. Keying on anchor (not
        // cell) is what fixes the multi-cell-bed contention bug —
        // checking just the iterator's `cell` lets the head cell of
        // a claimed bed look free even when the foot is taken.
        if bed_claims.is_taken_by_other(anchor, self_id) {
            continue;
        }
        let d = anchor - foot;
        let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
        out.push(NearbySleeper {
            cell: BlockPos {
                x: anchor.x,
                y: anchor.y,
                z: anchor.z,
            },
            need: s.need.clone(),
            restores: s.restores,
            distance,
        });
    }
    out.sort_by_key(|s| s.distance);
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

/// NPC pose yaw that aligns the body with a bed of the given placement
/// orientation — forward points from the bed's foot toward its head.
///
/// `Cardinal::yaw()` is the *mesh* rotation (model-space +X → cardinal
/// direction). The NPC mesh's model-space forward is -Z (Bevy
/// convention), so we shift by -π/2 to convert mesh-yaw into NPC-yaw
/// for the same direction.
fn sleep_yaw_for(orientation: Cardinal) -> f32 {
    orientation.yaw() - core::f32::consts::FRAC_PI_2
}

/// Resolve the [`Sleeper`] at a world cell, if any. Returns `None`
/// when the cell is empty, the chunk isn't loaded, or the block's
/// def has no `sleeper` metadata.
fn sleeper_at_cell(
    cell: IVec3,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<Sleeper> {
    let (coord, local) = world_to_chunk(cell);
    let entity = *chunk_map.0.get(&coord)?;
    let chunk = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    registry.def(slot).sleeper.clone()
}

/// K nearest consumable cells within `radius_cells` (Chebyshev) of
/// `foot`. Each entry pulls its `need` + `restores` from the block's
/// [`Consumable`](Consumable) def via the registry — the index only
/// stores `(cell, BlockSlot)` to avoid duplicating data that ultimately
/// lives in the def.
///
/// Distance is Manhattan (consistent with `nearby_rooms.distance`); the
/// radius filter uses Chebyshev because that matches how the index's
/// internal `iter_within` is bounded. The two metrics agree on "closer
/// is closer" for ranking purposes.
fn collect_nearby_consumables(
    index: &ConsumableIndex,
    block_registry: &BlockRegistry,
    foot: IVec3,
    radius_cells: i32,
    limit: usize,
) -> Vec<NearbyConsumable> {
    let mut out: Vec<NearbyConsumable> = index
        .iter_within(foot, radius_cells)
        .filter_map(|(cell, slot)| {
            // Defensive: the index *should* never carry a slot whose def
            // lacks `consumable`, but the path from CellEdit insert →
            // def lookup goes through the registry once and the data
            // here a second time; if a future mod-reload path changes a
            // block to no longer be consumable while a stale index
            // entry remains, we just skip rather than panic.
            let c = block_registry.def(slot).consumable.as_ref()?;
            let d = cell - foot;
            let distance = (d.x.abs() + d.y.abs() + d.z.abs()) as u32;
            Some(NearbyConsumable {
                cell: BlockPos {
                    x: cell.x,
                    y: cell.y,
                    z: cell.z,
                },
                need: c.need.clone(),
                restores: c.restores,
                distance,
            })
        })
        .collect();
    out.sort_by_key(|c| c.distance);
    out.truncate(limit);
    out
}

/// Resolve the [`Consumable`] at a world cell, if any. Returns `None`
/// when the cell is empty, the chunk isn't loaded, or the block's def
/// has no `consumable` metadata. Used by the Consume goal handler to
/// re-validate the planner's choice against current world state.
fn consumable_at_cell(
    cell: IVec3,
    chunks: &Query<&Chunk>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<Consumable> {
    let (coord, local) = world_to_chunk(cell);
    let entity = *chunk_map.0.get(&coord)?;
    let chunk = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    registry.def(slot).consumable.clone()
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
fn pose_to_foot_cell(pose: &AvatarPose) -> IVec3 {
    let feet_y = pose.translation.y - EYE_OFFSET_FROM_CENTRE - PLAYER_HALF_EXTENTS.y;
    IVec3::new(
        pose.translation.x.floor() as i32,
        feet_y.floor() as i32,
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
/// input.
fn npc_physics_step(
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
        With<Npc>,
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
