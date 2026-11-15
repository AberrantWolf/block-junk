//! Authoritative spatial lifecycle. Demand follows shared subscriptions and active
//! simulation; connected chunk domains are computation scopes, never room identity.
use crate::{
    architecture_solver::{
        self as solver, Budget, Computation, View, live_source::LiveSource, topology::Topology,
    },
    protocol::{CHUNK_SIZE, ChunkCoord},
    space_identity::{Membership, SpaceDraft, SpaceEvent, SpaceId, SpaceIdentities},
    spatial_geometry::StructuralSummaries,
    voxel::{Chunk, ChunkMap},
};
use bevy::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    task::Poll,
};

type Coord = [i32; 3];
#[derive(Resource, Default)]
pub struct SpatialDependencies(pub BTreeSet<Coord>);
#[derive(Resource, Default)]
pub struct RoofEnvelope(pub BTreeMap<[i32; 2], i32>);
#[derive(Resource, Default)]
pub struct PublishedSpaces {
    pub geometry: BTreeMap<SpaceId, solver::topology::Space>,
    pub events: Vec<SpaceEvent>,
    pub pending: BTreeSet<Coord>,
    pub revision: u64,
    pub basis: BTreeMap<SpaceId, BTreeMap<Coord, u64>>,
    pub connections: BTreeMap<SpaceId, Vec<solver::topology::Connection>>,
}
type RegionResult = (Topology, BTreeSet<SpaceId>);
struct Region {
    owned: BTreeSet<SpaceId>,
    ready: Option<RegionResult>,
    roots: BTreeSet<Coord>,
    domain: BTreeSet<Coord>,
    source_fingerprint: Option<[u8; 32]>,
    task: Option<Mutex<Computation<RegionResult>>>,
    basis: BTreeMap<Coord, u64>,
}
#[derive(Resource)]
pub struct SpatialComputation {
    regions: BTreeMap<Coord, Region>,
    pub operation_budget: usize,
    pub operations_last_tick: usize,
    cursor: usize,
    local_cache: Arc<Mutex<solver::local_topology::Cache>>,
}
impl Default for SpatialComputation {
    fn default() -> Self {
        Self {
            regions: BTreeMap::new(),
            operation_budget: 8192,
            operations_last_tick: 0,
            cursor: 0,
            local_cache: Arc::default(),
        }
    }
}
pub fn track_roof_envelope(
    edited: Query<&ChunkCoord, Added<crate::server::ChunkEdited>>,
    mut edits: MessageReader<crate::protocol::CellEdit>,
    mut envelope: ResMut<RoofEnvelope>,
) {
    for coord in &edited {
        let upper = (coord.0.y + 1) * CHUNK_SIZE as i32 - 1;
        envelope
            .0
            .entry([coord.0.x, coord.0.z])
            .and_modify(|y| *y = (*y).max(upper))
            .or_insert(upper);
    }
    for edit in edits.read() {
        let coord = crate::voxel::world_to_chunk(edit.world).0.0;
        let upper = (coord.y + 1) * CHUNK_SIZE as i32 - 1;
        envelope
            .0
            .entry([coord.x, coord.z])
            .and_modify(|y| *y = (*y).max(upper))
            .or_insert(upper);
    }
}
fn groups(mut roots: BTreeSet<Coord>) -> Vec<BTreeSet<Coord>> {
    let mut result = Vec::new();
    while let Some(seed) = roots.pop_first() {
        let mut group = BTreeSet::from([seed]);
        let mut queue = VecDeque::from([seed]);
        while let Some(cell) = queue.pop_front() {
            for offset in [
                [1, 0, 0],
                [-1, 0, 0],
                [0, 1, 0],
                [0, -1, 0],
                [0, 0, 1],
                [0, 0, -1],
            ] {
                let next = std::array::from_fn(|i| cell[i] + offset[i]);
                if roots.remove(&next) {
                    group.insert(next);
                    queue.push_back(next);
                }
            }
        }
        result.push(group);
    }
    result
}
fn membership_chunks(membership: &Membership) -> BTreeSet<Coord> {
    let size = CHUNK_SIZE as i32;
    let mut chunks = BTreeSet::new();
    for run in &membership.0 {
        let end = run.start[0] + run.len as i32 - 1;
        for x in run.start[0].div_euclid(size)..=end.div_euclid(size) {
            chunks.insert([
                x,
                run.start[1].div_euclid(size),
                run.start[2].div_euclid(size),
            ]);
        }
    }
    chunks
}
#[derive(bevy::ecs::system::SystemParam)]
pub struct SpatialWorld<'w, 's> {
    subscriptions: Res<'w, crate::server::SpatialSubscriptions>,
    settlements: Option<Res<'w, crate::settlements::SettlementDependencies>>,
    simulation: Res<'w, crate::server::ActiveSimulationChunks>,
    summaries: Res<'w, StructuralSummaries>,
    map: Res<'w, ChunkMap>,
    chunks: Query<'w, 's, &'static Chunk>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    placements: Res<'w, crate::placements::Placements>,
    preset: Res<'w, crate::scripting::SelectedWorld>,
    roofs: Res<'w, RoofEnvelope>,
}
fn capture(region: &Region, world: &SpatialWorld) -> Option<(Option<LiveSource>, [u8; 32])> {
    let mut chunks = BTreeMap::new();
    let mut hash = blake3::Hasher::new();
    let g = &world.preset.0.generation;
    let generated_ceiling = (g.base_height + g.amplitude[0].abs() + g.amplitude[1].abs()).ceil()
        as i32
        + g.features.tree_height[1] as i32
        + g.features.canopy_radius as i32
        + 2;
    let mut ceilings = BTreeMap::new();
    for coord in &region.domain {
        if world.summaries.pending.contains(coord) {
            return None;
        }
        let summary = world.summaries.completed.get(coord)?;
        for (dependency, revision) in &summary.dependencies {
            let current = world
                .map
                .0
                .get(&ChunkCoord(IVec3::from_array(*dependency)))
                .and_then(|e| world.chunks.get(*e).ok())
                .map(|chunk| chunk.revision);
            if current != *revision {
                return None;
            }
        }
        // Unknown chunk summaries are still useful observations. Their absence
        // is stamped and the completed topology remains explicitly pending.
        for value in coord {
            hash.update(&value.to_le_bytes());
        }
        for (dependency, revision) in &summary.dependencies {
            for value in dependency {
                hash.update(&value.to_le_bytes());
            }
            hash.update(&revision.unwrap_or(u64::MAX).to_le_bytes());
        }
        let ceiling = world
            .roofs
            .0
            .get(&[coord[0], coord[2]])
            .copied()
            .unwrap_or(generated_ceiling)
            .max(generated_ceiling);
        ceilings.insert([coord[0], coord[2]], ceiling);
        hash.update(&ceiling.to_le_bytes());
        for anchor in summary.elements.keys() {
            if let Some(record) = world.placements.at(IVec3::from_array(*anchor)) {
                hash.update(&record.id.0.to_le_bytes());
                hash.update(&record.revision.to_le_bytes());
                hash.update(record.authored_state.as_bytes());
            }
        }
        chunks.insert(*coord, summary.clone());
    }
    let fingerprint = *hash.finalize().as_bytes();
    let source = (region.source_fingerprint != Some(fingerprint))
        .then(|| LiveSource::capture(chunks, ceilings, &world.blocks, &world.placements));
    Some((source, fingerprint))
}

