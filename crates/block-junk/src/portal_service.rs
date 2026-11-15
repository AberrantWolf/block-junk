//! Authored portal state shares one transaction path for scripts and players.
use crate::{
    blocks::{BlockRegistry, BlockSlot},
    placements::Placements,
    terrain_transaction::TerrainTransaction,
    voxel::EntryKind,
};
use bevy::prelude::*;
use block_junk_mod_api::architecture::PlacementId;
use serde::{Deserialize, Serialize};

pub struct PortalChange<'a> {
    pub placement_id: u64,
    pub revision: u64,
    pub key: &'a str,
    pub open: bool,
}

pub fn stage(
    placements: &mut Placements,
    terrain: &mut TerrainTransaction,
    change: PortalChange<'_>,
    blocks: &BlockRegistry,
    read: &impl Fn(IVec3) -> Option<(BlockSlot, Option<EntryKind>, u64)>,
    actor_eyes: impl IntoIterator<Item = Vec3>,
) -> Result<u64, String> {
    let id = PlacementId(change.placement_id);
    let mut next = placements.clone();
    if !next.set_portal(id, change.revision, change.key, change.open, blocks)? {
        return Ok(change.revision);
    }
    let record = &next.records[&id];
    if !change.open {
        let slot = blocks
            .slot_of(&record.definition)
            .ok_or("portal definition missing")?;
        let obstacles = blocks.collision[slot.0 as usize][record.orientation as usize]
            .at(IVec3::from_array(record.anchor), record.portal_open(blocks));
        for eye in actor_eyes {
            let centre = eye - Vec3::Y * crate::physics::EYE_OFFSET_FROM_CENTRE;
            let lo = centre - crate::physics::PLAYER_HALF_EXTENTS;
            let hi = centre + crate::physics::PLAYER_HALF_EXTENTS;
            if obstacles
                .iter()
                .any(|bounds| bounds.min.cmplt(hi).all() && bounds.max.cmpgt(lo).all())
            {
                return Err("cannot close a portal through an actor".into());
            }
        }
    }
    terrain.portal_state(record, blocks, read)?;
    let revision = record.revision;
    *placements = next;
    Ok(revision)
}

#[derive(bevy::prelude::Message, Clone, Debug, Serialize, Deserialize)]
pub struct PortalRequest {
    pub request_id: u64,
    pub placement_id: u64,
    pub revision: u64,
    pub key: crate::protocol::BoundedString<128>,
    pub open: bool,
}
#[derive(bevy::prelude::Message, Clone, Debug, Serialize, Deserialize)]
pub struct PortalResult {
    pub request_id: u64,
    pub placement_id: u64,
    pub revision: Option<u64>,
    pub reason: Option<crate::protocol::BoundedString<512>>,
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct PortalWorld<'w, 's> {
    placements: ResMut<'w, Placements>,
    blocks: Res<'w, BlockRegistry>,
    chunks: Query<
        'w,
        's,
        (
            &'static mut crate::voxel::Chunk,
            &'static mut crate::voxel::ChunkEntities,
        ),
    >,
    map: Res<'w, crate::voxel::ChunkMap>,
    actors: Query<'w, 's, &'static crate::protocol::AvatarPose, With<crate::protocol::Actor>>,
    avatars: Res<'w, crate::server::ClientAvatars>,
    journal: ResMut<'w, crate::spatial_events::SpatialJournal>,
    effects: ResMut<'w, crate::server::PendingTerrainEffects>,
    commands: Commands<'w, 's>,
    edits: MessageWriter<'w, crate::protocol::CellEdit>,
}
pub fn receive(
    mut receivers: Query<
        (
            Entity,
            &mut lightyear::prelude::MessageReceiver<PortalRequest>,
        ),
        With<crate::protocol::GameReady>,
    >,
    mut senders: Query<&mut lightyear::prelude::MessageSender<PortalResult>>,
    mut validation: crate::server::ValidatedRequestContext,
    mut world: PortalWorld,
) {
    for (connection, mut receiver) in &mut receivers {
        for request in receiver.receive() {
            if validation
                .authorize(connection, crate::server::RequestClass::Ordinary)
                .is_none()
            {
                continue;
            }
            let outcome = (|| -> Result<u64, String> {
                let record = world
                    .placements
                    .records
                    .get(&PlacementId(request.placement_id))
                    .filter(|record| record.active)
                    .ok_or("portal placement unavailable")?;
                let avatar = world
                    .avatars
                    .0
                    .get(&connection)
                    .ok_or("player unavailable")?;
                let pose = world
                    .actors
                    .get(*avatar)
                    .map_err(|_| "player unavailable")?;
                if !crate::server::within_reach(
                    pose,
                    IVec3::from_array(record.anchor).as_vec3() + Vec3::splat(0.5),
                    crate::protocol::INTERACT_REACH,
                ) {
                    return Err("portal is out of reach".into());
                }
                if world.effects.0.len() >= 4096 {
                    return Err("terrain publication queue is full".into());
                }
                let mut placements = world.placements.clone();
                let mut terrain = TerrainTransaction::default();
                let read = |cell| {
                    let (coord, local) = crate::voxel::world_to_chunk(cell);
                    let entity = world.map.0.get(&coord)?;
                    let (chunk, side) = world.chunks.get(*entity).ok()?;
                    Some((chunk.get(local), side.get(cell), chunk.revision))
                };
                let revision = stage(
                    &mut placements,
                    &mut terrain,
                    PortalChange {
                        placement_id: request.placement_id,
                        revision: request.revision,
                        key: &request.key,
                        open: request.open,
                    },
                    &world.blocks,
                    &read,
                    world.actors.iter().map(|pose| pose.translation),
                )?;
                let cause = world
                    .journal
                    .prepare(terrain.changes().map(|(cell, _, _)| cell), None)?;
                world.journal.commit_change(cause);
                *world.placements = placements;
                let PortalWorld {
                    ref mut commands,
                    ref mut chunks,
                    ref map,
                    ref mut edits,
                    ..
                } = world;
                let effects = crate::server::commit_terrain_geometry(
                    terrain, commands, chunks, map, edits, None, false,
                );
                world.effects.0.extend(effects);
                Ok(revision)
            })();
            if let Ok(mut sender) = senders.get_mut(connection) {
                let reason = outcome.as_ref().err().map(|reason| {
                    crate::protocol::BoundedString::new(reason.clone()).unwrap_or_else(|_| {
                        crate::protocol::BoundedString::new("portal change rejected").unwrap()
                    })
                });
                sender.send::<crate::protocol::WorldChannel>(PortalResult {
                    request_id: request.request_id,
                    placement_id: request.placement_id,
                    revision: outcome.ok(),
                    reason,
                });
            }
        }
    }
}

#[derive(bevy::prelude::Resource, Default)]
struct PortalInputSequence(u64);

pub struct PortalClientPlugin;
impl bevy::prelude::Plugin for PortalClientPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        use bevy::prelude::*;
        crate::spatial::init_session_resource::<PortalInputSequence>(app);
        app.add_systems(
            Update,
            (portal_input, portal_feedback)
                .in_set(crate::protocol::GameSet::Input)
                .run_if(in_state(crate::menu::AppState::InGame)),
        );
    }
}

