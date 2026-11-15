//! Experimental content adapter. These definitions describe architectural effects
//! explicitly; they are not inferred from rendering/collision flags or vanilla IDs.
//! The schema is intentionally not yet part of the stable Lua API.
use crate::{Cell, Face, Partition, Placement, Scene, Voxel};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
pub enum Facing {
    North,
    East,
    South,
    West,
}

impl Facing {
    fn transform(self, local: Cell, anchor: Cell) -> Result<Cell, String> {
        let (x, y, z) = (i64::from(local.0), i64::from(local.1), i64::from(local.2));
        let (x, z) = match self {
            Self::North => (x, z),
            Self::East => (-z, x),
            Self::South => (-x, -z),
            Self::West => (z, -x),
        };
        Ok(Cell(
            i32::try_from(x + i64::from(anchor.0)).map_err(|_| "content coordinate overflow")?,
            i32::try_from(y + i64::from(anchor.1)).map_err(|_| "content coordinate overflow")?,
            i32::try_from(z + i64::from(anchor.2)).map_err(|_| "content coordinate overflow")?,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct LocalPartition {
    pub a: Cell,
    pub b: Cell,
    pub kind: Partition,
}

#[derive(Clone, Debug)]
pub struct SpatialDef {
    pub id: String,
    pub structural_cells: Vec<Cell>,
    pub furnishing_cells: Vec<Cell>,
    pub tags: Vec<String>,
    pub partitions: Vec<LocalPartition>,
    /// Authored stair connections, independent of partitions and furniture.
    pub steps: Vec<(Cell, Cell)>,
}

impl SpatialDef {
    /// Validate and apply as one operation. A bad rotated footprint or conflicting
    /// partition cannot leave half an object in the synthetic world.
    pub fn instantiate(
        &self,
        scene: &mut Scene,
        anchor: Cell,
        facing: Facing,
        id: u64,
    ) -> Result<(), String> {
        self.apply(scene, anchor, facing, id)
            .map_err(|e| format!("{}: {e}", self.id))
    }

    fn apply(
        &self,
        scene: &mut Scene,
        anchor: Cell,
        facing: Facing,
        id: u64,
    ) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("definition ID is empty".into());
        }
        let mut staged = scene.clone();
        let mut occupied = BTreeSet::new();
        for &local in self.structural_cells.iter().chain(&self.furnishing_cells) {
            if !occupied.insert(local) {
                return Err(format!("duplicate/overlapping local cell {local:?}"));
            }
            let world = facing.transform(local, anchor)?;
            if staged.voxel(world) != Voxel::Air {
                return Err(format!("occupied or unknown cell {world:?}"));
            }
        }
        for &local in &self.structural_cells {
            staged.set_voxel(facing.transform(local, anchor)?, Voxel::Solid)?;
        }
        if !self.furnishing_cells.is_empty() {
            let cells: Result<Vec<_>, _> = self
                .furnishing_cells
                .iter()
                .map(|&c| facing.transform(c, anchor))
                .collect();
            staged.add_placement(Placement::new(id, cells?, self.tags.clone())?)?;
        }
        let mut faces = BTreeSet::new();
        for p in &self.partitions {
            let a = facing.transform(p.a, anchor)?;
            let b = facing.transform(p.b, anchor)?;
            let face = Face::new(a, b)?;
            if !faces.insert(face) || staged.partition(a, b).is_some() {
                return Err(format!("conflicting partition {face:?}"));
            }
            if staged.voxel(a) != Voxel::Air || staged.voxel(b) != Voxel::Air {
                return Err("partition endpoints must be structural air".into());
            }
            staged.set_partition(face, p.kind)?;
        }
        for &(a, b) in &self.steps {
            staged.add_surface_link(facing.transform(a, anchor)?, facing.transform(b, anchor)?)?;
        }
        *scene = staged;
        Ok(())
    }
}
