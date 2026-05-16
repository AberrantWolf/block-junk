//! Player-issued work orders: cells the player has tagged for NPCs to
//! eventually build or remove.
//!
//! `Plans` is the canonical map of tagged cells. It lives as a server-
//! authoritative `Resource` on the server App and as a passive mirror on
//! each client App, updated via [`PlanEdit`] broadcasts and a one-shot
//! [`PlanFullSync`] on connect. Mutating from the client side is a wire
//! request only — the server validates against world state, applies, and
//! broadcasts. The mirror reflects what the server accepted.
//!
//! Phase 3 is the data + wire + replication layer. Phase 4 adds drag-
//! AABB batching, Phase 6 wires NPC pickup against this map.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;

use crate::blocks::BlockRegistry;
use crate::camera::FlyCam;
use crate::client::{
    PlaceablePalette, PlacementRotation, RAYCAST_REACH, SelectedBlock, entity_aware_raycast,
    placement_orientation,
};
use crate::menu::AppState;
use crate::player_mode::PlayerMode;
use crate::protocol::{AvatarPose, GameSet, PlanEdit, PlanFullSync, PlanKind, WorldChannel};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, world_to_chunk};

#[derive(Resource, Default, Debug)]
pub struct Plans {
    map: HashMap<IVec3, PlanKind>,
}

impl Plans {
    pub fn set(&mut self, cell: IVec3, kind: PlanKind) {
        self.map.insert(cell, kind);
    }

    pub fn clear(&mut self, cell: IVec3) {
        self.map.remove(&cell);
    }

    pub fn replace_all(&mut self, entries: impl IntoIterator<Item = (IVec3, PlanKind)>) {
        self.map.clear();
        for (cell, kind) in entries {
            self.map.insert(cell, kind);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &PlanKind)> {
        self.map.iter()
    }

    pub fn snapshot(&self) -> Vec<(IVec3, PlanKind)> {
        self.map.iter().map(|(c, k)| (*c, *k)).collect()
    }
}

pub struct PlansServerPlugin;
pub struct PlansClientPlugin;

impl Plugin for PlansServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Plans>();
        // Observer fires per new client connection — same shape as
        // `register_new_client` sending `BlockManifest`. Late observers
        // are safe because lightyear has already installed the per-
        // connection MessageSender by the time `Connected` lands.
        app.add_observer(send_plan_full_sync_on_connect);
        app.add_systems(
            Update,
            receive_plan_edits.in_set(GameSet::Simulation),
        );
    }
}

impl Plugin for PlansClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Plans>();
        // Full-sync runs before per-edit receive so a sync arriving in
        // the same Bevy tick as an edit doesn't clobber the edit.
        app.add_systems(
            Update,
            (receive_plan_full_sync, receive_plan_edit_broadcasts)
                .chain()
                .in_set(GameSet::Simulation)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            Update,
            plan_mode_input
                .in_set(GameSet::Input)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Plan-mode click handling. L-click on a solid cell tags it for
/// removal; Shift+L on the empty cell adjacent to the hit face tags it
/// for build (with the current hotbar block + facing-derived
/// orientation); R-click on a tagged cell clears it. Tagging the
/// "wrong" cell type (L on empty, Shift+L on solid) is a silent no-op
/// — the verb is intent-pure: world state decides applicability, not
/// whatever happens to be tagged there already.
#[allow(clippy::too_many_arguments, reason = "input system spans many subsystems")]
fn plan_mode_input(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mode: Res<PlayerMode>,
    cam: Query<(&GlobalTransform, &FlyCam, &AvatarPose)>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    plans: Res<Plans>,
    selected: Res<SelectedBlock>,
    palette: Res<PlaceablePalette>,
    rotation: Res<PlacementRotation>,
    mut sender: Query<&mut MessageSender<PlanEdit>>,
) {
    if *mode != PlayerMode::Plan {
        return;
    }
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        return;
    }
    let left = mouse.just_pressed(MouseButton::Left);
    let right = mouse.just_pressed(MouseButton::Right);
    if !left && !right {
        return;
    }
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

    let Ok((cam_t, fly, pose)) = cam.single() else {
        return;
    };
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();
    let Some(hit) = entity_aware_raycast(origin, dir, RAYCAST_REACH, &chunks, &chunk_map, &registry)
    else {
        return;
    };

    if right {
        // R-click cancels the closest tag along the camera ray. Has to
        // use a plan-aware raycast rather than the world raycast: Build
        // tags live in *empty* cells, so the world raycast either flies
        // straight through them (hitting something far behind) or
        // misses entirely if there's nothing solid in the line of fire.
        // Test each tag's unit-cube AABB and pick the nearest hit.
        if let Some(cell) = raycast_plans(origin, dir, RAYCAST_REACH, &plans) {
            sender.send::<WorldChannel>(PlanEdit { cell, kind: None });
        }
        return;
    }

    if shift {
        // Shift+L: tag the empty cell adjacent to the hit face for
        // build. Mirrors the Build-mode anchor (hit + face_normal).
        let target = hit.cell + hit.face_normal;
        if cell_is_solid_client(target, &chunks, &chunk_map) {
            return;
        }
        let visible_yaw = pose.yaw + fly.pending_dyaw;
        let slot = selected.current(&palette);
        let orientation = placement_orientation(visible_yaw, rotation.0);
        sender.send::<WorldChannel>(PlanEdit {
            cell: target,
            kind: Some(PlanKind::Build { slot, orientation }),
        });
    } else {
        // Plain L: tag the hit (solid) cell for remove.
        if !cell_is_solid_client(hit.cell, &chunks, &chunk_map) {
            return;
        }
        sender.send::<WorldChannel>(PlanEdit {
            cell: hit.cell,
            kind: Some(PlanKind::Remove),
        });
    }
}

