//! Player physics: walking controller. Hand-rolled Quake-style controller
//! against a voxel grid (Minecraft / Vintage Story / Veloren genre
//! standard) — collision math lives in `crate::collision`. This module
//! is the *client-side* wiring: F1 toggles between fly and walk, walk
//! reads the camera's FlyCam yaw and applies physics in `FixedUpdate`.
//!
//! Phase 2 will lift the controller body into a function reused by the
//! server (driven by replicated `PlayerInput`s) so the client predicts
//! and the server validates from the same source of truth.

use bevy::prelude::*;

use crate::blocks::BlockRegistry;
use crate::camera::FlyCam;
use crate::collision::{Aabb, WorldCollision, sweep_axis};
use crate::protocol::GameSet;
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

/// Player AABB half-extents. 0.6 × 1.8 × 0.6 matches the avatar mesh and
/// is close to Minecraft's player hitbox.
pub const PLAYER_HALF_EXTENTS: Vec3 = Vec3::new(0.3, 0.9, 0.3);

/// Camera position above the AABB centre — where the player's eyes are.
/// 1.62 m above the feet, matching the avatar's proportions and giving a
/// natural FPV head height. Camera Transform stores the eye position; the
/// AABB centre is derived as `eye - (0, EYE_OFFSET, 0)` where
/// `EYE_OFFSET = eye_height - half_height = 1.62 - 0.9 = 0.72`.
pub const EYE_OFFSET_FROM_CENTRE: f32 = 0.72;

/// Walking speed on the ground (m/s). Between Minecraft (~4.3) and Quake (~9).
pub const WALK_SPEED: f32 = 5.0;

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

/// Ground friction (Quake formula, dimensionless). Higher = quicker stops.
/// 6 matches the Quake/Source default and feels familiar.
pub const GROUND_FRICTION: f32 = 6.0;

#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MovementMode {
    /// Fly cam: free 6-dof movement, no gravity, no collision. Default
    /// because building rooms from inside is awkward without it.
    #[default]
    Fly,
    /// Walking player: AABB collision against world, gravity, jump.
    Walk,
}

#[derive(Component, Default)]
pub struct Player {
    pub velocity: Vec3,
    pub on_ground: bool,
}

pub struct PhysicsPlugin;

impl Plugin for PhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MovementMode>()
            .add_systems(Update, toggle_movement_mode.in_set(GameSet::Input))
            // FixedUpdate at Bevy's default 64 Hz keeps jump arcs and
            // friction hardware-independent and matches the cadence
            // lightyear's prediction will need in Phase 2.
            .add_systems(FixedUpdate, player_walk);
    }
}

fn toggle_movement_mode(keys: Res<ButtonInput<KeyCode>>, mut mode: ResMut<MovementMode>) {
    if !keys.just_pressed(KeyCode::F1) {
        return;
    }
    *mode = match *mode {
        MovementMode::Fly => MovementMode::Walk,
        MovementMode::Walk => MovementMode::Fly,
    };
    info!("movement mode → {:?}", *mode);
}

/// Walking controller. Runs every fixed tick when in Walk mode. Reads
/// WASD + Space, computes a wishdir from the camera's yaw, applies gravity
/// + friction + jump, then sweeps the player's AABB through the world.
#[allow(clippy::too_many_arguments, reason = "ECS system signature")]
fn player_walk(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<MovementMode>,
    chunks: Query<(&'static Chunk, &'static ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut players: Query<(&mut Transform, &mut Player, &FlyCam)>,
) {
    if *mode != MovementMode::Walk {
        return;
    }
    let Ok((mut transform, mut player, fly)) = players.single_mut() else {
        return;
    };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    // Wishdir in world space: WASD relative to the camera's yaw (pitch
    // ignored for movement — looking up shouldn't make you fly).
    let (sin_y, cos_y) = fly.yaw.sin_cos();
    let forward = Vec3::new(-sin_y, 0.0, -cos_y);
    let right = Vec3::new(cos_y, 0.0, -sin_y);
    let mut wish = Vec3::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        wish += forward;
    }
    if keys.pressed(KeyCode::KeyS) {
        wish -= forward;
    }
    if keys.pressed(KeyCode::KeyD) {
        wish += right;
    }
    if keys.pressed(KeyCode::KeyA) {
        wish -= right;
    }
    let wishspeed = wish.length();
    let wishdir = if wishspeed > f32::EPSILON {
        wish / wishspeed
    } else {
        Vec3::ZERO
    };
    let target_speed = if wishspeed > 0.0 { WALK_SPEED } else { 0.0 };

    // Quake-style ground friction: linear decay scaled by current speed.
    // Stops the player when wishdir is zero; barely affects them at full
    // wish-speed because the accelerate step counters it.
    if player.on_ground {
        let speed_xz = (player.velocity.x.powi(2) + player.velocity.z.powi(2)).sqrt();
        if speed_xz > 0.0 {
            let drop = speed_xz * GROUND_FRICTION * dt;
            let scale = (speed_xz - drop).max(0.0) / speed_xz;
            player.velocity.x *= scale;
            player.velocity.z *= scale;
        }
    }

    // Quake-style accelerate: only add velocity along wishdir up to the
    // amount that brings projected speed to target_speed. This is what
    // gives Quake-derived controllers their crisp feel — you accelerate
    // quickly until you're at target speed, then movement stops adding
    // (so air-strafing doesn't compound infinitely).
    let accel = if player.on_ground { GROUND_ACCEL } else { AIR_ACCEL };
    let projected = player.velocity.x * wishdir.x + player.velocity.z * wishdir.z;
    let add = (target_speed - projected).max(0.0);
    if add > 0.0 {
        let accel_amount = (accel * dt).min(add);
        player.velocity.x += wishdir.x * accel_amount;
        player.velocity.z += wishdir.z * accel_amount;
    }

    // Jump — only when grounded. Resets on_ground so subsequent ticks
    // apply gravity until the next floor contact.
    if player.on_ground && keys.pressed(KeyCode::Space) {
        player.velocity.y = JUMP_SPEED;
        player.on_ground = false;
    }

    // Gravity always applies; floor sweep below clears it on contact.
    player.velocity.y -= GRAVITY * dt;

    // Sweep — Y first so we settle on the floor cleanly before XZ resolves.
    let centre = transform.translation - Vec3::Y * EYE_OFFSET_FROM_CENTRE;
    let mut aabb = Aabb::from_centre_half(centre, PLAYER_HALF_EXTENTS);
    let delta = player.velocity * dt;
    let world = WorldCollision {
        chunks: &chunks,
        chunk_map: &chunk_map,
        registry: &registry,
    };

    let mut grounded = false;
    for axis in [1, 0, 2] {
        let mut step = Vec3::ZERO;
        step[axis] = delta[axis];
        let resolved = sweep_axis(&aabb, step, axis, &world);
        aabb = aabb.translated(resolved);
        if (resolved[axis] - step[axis]).abs() > f32::EPSILON {
            // Hit: zero velocity on that axis. If it was a downward Y hit
            // we're newly on the ground.
            player.velocity[axis] = 0.0;
            if axis == 1 && step.y < 0.0 {
                grounded = true;
            }
        }
    }
    player.on_ground = grounded;
    transform.translation = aabb.centre() + Vec3::Y * EYE_OFFSET_FROM_CENTRE;
}
