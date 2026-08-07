//! Storage-zone designation — S1 of the storage arc.
//!
//! `StorageZones` is the canonical set of cells the player has marked
//! as storage. Like `Plans`, it lives as a server-authoritative
//! `Resource` on the server App and as a passive mirror on each client,
//! updated via [`StorageEditBatch`] broadcasts and a one-shot
//! [`StorageFullSync`] on connect. Client mutation is a wire request;
//! the server validates (reach, batch cap, floor-cell shape) and
//! broadcasts what it accepted.
//!
//! A zone cell is the *air* cell items occupy — solid floor below,
//! non-solid at the cell — matching where a pile or container will
//! physically sit. Painting happens in `PlayerMode::Storage`: R-drag
//! on the ground marks, L-drag clears. Both anchor on an upward face
//! only; there is no such thing as wall-mounted storage.
//!
//! S1 is designation + replication + rendering. S2 (collective piles)
//! adds the NPC behavior that reads this set.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use lightyear::prelude::*;

use crate::blocks::BlockRegistry;
use crate::client::entity_aware_raycast;
use crate::menu::AppState;
use crate::plans::{cell_is_solid, project_to_face_plane, rect_cells_on_plane};
use crate::player_mode::PlayerMode;
use crate::protocol::{
    ActionRejected, Avatar, AvatarPose, GameReady, GameSet, PLAN_EDIT_BATCH_MAX, PLAN_REACH,
    RejectReason, StateSyncChannel, StorageEditBatch, StorageFullSync,
};
use crate::server::{RequestClass, ValidatedRequestContext, send_rejection, within_reach};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

/// The set of designated storage cells. Sparse and unordered; zone
/// "shape" is emergent (any painted cell counts, contiguity doesn't
/// matter to the engine).
#[derive(Resource, Default, Debug)]
pub struct StorageZones {
    cells: HashSet<IVec3>,
}

impl StorageZones {
    /// Returns true when the cell was newly inserted (state changed).
    pub fn insert(&mut self, cell: IVec3) -> bool {
        self.cells.insert(cell)
    }

    /// Returns true when the cell was present (state changed).
    pub fn remove(&mut self, cell: IVec3) -> bool {
        self.cells.remove(&cell)
    }

    pub fn iter(&self) -> impl Iterator<Item = &IVec3> {
        self.cells.iter()
    }

    pub fn snapshot(&self) -> Vec<IVec3> {
        self.cells.iter().copied().collect()
    }

    pub fn replace_all(&mut self, cells: impl IntoIterator<Item = IVec3>) {
        self.cells.clear();
        self.cells.extend(cells);
    }
}

pub struct StorageServerPlugin;
pub struct StorageClientPlugin;

impl Plugin for StorageServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StorageZones>();
        app.add_observer(send_storage_full_sync_on_connect);
        app.add_systems(
            Update,
            receive_storage_edit_batches.in_set(GameSet::Simulation),
        );
    }
}

impl Plugin for StorageClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StorageZones>();
        app.init_resource::<StorageDragState>();
        // Full-sync before per-batch receive: same-tick ordering rule
        // as plans (full sync must not clobber a delta applied first).
        app.add_systems(
            Update,
            (receive_storage_full_sync, receive_storage_batch_broadcasts)
                .chain()
                .in_set(GameSet::Simulation)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            Update,
            storage_mode_input
                .in_set(GameSet::Input)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            Update,
            (draw_storage_zones, draw_storage_drag_preview)
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// In-flight Storage-mode drag. Anchored on an upward face; the
/// rectangle tracks on that horizontal plane and commits the cells one
/// step above (the air cells items will sit in).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveStorageDrag {
    /// `true` = R-drag (mark storage), `false` = L-drag (clear).
    add: bool,
    anchor: IVec3,
    second: IVec3,
}

#[derive(Resource, Default)]
pub(crate) struct StorageDragState {
    pub(crate) active: Option<ActiveStorageDrag>,
}

