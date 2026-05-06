use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

#[derive(Component)]
pub struct FlyCam {
    pub speed: f32,
    pub sensitivity: f32,
    pub yaw: f32,
    pub pitch: f32,
}

impl Default for FlyCam {
    fn default() -> Self {
        Self {
            speed: 16.0,
            sensitivity: 0.002,
            yaw: 0.0,
            pitch: 0.0,
        }
    }
}

pub struct FlyCamPlugin;

impl Plugin for FlyCamPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DiscardNextMotion>()
            .add_systems(Startup, lock_cursor)
            .add_systems(Update, (toggle_cursor, fly_cam_input));
    }
}

/// Set on every cursor capture/recentre, cleared by the first nonzero
/// motion that arrives afterwards. macOS's `CGWarpMouseCursorPosition`
/// accumulates the warp distance into the *next user-generated* motion
/// event — which can land many frames later, not the next tick — so a
/// fixed-frame discard isn't enough. Discarding the first nonzero motion
/// after capture catches the synthetic delta whenever it actually shows up.
/// Cost: occasionally drops one legitimate motion frame (~16ms) on
/// platforms that don't add a warp delta. Imperceptible.
#[derive(Resource, Default)]
struct DiscardNextMotion(bool);

fn lock_cursor(
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    if let Ok((mut window, mut cursor)) = windows.single_mut() {
        capture(&mut window, &mut cursor, &mut discard);
    }
}

fn toggle_cursor(
    keys: Res<ButtonInput<KeyCode>>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    let Ok((mut window, mut cursor)) = windows.single_mut() else {
        return;
    };
    if cursor.grab_mode == CursorGrabMode::None {
        capture(&mut window, &mut cursor, &mut discard);
    } else {
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
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    motion: Res<AccumulatedMouseMotion>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mut cam: Query<(&mut FlyCam, &mut Transform)>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    let Ok((mut cam, mut transform)) = cam.single_mut() else {
        return;
    };
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);

    if locked && motion.delta != Vec2::ZERO {
        if discard.0 {
            // First nonzero motion since capture is the warp's phantom delta;
            // skip it once and resume normal processing.
            discard.0 = false;
        } else {
            cam.yaw -= motion.delta.x * cam.sensitivity;
            cam.pitch = (cam.pitch - motion.delta.y * cam.sensitivity).clamp(-1.54, 1.54);
        }
    }

    transform.rotation =
        Quat::from_axis_angle(Vec3::Y, cam.yaw) * Quat::from_axis_angle(Vec3::X, cam.pitch);

    let forward = *transform.forward();
    let right = *transform.right();
    let mut delta = Vec3::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        delta += forward;
    }
    if keys.pressed(KeyCode::KeyS) {
        delta -= forward;
    }
    if keys.pressed(KeyCode::KeyD) {
        delta += right;
    }
    if keys.pressed(KeyCode::KeyA) {
        delta -= right;
    }
    if keys.pressed(KeyCode::Space) {
        delta += Vec3::Y;
    }
    if keys.pressed(KeyCode::ShiftLeft) {
        delta -= Vec3::Y;
    }
    if delta != Vec3::ZERO {
        transform.translation += delta.normalize() * cam.speed * time.delta_secs();
    }
}
