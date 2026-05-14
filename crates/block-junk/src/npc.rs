//! NPCs — server-authoritative actors driven by a needs-based brain.
//!
//! Starting slice (validates the puppeteer model before adding the Lua
//! planner surface): one trivial NPC kind, one need (hunger), one goal
//! (wander toward a random nearby target via A*). The brain writes a
//! `MovementIntent` each tick and the same `apply_walk_step` players
//! use consumes it — single physics path for any actor.
//!
//! Not yet present: the Lua planner (the planner here is hardcoded
//! Rust), per-NPC error isolation, save/load, road-graph hierarchical
//! pathfinding. See the design memo's future-work backlog.

use bevy::prelude::*;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::blocks::BlockRegistry;
use crate::collision::WorldCollision;
use crate::pathfinding::{Walkability, find_path, nearest_standable_below, smooth_path};
use crate::physics::{EYE_OFFSET_FROM_CENTRE, PLAYER_HALF_EXTENTS, apply_walk_step};
use crate::protocol::{
    Actor, AvatarOnGround, AvatarPose, AvatarVelocity, MovementIntent, MovementMode,
};
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
/// Server-only today; allocated from a monotonic counter.
#[derive(Component, Clone, Copy, Debug)]
#[allow(dead_code, reason = "consumed by save/load layer (future work)")]
pub struct NpcId(pub u64);

/// Mod-namespaced kind, e.g. `vanilla:wanderer`. Determines which need
/// table and planner the brain runs. Currently inert (only one kind
/// exists and the brain is hardcoded); becomes load-bearing when the
/// Lua planner surface lands.
#[derive(Component, Clone, Debug)]
#[allow(dead_code, reason = "selects the brain/need table once Lua planners exist")]
pub struct NpcKind(pub String);

/// Floating-point need state. 0.0 = fully satisfied, 1.0 = critical.
/// Goal selection (future) will score actions by deficit reduction.
/// Starting slice has only hunger; this becomes a registry-backed map
/// (per the design memo, "needs registered in `vanilla` mod, not engine")
/// once the mod-API surface for needs lands.
#[derive(Component, Clone, Debug, Default)]
pub struct Needs {
    pub hunger: f32,
}

/// Per-second decay applied to each need. 1/300 ⇒ ~5 minutes of game
/// time from 0 to critical. Slow on purpose: until food + sleep systems
/// exist, an NPC visibly starving would just be confusing.
const HUNGER_DECAY_PER_SEC: f32 = 1.0 / 300.0;

