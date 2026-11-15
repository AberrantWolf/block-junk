//! Demand-driven structural summaries. Geometry reads are stamped even when a
//! chunk or anchor is unavailable. Collision/rendering never supply architecture.
use crate::{
    blocks::BlockRegistry,
    protocol::{CHUNK_SIZE, ChunkCoord},
    voxel::{Chunk, ChunkEntities, ChunkMap, EntryKind, world_to_chunk},
};
use bevy::prelude::*;
use block_junk_mod_api::blocks::Cardinal;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

type Coord = [i32; 3];
const CELLS: usize = (CHUNK_SIZE as usize).pow(3);
const QUANTUM: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructuralCell {
    pub occupied: bool,
    pub supports: bool,
    pub shelters: bool,
}
#[derive(Clone, Debug)]
pub struct ArchitecturalElement {
    pub slot: crate::blocks::BlockSlot,
    pub orientation: Cardinal,
}
#[derive(Clone, Debug)]
pub struct ChunkGeometry {
    pub cells: Vec<Option<StructuralCell>>,
    /// Physical occupancy is a separate layer; furnishing collision never
    /// changes structural support, floor membership or shelter measurements.
    pub physical: Vec<Option<crate::blocks::BlockSlot>>,
    pub sidecars: BTreeMap<Coord, EntryKind>,
    /// Exact anchor references, including elements anchored in another chunk.
    pub elements: BTreeMap<Coord, ArchitecturalElement>,
    pub dependencies: BTreeMap<Coord, Option<u64>>,
    pub pending_facilities: BTreeSet<Coord>,
}
impl Default for ChunkGeometry {
    fn default() -> Self {
        Self {
            cells: vec![None; CELLS],
            physical: vec![None; CELLS],
            sidecars: BTreeMap::new(),
            elements: BTreeMap::new(),
            dependencies: BTreeMap::new(),
            pending_facilities: BTreeSet::new(),
        }
    }
}
impl ChunkGeometry {
    pub fn cell_index(cell: IVec3) -> usize {
        let size = CHUNK_SIZE as i32;
        (cell.x.rem_euclid(size)
            + cell.y.rem_euclid(size) * size
            + cell.z.rem_euclid(size) * size * size) as usize
    }
}
struct Work {
    coord: Coord,
    next: usize,
    geometry: ChunkGeometry,
}
#[derive(Resource)]
pub struct StructuralSummaries {
    pub operation_budget: usize,
    pub completed: BTreeMap<Coord, Arc<ChunkGeometry>>,
    pub pending: BTreeSet<Coord>,
    pub operations_last_tick: usize,
    demand: BTreeSet<Coord>,
    reverse_dependencies: BTreeMap<Coord, BTreeSet<Coord>>,
    watched: BTreeMap<Coord, BTreeSet<Coord>>,
    observed: BTreeMap<Coord, Option<u64>>,
    dirty: BTreeSet<Coord>,
    queue: VecDeque<Work>,
    identities: VecDeque<(Coord, Coord)>,
}
impl Default for StructuralSummaries {
    fn default() -> Self {
        Self {
            operation_budget: 4096,
            completed: BTreeMap::new(),
            pending: BTreeSet::new(),
            operations_last_tick: 0,
            demand: BTreeSet::new(),
            reverse_dependencies: BTreeMap::new(),
            watched: BTreeMap::new(),
            observed: BTreeMap::new(),
            dirty: BTreeSet::new(),
            queue: VecDeque::new(),
            identities: VecDeque::new(),
        }
    }
}
fn cell_at(coord: Coord, index: usize) -> IVec3 {
    let size = CHUNK_SIZE as usize;
    IVec3::from_array(coord) * CHUNK_SIZE as i32
        + IVec3::new(
            (index % size) as i32,
            ((index / size) % size) as i32,
            (index / (size * size)) as i32,
        )
}
fn stamp(
    coord: Coord,
    map: &ChunkMap,
    chunks: &Query<(&Chunk, Option<&ChunkEntities>)>,
) -> Option<u64> {
    map.0
        .get(&ChunkCoord(IVec3::from_array(coord)))
        .and_then(|entity| chunks.get(*entity).ok())
        .map(|(chunk, _)| chunk.revision)
}
fn read(
    cell: IVec3,
    out: &mut ChunkGeometry,
    map: &ChunkMap,
    chunks: &Query<(&Chunk, Option<&ChunkEntities>)>,
) -> Option<(crate::blocks::BlockSlot, Option<EntryKind>)> {
    let (coord, local) = world_to_chunk(cell);
    let value = map
        .0
        .get(&coord)
        .and_then(|entity| chunks.get(*entity).ok());
    out.dependencies
        .insert(coord.0.to_array(), value.map(|(chunk, _)| chunk.revision));
    value.map(|(chunk, entities)| {
        (
            chunk.get(local),
            entities.and_then(|entities| entities.get(cell)),
        )
    })
}
fn summarize(
    cell: IVec3,
    out: &mut ChunkGeometry,
    map: &ChunkMap,
    chunks: &Query<(&Chunk, Option<&ChunkEntities>)>,
    blocks: &BlockRegistry,
) -> Option<StructuralCell> {
    summarize_with(cell, out, blocks, &|cell| {
        let (coord, local) = world_to_chunk(cell);
        let (chunk, entities) = chunks.get(*map.0.get(&coord)?).ok()?;
        Some((
            chunk.get(local),
            entities.and_then(|entities| entities.get(cell)),
            chunk.revision,
        ))
    })
}

