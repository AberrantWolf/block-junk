use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::menu::AppState;
use crate::protocol::AvatarPose;
use crate::ui_capture::{DiscardNextMotion, UiCaptures};

/// Per-camera mouse-look state. The avatar's `AvatarPose.yaw` is the
/// authoritative running yaw; this component holds local-only pitch and
/// `pending_dyaw` — mouse motion accumulated since the last input tick.
/// `buffer_input` drains `pending_dyaw` into the next `MovementIntent`, the
/// controller adds it to `pose.yaw`, and `fly_cam_input` shows the sum
/// at render rate so the camera tracks the mouse without waiting for
/// the next FixedUpdate.
#[derive(Component)]
pub struct FlyCam {
    pub sensitivity: f32,
    pub pitch: f32,
    pub pending_dyaw: f32,
}

impl Default for FlyCam {
    fn default() -> Self {
        Self {
            sensitivity: 0.002,
            pitch: 0.0,
            pending_dyaw: 0.0,
        }
    }
}

pub struct FlyCamPlugin;

impl Plugin for FlyCamPlugin {
    fn build(&self, app: &mut App) {
        // Cursor capture is bound to AppState transitions: entering
        // InGame locks the cursor, leaving releases. During gameplay
        // the [`UiCaptures`] SSOT owns the cursor — overlays
        // acquire/release captures and the apply_cursor_mode system
        // handles the window. lock_cursor / release_cursor here only
        // bridge the state-transition boundary.
        app.add_systems(OnEnter(AppState::InGame), lock_cursor)
            .add_systems(OnExit(AppState::InGame), release_cursor)
            .add_systems(Update, fly_cam_input.run_if(in_state(AppState::InGame)));
    }
}

fn lock_cursor(
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    if let Ok((mut window, mut cursor)) = windows.single_mut() {
        capture(&mut window, &mut cursor, &mut discard);
    }
}

fn release_cursor(mut cursors: Query<&mut CursorOptions, With<PrimaryWindow>>) {
    if let Ok(mut cursor) = cursors.single_mut() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

/// Lock the cursor *and* yank it to the window centre. Without the recentre,
/// a click immediately after capture lands at whatever screen position the
/// cursor was at — often outside the game window — activating other apps.
fn capture(window: &mut Window, cursor: &mut CursorOptions, discard: &mut DiscardNextMotion) {
    let centre = Vec2::new(window.resolution.width(), window.resolution.height()) * 0.5;
    window.set_cursor_position(Some(centre));
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
    discard.0 = true;
}

fn fly_cam_input(
    motion: Res<AccumulatedMouseMotion>,
    captures: Res<UiCaptures>,
    mut cam: Query<(&mut FlyCam, &mut Transform, &AvatarPose)>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    let Ok((mut cam, mut transform, pose)) = cam.single_mut() else {
        return;
    };
    // Single SSOT check: mouse-look fires when no overlay is holding
    // the cursor. The cursor's actual `grab_mode` is downstream of
    // this state and should not be queried as the source of truth.
    let active = !captures.is_captured();

    // Mouse-look only — translation goes through MovementIntent → the shared
    // controller now, so WASD / Space / Shift drive the avatar in both
    // walk and fly modes via the input pipeline.
    if active && motion.delta != Vec2::ZERO {
        if discard.0 {
            // First nonzero motion since capture is the warp's phantom delta;
            // skip it once and resume normal processing.
            discard.0 = false;
        } else {
            cam.pending_dyaw -= motion.delta.x * cam.sensitivity;
            cam.pitch = (cam.pitch - motion.delta.y * cam.sensitivity).clamp(-1.54, 1.54);
        }
    }

    // Visible yaw = authoritative pose.yaw plus mouse motion accumulated
    // since the last `buffer_input` drain. The next FixedUpdate will fold
    // pending_dyaw into pose.yaw and reset it; the rendered camera stays
    // continuous across that handoff because the sum is the same.
    let visible_yaw = pose.yaw + cam.pending_dyaw;
    transform.rotation =
        Quat::from_axis_angle(Vec3::Y, visible_yaw) * Quat::from_axis_angle(Vec3::X, cam.pitch);
}
