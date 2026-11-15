//! Deterministic facility components. Recognized empty geometry is never a seed.
use crate::{
    architecture_solver::{
        Budget, Computation,
        topology::{Boundary, Connection, Space},
    },
    space_identity::SpaceId,
    surface_navigation::SurfaceView,
};
use bevy::prelude::*;
use block_junk_mod_api::architecture::{MovementProfile, PlacementId, SettlementParticipation};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    task::Poll,
};
type Coord = [i32; 3];
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub id: PlacementId,
    pub facilities: BTreeSet<PlacementId>,
    pub min: IVec3,
    pub max: IVec3,
}
#[derive(Resource, Default)]
pub struct Settlements {
    pub groups: BTreeMap<PlacementId, Settlement>,
    pub by_facility: BTreeMap<PlacementId, PlacementId>,
    pub pending: bool,
    pub revision: u64,
}
impl Settlements {
    pub fn bounds_for_facility(
        &self,
        facility: PlacementId,
        buffer: i32,
    ) -> Option<(IVec3, IVec3)> {
        let group = self.groups.get(self.by_facility.get(&facility)?)?;
        let pad = IVec3::splat(buffer.max(0));
        Some((group.min - pad, group.max + pad))
    }
}
#[derive(Resource, Default)]
pub struct SettlementDependencies {
    pub roots: BTreeSet<Coord>,
    pub reads: BTreeSet<Coord>,
}
#[derive(Clone)]
struct Facility {
    id: PlacementId,
    role: SettlementParticipation,
    anchor: IVec3,
    approaches: Vec<IVec3>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Space(SpaceId),
    Facility(PlacementId),
}
fn root(parent: &BTreeMap<Node, Node>, mut node: Node) -> Node {
    while parent[&node] != node {
        node = parent[&node];
    }
    node
}
fn join(parent: &mut BTreeMap<Node, Node>, a: Node, b: Node) {
    let a = root(parent, a);
    let b = root(parent, b);
    parent.insert(a.max(b), a.min(b));
}
struct Reach {
    id: PlacementId,
    queue: BTreeSet<(u32, Coord)>,
    seen: BTreeMap<Coord, u32>,
}
struct Outcome {
    used_spaces: BTreeSet<SpaceId>,
    groups: BTreeMap<PlacementId, Settlement>,
    pending: bool,
}
async fn solve(
    view: SurfaceView,
    facilities: Vec<Facility>,
    spaces: Vec<(SpaceId, Space)>,
    connections: Vec<Connection>,
    profile: MovementProfile,
    range: u32,
) -> Outcome {
    let mut parent = BTreeMap::new();
    let mut floors = BTreeMap::new();
    let mut keys = BTreeMap::new();
    let mut room_bounds = BTreeMap::new();
    let participating: BTreeMap<_, _> = facilities
        .iter()
        .map(|facility| (facility.id, facility))
        .collect();
    for facility in &facilities {
        view.budget.step().await;
        let node = Node::Facility(facility.id);
        parent.insert(node, node);
    }
    let mut indoor = BTreeSet::new();
    for (id, space) in &spaces {
        view.budget.step().await;
        if space.pending || space.boundary != Boundary::Closed {
            continue;
        }
        let node = Node::Space(*id);
        parent.insert(node, node);
        let Some(key) = space.floor.first() else {
            continue;
        };
        keys.insert(*key, *id);
        let mut min = IVec3::splat(i32::MAX);
        let mut max = IVec3::splat(i32::MIN);
        for cell in &space.floor {
            view.budget.step().await;
            let cell = IVec3::new(cell.0, cell.1, cell.2);
            floors.insert(cell.to_array(), *id);
            min = min.min(cell);
            max = max.max(cell);
        }
        room_bounds.insert(*id, (min, max));
        for id in &space.placements {
            view.budget.step().await;
            let id = PlacementId(*id);
            if participating.contains_key(&id) {
                join(&mut parent, node, Node::Facility(id));
                indoor.insert(id);
            }
        }
    }
    let mut gateways = BTreeMap::<Coord, BTreeSet<Node>>::new();
    let mut observed = BTreeSet::new();
    for connection in connections {
        view.budget.step().await;
        if !observed.insert(connection.faces.clone()) {
            continue;
        }
        let rooms: BTreeSet<_> = connection
            .ends
            .iter()
            .flatten()
            .filter_map(|key| keys.get(key).copied())
            .collect();
        let Some(first) = rooms.first().copied() else {
            continue;
        };
        for room in &rooms {
            view.budget.step().await;
            join(&mut parent, Node::Space(first), Node::Space(*room));
        }
        for id in &connection.placements {
            let id = PlacementId(*id);
            view.budget.step().await;
            if participating.contains_key(&id) {
                join(&mut parent, Node::Space(first), Node::Facility(id));
                indoor.insert(id);
            }
        }
        let threshold = connection
            .threshold
            .iter()
            .copied()
            .chain(connection.faces.iter().flat_map(|face| [face.0, face.1]));
        for cell in threshold {
            for cell in std::iter::once(cell).chain(cell.horizontal()) {
                view.budget.step().await;
                let coord = [cell.0, cell.1, cell.2];
                if !floors.contains_key(&coord) {
                    gateways
                        .entry(coord)
                        .or_default()
                        .insert(Node::Space(first));
                }
            }
        }
    }
    let mut targets = BTreeMap::<Coord, BTreeSet<PlacementId>>::new();
    for facility in &facilities {
        for approach in &facility.approaches {
            view.budget.step().await;
            targets
                .entry(approach.to_array())
                .or_default()
                .insert(facility.id);
        }
    }
    let mut pending = false;
    let mut work = VecDeque::new();
    for facility in &facilities {
        if indoor.contains(&facility.id) {
            continue;
        }
        let mut reach = Reach {
            id: facility.id,
            queue: BTreeSet::new(),
            seen: BTreeMap::new(),
        };
        for &approach in &facility.approaches {
            view.budget.step().await;
            if floors.contains_key(&approach.to_array()) {
                continue;
            }
            match view.standable(approach, &profile).await {
                Some(true) if reach.seen.insert(approach.to_array(), 0).is_none() => {
                    reach.queue.insert((0, approach.to_array()));
                }
                None => pending = true,
                _ => {}
            }
        }
        if !reach.queue.is_empty() {
            work.push_back(reach);
        }
    }
    // Rotate facilities after every expanded surface; one large service area
    // cannot monopolize progress while another has a short reachable link.
    while let Some(mut reach) = work.pop_front() {
        let (distance, coord) = reach.queue.pop_first().unwrap();
        let cell = IVec3::from_array(coord);
        if reach.seen.get(&coord) != Some(&distance) {
            if !reach.queue.is_empty() {
                work.push_back(reach);
            }
            continue;
        }
        view.budget.step().await;
        if let Some(ids) = targets.get(&cell.to_array()) {
            for id in ids {
                join(&mut parent, Node::Facility(reach.id), Node::Facility(*id));
            }
        }
        if let Some(nodes) = gateways.get(&cell.to_array()) {
            for node in nodes {
                join(&mut parent, Node::Facility(reach.id), *node);
            }
        }
        if distance < range {
            for offset in [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z] {
                let mut floor_reached = false;
                for dy in
                    (0..=profile.step_up as i32).chain((1..=profile.max_drop as i32).map(|n| -n))
                {
                    view.budget.step().await;
                    if dy < 0 && floor_reached {
                        break;
                    }
                    let to = cell + offset + IVec3::Y * dy;
                    let cost = 1 + dy.unsigned_abs();
                    if reach
                        .seen
                        .get(&to.to_array())
                        .is_some_and(|old| *old <= distance + cost)
                    {
                        if dy >= 0 {
                            floor_reached = true;
                        }
                        continue;
                    }
                    if distance + cost > range || floors.contains_key(&to.to_array()) {
                        continue;
                    }
                    match view.step(cell, to, &profile).await {
                        Some(true) => match view.crosses_partition(cell, to, &profile).await {
                            Some(false) => {
                                floor_reached = true;
                                reach.seen.insert(to.to_array(), distance + cost);
                                reach.queue.insert((distance + cost, to.to_array()));
                            }
                            None => pending = true,
                            _ => {}
                        },
                        None => pending = true,
                        _ => {}
                    }
                }
            }
        }
        if !reach.queue.is_empty() {
            work.push_back(reach);
        }
    }
    let mut members = BTreeMap::<Node, BTreeSet<PlacementId>>::new();
    let mut seeds = BTreeMap::<Node, PlacementId>::new();
    for facility in &facilities {
        view.budget.step().await;
        let key = root(&parent, Node::Facility(facility.id));
        members.entry(key).or_default().insert(facility.id);
        if facility.role == SettlementParticipation::Seed {
            seeds
                .entry(key)
                .and_modify(|id| *id = (*id).min(facility.id))
                .or_insert(facility.id);
        }
    }
    let mut bounds = BTreeMap::<Node, (IVec3, IVec3)>::new();
    for (id, (min, max)) in room_bounds {
        view.budget.step().await;
        let key = root(&parent, Node::Space(id));
        bounds
            .entry(key)
            .and_modify(|(a, b)| {
                *a = a.min(min);
                *b = b.max(max);
            })
            .or_insert((min, max));
    }
    for facility in &facilities {
        view.budget.step().await;
        let key = root(&parent, Node::Facility(facility.id));
        bounds
            .entry(key)
            .and_modify(|(a, b)| {
                *a = a.min(facility.anchor);
                *b = b.max(facility.anchor);
            })
            .or_insert((facility.anchor, facility.anchor));
    }
    let used_spaces = parent
        .keys()
        .filter_map(|node| match node {
            Node::Space(id) if members.contains_key(&root(&parent, *node)) => Some(*id),
            _ => None,
        })
        .collect();
    let groups = members
        .into_iter()
        .filter_map(|(key, facilities)| {
            let id = *seeds.get(&key)?;
            let (min, max) = bounds[&key];
            Some((
                id,
                Settlement {
                    id,
                    facilities,
                    min,
                    max,
                },
            ))
        })
        .collect();
    Outcome {
        used_spaces,
        groups,
        pending,
    }
}
#[derive(Resource)]
pub struct SettlementWork {
    pub operation_budget: usize,
    pub operations_last_tick: usize,
    basis: Option<[u8; 32]>,
    task: Option<Mutex<Computation<Outcome>>>,
    view: Option<SurfaceView>,
}
impl Default for SettlementWork {
    fn default() -> Self {
        Self {
            operation_budget: 4096,
            operations_last_tick: 0,
            basis: None,
            task: None,
            view: None,
        }
    }
}
#[derive(bevy::ecs::system::SystemParam)]
pub struct SettlementWorld<'w, 's> {
    placements: Res<'w, crate::placements::Placements>,
    spaces: Res<'w, crate::live_spaces::PublishedSpaces>,
    summaries: Res<'w, crate::spatial_geometry::StructuralSummaries>,
    rules: Res<'w, crate::world_rules::WorldRules>,
    preset: Res<'w, crate::scripting::SelectedWorld>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
    map: Res<'w, crate::voxel::ChunkMap>,
}
pub fn refresh(
    world: SettlementWorld,
    mut work: ResMut<SettlementWork>,
    mut published: ResMut<Settlements>,
    mut demand: ResMut<SettlementDependencies>,
) {
    let Some(rule) = world.rules.settlements.get(&world.preset.0.settlement) else {
        published.pending = true;
        return;
    };
    let Some(profile) = world.rules.movement.get(&rule.movement_profile) else {
        published.pending = true;
        return;
    };
    let mut facilities = Vec::new();
    let mut digest = blake3::Hasher::new();
    digest.update(&world.spaces.revision.to_le_bytes());
    for record in world
        .placements
        .records
        .values()
        .filter(|record| record.active)
    {
        let Some(slot) = world.blocks.slot_of(&record.definition) else {
            continue;
        };
        let def = world.blocks.def(slot);
        if def.settlement == SettlementParticipation::None {
            continue;
        }
        digest.update(&record.id.0.to_le_bytes());
        digest.update(&record.revision.to_le_bytes());
        let anchor = IVec3::from_array(record.anchor);
        let approaches = world.blocks.architecture[slot.0 as usize][record.orientation as usize]
            .slots
            .iter()
            .flat_map(|slot| slot.approaches.iter())
            .map(|offset| anchor + IVec3::from_array(*offset))
            .collect();
        facilities.push(Facility {
            id: record.id,
            role: def.settlement,
            anchor,
            approaches,
        });
    }
    demand.roots = facilities
        .iter()
        .map(|facility| crate::voxel::world_to_chunk(facility.anchor).0.0.to_array())
        .collect();
    let basis = *digest.finalize().as_bytes();
    let valid = work.view.as_ref().is_none_or(|view| {
        view.dependencies
            .lock()
            .unwrap()
            .iter()
            .all(|(coord, revision)| {
                let current = world
                    .map
                    .0
                    .get(&crate::protocol::ChunkCoord(IVec3::from_array(*coord)))
                    .and_then(|entity| world.chunks.get(*entity).ok())
                    .map(|chunk| chunk.revision);
                current == *revision
            })
    });
    if work.basis != Some(basis) || !valid {
        work.task = None;
        work.view = None;
        work.basis = None;
    }
    if work.basis.is_none() {
        let budget = Arc::new(Budget::default());
        let view = SurfaceView {
            chunks: world
                .summaries
                .completed
                .iter()
                .filter(|(coord, _)| !world.summaries.pending.contains(*coord))
                .map(|(coord, chunk)| (*coord, chunk.clone()))
                .collect(),
            collision: world.blocks.collision.clone(),
            architecture: world.blocks.architecture.clone(),
            budget: budget.clone(),
            dependencies: Default::default(),
        };
        let spaces = world
            .spaces
            .geometry
            .iter()
            .map(|(id, space)| (*id, space.clone()))
            .collect();
        let connections = world
            .spaces
            .connections
            .values()
            .flatten()
            .cloned()
            .collect();
        work.task = Some(Mutex::new(Computation::new(
            budget,
            solve(
                view.clone(),
                facilities,
                spaces,
                connections,
                profile.clone(),
                rule.outdoor_link_range,
            ),
        )));
        work.view = Some(view);
        work.basis = Some(basis);
        published.pending = true;
    }
    work.operations_last_tick = 0;
    let budget = work.operation_budget;
    let Some(task) = work.task.as_mut() else {
        return;
    };
    let (result, used) = {
        let task = task.get_mut().unwrap();
        let before = task.operations;
        let result = task.advance(budget);
        (result, task.operations - before)
    };
    work.operations_last_tick = used;
    if let Some(view) = &work.view {
        demand.reads = view.dependencies.lock().unwrap().keys().copied().collect();
    }
    if let Poll::Ready(outcome) = result {
        work.task = None;
        let current = |coord: Coord| {
            world
                .map
                .0
                .get(&crate::protocol::ChunkCoord(IVec3::from_array(coord)))
                .and_then(|entity| world.chunks.get(*entity).ok())
                .map(|chunk| chunk.revision)
        };
        let queries_valid = work.view.as_ref().is_some_and(|view| {
            view.dependencies
                .lock()
                .unwrap()
                .iter()
                .all(|(coord, revision)| current(*coord) == *revision)
        });
        let spaces_valid = outcome.used_spaces.iter().all(|id| {
            world.spaces.basis.get(id).is_some_and(|basis| {
                basis
                    .iter()
                    .all(|(coord, revision)| current(*coord) == Some(*revision))
            })
        });
        if outcome.pending || !queries_valid || !spaces_valid {
            published.pending = true;
            work.basis = None;
            return;
        }
        if published.groups != outcome.groups {
            published.groups = outcome.groups;
            published.by_facility = published
                .groups
                .iter()
                .flat_map(|(id, group)| {
                    group
                        .facilities
                        .iter()
                        .map(move |facility| (*facility, *id))
                })
                .collect();
            published.revision = published
                .revision
                .checked_add(1)
                .expect("settlement revision exhausted");
        }
        published.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        architecture_solver::{
            Cell, Face,
            topology::{Ceiling, Volume},
        },
        blocks::{BlockRegistry, BlockSlot},
        spatial_geometry::{ChunkGeometry, StructuralCell},
    };
    fn fixture() -> (SurfaceView, MovementProfile) {
        let defs=serde_json::from_value(serde_json::json!([
            {"id":"test:floor","display_name":"Floor","flags":{"solid":true},"color":[1,1,1],"structure":{"solid_cells":[[0,0,0]],"supports":[[0,0,0]]}},
            {"id":"test:screen","display_name":"Screen","flags":{},"color":[1,1,1],"structure":{"partitions":[{"cell":[0,0,0],"normal":[1,0,0],"window":false}]}}
        ])).unwrap();
        let blocks = BlockRegistry::build(defs).unwrap().0;
        let mut chunk = ChunkGeometry::default();
        chunk.physical.fill(Some(BlockSlot::EMPTY));
        chunk.cells.fill(Some(StructuralCell::default()));
        for x in 0..32 {
            for z in 0..32 {
                let index = ChunkGeometry::cell_index(IVec3::new(x, 0, z));
                chunk.physical[index] = Some(blocks.require("test:floor"));
                chunk.cells[index] = Some(StructuralCell {
                    occupied: true,
                    supports: true,
                    shelters: false,
                });
            }
        }
        chunk.dependencies.insert([0, 0, 0], Some(1));
        let view = SurfaceView {
            chunks: BTreeMap::from([([0, 0, 0], Arc::new(chunk))]),
            collision: blocks.collision.clone(),
            architecture: blocks.architecture.clone(),
            budget: Arc::new(Budget::default()),
            dependencies: Default::default(),
        };
        let profile = MovementProfile {
            id: "test:walker".into(),
            height: 1.8,
            radius: 0.3,
            step_up: 1,
            max_drop: 3,
            stairs: true,
        };
        (view, profile)
    }
    fn facility(id: u64, x: i32, role: SettlementParticipation) -> Facility {
        Facility {
            id: PlacementId(id),
            role,
            anchor: IVec3::new(x, 1, 15),
            approaches: vec![IVec3::new(x, 1, 16)],
        }
    }
    fn run(
        view: SurfaceView,
        facilities: Vec<Facility>,
        spaces: Vec<(SpaceId, Space)>,
        connections: Vec<Connection>,
        profile: MovementProfile,
        range: u32,
    ) -> Outcome {
        let mut task = Computation::new(
            view.budget.clone(),
            solve(view, facilities, spaces, connections, profile, range),
        );
        for _ in 0..100_000 {
            let before = task.operations;
            if let Poll::Ready(outcome) = task.advance(37) {
                assert!(task.operations - before <= 37);
                return outcome;
            }
            assert!(task.operations - before <= 37);
        }
        panic!("finite settlement search did not converge");
    }
    #[test]
    fn outdoor_components_are_transitive_order_independent_and_require_a_seed() {
        let (view, profile) = fixture();
        let facilities = vec![
            facility(1, 12, SettlementParticipation::Seed),
            facility(2, 16, SettlementParticipation::Joiner),
            facility(3, 20, SettlementParticipation::Seed),
        ];
        let a = run(
            view.clone(),
            facilities.clone(),
            vec![],
            vec![],
            profile.clone(),
            4,
        );
        assert!(!a.pending);
        assert_eq!(a.groups.len(), 1);
        assert_eq!(
            a.groups[&PlacementId(1)].facilities,
            BTreeSet::from([PlacementId(1), PlacementId(2), PlacementId(3)])
        );
        let b = run(
            view.clone(),
            facilities.into_iter().rev().collect(),
            vec![],
            vec![],
            profile.clone(),
            4,
        );
        assert_eq!(a.groups, b.groups);
        let unseeded = run(
            view,
            vec![
                facility(1, 12, SettlementParticipation::Joiner),
                facility(2, 16, SettlementParticipation::Joiner),
            ],
            vec![],
            vec![],
            profile,
            4,
        );
        assert!(unseeded.groups.is_empty());
    }
    #[test]
    fn virtual_partitions_block_outdoor_links_and_unknown_geometry_stays_pending() {
        let (mut view, profile) = fixture();
        let chunk = Arc::make_mut(view.chunks.get_mut(&[0, 0, 0]).unwrap());
        for z in 0..32 {
            chunk.physical[ChunkGeometry::cell_index(IVec3::new(16, 1, z))] = Some(BlockSlot(2));
        }
        let facilities = vec![
            facility(1, 15, SettlementParticipation::Seed),
            facility(2, 18, SettlementParticipation::Seed),
        ];
        let blocked = run(
            view.clone(),
            facilities.clone(),
            vec![],
            vec![],
            profile.clone(),
            5,
        );
        assert!(!blocked.pending);
        assert_eq!(blocked.groups.len(), 2);
        view.chunks.clear();
        let unknown = run(view.clone(), facilities, vec![], vec![], profile, 5);
        assert!(unknown.pending);
        assert_eq!(view.dependencies.lock().unwrap()[&[0, 0, 0]], None);
    }
    fn room(id: u64, x: i32, placement: Option<u64>) -> (SpaceId, Space) {
        let cell = Cell(x, 1, 16);
        (
            SpaceId(id),
            Space {
                floor: BTreeSet::from([cell]),
                roofs: BTreeMap::from([(cell, Ceiling::At(Cell(x, 4, 16)))]),
                clearance: BTreeMap::from([(cell, Ceiling::At(Cell(x, 4, 16)))]),
                volume: Volume::Bounded(3),
                boundary: Boundary::Closed,
                pending: false,
                placements: placement.into_iter().collect(),
            },
        )
    }
    #[test]
    fn portal_state_does_not_change_settlement_identity_and_empty_caves_do_not_activate() {
        let (view, profile) = fixture();
        let spaces = vec![
            room(1, 12, Some(1)),
            room(2, 20, Some(2)),
            room(3, 24, None),
        ];
        let mut connection = Connection {
            ends: [
                BTreeSet::from([Cell(12, 1, 16)]),
                BTreeSet::from([Cell(20, 1, 16)]),
            ],
            faces: BTreeSet::from([Face::new(Cell(16, 1, 16), Cell(17, 1, 16))]),
            threshold: BTreeSet::from([Cell(16, 1, 16)]),
            volume: BTreeSet::from([Cell(16, 1, 16), Cell(16, 2, 16)]),
            evidence: BTreeSet::new(),
            traversable: true,
            placements: BTreeSet::new(),
        };
        let facilities = vec![
            facility(1, 12, SettlementParticipation::Seed),
            facility(2, 20, SettlementParticipation::Seed),
        ];
        let open = run(
            view.clone(),
            facilities.clone(),
            spaces.clone(),
            vec![connection.clone()],
            profile.clone(),
            1,
        );
        connection.traversable = false;
        let closed = run(
            view.clone(),
            facilities,
            spaces.clone(),
            vec![connection],
            profile.clone(),
            1,
        );
        assert_eq!(open.groups, closed.groups);
        assert_eq!(open.groups.len(), 1);
        let caves = run(view, vec![], spaces, vec![], profile, 1);
        assert!(caves.groups.is_empty());
        assert!(caves.used_spaces.is_empty());
    }
    #[test]
    fn physical_clearance_uses_the_actor_profile_and_rejects_furniture_obstructions() {
        let (mut view, profile) = fixture();
        let chunk = Arc::make_mut(view.chunks.get_mut(&[0, 0, 0]).unwrap());
        for x in [15, 17] {
            for y in 1..=2 {
                chunk.physical[ChunkGeometry::cell_index(IVec3::new(x, y, 16))] =
                    Some(BlockSlot(1));
            }
        }
        let narrow = profile.clone();
        let mut wide = profile;
        wide.radius = 0.7;
        let check = |view: SurfaceView, profile| {
            let budget = view.budget.clone();
            let mut task = Computation::new(budget, async move {
                view.standable(IVec3::new(16, 1, 16), &profile).await
            });
            loop {
                if let Poll::Ready(result) = task.advance(17) {
                    break result;
                }
            }
        };
        assert_eq!(check(view.clone(), narrow), Some(true));
        assert_eq!(check(view, wide), Some(false));
    }
}
