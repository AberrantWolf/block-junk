//! Kinematic NPC locomotion along the validated nav graph.
//!
//! Replaces the physics-controller execution layer (`npc_walk_step` +
//! jump impulses + stuck timers). NPCs move deterministically along the
//! smoothed cell path carried by [`Goal::MoveTo`]: same-Y edges walk at
//! constant speed, ±1-Y edges play a scripted parabolic arc, and losing
//! support plays a gravity-accelerated fall. The nav graph is the
//! single source of truth for "can I get there" — the mover never
//! consults swept collision, only the [`Walkability`] oracle, so
//! planning and execution cannot disagree.
//!
//! Failure has exactly one channel: an edge that fails its per-tick
//! oracle re-check (or a landing somewhere off-path) sets
//! `Goal::MoveTo::blocked`, and the brain repaths in place or abandons
//! through the existing claim/memo machinery. There is nothing to
//! wedge on and no displacement heuristic to time out.
//!
//! Player movement is untouched — this is NPC-only. NPCs are
//! server-authoritative and interpolated-only on clients, so the mover
//! never runs client-side; clients just lerp the replicated
//! [`AvatarPose`]. [`AvatarVelocity`] is synthesized from the pose
//! delta each tick because the client walk-animation driver keys off
//! its replicated XZ speed.

use bevy::prelude::*;

use crate::blocks::BlockRegistry;
use crate::npc::{Brain, Goal, Npc, WorldWalk, aim_yaw_step, pose_to_foot_cell, waypoint_xz};
use crate::pathfinding::{Walkability, standable};
use crate::physics::{
    EYE_OFFSET_FROM_CENTRE, GRAVITY, PLAYER_HALF_EXTENTS, WALK_SPEED, standing_pose_translation,
};
use crate::protocol::{AvatarOnGround, AvatarPose, AvatarVelocity, KinematicLock};
use crate::voxel::{Chunk, ChunkMap};

/// Seconds a ±1-cell step arc takes. A touch slower than walking one
/// cell at `WALK_SPEED` so the hop reads as deliberate effort.
const STEP_ARC_SECS: f32 = 1.3 / WALK_SPEED;

/// How far above the higher endpoint the step arc's apex reaches. The
/// visual "jump height" — enough to read as a hop, small enough that
/// the head never enters the cell above (which planning already
/// guaranteed is clear for climbs).
const STEP_ARC_APEX_CLEARANCE: f32 = 0.3;

/// Pose displacement beyond which the mover concludes something else
/// moved the body (block-placement push-out, load rescue, debug
/// teleport) and re-anchors instead of blindly continuing its edge.
const EXTERNAL_DISPLACEMENT_EPS: f32 = 1e-3;

/// Distance from a waypoint's centre at which a walk edge counts as
/// complete this tick. One tick of walking; keeps the polyline turn
/// crisp without a visible snap.
const WALK_ARRIVE_EPS: f32 = 1e-3;

/// Server-side kinematic mover state. Sits next to [`Brain`] on every
/// NPC; not replicated (clients only see the resulting `AvatarPose`).
/// The path cursor itself lives on [`Goal::MoveTo`] (`edge`) so a fresh
/// goal or an in-place repath resets it for free — this component only
/// holds what outlives any single goal: the motion mode (falling
/// happens to idle NPCs too) and the external-displacement watchdog.
#[derive(Component)]
pub(crate) struct NavMover {
    mode: MoverMode,
    /// Pose the mover last wrote. A mismatch at tick start means an
    /// outside system moved the body.
    last_written_pose: Vec3,
}

impl Default for NavMover {
    fn default() -> Self {
        Self {
            mode: MoverMode::Grounded,
            // NAN never equals the actual pose, so the first tick
            // always re-anchors (fresh spawn, freshly loaded save).
            last_written_pose: Vec3::NAN,
        }
    }
}

enum MoverMode {
    /// Standing or walking on support. The support watch and MoveTo
    /// edge execution both run in this mode.
    Grounded,
    /// Mid step-arc between two cells of a MoveTo path. `from_pose` is
    /// the eye pose at arc entry (wherever the walk actually was, so
    /// the arc never starts with a snap); the landing is exactly
    /// `standing_pose_translation(to)`.
    Step { from_pose: Vec3, to: IVec3, t: f32 },
    /// Support vanished (dug-out floor, walked off a severed ledge
    /// after an edit, external displacement into mid-air). Gravity
    /// integration downward until a landable cell, then re-anchor.
    Fall { vy: f32 },
}

