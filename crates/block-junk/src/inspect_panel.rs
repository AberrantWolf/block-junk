//! Hover-driven inspection panel: shows the cursor's current target's
//! details (NPC or block) in a bottom-right `bevy_ui` overlay.
//!
//! - **NPC**: hovering an NPC body sends one `RequestNpcDetails` for
//!   that NPC's id. The reply lands as `NpcDetails` and the panel
//!   renders name/kind/needs/goal. Subsequent frames don't re-request
//!   for the same NPC — we just keep the last reply visible.
//! - **Block**: hovering a block resolves the block-def from the local
//!   `BlockRegistry` and renders id/display name/tags + any
//!   interactable metadata. No round-trip needed.
//!
//! Works in every mode (Normal and Plan). Suppressed during an active
//! Plan-mode drag so the drag-rect preview owns the visual layer.
//!
//! Switched from R-click to hover 2026-05-18 alongside the 2-mode
//! collapse — clicks are commits, hover is informational.
//!
//! The panel is intentionally `bevy_ui`, not `egui`: egui is reserved
//! for debug (F3) so in-game UI reads as a distinct visual layer.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::camera::FlyCam;
use crate::client::{RAYCAST_REACH, entity_aware_raycast, raycast_npcs};
use crate::menu::AppState;
use crate::plans::PlanDragState;
use crate::protocol::{AvatarPose, GameSet, NpcDetails, RequestNpcDetails, WorldChannel};
use crate::npc::{Npc, NpcId};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

pub struct InspectPanelPlugin;

impl Plugin for InspectPanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InspectTarget>();
        app.add_systems(OnEnter(AppState::InGame), spawn_inspect_panel);
        app.add_systems(
            Update,
            (refresh_inspect_target, receive_npc_details, refresh_inspect_panel)
                .chain()
                .in_set(GameSet::Input)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// What the panel is currently showing (or fetching).
#[derive(Resource, Default, Clone, Debug)]
pub struct InspectTarget {
    pub state: InspectState,
}

#[derive(Default, Clone, Debug)]
pub enum InspectState {
    #[default]
    None,
    /// Waiting on the server's `NpcDetails` reply. `last_seen` is the
    /// previous reply (if any) so a re-inspection of the same NPC keeps
    /// the existing panel content visible instead of going blank.
    PendingNpc {
        npc_id: NpcId,
        last_seen: Option<NpcDetails>,
    },
    Npc(NpcDetails),
    Block {
        cell: IVec3,
        slot: BlockSlot,
    },
}

#[derive(Component)]
struct InspectPanelRoot;

#[derive(Component)]
struct InspectPanelText;

fn spawn_inspect_panel(mut commands: Commands, existing: Query<(), With<InspectPanelRoot>>) {
    if !existing.is_empty() {
        return;
    }
    commands
        .spawn((
            InspectPanelRoot,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(16.0),
                right: Val::Px(16.0),
                padding: UiRect::axes(Val::Px(14.0), Val::Px(12.0)),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                width: Val::Px(280.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(6.0),
                ..default()
            },
            BackgroundColor(Color::srgba(0.10, 0.08, 0.07, 0.85)),
            BorderColor::all(Color::srgba(0.95, 0.85, 0.55, 0.45)),
            Visibility::Hidden,
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new(""),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(Color::srgb(0.95, 0.92, 0.85)),
                InspectPanelText,
            ));
        });
}

#[allow(clippy::too_many_arguments, reason = "input system spans many subsystems")]
fn refresh_inspect_target(
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    drag: Res<PlanDragState>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    npcs: Query<(&NpcId, &AvatarPose), With<Npc>>,
    mut target: ResMut<InspectTarget>,
    mut sender: Query<&mut MessageSender<RequestNpcDetails>>,
) {
    // Cursor unlocked (menu / alt-tab) or mid-drag: hide the panel.
    // Plan-mode drag preview owns the visual layer until release.
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked || drag.active.is_some() {
        if !matches!(target.state, InspectState::None) {
            target.state = InspectState::None;
        }
        return;
    }
    let Ok(cam_t) = cam.single() else {
        return;
    };
    let origin = cam_t.translation();
    let dir = *cam_t.forward();

    // Compare ray distances: pick whichever target is closer. Block
    // raycast returns the cell; NPC raycast returns the body-AABB
    // hit's t (its distance directly). For block hits we approximate
    // distance as the centre of the cell.
    let block_hit = entity_aware_raycast(origin, dir, RAYCAST_REACH, &chunks, &chunk_map, &registry, None);
    let npc_hit = raycast_npcs(origin, dir, RAYCAST_REACH, &npcs);

    let block_dist = block_hit.as_ref().map(|h| {
        let centre = h.cell.as_vec3() + Vec3::splat(0.5);
        (centre - origin).length()
    });
    let npc_dist = npc_hit.as_ref().map(|h| h.distance);

    let pick_npc = match (npc_dist, block_dist) {
        (Some(n), Some(b)) => n <= b,
        (Some(_), None) => true,
        _ => false,
    };

    if pick_npc {
        let npc_id = npc_hit.unwrap().npc_id;
        // Only re-request when the targeted npc *changes* — hovering
        // the same NPC every frame shouldn't spam the server. Already-
        // pending and already-resolved states for this NPC count as
        // "no need to ask again."
        let already_targeted = match &target.state {
            InspectState::PendingNpc { npc_id: prev, .. } => prev.0 == npc_id.0,
            InspectState::Npc(prev) => prev.npc_id == npc_id.0,
            _ => false,
        };
        if !already_targeted {
            let last_seen = match &target.state {
                InspectState::Npc(d) if d.npc_id == npc_id.0 => Some(d.clone()),
                InspectState::PendingNpc { last_seen, .. } => last_seen.clone(),
                _ => None,
            };
            target.state = InspectState::PendingNpc { npc_id, last_seen };
            if let Ok(mut sender) = sender.single_mut() {
                sender.send::<WorldChannel>(RequestNpcDetails { npc_id: npc_id.0 });
            }
        }
    } else if let Some(hit) = block_hit {
        let (coord, local) = crate::voxel::world_to_chunk(hit.cell);
        let slot = chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|(chunk, _)| chunk.get(local))
            .unwrap_or(BlockSlot::EMPTY);
        // Skip the resource write when the cursor is parked on the
        // same block — keeps Bevy's change-detection from re-running
        // refresh_inspect_panel every frame for no reason.
        let same = matches!(
            &target.state,
            InspectState::Block { cell, slot: s } if *cell == hit.cell && *s == slot,
        );
        if !same {
            target.state = InspectState::Block {
                cell: hit.cell,
                slot,
            };
        }
    } else if !matches!(target.state, InspectState::None) {
        // Cursor on empty space: dismiss the panel.
        target.state = InspectState::None;
    }
}

