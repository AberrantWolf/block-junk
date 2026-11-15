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
use lightyear::prelude::*;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::camera::FlyCam;
use crate::client::{RAYCAST_REACH, entity_aware_raycast, raycast_npcs, raycast_world_items};
use crate::craft_stations::CraftStations;
use crate::items::{ItemRegistry, ItemSlot};
use crate::menu::AppState;
use crate::npc::{Npc, NpcId};
use crate::plans::{PlanDragState, Plans, raycast_plans};
use crate::protocol::{
    AvatarPose, GameSet, NpcDetails, PlanKind, RequestNpcDetails, WorldChannel, WorldItem,
};
use crate::voxel::{Chunk, ChunkEntities, ChunkMap};

pub struct InspectPanelPlugin;

impl Plugin for InspectPanelPlugin {
    fn build(&self, app: &mut App) {
        crate::spatial::init_session_resource::<InspectTarget>(app);
        app.add_systems(OnEnter(AppState::InGame), spawn_inspect_panel);
        app.add_systems(
            Update,
            (
                refresh_inspect_target,
                receive_npc_details,
                refresh_inspect_panel,
            )
                .chain()
                .in_set(GameSet::Input)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            Update,
            highlight_failed_capability_evidence.run_if(in_state(AppState::InGame)),
        );
    }
}

fn highlight_failed_capability_evidence(
    target: Res<InspectTarget>,
    spaces: Res<crate::space_network::ClientSpaces>,
    capabilities: Res<crate::capability_network::ClientCapabilities>,
    placements: Res<crate::placement_network::ClientPlacements>,
    mut gizmos: Gizmos,
) {
    use block_junk_mod_api::capabilities::Value;
    let cell = match target.state {
        InspectState::Block { cell, .. } | InspectState::Plan { cell } => cell,
        _ => return,
    };
    if spaces.waiting_for_terrain || capabilities.waiting_for_spaces {
        return;
    }
    let placement = placements.at(cell).map(|record| record.id);
    let Some((_, profiles)) = capabilities.visible.spaces.iter().find(|(id, _)| {
        spaces
            .visible
            .spaces
            .iter()
            .any(|space| space.id == *id && space.contains_cell(cell, placement))
    }) else {
        return;
    };
    let mut ids = std::collections::BTreeSet::new();
    for result in profiles.profiles.values().flatten() {
        for evidence in result.requirements.values().filter(|evidence| {
            evidence.value != Some(Value::Boolean(true)) || evidence.problem.is_some()
        }) {
            collect_evidence_placements(evidence, &mut ids);
        }
    }
    for id in ids {
        if let Some(placement) = placements.lookup(&id.0) {
            gizmos.cube(
                Transform::from_translation(placement.anchor.as_vec3() + Vec3::splat(0.5))
                    .with_scale(Vec3::splat(1.08)),
                Color::srgb(1.0, 0.35, 0.12),
            );
        }
    }
}

fn collect_evidence_placements(
    evidence: &block_junk_mod_api::capabilities::Evidence,
    output: &mut std::collections::BTreeSet<block_junk_mod_api::architecture::PlacementId>,
) {
    output.extend(evidence.placements.iter().copied());
    output.extend(evidence.slots.iter().map(|slot| slot.placement));
    for child in &evidence.children {
        collect_evidence_placements(child, output);
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
    /// A Build plan cell (currently empty in the world, tagged for
    /// construction). The panel reads the live `Plans` resource on
    /// every refresh to show the current materials progress — we
    /// don't snapshot here so deposits update without per-tick
    /// state-resolution churn.
    Plan {
        cell: IVec3,
    },
    /// A loose [`WorldItem`] under the cursor — wins over block /
    /// NPC / plan hits when it's the closest target. Keyed by
    /// [`ItemSlot`] + unit `count`: the panel renders kind-info plus a
    /// stack count for piles, and both fields drive same/different
    /// change detection so a pile that grows/shrinks re-renders.
    Item {
        slot: ItemSlot,
        count: u32,
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
            DespawnOnExit(AppState::InGame),
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
            BackgroundColor(crate::ui_theme::PANEL_BG),
            BorderColor::all(crate::ui_theme::PANEL_BORDER),
            Visibility::Hidden,
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new(""),
                TextFont {
                    font_size: FontSize::Px(14.0),
                    ..default()
                },
                TextColor(crate::ui_theme::TEXT),
                InspectPanelText,
            ));
        });
}

