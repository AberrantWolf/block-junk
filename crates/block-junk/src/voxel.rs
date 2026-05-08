use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::prelude::*;
use block_mesh::{GreedyQuadsBuffer, RIGHT_HANDED_Y_UP_CONFIG, greedy_quads};
use ndshape::{ConstShape, ConstShape3u32};
use serde::{Deserialize, Serialize};

use crate::blocks::{BlockRegistry, BlockSlot, MeshVoxel, TerrainSlots};
use crate::protocol::{CHUNK_PADDED, CHUNK_SIZE, ChunkCoord};

pub type ChunkShape = ConstShape3u32<CHUNK_PADDED, CHUNK_PADDED, CHUNK_PADDED>;

#[derive(Component, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    /// Flat array indexed by `ChunkShape`. Padded by one voxel per side so meshing
    /// has neighbour data at chunk borders.
    pub blocks: Vec<BlockSlot>,
}

pub struct RayHit {
    /// World cell containing the solid block that was hit.
    pub hit: IVec3,
    /// Outward normal of the face the ray entered through (unit vector along
    /// one axis, e.g. `(0, 1, 0)` for the +Y face). `hit + face_normal` is
    /// the world cell where a "place" action would put a new block.
    pub face_normal: IVec3,
}

impl Chunk {
    /// Generate a chunk at `coord` from the deterministic terrain function.
    /// Both server and client derive the same blocks for an unedited chunk
    /// — that's what enables the "procedural-default" bandwidth shortcut
    /// described in the networking-design skill.
    pub fn from_terrain(coord: ChunkCoord, slots: &TerrainSlots) -> Self {
        let mut blocks = vec![BlockSlot::EMPTY; ChunkShape::USIZE];
        for i in 0..ChunkShape::SIZE {
            let [lx, ly, lz] = ChunkShape::delinearize(i);
            let world = chunk_local_to_world(coord, IVec3::new(lx as i32, ly as i32, lz as i32));
            blocks[i as usize] = terrain_block(world, slots);
        }
        Self { blocks }
    }

    pub fn get(&self, cell: IVec3) -> BlockSlot {
        match Self::cell_index(cell) {
            Some(i) => self.blocks[i],
            None => BlockSlot::EMPTY,
        }
    }