fn cell_is_solid_client(
    cell: IVec3,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
) -> bool {
    let (coord, local) = world_to_chunk(cell);
    chunk_map
        .0
        .get(&coord)
        .and_then(|&entity| chunks.get(entity).ok())
        .map(|(chunk, _)| !chunk.get(local).is_empty())
        .unwrap_or(false)
}

/// Closest-hit raycast against the unit-cube AABB of every tagged cell.
/// O(n) per call, fine for sparse plans (a hundred tags is a long way
/// from a perf concern). Returns the cell whose AABB is hit nearest to
/// the ray origin, or `None` if nothing intersects within `max_distance`.
///
/// Uses the slab method with `1.0 / dir` — Vec3 division by a zero
/// component produces ±inf, which the min/max chain handles correctly
/// as long as the origin isn't exactly on an integer-aligned plane
/// (then 0 × inf yields NaN). The camera's eye position is fractional
/// in practice, so this is safe.
fn raycast_plans(origin: Vec3, dir: Vec3, max_distance: f32, plans: &Plans) -> Option<IVec3> {
    let inv_dir = Vec3::ONE / dir;
    let mut best: Option<(f32, IVec3)> = None;
    for (cell, _) in plans.iter() {
        let min = cell.as_vec3();
        let max = min + Vec3::ONE;
        let t1 = (min - origin) * inv_dir;
        let t2 = (max - origin) * inv_dir;
        let tmin_v = t1.min(t2);
        let tmax_v = t1.max(t2);
        let tmin = tmin_v.x.max(tmin_v.y).max(tmin_v.z);
        let tmax = tmax_v.x.min(tmax_v.y).min(tmax_v.z);
        if tmax < tmin || tmax < 0.0 {
            continue;
        }
        // Forward distance to entry — or to exit if the origin is
        // already inside the cell's AABB.
        let t = if tmin >= 0.0 { tmin } else { tmax };
        if t > max_distance {
            continue;
        }
        if best.map(|(bt, _)| t < bt).unwrap_or(true) {
            best = Some((t, *cell));
        }
    }
    best.map(|(_, c)| c)
}

/// Server: ingest client requests, validate against live world state,
/// apply to the authoritative `Plans`, broadcast the canonical applied
/// edit. Reject (silently) edits that don't make sense against the
/// world (tag-remove on empty, tag-build on solid).
fn receive_plan_edits(
    mut receivers: Query<&mut MessageReceiver<PlanEdit>>,
    mut plans: ResMut<Plans>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for mut receiver in receivers.iter_mut() {
        let edits: Vec<PlanEdit> = receiver.receive().collect();
        for edit in edits {
            let accepted = match edit.kind {
                Some(PlanKind::Remove) => {
                    if !cell_is_solid(edit.cell, &chunks, &chunk_map) {
                        continue;
                    }
                    plans.set(edit.cell, PlanKind::Remove);
                    true
                }
                Some(kind @ PlanKind::Build { .. }) => {
                    if cell_is_solid(edit.cell, &chunks, &chunk_map) {
                        continue;
                    }
                    plans.set(edit.cell, kind);
                    true
                }
                None => {
                    plans.clear(edit.cell);
                    true
                }
            };
            if !accepted {
                continue;
            }
            if let Err(err) =
                broadcast.send::<PlanEdit, WorldChannel>(&edit, server, &NetworkTarget::All)
            {
                warn!("PlanEdit broadcast failed: {err}");
            }
        }
    }
}

fn cell_is_solid(cell: IVec3, chunks: &Query<&Chunk>, chunk_map: &ChunkMap) -> bool {
    let (coord, local) = world_to_chunk(cell);
    chunk_map
        .0
        .get(&coord)
        .and_then(|&entity| chunks.get(entity).ok())
        .map(|chunk| !chunk.get(local).is_empty())
        .unwrap_or(false)
}

/// Server: on a new client connect, push the current `Plans` snapshot so
/// the joiner sees existing tags. Subsequent edits arrive as `PlanEdit`
/// broadcasts. Empty snapshots still send — the receive side relies on
/// the message to know "the full state is now what I've seen."
fn send_plan_full_sync_on_connect(
    trigger: On<Add, Connected>,
    plans: Res<Plans>,
    mut senders: Query<&mut MessageSender<PlanFullSync>>,
) {
    let connection = trigger.entity;
    let Ok(mut sender) = senders.get_mut(connection) else {
        return;
    };
    let sync = PlanFullSync {
        entries: plans.snapshot(),
    };
    sender.send::<WorldChannel>(sync);
}

/// Client: apply a broadcast edit to the local mirror. The server has
/// already done the validation; here we just trust the kind field.
fn receive_plan_edit_broadcasts(
    mut receivers: Query<&mut MessageReceiver<PlanEdit>>,
    mut plans: ResMut<Plans>,
) {
    for mut receiver in receivers.iter_mut() {
        for edit in receiver.receive() {
            match edit.kind {
                Some(kind) => plans.set(edit.cell, kind),
                None => plans.clear(edit.cell),
            }
        }
    }
}

/// Client: replace the local mirror with the join-time snapshot. Drops
/// any stale entries from a previous session.
fn receive_plan_full_sync(
    mut receivers: Query<&mut MessageReceiver<PlanFullSync>>,
    mut plans: ResMut<Plans>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            plans.replace_all(sync.entries);
        }
    }
}
