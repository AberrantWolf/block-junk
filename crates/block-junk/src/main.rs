mod camera;
mod client;
mod protocol;
mod scripting;
mod server;
mod voxel;

use bevy::log::{DEFAULT_FILTER, LogPlugin};
use bevy::prelude::*;

use crate::protocol::{BlockEdit, GameSet};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "host".into());

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

    match mode.as_str() {
        "server" => {
            app.add_plugins(server::ServerPlugin);
        }
        "client" => {
            app.add_plugins(client::ClientPlugin);
        }
        _ => {
            app.add_plugins(server::ServerPlugin);
            app.add_plugins(client::ClientPlugin);
        }
    }

    app.run();
}