async fn continue_empty_roots(
    view: &View,
    roots: &BTreeSet<Coord>,
    topology: &mut Topology,
) -> BTreeSet<solver::Cell> {
    let mut selected = BTreeSet::new();
    let size = CHUNK_SIZE as i32;
    for &[cx, cy, cz] in roots {
        if topology.floor_index.keys().any(|cell| {
            [
                cell.0.div_euclid(size),
                cell.1.div_euclid(size),
                cell.2.div_euclid(size),
            ] == [cx, cy, cz]
        }) {
            continue;
        }
        for x in 0..size {
            for z in 0..size {
                let mut cell = solver::Cell(cx * size + x, cy * size, cz * size + z);
                if view.read(cell).await.is_none_or(|m| m.occupied) {
                    continue;
                }
                loop {
                    cell = cell.offset(0, -1, 0);
                    match view.read(cell).await {
                        Some(material) if material.occupied => {
                            if let Some(key) = topology.floor_index.get(&cell.offset(0, 1, 0)) {
                                selected.insert(*key);
                            }
                            break;
                        }
                        Some(_) => {}
                        None => {
                            topology.continuations.insert([
                                cell.0.div_euclid(size),
                                cell.1.div_euclid(size),
                                cell.2.div_euclid(size),
                            ]);
                            break;
                        }
                    }
                }
            }
        }
    }
    topology
        .dependencies
        .extend(view.dependencies.lock().unwrap().iter().copied());
    selected
}

