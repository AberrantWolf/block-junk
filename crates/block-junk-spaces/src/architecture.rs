//! Architectural spaces over exact floor surfaces. The simple horizontal-patch
//! detector remains an independent primitive. Passage thresholds are excluded
//! before measuring space area; shared interior air connects multilevel surfaces.
use crate::{
    openings::{Opening, infer_openings},
    *,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SpaceId(pub Cell);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volume {
    Bounded(usize),
    Open,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub id: SpaceId,
    pub floor_surfaces: BTreeMap<i32, BTreeSet<Cell>>,
    pub roofs: BTreeMap<Cell, Roof>,
    pub boundary: BoundaryState,
    pub pending: bool,
    pub volume: Volume,
    pub placements: BTreeSet<PlacementId>,
    pub dependencies: BTreeSet<Cell>,
}
impl Space {
    pub fn structural_area(&self) -> usize {
        self.floor_surfaces.values().map(BTreeSet::len).sum()
    }
    pub fn floor(&self) -> impl Iterator<Item = Cell> + '_ {
        self.floor_surfaces.values().flat_map(|s| s.iter().copied())
    }
    pub fn roof_fraction(&self) -> Option<f64> {
        if self.roofs.values().any(|r| *r == Roof::Unknown) {
            return None;
        }
        Some(
            self.roofs
                .values()
                .filter(|r| matches!(r, Roof::Covered { .. }))
                .count() as f64
                / self.structural_area() as f64,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceConnection {
    pub partition: Option<Partition>,
    pub faces: BTreeSet<Face>,
    pub ends: [BTreeSet<SpaceId>; 2],
    pub threshold: BTreeSet<Cell>,
    pub volume: BTreeSet<Cell>,
    pub traversable: bool,
    pub evidence: BTreeSet<Cell>,
    pub placements: BTreeSet<PlacementId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Architecture {
    pub spaces: BTreeMap<SpaceId, Space>,
    pub connections: Vec<SpaceConnection>,
    pub placement_membership: BTreeMap<PlacementId, BTreeSet<SpaceId>>,
    pub openings: Vec<Opening>,
    floors: BTreeMap<Cell, SpaceId>,
}
impl Architecture {
    pub fn at_floor(&self, cell: Cell) -> Option<&Space> {
        self.floors.get(&cell).and_then(|id| self.spaces.get(id))
    }
}

struct AirRegion {
    cells: BTreeSet<Cell>,
}

/// Bound architectural membership horizontally before linking elevations. Air
/// above the walls belongs to weather/sky connectivity, not an automatic bridge
/// between every courtyard. Roof holes therefore never change surface grouping.
fn air_regions(scene: &Scene) -> (Vec<AirRegion>, BTreeMap<Cell, usize>) {
    let mut enclosed = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for seed in scene.bounds.cells() {
        if scene.voxel(seed) != Voxel::Air || !visited.insert(seed) {
            continue;
        }
        let mut section = BTreeSet::new();
        let mut queue = VecDeque::from([seed]);
        let mut bounded = true;
        while let Some(c) = queue.pop_front() {
            section.insert(c);
            for n in c.horizontal_neighbors() {
                if scene.partition(c, n).is_some() {
                    continue;
                }
                match scene.voxel(n) {
                    Voxel::Solid => {}
                    Voxel::Unknown => bounded = false,
                    Voxel::Air => {
                        if visited.insert(n) {
                            queue.push_back(n);
                        }
                    }
                }
            }
        }
        if bounded {
            enclosed.extend(section);
        }
    }
    let mut regions = Vec::new();
    let mut index = BTreeMap::new();
    for &seed in &enclosed {
        if index.contains_key(&seed) {
            continue;
        }
        let id = regions.len();
        index.insert(seed, id);
        let mut queue = VecDeque::from([seed]);
        let mut region = AirRegion {
            cells: BTreeSet::new(),
        };
        while let Some(c) = queue.pop_front() {
            region.cells.insert(c);
            for n in c
                .horizontal_neighbors()
                .into_iter()
                .chain([c.offset(0, -1, 0), c.offset(0, 1, 0)])
            {
                if !enclosed.contains(&n) || scene.partition(c, n).is_some() {
                    continue;
                }
                if let std::collections::btree_map::Entry::Vacant(entry) = index.entry(n) {
                    entry.insert(id);
                    queue.push_back(n);
                }
            }
        }
        regions.push(region);
    }
    (regions, index)
}

fn root(parent: &BTreeMap<PatchId, PatchId>, mut id: PatchId) -> PatchId {
    while parent[&id] != id {
        id = parent[&id];
    }
    id
}
fn floor_below(
    scene: &Scene,
    floors: &BTreeMap<Cell, PatchId>,
    cell: Cell,
) -> Option<(Cell, PatchId)> {
    for y in (scene.bounds.min.1..=cell.1).rev() {
        let c = Cell(cell.0, y, cell.2);
        if scene.voxel(c) != Voxel::Air {
            return None;
        }
        if let Some(&id) = floors.get(&c) {
            return Some((c, id));
        }
    }
    None
}

pub fn detect_architecture(authored: &Scene) -> Architecture {
    let openings = infer_openings(authored);
    let mut separated = authored.clone();
    // A temporary structural mask makes threshold air unavailable to either
    // room. This never mutates authored geometry or obstructs actual navigation.
    for opening in &openings {
        for &cell in &opening.volume_cells {
            separated.voxels.insert(cell, Voxel::Solid);
        }
    }
    let patches = detect(&separated);
    let floors: BTreeMap<_, _> = patches
        .patches
        .iter()
        .flat_map(|(&id, p)| p.floor.iter().map(move |&c| (c, id)))
        .collect();
    let (regions, air_index) = air_regions(&separated);
    let mut parent: BTreeMap<_, _> = patches.patches.keys().map(|&id| (id, id)).collect();
    // A visible drop between surfaces within the same bounded interior is
    // evidence of an open stepped hall/gallery. Traversal links alone do not
    // merge outdoor terraces, balconies, or separate storeys through portals.
    for (&upper, &upper_patch) in &floors {
        let Some(&region) = air_index.get(&upper) else {
            continue;
        };
        for n in upper.horizontal_neighbors() {
            if separated.partition(upper, n).is_some() {
                continue;
            }
            let Some((lower, lower_patch)) = floor_below(&separated, &floors, n) else {
                continue;
            };
            if lower.1 >= upper.1 || air_index.get(&lower) != Some(&region) {
                continue;
            }
            let a = root(&parent, upper_patch);
            let b = root(&parent, lower_patch);
            parent.insert(a.max(b), a.min(b));
        }
    }
    let mut result = Architecture {
        openings: openings.clone(),
        ..Architecture::default()
    };
    for (&patch_id, patch) in &patches.patches {
        let id = SpaceId(root(&parent, patch_id).0);
        let space = result.spaces.entry(id).or_insert_with(|| Space {
            id,
            floor_surfaces: BTreeMap::new(),
            roofs: BTreeMap::new(),
            boundary: BoundaryState::Closed,
            pending: false,
            volume: Volume::Unknown,
            placements: BTreeSet::new(),
            dependencies: BTreeSet::new(),
        });
        for &cell in &patch.floor {
            space.floor_surfaces.entry(cell.1).or_default().insert(cell);
            result.floors.insert(cell, id);
        }
        space.roofs.extend(&patch.roofs);
        space.dependencies.extend(&patch.dependencies);
        space.placements.extend(&patch.placements);
        for &placement in &patch.placements {
            result
                .placement_membership
                .entry(placement)
                .or_default()
                .insert(id);
        }
    }
    for space in result.spaces.values_mut() {
        let floor: BTreeSet<_> = space.floor().collect();
        let mut interior = BTreeSet::new();
        let mut unknown = false;
        let mut open = false;
        for (&cell, &roof) in &space.roofs {
            match roof {
                Roof::Unknown => unknown = true,
                Roof::OpenSky => open = true,
                Roof::Covered { ceiling } => {
                    for y in cell.1..ceiling.1 {
                        interior.insert(Cell(cell.0, y, cell.2));
                    }
                }
            }
        }
        for &cell in &floor {
            for n in cell.horizontal_neighbors() {
                space.dependencies.insert(n);
                if separated.partition(cell, n).is_some() || separated.voxel(n) == Voxel::Solid {
                    continue;
                }
                if floor_below(&separated, &floors, n).is_some_and(|(c, _)| floor.contains(&c)) {
                    continue;
                }
                space.boundary = match separated.voxel(n) {
                    Voxel::Unknown => BoundaryState::Unknown,
                    _ if space.boundary == BoundaryState::Unknown => BoundaryState::Unknown,
                    _ => BoundaryState::Open,
                };
            }
        }
        for &c in &interior {
            for n in c.horizontal_neighbors() {
                if interior.contains(&n) || separated.partition(c, n).is_some() {
                    continue;
                }
                space.dependencies.insert(n);
                match separated.voxel(n) {
                    Voxel::Air => open = true,
                    Voxel::Unknown => unknown = true,
                    Voxel::Solid => {}
                }
            }
        }
        space.pending = space.boundary == BoundaryState::Unknown
            || space.roofs.values().any(|roof| *roof == Roof::Unknown)
            || floor.iter().any(|cell| !air_index.contains_key(cell));
        if floor.iter().any(|cell| !air_index.contains_key(cell))
            && !space.roofs.values().any(|roof| *roof == Roof::OpenSky)
        {
            unknown = true;
        }
        space.volume = if unknown {
            Volume::Unknown
        } else if open {
            Volume::Open
        } else {
            Volume::Bounded(interior.len())
        };
        // Revalidation of multilevel grouping depends on the entire containing
        // air component, not just the successful connection between two floors.
        for cell in &floor {
            if let Some(&r) = air_index.get(cell) {
                space.dependencies.extend(&regions[r].cells);
            }
        }
    }
    for opening in openings {
        let ends = opening.approaches.each_ref().map(|cells| {
            cells
                .iter()
                .filter_map(|c| result.floors.get(c).copied())
                .collect()
        });
        result.connections.push(SpaceConnection {
            partition: Some(Partition::Portal { open: true }),
            faces: opening.faces.iter().copied().collect(),
            ends,
            threshold: opening.floor_cells.into_iter().collect(),
            volume: opening.volume_cells.clone(),
            traversable: true,
            evidence: opening.evidence,
            placements: authored
                .placements
                .values()
                .filter(|p| p.cells.iter().any(|c| opening.volume_cells.contains(c)))
                .map(|p| p.id)
                .collect(),
        });
    }
    // Group adjacent authored faces, preserving separate doors between the
    // same pair of rooms. A zero-thickness authored face has no threshold area.
    let mut authored_faces = BTreeMap::new();
    for (&face, &partition) in &authored.partitions {
        let (a, b) = face.cells();
        let (Some((_, pa)), Some((_, pb))) = (
            floor_below(&separated, &floors, a),
            floor_below(&separated, &floors, b),
        ) else {
            continue;
        };
        let ends = (SpaceId(root(&parent, pa).0), SpaceId(root(&parent, pb).0));
        if ends.0 != ends.1 {
            authored_faces.insert(face, (partition, ends));
        }
    }
    while let Some((&seed, &(partition, (a, b)))) = authored_faces.first_key_value() {
        authored_faces.remove(&seed);
        let mut faces = BTreeSet::from([seed]);
        let mut queue = VecDeque::from([seed]);
        while let Some(face) = queue.pop_front() {
            let neighbors: Vec<_> = authored_faces
                .iter()
                .filter(|(other, value)| {
                    **value == (partition, (a, b)) && adjacent_faces(face, **other)
                })
                .map(|(&face, _)| face)
                .collect();
            for next in neighbors {
                authored_faces.remove(&next);
                faces.insert(next);
                queue.push_back(next);
            }
        }
        let evidence = faces
            .iter()
            .flat_map(|face| {
                let (a, b) = face.cells();
                [a, b]
            })
            .collect();
        result.connections.push(SpaceConnection {
            partition: Some(partition),
            faces,
            ends: [BTreeSet::from([a]), BTreeSet::from([b])],
            threshold: BTreeSet::new(),
            volume: BTreeSet::new(),
            traversable: partition.traversable(),
            evidence,
            placements: BTreeSet::new(),
        });
    }
    for link in &authored.links {
        if !authored.step_is_clear(link.a, link.b) {
            continue;
        }
        let (Some(&a), Some(&b)) = (result.floors.get(&link.a), result.floors.get(&link.b)) else {
            continue;
        };
        if a == b {
            continue;
        }
        let (low, high) = if link.a.1 < link.b.1 {
            (link.a, link.b)
        } else {
            (link.b, link.a)
        };
        let faces: BTreeSet<_> = (0..2)
            .filter_map(|dy| Face::new(low.offset(0, dy + 1, 0), high.offset(0, dy, 0)).ok())
            .collect();
        let traversable = faces.iter().all(|face| {
            let (a, b) = face.cells();
            authored.partition(a, b).is_none_or(|p| p.traversable())
        });
        result.connections.push(SpaceConnection {
            partition: faces
                .iter()
                .any(|face| authored.partitions.contains_key(face))
                .then_some(Partition::Portal { open: traversable }),
            faces,
            ends: [BTreeSet::from([a]), BTreeSet::from([b])],
            threshold: BTreeSet::new(),
            volume: BTreeSet::new(),
            traversable,
            evidence: BTreeSet::from([link.a, link.b]),
            placements: BTreeSet::new(),
        });
    }
    result
}

fn adjacent_faces(a: Face, b: Face) -> bool {
    let (a0, a1) = a.cells();
    let (b0, b1) = b.cells();
    let dx = a1.0 - a0.0;
    let dz = a1.2 - a0.2;
    if dx != b1.0 - b0.0 || dz != b1.2 - b0.2 {
        return false;
    }
    let distance = a0.0.abs_diff(b0.0) + a0.1.abs_diff(b0.1) + a0.2.abs_diff(b0.2);
    distance == 1 && (dx == 0 || a0.0 == b0.0) && (dz == 0 || a0.2 == b0.2)
}
