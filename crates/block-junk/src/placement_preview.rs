//! Isolated, cooperative evaluation of a cursor edit. Reuses production structural,
//! navigation and capability evaluators without publishing identities or mutations.
use crate::{
    architecture_solver::{
        self as solver, Budget, Cell, Computation, View, live_source::LiveSource,
    },
    blocks::{BlockRegistry, BlockSlot},
    capabilities::{self, SpaceCapabilities},
    placements::{PlacementRecord, Placements},
    protocol::CHUNK_SIZE,
    spatial_geometry::{ChunkGeometry, summarize_with},
    voxel::{Chunk, ChunkEntities, EntryKind, world_to_chunk},
    world_rules::WorldRules,
};
use bevy::prelude::*;
use block_junk_mod_api::{architecture::PlacementId, blocks::Cardinal};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    task::Poll,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub anchor: IVec3,
    pub slot: BlockSlot,
    pub orientation: Cardinal,
    pub spatial_revision: u64,
    pub chunks: BTreeMap<[i32; 3], u64>,
    pub placements: BTreeMap<u64, u64>,
}
#[derive(Default)]
pub(crate) struct Assessment {
    pub text: String,
    pub results: Vec<SpaceCapabilities>,
}
#[derive(Resource, Default)]
pub(crate) struct Preview {
    key: Option<Key>,
    task: Option<Mutex<Computation<Assessment>>>,
    result: Option<Assessment>,
}
pub(crate) struct Input {
    pub key: Key,
    pub chunks: BTreeMap<[i32; 3], (Chunk, ChunkEntities)>,
    pub placements: Placements,
    pub blocks: BlockRegistry,
    pub rules: WorldRules,
    pub affected_floors: BTreeSet<Cell>,
}
impl Preview {
    pub fn advance(&mut self, key: Key, capture: impl FnOnce() -> Input) -> String {
        if self.key.as_ref() != Some(&key) {
            let input = capture();
            let budget = Arc::new(Budget::default());
            self.task = Some(Mutex::new(Computation::new(
                budget.clone(),
                evaluate(input, budget),
            )));
            self.result = None;
            self.key = Some(key.clone());
        }
        if let Some(task) = &mut self.task
            && let Poll::Ready(result) = task.get_mut().unwrap().advance(8192)
        {
            self.result = Some(result);
            self.task = None;
        }
        let mut text = format!(
            "Preview from spatial r{} / terrain r{} · ",
            key.spatial_revision,
            key.chunks
                .get(&world_to_chunk(key.anchor).0.0.to_array())
                .copied()
                .unwrap_or(0)
        );
        if let Some(result) = &self.result {
            text.push_str(&result.text);
            use block_junk_mod_api::capabilities::Value;
            use std::fmt::Write;
            let values: Vec<_> = result
                .results
                .iter()
                .flat_map(|result| result.profiles.iter())
                .flat_map(|(profile, values)| values.iter().map(move |value| (profile, value)))
                .collect();
            for (profile, value) in values.iter().take(8) {
                let number =
                    |evidence: Option<&block_junk_mod_api::capabilities::Evidence>| match evidence
                        .and_then(|evidence| evidence.value.as_ref())
                    {
                        Some(Value::Number(value)) => value.to_string(),
                        Some(Value::Unbounded) => "unbounded".into(),
                        Some(Value::Boolean(value)) => value.to_string(),
                        None => "pending".into(),
                    };
                let _ = write!(
                    text,
                    "\n{} ({profile}): {:?} · capacity {}",
                    value.display_name,
                    value.status,
                    number(value.capacity.as_ref())
                );
                for name in ["installed", "usable", "available"] {
                    if let Some(evidence) = value.attributes.get(name) {
                        let _ = write!(text, " · {name} {}", number(Some(evidence)));
                    }
                }
            }
            if values.len() > 8 {
                let _ = write!(text, "\n{} further capability results", values.len() - 8);
            }
        } else {
            text.push_str("evaluating staged edit");
        }
        text
    }
}

