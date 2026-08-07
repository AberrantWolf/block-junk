//! App lifecycle: main menu, esc-pause menu, host-thread management.
//!
//! The client process always starts as an `AppState::MainMenu` unless the
//! `client [addr]` CLI shortcut pre-sets `InGame`. Entering `InGame` is what
//! actually starts a game session — that's when the lightyear client triggers
//! `Connect`, and (in host mode) the server-side App is spawned on a worker
//! thread. Exiting `InGame` tears both down.
//!
//! Quit-to-menu drains the same way quit-to-desktop does (server saves,
//! exits, joins), then transitions back to `MainMenu` instead of writing
//! `AppExit`. Session cleanup hangs off `OnExit(InGame)`: locally-spawned
//! session entities carry `DespawnOnExit(AppState::InGame)`, replicated
//! entities are swept by `client::cleanup_session`, and the lightyear
//! client entity is disconnected here and despawned once back at the menu
//! (see `network.rs`). Re-entering a world from the menu is supported.

use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::time::Duration;
use std::sync::Arc;
use std::thread::JoinHandle;

/// Backstop for a pre-existing freeze on quit: on macOS the client App
/// occasionally beachballs after `AppExit` (winit / wgpu / lightyear
/// shutdown getting stuck somewhere on the main thread). Spawn a side
/// thread that sleeps the deadline and `process::exit`s. If Bevy's
/// normal shutdown completes first, the process dies and this thread
/// dies with it; if Bevy hangs, this rescues us. The warn! before
/// process::exit makes it visible in logs whether the watchdog ever
/// actually fired — if it never logs, the freeze is elsewhere.
fn arm_quit_watchdog(deadline: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        warn!("quit watchdog: Bevy didn't exit within {deadline:?} — forcing process::exit(0)");
        std::process::exit(0);
    });
}

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, egui};

use lightyear::prelude::Predicted;

use crate::network::LOCAL_CONNECT_ADDR;
use crate::protocol::AvatarPose;
use crate::save::{
    SaveListEntry, SaveStatus as SaveEntryStatus, delete_save, list_saves, save_exists,
    validate_name,
};
use crate::voxel::world_to_chunk;

/// Top-level lifecycle states for the client App. The server App, when
/// hosting, runs in its own thread and has no `AppState` — it just runs
/// until its shutdown flag is set.
///
/// There is no `Paused` variant: the in-game pause/options overlay is
/// just a UI flag ([`PauseMenuOpen`]) that doesn't halt simulation.
#[derive(States, Default, Debug, Clone, Eq, PartialEq, Hash)]
pub enum AppState {
    #[default]
    MainMenu,
    InGame,
}

/// What kind of session the user is starting / has started. Set by the menu
/// (or by the `client [addr]` CLI shortcut) before transitioning to InGame.
#[derive(Resource, Clone, Debug)]
pub enum LaunchMode {
    /// Host a fresh world locally. Spawns the server thread; on quit, the
    /// server writes to `save_name` (unless DebugNoSaveOnExit is set).
    HostNew { save_name: String },
    /// Host an existing save locally. Server loads chunks from `save_name`
    /// on startup; saves back to the same name on quit.
    HostLoad { save_name: String },
    /// Pure client — join a remote server. No server thread, no save.
    JoinRemote { addr: SocketAddr },
}

/// Carried across the thread boundary to the server App. Tells the server
/// which save to read/write and whether saving is enabled this session.
/// For the dedicated-server CLI path, `save_name` is `None`.
#[derive(Clone, Debug, Resource)]
pub struct ServerSaveConfig {
    pub save_name: Option<String>,
    pub load_existing: bool,
    pub no_save_on_exit: bool,
}

/// The address the lightyear client should connect to once `OnEnter(InGame)`
/// fires. Host mode points at localhost; JoinRemote points at the menu input.
#[derive(Resource, Clone, Copy, Debug)]
pub struct JoinTarget(pub SocketAddr);

impl Default for JoinTarget {
    fn default() -> Self {
        Self(LOCAL_CONNECT_ADDR)
    }
}

/// Handle to the server thread spawned when hosting. None when running as a
/// pure client (JoinRemote) or before any game has started.
///
/// Arc<AtomicBool> flags coordinate cross-thread state with the server App.
/// The client sets them; the server polls and acts. We use atomics rather
/// than channels because (a) the server doesn't need backpressure and (b)
/// atomics survive cleanly if the client crashes mid-set.
#[derive(Resource, Default)]
pub struct ServerSession {
    handle: Option<JoinHandle<()>>,
    shutdown: Option<Arc<AtomicBool>>,
    save_request: Option<Arc<AtomicBool>>,
    save_result: Option<Arc<AtomicU8>>,
    shutdown_requested: bool,
}

/// Outcome of the most recent requested save, published by the server
/// *before* it clears the request flag (so a client that observes the
/// flag drop always reads a fresh outcome, never a stale one).
pub const SAVE_RESULT_NONE: u8 = 0;
pub const SAVE_RESULT_OK: u8 = 1;
pub const SAVE_RESULT_FAILED: u8 = 2;

impl ServerSession {
    pub fn is_hosting(&self) -> bool {
        self.handle.is_some()
    }

