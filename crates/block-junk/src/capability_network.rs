//! Coherent, subscription-scoped capability evidence. Capability revisions share
//! the spatial reliable lane and become visible only with their source space revision.
use crate::{
    capabilities::{OutdoorCapability, SpaceCapabilities},
    protocol::BoundedVec,
    spatial_revision::{RevisionAssembler, RevisionDataset, RevisionFrame},
};
use bevy::prelude::*;
use block_junk_mod_api::{
    architecture::UseSlotId,
    capabilities::{CapabilityResult, EvaluationProblem, Evidence, Value},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_RECORDS: usize = 32_768;
const MAX_SURFACES: usize = 256 * (crate::protocol::CHUNK_SIZE as usize).pow(3);
const MAX_EVIDENCE_NODES: usize = 32_768;

pub struct CapabilityDataset;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutdoorView {
    pub anchor: UseSlotId,
    pub profile: String,
    pub range: u32,
    pub allow_portals: bool,
    pub surfaces: BoundedVec<[i32; 3], MAX_SURFACES>,
    pub pending: bool,
    pub result: CapabilityResult,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub revision: u64,
    pub spatial_revision: u64,
    pub chunks: BoundedVec<[i32; 3], 256>,
    pub spaces: BoundedVec<(u64, SpaceCapabilities), MAX_RECORDS>,
    pub outdoor: BoundedVec<OutdoorView, MAX_RECORDS>,
}

impl RevisionDataset for CapabilityDataset {
    type State = CapabilitySnapshot;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(8);
    const SCHEMA: u64 = 0x6361_7061_0000_0001;

    fn validate(state: &Self::State) -> Result<(), String> {
        if state.revision == 0 || state.spatial_revision == 0 {
            return Err("invalid capability revision".into());
        }
        let chunks: BTreeSet<_> = state.chunks.iter().copied().collect();
        if chunks.len() != state.chunks.len()
            || chunks
                .iter()
                .flatten()
                .any(|value| value.unsigned_abs() > 1_000_000 / crate::protocol::CHUNK_SIZE)
        {
            return Err("invalid capability subscription scope".into());
        }
        let mut ids = BTreeSet::new();
        let mut nodes = 0;
        for (id, capabilities) in &state.spaces {
            if *id == 0 || !ids.insert(*id) {
                return Err("duplicate or invalid capability space".into());
            }
            validate_profiles(&capabilities.profiles, &mut nodes)?;
        }
        let mut anchors = BTreeSet::new();
        for outdoor in &state.outdoor {
            if outdoor.anchor.placement.0 == 0
                || outdoor.anchor.key.is_empty()
                || outdoor.anchor.key.len() > 128
                || outdoor.profile.is_empty()
                || outdoor.profile.len() > 128
                || outdoor.range == 0
                || outdoor.range > 1024
                || !anchors.insert((
                    outdoor.anchor.clone(),
                    outdoor.profile.clone(),
                    outdoor.result.id.clone(),
                ))
            {
                return Err("invalid outdoor capability anchor".into());
            }
            let unique: BTreeSet<_> = outdoor.surfaces.iter().copied().collect();
            if unique.len() != outdoor.surfaces.len()
                || unique.iter().any(|cell| !visible(*cell, &chunks))
            {
                return Err("invalid outdoor service area".into());
            }
            validate_result(&outdoor.result, &mut nodes)?;
        }
        Ok(())
    }
}

fn visible(cell: [i32; 3], chunks: &BTreeSet<[i32; 3]>) -> bool {
    let size = crate::protocol::CHUNK_SIZE as i32;
    chunks.contains(&cell.map(|value| value.div_euclid(size)))
}

fn validate_profiles(
    profiles: &BTreeMap<String, Vec<CapabilityResult>>,
    nodes: &mut usize,
) -> Result<(), String> {
    if profiles.len() > 64 {
        return Err("too many capability movement profiles".into());
    }
    for (profile, results) in profiles {
        if profile.is_empty() || profile.len() > 128 || results.len() > 256 {
            return Err("invalid capability profile".into());
        }
        let mut ids = BTreeSet::new();
        for result in results {
            if !ids.insert(&result.id) {
                return Err("duplicate capability result".into());
            }
            validate_result(result, nodes)?;
        }
    }
    Ok(())
}

fn validate_result(result: &CapabilityResult, nodes: &mut usize) -> Result<(), String> {
    if result.id.is_empty()
        || result.id.len() > 128
        || result.display_name.is_empty()
        || result.display_name.len() > 256
        || result.requirements.len() > 32
        || result.attributes.len() > 32
    {
        return Err("invalid capability result header".into());
    }
    for (key, evidence) in result.requirements.iter().chain(result.attributes.iter()) {
        if key.is_empty() || key.len() > 128 {
            return Err("invalid capability evidence key".into());
        }
        validate_evidence(evidence, 0, nodes)?;
    }
    if let Some(capacity) = &result.capacity {
        validate_evidence(capacity, 0, nodes)?;
    }
    Ok(())
}

fn validate_evidence(evidence: &Evidence, depth: usize, nodes: &mut usize) -> Result<(), String> {
    *nodes += 1;
    if *nodes > MAX_EVIDENCE_NODES
        || depth > 24
        || evidence.description.len() > 1024
        || evidence.placements.len() > MAX_RECORDS
        || evidence.slots.len() > MAX_RECORDS
        || evidence.children.len() > 256
        || evidence.placements.iter().any(|id| id.0 == 0)
        || evidence
            .slots
            .iter()
            .any(|slot| slot.placement.0 == 0 || slot.key.is_empty() || slot.key.len() > 128)
    {
        return Err("capability evidence exceeds bounds".into());
    }
    if let Some(Value::Number(value)) = evidence.value
        && (!value.is_finite() || value.abs() > 1e12)
    {
        return Err("invalid capability evidence number".into());
    }
    if let Some(problem) = &evidence.problem {
        let text = match problem {
            EvaluationProblem::Unknown(text) | EvaluationProblem::Invalid(text) => text,
        };
        if text.is_empty() || text.len() > 1024 {
            return Err("invalid capability evidence problem".into());
        }
    }
    for child in &evidence.children {
        validate_evidence(child, depth + 1, nodes)?;
    }
    Ok(())
}

fn bounded<T, const N: usize>(values: Vec<T>) -> Result<BoundedVec<T, N>, String> {
    BoundedVec::new(values).map_err(|error| error.to_string())
}

fn snapshot(
    chunks: BTreeSet<[i32; 3]>,
    source: &crate::capabilities::CapabilitySnapshot,
    spaces: &crate::live_spaces::PublishedSpaces,
    placements: &crate::placements::Placements,
) -> Result<CapabilitySnapshot, String> {
    let visible_spaces: BTreeSet<_> = spaces
        .geometry
        .iter()
        .filter(|(_, space)| {
            space
                .floor
                .iter()
                .any(|cell| visible([cell.0, cell.1, cell.2], &chunks))
        })
        .map(|(id, _)| *id)
        .collect();
    let space_results = source
        .spaces
        .iter()
        .filter(|(id, _)| visible_spaces.contains(id))
        .map(|(id, result)| (id.0, result.clone()))
        .collect();
    let outdoor = source
        .outdoor
        .iter()
        .filter_map(|result| outdoor_view(result, &chunks, placements).transpose())
        .collect::<Result<Vec<_>, String>>()?;
    let snapshot = CapabilitySnapshot {
        revision: source.revision,
        spatial_revision: source.spatial_revision,
        chunks: bounded(chunks.into_iter().collect())?,
        spaces: bounded(space_results)?,
        outdoor: bounded(outdoor)?,
    };
    CapabilityDataset::validate(&snapshot)?;
    Ok(snapshot)
}

fn outdoor_view(
    source: &OutdoorCapability,
    chunks: &BTreeSet<[i32; 3]>,
    placements: &crate::placements::Placements,
) -> Result<Option<OutdoorView>, String> {
    let Some(anchor) = placements.records.get(&source.anchor.placement) else {
        return Ok(None);
    };
    if !visible(anchor.anchor, chunks) {
        return Ok(None);
    }
    Ok(Some(OutdoorView {
        anchor: source.anchor.clone(),
        profile: source.profile.clone(),
        range: source.range,
        allow_portals: source.allow_portals,
        surfaces: bounded(
            source
                .area
                .surfaces
                .iter()
                .map(|cell| [cell.0, cell.1, cell.2])
                .filter(|cell| visible(*cell, chunks))
                .collect(),
        )?,
        pending: source.area.pending,
        result: source.result.clone(),
    }))
}

#[derive(Resource, Default)]
pub struct ClientCapabilities {
    assembler: RevisionAssembler,
    decoded: CapabilitySnapshot,
    pub visible: CapabilitySnapshot,
    pub waiting_for_spaces: bool,
    pub replacement_requested: bool,
}

impl ClientCapabilities {
    pub fn receive(&mut self, frame: RevisionFrame) -> Result<(), String> {
        let result = (|| {
            let Some(payload) =
                self.assembler
                    .accept(frame, CapabilityDataset::ID, CapabilityDataset::SCHEMA)?
            else {
                return Ok(());
            };
            let snapshot: CapabilitySnapshot =
                serde_json::from_slice(&payload).map_err(|error| error.to_string())?;
            CapabilityDataset::validate(&snapshot)?;
            self.assembler.validated()?;
            self.decoded = snapshot;
            self.waiting_for_spaces = true;
            self.replacement_requested = false;
            Ok(())
        })();
        if result.is_err() {
            self.assembler.reject();
            self.replacement_requested = true;
        }
        result
    }
}

pub fn expose_ready_revision(
    mut capabilities: ResMut<ClientCapabilities>,
    spaces: Res<crate::space_network::ClientSpaces>,
    placements: Res<crate::placement_network::ClientPlacements>,
) {
    if spaces.waiting_for_terrain
        || capabilities.decoded.spatial_revision != spaces.visible.revision
    {
        capabilities.waiting_for_spaces = true;
        return;
    }
    let visible_spaces: BTreeSet<_> = spaces.visible.spaces.iter().map(|space| space.id).collect();
    let next = CapabilitySnapshot {
        revision: capabilities.decoded.revision,
        spatial_revision: capabilities.decoded.spatial_revision,
        chunks: capabilities.decoded.chunks.clone(),
        spaces: bounded(
            capabilities
                .decoded
                .spaces
                .iter()
                .filter(|(id, _)| visible_spaces.contains(id))
                .cloned()
                .collect(),
        )
        .expect("validated capability subset"),
        outdoor: bounded(
            capabilities
                .decoded
                .outdoor
                .iter()
                .filter(|result| placements.lookup(&result.anchor.placement.0).is_some())
                .cloned()
                .collect(),
        )
        .expect("validated capability subset"),
    };
    if capabilities.visible != next {
        capabilities.visible = next;
    }
    capabilities.waiting_for_spaces = false;
}

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct RequestCapabilitySnapshot;

#[derive(Default)]
struct PeerRevision {
    observed: Option<(u64, BTreeSet<[i32; 3]>)>,
    sequence: u64,
    pending: VecDeque<(crate::protocol::SpatialMessage, usize)>,
    replacement: bool,
}

#[derive(Resource, Default)]
pub struct CapabilitySenders(BTreeMap<Entity, PeerRevision>);

type ReadyPeers<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut lightyear::prelude::MessageSender<crate::protocol::SpatialMessage>,
        &'static mut lightyear::prelude::MessageReceiver<RequestCapabilitySnapshot>,
    ),
    (
        With<crate::protocol::GameReady>,
        With<lightyear::prelude::Connected>,
    ),