/// Storage-mode input: R-drag paints, L-drag erases. Mirrors
/// `plan_mode_input`'s drag lifecycle (anchor on press, re-project the
/// second corner while held, commit the rectangle on release; Escape
/// cancel lives in `ui_capture::handle_escape`).
///
/// Only upward faces anchor a drag — storage is a floor concept. A
/// press on a wall or ceiling face does nothing, which doubles as the
/// "you're aiming at the wrong thing" feedback.
#[allow(
    clippy::too_many_arguments,
    reason = "input system spans many subsystems"
)]
fn storage_mode_input(
    mouse: Res<ButtonInput<MouseButton>>,
    captures: Res<crate::ui_capture::UiCaptures>,
    mode: Res<PlayerMode>,
    cam: Query<&GlobalTransform, With<crate::camera::FlyCam>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    mut drag: ResMut<StorageDragState>,
    mut sender: Query<&mut MessageSender<StorageEditBatch>>,
) {
    if *mode != PlayerMode::Storage {
        drag.active = None;
        return;
    }
    if captures.is_captured() {
        drag.active = None;
        return;
    }
    let Ok(cam_t) = cam.single() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();

    if drag.active.is_none() {
        let l_pressed = mouse.just_pressed(MouseButton::Left);
        let r_pressed = mouse.just_pressed(MouseButton::Right);
        if l_pressed || r_pressed {
            let hit = entity_aware_raycast(
                origin, dir, PLAN_REACH, &chunks, &chunk_map, &registry, None,
            );
            if let Some(hit) = hit
                && hit.face_normal == IVec3::Y
            {
                drag.active = Some(ActiveStorageDrag {
                    add: r_pressed,
                    anchor: hit.cell,
                    second: hit.cell,
                });
            }
        }
    }

    if let Some(active) = drag.active.as_mut()
        && let Some(projected) = project_to_face_plane(origin, dir, active.anchor, IVec3::Y)
    {
        active.second = projected;
    }

    let release = drag
        .active
        .as_ref()
        .map(|a| {
            mouse.just_released(if a.add {
                MouseButton::Right
            } else {
                MouseButton::Left
            })
        })
        .unwrap_or(false);
    if release {
        let active = drag.active.take().unwrap();
        let mut cells: Vec<IVec3> = rect_cells_on_plane(active.anchor, active.second)
            .into_iter()
            .map(|c| c + IVec3::Y)
            .collect();
        if cells.len() > PLAN_EDIT_BATCH_MAX {
            cells.truncate(PLAN_EDIT_BATCH_MAX);
        }
        if cells.is_empty() {
            return;
        }
        if let Ok(mut sender) = sender.single_mut() {
            sender.send::<StateSyncChannel>(StorageEditBatch {
                add: active.add,
                cells: crate::protocol::BoundedVec::new(cells)
                    .expect("client storage batches are truncated before send"),
            });
        }
    }
}

/// Server: validate a client's zone-edit batch and broadcast what
/// stuck. Adds require the floor-cell shape (air at the cell, solid
/// below) so zones can't be painted mid-air or inside walls; clears
/// have no shape requirement (the floor may have been mined since).
/// Only state-changing cells broadcast — re-painting an existing zone
/// is a no-op, not traffic.
#[allow(
    clippy::too_many_arguments,
    reason = "wire handler + reach gate + rejection reply"
)]
fn receive_storage_edit_batches(
    mut receivers: Query<
        (Entity, &mut MessageReceiver<StorageEditBatch>),
        With<crate::protocol::GameReady>,
    >,
    mut zones: ResMut<StorageZones>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    avatars: Res<crate::server::ClientAvatars>,
    poses: Query<&AvatarPose, With<Avatar>>,
    mut rejections: Query<&mut MessageSender<ActionRejected>>,
    mut broadcast: ServerMultiMessageSender,
    servers: Query<&Server>,
    mut validation: ValidatedRequestContext,
) {
    let Ok(server) = servers.single() else {
        return;
    };
    for (connection, mut receiver) in receivers.iter_mut() {
        let batches: Vec<StorageEditBatch> = receiver.receive().collect();
        for batch in batches {
            if validation
                .authorize(connection, RequestClass::BatchCells(batch.cells.len()))
                .is_none()
            {
                continue;
            }
            let Some(&avatar) = avatars.0.get(&connection) else {
                continue;
            };
            let Ok(pose) = poses.get(avatar) else {
                continue;
            };
            let mut accepted: Vec<IVec3> = Vec::with_capacity(batch.cells.len());
            let mut first_out_of_reach: Option<IVec3> = None;
            for cell in batch.cells {
                if !within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), PLAN_REACH) {
                    first_out_of_reach.get_or_insert(cell);
                    continue;
                }
                let changed = if batch.add {
                    let is_floor_cell = !cell_is_solid(cell, &chunks, &chunk_map)
                        && cell_is_solid(cell - IVec3::Y, &chunks, &chunk_map);
                    is_floor_cell && zones.insert(cell)
                } else {
                    zones.remove(cell)
                };
                if changed {
                    accepted.push(cell);
                }
            }
            if let Some(cell) = first_out_of_reach {
                send_rejection(&mut rejections, connection, cell, RejectReason::OutOfReach);
            }
            if accepted.is_empty() {
                continue;
            }
            let reply = StorageEditBatch {
                add: batch.add,
                cells: crate::protocol::BoundedVec::new(accepted)
                    .expect("accepted cells are a subset of a bounded request"),
            };
            let target = validation.target_for_cells(reply.cells.iter().copied());
            if let Err(err) =
                broadcast.send::<StorageEditBatch, StateSyncChannel>(&reply, server, &target)
            {
                warn!("StorageEditBatch broadcast failed: {err}");
            }
        }
    }
}

