//! Spatial index of sleeper cells + per-bed claim table (server-only).
//!
//! Mirrors [`crate::consumables`] for the sleeper need-axis. Two
//! resources live here:
//!
//! - [`SleeperIndex`] — world-cell → [`BlockSlot`] map of every cell
//!   whose def has [`Sleeper`] metadata. Built incrementally on chunk
//!   spawn and `CellEdit`, scanned by the snapshot builder to surface
//!   "what beds are nearby?" to NPC planners.
//! - [`BedClaims`] — per-anchor-cell reservation table. Only one NPC
//!   may claim a given sleeper at a time. Brain code goes through
//!   [`BedClaims::try_claim`] before committing to a Sleep goal and
//!   [`BedClaims::release`] on wake / abandon / despawn.
//!
//! The index keys per-cell because a multi-cell bed exposes both the
//! foot and the head as candidate "this is a bed" hits — but the
//! claim keys per-anchor: only one NPC sleeps in a bed regardless of
//! whether they targeted the foot or head cell. The brain resolves
//! the anchor before claiming (via the chunk sidecar), so a planner
//! that picked the head and one that picked the foot of the same bed
//! contend for the same slot.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::npc::NpcId;
use crate::protocol::{CHUNK_PADDED, CellEdit, ChunkCoord, GameSet};
use crate::voxel::{Chunk, chunk_local_to_world};

/// Cell → sleeper [`BlockSlot`]. Same shape as [`crate::consumables::ConsumableIndex`]
/// — a slot is enough for the snapshot builder to look up the
/// [`Sleeper`](block_junk_mod_api::blocks::Sleeper) def via the
/// [`BlockRegistry`]; the index doesn't duplicate `restores` /
/// `duration_secs`.
#[derive(Resource, Default, Debug)]
pub struct SleeperIndex {
    cells: HashMap<IVec3, BlockSlot>,
}

impl SleeperIndex {
    /// Iterate every sleeper cell within `radius_cells` (Chebyshev) of
    /// `centre`. Linear in the index size — fine while sleepers are
    /// counted in dozens; needs spatial bucketing if the count climbs
    /// to thousands.
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

/// Per-anchor-cell reservation map. Anchor (not target cell) so that
/// the foot and head of a multi-cell bed contend for the same slot.
/// Claims are released by the brain on every transition out of
/// `Goal::Sleeping` (clean wake, abandonment, despawn) and on
/// `BrainDisabled` insertion. Claims do NOT survive save/load — the
/// brain resets to Idle on load so any in-flight sleep restarts from
/// scratch.
#[derive(Resource, Default, Debug)]
pub struct BedClaims {
    by_anchor: HashMap<IVec3, NpcId>,
}

impl BedClaims {
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
    /// not held by `npc` is silently a no-op — protects against double-
    /// release on transitions where multiple paths dispatch the same
    /// release (e.g. abandon + arrive racing).
    pub fn release(&mut self, anchor: IVec3, npc: NpcId) {
        if let Some(holder) = self.by_anchor.get(&anchor) {
            if holder.0 == npc.0 {
                self.by_anchor.remove(&anchor);
            }
        }
    }

    /// Drop every claim held by `npc`. Called on NPC despawn / brain-
    /// disable so a single NPC can't permanently lock a bed by failing
    /// in some unanticipated way.
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_anchor.retain(|_, holder| holder.0 != npc.0);
    }

    /// True if `anchor` is currently claimed by anyone other than `npc`.
    /// Used by the planner snapshot to filter "available beds" without
    /// needing to take the claim — taking happens later, atomically,
    /// when the brain commits to the Sleep goal.
    pub fn is_taken_by_other(&self, anchor: IVec3, npc: NpcId) -> bool {
        match self.by_anchor.get(&anchor) {
            Some(holder) => holder.0 != npc.0,
            None => false,
        }
    }
}

pub struct SleeperIndexPlugin;

impl Plugin for SleeperIndexPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SleeperIndex>();
        app.init_resource::<BedClaims>();
        app.add_observer(scan_chunk_on_add);
        app.add_systems(Update, apply_cell_edits.in_set(GameSet::PostSimulation));
    }
}

/// Scan a freshly-spawned chunk for sleeper cells. Same shape as the
/// consumable scanner — save-loaded chunks may carry beds placed in a
/// prior session; procedural chunks contain none, so the cost is one
/// linear walk per chunk.
fn scan_chunk_on_add(
    trigger: On<Add, Chunk>,
    chunks: Query<(&Chunk, &ChunkCoord)>,
    registry: Res<BlockRegistry>,
    mut index: ResMut<SleeperIndex>,
) {
    let Ok((chunk, coord)) = chunks.get(trigger.entity) else {
        return;
    };
    let mut added = 0usize;
    for x in 1..(CHUNK_PADDED as i32 - 1) {
        for y in 1..(CHUNK_PADDED as i32 - 1) {
            for z in 1..(CHUNK_PADDED as i32 - 1) {
                let local = IVec3::new(x, y, z);
                let slot = chunk.get(local);
                if slot.is_empty() {
                    continue;
                }
                if registry.def(slot).sleeper.is_none() {
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
            "indexed sleepers in chunk",
        );
    }
}

/// Mirror every `CellEdit` into the index. Symmetric with the
/// consumable handler — a sleeper slot inserts; anything else (empty
/// from a break, a non-sleeper replacement) removes. Claims keyed by
/// anchor cell are NOT auto-released here — when a bed is broken
/// out from under a sleeping NPC, the brain notices on its next
/// re-validation and ends the sleep. Aggressively releasing here
/// could race with the brain re-validating and produce a "this bed
/// has no claim" gap that another NPC swoops in on; keeping the
/// release brain-driven keeps it in one place.
fn apply_cell_edits(
    mut reader: MessageReader<CellEdit>,
    mut index: ResMut<SleeperIndex>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        if edit.slot.is_empty() {
            if index.cells.remove(&edit.world).is_some() {
                info!(
                    cell = ?edit.world.to_array(),
                    total = index.len(),
                    "sleeper cell removed from index",
                );
            }
            continue;
        }
        let def = registry.def(edit.slot);
        if def.sleeper.is_some() {
            let was_new = !index.cells.contains_key(&edit.world);
            index.cells.insert(edit.world, edit.slot);
            if was_new {
                info!(
                    cell = ?edit.world.to_array(),
                    block = %def.id,
                    total = index.len(),
                    "sleeper cell added to index",
                );
            }
        } else {
            index.cells.remove(&edit.world);
        }
    }
}