    /// Returns true if the block actually changed. Edits at padding cells are
    /// rejected — `block-mesh` only generates faces for interior cells, so a
    /// block placed at a padding index would mutate state but never render.
    pub fn set(&mut self, cell: IVec3, block: BlockSlot) -> bool {
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

    pub fn build_mesh(&self, registry: &BlockRegistry) -> Option<Mesh> {
        // Convert to MeshVoxels so the greedy mesher sees mesh-bearing
        // slots as Empty (those render as ECS entities elsewhere).
        let voxels: Vec<MeshVoxel> = self
            .blocks
            .iter()
            .map(|&slot| MeshVoxel {
                slot,
                visibility: registry.voxel_visibility(slot),
            })
            .collect();
        let mut buffer = GreedyQuadsBuffer::new(voxels.len());
        greedy_quads(
            &voxels,
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
        let mut colors = Vec::with_capacity(num_vertices);
        let mut indices = Vec::with_capacity(num_indices);

        for (group, face) in buffer
            .quads
            .groups
            .iter()
            .zip(RIGHT_HANDED_Y_UP_CONFIG.faces.iter())
        {
            for quad in group {
                // Look up the block this quad belongs to so each face's
                // four verts get the right colour (Bevy's StandardMaterial
                // multiplies vertex colour by base_color when ATTRIBUTE_COLOR
                // is present).
                let cell_idx = ChunkShape::linearize(quad.minimum) as usize;
                let slot = voxels[cell_idx].slot;
                let [r, g, b] = registry.def(slot).color;
                let rgba = [r, g, b, 1.0];

                indices.extend_from_slice(&face.quad_mesh_indices(positions.len() as u32));
                positions.extend_from_slice(&face.quad_mesh_positions(quad, 1.0));
                normals.extend_from_slice(&face.quad_mesh_normals());
                colors.extend_from_slice(&[rgba; 4]);
            }
        }

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
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

/// World-space Amanatides–Woo voxel raycast. Steps cells in world coords;
/// each cell is looked up via `get_block`, which is responsible for
/// resolving the world cell to its owning chunk and returning the slot.
///
/// Returning `BlockSlot::EMPTY` for unloaded chunks is fine — the ray
/// just walks past them. `max_distance` is in world cells.
pub fn world_raycast(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    get_block: impl Fn(IVec3) -> BlockSlot,
) -> Option<RayHit> {
    let mut cell = origin.floor().as_ivec3();
    if !get_block(cell).is_empty() {
        return None;
    }

    let step = dir.signum().as_ivec3();
    let next = cell.as_vec3() + dir.signum().max(Vec3::ZERO);
    // Per-axis t to the next voxel boundary. NaN-safe: when dir==0 the
    // division yields ±inf or NaN; select replaces it with +inf so that
    // axis is never picked.
    let mut t_max = Vec3::select(
        dir.cmpeq(Vec3::ZERO),
        Vec3::INFINITY,
        (next - origin) / dir,
    );
    let t_delta = dir.abs().recip();

    loop {
        let axis = argmin3(t_max);
        let t = t_max[axis];
        if t > max_distance {
            return None;
        }
        cell[axis] += step[axis];
        t_max[axis] += t_delta[axis];

        if !get_block(cell).is_empty() {
            let mut face_normal = IVec3::ZERO;
            face_normal[axis] = -step[axis];
            return Some(RayHit {
                hit: cell,
                face_normal,
            });
        }
    }
}

/// World-space coordinate of a chunk's local cell. Chunk-local indices live
/// in `[0, CHUNK_PADDED)` with interior at `[1, CHUNK_PADDED-1)`; the world
/// cell corresponds to the padding-stripped position.
pub fn chunk_local_to_world(coord: ChunkCoord, local: IVec3) -> IVec3 {
    coord.0 * CHUNK_SIZE as i32 + local - IVec3::ONE
}

/// Inverse of `chunk_local_to_world`: pick the unique (chunk, interior-local)
/// pair that corresponds to a world cell. Uses `div_euclid`/`rem_euclid` so
/// negative world coords land on the right chunk.
///
/// Important for edits at chunk boundaries: a raycast may hit a *padding*
/// cell of one chunk that's actually the *interior* of its neighbour. The
/// edit needs to be addressed to the neighbour or `Chunk::set` will refuse it.
pub fn world_to_chunk(world: IVec3) -> (ChunkCoord, IVec3) {
    let size = CHUNK_SIZE as i32;
    let coord = ChunkCoord(IVec3::new(
        world.x.div_euclid(size),
        world.y.div_euclid(size),
        world.z.div_euclid(size),
    ));
    let local = IVec3::new(
        world.x.rem_euclid(size) + 1,
        world.y.rem_euclid(size) + 1,
        world.z.rem_euclid(size) + 1,
    );
    (coord, local)
}

/// World-space transform for a chunk's render entity. Aligns interior cell
/// `(1,1,1)` of the chunk with world cell `(coord*CHUNK_SIZE)`.
pub fn chunk_world_transform(coord: ChunkCoord) -> Transform {
    let origin = (coord.0 * CHUNK_SIZE as i32 - IVec3::ONE).as_vec3();
    Transform::from_translation(origin)
}

/// Deterministic terrain: a gentle sine-wave heightmap with grass/dirt/stone
/// layering. Identical on every machine so an unedited chunk doesn't need
/// its bytes shipped over the wire — both sides regenerate from the coord.
fn terrain_block(world: IVec3, slots: &TerrainSlots) -> BlockSlot {
    let h = (world.x as f32 * 0.07).sin() * 4.0
        + (world.z as f32 * 0.05).sin() * 4.0
        + 8.0;
    let h = h.floor() as i32;
    if world.y >= h {
        slots.empty
    } else if world.y == h - 1 {
        slots.grass
    } else if world.y >= h - 4 {
        slots.dirt
    } else {
        slots.stone
    }
}
