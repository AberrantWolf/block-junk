//! Trait-driven storage and wire primitives for chunk-scoped state.
//!
//! This module is the synchronization boundary: gameplay owns concrete
//! datasets, while indexing, mutation journaling, snapshotting and replica
//! cleanup are implemented once here.

#![allow(
    dead_code,
    reason = "framework entry points are exercised by dataset plugins and staged migration tests"
)]

use std::any::type_name;
use std::hash::Hash;
use std::marker::PhantomData;
use std::time::Duration;
use std::time::Instant;

use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use lightyear::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::menu::AppState;
use crate::protocol::{BoundedVec, ChunkCoord};

pub const MAX_PARTITION_PAGE_BYTES: usize = 32 * 1024;
pub const MAX_PENDING_SPATIAL_BYTES: usize = 4 * 1024 * 1024;
pub const SPATIAL_BYTES_PER_SECOND: usize = 32 * 1024;
pub const SPATIAL_BURST_BYTES: usize = 128 * 1024;
pub const TERRAIN_DATASET_ID: DatasetId = DatasetId(0);
pub const TERRAIN_SCHEMA_FINGERPRINT: u64 = 0x7465_7272_0000_0001;

pub struct GlobalAudience;

impl GlobalAudience {
    pub fn target() -> NetworkTarget {
        NetworkTarget::All
    }
}

#[derive(Component, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct SpatialReplica;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub enum SpatialScope {
    Point(Vec3),
    Bounds { min: Vec3, max: Vec3 },
}

impl SpatialScope {
    pub fn intersects(&self, chunks: &HashSet<ChunkCoord>) -> bool {
        match *self {
            Self::Point(point) => chunks.contains(&world_point_chunk(point)),
            Self::Bounds { min, max } => {
                let min = world_point_chunk(min).0;
                let max = world_point_chunk(max).0;
                chunks.iter().any(|chunk| {
                    let value = chunk.0;
                    value.cmpge(min).all() && value.cmple(max).all()
                })
            }
        }
    }
}

fn world_point_chunk(point: Vec3) -> ChunkCoord {
    let cell = point.floor().as_ivec3();
    crate::voxel::world_to_chunk(cell).0
}

pub fn ordinary_spatial_replica(scope: SpatialScope) -> (SpatialReplica, SpatialScope, Replicate) {
    (
        SpatialReplica,
        scope,
        Replicate::to_clients(NetworkTarget::None),
    )
}

pub fn interpolated_spatial_replica(
    scope: SpatialScope,
) -> (SpatialReplica, SpatialScope, Replicate, InterpolationTarget) {
    (
        SpatialReplica,
        scope,
        Replicate::to_clients(NetworkTarget::None),
        InterpolationTarget::to_clients(NetworkTarget::All),
    )
}

pub fn predicted_owner_avatar(
    scope: SpatialScope,
    owner: PeerId,
) -> (
    SpatialReplica,
    SpatialScope,
    Replicate,
    PredictionTarget,
    InterpolationTarget,
) {
    (
        SpatialReplica,
        scope,
        Replicate::to_clients(NetworkTarget::None),
        PredictionTarget::to_clients(NetworkTarget::Single(owner)),
        InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(owner)),
    )
}

pub fn gain_replica_visibility(commands: &mut Commands, entity: Entity, connection: Entity) {
    use lightyear::prelude::VisibilityExt as _;
    commands.gain_visibility(entity, connection);
}