#[allow(
    clippy::too_many_arguments,
    reason = "input system spans many subsystems"
)]
fn refresh_inspect_target(
    captures: Res<crate::ui_capture::UiCaptures>,
    drag: Res<PlanDragState>,
    cam: Query<&GlobalTransform, With<FlyCam>>,
    chunks: Query<(&Chunk, &ChunkEntities)>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    plans: Res<Plans>,
    npcs: Query<(&NpcId, &AvatarPose), With<Npc>>,
    world_items: Query<&WorldItem>,
    mut target: ResMut<InspectTarget>,
    mut sender: Query<&mut MessageSender<RequestNpcDetails>>,
) {
    // Overlay open (menu / modal) or mid-drag: hide the panel.
    // Plan-mode drag preview owns the visual layer until release.
    if captures.is_captured() || drag.active.is_some() {
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
    let block_hit = entity_aware_raycast(
        origin,
        dir,
        RAYCAST_REACH,
        &chunks,
        &chunk_map,
        &registry,
        None,
    );
    let npc_hit = raycast_npcs(origin, dir, RAYCAST_REACH, &npcs);
    let plan_hit = raycast_plans(origin, dir, RAYCAST_REACH, &plans);
    let item_hit = raycast_world_items(origin, dir, RAYCAST_REACH, &world_items);

    let block_dist = block_hit.as_ref().map(|h| {
        let centre = h.cell.as_vec3() + Vec3::splat(0.5);
        (centre - origin).length()
    });
    let npc_dist = npc_hit.as_ref().map(|h| h.distance);
    let plan_dist = plan_hit.map(|(d, _)| d);
    let item_dist = item_hit.as_ref().map(|(_, d, _, _)| *d);

    // Loose-item hit wins outright when it's closer than everything
    // else. Inspect should describe what's actually under the cursor,
    // not raycast past a small pickup-able into the terrain behind
    // it. Mirrors the same priority `normal_mode_action_input` uses
    // for the pickup verb.
    let item_beats_all = item_dist
        .map(|i| {
            block_dist.map(|b| i < b).unwrap_or(true)
                && npc_dist.map(|n| i < n).unwrap_or(true)
                && plan_dist.map(|p| i < p).unwrap_or(true)
        })
        .unwrap_or(false);
    if item_beats_all && let Some((_, _, slot, count)) = item_hit {
        let same = matches!(
            &target.state,
            InspectState::Item { slot: s, count: c } if *s == slot && *c == count
        );
        if !same {
            target.state = InspectState::Item { slot, count };
        }
        return;
    }

    // Plan wins outright if it's closer than the block hit *and* the
    // npc hit — a tagged Build cell hangs in empty air, so the world
    // raycast would otherwise return whatever's behind it. NPCs still
    // win over plans when they're closer (you can stand on a tagged
    // cell while pointing at a villager).
    let plan_beats_block = match (plan_dist, block_dist) {
        (Some(p), Some(b)) => p < b,
        (Some(_), None) => true,
        _ => false,
    };
    let plan_beats_npc = match (plan_dist, npc_dist) {
        (Some(p), Some(n)) => p < n,
        (Some(_), None) => true,
        _ => false,
    };
    if plan_beats_block
        && plan_beats_npc
        && let Some((_, cell)) = plan_hit
    {
        let same = matches!(&target.state, InspectState::Plan { cell: c } if *c == cell);
        if !same {
            target.state = InspectState::Plan { cell };
        }
        return;
    }

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

#[allow(
    clippy::too_many_arguments,
    reason = "inspect render pulls from many sources"
)]
fn refresh_inspect_panel(
    target: Res<InspectTarget>,
    registry: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
    recipes: Res<crate::recipes::RecipeRegistry>,
    plans: Res<Plans>,
    stations: Res<CraftStations>,
    containers: Res<crate::containers::Containers>,
    spaces: Res<crate::space_network::ClientSpaces>,
    placements: Res<crate::placement_network::ClientPlacements>,
    capabilities: Res<crate::capability_network::ClientCapabilities>,
    mut mods: ResMut<crate::scripting::ClientMods>,
    mut roots: Query<&mut Visibility, With<InspectPanelRoot>>,
    mut texts: Query<&mut Text, With<InspectPanelText>>,
) {
    // Service and architectural revisions can change while the cursor remains
    // still; refresh those results without asking the player to move the aim.
    if !target.is_changed()
        && !plans.is_changed()
        && !stations.is_changed()
        && !containers.is_changed()
        && !spaces.is_changed()
        && !placements.is_changed()
        && !capabilities.is_changed()
    {
        return;
    }
    let mut body = render_body(
        &target.state,
        &registry,
        &items,
        &recipes,
        &plans,
        &stations,
        &containers,
        &spaces,
        &placements,
        &capabilities,
    );
    // Mod contributions (`engine.ui.on_inspect`) append below the
    // engine-rendered body. Only consulted on actual re-render — Lua
    // is never called per-frame while the cursor parks on one target.
    if let Some(text) = body.as_mut()
        && let Some(mod_target) = mod_inspect_target(&target.state, &registry, &items, &plans)
    {
        for line in mods.0.call_inspect(&mod_target) {
            text.push('\n');
            text.push_str(&line);
        }
    }
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

/// Map the panel's state to the side-agnostic [`InspectTarget`] handed
/// to `engine.ui.on_inspect` callbacks. `None` for states with nothing
/// stable to describe (empty space, in-flight NPC fetch).
fn mod_inspect_target(
    state: &InspectState,
    registry: &BlockRegistry,
    items: &ItemRegistry,
    plans: &Plans,
) -> Option<block_junk_mod_api::ui::InspectTarget> {
    use block_junk_mod_api::shared::BlockPos;
    use block_junk_mod_api::ui::InspectTarget as ModTarget;
    let pos = |cell: IVec3| BlockPos {
        x: cell.x,
        y: cell.y,
        z: cell.z,
    };
    match state {
        InspectState::None | InspectState::PendingNpc { .. } => None,
        InspectState::Npc(details) => Some(ModTarget {
            kind: "npc".to_owned(),
            id: details.kind.clone(),
            pos: None,
        }),
        InspectState::Block { cell, slot } => {
            if slot.is_empty() {
                return None;
            }
            Some(ModTarget {
                kind: "block".to_owned(),
                id: registry.id_of(*slot).to_string(),
                pos: Some(pos(*cell)),
            })
        }
        InspectState::Plan { cell } => {
            let id = match plans.get(*cell).map(|s| &s.kind) {
                Some(PlanKind::Build { slot, .. }) if (slot.0 as usize) < registry.slot_count() => {
                    registry.id_of(*slot).to_string()
                }
                _ => String::new(),
            };
            Some(ModTarget {
                kind: "plan".to_owned(),
                id,
                pos: Some(pos(*cell)),
            })
        }
        InspectState::Item { slot, .. } => Some(ModTarget {
            kind: "item".to_owned(),
            id: items.id_of(*slot).to_string(),
            pos: None,
        }),
    }
}

/// Format the panel's text content. `None` ⇒ panel hidden.
#[allow(
    clippy::too_many_arguments,
    reason = "pure formatter over every inspectable subsystem"
)]
fn render_body(
    state: &InspectState,
    registry: &BlockRegistry,
    items: &ItemRegistry,
    recipes: &crate::recipes::RecipeRegistry,
    plans: &Plans,
    stations: &CraftStations,
    containers: &crate::containers::Containers,
    spaces: &crate::space_network::ClientSpaces,
    placements: &crate::placement_network::ClientPlacements,
    capabilities: &crate::capability_network::ClientCapabilities,
) -> Option<String> {
    let room_line = |cell: IVec3| -> String {
        let placement = placements.at(cell).map(|record| record.id);
        let mut lines = String::new();
        for space in spaces.visible.spaces.iter() {
            if space.contains_cell(cell, placement) {
                use std::fmt::Write;
                let _ = write!(
                    lines,
                    "\nSpace #{} · revision {}\nStructural area: {} · covered: {} · roof unknown: {}\nVolume: {:?}{}\n",
                    space.id,
                    space.revision,
                    space.structural_area,
                    space.covered,
                    space.unknown_roof,
                    space.volume,
                    if space.pending || spaces.waiting_for_terrain {
                        " · geometry pending"
                    } else {
                        ""
                    }
                );
                append_capabilities(&mut lines, space.id, capabilities);
            }
            if space.connections.iter().any(|connection| {
                connection.threshold.iter().any(|threshold| {
                    threshold.0 == cell.x && threshold.1 == cell.y && threshold.2 == cell.z
                })
            }) {
                lines.push_str("\nArchitectural threshold — excluded from adjoining floor area\n");
                break;
            }
        }
        if lines.is_empty() {
            lines.push_str("\nNo resolved architectural membership at this cell\n");
        }
        lines
    };
    match state {
        InspectState::None => None,
        InspectState::PendingNpc {
            last_seen: Some(prev),
            ..
        } => Some(format_npc(prev, true)),
        InspectState::PendingNpc {
            last_seen: None,
            npc_id,
        } => Some(format!("NPC #{} — fetching…", npc_id.0)),
        InspectState::Npc(details) => Some(format_npc(details, false)),
        InspectState::Block { cell, slot } => {
            let mut out = format_block(*cell, *slot, registry);
            if let Some(placement) = placements.at(*cell)
                && let Some((index, portal)) =
                    crate::portal_service::inspected_portal(placement, *cell, registry)
            {
                let open = placement
                    .portal_open
                    .map_or(portal.initially_open, |mask| mask & (1 << index) != 0);
                out.push_str(&format!(
                    "\n[E] {} {}\n",
                    if open { "Close" } else { "Open" },
                    portal.key
                ));
            }
            // Station info appends when the inspected block is a
            // station — lists every recipe registered at the station
            // tag so the player knows what they could craft.
            if let Some(station_tag) = &registry.def(*slot).station_tag {
                out.push('\n');
                // Lead with the action hint when the station has no
                // queued orders, so a new player sees "what does this
                // bench do?" → "press R to queue something" at the top
                // of the panel rather than buried below recipes.
                let no_orders = stations
                    .get(*cell)
                    .map(|s| s.orders.is_empty())
                    .unwrap_or(true);
                if no_orders {
                    out.push_str("→ Right-click to queue up work\n\n");
                }
                out.push_str(&format_station_inner(station_tag, recipes, items));
            }
            // Container stock appends when the inspected block is a
            // container — live mirror read, so hovering a crate shows
            // what's inside without opening anything.
            if let Some(cfg) = &registry.def(*slot).container {
                out.push('\n');
                out.push_str(&format_container_inner(cfg, containers.get(*cell), items));
            }
            // Append plan info when the inspected block is tagged
            // (Remove plans live on solid cells, so the block raycast
            // resolves them).
            if let Some(plan_state) = plans.get(*cell) {
                out.push('\n');
                out.push_str(&format_plan_inner(plan_state, registry, items));
            }
            out.push_str(&room_line(*cell));
            Some(out)
        }
        InspectState::Plan { cell } => {
            let header = format!("Plan ({}, {}, {})\n", cell.x, cell.y, cell.z);
            let body = match plans.get(*cell) {
                Some(state) => format_plan_inner(state, registry, items),
                None => "(tag cleared)".to_owned(),
            };
            Some(format!("{header}{body}{}", room_line(*cell)))
        }
        InspectState::Item { slot, count } => Some(format_item(*slot, *count, items)),
    }
}