async fn retain_demanded_spaces(
    view: &View,
    roots: &BTreeSet<Coord>,
    owned: BTreeSet<SpaceId>,
    records: Vec<(SpaceId, Membership)>,
    topology: &mut Topology,
) -> BTreeSet<SpaceId> {
    let mut old_cells = BTreeMap::new();
    let mut scope = owned;
    for (id, membership) in records {
        if !membership_chunks(&membership).is_disjoint(roots) {
            scope.insert(id);
        }
        for run in membership.0 {
            for x in run.start[0]..run.start[0] + run.len as i32 {
                view.budget.step().await;
                old_cells.insert(solver::Cell(x, run.start[1], run.start[2]), id);
            }
        }
    }
    let mut selected = BTreeSet::new();
    let mut old_to_new = BTreeMap::<SpaceId, BTreeSet<solver::Cell>>::new();
    let mut new_to_old = BTreeMap::<solver::Cell, BTreeSet<SpaceId>>::new();
    let size = CHUNK_SIZE as i32;
    for (&key, space) in &topology.spaces {
        for cell in &space.floor {
            view.budget.step().await;
            if roots.contains(&[
                cell.0.div_euclid(size),
                cell.1.div_euclid(size),
                cell.2.div_euclid(size),
            ]) {
                selected.insert(key);
            }
            if let Some(&id) = old_cells.get(cell) {
                old_to_new.entry(id).or_default().insert(key);
                new_to_old.entry(key).or_default().insert(id);
            }
        }
    }
    // Probe first, retaining only the space directly below an otherwise empty
    // demanded chunk; other caves encountered in dependency chunks stay inactive.
    topology.continuations.clear();
    let projected = continue_empty_roots(view, roots, topology).await;
    let empty_continuations = topology.continuations.clone();
    selected.extend(projected);
    loop {
        let before = (scope.len(), selected.len());
        for key in selected.clone() {
            view.budget.step().await;
            scope.extend(new_to_old.get(&key).into_iter().flatten().copied());
        }
        for id in scope.clone() {
            view.budget.step().await;
            selected.extend(old_to_new.get(&id).into_iter().flatten().copied());
        }
        if before == (scope.len(), selected.len()) {
            break;
        }
    }
    topology.spaces.retain(|key, _| selected.contains(key));
    topology.floor_index.retain(|_, key| selected.contains(key));
    topology.connections.retain(|connection| {
        connection
            .ends
            .iter()
            .any(|ends| !ends.is_disjoint(&selected))
    });
    solver::topology::refresh_continuations(view, topology).await;
    // Only downward dependencies introduced by the demand probes are added here.
    topology.continuations.extend(empty_continuations);
    scope
}

