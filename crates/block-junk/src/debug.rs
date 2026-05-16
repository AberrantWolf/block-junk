//! Dev-only debug panel.
//!
//! Two halves, one per side of the always-client architecture:
//!
//! - [`DebugClientPlugin`] runs on the client. Owns an egui window
//!   (toggled by F3) with buttons for the things that are hard to
//!   wait for in a normal session — jumping the world clock to a
//!   specific time of day, fast-forwarding NPC needs so behaviour
//!   triggers without waiting minutes. Each button packs a small
//!   [`DebugSetClock`] / [`DebugBumpNeed`] message and sends it on
//!   [`WorldChannel`].
//! - [`DebugServerPlugin`] runs on the server. Receives those
//!   messages and applies them to the authoritative state
//!   (`WorldClock` resource, NPC `Needs` components).
//!
//! Eventually some of this becomes slash-commands once we add a chat
//! input; until then, buttons are the cheapest UX that doesn't need
//! text entry.
//!
//! **No permission gate today.** Anyone connected to a server can
//! send these messages. Gate on an auth/role flag once we have one.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow, Window};
use bevy_egui::{EguiContexts, egui};
use lightyear::prelude::*;

use crate::menu::AppState;
use crate::npc::{Needs, Npc};
use crate::npc_registry::NeedRegistry;
use crate::protocol::{
    DAY_LENGTH_SECS, DebugAdvanceTime, DebugBumpNeed, GameSet, WorldChannel, WorldClock,
};

pub struct DebugClientPlugin;

impl Plugin for DebugClientPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DebugPanelOpen>();
        app.init_resource::<InstantPlayerBuilds>();
        app.add_systems(
            Update,
            toggle_debug_panel
                .in_set(GameSet::Input)
                .run_if(in_state(AppState::InGame)),
        );
        // The UI lives in `EguiPrimaryContextPass`, not `Update`. egui
        // only collects pointer/click events during that schedule —
        // running the panel from `Update` would render the window but
        // every button-click would silently fall through (this bit me
        // the first time). The pause menu in `menu.rs` uses the same
        // schedule for the same reason.
        app.add_systems(
            bevy_egui::EguiPrimaryContextPass,
            debug_panel_ui.run_if(in_state(AppState::InGame)),
        );
    }
}

pub struct DebugServerPlugin;

impl Plugin for DebugServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (receive_debug_advance_time, receive_debug_bump_need)
                .in_set(GameSet::Simulation),
        );
    }
}

/// Whether the debug overlay window is currently visible. Persists
/// across pause/unpause (intentional — re-opening it every time you
/// come back from the menu would be tedious during a debug session).
#[derive(Resource, Default)]
pub struct DebugPanelOpen(pub bool);

/// When true, the player's Build/Destroy verbs skip the action timer
/// and resolve immediately — the way clicks worked before mode-gated
/// input landed. Defaults to `true` so the dev workflow today matches
/// pre-Phase-1 behaviour; flip to false (or change the default) once
/// the Phase 5 timer is in and we want timed actions as the baseline.
#[derive(Resource)]
pub struct InstantPlayerBuilds(pub bool);

impl Default for InstantPlayerBuilds {
    fn default() -> Self {
        Self(true)
    }
}

