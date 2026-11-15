//! Server-authoritative craft-order state, per station cell. Mirrors
//! to clients for the modal UI + outline tint.
//!
//! Each station has a `StationState` keyed by its cell coordinate:
//!
//! - `orders` is a queue of [`CraftOrder`]s. Player or NPC adds them
//!   via the craft-order modal; each holds a recipe id, a total
//!   requested quantity, and a completed-so-far counter. Order
//!   auto-clears when `completed == total`.
//! - `inventory` is a per-item-slot count of materials deposited at
//!   the station. Doesn't differentiate which order an item is "for"
//!   — orders share the pool and work draws from it as needed. Same
//!   reasoning as a Plans-style materials counter, but station-level
//!   so multiple orders can share a single deposit.
//!
//! Empty states (no orders + empty inventory) auto-remove from the
//! map so the by-cell HashMap stays sparse + replication doesn't ship
//! orphan entries.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

use crate::items::ItemSlot;
use crate::protocol::{ActionRejected, GameSet, INTERACT_REACH, RejectReason};
use crate::server::{send_rejection, within_reach};

pub struct StationDataset;

pub const MAX_STATION_INVENTORY_KINDS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StationOrderWire {
    id: u64,
    revision: u64,
    recipe_id: crate::protocol::BoundedString<{ crate::protocol::MAX_WIRE_ID_BYTES }>,
    total: u32,
    completed: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveWorkWire {
    order_id: u64,
    committed_inputs: crate::protocol::BoundedVec<(ItemSlot, u32), MAX_STATION_INVENTORY_KINDS>,
    recipe_id: crate::protocol::BoundedString<{ crate::protocol::MAX_WIRE_ID_BYTES }>,
    total_secs: f32,
    elapsed_secs: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StationWireRecord {
    cell: IVec3,
    orders: crate::protocol::BoundedVec<StationOrderWire, MAX_STATION_ORDERS>,
    inventory: crate::protocol::BoundedVec<(ItemSlot, u32), MAX_STATION_INVENTORY_KINDS>,
    active_work: Option<ActiveWorkWire>,
}

impl crate::spatial::SpatialDataset for StationDataset {
    type Key = IVec3;
    type Value = StationState;
    type Wire = StationWireRecord;
    type Persistence = crate::spatial::PersistedDataset;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(4);
    const SCHEMA_FINGERPRINT: u64 = 0x7374_6174_0000_0003;
    const MEMBERSHIP: crate::spatial::MembershipPolicy =
        crate::spatial::MembershipPolicy::AnchorCell;
    const REPLICATION: crate::spatial::ReplicationPolicy =
        crate::spatial::ReplicationPolicy::Coalesced(std::time::Duration::from_millis(250));
    const MAX_RECORD_BYTES: usize = 8192;
    fn chunks(key: &Self::Key, _: &Self::Value) -> Vec<crate::protocol::ChunkCoord> {
        vec![crate::voxel::world_to_chunk(*key).0]
    }
    fn to_wire(key: &Self::Key, value: &Self::Value) -> Self::Wire {
        let orders = value
            .orders
            .iter()
            .map(|order| StationOrderWire {
                id: order.id,
                revision: order.revision,
                recipe_id: crate::protocol::BoundedString::new(order.recipe_id.clone())
                    .expect("authoritative recipe ids enforce the wire bound"),
                total: order.total,
                completed: order.completed,
            })
            .collect();
        StationWireRecord {
            cell: *key,
            orders: crate::protocol::BoundedVec::new(orders)
                .expect("authoritative stations enforce the order bound"),
            inventory: crate::protocol::BoundedVec::new(
                value
                    .inventory
                    .iter()
                    .map(|(&item, &count)| (item, count))
                    .collect(),
            )
            .expect("authoritative stations enforce the inventory bound"),
            active_work: value.active_work.as_ref().map(|work| ActiveWorkWire {
                order_id: work.order_id,
                committed_inputs: crate::protocol::BoundedVec::new(work.committed_inputs.clone())
                    .expect("craft input bundle respects inventory bound"),
                recipe_id: crate::protocol::BoundedString::new(work.recipe_id.clone())
                    .expect("authoritative recipe ids enforce the wire bound"),
                total_secs: work.total_secs,
                elapsed_secs: work.elapsed_secs,
            }),
        }
    }
    fn from_wire(
        wire: Self::Wire,
        registry: &crate::spatial::SpatialDecodeRegistry,
    ) -> Result<(Self::Key, Self::Value), crate::spatial::SpatialError> {
        let mut orders = Vec::new();
        let mut seen_orders = HashSet::new();
        for order in wire.orders {
            if order.id == 0
                || !seen_orders.insert(order.id)
                || order.revision != u64::from(order.completed) + 1
                || order.total == 0
                || order.total > MAX_ORDER_QUANTITY
                || order.completed >= order.total
            {
                return Err(crate::spatial::SpatialError::Decode(
                    "invalid station order counts".into(),
                ));
            }
            orders.push(CraftOrder {
                id: order.id,
                revision: order.revision,
                recipe_id: order.recipe_id.to_string(),
                total: order.total,
                completed: order.completed,
            });
        }
        let mut inventory = HashMap::default();
        for (item, count) in wire.inventory {
            registry.require("item", item.0 as u32)?;
            if count == 0 || inventory.insert(item, count).is_some() {
                return Err(crate::spatial::SpatialError::Decode(
                    "invalid station inventory entry".into(),
                ));
            }
        }
        let active_work = wire.active_work.map(|work| ActiveWork {
            order_id: work.order_id,
            committed_inputs: work.committed_inputs.into_inner(),
            recipe_id: work.recipe_id.to_string(),
            total_secs: work.total_secs,
            elapsed_secs: work.elapsed_secs,
        });
        if active_work.as_ref().is_some_and(|work| {
            !orders
                .iter()
                .any(|order| order.id == work.order_id && order.recipe_id == work.recipe_id)
                || !work.total_secs.is_finite()
                || !work.elapsed_secs.is_finite()
                || work.total_secs <= 0.0
                || work.elapsed_secs < 0.0
                || work.elapsed_secs > work.total_secs
        }) {
            return Err(crate::spatial::SpatialError::Decode(
                "invalid station active work".into(),
            ));
        }
        let state = StationState {
            orders,
            inventory,
            active_work,
        };
        if let Some(work) = &state.active_work {
            let mut seen = HashSet::new();
            for &(item, count) in &work.committed_inputs {
                registry.require("item", item.0 as u32)?;
                if count == 0 || !seen.insert(item) {
                    return Err(crate::spatial::SpatialError::Decode(
                        "invalid committed input bundle".into(),
                    ));
                }
            }
        }
        if !state.refund_fits() {
            return Err(crate::spatial::SpatialError::Decode(
                "station cannot hold committed refund".into(),
            ));
        }
        Ok((wire.cell, state))
    }
}

/// World-owned monotonic order identities survive removal of empty stations and
/// destruction/replacement of facilities. IDs are never derived from recipe names.
#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CraftOrderIds {
    pub next: u64,
}
impl Default for CraftOrderIds {
    fn default() -> Self {
        Self { next: 1 }
    }
}
impl CraftOrderIds {
    pub fn queue(
        &mut self,
        state: &mut StationState,
        recipe: &crate::recipes::RecipeRegistry,
        station: &StationDefView,
        recipe_id: &str,
        quantity: u32,
    ) -> Result<u64, String> {
        let slot = recipe
            .slot_of(&recipe_id.into())
            .ok_or("unknown craft recipe")?;
        let def = recipe.def(slot);
        if def.station != station.tag || def.tier > station.tier {
            return Err("recipe is not supported by this station".into());
        }
        if quantity == 0 || quantity > MAX_ORDER_QUANTITY || self.next == 0 {
            return Err("invalid craft quantity or identity".into());
        }
        let next = self
            .next
            .checked_add(1)
            .ok_or("craft order identities exhausted")?;
        let id = self.next;
        if !state.try_queue(CraftOrder {
            id,
            revision: 1,
            recipe_id: recipe_id.into(),
            total: quantity,
            completed: 0,
        }) {
            return Err("craft order queue is full or identity conflicts".into());
        }
        self.next = next;
        Ok(id)
    }
}

/// One queued craft at a station. `total` is what the player asked
/// for; `completed` rises by 1 per Work cycle. When `completed ==
/// total` the order is finished and removed from the queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CraftOrder {
    pub id: u64,
    pub revision: u64,
    /// Stable recipe id ([`block_junk_mod_api::recipes::RecipeId`]).
    /// String here so wire + save formats stay decoupled from the
    /// recipe-slot interning.
    pub recipe_id: String,
    pub total: u32,
    pub completed: u32,
}

impl CraftOrder {
    pub fn is_done(&self) -> bool {
        self.completed >= self.total
    }
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.completed)
    }
}

/// An in-progress craft cycle at a station. Materials were consumed
/// from `inventory` at start (locked in) so the work can't be raced
/// — its inputs are already committed. On completion the output
/// spawns + `active_work` clears + the matching order's `completed`
/// bumps. A Cancel before completion refunds the consumed inputs
/// back into the inventory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveWork {
    pub order_id: u64,
    /// Exact inputs removed at start; never reconstructed from mutable content.
    pub committed_inputs: Vec<(ItemSlot, u32)>,
    pub recipe_id: String,
    /// Total seconds the recipe takes (snapshot of
    /// `recipe.duration_secs` at start, so a future modifier system
    /// can scale on per-craft start rather than per-tick lookup).
    pub total_secs: f32,
    /// Wall-time elapsed so far. Incremented by `tick_station_work`
    /// each FixedUpdate; when `elapsed_secs >= total_secs` the work
    /// completes.
    pub elapsed_secs: f32,
}

