//! Legacy planner discovery index and shared named-slot reservation table.
//! The discovery view still offers one candidate per anchor until the bounded
//! policy candidate provider replaces it. Execution reserves exact use-slot IDs
//! through `facility_interactions`; furniture cells are never separate seats.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::npc::NpcId;
use crate::protocol::{CHUNK_PADDED, CellEdit, ChunkCoord, GameSet};
use crate::voxel::{Chunk, chunk_local_to_world};

/// Cell → interactable [`BlockSlot`] map. `BlockSlot` is enough for
/// the snapshot builder to look up the matching
/// [`Interactable`](block_junk_mod_api::blocks::Interactable) def from
/// the [`BlockRegistry`] — duplicating restore/duration/exclusive
/// values here would drift if the registry ever supported live
/// reloads.
#[derive(Resource, Default, Debug)]
pub struct InteractableIndex {
    cells: HashMap<IVec3, BlockSlot>,
}

impl InteractableIndex {
    /// Iterate every cell in the index within `radius_cells`
    /// (Chebyshev / chessboard distance) of `centre`. Linear in
    /// `len()` — fine while the world holds dozens of interactables;
    /// needs a chunked acceleration structure if that grows to
    /// thousands.
    pub fn iter_within(
        &self,
        centre: IVec3,
        radius_cells: i32,
    ) -> impl Iterator<Item = (IVec3, BlockSlot)> + '_ {
        let r = radius_cells.max(0);
        self.cells.iter().filter_map(move |(cell, slot)| {
            let d = *cell - centre;
            if d.x.abs() <= r && d.y.abs() <= r && d.z.abs() <= r {
                Some((*cell, *slot))
            } else {
                None
            }
        })
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }
}

/// Exclusive leases key stable placement plus authored slot name. Different
/// seats on one object do not contend. Leases are transient; durable interaction
/// jobs retain progress and require revalidation after loading.
#[derive(Resource, Default, Debug, Clone)]
pub struct InteractionClaims {
    by_slot: crate::reservations::ReservationTable<block_junk_mod_api::architecture::UseSlotId>,
    slot_anchors: HashMap<block_junk_mod_api::architecture::UseSlotId, IVec3>,
    /// Anchors whose approach recently defeated an NPC (physically
    /// stuck mid-walk or no A* path) → world-time when retries may
    /// resume. Same 30 s backoff pattern as `HaulStore`'s unreachable
    /// memo: without it a starving NPC re-picks the same blocked
    /// basket every stuck-release forever while reachable alternatives
    /// sit ignored one entry further down the snapshot.
    unreachable_until: HashMap<IVec3, f32>,
}

impl InteractionClaims {
    pub fn slot_reserved(&self, slot: &block_junk_mod_api::architecture::UseSlotId) -> bool {
        self.by_slot.holder(slot).is_some()
    }
    pub fn claim_slot(
        &mut self,
        anchor: IVec3,
        slot: block_junk_mod_api::architecture::UseSlotId,
        owner: block_junk_mod_api::authority::ReservationOwner,
    ) -> Option<crate::reservations::Lease> {
        let lease = self.by_slot.acquire(slot.clone(), owner)?;
        self.slot_anchors.insert(slot, anchor);
        Some(lease)
    }
    pub fn matches_slot(
        &self,
        slot: &block_junk_mod_api::architecture::UseSlotId,
        lease: &crate::reservations::Lease,
    ) -> bool {
        self.by_slot.holder(slot) == Some(lease)
    }
    pub fn release_slot(
        &mut self,
        slot: &block_junk_mod_api::architecture::UseSlotId,
        lease: &crate::reservations::Lease,
    ) {
        if self.by_slot.release(slot, lease) {
            self.slot_anchors.remove(slot);
        }
    }
    /// Transitional goal cleanup; the action service also checks its exact lease.
    pub fn release(&mut self, anchor: IVec3, npc: NpcId) {
        let owner = block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 };
        let leases: Vec<_> = self
            .slot_anchors
            .iter()
            .filter(|(_, cell)| **cell == anchor)
            .filter_map(|(slot, _)| {
                self.by_slot
                    .holder(slot)
                    .filter(|lease| lease.owner == owner)
                    .map(|lease| (slot.clone(), lease.clone()))
            })
            .collect();
        for (slot, lease) in leases {
            self.release_slot(&slot, &lease);
        }
    }

    /// Drop every claim held by `npc`. Called on NPC despawn /
    /// brain-disable so a single NPC can't permanently lock an
    /// interactable by failing in some unanticipated way.
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_slot
            .release_owner(&block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 });
        self.slot_anchors
            .retain(|slot, _| self.by_slot.holder(slot).is_some());
    }

    /// True if `anchor` is currently claimed by anyone other than
    /// `npc`. Used by the planner snapshot to filter "available
    /// exclusive interactables" without needing to take the claim —
    /// taking happens later, atomically, when the brain commits to
    /// the Interact goal.
    pub fn is_taken_by_other(&self, anchor: IVec3, npc: NpcId) -> bool {
        self.slot_anchors.iter().any(|(slot, cell)| {
            *cell == anchor
                && self.by_slot.holder(slot).is_some_and(|lease| {
                    lease.owner
                        != block_junk_mod_api::authority::ReservationOwner::Npc { id: npc.0 }
                })
        })
    }

    /// Record that reaching `anchor` just failed (A* miss or physical
    /// stuck); the planner snapshot won't offer it again until
    /// `retry_at` (elapsed seconds).
    pub fn memo_unreachable(&mut self, anchor: IVec3, retry_at: f32) {
        self.unreachable_until.insert(anchor, retry_at);
    }

    /// True while `anchor` is memoized as unreachable. Read-only (no
    /// lazy prune — the table is bounded by the interactable count).
    pub fn is_unreachable(&self, anchor: IVec3, now: f32) -> bool {
        self.unreachable_until
            .get(&anchor)
            .is_some_and(|&retry_at| now < retry_at)
    }
}

