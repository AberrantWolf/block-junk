//! Storage-zone designation — S1 of the storage arc.
//!
//! `StorageZones` is the canonical set of cells the player has marked
//! as storage. Like `Plans`, it lives as a server-authoritative
//! `Resource` on the server App and as a passive spatial replica on each
//! client. Client mutation is a typed request validated for reach, batch
//! size, and floor-cell shape.
//!
//! A zone cell is the *air* cell items occupy — solid floor below,
//! non-solid at the cell — matching where a pile or container will
//! physically sit. Painting happens in `PlayerMode::Storage`: R-drag
//! on the ground marks, L-drag clears. Both anchor on an upward face
//! only; there is no such thing as wall-mounted storage.
//!
//! S1 is designation + replication + rendering. S2 (collective piles)
//! adds the NPC behavior that reads this set.

use bevy::prelude::*;
use lightyear::prelude::*;

use crate::blocks::BlockRegistry;
use crate::client::entity_aware_raycast;
use crate::menu::AppState;
use crate::plans::{cell_is_solid, project_to_face_plane, rect_cells_on_plane};
use crate::player_mode::PlayerMode;
use crate::protocol::{
    ActionRejected, Avatar, AvatarPose, GameSet, PLAN_EDIT_BATCH_MAX, PLAN_REACH, RejectReason,
    StateSyncChannel, StorageEditBatch,
};
use crate::server::{RequestClass, ValidatedRequestContext, send_rejection, within_reach};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

pub struct StorageDataset;

impl crate::spatial::SpatialDataset for StorageDataset {
    type Key = IVec3;
    type Value = IVec3;
    type Wire = IVec3;
    type Persistence = crate::spatial::PersistedDataset;
    const ID: crate::spatial::DatasetId = crate::spatial::DatasetId(2);
    const SCHEMA_FINGERPRINT: u64 = 0x7374_6f72_0000_0001;
    const MEMBERSHIP: crate::spatial::MembershipPolicy =
        crate::spatial::MembershipPolicy::AnchorCell;
    const REPLICATION: crate::spatial::ReplicationPolicy =
        crate::spatial::ReplicationPolicy::Immediate;
    const MAX_RECORD_BYTES: usize = 32;
    fn chunks(key: &Self::Key, _: &Self::Value) -> Vec<crate::protocol::ChunkCoord> {
        vec![crate::voxel::world_to_chunk(*key).0]
    }
    fn to_wire(_: &Self::Key, value: &Self::Value) -> Self::Wire {
        *value
    }
    fn from_wire(
        wire: Self::Wire,
        _: &crate::spatial::SpatialDecodeRegistry,
    ) -> Result<(Self::Key, Self::Value), crate::spatial::SpatialError> {
        Ok((wire, wire))
    }
}

/// The set of designated storage cells. Sparse and unordered; zone
/// "shape" is emergent (any painted cell counts, contiguity doesn't
/// matter to the engine).
pub type StorageZones = crate::spatial::PartitionedStore<StorageDataset>;

impl StorageZones {
    /// Returns true when the cell was newly inserted (state changed).
    pub fn insert(&mut self, cell: IVec3) -> bool {
        if self.lookup(&cell).is_some() {
            false
        } else {
            self.upsert(cell, cell, 0);
            true
        }
    }

    /// Returns true when the cell was present (state changed).
    pub fn remove(&mut self, cell: IVec3) -> bool {
        self.delete(&cell, 0).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = &IVec3> {
        self.entries().map(|(cell, _)| cell)
    }

    pub fn snapshot(&self) -> Vec<IVec3> {
        self.iter().copied().collect()
    }

    pub(crate) fn restore_all(&mut self, cells: impl IntoIterator<Item = IVec3>) {
        let old: Vec<_> = self.iter().copied().collect();
        for cell in old {
            self.delete(&cell, 0);
        }
        for cell in cells {
            self.upsert(cell, cell, 0);
        }
    }
}

pub struct StorageServerPlugin;
pub struct StorageClientPlugin;

impl Plugin for StorageServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<StorageDataset>::server());
        app.add_systems(
            Update,
            receive_storage_edit_batches.in_set(GameSet::Simulation),
        );
    }
}

impl Plugin for StorageClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(crate::spatial::SpatialFeaturePlugin::<StorageDataset>::client());
        crate::spatial::init_session_resource::<StorageDragState>(app);
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

/// Server: validate a client's zone-edit batch and apply what stuck.
/// Adds require the floor-cell shape (air at the cell, solid
/// below) so zones can't be painted mid-air or inside walls; clears
/// have no shape requirement (the floor may have been mined since).
/// Re-painting an existing zone is a no-op.
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
    mut validation: ValidatedRequestContext,
) {
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
            let mut first_out_of_reach: Option<IVec3> = None;
            for cell in batch.cells {
                if !within_reach(pose, cell.as_vec3() + Vec3::splat(0.5), PLAN_REACH) {
                    first_out_of_reach.get_or_insert(cell);
                    continue;
                }
                if batch.add {
                    let is_floor_cell = !cell_is_solid(cell, &chunks, &chunk_map)
                        && cell_is_solid(cell - IVec3::Y, &chunks, &chunk_map);
                    if is_floor_cell {
                        zones.insert(cell);
                    }
                } else {
                    zones.remove(cell);
                }
            }
            if let Some(cell) = first_out_of_reach {
                send_rejection(&mut rejections, connection, cell, RejectReason::OutOfReach);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaint_and_reclear_do_not_journal_noops() {
        let mut zones = StorageZones::default();
        assert!(zones.insert(IVec3::ZERO));
        assert_eq!(zones.take_dirty().len(), 1);
        assert!(!zones.insert(IVec3::ZERO));
        assert!(zones.take_dirty().is_empty());

        assert!(zones.remove(IVec3::ZERO));
        assert_eq!(zones.take_dirty().len(), 1);
        assert!(!zones.remove(IVec3::ZERO));
        assert!(zones.take_dirty().is_empty());
    }
}
