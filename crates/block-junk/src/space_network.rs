//! Revisioned, subscription-scoped architectural views. Full geometry remains
//! authoritative; clients receive only their visible floor/connection fragments.
use crate::{
    architecture_solver::{
        Cell, Face,
        topology::{Ceiling, Volume},
    },
    protocol::{BoundedVec, ChunkCoord},
    spatial_revision::{RevisionAssembler, RevisionDataset, RevisionFrame},
};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
const MAX_VISIBLE_CHUNKS: usize = 256;
const MAX_VISIBLE_CELLS: usize = MAX_VISIBLE_CHUNKS * (crate::protocol::CHUNK_SIZE as usize).pow(3);
const MAX_RECORDS: usize = MAX_VISIBLE_CELLS;
pub struct SpaceDataset;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Height {
    At(i32),
    Sky,
    Unknown,
}
impl From<Ceiling> for Height {
    fn from(value: Ceiling) -> Self {
        match value {
            Ceiling::At(cell) => Self::At(cell.1),
            Ceiling::Sky => Self::Sky,
            Ceiling::Unknown => Self::Unknown,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorSample {
    pub cell: [i32; 3],
    pub roof: Height,
    pub clearance: Height,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionView {
    pub ends: [BoundedVec<u64, MAX_RECORDS>; 2],
    pub faces: BoundedVec<Face, MAX_VISIBLE_CELLS>,
    pub threshold: BoundedVec<Cell, MAX_VISIBLE_CELLS>,
    pub traversable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceView {
    pub id: u64,
    pub revision: u64,
    pub structural_area: u64,
    pub covered: u64,
    pub unknown_roof: u64,
    pub volume: Volume,
    pub pending: bool,
    pub floors: BoundedVec<FloorSample, MAX_VISIBLE_CELLS>,
    pub placements: BoundedVec<u64, MAX_VISIBLE_CELLS>,
    pub connections: BoundedVec<ConnectionView, MAX_VISIBLE_CELLS>,
}
impl SpaceView {
    /// Membership for first-person inspection, including the supporting floor.
    /// A lower storey must never claim cells above its ceiling.
    pub fn contains_cell(&self, cell: IVec3, placement: Option<u64>) -> bool {
        placement.is_some_and(|id| self.placements.contains(&id))
            || self.floors.iter().any(|sample| {
                let [x, y, z] = sample.cell;
                x == cell.x
                    && z == cell.z
                    && cell.y >= y - 1
                    && match sample.clearance {
                        Height::At(top) => cell.y < top,
                        Height::Sky => true,
                        Height::Unknown => cell.y <= y,
                    }
            })
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSnapshot {
    pub revision: u64,
    pub chunks: BoundedVec<[i32; 3], MAX_VISIBLE_CHUNKS>,
    pub terrain_basis: BoundedVec<([i32; 3], u64), MAX_VISIBLE_CHUNKS>,
    pub spaces: BoundedVec<SpaceView, MAX_RECORDS>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpaceChanges {
    pub replace: bool,
    pub revision: u64,
    pub chunks: BoundedVec<[i32; 3], MAX_VISIBLE_CHUNKS>,
    pub terrain_basis: BoundedVec<([i32; 3], u64), MAX_VISIBLE_CHUNKS>,
    pub upserts: BoundedVec<SpaceView, MAX_RECORDS>,
    pub removed: BoundedVec<u64, MAX_RECORDS>,
}
impl RevisionDataset for SpaceDataset {
    type State = SpaceSnapshot;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(6);
    const SCHEMA: u64 = 0x7370616300000001;
    fn validate(state: &Self::State) -> Result<(), String> {
        let chunks: BTreeSet<_> = state.chunks.iter().copied().collect();
        if chunks.len() != state.chunks.len()
            || chunks
                .iter()
                .flatten()
                .any(|n| n.unsigned_abs() > 1_000_000 / crate::protocol::CHUNK_SIZE)
        {
            return Err("invalid architectural subscription scope".into());
        }
        let basis: BTreeMap<_, _> = state.terrain_basis.iter().copied().collect();
        if basis.len() != state.terrain_basis.len()
            || basis.keys().any(|chunk| !chunks.contains(chunk))
        {
            return Err("invalid terrain revision basis".into());
        }
        let mut ids = BTreeSet::new();
        let mut floors = BTreeSet::new();
        let mut thresholds = BTreeSet::new();
        for space in &state.spaces {
            if space.id == 0
                || space.revision == 0
                || !ids.insert(space.id)
                || space.structural_area == 0
                || space
                    .covered
                    .checked_add(space.unknown_roof)
                    .is_none_or(|n| n > space.structural_area)
            {
                return Err("invalid architectural header".into());
            }
            if space.floors.len() as u64 > space.structural_area {
                return Err("visible floor area exceeds structural area".into());
            }
            if matches!(space.volume,Volume::Bounded(volume) if volume < space.structural_area) {
                return Err("bounded volume is smaller than its floor area".into());
            }
            let placements: BTreeSet<_> = space.placements.iter().copied().collect();
            if placements.contains(&0) || placements.len() != space.placements.len() {
                return Err("duplicate or invalid facility identity".into());
            }
            for sample in &space.floors {
                if !visible(sample.cell, &chunks) || !floors.insert(sample.cell) {
                    return Err("floor is outside subscription or has duplicate ownership".into());
                }
                for height in [sample.roof, sample.clearance] {
                    if let Height::At(y) = height
                        && (y <= sample.cell[1] || y.unsigned_abs() > 1_000_064)
                    {
                        return Err("invalid architectural clearance/roof".into());
                    }
                }
            }
            for connection in &space.connections {
                let mut faces = BTreeSet::new();
                for face in &connection.faces {
                    if face.0 >= face.1 || !faces.insert(*face) {
                        return Err("duplicate or noncanonical connection face".into());
                    }
                    if face.0.1 != face.1.1
                        || face.0.0.abs_diff(face.1.0) as u64 + face.0.2.abs_diff(face.1.2) as u64
                            != 1
                        || !(visible([face.0.0, face.0.1, face.0.2], &chunks)
                            || visible([face.1.0, face.1.1, face.1.2], &chunks))
                    {
                        return Err("invalid connection face".into());
                    }
                }
                let mut unique_thresholds = BTreeSet::new();
                for cell in &connection.threshold {
                    if !unique_thresholds.insert(*cell) {
                        return Err("duplicate threshold cell".into());
                    }
                    thresholds.insert([cell.0, cell.1, cell.2]);
                    if !visible([cell.0, cell.1, cell.2], &chunks) {
                        return Err("threshold outside subscription".into());
                    }
                }
                for ends in &connection.ends {
                    let unique: BTreeSet<_> = ends.iter().copied().collect();
                    if unique.contains(&0) || unique.len() != ends.len() {
                        return Err("invalid connection endpoint identities".into());
                    }
                }
            }
        }
        if !floors.is_disjoint(&thresholds) {
            return Err("portal threshold belongs to a room floor".into());
        }
        Ok(())
    }
}
fn visible(cell: [i32; 3], chunks: &BTreeSet<[i32; 3]>) -> bool {
    if cell.iter().any(|n| n.unsigned_abs() > 1_000_064) {
        return false;
    }
    let size = crate::protocol::CHUNK_SIZE as i32;
    chunks.contains(&cell.map(|n| n.div_euclid(size)))
}
fn bounded<T, const N: usize>(values: Vec<T>) -> Result<BoundedVec<T, N>, String> {
    BoundedVec::new(values).map_err(|e| e.to_string())
}
fn snapshot(
    chunks: BTreeSet<[i32; 3]>,
    published: &crate::live_spaces::PublishedSpaces,
    identities: &crate::space_identity::SpaceIdentities,
) -> Result<SpaceSnapshot, String> {
    let owners: BTreeMap<_, _> = published
        .geometry
        .iter()
        .filter_map(|(id, space)| space.floor.first().map(|key| (*key, *id)))
        .collect();
    let mut views = Vec::new();
    let mut basis = BTreeMap::new();
    for (id, space) in &published.geometry {
        let floors: Vec<_> = space
            .floor
            .iter()
            .filter(|cell| visible([cell.0, cell.1, cell.2], &chunks))
            .map(|cell| FloorSample {
                cell: [cell.0, cell.1, cell.2],
                roof: space.roofs[cell].into(),
                clearance: space.clearance[cell].into(),
            })
            .collect();
        if floors.is_empty() {
            continue;
        }
        let record = identities
            .records
            .get(id)
            .filter(|record| record.active)
            .ok_or("published space has no active identity")?;
        let mut connections = Vec::new();
        for connection in published.connections.get(id).into_iter().flatten() {
            let faces: Vec<_> = connection
                .faces
                .iter()
                .filter(|face| {
                    visible([face.0.0, face.0.1, face.0.2], &chunks)
                        || visible([face.1.0, face.1.1, face.1.2], &chunks)
                })
                .copied()
                .collect();
            let threshold: Vec<_> = connection
                .threshold
                .iter()
                .filter(|cell| visible([cell.0, cell.1, cell.2], &chunks))
                .copied()
                .collect();
            if faces.is_empty() && threshold.is_empty() {
                continue;
            }
            connections.push(ConnectionView {
                ends: [
                    bounded(
                        connection.ends[0]
                            .iter()
                            .filter_map(|key| owners.get(key).map(|id| id.0))
                            .collect(),
                    )?,
                    bounded(
                        connection.ends[1]
                            .iter()
                            .filter_map(|key| owners.get(key).map(|id| id.0))
                            .collect(),
                    )?,
                ],
                faces: bounded(faces)?,
                threshold: bounded(threshold)?,
                traversable: connection.traversable,
            });
        }
        for (coord, revision) in published.basis.get(id).into_iter().flatten() {
            if chunks.contains(coord)
                && basis
                    .insert(*coord, *revision)
                    .is_some_and(|old| old != *revision)
            {
                return Err("spatial regions await a coherent terrain basis".into());
            }
        }
        views.push(SpaceView {
            id: id.0,
            revision: record.revision,
            structural_area: space.floor.len() as u64,
            covered: space
                .roofs
                .values()
                .filter(|roof| matches!(roof, Ceiling::At(_)))
                .count() as u64,
            unknown_roof: space
                .roofs
                .values()
                .filter(|roof| **roof == Ceiling::Unknown)
                .count() as u64,
            volume: space.volume,
            pending: space.pending || !published.pending.is_empty(),
            floors: bounded(floors)?,
            placements: bounded(space.placements.iter().copied().collect())?,
            connections: bounded(connections)?,
        });
    }
    let state = SpaceSnapshot {
        revision: published.revision,
        chunks: bounded(chunks.into_iter().collect())?,
        terrain_basis: bounded(basis.into_iter().collect())?,
        spaces: bounded(views)?,
    };
    SpaceDataset::validate(&state)?;
    Ok(state)
}

#[cfg(test)]
pub(crate) fn snapshot_wire_bytes(
    chunks: BTreeSet<[i32; 3]>,
    published: &crate::live_spaces::PublishedSpaces,
    identities: &crate::space_identity::SpaceIdentities,
) -> Result<usize, String> {
    let state = snapshot(chunks, published, identities)?;
    let delta = changes(None, &state);
    let bytes = bincode::serde::encode_to_vec(&delta, bincode::config::standard())
        .map_err(|error| error.to_string())?;
    crate::spatial_revision::encode(SpaceDataset::ID, SpaceDataset::SCHEMA, 1, None, bytes)?
        .into_iter()
        .try_fold(0usize, |total, frame| {
            let message = crate::protocol::SpatialMessage::Revision(frame);
            let bytes = bincode::serde::encode_to_vec(&message, bincode::config::standard())
                .map_err(|error| error.to_string())?
                .len();
            total
                .checked_add(bytes)
                .ok_or("wire byte count overflow".into())
        })
}
fn changes(old: Option<&SpaceSnapshot>, new: &SpaceSnapshot) -> SpaceChanges {
    let previous: BTreeMap<_, _> = old
        .into_iter()
        .flat_map(|state| state.spaces.iter().map(|space| (space.id, space)))
        .collect();
    let ids: BTreeSet<_> = new.spaces.iter().map(|space| space.id).collect();
    SpaceChanges {
        replace: old.is_none(),
        revision: new.revision,
        chunks: new.chunks.clone(),
        terrain_basis: new.terrain_basis.clone(),
        upserts: bounded(
            new.spaces
                .iter()
                .filter(|space| previous.get(&space.id).is_none_or(|old| **old != **space))
                .cloned()
                .collect(),
        )
        .unwrap(),
        removed: bounded(
            previous
                .keys()
                .filter(|id| !ids.contains(id))
                .copied()
                .collect(),
        )
        .unwrap(),
    }
}
#[derive(Resource, Default)]
pub struct ClientSpaces {
    assembler: RevisionAssembler,
    hidden: BTreeSet<[i32; 3]>,
    decoded: SpaceSnapshot,
    pub visible: SpaceSnapshot,
    pub waiting_for_terrain: bool,
    pub replacement_requested: bool,
}
impl ClientSpaces {
    pub fn leave(&mut self, coord: ChunkCoord) {
        self.hidden.insert(coord.0.to_array());
        self.visible = project(&self.visible, &self.hidden);
    }
    pub fn enter(&mut self, coord: ChunkCoord) {
        self.hidden.remove(&coord.0.to_array());
        self.waiting_for_terrain = true;
    }
    pub fn receive(&mut self, frame: RevisionFrame) -> Result<(), String> {
        let result = (|| {
            let Some(payload) =
                self.assembler
                    .accept(frame, SpaceDataset::ID, SpaceDataset::SCHEMA)?
            else {
                return Ok(());
            };
            let (delta, used): (SpaceChanges, usize) = bincode::serde::decode_from_slice(
                &payload,
                bincode::config::standard().with_limit::<{ 128 * 1024 * 1024 }>(),
            )
            .map_err(|e| e.to_string())?;
            if delta.replace != self.assembler.awaiting_replacement()? {
                return Err("architectural payload disagrees with revision base".into());
            }
            if used != payload.len() {
                return Err("trailing architectural revision bytes".into());
            }
            let mut spaces: BTreeMap<_, _> = if delta.replace {
                BTreeMap::new()
            } else {
                self.decoded
                    .spaces
                    .iter()
                    .cloned()
                    .map(|space| (space.id, space))
                    .collect()
            };
            let mut modified = BTreeSet::new();
            for id in delta.removed {
                if !modified.insert(id) || spaces.remove(&id).is_none() {
                    return Err("invalid architectural removal".into());
                }
            }
            for space in delta.upserts {
                if !modified.insert(space.id) {
                    return Err("duplicate architectural mutation".into());
                }
                spaces.insert(space.id, space);
            }
            let staged = SpaceSnapshot {
                revision: delta.revision,
                chunks: delta.chunks,
                terrain_basis: delta.terrain_basis,
                spaces: bounded(spaces.into_values().collect())?,
            };
            SpaceDataset::validate(&staged)?;
            self.assembler.validated()?;
            self.decoded = staged;
            self.waiting_for_terrain = true;
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
fn project(source: &SpaceSnapshot, hidden: &BTreeSet<[i32; 3]>) -> SpaceSnapshot {
    let mut next = source.clone();
    let chunks: BTreeSet<_> = source
        .chunks
        .iter()
        .filter(|coord| !hidden.contains(*coord))
        .copied()
        .collect();
    next.chunks = bounded(chunks.iter().copied().collect()).unwrap();
    next.terrain_basis = bounded(
        source
            .terrain_basis
            .iter()
            .filter(|(coord, _)| chunks.contains(coord))
            .copied()
            .collect(),
    )
    .unwrap();
    next.spaces = bounded(
        source
            .spaces
            .iter()
            .filter_map(|space| {
                let mut space = space.clone();
                space.floors = bounded(
                    space
                        .floors
                        .into_iter()
                        .filter(|sample| visible(sample.cell, &chunks))
                        .collect(),
                )
                .unwrap();
                if space.floors.is_empty() {
                    return None;
                }
                space.connections = bounded(
                    space
                        .connections
                        .into_iter()
                        .filter_map(|mut connection| {
                            connection.faces = bounded(
                                connection
                                    .faces
                                    .into_iter()
                                    .filter(|face| {
                                        visible([face.0.0, face.0.1, face.0.2], &chunks)
                                            || visible([face.1.0, face.1.1, face.1.2], &chunks)
                                    })
                                    .collect(),
                            )
                            .unwrap();
                            connection.threshold = bounded(
                                connection
                                    .threshold
                                    .into_iter()
                                    .filter(|cell| visible([cell.0, cell.1, cell.2], &chunks))
                                    .collect(),
                            )
                            .unwrap();
                            (!connection.faces.is_empty() || !connection.threshold.is_empty())
                                .then_some(connection)
                        })
                        .collect(),
                )
                .unwrap();
                Some(space)
            })
            .collect(),
    )
    .unwrap();
    next
}
pub fn expose_ready_revision(
    mut spaces: ResMut<ClientSpaces>,
    map: Res<crate::voxel::ChunkMap>,
    chunks: Query<&crate::voxel::Chunk>,
) {
    if spaces
        .decoded
        .terrain_basis
        .iter()
        .all(|(coord, revision)| {
            spaces.hidden.contains(coord)
                || map
                    .0
                    .get(&ChunkCoord(IVec3::from_array(*coord)))
                    .and_then(|entity| chunks.get(*entity).ok())
                    .is_some_and(|chunk| chunk.revision == *revision)
        })
    {
        if spaces.waiting_for_terrain {
            let next = project(&spaces.decoded, &spaces.hidden);
            if spaces.visible != next {
                spaces.visible = next;
            }
        }
        spaces.waiting_for_terrain = false;
    } else {
        spaces.waiting_for_terrain = true;
    }
}

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct RequestSpaceSnapshot;
type Observation = (u64, BTreeSet<[i32; 3]>, BTreeSet<[i32; 3]>);
#[derive(Default)]
struct PeerRevision {
    observed: Option<Observation>,
    sequence: u64,
    sent: Option<SpaceSnapshot>,
    pending: VecDeque<(crate::protocol::SpatialMessage, usize)>,
    replacement: bool,
}
#[derive(Resource, Default)]
pub struct SpaceSenders(BTreeMap<Entity, PeerRevision>);
type ReadyPeers<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut lightyear::prelude::MessageSender<crate::protocol::SpatialMessage>,
        &'static mut lightyear::prelude::MessageReceiver<RequestSpaceSnapshot>,
    ),
    (
        With<crate::protocol::GameReady>,
        With<lightyear::prelude::Connected>,
    ),
>;
#[allow(
    clippy::too_many_arguments,
    reason = "shared subscription, identity and bandwidth services"
)]
pub fn publish(
    mut peers: ReadyPeers,
    subscriptions: Res<crate::server::SpatialSubscriptions>,
    published: Res<crate::live_spaces::PublishedSpaces>,
    identities: Res<crate::space_identity::SpaceIdentities>,
    mut state: ResMut<SpaceSenders>,
    mut budgets: ResMut<crate::server::SnapshotBudgets>,
) {
    let active: BTreeSet<_> = peers.iter().map(|(entity, _, _)| entity).collect();
    state.0.retain(|entity, _| active.contains(entity));
    for (entity, mut sender, mut requests) in &mut peers {
        let peer = state.0.entry(entity).or_default();
        // Coalesce retries; never allocate one transfer per request.
        if requests.receive().count() > 0 {
            peer.replacement = true;
        }
        if peer.pending.is_empty() {
            let scope: BTreeSet<_> = subscriptions
                .chunks_for(entity)
                .map(|coord| coord.0.to_array())
                .collect();
            let observed = (published.revision, published.pending.clone(), scope.clone());
            if !peer.replacement && peer.observed.as_ref() == Some(&observed) {
                continue;
            }
            let Ok(next) = snapshot(scope, &published, &identities) else {
                continue;
            };
            peer.observed = Some(observed);
            if peer.replacement || peer.sent.as_ref() != Some(&next) {
                let previous = if peer.replacement {
                    None
                } else {
                    peer.sent.as_ref()
                };
                let delta = changes(previous, &next);
                let encoded = (|| {
                    let bytes = bincode::serde::encode_to_vec(&delta, bincode::config::standard())
                        .map_err(|e| e.to_string())?;
                    let sequence = peer
                        .sequence
                        .checked_add(1)
                        .ok_or("spatial sequence exhausted")?;
                    let frames = crate::spatial_revision::encode(
                        SpaceDataset::ID,
                        SpaceDataset::SCHEMA,
                        sequence,
                        previous.map(|_| peer.sequence),
                        bytes,
                    )?;
                    frames
                        .into_iter()
                        .map(|frame| {
                            let message = crate::protocol::SpatialMessage::Revision(frame);
                            let size = bincode::serde::encode_to_vec(
                                &message,
                                bincode::config::standard(),
                            )
                            .map_err(|e| e.to_string())?
                            .len();
                            Ok((message, size))
                        })
                        .collect::<Result<VecDeque<_>, String>>()
                })();
                match encoded {
                    Ok(frames) => {
                        peer.pending = frames;
                        peer.sequence += 1;
                        peer.sent = Some(next);
                        peer.replacement = false;
                    }
                    Err(error) => {
                        warn!(%error,"cannot encode architectural revision");
                        continue;
                    }
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
    mut spaces: ResMut<ClientSpaces>,
    mut senders: Query<&mut lightyear::prelude::MessageSender<RequestSpaceSnapshot>>,
) {
    if !spaces.replacement_requested {
        return;
    }
    for mut sender in &mut senders {
        sender.send::<crate::protocol::WorldChannel>(RequestSpaceSnapshot);
        spaces.replacement_requested = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(revision: u64) -> SpaceSnapshot {
        SpaceSnapshot {
            revision,
            chunks: bounded(vec![[0, 0, 0]]).unwrap(),
            terrain_basis: bounded(vec![([0, 0, 0], revision)]).unwrap(),
            spaces: bounded(vec![SpaceView {
                id: 1,
                revision,
                structural_area: 1,
                covered: 1,
                unknown_roof: 0,
                volume: Volume::Bounded(2),
                pending: false,
                floors: bounded(vec![FloorSample {
                    cell: [1, 1, 1],
                    roof: Height::At(4),
                    clearance: Height::At(4),
                }])
                .unwrap(),
                placements: BoundedVec::default(),
                connections: BoundedVec::default(),
            }])
            .unwrap(),
        }
    }
    fn frames(
        old: Option<&SpaceSnapshot>,
        new: &SpaceSnapshot,
        sequence: u64,
        base: Option<u64>,
    ) -> VecDeque<RevisionFrame> {
        crate::spatial_revision::encode(
            SpaceDataset::ID,
            SpaceDataset::SCHEMA,
            sequence,
            base,
            bincode::serde::encode_to_vec(changes(old, new), bincode::config::standard()).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn staged_pages_and_rejected_changes_preserve_last_valid_snapshot() {
        let first = state(1);
        let mut client = ClientSpaces::default();
        let mut initial = frames(None, &first, 1, None);
        while initial.len() > 1 {
            client.receive(initial.pop_front().unwrap()).unwrap();
            assert!(client.decoded.spaces.is_empty());
        }
        client.receive(initial.pop_front().unwrap()).unwrap();
        assert_eq!(client.decoded, first);
        assert!(client.visible.spaces.is_empty());
        let mut invalid = state(2);
        invalid.spaces =
            bounded(vec![invalid.spaces[0].clone(), invalid.spaces[0].clone()]).unwrap();
        let mut transfer = frames(Some(&first), &invalid, 2, Some(1));
        while transfer.len() > 1 {
            client.receive(transfer.pop_front().unwrap()).unwrap();
        }
        assert!(client.receive(transfer.pop_front().unwrap()).is_err());
        assert_eq!(client.decoded, first);
        assert!(client.replacement_requested);
        for frame in frames(None, &state(3), 3, None) {
            client.receive(frame).unwrap();
        }
        assert_eq!(client.decoded, state(3));
        assert!(!client.replacement_requested);
    }
    #[test]
    fn terrain_basis_and_subscription_loss_control_exposure() {
        let mut app = App::new();
        app.init_resource::<ClientSpaces>()
            .init_resource::<crate::voxel::ChunkMap>()
            .add_systems(Update, expose_ready_revision);
        let coord = ChunkCoord(IVec3::ZERO);
        let entity = app
            .world_mut()
            .spawn(crate::voxel::Chunk {
                revision: 0,
                blocks: Vec::new(),
            })
            .id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(coord, entity);
        for frame in frames(None, &state(1), 1, None) {
            app.world_mut()
                .resource_mut::<ClientSpaces>()
                .receive(frame)
                .unwrap();
        }
        app.update();
        assert!(
            app.world()
                .resource::<ClientSpaces>()
                .visible
                .spaces
                .is_empty()
        );
        app.world_mut()
            .get_mut::<crate::voxel::Chunk>(entity)
            .unwrap()
            .revision = 1;
        app.update();
        assert_eq!(app.world().resource::<ClientSpaces>().visible, state(1));
        app.world_mut()
            .get_mut::<crate::voxel::Chunk>(entity)
            .unwrap()
            .revision = 2;
        app.update();
        assert!(app.world().resource::<ClientSpaces>().waiting_for_terrain);
        for frame in frames(Some(&state(1)), &state(2), 2, Some(1)) {
            app.world_mut()
                .resource_mut::<ClientSpaces>()
                .receive(frame)
                .unwrap();
        }
        app.update();
        assert_eq!(app.world().resource::<ClientSpaces>().visible, state(2));
        app.world_mut().resource_mut::<ClientSpaces>().leave(coord);
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .clear();
        app.update();
        assert!(
            app.world()
                .resource::<ClientSpaces>()
                .visible
                .spaces
                .is_empty()
        );
        // Decoded state remains intact for subsequent deltas, and the same
        // durable space can become visible again after a chunk re-entry.
        assert_eq!(app.world().resource::<ClientSpaces>().decoded, state(2));
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(coord, entity);
        app.world_mut().resource_mut::<ClientSpaces>().enter(coord);
        app.update();
        assert_eq!(app.world().resource::<ClientSpaces>().visible, state(2));
    }
    #[test]
    fn payload_cannot_claim_replacement_on_a_delta_base() {
        let mut client = ClientSpaces::default();
        for frame in frames(None, &state(1), 1, None) {
            client.receive(frame).unwrap();
        }
        let mut invalid = frames(None, &state(2), 2, Some(1));
        while invalid.len() > 1 {
            client.receive(invalid.pop_front().unwrap()).unwrap();
        }
        assert!(client.receive(invalid.pop_front().unwrap()).is_err());
        assert_eq!(client.decoded, state(1));
    }
}

#[cfg(test)]
mod inspection_membership_tests {
    use super::*;
    #[test]
    fn stacked_storeys_and_unknown_clearance_have_distinct_membership() {
        let room = |id, y, top| SpaceView {
            id,
            revision: 1,
            structural_area: 1,
            covered: 1,
            unknown_roof: 0,
            volume: Volume::Bounded(3),
            pending: false,
            floors: bounded(vec![FloorSample {
                cell: [2, y, 2],
                roof: Height::At(top),
                clearance: Height::At(top),
            }])
            .unwrap(),
            placements: bounded(vec![id + 10]).unwrap(),
            connections: bounded(vec![]).unwrap(),
        };
        let downstairs = room(1, 1, 4);
        let mut upstairs = room(2, 5, 8);
        let cell = IVec3::new(2, 5, 2);
        assert!(!downstairs.contains_cell(cell, None));
        assert!(upstairs.contains_cell(cell, None));
        assert!(upstairs.contains_cell(IVec3::new(2, 4, 2), None));
        assert!(upstairs.contains_cell(IVec3::new(3, 5, 2), Some(12)));
        upstairs.floors = bounded(vec![FloorSample {
            cell: [2, 5, 2],
            roof: Height::Unknown,
            clearance: Height::Unknown,
        }])
        .unwrap();
        assert!(!upstairs.contains_cell(IVec3::new(2, 6, 2), None));
    }
}
