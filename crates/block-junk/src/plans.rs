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
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::camera::FlyCam;
use crate::client::{
    PlaceablePalette, PlacementRotation, RAYCAST_REACH, SelectedBlock, entity_aware_raycast,
    placement_orientation,
};
use crate::menu::AppState;
use crate::player_mode::PlayerMode;
use crate::protocol::{
    AvatarPose, GameSet, PLAN_EDIT_BATCH_MAX, PlanEdit, PlanEditBatch, PlanFullSync, PlanKind,
    WorldChannel,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, world_to_chunk};

#[derive(Resource, Default, Debug)]
pub struct Plans {
    map: HashMap<IVec3, PlanKind>,
}

impl Plans {
    pub fn get(&self, cell: IVec3) -> Option<PlanKind> {
        self.map.get(&cell).copied()
    }

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
            (receive_plan_edits, receive_plan_edit_batches)
                .in_set(GameSet::Simulation),
        );
    }
}

impl Plugin for PlansClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Plans>();
        app.init_resource::<PlanDragState>();
        // Full-sync runs before per-edit receive so a sync arriving in
        // the same Bevy tick as an edit doesn't clobber the edit.
        app.add_systems(
            Update,
            (
                receive_plan_full_sync,
                receive_plan_edit_broadcasts,
                receive_plan_edit_batch_broadcasts,
            )
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
        app.add_systems(
            Update,
            draw_drag_preview
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Per-frame state of an in-flight drag. A drag starts on mousedown
/// against a solid block (the anchor) and tracks the projected second
/// corner each frame until release. On release the rectangle of cells
/// in the anchor's face plane is committed as one [`PlanEditBatch`].
#[derive(Resource, Default)]
pub(crate) struct PlanDragState {
    pub(crate) active: Option<ActiveDrag>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveDrag {
    verb: DragVerb,
    anchor: IVec3,
    face_normal: IVec3,
    /// Latest cell on the face plane. Same row as `anchor` (F-axis
    /// coord locked to `anchor`'s value). Updated each frame the
    /// button is held.
    second: IVec3,
    /// Captured at mousedown so changing the wheel selection mid-
    /// drag doesn't shift the build payload partway through.
    build_slot: BlockSlot,
    build_orientation: Cardinal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragVerb {
    Remove,
    Build,
    Cancel,
}

impl DragVerb {
    fn from_press(button: MouseButton, shift: bool) -> Option<Self> {
        match (button, shift) {
            (MouseButton::Left, false) => Some(DragVerb::Remove),
            (MouseButton::Left, true) => Some(DragVerb::Build),
            (MouseButton::Right, _) => Some(DragVerb::Cancel),
            _ => None,
        }
    }

    fn button(self) -> MouseButton {
        match self {
            DragVerb::Remove | DragVerb::Build => MouseButton::Left,
            DragVerb::Cancel => MouseButton::Right,
        }
    }

    fn kind(self, slot: BlockSlot, orientation: Cardinal) -> Option<PlanKind> {
        match self {
            DragVerb::Remove => Some(PlanKind::Remove),
            DragVerb::Build => Some(PlanKind::Build { slot, orientation }),
            DragVerb::Cancel => None,
        }
    }
}

/// Plan-mode input. Drag-paints a rectangle on the clicked face's plane;
/// release commits a single [`PlanEditBatch`]. A click that doesn't move
/// the cursor is a 1×1 rectangle (single-cell tag).
///
/// L = tag-for-remove. Shift+L = tag-for-build (rectangle cells shifted
/// outward by the face normal). R = cancel. Escape during a drag aborts
/// without committing. The drag plane is locked to the initial click's
/// face — no 3D-AABB variant in this phase; it would collide with the
/// Shift modifier already in use for the build verb.
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
    mut drag: ResMut<PlanDragState>,
    mut sender: Query<&mut MessageSender<PlanEditBatch>>,
) {
    // Mode switch or losing cursor lock mid-drag — drop the in-flight
    // rectangle so we don't accidentally commit one on the next click.
    if *mode != PlayerMode::Plan {
        drag.active = None;
        return;
    }
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        drag.active = None;
        return;
    }
    // Escape aborts an in-flight drag without committing.
    if keys.just_pressed(KeyCode::Escape) {
        drag.active = None;
        return;
    }

    let Ok((cam_t, fly, pose)) = cam.single() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();

    // Start of drag: capture anchor + verb if no drag is active and a
    // valid mouse button was just pressed against something tagged-able.
    if drag.active.is_none() {
        let pressed = if mouse.just_pressed(MouseButton::Left) {
            Some(MouseButton::Left)
        } else if mouse.just_pressed(MouseButton::Right) {
            Some(MouseButton::Right)
        } else {
            None
        };
        if let Some(button) = pressed {
            let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
            let Some(verb) = DragVerb::from_press(button, shift) else {
                return;
            };
            let visible_yaw = pose.yaw + fly.pending_dyaw;
            // Cancel and Remove anchor against the world. Build does
            // too — Shift+L on a wall, the rectangle expands across
            // the wall face, and each rect cell's outward neighbour
            // gets tagged-build.
            //
            // Remove sees through cells already tagged for removal so
            // the player can stack tags through a wall. Build keeps
            // current behaviour — its outward offset would otherwise
            // land on a still-solid Remove cell, which the server
            // rejects anyway. Cancel needs to land on the tagged cell
            // itself to clear it.
            let skip_plan_remove: Option<&Plans> = match verb {
                DragVerb::Remove => Some(&plans),
                DragVerb::Build | DragVerb::Cancel => None,
            };
            let Some(hit) = entity_aware_raycast(
                origin,
                dir,
                RAYCAST_REACH,
                &chunks,
                &chunk_map,
                &registry,
                skip_plan_remove,
            ) else {
                // Cancel falls back to the plan-aware raycast so a
                // floating-in-air Build tag can still be one-shot
                // cancelled with no rectangle.
                if verb == DragVerb::Cancel
                    && let Some(cell) = raycast_plans(origin, dir, RAYCAST_REACH, &plans)
                {
                    commit_batch(
                        &mut sender,
                        None,
                        vec![cell],
                    );
                }
                return;
            };
            drag.active = Some(ActiveDrag {
                verb,
                anchor: hit.cell,
                face_normal: hit.face_normal,
                second: hit.cell,
                build_slot: selected.current(&palette),
                build_orientation: placement_orientation(visible_yaw, rotation.0),
            });
        }
    }

    // During hold: re-project the cursor ray onto the anchor's face
    // plane to track the rectangle's second corner.
    if let Some(active) = drag.active.as_mut() {
        if let Some(projected) =
            project_to_face_plane(origin, dir, active.anchor, active.face_normal)
        {
            active.second = projected;
        }
    }

    // On release of the verb's button: commit the rectangle.
    let release = drag
        .active
        .as_ref()
        .map(|a| mouse.just_released(a.verb.button()))
        .unwrap_or(false);
    if release {
        let active = drag.active.take().unwrap();
        let cells = rect_cells_for_verb(active);
        if cells.is_empty() {
            return;
        }
        let kind = active.verb.kind(active.build_slot, active.build_orientation);
        commit_batch(&mut sender, kind, cells);
    }
}

/// Expand an in-flight drag into the list of cells it would tag.
/// `Remove` and `Cancel` tag the cells *on* the anchor's face plane;
/// `Build` tags the cell offset one step outward (face_normal direction)
/// from each rect cell.
fn rect_cells_for_verb(drag: ActiveDrag) -> Vec<IVec3> {
    let plane_cells = rect_cells_on_plane(drag.anchor, drag.second);
    let mut out: Vec<IVec3> = match drag.verb {
        DragVerb::Build => plane_cells
            .into_iter()
            .map(|c| c + drag.face_normal)
            .collect(),
        DragVerb::Remove | DragVerb::Cancel => plane_cells,
    };
    // Hard cap. A 64×64 face drag is exactly the cap; bigger drags get
    // truncated rather than split, since a multi-batch split would need
    // a contig-id to keep undo coherent in the future.
    if out.len() > PLAN_EDIT_BATCH_MAX {
        out.truncate(PLAN_EDIT_BATCH_MAX);
    }
    out
}

/// Iterate every cell in the axis-aligned rectangle whose two corners
/// are `a` and `b`. Both corners share their value on one axis (the
/// "depth" axis fixed by the face normal); the other two range freely.
fn rect_cells_on_plane(a: IVec3, b: IVec3) -> Vec<IVec3> {
    let min = IVec3::new(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z));
    let max = IVec3::new(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z));
    let mut cells = Vec::new();
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                cells.push(IVec3::new(x, y, z));
            }
        }
    }
    cells
}

