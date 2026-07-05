//! Single source of truth for "is a UI overlay holding the cursor?"
//!
//! The principle, learned the hard way from a real bug: cursor lock
//! state and in-world input enable state MUST come from the same
//! piece of state. The previous design had each overlay manually
//! toggling `cursor.grab_mode` and each in-world input system
//! independently checking `is_open()` flags. The two went out of
//! sync when a single Esc press fired both the modal's close-handler
//! AND the pause-menu toggle, leaving the cursor unlocked while the
//! visible modal was gone. Hover-outlines (which don't gate on cursor
//! lock) kept working; in-world clicks (which do) didn't. The bug
//! looked like "L-click pickup is broken" instead of the truer
//! statement "the cursor is unlocked when it shouldn't be."
//!
//! [`UiCaptures`] is the SSOT. Every overlay that wants the cursor
//! [`acquire`](UiCaptures::acquire)s its [`UiCapture`] variant when it
//! opens and [`release`](UiCaptures::release)s when it closes. The
//! single [`apply_cursor_mode`] system reads the resource and applies
//! the window's grab_mode + visibility — no other code touches them.
//! Every in-world input system gates on
//! [`is_captured`](UiCaptures::is_captured) — no `!locked` checks, no
//! per-overlay `is_open()` checks. The cursor is locked ⇔ no captures
//! held ⇔ in-world input is active. One fact, three uses.
//!
//! Adding a new overlay = one new [`UiCapture`] variant and an
//! acquire/release call pair in the overlay's open/close logic.
//! Adding a new in-world input system = one `if captures.is_captured()
//! { return; }` line. There is no third option.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use std::collections::HashSet;

use crate::menu::AppState;

/// One distinct UI overlay that currently wants the cursor. Add a
/// variant when you build a new modal/dialog/in-flight cursor-takeover
/// flow. Variants are compared by equality only — no payload — so
/// "the craft modal is open" doesn't differ from "the craft modal
/// is open for cell (1,2,3)." Per-overlay state lives in that
/// overlay's resource; this enum is just the capture identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiCapture {
    /// Game pause overlay (Esc).
    PauseMenu,
    /// Craft-order modal (F-key on a station).
    CraftModal,
    /// F3 dev panel.
    DebugPanel,
    /// Blocking fatal-error modal (mod-set mismatch etc.). Esc does
    /// not release it — the only way out is the modal's Quit button.
    FatalError,
}

/// The set of overlays currently holding the cursor. Non-empty ⇒
/// cursor is unlocked, in-world input suppressed. Empty ⇒ mouse-look
/// mode, in-world input flowing.
#[derive(Resource, Default, Debug)]
pub struct UiCaptures {
    active: HashSet<UiCapture>,
}

impl UiCaptures {
    /// Mark `capture` as active. Idempotent — re-acquiring an already-
    /// held capture is a no-op (no extra cursor toggling).
    pub fn acquire(&mut self, capture: UiCapture) {
        self.active.insert(capture);
    }

    /// Mark `capture` as no longer active. Idempotent — releasing
    /// something that wasn't held is a no-op.
    pub fn release(&mut self, capture: UiCapture) {
        self.active.remove(&capture);
    }

    /// True when at least one overlay is capturing the cursor. Every
    /// in-world input system gates on `!is_captured()` (or
    /// equivalently early-returns on `is_captured()`).
    pub fn is_captured(&self) -> bool {
        !self.active.is_empty()
    }

    /// True when this specific capture is currently held. Used by an
    /// overlay's UI render system to decide whether to draw, and by
    /// `handle_escape` to dispatch close to the topmost overlay.
    pub fn contains(&self, capture: UiCapture) -> bool {
        self.active.contains(&capture)
    }
}

/// Set on every cursor recentre. The camera's mouse-look reader
/// (`fly_cam_input`) consumes it on the first nonzero motion that
/// arrives afterwards, dropping that one synthetic-warp delta. macOS's
/// `CGWarpMouseCursorPosition` accumulates the warp distance into the
/// *next user-generated* motion event — which can land many frames
/// later — so a fixed-frame discard isn't enough. Owned by this
/// module rather than the camera plugin because the SSOT principle
/// extends here too: the warp happens whenever apply_cursor_mode
/// recentres, which is whenever captures transitions to empty.
#[derive(Resource, Default)]
pub(crate) struct DiscardNextMotion(pub(crate) bool);

/// Apply [`UiCaptures`] to the primary window. The single system that
/// touches `cursor.grab_mode` / `cursor.visible` — every other system
/// goes through `UiCaptures`. Runs every frame to keep the window in
/// sync; `is_changed` short-circuit keeps the cost to one HashSet read
/// plus a comparison on the common path.
fn apply_cursor_mode(
    captures: Res<UiCaptures>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    mut discard: ResMut<DiscardNextMotion>,
) {
    if !captures.is_changed() {
        return;
    }
    let Ok((mut window, mut cursor)) = windows.single_mut() else {
        return;
    };
    if captures.is_captured() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else {
        // Re-centre the cursor on relock so the next mouse delta reads
        // from the middle of the screen, not wherever the player let
        // it drift to while clicking through UI. Set the discard flag
        // so the synthetic warp delta gets dropped by fly_cam_input.
        let centre = Vec2::new(window.resolution.width(), window.resolution.height()) * 0.5;
        window.set_cursor_position(Some(centre));
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
        discard.0 = true;
    }
}