pub(crate) async fn evaluate(mut input: Input, budget: Arc<Budget>) -> Assessment {
    let key = &input.key;
    let def = input.blocks.def(key.slot);
    let footprint: Vec<_> = def
        .footprint
        .iter()
        .map(|offset| key.anchor + IVec3::from_array(key.orientation.rotate_offset(*offset)))
        .collect();
    for &cell in &footprint {
        budget.step().await;
        let (coord, local) = world_to_chunk(cell);
        let Some((chunk, _)) = input.chunks.get(&coord.0.to_array()) else {
            return Assessment {
                text: "pending: footprint terrain is unavailable".into(),
                ..default()
            };
        };
        if !chunk.get(local).is_empty() {
            return Assessment {
                text: "blocked: footprint is occupied".into(),
                ..default()
            };
        }
    }
    let hypothetical = PlacementId(input.placements.next_id);
    let Some(next) = hypothetical.0.checked_add(1) else {
        return Assessment {
            text: "pending: placement identities exhausted".into(),
            ..default()
        };
    };
    if let Err(error) = input.placements.rebuild(&input.blocks) {
        return Assessment {
            text: format!("pending: {error}"),
            ..default()
        };
    }
    if Placements::tracked(def) {
        input.placements.records.insert(
            hypothetical,
            PlacementRecord {
                id: hypothetical,
                definition: def.id.clone(),
                anchor: key.anchor.to_array(),
                orientation: key.orientation,
                revision: 1,
                active: true,
                authored_state: "{}".into(),
            },
        );
        input.placements.next_id = next;
        if let Err(error) = input.placements.rebuild(&input.blocks) {
            return Assessment {
                text: format!("pending: {error}"),
                ..default()
            };
        }
    }
    for &cell in &footprint {
        let (coord, local) = world_to_chunk(cell);
        let (chunk, sidecar) = input
            .chunks
            .get_mut(&coord.0.to_array())
            .expect("validated footprint");
        chunk.apply_committed_cell(local, key.slot);
        sidecar.insert(
            cell,
            if cell == key.anchor {
                EntryKind::Anchor {
                    orientation: key.orientation,
                    portal_open: None,
                }
            } else {
                EntryKind::Ghost { anchor: key.anchor }
            },
        );
    }
    let lookup = |cell| {
        let (coord, local) = world_to_chunk(cell);
        let (chunk, sidecar) = input.chunks.get(&coord.0.to_array())?;
        Some((chunk.get(local), sidecar.get(cell), chunk.revision))
    };
    let mut summaries = BTreeMap::new();
    for coord in input.chunks.keys() {
        let mut summary = ChunkGeometry::default();
        let size = CHUNK_SIZE as i32;
        for z in 0..size {
            for y in 0..size {
                for x in 0..size {
                    budget.step().await;
                    let cell = IVec3::from_array(*coord) * size + IVec3::new(x, y, z);
                    let index = ChunkGeometry::cell_index(cell);
                    summary.cells[index] =
                        summarize_with(cell, &mut summary, &input.blocks, &lookup);
                }
            }
        }
        summaries.insert(*coord, Arc::new(summary));
    }
    if summaries
        .values()
        .flat_map(|summary| summary.elements.keys())
        .any(|anchor| input.placements.at(IVec3::from_array(*anchor)).is_none())
    {
        return Assessment {
            text: "pending: facility identity snapshot is incomplete".into(),
            ..default()
        };
    }
    // No finite probe establishes open sky. Without a complete roof or an
    // authoritative sky proof, the shared solver retains unknown measurements.
    let source = Arc::new(LiveSource::capture(
        summaries.clone(),
        BTreeMap::new(),
        &input.blocks,
        &input.placements,
    ));
    let view = View {
        source,
        budget: budget.clone(),
        dependencies: Default::default(),
    };
    let topology = solver::topology::detect(&view).await;
    let surface = crate::surface_navigation::SurfaceView {
        chunks: summaries,
        collision: input.blocks.collision.clone(),
        architecture: input.blocks.architecture.clone(),
        budget: budget.clone(),
        dependencies: Default::default(),
    };
    let mut facilities = Vec::new();
    for record in input
        .placements
        .records
        .values()
        .filter(|record| record.active)
    {
        budget.step().await;
        let slot = input
            .blocks
            .slot_of(&record.definition)
            .expect("validated placement");
        facilities.push(crate::facility_access::Facility {
            id: record.id,
            revision: record.revision,
            anchor: IVec3::from_array(record.anchor),
            slots: input.blocks.architecture[slot.0 as usize][record.orientation as usize]
                .slots
                .clone(),
        });
    }
    let access = crate::facility_access::evaluate(
        surface.clone(),
        facilities,
        input.rules.movement.values().cloned().collect(),
    )
    .await;
    let mut facilities = BTreeMap::new();
    for record in input
        .placements
        .records
        .values()
        .filter(|record| record.active)
    {
        budget.step().await;
        let slot = input
            .blocks
            .slot_of(&record.definition)
            .expect("validated placement");
        let def = input.blocks.def(slot);
        let mut slots = BTreeMap::<String, Vec<_>>::new();
        for ((id, profile), value) in &access {
            budget.step().await;
            if id.placement == record.id {
                slots
                    .entry(profile.clone())
                    .or_default()
                    .push(value.clone());
            }
        }
        facilities.insert(
            record.id,
            capabilities::Facility {
                id: record.id,
                cells: def
                    .footprint
                    .iter()
                    .map(|offset| {
                        let cell = IVec3::from_array(record.anchor)
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
    let touched: BTreeSet<_> = footprint
        .iter()
        .flat_map(|cell| {
            let c = Cell(cell.x, cell.y, cell.z);
            std::iter::once(c).chain(c.horizontal())
        })
        .chain(input.affected_floors.iter().copied())
        .collect();
    let spaces: BTreeMap<_, _> = topology
        .spaces
        .into_values()
        .filter(|space| {
            space.placements.contains(&hypothetical.0) || !space.floor.is_disjoint(&touched)
        })
        .enumerate()
        .map(|(i, space)| (crate::space_identity::SpaceId(i as u64 + 1), space))
        .collect();
    let pending = spaces.is_empty() || spaces.values().any(|space| space.pending);
    let area: usize = spaces.values().map(|space| space.floor.len()).sum();
    let count = spaces.len();
    let result = capabilities::evaluate(
        capabilities::Input {
            spaces,
            facilities,
            profiles: input.rules.movement,
            anchors: BTreeSet::from([hypothetical]),
            connections: topology.connections,
            view: surface,
            rules: input.rules.capabilities.into_values().collect(),
            reserved: BTreeSet::new(),
            reservations_known: false,
            pending: BTreeSet::new(),
        },
        budget,
    )
    .await;
    let mut results: Vec<_> = result.spaces.into_values().collect();
    for outdoor in result.outdoor {
        results.push(SpaceCapabilities {
            profiles: BTreeMap::from([(outdoor.profile, vec![outdoor.result])]),
        });
    }
    Assessment {
        text: format!(
            "{} · {count} affected spaces · structural area {area}",
            if pending {
                "pending geometry dependencies"
            } else {
                "staged geometry ready"
            }
        ),
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_and_occupied_footprints_never_report_ready() {
        for occupied in [false, true] {
            let blocks = BlockRegistry::build(serde_json::from_value(serde_json::json!([
                {"id":"test:stone", "display_name":"Stone", "flags":{"solid":true}, "color":[1,1,1]}
            ])).unwrap()).unwrap().0;
            let slot = blocks.require("test:stone");
            let mut chunks = BTreeMap::new();
            if occupied {
                let mut chunk = Chunk {
                    revision: 1,
                    blocks: vec![BlockSlot::EMPTY; crate::protocol::CHUNK_PADDED_CELLS],
                };
                chunk.apply_committed_cell(world_to_chunk(IVec3::ONE).1, slot);
                chunks.insert([0, 0, 0], (chunk, ChunkEntities::default()));
            }
            let input = Input {
                key: Key {
                    anchor: IVec3::ONE,
                    slot,
                    orientation: Cardinal::East,
                    spatial_revision: 1,
                    chunks: BTreeMap::new(),
                    placements: BTreeMap::new(),
                },
                chunks,
                placements: Placements::default(),
                blocks,
                rules: WorldRules::default(),
                affected_floors: BTreeSet::new(),
            };
            let budget = Arc::new(Budget::default());
            let mut task = Computation::new(budget.clone(), evaluate(input, budget));
            let Poll::Ready(result) = task.advance(127) else {
                panic!("footprint validation should finish within a quantum")
            };
            assert!(
                result
                    .text
                    .starts_with(if occupied { "blocked:" } else { "pending:" })
            );
            assert!(result.results.is_empty());
        }
    }
}
