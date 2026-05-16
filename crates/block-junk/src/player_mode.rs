//! Top-level player intent: which "tool" the player is wielding.
//!
//! The mode gates what L-click / R-click / wheel actually do in the rest
//! of the client (Phase 1+). This module owns just the state machine and
//! a small HUD chip showing the active mode. Default is `Select`; Tab
//! cycles forward, Shift+Tab reverses, and the number keys 1..=4 select
//! a mode directly.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::menu::AppState;
use crate::protocol::GameSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Resource)]
pub enum PlayerMode {
    Select,
    Plan,
    Build,
    Destroy,
}

impl Default for PlayerMode {
    fn default() -> Self {
        Self::Select
    }
}

impl PlayerMode {
    pub const ALL: [PlayerMode; 4] = [
        PlayerMode::Select,
        PlayerMode::Plan,
        PlayerMode::Build,
        PlayerMode::Destroy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PlayerMode::Select => "Select",
            PlayerMode::Plan => "Plan",
            PlayerMode::Build => "Build",
            PlayerMode::Destroy => "Destroy",
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            PlayerMode::Select => "ui/mode_icons/hand_point.png",
            PlayerMode::Plan => "ui/mode_icons/drawing_pencil.png",
            PlayerMode::Build => "ui/mode_icons/tool_hammer.png",
            PlayerMode::Destroy => "ui/mode_icons/tool_pickaxe.png",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }

    fn cycle(self, forward: bool) -> Self {
        let len = Self::ALL.len();
        let idx = self.index();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        Self::ALL[next]
    }
}

pub struct PlayerModePlugin;

impl Plugin for PlayerModePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlayerMode>()
            .add_systems(OnEnter(AppState::InGame), spawn_mode_pill)
            .add_systems(
                Update,
                (handle_mode_input, refresh_mode_pill).in_set(GameSet::Input),
            );
    }
}

#[derive(Component)]
struct ModePillRoot;

#[derive(Component)]
struct ModePillIcon;

#[derive(Component)]
struct ModePillLabel;

fn spawn_mode_pill(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mode: Res<PlayerMode>,
    existing: Query<(), With<ModePillRoot>>,
) {
    // OnEnter(InGame) re-fires on un-pause. The pill outlives pause, so
    // skip the respawn or we'd stack a duplicate chip per resume.
    if !existing.is_empty() {
        return;
    }
    let icon: Handle<Image> = asset_server.load(mode.icon_path());
    commands
        .spawn((
            ModePillRoot,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(16.0),
                left: Val::Px(16.0),
                padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                column_gap: Val::Px(8.0),
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.72)),
            BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.2)),
        ))
        .with_children(|pill| {
            pill.spawn((
                ImageNode::new(icon),
                Node {
                    width: Val::Px(28.0),
                    height: Val::Px(28.0),
                    ..default()
                },
                ModePillIcon,
            ));
            pill.spawn((
                Text::new(mode.label()),
                TextFont {
                    font_size: 18.0,
                    ..default()
                },
                TextColor(Color::WHITE),
                ModePillLabel,
            ));
        });
}

fn handle_mode_input(
    keys: Res<ButtonInput<KeyCode>>,
    cursors: Query<&CursorOptions, With<PrimaryWindow>>,
    mut mode: ResMut<PlayerMode>,
) {
    // Same locked-cursor gate the rest of input uses: while the cursor is
    // free (paused, alt-tabbed) the keys belong to the menu, not gameplay.
    let locked = cursors
        .single()
        .map(|c| c.grab_mode != CursorGrabMode::None)
        .unwrap_or(false);
    if !locked {
        return;
    }

    if keys.just_pressed(KeyCode::Tab) {
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let next = mode.cycle(!shift);
        if next != *mode {
            *mode = next;
        }
        return;
    }

    let direct = if keys.just_pressed(KeyCode::Digit1) {
        Some(PlayerMode::Select)
    } else if keys.just_pressed(KeyCode::Digit2) {
        Some(PlayerMode::Plan)
    } else if keys.just_pressed(KeyCode::Digit3) {
        Some(PlayerMode::Build)
    } else if keys.just_pressed(KeyCode::Digit4) {
        Some(PlayerMode::Destroy)
    } else {
        None
    };
    if let Some(m) = direct
        && m != *mode
    {
        *mode = m;
    }
}

fn refresh_mode_pill(
    mode: Res<PlayerMode>,
    asset_server: Res<AssetServer>,
    mut icons: Query<&mut ImageNode, With<ModePillIcon>>,
    mut labels: Query<&mut Text, With<ModePillLabel>>,
) {
    if !mode.is_changed() {
        return;
    }
    for mut icon in icons.iter_mut() {
        icon.image = asset_server.load(mode.icon_path());
    }
    for mut text in labels.iter_mut() {
        text.0 = mode.label().to_string();
    }
}
