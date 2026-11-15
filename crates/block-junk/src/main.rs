mod action_jobs;
#[cfg(test)]
mod architecture;
mod architecture_solver;
mod block_textures;
mod blocks;
mod build_palette;
mod camera;
mod capabilities;
mod capability_input;
mod capability_network;
mod civilization;
mod client;
mod client_chunks;
mod collision;
mod construction;
mod containers;
mod content_contracts;
#[cfg(test)]
mod content_validation;
mod craft_progress_hud;
mod craft_stations;
mod craft_ui;
mod debug;
mod facility_access;
mod facility_interactions;
mod facility_inventory;
mod haul;
mod identity;
mod inspect_panel;
mod interactables;
mod item_placement;
mod items;
mod live_spaces;
mod menu;
mod mod_assets;
mod mod_commands;
mod modset;
mod network;
mod npc;
mod npc_mover;
mod npc_registry;
mod pathfinding;
mod physics;
mod placement_network;
mod placement_preview;
mod placements;
mod plan_claims;
mod plan_ghosts;
mod plans;
mod player_mode;
mod portal_service;
mod preview;
mod protocol;
mod recipes;
mod regrow;
mod reservations;
#[cfg(test)]
mod rooms;
mod save;
mod save_upgrade;
mod scripting;
mod server;
mod settlements;
mod space_identity;
mod space_network;
mod spatial;
mod spatial_events;
mod spatial_geometry;
mod spatial_revision;
mod storage;
mod surface_navigation;
mod target_outline;
mod terrain_transaction;
mod ui_capture;
mod ui_theme;
mod voxel;
mod world_rules;
mod worldspace_toast;

#[cfg(test)]
mod allocation_counter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CALLS: AtomicU64 = AtomicU64::new(0);
    static REQUESTED_BYTES: AtomicU64 = AtomicU64::new(0);

    struct CountingAllocator;
    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            REQUESTED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            // SAFETY: forwards the unchanged allocation contract to System.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: forwards the original pointer and layout to System.
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            REQUESTED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            // SAFETY: forwards the original allocation contract to System.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub fn reset() {
        CALLS.store(0, Ordering::Relaxed);
        REQUESTED_BYTES.store(0, Ordering::Relaxed);
    }
    pub fn snapshot() -> (u64, u64) {
        (
            CALLS.load(Ordering::Relaxed),
            REQUESTED_BYTES.load(Ordering::Relaxed),
        )
    }
}

use core::sync::atomic::{AtomicBool, AtomicU8};
use core::time::Duration;
use std::sync::Arc;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;
use bevy::window::WindowPlugin;

use crate::menu::{
    AppState, JoinTarget, LaunchMode, MenuPlugin, ServerSaveConfig, ServerSaveRequestFlag,
    ServerSaveResultFlag, ServerShutdownFlag,
};
use crate::network::{NetMode, NetworkPlugin};
use crate::protocol::GameSet;

const TICK_HZ: f64 = 60.0;
/// Server tick interval. Drives both lightyear's protocol tick and the
/// headless server App's update loop.
fn tick_duration() -> Duration {
    Duration::from_secs_f64(1.0 / TICK_HZ)
}

#[derive(Default)]
struct ContentLaunch {
    mods_dir: Option<std::ffi::OsString>,
    preset: Option<String>,
}
static CONTENT_LAUNCH: std::sync::OnceLock<ContentLaunch> = std::sync::OnceLock::new();
fn content_launch() -> &'static ContentLaunch {
    CONTENT_LAUNCH.get_or_init(Default::default)
}

fn game_arguments() -> Vec<String> {
    let mut args = std::env::args().skip(1);
    let mut result = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--mods-dir" || arg == "--preset" {
            args.next();
        } else {
            result.push(arg);
        }
    }
    result
}

