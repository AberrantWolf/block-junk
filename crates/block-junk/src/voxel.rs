use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;
use block_mesh::{GreedyQuadsBuffer, RIGHT_HANDED_Y_UP_CONFIG, greedy_quads};
use ndshape::{ConstShape, ConstShape3u32};
use serde::{Deserialize, Serialize};

use crate::blocks::{BlockRegistry, BlockSlot, MeshVoxel, TerrainSlots};
use crate::protocol::{CHUNK_PADDED, CHUNK_SIZE, ChunkCoord};

pub type ChunkShape = ConstShape3u32<CHUNK_PADDED, CHUNK_PADDED, CHUNK_PADDED>;

/// `ChunkCoord → Entity` lookup for the chunk holding that coord. Both
/// client and server initialise their own copy of this resource — same
/// type, separate worlds. Sharing the type means collision/raycast code
/// that takes `&ChunkMap` works identically on either side.
#[derive(Resource, Default)]
pub struct ChunkMap(pub HashMap<ChunkCoord, Entity>);

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
        self.entries.iter().find(|e| e.cell == cell).map(|e| e.kind)
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

    /// Mirror of [`Chunk::set`] for the padding ring: writes a padding
    /// cell, rejecting interior cells. Padding duplicates neighbour
    /// chunks' border interiors so the mesher can cull faces across
    /// chunk seams — every interior edit must be echoed into the
    /// padding of the neighbours that mirror that cell (see
    /// [`padding_mirrors`]) or their seam faces cull against stale
    /// data. Returns true if the value actually changed.
    pub fn set_padding(&mut self, cell: IVec3, block: BlockSlot) -> bool {
        if Self::is_interior(cell) {
            return false;
        }
        let Some(i) = Self::cell_index(cell) else {
            return false;
        };
        if self.blocks[i] == block {
            return false;
        }
        self.blocks[i] = block;
        true
    }

    /// Overwrite this chunk's padding ring from authoritative neighbour
    /// data: `lookup(world_cell)` returns the neighbour-interior value
    /// for that cell, or `None` to leave the current value (neighbour
    /// not loaded / not relevant). Used when a chunk is (re)created
    /// next to chunks that have diverged from terrain — freshly
    /// generated server chunks pull from edited neighbours, client
    /// snapshot arrivals pull from loaded neighbours. Returns how many
    /// cells changed.
    pub fn refresh_padding(
        &mut self,
        coord: ChunkCoord,
        lookup: impl Fn(IVec3) -> Option<BlockSlot>,
    ) -> usize {
        let mut changed = 0;
        for i in 0..ChunkShape::SIZE {
            let [lx, ly, lz] = ChunkShape::delinearize(i);
            let local = IVec3::new(lx as i32, ly as i32, lz as i32);
            if Self::is_interior(local) {
                continue;
            }
            let world = chunk_local_to_world(coord, local);
            if let Some(slot) = lookup(world)
                && self.blocks[i as usize] != slot
            {
                self.blocks[i as usize] = slot;
                changed += 1;
            }
        }
        changed
    }

    /// Every interior cell on this chunk's border, as (world cell, slot)
    /// pairs — the cells that neighbour chunks mirror in their padding.
    /// Feed to [`apply_padding_mirrors`] when a chunk (re)appears next
    /// to already-loaded neighbours, so their seam padding catches up
    /// with this chunk's actual contents.
    pub fn border_cells(&self, coord: ChunkCoord) -> Vec<(IVec3, BlockSlot)> {
        let lo = 1;
        let hi = (CHUNK_PADDED - 2) as i32;
        let mut out = Vec::new();
        for i in 0..ChunkShape::SIZE {
            let [lx, ly, lz] = ChunkShape::delinearize(i);
            let local = IVec3::new(lx as i32, ly as i32, lz as i32);
            if !Self::is_interior(local) {
                continue;
            }
            let on_border = local.x == lo
                || local.y == lo
                || local.z == lo
                || local.x == hi
                || local.y == hi
                || local.z == hi;
            if on_border {
                out.push((chunk_local_to_world(coord, local), self.blocks[i as usize]));
            }
        }
        out
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
        cell.x >= lo && cell.y >= lo && cell.z >= lo && cell.x < hi && cell.y < hi && cell.z < hi
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
                // Encode the block slot in the vertex colour's alpha
                // channel: `slot.0 as f32 / 255.0`. The chunk fragment
                // shader decodes this back to an integer and uses it to
                // index the texture-2D-array. RGB stays at 1.0 so the
                // base PBR colour multiply is a no-op (the texture
                // provides the actual hue).
                //
                // Caps the placeable slot space at 255 — far above the
                // ~10 block types vanilla ships and any plausible mod
                // surface. A higher multiplier would erode rounding
                // headroom on the f32 vertex-interpolated value.
                let cell_idx = ChunkShape::linearize(quad.minimum) as usize;
                let slot = voxels[cell_idx].slot;
                let slot_a = (slot.0 as f32) / 255.0;
                let rgba = [1.0, 1.0, 1.0, slot_a];

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

/// Every (chunk, padded-local cell) location that mirrors `world` in a
/// *neighbour* chunk's padding ring. Empty for cells away from chunk
/// borders; 1 entry for a face cell, 3 for an edge cell, 7 for a corner
/// cell (face + edge + corner neighbours in 3D). The owning chunk's own
/// interior copy is NOT included — that's `world_to_chunk`'s job.
pub fn padding_mirrors(world: IVec3) -> Vec<(ChunkCoord, IVec3)> {
    let size = CHUNK_SIZE as i32;
    let (owner, local) = world_to_chunk(world);
    // Per axis: which neighbour directions see this cell in their
    // padding. First interior layer (local == 1) → the -1 neighbour;
    // last (local == size) → the +1 neighbour.
    let dirs = |l: i32| -> &'static [i32] {
        match l {
            1 => &[-1],
            l if l == CHUNK_PADDED as i32 - 2 => &[1],
            _ => &[],
        }
    };
    let mut out = Vec::new();
    for &dx in [0].iter().chain(dirs(local.x)) {
        for &dy in [0].iter().chain(dirs(local.y)) {
            for &dz in [0].iter().chain(dirs(local.z)) {
                let d = IVec3::new(dx, dy, dz);
                if d == IVec3::ZERO {
                    continue;
                }
                // The same world cell, addressed in the neighbour's
                // local space, lands on its padding ring.
                out.push((ChunkCoord(owner.0 + d), local - d * size));
            }
        }
    }
    out
}