/// Shared structural decoding for authoritative summaries and isolated previews.
pub(crate) fn summarize_with(
    cell: IVec3,
    out: &mut ChunkGeometry,
    blocks: &BlockRegistry,
    lookup: &impl Fn(IVec3) -> Option<(crate::blocks::BlockSlot, Option<EntryKind>, u64)>,
) -> Option<StructuralCell> {
    let read = |cell: IVec3, out: &mut ChunkGeometry| {
        let value = lookup(cell);
        out.dependencies.insert(
            world_to_chunk(cell).0.0.to_array(),
            value.map(|(_, _, revision)| revision),
        );
        value.map(|(slot, entry, _)| (slot, entry))
    };
    let (slot, entry) = read(cell, out)?;
    out.physical[ChunkGeometry::cell_index(cell)] = Some(slot);
    if let Some(entry) = entry {
        out.sidecars.insert(cell.to_array(), entry);
    }
    if slot.is_empty() {
        return Some(StructuralCell::default());
    }
    let (anchor, orientation) = match entry {
        Some(EntryKind::Anchor { orientation, .. }) => (cell, orientation),
        Some(EntryKind::Ghost { anchor }) => {
            let (anchor_slot, anchor_entry) = read(anchor, out)?;
            let Some(EntryKind::Anchor { orientation, .. }) = anchor_entry else {
                return None;
            };
            if anchor_slot != slot {
                return None;
            }
            (anchor, orientation)
        }
        None => (cell, Cardinal::default()),
    };
    let architecture = &blocks.architecture[usize::from(slot.0)][orientation as usize];
    let offset = (cell - anchor).to_array();
    let def = blocks.def(slot);
    if crate::placements::Placements::tracked(def) {
        out.elements.insert(
            anchor.to_array(),
            ArchitecturalElement { slot, orientation },
        );
    }
    Some(StructuralCell {
        occupied: architecture.structure.solid_cells.contains(&offset),
        supports: architecture.structure.supports.contains(&offset),
        shelters: architecture.structure.shelter_cells.contains(&offset),
    })
}
impl StructuralSummaries {
    fn unwatch(&mut self, owner: Coord) {
        for dependency in self.watched.remove(&owner).into_iter().flatten() {
            if let Some(owners) = self.reverse_dependencies.get_mut(&dependency) {
                owners.remove(&owner);
                if owners.is_empty() {
                    self.reverse_dependencies.remove(&dependency);
                    self.observed.remove(&dependency);
                }
            }
        }
    }
    fn watch(&mut self, owner: Coord, dependencies: &BTreeMap<Coord, Option<u64>>) {
        self.unwatch(owner);
        for (&dependency, &revision) in dependencies {
            self.reverse_dependencies
                .entry(dependency)
                .or_default()
                .insert(owner);
            self.observed.entry(dependency).or_insert(revision);
        }
        self.watched
            .insert(owner, dependencies.keys().copied().collect());
    }

