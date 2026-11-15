//! Finite-world reference model for the colony spatial rework.
//!
//! This is an experimental test oracle, not the live room detector. Structural
//! surfaces, architectural partitions, and furnishing placements are separate.
//! It deliberately recomputes everything and uses ordered collections so fixture
//! output is reproducible. See the crate README for unsupported geometry.

pub mod architecture;
pub use architecture::{Architecture, Space, SpaceId, Volume, detect_architecture};
pub mod content;
pub mod fixtures;
pub mod openings;
mod reference;
pub use reference::*;

use std::collections::{BTreeMap, BTreeSet};

/// Integer voxel coordinate. A floor sample is the air cell above its support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell(pub i32, pub i32, pub i32);

impl Cell {
    pub fn offset(self, x: i32, y: i32, z: i32) -> Self {
        Self(self.0 + x, self.1 + y, self.2 + z)
    }

    pub fn horizontal_neighbors(self) -> [Self; 4] {
        [
            self.offset(-1, 0, 0),
            self.offset(1, 0, 0),
            self.offset(0, 0, -1),
            self.offset(0, 0, 1),
        ]
    }
}

/// Inclusive finite observation domain. Space beyond it is unknown, not air.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub min: Cell,
    pub max: Cell,
}

impl Bounds {
    pub fn contains(self, c: Cell) -> bool {
        (self.min.0..=self.max.0).contains(&c.0)
            && (self.min.1..=self.max.1).contains(&c.1)
            && (self.min.2..=self.max.2).contains(&c.2)
    }

    pub fn cells(self) -> impl Iterator<Item = Cell> {
        (self.min.0..=self.max.0).flat_map(move |x| {
            (self.min.1..=self.max.1)
                .flat_map(move |y| (self.min.2..=self.max.2).map(move |z| Cell(x, y, z)))
        })
    }
}

/// Structural occupancy only; furniture is never inserted into this grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voxel {
    Air,
    Solid,
    Unknown,
}

/// Architectural separation is independent of physical traversal and weather.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Partition {
    Wall,
    Window,
    Portal { open: bool },
}

impl Partition {
    pub fn traversable(self) -> bool {
        matches!(self, Self::Portal { open: true })
    }

    pub fn shelters(self) -> bool {
        !self.traversable()
    }
}

/// Canonical horizontal face between two adjacent cells, never a volume or AABB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Face(Cell, Cell);

impl Face {
    pub fn new(a: Cell, b: Cell) -> Result<Self, String> {
        if a.1 != b.1 || a.0.abs_diff(b.0) as u64 + a.2.abs_diff(b.2) as u64 != 1 {
            return Err("partition faces require horizontally adjacent cells".into());
        }
        Ok(Self(a.min(b), a.max(b)))
    }