/// Full server-side state of one station cell, replicated through
/// [`StationDataset`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StationState {
    pub orders: Vec<CraftOrder>,
    /// Items deposited at the station, by slot. Shared across all
    /// orders. Counts of 0 are scrubbed so iteration only ever sees
    /// real entries.
    pub inventory: HashMap<ItemSlot, u32>,
    /// In-progress craft cycle, if any. At most one active work per
    /// station — players + NPCs share the single workspace. Other
    /// orders' Work buttons disable while this is `Some`.
    #[serde(default)]
    pub active_work: Option<ActiveWork>,
}

impl StationState {
    pub fn transfer_from_carry(
        &mut self,
        carry: &mut crate::protocol::Carrying,
        count: u32,
    ) -> u32 {
        let Some(item) = carry.item else {
            return 0;
        };
        let taken = count.min(carry.count);
        if taken == 0 || !self.deposit(item, taken) {
            return 0;
        }
        carry.count -= taken;
        if carry.count == 0 {
            carry.item = None;
        }
        taken
    }
    pub fn transfer_to_carry(
        &mut self,
        carry: &mut crate::protocol::Carrying,
        item: ItemSlot,
        count: u32,
        capacity: u32,
    ) -> u32 {
        let available = self.inventory.get(&item).copied().unwrap_or(0);
        let mut next = *carry;
        let taken = next.pickup_many(item, count.min(available), capacity);
        if taken == 0 {
            return 0;
        }
        let remaining = available - taken;
        if remaining == 0 {
            self.inventory.remove(&item);
        } else {
            self.inventory.insert(item, remaining);
        }
        *carry = next;
        taken
    }
    /// True when nothing references this station — caller drops it
    /// from the by-cell map. Active work counts as "non-empty"
    /// even when orders and inventory are both empty (mid-cancel
    /// transitions briefly land here).
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty() && self.inventory.is_empty() && self.active_work.is_none()
    }

    /// Accept a deposit only when both stock and committed refunds fit. Failure
    /// leaves inventory untouched, including on integer overflow.
    pub fn deposit(&mut self, item: ItemSlot, count: u32) -> bool {
        if count == 0 {
            return true;
        }
        if !self.can_deposit_with_refund_reserve(item, count) {
            return false;
        }
        *self.inventory.entry(item).or_insert(0) += count;
        true
    }

    pub fn can_deposit_with_refund_reserve(&self, item: ItemSlot, count: u32) -> bool {
        let mut kinds: HashSet<_> = self.inventory.keys().copied().collect();
        kinds.insert(item);
        let mut committed = 0;
        if let Some(active) = &self.active_work {
            for &(slot, amount) in &active.committed_inputs {
                kinds.insert(slot);
                if slot == item {
                    committed = amount;
                }
            }
        }
        kinds.len() <= MAX_STATION_INVENTORY_KINDS
            && self
                .inventory
                .get(&item)
                .copied()
                .unwrap_or(0)
                .checked_add(count)
                .and_then(|v| v.checked_add(committed))
                .is_some()
    }

    fn refund_fits(&self) -> bool {
        self.active_work.as_ref().is_none_or(|work| {
            work.committed_inputs
                .iter()
                .all(|&(item, _)| self.can_deposit_with_refund_reserve(item, 0))
        })
    }

    /// Validate the complete bundle before any consumption. Repeated starts,
    /// duplicate inputs, missing stock and malformed durations do nothing.
    pub fn begin_work(
        &mut self,
        order_id: u64,
        recipe_id: String,
        total_secs: f32,
        inputs: Vec<(ItemSlot, u32)>,
    ) -> bool {
        if self.active_work.is_some()
            || order_id == 0
            || !total_secs.is_finite()
            || total_secs <= 0.0
            || inputs.len() > MAX_STATION_INVENTORY_KINDS
        {
            return false;
        }
        let mut seen = HashSet::new();
        if inputs.iter().any(|&(item, count)| {
            count == 0
                || !seen.insert(item)
                || self.inventory.get(&item).copied().unwrap_or(0) < count
        }) {
            return false;
        }
        for &(item, count) in &inputs {
            let value = self.inventory.get_mut(&item).expect("validated input");
            *value -= count;
            if *value == 0 {
                self.inventory.remove(&item);
            }
        }
        self.active_work = Some(ActiveWork {
            order_id,
            recipe_id,
            total_secs,
            elapsed_secs: 0.0,
            committed_inputs: inputs,
        });
        true
    }

    /// Refund exactly once. The deposit service reserves room for this bundle.
    /// Invalid imported state remains intact instead of discarding its escrow.
    pub fn cancel_work(&mut self) -> bool {
        if self.active_work.is_none() || !self.refund_fits() {
            return false;
        }
        let work = self.active_work.take().expect("checked active work");
        for (item, count) in work.committed_inputs {
            *self.inventory.entry(item).or_insert(0) += count;
        }
        true
    }

    pub(crate) fn prepare_order_work(
        &mut self,
        id: u64,
        revision: u64,
        station: &StationDefView,
        tool: Option<ItemSlot>,
        items: &crate::items::ItemRegistry,
        recipes: &crate::recipes::RecipeRegistry,
    ) -> Result<(), String> {
        let order = self
            .orders
            .iter()
            .find(|order| order.id == id && order.revision == revision)
            .ok_or("craft order missing or revision stale")?;
        let recipe = recipes
            .slot_of(&order.recipe_id.clone().into())
            .map(|slot| recipes.def(slot))
            .ok_or("craft recipe missing")?;
        if recipe.station != station.tag
            || recipe.tier > station.tier
            || recipe
                .required_tool
                .as_ref()
                .is_some_and(|tag| !items.tool_has_tag(tool, tag))
        {
            return Err("station or tool does not support craft recipe".into());
        }
        if let Some(active) = &self.active_work {
            return if active.order_id == id {
                Ok(())
            } else {
                Err("another order has committed inputs".into())
            };
        }
        let inputs = recipe
            .inputs
            .iter()
            .map(|input| items.slot_of(&input.item).map(|slot| (slot, input.count)))
            .collect::<Option<Vec<_>>>()
            .ok_or("craft ingredient missing")?;
        if !self.begin_work(id, recipe.id.to_string(), recipe.duration_secs, inputs) {
            return Err("craft inputs unavailable".into());
        }
        Ok(())
    }

    /// Cancel precisely the selected order. Stale UI/policy cannot cancel a
    /// different order that happens to request the same recipe.
    pub fn cancel_order(&mut self, id: u64, revision: u64) -> Result<bool, String> {
        let index = self
            .orders
            .iter()
            .position(|order| order.id == id && order.revision == revision)
            .ok_or("craft order missing or revision stale")?;
        let active = self
            .active_work
            .as_ref()
            .is_some_and(|work| work.order_id == id);
        if active && !self.cancel_work() {
            return Err("craft refund cannot fit inventory".into());
        }
        self.orders.remove(index);
        Ok(active)
    }

    /// Queue without exceeding the protocol/save bound. Returns false and
    /// leaves the state untouched once the station already has 64 orders.
    pub fn try_queue(&mut self, order: CraftOrder) -> bool {
        if order.id == 0
            || order.revision != u64::from(order.completed) + 1
            || order.total == 0
            || order.total > MAX_ORDER_QUANTITY
            || order.completed >= order.total
            || self.orders.iter().any(|existing| existing.id == order.id)
            || self.orders.len() >= MAX_STATION_ORDERS
        {
            return false;
        }
        self.orders.push(order);
        true
    }
}

/// Server-only paired booking state for craft stations. Wraps:
///
/// - The NPC ↔ station assignment table (bidirectional so the
///   scheduler can ask "is this station taken?" in O(1)) — Phase
///   6c-A "this NPC is going to work that workbench."
/// - The cell ↔ active-worker entity table — who is currently
///   powering each station's `active_work`. tick_station_work only
///   increments elapsed when this map has an entry.
///
/// The two share a lifecycle for NPC workers: an NPC arrives, both
/// the assignment (already held) AND a worker registration get
/// inserted; the NPC leaves (preempt, completion), both get cleared.
/// `release_npc_booking` is the atomic clear-both for that case.
/// Players never set the assignment side — they directly register as
/// workers via WorkStart, no en-route phase.
///
/// Doesn't persist across save/load — NPCs reset to Idle and the
/// scheduler re-pairs from scratch, same pattern as [`HaulStore`].
///
/// (crate::haul::HaulStore — the HaulStore reference above.)
#[derive(Clone, Debug)]
pub(crate) struct WorkerAuthority {
    pub entity: Entity,
    pub owner: block_junk_mod_api::authority::ReservationOwner,
}
impl WorkerAuthority {
    pub fn npc(entity: Entity, id: crate::npc::NpcId) -> Self {
        Self {
            entity,
            owner: block_junk_mod_api::authority::ReservationOwner::Npc { id: id.0 },
        }
    }
    pub fn player(entity: Entity, id: u64) -> Self {
        Self {
            entity,
            owner: block_junk_mod_api::authority::ReservationOwner::Player { id },
        }
    }
}

