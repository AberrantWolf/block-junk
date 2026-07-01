use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;

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
        // Cursor lock/release is fully owned by [`crate::ui_capture`]
        // — including the AppState transition boundaries. This plugin
        // never touches `CursorOptions`; it only *reads* the capture
        // state to gate mouse-look.
        app.add_systems(Update, fly_cam_input.run_if(in_state(AppState::InGame)));
    }
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
