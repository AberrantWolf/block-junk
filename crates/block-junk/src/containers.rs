//! Per-cell container stock — S3 of the storage arc.
//!
//! A block whose def carries [`ContainerConfig`] is a storage
//! container: every placed instance can hold items, tracked here as a
//! [`ContainerState`] keyed by cell. The shape deliberately mirrors
//! `CraftStations` (the other per-cell inventory map): the server mutates
//! through spatial guards, clients keep passive replicas, and empty states
//! drop out so replication never ships orphan entries.
//!
//! Stock is *not* made of `WorldItem` entities — it's counts in a map,
//! like a station's inventory. The bulk math ([`ItemDef::bulk`] vs
//! [`ContainerConfig::capacity_bulk`]) decides how much fits; the
//! `accepts` tag filter decides what the tidy job will *put* here.
//! Withdrawal (build/craft hauling) ignores `accepts` — anything
//! stocked can come back out.
//!
//! Destroying a container block spills its stock as loose items on the
//! same `CellEdit` bus pattern the stations use.
//!
//! [`ContainerConfig`]: block_junk_mod_api::blocks::ContainerConfig
//! [`ItemDef::bulk`]: block_junk_mod_api::items::ItemDef::bulk
//! [`ContainerConfig::capacity_bulk`]: block_junk_mod_api::blocks::ContainerConfig::capacity_bulk

use std::collections::HashMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::items::ItemSlot;
use crate::protocol::GameSet;

pub struct ContainerDataset;

pub const MAX_CONTAINER_INVENTORY_KINDS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContainerWireRecord {
    cell: IVec3,
    inventory: crate::protocol::BoundedVec<(ItemSlot, u32), MAX_CONTAINER_INVENTORY_KINDS>,
}

impl crate::spatial::SpatialDataset for ContainerDataset {
    type Key = IVec3;
    type Value = ContainerState;
    type Wire = ContainerWireRecord;
    type Persistence = crate::spatial::PersistedDataset;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(3);
    const SCHEMA_FINGERPRINT: u64 = 0x636f_6e74_0000_0001;
    const MEMBERSHIP: crate::spatial::MembershipPolicy =
        crate::spatial::MembershipPolicy::AnchorCell;
    const REPLICATION: crate::spatial::ReplicationPolicy =
        crate::spatial::ReplicationPolicy::Immediate;
    const MAX_RECORD_BYTES: usize = 2048;
    fn chunks(key: &Self::Key, _: &Self::Value) -> Vec<crate::protocol::ChunkCoord> {
        vec![crate::voxel::world_to_chunk(*key).0]
    }
    fn to_wire(key: &Self::Key, value: &Self::Value) -> Self::Wire {
        ContainerWireRecord {
            cell: *key,
            inventory: crate::protocol::BoundedVec::new(
                value
                    .inventory
                    .iter()
                    .map(|(&item, &count)| (item, count))
                    .collect(),
            )
            .expect("authoritative containers enforce the inventory bound"),
        }
    }
    fn from_wire(
        wire: Self::Wire,
        registry: &crate::spatial::SpatialDecodeRegistry,
    ) -> Result<(Self::Key, Self::Value), crate::spatial::SpatialError> {
        let mut inventory = HashMap::default();
        for (item, count) in wire.inventory {
            registry.require("item", item.0 as u32)?;
            if count == 0 || inventory.insert(item, count).is_some() {
                return Err(crate::spatial::SpatialError::Decode(
                    "invalid container inventory entry".into(),
                ));
            }
        }
        Ok((wire.cell, ContainerState { inventory }))
    }
}

/// Stock of one placed container block. Just an inventory — containers
/// have no orders or work timers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerState {
    /// Items stocked, by slot. Counts of 0 are scrubbed so iteration
    /// only ever sees real entries.
    pub inventory: HashMap<ItemSlot, u32>,
}

impl ContainerState {
    /// True when nothing is stocked — caller drops the entry from the
    /// by-cell map.
    pub fn is_empty(&self) -> bool {
        self.inventory.is_empty()
    }