/// Evaluate the step-arc curve at `t ∈ [0, 1]`: linear interpolation of
/// the endpoints plus a parabolic lift whose apex clears the higher
/// endpoint by [`STEP_ARC_APEX_CLEARANCE`]. Exact at both endpoints —
/// the mover's determinism depends on landing precisely. Pure function
/// so the arc shape is unit-testable; squash/stretch for the cartoony
/// look hooks in here later.
fn step_arc_pose(from: Vec3, to: Vec3, t: f32) -> Vec3 {
    let t = t.clamp(0.0, 1.0);
    let base = from.lerp(to, t);
    // Lift such that the curve's value at t=0.5 sits
    // STEP_ARC_APEX_CLEARANCE above the higher endpoint.
    let lift = from.y.max(to.y) - (from.y + to.y) * 0.5 + STEP_ARC_APEX_CLEARANCE;
    base + Vec3::Y * (lift * 4.0 * t * (1.0 - t))
}

/// One tick of kinematic fall. Integrates gravity, scans the foot cells
/// crossed this tick for the highest landable one (body-passable foot
/// over solid support), and either lands exactly on it (returning the
/// landing cell) or keeps falling. Pure function over the oracle so the
/// landing rule is unit-testable.
fn fall_step<W: Walkability>(
    world: &W,
    translation: Vec3,
    vy: f32,
    dt: f32,
) -> (Vec3, f32, Option<IVec3>) {
    let vy = vy + GRAVITY * dt;
    let drop = vy * dt;
    let feet0 = translation.y - EYE_OFFSET_FROM_CENTRE - PLAYER_HALF_EXTENTS.y;
    let feet1 = feet0 - drop;
    let x = translation.x.floor() as i32;
    let z = translation.z.floor() as i32;
    // Highest foot cell whose floor the feet cross this tick and whose
    // support is solid. `is_solid` reports unloaded chunks solid, so a
    // fall parks on the loaded/unloaded boundary instead of dropping
    // out of the world — same convention as item settling.
    let cy_hi = (feet0 + 1e-4).floor() as i32;
    let cy_lo = feet1.floor() as i32;
    for cy in (cy_lo..=cy_hi).rev() {
        if feet1 > cy as f32 {
            // Feet don't reach this cell's floor this tick.
            continue;
        }
        let foot = IVec3::new(x, cy, z);
        if world.is_solid(foot - IVec3::Y) && !world.blocks_body(foot) {
            return (standing_pose_translation(foot), 0.0, Some(foot));
        }
    }
    (translation - Vec3::Y * drop, vy, None)
}

/// True if any foot cell the body's XZ extent straddles rests on solid
/// support. The straddle matters: an edge-balanced NPC whose pose
/// centre hangs over air is still held up by the neighbouring cell,
/// and must not be dropped into a fall.
fn pose_has_support<W: Walkability>(world: &W, pose: &AvatarPose) -> bool {
    let foot_y = pose_to_foot_cell(pose).y;
    let e = PLAYER_HALF_EXTENTS.x - 1e-4;
    let mut checked = [None; 4];
    for (i, (dx, dz)) in [(-e, -e), (-e, e), (e, -e), (e, e)].into_iter().enumerate() {
        let cell = IVec3::new(
            (pose.translation.x + dx).floor() as i32,
            foot_y,
            (pose.translation.z + dz).floor() as i32,
        );
        if checked[..i].contains(&Some(cell)) {
            continue;
        }
        checked[i] = Some(cell);
        if world.is_solid(cell - IVec3::Y) {
            return true;
        }
    }
    false
}

/// Legality of the edge `from -> to`, mirroring exactly what
/// `step_neighbours` promised at plan time. Re-checked every tick the
/// edge is being traversed — cheap (a few chunk-slot lookups) and it is
/// the *only* failure detector the mover has.
fn edge_traversable<W: Walkability>(world: &W, from: IVec3, to: IVec3) -> bool {
    match to.y - from.y {
        0 => standable(world, to),
        // Climb: head clearance two above the current foot.
        1 => standable(world, to) && !world.blocks_body(from + IVec3::new(0, 2, 0)),
        // Drop: the pass-through cell above the destination foot.
        -1 => standable(world, to) && !world.blocks_body(to + IVec3::Y),
        _ => false,
    }
}

/// The kinematic mover tick. Chained directly after `npc_brain_tick`
/// in `FixedUpdate`: the brain commits/advances goals, the mover moves
/// bodies, the brain reads the results (`edge` cursor at the last
/// waypoint = arrival, `blocked` = replan) next tick.
///
/// `KinematicLock` opts a body out entirely (use-slot snaps own the
/// pose), same contract the physics step had.
type MovingNpcData<'a> = (
    &'a mut AvatarPose,
    &'a mut AvatarVelocity,
    &'a mut AvatarOnGround,
    &'a mut Brain,
    &'a mut NavMover,
);
type MovingNpcFilter = (With<Npc>, Without<KinematicLock>);

