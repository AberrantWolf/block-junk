//! Worldspace-anchored progress bars for craft stations.
//!
//! Each station with an `active_work` gets a bevy_ui Node whose
//! screen position tracks the station cell via
//! [`Camera::world_to_viewport`]. The fill child's width is driven
//! off `elapsed_secs / total_secs`. Spawn / despawn happens whenever
//! the set of active stations changes (no asset churn — the bar is
//! plain UI nodes + flat colors, no images or text).
//!
//! Worldspace-tracked UI lives in two pieces:
//! - The bar root carries `WorldspaceProgressBar { station_cell }` so
//!   the update system can correlate it back to a station.
//! - The fill child carries `WorldspaceProgressBarFill` so the update
//!   system can find it via `Children`.
//!
//! The `WorldspaceProgressBars` resource is the entity-by-cell index;
//! the update system uses it for O(1) spawn-or-update decisions.
//! When a station's `active_work` clears (or the station disappears)
//! the corresponding entity is despawned and its entry removed.
//!
//! Why not gizmos: `Gizmos::line` is a 1-px screen-space stroke that
//! depth-tests against world geometry and reads as a thin smudge from
//! more than a couple metres away. The bar gets stacked-lines hacky
//! enough to look broken (and indistinguishable from a missing-texture
//! pink when the backdrop washes out). bevy_ui renders crisp 2D quads
//! in the HUD layer, which is the right pass for "always visible,
//! reads clearly at any distance" UI.

use bevy::prelude::*;
use std::collections::HashMap;

use crate::craft_stations::CraftStations;
use crate::menu::AppState;
use crate::protocol::GameSet;

/// Bar size in logical pixels. Roughly mirrors the on-crosshair
/// `ActionProgressBar`'s dimensions for visual consistency, but the
/// bar is offset from the crosshair via worldspace projection rather
/// than centred on the screen.
const BAR_WIDTH_PX: f32 = 64.0;
const BAR_HEIGHT_PX: f32 = 8.0;
/// Worldspace Y offset above the station cell origin. The cell
/// occupies `y..y+1`; +1.3 sits clear of the top face so the bar
/// doesn't z-fight with the block.
const BAR_Y_OFFSET: f32 = 1.3;

/// Marker on the root UI node of one station's progress bar. The
/// stored cell lets [`update_station_progress_bars`] find the right
/// `StationState` each frame.
#[derive(Component, Debug)]
struct WorldspaceProgressBar {
    station_cell: IVec3,
}

/// Marker on the fill child. The update system mutates its
/// `Node.width` to reflect progress percent.
#[derive(Component, Debug)]
struct WorldspaceProgressBarFill;

/// Server-broadcast → client mirror lookup. Keyed by station cell so
/// we can answer "does this station already have a bar?" without
/// scanning the UI tree. Cleared when the bar entity despawns.
#[derive(Resource, Default, Debug)]
struct WorldspaceProgressBars {
    by_cell: HashMap<IVec3, Entity>,
}

pub struct CraftProgressHudPlugin;

impl Plugin for CraftProgressHudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldspaceProgressBars>();
        // PostSimulation runs after the camera's frame-interpolated
        // transform is finalized, so `world_to_viewport` projects
        // against the same camera pose the renderer will use this
        // frame — no one-frame lag between block and bar.
        app.add_systems(
            Update,
            update_station_progress_bars
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

/// Per-frame system: ensure each station with `active_work` has a
/// bar entity, update its position + fill width, despawn bars whose
/// stations no longer have work.
///
/// Behind-camera or off-screen stations get the bar hidden via
/// `Visibility::Hidden` (rather than despawned) so brief view
/// occlusion doesn't churn UI entities every camera turn. Despawn
/// only fires when the underlying `active_work` goes away.
#[allow(
    clippy::too_many_arguments,
    reason = "UI tracker pulls camera + nodes + state"
)]
fn update_station_progress_bars(
    mut commands: Commands,
    stations: Res<CraftStations>,
    mut registry: ResMut<WorldspaceProgressBars>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut bars: Query<(
        Entity,
        &WorldspaceProgressBar,
        &mut Node,
        &mut Visibility,
        &Children,
    )>,
    mut fills: Query<
        &mut Node,
        (
            With<WorldspaceProgressBarFill>,
            Without<WorldspaceProgressBar>,
        ),
    >,
) {
    let Ok((camera, camera_transform)) = cameras.single() else {
        return;
    };

    // Pass 1: spawn bars for stations that don't have one yet.
    for (cell, state) in stations.iter() {
        if state.active_work.is_none() {
            continue;
        }
        if registry.by_cell.contains_key(cell) {
            continue;
        }
        let bar = spawn_bar_entity(&mut commands, *cell);
        registry.by_cell.insert(*cell, bar);
    }

    // Pass 2: update positions + fills for every tracked bar; mark
    // stale entries for removal.
    let mut to_despawn: Vec<IVec3> = Vec::new();
    for (entity, marker, mut node, mut visibility, children) in bars.iter_mut() {
        let cell = marker.station_cell;
        let Some(state) = stations.get(cell) else {
            to_despawn.push(cell);
            continue;
        };
        let Some(active) = state.active_work.as_ref() else {
            to_despawn.push(cell);
            continue;
        };
        let world_pos = Vec3::new(
            cell.x as f32 + 0.5,
            cell.y as f32 + BAR_Y_OFFSET,
            cell.z as f32 + 0.5,
        );
        let screen = camera.world_to_viewport(camera_transform, world_pos);
        match screen {
            Ok(pos) => {
                // Center the bar on the projected point.
                node.left = Val::Px(pos.x - BAR_WIDTH_PX * 0.5);
                node.top = Val::Px(pos.y - BAR_HEIGHT_PX * 0.5);
                *visibility = Visibility::Inherited;
            }
            Err(_) => {
                // Behind near plane or past far plane — hide for now,
                // don't despawn (camera turn is transient).
                *visibility = Visibility::Hidden;
                continue;
            }
        }
        let progress = (active.elapsed_secs / active.total_secs.max(0.001)).clamp(0.0, 1.0);
        for child in children.iter() {
            if let Ok(mut fill_node) = fills.get_mut(child) {
                fill_node.width = Val::Percent(progress * 100.0);
            }
        }
        let _ = entity;
    }

    // Pass 3: despawn bars whose stations are gone or no longer
    // active. Done in a separate pass so the iterator above isn't
    // invalidated mid-loop.
    for cell in to_despawn {
        if let Some(entity) = registry.by_cell.remove(&cell) {
            commands.entity(entity).despawn();
        }
    }
}

fn spawn_bar_entity(commands: &mut Commands, station_cell: IVec3) -> Entity {
    // Root: absolute-positioned narrow rectangle. Position is set on
    // first update tick from world_to_viewport; hidden initially so a
    // freshly-spawned bar doesn't flash in the top-left for one frame
    // before its tracker runs.
    commands
        .spawn((
            WorldspaceProgressBar { station_cell },
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Px(BAR_WIDTH_PX),
                height: Val::Px(BAR_HEIGHT_PX),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(2.0)),
                overflow: Overflow::clip(),
                ..default()
            },
            BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.85)),
            BackgroundColor(crate::ui_theme::PANEL_BG),
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                WorldspaceProgressBarFill,
                Node {
                    width: Val::Percent(0.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                // Same magenta-pink the station outline uses in Normal
                // mode — keeps the visual language consistent ("this
                // is a craft station thing").
                BackgroundColor(crate::ui_theme::ACCENT_STATION),
            ));
        })
        .id()
}
