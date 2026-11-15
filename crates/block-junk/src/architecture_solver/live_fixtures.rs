//! Contract scenes translated into actual chunks, authored block metadata and
//! placement sidecars. Only the nominated architectural space is compared: the
//! finite oracle's artificial observation perimeter is not a live room boundary.
use super::*;
use crate::{
    blocks::{BlockRegistry, BlockSlot},
    placements::Placements,
    protocol::ChunkCoord,
    spatial_geometry::StructuralSummaries,
    voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind, world_to_chunk},
};
use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockId, Cardinal};
use serde_json::{Value, json};
fn point(cell: block_junk_spaces::Cell) -> IVec3 {
    IVec3::new(cell.0 + 16, cell.1, cell.2 + 16)
}
fn definition(id: String, footprint: Vec<[i32; 3]>, solid: bool) -> Value {
    let occupied = if solid { footprint.clone() } else { Vec::new() };
    json!({"id":id,"display_name":"Fixture", "color":[1,1,1], "flags":{},"footprint":footprint,
        "structure":{"solid_cells":occupied,"supports":occupied,"shelter_cells":occupied,"partitions":[],"portals":[],"stairs":[]}})
}
fn push_array(def: &mut Value, key: &str, value: Value) {
    def["structure"][key].as_array_mut().unwrap().push(value);
}
fn translate(scene: &block_junk_spaces::Scene, reverse: bool) -> (App, BTreeMap<u64, u64>) {
    let mut authors = BTreeMap::<[i32; 3], Value>::new();
    for (face, partition) in scene.authored_partitions() {
        let (a, b) = face.cells();
        let anchor = point(a);
        let normal = (point(b) - anchor).to_array();
        let def = authors.entry(anchor.to_array()).or_insert_with(|| {
            definition(
                format!("fixture:author_{}_{}_{}", anchor.x, anchor.y, anchor.z),
                vec![[0, 0, 0]],
                scene.voxel(a) == block_junk_spaces::Voxel::Solid,
            )
        });
        let face = json!({"cell":[0,0,0],"normal":normal,"window":partition==block_junk_spaces::Partition::Window});
        match partition {
            block_junk_spaces::Partition::Portal { open } => {
                let key = format!(
                    "door_{}",
                    def["structure"]["portals"].as_array().unwrap().len()
                );
                push_array(
                    def,
                    "portals",
                    json!({"key":key,"faces":[face],"threshold":[],"volume":[],"initially_open":open}),
                );
            }
            _ => push_array(def, "partitions", face),
        }
    }
    for link in scene.authored_links() {
        let anchor = point(link.a);
        let def = authors.entry(anchor.to_array()).or_insert_with(|| {
            definition(
                format!("fixture:author_{}_{}_{}", anchor.x, anchor.y, anchor.z),
                vec![[0, 0, 0]],
                scene.voxel(link.a) == block_junk_spaces::Voxel::Solid,
            )
        });
        push_array(
            def,
            "stairs",
            json!({"from":[0,0,0],"to":(point(link.b)-anchor).to_array()}),
        );
    }
    let mut furniture = BTreeMap::new();
    for placement in scene.placements() {
        let anchor = point(*placement.cells.first().unwrap());
        assert!(!authors.contains_key(&anchor.to_array()));
        let mut def = definition(
            format!("fixture:furnishing_{}", placement.id.0),
            placement
                .cells
                .iter()
                .map(|cell| (point(*cell) - anchor).to_array())
                .collect(),
            false,
        );
        def["mesh"] = json!("fixture.gltf");
        authors.insert(anchor.to_array(), def);
        furniture.insert(anchor.to_array(), placement.id.0);
    }
    let mut definitions = vec![
        definition("fixture:stone".into(), vec![[0, 0, 0]], true),
        definition("fixture:unknown".into(), vec![[0, 0, 0], [0, 32, 0]], true),
    ];
    definitions.extend(authors.values().cloned());
    let (registry, _) =
        BlockRegistry::build(serde_json::from_value(Value::Array(definitions)).unwrap()).unwrap();
    let stone = registry.slot_of(&BlockId("fixture:stone".into())).unwrap();
    let unknown = registry
        .slot_of(&BlockId("fixture:unknown".into()))
        .unwrap();
    let mut chunks = BTreeMap::<[i32; 3], (Chunk, ChunkEntities)>::new();
    for cell in scene.bounds().cells() {
        let world = point(cell);
        let (coord, local) = world_to_chunk(world);
        let (chunk, sidecar) = chunks.entry(coord.0.to_array()).or_insert_with(|| {
            (
                Chunk {
                    revision: 0,
                    blocks: vec![BlockSlot::EMPTY; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
                },
                ChunkEntities::default(),
            )
        });
        match scene.voxel(cell) {
            block_junk_spaces::Voxel::Solid => {
                chunk.set(local, stone);
            }
            block_junk_spaces::Voxel::Unknown => {
                // A real unresolved cross-chunk architectural placement. Its
                // anchor is below the observed domain, in an unavailable chunk.
                chunk.set(local, unknown);
                sidecar.insert(
                    world,
                    EntryKind::Ghost {
                        anchor: world - IVec3::Y * 32,
                    },
                );
            }
            _ => {}
        }
    }
    for (anchor, def) in &authors {
        let anchor = IVec3::from_array(*anchor);
        let slot = registry
            .slot_of(&BlockId(def["id"].as_str().unwrap().into()))
            .unwrap();
        for offset in &registry.def(slot).footprint {
            let world = anchor + IVec3::from_array(*offset);
            let (coord, local) = world_to_chunk(world);
            let (chunk, sidecar) = chunks.get_mut(&coord.0.to_array()).unwrap();
            chunk.set(local, slot);
            sidecar.insert(
                world,
                if world == anchor {
                    EntryKind::Anchor {
                        orientation: Cardinal::default(),
                        portal_open: None,
                    }
                } else {
                    EntryKind::Ghost { anchor }
                },
            );
        }
    }
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(registry)
        .init_resource::<ChunkMap>()
        .init_resource::<Placements>()
        .init_resource::<StructuralSummaries>()
        .init_resource::<crate::server::SpatialSubscriptions>()
        .init_resource::<crate::server::ActiveSimulationChunks>()
        .add_systems(
            Update,
            (
                crate::spatial_geometry::refresh_structural_summaries,
                crate::spatial_geometry::materialize_facilities,
            )
                .chain(),
        );
    let mut entries: Vec<_> = chunks.into_iter().collect();
    if reverse {
        entries.reverse();
    }
    for (coord, (chunk, sidecar)) in entries {
        let coord = ChunkCoord(IVec3::from_array(coord));
        let entity = app.world_mut().spawn((chunk, sidecar)).id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(coord, entity);
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(coord);
    }
    for _ in 0..512 {
        app.update();
        if app
            .world()
            .resource::<StructuralSummaries>()
            .pending
            .is_empty()
        {
            break;
        }
    }
    assert!(
        app.world()
            .resource::<StructuralSummaries>()
            .pending
            .is_empty()
    );
    let ids = furniture
        .into_iter()
        .map(|(anchor, id)| {
            (
                app.world()
                    .resource::<Placements>()
                    .at(IVec3::from_array(anchor))
                    .unwrap()
                    .id
                    .0,
                id,
            )
        })
        .collect();
    (app, ids)
}
#[test]
fn contract_spaces_match_through_real_chunks_and_authored_metadata() {
    for fixture in block_junk_spaces::fixtures::contract_cases() {
        for turns in 0..4 {
            for reverse in [false, true] {
                let scene = block_junk_spaces::fixtures::permuted(&fixture.scene, turns, reverse);
                let sample = match turns {
                    0 => fixture.sample,
                    1 => block_junk_spaces::Cell(
                        -fixture.sample.2,
                        fixture.sample.1,
                        fixture.sample.0,
                    ),
                    2 => block_junk_spaces::Cell(
                        -fixture.sample.0,
                        fixture.sample.1,
                        -fixture.sample.2,
                    ),
                    _ => block_junk_spaces::Cell(
                        fixture.sample.2,
                        fixture.sample.1,
                        -fixture.sample.0,
                    ),
                };
                let reference = block_junk_spaces::detect_architecture(&scene);
                let expected = reference
                    .spaces
                    .values()
                    .find(|space| space.floor().any(|cell| cell == sample))
                    .unwrap();
                let (app, ids) = translate(&scene, reverse);
                let summaries = app.world().resource::<StructuralSummaries>();
                let ceilings = summaries
                    .completed
                    .keys()
                    .filter(|coord| {
                        scene
                            .bounds()
                            .cells()
                            .filter(|cell| {
                                point(*cell).x.div_euclid(32) == coord[0]
                                    && point(*cell).z.div_euclid(32) == coord[2]
                            })
                            .all(|cell| scene.known_sky(cell.0, cell.2))
                    })
                    .map(|coord| ([coord[0], coord[2]], scene.bounds().max.1))
                    .collect();
                let source = live_source::LiveSource::capture(
                    summaries.completed.clone(),
                    ceilings,
                    app.world().resource::<BlockRegistry>(),
                    app.world().resource::<Placements>(),
                );
                let budget = Arc::new(Budget::default());
                let view = View {
                    source: Arc::new(source),
                    budget: budget.clone(),
                    dependencies: Arc::default(),
                };
                let mut task =
                    Computation::new(budget, async move { topology::detect(&view).await });
                let result = loop {
                    if let Poll::Ready(result) = task.advance(8192) {
                        break result;
                    }
                };
                let sample = point(sample);
                let key = result
                    .floor_index
                    .get(&Cell(sample.x, sample.y, sample.z))
                    .unwrap_or_else(|| panic!("{} rotation {turns}: missing sample", fixture.name));
                let actual = &result.spaces[key];
                let wanted: BTreeSet<_> = expected
                    .floor()
                    .map(|cell| {
                        let p = point(cell);
                        Cell(p.x, p.y, p.z)
                    })
                    .collect();
                assert_eq!(
                    actual.floor, wanted,
                    "{} rotation {turns} order {reverse}: exact membership",
                    fixture.name
                );
                let volume = match fixture.volume {
                    block_junk_spaces::Volume::Bounded(n) => topology::Volume::Bounded(n as u64),
                    block_junk_spaces::Volume::Open => topology::Volume::Open,
                    block_junk_spaces::Volume::Unknown => topology::Volume::Unknown,
                };
                assert_eq!(actual.floor.len(), fixture.area, "{} area", fixture.name);
                assert_eq!(actual.volume, volume, "{} volume", fixture.name);
                assert_eq!(actual.pending, expected.pending, "{} pending", fixture.name);
                let convert = |cell: block_junk_spaces::Cell| {
                    let p = point(cell);
                    Cell(p.x, p.y, p.z)
                };
                for (cell, roof) in &expected.roofs {
                    let wanted = match roof {
                        block_junk_spaces::Roof::Covered { ceiling } => {
                            topology::Ceiling::At(convert(*ceiling))
                        }
                        block_junk_spaces::Roof::OpenSky => topology::Ceiling::Sky,
                        block_junk_spaces::Roof::Unknown => topology::Ceiling::Unknown,
                    };
                    assert_eq!(
                        actual.roofs[&convert(*cell)],
                        wanted,
                        "{} roof coverage",
                        fixture.name
                    );
                }
                let wanted_connections: BTreeSet<_> = reference
                    .connections
                    .iter()
                    .filter(|connection| {
                        connection
                            .ends
                            .iter()
                            .any(|ends| ends.contains(&expected.id))
                    })
                    .map(|connection| {
                        (
                            connection
                                .faces
                                .iter()
                                .map(|face| {
                                    let (a, b) = face.cells();
                                    Face::new(convert(a), convert(b))
                                })
                                .collect::<BTreeSet<_>>(),
                            connection
                                .threshold
                                .iter()
                                .copied()
                                .map(convert)
                                .collect::<BTreeSet<_>>(),
                            connection
                                .volume
                                .iter()
                                .copied()
                                .map(convert)
                                .collect::<BTreeSet<_>>(),
                            connection
                                .evidence
                                .iter()
                                .copied()
                                .map(convert)
                                .collect::<BTreeSet<_>>(),
                            connection.traversable,
                        )
                    })
                    .collect();
                let actual_connections: BTreeSet<_> = result
                    .connections
                    .iter()
                    .filter(|connection| connection.ends.iter().any(|ends| ends.contains(key)))
                    .map(|connection| {
                        (
                            connection.faces.clone(),
                            connection.threshold.clone(),
                            connection.volume.clone(),
                            connection.evidence.clone(),
                            connection.traversable,
                        )
                    })
                    .collect();
                assert_eq!(
                    actual_connections, wanted_connections,
                    "{} connection geometry and evidence",
                    fixture.name
                );
                let furnishings: BTreeSet<_> = actual
                    .placements
                    .iter()
                    .filter_map(|id| ids.get(id).copied())
                    .collect();
                assert_eq!(
                    furnishings,
                    expected.placements.iter().map(|id| id.0).collect(),
                    "{} furnishings",
                    fixture.name
                );
            }
        }
    }
}

struct Uncached(live_source::LiveSource);
impl GeometrySource for Uncached {
    fn cells(&self) -> Box<dyn Iterator<Item = Cell> + Send + '_> {
        self.0.cells()
    }
    fn material(&self, cell: Cell) -> Option<super::Material> {
        self.0.material(cell)
    }
    fn sky_above(&self, cell: Cell) -> bool {
        self.0.sky_above(cell)
    }
    fn partitions(&self) -> &BTreeMap<Face, Partition> {
        self.0.partitions()
    }
    fn portals(&self) -> &[AuthoredPortal] {
        self.0.portals()
    }
    fn placements(&self) -> &[Placement] {
        self.0.placements()
    }
    fn stairs(&self) -> &[(Cell, Cell)] {
        self.0.stairs()
    }
}
fn settle_source(source: Arc<dyn GeometrySource>) -> topology::Topology {
    let budget = Arc::new(Budget::default());
    let view = View {
        source,
        budget: budget.clone(),
        dependencies: Arc::default(),
    };
    let mut task = Computation::new(budget, async move { topology::detect(&view).await });
    loop {
        let before = task.operations;
        let outcome = task.advance(1009);
        assert!(task.operations - before <= 1009);
        if let Poll::Ready(result) = outcome {
            return result;
        }
    }
}
#[test]
fn cached_floor_and_air_sections_match_full_rebuilds_after_random_edits() {
    let scene = block_junk_spaces::fixtures::profiled_opening(&[2, 3, 4, 3, 2], 2);
    let (mut app, _) = translate(&scene, false);
    let extra = ChunkCoord(IVec3::X);
    let entity = app
        .world_mut()
        .spawn((
            Chunk {
                revision: 0,
                blocks: vec![BlockSlot::EMPTY; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
            },
            ChunkEntities::default(),
        ))
        .id();
    app.world_mut()
        .resource_mut::<ChunkMap>()
        .0
        .insert(extra, entity);
    app.world_mut()
        .resource_mut::<crate::server::ActiveSimulationChunks>()
        .0
        .insert(extra);
    let cache = Arc::new(Mutex::new(local_topology::Cache::default()));
    let stone = app
        .world()
        .resource::<BlockRegistry>()
        .slot_of(&BlockId("fixture:stone".into()))
        .unwrap();
    let mut random = 91_u64;
    for iteration in 0..16 {
        if iteration > 0 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let cell = point(block_junk_spaces::Cell(
                1 + (random % 8) as i32,
                if iteration % 2 == 0 { 5 } else { 2 },
                1 + ((random >> 8) % 5) as i32,
            ));
            let (coord, local) = world_to_chunk(cell);
            let entity = app.world().resource::<ChunkMap>().0[&coord];
            let mut chunk = app.world_mut().get_mut::<Chunk>(entity).unwrap();
            let slot = if chunk.get(local).is_empty() {
                stone
            } else {
                BlockSlot::EMPTY
            };
            chunk.set(local, slot);
        }
        for _ in 0..128 {
            app.update();
            if app
                .world()
                .resource::<StructuralSummaries>()
                .pending
                .is_empty()
            {
                break;
            }
        }
        let capture = || {
            live_source::LiveSource::capture(
                app.world()
                    .resource::<StructuralSummaries>()
                    .completed
                    .clone(),
                BTreeMap::from([([0, 0], 31), ([1, 0], 31)]),
                app.world().resource::<BlockRegistry>(),
                app.world().resource::<Placements>(),
            )
        };
        let mut source = capture();
        source.cache = cache.clone();
        let cached = settle_source(Arc::new(source));
        let rebuilt = settle_source(Arc::new(Uncached(capture())));
        assert_eq!(cached.spaces, rebuilt.spaces, "edit {iteration}: geometry");
        assert_eq!(
            cached.connections, rebuilt.connections,
            "edit {iteration}: connections"
        );
        assert_eq!(
            cached.openings, rebuilt.openings,
            "edit {iteration}: openings"
        );
    }
    let cache = cache.lock().unwrap();
    assert!(
        cache.hits >= 15,
        "unchanged neighboring chunk must survive edits"
    );
    assert!(
        cache.rebuilds >= 16,
        "edited chunks must invalidate local topology"
    );
}
