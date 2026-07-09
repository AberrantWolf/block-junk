//! Top-level player intent: which "tool" the player is wielding.
//!
//! Three modes: `Normal` (avatar default — L mines/picks/deposits/
//! works, R opens menus / interacts), `Plan` (DF-style designation
//! — L tags-for-remove or un-tags, R places Build tags), and
//! `Storage` (zone painting — R paints storage floor cells, L
//! erases; see `storage.rs`). Tab / Shift+Tab cycles.
//!
//! Two on-screen surfaces live here:
//!   - bottom-left "mode pill": icon + label naming the current mode.
//!   - centre-bottom verb-hint chip: short summary of what L and R do
//!     this frame, sitting just below the crosshair so the eye picks
//!     up the disambiguation without scanning to a corner.
//!
//! See `feedback_player_input_scheme` memory for the full target ×
//! verb matrix.

use bevy::prelude::*;

use crate::menu::AppState;
use crate::protocol::GameSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Resource)]
pub enum PlayerMode {
    Normal,
    Plan,
    Storage,
}

impl Default for PlayerMode {
    fn default() -> Self {
        Self::Normal
    }
}

impl PlayerMode {
    pub const ALL: [PlayerMode; 3] = [PlayerMode::Normal, PlayerMode::Plan, PlayerMode::Storage];

    pub fn label(self) -> &'static str {
        match self {
            PlayerMode::Normal => "Normal",
            PlayerMode::Plan => "Plan",
            PlayerMode::Storage => "Storage",
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            PlayerMode::Normal => "ui/mode_icons/hand_point.png",
            PlayerMode::Plan => "ui/mode_icons/drawing_pencil.png",
            PlayerMode::Storage => "ui/mode_icons/basket.png",
        }
    }

    /// Short summary of the L/R verbs in this mode for the crosshair
    /// hint chip. Kept terse — the player learns it once and the chip
    /// becomes a recall aid. Plan mode gets a second line for the
    /// wheel verbs, which have no other in-world surface.
    pub fn verb_hint(self) -> &'static str {
        match self {
            PlayerMode::Normal => "L: mine · R: interact",
            PlayerMode::Plan => {
                "L: remove · R: build\n1-9/wheel: block · B: palette · Ctrl+wheel: rotate"
            }
            PlayerMode::Storage => "L: clear storage · R: mark storage (drag on ground)",
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
                (handle_mode_input, refresh_mode_pill, refresh_mode_hints).in_set(GameSet::Input),
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
            DespawnOnExit(AppState::InGame),
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
            BackgroundColor(crate::ui_theme::CHIP_BG),
            BorderColor::all(crate::ui_theme::CHIP_BORDER),
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
                    font_size: FontSize::Px(18.0),
                    ..default()
                },
                TextColor(crate::ui_theme::TEXT),
                ModePillLabel,
            ));
        });

    spawn_mode_hints(&mut commands, &asset_server, *mode);
    spawn_verb_hint(&mut commands, *mode);
}

/// Crosshair-adjacent verb hint. A small chip just below the screen
/// centre that names the L and R verbs in the current mode. Always
/// on-screen so the player can flick their gaze to it without scanning
/// to a corner; refreshed by `refresh_verb_hint` on mode change.
fn spawn_verb_hint(commands: &mut Commands, mode: PlayerMode) {
    // Anchor: take up the full screen width, centre the chip
    // horizontally, push it ~40 px below the vertical centre so it
    // sits just under the crosshair without occluding it.
    commands
        .spawn((
            VerbHintRoot,
            DespawnOnExit(AppState::InGame),
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                margin: UiRect::top(Val::Px(40.0)),
                justify_content: JustifyContent::Center,
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(3.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(crate::ui_theme::CHIP_BG),
                BorderColor::all(crate::ui_theme::CHIP_BORDER),
            ))
            .with_children(|chip| {
                chip.spawn((
                    Text::new(mode.verb_hint()),
                    TextFont {
                        font_size: FontSize::Px(12.0),
                        ..default()
                    },
                    TextColor(crate::ui_theme::TEXT_DIM),
                    VerbHintLabel,
                ));
            });
        });
}