enum CliMode {
    /// Dedicated headless server: `server [save_name] [--bind ip:port]`.
    /// Loads `save_name` if it exists (creates it otherwise) and
    /// quit-saves on Ctrl-C. No UI; lives until SIGINT.
    DedicatedServer {
        save_name: String,
        bind: core::net::SocketAddr,
        requested_access: Option<(crate::network::ServerAccess, Option<[u8; 32]>)>,
        administrator_keys: Vec<[u8; 32]>,
    },
    /// Pure client connecting to `addr`. Skips the main menu.
    Client {
        addr: Option<core::net::SocketAddr>,
        invite_secret: Option<[u8; 32]>,
    },
    /// Hosted session loading `save_name`, skipping the main menu — the
    /// same path as menu → Load, for dev/debug of real saves.
    HostLoad { save_name: String },
    /// Default: client with the main menu.
    Solo,
}

/// Fallback world name for a dedicated server started without one.
const DEDICATED_DEFAULT_SAVE: &str = "dedicated";

fn parse_cli() -> CliMode {
    let mut args = game_arguments().into_iter();
    let Some(first) = args.next() else {
        return CliMode::Solo;
    };
    match first.trim_start_matches('-') {
        "server" | "s" => {
            let mut save_name = DEDICATED_DEFAULT_SAVE.to_string();
            let mut bind = crate::network::DEFAULT_BIND_ADDR;
            let mut requested_access = None;
            let mut administrator_keys = Vec::new();
            while let Some(arg) = args.next() {
                if arg == "--bind" {
                    match args.next().map(|raw| (raw.parse(), raw)) {
                        Some((Ok(addr), _)) => bind = addr,
                        Some((Err(e), raw)) => {
                            eprintln!("invalid --bind addr {raw:?}: {e}; using {bind}");
                        }
                        None => eprintln!("--bind needs an ip:port argument; using {bind}"),
                    }
                } else if arg == "--open" {
                    if requested_access.is_some() {
                        panic!("--open conflicts with --invite-secret");
                    }
                    requested_access = Some((crate::network::ServerAccess::Open, None));
                } else if arg == "--invite-secret" {
                    if requested_access.is_some() {
                        panic!("--invite-secret conflicts with --open or a duplicate flag");
                    }
                    let raw = args.next().expect("--invite-secret needs a HEX argument");
                    let key = crate::network::decode_key_hex(&raw)
                        .unwrap_or_else(|error| panic!("invalid --invite-secret: {error}"));
                    requested_access = Some((crate::network::ServerAccess::Invite, Some(key)));
                } else if arg == "--admin-key" {
                    let raw = args.next().expect("--admin-key needs a HEX argument");
                    administrator_keys.push(
                        crate::network::decode_key_hex(&raw)
                            .unwrap_or_else(|error| panic!("invalid --admin-key: {error}")),
                    );
                } else {
                    save_name = arg;
                }
            }
            CliMode::DedicatedServer {
                save_name,
                bind,
                requested_access,
                administrator_keys,
            }
        }
        "client" | "c" => {
            let mut addr = None;
            let mut invite_secret = None;
            while let Some(arg) = args.next() {
                if arg == "--invite-secret" {
                    let raw = args.next().expect("--invite-secret needs a HEX argument");
                    invite_secret = Some(
                        crate::network::decode_key_hex(&raw)
                            .unwrap_or_else(|error| panic!("invalid --invite-secret: {error}")),
                    );
                } else {
                    addr = match arg.parse() {
                        Ok(value) => Some(value),
                        Err(error) => panic!("invalid client addr {arg:?}: {error}"),
                    };
                }
            }
            CliMode::Client {
                addr,
                invite_secret,
            }
        }
        "host" | "h" => match args.next() {
            Some(save_name) => CliMode::HostLoad { save_name },
            None => {
                eprintln!("host needs a save name; opening the menu instead");
                CliMode::Solo
            }
        },
        _ => CliMode::Solo,
    }
}

