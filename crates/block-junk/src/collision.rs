//! AABB-vs-voxel-grid collision sweep, shared between client (prediction)
//! and server (authority). Pure logic — no Bevy systems or schedules
//! here, just structs and functions the controller calls.
//!
//! Candidate AABBs come from three potential sources:
//!   1. Solid cube blocks via `ChunkMap` lookup (today).
//!   2. Block-entity AABBs via `ChunkEntities` sidecar, rotated by the
//!      stored `Cardinal` (today).
//!   3. Dynamic AABB entities — NPCs, players, projectiles (later).
//!
//! Sources 1 and 2 are produced by `WorldCollision::candidates`. The
//! sweep itself is source-agnostic so a third stream can be added without
//! touching collision math.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use block_junk_mod_api::blocks::EntityAabb;

use crate::blocks::BlockRegistry;
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind, world_to_chunk};

/// Closed AABB in world space. `min` and `max` are inclusive on both ends.
#[derive(Clone, Copy, Debug)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn from_centre_half(centre: Vec3, half: Vec3) -> Self {
        Self {
            min: centre - half,
            max: centre + half,
        }
    }

    pub fn from_min_max(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    pub fn centre(self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    pub fn translated(self, delta: Vec3) -> Self {
        Self {
            min: self.min + delta,
            max: self.max + delta,
        }
    }

    /// Expand to also include `self.translated(delta)` — a swept volume.
    pub fn swept(self, delta: Vec3) -> Self {
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
pub fn sweep_axis(mover: &Aabb, step: Vec3, axis: usize, world: &WorldCollision) -> Vec3 {
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
///
/// Actor-vs-actor collision deliberately lives outside this struct —
/// blocks are hard obstacles in the sweep, but actors only experience
/// each other through the per-tick soft-separation pass
/// (`soft_separate_actors`) so they can gently push one another instead
/// of getting hard-stopped at contact.
pub struct WorldCollision<'w, 's, 'a> {
    pub chunks: &'a Query<'w, 's, (&'static Chunk, &'static ChunkEntities)>,
    pub chunk_map: &'a ChunkMap,
    pub registry: &'a BlockRegistry,
}

impl WorldCollision<'_, '_, '_> {
    /// All candidate AABBs that overlap `region`. See module docs for
    /// the source-stream breakdown.
    pub fn candidates(&self, region: Aabb) -> Vec<Aabb> {
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