    /// Ask the hosted server App to save/exit. The thread handle is retained
    /// until [`join_if_finished`] observes the server has actually returned;
    /// quitting the client before that point can lose the quit-save.
    pub fn request_shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        if let Some(flag) = self.shutdown.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        self.shutdown_requested = true;
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Join the hosted server once it has already finished. Returns `true`
    /// when no hosted server remains, so callers can safely continue with
    /// client shutdown.
    pub fn join_if_finished(&mut self) -> bool {
        if let Some(handle) = self.handle.as_ref()
            && !handle.is_finished()
        {
            return false;
        }

        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => info!("hosted server thread joined after shutdown"),
                Err(_) => error!("hosted server thread panicked during shutdown"),
            }
        }
        self.shutdown = None;
        self.save_request = None;
        self.save_result = None;
        self.shutdown_requested = false;
        true
    }

    /// Request a mid-session save. Server clears the flag once it has
    /// resolved the save (written or refused); spamming the button is
    /// harmless (extra requests during the same tick just collapse).
    pub fn request_save(&self) {
        if self.shutdown_requested {
            return;
        }
        if let Some(result) = self.save_result.as_ref() {
            result.store(SAVE_RESULT_NONE, Ordering::SeqCst);
        }
        if let Some(flag) = self.save_request.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// True while a requested save hasn't been resolved yet (the server
    /// clears the flag after publishing the outcome). Drives the pause
    /// menu's "Saving… / Saved / failed" feedback.
    pub fn save_pending(&self) -> bool {
        self.save_request
            .as_ref()
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Outcome of the last resolved save request. Only meaningful once
    /// [`save_pending`] has dropped; `SAVE_RESULT_NONE` means no request
    /// has resolved since the last [`request_save`].
    pub fn last_save_result(&self) -> u8 {
        self.save_result
            .as_ref()
            .map(|result| result.load(Ordering::SeqCst))
            .unwrap_or(SAVE_RESULT_NONE)
    }
}

/// Inserted into the server App as a Resource so the shutdown-check system
/// can read it. Setting it true causes the server App to emit `AppExit`.
#[derive(Resource, Clone)]
pub struct ServerShutdownFlag(pub Arc<AtomicBool>);

/// Mid-session save request. Set true to make the server flush to disk
/// without exiting; the server clears it once the save is resolved.
#[derive(Resource, Clone)]
pub struct ServerSaveRequestFlag(pub Arc<AtomicBool>);

/// Where the server publishes the outcome of a requested save (one of
/// the `SAVE_RESULT_*` codes), written *before* it clears the request
/// flag. Without this the pause menu could only infer "flag dropped ⇒
/// saved" — which reads a refused or failed write as success.
#[derive(Resource, Clone)]
pub struct ServerSaveResultFlag(pub Arc<AtomicU8>);

/// Debug override for skipping the hosted-server quit-save. Normal play
/// saves on quit by default; tests/dev tooling can insert `Self(true)` when
/// they need a disposable session.
#[derive(Resource, Clone, Copy, Default)]
pub struct DebugNoSaveOnExit(pub bool);

/// Set after an in-game quit button is clicked. `drive_pending_quit` keeps
/// the session alive until the hosted server thread has saved, exited, and
/// joined — then either exits the process or transitions back to the main
/// menu, per `to_menu`.
#[derive(Resource, Debug)]
struct PendingQuit {
    requested_at: f32,
    warned_slow: bool,
    /// `true` ⇒ resolve to `AppState::MainMenu`; `false` ⇒ `AppExit`.
    to_menu: bool,
}

impl PendingQuit {
    fn new(requested_at: f32, to_menu: bool) -> Self {
        Self {
            requested_at,
            warned_slow: false,
            to_menu,
        }
    }
}

/// A connection-level failure the session cannot recover from (mod-set
/// mismatch, future auth failures). Inserting this resource — paired
/// with acquiring [`crate::ui_capture::UiCapture::FatalError`] — locks
/// the session behind a blocking modal whose only exit is quitting.
/// Esc is deliberately inert while it's up (see `handle_escape`).
#[derive(Resource, Debug)]
pub struct ConnectionFatal {
    pub title: String,
    /// Human-readable mismatch lines, already truncated by the producer.
    pub details: Vec<String>,
}

/// One-time egui context setup: register DejaVu Sans as a fallback font
/// (egui's bundled fonts render arrows / triangles / die faces / ⌘ as
/// tofu) and apply the in-game skin from [`crate::ui_theme`]. Player-
/// facing windows inherit the skin from the context; dev windows opt
/// into the dev frame per-window.
fn setup_egui_context(mut contexts: EguiContexts, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else { return };
    block_junk_textures::egui_fonts::install(ctx);
    crate::ui_theme::apply_ingame_style(ctx);
    *done = true;
}

pub struct MenuPlugin;

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default());
        app.init_state::<AppState>();
        app.init_resource::<JoinTarget>();
        app.init_resource::<ServerSession>();
        app.init_resource::<DebugNoSaveOnExit>();
        app.init_resource::<ConnectAddrInput>();
        app.init_resource::<InviteSecretInput>();
        app.init_resource::<HostAccessSelection>();
        app.init_resource::<HostedJoinCode>();
        app.init_resource::<NewWorldName>();
        app.init_resource::<SaveListing>();
        app.init_resource::<SaveStatus>();
        app.init_resource::<MenuPage>();
        app.add_systems(OnEnter(AppState::MainMenu), refresh_save_listing);

        // bevy_egui attaches its primary context to the FIRST camera that
        // appears. Without this, the menu state has no camera (the 3D one
        // doesn't spawn until an avatar replicates inside InGame) and egui
        // renders nothing. Order 1 with ClearColorConfig::None means: when
        // a 3D game camera exists (order 0), it draws the world first and
        // this camera composites egui on top without wiping it.
        app.add_systems(Startup, spawn_ui_camera);

        app.add_systems(
            bevy_egui::EguiPrimaryContextPass,
            (
                setup_egui_context,
                main_menu_ui.run_if(in_state(AppState::MainMenu)),
                // Pause menu rides on top of InGame — the world keeps
                // simulating beneath it, the menu just releases the
                // cursor so its buttons are clickable. The `if open`
                // gate is inside the system rather than `run_if` so
                // egui's window can claim the click in the same frame
                // it's opened (no one-frame gap before it renders).
                pause_menu_ui.run_if(in_state(AppState::InGame)),
                debug_overlay_ui.run_if(in_state(AppState::InGame)),
                // Last so it draws over whatever else is open.
                fatal_error_ui.run_if(resource_exists::<ConnectionFatal>),
            ),
        );

        // Esc handling is centralized in `ui_capture::handle_escape` —
        // it dispatches to the topmost capture (or opens pause if
        // nothing held). No per-overlay Esc handlers, no per-overlay
        // priority logic. See that module for the dispatch table.

        // Server thread lifecycle is tied to *session* boundaries, not to
        // InGame ↔ Paused. Pausing must not tear down the server; only
        // explicit quit (to menu or desktop) does.
        app.add_systems(OnEnter(AppState::InGame), spawn_server_if_hosting);
        // Chained so a ✕ press and the (fast-path) exit resolve in the
        // same frame when nothing needs saving.
        app.add_systems(Update, (quit_on_window_close, drive_pending_quit).chain());
        app.add_systems(OnExit(AppState::InGame), cleanup_lifecycle_resources);
    }
}

