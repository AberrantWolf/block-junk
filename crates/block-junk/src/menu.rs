//! App lifecycle: main menu, esc-pause menu, host-thread management.
//!
//! The client process always starts as an `AppState::MainMenu` unless the
//! `client [addr]` CLI shortcut pre-sets `InGame`. Entering `InGame` is what
//! actually starts a game session — that's when the lightyear client triggers
//! `Connect`, and (in host mode) the server-side App is spawned on a worker
//! thread. Exiting `InGame` tears both down.
//!
//! State cleanup on quit-to-menu is currently partial: we disconnect the
//! client and stop the server thread, but in-world entities (replicated
//! avatars, chunk entities, UI nodes) are left orphaned. Re-entering a game
//! from the menu without a process restart is therefore not yet supported.
//! Tracked as future work.
//!
//! See `project_npc_design.md` for the wider future-work backlog this slots
//! into; this module's job is the "executable lifecycle" prerequisite.

use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
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

use crate::network::SERVER_ADDR;
use crate::protocol::AvatarPose;
use crate::save::{SaveMetadata, delete_save, list_saves, save_exists, validate_name};
use crate::voxel::world_to_chunk;

/// Top-level lifecycle states for the client App. The server App, when
/// hosting, runs in its own thread and has no `AppState` — it just runs
/// until its shutdown flag is set.
#[derive(States, Default, Debug, Clone, Eq, PartialEq, Hash)]
pub enum AppState {
    #[default]
    MainMenu,
    InGame,
    Paused,
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

impl ServerSaveConfig {
    pub fn dedicated() -> Self {
        Self {
            save_name: None,
            load_existing: false,
            no_save_on_exit: true,
        }
    }
}

/// The address the lightyear client should connect to once `OnEnter(InGame)`
/// fires. Host mode points at localhost; JoinRemote points at the menu input.
#[derive(Resource, Clone, Copy, Debug)]
pub struct JoinTarget(pub SocketAddr);

impl Default for JoinTarget {
    fn default() -> Self {
        Self(SERVER_ADDR)
    }
}

/// Handle to the server thread spawned when hosting. None when running as a
/// pure client (JoinRemote) or before any game has started.
///
/// Three Arc<AtomicBool> flags coordinate cross-thread state with the server
/// App. The client sets them; the server polls and acts. We use atomics
/// rather than channels because (a) the server doesn't need backpressure
/// and (b) atomics survive cleanly if the client crashes mid-set.
#[derive(Resource, Default)]
pub struct ServerSession {
    handle: Option<JoinHandle<()>>,
    shutdown: Option<Arc<AtomicBool>>,
    save_request: Option<Arc<AtomicBool>>,
}

impl ServerSession {
    pub fn is_hosting(&self) -> bool {
        self.handle.is_some()
    }

    /// Signal the server thread to exit, then detach. We *don't* join here:
    /// joining would block the main thread for a tick or two, and on
    /// quit-to-desktop we're about to terminate the process anyway — the OS
    /// reaps the thread. The trade-off is that a noisy server (still
    /// flushing UDP, say) might keep a thread alive a few ms after main
    /// exits; harmless. If we ever need a clean handover (e.g. quit-to-menu
    /// followed immediately by re-host on the same port), we'd join here.
    pub fn signal_shutdown(&mut self) {
        if let Some(flag) = self.shutdown.take() {
            flag.store(true, Ordering::SeqCst);
        }
        // Drop the handle to detach. We're not waiting.
        let _ = self.handle.take();
    }