pub fn compute_spaces(
    world: SpatialWorld,
    mut computation: ResMut<SpatialComputation>,
    mut dependencies: ResMut<SpatialDependencies>,
    mut identities: ResMut<SpaceIdentities>,
    mut published: ResMut<PublishedSpaces>,
    mut journal: ResMut<crate::spatial_events::SpatialJournal>,
) {
    let roots: BTreeSet<_> = world
        .subscriptions
        .requested_chunks()
        .chain(world.simulation.0.iter().copied())
        .map(|coord| coord.0.to_array())
        .chain(
            world
                .settlements
                .iter()
                .flat_map(|demand| demand.roots.iter().copied()),
        )
        .collect();
    let mut domains: Vec<_> = groups(roots.clone())
        .into_iter()
        .map(|roots| (roots.clone(), roots))
        .collect();
    // Dependency domains shared by two demand roots are computed together. This
    // avoids competing publications for the same cave or persisted space.
    let old_domains: Vec<_> = computation
        .regions
        .values()
        .filter(|region| !region.roots.is_disjoint(&roots))
        .map(|region| {
            (
                region.roots.clone(),
                region
                    .domain
                    .difference(&region.roots)
                    .copied()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect();
    let recorded: Vec<_> = identities
        .records
        .values()
        .filter(|record| record.active)
        .map(|record| membership_chunks(&record.membership))
        .collect();
    loop {
        let mut changed = false;
        for (active_roots, domain) in &mut domains {
            for (old_roots, extra) in &old_domains {
                if !active_roots.is_disjoint(old_roots) || !domain.is_disjoint(extra) {
                    let before = domain.len();
                    domain.extend(extra);
                    changed |= domain.len() != before;
                }
            }
            for linked in &recorded {
                if !domain.is_disjoint(linked) {
                    let before = domain.len();
                    domain.extend(linked);
                    changed |= domain.len() != before;
                }
            }
            let metadata: Vec<_> = domain
                .iter()
                .filter_map(|coord| world.summaries.completed.get(coord))
                .flat_map(|summary| summary.dependencies.keys().copied())
                .collect();
            let before = domain.len();
            domain.extend(metadata);
            changed |= domain.len() != before;
        }
        let mut merge = None;
        'pairs: for a in 0..domains.len() {
            for b in a + 1..domains.len() {
                if !domains[a].1.is_disjoint(&domains[b].1) {
                    merge = Some((a, b));
                    break 'pairs;
                }
            }
        }
        if let Some((a, b)) = merge {
            let (roots, domain) = domains.remove(b);
            domains[a].0.extend(roots);
            domains[a].1.extend(domain);
            changed = true;
        }
        if !changed {
            break;
        }
    }
    let keys: BTreeSet<_> = domains
        .iter()
        .map(|(roots, _)| *roots.first().unwrap())
        .collect();
    computation.regions.retain(|key, _| keys.contains(key));
    for (roots, domain) in domains {
        let key = *roots.first().unwrap();
        let region = computation.regions.entry(key).or_insert_with(|| Region {
            owned: BTreeSet::new(),
            ready: None,
            basis: BTreeMap::new(),
            roots: roots.clone(),
            domain: domain.clone(),
            source_fingerprint: None,
            task: None,
        });
        if region.roots != roots || region.domain != domain {
            region.roots = roots;
            region.domain = domain;
            region.task = None;
            region.ready = None;
            region.source_fingerprint = None;
        }
    }
    let domain: BTreeSet<Coord> = computation
        .regions
        .values()
        .flat_map(|region| region.domain.iter().copied())
        .collect();
    journal.set_demand(domain.clone());
    computation.local_cache.lock().unwrap().retain(&domain);
    let local_cache = computation.local_cache.clone();
    computation.operations_last_tick = 0;
    let keys: Vec<_> = computation.regions.keys().copied().collect();
    published.pending.clear();
    for offset in 0..keys.len() {
        let key = keys[(computation.cursor + offset) % keys.len()];
        let remaining = computation.operation_budget - computation.operations_last_tick;
        let region = computation.regions.get_mut(&key).unwrap();
        let Some((source, fingerprint)) = capture(region, &world) else {
            published.pending.insert(key);
            continue;
        };
        if region.source_fingerprint != Some(fingerprint) {
            region.ready = None;
            let mut source = source.expect("changed source fingerprint");
            source.cache = local_cache.clone();
            region.basis = source
                .chunks
                .values()
                .flat_map(|chunk| {
                    chunk
                        .dependencies
                        .iter()
                        .filter_map(|(coord, revision)| revision.map(|revision| (*coord, revision)))
                })
                .collect();
            let budget = Arc::new(Budget::default());
            let view = View {
                source: Arc::new(source),
                budget: budget.clone(),
                dependencies: Arc::default(),
            };
            let roots = region.roots.clone();
            let owned = region.owned.clone();
            let records = identities
                .records
                .values()
                .filter(|record| {
                    record.active
                        && !membership_chunks(&record.membership).is_disjoint(&region.domain)
                })
                .map(|record| (record.id, record.membership.clone()))
                .collect();
            region.task = Some(Mutex::new(Computation::new(budget, async move {
                let mut topology = solver::topology::detect(&view).await;
                let scope =
                    retain_demanded_spaces(&view, &roots, owned, records, &mut topology).await;
                (topology, scope)
            })));
            region.source_fingerprint = Some(fingerprint);
        }
        if remaining == 0 {
            if region.task.is_some() || region.ready.is_some() {
                published.pending.insert(key);
            }
            continue;
        }
        let (result, used) = if let Some(ready) = region.ready.take() {
            (Poll::Ready(ready), 0)
        } else {
            let Some(task) = region.task.as_mut() else {
                continue;
            };
            let task = task.get_mut().unwrap();
            let before = task.operations;
            let result = task.advance(remaining.div_ceil(keys.len() - offset));
            (result, task.operations - before)
        };
        if let Poll::Ready((topology, scope)) = result {
            region.task = None;
            let additional: BTreeSet<_> = topology
                .continuations
                .difference(&region.domain)
                .copied()
                .collect();
            if !additional.is_empty() {
                region.domain.extend(additional);
                region.source_fingerprint = None;
                published.pending.insert(key);
            } else {
                if !journal.has_capacity() {
                    region.ready = Some((topology, scope));
                    published.pending.insert(key);
                    computation.operations_last_tick += used;
                    continue;
                }
                let mut drafts = Vec::new();
                let mut values = Vec::new();
                let mut failed = false;
                for (key, space) in topology.spaces {
                    let Ok(membership) = Membership::from_cells(
                        space.floor.iter().map(|cell| [cell.0, cell.1, cell.2]),
                    ) else {
                        failed = true;
                        break;
                    };
                    let mut digest = blake3::Hasher::new();
                    digest.update(&membership.fingerprint());
                    // Connections and all measurements participate, so content
                    // edits emit Updated even when a display function is unchanged.
                    bincode::serde::encode_into_std_write(
                        &space,
                        &mut digest,
                        bincode::config::standard(),
                    )
                    .expect("hash writer is infallible");
                    for connection in &topology.connections {
                        if connection.ends.iter().any(|ends| ends.contains(&key)) {
                            bincode::serde::encode_into_std_write(
                                connection,
                                &mut digest,
                                bincode::config::standard(),
                            )
                            .expect("hash writer is infallible");
                        }
                    }
                    drafts.push(SpaceDraft {
                        membership,
                        content_fingerprint: *digest.finalize().as_bytes(),
                    });
                    let connections = topology
                        .connections
                        .iter()
                        .filter(|connection| connection.ends.iter().any(|ends| ends.contains(&key)))
                        .cloned()
                        .collect();
                    values.push((space, connections));
                }
                if !failed {
                    match identities.reconcile(&scope, &drafts) {
                        Ok((ids, events)) => {
                            region.owned = ids.iter().copied().collect();
                            for id in scope {
                                published.geometry.remove(&id);
                                published.connections.remove(&id);
                                published.basis.remove(&id);
                            }
                            for (id, (space, connections)) in ids.into_iter().zip(values) {
                                published.geometry.insert(id, space);
                                published.connections.insert(id, connections);
                                published.basis.insert(id, region.basis.clone());
                            }
                            published.revision =
                                journal.publish(&region.domain, &events, &identities);
                            published.events = events;
                        }
                        Err(error) => {
                            warn!(%error,"spatial identity reconciliation rejected");
                            region.source_fingerprint = None;
                            published.pending.insert(key);
                        }
                    }
                }
            }
        } else {
            published.pending.insert(key);
        }
        computation.operations_last_tick += used;
    }
    if !keys.is_empty() {
        computation.cursor = (computation.cursor + 1) % keys.len();
    }
    dependencies.0 = computation
        .regions
        .values()
        .flat_map(|region| region.domain.iter().copied())
        .collect();
}

pub fn request_dependency_chunks(
    dependencies: Res<SpatialDependencies>,
    facilities: Option<Res<crate::facility_access::FacilityDependencies>>,
    capabilities: Option<Res<crate::capabilities::CapabilityDependencies>>,
    settlements: Option<Res<crate::settlements::SettlementDependencies>>,
    map: Res<ChunkMap>,
    mut pending: ResMut<crate::server::PendingChunks>,
    slots: Res<crate::blocks::TerrainSlots>,
) {
    let mut remaining = 4;
    let wanted: BTreeSet<_> = dependencies
        .0
        .iter()
        .chain(
            settlements
                .iter()
                .flat_map(|demand| demand.roots.iter().chain(demand.reads.iter())),
        )
        .chain(facilities.iter().flat_map(|demand| demand.0.iter()))
        .chain(capabilities.iter().flat_map(|demand| demand.0.iter()))
        .copied()
        .collect();
    for coord in wanted {
        if remaining == 0 || pending.0.len() >= 32 {
            break;
        }
        if coord
            .iter()
            .any(|n| n.unsigned_abs() > 1_000_000 / CHUNK_SIZE)
        {
            continue;
        }
        let coord = ChunkCoord(IVec3::from_array(coord));
        if map.0.contains_key(&coord) || pending.0.contains_key(&coord) {
            continue;
        }
        let slots = *slots;
        pending.0.insert(
            coord,
            bevy::tasks::AsyncComputeTaskPool::get()
                .spawn(async move { Chunk::from_terrain(coord, &slots) }),
        );
        remaining -= 1;
    }
}

#[cfg(test)]
mod demand_tests {
    use super::*;
    use solver::{
        Cell,
        topology::{Boundary, Ceiling, Space, Volume},
    };
    fn sample(cell: Cell) -> Space {
        Space {
            floor: BTreeSet::from([cell]),
            roofs: BTreeMap::from([(cell, Ceiling::Unknown)]),
            clearance: BTreeMap::from([(cell, Ceiling::Unknown)]),
            volume: Volume::Unknown,
            boundary: Boundary::Unknown,
            pending: true,
            placements: BTreeSet::new(),
        }
    }
    fn source() -> View {
        let (blocks, _) = crate::blocks::BlockRegistry::build(Vec::new()).unwrap();
        View {
            source: Arc::new(LiveSource::capture(
                BTreeMap::new(),
                BTreeMap::new(),
                &blocks,
                &crate::placements::Placements::default(),
            )),
            budget: Arc::default(),
            dependencies: Arc::default(),
        }
    }
    fn selected(records: Vec<(SpaceId, Membership)>) -> (Topology, BTreeSet<SpaceId>) {
        let a = Cell(1, 1, 1);
        let b = Cell(63, 1, 1);
        let topology = Topology {
            spaces: BTreeMap::from([(a, sample(a)), (b, sample(b))]),
            floor_index: BTreeMap::from([(a, a), (b, b)]),
            dependencies: BTreeSet::from([Cell(64, 1, 1)]),
            continuations: BTreeSet::from([[2, 0, 0]]),
            ..Default::default()
        };
        let view = source();
        let mut work = Computation::new(view.budget.clone(), async move {
            let mut topology = topology;
            let scope = retain_demanded_spaces(
                &view,
                &BTreeSet::from([[0, 0, 0]]),
                BTreeSet::new(),
                records,
                &mut topology,
            )
            .await;
            (topology, scope)
        });
        loop {
            if let Poll::Ready(result) = work.advance(7) {
                return result;
            }
        }
    }
    #[test]
    fn unrelated_dependency_cave_has_no_publication_or_continuation() {
        let records = vec![
            (SpaceId(1), Membership::from_cells([[1, 1, 1]]).unwrap()),
            (SpaceId(2), Membership::from_cells([[63, 1, 1]]).unwrap()),
        ];
        let (topology, scope) = selected(records);
        assert_eq!(
            topology.spaces.keys().copied().collect::<Vec<_>>(),
            vec![Cell(1, 1, 1)]
        );
        assert!(topology.continuations.is_empty());
        assert_eq!(
            scope,
            BTreeSet::from([SpaceId(1)]),
            "unsubscribed identity must not be retired"
        );
    }
    #[test]
    fn split_children_of_a_demanded_old_space_remain_in_its_lineage_scope() {
        let (topology, scope) = selected(vec![(
            SpaceId(1),
            Membership::from_cells([[1, 1, 1], [63, 1, 1]]).unwrap(),
        )]);
        assert_eq!(topology.spaces.len(), 2);
        assert_eq!(scope, BTreeSet::from([SpaceId(1)]));
        assert!(topology.continuations.contains(&[2, 0, 0]));
    }
}

#[cfg(test)]
mod large_cave_tests {
    use super::*;
    #[test]
    fn a_requested_cave_converges_across_64_chunks_without_censusing_other_caves() {
        let defs=serde_json::from_value(serde_json::json!([{"id":"test:stone","display_name":"Stone","color":[1,1,1],"flags":{},"structure":{"solid_cells":[[0,0,0]],"supports":[[0,0,0]],"shelter_cells":[[0,0,0]]}}])).unwrap();
        let (blocks, _) = crate::blocks::BlockRegistry::build(defs).unwrap();
        let stone = blocks.require("test:stone");
        let preset=serde_json::from_value(serde_json::json!({"id":"test:world","display_name":"Test world","terrain":{"stone":"test:stone","dirt":"test:stone","grass":"test:stone","wood":"test:stone","leaves":"test:stone","gravel":"test:stone","forage":"test:stone"},"generation":block_junk_mod_api::worlds::GenerationConfig::default(),"player_spawn":[2,1,2],"movement_profile":"test:walker","settlement":"test:settlement"})).unwrap();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(blocks)
            .insert_resource(crate::scripting::SelectedWorld(preset))
            .init_resource::<ChunkMap>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::server::SpatialSubscriptions>()
            .init_resource::<crate::server::ActiveSimulationChunks>()
            .init_resource::<StructuralSummaries>()
            .init_resource::<SpatialDependencies>()
            .init_resource::<RoofEnvelope>()
            .init_resource::<SpatialComputation>()
            .init_resource::<PublishedSpaces>()
            .init_resource::<crate::spatial_events::SpatialJournal>()
            .init_resource::<SpaceIdentities>()
            .add_systems(
                Update,
                (
                    crate::spatial_geometry::refresh_structural_summaries,
                    crate::spatial_geometry::materialize_facilities,
                    compute_spaces,
                )
                    .chain(),
            );
        for cx in (0..64).chain(200..216) {
            let coord = ChunkCoord(IVec3::new(cx, 0, 0));
            let mut chunk = Chunk {
                revision: 0,
                blocks: vec![stone; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
            };
            for x in 0..32 {
                for y in 1..4 {
                    for z in 1..6 {
                        let world = IVec3::new(cx * 32 + x, y, z);
                        if world.x != 0 && world.x != 64 * 32 - 1 {
                            chunk.set(
                                crate::voxel::world_to_chunk(world).1,
                                crate::blocks::BlockSlot::EMPTY,
                            );
                        }
                    }
                }
            }
            let entity = app
                .world_mut()
                .spawn((chunk, crate::voxel::ChunkEntities::default()))
                .id();
            app.world_mut()
                .resource_mut::<ChunkMap>()
                .0
                .insert(coord, entity);
        }
        app.world_mut()
            .resource_mut::<crate::server::ActiveSimulationChunks>()
            .0
            .insert(ChunkCoord(IVec3::ZERO));
        let expected = (64 * 32 - 2) * 5;
        let mut converged = false;
        let mut tick_times = Vec::new();
        crate::allocation_counter::reset();
        let started = std::time::Instant::now();
        for tick in 0..50_000 {
            let tick_started = std::time::Instant::now();
            app.update();
            tick_times.push(tick_started.elapsed());
            let work = app.world().resource::<SpatialComputation>();
            assert!(work.operations_last_tick <= work.operation_budget);
            let published = app.world().resource::<PublishedSpaces>();
            if published.pending.is_empty()
                && published
                    .geometry
                    .values()
                    .any(|space| space.floor.len() == expected)
            {
                tick_times.sort_unstable();
                let percentile = |fraction: f64| {
                    tick_times[((tick_times.len() - 1) as f64 * fraction).round() as usize]
                };
                let (allocations, allocated_bytes) = crate::allocation_counter::snapshot();
                eprintln!(
                    "64-chunk cave: ticks={tick} wall_ms={} p50_us={} p95_us={} p99_us={} max_us={} allocations={} allocated_bytes={}",
                    started.elapsed().as_millis(),
                    percentile(0.50).as_micros(),
                    percentile(0.95).as_micros(),
                    percentile(0.99).as_micros(),
                    tick_times.last().unwrap().as_micros(),
                    allocations,
                    allocated_bytes,
                );
                converged = true;
                break;
            }
        }
        assert!(
            converged,
            "64-chunk cave must eventually settle: domains {:?}, summaries {}, pending {:?}, spaces {:?}",
            app.world()
                .resource::<SpatialComputation>()
                .regions
                .values()
                .map(|region| (
                    &region.domain,
                    region
                        .task
                        .as_ref()
                        .map(|task| task.lock().unwrap().operations)
                ))
                .collect::<Vec<_>>(),
            app.world()
                .resource::<StructuralSummaries>()
                .completed
                .len(),
            app.world().resource::<PublishedSpaces>().pending,
            app.world()
                .resource::<PublishedSpaces>()
                .geometry
                .values()
                .map(|space| (space.floor.len(), space.pending))
                .collect::<Vec<_>>()
        );
        let published = app.world().resource::<PublishedSpaces>();
        assert_eq!(published.geometry.len(), 1);
        let space = published.geometry.values().next().unwrap();
        assert_eq!(
            space.volume,
            solver::topology::Volume::Bounded((expected * 3) as u64)
        );
        assert!(!space.pending);
        assert!(
            app.world()
                .resource::<SpatialDependencies>()
                .0
                .iter()
                .all(|coord| coord[0] < 64)
        );
        assert!(
            app.world()
                .resource::<StructuralSummaries>()
                .completed
                .keys()
                .all(|coord| coord[0] < 64)
        );
        assert_eq!(
            app.world().resource::<SpaceIdentities>().records.len(),
            1,
            "unrequested caves allocate no identities"
        );
    }

    #[test]
    #[ignore = "reproducible scale benchmark; run with --ignored --nocapture"]
    fn requested_space_and_policy_scale_matrix() {
        fn percentile(samples: &mut [std::time::Duration], fraction: f64) -> u128 {
            samples.sort_unstable();
            samples[((samples.len() - 1) as f64 * fraction).round() as usize].as_micros()
        }
        fn geometry_case(count: i32) -> (Vec<std::time::Duration>, usize, usize, u64, u64) {
            let defs=serde_json::from_value(serde_json::json!([{"id":"test:stone","display_name":"Stone","color":[1,1,1],"flags":{},"structure":{"solid_cells":[[0,0,0]],"supports":[[0,0,0]],"shelter_cells":[[0,0,0]]}}])).unwrap();
            let (blocks, _) = crate::blocks::BlockRegistry::build(defs).unwrap();
            let stone = blocks.require("test:stone");
            let preset=serde_json::from_value(serde_json::json!({"id":"test:world","display_name":"Test world","terrain":{"stone":"test:stone","dirt":"test:stone","grass":"test:stone","wood":"test:stone","leaves":"test:stone","gravel":"test:stone","forage":"test:stone"},"generation":block_junk_mod_api::worlds::GenerationConfig::default(),"player_spawn":[2,1,2],"movement_profile":"test:walker","settlement":"test:settlement"})).unwrap();
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .insert_resource(blocks)
                .insert_resource(crate::scripting::SelectedWorld(preset))
                .init_resource::<ChunkMap>()
                .init_resource::<crate::placements::Placements>()
                .init_resource::<crate::server::SpatialSubscriptions>()
                .init_resource::<crate::server::ActiveSimulationChunks>()
                .init_resource::<StructuralSummaries>()
                .init_resource::<SpatialDependencies>()
                .init_resource::<RoofEnvelope>()
                .init_resource::<SpatialComputation>()
                .init_resource::<PublishedSpaces>()
                .init_resource::<crate::spatial_events::SpatialJournal>()
                .init_resource::<SpaceIdentities>()
                .add_systems(
                    Update,
                    (
                        crate::spatial_geometry::refresh_structural_summaries,
                        crate::spatial_geometry::materialize_facilities,
                        compute_spaces,
                    )
                        .chain(),
                );
            for index in 0..count {
                let coord = ChunkCoord(IVec3::new(index * 3, 0, 0));
                let mut chunk = Chunk {
                    revision: 0,
                    blocks: vec![stone; (crate::protocol::CHUNK_PADDED as usize).pow(3)],
                };
                let origin = coord.0 * crate::protocol::CHUNK_SIZE as i32;
                for x in 2..9 {
                    for y in 1..4 {
                        for z in 2..9 {
                            let cell = origin + IVec3::new(x, y, z);
                            chunk.set(
                                crate::voxel::world_to_chunk(cell).1,
                                crate::blocks::BlockSlot::EMPTY,
                            );
                        }
                    }
                }
                let entity = app
                    .world_mut()
                    .spawn((chunk, crate::voxel::ChunkEntities::default()))
                    .id();
                app.world_mut()
                    .resource_mut::<ChunkMap>()
                    .0
                    .insert(coord, entity);
                app.world_mut()
                    .resource_mut::<crate::server::ActiveSimulationChunks>()
                    .0
                    .insert(coord);
            }
            crate::allocation_counter::reset();
            let mut times = Vec::new();
            for _ in 0..20_000 {
                let now = std::time::Instant::now();
                app.update();
                times.push(now.elapsed());
                let published = app.world().resource::<PublishedSpaces>();
                if published.pending.is_empty() && published.geometry.len() == count as usize {
                    break;
                }
            }
            assert_eq!(
                app.world().resource::<PublishedSpaces>().geometry.len(),
                count as usize
            );
            let scope = app
                .world()
                .resource::<crate::server::ActiveSimulationChunks>()
                .0
                .iter()
                .map(|coord| coord.0.to_array())
                .collect();
            let wire_bytes = crate::space_network::snapshot_wire_bytes(
                scope,
                app.world().resource::<PublishedSpaces>(),
                app.world().resource::<SpaceIdentities>(),
            )
            .unwrap();
            let queue = app
                .world()
                .resource::<SpatialComputation>()
                .regions
                .values()
                .filter(|region| region.task.is_some() || region.ready.is_some())
                .count();
            let (allocations, allocated_bytes) = crate::allocation_counter::snapshot();
            (times, wire_bytes, queue, allocations, allocated_bytes)
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/mods/independent");
        let mut content = crate::scripting::load_content(
            block_junk_mod_api::Side::Server,
            &root,
            Some("foundation:plain"),
        )
        .unwrap();
        content.mods.activate_runtime().unwrap();
        eprintln!(
            "spaces,npcs,convergence_ticks,tick_p50_us,tick_p95_us,tick_p99_us,policy_p50_us,policy_p95_us,policy_p99_us,framed_bytes,geometry_allocations,geometry_allocated_bytes,policy_allocations,policy_allocated_bytes,remaining_queue"
        );
        for spaces in [1, 10, 100] {
            let (mut geometry, wire_bytes, queue, geometry_allocations, geometry_allocated_bytes) =
                geometry_case(spaces);
            let convergence = geometry.len();
            let gp50 = percentile(&mut geometry.clone(), 0.50);
            let gp95 = percentile(&mut geometry.clone(), 0.95);
            let gp99 = percentile(&mut geometry, 0.99);
            for npcs in [10, 50, 100] {
                crate::allocation_counter::reset();
                let mut policy = Vec::new();
                for frame in 0..120 {
                    let now = std::time::Instant::now();
                    for npc in 0..npcs {
                        let snapshot: block_junk_mod_api::npcs::NpcSnapshot=serde_json::from_value(serde_json::json!({"id":npc+1,"kind":"foundation:settler","foot":{"x":0,"y":1,"z":0},"needs":{"work":0.9},"job_candidates":[{"id":format!("construction:{}",npc+1),"revision":1,"class":"construction","target":{"x":npc,"y":1,"z":frame%8},"estimated_cost":npc as u32,"requirements":[],"available":true}]})).unwrap();
                        let _ = content
                            .mods
                            .call_planner(&"foundation:settler".into(), &snapshot)
                            .unwrap();
                    }
                    policy.push(now.elapsed());
                }
                let pp50 = percentile(&mut policy.clone(), 0.50);
                let pp95 = percentile(&mut policy.clone(), 0.95);
                let pp99 = percentile(&mut policy, 0.99);
                let (policy_allocations, policy_allocated_bytes) =
                    crate::allocation_counter::snapshot();
                eprintln!(
                    "{spaces},{npcs},{convergence},{gp50},{gp95},{gp99},{pp50},{pp95},{pp99},{wire_bytes},{geometry_allocations},{geometry_allocated_bytes},{policy_allocations},{policy_allocated_bytes},{queue}"
                );
            }
        }
    }
}
