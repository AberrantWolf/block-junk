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
                        out.push(unit_cube_aabb(world));
                        continue;
                    };
                    let slot = chunk.get(local);
                    if slot.is_empty() {
                        continue;
                    }
                    let anchor = match sidecar.get(world) {
                        Some(EntryKind::Ghost { anchor }) => anchor,
                        _ => world,
                    };
                    let (anchor_coord, anchor_local) = world_to_chunk(anchor);
                    let Some((anchor_chunk, anchor_sidecar)) = self
                        .chunk_map
                        .0
                        .get(&anchor_coord)
                        .and_then(|entity| self.chunks.get(*entity).ok())
                    else {
                        // Each unresolved ghost remains an obstacle. Do not mark
                        // its anchor seen until authoritative geometry is available.
                        out.push(unit_cube_aabb(world));
                        continue;
                    };
                    if anchor_chunk.get(anchor_local) != slot {
                        out.push(unit_cube_aabb(world));
                        continue;
                    }
                    if !entity_anchors_seen.insert(anchor) {
                        continue;
                    }
                    let (orientation, open) = match anchor_sidecar.get(anchor) {
                        Some(EntryKind::Anchor {
                            orientation,
                            portal_open,
                        }) => (orientation, portal_open),
                        _ => (Default::default(), None),
                    };
                    out.extend(
                        self.registry.collision[slot.0 as usize][orientation as usize]
                            .at(anchor, open),
                    );
                }
            }
        }
        out
    }
}

/// Collision geometry is compiled once for each orientation. Runtime portal
/// changes select leaves; they never rebuild or rotate a frame during movement.
#[derive(Clone, Debug)]
pub(crate) struct CollisionShapes {
    pub frame: Vec<Aabb>,
    pub leaves: Vec<Vec<Aabb>>,
    pub default_open: u64,
}
impl CollisionShapes {
    pub fn compile(
        def: &block_junk_mod_api::blocks::BlockDef,
        orientation: block_junk_mod_api::blocks::Cardinal,
    ) -> Self {
        let mut frame = if def.flags.solid && !def.flags.walkable_boundary {
            vec![model_aabb_at(
                IVec3::ZERO,
                def.entity_aabb
                    .unwrap_or_else(|| EntityAabb::cube_union(&def.footprint))
                    .rotated(orientation),
            )]
        } else {
            Vec::new()
        };
        let mut leaves = Vec::new();
        let mut default_open = 0;
        for (index, portal) in def.structure.portals.iter().enumerate() {
            default_open |= u64::from(portal.initially_open) << index;
            let mut leaf = Vec::new();
            if portal.volume.is_empty() {
                for face in &portal.faces {
                    let cell = IVec3::from_array(orientation.rotate_offset(face.cell));
                    let normal = orientation.rotate_offset(face.normal);
                    let axis = normal
                        .iter()
                        .position(|component| *component != 0)
                        .expect("validated face");
                    let plane = cell[axis] as f32 + u8::from(normal[axis] > 0) as f32;
                    let mut bounds = unit_cube_aabb(cell);
                    bounds.min[axis] = plane - 0.001;
                    bounds.max[axis] = plane + 0.001;
                    leaf.push(bounds);
                }
            }
            for offset in &portal.volume {
                let cut = unit_cube_aabb(IVec3::from_array(orientation.rotate_offset(*offset)));
                frame = frame
                    .into_iter()
                    .flat_map(|bounds| subtract_box(bounds, cut))
                    .collect();
                leaf.push(cut);
            }
            leaves.push(leaf);
        }
        Self {
            frame,
            leaves,
            default_open,
        }
    }
    pub fn at(&self, anchor: IVec3, open: Option<u64>) -> Vec<Aabb> {
        let open = open.unwrap_or(self.default_open);
        self.frame
            .iter()
            .chain(
                self.leaves
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| open & (1 << index) == 0)
                    .flat_map(|(_, leaf)| leaf.iter()),
            )
            .map(|bounds| Aabb {
                min: bounds.min + anchor.as_vec3(),
                max: bounds.max + anchor.as_vec3(),
            })
            .collect()
    }
}

fn subtract_box(bounds: Aabb, cut: Aabb) -> Vec<Aabb> {
    let lo = bounds.min.max(cut.min);
    let hi = bounds.max.min(cut.max);
    if lo.cmpge(hi).any() {
        return vec![bounds];
    }
    let mut remainder = bounds;
    let mut parts = Vec::with_capacity(6);
    for axis in 0..3 {
        if remainder.min[axis] < lo[axis] {
            let mut part = remainder;
            part.max[axis] = lo[axis];
            parts.push(part);
            remainder.min[axis] = lo[axis];
        }
        if remainder.max[axis] > hi[axis] {
            let mut part = remainder;
            part.min[axis] = hi[axis];
            parts.push(part);
            remainder.max[axis] = hi[axis];
        }
    }
    parts
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn procedural_furnishing_uses_authored_collision_before_sidecar_materialization() {
        let registry = BlockRegistry::build(serde_json::from_value(serde_json::json!([
            {"id":"test:stool","display_name":"Stool","color":[1,1,1],"flags":{"solid":true},"mesh":"fixture.gltf",
             "entity_aabb":{"min":[-0.25,0,-0.25],"max":[0.25,0.5,0.25]}}
        ])).unwrap()).unwrap().0;
        let mut world = World::new();
        let mut chunk = Chunk {
            revision: 1,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        chunk.set(world_to_chunk(IVec3::Y).1, registry.require("test:stool"));
        let entity = world.spawn((chunk, ChunkEntities::default())).id();
        let mut map = ChunkMap::default();
        map.0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        let mut state =
            bevy::ecs::system::SystemState::<Query<(&Chunk, &ChunkEntities)>>::new(&mut world);
        let query = state.get(&world).unwrap();
        let collision = WorldCollision {
            chunks: &query,
            chunk_map: &map,
            registry: &registry,
        };
        let bounds = collision.candidates(Aabb {
            min: Vec3::Y,
            max: Vec3::new(1., 2., 1.),
        });
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].min, Vec3::new(0.25, 1., 0.25));
        assert_eq!(bounds[0].max, Vec3::new(0.75, 1.5, 0.75));
    }
}