fn commit_batch(
    sender: &mut Query<&mut MessageSender<PlanEditBatch>>,
    kind: Option<PlanKind>,
    cells: Vec<IVec3>,
) {
    let Ok(mut sender) = sender.single_mut() else {
        return;
    };
    sender.send::<WorldChannel>(PlanEditBatch { kind, cells });
}

/// Project the ray (`origin`, `dir`) onto the plane through
/// `anchor_cell`'s face (the face whose outward normal is `face_normal`).
/// Returns the integer cell whose in-plane axes contain the projected
/// point and whose F-axis coordinate is `anchor_cell`'s F-axis value —
/// i.e. the cell in the *same row* as the anchor along the face plane.
///
/// `None` if the ray is parallel to the plane (dot(dir, normal) ≈ 0)
/// or if the intersection lies behind the camera.
fn project_to_face_plane(
    origin: Vec3,
    dir: Vec3,
    anchor_cell: IVec3,
    face_normal: IVec3,
) -> Option<IVec3> {
    let n = face_normal.as_vec3();
    let denom = dir.dot(n);
    if denom.abs() < 1e-6 {
        return None;
    }
    // Plane point: the centre of the anchor cell's outward face.
    let plane_point = anchor_cell.as_vec3() + Vec3::splat(0.5) + n * 0.5;
    let t = (plane_point - origin).dot(n) / denom;
    if t < 0.0 {
        return None;
    }
    let p = origin + dir * t;
    let mut cell = IVec3::new(p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
    // Lock the F-axis component to the anchor row so a slightly off-
    // plane floor() (numerical noise) doesn't pop the rect to the
    // wrong depth.
    if face_normal.x != 0 {
        cell.x = anchor_cell.x;
    } else if face_normal.y != 0 {
        cell.y = anchor_cell.y;
    } else {
        cell.z = anchor_cell.z;
    }
    Some(cell)
}

/// Render an in-flight drag as a translucent gizmo box covering the
/// rectangle's footprint. Colour matches the verb (red Remove / green
/// Build / yellow Cancel) so the player can read the action at a glance.
fn draw_drag_preview(drag: Res<PlanDragState>, mut gizmos: Gizmos) {
    let Some(active) = drag.active else {
        return;
    };
    let cells = rect_cells_for_verb(active);
    if cells.is_empty() {
        return;
    }
    let mut min = cells[0];
    let mut max = cells[0];
    for c in cells.iter().skip(1) {
        min = IVec3::new(min.x.min(c.x), min.y.min(c.y), min.z.min(c.z));
        max = IVec3::new(max.x.max(c.x), max.y.max(c.y), max.z.max(c.z));
    }
    let centre = (min.as_vec3() + max.as_vec3() + Vec3::ONE) * 0.5;
    let scale = (max - min).as_vec3() + Vec3::ONE;
    let colour = match active.verb {
        DragVerb::Remove => Color::srgba(1.0, 0.3, 0.3, 1.0),
        DragVerb::Build => Color::srgba(0.3, 1.0, 0.4, 1.0),
        DragVerb::Cancel => Color::srgba(1.0, 0.9, 0.2, 1.0),
    };
    gizmos.cube(
        Transform::from_translation(centre).with_scale(scale),
        colour,
    );
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

/// Server: bulk version of `receive_plan_edits`. Validates each cell
/// against the same per-kind rules, applies the surviving cells to
/// `Plans`, and broadcasts a new batch containing only the surviving
/// cells so clients see exactly what the server accepted.
fn receive_plan_edit_batches(
    mut receivers: Query<&mut MessageReceiver<PlanEditBatch>>,
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
        let batches: Vec<PlanEditBatch> = receiver.receive().collect();
        for batch in batches {
            let mut accepted: Vec<IVec3> = Vec::with_capacity(batch.cells.len());
            for cell in batch.cells {
                match batch.kind {
                    Some(PlanKind::Remove) => {
                        if !cell_is_solid(cell, &chunks, &chunk_map) {
                            continue;
                        }
                        plans.set(cell, PlanKind::Remove);
                    }
                    Some(kind @ PlanKind::Build { .. }) => {
                        if cell_is_solid(cell, &chunks, &chunk_map) {
                            continue;
                        }
                        plans.set(cell, kind);
                    }
                    None => {
                        plans.clear(cell);
                    }
                }
                accepted.push(cell);
            }
            if accepted.is_empty() {
                continue;
            }
            let reply = PlanEditBatch {
                kind: batch.kind,
                cells: accepted,
            };
            if let Err(err) = broadcast.send::<PlanEditBatch, WorldChannel>(
                &reply,
                server,
                &NetworkTarget::All,
            ) {
                warn!("PlanEditBatch broadcast failed: {err}");
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

/// Client: apply a broadcast batch to the local mirror.
fn receive_plan_edit_batch_broadcasts(
    mut receivers: Query<&mut MessageReceiver<PlanEditBatch>>,
    mut plans: ResMut<Plans>,
) {
    for mut receiver in receivers.iter_mut() {
        for batch in receiver.receive() {
            for cell in batch.cells {
                match batch.kind {
                    Some(kind) => plans.set(cell, kind),
                    None => plans.clear(cell),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_to_face_plane_top_face_under_cursor() {
        // Anchor at (5,3,7), top face. Camera looking straight down
        // from above the anchor — projected cell is the anchor itself.
        let anchor = IVec3::new(5, 3, 7);
        let face = IVec3::new(0, 1, 0);
        let origin = Vec3::new(5.5, 10.0, 7.5);
        let dir = Vec3::new(0.0, -1.0, 0.0);
        assert_eq!(
            project_to_face_plane(origin, dir, anchor, face),
            Some(anchor)
        );
    }

    #[test]
    fn project_to_face_plane_top_face_offset_cursor() {
        // Same anchor, cursor offset 3 cells along +X and 2 along +Z.
        // F-axis (Y) stays locked to anchor's row.
        let anchor = IVec3::new(5, 3, 7);
        let face = IVec3::new(0, 1, 0);
        let origin = Vec3::new(8.5, 10.0, 9.5);
        let dir = Vec3::new(0.0, -1.0, 0.0);
        assert_eq!(
            project_to_face_plane(origin, dir, anchor, face),
            Some(IVec3::new(8, 3, 9))
        );
    }

    #[test]
    fn project_to_face_plane_ray_parallel_returns_none() {
        // Anchor at (5,3,7), top face. Ray travels along +X — never
        // intersects the y=4 plane.
        let anchor = IVec3::new(5, 3, 7);
        let face = IVec3::new(0, 1, 0);
        let origin = Vec3::new(0.0, 4.0, 7.5);
        let dir = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(project_to_face_plane(origin, dir, anchor, face), None);
    }

    #[test]
    fn project_to_face_plane_ray_behind_returns_none() {
        // Ray points up but the plane is below the camera: t < 0.
        let anchor = IVec3::new(5, 3, 7);
        let face = IVec3::new(0, 1, 0);
        let origin = Vec3::new(5.5, 6.0, 7.5);
        let dir = Vec3::new(0.0, 1.0, 0.0);
        assert_eq!(project_to_face_plane(origin, dir, anchor, face), None);
    }

    #[test]
    fn project_to_face_plane_side_face() {
        // Anchor at (5,3,7), east face (+X). Plane at x=6. Camera east
        // of the wall looking back: projection lands on anchor's row.
        let anchor = IVec3::new(5, 3, 7);
        let face = IVec3::new(1, 0, 0);
        let origin = Vec3::new(10.0, 5.5, 8.5);
        let dir = Vec3::new(-1.0, 0.0, 0.0);
        assert_eq!(
            project_to_face_plane(origin, dir, anchor, face),
            Some(IVec3::new(5, 5, 8))
        );
    }

    #[test]
    fn rect_cells_on_plane_inclusive_range() {
        let cells = rect_cells_on_plane(IVec3::new(0, 0, 0), IVec3::new(2, 0, 1));
        // 3 × 1 × 2 = 6 cells.
        assert_eq!(cells.len(), 6);
        assert!(cells.contains(&IVec3::new(0, 0, 0)));
        assert!(cells.contains(&IVec3::new(2, 0, 1)));
        assert!(cells.contains(&IVec3::new(1, 0, 1)));
    }
}