fn spawn_ui_camera(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Name::new("UiCamera"),
    ));
}

/// Buffer for the "Connect to remote" text field. Separate from `JoinTarget`
/// because the user is typing free-form text we don't promote to a SocketAddr
/// until they press Connect.
#[derive(Resource, Default)]
struct ConnectAddrInput(String);

#[derive(Resource, Default)]
struct InviteSecretInput(String);

#[derive(Resource)]
pub(crate) struct HostAccessSelection(crate::network::ServerAccess);

impl Default for HostAccessSelection {
    fn default() -> Self {
        Self(crate::network::ServerAccess::Invite)
    }
}

#[derive(Resource, Default)]
pub(crate) struct HostedJoinCode(Option<String>);

/// Buffer for the "New world name" text field. Validated as a save name on
/// Create (file-safe charset, non-empty).
#[derive(Resource)]
struct NewWorldName(String);

impl Default for NewWorldName {
    fn default() -> Self {
        Self("world1".to_string())
    }
}

/// Cached listing of saves on disk. Refreshed when entering MainMenu and
/// after any mutation (delete, create). We don't re-scan every frame —
/// the directory wouldn't normally change while the user is at the menu,
/// and it's polite to filesystems we might be reading from.
#[derive(Resource, Default)]
struct SaveListing(Vec<SaveListEntry>);

/// Most-recent save error (delete failed, create with bad name, etc.) so
/// the main menu can surface it. Cleared on next valid action.
#[derive(Resource, Default)]
struct SaveStatus(Option<String>);

/// Which screen of the main menu is showing. The menu is split into a
/// game-like landing page plus focused sub-pages, so the first thing a
/// new player sees is a short list of big choices rather than a form.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq)]
enum MenuPage {
    #[default]
    Home,
    Play,
    Multiplayer,
    Settings,
}

/// Scratch values for the placeholder Settings page. None of these are
/// wired to anything yet — they exist so the page reads as a real
/// options screen instead of an empty panel. Replace with real settings
/// when the options pass happens.
struct DummySettings {
    master_volume: f32,
    render_distance: u32,
    invert_y: bool,
}

impl Default for DummySettings {
    fn default() -> Self {
        Self {
            master_volume: 0.8,
            render_distance: 8,
            invert_y: false,
        }
    }
}

/// A big, fixed-width menu button. Centralises the size + font bump so
/// every landing-page choice reads at the same weight.
fn menu_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [260.0, 46.0],
        egui::Button::new(egui::RichText::new(label).size(22.0)),
    )
}

/// Width of the pause-menu window's stacked action buttons. Narrower than
/// the landing-page buttons (the window itself is compact) but wide enough
/// that Resume / Quit read at the same weight as the start screen rather
/// than as default egui chips.
const PAUSE_BUTTON_SIZE: [f32; 2] = [220.0, 38.0];

/// A pause-menu action button, sized to match the start screen's look so
/// the two menus feel like one game rather than two different apps.
fn pause_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        PAUSE_BUTTON_SIZE,
        egui::Button::new(egui::RichText::new(label).size(17.0)),
    )
}

#[allow(clippy::too_many_arguments)]
fn main_menu_ui(
    mut contexts: EguiContexts,
    mut next_state: ResMut<NextState<AppState>>,
    mut commands: Commands,
    mut join_target: ResMut<JoinTarget>,
    mut addr_input: ResMut<ConnectAddrInput>,
    mut invite_input: ResMut<InviteSecretInput>,
    mut join_credentials: ResMut<crate::network::JoinCredentials>,
    mut host_access: ResMut<HostAccessSelection>,
    mut new_name: ResMut<NewWorldName>,
    mut listing: ResMut<SaveListing>,
    mut status: ResMut<SaveStatus>,
    mut page: ResMut<MenuPage>,
    mut exit: MessageWriter<AppExit>,
    // World name awaiting delete confirmation. Delete is irreversible
    // and sits one row away from Load — a single misclick must not
    // destroy a world, so the first click only arms the confirm row.
    mut confirm_delete: Local<Option<String>>,
    // Placeholder values for the not-yet-wired Settings page.
    mut dummy: Local<DummySettings>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    if addr_input.0.is_empty() {
        addr_input.0 = LOCAL_CONNECT_ADDR.to_string();
    }

    // Push content down from the top so the menu sits toward the middle
    // of the window regardless of its size.
    let screen_h = ctx.viewport_rect().height();

    let mut root = crate::ui_theme::root_ui(ctx, "main_menu_root");
    egui::CentralPanel::default().show_inside(&mut root, |ui| {
        ui.vertical_centered(|ui| {
            // Fixed column width keeps the buttons and forms from
            // stretching edge-to-edge on a wide window.
            ui.set_max_width(440.0);
            match *page {
                MenuPage::Home => home_page(ui, screen_h, &mut page, &mut exit),
                MenuPage::Play => play_page(
                    ui,
                    screen_h,
                    &mut page,
                    &mut next_state,
                    &mut commands,
                    &mut join_target,
                    &mut new_name,
                    &mut listing,
                    &mut status,
                    &mut confirm_delete,
                    &mut host_access,
                ),
                MenuPage::Multiplayer => multiplayer_page(
                    ui,
                    screen_h,
                    &mut page,
                    &mut next_state,
                    &mut commands,
                    &mut join_target,
                    &mut addr_input,
                    &mut invite_input,
                    &mut join_credentials,
                    &mut status,
                ),
                MenuPage::Settings => settings_page(ui, screen_h, &mut page, &mut dummy),
            }
        });
    });
}

