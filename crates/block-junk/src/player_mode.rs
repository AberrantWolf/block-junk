//! Top-level player intent: which "tool" the player is wielding.
//!
//! The mode gates what L-click / R-click / wheel actually do in the rest
//! of the client (Phase 1+). This module owns just the state machine and
//! a small HUD chip showing the active mode. Default is `Select`; Tab
//! cycles forward, Shift+Tab reverses, and the number keys 1..=3 select
//! a mode directly. The Destroy verb lives in the hotbar as a synthetic
//! slot rather than as its own mode — both Plan and Build read the
//! hotbar to decide what an L-click does.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::menu::AppState;
use crate::protocol::GameSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Resource)]
pub enum PlayerMode {
    Select,
    Plan,
    Build,
}

impl Default for PlayerMode {
    fn default() -> Self {
        Self::Select
    }
}

impl PlayerMode {
    pub const ALL: [PlayerMode; 3] = [
        PlayerMode::Select,
        PlayerMode::Plan,
        PlayerMode::Build,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PlayerMode::Select => "Select/Examine",
            PlayerMode::Plan => "Plan",
            PlayerMode::Build => "Build",
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            PlayerMode::Select => "ui/mode_icons/hand_point.png",
            PlayerMode::Plan => "ui/mode_icons/drawing_pencil.png",
            PlayerMode::Build => "ui/mode_icons/tool_hammer.png",
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

    spawn_mode_hints(&mut commands, &asset_server);
}

/// Compact hint strip sitting just above the mode pill: one `Tab` key
/// cap (the cycle binding) followed by `1`/`2`/`3` key caps each paired
/// with the destination mode's icon. Always-on; cheap to leave in the
/// HUD because the player can stop reading once they've memorised it.
fn spawn_mode_hints(commands: &mut Commands, asset_server: &AssetServer) {
    commands
        .spawn((
            ModeHintsRoot,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(64.0),
                left: Val::Px(16.0),
                column_gap: Val::Px(6.0),
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|row| {
            spawn_key_cap(row, "Tab");
            for m in PlayerMode::ALL {
                let label = match m {
                    PlayerMode::Select => "1",
                    PlayerMode::Plan => "2",
                    PlayerMode::Build => "3",
                };
                row.spawn(Node {
                    column_gap: Val::Px(3.0),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|pair| {
                    spawn_key_cap(pair, label);
                    pair.spawn((
                        ImageNode::new(asset_server.load(m.icon_path())),
                        Node {
                            width: Val::Px(18.0),
                            height: Val::Px(18.0),
                            ..default()
                        },
                    ));
                });
            }
        });
}

/// Small dark "key cap" chip. Used for kbd hint clusters.
fn spawn_key_cap(parent: &mut ChildSpawnerCommands<'_>, label: &str) {
    parent
        .spawn((
            Node {
                padding: UiRect::axes(Val::Px(6.0), Val::Px(2.0)),
                min_width: Val::Px(18.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(3.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.05, 0.05, 0.05, 0.72)),
            BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.25)),
        ))
        .with_children(|cap| {
            cap.spawn((
                Text::new(label),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(Color::srgba(0.9, 0.9, 0.9, 1.0)),
            ));
        });
}

#[derive(Component)]
struct ModeHintsRoot;

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