/// Echo a set of just-written cells into the padding rings of every
/// loaded neighbour chunk that mirrors them. Call after any interior
/// write (place, break, snapshot arrival) with the cells' new values;
/// chunks the map doesn't know about are skipped (they regenerate or
/// refresh their padding when they appear). Compare-first so an
/// already-current mirror doesn't trip `Changed<Chunk>` — on the
/// client that flag is a full remesh.
pub fn apply_padding_mirrors(
    cells: &[(IVec3, BlockSlot)],
    map: &ChunkMap,
    chunks: &mut Query<(&mut Chunk, &mut ChunkEntities)>,
) {
    for &(world, slot) in cells {
        for (coord, local) in padding_mirrors(world) {
            let Some(&entity) = map.0.get(&coord) else {
                continue;
            };
            let Ok((mut chunk, _)) = chunks.get_mut(entity) else {
                continue;
            };
            // `get` goes through Deref (no change flag); only the
            // actual write DerefMuts and marks the chunk changed.
            if chunk.get(local) != slot {
                chunk.set_padding(local, slot);
            }
        }
    }
}

/// World-space transform for a chunk's render entity. Aligns interior cell
/// `(1,1,1)` of the chunk with world cell `(coord*CHUNK_SIZE)`.
pub fn chunk_world_transform(coord: ChunkCoord) -> Transform {
    let origin = (coord.0 * CHUNK_SIZE as i32 - IVec3::ONE).as_vec3();
    Transform::from_translation(origin)
}

