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

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

use crate::items::ItemSlot;
use crate::menu::AppState;
use crate::protocol::{GameSet, WorldChannel};

/// One queued craft at a station. `total` is what the player asked
/// for; `completed` rises by 1 per Work cycle. When `completed ==
/// total` the order is finished and removed from the queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CraftOrder {
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

/// Full server-side state of one station cell. Replicated to clients
/// via [`StationUpdate`] / [`StationsFullSync`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StationState {
    pub orders: Vec<CraftOrder>,
    /// Items deposited at the station, by slot. Shared across all
    /// orders. Counts of 0 are scrubbed so iteration only ever sees
    /// real entries.
    pub inventory: HashMap<ItemSlot, u32>,
}

impl Default for StationState {
    fn default() -> Self {
        Self {
            orders: Vec::new(),
            inventory: HashMap::new(),
        }
    }
}

impl StationState {
    /// True when nothing references this station — caller drops it
    /// from the by-cell map.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty() && self.inventory.is_empty()
    }

    /// Add `count` of `item` to the inventory. 0 is a no-op.
    pub fn deposit(&mut self, item: ItemSlot, count: u32) {
        if count == 0 {
            return;
        }
        *self.inventory.entry(item).or_insert(0) += count;
    }

    /// Try to remove `count` of `item`. Returns true on success;
    /// false (no mutation) when the inventory is short.
    pub fn try_consume(&mut self, item: ItemSlot, count: u32) -> bool {
        let entry = match self.inventory.get_mut(&item) {
            Some(e) => e,
            None => return false,
        };
        if *entry < count {
            return false;
        }
        *entry -= count;
        if *entry == 0 {
            self.inventory.remove(&item);
        }
        true
    }
}

/// Server-authoritative + client-mirrored craft-order map. Same
/// shape on both sides; the server mutates + broadcasts, the client
/// applies broadcasts.
#[derive(Resource, Default, Debug)]
pub struct CraftStations {
    by_cell: HashMap<IVec3, StationState>,
}

impl CraftStations {
    pub fn get(&self, cell: IVec3) -> Option<&StationState> {
        self.by_cell.get(&cell)
    }

    pub fn get_mut(&mut self, cell: IVec3) -> Option<&mut StationState> {
        self.by_cell.get_mut(&cell)
    }

    /// Get the state at `cell`, creating an empty one if needed.
    /// Caller is responsible for `remove_if_empty` after a mutation
    /// that could leave it empty.
    pub fn get_or_insert(&mut self, cell: IVec3) -> &mut StationState {
        self.by_cell.entry(cell).or_default()
    }

    /// Drop the entry at `cell` if its state is empty. Keeps the map
    /// sparse — `iter` only yields real stations.
    pub fn remove_if_empty(&mut self, cell: IVec3) {
        if let Some(state) = self.by_cell.get(&cell)
            && state.is_empty()
        {
            self.by_cell.remove(&cell);
        }
    }

    pub fn remove(&mut self, cell: IVec3) -> Option<StationState> {
        self.by_cell.remove(&cell)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &StationState)> {
        self.by_cell.iter()
    }