/// Server: push the whole zone set to a fresh connection. One message
/// — the client applies it as a replace, so it must not be split.
fn send_storage_full_sync_on_connect(
    trigger: On<Add, GameReady>,
    zones: Res<StorageZones>,
    mut senders: Query<&mut MessageSender<StorageFullSync>>,
) {
    let connection = trigger.entity;
    let Ok(mut sender) = senders.get_mut(connection) else {
        return;
    };
    let snapshot = zones.snapshot();
    if snapshot.is_empty() {
        sender.send::<StateSyncChannel>(StorageFullSync {
            reset: true,
            cells: crate::protocol::BoundedVec::default(),
        });
        return;
    }
    for (index, part) in snapshot.chunks(PLAN_EDIT_BATCH_MAX).enumerate() {
        sender.send::<StateSyncChannel>(StorageFullSync {
            reset: index == 0,
            cells: crate::protocol::BoundedVec::new(part.to_vec())
                .expect("storage full-sync chunks use the wire bound"),
        });
    }
}

/// Client: replace the mirror with the connect-time snapshot.
fn receive_storage_full_sync(
    mut receivers: Query<&mut MessageReceiver<StorageFullSync>>,
    mut zones: ResMut<StorageZones>,
) {
    for mut receiver in receivers.iter_mut() {
        for sync in receiver.receive() {
            if sync.reset {
                zones.replace_all(Vec::new());
            }
            for cell in sync.cells {
                zones.insert(cell);
            }
        }
    }
}

/// Client: apply broadcast deltas to the mirror.
fn receive_storage_batch_broadcasts(
    mut receivers: Query<&mut MessageReceiver<StorageEditBatch>>,
    mut zones: ResMut<StorageZones>,
) {
    for mut receiver in receivers.iter_mut() {
        for batch in receiver.receive() {
            for cell in batch.cells {
                if batch.add {
                    zones.insert(cell);
                } else {
                    zones.remove(cell);
                }
            }
        }
    }
}

/// Zone-overlay tint: drawn only in Storage mode (stockpiles are a
/// management-view concern; Normal/Plan keep the world clean). One
/// flat gizmo slab per cell, hovering just above the floor face so it
/// doesn't z-fight the block top.
fn draw_storage_zones(zones: Res<StorageZones>, mode: Res<PlayerMode>, mut gizmos: Gizmos) {
    if *mode != PlayerMode::Storage {
        return;
    }
    // Warm amber — reads "stockpile", distinct from the red/green
    // plan-drag verbs and the purple station outlines.
    let colour = Color::srgba(1.0, 0.75, 0.2, 0.8);
    for cell in zones.iter() {
        let centre = cell.as_vec3() + Vec3::new(0.5, 0.02, 0.5);
        gizmos.cube(
            Transform::from_translation(centre).with_scale(Vec3::new(0.94, 0.0, 0.94)),
            colour,
        );
    }
}

/// In-flight drag preview: one translucent box over the rectangle,
/// amber for mark / red for clear — same "advertise the sweep, not the
/// per-cell outcome" contract as the plan drag preview.
fn draw_storage_drag_preview(drag: Res<StorageDragState>, mut gizmos: Gizmos) {
    let Some(active) = drag.active else {
        return;
    };
    let cells: Vec<IVec3> = rect_cells_on_plane(active.anchor, active.second)
        .into_iter()
        .map(|c| c + IVec3::Y)
        .collect();
    if cells.is_empty() {
        return;
    }
    let mut min = cells[0];
    let mut max = cells[0];
    for c in cells.iter().skip(1) {
        min = min.min(*c);
        max = max.max(*c);
    }
    let centre = (min.as_vec3() + max.as_vec3() + Vec3::ONE) * 0.5;
    let scale = (max - min).as_vec3() + Vec3::ONE;
    let colour = if active.add {
        Color::srgba(1.0, 0.75, 0.2, 1.0)
    } else {
        Color::srgba(1.0, 0.3, 0.3, 1.0)
    };
    gizmos.cube(
        Transform::from_translation(centre).with_scale(scale),
        colour,
    );
}