/// Landing page: big title, a stack of large choices. This is the first
/// thing a new player sees, so it stays to a handful of buttons.
fn home_page(
    ui: &mut egui::Ui,
    screen_h: f32,
    page: &mut MenuPage,
    exit: &mut MessageWriter<AppExit>,
) {
    ui.add_space(screen_h * 0.15);
    ui.label(egui::RichText::new("block-junk").size(68.0).strong());
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("a little voxel settlement")
            .size(15.0)
            .italics()
            .weak(),
    );
    ui.add_space(36.0);
    if menu_button(ui, "Play").clicked() {
        *page = MenuPage::Play;
    }
    ui.add_space(10.0);
    if menu_button(ui, "Multiplayer").clicked() {
        *page = MenuPage::Multiplayer;
    }
    ui.add_space(10.0);
    if menu_button(ui, "Settings").clicked() {
        *page = MenuPage::Settings;
    }
    ui.add_space(10.0);
    if menu_button(ui, "Quit").clicked() {
        exit.write(AppExit::Success);
        // No server session at the main menu; 1s is plenty for a clean
        // Bevy shutdown if it works at all.
        arm_quit_watchdog(Duration::from_secs(1));
    }
}

/// Single-player page: the worlds list (load / delete) plus new-world
/// creation. All the state transitions into a hosted session live here.
#[allow(clippy::too_many_arguments)]
fn play_page(
    ui: &mut egui::Ui,
    screen_h: f32,
    page: &mut MenuPage,
    next_state: &mut NextState<AppState>,
    commands: &mut Commands,
    join_target: &mut JoinTarget,
    new_name: &mut NewWorldName,
    listing: &mut SaveListing,
    status: &mut SaveStatus,
    confirm_delete: &mut Option<String>,
    host_access: &mut HostAccessSelection,
) {
    ui.add_space(screen_h * 0.08);
    ui.label(egui::RichText::new("Play").size(40.0).strong());
    ui.add_space(16.0);

    if listing.0.is_empty() {
        ui.label(
            egui::RichText::new("(no worlds yet — create one below)")
                .italics()
                .weak(),
        );
    } else {
        let mut load_request: Option<String> = None;
        let mut delete_request: Option<String> = None;
        egui::ScrollArea::vertical()
            .max_height(200.0)
            .show(ui, |ui| {
                for meta in &listing.0 {
                    let confirming = confirm_delete.as_deref() == Some(meta.name.as_str());
                    ui.horizontal(|ui| {
                        if confirming {
                            ui.label(
                                egui::RichText::new(format!("Delete {:?}?", meta.name)).strong(),
                            );
                            // Keep the destructive confirm on the right,
                            // where the Delete button lived — the eye is
                            // already there from the click that armed it.
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Cancel").clicked() {
                                        *confirm_delete = None;
                                    }
                                    let danger = egui::Button::new(
                                        egui::RichText::new("Delete")
                                            .color(egui::Color32::from_rgb(255, 120, 110)),
                                    );
                                    if ui.add(danger).clicked() {
                                        delete_request = Some(meta.name.clone());
                                        *confirm_delete = None;
                                    }
                                },
                            );
                            return;
                        }
                        if ui
                            .add_enabled(
                                meta.status == SaveEntryStatus::Valid,
                                egui::Button::new("Load"),
                            )
                            .clicked()
                        {
                            load_request = Some(meta.name.clone());
                        }
                        ui.label(egui::RichText::new(&meta.name).strong().monospace());
                        ui.label(
                            egui::RichText::new(format!("({})", relative_time(meta.modified_at)))
                                .weak(),
                        );
                        match meta.status {
                            SaveEntryStatus::Valid => {}
                            SaveEntryStatus::Incompatible => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "incompatible v{}",
                                        meta.version.unwrap_or_default()
                                    ))
                                    .color(egui::Color32::YELLOW),
                                );
                            }
                            SaveEntryStatus::Corrupt => {
                                ui.label(
                                    egui::RichText::new("corrupt").color(egui::Color32::LIGHT_RED),
                                );
                            }
                        }
                        // Delete lives at the far right edge, well clear of
                        // Load, so a misclick can't nuke a world. The first
                        // click only arms the inline confirm row above.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Delete").clicked() {
                                *confirm_delete = Some(meta.name.clone());
                            }
                        });
                    });
                }
            });
        if let Some(name) = load_request {
            commands.insert_resource(LaunchMode::HostLoad {
                save_name: name.clone(),
            });
            *join_target = JoinTarget(LOCAL_CONNECT_ADDR);
            status.0 = None;
            next_state.set(AppState::InGame);
        }
        if let Some(name) = delete_request {
            match delete_save(&name) {
                Ok(()) => {
                    status.0 = Some(format!("deleted {name:?}"));
                    // Refresh inline so the list updates this frame.
                    listing.0 = list_saves().unwrap_or_default();
                }
                Err(e) => {
                    status.0 = Some(format!("delete failed: {e}"));
                }
            }
        }
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label("New world name:");
        ui.add(
            egui::TextEdit::singleline(&mut new_name.0)
                .desired_width(160.0)
                .hint_text("worldN"),
        );
        if ui.button("Create").clicked() {
            let trimmed = new_name.0.trim().to_string();
            match validate_name(&trimmed) {
                Ok(()) => {
                    if save_exists(&trimmed) {
                        status.0 = Some(format!(
                            "{trimmed:?} already exists — pick a different name or delete it"
                        ));
                    } else {
                        commands.insert_resource(LaunchMode::HostNew {
                            save_name: trimmed.clone(),
                        });
                        *join_target = JoinTarget(LOCAL_CONNECT_ADDR);
                        status.0 = None;
                        next_state.set(AppState::InGame);
                    }
                }
                Err(e) => {
                    status.0 = Some(format!("{e}"));
                }
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Access:");
        ui.selectable_value(
            &mut host_access.0,
            crate::network::ServerAccess::Invite,
            "Invite",
        );
        ui.selectable_value(
            &mut host_access.0,
            crate::network::ServerAccess::Open,
            "Open",
        );
    });

    if let Some(msg) = &status.0 {
        ui.colored_label(egui::Color32::YELLOW, msg);
    }

    ui.add_space(20.0);
    if menu_button(ui, "Back").clicked() {
        status.0 = None;
        *page = MenuPage::Home;
    }
}

