//! Player physics: Quake-style controller against a voxel grid. Hand-rolled
//! (Minecraft / Vintage Story / Veloren genre standard) — collision math
//! lives in `crate::collision`.
//!
//! The controller body is `apply_walk_step`, a pure function called from
//! both client (prediction) and server (authority) systems in
//! `FixedUpdate`. Identical inputs → identical outputs ⇒ no rollback.
//! Lightyear's prediction pipeline replays inputs through this same
//! function when the server sends a correction.

use bevy::prelude::*;

use crate::blocks::BlockRegistry;
use crate::collision::{Aabb, WorldCollision, sweep_axis};
use crate::protocol::{Actor, AvatarOnGround, AvatarPose, AvatarVelocity, MovementMode, MovementIntent};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

/// Player AABB half-extents. 0.6 × 1.8 × 0.6 matches the avatar mesh and
/// is close to Minecraft's player hitbox.
pub const PLAYER_HALF_EXTENTS: Vec3 = Vec3::new(0.3, 0.9, 0.3);

/// Camera position above the AABB centre — where the player's eyes are.
/// 1.62 m above the feet, matching the avatar's proportions and giving a
/// natural FPV head height. `AvatarPose.translation` stores the eye
/// position; the AABB centre is derived as `eye - (0, EYE_OFFSET, 0)`.
pub const EYE_OFFSET_FROM_CENTRE: f32 = 0.72;

/// XZ half-extent for the actor-vs-actor soft-separation pass. Actors
/// closer than `2 * ACTOR_SEPARATION_HALF_XZ` on XZ are considered
/// overlapping for separation purposes. Half of the body's XZ extent so
/// shoulder-brushing in a doorway doesn't fire the push — overlap only
/// counts once their *cores* meet, not their outer cuboids.
pub const ACTOR_SEPARATION_HALF_XZ: f32 = 0.15;

/// What fraction of an actor-vs-actor overlap each actor absorbs per
/// tick. 0.5 splits the displacement evenly: if A walks into a
/// stationary B, A advances less than its walk wanted, B drifts forward
/// in the same direction, and the equilibrium is "both moving at half
/// A's walk speed, A trailing B." That's the "gently push" feel.
pub const ACTOR_SEPARATION_PUSH_FRACTION: f32 = 0.5;

/// Tiny gap restored after the separation push so the next tick's
/// overlap test doesn't immediately re-fire on a floating-point
/// touching configuration.
pub const ACTOR_SEPARATION_PUSH_EPS: f32 = 1e-3;

/// Per-tick actor-vs-actor soft separation. Runs after the physics
/// steps; for every pair of actors whose XZ centres are closer than
/// `2 * ACTOR_SEPARATION_HALF_XZ`, applies a swept push to each in
/// opposite directions so they spread apart. The push is bidirectional
/// and equal-share, so the "pusher" still makes forward progress —
/// just at roughly half their walk speed while in contact.
///
/// **Why not in the sweep itself.** Doing this inside `apply_walk_step`
/// would either hard-stop the pusher (current state before this system
/// existed) or require mutating the pushee's pose from inside another
/// actor's sweep — both worse than a post-physics pairwise pass.
///
/// **Why swept.** A direct `pose.translation += push` can shove an
/// actor into a wall. Running each per-axis push component through
/// `sweep_axis` against the block grid lets the wall stop the push,
/// leaving the cornered actor where it was and the pusher with their
/// half of the displacement consumed against the wall — they don't
/// magically tunnel.
pub fn soft_separate_actors(
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut actors: Query<(Entity, &mut AvatarPose), With<Actor>>,
) {
    let snapshot: Vec<(Entity, Vec3)> = actors
        .iter()
        .map(|(e, pose)| (e, pose.translation))
        .collect();
    let n = snapshot.len();
    if n < 2 {
        return;
    }

    let min_xz = 2.0 * ACTOR_SEPARATION_HALF_XZ;
    let min_xz_sq = min_xz * min_xz;
    let min_dy = 2.0 * PLAYER_HALF_EXTENTS.y;

    // Accumulate XZ push per entity. Index aligned with `snapshot`.
    let mut pushes: Vec<Vec2> = vec![Vec2::ZERO; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let dy = snapshot[i].1.y - snapshot[j].1.y;
            if dy.abs() >= min_dy {
                // Vertically far apart (one on a roof, one on ground)
                // — different "floor," no pushing.
                continue;
            }
            let dx = snapshot[i].1.x - snapshot[j].1.x;
            let dz = snapshot[i].1.z - snapshot[j].1.z;
            let d_sq = dx * dx + dz * dz;
            if d_sq >= min_xz_sq {
                continue;
            }
            let dist = d_sq.sqrt();
            // Coincident centres: any direction works; pick +X so two
            // load-time-stacked NPCs deterministically end up east/west.
            let (nx, nz) = if dist > 1e-4 {
                (dx / dist, dz / dist)
            } else {
                (1.0, 0.0)
            };
            let overlap = min_xz - dist + ACTOR_SEPARATION_PUSH_EPS;
            let share = overlap * ACTOR_SEPARATION_PUSH_FRACTION;
            pushes[i].x += nx * share;
            pushes[i].y += nz * share;
            pushes[j].x -= nx * share;
            pushes[j].y -= nz * share;
        }
    }

    // Apply pushes through the sweep so walls clip them.
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };
    for (i, (entity, _)) in snapshot.iter().enumerate() {
        let push = pushes[i];
        if push.x == 0.0 && push.y == 0.0 {
            continue;
        }
        let Ok((_, mut pose)) = actors.get_mut(*entity) else {
            continue;
        };
        let centre = pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
        let mut aabb = Aabb::from_centre_half(centre, PLAYER_HALF_EXTENTS);
        let mut total = Vec3::ZERO;
        for (axis, want) in [(0, push.x), (2, push.y)] {
            let mut step = Vec3::ZERO;
            step[axis] = want;
            let resolved = sweep_axis(&aabb, step, axis, &world);
            aabb = aabb.translated(resolved);
            total += resolved;
        }
        pose.translation += total;
    }
}

