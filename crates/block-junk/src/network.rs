//! Lightyear network setup. Two modes only:
//!
//! - `Server` spawns a netcode-UDP listener on `SERVER_ADDR`.
//! - `Client` connects to `SERVER_ADDR` over UDP.
//!
//! Solo play (the `cargo run` default) is "spawn a server thread + run a
//! client App that connects to localhost." Same wire format as friends-mode;
//! no special-case host pattern.
//!
//! See the networking-design skill for what crosses the wire (events, not
//! state) and why we identify chunks by `ChunkCoord` rather than `Entity`.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use bevy::prelude::*;
use lightyear::prelude::server::Start;
use lightyear::prelude::*;

use crate::protocol::{BlockEdit, ChunkSnapshot, PlayerPosition, WorldChannel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    Server,
    Client,
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
            NetMode::Server => app.add_systems(Startup, start_netcode_server),
            NetMode::Client => app.add_systems(Startup, start_netcode_client),
        };
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
        app.register_message::<PlayerPosition>()
            .add_direction(NetworkDirection::ClientToServer);
    }
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
    // Authentication and UdpIo come from the top-level prelude (already
    // imported via `lightyear::prelude::*`).

    // Process-unique client ID. Hardcoding 0 means a second client trying
    // to connect to the same server gets `ClientIdInUse` — fine for unit
    // tests, fatal for actually playing together. A real auth flow lands
    // when we add accounts; PID is enough until then.
    let client_id: u64 = std::process::id() as u64;
    let auth = Authentication::Manual {
        server_addr: SERVER_ADDR,
        client_id,
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