/// Multiplayer page: connect to a remote host by address.
#[allow(clippy::too_many_arguments)]
fn multiplayer_page(
    ui: &mut egui::Ui,
    screen_h: f32,
    page: &mut MenuPage,
    next_state: &mut NextState<AppState>,
    commands: &mut Commands,
    join_target: &mut JoinTarget,
    addr_input: &mut ConnectAddrInput,
    invite_input: &mut InviteSecretInput,
    join_credentials: &mut crate::network::JoinCredentials,
    status: &mut SaveStatus,
) {
    ui.add_space(screen_h * 0.1);
    ui.label(egui::RichText::new("Multiplayer").size(40.0).strong());
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("Join a friend who's hosting a world.")
            .size(13.0)
            .weak(),
    );
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.label("Server address:");
        ui.add(
            egui::TextEdit::singleline(&mut addr_input.0)
                .desired_width(180.0)
                .hint_text("127.0.0.1:5050"),
        );
        if ui.button("Connect").clicked() {
            match addr_input.0.parse::<SocketAddr>() {
                Ok(addr) => {
                    let key = if invite_input.0.trim().is_empty() {
                        Ok(crate::network::OPEN_NETCODE_KEY)
                    } else {
                        crate::network::decode_key_hex(invite_input.0.trim())
                    };
                    let Ok(key) = key else {
                        status.0 = Some("invite secret must be 64 hexadecimal characters".into());
                        return;
                    };
                    join_credentials.0 = key;
                    commands.insert_resource(LaunchMode::JoinRemote { addr });
                    *join_target = JoinTarget(addr);
                    next_state.set(AppState::InGame);
                }
                Err(e) => {
                    status.0 = Some(format!("invalid address: {e}"));
                }
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Join secret:");
        ui.add(
            egui::TextEdit::singleline(&mut invite_input.0)
                .desired_width(300.0)
                .password(true)
                .hint_text("blank for Open worlds"),
        );
    });

    if let Some(msg) = &status.0 {
        ui.colored_label(egui::Color32::YELLOW, msg);
    }

    ui.add_space(20.0);
    if menu_button(ui, "Back").clicked() {
        status.0 = None;
        *page = MenuPage::Home;
    }
}

/// Placeholder Settings page. Every control here is disabled — it exists
/// to make the menu feel complete and to reserve a home for the real
/// options pass. See [`DummySettings`].
fn settings_page(ui: &mut egui::Ui, screen_h: f32, page: &mut MenuPage, dummy: &mut DummySettings) {
    ui.add_space(screen_h * 0.1);
    ui.label(egui::RichText::new("Settings").size(40.0).strong());
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("Nothing here works yet — placeholder for the real options pass.")
            .size(13.0)
            .italics()
            .weak(),
    );
    ui.add_space(20.0);
    ui.add_enabled_ui(false, |ui| {
        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Master volume");
                ui.add(egui::Slider::new(&mut dummy.master_volume, 0.0..=1.0));
                ui.end_row();
                ui.label("Render distance");
                ui.add(egui::Slider::new(&mut dummy.render_distance, 2..=16).suffix(" chunks"));
                ui.end_row();
                ui.label("Invert Y");
                ui.checkbox(&mut dummy.invert_y, "");
                ui.end_row();
            });
    });

    ui.add_space(24.0);
    if menu_button(ui, "Back").clicked() {
        *page = MenuPage::Home;
    }
}

fn refresh_save_listing(
    mut listing: ResMut<SaveListing>,
    mut new_name: ResMut<NewWorldName>,
    mut page: ResMut<MenuPage>,
) {
    // Always land on the home page — a fresh menu (or a future
    // quit-to-menu) should start at the top of the tree, not wherever
    // the player last was.
    *page = MenuPage::Home;
    listing.0 = list_saves().unwrap_or_default();
    // Auto-pick a free default name so consecutive "Create" clicks don't
    // collide.
    if save_exists(&new_name.0) {
        new_name.0 = next_free_world_name(&listing.0);
    }
}