    fn valid(
        geometry: &ChunkGeometry,
        map: &ChunkMap,
        chunks: &Query<(&Chunk, Option<&ChunkEntities>)>,
    ) -> bool {
        geometry
            .dependencies
            .iter()
            .all(|(coord, revision)| stamp(*coord, map, chunks) == *revision)
    }
    pub fn refresh(
        &mut self,
        demand: BTreeSet<Coord>,
        map: &ChunkMap,
        chunks: &Query<(&Chunk, Option<&ChunkEntities>)>,
        blocks: &BlockRegistry,
    ) {
        self.operations_last_tick = 0;
        let removed: Vec<_> = self.demand.difference(&demand).copied().collect();
        for coord in removed {
            self.unwatch(coord);
            self.dirty.remove(&coord);
        }
        let added: Vec<_> = demand.difference(&self.demand).copied().collect();
        for coord in added {
            if let Some(geometry) = self.completed.get(&coord).cloned() {
                if !Self::valid(&geometry, map, chunks) {
                    self.dirty.insert(coord);
                }
                self.watch(coord, &geometry.dependencies);
            } else {
                self.dirty.insert(coord);
            }
        }
        // Check each demanded dependency once, then invalidate its reverse
        // dependents. Unrequested generated chunks never enter this index.
        for (dependency, previous) in &mut self.observed {
            let current = stamp(*dependency, map, chunks);
            if current != *previous {
                *previous = current;
                self.dirty
                    .extend(self.reverse_dependencies[dependency].iter().copied());
            }
        }
        self.demand = demand;
        self.queue.retain(|work| self.demand.contains(&work.coord));
        self.identities
            .retain(|(coord, _)| self.demand.contains(coord));
        self.pending.retain(|coord| self.demand.contains(coord));
        for &coord in &self.demand {
            if self.pending.contains(&coord) {
                continue;
            }
            if self.completed.contains_key(&coord) && !self.dirty.contains(&coord) {
                continue;
            }
            self.pending.insert(coord);
            self.queue.push_back(Work {
                coord,
                next: 0,
                geometry: ChunkGeometry::default(),
            });
        }
        while self.operations_last_tick < self.operation_budget {
            let Some(mut work) = self.queue.pop_front() else {
                break;
            };
            if !Self::valid(&work.geometry, map, chunks) {
                work.next = 0;
                work.geometry = ChunkGeometry::default();
            }
            let steps = QUANTUM
                .min(self.operation_budget - self.operations_last_tick)
                .min(CELLS - work.next);
            for _ in 0..steps {
                let cell = cell_at(work.coord, work.next);
                work.geometry.cells[work.next] =
                    summarize(cell, &mut work.geometry, map, chunks, blocks);
                work.next += 1;
                self.operations_last_tick += 1;
            }
            if work.next == CELLS {
                // Every read in this synchronous quantum shares authoritative
                // borrows; no partially rebuilt summary is published.
                self.pending.remove(&work.coord);
                self.dirty.remove(&work.coord);
                self.watch(work.coord, &work.geometry.dependencies);
                for &anchor in work.geometry.elements.keys() {
                    work.geometry.pending_facilities.insert(anchor);
                    self.identities.push_back((work.coord, anchor));
                }
                self.completed.insert(work.coord, Arc::new(work.geometry));
            } else {
                self.queue.push_back(work);
            }
        }
    }
}

