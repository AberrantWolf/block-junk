//! Timed block self-transitions — the "grow back" half of the S4 forage
//! loop (bare berry bush → ripe berry bush; the substrate future crops
//! will reuse).
//!
//! A block whose [`BlockDef.regrow`](block_junk_mod_api::blocks::BlockDef::regrow)
//! is set schedules a transition the moment a live instance appears — by
//! placement, by the harvest-transform in `apply_break`, or by a save-
//! loaded chunk coming online. When the timer elapses the server rewrites
//! the cell through the same `apply_block_edit` path client edits use, so
//! the change replicates like any other block edit and every downstream
//! index (interactables, rooms) updates off the resulting `CellEdit`.
//!
//! **Runtime-only.** The schedule lives in memory and is re-primed on
//! load (a fresh chunk carries no bare bushes; a loaded one gets its
//! timers restarted by the chunk-add scan). A reload therefore restarts
//! every pending regrow's clock — which reads as "the world paused while
//! you were away," an acceptable simplification that keeps the save
//! format unchanged. The clock is Bevy `Time::elapsed_secs`, matching the
//! NPC scheduler's `now_secs`.

use bevy::prelude::*;
use std::collections::HashMap;

use crate::blocks::BlockRegistry;
use crate::protocol::{BlockEdit, CHUNK_PADDED, CellEdit, ChunkCoord, GameSet};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, chunk_local_to_world, world_to_chunk};
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::{Server, ServerMultiMessageSender};

/// Cell → wall-clock second (`Time::elapsed_secs`) at which the block
/// there transforms into its `regrow.into`. Server-only; never
/// serialised (see module docs on the re-prime-on-load contract).
#[derive(Resource, Default, Debug)]
pub struct RegrowSchedule {
    due: HashMap<IVec3, f32>,
}

impl RegrowSchedule {
    /// Schedule `cell` to regrow at `at` unless it already has a running
    /// timer — re-scanning a reloaded chunk must not reset an in-flight
    /// countdown.
    fn arm(&mut self, cell: IVec3, at: f32) {
        self.due.entry(cell).or_insert(at);
    }
}

pub struct RegrowServerPlugin;

impl Plugin for RegrowServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RegrowSchedule>();
        // Loaded chunks may carry bare bushes harvested in a prior
        // session; arm them the moment the chunk entity spawns.
        app.add_observer(scan_chunk_on_add);
        // Mirror live edits into the schedule (same CellEdit slot as the
        // container/interactable indices), then fire the due ones. Both
        // in PostSimulation so they read this tick's applied edits;
        // `arm` before `fire` keeps a just-placed bare bush from waiting
        // an extra frame to be considered.
        app.add_systems(
            Update,
            (arm_from_cell_edits, fire_due_regrows)
                .chain()
                .in_set(GameSet::PostSimulation),
        );
    }
}

/// Arm every bare bush in a freshly-spawned chunk. Fresh procedural
/// chunks generate bushes *ripe*, so this only ever finds anything in
/// save-loaded chunks — exactly the re-prime path.
fn scan_chunk_on_add(
    trigger: On<Add, Chunk>,
    chunks: Query<(&Chunk, &ChunkCoord)>,
    registry: Res<BlockRegistry>,
    time: Res<Time>,
    mut schedule: ResMut<RegrowSchedule>,
) {
    let Ok((chunk, coord)) = chunks.get(trigger.entity) else {
        return;
    };
    let now = time.elapsed_secs();
    let padded = CHUNK_PADDED as i32;
    for x in 1..(padded - 1) {
        for y in 1..(padded - 1) {
            for z in 1..(padded - 1) {
                let local = IVec3::new(x, y, z);
                let slot = chunk.get(local);
                if slot.is_empty() {
                    continue;
                }
                let Some(regrow) = &registry.def(slot).regrow else {
                    continue;
                };
                let world = chunk_local_to_world(*coord, local);
                schedule.arm(world, now + regrow.after_secs);
            }
        }
    }
}