    /// Add `count` of `item`. 0 is a no-op.
    pub fn deposit(&mut self, item: ItemSlot, count: u32) -> bool {
        if count == 0 {
            return true;
        }
        if !self.inventory.contains_key(&item)
            && self.inventory.len() >= MAX_CONTAINER_INVENTORY_KINDS
        {
            return false;
        }
        let entry = self.inventory.entry(item).or_insert(0);
        *entry = entry.saturating_add(count);
        true
    }

    /// Units of `item` currently stocked.
    pub fn available(&self, item: ItemSlot) -> u32 {
        self.inventory.get(&item).copied().unwrap_or(0)
    }

    /// Any stocked item slot (counts are always > 0 — zeros are
    /// scrubbed). Used by the S4 finite-food eat to pick something to
    /// draw down from a food container: a food-only basket holds nothing
    /// but food, so "any stocked item" is "a berry to eat." Iteration
    /// order is unspecified; a mixed-food basket feeds an arbitrary kind.
    pub fn any_stocked(&self) -> Option<ItemSlot> {
        self.inventory.keys().copied().next()
    }

    /// Remove up to `count` of `item`; returns how many actually came
    /// out (short stock withdraws what's there). Zero entries scrub.
    pub fn withdraw_up_to(&mut self, item: ItemSlot, count: u32) -> u32 {
        let Some(entry) = self.inventory.get_mut(&item) else {
            return 0;
        };
        let taken = count.min(*entry);
        *entry -= taken;
        if *entry == 0 {
            self.inventory.remove(&item);
        }
        taken
    }

    /// Total bulk the current stock occupies.
    pub fn used_bulk(&self, item_registry: &crate::items::ItemRegistry) -> u32 {
        self.inventory
            .iter()
            .map(|(slot, count)| count * item_registry.def(*slot).bulk.max(1))
            .sum()
    }
}

/// Server-authoritative container stock and its client spatial replica.
/// Sparse: a placed-but-empty container has no entry (the block def alone
/// says "this is a container").
pub type Containers = crate::spatial::PartitionedStore<ContainerDataset>;

impl Containers {
    pub fn get(&self, cell: IVec3) -> Option<&ContainerState> {
        self.lookup(&cell)
    }

    pub fn edit_existing<R>(
        &mut self,
        cell: IVec3,
        edit: impl FnOnce(&mut ContainerState) -> R,
    ) -> Option<R> {
        self.edit(&cell, 0, edit)
    }

    pub fn edit_or_insert<R>(
        &mut self,
        cell: IVec3,
        edit: impl FnOnce(&mut ContainerState) -> R,
    ) -> R {
        if self.lookup(&cell).is_none() {
            self.upsert(cell, ContainerState::default(), 0);
        }
        self.edit(&cell, 0, edit)
            .expect("container inserted before edit")
    }

    /// Drop the entry at `cell` if its state is empty.
    pub fn remove_if_empty(&mut self, cell: IVec3) {
        if let Some(state) = self.lookup(&cell)
            && state.is_empty()
        {
            self.delete(&cell, 0);
        }
    }

