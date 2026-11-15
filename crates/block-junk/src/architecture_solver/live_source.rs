//! Immutable solver input assembled from authoritative structural summaries.
use super::*;
use crate::{blocks::BlockRegistry, placements::Placements, spatial_geometry::ChunkGeometry};
use bevy::prelude::IVec3;

pub struct LiveSource {
    pub cache: Arc<Mutex<local_topology::Cache>>,
    pub chunks: BTreeMap<[i32; 3], Arc<ChunkGeometry>>,
    pub sky_ceiling: BTreeMap<[i32; 2], i32>,
    partitions: BTreeMap<Face, Partition>,
    portals: Vec<AuthoredPortal>,
    placements: Vec<Placement>,
    stairs: Vec<(Cell, Cell)>,
}
fn translated(anchor: [i32; 3], offset: [i32; 3]) -> Cell {
    Cell(
        anchor[0] + offset[0],
        anchor[1] + offset[1],
        anchor[2] + offset[2],
    )
}
impl LiveSource {
    pub fn capture(
        chunks: BTreeMap<[i32; 3], Arc<ChunkGeometry>>,
        sky_ceiling: BTreeMap<[i32; 2], i32>,
        blocks: &BlockRegistry,
        placements: &Placements,
    ) -> Self {
        let mut source = Self {
            cache: Arc::default(),
            chunks,
            sky_ceiling,
            partitions: BTreeMap::new(),
            portals: Vec::new(),
            placements: Vec::new(),
            stairs: Vec::new(),
        };
        let mut elements = BTreeMap::new();
        for chunk in source.chunks.values() {
            for (anchor, element) in &chunk.elements {
                elements.insert(*anchor, element.clone());
            }
        }
        for (anchor, element) in elements {
            let geometry =
                &blocks.architecture[usize::from(element.slot.0)][element.orientation as usize];
            for face in &geometry.structure.partitions {
                let a = translated(anchor, face.cell);
                let b = a.offset(face.normal[0], face.normal[1], face.normal[2]);
                source.partitions.insert(
                    Face::new(a, b),
                    if face.window {
                        Partition::Window
                    } else {
                        Partition::Wall
                    },
                );
            }
            let record = placements.at(IVec3::from_array(anchor));
            let authored: serde_json::Value = record
                .and_then(|record| serde_json::from_str(&record.authored_state).ok())
                .unwrap_or_default();
            for portal in &geometry.structure.portals {
                let open = authored
                    .get("portals")
                    .and_then(|states| states.get(&portal.key))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(portal.initially_open);
                let faces: BTreeSet<_> = portal
                    .faces
                    .iter()
                    .map(|face| {
                        let a = translated(anchor, face.cell);
                        Face::new(a, a.offset(face.normal[0], face.normal[1], face.normal[2]))
                    })
                    .collect();
                for &face in &faces {
                    source.partitions.insert(face, Partition::Portal { open });
                }
                source.portals.push(AuthoredPortal {
                    faces,
                    threshold: portal
                        .threshold
                        .iter()
                        .map(|offset| translated(anchor, *offset))
                        .collect(),
                    volume: portal
                        .volume
                        .iter()
                        .map(|offset| translated(anchor, *offset))
                        .collect(),
                    open,
                });
            }
            for stair in &geometry.structure.stairs {
                source
                    .stairs
                    .push((translated(anchor, stair.from), translated(anchor, stair.to)));
            }
            if let Some(record) = record {
                source.placements.push(Placement {
                    id: record.id.0,
                    cells: blocks
                        .def(element.slot)
                        .footprint
                        .iter()
                        .map(|offset| {
                            translated(anchor, element.orientation.rotate_offset(*offset))
                        })
                        .collect(),
                });
            }
        }
        source
    }
}
impl GeometrySource for LiveSource {
    fn topology_cache(&self) -> Option<&Arc<Mutex<local_topology::Cache>>> {
        Some(&self.cache)
    }
    fn chunk_fingerprints(&self) -> Option<BTreeMap<[i32; 3], [u8; 32]>> {
        Some(
            self.chunks
                .iter()
                .map(|(coord, summary)| {
                    let mut hash = blake3::Hasher::new();
                    bincode::serde::encode_into_std_write(
                        &summary.dependencies,
                        &mut hash,
                        bincode::config::standard(),
                    )
                    .unwrap();
                    bincode::serde::encode_into_std_write(
                        self.sky_ceiling.get(&[coord[0], coord[2]]),
                        &mut hash,
                        bincode::config::standard(),
                    )
                    .unwrap();
                    let chunk_of = |cell: Cell| {
                        [
                            cell.0.div_euclid(crate::protocol::CHUNK_SIZE as i32),
                            cell.1.div_euclid(crate::protocol::CHUNK_SIZE as i32),
                            cell.2.div_euclid(crate::protocol::CHUNK_SIZE as i32),
                        ]
                    };
                    for (face, partition) in &self.partitions {
                        if chunk_of(face.0) == *coord || chunk_of(face.1) == *coord {
                            let kind = match partition {
                                Partition::Wall => 0_u8,
                                Partition::Window => 1,
                                Partition::Portal { open: false } => 2,
                                Partition::Portal { open: true } => 3,
                            };
                            bincode::serde::encode_into_std_write(
                                (0_u8, face, kind),
                                &mut hash,
                                bincode::config::standard(),
                            )
                            .unwrap();
                        }
                    }
                    for portal in &self.portals {
                        for cell in &portal.volume {
                            if chunk_of(*cell) == *coord {
                                bincode::serde::encode_into_std_write(
                                    (1_u8, cell),
                                    &mut hash,
                                    bincode::config::standard(),
                                )
                                .unwrap();
                            }
                        }
                    }
                    (*coord, *hash.finalize().as_bytes())
                })
                .collect(),
        )
    }
    fn cells(&self) -> Box<dyn Iterator<Item = Cell> + Send + '_> {
        let size = crate::protocol::CHUNK_SIZE as i32;
        Box::new(self.chunks.keys().flat_map(move |&[cx, cy, cz]| {
            (0..size).flat_map(move |x| {
                (0..size).flat_map(move |y| {
                    (0..size).map(move |z| Cell(cx * size + x, cy * size + y, cz * size + z))
                })
            })
        }))
    }
    fn material(&self, cell: Cell) -> Option<Material> {
        let size = crate::protocol::CHUNK_SIZE as i32;
        let chunk = self.chunks.get(&[
            cell.0.div_euclid(size),
            cell.1.div_euclid(size),
            cell.2.div_euclid(size),
        ])?;
        let (x, y, z) = (
            cell.0.rem_euclid(size),
            cell.1.rem_euclid(size),
            cell.2.rem_euclid(size),
        );
        let value = chunk.cells[(x + y * size + z * size * size) as usize]?;
        Some(Material {
            occupied: value.occupied,
            supports: value.supports,
            shelters: value.shelters,
        })
    }
    fn sky_above(&self, cell: Cell) -> bool {
        let size = crate::protocol::CHUNK_SIZE as i32;
        self.sky_ceiling
            .get(&[cell.0.div_euclid(size), cell.2.div_euclid(size)])
            .is_some_and(|ceiling| cell.1 > *ceiling)
    }
    fn partitions(&self) -> &BTreeMap<Face, Partition> {
        &self.partitions
    }
    fn portals(&self) -> &[AuthoredPortal] {
        &self.portals
    }
    fn placements(&self) -> &[Placement] {
        &self.placements
    }
    fn stairs(&self) -> &[(Cell, Cell)] {
        &self.stairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;
    #[test]
    fn live_chunks_and_rotated_furnishing_metadata_feed_exact_structural_spaces() {
        let blocks = BlockRegistry::build(serde_json::from_value(serde_json::json!([
            {"id":"test:stone","display_name":"Stone","flags":{"solid":true},"color":[1,1,1],"structure":{"solid_cells":[[0,0,0]],"supports":[[0,0,0]],"shelter_cells":[[0,0,0]]}},
            {"id":"test:bed","display_name":"Bed","flags":{"solid":true},"color":[1,1,1],"mesh":"unused.gltf","footprint":[[0,0,0],[1,0,0]]}
        ])).unwrap()).unwrap().0;
        let stone = blocks.require("test:stone");
        let bed = blocks.require("test:bed");
        let scene = block_junk_spaces::fixtures::room(4, 4, 2, Some(3));
        let mut chunk = crate::voxel::Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        for cell in scene.bounds().cells() {
            if scene.voxel(cell) == block_junk_spaces::Voxel::Solid {
                chunk.set(
                    crate::voxel::world_to_chunk(IVec3::new(cell.0 + 8, cell.1, cell.2 + 8)).1,
                    stone,
                );
            }
        }
        let anchor = IVec3::new(9, 1, 10);
        let facing = block_junk_mod_api::blocks::Cardinal::South;
        let other = anchor + IVec3::from_array(facing.rotate_offset([1, 0, 0]));
        let mut sidecar = crate::voxel::ChunkEntities::default();
        sidecar.insert(
            anchor,
            crate::voxel::EntryKind::Anchor {
                orientation: facing,
                portal_open: None,
            },
        );
        sidecar.insert(other, crate::voxel::EntryKind::Ghost { anchor });
        chunk.set(crate::voxel::world_to_chunk(anchor).1, bed);
        chunk.set(crate::voxel::world_to_chunk(other).1, bed);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(blocks)
            .init_resource::<crate::voxel::ChunkMap>()
            .init_resource::<crate::server::SpatialSubscriptions>()
            .init_resource::<crate::server::ActiveSimulationChunks>()
            .init_resource::<Placements>()
            .init_resource::<crate::spatial_geometry::StructuralSummaries>()
            .add_systems(
                Update,
                (
                    crate::spatial_geometry::refresh_structural_summaries,
                    crate::spatial_geometry::materialize_facilities,
                )
                    .chain(),
            );
        let entity = app.world_mut().spawn((chunk, sidecar)).id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), entity);
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO));
        for _ in 0..16 {
            app.update();
        }
        let cache = app
            .world()
            .resource::<crate::spatial_geometry::StructuralSummaries>();
        assert!(cache.pending.is_empty());
        assert!(cache.completed[&[0, 0, 0]].pending_facilities.is_empty());
        let source = LiveSource::capture(
            cache.completed.clone(),
            BTreeMap::from([([0, 0], 31)]),
            app.world().resource::<BlockRegistry>(),
            app.world().resource::<Placements>(),
        );
        let budget = Arc::new(Budget::default());
        let view = View {
            source: Arc::new(source),
            budget: budget.clone(),
            dependencies: Arc::default(),
        };
        let mut task = Computation::new(budget, async move { topology::detect(&view).await });
        let result = loop {
            if let Poll::Ready(result) = task.advance(1024) {
                break result;
            }
        };
        let space = &result.spaces[&result.floor_index[&Cell(10, 1, 10)]];
        assert_eq!(space.floor.len(), 16);
        assert_eq!(space.volume, topology::Volume::Bounded(32));
        assert_eq!(space.placements, BTreeSet::from([1]));
        assert!(!space.pending);
        let preset = serde_json::from_value(serde_json::json!({
            "id":"test:world","display_name":"Test world",
            "terrain":{"stone":"test:stone","dirt":"test:stone","grass":"test:stone","wood":"test:stone","leaves":"test:stone","gravel":"test:stone","forage":"test:stone"},
            "generation":block_junk_mod_api::worlds::GenerationConfig::default(),"player_spawn":[10,1,10],"movement_profile":"test:walker","settlement":"test:settlement"
        })).unwrap();
        app.insert_resource(crate::scripting::SelectedWorld(preset))
            .init_resource::<crate::live_spaces::RoofEnvelope>()
            .init_resource::<crate::live_spaces::SpatialDependencies>()
            .init_resource::<crate::space_identity::SpaceIdentities>()
            .init_resource::<crate::live_spaces::SpatialComputation>()
            .init_resource::<crate::live_spaces::PublishedSpaces>()
            .init_resource::<crate::spatial_events::SpatialJournal>()
            .add_systems(PostUpdate, crate::live_spaces::compute_spaces);
        fn converge(app: &mut App) {
            for _ in 0..1000 {
                app.update();
                let published = app
                    .world()
                    .resource::<crate::live_spaces::PublishedSpaces>();
                if !published.geometry.is_empty() && published.pending.is_empty() {
                    return;
                }
            }
            panic!("live spatial publication did not converge");
        }
        converge(&mut app);
        let mut id = *app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .geometry
            .iter()
            .find(|(_, space)| space.floor.contains(&Cell(10, 1, 10)))
            .unwrap()
            .0;
        app.world_mut()
            .resource_mut::<crate::live_spaces::PublishedSpaces>()
            .events
            .clear();
        // Saturate the durable event journal before a real roof edit. A ready
        // geometry result must remain unpublished until delivery makes room.
        let identities = app
            .world()
            .resource::<crate::space_identity::SpaceIdentities>()
            .clone();
        let original_revision = app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>()
            .revision;
        {
            let mut journal = app
                .world_mut()
                .resource_mut::<crate::spatial_events::SpatialJournal>();
            while journal.has_capacity() {
                journal.publish(
                    &BTreeSet::from([[0, 0, 0]]),
                    &[crate::space_identity::SpaceEvent::Updated(id)],
                    &identities,
                );
            }
        }
        app.world_mut()
            .get_mut::<crate::voxel::Chunk>(entity)
            .unwrap()
            .set(
                crate::voxel::world_to_chunk(IVec3::new(10, 3, 10)).1,
                crate::blocks::BlockSlot::EMPTY,
            );
        for _ in 0..1000 {
            app.update();
        }
        let blocked = app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>();
        assert_eq!(blocked.revision, original_revision);
        assert_eq!(blocked.geometry[&id].volume, topology::Volume::Bounded(32));
        assert!(!blocked.pending.is_empty());
        let empty_mods = tempfile::tempdir().unwrap();
        let mods = block_junk_scripting::ModRegistry::load_dir(
            block_junk_mod_api::Side::Server,
            empty_mods.path(),
            &block_junk_scripting::LoadContext::new(),
        )
        .unwrap();
        app.insert_resource(crate::scripting::ServerMods(mods))
            .add_systems(
                PostUpdate,
                crate::spatial_events::dispatch.after(crate::live_spaces::compute_spaces),
            );
        converge(&mut app);
        let published = app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>();
        assert_eq!(published.geometry[&id].floor.len(), 16);
        assert_eq!(published.geometry[&id].volume, topology::Volume::Open);
        assert!(
            published
                .events
                .contains(&crate::space_identity::SpaceEvent::Updated(id))
        );
        let bytes = bincode::serde::encode_to_vec(
            app.world()
                .resource::<crate::space_identity::SpaceIdentities>(),
            bincode::config::standard(),
        )
        .unwrap();
        let (restored, _): (crate::space_identity::SpaceIdentities, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        app.insert_resource(restored)
            .insert_resource(crate::live_spaces::PublishedSpaces::default())
            .insert_resource(crate::live_spaces::SpatialComputation::default());
        converge(&mut app);
        assert_eq!(
            app.world()
                .resource::<crate::live_spaces::PublishedSpaces>()
                .geometry[&id]
                .floor
                .len(),
            16
        );
        assert!(
            app.world()
                .resource::<crate::live_spaces::PublishedSpaces>()
                .events
                .is_empty()
        );
        // Split and merge through live chunk edits, retaining furnishing IDs
        // while allocating explicit replacement space identities.
        for y in 1..=2 {
            for z in 9..=12 {
                app.world_mut()
                    .get_mut::<crate::voxel::Chunk>(entity)
                    .unwrap()
                    .set(crate::voxel::world_to_chunk(IVec3::new(11, y, z)).1, stone);
            }
        }
        converge(&mut app);
        let split = app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>();
        let children: BTreeSet<_> = split
            .geometry
            .iter()
            .filter(|(_, space)| {
                space.floor.contains(&Cell(10, 1, 10)) || space.floor.contains(&Cell(12, 1, 10))
            })
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(children.len(), 2);
        assert!(!children.contains(&id));
        assert!(split.events.iter().any(|event| matches!(event, crate::space_identity::SpaceEvent::Split { predecessor, successors } if *predecessor == id && successors.iter().copied().collect::<BTreeSet<_>>() == children)));
        assert_eq!(
            app.world()
                .resource::<Placements>()
                .at(anchor)
                .unwrap()
                .id
                .0,
            1
        );
        for y in 1..=2 {
            for z in 9..=12 {
                app.world_mut()
                    .get_mut::<crate::voxel::Chunk>(entity)
                    .unwrap()
                    .set(
                        crate::voxel::world_to_chunk(IVec3::new(11, y, z)).1,
                        crate::blocks::BlockSlot::EMPTY,
                    );
            }
        }
        converge(&mut app);
        let merged = app
            .world()
            .resource::<crate::live_spaces::PublishedSpaces>();
        let successor = *merged
            .geometry
            .iter()
            .find(|(_, space)| space.floor.contains(&Cell(10, 1, 10)))
            .unwrap()
            .0;
        assert!(!children.contains(&successor));
        assert_eq!(merged.geometry[&successor].floor.len(), 16);
        assert!(merged.events.iter().any(|event| matches!(event, crate::space_identity::SpaceEvent::Merged { successor: found, predecessors } if *found == successor && predecessors.iter().copied().collect::<BTreeSet<_>>() == children)));
        let identities = app
            .world()
            .resource::<crate::space_identity::SpaceIdentities>();
        assert!(!identities.records[&id].active);
        assert!(children.iter().all(|id| !identities.records[id].active));
        id = successor;
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .clear();
        app.update();
        app.update();
        assert!(
            app.world()
                .resource::<crate::live_spaces::SpatialDependencies>()
                .0
                .is_empty()
        );
        assert!(
            app.world()
                .resource::<crate::space_identity::SpaceIdentities>()
                .records[&id]
                .active
        );
        assert_eq!(
            app.world()
                .resource::<crate::live_spaces::SpatialComputation>()
                .operations_last_tick,
            0
        );
    }
}