fn append_capabilities(
    out: &mut String,
    space_id: u64,
    capabilities: &crate::capability_network::ClientCapabilities,
) {
    if capabilities.waiting_for_spaces {
        out.push_str("Capabilities: pending architectural revision\n");
        return;
    }
    let Some((_, result)) = capabilities
        .visible
        .spaces
        .iter()
        .find(|(id, _)| *id == space_id)
    else {
        out.push_str("Capabilities: pending\n");
        return;
    };
    append_space_capabilities(out, result);
}

pub(crate) fn append_space_capabilities(
    out: &mut String,
    result: &crate::capabilities::SpaceCapabilities,
) {
    use block_junk_mod_api::capabilities::{CapabilityStatus, EvaluationProblem, Value};
    use std::fmt::Write;

    for (profile, values) in &result.profiles {
        let _ = writeln!(out, "Capabilities for {profile}:");
        for capability in values {
            let status = match capability.status {
                CapabilityStatus::Supported => "supported",
                CapabilityStatus::Unsupported => "requirements unmet",
                CapabilityStatus::Pending => "pending",
                CapabilityStatus::Invalid => "invalid rule",
            };
            let capacity = capability
                .capacity
                .as_ref()
                .and_then(|evidence| evidence.value.as_ref())
                .map(|value| match value {
                    Value::Number(value) => format!(" · capacity {value}"),
                    Value::Unbounded => " · unbounded".into(),
                    Value::Boolean(value) => format!(" · {value}"),
                })
                .unwrap_or_default();
            let _ = writeln!(out, "  {}: {status}{capacity}", capability.display_name);
            for (name, evidence) in &capability.attributes {
                let value = match &evidence.value {
                    Some(Value::Number(value)) => value.to_string(),
                    Some(Value::Unbounded) => "unbounded".into(),
                    Some(Value::Boolean(value)) => value.to_string(),
                    None => match &evidence.problem {
                        Some(EvaluationProblem::Invalid(reason)) => format!("invalid: {reason}"),
                        Some(EvaluationProblem::Unknown(reason)) => format!("pending: {reason}"),
                        None => "pending".into(),
                    },
                };
                let _ = writeln!(out, "    {name}: {value}");
            }
            for evidence in capability.requirements.values().filter(|evidence| {
                evidence.value != Some(Value::Boolean(true)) || evidence.problem.is_some()
            }) {
                let observed = match &evidence.value {
                    Some(Value::Number(value)) => format!("observed {value}"),
                    Some(Value::Boolean(false)) => "not satisfied".into(),
                    Some(Value::Boolean(true)) => "satisfied".into(),
                    Some(Value::Unbounded) => "observed unbounded".into(),
                    None => match &evidence.problem {
                        Some(EvaluationProblem::Unknown(reason)) => format!("pending: {reason}"),
                        Some(EvaluationProblem::Invalid(reason)) => format!("invalid: {reason}"),
                        None => "unavailable".into(),
                    },
                };
                let _ = writeln!(out, "    {} — {observed}", evidence.description);
                for child in evidence.children.iter().filter(|child| {
                    child.value != Some(Value::Number(1.)) || child.problem.is_some()
                }) {
                    let _ = writeln!(out, "      {}", child.description);
                }
            }
        }
    }
}