    /// Request a mid-session save. Server clears the flag once it has
    /// written to disk; spamming the button is harmless (extra requests
    /// during the same tick just collapse).
    pub fn request_save(&self) {
        if let Some(flag) = self.save_request.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Inserted into the server App as a Resource so the shutdown-check system
/// can read it. Setting it true causes the server App to emit `AppExit`.
#[derive(Resource, Clone)]
pub struct ServerShutdownFlag(pub Arc<AtomicBool>);

/// Mid-session save request. Set true to make the server flush to disk
/// without exiting; the server clears it once the save is written.
#[derive(Resource, Clone)]
pub struct ServerSaveRequestFlag(pub Arc<AtomicBool>);

/// Don't auto-save on Quit-to-menu / Quit-to-desktop. Defaulting to `true`
/// during development per design note — pre-ship pass will flip to `false`.
/// Visible in the pause menu as a checkbox so it can be flipped per-session
/// without rebuilding.
#[derive(Resource, Clone, Copy)]
pub struct DebugNoSaveOnExit(pub bool);

impl Default for DebugNoSaveOnExit {
    fn default() -> Self {
        Self(true)
    }
}

/// Marker for entities spawned during a game session that should be cleaned
/// up on quit-to-menu. Phase A doesn't actually exercise the cleanup path
/// (no quit-to-menu yet — see module docs), but tagging now means the cleanup
/// system in phase A.7 just queries for this marker.
#[derive(Component)]
pub struct GameRoot;

pub struct MenuPlugin;

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default());
        app.init_state::<AppState>();
        app.init_resource::<JoinTarget>();
        app.init_resource::<ServerSession>();
        app.init_resource::<DebugNoSaveOnExit>();
        app.init_resource::<ConnectAddrInput>();
        app.init_resource::<NewWorldName>();
        app.init_resource::<SaveListing>();
        app.init_resource::<SaveStatus>();
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
                main_menu_ui.run_if(in_state(AppState::MainMenu)),
                pause_menu_ui.run_if(in_state(AppState::Paused)),
                debug_overlay_ui.run_if(in_state(AppState::InGame)),
            ),
        );

        app.add_systems(
            Update,
            toggle_pause.run_if(in_state(AppState::InGame).or(in_state(AppState::Paused))),
        );

        // Server thread lifecycle is tied to *session* boundaries, not to
        // InGame ↔ Paused. Pausing must not tear down the server; only
        // explicit quit (to menu or desktop) does.
        app.add_systems(OnEnter(AppState::InGame), spawn_server_if_hosting);
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
struct SaveListing(Vec<SaveMetadata>);

/// Most-recent save error (delete failed, create with bad name, etc.) so
/// the main menu can surface it. Cleared on next valid action.
#[derive(Resource, Default)]
struct SaveStatus(Option<String>);

fn main_menu_ui(
    mut contexts: EguiContexts,
    mut next_state: ResMut<NextState<AppState>>,
    mut commands: Commands,
    mut join_target: ResMut<JoinTarget>,
    mut addr_input: ResMut<ConnectAddrInput>,
    mut new_name: ResMut<NewWorldName>,
    mut listing: ResMut<SaveListing>,
    mut status: ResMut<SaveStatus>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    if addr_input.0.is_empty() {
        addr_input.0 = SERVER_ADDR.to_string();
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("block-junk");
            ui.add_space(24.0);
        });

        ui.separator();
        ui.add_space(8.0);
        ui.heading("Worlds");

        // Existing worlds list.
        if listing.0.is_empty() {
            ui.label(egui::RichText::new("(no saves yet)").italics().weak());
        } else {
            let mut load_request: Option<String> = None;
            let mut delete_request: Option<String> = None;
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    for meta in &listing.0 {
                        ui.horizontal(|ui| {
                            if ui.button("Load").clicked() {
                                load_request = Some(meta.name.clone());
                            }
                            if ui.button("Delete").clicked() {
                                delete_request = Some(meta.name.clone());
                            }
                            ui.label(
                                egui::RichText::new(&meta.name).strong().monospace(),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "({})",
                                    relative_time(meta.modified_at)
                                ))
                                .weak(),
                            );
                        });
                    }
                });
            if let Some(name) = load_request {
                commands.insert_resource(LaunchMode::HostLoad {
                    save_name: name.clone(),
                });
                *join_target = JoinTarget(SERVER_ADDR);
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
                            *join_target = JoinTarget(SERVER_ADDR);
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

        if let Some(msg) = &status.0 {
            ui.colored_label(egui::Color32::YELLOW, msg);
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.heading("Multiplayer");
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

        ui.add_space(24.0);
        ui.separator();
        ui.vertical_centered(|ui| {
            if ui.button("Quit").clicked() {
                exit.write(AppExit::Success);
                // No server session at the main menu; 1s is plenty for a
                // clean Bevy shutdown if it works at all.
                arm_quit_watchdog(Duration::from_secs(1));
            }
        });
    });
}

