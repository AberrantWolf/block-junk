//! Spatial index of consumable cells (server-only).
//!
//! Lets the NPC snapshot builder answer "what consumables are near
//! this NPC?" in O(consumables) without scanning every cell within a
//! radius. Each entry is a world-space cell paired with its
//! [`BlockSlot`] — the planner snapshot uses the slot to look up
//! [`block_junk_mod_api::blocks::Consumable`] metadata (which need it
//! restores, by how much).
//!
//! Maintenance has two sources:
//! 1. **Observer on chunk add.** Scans every newly-spawned chunk
//!    entity once, picking up consumables that arrived with a saved
//!    chunk. Procedural chunks contain none, so the scan is cheap and
//!    no-ops in that case — simpler than gating on `ChunkEdited`.
//! 2. **`CellEdit` reader.** Each per-cell broadcast from place/break
//!    updates the index incrementally. A placed consumable inserts; a
//!    broken or replaced consumable removes.
//!
//! Server-only — clients never read this. NPCs are server-authoritative,
//! so any consumer of the index lives on the server side.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::protocol::{CHUNK_PADDED, CellEdit, ChunkCoord, GameSet};
use crate::voxel::{Chunk, chunk_local_to_world};

/// Cell → consumable [`BlockSlot`] map. `BlockSlot` is enough for the
/// snapshot builder to look up the matching `Consumable` def from the
/// [`BlockRegistry`] — duplicating restore/duration values here would
/// drift if the registry ever supported live reloads.
#[derive(Resource, Default, Debug)]
pub struct ConsumableIndex {
    cells: HashMap<IVec3, BlockSlot>,
}

impl ConsumableIndex {
    /// Iterate every cell in the index within `radius_cells` (Chebyshev /
    /// chessboard distance) of `centre`. Linear in `len()` — fine while
    /// the world holds dozens of consumables; needs a chunked acceleration
    /// structure if that grows to thousands.
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

pub struct ConsumableIndexPlugin;

impl Plugin for ConsumableIndexPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConsumableIndex>();
        app.add_observer(scan_chunk_on_add);
        // Run in PostSimulation alongside the room-detector's dirty
        // queue — both subscribe to CellEdit and want to see the same
        // post-apply state.
        app.add_systems(Update, apply_cell_edits.in_set(GameSet::PostSimulation));
    }
}

/// On every newly-spawned chunk entity, scan its interior for consumable
/// cells and insert them into the index. Save-loaded chunks may carry
/// consumables placed in a prior session; freshly-generated procedural
/// chunks contain none, so the scan inserts nothing and the cost is one
/// linear walk per chunk life cycle.
fn scan_chunk_on_add(
    trigger: On<Add, Chunk>,
    chunks: Query<(&Chunk, &ChunkCoord)>,
    registry: Res<BlockRegistry>,
    mut index: ResMut<ConsumableIndex>,
) {
    let Ok((chunk, coord)) = chunks.get(trigger.entity) else {
        return;
    };
    let mut added = 0usize;
    // Interior cells run [1, CHUNK_PADDED - 1) in chunk-local coords —
    // same range the save-load and meshing paths use. Padding cells
    // never render so we never index them.
    for x in 1..(CHUNK_PADDED as i32 - 1) {
        for y in 1..(CHUNK_PADDED as i32 - 1) {
            for z in 1..(CHUNK_PADDED as i32 - 1) {
                let local = IVec3::new(x, y, z);
                let slot = chunk.get(local);
                if slot.is_empty() {
                    continue;
                }
                if registry.def(slot).consumable.is_none() {
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
            "indexed consumables in chunk",
        );
    }
}

/// Mirror every `CellEdit` into the index: a consumable slot inserts
/// (or replaces an existing entry of a different consumable kind); any
/// other slot — including the empty slot from a break — removes the
/// entry. We can't decide insert-vs-remove without consulting the
/// [`BlockRegistry`] because consumability is a per-def property, not
/// a flag bit.
fn apply_cell_edits(
    mut reader: MessageReader<CellEdit>,
    mut index: ResMut<ConsumableIndex>,
    registry: Res<BlockRegistry>,
) {
    for edit in reader.read() {
        if edit.slot.is_empty() {
            index.cells.remove(&edit.world);
            continue;
        }
        let def = registry.def(edit.slot);
        if def.consumable.is_some() {
            index.cells.insert(edit.world, edit.slot);
        } else {
            index.cells.remove(&edit.world);
        }
    }
}