pub struct InteractableIndexPlugin;

impl Plugin for InteractableIndexPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InteractableIndex>();
        app.init_resource::<InteractionClaims>();
        app.add_observer(scan_chunk_on_add);
        // Run in PostSimulation alongside the room-detector's dirty
        // queue — both subscribe to CellEdit and want to see the
        // same post-apply state.
        app.add_systems(Update, apply_cell_edits.in_set(GameSet::PostSimulation));
    }
}

/// On every newly-spawned chunk entity, scan its interior for
/// interactable cells and insert them into the index. Save-loaded
/// chunks may carry interactables placed in a prior session;
/// freshly-generated procedural chunks contain none, so the scan
/// inserts nothing and the cost is one linear walk per chunk life
/// cycle.
fn scan_chunk_on_add(
    trigger: On<Add, Chunk>,
    chunks: Query<(&Chunk, &ChunkCoord)>,
    registry: Res<BlockRegistry>,
    mut index: ResMut<InteractableIndex>,
) {
    let Ok((chunk, coord)) = chunks.get(trigger.entity) else {
        return;
    };
    let mut added = 0usize;
    // Interior cells run [1, CHUNK_PADDED - 1) in chunk-local coords
    // — same range the save-load and meshing paths use. Padding
    // cells never render so we never index them.
    for x in 1..(CHUNK_PADDED as i32 - 1) {
        for y in 1..(CHUNK_PADDED as i32 - 1) {
            for z in 1..(CHUNK_PADDED as i32 - 1) {
                let local = IVec3::new(x, y, z);
                let slot = chunk.get(local);
                if slot.is_empty() {
                    continue;
                }
                if registry.def(slot).interactable.is_none() {
                    continue;
                }
                let world = chunk_local_to_world(*coord, local);
                index.cells.insert(world, slot);
                added += 1;
            }
        }
    }
    if added > 0 {
        info!(
            chunk = ?coord.0.to_array(),
            added,
            total = index.len(),
            "indexed interactables in chunk",
        );
    }
}

/// Mirror every `CellEdit` into the index: an interactable slot
/// inserts (or replaces an existing entry of a different
/// interactable kind); any other slot — including the empty slot
/// from a break — removes the entry.
///
/// Claims keyed by anchor cell are NOT auto-released here — when an
/// exclusive interactable is broken out from under an active NPC,
/// the brain notices on its next re-validation and ends the action.
/// Aggressively releasing here could race with the brain and
/// produce a "this anchor has no claim" gap that another NPC swoops
/// in on; keeping the release brain-driven keeps it in one place.
fn apply_cell_edits(
    mut reader: MessageReader<CellEdit>,
    mut index: ResMut<InteractableIndex>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        if edit.slot.is_empty() {
            index.cells.remove(&edit.world);
            continue;
        }
        let def = registry.def(edit.slot);
        if def.interactable.is_some() {
            index.cells.insert(edit.world, edit.slot);
        } else {
            index.cells.remove(&edit.world);
        }
    }
}