/// Render the inspect panel body for a loose [`WorldItem`]. Display
/// name + id + any tool tags so tools read as "what kind of action
/// does this enable" at a glance. A `count > 1` is a collective pile,
/// shown with its size and per-pile capacity.
fn format_item(slot: ItemSlot, count: u32, items: &ItemRegistry) -> String {
    let def = items.def(slot);
    let mut out = String::new();
    out.push_str(&format!("{}\n", def.display_name));
    if count > 1 {
        out.push_str(&format!("pile: {} / {}\n", count, def.pile_capacity()));
    }
    out.push_str(&format!("id: {}\n", def.id));
    if !def.tool_tags.is_empty() {
        out.push_str("tool tags:\n");
        for tag in &def.tool_tags {
            out.push_str(&format!("  {tag}\n"));
        }
    }
    out
}

/// Render the stock list when the inspected block is a container.
/// Shows bulk usage against capacity plus a line per stocked kind;
/// an empty container reads its accepts filter so the player knows
/// what NPCs will stow here.
fn format_container_inner(
    cfg: &block_junk_mod_api::blocks::ContainerConfig,
    state: Option<&crate::containers::ContainerState>,
    items: &ItemRegistry,
) -> String {
    let mut out = String::new();
    let used = state.map(|s| s.used_bulk(items)).unwrap_or(0);
    out.push_str(&format!(
        "container: {} / {} bulk\n",
        used, cfg.capacity_bulk
    ));
    if !cfg.accepts.is_empty() {
        let tags = cfg
            .accepts
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("accepts: {tags}\n"));
    }
    match state {
        Some(s) if !s.is_empty() => {
            // Sorted by display name so the list doesn't reshuffle
            // per frame (HashMap iteration order is arbitrary).
            let mut lines: Vec<(String, u32)> = s
                .inventory
                .iter()
                .map(|(slot, count)| (items.def(*slot).display_name.clone(), *count))
                .collect();
            lines.sort();
            for (name, count) in lines {
                out.push_str(&format!("  {count}x {name}\n"));
            }
        }
        _ => out.push_str("  (empty)\n"),
    }
    out
}

