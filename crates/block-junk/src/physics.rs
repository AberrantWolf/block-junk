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

use crate::collision::{Aabb, WorldCollision, sweep_axis};
use crate::protocol::{AvatarOnGround, AvatarPose, AvatarVelocity, MovementMode, MovementIntent};

/// Player AABB half-extents. 0.6 × 1.8 × 0.6 matches the avatar mesh and
/// is close to Minecraft's player hitbox.
pub const PLAYER_HALF_EXTENTS: Vec3 = Vec3::new(0.3, 0.9, 0.3);

/// Camera position above the AABB centre — where the player's eyes are.
/// 1.62 m above the feet, matching the avatar's proportions and giving a
/// natural FPV head height. `AvatarPose.translation` stores the eye
/// position; the AABB centre is derived as `eye - (0, EYE_OFFSET, 0)`.
pub const EYE_OFFSET_FROM_CENTRE: f32 = 0.72;

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