>;

pub fn publish(
    mut peers: ReadyPeers,
    subscriptions: Res<crate::server::SpatialSubscriptions>,
    capabilities: Res<crate::capabilities::Capabilities>,
    spaces: Res<crate::live_spaces::PublishedSpaces>,
    placements: Res<crate::placements::Placements>,
    mut state: ResMut<CapabilitySenders>,
    mut budgets: ResMut<crate::server::SnapshotBudgets>,
) {
    let active: BTreeSet<_> = peers.iter().map(|(entity, _, _)| entity).collect();
    state.0.retain(|entity, _| active.contains(entity));
    for (entity, mut sender, mut requests) in &mut peers {
        let peer = state.0.entry(entity).or_default();
        if requests.receive().count() > 0 {
            peer.replacement = true;
        }
        if peer.pending.is_empty() && !capabilities.pending && capabilities.snapshot.revision > 0 {
            let scope: BTreeSet<_> = subscriptions
                .chunks_for(entity)
                .map(|coord| coord.0.to_array())
                .collect();
            let observed = (capabilities.snapshot.revision, scope.clone());
            if !peer.replacement && peer.observed.as_ref() == Some(&observed) {
                continue;
            }
            let Ok(next) = snapshot(scope, &capabilities.snapshot, &spaces, &placements) else {
                continue;
            };
            let encoded = (|| {
                let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
                let sequence = peer
                    .sequence
                    .checked_add(1)
                    .ok_or("capability sequence exhausted")?;
                crate::spatial_revision::encode(
                    CapabilityDataset::ID,
                    CapabilityDataset::SCHEMA,
                    sequence,
                    None,
                    bytes,
                )?
                .into_iter()
                .map(|frame| {
                    let message = crate::protocol::SpatialMessage::Revision(frame);
                    let size = bincode::serde::encode_to_vec(&message, bincode::config::standard())
                        .map_err(|error| error.to_string())?
                        .len();
                    Ok((message, size))
                })
                .collect::<Result<VecDeque<_>, String>>()
            })();
            match encoded {
                Ok(frames) => {
                    peer.pending = frames;
                    peer.sequence += 1;
                    peer.observed = Some(observed);
                    peer.replacement = false;
                }
                Err(error) => {
                    warn!(%error, "cannot encode capability revision");
                    continue;
                }
            }
        }
        while let Some((_, bytes)) = peer.pending.front() {
            if !budgets.spend(entity, *bytes) {
                break;
            }
            sender.send::<crate::protocol::SpatialChannel>(peer.pending.pop_front().unwrap().0);
        }
    }
}