    pub fn remove(&mut self, cell: IVec3) -> Option<ContainerState> {
        self.delete(&cell, 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &ContainerState)> {
        self.entries()
    }

    pub(crate) fn restore_all(
        &mut self,
        entries: impl IntoIterator<Item = (IVec3, ContainerState)>,
    ) {
        let old: Vec<_> = self.entries().map(|(cell, _)| *cell).collect();
        for cell in old {
            self.delete(&cell, 0);
        }
        for (cell, state) in entries {
            if !state.is_empty() {
                self.upsert(cell, state, 0);
            }
        }
    }
}

/// How many more units of `item` fit into a container with `cfg`,
/// given its current state (`None` ⇒ empty). Bulk math: remaining
/// bulk divided by the item's per-unit bulk, so heavy items fill a
/// container in fewer units.
pub fn room_for(
    state: Option<&ContainerState>,
    cfg: &block_junk_mod_api::blocks::ContainerConfig,
    item_registry: &crate::items::ItemRegistry,
    item: ItemSlot,
) -> u32 {
    if state.is_some_and(|state| {
        !state.inventory.contains_key(&item)
            && state.inventory.len() >= MAX_CONTAINER_INVENTORY_KINDS
    }) {
        return 0;
    }
    let used = state.map(|s| s.used_bulk(item_registry)).unwrap_or(0);
    let room_bulk = cfg.capacity_bulk.saturating_sub(used);
    room_bulk / item_registry.def(item).bulk.max(1)
}

/// Read the [`ContainerConfig`] of the block at `cell`, if the cell
/// currently holds a container block. The single validity check every
/// container path shares — a stale cell (block mined, chunk unloaded)
/// reads as `None` and the caller bails gracefully.
///
/// [`ContainerConfig`]: block_junk_mod_api::blocks::ContainerConfig
pub fn container_config_at<'a>(
    cell: IVec3,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
    block_registry: &'a crate::blocks::BlockRegistry,
) -> Option<&'a block_junk_mod_api::blocks::ContainerConfig> {
    let (coord, local) = crate::voxel::world_to_chunk(cell);
    let entity = chunk_map.0.get(&coord)?;
    let chunk = chunks.get(*entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    block_registry.def(slot).container.as_ref()
}

/// Server-side cell → container-block index, kept so the tidy
/// scheduler can find *empty* containers too (the sparse [`Containers`]
/// stock map only lists stocked ones). Same build pattern as
/// `InteractableIndex`: a chunk-add observer seeds it, the `CellEdit`
/// bus keeps it current. Values are the block slot so callers look the
/// [`ContainerConfig`] up from the registry rather than us caching a
/// copy that could drift.
///
/// [`ContainerConfig`]: block_junk_mod_api::blocks::ContainerConfig
#[derive(Resource, Default, Debug)]
pub struct ContainerIndex {
    cells: HashMap<IVec3, crate::blocks::BlockSlot>,
}

impl ContainerIndex {
    pub fn iter(&self) -> impl Iterator<Item = (IVec3, crate::blocks::BlockSlot)> + '_ {
        self.cells.iter().map(|(c, s)| (*c, *s))
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }
}

/// On every newly-spawned chunk entity, scan its interior for
/// container blocks and index them (save-loaded chunks may carry
/// containers from a prior session; fresh procedural chunks have
/// none).
fn scan_chunk_on_add(
    trigger: On<Add, crate::voxel::Chunk>,
    chunks: Query<(&crate::voxel::Chunk, &crate::protocol::ChunkCoord)>,
    registry: Res<crate::blocks::BlockRegistry>,
    mut index: ResMut<ContainerIndex>,
) {
    let Ok((chunk, coord)) = chunks.get(trigger.entity) else {
        return;
    };
    let padded = crate::protocol::CHUNK_PADDED as i32;
    for x in 1..(padded - 1) {
        for y in 1..(padded - 1) {
            for z in 1..(padded - 1) {
                let local = IVec3::new(x, y, z);
                let slot = chunk.get(local);
                if slot.is_empty() || registry.def(slot).container.is_none() {
                    continue;
                }
                let world = crate::voxel::chunk_local_to_world(*coord, local);
                index.cells.insert(world, slot);
            }
        }
    }
}

/// Mirror every `CellEdit` into the index: a container slot inserts,
/// anything else (including the empty slot from a break) removes.
fn apply_cell_edits_to_index(
    mut reader: MessageReader<crate::protocol::CellEdit>,
    mut index: ResMut<ContainerIndex>,
    registry: Res<crate::blocks::BlockRegistry>,
) {
    for edit in reader.read() {
        if !edit.slot.is_empty() && registry.def(edit.slot).container.is_some() {
            index.cells.insert(edit.world, edit.slot);
        } else {
            index.cells.remove(&edit.world);
        }
    }
}

pub struct ContainersServerPlugin;
pub struct ContainersClientPlugin;