fn main() {
    let raw: Vec<_> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|arg| arg == "validate-mods") {
        std::process::exit(crate::scripting::validate_mods(&raw[1..]));
    }
    let mut options = ContentLaunch::default();
    let mut args = raw.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mods-dir" => {
                options.mods_dir =
                    Some(args.next().expect("--mods-dir requires a directory").into())
            }
            "--preset" => {
                options.preset = Some(args.next().expect("--preset requires an id").clone())
            }
            _ => {}
        }
    }
    let _ = CONTENT_LAUNCH.set(options);
    match parse_cli() {
        CliMode::DedicatedServer {
            save_name,
            bind,
            requested_access,
            administrator_keys,
        } => run_dedicated_server(save_name, bind, requested_access, administrator_keys),
        CliMode::Client {
            addr,
            invite_secret,
        } => {
            let target = addr.unwrap_or(crate::network::LOCAL_CONNECT_ADDR);
            run_client(Some(LaunchMode::JoinRemote { addr: target }), invite_secret);
        }
        CliMode::HostLoad { save_name } => {
            run_client(Some(LaunchMode::HostLoad { save_name }), None);
        }
        CliMode::Solo => run_client(None, None),
    }
}

/// Dedicated server: same save lifecycle as a hosted session, with
/// Ctrl-C standing in for the quit button. The signal handler sets the
/// shutdown flag; `save_then_shutdown` (server.rs) does the rest.
fn run_dedicated_server(
    save_name: String,
    bind: core::net::SocketAddr,
    requested_access: Option<(crate::network::ServerAccess, Option<[u8; 32]>)>,
    administrator_keys: Vec<[u8; 32]>,
) {
    let load_existing = crate::save::save_exists(&save_name);
    let credentials = crate::network::world_credentials(
        &save_name,
        load_existing,
        requested_access,
        administrator_keys,
    )
    .unwrap_or_else(|error| panic!("cannot configure server access: {error}"));
    let shutdown = Arc::new(AtomicBool::new(false));
    let handler_flag = shutdown.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        if handler_flag.swap(true, core::sync::atomic::Ordering::SeqCst) {
            // Second Ctrl-C: the quit-save is either done or hung.
            // 130 = conventional SIGINT exit code.
            eprintln!("second Ctrl-C — force-quitting; the save may not have been written");
            std::process::exit(130);
        }
        eprintln!("Ctrl-C: saving and shutting down (press again to force-quit)");
    }) {
        eprintln!("WARNING: cannot install Ctrl-C handler ({e}); kill will skip the quit-save");
    }
    let config = ServerSaveConfig {
        preset: content_launch().preset.clone(),
        save_name: Some(save_name),
        load_existing,
        no_save_on_exit: false,
    };
    run_server_inner(
        Some(shutdown),
        None,
        config,
        /*install_log_plugin*/ true,
        bind,
        credentials,
    );
}

/// Public entrypoint for the menu module to spawn a hosted server on a
/// worker thread.
///   - `shutdown` flag polled each tick; when set, server saves (per
///     `config`) then emits `AppExit`.
///   - `save_request` flag polled each tick; when set, server saves
///     mid-session and clears the flag.
pub fn run_server_with_shutdown(
    shutdown: Arc<AtomicBool>,
    save_request: Arc<AtomicBool>,
    save_result: Arc<AtomicU8>,
    config: ServerSaveConfig,
    credentials: crate::network::ServerCredentials,
) {
    run_server_inner(
        Some(shutdown),
        Some((save_request, save_result)),
        config,
        /*install_log_plugin*/ false,
        // Hosted worlds are always LAN-joinable — no private solo bind.
        crate::network::DEFAULT_BIND_ADDR,
        credentials,
    );
}

fn run_server_inner(
    shutdown: Option<Arc<AtomicBool>>,
    save_request: Option<(Arc<AtomicBool>, Arc<AtomicU8>)>,
    save_config: ServerSaveConfig,
    install_log_plugin: bool,
    bind: core::net::SocketAddr,
    credentials: crate::network::ServerCredentials,
) {
    let tick = tick_duration();
    let mut app = App::new();

    // Headless: MinimalPlugins drives the update loop via ScheduleRunnerPlugin.
    // Throttle the run loop to the tick rate so we don't peg a core spinning.
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(tick)));
    app.add_plugins(TransformPlugin);
    // Bevy 0.19 panics on init_state without this; MinimalPlugins
    // doesn't bring it and DefaultPlugins (which does) is client-only.
    app.add_plugins(bevy::state::app::StatesPlugin);
    if install_log_plugin {
        app.add_plugins(LogPlugin {
            filter: format!("{DEFAULT_FILTER}gilrs_core=off,"),
            ..default()
        });
    }
    // TODO: avian3d needs server-headless feature flags before we can add
    // physics on the dedicated server. Defer until we have a server-side use.

    app.add_plugins(lightyear::prelude::server::ServerPlugins {
        tick_duration: tick,
    });
    configure_shared_schedule(&mut app);
    app.insert_resource(credentials);
    app.add_plugins(NetworkPlugin {
        mode: NetMode::Server,
    });
    app.add_plugins(server::ServerPlugin);
    app.add_plugins(civilization::CivilizationPlugin);

    if let Some(flag) = shutdown {
        app.insert_resource(ServerShutdownFlag(flag));
    }
    if let Some((request, result)) = save_request {
        app.insert_resource(ServerSaveRequestFlag(request));
        app.insert_resource(ServerSaveResultFlag(result));
    }
    app.insert_resource(save_config);
    app.insert_resource(crate::network::ServerBindAddr(bind));

    app.run();
}