/// Render the station-tag + recipes list when the inspected block is
/// a station. Recipes show input → output so the player sees what
/// kind of work this station does without trial-and-error.
fn format_station_inner(
    tag: &block_junk_mod_api::blocks::TagId,
    recipes: &crate::recipes::RecipeRegistry,
    items: &ItemRegistry,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("station: {tag}\n"));
    let slots = recipes.at_station(tag);
    if slots.is_empty() {
        out.push_str("recipes: (none registered)\n");
        return out;
    }
    out.push_str("recipes:\n");
    for &slot in slots {
        let def = recipes.def(slot);
        let inputs_str = if def.inputs.is_empty() {
            "(free)".to_owned()
        } else {
            def.inputs
                .iter()
                .map(|i| {
                    let name = items
                        .slot_of(&i.item)
                        .map(|s| items.def(s).display_name.clone())
                        .unwrap_or_else(|| i.item.to_string());
                    format!("{}x {}", i.count, name)
                })
                .collect::<Vec<_>>()
                .join(" + ")
        };
        let out_name = items
            .slot_of(&def.output.item)
            .map(|s| items.def(s).display_name.clone())
            .unwrap_or_else(|| def.output.item.to_string());
        out.push_str(&format!(
            "  {} → {}x {} ({:.1}s)\n",
            inputs_str, def.output.count, out_name, def.duration_secs
        ));
    }
    out
}