/// Mirror every `CellEdit` into the schedule: a block with `regrow` arms
/// a timer, anything else (including the empty slot from a break, or the
/// ripe bush a regrow produces) clears the entry.
fn arm_from_cell_edits(
    mut reader: MessageReader<CellEdit>,
    registry: Res<BlockRegistry>,
    time: Res<Time>,
    mut schedule: ResMut<RegrowSchedule>,
) {
    let now = time.elapsed_secs();
    for edit in reader.read() {
        let regrow = (!edit.slot.is_empty())
            .then(|| registry.def(edit.slot).regrow.as_ref())
            .flatten();
        match regrow {
            Some(r) => schedule.arm(edit.world, now + r.after_secs),
            None => {
                schedule.due.remove(&edit.world);
            }
        }
    }
}

/// Fire every regrow whose timer has elapsed. For each due cell, confirm
/// the block still carries `regrow` (a mined bare bush would have been
/// dropped from the schedule by `arm_from_cell_edits`, but re-read
/// defensively), then rewrite it to `regrow.into` through the shared
/// block-edit path.
#[allow(
    clippy::too_many_arguments,
    reason = "block-edit application spans many subsystems"
)]
fn fire_due_regrows(
    time: Res<Time>,
    mut schedule: ResMut<RegrowSchedule>,
    mut commands: Commands,
    mut chunks: Query<(&mut Chunk, &mut ChunkEntities)>,
    map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    servers: Query<&Server>,
    mut broadcast: ServerMultiMessageSender,
    mut bus: MessageWriter<CellEdit>,
) {
    let now = time.elapsed_secs();
    // Collect due cells first — we can't mutate the schedule while
    // borrowing it, and the edit path may itself write CellEdits.
    let due: Vec<IVec3> = schedule
        .due
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(cell, _)| *cell)
        .collect();
    if due.is_empty() {
        return;
    }
    let Ok(server) = servers.single() else {
        return;
    };
    for cell in due {
        schedule.due.remove(&cell);
        // Re-read the live block; resolve its regrow target now (the def
        // may name a different `into` than when armed, and the block may
        // have changed out from under us on a chunk we didn't observe).
        let Some(into_slot) = current_regrow_target(cell, &chunks, &map, &registry) else {
            continue;
        };
        crate::server::apply_block_edit(
            BlockEdit {
                anchor: cell,
                slot: into_slot,
                orientation: Cardinal::default(),
            },
            &mut commands,
            &mut chunks,
            &map,
            &registry,
            server,
            &mut broadcast,
            &mut bus,
        );
        info!(cell = ?cell.to_array(), block = %registry.id_of(into_slot), "block regrew");
    }
}

/// The resolved `regrow.into` slot of whatever block currently sits at
/// `cell`, or `None` if the cell is unloaded, empty, or holds a block
/// that no longer regrows.
fn current_regrow_target(
    cell: IVec3,
    chunks: &Query<(&mut Chunk, &mut ChunkEntities)>,
    map: &ChunkMap,
    registry: &BlockRegistry,
) -> Option<crate::blocks::BlockSlot> {
    let (coord, local) = world_to_chunk(cell);
    let &entity = map.0.get(&coord)?;
    let (chunk, _) = chunks.get(entity).ok()?;
    let slot = chunk.get(local);
    if slot.is_empty() {
        return None;
    }
    let regrow = registry.def(slot).regrow.as_ref()?;
    registry.slot_of(&regrow.into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `arm` sets a fresh timer but never resets a running one — a
    /// reloaded chunk re-scanning an already-scheduled bare bush must
    /// not push its regrow further into the future each load.
    #[test]
    fn arm_does_not_reset_a_running_timer() {
        let mut schedule = RegrowSchedule::default();
        let cell = IVec3::new(3, 8, -2);
        schedule.arm(cell, 100.0);
        schedule.arm(cell, 250.0); // later re-scan
        assert_eq!(
            schedule.due.get(&cell),
            Some(&100.0),
            "the original due time must survive a re-arm"
        );
    }
}
