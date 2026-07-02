//! Shared visual language for every UI surface — the single place
//! colors and egui skins live.
//!
//! Two skins, on purpose:
//!
//! - **In-game** (player-facing): warm dark-brown panels with a beige
//!   accent border and cream text — the look the bevy_ui HUD (inspect
//!   panel, toasts, mode pill) already established. Applied to the
//!   egui *context* style, so player-facing egui windows (main menu,
//!   pause menu, craft modal) pick it up without per-window code.
//! - **Dev** (debug-facing): flat near-black, square corners, thin
//!   neutral border, monospace text. Debug surfaces opt in per-window
//!   via [`dev_frame`] + [`dev_text_style`] so they read as tools, not
//!   game UI, even though they share the egui context.
//!
//! bevy_ui HUD files import the `Color` constants instead of hand-coding
//! srgba values; egui windows go through the two skin helpers. Adding a
//! third hand-picked panel color anywhere else is a bug.

use bevy::prelude::Color;
use bevy_egui::egui;

// ---------------------------------------------------------------------
// In-game palette (bevy_ui side).
// ---------------------------------------------------------------------

/// Standard panel background: warm dark brown, mostly opaque.
pub const PANEL_BG: Color = Color::srgba(0.10, 0.08, 0.07, 0.88);
/// Accent border: warm beige, semi-transparent. The "this is game UI"
/// signature stroke.
pub const PANEL_BORDER: Color = Color::srgba(0.95, 0.85, 0.55, 0.45);
/// Primary text: warm cream.
pub const TEXT: Color = Color::srgb(0.95, 0.92, 0.85);
/// Secondary text: dimmed cream for hints and annotations.
pub const TEXT_DIM: Color = Color::srgba(0.92, 0.92, 0.88, 0.85);
/// Key-cap / small-chip background: near-black, readable over terrain.
pub const CHIP_BG: Color = Color::srgba(0.05, 0.05, 0.05, 0.72);
/// Hairline border for chips and key caps.
pub const CHIP_BORDER: Color = Color::srgba(1.0, 1.0, 1.0, 0.22);
/// Craft-station accent (matches the Normal-mode station outline).
pub const ACCENT_STATION: Color = Color::srgb(0.78, 0.42, 0.95);

// ---------------------------------------------------------------------
// egui conversions + skin helpers.
// ---------------------------------------------------------------------

/// Convert a bevy `Color` to an egui `Color32` (premultiplication is
/// handled by egui's `from_rgba_unmultiplied`).
pub fn egui_color(c: Color) -> egui::Color32 {
    let s = c.to_srgba();
    egui::Color32::from_rgba_unmultiplied(
        (s.red * 255.0) as u8,
        (s.green * 255.0) as u8,
        (s.blue * 255.0) as u8,
        (s.alpha * 255.0) as u8,
    )
}

/// Apply the in-game skin to the egui context. Called once per context
/// at startup (alongside the fallback-font install). Player-facing
/// windows then need zero styling code; dev windows override locally.
pub fn apply_ingame_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    let v = &mut style.visuals;

    let panel = egui::Color32::from_rgba_unmultiplied(26, 20, 18, 245);
    let accent = egui_color(PANEL_BORDER);
    let text = egui_color(TEXT);

    v.override_text_color = Some(text);
    v.window_fill = panel;
    v.panel_fill = panel;
    v.window_stroke = egui::Stroke::new(1.0, accent);
    v.window_corner_radius = egui::CornerRadius::same(8);
    v.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 240, 200, 8);
    v.extreme_bg_color = egui::Color32::from_rgba_unmultiplied(12, 9, 8, 245);
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(199, 107, 242, 110);

    // Widgets: warm dark fills, beige strokes that brighten on hover.
    let warm = |l: u8, a: u8| egui::Color32::from_rgba_unmultiplied(l, l - l / 6, l - l / 4, a);
    for (w, fill, stroke_alpha) in [
        (&mut v.widgets.noninteractive, warm(36, 200), 60u8),
        (&mut v.widgets.inactive, warm(52, 230), 90),
        (&mut v.widgets.hovered, warm(72, 240), 160),
        (&mut v.widgets.active, warm(90, 255), 220),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = egui::Stroke::new(1.0, accent.gamma_multiply(stroke_alpha as f32 / 255.0));
        w.fg_stroke = egui::Stroke::new(1.0, text);
        w.corner_radius = egui::CornerRadius::same(4);
    }

    ctx.set_global_style(style);
}

/// Root `Ui` over the background layer, covering the whole viewport.
/// egui 0.34 deprecated showing panels directly on a `Context`; fullscreen
/// surfaces (main menu, studio chrome) build this and `show_inside` it.
pub fn root_ui(ctx: &egui::Context, id: &'static str) -> egui::Ui {
    egui::Ui::new(
        ctx.clone(),
        id.into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    )
}

/// Window frame for dev/debug surfaces: flat near-black, square-ish
/// corners, thin neutral stroke. Pass to `egui::Window::frame(..)`.
pub fn dev_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(egui::Color32::from_rgba_unmultiplied(18, 20, 22, 235))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(120, 130, 140, 160),
        ))
        .corner_radius(egui::CornerRadius::same(2))
        .inner_margin(egui::Margin::same(8))
}

/// Apply the dev treatment inside a dev window's closure: monospace
/// everywhere, muted-gray labels — and *clearly raised* interactive
/// widgets. A blanket text-color override made buttons and labels read
/// identically; instead, plain text takes the noninteractive tier's
/// gray while buttons/checkboxes sit on slate fills with visible
/// borders and near-white text, brightening through hover/active.
pub fn dev_skin(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.override_text_style = Some(egui::TextStyle::Monospace);
    let v = &mut style.visuals;
    v.override_text_color = None;
    // Labels / separators: muted gray, flat.
    v.widgets.noninteractive.fg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(160, 170, 175));
    v.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 78, 84));
    // Interactive tiers: cool slate fill + 1px border + bright text,
    // each stepping lighter so hover/press feedback is obvious.
    let slate = |l: u8| egui::Color32::from_rgb(l, l + 4, l + 8);
    for (w, fill, border, text) in [
        (
            &mut v.widgets.inactive,
            slate(52),
            egui::Color32::from_rgb(115, 125, 135),
            egui::Color32::from_rgb(225, 230, 235),
        ),
        (
            &mut v.widgets.hovered,
            slate(70),
            egui::Color32::from_rgb(155, 165, 175),
            egui::Color32::WHITE,
        ),
        (
            &mut v.widgets.active,
            slate(88),
            egui::Color32::from_rgb(195, 205, 215),
            egui::Color32::WHITE,
        ),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = egui::Stroke::new(1.0, border);
        w.fg_stroke = egui::Stroke::new(1.0, text);
        w.corner_radius = egui::CornerRadius::same(2);
    }
}