#[derive(Resource, Default, Debug, Clone)]
pub struct CraftBookings {
    pub jobs: crate::action_jobs::JobLedger,
    jobs_by_cell: HashMap<IVec3, u64>,
    reservations: crate::reservations::ReservationTable<IVec3>,
    station_by_npc: HashMap<crate::npc::NpcId, IVec3>,
    worker_by_cell: HashMap<IVec3, Entity>,
}
impl CraftBookings {
    pub(crate) fn prepare_job(
        &mut self,
        cell: IVec3,
        actor: block_junk_mod_api::authority::ReservationOwner,
        controller: block_junk_mod_api::authority::ReservationOwner,
        order_id: u64,
        order_revision: u64,
    ) -> Result<u64, String> {
        let mut next = self.clone();
        let block_junk_mod_api::authority::ReservationOwner::Npc { id: actor_id } = actor else {
            return Err("queued craft actor must be an NPC".into());
        };
        if !next.try_book_npc(crate::npc::NpcId(actor_id), cell) {
            return Err("craft actor or station is reserved".into());
        }
        let before = next.jobs.next;
        let id = next.jobs.start(
            actor,
            controller,
            block_junk_mod_api::authority::JobTarget::CraftOrder {
                order_id,
                order_revision,
            },
        )?;
        if id == before {
            next.jobs.records.get_mut(&id).expect("accepted job").state =
                block_junk_mod_api::authority::JobState::Accepted;
        }
        next.jobs_by_cell.insert(cell, id);
        *self = next;
        Ok(id)
    }
    pub(crate) fn begin_job(
        &mut self,
        cell: IVec3,
        worker: WorkerAuthority,
        controller: block_junk_mod_api::authority::ReservationOwner,
        order_id: u64,
        order_revision: u64,
    ) -> Result<u64, String> {
        let mut next = self.clone();
        if !next.register_worker(cell, worker.clone()) {
            return Err("craft station or actor is reserved".into());
        }
        let id = next.jobs.start(
            worker.owner,
            controller,
            block_junk_mod_api::authority::JobTarget::CraftOrder {
                order_id,
                order_revision,
            },
        )?;
        next.jobs_by_cell.insert(cell, id);
        *self = next;
        Ok(id)
    }
    pub(crate) fn resume_job(
        &mut self,
        cell: IVec3,
        worker: WorkerAuthority,
        state: &StationState,
    ) -> Result<u64, String> {
        let active = state
            .active_work
            .as_ref()
            .ok_or("no committed craft to resume")?;
        let order = state
            .orders
            .iter()
            .find(|order| order.id == active.order_id)
            .ok_or("committed order missing")?;
        self.begin_job(cell, worker.clone(), worker.owner, order.id, order.revision)
    }
    fn complete_job(&mut self, cell: IVec3) {
        if let Some(id) = self.jobs_by_cell.remove(&cell) {
            self.jobs
                .finish(id, block_junk_mod_api::authority::JobState::Completed, None);
        }
    }
    pub fn fail_station(&mut self, cell: IVec3) {
        if let Some(id) = self.jobs_by_cell.remove(&cell) {
            self.jobs.finish(
                id,
                block_junk_mod_api::authority::JobState::Failed,
                Some("craft facility removed".into()),
            );
        }
        self.force_clear_station(cell);
    }
    pub fn disable_mod_jobs(&mut self, mod_id: &str, reason: &str) {
        let jobs:Vec<_>=self.jobs.records.values().filter(|job|!job.state.terminal() && matches!(&job.controller,block_junk_mod_api::authority::ReservationOwner::Behavior {mod_id:owner,..} if owner==mod_id)).map(|job|job.id).collect();
        for id in jobs {
            self.jobs.finish(
                id,
                block_junk_mod_api::authority::JobState::Failed,
                Some(format!("controller disabled: {reason}")),
            );
            if let Some(cell) = self
                .jobs_by_cell
                .iter()
                .find(|(_, job)| **job == id)
                .map(|(cell, _)| *cell)
            {
                self.force_clear_station(cell);
            }
        }
    }
    pub fn fail_actor(&mut self, id: crate::npc::NpcId) {
        let jobs: Vec<_> = self
            .jobs
            .records
            .values()
            .filter(|job| {
                job.actor == block_junk_mod_api::authority::ReservationOwner::Npc { id: id.0 }
                    && !job.state.terminal()
            })
            .map(|job| job.id)
            .collect();
        for job in jobs {
            self.jobs.finish(
                job,
                block_junk_mod_api::authority::JobState::Failed,
                Some("actor removed".into()),
            );
        }
        if let Some(cell) = self.station_by_npc.get(&id).copied() {
            self.force_clear_station(cell);
        }
    }
    pub fn try_book_npc(&mut self, npc: crate::npc::NpcId, station: IVec3) -> bool {
        if self
            .station_by_npc
            .get(&npc)
            .is_some_and(|existing| *existing != station)
        {
            return false;
        }
        if self
            .reservations
            .acquire(
                station,
                block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 },
            )
            .is_none()
        {
            return false;
        }
        self.station_by_npc.insert(npc, station);
        true
    }
    pub fn station_taken_by_other(&self, station: IVec3, npc: crate::npc::NpcId) -> bool {
        self.reservations.holder(&station).is_some_and(|lease| {
            lease.owner != block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 }
        })
    }
    pub fn station_of_npc(&self, npc: crate::npc::NpcId) -> Option<IVec3> {
        self.station_by_npc.get(&npc).copied()
    }
    pub fn contains_npc(&self, npc: crate::npc::NpcId) -> bool {
        self.station_by_npc.contains_key(&npc)
    }
    pub fn release_npc_booking(&mut self, npc: crate::npc::NpcId, entity: Entity) {
        if self
            .station_by_npc
            .get(&npc)
            .and_then(|cell| self.worker_at(*cell))
            .is_some_and(|worker| worker != entity)
        {
            return;
        }
        if let Some(station) = self.station_by_npc.remove(&npc)
            && self.reservations.holder(&station).is_some_and(|lease| {
                lease.owner == block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 }
            })
        {
            self.force_clear_station(station);
        }
    }
    pub(crate) fn register_worker(&mut self, cell: IVec3, worker: WorkerAuthority) -> bool {
        if self.worker_by_cell.iter().any(|(at, entity)| {
            (*at == cell && *entity != worker.entity) || (*at != cell && *entity == worker.entity)
        }) {
            return false;
        }
        if let block_junk_mod_api::authority::ReservationOwner::Npc { id } = worker.owner
            && self
                .station_by_npc
                .get(&crate::npc::NpcId(id))
                .is_some_and(|at| *at != cell)
        {
            return false;
        }
        if self
            .reservations
            .acquire(cell, worker.owner.clone())
            .is_none()
        {
            return false;
        }
        if let block_junk_mod_api::authority::ReservationOwner::Npc { id } = worker.owner {
            self.station_by_npc.insert(crate::npc::NpcId(id), cell);
        }
        self.worker_by_cell.insert(cell, worker.entity);
        true
    }
    pub fn release_worker(&mut self, cell: IVec3, worker: Entity) {
        if self.worker_by_cell.get(&cell) == Some(&worker) {
            self.force_clear_station(cell);
        }
    }
    pub fn force_clear_worker(&mut self, cell: IVec3) {
        self.force_clear_station(cell);
    }
    pub fn release_all_workers_for(&mut self, entity: Entity) {
        let cells: Vec<_> = self
            .worker_by_cell
            .iter()
            .filter(|(_, worker)| **worker == entity)
            .map(|(cell, _)| *cell)
            .collect();
        for cell in cells {
            self.force_clear_station(cell);
        }
    }
    pub fn force_clear_station(&mut self, station: IVec3) {
        if let Some(id) = self.jobs_by_cell.remove(&station) {
            self.jobs.finish(
                id,
                block_junk_mod_api::authority::JobState::Interrupted,
                Some("worker released or order canceled".into()),
            );
        }
        if let Some(lease) = self.reservations.holder(&station)
            && let block_junk_mod_api::authority::ReservationOwner::Npc { id } = lease.owner
        {
            self.station_by_npc.remove(&crate::npc::NpcId(id));
        }
        self.reservations.revoke(&station);
        self.worker_by_cell.remove(&station);
    }
    pub fn has_worker(&self, cell: IVec3) -> bool {
        self.worker_by_cell.contains_key(&cell)
    }
    pub fn worker_at(&self, cell: IVec3) -> Option<Entity> {
        self.worker_by_cell.get(&cell).copied()
    }
    pub(crate) fn worker_authority(&self, cell: IVec3) -> Option<WorkerAuthority> {
        Some(WorkerAuthority {
            entity: self.worker_at(cell)?,
            owner: self.reservations.holder(&cell)?.owner.clone(),
        })
    }
}

/// Client-side check: does any queued order at this station have
/// every input satisfied by the current inventory? Used to gate the
/// L-click work-hold and the outline's "this station is engageable"
/// purple shade. Returns false if the station has no orders or no
/// queued order is fully stocked.
pub fn station_has_work_ready(
    state: &StationState,
    recipes: &crate::recipes::RecipeRegistry,
    items: &crate::items::ItemRegistry,
) -> bool {
    state.orders.iter().any(|order| {
        let Some(recipe_slot) = recipes.slot_of(&block_junk_mod_api::recipes::RecipeId::new(
            order.recipe_id.clone(),
        )) else {
            return false;
        };
        let def = recipes.def(recipe_slot);
        def.inputs.iter().all(|input| {
            let Some(input_slot) = items.slot_of(&input.item) else {
                return false;
            };
            state.inventory.get(&input_slot).copied().unwrap_or(0) >= input.count
        })
    })
}

