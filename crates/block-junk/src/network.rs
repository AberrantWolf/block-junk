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

use crate::menu::{AppState, JoinTarget};
use crate::npc::{Npc, NpcPath};
use crate::protocol::{
    Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, BlockManifest,
    ChunkSnapshot, ChunkUnload, DebugAdvanceTime, DebugBumpNeed, MovementIntent, MovementMode,
    PlanEdit, PlanFullSync, WorldChannel, WorldClockSync,
};

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
            // Server App is spawned per game session, so its Startup
            // coincides with entering a game — bind the socket immediately.
            NetMode::Server => {
                app.add_systems(Startup, start_netcode_server);
            }
            // Client App outlives a session and shows a menu first. Defer
            // the netcode connect until the user starts a game.
            NetMode::Client => {
                app.add_systems(OnEnter(AppState::InGame), start_netcode_client);
            }
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
        app.register_message::<PlanEdit>()
            .add_direction(NetworkDirection::Bidirectional);
        app.register_message::<PlanFullSync>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<ChunkSnapshot>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<ChunkUnload>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<BlockManifest>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<WorldClockSync>()
            .add_direction(NetworkDirection::ServerToClient);
        // Debug-only client→server requests. No auth gate yet — see the
        // doc comments on `DebugSetClock` / `DebugBumpNeed`.
        app.register_message::<DebugAdvanceTime>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DebugBumpNeed>()
            .add_direction(NetworkDirection::ClientToServer);

        // Player-avatar replication. Server owns the avatar entities; the
        // marker tells receivers "attach a mesh," and `AvatarPose` is the
        // per-tick state. We deliberately don't replicate `Transform` — the
        // 40-byte rotation+scale baggage isn't used.
        // See networking-design: state for entities, events for the grid.
        app.register_component::<Actor>();
        app.register_component::<Avatar>();
        app.register_component::<Npc>();
        app.register_component::<NpcPath>();
        // AvatarPose participates in both prediction (owner rolls back when
        // server disagrees) and interpolation (remote viewers lerp between
        // server samples instead of snapping every 50 ms).
        app.register_component::<AvatarPose>()
            .add_prediction()
            .add_linear_interpolation();
        // Velocity, ground state, and movement mode are simulation-only —
        // remote viewers don't need them, but the predicted owner does
        // (rollback restarts the controller from these values, so they
        // must be in the prediction history).
        app.register_component::<AvatarVelocity>().add_prediction();
        app.register_component::<AvatarOnGround>().add_prediction();
        app.register_component::<MovementMode>().add_prediction();

        // Per-tick input replication. Adds `ActionState<MovementIntent>` and
        // the buffering machinery on both sides. Phase 2.4 hangs the
        // avatar entity off this; this registration alone is inert.
        app.add_plugins(input::native::InputPlugin::<MovementIntent>::default());
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

fn start_netcode_client(
    mut commands: Commands,
    target: Res<JoinTarget>,
    existing: Query<(), With<Client>>,
) {
    use lightyear::netcode::Key;
    use lightyear::prelude::client::{NetcodeClient, NetcodeConfig};
    // Authentication and UdpIo come from the top-level prelude (already
    // imported via `lightyear::prelude::*`).

    // `OnEnter(InGame)` fires every time the player un-pauses, but the
    // session — and the netcode client entity — outlives pause. Spawning
    // a second client with the same PID-derived id would race the first
    // until it timed out, spamming `ClientIdInUse` warnings for seconds.
    if !existing.is_empty() {
        return;
    }

    let server_addr = target.0;

    // Process-unique client ID. Hardcoding 0 means a second client trying
    // to connect to the same server gets `ClientIdInUse` — fine for unit
    // tests, fatal for actually playing together. A real auth flow lands
    // when we add accounts; PID is enough until then.
    let client_id: u64 = std::process::id() as u64;
    let auth = Authentication::Manual {
        server_addr,
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
            PeerAddr(server_addr),
            Link::new(None),
            ReplicationReceiver::default(),
            client,
            UdpIo::default(),
        ))
        .id();
    commands.trigger(Connect { entity });
    info!("netcode client connecting to {server_addr}");
}