fn refresh_save_listing(mut listing: ResMut<SaveListing>, mut new_name: ResMut<NewWorldName>) {
    listing.0 = list_saves().unwrap_or_default();
    // Auto-pick a free default name so consecutive "Create" clicks don't
    // collide.
    if save_exists(&new_name.0) {
        new_name.0 = next_free_world_name(&listing.0);
    }
}

fn next_free_world_name(existing: &[SaveMetadata]) -> String {
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

fn pause_menu_ui(
    mut contexts: EguiContexts,
    mut next_state: ResMut<NextState<AppState>>,
    mut session: ResMut<ServerSession>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let hosting = session.is_hosting();
    egui::Window::new("Paused")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                if ui.button("Resume").clicked() {
                    next_state.set(AppState::InGame);
                }
                // Save Now bypasses DebugNoSaveOnExit so you can verify a
                // save without quitting. Hidden on JoinRemote (the local
                // App isn't authoritative over the world).
                ui.add_enabled_ui(hosting, |ui| {
                    if ui.button("Save Now").clicked() {
                        session.request_save();
                    }
                });
                // Quit-to-menu is a stub: in-world state cleanup
                // (despawning replicated entities, resetting resources)
                // isn't built yet, so we exit the process the same way as
                // Quit-to-Desktop. See module docs.
                if ui.button("Quit to Menu (exits process for now)").clicked() {
                    session.signal_shutdown();
                    exit.write(AppExit::Success);
                    // 3s gives the server thread time to flush a save if one
                    // is in progress; a "no save" quit completes well under
                    // that. See `arm_quit_watchdog` doc.
                    arm_quit_watchdog(Duration::from_secs(3));
                }
                if ui.button("Quit to Desktop").clicked() {
                    session.signal_shutdown();
                    exit.write(AppExit::Success);
                    arm_quit_watchdog(Duration::from_secs(3));
                }
            });
        });
}

/// Small in-game overlay in the top-left corner showing the local player's
/// world position, the cell (block grid index) the camera is in, and the
/// chunk coord of that cell. Useful for reporting bugs by location.
fn debug_overlay_ui(
    mut contexts: EguiContexts,
    avatar: Query<&AvatarPose, With<Predicted>>,
) {
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
        .frame(
            egui::Frame::default()
                .fill(egui::Color32::from_black_alpha(160))
                .inner_margin(egui::Margin::same(6)),
        )
        .show(ctx, |ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
            ui.label(format!("pos   {:>7.2} {:>7.2} {:>7.2}", p.x, p.y, p.z));
            ui.label(format!("cell  {:>7} {:>7} {:>7}", cell.x, cell.y, cell.z));
            ui.label(format!(
                "chunk {:>3} {:>3} {:>3}   local {:>2} {:>2} {:>2}",
                chunk.0.x, chunk.0.y, chunk.0.z, local.x, local.y, local.z
            ));
            ui.label(format!("yaw   {:>7.2}", pose.yaw));
        });
}

fn toggle_pause(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<State<AppState>>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    match state.get() {
        AppState::InGame => next_state.set(AppState::Paused),
        AppState::Paused => next_state.set(AppState::InGame),
        AppState::MainMenu => {}
    }
}

/// On entering InGame in a host mode, spawn the server thread. On JoinRemote
/// this is a no-op.
fn spawn_server_if_hosting(
    launch: Option<Res<LaunchMode>>,
    debug_no_save: Res<DebugNoSaveOnExit>,
    mut session: ResMut<ServerSession>,
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
        let (handle, shutdown, save_request) = spawn_server_thread(cfg);
        session.handle = Some(handle);
        session.shutdown = Some(shutdown);
        session.save_request = Some(save_request);
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
    let (handle, shutdown, save_request) = spawn_server_thread(cfg);
    session.handle = Some(handle);
    session.shutdown = Some(shutdown);
    session.save_request = Some(save_request);
}

fn spawn_server_thread(
    config: ServerSaveConfig,
) -> (JoinHandle<()>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let save_request = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = shutdown.clone();
    let save_for_thread = save_request.clone();
    let handle = std::thread::Builder::new()
        .name("block-junk-server".into())
        .spawn(move || {
            crate::run_server_with_shutdown(shutdown_for_thread, save_for_thread, config);
        })
        .expect("spawn server thread");
    (handle, shutdown, save_request)
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
