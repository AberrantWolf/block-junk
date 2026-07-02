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
use block_junk_mod_api::blocks::Cardinal;
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::camera::FlyCam;
use crate::client::{
    PlaceablePalette, PlacementRotation, SelectedBlock, entity_aware_raycast, placement_orientation,
};
use crate::menu::AppState;
use crate::player_mode::PlayerMode;
use crate::protocol::{
    ActionRejected, Avatar, AvatarPose, GameSet, MaterialEntry, PLAN_EDIT_BATCH_MAX, PLAN_REACH,
    PlanEdit, PlanEditBatch, PlanFullSync, PlanKind, PlanState, RejectReason, WorldChannel,
};
use crate::server::{send_rejection, within_reach};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap, world_to_chunk};

#[derive(Resource, Default, Debug)]
pub struct Plans {
    map: HashMap<IVec3, PlanState>,
}

impl Plans {
    /// Full state at `cell`, including materials progress. Use this
    /// when the caller needs the kind, the materials, or
    /// [`PlanState::is_satisfied`].
    pub fn get(&self, cell: IVec3) -> Option<&PlanState> {
        self.map.get(&cell)
    }

    /// Just the kind (the original pre-Phase-3 contract). Useful for
    /// callers that only branch Remove vs Build and don't care about
    /// materials. Copies so the caller doesn't borrow the map.
    pub fn kind(&self, cell: IVec3) -> Option<PlanKind> {
        self.map.get(&cell).map(|s| s.kind)
    }

    pub fn set(&mut self, cell: IVec3, state: PlanState) {
        self.map.insert(cell, state);
    }

    pub fn clear(&mut self, cell: IVec3) {
        self.map.remove(&cell);
    }