pub fn lose_replica_visibility(commands: &mut Commands, entity: Entity, connection: Entity) {
    use lightyear::prelude::VisibilityExt as _;
    commands.lose_visibility(entity, connection);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct DatasetId(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipPolicy {
    AnchorCell,
    Bounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicationPolicy {
    Immediate,
    Coalesced(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistencePolicy {
    Persisted,
    Ephemeral,
}

pub trait PersistenceAdapter<D: SpatialDataset>: Send + Sync + 'static {
    const POLICY: PersistencePolicy;
}

pub struct PersistedDataset;
pub struct EphemeralDataset;

impl<D: SpatialDataset> PersistenceAdapter<D> for PersistedDataset {
    const POLICY: PersistencePolicy = PersistencePolicy::Persisted;
}

impl<D: SpatialDataset> PersistenceAdapter<D> for EphemeralDataset {
    const POLICY: PersistencePolicy = PersistencePolicy::Ephemeral;
}

/// A bounded, client-visible dataset. Implementing this trait is required
/// before a value can be placed in [`PartitionedStore`].
pub trait SpatialDataset: Send + Sync + Sized + 'static {
    type Key: Clone + Eq + Hash + Send + Sync + Serialize + DeserializeOwned + 'static;
    type Value: Clone + PartialEq + Send + Sync + 'static;
    type Wire: Clone + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Persistence: PersistenceAdapter<Self>;

    const ID: DatasetId;
    const SCHEMA_FINGERPRINT: u64;
    const MEMBERSHIP: MembershipPolicy;
    const REPLICATION: ReplicationPolicy;
    const MAX_RECORD_BYTES: usize;

    fn chunks(key: &Self::Key, value: &Self::Value) -> Vec<ChunkCoord>;
    fn to_wire(key: &Self::Key, value: &Self::Value) -> Self::Wire;
    fn from_wire(
        wire: Self::Wire,
        registry: &SpatialDecodeRegistry,
    ) -> Result<(Self::Key, Self::Value), SpatialError>;
}

#[derive(Default, Debug)]
pub struct SpatialDecodeRegistry {
    references: HashMap<&'static str, HashSet<u32>>,
}

impl SpatialDecodeRegistry {
    pub fn register_reference(&mut self, namespace: &'static str, value: u32) {
        self.references.entry(namespace).or_default().insert(value);
    }

    pub fn require(&self, namespace: &'static str, value: u32) -> Result<(), SpatialError> {
        self.references
            .get(namespace)
            .is_some_and(|values| values.contains(&value))
            .then_some(())
            .ok_or(SpatialError::UnknownRegistryReference { namespace, value })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpatialError {
    #[error("dataset {0:?} is not registered")]
    UnknownDataset(DatasetId),
    #[error("dataset schema mismatch: expected {expected:#x}, got {actual:#x}")]
    SchemaMismatch { expected: u64, actual: u64 },
    #[error("spatial record exceeds its {limit}-byte bound: {actual} bytes")]
    RecordTooLarge { limit: usize, actual: usize },
    #[error("spatial record has {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("invalid spatial record: {0}")]
    Decode(String),
    #[error("unknown {namespace} registry reference {value}")]
    UnknownRegistryReference { namespace: &'static str, value: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum DirtyValue<V> {
    Upsert(V),
    Remove,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SpatialDelta<K, W> {
    Upsert(W),
    Remove(K),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirtyRecord<K, V> {
    pub key: K,
    pub value: DirtyValue<V>,
    pub chunks_before: Vec<ChunkCoord>,
    pub chunks_after: Vec<ChunkCoord>,
    pub tick: u64,
}

/// Sparse authoritative store and its reverse chunk index. The maps are
/// deliberately private; mutation is only possible through journaling guards.
#[derive(Resource)]
pub struct PartitionedStore<D: SpatialDataset> {
    values: HashMap<D::Key, D::Value>,
    chunk_to_keys: HashMap<ChunkCoord, HashSet<D::Key>>,
    key_to_chunks: HashMap<D::Key, HashSet<ChunkCoord>>,
    dirty: HashMap<D::Key, DirtyRecord<D::Key, D::Value>>,
    dirty_since: HashMap<D::Key, Instant>,
    force_dirty: HashSet<D::Key>,
    _dataset: PhantomData<D>,
}

impl<D: SpatialDataset> Default for PartitionedStore<D> {
    fn default() -> Self {
        Self {
            values: HashMap::default(),
            chunk_to_keys: HashMap::default(),
            key_to_chunks: HashMap::default(),
            dirty: HashMap::default(),
            dirty_since: HashMap::default(),
            force_dirty: HashSet::default(),
            _dataset: PhantomData,
        }
    }
}

impl<D: SpatialDataset> PartitionedStore<D> {
    pub fn lookup(&self, key: &D::Key) -> Option<&D::Value> {
        self.values.get(key)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&D::Key, &D::Value)> {
        self.values.iter()
    }

    pub fn keys_in_chunk(&self, chunk: ChunkCoord) -> impl Iterator<Item = &D::Key> {
        self.chunk_to_keys.get(&chunk).into_iter().flatten()
    }

    pub fn upsert(&mut self, key: D::Key, value: D::Value, tick: u64) -> Option<D::Value> {
        let before = self.membership(&key);
        let previous = self.values.insert(key.clone(), value.clone());
        let after = normalized_chunks(D::chunks(&key, &value));
        self.reindex(&key, &before, &after);
        self.record_dirty(key, DirtyValue::Upsert(value), before, after, tick);
        previous
    }

    pub fn delete(&mut self, key: &D::Key, tick: u64) -> Option<D::Value> {
        let before = self.membership(key);
        let previous = self.values.remove(key)?;
        self.reindex(key, &before, &[]);
        self.record_dirty(key.clone(), DirtyValue::Remove, before, Vec::new(), tick);
        Some(previous)
    }

    /// Mutate a value while automatically comparing its membership before and
    /// after the closure. Even an in-place change that keeps membership stable
    /// is journaled for replication.
    pub fn edit<R>(
        &mut self,
        key: &D::Key,
        tick: u64,
        edit: impl FnOnce(&mut D::Value) -> R,
    ) -> Option<R> {
        let before = self.membership(key);
        let (result, value) = {
            let value = self.values.get_mut(key)?;
            let result = edit(value);
            (result, value.clone())
        };
        let after = normalized_chunks(D::chunks(key, &value));
        self.reindex(key, &before, &after);
        self.record_dirty(key.clone(), DirtyValue::Upsert(value), before, after, tick);
        Some(result)
    }

    pub fn take_dirty(&mut self) -> Vec<DirtyRecord<D::Key, D::Value>> {
        self.take_ready_dirty_at(Instant::now())
    }

    pub fn force_flush(&mut self, key: &D::Key) {
        if self.dirty.contains_key(key) {
            self.force_dirty.insert(key.clone());
        }
    }

    fn take_ready_dirty_at(&mut self, now: Instant) -> Vec<DirtyRecord<D::Key, D::Value>> {
        let ready: Vec<_> = self
            .dirty
            .keys()
            .filter(|key| match D::REPLICATION {
                ReplicationPolicy::Immediate => true,
                ReplicationPolicy::Coalesced(delay) => {
                    self.force_dirty.contains(*key)
                        || self
                            .dirty_since
                            .get(*key)
                            .is_some_and(|since| now.saturating_duration_since(*since) >= delay)
                }
            })
            .cloned()
            .collect();
        ready
            .into_iter()
            .filter_map(|key| {
                self.dirty_since.remove(&key);
                self.force_dirty.remove(&key);
                self.dirty.remove(&key)
            })
            .collect()
    }

    pub(crate) fn apply_replica_delta(
        &mut self,
        payload: &[u8],
        registry: &SpatialDecodeRegistry,
        active_chunks: &HashSet<ChunkCoord>,
    ) -> Result<(), SpatialError> {
        let (delta, consumed): (SpatialDelta<D::Key, D::Wire>, usize) =
            bincode::serde::decode_from_slice(
                payload,
                bincode::config::standard().with_limit::<MAX_PARTITION_PAGE_BYTES>(),
            )
            .map_err(|error| SpatialError::Decode(error.to_string()))?;
        if consumed != payload.len() {
            return Err(SpatialError::TrailingBytes(payload.len() - consumed));
        }
        match delta {
            SpatialDelta::Upsert(wire) => {
                let (key, value) = D::from_wire(wire, registry)?;
                let chunks: Vec<_> = normalized_chunks(D::chunks(&key, &value))
                    .into_iter()
                    .filter(|chunk| active_chunks.contains(chunk))
                    .collect();
                let before = self.membership(&key);
                if chunks.is_empty() {
                    self.values.remove(&key);
                } else {
                    self.values.insert(key.clone(), value);
                }
                self.reindex(&key, &before, &chunks);
            }
            SpatialDelta::Remove(key) => {
                self.values.remove(&key);
                let before = self.membership(&key);
                self.reindex(&key, &before, &[]);
            }
        }
        Ok(())
    }

    pub fn encoded_snapshot(&self, chunk: ChunkCoord) -> Result<Vec<Vec<u8>>, SpatialError> {
        let mut records: Vec<_> = self
            .keys_in_chunk(chunk)
            .filter_map(|key| self.values.get(key).map(|value| (key, value)))
            .map(|(key, value)| encode_record::<D>(&D::to_wire(key, value)))
            .collect::<Result<_, _>>()?;
        records.sort_unstable();
        Ok(records)
    }

    fn membership(&self, key: &D::Key) -> Vec<ChunkCoord> {
        self.key_to_chunks
            .get(key)
            .map(|chunks| chunks.iter().copied().collect())
            .unwrap_or_default()
    }

    fn reindex(&mut self, key: &D::Key, before: &[ChunkCoord], after: &[ChunkCoord]) {
        let before: HashSet<_> = before.iter().copied().collect();
        let after: HashSet<_> = after.iter().copied().collect();
        for chunk in before.difference(&after) {
            if let Some(keys) = self.chunk_to_keys.get_mut(chunk) {
                keys.remove(key);
                if keys.is_empty() {
                    self.chunk_to_keys.remove(chunk);
                }
            }
        }
        for chunk in after.difference(&before) {
            self.chunk_to_keys
                .entry(*chunk)
                .or_default()
                .insert(key.clone());
        }
        if after.is_empty() {
            self.key_to_chunks.remove(key);
        } else {
            self.key_to_chunks.insert(key.clone(), after);
        }
    }

    fn record_dirty(
        &mut self,
        key: D::Key,
        value: DirtyValue<D::Value>,
        before: Vec<ChunkCoord>,
        after: Vec<ChunkCoord>,
        tick: u64,
    ) {
        if let Some(existing) = self.dirty.get_mut(&key)
            && existing.tick == tick
        {
            existing.value = value;
            existing.chunks_after = after;
            return;
        }
        self.dirty_since
            .entry(key.clone())
            .or_insert_with(Instant::now);
        self.dirty.insert(
            key.clone(),
            DirtyRecord {
                key,
                value,
                chunks_before: before,
                chunks_after: after,
                tick,
            },
        );
    }

    /// Framework-only snapshot application. A multi-chunk value is retained
    /// until its last chunk reference is unloaded.
    pub(crate) fn apply_replica_record(
        &mut self,
        source_chunk: ChunkCoord,
        payload: &[u8],
        registry: &SpatialDecodeRegistry,
    ) -> Result<(), SpatialError> {
        let wire = decode_record::<D>(payload)?;
        let (key, value) = D::from_wire(wire, registry)?;
        self.values.insert(key.clone(), value);
        self.chunk_to_keys
            .entry(source_chunk)
            .or_default()
            .insert(key.clone());
        self.key_to_chunks
            .entry(key)
            .or_default()
            .insert(source_chunk);
        Ok(())
    }

    pub(crate) fn unload_replica_chunk(&mut self, chunk: ChunkCoord) {
        let Some(keys) = self.chunk_to_keys.remove(&chunk) else {
            return;
        };
        for key in keys {
            let remove_value = if let Some(chunks) = self.key_to_chunks.get_mut(&key) {
                chunks.remove(&chunk);
                chunks.is_empty()
            } else {
                true
            };
            if remove_value {
                self.key_to_chunks.remove(&key);
                self.values.remove(&key);
            }
        }
    }
}

fn normalized_chunks(chunks: Vec<ChunkCoord>) -> Vec<ChunkCoord> {
    let mut unique = HashSet::new();
    chunks
        .into_iter()
        .filter(|chunk| unique.insert(*chunk))
        .collect()
}

pub fn encode_record<D: SpatialDataset>(wire: &D::Wire) -> Result<Vec<u8>, SpatialError> {
    let bytes = bincode::serde::encode_to_vec(wire, bincode::config::standard())
        .map_err(|error| SpatialError::Decode(error.to_string()))?;
    let limit = D::MAX_RECORD_BYTES.min(MAX_PARTITION_PAGE_BYTES);
    if bytes.len() > limit {
        return Err(SpatialError::RecordTooLarge {
            limit,
            actual: bytes.len(),
        });
    }
    Ok(bytes)
}

pub fn decode_record<D: SpatialDataset>(payload: &[u8]) -> Result<D::Wire, SpatialError> {
    let limit = D::MAX_RECORD_BYTES.min(MAX_PARTITION_PAGE_BYTES);
    if payload.len() > limit {
        return Err(SpatialError::RecordTooLarge {
            limit,
            actual: payload.len(),
        });
    }
    let config = bincode::config::standard().with_limit::<MAX_PARTITION_PAGE_BYTES>();
    let (wire, consumed) = bincode::serde::decode_from_slice(payload, config)
        .map_err(|error| SpatialError::Decode(error.to_string()))?;
    if consumed != payload.len() {
        return Err(SpatialError::TrailingBytes(payload.len() - consumed));
    }
    Ok(wire)
}

#[derive(Clone, Debug)]
pub struct DatasetRegistration {
    pub id: DatasetId,
    pub schema_fingerprint: u64,
    pub type_name: &'static str,
    pub max_record_bytes: usize,
    pub persistence: PersistencePolicy,
}

#[derive(Resource, Default, Debug)]
pub struct SpatialDatasetRegistry {
    datasets: HashMap<DatasetId, DatasetRegistration>,
    fingerprints: HashSet<u64>,
}

#[derive(Clone, Debug)]
pub struct SpatialSnapshotPage {
    pub dataset: DatasetId,
    pub schema_fingerprint: u64,
    pub payload: Vec<u8>,
}

/// Type-erased snapshot pages populated by each generic feature plugin.
/// AOI streaming reads this cache without knowing any concrete dataset types.
#[derive(Resource, Default)]
pub struct SpatialSnapshotCache {
    pages: HashMap<(DatasetId, ChunkCoord), CachedSnapshotPages>,
}

struct CachedSnapshotPages {
    schema_fingerprint: u64,
    records: Vec<Vec<u8>>,
}

impl SpatialSnapshotCache {
    pub fn pages(&self, chunk: ChunkCoord) -> std::vec::IntoIter<SpatialSnapshotPage> {
        let mut pages: Vec<_> = self
            .pages
            .iter()
            .filter(|((_, candidate), _)| *candidate == chunk)
            .flat_map(|((dataset, _), cached)| {
                cached
                    .records
                    .iter()
                    .cloned()
                    .map(|payload| SpatialSnapshotPage {
                        dataset: *dataset,
                        schema_fingerprint: cached.schema_fingerprint,
                        payload,
                    })
            })
            .collect();
        pages.sort_unstable_by(|left, right| {
            left.dataset
                .0
                .cmp(&right.dataset.0)
                .then_with(|| left.payload.cmp(&right.payload))
        });
        pages.into_iter()
    }

    fn replace<D: SpatialDataset>(&mut self, chunk: ChunkCoord, records: Vec<Vec<u8>>) {
        let key = (D::ID, chunk);
        if records.is_empty() {
            self.pages.remove(&key);
        } else {
            self.pages.insert(
                key,
                CachedSnapshotPages {
                    schema_fingerprint: D::SCHEMA_FINGERPRINT,
                    records,
                },
            );
        }
    }
}

impl SpatialDatasetRegistry {
    pub fn register<D: SpatialDataset>(&mut self) {
        assert!(
            D::MAX_RECORD_BYTES <= MAX_PARTITION_PAGE_BYTES,
            "{} declares an oversized wire record",
            type_name::<D>()
        );
        assert!(
            !self.datasets.contains_key(&D::ID),
            "duplicate spatial dataset id {:?}",
            D::ID
        );
        assert!(
            self.fingerprints.insert(D::SCHEMA_FINGERPRINT),
            "duplicate spatial schema fingerprint {:#x}",
            D::SCHEMA_FINGERPRINT
        );
        self.datasets.insert(
            D::ID,
            DatasetRegistration {
                id: D::ID,
                schema_fingerprint: D::SCHEMA_FINGERPRINT,
                type_name: type_name::<D>(),
                max_record_bytes: D::MAX_RECORD_BYTES,
                persistence: D::Persistence::POLICY,
            },
        );
    }

    pub fn get(&self, id: DatasetId) -> Option<&DatasetRegistration> {
        self.datasets.get(&id)
    }

    pub fn validate_record(
        &self,
        id: DatasetId,
        schema_fingerprint: u64,
        payload_len: usize,
    ) -> Result<&DatasetRegistration, SpatialError> {
        let registration = self.get(id).ok_or(SpatialError::UnknownDataset(id))?;
        if registration.schema_fingerprint != schema_fingerprint {
            return Err(SpatialError::SchemaMismatch {
                expected: registration.schema_fingerprint,
                actual: schema_fingerprint,
            });
        }
        if payload_len > registration.max_record_bytes {
            return Err(SpatialError::RecordTooLarge {
                limit: registration.max_record_bytes,
                actual: payload_len,
            });
        }
        Ok(registration)
    }

    pub fn validate_delta(
        &self,
        id: DatasetId,
        schema_fingerprint: u64,
        payload_len: usize,
    ) -> Result<&DatasetRegistration, SpatialError> {
        let registration = self.get(id).ok_or(SpatialError::UnknownDataset(id))?;
        if registration.schema_fingerprint != schema_fingerprint {
            return Err(SpatialError::SchemaMismatch {
                expected: registration.schema_fingerprint,
                actual: schema_fingerprint,
            });
        }
        if payload_len > MAX_PARTITION_PAGE_BYTES {
            return Err(SpatialError::RecordTooLarge {
                limit: MAX_PARTITION_PAGE_BYTES,
                actual: payload_len,
            });
        }
        Ok(registration)
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let mut entries: Vec<_> = self.datasets.values().collect();
        entries.sort_by_key(|entry| entry.id.0);
        let mut hasher = blake3::Hasher::new();
        for entry in entries {
            hasher.update(&entry.id.0.to_le_bytes());
            hasher.update(&entry.schema_fingerprint.to_le_bytes());
            hasher.update(&(entry.max_record_bytes as u64).to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}

/// Installs the non-optional storage and registry side of a synchronized
/// feature. Network snapshot/delta dispatch is keyed by the same registration.
pub struct SpatialFeaturePlugin<D: SpatialDataset> {
    server: bool,
    marker: PhantomData<D>,
}

impl<D: SpatialDataset> Default for SpatialFeaturePlugin<D> {
    fn default() -> Self {
        Self::client()
    }
}

impl<D: SpatialDataset> SpatialFeaturePlugin<D> {
    pub fn server() -> Self {
        Self {
            server: true,
            marker: PhantomData,
        }
    }

    pub fn client() -> Self {
        Self {
            server: false,
            marker: PhantomData,
        }
    }
}

impl<D: SpatialDataset> Plugin for SpatialFeaturePlugin<D> {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpatialDatasetRegistry>();
        app.world_mut()
            .resource_mut::<SpatialDatasetRegistry>()
            .register::<D>();
        init_session_resource::<PartitionedStore<D>>(app);
        if self.server {
            app.init_resource::<SpatialSnapshotCache>();
            app.add_systems(
                Update,
                flush_spatial_dataset::<D>.in_set(crate::protocol::GameSet::SpatialSync),
            );
        }
    }
}

pub fn init_session_resource<T: Resource + Default>(app: &mut App) {
    app.init_resource::<T>();
    app.add_systems(OnExit(AppState::InGame), reset_session_resource::<T>);
}

pub fn remove_session_resource_on_exit<T: Resource>(app: &mut App) {
    app.add_systems(OnExit(AppState::InGame), remove_session_resource::<T>);
}

fn reset_session_resource<T: Resource + Default>(mut commands: Commands) {
    commands.insert_resource(T::default());
}

fn remove_session_resource<T: Resource>(mut commands: Commands) {
    commands.remove_resource::<T>();
}

pub fn bounded_payload(
    bytes: Vec<u8>,
) -> Result<BoundedVec<u8, MAX_PARTITION_PAGE_BYTES>, SpatialError> {
    BoundedVec::new(bytes).map_err(|error| SpatialError::RecordTooLarge {
        limit: error.limit,
        actual: error.actual,
    })
}

pub(crate) fn flush_spatial_dataset<D: SpatialDataset>(
    mut store: ResMut<PartitionedStore<D>>,
    mut snapshots: ResMut<SpatialSnapshotCache>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    subscriptions: Res<crate::server::SpatialSubscriptions>,
) {
    let Ok(server) = servers.single() else { return };
    let dirty_records = store.take_dirty();
    let affected_chunks: HashSet<_> = dirty_records
        .iter()
        .flat_map(|dirty| dirty.chunks_before.iter().chain(&dirty.chunks_after))
        .copied()
        .collect();
    for chunk in affected_chunks {
        match store.encoded_snapshot(chunk) {
            Ok(records) => snapshots.replace::<D>(chunk, records),
            Err(error) => warn!(%error, dataset = ?D::ID, ?chunk, "snapshot cache refresh failed"),
        }
    }
    for dirty in dirty_records {
        let chunks: HashSet<_> = dirty
            .chunks_before
            .iter()
            .chain(&dirty.chunks_after)
            .copied()
            .collect();
        let target = subscriptions.target_for_chunks(chunks);
        let delta = match dirty.value {
            DirtyValue::Upsert(value) => SpatialDelta::Upsert(D::to_wire(&dirty.key, &value)),
            DirtyValue::Remove => SpatialDelta::Remove(dirty.key),
        };
        let Ok(bytes) = bincode::serde::encode_to_vec(&delta, bincode::config::standard()) else {
            continue;
        };
        let Ok(payload) = BoundedVec::new(bytes) else {
            continue;
        };
        let message = crate::protocol::SpatialMessage::Delta {
            dataset: D::ID,
            schema_fingerprint: D::SCHEMA_FINGERPRINT,
            payload,
        };
        if let Err(error) = broadcast
            .send::<crate::protocol::SpatialMessage, crate::protocol::SpatialChannel>(
                &message, server, &target,
            )
        {
            warn!(%error, dataset = ?D::ID, "spatial delta send failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
    struct Value {
        key: u32,
        min: IVec3,
        max: IVec3,
        amount: u32,
    }

    struct TestDataset;
    impl SpatialDataset for TestDataset {
        type Key = u32;
        type Value = Value;
        type Wire = Value;
        type Persistence = EphemeralDataset;
        const ID: DatasetId = DatasetId(65000);
        const SCHEMA_FINGERPRINT: u64 = 0x54_45_53_54;
        const MEMBERSHIP: MembershipPolicy = MembershipPolicy::Bounds;
        const REPLICATION: ReplicationPolicy = ReplicationPolicy::Immediate;
        const MAX_RECORD_BYTES: usize = 256;
        fn chunks(_: &Self::Key, value: &Self::Value) -> Vec<ChunkCoord> {
            let min = crate::voxel::world_to_chunk(value.min).0.0;
            let max = crate::voxel::world_to_chunk(value.max).0.0;
            let mut result = Vec::new();
            for x in min.x..=max.x {
                for y in min.y..=max.y {
                    for z in min.z..=max.z {
                        result.push(ChunkCoord(IVec3::new(x, y, z)));
                    }
                }
            }
            result
        }
        fn to_wire(_: &Self::Key, value: &Self::Value) -> Self::Wire {
            value.clone()
        }
        fn from_wire(
            wire: Self::Wire,
            _: &SpatialDecodeRegistry,
        ) -> Result<(Self::Key, Self::Value), SpatialError> {
            Ok((wire.key, wire))
        }
    }

    struct SlowDataset;
    impl SpatialDataset for SlowDataset {
        type Key = <TestDataset as SpatialDataset>::Key;
        type Value = <TestDataset as SpatialDataset>::Value;
        type Wire = <TestDataset as SpatialDataset>::Wire;
        type Persistence = EphemeralDataset;
        const ID: DatasetId = DatasetId(64999);
        const SCHEMA_FINGERPRINT: u64 = 0x53_4c_4f_57;
        const MEMBERSHIP: MembershipPolicy = MembershipPolicy::Bounds;
        const REPLICATION: ReplicationPolicy =
            ReplicationPolicy::Coalesced(Duration::from_millis(250));
        const MAX_RECORD_BYTES: usize = 256;
        fn chunks(key: &Self::Key, value: &Self::Value) -> Vec<ChunkCoord> {
            TestDataset::chunks(key, value)
        }
        fn to_wire(key: &Self::Key, value: &Self::Value) -> Self::Wire {
            TestDataset::to_wire(key, value)
        }
        fn from_wire(
            wire: Self::Wire,
            registry: &SpatialDecodeRegistry,
        ) -> Result<(Self::Key, Self::Value), SpatialError> {
            TestDataset::from_wire(wire, registry)
        }
    }

    fn value(key: u32, min: IVec3, max: IVec3, amount: u32) -> Value {
        Value {
            key,
            min,
            max,
            amount,
        }
    }

    #[test]
    fn mutations_coalesce_and_membership_reindexes() {
        let mut store = PartitionedStore::<TestDataset>::default();
        store.upsert(1, value(1, IVec3::ZERO, IVec3::ZERO, 1), 7);
        store.edit(&1, 7, |value| {
            value.max.x = 40;
            value.amount = 2;
        });
        let dirty = store.take_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].chunks_before, Vec::<ChunkCoord>::new());
        assert_eq!(dirty[0].chunks_after.len(), 2);
        assert_eq!(store.keys_in_chunk(ChunkCoord(IVec3::X)).count(), 1);
    }

    #[test]
    fn snapshot_cache_is_type_erased_bounded_and_deterministic() {
        let chunk = ChunkCoord(IVec3::ZERO);
        let mut store = PartitionedStore::<TestDataset>::default();
        store.upsert(2, value(2, IVec3::ZERO, IVec3::ZERO, 2), 1);
        store.upsert(1, value(1, IVec3::ZERO, IVec3::ZERO, 1), 1);
        let records = store.encoded_snapshot(chunk).unwrap();
        assert!(records.windows(2).all(|pair| pair[0] <= pair[1]));

        let mut cache = SpatialSnapshotCache::default();
        cache.replace::<TestDataset>(chunk, records.clone());
        let pages: Vec<_> = cache.pages(chunk).collect();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].dataset, TestDataset::ID);
        assert_eq!(pages[0].schema_fingerprint, TestDataset::SCHEMA_FINGERPRINT);
        assert_eq!(
            pages.iter().map(|page| &page.payload).collect::<Vec<_>>(),
            records.iter().collect::<Vec<_>>()
        );

        cache.replace::<TestDataset>(chunk, Vec::new());
        assert_eq!(cache.pages(chunk).count(), 0);
    }

    #[test]
    fn coalesced_dataset_waits_unless_force_flushed() {
        let mut store = PartitionedStore::<SlowDataset>::default();
        store.upsert(1, value(1, IVec3::ZERO, IVec3::ZERO, 1), 1);
        let since = store.dirty_since[&1];
        assert!(
            store
                .take_ready_dirty_at(since + Duration::from_millis(249))
                .is_empty()
        );
        assert_eq!(
            store
                .take_ready_dirty_at(since + Duration::from_millis(250))
                .len(),
            1
        );

        store.upsert(2, value(2, IVec3::ZERO, IVec3::ZERO, 1), 2);
        store.force_flush(&2);
        assert_eq!(store.take_dirty().len(), 1);
    }

    #[test]
    fn snapshot_decode_rejects_trailing_bytes() {
        let encoded = encode_record::<TestDataset>(&value(1, IVec3::ZERO, IVec3::ZERO, 2)).unwrap();
        let mut hostile = encoded.clone();
        hostile.push(0);
        assert!(matches!(
            decode_record::<TestDataset>(&hostile),
            Err(SpatialError::TrailingBytes(1))
        ));
    }

    #[test]
    fn replica_retains_multichunk_value_until_last_unload() {
        let mut store = PartitionedStore::<TestDataset>::default();
        let payload =
            encode_record::<TestDataset>(&value(1, IVec3::ZERO, IVec3::new(40, 0, 0), 2)).unwrap();
        let registry = SpatialDecodeRegistry::default();
        store
            .apply_replica_record(ChunkCoord(IVec3::ZERO), &payload, &registry)
            .unwrap();
        store
            .apply_replica_record(ChunkCoord(IVec3::X), &payload, &registry)
            .unwrap();
        store.unload_replica_chunk(ChunkCoord(IVec3::ZERO));
        assert!(store.lookup(&1).is_some());
        store.unload_replica_chunk(ChunkCoord(IVec3::X));
        assert!(store.lookup(&1).is_none());
    }

    #[test]
    fn replica_delta_applies_upsert_and_remove_without_journaling() {
        let registry = SpatialDecodeRegistry::default();
        let mut store = PartitionedStore::<TestDataset>::default();
        let upsert =
            SpatialDelta::<u32, Value>::Upsert(value(7, IVec3::ZERO, IVec3::new(40, 0, 0), 3));
        let payload = bincode::serde::encode_to_vec(&upsert, bincode::config::standard()).unwrap();
        let active = [ChunkCoord(IVec3::ZERO), ChunkCoord(IVec3::X)]
            .into_iter()
            .collect();
        store
            .apply_replica_delta(&payload, &registry, &active)
            .unwrap();
        assert_eq!(store.lookup(&7).unwrap().amount, 3);
        assert_eq!(store.keys_in_chunk(ChunkCoord(IVec3::X)).count(), 1);
        assert!(store.take_dirty().is_empty());

        let remove = SpatialDelta::<u32, Value>::Remove(7);
        let payload = bincode::serde::encode_to_vec(&remove, bincode::config::standard()).unwrap();
        store
            .apply_replica_delta(&payload, &registry, &active)
            .unwrap();
        assert!(store.lookup(&7).is_none());
        assert_eq!(store.keys_in_chunk(ChunkCoord(IVec3::X)).count(), 0);
    }

    #[test]
    fn replica_delta_never_creates_references_to_inactive_chunks() {
        let registry = SpatialDecodeRegistry::default();
        let mut store = PartitionedStore::<TestDataset>::default();
        let upsert =
            SpatialDelta::<u32, Value>::Upsert(value(9, IVec3::ZERO, IVec3::new(40, 0, 0), 1));
        let payload = bincode::serde::encode_to_vec(&upsert, bincode::config::standard()).unwrap();
        let active = [ChunkCoord(IVec3::ZERO)].into_iter().collect();
        store
            .apply_replica_delta(&payload, &registry, &active)
            .unwrap();
        assert_eq!(store.keys_in_chunk(ChunkCoord(IVec3::ZERO)).count(), 1);
        assert_eq!(store.keys_in_chunk(ChunkCoord(IVec3::X)).count(), 0);
        store.unload_replica_chunk(ChunkCoord(IVec3::ZERO));
        assert!(store.lookup(&9).is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate spatial dataset id")]
    fn registry_rejects_duplicate_dataset() {
        let mut registry = SpatialDatasetRegistry::default();
        registry.register::<TestDataset>();
        registry.register::<TestDataset>();
    }

    #[test]
    fn registry_rejects_unknown_schema_and_oversized_records() {
        let mut registry = SpatialDatasetRegistry::default();
        registry.register::<TestDataset>();
        assert!(matches!(
            registry.validate_record(TestDataset::ID, 7, 1),
            Err(SpatialError::SchemaMismatch { .. })
        ));
        assert!(matches!(
            registry.validate_record(TestDataset::ID, TestDataset::SCHEMA_FINGERPRINT, 257),
            Err(SpatialError::RecordTooLarge { .. })
        ));
        assert!(matches!(
            registry.validate_record(DatasetId(1), 0, 0),
            Err(SpatialError::UnknownDataset(DatasetId(1)))
        ));
    }

    proptest! {
        #[test]
        fn hostile_record_bytes_never_escape_bounds(payload in proptest::collection::vec(any::<u8>(), 0..70_000)) {
            let result = decode_record::<TestDataset>(&payload);
            if payload.len() > TestDataset::MAX_RECORD_BYTES {
                prop_assert!(result.is_err());
            }
        }

        #[test]
        fn room_chunk_membership_is_bounded_and_reference_counted(
            x in -32i32..32,
            width in 0i32..64,
        ) {
            use crate::spatial::SpatialDataset as _;
            let room = crate::protocol::RoomSummary {
                room_id: 1,
                pattern: "vanilla:test".to_owned(),
                anchor: IVec3::new(x, 0, 0),
                bbox_min: IVec3::new(x, 0, 0),
                bbox_max: IVec3::new(x + width, 2, 2),
                floor_area: width as u32 + 1,
            };
            let chunks = crate::room_sync::RoomSummaryDataset::chunks(&1, &room);
            prop_assert!(!chunks.is_empty());
            prop_assert!(chunks.len() <= 3);
            let wire = crate::room_sync::RoomSummaryDataset::to_wire(&1, &room);
            let encoded = encode_record::<crate::room_sync::RoomSummaryDataset>(&wire).unwrap();
            let decoded = decode_record::<crate::room_sync::RoomSummaryDataset>(&encoded).unwrap();
            prop_assert_eq!(decoded.room_id, room.room_id);
            prop_assert_eq!(decoded.pattern.as_ref(), room.pattern);
        }
    }

    #[derive(Resource, Default)]
    struct SessionValue(u32);

    #[test]
    fn two_session_exits_reset_owned_resources() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_state::<AppState>();
        init_session_resource::<SessionValue>(&mut app);
        for dirty_value in [42, 99] {
            app.world_mut()
                .resource_mut::<NextState<AppState>>()
                .set(AppState::InGame);
            app.update();
            app.world_mut().resource_mut::<SessionValue>().0 = dirty_value;
            app.world_mut()
                .resource_mut::<NextState<AppState>>()
                .set(AppState::MainMenu);
            app.update();
            app.update();
            assert_eq!(app.world().resource::<SessionValue>().0, 0);
        }
    }
}
