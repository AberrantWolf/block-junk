//! Per-cell container stock — S3 of the storage arc.
//!
//! A block whose def carries [`ContainerConfig`] is a storage
//! container: every placed instance can hold items, tracked here as a
//! [`ContainerState`] keyed by cell. The shape deliberately mirrors
//! `CraftStations` (the other per-cell inventory map): the server
//! mutates + broadcasts [`ContainerUpdate`]s, each client keeps a
//! passive mirror fed by those plus a connect-time
//! [`ContainersFullSync`], and empty states drop out of the map so
//! replication never ships orphan entries.
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
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

use crate::items::ItemSlot;
use crate::menu::AppState;
use crate::protocol::{GameSet, StateSyncChannel};

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
    pub fn deposit(&mut self, item: ItemSlot, count: u32) {
        if count == 0 {
            return;
        }
        *self.inventory.entry(item).or_insert(0) += count;
    }

    /// Units of `item` currently stocked.
    pub fn available(&self, item: ItemSlot) -> u32 {
        self.inventory.get(&item).copied().unwrap_or(0)
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

/// Server-authoritative + client-mirrored container stock map. Same
/// shape on both sides; the server mutates + broadcasts, the client
/// applies broadcasts. Sparse: a placed-but-empty container has no
/// entry (the block def alone says "this is a container").
#[derive(Resource, Default, Debug)]
pub struct Containers {
    by_cell: HashMap<IVec3, ContainerState>,
}

impl Containers {
    pub fn get(&self, cell: IVec3) -> Option<&ContainerState> {
        self.by_cell.get(&cell)
    }

    pub fn get_mut(&mut self, cell: IVec3) -> Option<&mut ContainerState> {
        self.by_cell.get_mut(&cell)
    }

    /// Get the state at `cell`, creating an empty one if needed.
    /// Caller is responsible for `remove_if_empty` after a mutation
    /// that could leave it empty.
    pub fn get_or_insert(&mut self, cell: IVec3) -> &mut ContainerState {
        self.by_cell.entry(cell).or_default()
    }

    /// Drop the entry at `cell` if its state is empty.
    pub fn remove_if_empty(&mut self, cell: IVec3) {
        if let Some(state) = self.by_cell.get(&cell)
            && state.is_empty()
        {
            self.by_cell.remove(&cell);
        }
    }

    pub fn remove(&mut self, cell: IVec3) -> Option<ContainerState> {
        self.by_cell.remove(&cell)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &ContainerState)> {
        self.by_cell.iter()
    }

    pub fn replace_all(&mut self, entries: impl IntoIterator<Item = (IVec3, ContainerState)>) {
        self.by_cell.clear();
        for (cell, state) in entries {
            if !state.is_empty() {
                self.by_cell.insert(cell, state);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<(IVec3, ContainerState)> {
        self.by_cell.iter().map(|(c, s)| (*c, s.clone())).collect()
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

/// Server → client: per-cell broadcast. `state: None` is the "remove
/// this cell" signal (stock fully withdrawn or block destroyed).
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ContainerUpdate {
    pub cell: IVec3,
    pub state: Option<ContainerState>,
}

/// Server → client: one-shot dump of every non-empty container, sent
/// when a client connects. Mirrors `StationsFullSync` exactly.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct ContainersFullSync {
    pub entries: Vec<(IVec3, ContainerState)>,
}

/// Helper: send one [`ContainerUpdate`] to every client.
pub(crate) fn broadcast_container(
    broadcast: &mut ServerMultiMessageSender,
    server: &Server,
    cell: IVec3,
    state: Option<ContainerState>,
) {
    let msg = ContainerUpdate { cell, state };
    if let Err(err) =
        broadcast.send::<ContainerUpdate, StateSyncChannel>(&msg, server, &NetworkTarget::All)
    {
        warn!("container update broadcast failed: {err}");
    }
}

pub struct ContainersServerPlugin;
pub struct ContainersClientPlugin;

impl Plugin for ContainersServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Containers>();
        app.init_resource::<ContainerIndex>();
        app.add_observer(send_containers_full_sync_on_connect);
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
        app.init_resource::<Containers>();
        // Full-sync before per-update receive: same same-tick ordering
        // rule as stations/plans (full sync must not clobber a delta
        // applied first).
        app.add_systems(
            Update,
            (
                receive_containers_full_sync,
                receive_container_update_broadcasts,
            )
                .chain()
                .in_set(GameSet::Simulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Server: push the whole container map to a fresh connection.
fn send_containers_full_sync_on_connect(
    trigger: On<Add, Connected>,
    containers: Res<Containers>,
    mut senders: Query<&mut MessageSender<ContainersFullSync>>,
) {
    let connection = trigger.entity;
    let Ok(mut sender) = senders.get_mut(connection) else {
        return;
    };
    sender.send::<StateSyncChannel>(ContainersFullSync {
        entries: containers.snapshot(),
    });
}

/// Client: replace the mirror with the connect-time snapshot.
fn receive_containers_full_sync(
    mut receivers: Query<&mut MessageReceiver<ContainersFullSync>>,
    mut containers: ResMut<Containers>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            containers.replace_all(sync.entries);
        }
    }
}

/// Client: apply broadcast deltas to the mirror.
fn receive_container_update_broadcasts(
    mut receivers: Query<&mut MessageReceiver<ContainerUpdate>>,
    mut containers: ResMut<Containers>,
) {
    for mut receiver in receivers.iter_mut() {
        for update in receiver.receive() {
            match update.state {
                Some(state) if !state.is_empty() => {
                    containers.by_cell.insert(update.cell, state);
                }
                _ => {
                    containers.by_cell.remove(&update.cell);
                }
            }
        }
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
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut commands: Commands,
) {
    let Ok(server) = servers.single() else {
        return;
    };
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
                Replicate::to_clients(NetworkTarget::All),
                Name::new(format!("WorldItem(container_spill:{})", slot.0)),
            ));
        }
        info!(
            cell = ?edit.world.to_array(),
            spilled,
            "container destroyed; spilled stock",
        );
        broadcast_container(&mut broadcast, server, edit.world, None);
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
    fn replace_all_skips_empty_states() {
        let mut c = Containers::default();
        let mut stocked = ContainerState::default();
        stocked.deposit(slot(1), 1);
        c.replace_all(vec![
            (IVec3::ZERO, ContainerState::default()),
            (IVec3::ONE, stocked),
        ]);
        assert!(c.get(IVec3::ZERO).is_none());
        assert!(c.get(IVec3::ONE).is_some());
    }
}