/// Materialize only elements discovered by demanded structural work. A failed
/// footprint read is a dependency, so arrival of that chunk retries the index.
pub fn materialize_facilities(
    mut commands: Commands,
    map: Res<ChunkMap>,
    chunks: Query<(&Chunk, Option<&ChunkEntities>)>,
    blocks: Res<BlockRegistry>,
    mut placements: ResMut<crate::placements::Placements>,
    mut summaries: ResMut<StructuralSummaries>,
) {
    let mut sidecars = BTreeMap::<Coord, ChunkEntities>::new();
    for _ in 0..32 {
        let Some((coord, anchor)) = summaries.identities.pop_front() else {
            break;
        };
        if !summaries.demand.contains(&coord) || summaries.pending.contains(&coord) {
            continue;
        }
        let Some(summary) = summaries.completed.get_mut(&coord) else {
            continue;
        };
        let summary = Arc::make_mut(summary);
        if !StructuralSummaries::valid(summary, &map, &chunks) {
            continue;
        }
        let Some(element) = summary.elements.get(&anchor).cloned() else {
            continue;
        };
        let anchor_cell = IVec3::from_array(anchor);
        let mut footprint = BTreeMap::new();
        for offset in &blocks.def(element.slot).footprint {
            let cell = anchor_cell + IVec3::from_array(element.orientation.rotate_offset(*offset));
            footprint.insert(
                cell.to_array(),
                read(cell, summary, &map, &chunks).map(|value| value.0),
            );
        }
        if placements
            .index_existing(
                anchor_cell,
                element.slot,
                element.orientation,
                &blocks,
                |cell| footprint.get(&cell.to_array()).copied().flatten(),
            )
            .is_ok()
        {
            summary.pending_facilities.remove(&anchor);
            // Merge all discoveries in a chunk before inserting the component:
            // separate deferred inserts would otherwise overwrite each other.
            for cell in footprint.keys() {
                let cell = IVec3::from_array(*cell);
                let (chunk_coord, _) = world_to_chunk(cell);
                let next = sidecars.entry(chunk_coord.0.to_array()).or_insert_with(|| {
                    map.0
                        .get(&chunk_coord)
                        .and_then(|entity| chunks.get(*entity).ok())
                        .and_then(|(_, sidecar)| sidecar.cloned())
                        .unwrap_or_default()
                });
                next.insert(
                    cell,
                    if cell == anchor_cell {
                        EntryKind::Anchor {
                            orientation: element.orientation,
                            portal_open: placements
                                .at(cell)
                                .and_then(|record| record.portal_open(&blocks)),
                        }
                    } else {
                        EntryKind::Ghost {
                            anchor: anchor_cell,
                        }
                    },
                );
            }
        }
        let dependencies = summary.dependencies.clone();
        summaries.watch(coord, &dependencies);
    }
    for (coord, sidecar) in sidecars {
        if let Some(entity) = map.0.get(&ChunkCoord(IVec3::from_array(coord))) {
            commands.entity(*entity).insert(sidecar);
        }
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct StructuralDemand<'w> {
    subscriptions: Res<'w, crate::server::SpatialSubscriptions>,
    simulation: Res<'w, crate::server::ActiveSimulationChunks>,
    dependencies: Option<Res<'w, crate::live_spaces::SpatialDependencies>>,
    settlements: Option<Res<'w, crate::settlements::SettlementDependencies>>,
    facilities: Option<Res<'w, crate::facility_access::FacilityDependencies>>,
    capabilities: Option<Res<'w, crate::capabilities::CapabilityDependencies>>,
}

pub fn refresh_structural_summaries(
    demand: StructuralDemand,
    map: Res<ChunkMap>,
    chunks: Query<(&Chunk, Option<&ChunkEntities>)>,
    blocks: Res<BlockRegistry>,
    mut summaries: ResMut<StructuralSummaries>,
) {
    let StructuralDemand {
        subscriptions,
        simulation,
        dependencies,
        settlements,
        facilities,
        capabilities,
    } = demand;
    let demand = subscriptions
        .requested_chunks()
        .chain(simulation.0.iter().copied())
        .map(|coord| coord.0.to_array())
        .chain(
            dependencies
                .iter()
                .flat_map(|demand| demand.0.iter().copied()),
        )
        .chain(
            settlements
                .iter()
                .flat_map(|demand| demand.roots.iter().chain(demand.reads.iter()).copied()),
        )
        .chain(
            facilities
                .iter()
                .flat_map(|demand| demand.0.iter().copied()),
        )
        .collect::<BTreeSet<_>>();
    let demand = demand
        .into_iter()
        .chain(
            capabilities
                .iter()
                .flat_map(|demand| demand.0.iter().copied()),
        )
        .collect();
    summaries.refresh(demand, &map, &chunks, &blocks);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> App {
        let blocks = BlockRegistry::build(serde_json::from_value(serde_json::json!([
            {"id":"test:floor","display_name":"Floor","flags":{},"color":[1,1,1],"structure":{"solid_cells":[[0,0,0]],"supports":[[0,0,0]],"shelter_cells":[[0,0,0]]}},
            {"id":"test:bed","display_name":"Bed","color":[1,1,1],"mesh":"unused.gltf","flags":{"solid":true},"footprint":[[0,0,0],[1,0,0]]},
            {"id":"test:canopy","display_name":"Canopy","flags":{},"color":[1,1,1],"structure":{"shelter_cells":[[0,0,0]]}},
            {"id":"test:stool","display_name":"Stool","mesh":"unused.gltf","flags":{"solid":true},"color":[1,1,1]}
        ])).unwrap()).unwrap().0;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(blocks)
            .init_resource::<ChunkMap>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::server::SpatialSubscriptions>()
            .init_resource::<crate::server::ActiveSimulationChunks>()
            .insert_resource(StructuralSummaries {
                operation_budget: 127,
                ..default()
            })
            .add_systems(
                Update,
                (refresh_structural_summaries, materialize_facilities).chain(),
            );
        app
    }
    fn empty() -> Chunk {
        Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        }
    }
    fn settle(app: &mut App, coord: Coord) {
        for _ in 0..(CELLS / 127 + 3) {
            app.update();
            let summary = app.world().resource::<StructuralSummaries>();
            assert!(summary.operations_last_tick <= 127);
            if summary.completed.contains_key(&coord) && !summary.pending.contains(&coord) {
                return;
            }
        }
        panic!("finite structural work failed to converge");
    }
    #[test]
    fn demanded_procedural_furnishings_merge_sidecars_and_keep_unique_ids() {
        let mut app = fixture();
        let bed = app
            .world()
            .resource::<BlockRegistry>()
            .require("test:stool");
        let mut chunk = empty();
        let anchors = [IVec3::new(2, 1, 2), IVec3::new(6, 1, 2)];
        for anchor in anchors {
            chunk.set(world_to_chunk(anchor).1, bed);
        }
        let entity = app.world_mut().spawn(chunk).id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(ChunkCoord(IVec3::ZERO));
        settle(&mut app, [0, 0, 0]);
        let sidecar = app.world().get::<ChunkEntities>(entity).unwrap();
        assert_eq!(sidecar.entries.len(), 2);
        for anchor in anchors {
            assert!(matches!(
                sidecar.get(anchor),
                Some(EntryKind::Anchor { .. })
            ));
        }
        let placements = app.world().resource::<crate::placements::Placements>();
        assert_eq!(placements.records.len(), 2);
        let ids: Vec<_> = anchors
            .iter()
            .map(|cell| placements.at(*cell).unwrap().id)
            .collect();
        app.update();
        let placements = app.world().resource::<crate::placements::Placements>();
        assert_eq!(placements.records.len(), 2);
        assert_eq!(
            ids,
            anchors
                .iter()
                .map(|cell| placements.at(*cell).unwrap().id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn summaries_separate_furniture_support_and_shelter_and_ignore_unrequested_chunks() {
        let mut app = fixture();
        let blocks = app.world().resource::<BlockRegistry>();
        let mut chunk = empty();
        chunk.set(
            world_to_chunk(IVec3::new(2, 0, 2)).1,
            blocks.require("test:floor"),
        );
        chunk.set(
            world_to_chunk(IVec3::new(2, 1, 2)).1,
            blocks.require("test:bed"),
        );
        chunk.set(
            world_to_chunk(IVec3::new(2, 4, 2)).1,
            blocks.require("test:canopy"),
        );
        let entity = app.world_mut().spawn(chunk).id();
        let unused = app.world_mut().spawn(empty()).id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::splat(100)), unused);
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(ChunkCoord(IVec3::ZERO));
        settle(&mut app, [0, 0, 0]);
        let summaries = app.world().resource::<StructuralSummaries>();
        assert_eq!(summaries.completed.len(), 1);
        let summary = &summaries.completed[&[0, 0, 0]];
        assert_eq!(
            summary
                .cells
                .iter()
                .flatten()
                .filter(|cell| cell.occupied)
                .count(),
            1
        );
        assert_eq!(
            summary
                .cells
                .iter()
                .flatten()
                .filter(|cell| cell.supports)
                .count(),
            1
        );
        assert_eq!(
            summary
                .cells
                .iter()
                .flatten()
                .filter(|cell| cell.shelters)
                .count(),
            2
        );
        let element = &summary.elements[&[2, 1, 2]];
        assert_eq!(
            element.slot,
            app.world().resource::<BlockRegistry>().require("test:bed")
        );
        assert_eq!(element.orientation, Cardinal::East);
        assert!(!summary.dependencies.contains_key(&[100, 100, 100]));
    }
    #[test]
    fn unsuccessful_cross_chunk_anchor_reads_invalidate_when_geometry_arrives() {
        let mut app = fixture();
        let bed = app.world().resource::<BlockRegistry>().require("test:bed");
        let anchor = IVec3::new(CHUNK_SIZE as i32 - 1, 1, 4);
        let ghost = anchor + IVec3::X;
        let mut chunk = empty();
        chunk.set(world_to_chunk(ghost).1, bed);
        let mut entities = ChunkEntities::default();
        entities.insert(ghost, EntryKind::Ghost { anchor });
        let entity = app.world_mut().spawn((chunk, entities)).id();
        let coord = world_to_chunk(ghost).0;
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(coord, entity);
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(coord);
        settle(&mut app, coord.0.to_array());
        let summary = &app.world().resource::<StructuralSummaries>().completed[&coord.0.to_array()];
        assert_eq!(summary.dependencies[&[0, 0, 0]], None);
        assert_eq!(
            summary.cells.iter().filter(|cell| cell.is_none()).count(),
            1
        );
        let mut chunk = empty();
        chunk.set(world_to_chunk(anchor).1, bed);
        let mut entities = ChunkEntities::default();
        entities.insert(
            anchor,
            EntryKind::Anchor {
                orientation: Cardinal::East,
                portal_open: None,
            },
        );
        let entity = app.world_mut().spawn((chunk, entities)).id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        settle(&mut app, coord.0.to_array());
        let summary = &app.world().resource::<StructuralSummaries>().completed[&coord.0.to_array()];
        assert_eq!(summary.dependencies[&[0, 0, 0]], Some(1));
        assert!(summary.cells.iter().all(Option::is_some));
        assert!(summary.elements.contains_key(&anchor.to_array()));
        assert_eq!(
            app.world()
                .resource::<StructuralSummaries>()
                .completed
                .len(),
            1
        );
    }
}