/// Render the kind + materials list for a [`PlanState`]. No header —
/// callers prepend their own ("Plan (x,y,z)" or "tagged" depending
/// on the surrounding context).
fn format_plan_inner(
    state: &crate::protocol::PlanState,
    registry: &BlockRegistry,
    items: &ItemRegistry,
) -> String {
    let mut out = String::new();
    let kind_str = match &state.kind {
        PlanKind::Remove => "tag: remove".to_owned(),
        PlanKind::Build { slot, .. } => {
            // Show the player the block's name, not its internal slot
            // number. Bounds-check before `def` (which panics on
            // unknown slots) so a stale plan from a mismatched save
            // degrades to a readable label instead of a crash.
            if (slot.0 as usize) < registry.slot_count() {
                format!("tag: build {}", registry.def(*slot).display_name)
            } else {
                format!("tag: build (unknown block, slot {})", slot.0)
            }
        }
    };
    out.push_str(&kind_str);
    out.push('\n');
    if state.materials.is_empty() {
        out.push_str("materials: (none)\n");
    } else {
        out.push_str("materials:\n");
        for m in &state.materials {
            let name = items.def(m.item).display_name.clone();
            out.push_str(&format!("  {}: {}/{}\n", name, m.present, m.needed));
        }
    }
    if state.is_satisfied() {
        out.push_str("status: ready\n");
    } else {
        out.push_str("status: waiting on materials\n");
    }
    out
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
    if !details.stats.is_empty() {
        out.push_str("\nstats:\n");
        let mut entries: Vec<(&String, &f32)> = details.stats.iter().collect();
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
            out.push_str(&format!(
                "  need: {}\n  restores: {:.2}\n",
                nr.need, nr.restores
            ));
        }
        out.push_str(&format!("  duration: {:.1}s\n", i.duration_secs));
        out.push_str(&format!("  exclusive: {}\n", i.exclusive));
    }
    out
}

#[cfg(test)]
mod capacity_inspection_tests {
    use super::*;
    use block_junk_mod_api::capabilities::*;
    #[test]
    fn fully_reserved_facility_shows_support_and_zero_availability() {
        let result: CapabilityResult = serde_json::from_value(serde_json::json!({
            "id":"test:sleeping", "display_name":"Sleeping", "status":"supported",
            "requirements":{}, "capacity":Evidence::number("usable".into(), 2.),
            "attributes": {"installed": Evidence::number("installed".into(), 2.), "usable": Evidence::number("usable".into(), 2.), "available": Evidence::number("available".into(), 0.)}
        })).unwrap();
        let mut text = String::new();
        append_space_capabilities(
            &mut text,
            &crate::capabilities::SpaceCapabilities {
                profiles: std::collections::BTreeMap::from([("humanoid".into(), vec![result])]),
            },
        );
        assert!(text.contains("Sleeping: supported"));
        assert!(text.contains("installed: 2"));
        assert!(text.contains("usable: 2"));
        assert!(text.contains("available: 0"));
        assert!(!text.contains("Sleeping: available"));
    }
}
