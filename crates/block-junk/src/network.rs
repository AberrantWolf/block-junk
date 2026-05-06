//! Lightyear network setup. Mode-aware: spawns the server entity on the
//! server, the host-client entity in host mode, and (eventually) a netcode
//! client for split-mode friends-play.
//!
//! The host-client pattern bypasses transport entirely — `lightyear`'s
//! `HostPlugin` notices a `Client` entity with a `LinkOf` pointing at a
//! `Started` `Server` and routes their messages through ECS instead of
//! serializing bytes. No crossbeam, no UDP, no netcode handshake.

use bevy::prelude::*;
use lightyear::prelude::client::*;
use lightyear::prelude::server::*;
use lightyear::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    /// Server only (dedicated). No local client.
    Server,
    /// Client only (connects to a remote server). Not yet wired.
    Client,
    /// Server + a local host-client in the same process.
    Host,
}

pub struct NetworkPlugin {
    pub mode: NetMode,
}

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ProtocolPlugin);

        if matches!(self.mode, NetMode::Server | NetMode::Host) {
            app.add_systems(Startup, start_server);
        }
        if matches!(self.mode, NetMode::Host) {
            // PostStartup so the server's `Start` trigger has been processed
            // and `Started` is on the entity by the time the host-client
            // tries to Connect.
            app.add_systems(PostStartup, connect_host_client);
        }
        if matches!(self.mode, NetMode::Client) {
            // TODO: spawn netcode/UDP client when split-mode lands.
        }
    }
}

/// Engine protocol — lives next to the engine because it knows about engine
/// types (Block, BlockEdit). Mod-facing types are in block-junk-mod-api.
struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, _app: &mut App) {
        // Empty for now — adding messages, channels, and replicated
        // components lands in step 3 of the lightyear plan.
    }
}

#[derive(Resource)]
struct LocalServerEntity(Entity);

fn start_server(mut commands: Commands) {
    let server = commands.spawn(Server::default()).id();
    commands.trigger(Start { entity: server });
    commands.insert_resource(LocalServerEntity(server));
    info!("server started (entity {:?})", server);
}

fn connect_host_client(mut commands: Commands, server: Res<LocalServerEntity>) {
    let host_client = commands
        .spawn((
            Client::default(),
            LinkOf { server: server.0 },
            Link::new(None),
            Linked,
        ))
        .id();
    commands.trigger(Connect { entity: host_client });
    info!("host client connecting (entity {:?})", host_client);
}