/// Compact hint strip sitting just above the mode pill: one `Tab` key
/// cap (the cycle binding) followed by the mode icons in cycle order.
/// The icon for the *current* mode is lit (full colour + accent chip);
/// the others are dimmed, so the strip doubles as a "you are here /
/// what's next" indicator. Digit keys belong to the hotbar now, so no
/// per-mode key caps. Always-on; cheap to leave in the HUD.
fn spawn_mode_hints(commands: &mut Commands, asset_server: &AssetServer, current: PlayerMode) {
    commands
        .spawn((
            ModeHintsRoot,
            DespawnOnExit(AppState::InGame),
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
                let (tint, bg, border) = mode_hint_visual(m == current);
                row.spawn((
                    ModeHintIcon(m),
                    ImageNode::new(asset_server.load(m.icon_path())).with_color(tint),
                    Node {
                        width: Val::Px(18.0),
                        height: Val::Px(18.0),
                        // Always reserve the padding + border box so the
                        // active/inactive swap only changes colour, never
                        // layout (no reflow jitter as you Tab through).
                        padding: UiRect::all(Val::Px(3.0)),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(bg),
                    BorderColor::all(border),
                ));
            }
        });
}

/// Colours for a mode-hint icon in its active / inactive states, as
/// `(image tint, chip background, chip border)`. Shared by the initial
/// spawn and [`refresh_mode_hints`] so the two can never drift.
fn mode_hint_visual(active: bool) -> (Color, Color, Color) {
    if active {
        (
            Color::WHITE,
            crate::ui_theme::CHIP_ACTIVE_BG,
            crate::ui_theme::PANEL_BORDER,
        )
    } else {
        // Tint the icon down toward transparent so unselected modes read
        // as "available but not current" without a second background.
        (Color::srgba(1.0, 1.0, 1.0, 0.4), Color::NONE, Color::NONE)
    }
}

/// Repaint the mode-hint strip when the mode changes: light up the new
/// mode's icon, dim the rest. Mirrors `update_hotbar_highlight`.
fn refresh_mode_hints(
    mode: Res<PlayerMode>,
    mut icons: Query<(&ModeHintIcon, &mut ImageNode, &mut BackgroundColor, &mut BorderColor)>,
) {
    if !mode.is_changed() {
        return;
    }
    for (hint, mut image, mut bg, mut border) in icons.iter_mut() {
        let (tint, bg_c, border_c) = mode_hint_visual(hint.0 == *mode);
        image.color = tint;
        *bg = BackgroundColor(bg_c);
        *border = BorderColor::all(border_c);
    }
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
            BackgroundColor(crate::ui_theme::CHIP_BG),
            BorderColor::all(crate::ui_theme::CHIP_BORDER),
        ))
        .with_children(|cap| {
            cap.spawn((
                Text::new(label),
                TextFont {
                    font_size: FontSize::Px(12.0),
                    ..default()
                },
                TextColor(crate::ui_theme::TEXT),
            ));
        });
}

#[derive(Component)]
struct ModeHintsRoot;

/// One icon in the mode-hint strip, tagged with the mode it represents so
/// [`refresh_mode_hints`] can find and light the current one.
#[derive(Component)]
struct ModeHintIcon(PlayerMode);

#[derive(Component)]
struct VerbHintRoot;

#[derive(Component)]
struct VerbHintLabel;

fn handle_mode_input(
    keys: Res<ButtonInput<KeyCode>>,
    captures: Res<crate::ui_capture::UiCaptures>,
    mut mode: ResMut<PlayerMode>,
) {
    // SSOT input gate: while any overlay holds the cursor the keys
    // belong to the UI, not gameplay. Gating on `CursorOptions.grab_mode`
    // would read a *derived* value one frame late — captures is the
    // source of truth.
    if captures.is_captured() {
        return;
    }

    // Tab / Shift+Tab is the whole binding — digit keys belong to the
    // hotbar (`client::hotbar_digit_select`), which is why the old
    // 1/2 direct-select was retired.
    if keys.just_pressed(KeyCode::Tab) {
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let next = mode.cycle(!shift);
        if next != *mode {
            *mode = next;
        }
    }
}

fn refresh_mode_pill(
    mode: Res<PlayerMode>,
    asset_server: Res<AssetServer>,
    mut icons: Query<&mut ImageNode, With<ModePillIcon>>,
    mut labels: Query<&mut Text, (With<ModePillLabel>, Without<VerbHintLabel>)>,
    mut verb_labels: Query<&mut Text, (With<VerbHintLabel>, Without<ModePillLabel>)>,
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
    for mut text in verb_labels.iter_mut() {
        text.0 = mode.verb_hint().to_string();
    }
}