/// Current goal the brain is executing.
///
/// MOD HOOK (future): the transition from `Idle` is the seam where
/// a Lua planner will eventually decide what to do next based on
/// `Needs`, opinions, and the world. Today the engine hardcodes
/// "Wander → Resting → Wander → …" because there's only one need
/// (hunger, decorative) and one action (wander). The shape of this
/// enum is what mods will return from their planner callback.
#[derive(Clone, Debug)]
pub enum Goal {
    /// No active goal — brain picks one next tick. Newly-spawned
    /// NPCs start here, and any `Resting` that expired drops back
    /// here so the planner picks a fresh action.
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
    /// Stand still for a while. Inserted between Wanders so the NPC
    /// reads as deliberate rather than frantic. Duration is sampled
    /// per-rest from `[REST_MIN_SECS, REST_MAX_SECS]`.
    ///
    /// Once needs/opinions exist, mods will replace the random
    /// duration with something derived from state ("rest until
    /// fatigue < 0.4", "look at a thing for a beat after greeting
    /// someone").
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

const WANDER_RADIUS_CELLS: i32 = 12;
const WANDER_TIMEOUT_SECS: f32 = 12.0;
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
/// Random rest duration between Wanders, in seconds. Keeps the NPC
/// from looking frantic. Once mods can compute this from needs the
/// range goes away; for now, a coarse uniform sample.
const REST_MIN_SECS: f32 = 3.0;
const REST_MAX_SECS: f32 = 8.0;
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
/// re-spawn the smoke-test NPC. `AtomicBool` rather than `Local<bool>`
/// because observers don't take `Local`.
static SMOKE_TEST_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Smoke-test spawn — one NPC near the player's default landing spot
/// (player spawn = (0, 32, 60)). 4 m offset puts them in view but out
/// of collision range. Replicated to all clients with interpolation
/// (no client predicts NPCs — there's no per-client "owner" of one).
fn spawn_initial_npc_on_first_connect(_: On<Add, Connected>, mut commands: Commands) {
    if SMOKE_TEST_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let id: u64 = 1;
    commands.spawn((
        Actor,
        Npc,
        NpcId(id),
        NpcKind("vanilla:wanderer".into()),
        Needs::default(),
        Brain {
            goal: Goal::Idle,
            rng: 0xDEAD_BEEF_CAFE_F00D ^ id,
        },
        AvatarPose {
            translation: Vec3::new(4.0, 32.0, 60.0),
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
    info!("spawned smoke-test NPC #{id}");
}

/// Adapter that lets pathfinding query the live world. Treats unloaded
/// chunks as solid so the search doesn't commit to a path through
/// territory whose contents we don't know.
struct WorldWalk<'q, 'w, 's> {
    chunks: &'q Query<'w, 's, &'static Chunk>,
    chunk_map: &'q ChunkMap,
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
        !chunk.get(local).is_empty()
    }
    // cost() default 1.0 — future road tags hook in here without
    // changing the algorithm.
}

/// Per fixed-tick brain. Three phases per NPC:
///   1. Decay needs.
///   2. Tick the active goal (advance waypoint, check completion,
///      detect stuck).
///   3. If Idle (newly or post-completion), try to pick + path a new
///      wander target. On A* failure stay Idle and retry next tick.
///   4. Steer the `MovementIntent` toward the current waypoint.
fn npc_brain_tick(
    time: Res<Time>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    mut npcs: Query<
        (
            &AvatarPose,
            &mut Needs,
            &mut Brain,
            &mut MovementIntent,
            &mut NpcPath,
        ),
        With<Npc>,
    >,
) {
    let dt = time.delta_secs();
    let world = WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
    };

    for (pose, mut needs, mut brain, mut intent, mut npc_path) in npcs.iter_mut() {
        needs.hunger = (needs.hunger + HUNGER_DECAY_PER_SEC * dt).min(1.0);

        // Phase 1: advance the active goal. Each branch may flip a
        // local flag asking Phase 2 to transition the goal.
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

                // Project the pose onto the path, monotonically.
                let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
                let new_progress = closest_progress_after(path, pose_xz, *progress);
                if new_progress > *progress {
                    *progress = new_progress;
                }

                // Completion is "near the end" — the progress
                // gate from the previous version turned the orbit
                // problem into a livelock when the NPC's turn
                // radius prevented it from tightening into a
                // small arrive radius. PATH_ARRIVE_RADIUS is now
                // wider than the turn radius so this fires.
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

        // Phase 2: transition. Wander → Resting → Idle → Wander.
        // MOD HOOK: this is the seam where a Lua planner will
        // eventually decide what comes next based on Needs +
        // opinions + world state. Today the engine rotates
        // mechanically through the three states.
        if wander_done {
            let secs = REST_MIN_SECS
                + rand_unit(&mut brain.rng) * (REST_MAX_SECS - REST_MIN_SECS);
            brain.goal = Goal::Resting {
                remaining_secs: secs,
            };
            // Hide the (now-stale) overlay during rest.
            if !npc_path.0.is_empty() {
                npc_path.0.clear();
            }
        } else if rest_done {
            brain.goal = Goal::Idle;
        }
        if matches!(brain.goal, Goal::Idle) {
            let foot = pose_to_foot_cell(pose);
            match pick_wander_path(foot, &mut brain.rng, &world) {
                Some(path) => {
                    // `set_if_neq` keeps the wire quiet on the
                    // rare tick where the planner happens to pick
                    // an identical path twice.
                    npc_path.set_if_neq(NpcPath(path.clone()));
                    brain.goal = Goal::Wander {
                        path,
                        progress: 0.0,
                        deadline_secs: WANDER_TIMEOUT_SECS,
                        last_pos: pose.translation,
                        stuck_secs: 0.0,
                    };
                }
                None => {
                    if !npc_path.0.is_empty() {
                        npc_path.0.clear();
                    }
                    // Stay Idle; retry next tick.
                }
            }
        }

        // Phase 3: steering. Only Wander drives intent; Idle and
        // Resting both stay still.
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

/// Try a few random XZ targets within `WANDER_RADIUS_CELLS` of `foot`;
/// project each onto the surface via `nearest_standable_below`; run A*.
/// Return the first path that has at least one step. `None` if every
/// attempt fails (caller stays Idle and retries next tick).
fn pick_wander_path<W: Walkability>(
    foot: IVec3,
    rng: &mut u64,
    world: &W,
) -> Option<Vec<IVec3>> {
    for _ in 0..MAX_WANDER_ATTEMPTS {
        let dx = (rand_unit(rng) * 2.0 - 1.0) * WANDER_RADIUS_CELLS as f32;
        let dz = (rand_unit(rng) * 2.0 - 1.0) * WANDER_RADIUS_CELLS as f32;
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