pub fn request_replacement(
    mut capabilities: ResMut<ClientCapabilities>,
    mut senders: Query<&mut lightyear::prelude::MessageSender<RequestCapabilitySnapshot>>,
) {
    if !capabilities.replacement_requested {
        return;
    }
    for mut sender in &mut senders {
        sender.send::<crate::protocol::WorldChannel>(RequestCapabilitySnapshot);
        capabilities.replacement_requested = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use block_junk_mod_api::capabilities::CapabilityStatus;

    fn result() -> CapabilityResult {
        CapabilityResult {
            id: "test:sleeping".into(),
            display_name: "Sleeping".into(),
            status: CapabilityStatus::Supported,
            requirements: BTreeMap::new(),
            capacity: Some(Evidence::number("capacity".into(), 2.)),
            attributes: BTreeMap::new(),
        }
    }

    fn state(revision: u64) -> CapabilitySnapshot {
        CapabilitySnapshot {
            revision,
            spatial_revision: revision,
            chunks: bounded(vec![[0, 0, 0]]).unwrap(),
            spaces: bounded(vec![(
                1,
                SpaceCapabilities {
                    profiles: BTreeMap::from([("test:adult".into(), vec![result()])]),
                },
            )])
            .unwrap(),
            outdoor: BoundedVec::default(),
        }
    }

    #[test]
    fn capability_inspection_waits_for_its_terrain_basis() {
        let mut app = App::new();
        let snapshot = state(1);
        app.insert_resource(ClientCapabilities {
            decoded: snapshot.clone(),
            visible: snapshot,
            ..default()
        });
        let mut spaces = crate::space_network::ClientSpaces::default();
        spaces.visible.revision = 1;
        spaces.waiting_for_terrain = true;
        app.insert_resource(spaces)
            .init_resource::<crate::placement_network::ClientPlacements>()
            .add_systems(Update, expose_ready_revision);
        app.update();
        assert!(
            app.world()
                .resource::<ClientCapabilities>()
                .waiting_for_spaces
        );
        app.world_mut()
            .resource_mut::<crate::space_network::ClientSpaces>()
            .waiting_for_terrain = false;
        app.update();
        assert!(
            !app.world()
                .resource::<ClientCapabilities>()
                .waiting_for_spaces
        );
    }

    #[test]
    fn complete_revision_is_atomic_and_hostile_evidence_is_rejected() {
        let first = state(1);
        let bytes = serde_json::to_vec(&first).unwrap();
        let mut frames = crate::spatial_revision::encode(
            CapabilityDataset::ID,
            CapabilityDataset::SCHEMA,
            1,
            None,
            bytes,
        )
        .unwrap();
        let mut client = ClientCapabilities::default();
        while frames.len() > 1 {
            client.receive(frames.pop_front().unwrap()).unwrap();
            assert_eq!(client.decoded.revision, 0);
        }
        client.receive(frames.pop_front().unwrap()).unwrap();
        assert_eq!(client.decoded, first);

        let mut invalid = state(2);
        invalid.spaces[0].1.profiles.values_mut().next().unwrap()[0]
            .capacity
            .as_mut()
            .unwrap()
            .value = Some(Value::Number(f64::NAN));
        assert!(CapabilityDataset::validate(&invalid).is_err());
    }
}
