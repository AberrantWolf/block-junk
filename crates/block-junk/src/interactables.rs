//! Spatial index of interactable cells + per-block claim table
//! (server-only).
//!
//! Unifies what used to be `ConsumableIndex` + `SleeperIndex` +
//! `BedClaims`: now that every NPC-usable block carries one
//! [`Interactable`](block_junk_mod_api::blocks::Interactable) struct
//! regardless of the action it represents (eat, sleep, enchant, sit),
//! one index + one claim table covers them all.
//!
//! Two resources:
//!
//! - [`InteractableIndex`] — world-cell → [`BlockSlot`] for every cell
//!   whose def has `interactable` metadata. The snapshot builder
//!   surfaces "what interactions are nearby?" to NPC planners by
//!   scanning this; the planner reads `Interactable::need_restore` /
//!   `exclusive` via the [`BlockRegistry`] to decide which entries are
//!   relevant.
//! - [`InteractionClaims`] — per-anchor reservation table for
//!   *exclusive* interactables (today: beds; tomorrow: enchantment
//!   altars, forges). Non-exclusive interactables (food, water) skip
//!   the claim path entirely so a queue of hungry NPCs can use one
//!   basket without serialising on it.
//!
//! The index keys per-cell because a multi-cell interactable
//! (bed, large altar) exposes every footprint cell as a candidate
//! "this is an interaction" hit. Claims key per-anchor so foot/head
//! of a bed (or any other multi-cell interactable) contend for the
//! same slot — the brain resolves the anchor before claiming via the
//! chunk sidecar.

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

/// Per-anchor-cell reservation map for *exclusive* interactables.
/// Anchor (not target cell) so foot/head of a multi-cell interactable
/// contend for the same slot. Claims are released by the brain on
/// every transition out of `Goal::Interacting` (clean completion,
/// abandonment, despawn) and on `BrainDisabled` insertion. Claims do
/// NOT survive save/load — the brain resets to Idle on load so any
/// in-flight interaction restarts from scratch.
#[derive(Resource, Default, Debug)]
pub struct InteractionClaims {
    by_anchor: HashMap<IVec3, NpcId>,
}

impl InteractionClaims {
    /// Try to claim `anchor` for `npc`. Succeeds if the slot is empty
    /// or already held by the same NPC (re-claim is a no-op rather
    /// than a contention failure — the brain may re-call this if a
    /// goal restarts mid-flight).
    pub fn try_claim(&mut self, anchor: IVec3, npc: NpcId) -> bool {
        match self.by_anchor.get(&anchor) {
            Some(holder) if holder.0 == npc.0 => true,
            Some(_) => false,
            None => {
                self.by_anchor.insert(anchor, npc);
                true
            }
        }
    }

    /// Release `anchor`'s claim if `npc` holds it. Releasing a claim
    /// not held by `npc` is silently a no-op — protects against
    /// double-release on transitions where multiple paths dispatch
    /// the same release (e.g. abandon + arrive racing).
    pub fn release(&mut self, anchor: IVec3, npc: NpcId) {
        if let Some(holder) = self.by_anchor.get(&anchor) {
            if holder.0 == npc.0 {
                self.by_anchor.remove(&anchor);
            }
        }
    }

    /// Drop every claim held by `npc`. Called on NPC despawn /
    /// brain-disable so a single NPC can't permanently lock an
    /// interactable by failing in some unanticipated way.
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_anchor.retain(|_, holder| holder.0 != npc.0);
    }

    /// True if `anchor` is currently claimed by anyone other than
    /// `npc`. Used by the planner snapshot to filter "available
    /// exclusive interactables" without needing to take the claim —
    /// taking happens later, atomically, when the brain commits to
    /// the Interact goal.
    pub fn is_taken_by_other(&self, anchor: IVec3, npc: NpcId) -> bool {
        match self.by_anchor.get(&anchor) {
            Some(holder) => holder.0 != npc.0,
            None => false,
        }
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