/// Client-side check: would depositing one unit of `carry_item` at
/// this station help any queued order? True when at least one queued
/// recipe lists `carry_item` as an input and the station's inventory
/// is still below that input's count. Gates the L-click instant-
/// deposit fast-path so it doesn't fire when the player's carry is
/// irrelevant to whatever's queued.
pub fn station_needs_carry(
    state: &StationState,
    recipes: &crate::recipes::RecipeRegistry,
    items: &crate::items::ItemRegistry,
    carry_item: ItemSlot,
) -> bool {
    state.orders.iter().any(|order| {
        let Some(recipe_slot) = recipes.slot_of(&block_junk_mod_api::recipes::RecipeId::new(
            order.recipe_id.clone(),
        )) else {
            return false;
        };
        let def = recipes.def(recipe_slot);
        def.inputs.iter().any(|input| {
            let Some(input_slot) = items.slot_of(&input.item) else {
                return false;
            };
            if input_slot != carry_item {
                return false;
            }
            let have = state.inventory.get(&input_slot).copied().unwrap_or(0);
            have < input.count
        })
    })
}

/// Server-authoritative craft-order map and its client spatial replica.
pub type CraftStations = crate::spatial::PartitionedStore<StationDataset>;

impl CraftStations {
    pub fn get(&self, cell: IVec3) -> Option<&StationState> {
        self.lookup(&cell)
    }

    /// Get the state at `cell`, creating an empty one if needed.
    /// Caller is responsible for `remove_if_empty` after a mutation
    /// that could leave it empty.
    pub fn edit_or_insert<R>(
        &mut self,
        cell: IVec3,
        edit: impl FnOnce(&mut StationState) -> R,
    ) -> R {
        if self.lookup(&cell).is_none() {
            self.upsert(cell, StationState::default(), 0);
        }
        let result = self
            .edit(&cell, 0, edit)
            .expect("station inserted before edit");
        self.force_flush(&cell);
        result
    }

    /// Drop the entry at `cell` if its state is empty. Keeps the map
    /// sparse — `iter` only yields real stations.
    pub fn remove_if_empty(&mut self, cell: IVec3) {
        if let Some(state) = self.lookup(&cell)
            && state.is_empty()
        {
            self.delete(&cell, 0);
            self.force_flush(&cell);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &StationState)> {
        self.entries()
    }

    pub(crate) fn restore_all(&mut self, entries: impl IntoIterator<Item = (IVec3, StationState)>) {
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

/// Max Chebyshev distance (cells) from an NPC's foot to a craft
/// station the scheduler will commit to. Same magnitude as the haul
/// scheduler's plan radius so an NPC doesn't cross-map for one
/// distant anvil while a closer workbench sits with orders.
pub(crate) const MAX_CRAFT_STATION_RADIUS_CELLS: i32 = 48;

/// Per-NPC craft scheduler. Called from the brain-tick Idle entry
/// BEFORE the haul scheduler. Returns `Some(station_cell)` when it
/// books an assignment; the caller dispatches a `MoveTo` with
/// [`ArrivalAction::WorkStation`](crate::npc::ArrivalAction::WorkStation).
///
/// Pick rule: nearest station (Chebyshev cells, within
/// `MAX_CRAFT_STATION_RADIUS_CELLS`) where every gate clears:
///   1. Station's block def declares a `station_tag` (it is a station).
///   2. No active worker registered AND no in-progress `active_work`
///      (resume of abandoned work is a future feature; for now only
///      start-fresh orders qualify).
///   3. At least one queued order whose recipe `station == station_tag`,
///      `tier <= station_tier`, `required_tool` (if any) matches the
///      NPC's `EquippedTool`, and whose inputs the station inventory
///      currently satisfies.
///   4. Not already booked in `CraftAssignments` by another NPC.
///
/// Atomic: the final `try_reserve` is the only mutation. A scheduler
/// call that picks a target but loses the reservation race returns
/// `None` and the next tick retries.
#[allow(
    clippy::too_many_arguments,
    reason = "scheduler reaches into many subsystems"
)]
pub fn try_schedule_craft_for_npc(
    npc_id: crate::npc::NpcId,
    pose: Vec3,
    equipped_tool: &crate::protocol::EquippedTool,
    stations: &CraftStations,
    bookings: &mut CraftBookings,
    block_registry: &crate::blocks::BlockRegistry,
    recipes: &crate::recipes::RecipeRegistry,
    item_registry: &crate::items::ItemRegistry,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
) -> Option<IVec3> {
    use block_junk_mod_api::recipes::RecipeId;

    if bookings.contains_npc(npc_id) {
        return None;
    }
    let foot = IVec3::new(
        pose.x.floor() as i32,
        pose.y.floor() as i32,
        pose.z.floor() as i32,
    );

    let mut best: Option<(IVec3, i32)> = None;
    for (cell, state) in stations.iter() {
        if bookings.station_taken_by_other(*cell, npc_id) {
            continue;
        }
        if bookings.has_worker(*cell) {
            continue;
        }
        // An in-progress craft whose worker left (preempt, player
        // disconnect) is RESUMABLE — without this branch a paused
        // active_work blocked the whole station forever: the ticker
        // won't advance worker-less work, and the queue behind it
        // never drains. Resume requires the right tool for the active
        // recipe; fresh starts go through the order checks below.
        let resume = state.active_work.is_some();
        if resume {
            let tool_ok = state
                .active_work
                .as_ref()
                .and_then(|aw| {
                    let recipe_id = RecipeId::new(aw.recipe_id.clone());
                    recipes.slot_of(&recipe_id).map(|rs| {
                        recipes
                            .def(rs)
                            .required_tool
                            .as_ref()
                            .map(|tag| item_registry.tool_has_tag(equipped_tool.item, tag))
                            .unwrap_or(true)
                    })
                })
                .unwrap_or(false);
            if !tool_ok {
                continue;
            }
        } else if state.orders.is_empty() {
            continue;
        }

        let dist = (cell.x - foot.x)
            .abs()
            .max((cell.y - foot.y).abs())
            .max((cell.z - foot.z).abs());
        if dist > MAX_CRAFT_STATION_RADIUS_CELLS {
            continue;
        }
        if let Some((_, best_dist)) = best
            && dist >= best_dist
        {
            continue;
        }

        // Read the block at the station cell to get its tag + tier.
        let (coord, local) = crate::voxel::world_to_chunk(*cell);
        let Some(&chunk_entity) = chunk_map.0.get(&coord) else {
            continue;
        };
        let Ok(chunk) = chunks.get(chunk_entity) else {
            continue;
        };
        let block_slot = chunk.get(local);
        if block_slot.is_empty() {
            continue;
        }
        let block_def = block_registry.def(block_slot);
        let Some(station_tag) = block_def.station_tag.as_ref() else {
            continue;
        };
        let max_tier = block_def.station_tier;

        // At least one order workable right now (recipe matches the
        // station's tag/tier, inputs satisfied, tool gate met).
        let any_workable = state.orders.iter().any(|order| {
            if order.is_done() {
                return false;
            }
            let recipe_id = RecipeId::new(order.recipe_id.clone());
            let Some(recipe_slot) = recipes.slot_of(&recipe_id) else {
                return false;
            };
            let recipe = recipes.def(recipe_slot);
            if recipe.station != *station_tag || recipe.tier > max_tier {
                return false;
            }
            if let Some(tag) = &recipe.required_tool
                && !item_registry.tool_has_tag(equipped_tool.item, tag)
            {
                return false;
            }
            recipe.inputs.iter().all(|input| {
                let Some(slot) = item_registry.slot_of(&input.item) else {
                    return false;
                };
                state.inventory.get(&slot).copied().unwrap_or(0) >= input.count
            })
        });
        if !resume && !any_workable {
            continue;
        }
        best = Some((*cell, dist));
    }

    let (cell, _) = best?;
    if !bookings.try_book_npc(npc_id, cell) {
        return None;
    }
    info!(
        npc = npc_id.0,
        cell = ?cell.to_array(),
        "craft assignment booked",
    );
    Some(cell)
}

/// Client → server: queue a new craft order at `station_cell`.
/// Server validates recipe is available at the station's tag + tier
/// before appending. Quantity is clamped at the server.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct QueueOrder {
    pub station_cell: IVec3,
    pub recipe_id: String,
    pub quantity: u32,
}

/// Client → server: cancel one durable order at its observed revision.
/// If that exact order is active, its committed inputs return to inventory.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct CancelOrder {
    pub station_cell: IVec3,
    pub order_id: u64,
    pub revision: u64,
}

/// Client → server: drain the player's carry into the station's
/// inventory. Whole stack, any kind — stations accept whatever the
/// player brings (only the recipe matching at work time gates output).
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DepositToStation {
    pub station_cell: IVec3,
}

/// Client → server: start (or resume) being the active worker at
/// `station_cell`. Server registers this connection as the worker;
/// while registered, the server's tick advances the station's
/// `active_work.elapsed_secs`. If there's no `active_work` yet but
/// a queued order has its inputs satisfied, the server creates a
/// new `active_work` (consuming the inputs) for the first such
/// order and registers the worker.
///
/// Phase 6b.2 "hold to progress" rule: clients send WorkStart on
/// L-click `just_pressed` against a station and WorkStop on
/// `just_released`. Without an active worker the timer doesn't
/// tick, so a half-finished craft pauses cleanly when the player
/// walks away and resumes when they hold again.
///
/// Replaces the single-shot `WorkStation` message that finished a
/// craft cycle in one click — that path violated the
/// no-instant-crafting rule.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorkStart {
    pub station_cell: IVec3,
}

/// Client → server: stop being the active worker at `station_cell`.
/// Server clears the worker registration; the timer pauses but
/// `active_work` (and its accumulated `elapsed_secs`) persists for
/// resume. Quiet no-op when this connection wasn't the current
/// worker.
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WorkStop {
    pub station_cell: IVec3,
}

