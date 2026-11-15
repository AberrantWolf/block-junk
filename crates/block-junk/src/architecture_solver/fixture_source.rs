//! Test-only bridge. Production never calls the finite reference detector.
use super::*;
pub struct FixtureSource {
    scene: block_junk_spaces::Scene,
    partitions: BTreeMap<Face, Partition>,
    placements: Vec<Placement>,
    stairs: Vec<(Cell, Cell)>,
}
fn cell(c: block_junk_spaces::Cell) -> Cell {
    Cell(c.0, c.1, c.2)
}
impl FixtureSource {
    pub fn new(scene: block_junk_spaces::Scene) -> Self {
        let partitions = scene
            .authored_partitions()
            .map(|(face, partition)| {
                let (a, b) = face.cells();
                (
                    Face::new(cell(a), cell(b)),
                    match partition {
                        block_junk_spaces::Partition::Wall => Partition::Wall,
                        block_junk_spaces::Partition::Window => Partition::Window,
                        block_junk_spaces::Partition::Portal { open } => Partition::Portal { open },
                    },
                )
            })
            .collect();
        let placements = scene
            .placements()
            .map(|p| Placement {
                id: p.id.0,
                cells: p.cells.iter().copied().map(cell).collect(),
            })
            .collect();
        let stairs = scene
            .authored_links()
            .map(|link| (cell(link.a), cell(link.b)))
            .collect();
        Self {
            scene,
            partitions,
            placements,
            stairs,
        }
    }
}
impl GeometrySource for FixtureSource {
    fn cells(&self) -> Box<dyn Iterator<Item = Cell> + Send + '_> {
        Box::new(self.scene.bounds().cells().map(cell))
    }
    fn material(&self, c: Cell) -> Option<Material> {
        match self.scene.voxel(block_junk_spaces::Cell(c.0, c.1, c.2)) {
            block_junk_spaces::Voxel::Unknown => None,
            block_junk_spaces::Voxel::Air => Some(Material::default()),
            block_junk_spaces::Voxel::Solid => Some(Material {
                occupied: true,
                supports: true,
                shelters: true,
            }),
        }
    }
    fn sky_above(&self, c: Cell) -> bool {
        c.1 > self.scene.bounds().max.1 && self.scene.known_sky(c.0, c.2)
    }
    fn partitions(&self) -> &BTreeMap<Face, Partition> {
        &self.partitions
    }
    fn portals(&self) -> &[AuthoredPortal] {
        &[]
    }
    fn placements(&self) -> &[Placement] {
        &self.placements
    }
    fn stairs(&self) -> &[(Cell, Cell)] {
        &self.stairs
    }
}
#[test]
fn resumed_opening_recognition_matches_all_finite_contract_fixtures() {
    for fixture in block_junk_spaces::fixtures::contract_cases() {
        let expected = block_junk_spaces::openings::infer_openings(&fixture.scene);
        let source = Arc::new(FixtureSource::new(fixture.scene));
        let budget = Arc::new(Budget::default());
        let view = View {
            source,
            budget: budget.clone(),
            dependencies: Arc::default(),
        };
        let mut task = Computation::new(budget, async move { openings::infer(&view).await });
        let result = loop {
            if let Poll::Ready(result) = task.advance(127) {
                break result;
            }
        };
        let actual: BTreeSet<_> = result
            .iter()
            .map(|opening| opening.floor.iter().copied().collect::<Vec<_>>())
            .collect();
        let expected: BTreeSet<_> = expected
            .iter()
            .map(|opening| {
                opening
                    .floor_cells
                    .iter()
                    .copied()
                    .map(cell)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(actual, expected, "{}", fixture.name);
    }
}

#[test]
fn resumable_topology_matches_exact_reference_membership_and_measurements() {
    for fixture in block_junk_spaces::fixtures::contract_cases() {
        for turns in 0..4 {
            let scene =
                block_junk_spaces::fixtures::permuted(&fixture.scene, turns, turns % 2 == 1);
            let expected = block_junk_spaces::detect_architecture(&scene);
            let source = Arc::new(FixtureSource::new(scene));
            let budget = Arc::new(Budget::default());
            let view = View {
                source,
                budget: budget.clone(),
                dependencies: Arc::default(),
            };
            let mut task = Computation::new(budget, async move { topology::detect(&view).await });
            let result = loop {
                if let Poll::Ready(result) = task.advance(127) {
                    break result;
                }
            };
            let actual: BTreeSet<_> = result
                .spaces
                .values()
                .map(|space| space.floor.iter().copied().collect::<Vec<_>>())
                .collect();
            let wanted: BTreeSet<_> = expected
                .spaces
                .values()
                .map(|space| {
                    space
                        .floor()
                        .map(cell)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                })
                .collect();
            assert_eq!(
                actual, wanted,
                "{} rotation {} membership",
                fixture.name, turns
            );
            for space in expected.spaces.values() {
                let key = cell(space.id.0);
                let actual = &result.spaces[&key];
                let volume = match space.volume {
                    block_junk_spaces::Volume::Bounded(n) => topology::Volume::Bounded(n as u64),
                    block_junk_spaces::Volume::Open => topology::Volume::Open,
                    block_junk_spaces::Volume::Unknown => topology::Volume::Unknown,
                };
                assert_eq!(
                    actual.volume, volume,
                    "{} rotation {} volume at {:?}",
                    fixture.name, turns, key
                );
                assert_eq!(
                    actual.pending, space.pending,
                    "{} pending at {:?}",
                    fixture.name, key
                );
                assert_eq!(
                    actual.placements,
                    space.placements.iter().map(|id| id.0).collect(),
                    "{} furnishing membership",
                    fixture.name
                );
            }
            assert_eq!(
                result.connections.len(),
                expected.connections.len(),
                "{} rotation {} connections",
                fixture.name,
                turns
            );
        }
    }
}