    pub fn replace_all(&mut self, entries: impl IntoIterator<Item = (IVec3, PlanState)>) {
        self.map.clear();
        for (cell, state) in entries {
            self.map.insert(cell, state);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &PlanState)> {
        self.map.iter()
    }

    pub fn snapshot(&self) -> Vec<(IVec3, PlanState)> {
        self.map.iter().map(|(c, s)| (*c, s.clone())).collect()
    }

    /// Deposit `count` units of `item` toward this plan's materials.
    /// Returns how many units were actually accepted (capped at the
    /// remaining need). 0 ⇒ plan doesn't need this item, or already
    /// full, or no plan at this cell.
    pub fn deposit(&mut self, cell: IVec3, item: crate::items::ItemSlot, count: u32) -> u32 {
        let Some(state) = self.map.get_mut(&cell) else {
            return 0;
        };
        let Some(entry) = state.materials.iter_mut().find(|m| m.item == item) else {
            return 0;
        };
        let accepted = count.min(entry.needed.saturating_sub(entry.present));
        entry.present += accepted;
        accepted
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
            (receive_plan_edits, receive_plan_edit_batches).in_set(GameSet::Simulation),
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
    /// drag doesn't shift the build payload partway through. Only
    /// read for `DragVerb::Build`.
    build_slot: BlockSlot,
    build_orientation: Cardinal,
}

/// Plan-mode drag verb. Under the L=destroy / R=create Minecraft-style
/// scheme:
/// - `LeftSweep`: L-drag against a solid block. At commit time, each
///   plane cell evaluates independently — solid+untagged cells get a
///   Remove tag, any tagged cell (Build or Remove) gets cleared.
///   Empty+untagged cells become no-ops (server rejects them).
/// - `Build`: R-drag against a solid block with a hotbar block selected.
///   Rectangle cells shift outward by `face_normal` so the tags land in
///   the empty space adjacent to the face.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DragVerb {
    LeftSweep,
    Build,
}

impl DragVerb {
    fn button(self) -> MouseButton {
        match self {
            DragVerb::LeftSweep => MouseButton::Left,
            DragVerb::Build => MouseButton::Right,
        }
    }
}

/// Plan-mode input under the L=destroy / R=create scheme.
///
/// **L-click (destructive sweep)**: drag-paints against a solid block;
/// at commit, each plane cell evaluates independently — solid+untagged
/// becomes a Remove tag, any tagged cell (Build or Remove) gets cleared.
/// Single-clicking directly on a Build tag floating in empty space
/// (caught by the plan-aware raycast) commits a one-cell un-tag without
/// starting a drag.
///
/// **R-click (place Build)**: drag-paints against a solid block with a
/// hotbar block selected; rectangle cells shift outward by the face
/// normal so the tags land in the empty space adjacent. Inert if the
/// hotbar has no real block, or if the cursor isn't on a solid block.
///
/// Escape during a drag aborts without committing.
#[allow(
    clippy::too_many_arguments,
    reason = "input system spans many subsystems"
)]
fn plan_mode_input(
    mouse: Res<ButtonInput<MouseButton>>,
    captures: Res<crate::ui_capture::UiCaptures>,
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
    // Mode switch or any overlay open mid-drag — drop the in-flight
    // rectangle so we don't accidentally commit one on the next click.
    if *mode != PlayerMode::Plan {
        drag.active = None;
        return;
    }
    if captures.is_captured() {
        drag.active = None;
        return;
    }
    // Escape aborting an in-flight drag lives in the central
    // `ui_capture::handle_escape` dispatcher (drag cancel is the
    // topmost Esc consumer) — reading the key here as well would
    // double-fire and also open the pause menu on the same press.

    let Ok((cam_t, fly, pose)) = cam.single() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();

    // Start of drag.
    if drag.active.is_none() {
        let l_pressed = mouse.just_pressed(MouseButton::Left);
        let r_pressed = mouse.just_pressed(MouseButton::Right);
        if l_pressed {
            handle_left_press(
                origin,
                dir,
                &chunks,
                &chunk_map,
                &registry,
                &plans,
                &mut drag,
                &mut sender,
            );
        } else if r_pressed {
            handle_right_press(
                origin, dir, fly, pose, &chunks, &chunk_map, &registry, &selected, &palette,
                &rotation, &mut drag,
            );
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
        commit_drag(active, &plans, &mut sender);
    }
}

/// L-press handling. First tries a plan-aware raycast to catch Build
/// tags floating in empty space (a single-cell un-tag without a drag).
/// Otherwise anchors a destructive-sweep drag against a solid block.
#[allow(clippy::too_many_arguments)]
fn handle_left_press(
    origin: Vec3,
    dir: Vec3,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    plans: &Plans,
    drag: &mut PlanDragState,
    sender: &mut Query<&mut MessageSender<PlanEditBatch>>,
) {
    let plan_hit = raycast_plans(origin, dir, PLAN_REACH, plans);
    // No skip_plan_remove — under the new scheme we want L on a Remove
    // tag to un-tag it, so the ray must stop AT the tagged cell.
    let world_hit =
        entity_aware_raycast(origin, dir, PLAN_REACH, chunks, chunk_map, registry, None);
    let world_dist = world_hit.as_ref().map(|h| cell_centre_dist(h.cell, origin));
    let plan_dist = plan_hit.as_ref().map(|(d, _)| *d);
    // If a plan tag (typically a Build floating in empty space) is
    // strictly closer than the world hit, single-cell un-tag and
    // don't start a drag — there's no meaningful face plane for an
    // empty-cell anchor.
    let prefer_plan = match (plan_dist, world_dist) {
        (Some(p), Some(w)) => p < w,
        (Some(_), None) => true,
        _ => false,
    };
    if prefer_plan {
        let (_, cell) = plan_hit.unwrap();
        commit_batch(sender, None, vec![cell]);
        return;
    }
    let Some(hit) = world_hit else { return };
    drag.active = Some(ActiveDrag {
        verb: DragVerb::LeftSweep,
        anchor: hit.cell,
        face_normal: hit.face_normal,
        second: hit.cell,
        // LeftSweep never reads build_slot/orientation at commit.
        build_slot: BlockSlot::EMPTY,
        build_orientation: Cardinal::default(),
    });
}

/// R-press handling. Inert if no block is selected in the hotbar or if
/// the cursor isn't on a solid block. Otherwise anchors a Build drag.
#[allow(clippy::too_many_arguments)]
fn handle_right_press(
    origin: Vec3,
    dir: Vec3,
    fly: &FlyCam,
    pose: &AvatarPose,
    chunks: &Query<(&Chunk, &ChunkEntities)>,
    chunk_map: &ChunkMap,
    registry: &BlockRegistry,
    selected: &SelectedBlock,
    palette: &PlaceablePalette,
    rotation: &PlacementRotation,
    drag: &mut PlanDragState,
) {
    let Some(build_slot) = selected.current_block(palette) else {
        return;
    };
    let Some(hit) =
        entity_aware_raycast(origin, dir, PLAN_REACH, chunks, chunk_map, registry, None)
    else {
        return;
    };
    let visible_yaw = pose.yaw + fly.pending_dyaw;
    drag.active = Some(ActiveDrag {
        verb: DragVerb::Build,
        anchor: hit.cell,
        face_normal: hit.face_normal,
        second: hit.cell,
        build_slot,
        build_orientation: placement_orientation(visible_yaw, rotation.0),
    });
}

/// Compute the cell lists for each batch the release will commit. For
/// `LeftSweep`, splits plane cells into untag (any existing tag) vs
/// remove (no existing tag — server validates solid). For `Build`,
/// shifts plane cells outward by the face normal.
fn commit_drag(
    drag: ActiveDrag,
    plans: &Plans,
    sender: &mut Query<&mut MessageSender<PlanEditBatch>>,
) {
    let plane_cells = rect_cells_on_plane(drag.anchor, drag.second);
    if plane_cells.is_empty() {
        return;
    }
    match drag.verb {
        DragVerb::LeftSweep => {
            let mut untag_cells: Vec<IVec3> = Vec::new();
            let mut remove_cells: Vec<IVec3> = Vec::new();
            for cell in plane_cells {
                if plans.get(cell).is_some() {
                    untag_cells.push(cell);
                } else {
                    // Server rejects Remove tags on empty cells, so
                    // we don't need to check `cell_is_solid` here.
                    remove_cells.push(cell);
                }
            }
            cap_to_batch_max(&mut untag_cells);
            cap_to_batch_max(&mut remove_cells);
            if !untag_cells.is_empty() {
                commit_batch(sender, None, untag_cells);
            }
            if !remove_cells.is_empty() {
                commit_batch(sender, Some(PlanKind::Remove), remove_cells);
            }
        }
        DragVerb::Build => {
            let mut cells: Vec<IVec3> = plane_cells
                .into_iter()
                .map(|c| c + drag.face_normal)
                .collect();
            cap_to_batch_max(&mut cells);
            commit_batch(
                sender,
                Some(PlanKind::Build {
                    slot: drag.build_slot,
                    orientation: drag.build_orientation,
                }),
                cells,
            );
        }
    }
}

fn cap_to_batch_max(cells: &mut Vec<IVec3>) {
    // Hard cap. A 64×64 face drag is exactly the cap; bigger drags get
    // truncated rather than split, since a multi-batch split would need
    // a contig-id to keep undo coherent in the future.
    if cells.len() > PLAN_EDIT_BATCH_MAX {
        cells.truncate(PLAN_EDIT_BATCH_MAX);
    }
}

/// Expand an in-flight drag into the cells the preview gizmo would
/// cover. Build cells live one step outward from the anchor's face
/// plane; LeftSweep cells stay on it. Same cap as [`commit_drag`] so
/// the preview matches what will actually commit.
fn rect_cells_for_preview(drag: ActiveDrag) -> Vec<IVec3> {
    let plane_cells = rect_cells_on_plane(drag.anchor, drag.second);
    let mut out: Vec<IVec3> = match drag.verb {
        DragVerb::Build => plane_cells
            .into_iter()
            .map(|c| c + drag.face_normal)
            .collect(),
        DragVerb::LeftSweep => plane_cells,
    };
    cap_to_batch_max(&mut out);
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
    // Client request: materials is server-set. Empty on the wire here.
    sender.send::<WorldChannel>(PlanEditBatch {
        kind,
        cells,
        materials: Vec::new(),
    });
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
/// rectangle's footprint. LeftSweep previews red (the dominant verb is
/// tag-for-remove on a plain wall); Build previews green. LeftSweep on
/// a rectangle of already-tagged cells *also* paints red — the box just
/// advertises "destructive sweep is committing here," not the per-cell
/// outcome, which would need per-cell glyphs and wouldn't help.
fn draw_drag_preview(drag: Res<PlanDragState>, mut gizmos: Gizmos) {
    let Some(active) = drag.active else {
        return;
    };
    let cells = rect_cells_for_preview(active);
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
        DragVerb::LeftSweep => Color::srgba(1.0, 0.3, 0.3, 1.0),
        DragVerb::Build => Color::srgba(0.3, 1.0, 0.4, 1.0),
    };
    gizmos.cube(
        Transform::from_translation(centre).with_scale(scale),
        colour,
    );
}

/// Closest-hit raycast against the unit-cube AABB of every tagged cell.
/// O(n) per call, fine for sparse plans (a hundred tags is a long way
/// from a perf concern). Returns `(distance, cell)` for the nearest
/// hit within `max_distance`, or `None` if nothing intersects.
///
/// Uses the slab method with `1.0 / dir` — Vec3 division by a zero
/// component produces ±inf, which the min/max chain handles correctly
/// as long as the origin isn't exactly on an integer-aligned plane
/// (then 0 × inf yields NaN). The camera's eye position is fractional
/// in practice, so this is safe.
///
/// Crate-visible so Normal-mode self-work in `client.rs` can pick a
/// Build-plan-in-empty-space target the world raycast misses.
pub(crate) fn raycast_plans(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    plans: &Plans,
) -> Option<(f32, IVec3)> {
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
    best
}

/// Distance from `origin` to the centre of `cell`. Used to compare a
/// world-raycast hit against a plan-raycast hit on the same axis.
fn cell_centre_dist(cell: IVec3, origin: Vec3) -> f32 {
    (cell.as_vec3() + Vec3::splat(0.5) - origin).length()
}

/// Compute the [`MaterialEntry`] list for a Build plan from the
/// target block's [`BlockDef::materials`](block_junk_mod_api::blocks::BlockDef::materials).
/// `present` starts at 0; deposits fill it. Returns an empty vec for
/// Remove plans or when the block declares no materials (Phase 0
/// blocks with empty materials — free to build).
fn build_initial_materials(
    kind: PlanKind,
    block_registry: &BlockRegistry,
    item_registry: &crate::items::ItemRegistry,
) -> Vec<MaterialEntry> {
    let PlanKind::Build { slot, .. } = kind else {
        return Vec::new();
    };
    let def = block_registry.def(slot);
    let mut out = Vec::with_capacity(def.materials.len());
    for mat in &def.materials {
        if let Some(item_slot) = item_registry.slot_of(&mat.item) {
            out.push(MaterialEntry {
                item: item_slot,
                needed: mat.count,
                present: 0,
            });
        }
    }
    out
}

/// Server: ingest client requests, validate against live world state,
/// apply to the authoritative `Plans`, broadcast the canonical applied
/// edit. Reject (silently) edits that don't make sense against the
/// world (tag-remove on empty, tag-build on solid).
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler + reach gate + rejection reply"
)]
fn receive_plan_edits(
    mut receivers: Query<(Entity, &mut MessageReceiver<PlanEdit>)>,
    mut plans: ResMut<Plans>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    item_registry: Res<crate::items::ItemRegistry>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&AvatarPose, With<Avatar>>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let edits: Vec<PlanEdit> = receiver.receive().collect();
        for edit in edits {
            // Designation reach: deliberately looser than INTERACT_REACH
            // (tags are NPC orders, not direct mutations) but bounded so
            // a client can't tag cells far beyond anything it can see.
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            if !within_reach(pose, edit.cell.as_vec3() + Vec3::splat(0.5), PLAN_REACH) {
                send_rejection(
                    &mut rejections,
                    connection,
                    edit.cell,
                    RejectReason::OutOfReach,
                );
                continue;
            }
            let (accepted_state, materials_for_broadcast) = match edit.kind {
                Some(PlanKind::Remove) => {
                    if !cell_is_solid(edit.cell, &chunks, &chunk_map) {
                        continue;
                    }
                    (
                        Some(PlanState::new(PlanKind::Remove, Vec::new())),
                        Vec::new(),
                    )
                }
                Some(kind @ PlanKind::Build { .. }) => {
                    if cell_is_solid(edit.cell, &chunks, &chunk_map) {
                        continue;
                    }
                    let materials = build_initial_materials(kind, &block_registry, &item_registry);
                    (Some(PlanState::new(kind, materials.clone())), materials)
                }
                None => {
                    plans.clear(edit.cell);
                    (None, Vec::new())
                }
            };
            if let Some(state) = accepted_state {
                plans.set(edit.cell, state);
            }
            let reply = PlanEdit {
                cell: edit.cell,
                kind: edit.kind,
                materials: materials_for_broadcast,
            };
            if let Err(err) =
                broadcast.send::<PlanEdit, WorldChannel>(&reply, server, &NetworkTarget::All)
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
#[allow(clippy::too_many_arguments)]
fn receive_plan_edit_batches(
    mut receivers: Query<(Entity, &mut MessageReceiver<PlanEditBatch>)>,
    mut plans: ResMut<Plans>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    item_registry: Res<crate::items::ItemRegistry>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&AvatarPose, With<Avatar>>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let batches: Vec<PlanEditBatch> = receiver.receive().collect();
        for mut batch in batches {
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            // Server-of-record batch cap. A well-behaved client
            // truncates before sending; anything past the cap here is
            // a bug or a hostile peer, so drop the excess loudly
            // rather than relaying an unbounded vec to every client.
            if batch.cells.len() > PLAN_EDIT_BATCH_MAX {
                warn!(
                    got = batch.cells.len(),
                    cap = PLAN_EDIT_BATCH_MAX,
                    "plan batch over server cap; truncating",
                );
                batch.cells.truncate(PLAN_EDIT_BATCH_MAX);
            }
            // Compute the shared materials list once per batch — every
            // Build cell in the batch shares the same `kind` and
            // therefore the same materials_needed.
            let shared_materials = match batch.kind {
                Some(kind @ PlanKind::Build { .. }) => {
                    build_initial_materials(kind, &block_registry, &item_registry)
                }
                _ => Vec::new(),
            };
            let mut accepted: Vec<IVec3> = Vec::with_capacity(batch.cells.len());
            let mut first_out_of_reach: Option<IVec3> = None;
            for cell in batch.cells {
                // Same designation reach as single edits. A drag can
                // legitimately straddle the boundary — keep the cells
                // in range, toast once for the dropped remainder.
                if !within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), PLAN_REACH) {
                    first_out_of_reach.get_or_insert(cell);
                    continue;
                }
                match batch.kind {
                    Some(PlanKind::Remove) => {
                        if !cell_is_solid(cell, &chunks, &chunk_map) {
                            continue;
                        }
                        plans.set(cell, PlanState::new(PlanKind::Remove, Vec::new()));
                    }
                    Some(kind @ PlanKind::Build { .. }) => {
                        if cell_is_solid(cell, &chunks, &chunk_map) {
                            continue;
                        }
                        plans.set(cell, PlanState::new(kind, shared_materials.clone()));
                    }
                    None => {
                        plans.clear(cell);
                    }
                }
                accepted.push(cell);
            }
            if let Some(cell) = first_out_of_reach {
                send_rejection(&mut rejections, connection, cell, RejectReason::OutOfReach);
            }
            if accepted.is_empty() {
                continue;
            }
            let reply = PlanEditBatch {
                kind: batch.kind,
                cells: accepted,
                materials: shared_materials,
            };
            if let Err(err) =
                broadcast.send::<PlanEditBatch, WorldChannel>(&reply, server, &NetworkTarget::All)
            {
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
/// already done the validation + materials computation; we trust
/// both fields directly.
fn receive_plan_edit_broadcasts(
    mut receivers: Query<&mut MessageReceiver<PlanEdit>>,
    mut plans: ResMut<Plans>,
) {
    for mut receiver in receivers.iter_mut() {
        for edit in receiver.receive() {
            match edit.kind {
                Some(kind) => plans.set(edit.cell, PlanState::new(kind, edit.materials)),
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
                    Some(kind) => {
                        plans.set(cell, PlanState::new(kind, batch.materials.clone()));
                    }
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