/// Walking speed on the ground (m/s). Between Minecraft (~4.3) and Quake (~9).
pub const WALK_SPEED: f32 = 5.0;

/// Flight speed in fly mode (m/s). Faster than walking — creative mode
/// is for getting around quickly. Tunable.
pub const FLY_SPEED: f32 = 12.0;

/// Initial vertical velocity on jump (m/s). With g=25, this gives ~1.4 m
/// max height — clears a single block comfortably.
pub const JUMP_SPEED: f32 = 8.5;

/// Game-feel gravity (m/s²). Real-world is 9.8; voxel games typically
/// amplify so the player doesn't float.
pub const GRAVITY: f32 = 25.0;

/// Acceleration toward wishdir on the ground (m/s²-ish — Quake's units).
/// Combined with WALK_SPEED this means you reach top speed in ~1/10 s.
pub const GROUND_ACCEL: f32 = 80.0;

/// Acceleration in the air. Deliberately low — the player can nudge their
/// trajectory mid-jump but can't fully redirect like a ground runner.
pub const AIR_ACCEL: f32 = 12.0;

/// Acceleration in fly mode — high so movement feels snappy in 6-dof.
pub const FLY_ACCEL: f32 = 60.0;

/// Ground friction (Quake formula, dimensionless). Higher = quicker stops.
/// 6 matches the Quake/Source default and feels familiar.
pub const GROUND_FRICTION: f32 = 6.0;

/// Friction in fly mode. Stops you cleanly when you let go of WASD.
pub const FLY_FRICTION: f32 = 8.0;

/// One step of the player simulation: read input + current state, run
/// physics + collision, write state. Pure function — no Bevy systems,
/// resources, or queries here. Both server-authority and client-prediction
/// call this per `FixedUpdate` tick.
///
/// On entry: `pose` carries the current eye position + body yaw. On exit:
/// pose, velocity, on_ground, and mode all reflect this tick's outcome.
/// `input.toggle_mode == true` flips the mode this tick (server-authority
/// gates this later via permissions).
pub fn apply_walk_step(
    pose: &mut AvatarPose,
    velocity: &mut AvatarVelocity,
    on_ground: &mut AvatarOnGround,
    mode: &mut MovementMode,
    input: &MovementIntent,
    dt: f32,
    world: &WorldCollision,
) {
    if dt <= 0.0 {
        return;
    }

    // Mode flip happens at the start of the tick — the rest of the step
    // then uses the new mode.
    if input.toggle_mode {
        *mode = match *mode {
            MovementMode::Walk => MovementMode::Fly,
            MovementMode::Fly => MovementMode::Walk,
        };
    }

    // Yaw is owned by the pose; input contributes a delta (mouse motion
    // since the previous tick). Keeping pose.yaw authoritative lets the
    // saved spawn yaw survive — a default input means "no rotation" rather
    // than "snap to 0." Wrap to keep the running value bounded after a
    // long session of looking around.
    pose.yaw = (pose.yaw + input.dyaw).rem_euclid(core::f32::consts::TAU);

    // Wishdir basis from current yaw. In FPV terms: forward = -Z when
    // yaw = 0, right = +X. WASD packs into wishdir as i8s on the wire.
    let (sin_y, cos_y) = pose.yaw.sin_cos();
    let forward = Vec3::new(-sin_y, 0.0, -cos_y);
    let right = Vec3::new(cos_y, 0.0, -sin_y);
    let wishdir_local = Vec3::new(
        input.wishdir[0] as f32,
        input.wishdir[1] as f32,
        input.wishdir[2] as f32,
    );
    let wish_horizontal = right * wishdir_local.x + forward * (-wishdir_local.z);

    match *mode {
        MovementMode::Walk => walk_step(pose, velocity, on_ground, wish_horizontal, input, dt, world),
        MovementMode::Fly => fly_step(pose, velocity, wish_horizontal, wishdir_local.y, dt),
    }

    // Walk mode resolves on_ground via the sweep below; fly mode is
    // never grounded.
    if matches!(*mode, MovementMode::Fly) {
        on_ground.0 = false;
    }
}