/// On entering [`AppState::InGame`], reset the captures set so any
/// stale flags left over from the previous session (e.g. PauseMenu
/// held when the player quit via the pause menu) don't leak across
/// state transitions and silently disable in-world input. The
/// `set_changed()` call forces Bevy's change detection to fire even
/// when the set was already empty — that way the next
/// [`apply_cursor_mode`] tick locks the cursor (with recentre +
/// motion discard) regardless of whether `clear` actually mutated
/// anything.
fn reset_captures_on_ingame_enter(mut captures: ResMut<UiCaptures>) {
    captures.active.clear();
    captures.set_changed();
}

/// On leaving [`AppState::InGame`], hand the cursor back to the OS.
/// [`apply_cursor_mode`] only runs in-game, so the transition boundary
/// needs an explicit release — and it lives here, not in the camera
/// plugin, because this module is the only writer of `CursorOptions`.
fn release_cursor_on_ingame_exit(mut cursors: Query<&mut CursorOptions, With<PrimaryWindow>>) {
    if let Ok(mut cursor) = cursors.single_mut() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

/// Centralized Esc dispatcher. The only system that listens for the
/// Escape key — no per-overlay Esc handlers. This is the SSOT
/// principle applied to one specific input: "what does Esc do?" is a
/// single fact derived from the current capture state, not a race
/// between independent handlers each owning their own state.
///
/// Priority (topmost-first):
///   0. In-flight Plan-mode drag — aborts without committing. Not a
///      capture (the cursor stays locked mid-drag), but it's the
///      "topmost" thing Esc should mean while it exists; without
///      this branch the same press would both cancel the drag (in
///      plans.rs) and open the pause menu here.
///   1. Craft modal — clears `CraftStationUiState`; the modal's own
///      capture-sync system releases the capture next tick.
///   2. Debug panel — releases its capture directly.
///   3. Pause menu — releases its capture directly.
///   4. Nothing captured — opens the pause menu.
///
/// Adding a new overlay = one new branch here. The previous design
/// had every overlay registering its own `keys.just_pressed(Escape)`
/// system, which (a) made priority depend on system-execution order
/// and (b) let Esc fire two close-handlers on the same press,
/// causing the cursor/input desync that motivated this whole
/// refactor.
fn handle_escape(
    keys: Res<ButtonInput<KeyCode>>,
    mut captures: ResMut<UiCaptures>,
    mut craft_ui: ResMut<crate::craft_stations::CraftStationUiState>,
    mut drag: ResMut<crate::plans::PlanDragState>,
    mut storage_drag: ResMut<crate::storage::StorageDragState>,
    session: Res<crate::menu::ServerSession>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    // A pending quit owns the UI. Releasing the pause menu here would
    // relock the cursor and re-enable world edits *after* the server has
    // snapshotted for the quit-save — those edits would silently miss
    // the save — and it would hide the "Saving and shutting down..."
    // line, the only feedback that a drain is in progress.
    if session.shutdown_requested() {
        return;
    }
    // A fatal error owns the session; there is nothing to Esc back to.
    // Consume the key so it can't open the pause menu underneath.
    if captures.contains(UiCapture::FatalError) {
        return;
    }
    if drag.active.is_some() {
        drag.active = None;
        return;
    }
    if storage_drag.active.is_some() {
        storage_drag.active = None;
        return;
    }
    if captures.contains(UiCapture::CraftModal) {
        // Clear the modal's data; `sync_craft_modal_capture` mirrors
        // the empty `open_cell` into the captures set next tick.
        craft_ui.open_cell = None;
        craft_ui.pending_quantities.clear();
        return;
    }
    if captures.contains(UiCapture::DebugPanel) {
        captures.release(UiCapture::DebugPanel);
        return;
    }
    if captures.contains(UiCapture::PauseMenu) {
        captures.release(UiCapture::PauseMenu);
        return;
    }
    captures.acquire(UiCapture::PauseMenu);
}

pub struct UiCapturesPlugin;

impl Plugin for UiCapturesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiCaptures>();
        app.init_resource::<DiscardNextMotion>();
        // apply_cursor_mode is gated on InGame so it doesn't fire at
        // startup, when AppState is MainMenu and the captures resource
        // is freshly-added (which would otherwise trip is_changed and
        // execute the "no captures → lock cursor" branch, eating the
        // cursor on the menu screen).
        app.add_systems(
            Update,
            (apply_cursor_mode, handle_escape).run_if(in_state(AppState::InGame)),
        );
        app.add_systems(OnEnter(AppState::InGame), reset_captures_on_ingame_enter);
        app.add_systems(OnExit(AppState::InGame), release_cursor_on_ingame_exit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_not_captured() {
        let c = UiCaptures::default();
        assert!(!c.is_captured());
        assert!(!c.contains(UiCapture::PauseMenu));
    }

    #[test]
    fn acquire_release_round_trips() {
        let mut c = UiCaptures::default();
        c.acquire(UiCapture::CraftModal);
        assert!(c.is_captured());
        assert!(c.contains(UiCapture::CraftModal));
        c.release(UiCapture::CraftModal);
        assert!(!c.is_captured());
        assert!(!c.contains(UiCapture::CraftModal));
    }

    #[test]
    fn acquire_is_idempotent() {
        let mut c = UiCaptures::default();
        c.acquire(UiCapture::CraftModal);
        c.acquire(UiCapture::CraftModal);
        c.release(UiCapture::CraftModal);
        assert!(!c.is_captured());
    }

    #[test]
    fn release_without_acquire_is_noop() {
        let mut c = UiCaptures::default();
        c.release(UiCapture::CraftModal);
        assert!(!c.is_captured());
    }

    #[test]
    fn contains_distinguishes_variants() {
        let mut c = UiCaptures::default();
        c.acquire(UiCapture::DebugPanel);
        assert!(c.contains(UiCapture::DebugPanel));
        assert!(!c.contains(UiCapture::PauseMenu));
        assert!(!c.contains(UiCapture::CraftModal));
    }
}