pub(crate) fn npc_mover_step(
    time: Res<Time>,
    chunks: Query<&'static Chunk>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut npcs: Query<MovingNpcData, MovingNpcFilter>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let world = WorldWalk {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (mut pose, mut vel, mut on_ground, mut brain, mut mover) in npcs.iter_mut() {
        let tick_start = pose.translation;

        // Someone else moved the body since we last wrote it
        // (placement push-out, load rescue, snap eject, teleport).
        // Re-anchor: drop any in-flight arc and let the grounded
        // branch below decide between fall and replan.
        if (pose.translation - mover.last_written_pose)
            .abs()
            .max_element()
            > EXTERNAL_DISPLACEMENT_EPS
            || !mover.last_written_pose.is_finite()
        {
            mover.mode = MoverMode::Grounded;
            if let Goal::MoveTo { blocked, .. } = &mut brain.goal {
                *blocked = true;
            }
        }

        match &mut mover.mode {
            MoverMode::Fall { vy } => {
                let (new_pose, new_vy, landed) = fall_step(&world, pose.translation, *vy, dt);
                pose.translation = new_pose;
                *vy = new_vy;
                if landed.is_some() {
                    mover.mode = MoverMode::Grounded;
                    // Wherever we landed is not necessarily on the
                    // path; force a replan rather than resuming a
                    // now-fictional edge.
                    if let Goal::MoveTo { blocked, .. } = &mut brain.goal {
                        *blocked = true;
                    }
                }
            }
            MoverMode::Step { from_pose, to, t } => {
                *t += dt / STEP_ARC_SECS;
                let (from_pose, to, t_now) = (*from_pose, *to, *t);
                let target = standing_pose_translation(to);
                let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
                if let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, waypoint_xz(to), dt) {
                    pose.yaw = (pose.yaw + dyaw).rem_euclid(core::f32::consts::TAU);
                }
                if t_now >= 1.0 {
                    pose.translation = target;
                    mover.mode = MoverMode::Grounded;
                    // Advance the cursor only if this arc still belongs
                    // to the live path (a preempt may have swapped the
                    // goal mid-hop).
                    if let Goal::MoveTo { edge, path, .. } = &mut brain.goal
                        && path.get(*edge + 1).copied() == Some(to)
                    {
                        *edge += 1;
                    }
                } else {
                    pose.translation = step_arc_pose(from_pose, target, t_now);
                }
            }
            MoverMode::Grounded => {
                // Support watch — every NPC, walking or idle. A pit dug
                // under a standing NPC drops them; CellEdit needs no
                // special case here because the check is per-tick.
                if !pose_has_support(&world, &pose) {
                    mover.mode = MoverMode::Fall { vy: 0.0 };
                } else if let Goal::MoveTo {
                    path,
                    edge,
                    blocked: blocked @ false,
                    ..
                } = &mut brain.goal
                    && *edge + 1 < path.len()
                {
                    let from = path[*edge];
                    let to = path[*edge + 1];
                    if !edge_traversable(&world, from, to) {
                        *blocked = true;
                    } else if to.y != from.y {
                        mover.mode = MoverMode::Step {
                            from_pose: pose.translation,
                            to,
                            t: 0.0,
                        };
                    } else {
                        // Walk the polyline segment at constant speed,
                        // foot Y locked to the cell level.
                        let target_xz = waypoint_xz(to);
                        let pose_xz = Vec2::new(pose.translation.x, pose.translation.z);
                        if let Some(dyaw) = aim_yaw_step(pose_xz, pose.yaw, target_xz, dt) {
                            pose.yaw = (pose.yaw + dyaw).rem_euclid(core::f32::consts::TAU);
                        }
                        let remaining = target_xz - pose_xz;
                        let step = WALK_SPEED * dt;
                        let level_y = standing_pose_translation(from).y;
                        if remaining.length() <= step + WALK_ARRIVE_EPS {
                            *edge += 1;
                            if *edge + 1 >= path.len() {
                                // Final waypoint: land exactly. The
                                // brain fires arrival off the cursor.
                                pose.translation = standing_pose_translation(to);
                            } else {
                                pose.translation = Vec3::new(target_xz.x, level_y, target_xz.y);
                            }
                        } else {
                            let advance = remaining.normalize() * step;
                            pose.translation =
                                Vec3::new(pose_xz.x + advance.x, level_y, pose_xz.y + advance.y);
                        }
                    }
                }
            }
        }

        // The client anim driver keys walk/idle off replicated XZ
        // speed; synthesize it from the pose delta so NPCs don't glide
        // in the idle pose.
        vel.0 = (pose.translation - tick_start) / dt;
        on_ground.0 = matches!(mover.mode, MoverMode::Grounded);
        mover.last_written_pose = pose.translation;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct TestGrid {
        solid: HashSet<IVec3>,
    }

    impl TestGrid {
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
    fn arc_endpoints_are_exact() {
        let from = standing_pose_translation(IVec3::new(0, 1, 0));
        let to = standing_pose_translation(IVec3::new(1, 2, 0));
        assert_eq!(step_arc_pose(from, to, 0.0), from);
        assert_eq!(step_arc_pose(from, to, 1.0), to);
    }

    #[test]
    fn arc_apex_clears_higher_endpoint() {
        let from = standing_pose_translation(IVec3::new(0, 1, 0));
        let to = standing_pose_translation(IVec3::new(1, 2, 0));
        let apex = step_arc_pose(from, to, 0.5);
        let higher = from.y.max(to.y);
        assert!(
            (apex.y - (higher + STEP_ARC_APEX_CLEARANCE)).abs() < 1e-5,
            "apex {} vs expected {}",
            apex.y,
            higher + STEP_ARC_APEX_CLEARANCE
        );
        // Same guarantee stepping DOWN.
        let apex_down = step_arc_pose(to, from, 0.5);
        assert!((apex_down.y - (higher + STEP_ARC_APEX_CLEARANCE)).abs() < 1e-5);
    }

    #[test]
    fn arc_horizontal_is_monotone() {
        let from = standing_pose_translation(IVec3::new(0, 1, 0));
        let to = standing_pose_translation(IVec3::new(1, 2, 0));
        let mut prev_x = f32::NEG_INFINITY;
        for i in 0..=20 {
            let p = step_arc_pose(from, to, i as f32 / 20.0);
            assert!(p.x >= prev_x, "horizontal reversed at sample {i}");
            prev_x = p.x;
        }
    }

    #[test]
    fn fall_lands_exactly_on_standing_pose() {
        let world = TestGrid::floored();
        // Drop from well above the floor; iterate ticks until landing.
        let mut translation = standing_pose_translation(IVec3::new(0, 5, 0));
        let mut vy = 0.0;
        let mut landed = None;
        for _ in 0..600 {
            let (p, v, l) = fall_step(&world, translation, vy, 1.0 / 60.0);
            translation = p;
            vy = v;
            if l.is_some() {
                landed = l;
                break;
            }
        }
        assert_eq!(landed, Some(IVec3::new(0, 1, 0)));
        assert_eq!(translation, standing_pose_translation(IVec3::new(0, 1, 0)));
    }

    #[test]
    fn fall_passes_through_body_passable_cells_only() {
        // A "roof" slab: solid at y=3 under the fall line stops it.
        let mut world = TestGrid::floored();
        world.solid.insert(IVec3::new(0, 3, 0));
        let mut translation = standing_pose_translation(IVec3::new(0, 8, 0));
        let mut vy = 0.0;
        let mut landed = None;
        for _ in 0..600 {
            let (p, v, l) = fall_step(&world, translation, vy, 1.0 / 60.0);
            translation = p;
            vy = v;
            if l.is_some() {
                landed = l;
                break;
            }
        }
        assert_eq!(landed, Some(IVec3::new(0, 4, 0)), "lands on the slab");
    }

    #[test]
    fn support_straddle_keeps_edge_balanced_npc_up() {
        // Floor only under x=0; an NPC centred just past the boundary
        // at x=1.05 still straddles the x=0 cell with its 0.3
        // half-extent and keeps support.
        let mut world = TestGrid {
            solid: HashSet::new(),
        };
        world.solid.insert(IVec3::new(0, 0, 0));
        let mut pose = AvatarPose {
            translation: standing_pose_translation(IVec3::new(1, 1, 0)),
            ..AvatarPose::default()
        };
        pose.translation.x = 1.05;
        assert!(pose_has_support(&world, &pose));
        // Centred fully inside x=1 (no straddle) — no support.
        pose.translation.x = 1.5;
        assert!(!pose_has_support(&world, &pose));
    }

    #[test]
    fn edge_traversable_mirrors_planner_rules() {
        let mut world = TestGrid::floored();
        let a = IVec3::new(0, 1, 0);
        // Flat neighbour: fine.
        assert!(edge_traversable(&world, a, IVec3::new(1, 1, 0)));
        // Step up onto a ledge.
        world.solid.insert(IVec3::new(2, 1, 0));
        assert!(edge_traversable(
            &world,
            IVec3::new(1, 1, 0),
            IVec3::new(2, 2, 0)
        ));
        // Climb blocked by a canopy over the climber's head.
        world.solid.insert(IVec3::new(1, 3, 0));
        assert!(!edge_traversable(
            &world,
            IVec3::new(1, 1, 0),
            IVec3::new(2, 2, 0)
        ));
        // Anything beyond ±1 Y is never a legal edge.
        assert!(!edge_traversable(&world, a, IVec3::new(0, 3, 0)));
    }
}
