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
    chunks: Query<(&Chunk, &ChunkCoord, Option<&ChunkEntities>)>,
    registry: Res<BlockRegistry>,
    time: Res<Time>,
    mut schedule: ResMut<RegrowSchedule>,
) {
    let Ok((chunk, coord, sidecar)) = chunks.get(trigger.entity) else {
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
                if sidecar.is_some_and(|sidecar| {
                    matches!(
                        sidecar.get(world),
                        Some(crate::voxel::EntryKind::Ghost { .. })
                    )
                }) {
                    continue;
                }
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
        let regrow = (edit.is_anchor && !edit.slot.is_empty())
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
#[derive(bevy::ecs::system::SystemParam)]
struct RegrowWorld<'w, 's> {
    facilities: crate::facility_inventory::FacilityService<'w>,
    chunks: Query<'w, 's, (&'static mut Chunk, &'static mut ChunkEntities)>,
    map: Res<'w, ChunkMap>,
    registry: Res<'w, BlockRegistry>,
    placements: ResMut<'w, crate::placements::Placements>,
    construction: crate::construction::ConstructionService<'w>,
    effects: ResMut<'w, crate::server::PendingTerrainEffects>,
}
fn fire_due_regrows(
    time: Res<Time>,
    mut schedule: ResMut<RegrowSchedule>,
    mut commands: Commands,
    mut world: RegrowWorld,
    mut bus: MessageWriter<CellEdit>,
) {
    let now = time.elapsed_secs();
    let mut due: Vec<_> = schedule
        .due
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(cell, at)| (*cell, *at))
        .collect();
    due.sort_by(|(a, at), (b, bt)| at.total_cmp(bt).then(a.to_array().cmp(&b.to_array())));
    for (cell, _) in due.into_iter().take(16) {
        // Unknown geometry and temporary obstructions retain the obligation.
        schedule.due.insert(cell, now + 1.);
        let base = |cell| {
            let (coord, local) = world_to_chunk(cell);
            let (chunk, entities) = world.chunks.get(*world.map.0.get(&coord)?).ok()?;
            Some((chunk.get(local), entities.get(cell), chunk.revision))
        };
        let Some((slot, entry, _)) = base(cell) else {
            continue;
        };
        let orientation = match entry {
            Some(crate::voxel::EntryKind::Anchor { orientation, .. }) => orientation,
            Some(crate::voxel::EntryKind::Ghost { .. }) => {
                schedule.due.remove(&cell);
                continue;
            }
            None => Cardinal::default(),
        };
        let Some(regrow) = world.registry.def(slot).regrow.as_ref() else {
            schedule.due.remove(&cell);
            continue;
        };
        let Some(into_slot) = world.registry.slot_of(&regrow.into) else {
            continue;
        };
        let mut terrain = crate::terrain_transaction::TerrainTransaction::default();
        if terrain
            .replace(
                BlockEdit {
                    anchor: cell,
                    slot: into_slot,
                    orientation,
                },
                &world.registry,
                &base,
            )
            .is_err()
        {
            continue;
        }
        if world.effects.0.len() + terrain.effects.len() > 4096 {
            continue;
        }
        let mut placements = world.placements.clone();
        if placements
            .apply_effects(&terrain.effects, &world.registry)
            .is_err()
        {
            continue;
        }
        let mut construction = world.construction.stage();
        construction.resolve_terrain(terrain.changes().map(|(cell, _, _)| cell));
        world.construction.commit(construction);
        world.facilities.retire(&terrain.effects, &world.registry);
        *world.placements = placements;
        let effects = crate::server::commit_terrain_geometry(
            terrain,
            &mut commands,
            &mut world.chunks,
            &world.map,
            &mut bus,
            None,
            false,
        );
        world.effects.0.extend(effects);
        schedule.due.remove(&cell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_regrowth_retries_without_removing_the_existing_resource() {
        let defs = serde_json::from_value(serde_json::json!([
            {"id":"test:bare","display_name":"Bare","flags":{},"color":[1,1,1],"regrow":{"into":"test:ripe","after_secs":0.01}},
            {"id":"test:ripe","display_name":"Ripe","flags":{},"color":[1,1,1],"footprint":[[0,0,0],[1,0,0]]}
        ])).unwrap();
        let registry = BlockRegistry::build(defs).unwrap().0;
        let bare = registry.require("test:bare");
        let ripe = registry.require("test:ripe");
        let mut app = App::new();
        crate::facility_inventory::install_fixture(&mut app);
        app.add_plugins(MinimalPlugins)
            .insert_resource(registry)
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(100),
            ))
            .init_resource::<ChunkMap>()
            .init_resource::<crate::plans::Plans>()
            .init_resource::<crate::construction::ConstructionLedger>()
            .init_resource::<crate::placements::Placements>()
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::haul::HaulStore>()
            .init_resource::<crate::server::PendingTerrainEffects>()
            .add_message::<CellEdit>()
            .add_plugins(RegrowServerPlugin);
        let cell = IVec3::new(4, 2, 4);
        let blocked = cell + IVec3::from_array(Cardinal::default().rotate_offset([1, 0, 0]));
        let mut chunk = Chunk {
            revision: 0,
            blocks: vec![crate::blocks::BlockSlot::EMPTY; (CHUNK_PADDED as usize).pow(3)],
        };
        chunk.set(world_to_chunk(cell).1, bare);
        // A ripe single cell is deliberately an unrelated obstruction; this
        // fixture does not ask the service to remove or validate that object.
        chunk.set(world_to_chunk(blocked).1, ripe);
        let entity = app
            .world_mut()
            .spawn((ChunkCoord(IVec3::ZERO), chunk, ChunkEntities::default()))
            .id();
        app.world_mut()
            .resource_mut::<ChunkMap>()
            .0
            .insert(ChunkCoord(IVec3::ZERO), entity);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world()
                .get::<Chunk>(entity)
                .unwrap()
                .get(world_to_chunk(cell).1),
            bare
        );
        assert!(
            app.world()
                .resource::<RegrowSchedule>()
                .due
                .contains_key(&cell)
        );
        assert!(
            app.world()
                .resource::<crate::server::PendingTerrainEffects>()
                .0
                .is_empty()
        );
        app.world_mut()
            .get_mut::<Chunk>(entity)
            .unwrap()
            .set(world_to_chunk(blocked).1, crate::blocks::BlockSlot::EMPTY);
        for _ in 0..20 {
            app.update();
        }
        let chunk = app.world().get::<Chunk>(entity).unwrap();
        assert_eq!(chunk.get(world_to_chunk(cell).1), ripe);
        assert_eq!(chunk.get(world_to_chunk(blocked).1), ripe);
        assert!(
            !app.world()
                .resource::<RegrowSchedule>()
                .due
                .contains_key(&cell)
        );
        assert_eq!(
            app.world()
                .resource::<crate::server::PendingTerrainEffects>()
                .0
                .len(),
            2
        );
    }

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