pub struct CraftStationsServerPlugin;
pub struct CraftStationsClientPlugin;

impl Plugin for CraftStationsServerPlugin {
    fn build(&self, app: &mut App) {
        crate::spatial::init_session_resource::<CraftOrderIds>(app);
        app.add_systems(PreUpdate, activate_accepted_jobs);
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<StationDataset>::server());
        crate::spatial::init_session_resource::<CraftBookings>(app);
        app.add_observer(release_workers_on_disconnect);
        app.add_systems(
            Update,
            (
                receive_queue_orders,
                receive_cancel_orders,
                receive_deposit_to_station,
                receive_work_start,
                receive_work_stop,
            )
                .in_set(GameSet::Simulation),
        );
        // Work timer ticks in FixedUpdate so duration_secs reads as
        // wall-clock seconds independent of frame rate. Output spawn
        // + state mutation happen on the tick that crosses the threshold.
        app.add_systems(FixedUpdate, tick_station_work);
    }
}

impl Plugin for CraftStationsClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<StationDataset>::client());
        crate::spatial::init_session_resource::<CraftStationUiState>(app);
    }
}

/// Client-side: which station's modal is currently open (None ⇒ no
/// modal). Set by the L-click handler on a station block; cleared
/// by the modal's close button / Esc.
#[derive(Resource, Default)]
pub struct CraftStationUiState {
    pub open_cell: Option<IVec3>,
    /// Per-recipe pending quantity input. Keyed by recipe id so the
    /// modal can render distinct counters per recipe row without
    /// allocating a per-frame HashMap.
    pub pending_quantities: HashMap<String, u32>,
}

impl CraftStationUiState {
    /// True when the modal is open (any station). Click resolvers
    /// use this to suppress in-world interactions while UI is active.
    pub fn is_open(&self) -> bool {
        self.open_cell.is_some()
    }
}

/// Reflect [`CraftStationUiState::open_cell`] into the [`UiCaptures`]
/// SSOT. Lets the modal stay in charge of its own open state (set
/// from many call sites — Esc handler, F-key toggle, block-gone
/// auto-close, etc.) while the cursor lock lives in one place. The
/// previous direct cursor toggling created a bug class where modal
/// open/close paths got out of sync with cursor lock; this routes
/// everything through the capture set.
pub fn sync_craft_modal_capture(
    ui_state: Res<CraftStationUiState>,
    mut captures: ResMut<crate::ui_capture::UiCaptures>,
) {
    if !ui_state.is_changed() {
        return;
    }
    if ui_state.is_open() {
        captures.acquire(crate::ui_capture::UiCapture::CraftModal);
    } else {
        captures.release(crate::ui_capture::UiCapture::CraftModal);
    }
}

/// Hard upper bound on a single queue order. Past this the modal
/// scrolls forever and the server's order tick stalls; gameplay-wise
/// nobody plausibly wants 1000 of one item from one click. The modal
/// also clamps client-side; this is the server-of-record cap.
pub(crate) const MAX_ORDER_QUANTITY: u32 = 99;
pub const MAX_STATION_ORDERS: usize = 64;

#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins many subsystems"
)]
fn receive_queue_orders(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<QueueOrder>),
        With<crate::protocol::GameReady>,
    >,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&crate::protocol::AvatarPose, With<crate::protocol::Avatar>>,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    mut stations: ResMut<CraftStations>,
    mut order_ids: ResMut<CraftOrderIds>,
    mut validation: crate::server::ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            if validation
                .authorize(connection, crate::server::RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if !within_reach(pose, centre, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
            let Some(station_def) =
                lookup_station_def(req.station_cell, &chunks, &chunk_map, &block_registry)
            else {
                continue;
            };
            let mut state = stations.get(req.station_cell).cloned().unwrap_or_default();
            if order_ids
                .queue(
                    &mut state,
                    &recipes,
                    &station_def,
                    &req.recipe_id,
                    req.quantity,
                )
                .is_err()
            {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::QueueFull,
                );
                continue;
            }
            stations.upsert(req.station_cell, state, 0);
            stations.force_flush(&req.station_cell);
            info!(
                cell = ?req.station_cell.to_array(),
                recipe = %req.recipe_id,
                quantity = req.quantity,
                "craft order queued",
            );
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "cancel refunds need item registry + recipes"
)]
fn receive_cancel_orders(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<CancelOrder>),
        With<crate::protocol::GameReady>,
    >,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&crate::protocol::AvatarPose, With<crate::protocol::Avatar>>,
    mut stations: ResMut<CraftStations>,
    mut bookings: ResMut<CraftBookings>,
    mut validation: crate::server::ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            if validation
                .authorize(connection, crate::server::RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
            // Same reach gate as every other station verb — cancel was
            // the one handler accepting any cell from any connection
            // (it force-clears active work, so it's mutating too).
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if !within_reach(pose, centre, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
            let Some(mut state) = stations.get(req.station_cell).cloned() else {
                continue;
            };
            let Ok(refunded) = state.cancel_order(req.order_id, req.revision) else {
                continue;
            };
            if refunded {
                bookings.force_clear_station(req.station_cell);
            }
            stations.upsert(req.station_cell, state, 0);
            stations.force_flush(&req.station_cell);
            stations.remove_if_empty(req.station_cell);
            info!(
                cell = ?req.station_cell.to_array(),
                order_id = req.order_id,
                refunded,
                "craft order cancelled",
            );
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins many subsystems"
)]
fn receive_deposit_to_station(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<DepositToStation>),
        With<crate::protocol::GameReady>,
    >,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    avatars: Res<crate::server::ClientAvatars>,
    mut players: Query<
        (&crate::protocol::AvatarPose, &mut crate::protocol::Carrying),
        With<crate::protocol::Avatar>,
    >,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    mut stations: ResMut<CraftStations>,
    mut validation: crate::server::ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            if validation
                .authorize(connection, crate::server::RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok((pose, mut carry)) = players.get_mut(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if !within_reach(pose, centre, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
            // Block must still be a station.
            if lookup_station_def(req.station_cell, &chunks, &chunk_map, &block_registry).is_none()
            {
                continue;
            }
            let Some(item) = carry.item else {
                continue;
            };
            if stations
                .get(req.station_cell)
                .is_some_and(|state| !state.can_deposit_with_refund_reserve(item, carry.count))
            {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::InventoryFull,
                );
                continue;
            }
            let count = carry.count;
            if count == 0 {
                continue;
            }
            let accepted = stations.edit_or_insert(req.station_cell, |state| {
                state.transfer_from_carry(&mut carry, count)
            });
            if accepted == 0 {
                continue;
            }
            info!(
                cell = ?req.station_cell.to_array(),
                item = item.0,
                count,
                "station deposit",
            );
        }
    }
}

/// Handle `WorkStart`: register the connection as the active worker
/// at the station + start a new craft cycle if needed.
///
/// Two cases:
/// 1. Station already has `active_work` (paused or being worked by
///    someone who walked away). Re-register this connection as the
///    worker; don't touch elapsed/total/recipe. Lets a player who
///    pauses-resumes pick up where they left off, and lets a
///    different player (or future NPC) take over a paused craft
///    cleanly.
/// 2. Station has no `active_work` yet. Find the first queued order
///    whose inputs the inventory satisfies; consume inputs, set
///    active_work, register the worker. Silent skip if no such
///    order exists.
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler joins many subsystems"
)]
fn receive_work_start(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<WorkStart>),
        With<crate::protocol::GameReady>,
    >,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<
        (&crate::protocol::AvatarPose, &crate::protocol::EquippedTool),
        With<crate::protocol::Avatar>,
    >,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    item_registry: Res<crate::items::ItemRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    mut stations: ResMut<CraftStations>,
    mut bookings: ResMut<CraftBookings>,
    mut validation: crate::server::ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            let Some(auth) =
                validation.authorize(connection, crate::server::RequestClass::Ordinary)
            else {
                continue;
            };
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok((pose, tool)) = poses.get(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if !within_reach(pose, centre, INTERACT_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    req.station_cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
            let Some(station_def) =
                lookup_station_def(req.station_cell, &chunks, &chunk_map, &block_registry)
            else {
                continue;
            };
            let worker_tool = tool.item;
            // Case 1: resume only after revalidating the live station,
            // recipe and current tool. A different worker may take over
            // only after the previous registration has become invalid and
            // the tick predicate released it.
            if let Some(active) = stations
                .get(req.station_cell)
                .and_then(|state| state.active_work.as_ref())
            {
                if bookings
                    .worker_at(req.station_cell)
                    .is_some_and(|worker| worker != connection)
                {
                    continue;
                }
                let recipe_id =
                    block_junk_mod_api::recipes::RecipeId::new(active.recipe_id.clone());
                let Some(recipe) = recipes
                    .slot_of(&recipe_id)
                    .and_then(|slot| recipes.try_def(slot))
                else {
                    continue;
                };
                if recipe.station != station_def.tag
                    || recipe.tier > station_def.tier
                    || recipe
                        .required_tool
                        .as_ref()
                        .is_some_and(|tag| !item_registry.tool_has_tag(worker_tool, tag))
                {
                    continue;
                }
                let _ = bookings.resume_job(
                    req.station_cell,
                    WorkerAuthority::player(connection, auth.player_id),
                    stations
                        .get(req.station_cell)
                        .expect("observed active craft"),
                );
                continue;
            }
            // Case 2: no active_work. Find the first queued order
            // whose recipe is valid at this station + inputs are
            // available, then start it. Shared with the NPC arrival
            // path — single source of truth for "start the first
            // satisfiable order."
            if stations.get(req.station_cell).is_none() {
                stations.upsert(req.station_cell, StationState::default(), 0);
            }
            let started = try_start_first_satisfiable_order(
                req.station_cell,
                WorkerAuthority::player(connection, auth.player_id),
                worker_tool,
                &station_def,
                CraftStartContext {
                    item_registry: &item_registry,
                    recipes: &recipes,
                    stations: &mut stations,
                    bookings: &mut bookings,
                },
            )
            .is_some();
            if started {
            } else {
                // The get_or_insert above may have left an empty
                // state behind — clean up so the by-cell map stays
                // sparse.
                stations.remove_if_empty(req.station_cell);
            }
        }
    }
}