fn walk_step(
    pose: &mut AvatarPose,
    velocity: &mut AvatarVelocity,
    on_ground: &mut AvatarOnGround,
    wish_horizontal: Vec3,
    input: &MovementIntent,
    dt: f32,
    world: &WorldCollision,
) {
    let v = &mut velocity.0;
    let wishspeed = wish_horizontal.length();
    let wishdir = if wishspeed > f32::EPSILON {
        wish_horizontal / wishspeed
    } else {
        Vec3::ZERO
    };
    let target_speed = if wishspeed > 0.0 { WALK_SPEED } else { 0.0 };

    // Quake friction: linear decay scaled by current speed. Snaps you to
    // a stop when wishdir is zero; barely affects movement at full speed
    // because the accelerate step counters it.
    if on_ground.0 {
        let speed_xz = (v.x.powi(2) + v.z.powi(2)).sqrt();
        if speed_xz > 0.0 {
            let drop = speed_xz * GROUND_FRICTION * dt;
            let scale = (speed_xz - drop).max(0.0) / speed_xz;
            v.x *= scale;
            v.z *= scale;
        }
    }

    // Quake accelerate: only add velocity along wishdir up to the amount
    // that brings projected speed to target_speed. Air-strafing therefore
    // can't compound infinitely.
    let accel = if on_ground.0 { GROUND_ACCEL } else { AIR_ACCEL };
    let projected = v.x * wishdir.x + v.z * wishdir.z;
    let add = (target_speed - projected).max(0.0);
    if add > 0.0 {
        let accel_amount = (accel * dt).min(add);
        v.x += wishdir.x * accel_amount;
        v.z += wishdir.z * accel_amount;
    }

    // Jump on rising-edge of `jump` while grounded. The buffer encodes
    // `jump` as held-this-tick; the controller doesn't see "just-pressed"
    // edges, so a jumping-while-grounded condition fires every tick the
    // player holds space until they leave the ground — that's fine and
    // matches Minecraft's bunny-hop behaviour.
    if on_ground.0 && input.jump {
        v.y = JUMP_SPEED;
        on_ground.0 = false;
    }

    v.y -= GRAVITY * dt;

    // Sweep — Y first so we settle on the floor cleanly before XZ resolves.
    let centre = pose.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
    let mut aabb = Aabb::from_centre_half(centre, PLAYER_HALF_EXTENTS);
    let delta = *v * dt;

    let mut grounded = false;
    for axis in [1, 0, 2] {
        let mut step = Vec3::ZERO;
        step[axis] = delta[axis];
        let resolved = sweep_axis(&aabb, step, axis, world);
        aabb = aabb.translated(resolved);
        if (resolved[axis] - step[axis]).abs() > f32::EPSILON {
            v[axis] = 0.0;
            if axis == 1 && step.y < 0.0 {
                grounded = true;
            }
        }
    }
    on_ground.0 = grounded;
    pose.translation = aabb.centre() + Vec3::Y * EYE_OFFSET_FROM_CENTRE;
}

fn fly_step(
    pose: &mut AvatarPose,
    velocity: &mut AvatarVelocity,
    wish_horizontal: Vec3,
    wish_up: f32,
    dt: f32,
) {
    let v = &mut velocity.0;
    let wish = wish_horizontal + Vec3::Y * wish_up;
    let wishspeed = wish.length();
    let wishdir = if wishspeed > f32::EPSILON {
        wish / wishspeed
    } else {
        Vec3::ZERO
    };
    let target_speed = if wishspeed > 0.0 { FLY_SPEED } else { 0.0 };

    // Friction in all three axes — fly mode has no inertia carryover
    // when you let go of WASD/space.
    let speed = v.length();
    if speed > 0.0 {
        let drop = speed * FLY_FRICTION * dt;
        let scale = (speed - drop).max(0.0) / speed;
        *v *= scale;
    }

    let projected = v.dot(wishdir);
    let add = (target_speed - projected).max(0.0);
    if add > 0.0 {
        let accel_amount = (FLY_ACCEL * dt).min(add);
        *v += wishdir * accel_amount;
    }

    // Fly mode bypasses collision entirely — moves through everything.
    pose.translation += *v * dt;
}
