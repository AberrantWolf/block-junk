//! Authored approaches evaluated against the same physical movement contract used
//! by surface navigation. Derived results never change architectural floor area.
use crate::{
    architecture_solver::{Budget, Computation},
    surface_navigation::{PhysicalAccess, SurfaceView},
};
use bevy::prelude::*;
use block_junk_mod_api::architecture::{PlacementId, UseSlotId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    task::Poll,
};
type Coord = [i32; 3];
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessStatus {
    Usable,
    Blocked,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproachAccess {
    pub cell: Coord,
    pub status: AccessStatus,
    pub obstruction: Option<Coord>,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAccess {
    pub id: UseSlotId,
    pub placement_revision: u64,
    pub profile: String,
    pub interactions: Vec<String>,
    pub exclusive: bool,
    pub approaches: Vec<ApproachAccess>,
}
impl SlotAccess {
    pub fn usable(&self) -> Option<bool> {
        if self
            .approaches
            .iter()
            .any(|approach| approach.status == AccessStatus::Usable)
        {
            Some(true)
        } else if self
            .approaches
            .iter()
            .any(|approach| approach.status == AccessStatus::Unknown)
        {
            None
        } else {
            Some(false)
        }
    }
}

pub fn require_usable_approach(
    access: &FacilityAccess,
    placement_revision: u64,
    slot: &UseSlotId,
    profile: &str,
    approach: IVec3,
) -> Result<(), String> {
    let record = access
        .slots
        .get(&(slot.clone(), profile.to_owned()))
        .filter(|record| record.placement_revision == placement_revision)
        .ok_or("facility access is pending or stale")?;
    let approach = record
        .approaches
        .iter()
        .find(|candidate| candidate.cell == approach.to_array())
        .ok_or("interaction approach is not authored for this slot")?;
    match approach.status {
        AccessStatus::Usable => Ok(()),
        AccessStatus::Unknown => Err("interaction approach geometry is pending".into()),
        AccessStatus::Blocked => Err(format!(
            "interaction approach is blocked: {}{}",
            approach.reason,
            approach
                .obstruction
                .map(|cell| format!(" at {cell:?}"))
                .unwrap_or_default()
        )),
    }
}
#[derive(Resource, Default)]
pub struct FacilityAccess {
    pub slots: BTreeMap<(UseSlotId, String), SlotAccess>,
    pub pending: BTreeSet<PlacementId>,
    pub revision: u64,
}
#[derive(Resource, Default)]
pub struct FacilityDependencies(pub BTreeSet<Coord>);
#[derive(Clone)]
pub(crate) struct Facility {
    pub(crate) id: PlacementId,
    pub(crate) revision: u64,
    pub(crate) anchor: IVec3,
    pub(crate) slots: Vec<block_junk_mod_api::architecture::FacilitySlot>,
}
type Results = BTreeMap<(UseSlotId, String), SlotAccess>;
#[derive(Resource)]
pub struct AccessWork {
    pub operation_budget: usize,
    pub operations_last_tick: usize,
    signature: Option<[u8; 32]>,
    view: Option<SurfaceView>,
    task: Option<Mutex<Computation<Results>>>,
}
impl Default for AccessWork {
    fn default() -> Self {
        Self {
            operation_budget: 4096,
            operations_last_tick: 0,
            signature: None,
            view: None,
            task: None,
        }
    }
}
pub(crate) async fn evaluate(
    view: SurfaceView,
    facilities: Vec<Facility>,
    profiles: Vec<block_junk_mod_api::architecture::MovementProfile>,
) -> Results {
    let mut records = BTreeMap::new();
    for facility in facilities {
        for slot in &facility.slots {
            for profile in &profiles {
                view.budget.step().await;
                let id = UseSlotId {
                    placement: facility.id,
                    key: slot.key.clone(),
                };
                let mut record = SlotAccess {
                    id: id.clone(),
                    placement_revision: facility.revision,
                    profile: profile.id.clone(),
                    interactions: slot.interactions.clone(),
                    exclusive: slot.exclusive,
                    approaches: Vec::new(),
                };
                for offset in &slot.approaches {
                    let cell = facility.anchor + IVec3::from_array(*offset);
                    let (status, obstruction, reason) = match view.access(cell, profile).await {
                        PhysicalAccess::Clear => (
                            AccessStatus::Usable,
                            None,
                            "approach supports this movement profile",
                        ),
                        PhysicalAccess::Obstructed(cell) => (
                            AccessStatus::Blocked,
                            Some(cell.to_array()),
                            "body clearance is obstructed",
                        ),
                        PhysicalAccess::Unsupported(cell) => (
                            AccessStatus::Blocked,
                            Some(cell.to_array()),
                            "approach has no physical supporting surface",
                        ),
                        PhysicalAccess::Unknown => (
                            AccessStatus::Unknown,
                            None,
                            "approach geometry is not available yet",
                        ),
                    };
                    record.approaches.push(ApproachAccess {
                        cell: cell.to_array(),
                        status,
                        obstruction,
                        reason: reason.into(),
                    });
                }
                records.insert((id, profile.id.clone()), record);
            }
        }
    }
    records
}
#[derive(bevy::ecs::system::SystemParam)]
pub struct AccessWorld<'w, 's> {
    subscriptions: Res<'w, crate::server::SpatialSubscriptions>,
    simulation: Res<'w, crate::server::ActiveSimulationChunks>,
    spatial: Res<'w, crate::live_spaces::SpatialDependencies>,
    settlements: Res<'w, crate::settlements::SettlementDependencies>,
    capabilities: Option<Res<'w, crate::capabilities::CapabilityDependencies>>,
    summaries: Res<'w, crate::spatial_geometry::StructuralSummaries>,
    placements: Res<'w, crate::placements::Placements>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    rules: Res<'w, crate::world_rules::WorldRules>,
    map: Res<'w, crate::voxel::ChunkMap>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
}
pub fn refresh(
    world: AccessWorld,
    mut work: ResMut<AccessWork>,
    mut published: ResMut<FacilityAccess>,
    mut dependencies: ResMut<FacilityDependencies>,
) {
    // Dependency chunks satisfy queries but never become their own demand roots.
    let roots: BTreeSet<_> = world
        .subscriptions
        .requested_chunks()
        .chain(world.simulation.0.iter().copied())
        .map(|coord| coord.0.to_array())
        .chain(world.spatial.0.iter().copied())
        .chain(world.settlements.roots.iter().copied())
        .chain(
            world
                .capabilities
                .iter()
                .flat_map(|dependencies| dependencies.0.iter().copied()),
        )
        .collect();
    let anchors: BTreeSet<_> = world
        .summaries
        .completed
        .iter()
        .filter(|(coord, _)| roots.contains(*coord))
        .flat_map(|(_, summary)| summary.elements.keys().copied())
        .collect();
    let mut digest = blake3::Hasher::new();
    let mut facilities = Vec::new();
    let mut seen = BTreeSet::new();
    for anchor in anchors {
        let Some(record) = world
            .placements
            .at(IVec3::from_array(anchor))
            .filter(|record| record.active)
        else {
            continue;
        };
        if !seen.insert(record.id) {
            continue;
        }
        let Some(slot) = world.blocks.slot_of(&record.definition) else {
            continue;
        };
        let slots = &world.blocks.architecture[slot.0 as usize][record.orientation as usize].slots;
        if slots.is_empty() {
            continue;
        }
        digest.update(&record.id.0.to_le_bytes());
        digest.update(&record.revision.to_le_bytes());
        facilities.push(Facility {
            id: record.id,
            revision: record.revision,
            anchor: IVec3::from_array(record.anchor),
            slots: slots.clone(),
        });
    }
    for profile in world.rules.movement.values() {
        digest.update(&serde_json::to_vec(profile).expect("frozen profile"));
    }
    let signature = *digest.finalize().as_bytes();
    let valid = |view: &SurfaceView| {
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
    if work.signature != Some(signature) || work.view.as_ref().is_some_and(|view| !valid(view)) {
        work.signature = None;
        work.task = None;
        work.view = None;
    }
    if work.signature.is_none() {
        let budget = Arc::new(Budget::default());
        let view = SurfaceView {
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
        published.pending = facilities.iter().map(|facility| facility.id).collect();
        work.task = Some(Mutex::new(Computation::new(
            budget,
            evaluate(
                view.clone(),
                facilities,
                world.rules.movement.values().cloned().collect(),
            ),
        )));
        work.view = Some(view);
        work.signature = Some(signature);
    }
    work.operations_last_tick = 0;
    let budget = work.operation_budget;
    if let Some(task) = work.task.as_mut() {
        let task = task.get_mut().unwrap();
        let before = task.operations;
        let result = task.advance(budget);
        let used = task.operations - before;
        work.operations_last_tick = used;
        if let Poll::Ready(records) = result {
            if work.view.as_ref().is_some_and(valid) {
                published.pending.clear();
                for record in records.values().filter(|record| record.usable().is_none()) {
                    published.pending.insert(record.id.placement);
                }
                if published.slots != records {
                    published.slots = records;
                    published.revision = published
                        .revision
                        .checked_add(1)
                        .expect("facility revision exhausted");
                }
                work.task = None;
            } else {
                work.signature = None;
                work.task = None;
            }
        }
    }
    dependencies.0 = work
        .view
        .as_ref()
        .map(|view| view.dependencies.lock().unwrap().keys().copied().collect())
        .unwrap_or_default();
}