/// Select the nearest authored portal to the inspected cell; the inspector uses
/// this same selection to describe what the interaction key will operate.
pub(crate) fn inspected_portal<'a>(
    placement: &crate::placement_network::PlacementView,
    cell: bevy::prelude::IVec3,
    blocks: &'a crate::blocks::BlockRegistry,
) -> Option<(usize, &'a block_junk_mod_api::architecture::PortalGeometry)> {
    blocks
        .def(placement.slot)
        .structure
        .portals
        .iter()
        .enumerate()
        .min_by_key(|(_, portal)| {
            let distance = portal
                .volume
                .iter()
                .copied()
                .chain(portal.faces.iter().map(|face| face.cell))
                .map(|offset| {
                    let target = placement.anchor
                        + bevy::prelude::IVec3::from_array(
                            placement.orientation.rotate_offset(offset),
                        );
                    (target - cell).abs().element_sum()
                })
                .min()
                .unwrap_or(i32::MAX);
            (distance, portal.key.as_str())
        })
}

fn portal_input(
    keys: bevy::prelude::Res<bevy::prelude::ButtonInput<bevy::prelude::KeyCode>>,
    captures: bevy::prelude::Res<crate::ui_capture::UiCaptures>,
    target: bevy::prelude::Res<crate::inspect_panel::InspectTarget>,
    placements: bevy::prelude::Res<crate::placement_network::ClientPlacements>,
    blocks: bevy::prelude::Res<crate::blocks::BlockRegistry>,
    mut sequence: bevy::prelude::ResMut<PortalInputSequence>,
    mut senders: bevy::prelude::Query<&mut lightyear::prelude::MessageSender<PortalRequest>>,
) {
    if captures.is_captured() || !keys.just_pressed(bevy::prelude::KeyCode::KeyE) {
        return;
    }
    let crate::inspect_panel::InspectState::Block { cell, .. } = target.state else {
        return;
    };
    let Some(placement) = placements.at(cell) else {
        return;
    };
    let Some((index, portal)) = inspected_portal(placement, cell, &blocks) else {
        return;
    };
    let Some(next) = sequence.0.checked_add(1) else {
        return;
    };
    let Ok(mut sender) = senders.single_mut() else {
        return;
    };
    sequence.0 = next;
    let open = placement
        .portal_open
        .map_or(portal.initially_open, |mask| mask & (1 << index) != 0);
    sender.send::<crate::protocol::WorldChannel>(PortalRequest {
        request_id: next,
        placement_id: placement.id,
        revision: placement.revision,
        key: crate::protocol::BoundedString::new(portal.key.clone()).expect("validated portal key"),
        open: !open,
    });
}
fn portal_feedback(
    mut receivers: bevy::prelude::Query<&mut lightyear::prelude::MessageReceiver<PortalResult>>,
    target: bevy::prelude::Res<crate::inspect_panel::InspectTarget>,
    mut toasts: bevy::prelude::ResMut<crate::worldspace_toast::PendingToasts>,
) {
    for mut receiver in &mut receivers {
        for result in receiver.receive() {
            if let Some(reason) = result.reason {
                let crate::inspect_panel::InspectState::Block { cell, .. } = target.state else {
                    continue;
                };
                toasts.push(crate::worldspace_toast::SpawnToast {
                    cell,
                    text: reason.to_string(),
                });
            }
        }
    }
}
