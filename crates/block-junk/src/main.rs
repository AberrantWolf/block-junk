mod camera;
mod client;
mod protocol;
mod scripting;
mod server;
mod voxel;

use core::time::Duration;

use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;

use crate::protocol::{BlockEdit, GameSet};

const TICK_HZ: f64 = 60.0;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "host".into());
    let run_server = mode != "client";
    let run_client = mode != "server";
    let tick_duration = Duration::from_secs_f64(1.0 / TICK_HZ);

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(LogPlugin {
        // gilrs_core spams "Failed to find device" on macOS — IOKit reports
        // device IDs that aren't real controllers, and emits both warnings
        // and errors. Silence the whole module until we actually wire up
        // gamepad support and care what it has to say.
        filter: format!("{DEFAULT_FILTER}gilrs_core=off,"),
        ..default()
    }));
    app.add_plugins(avian3d::PhysicsPlugins::default());

    // lightyear plugin groups must be added BEFORE any ProtocolPlugin and
    // before our gameplay plugins. Both groups can coexist in host mode —
    // the shared sub-plugins dedupe internally.
    if run_server {
        app.add_plugins(lightyear::prelude::server::ServerPlugins { tick_duration });
    }
    if run_client {
        app.add_plugins(lightyear::prelude::client::ClientPlugins { tick_duration });
    }

    // Cross-plugin schedule order so client input → server simulation → re-mesh
    // runs in a single deterministic frame.
    app.configure_sets(
        Update,
        (
            GameSet::Input,
            GameSet::Simulation,
            GameSet::PostSimulation,
        )
            .chain(),
    );
    app.add_message::<BlockEdit>();

    if run_server {
        app.add_plugins(server::ServerPlugin);
    }
    if run_client {
        app.add_plugins(client::ClientPlugin);
    }

    app.run();
}