    pub fn replace_all(&mut self, entries: impl IntoIterator<Item = (IVec3, StationState)>) {
        self.by_cell.clear();
        for (cell, state) in entries {
            if !state.is_empty() {
                self.by_cell.insert(cell, state);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<(IVec3, StationState)> {
        self.by_cell.iter().map(|(c, s)| (*c, s.clone())).collect()
    }
}

/// Server → client: per-cell broadcast. `state: None` is the "remove
/// this cell" signal (when the last order completes + inventory
/// empties).
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct StationUpdate {
    pub cell: IVec3,
    pub state: Option<StationState>,
}

/// Server → client: one-shot dump of every non-empty station, sent
/// when a client connects. Mirrors `PlanFullSync` exactly.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct StationsFullSync {
    pub entries: Vec<(IVec3, StationState)>,
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

/// Client → server: drop the first matching queued order at
/// `station_cell`. Inventory is not refunded — same convention as
/// Plans (canceling a tag doesn't return materials).
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct CancelOrder {
    pub station_cell: IVec3,
    pub recipe_id: String,
}

/// Client → server: drain the player's carry into the station's
/// inventory. Whole stack, any kind — stations accept whatever the
/// player brings (only the recipe matching at work time gates output).
#[derive(Message, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DepositToStation {
    pub station_cell: IVec3,
}

/// Client → server: perform one craft cycle for `recipe_id` at the
/// station. Server validates the order exists + station inventory
/// satisfies inputs + reach. Consumes inputs from inventory, spawns
/// the output as a `WorldItem` on top of the station, and increments
/// the order's `completed`.
///
/// Replaces Phase 6a's `CraftRequest`, which crafted directly from
/// carry without an order. Phase 6b's "no auto-craft" rule means
/// crafting only happens via an explicit `WorkStation` invocation.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct WorkStation {
    pub station_cell: IVec3,
    pub recipe_id: String,
}

pub struct CraftStationsServerPlugin;
pub struct CraftStationsClientPlugin;

impl Plugin for CraftStationsServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CraftStations>();
        app.add_observer(send_stations_full_sync_on_connect);
        app.add_systems(
            Update,
            (
                receive_queue_orders,
                receive_cancel_orders,
                receive_deposit_to_station,
                receive_work_station,
            )
                .in_set(GameSet::Simulation),
        );
    }
}

impl Plugin for CraftStationsClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CraftStations>();
        app.init_resource::<CraftStationUiState>();
        app.add_systems(
            Update,
            (receive_stations_full_sync, receive_station_update_broadcasts)
                .chain()
                .in_set(GameSet::Simulation)
                .run_if(in_state(AppState::InGame)),
        );
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

/// Client observer that requests a full sync from the server on
/// connect. Wired alongside the existing PlanFullSync flow so
/// reconnects come up with the full craft-order state.
fn send_stations_full_sync_on_connect(
    trigger: On<Add, Connected>,
    stations: Res<CraftStations>,
    mut senders: Query<&mut MessageSender<StationsFullSync>>,
) {
    let connection = trigger.entity;
    let Ok(mut sender) = senders.get_mut(connection) else {
        return;
    };
    let sync = StationsFullSync {
        entries: stations.snapshot(),
    };
    sender.send::<WorldChannel>(sync);
}

fn receive_stations_full_sync(
    mut receivers: Query<&mut MessageReceiver<StationsFullSync>>,
    mut stations: ResMut<CraftStations>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            stations.replace_all(sync.entries);
        }
    }
}

fn receive_station_update_broadcasts(
    mut receivers: Query<&mut MessageReceiver<StationUpdate>>,
    mut stations: ResMut<CraftStations>,
) {
    for mut receiver in receivers.iter_mut() {
        for update in receiver.receive() {
            match update.state {
                Some(state) if !state.is_empty() => {
                    stations.by_cell.insert(update.cell, state);
                }
                _ => {
                    stations.by_cell.remove(&update.cell);
                }
            }
        }
    }
}

/// Suppress the camera mouse-look + relock when the craft-order
/// modal is open. Mirrors the F3 panel's cursor handling — opens
/// the cursor so the modal is interactive; closes when the modal
/// dismisses.
pub fn craft_modal_cursor_lock(
    ui_state: Res<CraftStationUiState>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    if !ui_state.is_changed() {
        return;
    }
    let Ok((mut window, mut cursor)) = windows.single_mut() else {
        return;
    };
    if ui_state.is_open() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else {
        let centre = Vec2::new(window.resolution.width(), window.resolution.height()) * 0.5;
        window.set_cursor_position(Some(centre));
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Hard upper bound on a single queue order. Past this the modal
/// scrolls forever and the server's order tick stalls; gameplay-wise
/// nobody plausibly wants 1000 of one item from one click. The modal
/// also clamps client-side; this is the server-of-record cap.
const MAX_ORDER_QUANTITY: u32 = 99;

/// Anti-cheat reach gate. Same magnitude as the pickup / craft
/// constants in `server.rs`.
const STATION_REACH: f32 = 12.0;

#[allow(clippy::too_many_arguments, reason = "wire handler joins many subsystems")]
fn receive_queue_orders(
    mut receivers: Query<(Entity, &mut MessageReceiver<QueueOrder>)>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&crate::protocol::AvatarPose, With<crate::protocol::Avatar>>,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    mut stations: ResMut<CraftStations>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    use block_junk_mod_api::recipes::RecipeId;
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if (pose.translation - centre).length() > STATION_REACH {
                continue;
            }
            let Some(station_def) = lookup_station_def(
                req.station_cell,
                &chunks,
                &chunk_map,
                &block_registry,
            ) else {
                continue;
            };
            // Recipe must exist + match the station's tag + tier.
            let recipe_id = RecipeId::new(req.recipe_id.clone());
            let Some(recipe_slot) = recipes.slot_of(&recipe_id) else {
                continue;
            };
            let recipe = recipes.def(recipe_slot);
            if recipe.station != station_def.tag || recipe.tier > station_def.tier {
                continue;
            }
            let quantity = req.quantity.min(MAX_ORDER_QUANTITY);
            if quantity == 0 {
                continue;
            }
            let state = stations.get_or_insert(req.station_cell);
            state.orders.push(CraftOrder {
                recipe_id: req.recipe_id.clone(),
                total: quantity,
                completed: 0,
            });
            broadcast_station(&mut broadcast, server, req.station_cell, Some(state.clone()));
            info!(
                cell = ?req.station_cell.to_array(),
                recipe = %req.recipe_id,
                quantity,
                "craft order queued",
            );
        }
    }
}

