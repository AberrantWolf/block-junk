mod block_textures;
mod blocks;
mod camera;
mod client;
mod collision;
mod debug;
mod inspect_panel;
mod interactables;
mod menu;
mod network;
mod npc;
mod npc_registry;
mod pathfinding;
mod physics;
mod plan_claims;
mod plans;
mod player_mode;
mod preview;
mod protocol;
mod rooms;
mod save;
mod scripting;
mod server;
mod target_outline;
mod voxel;

use core::sync::atomic::AtomicBool;
use core::time::Duration;
use std::sync::Arc;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;

use crate::menu::{
    AppState, JoinTarget, LaunchMode, MenuPlugin, ServerSaveConfig, ServerSaveRequestFlag,
    ServerShutdownFlag,
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
    /// Dedicated headless server. No UI; lives until SIGINT.
    DedicatedServer,
    /// Pure client connecting to `addr`. Skips the main menu.
    Client { addr: Option<core::net::SocketAddr> },
    /// Default: client with the main menu.
    Solo,
}

fn parse_cli() -> CliMode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        return CliMode::Solo;
    };
    match first.trim_start_matches('-') {
        "server" | "s" => CliMode::DedicatedServer,
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
        CliMode::DedicatedServer => run_server_inner(
            None,
            None,
            ServerSaveConfig::dedicated(),
            /*install_log_plugin*/ true,
        ),
        CliMode::Client { addr } => {
            let target = addr.unwrap_or(crate::network::SERVER_ADDR);
            run_client(Some(LaunchMode::JoinRemote { addr: target }));
        }
        CliMode::Solo => run_client(None),
    }
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
    config: ServerSaveConfig,
) {
    run_server_inner(
        Some(shutdown),
        Some(save_request),
        config,
        /*install_log_plugin*/ false,
    );
}

fn run_server_inner(
    shutdown: Option<Arc<AtomicBool>>,
    save_request: Option<Arc<AtomicBool>>,
    save_config: ServerSaveConfig,
    install_log_plugin: bool,
) {
    let tick = tick_duration();
    let mut app = App::new();

    // Headless: MinimalPlugins drives the update loop via ScheduleRunnerPlugin.
    // Throttle the run loop to the tick rate so we don't peg a core spinning.
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(tick)));
    app.add_plugins(TransformPlugin);
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

    if let Some(flag) = shutdown {
        app.insert_resource(ServerShutdownFlag(flag));
    }
    if let Some(flag) = save_request {
        app.insert_resource(ServerSaveRequestFlag(flag));
    }
    app.insert_resource(save_config);

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

    app.add_plugins(DefaultPlugins.set(LogPlugin {
        // gilrs_core spams "Failed to find device" on macOS — IOKit reports
        // device IDs that aren't real controllers. Silence the whole module
        // until we wire up gamepad support and care what it has to say.
        filter: format!("{DEFAULT_FILTER}gilrs_core=off,"),
        ..default()
    }));
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
            GameSet::PostSimulation,
        )
            .chain()
            .run_if(in_state(AppState::InGame)),
    );
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
        (
            GameSet::Input,
            GameSet::Simulation,
            GameSet::PostSimulation,
        )
            .chain(),
    );
}