fn next_free_world_name(existing: &[SaveListEntry]) -> String {
    let taken: std::collections::HashSet<&str> = existing.iter().map(|m| m.name.as_str()).collect();
    for n in 1..1000 {
        let candidate = format!("world{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    "world".to_string()
}

fn relative_time(unix_seconds: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(unix_seconds);
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

/// Tracks the in-flight Save Now request so the pause menu can show saving
/// feedback while the server flushes and completion for a moment after.
#[derive(Default)]
struct SaveFeedback {
    /// A Save Now click is outstanding (set on click, cleared when the
    /// server's flag drops).
    requested: bool,
    /// `Time::elapsed_secs()` when the last save finished; drives the
    /// transient "Saved ✓" label.
    completed_at: Option<f32>,
    /// The last resolved save was refused or its write failed. Sticky
    /// until the next Save Now click — a failure must not quietly age
    /// out the way the success label does.
    failed: bool,
}

#[derive(bevy::ecs::system::SystemParam)]
struct PauseMenuLocal<'s> {
    save_feedback: Local<'s, SaveFeedback>,
    // Cached for the session: interface-route lookup is a syscall pair.
    lan_addr: Local<'s, Option<Option<core::net::IpAddr>>>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "pause UI coordinates session and save controls"
)]
fn pause_menu_ui(
    mut contexts: EguiContexts,
    mut captures: ResMut<crate::ui_capture::UiCaptures>,
    mut commands: Commands,
    mut session: ResMut<ServerSession>,
    debug_no_save: Res<DebugNoSaveOnExit>,
    time: Res<Time>,
    join_code: Res<HostedJoinCode>,
    local: PauseMenuLocal,
) {
    let PauseMenuLocal {
        mut save_feedback,
        mut lan_addr,
    } = local;
    if !captures.contains(crate::ui_capture::UiCapture::PauseMenu) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let hosting = session.is_hosting();
    let quit_pending = session.shutdown_requested();
    // Resolve the save round-trip: the server publishes the outcome and
    // then clears the flag. Checked before drawing so the label flips in
    // the same frame the flag drops. Anything other than an explicit OK
    // renders as failure — "flag dropped" alone proves the server looked
    // at the request, not that a file reached disk.
    if save_feedback.requested && !session.save_pending() {
        save_feedback.requested = false;
        if session.last_save_result() == SAVE_RESULT_OK {
            save_feedback.completed_at = Some(time.elapsed_secs());
            save_feedback.failed = false;
        } else {
            save_feedback.completed_at = None;
            save_feedback.failed = true;
        }
    }
    let mut close_request = false;
    // Some(to_menu): a quit button was clicked this frame.
    let mut quit_request: Option<bool> = None;
    egui::Window::new("Paused")
        .collapsible(false)
        .resizable(false)
        // Fixed width so the button column reads at a deliberate size to
        // match the start screen, rather than shrink-wrapping to the
        // shortest label.
        .default_width(260.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Paused").size(28.0).strong());
            });
            ui.add_space(2.0);
            ui.label("The world is still running.");
            // Hosted worlds always bind all interfaces, so the only
            // thing a friend needs is this machine's LAN address.
            if hosting {
                let addr = *lan_addr.get_or_insert_with(crate::network::local_lan_ip);
                let text = match addr {
                    Some(ip) => {
                        format!("Friends can join at {ip}:{}", crate::network::SERVER_PORT)
                    }
                    None => "LAN address unavailable (offline?)".to_string(),
                };
                ui.label(egui::RichText::new(text).weak());
                if let Some(code) = &join_code.0 {
                    ui.label(egui::RichText::new(format!("Join code: {code}")).monospace());
                }
            }
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                if quit_pending {
                    let label = if debug_no_save.0 {
                        "Shutting down..."
                    } else {
                        "Saving and shutting down..."
                    };
                    ui.label(egui::RichText::new(label).weak());
                }
                ui.add_enabled_ui(!quit_pending, |ui| {
                    if pause_button(ui, "Resume").clicked() {
                        close_request = true;
                    }
                    // Save Now bypasses DebugNoSaveOnExit so you can verify a
                    // save without quitting. Disabled on JoinRemote (the
                    // local App isn't authoritative over the world).
                    ui.add_enabled_ui(hosting, |ui| {
                        if pause_button(ui, "Save Now").clicked() {
                            session.request_save();
                            save_feedback.requested = true;
                            save_feedback.completed_at = None;
                            save_feedback.failed = false;
                        }
                    });
                    if save_feedback.requested {
                        ui.label(egui::RichText::new("Saving...").weak());
                    } else if save_feedback.failed {
                        ui.label(
                            egui::RichText::new("Save failed — see log")
                                .color(egui::Color32::from_rgb(230, 150, 150)),
                        );
                    } else if let Some(done) = save_feedback.completed_at
                        && time.elapsed_secs() - done < 3.0
                    {
                        ui.label(
                            egui::RichText::new("Saved")
                                .color(egui::Color32::from_rgb(180, 230, 150)),
                        );
                    }
                    if ui.button("Quit to Menu").clicked() {
                        quit_request = Some(true);
                    }
                    if ui.button("Quit to Desktop").clicked() {
                        quit_request = Some(false);
                    }
                });
            });
            ui.add_space(8.0);
            // The full keybind reference. Several binds (Q/T/F1/wheel)
            // have no other discoverable surface — the HUD hints cover
            // the moment-to-moment verbs, this covers everything.
            ui.collapsing("Controls", |ui| {
                egui::Grid::new("controls_grid")
                    .num_columns(2)
                    .spacing([16.0, 2.0])
                    .show(ui, |ui| {
                        let mut row = |key: &str, what: &str| {
                            ui.label(egui::RichText::new(key).monospace().strong());
                            ui.label(egui::RichText::new(what).weak());
                            ui.end_row();
                        };
                        row("W A S D", "move");
                        row("Space", "jump / fly up");
                        row("Shift", "fly down");
                        row("F1", "toggle walk / fly");
                        row("Tab", "cycle mode (Shift+Tab reverses)");
                        row("1 / 2", "Normal / Plan mode");
                        row("L-click", "Normal: mine, pick up, work · Plan: tag remove");
                        row(
                            "R-click",
                            "Normal: interact, open station · Plan: tag build",
                        );
                        row("Q", "drop carried items");
                        row("T", "drop held tool");
                        row("Scroll", "Plan: cycle block palette");
                        row("Ctrl+Scroll", "Plan: rotate placement");
                        row("F3", "debug panel");
                        row("Esc", "close topmost window / pause");
                    });
            });
        });
    if close_request {
        // Single state mutation handles everything: capture released
        // ⇒ apply_cursor_mode relocks the cursor + clears
        // DiscardNextMotion, ⇒ in-world input gates re-enable. No
        // manual window touching, no separate "open" flag to update.
        captures.release(crate::ui_capture::UiCapture::PauseMenu);
    }
    if let Some(to_menu) = quit_request {
        session.request_shutdown();
        save_feedback.requested = false;
        save_feedback.completed_at = None;
        commands.insert_resource(PendingQuit::new(time.elapsed_secs(), to_menu));
    }
}