fn receive_cancel_orders(
    mut receivers: Query<(Entity, &mut MessageReceiver<CancelOrder>)>,
    mut stations: ResMut<CraftStations>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (_connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            let Some(state) = stations.get_mut(req.station_cell) else {
                continue;
            };
            let before = state.orders.len();
            // Drop the first matching order, leave the rest.
            if let Some(idx) = state
                .orders
                .iter()
                .position(|o| o.recipe_id == req.recipe_id)
            {
                state.orders.remove(idx);
            }
            if state.orders.len() == before {
                continue;
            }
            let snapshot = state.clone();
            let now_empty = snapshot.is_empty();
            stations.remove_if_empty(req.station_cell);
            broadcast_station(
                &mut broadcast,
                server,
                req.station_cell,
                if now_empty { None } else { Some(snapshot) },
            );
            info!(
                cell = ?req.station_cell.to_array(),
                recipe = %req.recipe_id,
                "craft order cancelled",
            );
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "wire handler joins many subsystems")]
fn receive_deposit_to_station(
    mut receivers: Query<(Entity, &mut MessageReceiver<DepositToStation>)>,
    avatars: Res<crate::server::ClientAvatars>,
    mut players: Query<
        (&crate::protocol::AvatarPose, &mut crate::protocol::Carrying),
        With<crate::protocol::Avatar>,
    >,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    mut stations: ResMut<CraftStations>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok((pose, mut carry)) = players.get_mut(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if (pose.translation - centre).length() > STATION_REACH {
                continue;
            }
            // Block must still be a station.
            if lookup_station_def(req.station_cell, &chunks, &chunk_map, &block_registry).is_none()
            {
                continue;
            }
            let Some((item, count)) = carry.drop_all() else {
                continue;
            };
            let state = stations.get_or_insert(req.station_cell);
            state.deposit(item, count);
            broadcast_station(&mut broadcast, server, req.station_cell, Some(state.clone()));
            info!(
                cell = ?req.station_cell.to_array(),
                item = item.0,
                count,
                "station deposit",
            );
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "wire handler joins many subsystems")]
fn receive_work_station(
    mut receivers: Query<(Entity, &mut MessageReceiver<WorkStation>)>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&crate::protocol::AvatarPose, With<crate::protocol::Avatar>>,
    chunks: Query<&crate::voxel::Chunk>,
    chunk_map: Res<crate::voxel::ChunkMap>,
    block_registry: Res<crate::blocks::BlockRegistry>,
    item_registry: Res<crate::items::ItemRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    mut stations: ResMut<CraftStations>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut commands: Commands,
) {
    use block_junk_mod_api::recipes::RecipeId;
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        for req in receiver.receive() {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let centre = req.station_cell.as_vec3() + Vec3::splat(0.5);
            if (pose.translation - centre).length() > STATION_REACH {
                continue;
            }
            let Some(station_def) = lookup_station_def(
                req.station_cell,
                &chunks,
                &chunk_map,
                &block_registry,
            ) else {
                continue;
            };
            let recipe_id = RecipeId::new(req.recipe_id.clone());
            let Some(recipe_slot) = recipes.slot_of(&recipe_id) else {
                continue;
            };
            let recipe = recipes.def(recipe_slot);
            if recipe.station != station_def.tag || recipe.tier > station_def.tier {
                continue;
            }
            let Some(state) = stations.get_mut(req.station_cell) else {
                continue;
            };
            // Order must exist + have remaining quantity.
            let Some(order_idx) = state
                .orders
                .iter()
                .position(|o| o.recipe_id == req.recipe_id && !o.is_done())
            else {
                continue;
            };
            // Inputs must resolve + station inventory must satisfy
            // every entry. Pre-check before any consume so a
            // multi-input recipe doesn't partially deplete on
            // failure.
            let inputs_ok = recipe.inputs.iter().all(|input| {
                let Some(slot) = item_registry.slot_of(&input.item) else {
                    return false;
                };
                state.inventory.get(&slot).copied().unwrap_or(0) >= input.count
            });
            if !inputs_ok {
                continue;
            }
            // Output slot resolves (boot validator guarantees, but
            // defensive).
            let Some(output_slot) = item_registry.slot_of(&recipe.output.item) else {
                warn!(
                    recipe = %recipe.id,
                    output = %recipe.output.item,
                    "work commit: output item missing from registry; skipping",
                );
                continue;
            };
            // Commit: consume inputs, bump completed, maybe drop the
            // order if it just finished.
            for input in &recipe.inputs {
                let slot = item_registry.slot_of(&input.item).expect("checked above");
                state.try_consume(slot, input.count);
            }
            state.orders[order_idx].completed += 1;
            let order_done = state.orders[order_idx].is_done();
            if order_done {
                state.orders.remove(order_idx);
            }
            // Spawn output on top of the station.
            let top_of_station = Vec3::new(
                req.station_cell.x as f32 + 0.5,
                req.station_cell.y as f32 + 1.05,
                req.station_cell.z as f32 + 0.5,
            );
            for unit in 0..recipe.output.count {
                let angle = (unit as f32) * std::f32::consts::TAU
                    / recipe.output.count.max(1) as f32;
                let offset = Vec3::new(angle.cos() * 0.12, 0.0, angle.sin() * 0.12);
                let translation = top_of_station + offset;
                commands.spawn((
                    crate::protocol::WorldItem {
                        item: output_slot,
                        translation,
                    },
                    Transform::from_translation(translation),
                    GlobalTransform::default(),
                    Replicate::to_clients(NetworkTarget::All),
                    Name::new(format!("WorldItem(crafted:{})", recipe.output.item)),
                ));
            }
            info!(
                cell = ?req.station_cell.to_array(),
                recipe = %recipe.id,
                output = %recipe.output.item,
                count = recipe.output.count,
                done = order_done,
                "station work cycle",
            );
            let snapshot = state.clone();
            let now_empty = snapshot.is_empty();
            stations.remove_if_empty(req.station_cell);
            broadcast_station(
                &mut broadcast,
                server,
                req.station_cell,
                if now_empty { None } else { Some(snapshot) },
            );
        }
    }
}

/// Helper: resolve the station def at `cell`, returning the (tag,
/// tier) pair. `None` ⇒ cell is empty, unloaded, or holds a
/// non-station block. Skips having to thread an Option<&BlockDef>
/// through every handler.
struct StationDefView {
    tag: block_junk_mod_api::blocks::TagId,
    tier: u8,
}

fn lookup_station_def(
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

/// Helper: send one `StationUpdate` to every client. `state = None`
/// signals "remove this cell from the mirror" — used when an order
/// completes and the station has nothing left.
fn broadcast_station(
    broadcast: &mut ServerMultiMessageSender,
    server: &Server,
    cell: IVec3,
    state: Option<StationState>,
) {
    let msg = StationUpdate { cell, state };
    if let Err(err) = broadcast.send::<StationUpdate, WorldChannel>(
        &msg,
        server,
        &NetworkTarget::All,
    ) {
        warn!("station update broadcast failed: {err}");
    }
}