/// Toggle the debug panel on F3 and also un/relock the cursor so the
/// panel's buttons are actually clickable. The cursor-lock toggle
/// mirrors `camera::capture`/`release_cursor` rather than calling them
/// — the camera plugin owns its own discard-next-motion bookkeeping
/// and going through there from a non-state-transition system would
/// risk a phantom mouse delta on re-capture. The minor duplication
/// is the right trade for a dev-only panel.
fn toggle_debug_panel(
    keys: Res<ButtonInput<KeyCode>>,
    mut open: ResMut<DebugPanelOpen>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    if !keys.just_pressed(KeyCode::F3) {
        return;
    }
    open.0 = !open.0;
    let Ok((mut window, mut cursor)) = windows.single_mut() else {
        return;
    };
    if open.0 {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else {
        let centre = Vec2::new(window.resolution.width(), window.resolution.height()) * 0.5;
        window.set_cursor_position(Some(centre));
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Hard-coded list of needs the bump-need buttons offer. Could be
/// driven off the live `NeedRegistry`, but a fixed list keeps the
/// panel layout predictable and avoids the panel jumping around when
/// a mod registers a new need mid-session. Add entries here as new
/// needs become part of the dev workflow.
const BUMPABLE_NEEDS: &[(&str, &str)] = &[
    ("hunger", "Hunger"),
    ("sleep", "Tiredness"),
];

fn debug_panel_ui(
    mut contexts: EguiContexts,
    mut open: ResMut<DebugPanelOpen>,
    mut instant_builds: ResMut<InstantPlayerBuilds>,
    clock: Option<Res<WorldClock>>,
    mut advance_sender: Query<&mut MessageSender<DebugAdvanceTime>>,
    mut need_sender: Query<&mut MessageSender<DebugBumpNeed>>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
) {
    if !open.0 {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    // Snapshot what we'll send before the closure — egui's `show` borrows
    // the response struct and we don't want a long borrow on resources
    // inside the closure.
    let mut advance_secs: Option<f32> = None;
    let mut need_bump: Option<(String, f32)> = None;

    let time_label = match clock.as_deref() {
        Some(c) => format!(
            "day {} · t {:.3} ({})",
            c.day,
            c.time_of_day,
            if c.is_night() { "night" } else { "day" }
        ),
        None => "(clock unset)".to_owned(),
    };
    let current_t = clock.as_deref().map(|c| c.time_of_day).unwrap_or(0.0);
    // Forward delta (in fraction-of-day) to reach `target` from
    // `current_t`. If `target` is "now or moments ago", the user
    // presumably wants the *next* occurrence — i.e. roll forward
    // almost a full day rather than skip 0. `rem_euclid(1.0)` does
    // this naturally; we just guard against the exact-match case
    // by using "next" semantics universally.
    let forward_delta = |target: f32| -> f32 {
        let d = (target - current_t).rem_euclid(1.0);
        // Treat "exactly the current phase" as "advance a full day"
        // so the button still has an effect.
        if d < f32::EPSILON { 1.0 } else { d }
    };

    let mut show_open = open.0;
    egui::Window::new("Debug")
        .open(&mut show_open)
        .anchor(egui::Align2::RIGHT_TOP, egui::Vec2::new(-12.0, 12.0))
        .default_width(280.0)
        .show(ctx, |ui| {
            ui.label("F3 toggles this panel.");
            ui.separator();
            ui.label(egui::RichText::new("Player").strong());
            ui.checkbox(
                &mut instant_builds.0,
                "Instant player builds (skip action timer)",
            );
            ui.separator();
            ui.label(egui::RichText::new("Advance time").strong());
            ui.label(time_label);
            ui.label("Skips the clock forward and ages NPC needs by the same elapsed time.");
            ui.horizontal(|ui| {
                if ui.button("Sunrise").clicked() {
                    advance_secs = Some(forward_delta(0.25) * DAY_LENGTH_SECS);
                }
                if ui.button("Noon").clicked() {
                    advance_secs = Some(forward_delta(0.5) * DAY_LENGTH_SECS);
                }
                if ui.button("Sunset").clicked() {
                    advance_secs = Some(forward_delta(0.75) * DAY_LENGTH_SECS);
                }
                if ui.button("Midnight").clicked() {
                    advance_secs = Some(forward_delta(0.0) * DAY_LENGTH_SECS);
                }
            });
            ui.horizontal(|ui| {
                // One in-game hour = DAY_LENGTH_SECS / 24 real seconds.
                if ui.button("+1h").clicked() {
                    advance_secs = Some(DAY_LENGTH_SECS / 24.0);
                }
                if ui.button("+6h").clicked() {
                    advance_secs = Some(DAY_LENGTH_SECS * 6.0 / 24.0);
                }
            });
            ui.separator();
            ui.label(egui::RichText::new("Bump NPC needs").strong());
            ui.label("Adds to every NPC's need value (1.0 = critical). For when you want to trigger behaviour without waiting on natural decay.");
            for &(id, display) in BUMPABLE_NEEDS {
                ui.horizontal(|ui| {
                    ui.label(format!("{display}:"));
                    if ui.button("+0.3").clicked() {
                        need_bump = Some((id.to_owned(), 0.3));
                    }
                    if ui.button("-0.3").clicked() {
                        need_bump = Some((id.to_owned(), -0.3));
                    }
                    if ui.button("max").clicked() {
                        need_bump = Some((id.to_owned(), 1.0));
                    }
                    if ui.button("zero").clicked() {
                        need_bump = Some((id.to_owned(), -1.0));
                    }
                });
            }
        });
    // If egui's title-bar X just closed the window, mirror the F3
    // path: relock the cursor so the player can keep playing without
    // having to press F3 to re-grab focus.
    if open.0 && !show_open {
        if let Ok((mut window, mut cursor)) = windows.single_mut() {
            let centre =
                Vec2::new(window.resolution.width(), window.resolution.height()) * 0.5;
            window.set_cursor_position(Some(centre));
            cursor.grab_mode = CursorGrabMode::Locked;
            cursor.visible = false;
        }
    }
    open.0 = show_open;

    if let Some(secs) = advance_secs {
        // MessageSender lives on the connection entity; one in solo
        // mode. A missing sender silently drops the request, same
        // pattern as place/break.
        if let Ok(mut sender) = advance_sender.single_mut() {
            sender.send::<WorldChannel>(DebugAdvanceTime { secs });
        }
    }
    if let Some((need, delta)) = need_bump {
        if let Ok(mut sender) = need_sender.single_mut() {
            sender.send::<WorldChannel>(DebugBumpNeed { need, delta });
        }
    }
}

/// Apply [`DebugAdvanceTime`] — fast-forward the world by `secs`
/// real-time seconds. Two effects, applied atomically so the clock
/// and NPC needs stay coherent:
///
/// 1. The [`WorldClock`] rolls forward by `secs / DAY_LENGTH_SECS`,
///    wrapping into the next day if it crosses midnight.
/// 2. Each NPC's needs decay by `decay_per_sec * secs` per entry —
///    mirrors the per-tick decay loop in `npc::npc_brain_tick` so
///    skipping 6 hours has the same effect as 6 hours of natural
///    play, modulo the brain's behaviour during that span.
///
/// Negative or NaN payloads clamp to 0 (no rewind support — see
/// the protocol-side comment).
fn receive_debug_advance_time(
    mut receivers: Query<&mut MessageReceiver<DebugAdvanceTime>>,
    mut clock: ResMut<WorldClock>,
    mut npcs: Query<&mut Needs, With<Npc>>,
    needs_registry: Res<NeedRegistry>,
) {
    for mut receiver in receivers.iter_mut() {
        for msg in receiver.receive() {
            let secs = msg.secs.max(0.0);
            if secs == 0.0 || !secs.is_finite() {
                continue;
            }
            clock.advance(secs);
            // Per-NPC need decay over the elapsed window. Same body
            // as the brain's Phase 1 loop, just driven by the
            // skip-duration rather than per-tick `dt`.
            for mut needs in npcs.iter_mut() {
                for (id, value) in needs.0.iter_mut() {
                    let decay = needs_registry.decay_per_sec(id);
                    *value = (*value + decay * secs).clamp(0.0, 1.0);
                }
            }
            info!(
                secs,
                day = clock.day,
                time_of_day = clock.time_of_day,
                "debug: advanced world time",
            );
        }
    }
}

/// Apply [`DebugBumpNeed`] to every NPC's `Needs` map. Iterates all
/// NPCs intentionally — the debug workflow is "fast-forward the
/// world," not "edit one specific entity." A future per-NPC variant
/// would need an NpcId in the payload + a target picker UI; not
/// worth the complexity yet.
///
/// `delta` is added to whatever value the NPC currently carries,
/// then clamped to `[0, 1]`. NPCs that don't subscribe to the
/// need are skipped — bumping `sleep` on a kind that doesn't have
/// it shouldn't fabricate the entry, since the brain's decay loop
/// would then carry it forever as a no-op key.
fn receive_debug_bump_need(
    mut receivers: Query<&mut MessageReceiver<DebugBumpNeed>>,
    mut npcs: Query<&mut Needs, With<Npc>>,
    needs_registry: Res<NeedRegistry>,
) {
    for mut receiver in receivers.iter_mut() {
        for msg in receiver.receive() {
            if !needs_registry.contains(&msg.need) {
                warn!(
                    need = %msg.need,
                    "debug bump: unknown need id; ignoring",
                );
                continue;
            }
            let mut touched = 0usize;
            for mut needs in npcs.iter_mut() {
                if let Some(v) = needs.0.get_mut(&msg.need) {
                    *v = (*v + msg.delta).clamp(0.0, 1.0);
                    touched += 1;
                }
            }
            info!(
                need = %msg.need,
                delta = msg.delta,
                touched,
                "debug: bumped need on NPCs",
            );
        }
    }
}
