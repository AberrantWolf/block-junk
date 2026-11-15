//! Coherent, budgeted additive capability publication over live architecture.
mod service_area;
use crate::{
    architecture_solver::{Budget, Cell, Computation, topology::Space},
    capability_input::{Object, Observations},
    space_identity::SpaceId,
};
use bevy::prelude::*;
use block_junk_mod_api::{
    architecture::{PlacementId, UseSlotId},
    capabilities::{CapabilityDefinition, CapabilityResult, CapabilityScope, ObservationQuery},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    task::Poll,
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceCapabilities {
    pub profiles: BTreeMap<String, Vec<CapabilityResult>>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub revision: u64,
    pub spatial_revision: u64,
    pub access_revision: u64,
    pub spaces: BTreeMap<SpaceId, SpaceCapabilities>,
    pub outdoor: Vec<OutdoorCapability>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutdoorCapability {
    pub anchor: UseSlotId,
    pub profile: String,
    pub range: u32,
    pub allow_portals: bool,
    pub area: service_area::ServiceArea,
    pub result: CapabilityResult,
}
#[derive(Resource, Default)]
pub struct CapabilityDependencies(pub BTreeSet<[i32; 3]>);
#[derive(Default)]
pub(crate) struct Evaluation {
    pub(crate) spaces: BTreeMap<SpaceId, SpaceCapabilities>,
    pub(crate) outdoor: Vec<OutdoorCapability>,
}
#[derive(Resource, Default)]
pub struct Capabilities {
    pub snapshot: CapabilitySnapshot,
    pub pending: bool,
}
#[derive(Resource)]
pub struct CapabilityWork {
    pub operation_budget: usize,
    pub operations_last_tick: usize,
    signature: Option<[u8; 32]>,
    task: Option<Mutex<Computation<Evaluation>>>,
    view: Option<crate::surface_navigation::SurfaceView>,
}
impl Default for CapabilityWork {
    fn default() -> Self {
        Self {
            operation_budget: 8192,
            operations_last_tick: 0,
            signature: None,
            task: None,
            view: None,
        }
    }
}
#[derive(Clone)]
pub(crate) struct Facility {
    pub(crate) id: PlacementId,
    pub(crate) cells: BTreeSet<Cell>,
    pub(crate) tags: BTreeSet<String>,
    pub(crate) attributes: BTreeMap<String, f64>,
    pub(crate) slots: BTreeMap<String, Vec<crate::facility_access::SlotAccess>>,
}
pub(crate) struct Input {
    pub(crate) spaces: BTreeMap<SpaceId, Space>,
    pub(crate) facilities: BTreeMap<PlacementId, Facility>,
    pub(crate) profiles: BTreeMap<String, block_junk_mod_api::architecture::MovementProfile>,
    pub(crate) anchors: BTreeSet<PlacementId>,
    pub(crate) connections: Vec<crate::architecture_solver::topology::Connection>,
    pub(crate) view: crate::surface_navigation::SurfaceView,
    pub(crate) rules: Vec<CapabilityDefinition>,
    pub(crate) reserved: BTreeSet<UseSlotId>,
    pub(crate) reservations_known: bool,
    pub(crate) pending: BTreeSet<PlacementId>,
}
pub(crate) async fn evaluate(input: Input, budget: Arc<Budget>) -> Evaluation {
    let rules: Vec<_> = input
        .rules
        .iter()
        .filter(|rule| matches!(rule.scope, CapabilityScope::ArchitecturalSpace))
        .collect();
    let mut queries = BTreeSet::<ObservationQuery>::new();
    for rule in &rules {
        for requirement in &rule.requirements {
            requirement.condition.observations(&mut queries);
        }
        if let Some(capacity) = &rule.capacity {
            capacity.observations(&mut queries);
        }
        for attribute in rule.attributes.values() {
            attribute.observations(&mut queries);
        }
    }
    let mut output = BTreeMap::new();
    for (id, space) in &input.spaces {
        let mut result = SpaceCapabilities::default();
        for profile in input.profiles.keys() {
            let mut objects = Vec::new();
            for id in &space.placements {
                budget.step().await;
                let Some(facility) = input.facilities.get(&PlacementId(*id)) else {
                    continue;
                };
                objects.push(Object {
                    id: facility.id,
                    cells: facility.cells.clone(),
                    tags: facility.tags.clone(),
                    attributes: facility.attributes.clone(),
                    slots: facility.slots.get(profile).cloned().unwrap_or_default(),
                });
            }
            let mut observations = Observations(BTreeMap::new());
            for query in &queries {
                observations.0.insert(
                    query.clone(),
                    crate::capability_input::observe(
                        query,
                        Some(space),
                        &objects,
                        &input.reserved,
                        &input.pending,
                        &budget,
                    )
                    .await,
                );
            }
            if !input.reservations_known {
                mask_unknown_reservations(&mut observations);
            }
            let mut results = Vec::new();
            for rule in &rules {
                let operations: usize = rule
                    .requirements
                    .iter()
                    .map(|requirement| requirement.condition.node_count())
                    .chain(
                        rule.capacity
                            .iter()
                            .map(|expression| expression.node_count()),
                    )
                    .chain(
                        rule.attributes
                            .values()
                            .map(|expression| expression.node_count()),
                    )
                    .sum();
                for _ in 0..operations.max(1) {
                    budget.step().await;
                }
                results.push(rule.evaluate(&observations));
            }
            result.profiles.insert(profile.clone(), results);
        }
        output.insert(*id, result);
    }
    let outdoor = evaluate_outdoor(&input).await;
    Evaluation {
        spaces: output,
        outdoor,
    }
}

fn mask_unknown_reservations(observations: &mut Observations) {
    for (query, evidence) in &mut observations.0 {
        if matches!(
            query,
            ObservationQuery::Slots {
                capacity: block_junk_mod_api::capabilities::CapacityKind::Available,
                ..
            }
        ) {
            *evidence = block_junk_mod_api::capabilities::Evidence::unknown(
                query.label(),
                "requires authoritative reservation validation".into(),
            );
        }
    }
}

async fn evaluate_outdoor(input: &Input) -> Vec<OutdoorCapability> {
    use block_junk_mod_api::capabilities::{CapabilityStatus, Evidence};
    let mut output = Vec::new();
    for rule in &input.rules {
        let CapabilityScope::Outdoor {
            anchor_interaction,
            range,
            allow_portals,
        } = &rule.scope
        else {
            continue;
        };
        let mut queries = BTreeSet::new();
        for requirement in &rule.requirements {
            requirement.condition.observations(&mut queries);
        }
        for expr in rule.capacity.iter().chain(rule.attributes.values()) {
            expr.observations(&mut queries);
        }
        for (profile, movement) in &input.profiles {
            for facility in input
                .facilities
                .values()
                .filter(|facility| input.anchors.contains(&facility.id))
            {
                for anchor in facility
                    .slots
                    .get(profile)
                    .into_iter()
                    .flatten()
                    .filter(|slot| slot.interactions.contains(anchor_interaction))
                {
                    let starts = anchor
                        .approaches
                        .iter()
                        .map(|approach| {
                            let [x, y, z] = approach.cell;
                            Cell(x, y, z)
                        })
                        .collect();
                    let area = service_area::compute(
                        &input.view,
                        &starts,
                        movement,
                        *range,
                        *allow_portals,
                        &input.connections,
                    )
                    .await;
                    let mut objects = Vec::new();
                    for candidate in input.facilities.values() {
                        input.view.budget.step().await;
                        let slots: Vec<_> = candidate
                            .slots
                            .get(profile)
                            .into_iter()
                            .flatten()
                            .filter(|slot| {
                                slot.approaches.iter().any(|approach| {
                                    let [x, y, z] = approach.cell;
                                    area.surfaces.contains(&Cell(x, y, z))
                                })
                            })
                            .cloned()
                            .collect();
                        let mut touches = !slots.is_empty();
                        for cell in &candidate.cells {
                            input.view.budget.step().await;
                            touches |= area.surfaces.contains(cell)
                                || cell
                                    .horizontal()
                                    .iter()
                                    .any(|cell| area.surfaces.contains(cell));
                        }
                        if touches {
                            objects.push(Object {
                                id: candidate.id,
                                cells: candidate.cells.clone(),
                                tags: candidate.tags.clone(),
                                attributes: candidate.attributes.clone(),
                                slots,
                            });
                        }
                    }
                    let mut observations = Observations(BTreeMap::new());
                    for query in &queries {
                        let evidence = if area.pending {
                            Evidence::unknown(
                                query.label(),
                                "service-area reachability is pending".into(),
                            )
                        } else {
                            crate::capability_input::observe(
                                query,
                                None,
                                &objects,
                                &input.reserved,
                                &input.pending,
                                &input.view.budget,
                            )
                            .await
                        };
                        observations.0.insert(query.clone(), evidence);
                    }
                    if !input.reservations_known {
                        mask_unknown_reservations(&mut observations);
                    }
                    let cost: usize = rule
                        .requirements
                        .iter()
                        .map(|requirement| requirement.condition.node_count())
                        .chain(
                            rule.capacity
                                .iter()
                                .chain(rule.attributes.values())
                                .map(|expr| expr.node_count()),
                        )
                        .sum();
                    for _ in 0..cost.max(1) {
                        input.view.budget.step().await;
                    }
                    let mut result = rule.evaluate(&observations);
                    if area.pending {
                        result.status = CapabilityStatus::Pending;
                    }
                    output.push(OutdoorCapability {
                        anchor: anchor.id.clone(),
                        profile: profile.clone(),
                        range: *range,
                        allow_portals: *allow_portals,
                        area,
                        result,
                    });
                }
            }
        }
    }
    output
}
#[derive(bevy::ecs::system::SystemParam)]
pub struct CapabilityWorld<'w, 's> {
    spaces: Res<'w, crate::live_spaces::PublishedSpaces>,
    summaries: Res<'w, crate::spatial_geometry::StructuralSummaries>,
    access: Res<'w, crate::facility_access::FacilityAccess>,
    placements: Res<'w, crate::placements::Placements>,
    claims: Res<'w, crate::interactables::InteractionClaims>,
    rules: Res<'w, crate::world_rules::WorldRules>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    roots: Res<'w, crate::live_spaces::SpatialDependencies>,
    map: Res<'w, crate::voxel::ChunkMap>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
}
pub fn refresh(
    world: CapabilityWorld,
    mut work: ResMut<CapabilityWork>,
    mut published: ResMut<Capabilities>,
    mut dependencies: ResMut<CapabilityDependencies>,
) {
    work.operations_last_tick = 0;
    let valid = |view: &crate::surface_navigation::SurfaceView| {
        view.dependencies
            .lock()
            .unwrap()
            .iter()
            .all(|(coord, revision)| {
                world
                    .map
                    .0
                    .get(&crate::protocol::ChunkCoord(IVec3::from_array(*coord)))
                    .and_then(|entity| world.chunks.get(*entity).ok())
                    .map(|chunk| chunk.revision)
                    == *revision
            })
    };
    if work.view.as_ref().is_some_and(|view| !valid(view)) {
        work.signature = None;
        work.task = None;
        work.view = None;
    }
    let active: BTreeSet<_> = world
        .spaces
        .basis
        .iter()
        .filter(|(_, basis)| basis.keys().any(|coord| world.roots.0.contains(coord)))
        .map(|(id, _)| *id)
        .collect();
    let coherent = active.iter().all(|id| {
        world.spaces.basis[id].iter().all(|(coord, revision)| {
            !world.spaces.pending.contains(coord)
                && world
                    .map
                    .0
                    .get(&crate::protocol::ChunkCoord(IVec3::from_array(*coord)))
                    .and_then(|entity| world.chunks.get(*entity).ok())
                    .is_some_and(|chunk| chunk.revision == *revision)
        })
    });
    if !coherent {
        published.pending = true;
        work.signature = None;
        work.task = None;
        return;
    }
    let reserved: BTreeSet<_> = world
        .access
        .slots
        .keys()
        .filter_map(|(slot, _)| world.claims.slot_reserved(slot).then_some(slot.clone()))
        .collect();
    let mut digest = blake3::Hasher::new();
    digest.update(&world.spaces.revision.to_le_bytes());
    digest.update(&world.access.revision.to_le_bytes());
    digest.update(
        &serde_json::to_vec(&(&active, &reserved, &world.access.pending))
            .expect("bounded engine identities"),
    );
    for rule in world.rules.capabilities.values() {
        digest.update(&serde_json::to_vec(rule).expect("frozen capability"));
    }
    let signature = *digest.finalize().as_bytes();
    if work.signature != Some(signature) {
        let budget = Arc::new(Budget::default());
        let view = crate::surface_navigation::SurfaceView {
            chunks: world
                .summaries
                .completed
                .iter()
                .filter(|(coord, _)| !world.summaries.pending.contains(*coord))
                .map(|(coord, summary)| (*coord, summary.clone()))
                .collect(),
            collision: world.blocks.collision.clone(),
            architecture: world.blocks.architecture.clone(),
            budget: budget.clone(),
            dependencies: Default::default(),
        };
        let mut facilities = BTreeMap::new();
        let mut demanded: BTreeSet<_> = world
            .spaces
            .geometry
            .iter()
            .filter(|(id, _)| active.contains(id))
            .flat_map(|(_, space)| space.placements.iter().copied())
            .collect();
        let anchors: BTreeSet<_> = world
            .access
            .slots
            .keys()
            .map(|(id, _)| id.placement)
            .filter(|id| {
                world.placements.records.get(id).is_some_and(|record| {
                    demanded.contains(&id.0)
                        || world.roots.0.contains(
                            &crate::voxel::world_to_chunk(IVec3::from_array(record.anchor))
                                .0
                                .0
                                .to_array(),
                        )
                })
            })
            .collect();
        demanded.extend(anchors.iter().map(|id| id.0));
        if world
            .rules
            .capabilities
            .values()
            .any(|rule| matches!(rule.scope, CapabilityScope::Outdoor { .. }))
        {
            demanded.extend(
                view.chunks
                    .values()
                    .flat_map(|summary| summary.elements.keys())
                    .filter_map(|anchor| {
                        world
                            .placements
                            .at(IVec3::from_array(*anchor))
                            .map(|record| record.id.0)
                    }),
            );
        }
        for record in world
            .placements
            .records
            .values()
            .filter(|record| record.active && demanded.contains(&record.id.0))
        {
            let Some(slot) = world.blocks.slot_of(&record.definition) else {
                continue;
            };
            let def = world.blocks.def(slot);
            let anchor = IVec3::from_array(record.anchor);
            let mut slots = BTreeMap::<String, Vec<_>>::new();
            for profile in world.rules.movement.keys() {
                for slot in
                    &world.blocks.architecture[slot.0 as usize][record.orientation as usize].slots
                {
                    let id = UseSlotId {
                        placement: record.id,
                        key: slot.key.clone(),
                    };
                    let access = world
                        .access
                        .slots
                        .get(&(id.clone(), profile.clone()))
                        .filter(|access| access.placement_revision == record.revision)
                        .cloned()
                        .unwrap_or_else(|| crate::facility_access::SlotAccess {
                            id,
                            placement_revision: record.revision,
                            profile: profile.clone(),
                            interactions: slot.interactions.clone(),
                            exclusive: slot.exclusive,
                            approaches: slot
                                .approaches
                                .iter()
                                .map(|offset| crate::facility_access::ApproachAccess {
                                    cell: (anchor + IVec3::from_array(*offset)).to_array(),
                                    status: crate::facility_access::AccessStatus::Unknown,
                                    obstruction: None,
                                    reason: "access computation is pending".into(),
                                })
                                .collect(),
                        });
                    slots.entry(profile.clone()).or_default().push(access);
                }
            }
            facilities.insert(
                record.id,
                Facility {
                    id: record.id,
                    cells: def
                        .footprint
                        .iter()
                        .map(|offset| {
                            let cell = anchor
                                + IVec3::from_array(record.orientation.rotate_offset(*offset));
                            Cell(cell.x, cell.y, cell.z)
                        })
                        .collect(),
                    tags: def.tags.iter().map(|tag| tag.as_str().to_owned()).collect(),
                    attributes: def.environmental_attributes.clone(),
                    slots,
                },
            );
        }
        let input = Input {
            spaces: world
                .spaces
                .geometry
                .iter()
                .filter(|(id, _)| active.contains(id))
                .map(|(id, space)| (*id, space.clone()))
                .collect(),
            facilities,
            profiles: world.rules.movement.clone(),
            anchors,
            connections: world
                .spaces
                .connections
                .iter()
                .filter(|(id, _)| active.contains(id))
                .flat_map(|(_, connections)| connections.iter().cloned())
                .collect(),
            view: view.clone(),
            rules: world.rules.capabilities.values().cloned().collect(),
            reserved,
            reservations_known: true,
            pending: world.access.pending.clone(),
        };
        work.view = Some(view);
        work.task = Some(Mutex::new(Computation::new(
            budget.clone(),
            evaluate(input, budget),
        )));
        work.signature = Some(signature);
        published.pending = true;
    }
    let budget = work.operation_budget;
    if let Some(task) = work.task.as_mut() {
        let task = task.get_mut().unwrap();
        let before = task.operations;
        let result = task.advance(budget);
        let used = task.operations - before;
        work.operations_last_tick = used;
        if let Poll::Ready(result) = result {
            published.snapshot.revision = published
                .snapshot
                .revision
                .checked_add(1)
                .expect("capability revision exhausted");
            published.snapshot.spatial_revision = world.spaces.revision;
            published.snapshot.access_revision = world.access.revision;
            published.snapshot.spaces = result.spaces;
            published.snapshot.outdoor = result.outdoor;
            published.pending = false;
            work.task = None;
        }
    }
    dependencies.0 = work
        .view
        .as_ref()
        .map(|view| view.dependencies.lock().unwrap().keys().copied().collect())
        .unwrap_or_default();
}
