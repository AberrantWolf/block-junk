//! Budgeted surface queries over coherent demanded geometry. Physical clearance
//! and architectural support remain separate, with unknown reads retained.
use crate::{
    architecture_solver::Budget,
    blocks::BlockSlot,
    collision::{Aabb, CollisionShapes},
    spatial_geometry::ChunkGeometry,
    voxel::EntryKind,
};
use bevy::math::{IVec3, Vec3};
use block_junk_mod_api::{architecture::MovementProfile, blocks::Cardinal};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

type Coord = [i32; 3];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalAccess {
    Clear,
    Obstructed(IVec3),
    Unsupported(IVec3),
    Unknown,
}
impl PhysicalAccess {
    pub fn usable(self) -> Option<bool> {
        match self {
            Self::Clear => Some(true),
            Self::Unknown => None,
            _ => Some(false),
        }
    }
}
#[derive(Clone)]
pub struct SurfaceView {
    pub chunks: BTreeMap<Coord, Arc<ChunkGeometry>>,
    pub collision: Arc<Vec<[CollisionShapes; 4]>>,
    pub architecture: Arc<Vec<[block_junk_mod_api::architecture::CompiledArchitecture; 4]>>,
    pub budget: Arc<Budget>,
    pub dependencies: Arc<Mutex<BTreeMap<Coord, Option<u64>>>>,
}
impl SurfaceView {
    pub async fn crosses_partition(
        &self,
        from: IVec3,
        to: IVec3,
        profile: &MovementProfile,
    ) -> Option<bool> {
        self.crosses_boundary(from, to, profile, false).await
    }
    pub async fn crosses_boundary(
        &self,
        from: IVec3,
        to: IVec3,
        profile: &MovementProfile,
        allow_portals: bool,
    ) -> Option<bool> {
        let axis = if from.x != to.x { 0 } else { 2 };
        let min = from.as_vec3().min(to.as_vec3()) + Vec3::new(0.5, 0., 0.5);
        let max = from.as_vec3().max(to.as_vec3()) + Vec3::new(0.5, profile.height, 0.5);
        let mut seen = BTreeSet::new();
        let mut unknown = false;
        for x in from.x.min(to.x)..=from.x.max(to.x) {
            for z in from.z.min(to.z)..=from.z.max(to.z) {
                for y in from.y.min(to.y)..=(from.y.max(to.y) as f32 + profile.height).ceil() as i32
                {
                    let Some((anchor, slot, orientation, _)) =
                        self.object(IVec3::new(x, y, z)).await
                    else {
                        unknown = true;
                        continue;
                    };
                    if !seen.insert(anchor.to_array()) {
                        continue;
                    }
                    let structure =
                        &self.architecture[slot.0 as usize][orientation as usize].structure;
                    for face in structure.partitions.iter().chain(
                        structure
                            .portals
                            .iter()
                            .filter(|_| !allow_portals)
                            .flat_map(|portal| portal.faces.iter()),
                    ) {
                        self.budget.step().await;
                        if face.normal[axis] == 0 {
                            continue;
                        }
                        let cell = anchor + IVec3::from_array(face.cell);
                        let plane = cell[axis] as f32 + u8::from(face.normal[axis] > 0) as f32;
                        if plane > min[axis]
                            && plane < max[axis]
                            && (cell.y as f32) < max.y
                            && (cell.y + 1) as f32 > min.y
                        {
                            return Some(true);
                        }
                    }
                }
            }
        }
        if unknown { None } else { Some(false) }
    }
    async fn read(&self, cell: IVec3) -> Option<(BlockSlot, Option<EntryKind>)> {
        self.budget.step().await;
        let coord = crate::voxel::world_to_chunk(cell).0.0.to_array();
        let Some(chunk) = self.chunks.get(&coord) else {
            self.dependencies.lock().unwrap().insert(coord, None);
            return None;
        };
        for (&coord, &revision) in &chunk.dependencies {
            self.budget.step().await;
            self.dependencies.lock().unwrap().insert(coord, revision);
        }
        Some((
            chunk.physical[ChunkGeometry::cell_index(cell)]?,
            chunk.sidecars.get(&cell.to_array()).copied(),
        ))
    }
    pub(crate) async fn object(
        &self,
        cell: IVec3,
    ) -> Option<(IVec3, BlockSlot, Cardinal, Option<u64>)> {
        let (slot, entry) = self.read(cell).await?;
        let (anchor, entry) = match entry {
            Some(EntryKind::Ghost { anchor }) => {
                let (anchor_slot, entry) = self.read(anchor).await?;
                if slot != anchor_slot || !matches!(entry, Some(EntryKind::Anchor { .. })) {
                    return None;
                }
                (anchor, entry)
            }
            entry => (cell, entry),
        };
        let (orientation, open) = match entry {
            Some(EntryKind::Anchor {
                orientation,
                portal_open,
            }) => (orientation, portal_open),
            _ => (Cardinal::default(), None),
        };
        Some((anchor, slot, orientation, open))
    }
    pub async fn clear(&self, bounds: Aabb) -> Option<bool> {
        self.clearance(bounds).await.usable()
    }
    pub async fn clearance(&self, bounds: Aabb) -> PhysicalAccess {
        let min = (bounds.min + Vec3::splat(0.0001)).floor().as_ivec3();
        let max = (bounds.max - Vec3::splat(0.0001)).floor().as_ivec3();
        let mut seen = BTreeSet::new();
        let mut unknown = false;
        for x in min.x..=max.x {
            for y in (min.y..=max.y).rev() {
                for z in min.z..=max.z {
                    let Some((anchor, slot, orientation, open)) =
                        self.object(IVec3::new(x, y, z)).await
                    else {
                        unknown = true;
                        continue;
                    };
                    if slot.is_empty() || !seen.insert(anchor.to_array()) {
                        continue;
                    }
                    for obstacle in
                        self.collision[slot.0 as usize][orientation as usize].at(anchor, open)
                    {
                        self.budget.step().await;
                        if obstacle.min.cmplt(bounds.max - Vec3::splat(0.0001)).all()
                            && obstacle.max.cmpgt(bounds.min + Vec3::splat(0.0001)).all()
                        {
                            return PhysicalAccess::Obstructed(anchor);
                        }
                    }
                }
            }
        }
        if unknown {
            PhysicalAccess::Unknown
        } else {
            PhysicalAccess::Clear
        }
    }
    pub fn body(foot: IVec3, profile: &MovementProfile) -> Aabb {
        let centre = foot.as_vec3() + Vec3::new(0.5, profile.height * 0.5, 0.5);
        Aabb::from_centre_half(
            centre,
            Vec3::new(profile.radius, profile.height * 0.5, profile.radius),
        )
    }
    pub async fn standable(&self, foot: IVec3, profile: &MovementProfile) -> Option<bool> {
        self.access(foot, profile).await.usable()
    }
    pub async fn access(&self, foot: IVec3, profile: &MovementProfile) -> PhysicalAccess {
        let below = foot - IVec3::Y;
        let Some((anchor, slot, orientation, open)) = self.object(below).await else {
            return PhysicalAccess::Unknown;
        };
        // Physical support is independent of architectural floor ownership:
        // actors may stand on furniture without adding it to structural area.
        let centre = foot.as_vec3() + Vec3::new(0.5, 0., 0.5);
        let supported = self.collision[slot.0 as usize][orientation as usize]
            .at(anchor, open)
            .iter()
            .any(|bounds| {
                (bounds.max.y - centre.y).abs() < 0.0001
                    && bounds.min.x <= centre.x
                    && bounds.max.x >= centre.x
                    && bounds.min.z <= centre.z
                    && bounds.max.z >= centre.z
            });
        if !supported {
            return PhysicalAccess::Unsupported(below);
        }
        self.clearance(Self::body(foot, profile)).await
    }
    /// Validate the raised horizontal segment and the vertical step/drop. This
    /// never grants the old "walk through furniture" navigation exception.
    pub async fn step(&self, from: IVec3, to: IVec3, profile: &MovementProfile) -> Option<bool> {
        let delta = to - from;
        if delta.x.abs() + delta.z.abs() != 1
            || delta.y > profile.step_up as i32
            || -delta.y > profile.max_drop as i32
        {
            return Some(false);
        }
        let high = from.y.max(to.y);
        let a = Self::body(IVec3::new(from.x, high, from.z), profile);
        let b = Self::body(IVec3::new(to.x, high, to.z), profile);
        if !self
            .clear(Aabb {
                min: a.min.min(b.min),
                max: a.max.max(b.max),
            })
            .await?
        {
            return Some(false);
        }
        let low = if to.y > from.y {
            Self::body(from, profile)
        } else {
            Self::body(to, profile)
        };
        let raised = if to.y > from.y { a } else { b };
        if !self
            .clear(Aabb {
                min: low.min.min(raised.min),
                max: low.max.max(raised.max),
            })
            .await?
        {
            return Some(false);
        }
        self.standable(to, profile).await
    }
}
