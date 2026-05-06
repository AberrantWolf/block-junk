use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::prelude::*;
use block_mesh::{
    GreedyQuadsBuffer, MergeVoxel, RIGHT_HANDED_Y_UP_CONFIG, Voxel, VoxelVisibility, greedy_quads,
};
use ndshape::{ConstShape, ConstShape3u32};
use serde::{Deserialize, Serialize};

use crate::protocol::{Block, CHUNK_PADDED};

pub type ChunkShape = ConstShape3u32<CHUNK_PADDED, CHUNK_PADDED, CHUNK_PADDED>;

impl Voxel for Block {
    fn get_visibility(&self) -> VoxelVisibility {
        match self {
            Block::Empty => VoxelVisibility::Empty,
            Block::Solid => VoxelVisibility::Opaque,
        }
    }
}

impl MergeVoxel for Block {
    type MergeValue = Self;
    fn merge_value(&self) -> Self {
        *self
    }
}

#[derive(Component, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    /// Flat array indexed by `ChunkShape`. Padded by one voxel per side so meshing
    /// has neighbour data at chunk borders.
    pub blocks: Vec<Block>,
}

pub struct RayHit {
    /// Cell containing the solid block that was hit.
    pub hit: IVec3,
    /// Outward normal of the face the ray entered through (unit vector along
    /// one axis, e.g. `(0, 1, 0)` for the +Y face). `hit + face_normal` is
    /// the cell where a "place" action would put a new block.
    pub face_normal: IVec3,
}

impl RayHit {
    /// Cell where placing a block puts it. The caller still has to check
    /// whether the chunk accepts edits at that position (e.g. interior bounds).
    pub fn place_cell(&self) -> IVec3 {
        self.hit + self.face_normal
    }
}

impl Chunk {
    pub fn new_sphere() -> Self {
        let mut blocks = vec![Block::Empty; ChunkShape::USIZE];
        let center = (CHUNK_PADDED as f32) * 0.5;
        let radius = (CHUNK_PADDED as f32) * 0.4;
        for i in 0..ChunkShape::SIZE {
            let [x, y, z] = ChunkShape::delinearize(i);
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let dz = z as f32 + 0.5 - center;
            if dx * dx + dy * dy + dz * dz <= radius * radius {
                blocks[i as usize] = Block::Solid;
            }
        }
        Self { blocks }
    }

    pub fn get(&self, cell: IVec3) -> Block {
        match Self::cell_index(cell) {
            Some(i) => self.blocks[i],
            None => Block::Empty,
        }
    }

    /// Returns true if the block actually changed. Edits at padding cells are
    /// rejected — `block-mesh` only generates faces for interior cells, so a
    /// block placed at a padding index would mutate state but never render.
    pub fn set(&mut self, cell: IVec3, block: Block) -> bool {
        if !Self::is_interior(cell) {
            return false;
        }
        // Safe: is_interior implies cell_index is Some.
        let i = Self::cell_index(cell).unwrap();
        if self.blocks[i] == block {
            return false;
        }
        self.blocks[i] = block;
        true
    }

    fn cell_index(cell: IVec3) -> Option<usize> {
        if cell.x < 0 || cell.y < 0 || cell.z < 0 {
            return None;
        }
        let max = CHUNK_PADDED as i32;
        if cell.x >= max || cell.y >= max || cell.z >= max {
            return None;
        }
        Some(ChunkShape::linearize([cell.x as u32, cell.y as u32, cell.z as u32]) as usize)
    }

    /// True for cells in the meshable interior (excludes the 1-cell padding ring).
    fn is_interior(cell: IVec3) -> bool {
        let lo = 1;
        let hi = (CHUNK_PADDED - 1) as i32;
        cell.x >= lo
            && cell.y >= lo
            && cell.z >= lo
            && cell.x < hi
            && cell.y < hi
            && cell.z < hi
    }

    /// Amanatides–Woo voxel ray traversal. `origin` and `dir` are in chunk-local
    /// coordinates (caller transforms world → local). Cells outside the chunk
    /// are treated as Empty — the ray walks until it enters the chunk or runs
    /// out of `max_distance`, so cameras outside the chunk hit fine.
    pub fn raycast(&self, origin: Vec3, dir: Vec3, max_distance: f32) -> Option<RayHit> {
        let mut cell = origin.floor().as_ivec3();
        if self.get(cell) == Block::Solid {
            return None;
        }

        let step = dir.signum().as_ivec3();
        // For each axis, the first voxel boundary along the ray is at cell+1
        // for dir>0, cell for dir<=0. When dir==0 the division gives ±inf or
        // NaN — Vec3::select replaces those with +inf so the axis is never
        // picked by argmin.
        let next = cell.as_vec3() + dir.signum().max(Vec3::ZERO);
        let mut t_max = Vec3::select(
            dir.cmpeq(Vec3::ZERO),
            Vec3::INFINITY,
            (next - origin) / dir,
        );
        // recip(0.0) = inf, so a zero-direction axis naturally never advances.
        let t_delta = dir.abs().recip();

        loop {
            let axis = argmin3(t_max);
            let t = t_max[axis];
            if t > max_distance {
                return None;
            }
            cell[axis] += step[axis];
            t_max[axis] += t_delta[axis];

            if self.get(cell) == Block::Solid {
                let mut face_normal = IVec3::ZERO;
                face_normal[axis] = -step[axis];
                return Some(RayHit { hit: cell, face_normal });
            }
        }
    }

    pub fn build_mesh(&self) -> Option<Mesh> {
        let mut buffer = GreedyQuadsBuffer::new(self.blocks.len());
        greedy_quads(
            &self.blocks,
            &ChunkShape {},
            [0; 3],
            [CHUNK_PADDED - 1; 3],
            &RIGHT_HANDED_Y_UP_CONFIG.faces,
            &mut buffer,
        );

        if buffer.quads.num_quads() == 0 {
            return None;
        }

        let num_indices = buffer.quads.num_quads() * 6;
        let num_vertices = buffer.quads.num_quads() * 4;
        let mut positions = Vec::with_capacity(num_vertices);
        let mut normals = Vec::with_capacity(num_vertices);
        let mut indices = Vec::with_capacity(num_indices);

        for (group, face) in buffer
            .quads
            .groups
            .iter()
            .zip(RIGHT_HANDED_Y_UP_CONFIG.faces.iter())
        {
            for quad in group {
                indices.extend_from_slice(&face.quad_mesh_indices(positions.len() as u32));
                positions.extend_from_slice(&face.quad_mesh_positions(quad, 1.0));
                normals.extend_from_slice(&face.quad_mesh_normals());
            }
        }

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
        mesh.insert_indices(Indices::U32(indices));
        Some(mesh)
    }
}

/// Index of the smallest component of `v`; ties go to the lower axis.
fn argmin3(v: Vec3) -> usize {
    if v.x <= v.y && v.x <= v.z {
        0
    } else if v.y <= v.z {
        1
    } else {
        2
    }
}
