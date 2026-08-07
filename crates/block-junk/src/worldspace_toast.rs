//! Transient floating-text toasts anchored to a world cell.
//!
//! Click-feedback channel for verbs that don't progress: e.g. L-click
//! on an unqueued craft station fires "No work to do" so the player
//! sees their input was received even when nothing changed. Lifetime
//! ~2s with an alpha fade in the last 0.5s so the toast doesn't
//! linger or harshly pop out.
//!
//! Spawning goes through a `PendingToasts` resource queue. Bevy 0.19's
//! local "buffered events" trait was renamed to `Message`, which
//! conflicts with lightyear's networked `Message` macro — using a
//! plain resource sidesteps the collision and keeps spawn requests
//! ergonomic (just `pending.push(SpawnToast { … })` from any system).
//!
//! Reuses the same `Camera::world_to_viewport` projection pattern as
//! `craft_progress_hud` — different content, same anchoring math.

use bevy::prelude::*;

use crate::menu::AppState;
use crate::protocol::GameSet;

/// Wall-time lifetime of a toast before despawn.
const TOAST_LIFETIME_SECS: f32 = 2.0;
/// Last N seconds of the lifetime are spent fading alpha from 1 to 0.
const TOAST_FADE_TAIL_SECS: f32 = 0.5;
/// Worldspace Y offset above the cell origin. Matches the worldspace
/// progress bar's anchor so toasts read at the same visual height as
/// progress feedback on the same target.
const TOAST_Y_OFFSET: f32 = 1.3;

#[derive(Debug, Clone)]
pub struct SpawnToast {
    pub cell: IVec3,
    pub text: String,
}

/// Single-frame queue of toast spawn requests. Push to this from any
/// input/handler system; `drain_pending_toasts` empties it each
/// PostSimulation tick and spawns the corresponding UI entities.
/// Cheap — duplicate pushes in the same frame just stack toasts on
/// the same cell (the player rarely triggers two toasts in one frame
/// in practice; if they do, they'll see both).
#[derive(Resource, Default, Debug)]
pub struct PendingToasts {
    pub queue: Vec<SpawnToast>,
}

impl PendingToasts {
    pub fn push(&mut self, toast: SpawnToast) {
        self.queue.push(toast);
    }
}

#[derive(Component, Debug)]
struct WorldspaceToast {
    cell: IVec3,
    /// `Time::elapsed_secs()` at spawn — used to compute current age
    /// against `TOAST_LIFETIME_SECS` for fade + despawn.
    spawned_at: f32,
}

#[derive(Component, Debug)]
struct WorldspaceToastText;

pub struct WorldspaceToastPlugin;

impl Plugin for WorldspaceToastPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingToasts>();
        // PostSimulation: after the frame-interpolated camera transform
        // finalises so `world_to_viewport` projects against the same
        // pose the renderer uses (matches craft_progress_hud). Drain
        // before update so a toast pushed this frame still renders
        // this frame.
        app.add_systems(
            Update,
            (drain_pending_toasts, update_toasts)
                .chain()
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
    }
}

fn drain_pending_toasts(
    mut commands: Commands,
    mut pending: ResMut<PendingToasts>,
    time: Res<Time>,
) {
    for toast in pending.queue.drain(..) {
        commands
            .spawn((
                WorldspaceToast {
                    cell: toast.cell,
                    spawned_at: time.elapsed_secs(),
                },
                DespawnOnExit(AppState::InGame),
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(crate::ui_theme::PANEL_BG),
                BorderColor::all(crate::ui_theme::PANEL_BORDER),
                Visibility::Hidden,
            ))
            .with_children(|root| {
                root.spawn((
                    WorldspaceToastText,
                    Text::new(toast.text.clone()),
                    TextFont {
                        font_size: FontSize::Px(14.0),
                        ..default()
                    },
                    TextColor(crate::ui_theme::TEXT),
                ));
            });
    }
}

fn update_toasts(
    mut commands: Commands,
    time: Res<Time>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut toasts: Query<(
        Entity,
        &WorldspaceToast,
        &mut Node,
        &mut Visibility,
        &mut BackgroundColor,
        &mut BorderColor,
        &Children,
    )>,
    mut texts: Query<&mut TextColor, With<WorldspaceToastText>>,
) {
    let Ok((camera, camera_transform)) = cameras.single() else {
        return;
    };
    let now = time.elapsed_secs();
    for (entity, toast, mut node, mut visibility, mut bg, mut border, children) in toasts.iter_mut()
    {
        let age = now - toast.spawned_at;
        if age >= TOAST_LIFETIME_SECS {
            commands.entity(entity).despawn();
            continue;
        }
        let world_pos = Vec3::new(
            toast.cell.x as f32 + 0.5,
            toast.cell.y as f32 + TOAST_Y_OFFSET,
            toast.cell.z as f32 + 0.5,
        );
        match camera.world_to_viewport(camera_transform, world_pos) {
            Ok(pos) => {
                // Anchor the top-left of the toast at the projected
                // point with a small upward bias so the cell stays
                // visible underneath.
                node.left = Val::Px(pos.x - 60.0);
                node.top = Val::Px(pos.y - 28.0);
                *visibility = Visibility::Inherited;
            }
            Err(_) => {
                *visibility = Visibility::Hidden;
                continue;
            }
        }
        // Fade alpha during the tail. The bg/border/text colors all
        // share the same fade factor so the toast reads as a unit
        // dimming, not parts of it disappearing first.
        let fade_start = TOAST_LIFETIME_SECS - TOAST_FADE_TAIL_SECS;
        let alpha = if age <= fade_start {
            1.0
        } else {
            1.0 - ((age - fade_start) / TOAST_FADE_TAIL_SECS).clamp(0.0, 1.0)
        };
        let faded = |c: Color| c.with_alpha(c.alpha() * alpha);
        bg.0 = faded(crate::ui_theme::PANEL_BG);
        *border = BorderColor::all(faded(crate::ui_theme::PANEL_BORDER));
        for child in children.iter() {
            if let Ok(mut tc) = texts.get_mut(child) {
                tc.0 = faded(crate::ui_theme::TEXT);
            }
        }
    }
}