/// Handle `WorkStop`: clear the worker registration. `active_work`
/// itself persists so a resume picks up where this paused.
fn receive_work_stop(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<WorkStop>),
        With<crate::protocol::GameReady>,
    >,
    mut bookings: ResMut<CraftBookings>,
    mut validation: crate::server::ValidatedRequestContext,
) {
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            if validation
                .authorize(connection, crate::server::RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
            bookings.release_worker(req.station_cell, connection);
        }
    }
}

/// Observer: a player disconnect clears every work registration
/// they held, so a half-finished craft pauses cleanly. The
/// `active_work` itself (consumed inputs + elapsed_secs) persists;
/// another player or NPC can resume by sending `WorkStart`.
fn release_workers_on_disconnect(
    trigger: On<Remove, Connected>,
    mut bookings: ResMut<CraftBookings>,
) {
    bookings.release_all_workers_for(trigger.entity);
}

type JobActors<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static crate::npc::NpcId,
        &'static mut crate::npc::Brain,
        &'static crate::protocol::AvatarPose,
        &'static crate::protocol::EquippedTool,
        Has<crate::npc::BrainDisabled>,
    ),
>;

/// Accepted script work binds to runtime actors only after the batch's deferred
/// spawns have committed. This also supports spawn-and-start in one transaction.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct JobActivationWorld<'w, 's> {
    bookings: ResMut<'w, CraftBookings>,
    stations: Res<'w, CraftStations>,
    items: Res<'w, crate::items::ItemRegistry>,
    recipes: Res<'w, crate::recipes::RecipeRegistry>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    map: Res<'w, crate::voxel::ChunkMap>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
    actors: JobActors<'w, 's>,
}
pub(crate) fn activate_accepted_jobs(mut world: JobActivationWorld) {
    use block_junk_mod_api::authority::{JobState, JobTarget, ReservationOwner};
    let pending: Vec<_> = world
        .bookings
        .jobs
        .records
        .values()
        .filter(|job| job.state == JobState::Accepted)
        .take(32)
        .cloned()
        .collect();
    for job in pending {
        let cell = world
            .bookings
            .jobs_by_cell
            .iter()
            .find(|(_, id)| **id == job.id)
            .map(|(cell, _)| *cell);
        let result = (|| -> Result<(), String> {
            let cell = cell.ok_or("craft reservation lost")?;
            let ReservationOwner::Npc { id } = job.actor else {
                return Err("invalid queued craft actor".into());
            };
            let (entity, _, mut brain, pose, tool, disabled) = world
                .actors
                .iter_mut()
                .find(|(_, npc, _, _, _, _)| npc.0 == id)
                .ok_or("craft actor removed")?;
            if disabled
                || !(matches!(brain.goal, crate::npc::Goal::Idle)
                    || matches!(brain.goal,crate::npc::Goal::CraftingAtStation {station_cell} if station_cell==cell))
            {
                return Err("craft actor interrupted before activation".into());
            }
            if !within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), INTERACT_REACH) {
                return Err("craft actor moved out of reach".into());
            }
            let station = lookup_station_def(cell, &world.chunks, &world.map, &world.blocks)
                .ok_or("craft facility removed")?;
            let state = world
                .stations
                .get(cell)
                .ok_or("craft station state removed")?;
            let JobTarget::CraftOrder {
                order_id,
                order_revision,
            } = job.target
            else {
                return Err("invalid crafting job target".into());
            };
            if state
                .active_work
                .as_ref()
                .is_none_or(|work| work.order_id != order_id)
            {
                return Err("craft commitment changed".into());
            }
            let mut validation = state.clone();
            validation.prepare_order_work(
                order_id,
                order_revision,
                &station,
                tool.item,
                &world.items,
                &world.recipes,
            )?;
            if !world
                .bookings
                .register_worker(cell, WorkerAuthority::npc(entity, crate::npc::NpcId(id)))
            {
                return Err("craft reservation changed".into());
            }
            brain.goal = crate::npc::Goal::CraftingAtStation { station_cell: cell };
            let committed = world
                .bookings
                .jobs
                .records
                .get_mut(&job.id)
                .expect("accepted job remains borrowed");
            committed.state = JobState::Working;
            committed.revision += 1;
            Ok(())
        })();
        if let Err(reason) = result {
            world
                .bookings
                .jobs
                .finish(job.id, JobState::Failed, Some(reason));
            if let Some(cell) = cell {
                world.bookings.force_clear_station(cell);
            }
        }
    }
}

