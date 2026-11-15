use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{Cell, Face, Partition, PlacementId, Scene, Voxel};

/// Deterministic key for one rebuild, not a persistent room handle. Lineage and
/// durable space identity belong to the later production service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PatchId(pub Cell);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryState {
    Closed,
    Open,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Roof {
    Covered { ceiling: Cell },
    OpenSky,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub id: PatchId,
    pub floor: BTreeSet<Cell>,
    pub boundary: BoundaryState,
    pub roofs: BTreeMap<Cell, Roof>,
    /// Architectural volume, measured exactly from roofed columns. Open portals
    /// can bound it, but an unpartitioned gap below a floating roof cannot.
    pub interior_volume: Option<usize>,
    /// Known lateral exposure; None if any inspected side is unknown.
    pub exposed_side_faces: Option<usize>,
    pub placements: BTreeSet<PlacementId>,
    /// Exact queried cells, including unsuccessful roof and support probes.
    /// This prototype rebuilds globally; these are evidence for future invalidation.
    pub dependencies: BTreeSet<Cell>,
}

impl Patch {
    pub fn structural_area(&self) -> usize {
        self.floor.len()
    }

    /// Unknown columns do not count as unroofed: the fraction is unavailable.
    pub fn roof_fraction(&self) -> Option<f64> {
        if self.roofs.values().any(|r| *r == Roof::Unknown) {
            return None;
        }
        let covered = self
            .roofs
            .values()
            .filter(|r| matches!(r, Roof::Covered { .. }))
            .count();
        Some(covered as f64 / self.floor.len() as f64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub a: PatchId,
    pub b: PatchId,
    /// None denotes an explicit link between elevations.
    pub partition: Option<Partition>,
    /// Physical doorway/window face for diagnostics; step links have no face.
    pub face: Option<Face>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detection {
    pub patches: BTreeMap<PatchId, Patch>,
    pub connections: Vec<Connection>,
    /// Placements may straddle spaces. This is membership, NOT duplicated capacity.
    pub placement_membership: BTreeMap<PlacementId, BTreeSet<PatchId>>,
    floor_index: BTreeMap<Cell, PatchId>,
}

impl Detection {
    pub fn at_floor(&self, cell: Cell) -> Option<&Patch> {
        self.floor_index
            .get(&cell)
            .and_then(|id| self.patches.get(id))
    }

    pub fn tagged_placements(
        &self,
        scene: &Scene,
        patch: PatchId,
        tag: &str,
    ) -> BTreeSet<PlacementId> {
        let Some(patch) = self.patches.get(&patch) else {
            return BTreeSet::new();
        };
        patch
            .placements
            .iter()
            .copied()
            .filter(|id| scene.placements[id].tags.contains(tag))
            .collect()
    }

    /// Ordered pairs of distinct placements with horizontally touching footprint
    /// cells belonging to this patch. Partitioned faces do not qualify.
    pub fn adjacent_placements(
        &self,
        scene: &Scene,
        patch: PatchId,
        tag_a: &str,
        tag_b: &str,
    ) -> BTreeSet<(PlacementId, PlacementId)> {
        let aa = self.tagged_placements(scene, patch, tag_a);
        let bb = self.tagged_placements(scene, patch, tag_b);
        let mut pairs = BTreeSet::new();
        for a in aa {
            for &b in &bb {
                if a == b {
                    continue;
                }
                let touches = scene.placements[&a].cells.iter().any(|&cell| {
                    owner_at(self, scene, cell) == Some(patch)
                        && cell.horizontal_neighbors().into_iter().any(|neighbor| {
                            scene.placements[&b].cells.contains(&neighbor)
                                && owner_at(self, scene, neighbor) == Some(patch)
                                && scene.partition(cell, neighbor).is_none()
                        })
                });
                if touches {
                    pairs.insert((a, b));
                }
            }
        }
        pairs
    }

    /// Count A placements, not contact cells and not the number of pair edges.
    pub fn adjacent_count(&self, scene: &Scene, patch: PatchId, a: &str, b: &str) -> usize {
        self.adjacent_placements(scene, patch, a, b)
            .into_iter()
            .map(|(a, _)| a)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

fn read(scene: &Scene, dependencies: &mut BTreeSet<Cell>, cell: Cell) -> Voxel {
    dependencies.insert(cell);
    scene.voxel(cell)
}

/// Full rebuild over a finite structural world. No size cutoff classifies a patch
/// as outdoors. Furnishings enter only after topology and measurements are complete.
pub fn detect(scene: &Scene) -> Detection {
    let mut floors = BTreeSet::new();
    for c in scene.bounds.cells() {
        if scene.voxel(c) == Voxel::Air && scene.voxel(c.offset(0, -1, 0)) == Voxel::Solid {
            floors.insert(c);
        }
    }
    let mut result = Detection::default();
    let mut visited = BTreeSet::new();
    for &seed in &floors {
        if !visited.insert(seed) {
            continue;
        }
        let mut floor = BTreeSet::new();
        let mut dependencies = BTreeSet::new();
        let mut queue = VecDeque::from([seed]);
        let mut open = false;
        let mut unknown = false;
        while let Some(c) = queue.pop_front() {
            floor.insert(c);
            read(scene, &mut dependencies, c);
            read(scene, &mut dependencies, c.offset(0, -1, 0));
            for n in c.horizontal_neighbors() {
                let here = read(scene, &mut dependencies, n);
                if scene.partition(c, n).is_some() || here == Voxel::Solid {
                    continue;
                }
                if here == Voxel::Unknown {
                    unknown = true;
                    continue;
                }
                let support = read(scene, &mut dependencies, n.offset(0, -1, 0));
                match support {
                    Voxel::Unknown => unknown = true,
                    Voxel::Air => open = true,
                    Voxel::Solid => {
                        if visited.insert(n) {
                            queue.push_back(n);
                        }
                    }
                }
            }
        }
        let id = PatchId(*floor.first().expect("a flood includes its seed"));
        let roofs = floor
            .iter()
            .map(|&c| (c, probe_roof(scene, c, &mut dependencies)))
            .collect();
        let mut patch = Patch {
            id,
            floor,
            roofs,
            boundary: if unknown {
                BoundaryState::Unknown
            } else if open {
                BoundaryState::Open
            } else {
                BoundaryState::Closed
            },
            interior_volume: None,
            exposed_side_faces: None,
            placements: BTreeSet::new(),
            dependencies,
        };
        measure_volume(scene, &mut patch);
        for &c in &patch.floor {
            result.floor_index.insert(c, id);
        }
        result.patches.insert(id, patch);
    }
    for (&face, &partition) in &scene.partitions {
        let (a, b) = face.cells();
        if let (Some(&a), Some(&b)) = (result.floor_index.get(&a), result.floor_index.get(&b))
            && a != b
        {
            result.connections.push(Connection {
                a,
                b,
                partition: Some(partition),
                face: Some(face),
            });
        }
    }
    for link in &scene.links {
        if !scene.step_is_clear(link.a, link.b) {
            continue;
        }
        // Links can become stale after a structural edit; only current floors
        // produce connections. Never retain an endpoint based on old scene state.
        if let (Some(&a), Some(&b)) = (
            result.floor_index.get(&link.a),
            result.floor_index.get(&link.b),
        ) && a != b
        {
            result.connections.push(Connection {
                a,
                b,
                partition: None,
                face: None,
            });
        }
    }
    for placement in scene.placements.values() {
        let membership: BTreeSet<_> = placement
            .cells
            .iter()
            .filter_map(|&c| owner_at(&result, scene, c))
            .collect();
        for id in &membership {
            result
                .patches
                .get_mut(id)
                .expect("owner comes from floor index")
                .placements
                .insert(placement.id);
        }
        result.placement_membership.insert(placement.id, membership);
    }
    result
}

fn owner_at(result: &Detection, scene: &Scene, cell: Cell) -> Option<PatchId> {
    // Follow the column to its actual support. Furniture is transparent to this
    // query; floors/ceilings and unknown cells are not. No bbox membership guesses.
    for y in (scene.bounds.min.1..=cell.1).rev() {
        let c = Cell(cell.0, y, cell.2);
        if scene.voxel(c) != Voxel::Air {
            return None;
        }
        if let Some(id) = result.floor_index.get(&c) {
            return Some(*id);
        }
    }
    None
}

fn probe_roof(scene: &Scene, floor: Cell, dependencies: &mut BTreeSet<Cell>) -> Roof {
    for y in floor.1 + 1..=scene.bounds.max.1 {
        let c = Cell(floor.0, y, floor.2);
        match read(scene, dependencies, c) {
            Voxel::Solid => return Roof::Covered { ceiling: c },
            Voxel::Unknown => return Roof::Unknown,
            Voxel::Air => {}
        }
    }
    // Include the failed boundary probe even when an explicit sky observation
    // resolves it. Dependency tracking must not only record successful roof hits.
    dependencies.insert(Cell(floor.0, scene.bounds.max.1 + 1, floor.2));
    if scene.sky_columns.contains(&(floor.0, floor.2)) {
        Roof::OpenSky
    } else {
        Roof::Unknown
    }
}

fn measure_volume(scene: &Scene, patch: &mut Patch) {
    if patch
        .roofs
        .values()
        .any(|r| !matches!(r, Roof::Covered { .. }))
    {
        return;
    }
    let mut interior = BTreeSet::new();
    for (&floor, &roof) in &patch.roofs {
        let Roof::Covered { ceiling } = roof else {
            unreachable!()
        };
        for y in floor.1..ceiling.1 {
            interior.insert(Cell(floor.0, y, floor.2));
        }
    }
    let mut bounded = patch.boundary == BoundaryState::Closed;
    let mut unknown = false;
    let mut exposed = 0;
    for &c in &interior {
        for n in c.horizontal_neighbors() {
            if interior.contains(&n) {
                continue;
            }
            match read(scene, &mut patch.dependencies, n) {
                Voxel::Solid => {}
                Voxel::Unknown => {
                    unknown = true;
                    bounded = false;
                }
                Voxel::Air => match scene.partition(c, n) {
                    Some(partition) => {
                        if !partition.shelters() {
                            exposed += 1;
                        }
                    }
                    None => {
                        exposed += 1;
                        bounded = false;
                    }
                },
            }
        }
    }
    patch.exposed_side_faces = (!unknown).then_some(exposed);
    patch.interior_volume = bounded.then_some(interior.len());
}
