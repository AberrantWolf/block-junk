//! Lightyear network setup. Mode-aware: spawns the server entity on
//! server/host, the host-client entity in host mode, the netcode-client in
//! split-client mode.
//!
//! Cross-side messaging shape (see networking-design skill):
//!   - server-only state: `ChunkMap`, `Chunk` components — never replicated.
//!   - client-side state: built from `ChunkSnapshot` (initial) +
//!     `BlockEdit` broadcasts (deltas).
//!   - one ordered-reliable `WorldChannel` carries both message types.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use bevy::prelude::*;
use lightyear::prelude::server::Start;
use lightyear::prelude::*;

use crate::protocol::{BlockEdit, ChunkSnapshot, WorldChannel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    /// Dedicated server (no rendering). Listens on UDP for clients.
    Server,
    /// Dedicated client connecting to a remote server over UDP.
    Client,
    /// Server + a local host-client in the same process. No transport.
    Host,
}

pub const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5050);
pub const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

pub struct NetworkPlugin {
    pub mode: NetMode,
}

impl Plugin for NetworkPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ProtocolPlugin);

        match self.mode {
            NetMode::Server => {
                app.add_systems(Startup, start_netcode_server);
            }
            NetMode::Host => {
                app.add_systems(Startup, start_host_server);
                app.add_systems(PostStartup, connect_host_client);
            }
            NetMode::Client => {
                app.add_systems(Startup, start_netcode_client);
            }
        }
    }
}

/// Wire-protocol registration. Messages, channels, and entity-mapping bits
/// for anything that crosses the client/server boundary.
struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.add_channel::<WorldChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        app.register_message::<BlockEdit>()
            .add_direction(NetworkDirection::Bidirectional);
        app.register_message::<ChunkSnapshot>()
            .add_direction(NetworkDirection::ServerToClient);
    }
}

#[derive(Resource)]
struct LocalServerEntity(Entity);

fn start_host_server(mut commands: Commands) {
    let server = commands.spawn(Server::default()).id();
    commands.trigger(Start { entity: server });
    commands.insert_resource(LocalServerEntity(server));
    info!("host server started ({:?})", server);
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
    info!("host client connecting ({:?})", host_client);
}

fn start_netcode_server(mut commands: Commands) {
    use lightyear::prelude::server::{NetcodeConfig, NetcodeServer, ServerUdpIo};

    let server = commands
        .spawn((
            NetcodeServer::new(NetcodeConfig::default()),
            LocalAddr(SERVER_ADDR),
            ServerUdpIo::default(),
        ))
        .id();
    commands.trigger(Start { entity: server });
    info!("netcode server listening on {SERVER_ADDR}");
}

fn start_netcode_client(mut commands: Commands) {
    use lightyear::netcode::Key;
    use lightyear::prelude::client::{NetcodeClient, NetcodeConfig};
    // Authentication and UdpIo come from the top-level prelude, not the
    // client-only one. Imported via the file's `lightyear::prelude::*`.

    let auth = Authentication::Manual {
        server_addr: SERVER_ADDR,
        client_id: 0,
        private_key: Key::default(),
        protocol_id: 0,
    };
    let client = match NetcodeClient::new(auth, NetcodeConfig::default()) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to construct NetcodeClient: {e}");
            return;
        }
    };
    let entity = commands
        .spawn((
            Client::default(),
            LocalAddr(CLIENT_ADDR),
            PeerAddr(SERVER_ADDR),
            Link::new(None),
            ReplicationReceiver::default(),
            client,
            UdpIo::default(),
        ))
        .id();
    commands.trigger(Connect { entity });
    info!("netcode client connecting to {SERVER_ADDR}");
}