/// Tick every station's active work toward completion. On completion:
/// spawn the recipe's output as a `WorldItem` on top of the station,
/// bump the matching order's `completed`, remove the order if it just
/// finished, and clear `active_work`. Inputs were already
/// consumed at start, so completion just produces the output.
///
/// Progress mutations are coalesced by [`StationDataset`]'s replication
/// policy; completion is explicitly force-flushed.
#[allow(
    clippy::too_many_arguments,
    reason = "completion path joins many subsystems"
)]
fn tick_station_work(
    time: Res<Time>,
    item_registry: Res<crate::items::ItemRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    mut stations: ResMut<CraftStations>,
    mut bookings: ResMut<CraftBookings>,
    mut commands: Commands,
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
    chunks: Query<&'static crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    npc_q: Query<(), With<crate::npc::Npc>>,
    tools: Query<&crate::protocol::EquippedTool>,
    actor_state: Query<(&crate::protocol::AvatarPose, &crate::protocol::EquippedTool)>,
    client_avatars: Res<crate::server::ClientAvatars>,
    mut brains: Query<&mut crate::npc::Brain>,
) {
    use block_junk_mod_api::recipes::RecipeId;
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    // Collect cells with active_work AND an active worker — only
    // those tick. Stations with active_work but no worker are paused
    // (player walked away mid-craft); the elapsed_secs sits intact
    // until someone resumes via WorkStart.
    let active_cells: Vec<IVec3> = stations
        .iter()
        .filter(|(cell, state)| state.active_work.is_some() && bookings.has_worker(**cell))
        .map(|(cell, _)| *cell)
        .collect();
    for cell in active_cells {
        let Some(worker) = bookings.worker_at(cell) else {
            continue;
        };
        let actor = client_avatars.0.get(&worker).copied().unwrap_or(worker);
        let Some(active_recipe) = stations
            .get(cell)
            .and_then(|state| state.active_work.as_ref())
            .map(|active| active.recipe_id.clone())
        else {
            bookings.force_clear_worker(cell);
            continue;
        };
        let recipe_id = RecipeId::new(active_recipe);
        let valid = recipes
            .slot_of(&recipe_id)
            .and_then(|slot| recipes.try_def(slot))
            .zip(lookup_station_def(
                cell,
                &chunks,
                &chunk_map,
                &block_registry,
            ))
            .zip(actor_state.get(actor).ok())
            .is_some_and(|((recipe, station), (pose, tool))| {
                recipe.station == station.tag
                    && recipe.tier <= station.tier
                    && within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), INTERACT_REACH)
                    && recipe
                        .required_tool
                        .as_ref()
                        .is_none_or(|tag| item_registry.tool_has_tag(tool.item, tag))
            });
        if !valid {
            // Pause without producing output or consuming anything else.
            // The already-locked inputs remain in `active_work` until a
            // valid worker resumes or cancellation refunds them.
            bookings.force_clear_worker(cell);
            continue;
        }
        let Some(mut state) = stations.get(cell).cloned() else {
            continue;
        };
        let Some(active) = state.active_work.as_mut() else {
            continue;
        };
        active.elapsed_secs = (active.elapsed_secs + dt).min(active.total_secs);
        if active.elapsed_secs < active.total_secs {
            stations.upsert(cell, state, 0);
            continue;
        }
        // Completion path. Clone the recipe id out of the active
        // entry before taking it.
        let order_id = active.order_id;
        let recipe_id_str = active.recipe_id.clone();
        let recipe_id = RecipeId::new(recipe_id_str.clone());
        let Some(recipe_slot) = recipes.slot_of(&recipe_id) else {
            warn!(
                cell = ?cell.to_array(),
                recipe = %recipe_id_str,
                "work complete: recipe id missing from registry; skipping output",
            );
            stations.upsert(cell, state, 0);
            stations.force_flush(&cell);
            continue;
        };
        let recipe = recipes.def(recipe_slot);
        let Some(output_slot) = item_registry.slot_of(&recipe.output.item) else {
            warn!(
                recipe = %recipe.id,
                output = %recipe.output.item,
                "work complete: output item missing from registry; skipping",
            );
            stations.upsert(cell, state, 0);
            stations.force_flush(&cell);
            continue;
        };
        // Spawn output(s) in the first EMPTY cell around the station —
        // straight above first, then that cell's horizontal
        // neighbours, then beside the station itself. Spawning blind
        // into a solid cell would bury the output; an empty pick can
        // still be an unsupported ledge cell, so the final position is
        // settled like every other item spawn (a floating output never
        // gets a CellEdit to rescue it and is unreachable to haulers).
        let above = cell + IVec3::Y;
        let candidates = [
            above,
            above + IVec3::X,
            above - IVec3::X,
            above + IVec3::Z,
            above - IVec3::Z,
            cell + IVec3::X,
            cell - IVec3::X,
            cell + IVec3::Z,
            cell - IVec3::Z,
        ];
        let cell_is_empty = |c: IVec3| {
            let (coord, local) = crate::voxel::world_to_chunk(c);
            chunk_map
                .0
                .get(&coord)
                .and_then(|&e| chunks.get(e).ok())
                .map(|ch| ch.get(local).is_empty())
                .unwrap_or(false)
        };
        let Some(spawn_cell) = candidates.into_iter().find(|&c| cell_is_empty(c)) else {
            stations.upsert(cell, state, 0);
            continue;
        };
        let policy_owned = brains.get(actor).is_ok_and(|brain| {
            brain
                .active_policy_job
                .as_ref()
                .is_some_and(|(id, _)| id == &format!("craft:{},{},{}", cell.x, cell.y, cell.z))
        });
        state.active_work = None;
        bookings.complete_job(cell);
        let spawn_base = spawn_cell.as_vec3() + Vec3::new(0.5, 0.05, 0.5);
        let walk = crate::npc::WorldWalk {
            chunks: &chunks,
            sidecars: &sidecars,
            chunk_map: &chunk_map,
            registry: &block_registry,
        };
        let translation = crate::server::settled_translation(&walk, spawn_base);
        crate::item_placement::spawn_stack(
            &mut commands,
            output_slot,
            recipe.output.count,
            translation,
        );
        // Bump the matching order's completed counter; remove the
        // order if it just finished.
        let order_done = if let Some(idx) = state
            .orders
            .iter()
            .position(|o| o.id == order_id && !o.is_done())
        {
            state.orders[idx].completed += 1;
            state.orders[idx].revision += 1;
            let done = state.orders[idx].is_done();
            if done {
                state.orders.remove(idx);
            }
            done
        } else {
            // Order was cancelled mid-work but active_work somehow
            // outlived it — shouldn't happen because cancel-with-
            // matching-active refunds + clears, but defensive.
            warn!(
                cell = ?cell.to_array(),
                recipe = %recipe_id_str,
                "work complete: matching order gone; output produced anyway",
            );
            false
        };
        info!(
            cell = ?cell.to_array(),
            recipe = %recipe.id,
            output = %recipe.output.item,
            count = recipe.output.count,
            order_done,
            "station work complete",
        );
        // NPC workers auto-continue with the next satisfiable order
        // so they don't have to round-trip through Idle to keep
        // crafting (Phase 6c-A "park until done"). Players clear on
        // completion — their hold-L-click UX requires a re-press
        // before the next craft starts. Determining "is this an
        // NPC?" is a single ECS lookup on the worker entity.
        let worker_entity = bookings.worker_at(cell);
        let worker_is_npc = worker_entity.map(|e| npc_q.get(e).is_ok()).unwrap_or(false);
        let mut auto_continued = false;
        stations.upsert(cell, state, 0);
        stations.force_flush(&cell);
        if policy_owned && let Ok(mut brain) = brains.get_mut(actor) {
            crate::npc::finish_policy_job(
                &mut brain,
                block_junk_mod_api::npcs::PolicyJobState::Completed,
                None,
            );
        }
        if worker_is_npc
            && !policy_owned
            && let Some(worker) = bookings.worker_authority(cell)
        {
            // Look up the station def fresh in case the block was
            // replaced mid-craft (degenerate but defensive).
            if let Some(station_def) =
                lookup_station_def(cell, &chunks, &chunk_map, &block_registry)
                && try_start_first_satisfiable_order(
                    cell,
                    worker.clone(),
                    tools.get(worker.entity).ok().and_then(|t| t.item),
                    &station_def,
                    CraftStartContext {
                        item_registry: &item_registry,
                        recipes: &recipes,
                        stations: &mut stations,
                        bookings: &mut bookings,
                    },
                )
                .is_some()
            {
                auto_continued = true;
            }
        }
        if !auto_continued {
            // Either a player worker (release + re-press to continue)
            // or no next satisfiable order — clear the registration
            // so the station goes paused (paused-with-active_work
            // never happens here since we already set it to None
            // above; this just frees the slot for the next worker).
            bookings.force_clear_worker(cell);
        }
        stations.remove_if_empty(cell);
    }
}

/// Helper: resolve the station def at `cell`, returning the (tag,
/// tier) pair. `None` ⇒ cell is empty, unloaded, or holds a
/// non-station block. Skips having to thread an Option<&BlockDef>
/// through every handler.
pub(crate) struct StationDefView {
    pub(crate) tag: block_junk_mod_api::blocks::TagId,
    pub(crate) tier: u8,
}

pub(crate) fn lookup_station_def(
    cell: IVec3,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
    block_registry: &crate::blocks::BlockRegistry,
) -> Option<StationDefView> {
    let (coord, local_idx) = crate::voxel::world_to_chunk(cell);
    let entity = chunk_map.0.get(&coord)?;
    let chunk = chunks.get(*entity).ok()?;
    let slot = chunk.get(local_idx);
    if slot.is_empty() {
        return None;
    }
    let def = block_registry.def(slot);
    let tag = def.station_tag.clone()?;
    Some(StationDefView {
        tag,
        tier: def.station_tier,
    })
}

/// Shared "start the first satisfiable order at this station for
/// this worker" path. Used by the player [`receive_work_start`]
/// (Case 2), the NPC arrival handler in `npc_brain_tick`
/// (`ArrivalAction::WorkStation`), and the auto-continue branch of
/// [`tick_station_work`] (start the next order when an NPC's current
/// craft completes).
///
/// Returns `Some(recipe_id)` of the order that was started, or `None`
/// when no order matches (queue empty, no inputs available, tier/tag
/// mismatch). On success: inputs are consumed up front (locked in;
/// Cancel refunds, completion produces output), `active_work` is set,
/// `worker_entity` is registered in [`ActiveWorkers`] for the cell
/// (idempotent — re-registering the same worker is a no-op). The store
/// journal is force-flushed because work start is a logical state change.
///
/// Caller is responsible for `stations.remove_if_empty(cell)` on the
/// `None` path — this helper doesn't probe emptiness because the
/// player path's `get_or_insert` already created the entry and the
/// NPC path always has an existing entry to act on.
pub(crate) struct CraftStartContext<'a> {
    pub item_registry: &'a crate::items::ItemRegistry,
    pub recipes: &'a crate::recipes::RecipeRegistry,
    pub stations: &'a mut CraftStations,
    pub bookings: &'a mut CraftBookings,
}