    pub fn cells(self) -> (Cell, Cell) {
        (self.0, self.1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlacementId(pub u64);

/// Exact world-space footprint. Tags name installed content, not usable capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub id: PlacementId,
    pub cells: BTreeSet<Cell>,
    pub tags: BTreeSet<String>,
}

impl Placement {
    pub fn new(
        id: u64,
        cells: impl IntoIterator<Item = Cell>,
        tags: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, String> {
        let mut footprint = BTreeSet::new();
        for cell in cells {
            if !footprint.insert(cell) {
                return Err(format!(
                    "placement {id} has duplicate footprint cell {cell:?}"
                ));
            }
        }
        if footprint.is_empty() || id == 0 {
            return Err("placements require a nonzero ID and a nonempty footprint".into());
        }
        Ok(Self {
            id: PlacementId(id),
            cells: footprint,
            tags: tags.into_iter().map(Into::into).collect(),
        })
    }
}

/// Explicit fixture stair/ramp connection. Links connect patches; they do not
/// merge floors or turn wall tops into the same room through adjacency alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceLink {
    pub a: Cell,
    pub b: Cell,
}

/// Synthetic structural world. Construction validates the finite coordinate
/// domain; mutators reject conflicting geometry rather than silently repairing it.
#[derive(Clone, Debug)]
pub struct Scene {
    bounds: Bounds,
    voxels: BTreeMap<Cell, Voxel>,
    partitions: BTreeMap<Face, Partition>,
    placements: BTreeMap<PlacementId, Placement>,
    links: BTreeSet<SurfaceLink>,
    /// Explicit knowledge that above the observed column there is open sky.
    sky_columns: BTreeSet<(i32, i32)>,
}

impl Scene {
    /// Read-only fixture export for independent live-engine parity tests.
    pub fn authored_partitions(&self) -> impl Iterator<Item = (Face, Partition)> + '_ {
        self.partitions
            .iter()
            .map(|(face, partition)| (*face, *partition))
    }
    pub fn authored_links(&self) -> impl Iterator<Item = SurfaceLink> + '_ {
        self.links.iter().copied()
    }
    pub fn known_sky(&self, x: i32, z: i32) -> bool {
        self.sky_columns.contains(&(x, z))
    }
    pub fn new(bounds: Bounds) -> Result<Self, String> {
        let Bounds { min, max } = bounds;
        if min.0 > max.0 || min.1 > max.1 || min.2 > max.2 {
            return Err("inverted observation bounds".into());
        }
        // The reference detector probes neighboring cells; retain arithmetic
        // headroom. This is an input restriction, not an outdoor classification.
        if [min.0, min.1, min.2, max.0, max.1, max.2]
            .into_iter()
            .any(|v| !(i32::MIN + 2..=i32::MAX - 2).contains(&v))
        {
            return Err("observation bounds need two cells of coordinate headroom".into());
        }
        Ok(Self {
            bounds,
            voxels: BTreeMap::new(),
            partitions: BTreeMap::new(),
            placements: BTreeMap::new(),
            links: BTreeSet::new(),
            sky_columns: BTreeSet::new(),
        })
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    pub fn voxel(&self, c: Cell) -> Voxel {
        if !self.bounds.contains(c) {
            return Voxel::Unknown;
        }
        self.voxels.get(&c).copied().unwrap_or(Voxel::Air)
    }

    pub fn set_voxel(&mut self, cell: Cell, voxel: Voxel) -> Result<(), String> {
        if !self.bounds.contains(cell) {
            return Err(format!("cell {cell:?} outside observation bounds"));
        }
        if voxel != Voxel::Air && self.placements.values().any(|p| p.cells.contains(&cell)) {
            return Err(format!("structural edit overlaps furniture at {cell:?}"));
        }
        self.voxels.insert(cell, voxel);
        Ok(())
    }

    pub fn set_partition(&mut self, face: Face, partition: Partition) -> Result<(), String> {
        let (a, b) = face.cells();
        if !self.bounds.contains(a) || !self.bounds.contains(b) {
            return Err("partition outside observation bounds".into());
        }
        self.partitions.insert(face, partition);
        Ok(())
    }

    pub fn remove_partition(&mut self, face: Face) {
        self.partitions.remove(&face);
    }

    pub fn partition(&self, a: Cell, b: Cell) -> Option<Partition> {
        Face::new(a, b)
            .ok()
            .and_then(|face| self.partitions.get(&face).copied())
    }

    pub fn add_placement(&mut self, placement: Placement) -> Result<(), String> {
        if placement.id.0 == 0 || placement.cells.is_empty() {
            return Err("placements require a nonzero ID and a nonempty footprint".into());
        }
        if self.placements.contains_key(&placement.id) {
            return Err(format!("duplicate placement ID {:?}", placement.id));
        }
        for &c in &placement.cells {
            if self.voxel(c) != Voxel::Air {
                return Err(format!(
                    "placement {:?} overlaps structure/unknown cell {c:?}",
                    placement.id
                ));
            }
            if self.placements.values().any(|p| p.cells.contains(&c)) {
                return Err(format!("overlapping placement at {c:?}"));
            }
        }
        self.placements.insert(placement.id, placement);
        Ok(())
    }

    pub fn placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.values()
    }

    pub fn add_surface_link(&mut self, a: Cell, b: Cell) -> Result<(), String> {
        if a.1.abs_diff(b.1) != 1 || a.0.abs_diff(b.0) as u64 + a.2.abs_diff(b.2) as u64 != 1 {
            return Err("prototype surface links require a one-cell step".into());
        }
        if !self.step_is_clear(a, b) {
            return Err("surface links require supported endpoints, two-cell swept clearance, and no partition".into());
        }
        self.links.insert(SurfaceLink {
            a: a.min(b),
            b: a.max(b),
        });
        Ok(())
    }

    /// Structural clearance for the prototype's two-cell-tall walker. Furniture
    /// and actual actor collision remain separate; this is not a navigation API.
    pub(crate) fn step_is_clear(&self, a: Cell, b: Cell) -> bool {
        let (low, high) = if a.1 < b.1 { (a, b) } else { (b, a) };
        if !self.bounds.contains(low) || !self.bounds.contains(high) {
            return false;
        }
        for c in [low, high] {
            if self.voxel(c.offset(0, -1, 0)) != Voxel::Solid {
                return false;
            }
            for dy in 0..2 {
                if self.voxel(c.offset(0, dy, 0)) != Voxel::Air {
                    return false;
                }
            }
        }
        if self.voxel(low.offset(0, 2, 0)) != Voxel::Air {
            return false;
        }
        for dy in 0..2 {
            if self
                .partition(low.offset(0, dy + 1, 0), high.offset(0, dy, 0))
                .is_some_and(|p| !matches!(p, Partition::Portal { .. }))
            {
                return false;
            }
        }
        true
    }

    pub fn set_known_sky(&mut self, x: i32, z: i32, known: bool) -> Result<(), String> {
        if !self.bounds.contains(Cell(x, self.bounds.max.1, z)) {
            return Err("sky column outside observation bounds".into());
        }
        if known {
            self.sky_columns.insert((x, z));
        } else {
            self.sky_columns.remove(&(x, z));
        }
        Ok(())
    }
}
