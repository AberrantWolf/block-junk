//! Select-mode inspection panel: shows the cursor's target's details
//! (NPC or block) in a bottom-right `bevy_ui` overlay.
//!
//! - **NPC**: R-click an NPC body in Select mode → client sends
//!   `RequestNpcDetails` → server replies with `NpcDetails` →
//!   panel renders name/kind/needs/goal.
//! - **Block**: R-click a block → client resolves the block-def from
//!   its local `BlockRegistry` and renders id/display name/tags +
//!   any consumable/sleeper metadata. No round-trip needed.
//!
//! The panel is intentionally `bevy_ui`, not `egui`: egui is reserved
//! for debug (F3) so in-game UI reads as a distinct visual layer.
//! Skin (Kenney UI Pack adventure 9-slice) lands in a later polish
//! pass; this phase uses a solid translucent dark panel.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::camera::FlyCam;
use crate::client::{RAYCAST_REACH, entity_aware_raycast, raycast_npcs};
use crate::menu::AppState;
use crate::player_mode::PlayerMode;
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
            (handle_inspect_input, receive_npc_details, refresh_inspect_panel)
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
fn handle_inspect_input(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mode: Res<PlayerMode>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    npcs: Query<(&NpcId, &AvatarPose), With<Npc>>,
    mut target: ResMut<InspectTarget>,
    mut sender: Query<&mut MessageSender<RequestNpcDetails>>,
) {
    // Mode + escape: dismiss the panel when the player leaves Select
    // mode or presses Escape. Outside Select the panel just hides.
    if *mode != PlayerMode::Select {
        if !matches!(target.state, InspectState::None) {
            target.state = InspectState::None;
        }
        return;
    }
    if keys.just_pressed(KeyCode::Escape) && !matches!(target.state, InspectState::None) {
        target.state = InspectState::None;
        return;
    }
    if !mouse.just_pressed(MouseButton::Right) {
        return;
    }
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
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
    // distance as the centre of the cell — close enough for tie-
    // breaking under normal play (mistargets dissolve once we add
    // proper picking later).
    let block_hit = entity_aware_raycast(origin, dir, RAYCAST_REACH, &chunks, &chunk_map, &registry);
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
        // Preserve prior content for the same NPC so re-inspects don't
        // flash empty while waiting for the next reply.
        let last_seen = match &target.state {
            InspectState::Npc(d) if d.npc_id == npc_id.0 => Some(d.clone()),
            InspectState::PendingNpc { last_seen, .. } => last_seen.clone(),
            _ => None,
        };
        target.state = InspectState::PendingNpc { npc_id, last_seen };
        if let Ok(mut sender) = sender.single_mut() {
            sender.send::<WorldChannel>(RequestNpcDetails { npc_id: npc_id.0 });
        }
    } else if let Some(hit) = block_hit {
        let (coord, local) = crate::voxel::world_to_chunk(hit.cell);
        let slot = chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|(chunk, _)| chunk.get(local))
            .unwrap_or(BlockSlot::EMPTY);
        target.state = InspectState::Block {
            cell: hit.cell,
            slot,
        };
    } else {
        // R-click on nothing in Select mode dismisses the panel.
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
    if let Some(c) = def.consumable.as_ref() {
        out.push_str(&format!(
            "\nconsumable\n  need: {}\n  restores: {:.2}\n  duration: {:.1}s\n",
            c.need, c.restores, c.duration_secs
        ));
    }
    if let Some(s) = def.sleeper.as_ref() {
        out.push_str(&format!(
            "\nsleeper\n  need: {}\n  restores: {:.2}\n  duration: {:.1}s\n",
            s.need, s.restores, s.duration_secs
        ));
    }
    out
}
