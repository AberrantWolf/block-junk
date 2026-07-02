mod block_textures;
mod blocks;
mod camera;
mod civilization;
mod client;
mod client_chunks;
mod collision;
mod craft_progress_hud;
mod craft_stations;
mod craft_ui;
mod debug;
mod haul;
mod identity;
mod inspect_panel;
mod interactables;
mod items;
mod menu;
mod modset;
mod network;
mod npc;
mod npc_registry;
mod pathfinding;
mod physics;
mod plan_claims;
mod plan_ghosts;
mod plans;
mod player_mode;
mod preview;
mod protocol;
mod recipes;
mod rooms;
mod save;
mod scripting;
mod server;
mod target_outline;
mod ui_capture;
mod ui_theme;
mod voxel;
mod worldspace_toast;

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

enum CliMode {
    /// Dedicated headless server: `server [save_name] [--bind ip:port]`.
    /// Loads `save_name` if it exists (creates it otherwise) and
    /// quit-saves on Ctrl-C. No UI; lives until SIGINT.
    DedicatedServer {
        save_name: String,
        bind: core::net::SocketAddr,
    },
    /// Pure client connecting to `addr`. Skips the main menu.
    Client { addr: Option<core::net::SocketAddr> },
    /// Default: client with the main menu.
    Solo,
}

/// Fallback world name for a dedicated server started without one.
const DEDICATED_DEFAULT_SAVE: &str = "dedicated";

fn parse_cli() -> CliMode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return CliMode::Solo;
    };
    match first.trim_start_matches('-') {
        "server" | "s" => {
            let mut save_name = DEDICATED_DEFAULT_SAVE.to_string();
            let mut bind = crate::network::DEFAULT_BIND_ADDR;
            while let Some(arg) = args.next() {
                if arg == "--bind" {
                    match args.next().map(|raw| (raw.parse(), raw)) {
                        Some((Ok(addr), _)) => bind = addr,
                        Some((Err(e), raw)) => {
                            eprintln!("invalid --bind addr {raw:?}: {e}; using {bind}");
                        }
                        None => eprintln!("--bind needs an ip:port argument; using {bind}"),
                    }
                } else {
                    save_name = arg;
                }
            }
            CliMode::DedicatedServer { save_name, bind }
        }
        "client" | "c" => {
            let addr = args.next().and_then(|raw| match raw.parse() {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("invalid client addr {raw:?}: {e}; falling back to default");
                    None
                }
            });
            CliMode::Client { addr }
        }
        _ => CliMode::Solo,
    }
}

fn main() {
    match parse_cli() {
        CliMode::DedicatedServer { save_name, bind } => run_dedicated_server(save_name, bind),
        CliMode::Client { addr } => {
            let target = addr.unwrap_or(crate::network::LOCAL_CONNECT_ADDR);
            run_client(Some(LaunchMode::JoinRemote { addr: target }));
        }
        CliMode::Solo => run_client(None),
    }
}

/// Dedicated server: same save lifecycle as a hosted session, with
/// Ctrl-C standing in for the quit button. The signal handler sets the
/// shutdown flag; `save_then_shutdown` (server.rs) does the rest.
fn run_dedicated_server(save_name: String, bind: core::net::SocketAddr) {
    let load_existing = crate::save::save_exists(&save_name);
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
) {
    run_server_inner(
        Some(shutdown),
        Some((save_request, save_result)),
        config,
        /*install_log_plugin*/ false,
        // Hosted worlds are always LAN-joinable — no private solo bind.
        crate::network::DEFAULT_BIND_ADDR,
    );
}

fn run_server_inner(
    shutdown: Option<Arc<AtomicBool>>,
    save_request: Option<(Arc<AtomicBool>, Arc<AtomicU8>)>,
    save_config: ServerSaveConfig,
    install_log_plugin: bool,
    bind: core::net::SocketAddr,
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

fn run_client(preset: Option<LaunchMode>) {
    let tick = tick_duration();
    let mut app = App::new();

    // Register the `mods://` asset source so block defs can reference
    // mod-shipped meshes by `mods://<modname>/path/to/file.glb`. Must
    // run before `DefaultPlugins` initialises `AssetPlugin`.
    app.register_asset_source(
        "mods",
        bevy::asset::io::AssetSourceBuilder::platform_default("mods", None),
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
        (GameSet::Input, GameSet::Simulation, GameSet::PostSimulation)
            .chain()
            .run_if(in_state(AppState::InGame)),
    );
    app.add_plugins(ui_capture::UiCapturesPlugin);
    app.add_plugins(MenuPlugin);
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
        (GameSet::Input, GameSet::Simulation, GameSet::PostSimulation).chain(),
    );
}
