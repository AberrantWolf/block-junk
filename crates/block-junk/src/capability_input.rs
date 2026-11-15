//! Build distinct-object and distinct-slot observations for declarative rules.
use crate::{
    architecture_solver::{
        Budget, Cell,
        topology::{Ceiling, Space, Volume},
    },
    facility_access::SlotAccess,
};
use block_junk_mod_api::{
    architecture::{PlacementId, UseSlotId},
    capabilities::*,
};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone)]
pub struct Object {
    pub id: PlacementId,
    pub cells: BTreeSet<Cell>,
    pub tags: BTreeSet<String>,
    pub attributes: BTreeMap<String, f64>,
    pub slots: Vec<SlotAccess>,
}
pub struct Observations(pub BTreeMap<ObservationQuery, Evidence>);
impl CapabilityInput for Observations {
    fn observe(&self, query: &ObservationQuery) -> Evidence {
        self.0
            .get(query)
            .cloned()
            .unwrap_or_else(|| Evidence::unknown(query.label(), "observation is pending".into()))
    }
}
pub async fn observe(
    query: &ObservationQuery,
    space: Option<&Space>,
    objects: &[Object],
    reserved: &BTreeSet<UseSlotId>,
    pending: &BTreeSet<PlacementId>,
    budget: &Budget,
) -> Evidence {
    budget.step().await;
    if space.is_some_and(|space| space.pending) {
        return Evidence::unknown(
            query.label(),
            "architectural membership is still pending".into(),
        );
    }
    let mut evidence = Evidence::number(query.label(), 0.);
    let mut count = 0.;
    let mut unknown = false;
    match query {
        ObservationQuery::Measure { measurement } => {
            let Some(space) = space.filter(|space| !space.pending) else {
                return Evidence::unknown(
                    query.label(),
                    "architectural geometry is pending or this is an outdoor service area".into(),
                );
            };
            match measurement {
                Measurement::StructuralArea => count = space.floor.len() as f64,
                Measurement::RoofCoverage => {
                    let mut covered = 0;
                    for roof in space.roofs.values() {
                        budget.step().await;
                        match roof {
                            Ceiling::At(_) => covered += 1,
                            Ceiling::Unknown => unknown = true,
                            Ceiling::Sky => {}
                        }
                    }
                    if space.floor.is_empty() {
                        unknown = true;
                    } else {
                        count = covered as f64 / space.floor.len() as f64;
                    }
                }
                Measurement::MinimumClearance => {
                    let mut minimum = None;
                    for (floor, clearance) in &space.clearance {
                        budget.step().await;
                        match clearance {
                            Ceiling::At(top) => {
                                let height = (top.1 - floor.1) as f64;
                                minimum =
                                    Some(minimum.map_or(height, |value: f64| value.min(height)));
                            }
                            Ceiling::Unknown => unknown = true,
                            Ceiling::Sky => {}
                        }
                    }
                    if let Some(value) = minimum {
                        count = value;
                    } else if !unknown && !space.clearance.is_empty() {
                        evidence.value = Some(Value::Unbounded);
                        return evidence;
                    } else {
                        unknown = true;
                    }
                }
                Measurement::InteriorVolume => match space.volume {
                    Volume::Bounded(volume) => count = volume as f64,
                    _ => unknown = true,
                },
            }
        }
        ObservationQuery::Placements { tag } => {
            for object in objects {
                budget.step().await;
                if object.tags.contains(tag) {
                    evidence.placements.insert(object.id);
                }
            }
            count = evidence.placements.len() as f64;
        }
        ObservationQuery::Slots {
            interaction,
            capacity,
            adjacent_to_tag,
        } => {
            let mut neighbors = BTreeMap::<Cell, BTreeSet<PlacementId>>::new();
            if let Some(tag) = adjacent_to_tag {
                for object in objects.iter().filter(|object| object.tags.contains(tag)) {
                    for cell in &object.cells {
                        for (x, y, z) in [
                            (1, 0, 0),
                            (-1, 0, 0),
                            (0, 1, 0),
                            (0, -1, 0),
                            (0, 0, 1),
                            (0, 0, -1),
                        ] {
                            budget.step().await;
                            neighbors
                                .entry(Cell(cell.0 + x, cell.1 + y, cell.2 + z))
                                .or_default()
                                .insert(object.id);
                        }
                    }
                }
            }
            for object in objects {
                let mut adjacent = adjacent_to_tag.is_none();
                for cell in &object.cells {
                    budget.step().await;
                    adjacent |= neighbors
                        .get(cell)
                        .is_some_and(|ids| ids.iter().any(|id| *id != object.id));
                }
                for slot in &object.slots {
                    budget.step().await;
                    if !slot.interactions.contains(interaction) {
                        continue;
                    }
                    let usable = if pending.contains(&object.id) {
                        None
                    } else {
                        slot.usable()
                    };
                    let contributes = adjacent
                        && match capacity {
                            CapacityKind::Installed => true,
                            CapacityKind::Usable => usable == Some(true),
                            CapacityKind::Available => {
                                usable == Some(true)
                                    && (!slot.exclusive || !reserved.contains(&slot.id))
                            }
                        };
                    if adjacent && *capacity != CapacityKind::Installed && usable.is_none() {
                        unknown = true;
                    }
                    if contributes {
                        evidence.slots.insert(slot.id.clone());
                        evidence.placements.insert(object.id);
                    }
                    if !contributes && *capacity != CapacityKind::Installed {
                        let mut failure = Evidence::number(
                            format!("facility {} slot {}", object.id.0, slot.id.key),
                            0.,
                        );
                        failure.placements.insert(object.id);
                        failure.slots.insert(slot.id.clone());
                        if !adjacent {
                            failure
                                .description
                                .push_str(": no distinct adjacent object with the required tag");
                        } else if usable == Some(true) {
                            failure.description.push_str(": currently reserved");
                        } else {
                            for approach in &slot.approaches {
                                let reason = format!(
                                    "approach {:?}: {}{}",
                                    approach.cell,
                                    approach.reason,
                                    approach
                                        .obstruction
                                        .map(|cell| format!(" at {cell:?}"))
                                        .unwrap_or_default()
                                );
                                failure.children.push(Evidence::number(reason, 0.));
                            }
                        }
                        evidence.children.push(failure);
                    }
                }
            }
            count = evidence.slots.len() as f64;
        }
        ObservationQuery::Adjacency {
            left_tag,
            right_tag,
        } => {
            let mut right_cells = BTreeMap::<Cell, BTreeSet<PlacementId>>::new();
            for object in objects
                .iter()
                .filter(|object| object.tags.contains(right_tag))
            {
                for cell in &object.cells {
                    budget.step().await;
                    right_cells.entry(*cell).or_default().insert(object.id);
                }
            }
            let mut pairs = BTreeSet::new();
            for object in objects
                .iter()
                .filter(|object| object.tags.contains(left_tag))
            {
                for cell in &object.cells {
                    for (x, y, z) in [
                        (1, 0, 0),
                        (-1, 0, 0),
                        (0, 1, 0),
                        (0, -1, 0),
                        (0, 0, 1),
                        (0, 0, -1),
                    ] {
                        budget.step().await;
                        if let Some(others) =
                            right_cells.get(&Cell(cell.0 + x, cell.1 + y, cell.2 + z))
                        {
                            for other in others {
                                if *other != object.id {
                                    pairs.insert((object.id.min(*other), object.id.max(*other)));
                                }
                            }
                        }
                    }
                }
            }
            count = pairs.len() as f64;
            evidence.placements = pairs.into_iter().flat_map(|(a, b)| [a, b]).collect();
        }
        ObservationQuery::Environment { attribute } => {
            for object in objects {
                budget.step().await;
                if let Some(value) = object.attributes.get(attribute)
                    && evidence.placements.insert(object.id)
                {
                    count += value;
                }
            }
        }
    }
    if unknown {
        evidence.value = None;
        evidence.problem = Some(EvaluationProblem::Unknown(
            "required geometry or facility access is pending".into(),
        ));
    } else {
        evidence.value = Some(Value::Number(count));
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        architecture_solver::Computation,
        facility_access::{AccessStatus, ApproachAccess},
    };
    use std::{sync::Arc, task::Poll};

    fn object(id: u64, cells: &[Cell], tags: &[&str]) -> Object {
        Object {
            id: PlacementId(id),
            cells: cells.iter().copied().collect(),
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
            attributes: BTreeMap::new(),
            slots: vec![SlotAccess {
                id: UseSlotId {
                    placement: PlacementId(id),
                    key: "seat".into(),
                },
                placement_revision: 1,
                profile: "adult".into(),
                interactions: vec!["dining".into()],
                exclusive: true,
                approaches: vec![ApproachAccess {
                    cell: [0, 1, 0],
                    status: AccessStatus::Usable,
                    obstruction: None,
                    reason: "clear".into(),
                }],
            }],
        }
    }
    fn run(
        query: ObservationQuery,
        objects: Vec<Object>,
        space: Option<Space>,
        pending: BTreeSet<PlacementId>,
    ) -> Evidence {
        let budget = Arc::new(Budget::default());
        let mut task = Computation::new(budget.clone(), async move {
            observe(
                &query,
                space.as_ref(),
                &objects,
                &BTreeSet::new(),
                &pending,
                &budget,
            )
            .await
        });
        for _ in 0..10_000 {
            let before = task.operations;
            let result = task.advance(7);
            assert!(task.operations - before <= 7);
            if let Poll::Ready(result) = result {
                return result;
            }
        }
        panic!("finite observation failed to converge")
    }
    #[test]
    fn multicell_contacts_and_duplicate_memberships_count_distinct_objects_and_slots() {
        for rotate in [false, true] {
            let cell = |x: i32, z: i32| {
                if rotate {
                    Cell(z, 1, -x)
                } else {
                    Cell(x, 1, z)
                }
            };
            let seat = object(1, &[cell(0, 0), cell(0, 1), cell(0, 2)], &["seat"]);
            let table = object(2, &[cell(1, 0), cell(1, 1), cell(1, 2)], &["table"]);
            let objects = vec![seat.clone(), table.clone(), seat, table];
            let adjacency = run(
                ObservationQuery::Adjacency {
                    left_tag: "seat".into(),
                    right_tag: "table".into(),
                },
                objects.clone(),
                None,
                BTreeSet::new(),
            );
            assert_eq!(adjacency.value, Some(Value::Number(1.)));
            assert_eq!(adjacency.placements.len(), 2);
            let slots = run(
                ObservationQuery::Slots {
                    interaction: "dining".into(),
                    capacity: CapacityKind::Usable,
                    adjacent_to_tag: Some("table".into()),
                },
                objects,
                None,
                BTreeSet::new(),
            );
            assert_eq!(slots.value, Some(Value::Number(1.)));
            assert_eq!(slots.slots.len(), 1);
        }
    }
    #[test]
    fn an_object_cannot_satisfy_its_own_adjacency_requirement() {
        let table_seat = object(1, &[Cell(0, 1, 0), Cell(1, 1, 0)], &["seat", "table"]);
        let result = run(
            ObservationQuery::Slots {
                interaction: "dining".into(),
                capacity: CapacityKind::Usable,
                adjacent_to_tag: Some("table".into()),
            },
            vec![table_seat],
            None,
            BTreeSet::new(),
        );
        assert_eq!(result.value, Some(Value::Number(0.)));
        assert!(result.children[0].description.contains("distinct adjacent"));
    }
    #[test]
    fn pending_access_preserves_installed_capacity_and_environment_is_not_duplicated() {
        let mut seat = object(1, &[Cell(0, 1, 0)], &["seat"]);
        seat.attributes.insert("scenic_interest".into(), 3.);
        let objects = vec![seat.clone(), seat];
        for capacity in [
            CapacityKind::Installed,
            CapacityKind::Usable,
            CapacityKind::Available,
        ] {
            let result = run(
                ObservationQuery::Slots {
                    interaction: "dining".into(),
                    capacity,
                    adjacent_to_tag: None,
                },
                objects.clone(),
                None,
                BTreeSet::from([PlacementId(1)]),
            );
            if capacity == CapacityKind::Installed {
                assert_eq!(result.value, Some(Value::Number(1.)));
            } else {
                assert_eq!(result.value, None);
                assert!(matches!(
                    result.problem,
                    Some(EvaluationProblem::Unknown(_))
                ));
            }
        }
        let result = run(
            ObservationQuery::Environment {
                attribute: "scenic_interest".into(),
            },
            objects,
            None,
            BTreeSet::new(),
        );
        assert_eq!(result.value, Some(Value::Number(3.)));
    }
    #[test]
    fn known_open_sky_clearance_is_unbounded_but_unavailable_geometry_is_unknown() {
        let floor = Cell(0, 1, 0);
        let mut space = Space {
            floor: BTreeSet::from([floor]),
            roofs: BTreeMap::from([(floor, Ceiling::Sky)]),
            clearance: BTreeMap::from([(floor, Ceiling::Sky)]),
            volume: Volume::Open,
            boundary: crate::architecture_solver::topology::Boundary::Open,
            pending: false,
            placements: BTreeSet::new(),
        };
        let query = ObservationQuery::Measure {
            measurement: Measurement::MinimumClearance,
        };
        assert_eq!(
            run(query.clone(), vec![], Some(space.clone()), BTreeSet::new()).value,
            Some(Value::Unbounded)
        );
        space.clearance.insert(floor, Ceiling::Unknown);
        assert!(matches!(
            run(query, vec![], Some(space), BTreeSet::new()).problem,
            Some(EvaluationProblem::Unknown(_))
        ));
    }
}
