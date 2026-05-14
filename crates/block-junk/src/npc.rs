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
use block_junk_mod_api::npcs::{NearbyRoom, NpcKindId, NpcSnapshot, PlannerGoal};
use block_junk_mod_api::shared::BlockPos;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blocks::BlockRegistry;
use crate::collision::WorldCollision;
use crate::npc_registry::{NeedRegistry, NpcKindRegistry};
use crate::pathfinding::{Walkability, find_path, nearest_standable_below, smooth_path};
use crate::rooms::RoomMap;
use crate::physics::{EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step};
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, MovementIntent, MovementMode,
};
use crate::scripting::ServerMods;
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, world_to_chunk};

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
/// Server-only today; allocated from a monotonic counter and exposed
/// to mods in [`NpcSnapshot::id`].
#[derive(Component, Clone, Copy, Debug)]
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

/// Current goal the brain is executing. Variants map 1:1 to the
/// [`PlannerGoal`] surface mods see, plus engine-only bookkeeping
/// fields needed to actually drive each one (current path + progress,
/// remaining timer, stuck detector). The planner returns the abstract
/// surface form; this enum is the live engine form.
#[derive(Clone, Debug)]
pub enum Goal {
    /// No active goal. Entering this state triggers a planner call on
    /// the next brain tick; newly-spawned NPCs start here, and any
    /// completed Wander/Resting drops back here so the planner picks
    /// what's next.
    Idle,
    /// Walk a precomputed A* path of foot cells using pure-pursuit
    /// steering. `progress` is the NPC's monotonically-increasing
    /// arc length along the path — recomputed each tick by
    /// projecting the pose onto the path (never backwards), then
    /// offset by `LOOKAHEAD_DIST` to produce the actual aim point.
    /// Steering toward an always-ahead carrot (vs. the current
    /// waypoint) stops the NPC from circling a point it's trying
    /// to reach faster than its turn rate allows. `last_pos` +
    /// `stuck_secs` detect a path that's become impossible (player
    /// dug in front of us, NPC wedged on a corner) and force a
    /// replan via completion.
    Wander {
        path: Vec<IVec3>,
        progress: f32,
        deadline_secs: f32,
        last_pos: Vec3,
        stuck_secs: f32,
    },
    /// Stand still for a while. Duration is whatever the planner
    /// returned in [`PlannerGoal::Rest`], clamped to
    /// `[MIN_REST_SECS, MAX_REST_SECS]` so a misbehaving mod can't
    /// freeze an NPC indefinitely or churn the planner at 60 Hz.
    Resting { remaining_secs: f32 },
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
const MIN_REST_SECS: f32 = 0.5;
const MAX_REST_SECS: f32 = 60.0;
/// How many nearby matched rooms to include in each planner snapshot.
/// Cap exists so a world with hundreds of registered rooms doesn't
/// blow up the per-call serialization cost; 8 is enough headroom for
/// a planner to pick between "nearest of each kind" without flooding
/// the table.
const SNAPSHOT_ROOM_LIMIT: usize = 8;
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
        // Brain → physics order matters: physics consumes the intent
        // the brain writes this tick. Both run in FixedUpdate alongside
        // the player physics so all actors advance together.
        app.add_systems(FixedUpdate, (npc_brain_tick, npc_physics_step).chain());
    }
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
        commands.spawn((
            Actor,
            Npc,
            NpcId(id),
            NpcKind(kind_id.into()),
            Needs(default_needs.clone()),
            Brain {
                goal: Goal::Idle,
                rng: 0xDEAD_BEEF_CAFE_F00D ^ id,
            },
            AvatarPose {
                translation,
                yaw: 0.0,
            },
            AvatarVelocity::default(),
            AvatarOnGround::default(),
            MovementMode::Walk,
            MovementIntent::default(),
            NpcPath::default(),
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
fn npc_brain_tick(
    time: Res<Time>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    mods: Res<ServerMods>,
    need_registry: Res<NeedRegistry>,
    room_map: Res<RoomMap>,
    mut commands: Commands,
    mut npcs: Query<
        (
            Entity,
            &NpcId,
            &AvatarPose,
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

    for (entity, npc_id, pose, mut needs, mut brain, mut intent, mut npc_path, kind) in
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

        // Phase 2: advance the active goal. Each branch may flip
        // `wander_done` or `rest_done` asking phase 3 to transition.
        let mut wander_done = false;
        let mut rest_done = false;
        match &mut brain.goal {
            Goal::Idle => {}
            Goal::Resting { remaining_secs } => {
                *remaining_secs -= dt;
                if *remaining_secs <= 0.0 {
                    rest_done = true;
                }
            }
            Goal::Wander {
                path,
                progress,
                deadline_secs,
                last_pos,
                stuck_secs,
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
                if dist_to_end < PATH_ARRIVE_RADIUS
                    || *deadline_secs <= 0.0
                    || *stuck_secs > STUCK_REPLAN_SECS
                {
                    wander_done = true;
                }
            }
        }

        // Phase 3: transition. Completed Wander/Rest → Idle; Idle →
        // ask the planner what's next. The planner result is
        // converted to an engine Goal (Wander picks an A* path here;
        // Rest commits a timer).
        if wander_done || rest_done {
            brain.goal = Goal::Idle;
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
        }
        if matches!(brain.goal, Goal::Idle) {
            let kind_id = NpcKindId(kind.0.clone());
            let snapshot = build_snapshot(*npc_id, &kind_id, pose, &needs, &room_map);
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
                    let foot = pose_to_foot_cell(pose);
                    match pick_wander_path(foot, radius, &mut brain.rng, &world) {
                        Some(path) => {
                            // set_if_neq keeps the wire quiet on the
                            // rare repeat path; planner-driven calls
                            // are several seconds apart so it triggers
                            // basically every time, but the guard is
                            // free if it doesn't.
                            npc_path.set_if_neq(NpcPath(path.clone()));
                            brain.goal = Goal::Wander {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                            };
                        }
                        None => {
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
                    let foot = pose_to_foot_cell(pose);
                    let target = IVec3::new(cell.x, cell.y, cell.z);
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
                            brain.goal = Goal::Wander {
                                path,
                                progress: 0.0,
                                deadline_secs: timeout,
                                last_pos: pose.translation,
                                stuck_secs: 0.0,
                            };
                        }
                        None => {
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
            }
        }

        // Phase 4: steering. Only Wander drives intent; Idle and
        // Resting both clear it (default = no motion).
        let Goal::Wander {
            path, progress, ..
        } = &brain.goal
        else {
            *intent = MovementIntent::default();
            continue;
        };
        // Pure-pursuit aim: LOOKAHEAD_DIST ahead of the closest
        // projection along the path. The `forward` in
        // `apply_walk_step` is `(-sin(yaw), 0, -cos(yaw))`, so the
        // yaw pointing toward `(dx, 0, dz)` is `atan2(-dx, -dz)`.
        let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
        let aim = lookahead_point(path, *progress, LOOKAHEAD_DIST);
        let dx = aim.x - pose_xz.x;
        let dz = aim.y - pose_xz.y;
        if dx * dx + dz * dz < f32::EPSILON {
            *intent = MovementIntent::default();
            continue;
        }
        let desired_yaw = (-dx).atan2(-dz);
        let mut delta = (desired_yaw - pose.yaw) % core::f32::consts::TAU;
        if delta > core::f32::consts::PI {
            delta -= core::f32::consts::TAU;
        } else if delta < -core::f32::consts::PI {
            delta += core::f32::consts::TAU;
        }
        let dyaw = delta.clamp(-NPC_TURN_RATE * dt, NPC_TURN_RATE * dt);
        let foot_y = pose_to_foot_cell(pose).y;
        let jump = step_up_imminent(path, *progress, foot_y);
        *intent = MovementIntent {
            wishdir: [0, 0, -1],
            jump,
            toggle_mode: false,
            interact: false,
            dyaw,
        };
    }
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
fn build_snapshot(
    id: NpcId,
    kind: &NpcKindId,
    pose: &AvatarPose,
    needs: &Needs,
    rooms: &RoomMap,
) -> NpcSnapshot {
    let foot = pose_to_foot_cell(pose);
    let nearby_rooms = collect_nearby_rooms(rooms, foot, SNAPSHOT_ROOM_LIMIT);
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
    }
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
fn pose_to_foot_cell(pose: &AvatarPose) -> IVec3 {
    let feet_y = pose.translation.y - EYE_OFFSET_FROM_CENTRE - PLAYER_HALF_EXTENTS.y;
    IVec3::new(
        pose.translation.x.floor() as i32,
        feet_y.floor() as i32,
        pose.translation.z.floor() as i32,
    )
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