/// Deterministic terrain: a gentle sine-wave heightmap with grass/dirt/stone
/// layering, plus a sparse stamp-based tree population on the grass layer.
/// Identical on every machine so an unedited chunk doesn't need its bytes
/// shipped over the wire — both sides regenerate from the coord.
///
/// Tree generation is per-cell rather than per-chunk: at each cell we
/// scan the small neighborhood of potential tree roots (every (x, z)
/// is a candidate; an integer-hash of (x, z) decides whether a tree
/// actually grows there). For each root within stamp radius, we test
/// whether the current cell falls inside that tree's stamp shape and
/// return wood/leaves accordingly. This keeps `terrain_block` pure —
/// no shared mutable state between cells — so chunks regenerate the
/// same way no matter the cell-iteration order.
fn terrain_block(world: IVec3, slots: &TerrainSlots) -> BlockSlot {
    let h_here = surface_height(world.x, world.z);
    // Ground layers first. Trees only stamp into cells the ground
    // function would otherwise return `empty`, so a tree near a hill
    // doesn't bury its trunk inside the slope.
    if world.y < h_here {
        return if world.y == h_here - 1 {
            slots.grass
        } else if world.y >= h_here - 4 {
            slots.dirt
        } else {
            slots.stone
        };
    }
    // Air above the surface — check whether any nearby tree's stamp
    // claims this cell. STAMP_RADIUS bounds the (x, z) search; bigger
    // radius = more lookups per cell. 2 covers the 5-wide canopy.
    const STAMP_RADIUS: i32 = 2;
    for dx in -STAMP_RADIUS..=STAMP_RADIUS {
        for dz in -STAMP_RADIUS..=STAMP_RADIUS {
            let rx = world.x + dx;
            let rz = world.z + dz;
            if !is_tree_root(rx, rz) {
                continue;
            }
            let root_h = surface_height(rx, rz);
            let local_y = world.y - root_h;
            let trunk_h = tree_height(rx, rz);
            // Trunk: 4-5-tall column of wood, centred on the root.
            if dx == 0 && dz == 0 && (0..trunk_h).contains(&local_y) {
                return slots.wood;
            }
            // Canopy layer 1: 5x5 minus the four corners, wrapping the
            // trunk top — the corner cut is what rounds the silhouette.
            if local_y == trunk_h && !(dx.abs() == 2 && dz.abs() == 2) {
                return slots.leaves;
            }
            // Canopy layer 2: 3x3 crown one above.
            if local_y == trunk_h + 1 && dx.abs() <= 1 && dz.abs() <= 1 {
                return slots.leaves;
            }
        }
    }
    slots.empty
}

/// Trunk height (wood cells) for the tree rooted at `(x, z)`: 4 or 5,
/// picked from high bits of the same hash `is_tree_root` masks low bits
/// of, so height varies independently of placement. Pure — chunk
/// regeneration must agree on both sides of the wire.
fn tree_height(x: i32, z: i32) -> i32 {
    4 + ((tree_hash(x, z) >> 8) & 1) as i32
}

/// Floor of the sine-wave heightmap at column `(x, z)`. Pulled out so
/// the tree generator can call it for candidate root columns without
/// re-deriving the math.
fn surface_height(x: i32, z: i32) -> i32 {
    let h = (x as f32 * 0.07).sin() * 4.0 + (z as f32 * 0.05).sin() * 4.0 + 8.0;
    h.floor() as i32
}

/// Whether column `(x, z)` is a tree root. Pure hash → bool: deterministic
/// across runs, no per-chunk state, no neighbour communication. Density
/// is roughly one tree per 32 columns on average; tune by tightening or
/// loosening the mask.
fn is_tree_root(x: i32, z: i32) -> bool {
    // ~1 in 32 columns: dense enough to recognise as "a forest" along a
    // path, sparse enough that flat-grass-with-occasional-tree still
    // reads as the dominant terrain.
    (tree_hash(x, z) & 0x1F) == 0
}