/// Blocking modal for [`ConnectionFatal`]. The capture was acquired by
/// whoever inserted the resource, so the cursor is already free and
/// in-world input already suppressed; this system just renders and
/// offers the one exit. Quit routes through the same `PendingQuit`
/// drain as the pause menu, so a hosting player still gets their
/// quit-save (a JoinRemote client has no hosted server and exits
/// immediately).
fn fatal_error_ui(
    mut contexts: EguiContexts,
    fatal: Res<ConnectionFatal>,
    mut session: ResMut<ServerSession>,
    time: Res<Time>,
    pending: Option<Res<PendingQuit>>,
    mut commands: Commands,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui::Window::new("Connection failed")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(&fatal.title).strong());
            ui.add_space(6.0);
            for line in &fatal.details {
                ui.label(egui::RichText::new(line).monospace().weak());
            }
            ui.add_space(6.0);
            ui.label("Make sure both sides run the same mods and versions, then reconnect.");
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                if pending.is_some() {
                    ui.label(egui::RichText::new("Shutting down...").weak());
                } else {
                    if ui.button("Back to Menu").clicked() {
                        session.request_shutdown();
                        commands.insert_resource(PendingQuit::new(time.elapsed_secs(), true));
                    }
                    if ui.button("Quit to Desktop").clicked() {
                        session.request_shutdown();
                        commands.insert_resource(PendingQuit::new(time.elapsed_secs(), false));
                    }
                }
            });
        });
}

/// How long `drive_pending_quit` waits on the hosted server before
/// giving up and exiting anyway. Generous on purpose: a local quit-save
/// is an atomic temp-file write that finishes in well under a second,
/// so anything that outlives this is a *hung* shutdown (winit / wgpu /
/// lightyear — see `arm_quit_watchdog`), not a slow one. Without the
/// deadline a hung server wedges the app on "Saving and shutting
/// down..." with every button disabled, and the user's only move is a
/// force-kill — which loses the save anyway, plus their patience.
const QUIT_FORCE_EXIT_SECS: f32 = 20.0;

fn drive_pending_quit(
    mut commands: Commands,
    mut session: ResMut<ServerSession>,
    pending: Option<ResMut<PendingQuit>>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(mut pending) = pending else {
        return;
    };

    session.request_shutdown();
    if session.join_if_finished() {
        commands.remove_resource::<PendingQuit>();
        if pending.to_menu {
            // Session fully drained (save written, thread joined). The
            // state transition fires the OnExit(InGame) teardown chain.
            next_state.set(AppState::MainMenu);
        } else {
            exit.write(AppExit::Success);
            arm_quit_watchdog(Duration::from_secs(3));
        }
        return;
    }

    let waited = time.elapsed_secs() - pending.requested_at;
    if !pending.warned_slow && waited > 5.0 {
        pending.warned_slow = true;
        warn!("waiting for hosted server to save and shut down before exiting");
    }
    if waited > QUIT_FORCE_EXIT_SECS {
        // Even for a quit-to-menu request: a hung server thread can't be
        // re-hosted (port still bound, `is_hosting` still true), so the
        // process exit is the only recovery either way.
        error!(
            "hosted server did not shut down within {QUIT_FORCE_EXIT_SECS}s — exiting anyway; \
             the quit-save may not have been written"
        );
        commands.remove_resource::<PendingQuit>();
        exit.write(AppExit::error());
        arm_quit_watchdog(Duration::from_secs(3));
    }
}

/// Route the OS window-close request (titlebar ✕, Cmd-Q) through the
/// same save-then-exit path as the pause menu's quit buttons. Without
/// this — Bevy's default `close_when_requested` — the ✕ despawns the
/// window, `exit_on_all_closed` fires, and the process dies with the
/// hosted server mid-state: the quit-save never happens. `WindowPlugin`
/// is configured with `close_when_requested: false` in main.rs so this
/// system is the only consumer of the request.
fn quit_on_window_close(
    mut close_requests: MessageReader<bevy::window::WindowCloseRequested>,
    state: Res<State<AppState>>,
    mut captures: ResMut<crate::ui_capture::UiCaptures>,
    mut session: ResMut<ServerSession>,
    pending: Option<Res<PendingQuit>>,
    time: Res<Time>,
    mut commands: Commands,
) {
    if close_requests.is_empty() {
        return;
    }
    close_requests.clear();
    // Already draining — a second ✕ while the save runs changes nothing.
    if pending.is_some() {
        return;
    }
    session.request_shutdown();
    commands.insert_resource(PendingQuit::new(time.elapsed_secs(), false));
    // Bring up the pause menu so the player sees "Saving and shutting
    // down..." instead of a game that ignores the ✕, and so world input
    // is blocked while the server snapshots.
    if *state.get() == AppState::InGame {
        captures.acquire(crate::ui_capture::UiCapture::PauseMenu);
    }
}

/// Small dev overlay in the top-left corner showing the local player's
/// world position, the cell (block grid index) the camera is in, and the
/// chunk coord of that cell. Useful for reporting bugs by location.
/// Dev-skinned (flat monospace) so it reads as a tool, not game UI.
fn debug_overlay_ui(mut contexts: EguiContexts, avatar: Query<&AvatarPose, With<Predicted>>) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let Ok(pose) = avatar.single() else {
        return;
    };
    let p = pose.translation;
    let cell = p.floor().as_ivec3();
    let (chunk, local) = world_to_chunk(cell);
    egui::Window::new("debug")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::Vec2::new(8.0, 8.0))
        .frame(crate::ui_theme::dev_frame())
        .show(ctx, |ui| {
            crate::ui_theme::dev_skin(ui);
            ui.label(format!("pos   {:>7.2} {:>7.2} {:>7.2}", p.x, p.y, p.z));
            ui.label(format!("cell  {:>7} {:>7} {:>7}", cell.x, cell.y, cell.z));
            ui.label(format!(
                "chunk {:>3} {:>3} {:>3}   local {:>2} {:>2} {:>2}",
                chunk.0.x, chunk.0.y, chunk.0.z, local.x, local.y, local.z
            ));
            ui.label(format!("yaw   {:>7.2}", pose.yaw));
        });
}