fn receive_npc_details(
    mut receivers: Query<&mut MessageReceiver<NpcDetails>>,
    mut target: ResMut<InspectTarget>,
) {
    for mut receiver in receivers.iter_mut() {
        for details in receiver.receive() {
            // Drop the response if the player has moved on to a
            // different target — stale data shouldn't overwrite a
            // newer selection.
            let pending_match = matches!(
                &target.state,
                InspectState::PendingNpc { npc_id, .. } if npc_id.0 == details.npc_id,
            );
            let existing_match = matches!(
                &target.state,
                InspectState::Npc(prev) if prev.npc_id == details.npc_id,
            );
            if pending_match || existing_match {
                target.state = InspectState::Npc(details);
            }
        }
    }
}

fn refresh_inspect_panel(
    target: Res<InspectTarget>,
    registry: Res<BlockRegistry>,
    mut roots: Query<&mut Visibility, With<InspectPanelRoot>>,
    mut texts: Query<&mut Text, With<InspectPanelText>>,
) {
    if !target.is_changed() {
        return;
    }
    let body = render_body(&target.state, &registry);
    let visibility = match body {
        Some(_) => Visibility::Inherited,
        None => Visibility::Hidden,
    };
    for mut v in roots.iter_mut() {
        *v = visibility;
    }
    for mut text in texts.iter_mut() {
        text.0 = body.clone().unwrap_or_default();
    }
}

/// Format the panel's text content. `None` ⇒ panel hidden.
fn render_body(state: &InspectState, registry: &BlockRegistry) -> Option<String> {
    match state {
        InspectState::None => None,
        InspectState::PendingNpc { last_seen: Some(prev), .. } => Some(format_npc(prev, true)),
        InspectState::PendingNpc { last_seen: None, npc_id } => {
            Some(format!("NPC #{} — fetching…", npc_id.0))
        }
        InspectState::Npc(details) => Some(format_npc(details, false)),
        InspectState::Block { cell, slot } => Some(format_block(*cell, *slot, registry)),
    }
}

fn format_npc(details: &NpcDetails, stale: bool) -> String {
    let mut out = String::new();
    let tag = if stale { " (refreshing…)" } else { "" };
    out.push_str(&format!("{}{}\n", details.kind, tag));
    out.push_str(&format!("id #{}\n", details.npc_id));
    out.push_str(&format!("goal: {}\n", details.current_goal));
    if let Some(target) = details.target_cell {
        out.push_str(&format!(
            "target: ({}, {}, {})\n",
            target.x, target.y, target.z
        ));
    }
    if !details.needs.is_empty() {
        out.push_str("\nneeds:\n");
        let mut entries: Vec<(&String, &f32)> = details.needs.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (id, value) in entries {
            out.push_str(&format!("  {:<10} {:.2}\n", id, value));
        }
    }
    out
}

fn format_block(cell: IVec3, slot: BlockSlot, registry: &BlockRegistry) -> String {
    if slot.is_empty() {
        return format!("Empty\ncell ({}, {}, {})", cell.x, cell.y, cell.z);
    }
    let def = registry.def(slot);
    let mut out = String::new();
    out.push_str(&format!("{}\n", def.display_name));
    out.push_str(&format!("id: {}\n", def.id));
    out.push_str(&format!("cell ({}, {}, {})\n", cell.x, cell.y, cell.z));
    if let Some(i) = def.interactable.as_ref() {
        out.push_str("\ninteractable\n");
        if let Some(nr) = &i.need_restore {
            out.push_str(&format!("  need: {}\n  restores: {:.2}\n", nr.need, nr.restores));
        }
        out.push_str(&format!("  duration: {:.1}s\n", i.duration_secs));
        out.push_str(&format!("  exclusive: {}\n", i.exclusive));
    }
    out
}
