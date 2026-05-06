mod camera;
mod client;
mod network;
mod protocol;
mod scripting;
mod server;
mod voxel;

use core::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;

use crate::network::{NetMode, NetworkPlugin};
use crate::protocol::GameSet;

const TICK_HZ: f64 = 60.0;
/// Server tick interval. Drives both lightyear's protocol tick and the
/// headless server App's update loop.
fn tick_duration() -> Duration {
    Duration::from_secs_f64(1.0 / TICK_HZ)
}

fn main() {
    let mode_arg = std::env::args().nth(1).unwrap_or_else(|| "solo".into());
    match mode_arg.as_str() {
        "server" => run_server(),
        "client" => run_client(),
        _ => {
            // Solo: server thread, client main thread. The thread is detached
            // — when main exits, the process ends and the server dies with it.
            std::thread::spawn(run_server);
            run_client();
        }
    }
}

fn run_server() {
    let tick = tick_duration();
    let mut app = App::new();

    // Headless: MinimalPlugins drives the update loop via ScheduleRunnerPlugin.
    // Throttle the run loop to the tick rate so we don't peg a core spinning.
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(tick)));
    app.add_plugins(TransformPlugin);
    app.add_plugins(LogPlugin {
        filter: format!("{DEFAULT_FILTER}gilrs_core=off,"),
        ..default()
    });
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

    app.run();
}

fn run_client() {
    let tick = tick_duration();
    let mut app = App::new();

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
    configure_shared_schedule(&mut app);
    app.add_plugins(NetworkPlugin {
        mode: NetMode::Client,
    });
    app.add_plugins(client::ClientPlugin);

    app.run();
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