pub(crate) fn try_start_first_satisfiable_order(
    station_cell: IVec3,
    worker: WorkerAuthority,
    worker_tool: Option<crate::items::ItemSlot>,
    station_def: &StationDefView,
    context: CraftStartContext<'_>,
) -> Option<String> {
    let CraftStartContext {
        item_registry,
        recipes,
        stations,
        bookings,
    } = context;
    let mut state = stations.get(station_cell).cloned()?;
    if state.active_work.is_some()
        || bookings
            .worker_at(station_cell)
            .is_some_and(|entity| entity != worker.entity)
    {
        return None;
    }
    let mut started_recipe_id = None;
    for order in state.orders.clone() {
        if state
            .prepare_order_work(
                order.id,
                order.revision,
                station_def,
                worker_tool,
                item_registry,
                recipes,
            )
            .is_ok()
        {
            started_recipe_id = Some(order.recipe_id);
            break;
        }
    }
    if let Some(recipe_id_str) = &started_recipe_id {
        let active = state.active_work.as_ref().expect("started work");
        let order = state
            .orders
            .iter()
            .find(|order| order.id == active.order_id)
            .expect("selected order");
        if bookings
            .begin_job(
                station_cell,
                worker.clone(),
                worker.owner,
                order.id,
                order.revision,
            )
            .is_err()
        {
            return None;
        }
        info!(
            cell = ?station_cell.to_array(),
            recipe = %recipe_id_str,
            "station work started",
        );
    }
    stations.upsert(station_cell, state, 0);
    stations.force_flush(&station_cell);
    started_recipe_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::npc::NpcId;

    #[test]
    fn canceling_duplicate_recipes_targets_the_exact_order_and_revision() {
        let mut state = StationState::default();
        for id in [1, 2] {
            assert!(state.try_queue(CraftOrder {
                id,
                revision: 1,
                recipe_id: "test:recipe".into(),
                total: 1,
                completed: 0,
            }));
        }
        let item = ItemSlot(1);
        assert!(state.deposit(item, 3));
        assert!(state.begin_work(1, "test:recipe".into(), 2., vec![(item, 2)]));
        let before = state.clone();
        assert!(state.cancel_order(1, 2).is_err());
        assert_eq!(state, before);
        assert_eq!(state.cancel_order(2, 1), Ok(false));
        assert_eq!(state.active_work.as_ref().unwrap().order_id, 1);
        assert_eq!(state.inventory[&item], 1);
        assert!(state.cancel_order(2, 1).is_err());
        assert_eq!(state.cancel_order(1, 1), Ok(true));
        assert_eq!(state.inventory[&item], 3);
        assert!(state.active_work.is_none());
        assert!(state.orders.is_empty());
        assert!(state.cancel_order(1, 1).is_err());
        assert_eq!(state.inventory[&item], 3);
    }

    #[test]
    fn facility_transfers_cannot_withdraw_committed_inputs_or_overfill_refunds() {
        let item = ItemSlot(1);
        let mut state = StationState::default();
        assert!(state.deposit(item, 5));
        assert!(state.begin_work(1, "test:recipe".into(), 2.0, vec![(item, 3)]));
        let mut carry = crate::protocol::Carrying::default();
        assert_eq!(state.transfer_to_carry(&mut carry, item, 5, 20), 2);
        assert_eq!(carry.count, 2);
        assert_eq!(state.transfer_to_carry(&mut carry, item, 5, 20), 0);
        assert_eq!(
            state.active_work.as_ref().unwrap().committed_inputs,
            vec![(item, 3)]
        );
        assert!(state.deposit(item, u32::MAX - 3));
        let before = state.clone();
        assert_eq!(state.transfer_from_carry(&mut carry, 2), 0);
        assert_eq!(state, before);
        assert_eq!(carry.count, 2);
        assert!(state.cancel_work());
        assert_eq!(state.inventory[&item], u32::MAX);
    }

    #[test]
    fn craft_commitment_is_atomic_and_refunds_actual_inputs_once() {
        let mut state = StationState::default();
        let wood = ItemSlot(1);
        let iron = ItemSlot(2);
        assert!(state.deposit(wood, 5));
        let original = state.clone();
        assert!(!state.begin_work(1, "test:recipe".into(), 2.0, vec![(wood, 3), (iron, 1)]));
        assert_eq!(state, original);
        assert!(!state.begin_work(1, "test:recipe".into(), 2.0, vec![(wood, 3), (wood, 3)]));
        assert_eq!(state, original);
        assert!(state.begin_work(1, "test:recipe".into(), 2.0, vec![(wood, 3)]));
        assert_eq!(state.inventory[&wood], 2);
        let running = state.clone();
        assert!(!state.begin_work(1, "test:other".into(), 2.0, vec![(wood, 1)]));
        assert_eq!(state, running);
        assert!(state.cancel_work());
        assert_eq!(state, original);
        assert!(!state.cancel_work());
        assert_eq!(state, original);
    }

    #[test]
    fn deposits_reserve_refund_kind_slots_and_integer_headroom() {
        let mut state = StationState::default();
        let wood = ItemSlot(1);
        assert!(state.deposit(wood, u32::MAX));
        assert!(state.begin_work(1, "test:recipe".into(), 2.0, vec![(wood, 3)]));
        let running = state.clone();
        assert!(!state.deposit(wood, 1));
        assert_eq!(state, running);
        assert!(state.cancel_work());
        assert_eq!(state.inventory[&wood], u32::MAX);
        state.inventory.clear();
        assert!(state.deposit(wood, 1));
        assert!(state.begin_work(1, "test:recipe".into(), 2.0, vec![(wood, 1)]));
        for slot in 2..=64 {
            assert!(state.deposit(ItemSlot(slot), 1));
        }
        let running = state.clone();
        assert!(!state.deposit(ItemSlot(65), 1));
        assert_eq!(state, running);
        assert!(state.cancel_work());
        assert_eq!(state.inventory.len(), 64);
    }

    fn entity(id: u32) -> Entity {
        Entity::from_raw_u32(id).expect("nonzero entity id")
    }

    #[test]
    fn try_book_npc_atomic_both_or_neither() {
        let mut b = CraftBookings::default();
        let npc = NpcId(1);
        let cell = IVec3::new(5, 6, 7);

        // First book: succeeds, both directions populated.
        assert!(b.try_book_npc(npc, cell));
        assert_eq!(b.station_of_npc(npc), Some(cell));
        assert!(b.contains_npc(npc));

        // Re-book same pairing: idempotent.
        assert!(b.try_book_npc(npc, cell));

        // Conflict — same NPC, different station: refuses.
        let other_cell = IVec3::new(10, 6, 7);
        assert!(!b.try_book_npc(npc, other_cell));
        assert_eq!(b.station_of_npc(npc), Some(cell));

        // Conflict — different NPC, same station: refuses.
        let other_npc = NpcId(2);
        assert!(!b.try_book_npc(other_npc, cell));
        assert!(!b.contains_npc(other_npc));
    }

    #[test]
    fn station_taken_by_other_distinguishes_self() {
        let mut b = CraftBookings::default();
        let me = NpcId(1);
        let other = NpcId(2);
        let cell = IVec3::new(3, 4, 5);
        assert!(b.try_book_npc(me, cell));
        assert!(!b.station_taken_by_other(cell, me));
        assert!(b.station_taken_by_other(cell, other));
        let empty = IVec3::new(99, 99, 99);
        assert!(!b.station_taken_by_other(empty, me));
    }

    #[test]
    fn release_npc_booking_clears_both_directions_and_worker() {
        let mut b = CraftBookings::default();
        let npc = NpcId(7);
        let npc_entity = entity(70);
        let cell = IVec3::new(0, 0, 0);
        assert!(b.try_book_npc(npc, cell));
        assert!(b.register_worker(cell, WorkerAuthority::npc(npc_entity, npc)));
        assert!(b.has_worker(cell));

        b.release_npc_booking(npc, npc_entity);

        assert!(!b.contains_npc(npc));
        assert!(!b.has_worker(cell));
        // Reverse map clean — another NPC can now claim.
        let other = NpcId(8);
        assert!(b.try_book_npc(other, cell));
    }

    #[test]
    fn release_npc_booking_keeps_unrelated_worker() {
        let mut b = CraftBookings::default();
        let npc = NpcId(1);
        let npc_entity = entity(10);
        let other_entity = entity(20); // e.g., a player who took over the slot
        let cell = IVec3::new(1, 2, 3);
        assert!(b.try_book_npc(npc, cell));
        // Someone else grabbed the worker slot in the meantime.
        assert!(!b.register_worker(cell, WorkerAuthority::player(other_entity, 9)));
        b.release_npc_booking(npc, npc_entity);
        assert!(b.register_worker(cell, WorkerAuthority::player(other_entity, 9)));

        b.release_npc_booking(npc, npc_entity);

        // NPC's booking is cleared, but the unrelated worker survives.
        assert!(!b.contains_npc(npc));
        assert_eq!(b.worker_at(cell), Some(other_entity));
    }

    #[test]
    fn worker_release_guarded_by_holder() {
        let mut b = CraftBookings::default();
        let a = entity(1);
        let other = entity(2);
        let cell = IVec3::new(0, 0, 0);
        assert!(b.register_worker(cell, WorkerAuthority::player(a, 1)));
        b.release_worker(cell, other); // wrong holder
        assert_eq!(b.worker_at(cell), Some(a));
        b.release_worker(cell, a); // correct
        assert!(!b.has_worker(cell));
    }

    #[test]
    fn station_owners_exclude_players_npcs_and_double_work() {
        let mut bookings = CraftBookings::default();
        let npc = NpcId(1);
        let a = IVec3::ZERO;
        let b = IVec3::X;
        let player = WorkerAuthority::player(entity(10), 1);
        assert!(bookings.try_book_npc(npc, a));
        assert!(!bookings.register_worker(a, player.clone()));
        assert!(bookings.register_worker(a, WorkerAuthority::npc(entity(11), npc)));
        bookings.release_npc_booking(npc, entity(11));
        assert!(bookings.register_worker(a, player.clone()));
        assert!(!bookings.try_book_npc(npc, a));
        assert!(!bookings.register_worker(b, player.clone()));
        assert!(!bookings.has_worker(b));
        bookings.release_all_workers_for(player.entity);
        assert!(bookings.register_worker(b, player));
    }

    #[test]
    fn release_all_workers_for_only_clears_that_entity() {
        let mut b = CraftBookings::default();
        let a = entity(1);
        let c = entity(2);
        let cell_a = IVec3::new(0, 0, 0);
        let cell_c = IVec3::new(1, 0, 0);
        assert!(b.register_worker(cell_a, WorkerAuthority::player(a, 1)));
        assert!(b.register_worker(cell_c, WorkerAuthority::player(c, 2)));
        b.release_all_workers_for(a);
        assert!(!b.has_worker(cell_a));
        assert_eq!(b.worker_at(cell_c), Some(c));
    }

    #[test]
    fn station_queue_bound_rejects_without_mutation() {
        let mut state = StationState::default();
        for index in 0..MAX_STATION_ORDERS {
            assert!(state.try_queue(CraftOrder {
                id: state.orders.len() as u64 + 1,
                revision: 1,
                recipe_id: format!("recipe:{index}"),
                total: 1,
                completed: 0,
            }));
        }
        let before = state.clone();
        assert!(!state.try_queue(CraftOrder {
            id: state.orders.len() as u64 + 1,
            revision: 1,
            recipe_id: "recipe:overflow".to_owned(),
            total: 1,
            completed: 0,
        }));
        assert_eq!(state, before);
    }

    #[test]
    fn station_inventory_kind_bound_rejects_without_mutation() {
        let mut state = StationState::default();
        for index in 0..MAX_STATION_INVENTORY_KINDS {
            assert!(state.deposit(ItemSlot(index as u16), 1));
        }
        let before = state.clone();
        assert!(!state.deposit(ItemSlot(MAX_STATION_INVENTORY_KINDS as u16), 1));
        assert_eq!(state, before);
        assert!(state.deposit(ItemSlot(0), 1));
        assert_eq!(state.inventory[&ItemSlot(0)], 2);
    }
}