/// Integer hash mixing (large primes, xorshift-style finalizer) shared
/// by tree placement (low bits) and per-tree variation (higher bits).
/// The specific constants are arbitrary; the only requirement is good
/// bit dispersion so neighbouring columns don't cluster.
fn tree_hash(x: i32, z: i32) -> u32 {
    let mut h = (x as u32)
        .wrapping_mul(73_856_093)
        .wrapping_add((z as u32).wrapping_mul(19_349_663));
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: i32 = CHUNK_SIZE as i32;

    fn empty_chunk() -> Chunk {
        Chunk {
            blocks: vec![BlockSlot::EMPTY; ChunkShape::USIZE],
        }
    }

    /// Every mirror must address the SAME world cell from the
    /// neighbour's local space, and land on its padding ring (never
    /// interior — a world cell has exactly one interior owner).
    fn assert_mirrors_roundtrip(world: IVec3, expected_count: usize) {
        let mirrors = padding_mirrors(world);
        assert_eq!(mirrors.len(), expected_count, "mirror count for {world}");
        let (owner, _) = world_to_chunk(world);
        for (coord, local) in mirrors {
            assert_ne!(coord, owner, "owner is not a mirror");
            assert_eq!(
                chunk_local_to_world(coord, local),
                world,
                "mirror ({coord:?}, {local}) must address {world}"
            );
            assert!(
                !Chunk::is_interior(local),
                "mirror local {local} must be padding"
            );
        }
    }

    #[test]
    fn padding_mirrors_by_cell_class() {
        // Deep interior: no mirrors.
        assert_mirrors_roundtrip(IVec3::new(5, 5, 5), 0);
        // Face cells (one axis on a border): 1 mirror, both sides.
        assert_mirrors_roundtrip(IVec3::new(0, 5, 5), 1);
        assert_mirrors_roundtrip(IVec3::new(SIZE - 1, 5, 5), 1);
        // Edge cell (two axes): face + face + edge neighbours = 3.
        assert_mirrors_roundtrip(IVec3::new(0, 0, 5), 3);
        // Corner cell (three axes): 3 face + 3 edge + 1 corner = 7.
        assert_mirrors_roundtrip(IVec3::new(0, 0, 0), 7);
        // Negative coords land on the right neighbours too.
        assert_mirrors_roundtrip(IVec3::new(-1, -1, -1), 7);
        assert_mirrors_roundtrip(IVec3::new(-SIZE, 7, 9), 1);
    }

    #[test]
    fn set_and_set_padding_split_the_grid() {
        let mut chunk = empty_chunk();
        let interior = IVec3::new(1, 5, 5);
        let padding = IVec3::new(0, 5, 5);
        assert!(chunk.set(interior, BlockSlot(2)));
        assert!(!chunk.set(padding, BlockSlot(2)), "set must reject padding");
        assert!(chunk.set_padding(padding, BlockSlot(2)));
        assert!(
            !chunk.set_padding(interior, BlockSlot(3)),
            "set_padding must reject interior"
        );
        assert_eq!(chunk.get(padding), BlockSlot(2));
        assert!(!chunk.set_padding(padding, BlockSlot(2)), "no-op write");
    }

    #[test]
    fn refresh_padding_pulls_only_offered_cells() {
        let mut chunk = empty_chunk();
        // World (-1, 5, 5) sits in chunk (0,0,0)'s low-x padding at
        // local (0, 6, 6).
        let target_world = IVec3::new(-1, 5, 5);
        let changed = chunk.refresh_padding(ChunkCoord(IVec3::ZERO), |w| {
            (w == target_world).then_some(BlockSlot(7))
        });
        assert_eq!(changed, 1);
        assert_eq!(chunk.get(IVec3::new(0, 6, 6)), BlockSlot(7));
        // Second pass: value already current, nothing changes.
        let changed = chunk.refresh_padding(ChunkCoord(IVec3::ZERO), |w| {
            (w == target_world).then_some(BlockSlot(7))
        });
        assert_eq!(changed, 0);
    }

    #[test]
    fn border_cells_cover_the_interior_shell() {
        let chunk = empty_chunk();
        let coord = ChunkCoord(IVec3::new(2, -1, 0));
        let borders = chunk.border_cells(coord);
        // Interior shell = interior volume minus the strict interior.
        let expected = (SIZE * SIZE * SIZE - (SIZE - 2) * (SIZE - 2) * (SIZE - 2)) as usize;
        assert_eq!(borders.len(), expected);
        for (world, _) in &borders {
            let (owner, local) = world_to_chunk(*world);
            assert_eq!(owner, coord, "border cell {world} owned elsewhere");
            assert!(Chunk::is_interior(local));
            assert!(
                !padding_mirrors(*world).is_empty(),
                "border cell {world} must have at least one mirror"
            );
        }
    }

    /// The seam invariant end-to-end (no ECS): edit a cell at the high-x
    /// border of chunk A, write the mirrors, and B's padding must agree
    /// with A's interior about that world cell.
    #[test]
    fn mirror_write_keeps_seam_consistent() {
        let mut a = empty_chunk();
        let mut b = empty_chunk();
        let a_coord = ChunkCoord(IVec3::ZERO);
        let b_coord = ChunkCoord(IVec3::new(1, 0, 0));
        let world = IVec3::new(SIZE - 1, 5, 5); // A's last interior column on x

        let (owner, local) = world_to_chunk(world);
        assert_eq!(owner, a_coord);
        assert!(a.set(local, BlockSlot(4)));

        let mirrors = padding_mirrors(world);
        assert_eq!(mirrors.len(), 1);
        let (mirror_coord, mirror_local) = mirrors[0];
        assert_eq!(mirror_coord, b_coord);
        assert!(b.set_padding(mirror_local, BlockSlot(4)));

        // Both chunks now answer "what's at `world`?" identically.
        assert_eq!(a.get(local), b.get(mirror_local));
    }
}
