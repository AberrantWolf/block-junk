//! Player physics: AABB-on-voxel-grid collision and a Quake-style walking
//! controller. Hand-rolled rather than reaching for Avian — for player vs
//! cube grid, grid-sweep is the genre standard (Minecraft / Vintage Story
//! / Veloren) and avoids per-chunk collider rebuilds on every block edit.
//! Avian stays in deps for later use (NPCs, ragdolls, projectiles).
//!
//! The collision sweep gathers candidate AABBs from three potential sources
//! — cube blocks (today), block-entity AABBs (today, via `ChunkEntities`),
//! and dynamic entities (later, via a query). The sweep itself is agnostic
//! to the source.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use block_junk_mod_api::blocks::EntityAabb;

use crate::blocks::BlockRegistry;
use crate::camera::FlyCam;
use crate::client::ChunkMap;
use crate::protocol::GameSet;
use crate::voxel::{Chunk, ChunkEntities, EntryKind, world_to_chunk};

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

#[derive(Clone, Copy, Debug)]
struct Aabb {
    min: Vec3,
    max: Vec3,
}

impl Aabb {
    fn from_centre_half(centre: Vec3, half: Vec3) -> Self {
        Self {
            min: centre - half,
            max: centre + half,
        }
    }

    fn from_min_max(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    fn centre(self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    fn translated(self, delta: Vec3) -> Self {
        Self {
            min: self.min + delta,
            max: self.max + delta,
        }
    }

    /// Expand to also include `self.translated(delta)` — a swept volume.
    fn swept(self, delta: Vec3) -> Self {
        let other = self.translated(delta);
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

}

/// Per-axis sweep: how far can `mover` move along `step` (which has only
/// one nonzero component, `axis`) before colliding with anything in `world`?
/// Returns the actual delta vector (with the colliding axis clipped to
/// stop just shy of the obstacle).
fn sweep_axis(mover: &Aabb, step: Vec3, axis: usize, world: &WorldCollision) -> Vec3 {
    let move_along = step[axis];
    if move_along == 0.0 {
        return Vec3::ZERO;
    }
    let probe = mover.swept(step);
    let candidates = world.candidates(probe);
    if candidates.is_empty() {
        return step;
    }

    // Tiny epsilon prevents the player from coming to rest exactly on a
    // surface — they land 1 µm short, so subsequent floor checks pass
    // reliably without round-off making them re-sink.
    const EPS: f32 = 1e-4;

    let mut clipped = move_along;
    for cand in &candidates {
        // Skip boxes the mover doesn't even overlap on the *other* two
        // axes. Without this, a wall to the left would clip your forward
        // motion just because its X interval contains yours.
        let other = [(axis + 1) % 3, (axis + 2) % 3];
        let m_lo_a = mover.min[other[0]];
        let m_hi_a = mover.max[other[0]];
        let m_lo_b = mover.min[other[1]];
        let m_hi_b = mover.max[other[1]];
        if cand.max[other[0]] <= m_lo_a
            || cand.min[other[0]] >= m_hi_a
            || cand.max[other[1]] <= m_lo_b
            || cand.min[other[1]] >= m_hi_b
        {
            continue;
        }
        if move_along > 0.0 {
            // Mover advancing toward +axis: stop when its max hits cand.min.
            //   max_allowed > 0  → cand is ahead, allow up to that distance.
            //   max_allowed < 0 + mover overlaps cand on this axis
            //     (mover.min < cand.max) → already partially inside; clamp
            //     to zero so a block appearing around the player mid-stride
            //     can't be tunneled through.
            //   max_allowed < 0 + mover fully past cand (mover.min ≥
            //     cand.max) → cand is *behind*; irrelevant. Important when
            //     standing on top of a half-height block (bed): you're
            //     above it, jumping up shouldn't be blocked by the floor
            //     of the bed below.
            let max_allowed = cand.min[axis] - mover.max[axis] - EPS;
            if max_allowed >= 0.0 {
                if max_allowed < clipped {
                    clipped = max_allowed;
                }
            } else if mover.min[axis] < cand.max[axis] {
                clipped = clipped.min(0.0);
            }
        } else {
            // Mirror of the above for negative motion.
            let max_allowed = cand.max[axis] - mover.min[axis] + EPS;
            if max_allowed <= 0.0 {
                if max_allowed > clipped {
                    clipped = max_allowed;
                }
            } else if mover.max[axis] > cand.min[axis] {
                clipped = clipped.max(0.0);
            }
        }
    }

    let mut out = Vec3::ZERO;
    out[axis] = clipped;
    out
}

/// Bundles the queries the AABB sweep needs to enumerate candidate
/// collision boxes. Decoupled from the sweep itself so adding sources
/// (dynamic entities, NPCs, fluids) later is "another candidate
/// generator," not a sweep rewrite.
///
/// `'static` on the inner Query data params matches Bevy's actual Query
/// shape — elided lifetimes in system signatures look like `&Chunk` but
/// the Query is invariant and concretely `&'static Chunk`. Without the
/// explicit `'static`, the struct field's elided lifetime can't unify
/// with the system-arg Query.
struct WorldCollision<'w, 's, 'a> {
    chunks: &'a Query<'w, 's, (&'static Chunk, &'static ChunkEntities)>,
    chunk_map: &'a ChunkMap,
    registry: &'a BlockRegistry,
}

impl WorldCollision<'_, '_, '_> {
    /// All candidate AABBs that overlap `region`. Two source streams:
    ///   1. Solid cube blocks: one unit-cube AABB per cell with a solid slot.
    ///   2. Block-entity AABBs: one per *anchor* in the overlapping cells,
    ///      pulled from the chunk sidecar and rotated by the stored
    ///      `Cardinal`. Ghost cells of the same entity dedupe via the
    ///      anchor world cell.
    fn candidates(&self, region: Aabb) -> Vec<Aabb> {
        let mut out = Vec::new();
        let mut entity_anchors_seen: HashSet<IVec3> = HashSet::default();

        let lo = region.min.floor().as_ivec3();
        let hi = region.max.ceil().as_ivec3();
        for x in lo.x..hi.x {
            for y in lo.y..hi.y {
                for z in lo.z..hi.z {
                    let world = IVec3::new(x, y, z);
                    let (coord, local) = world_to_chunk(world);
                    let Some(&chunk_entity) = self.chunk_map.0.get(&coord) else {
                        // Unloaded chunk: treat as solid so the player
                        // can't walk into the unknown. Cheaper than
                        // letting them clip and re-resolve later.
                        out.push(unit_cube_aabb(world));
                        continue;
                    };
                    let Ok((chunk, sidecar)) = self.chunks.get(chunk_entity) else {
                        continue;
                    };
                    let slot = chunk.get(local);
                    if slot.is_empty() {
                        continue;
                    }
                    let def = self.registry.def(slot);
                    if !def.flags.solid {
                        continue;
                    }
                    if def.flags.walkable_boundary {
                        // Doors / open gates: solid for room detection
                        // but not for collision. The block-entity render
                        // (e.g. door mesh) still draws.
                        continue;
                    }
                    // Block-entity? Find its anchor + AABB. Otherwise it's
                    // a plain cube cell.
                    let entry = sidecar.get(world);
                    match entry {
                        Some(EntryKind::Anchor { orientation }) => {
                            if entity_anchors_seen.insert(world) {
                                let aabb = def
                                    .entity_aabb
                                    .unwrap_or_else(|| EntityAabb::cube_union(&def.footprint))
                                    .rotated(orientation);
                                out.push(model_aabb_at(world, aabb));
                            }
                        }
                        Some(EntryKind::Ghost { anchor }) => {
                            if !entity_anchors_seen.insert(anchor) {
                                continue;
                            }
                            // Look up the anchor cell to read its
                            // orientation. Anchor may live in a
                            // neighbouring (loaded) chunk.
                            let (a_coord, a_local) = world_to_chunk(anchor);
                            let Some(&a_entity) = self.chunk_map.0.get(&a_coord) else {
                                // Anchor's chunk not loaded yet — fall
                                // back to the cube to be safe.
                                out.push(unit_cube_aabb(world));
                                continue;
                            };
                            let Ok((a_chunk, a_side)) = self.chunks.get(a_entity) else {
                                continue;
                            };
                            let a_slot = a_chunk.get(a_local);
                            if a_slot.is_empty() {
                                continue;
                            }
                            let a_def = self.registry.def(a_slot);
                            let orientation = match a_side.get(anchor) {
                                Some(EntryKind::Anchor { orientation }) => orientation,
                                _ => Default::default(),
                            };
                            let aabb = a_def
                                .entity_aabb
                                .unwrap_or_else(|| EntityAabb::cube_union(&a_def.footprint))
                                .rotated(orientation);
                            out.push(model_aabb_at(anchor, aabb));
                        }
                        None => {
                            // Plain cube block.
                            out.push(unit_cube_aabb(world));
                        }
                    }
                }
            }
        }
        out
    }
}

fn unit_cube_aabb(cell: IVec3) -> Aabb {
    let f = cell.as_vec3();
    Aabb::from_min_max(f, f + Vec3::ONE)
}

/// Place a model-space `EntityAabb` (origin = anchor's bottom-centre) at
/// the world cell `anchor`. Matches the modeling convention used by the
/// raycast / preview code.
fn model_aabb_at(anchor: IVec3, aabb: EntityAabb) -> Aabb {
    let centre = anchor.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
    Aabb::from_min_max(
        centre + Vec3::from_array(aabb.min),
        centre + Vec3::from_array(aabb.max),
    )
}