/// Session-scoped lifecycle resources must not leak into the menu:
/// `ConnectionFatal` would keep the blocking modal up over the menu
/// (`fatal_error_ui` gates on the resource, not on state), and a stale
/// `LaunchMode` could silently relaunch the wrong world if a future
/// entry path forgets to set one.
fn cleanup_lifecycle_resources(mut commands: Commands) {
    commands.remove_resource::<ConnectionFatal>();
    commands.remove_resource::<LaunchMode>();
}

/// On entering InGame in a host mode, spawn the server thread. On JoinRemote
/// this is a no-op.
pub(crate) fn spawn_server_if_hosting(
    launch: Option<Res<LaunchMode>>,
    debug_no_save: Res<DebugNoSaveOnExit>,
    mut session: ResMut<ServerSession>,
    identity: Res<crate::identity::ClientIdentity>,
    mut join_credentials: ResMut<crate::network::JoinCredentials>,
    host_access: Res<HostAccessSelection>,
    mut join_code: ResMut<HostedJoinCode>,
) {
    // Already hosting — Resume from pause re-enters InGame and must not
    // double-spawn.
    if session.is_hosting() {
        return;
    }
    let no_save_on_exit = debug_no_save.0;
    let Some(launch) = launch else {
        warn!("entering InGame with no LaunchMode resource; defaulting to HostNew");
        let cfg = ServerSaveConfig {
            save_name: Some("autosave".to_string()),
            load_existing: false,
            no_save_on_exit,
        };
        let credentials =
            crate::network::world_credentials("autosave", false, None, vec![identity.public_key()])
                .unwrap_or_else(|error| panic!("cannot configure hosted access: {error}"));
        join_credentials.0 = credentials.netcode_key;
        join_code.0 = Some(crate::network::encode_key_hex(&credentials.netcode_key));
        spawn_server_thread(cfg, credentials, &mut session);
        return;
    };
    let cfg = match &*launch {
        LaunchMode::HostNew { save_name } => ServerSaveConfig {
            save_name: Some(save_name.clone()),
            load_existing: false,
            no_save_on_exit,
        },
        LaunchMode::HostLoad { save_name } => ServerSaveConfig {
            save_name: Some(save_name.clone()),
            load_existing: true,
            no_save_on_exit,
        },
        LaunchMode::JoinRemote { .. } => {
            info!("JoinRemote: skipping local server thread");
            return;
        }
    };
    let save_name = cfg
        .save_name
        .as_deref()
        .expect("hosted sessions always have a save name");
    let credentials = crate::network::world_credentials(
        save_name,
        cfg.load_existing,
        (!cfg.load_existing).then_some((host_access.0, None)),
        vec![identity.public_key()],
    )
    .unwrap_or_else(|error| panic!("cannot configure hosted access: {error}"));
    join_credentials.0 = credentials.netcode_key;
    join_code.0 = Some(match credentials.access {
        crate::network::ServerAccess::Invite => {
            crate::network::encode_key_hex(&credentials.netcode_key)
        }
        crate::network::ServerAccess::Open => "OPEN".to_owned(),
    });
    info!(
        access = ?credentials.access,
        join_code = %crate::network::encode_key_hex(&credentials.netcode_key),
        "hosted world access configured"
    );
    spawn_server_thread(cfg, credentials, &mut session);
}

fn spawn_server_thread(
    config: ServerSaveConfig,
    credentials: crate::network::ServerCredentials,
    session: &mut ServerSession,
) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let save_request = Arc::new(AtomicBool::new(false));
    let save_result = Arc::new(AtomicU8::new(SAVE_RESULT_NONE));
    let shutdown_for_thread = shutdown.clone();
    let save_for_thread = save_request.clone();
    let result_for_thread = save_result.clone();
    let handle = std::thread::Builder::new()
        .name("block-junk-server".into())
        .spawn(move || {
            crate::run_server_with_shutdown(
                shutdown_for_thread,
                save_for_thread,
                result_for_thread,
                config,
                credentials,
            );
        })
        .expect("spawn server thread");
    session.handle = Some(handle);
    session.shutdown = Some(shutdown);
    session.save_request = Some(save_request);
    session.save_result = Some(save_result);
}

/// Visible from tests / dev tooling that want to drive the server App without
/// the client App, e.g. integration tests for save/load.
#[allow(dead_code)]
pub fn shutdown_after(flag: &Arc<AtomicBool>, after: Duration) {
    let flag = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        flag.store(true, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autosave_on_exit_is_the_default() {
        assert!(!DebugNoSaveOnExit::default().0);
    }

    #[test]
    fn shutdown_request_keeps_handle_until_thread_can_be_joined() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let allow_exit = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = shutdown.clone();
        let finished_for_thread = finished.clone();
        let allow_exit_for_thread = allow_exit.clone();
        let handle = std::thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            finished_for_thread.store(true, Ordering::SeqCst);
            while !allow_exit_for_thread.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });
        let mut session = ServerSession {
            handle: Some(handle),
            shutdown: Some(shutdown),
            save_request: None,
            save_result: None,
            shutdown_requested: false,
        };

        session.request_shutdown();
        while !finished.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        assert!(!session.join_if_finished());
        allow_exit.store(true, Ordering::SeqCst);
        while !session.join_if_finished() {
            std::thread::yield_now();
        }

        assert!(finished.load(Ordering::SeqCst));
        assert!(!session.is_hosting());
        assert!(!session.shutdown_requested());
    }
}