impl Plugin for ContainersServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<ContainerDataset>::server());
        crate::spatial::init_session_resource::<Containers>(app);
        crate::spatial::init_session_resource::<ContainerIndex>(app);
        app.add_observer(scan_chunk_on_add);
        // Same schedule slot as the interactable index's CellEdit
        // consumer — both want the post-apply state of the same bus.
        app.add_systems(
            Update,
            apply_cell_edits_to_index.in_set(GameSet::PostSimulation),
        );
        // `clear_destroyed_containers` registers in server.rs alongside
        // `clear_destroyed_stations` — it must order after
        // `receive_block_edits`, which is private to that module.
    }
}

impl Plugin for ContainersClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<ContainerDataset>::client());
    }
}

/// Server: a container block was broken or replaced — spill its stock
/// as loose world items and drop the state, mirroring
/// `clear_destroyed_stations`. Spills one `WorldItem` *stack* per
/// stocked kind (counts survived as a pile), positioned with the same
/// per-unit jitter drops use so a busted crate reads as a burst.
pub(crate) fn clear_destroyed_containers(
    mut reader: MessageReader<crate::protocol::CellEdit>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    mut containers: ResMut<Containers>,
    mut commands: Commands,
) {
    for edit in reader.read() {
        if edit.prev_slot.is_empty() || block_registry.def(edit.prev_slot).container.is_none() {
            continue;
        }
        let Some(state) = containers.remove(edit.world) else {
            continue;
        };
        let centre = edit.world.as_vec3() + Vec3::new(0.5, 0.05, 0.5);
        let mut spilled = 0u32;
        for (kind_index, (slot, count)) in state.inventory.iter().enumerate() {
            let translation = centre + crate::items::drop_jitter(edit.world, kind_index as u32);
            spilled += count;
            commands.spawn((
                crate::protocol::WorldItem {
                    item: *slot,
                    translation,
                    count: *count,
                },
                Transform::from_translation(translation),
                GlobalTransform::default(),
                crate::spatial::ordinary_spatial_replica(crate::spatial::SpatialScope::Point(
                    translation,
                )),
                Name::new(format!("WorldItem(container_spill:{})", slot.0)),
            ));
        }
        info!(
            cell = ?edit.world.to_array(),
            spilled,
            "container destroyed; spilled stock",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(n: u16) -> ItemSlot {
        ItemSlot(n)
    }

    #[test]
    fn deposit_and_withdraw_roundtrip() {
        let mut s = ContainerState::default();
        s.deposit(slot(3), 5);
        assert_eq!(s.available(slot(3)), 5);
        assert_eq!(s.withdraw_up_to(slot(3), 2), 2);
        assert_eq!(s.available(slot(3)), 3);
        // Over-ask withdraws only what's there and scrubs the entry.
        assert_eq!(s.withdraw_up_to(slot(3), 10), 3);
        assert_eq!(s.available(slot(3)), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn withdraw_missing_kind_is_zero() {
        let mut s = ContainerState::default();
        s.deposit(slot(1), 2);
        assert_eq!(s.withdraw_up_to(slot(9), 1), 0);
        assert_eq!(s.available(slot(1)), 2);
    }

    #[test]
    fn zero_deposit_is_a_no_op() {
        let mut s = ContainerState::default();
        s.deposit(slot(1), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn distinct_inventory_kind_bound_rejects_without_mutation() {
        let mut state = ContainerState::default();
        for index in 0..MAX_CONTAINER_INVENTORY_KINDS {
            assert!(state.deposit(slot(index as u16), 1));
        }
        let before = state.clone();
        assert!(!state.deposit(slot(MAX_CONTAINER_INVENTORY_KINDS as u16), 1));
        assert_eq!(state, before);
        assert!(state.deposit(slot(0), 1));
        assert_eq!(state.available(slot(0)), 2);
    }

    #[test]
    fn restore_all_skips_empty_states() {
        let mut c = Containers::default();
        let mut stocked = ContainerState::default();
        stocked.deposit(slot(1), 1);
        c.restore_all(vec![
            (IVec3::ZERO, ContainerState::default()),
            (IVec3::ONE, stocked),
        ]);
        assert!(c.get(IVec3::ZERO).is_none());
        assert!(c.get(IVec3::ONE).is_some());
    }
}