fn run_client(preset: Option<LaunchMode>, invite_secret: Option<[u8; 32]>) {
    let tick = tick_duration();
    let mut app = App::new();

    let root = content_launch()
        .mods_dir
        .clone()
        .or_else(|| std::env::var_os("BLOCK_JUNK_MODS_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "mods".into());
    let roots = block_junk_scripting::resources::ResourceRoots::discover(&root)
        .unwrap_or_else(|e| panic!("mod resource discovery failed: {e}"));
    app.register_asset_source(
        "mods",
        bevy::asset::io::AssetSourceBuilder::new(move || {
            Box::new(crate::mod_assets::ModAssetReader {
                roots: roots.clone(),
            })
        }),
    );

    app.add_plugins(
        DefaultPlugins
            .set(LogPlugin {
                // gilrs_core spams "Failed to find device" on macOS — IOKit
                // reports device IDs that aren't real controllers. Silence the
                // whole module until we wire up gamepad support and care what
                // it has to say.
                filter: format!("{DEFAULT_FILTER}gilrs_core=off,"),
                ..default()
            })
            .set(WindowPlugin {
                // The titlebar ✕ / Cmd-Q must not despawn the window directly
                // — that exits the process out from under the hosted server
                // thread before it can quit-save. menu.rs::quit_on_window_close
                // reads the request and routes it through the same
                // save-then-exit path as the pause menu's quit buttons.
                close_when_requested: false,
                ..default()
            }),
    );
    app.add_plugins(avian3d::PhysicsPlugins::default());

    app.add_plugins(lightyear::prelude::client::ClientPlugins {
        tick_duration: tick,
    });
    // Gameplay sets are chained AND gated on in_state(InGame) so nothing
    // gameplay-y runs while the main menu is open.
    app.configure_sets(
        Update,
        (
            GameSet::Input,
            GameSet::Simulation,
            GameSet::SpatialSync,
            GameSet::PostSimulation,
        )
            .chain()
            .run_if(in_state(AppState::InGame)),
    );
    app.add_plugins(ui_capture::UiCapturesPlugin);
    app.add_plugins(MenuPlugin);
    app.init_resource::<crate::network::JoinCredentials>();
    if let Some(secret) = invite_secret {
        app.insert_resource(crate::network::JoinCredentials(secret));
    }
    app.add_plugins(NetworkPlugin {
        mode: NetMode::Client,
    });
    app.add_plugins(client::ClientPlugin);

    // CLI shortcut: skip the menu and go straight to a session.
    if let Some(mode) = preset {
        if let LaunchMode::JoinRemote { addr } = &mode {
            app.insert_resource(JoinTarget(*addr));
        }
        app.insert_resource(mode);
        app.add_systems(Startup, kick_to_ingame);
    }

    app.run();
}

fn kick_to_ingame(mut next: ResMut<NextState<AppState>>) {
    next.set(AppState::InGame);
}

/// Schedule order shared by both Apps. Input → Simulation → PostSimulation
/// happens in one frame regardless of which side a system lives on.
fn configure_shared_schedule(app: &mut App) {
    app.configure_sets(
        Update,
        (
            GameSet::Input,
            GameSet::Simulation,
            GameSet::SpatialSync,
            GameSet::PostSimulation,
        )
            .chain(),
    );
}
