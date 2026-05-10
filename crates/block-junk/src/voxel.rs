use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;
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

/// Per-chunk sidecar describing block-entity metadata at specific cells.
/// One entry per cell *inside this chunk* that participates in a
/// block-entity (anchor or ghost). Cross-chunk footprints are encoded with
/// a `Ghost` entry pointing at an `Anchor` entry that may live in a
/// neighbouring chunk — until both chunks have arrived at the client,
/// orphan ghosts are non-rendering placeholders.
///
/// Cells that hold a single-cell non-mesh block (stone, dirt, etc.) have
/// no entry here; the slot grid alone tells the full story for them.
///
/// Stored as a flat `Vec` because the entry count per chunk is small in
/// practice (most chunks have zero) and a linear scan is faster than a
/// hashmap lookup at that size. Cell coords are world-space — same key
/// used by the placement / break protocol — so no chunk-local conversion
/// is needed when looking up an entry.
#[derive(Component, Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChunkEntities {
    pub entries: Vec<EntityEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityEntry {
    /// World-space cell this entry describes.
    pub cell: IVec3,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryKind {
    /// The anchor cell of a block-entity. Carries the orientation the
    /// entity was placed at; the slot at this cell tells you which entity.
    Anchor { orientation: Cardinal },
    /// A non-anchor cell of a block-entity whose anchor is at `anchor`
    /// (world cell). The slot at this cell echoes the anchor's slot —
    /// that's how the cube mesher knows to skip these cells (their slot's
    /// `def.mesh.is_some()`).
    Ghost { anchor: IVec3 },
}

impl ChunkEntities {
    /// Linear lookup. O(n) but n is tiny in practice.
    pub fn get(&self, cell: IVec3) -> Option<EntryKind> {
        self.entries
            .iter()
            .find(|e| e.cell == cell)
            .map(|e| e.kind)
    }

    /// Set or replace the entry at `cell`. Returns the previous entry if
    /// any.
    pub fn insert(&mut self, cell: IVec3, kind: EntryKind) -> Option<EntryKind> {
        let prev = self
            .entries
            .iter()
            .position(|e| e.cell == cell)
            .map(|i| self.entries.swap_remove(i).kind);
        self.entries.push(EntityEntry { cell, kind });
        prev
    }

    /// Remove and return the entry at `cell`.
    pub fn remove(&mut self, cell: IVec3) -> Option<EntryKind> {
        self.entries
            .iter()
            .position(|e| e.cell == cell)
            .map(|i| self.entries.swap_remove(i).kind)
    }

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
