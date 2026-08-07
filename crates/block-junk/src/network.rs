//! Lightyear network setup. Two modes only:
//!
//! - `Server` spawns a netcode-UDP listener on [`ServerBindAddr`]
//!   (default: all interfaces, port 5050 — every hosted world is
//!   LAN-joinable by design; there is no private "solo bind").
//! - `Client` connects to [`JoinTarget`] over UDP (localhost when this
//!   process is also hosting).
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

use crate::craft_stations::{
    CancelOrder, DepositToStation, QueueOrder, StationUpdate, StationsFullSync, WorkStart, WorkStop,
};
use crate::menu::{AppState, JoinTarget};
use crate::npc::{Npc, NpcId, NpcPath};
use crate::protocol::{
    Actor, Avatar, AvatarOnGround, AvatarPose, AvatarVelocity, BlockEdit, ModSetManifest, Carrying,
    ChunkChannel, ChunkSnapshot, ChunkUnload, DebugAdvanceTime, DebugBumpNeed,
    DebugFillNearestPlan, DebugSpawnTools, DebugSpawnWorkbench, DepositRequest, DropRequest,
    DropToolRequest, EquippedTool, MovementIntent, MovementMode, NpcAnimOverride, NpcDetails,
    PeriodicSyncChannel, PickupRequest, PlanEdit, PlanEditBatch, PlanFullSync, RequestNpcDetails,
    StateSyncChannel, WorldChannel, WorldClockSync, WorldItem,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    Server,
    Client,
}

/// Default port for both hosted-thread and dedicated servers.
pub const SERVER_PORT: u16 = 5050;

/// Where a server binds unless overridden: all interfaces. Binding
/// loopback would make hosted worlds unreachable from other machines,
/// which contradicts the always-client architecture's whole point.
pub const DEFAULT_BIND_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SERVER_PORT);

/// Where the local client connects when this process is also hosting,
/// and the default the join-address field starts from.
pub const LOCAL_CONNECT_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SERVER_PORT);

pub const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

/// Netcode protocol discriminator, checked during the handshake before
/// any app code runs. Both sides must present the same value; a client
/// built with a different id is rejected at the transport layer. Bump
/// the low byte whenever the wire protocol changes incompatibly
/// (message shapes, channel set, replication registrations) — cheaper
/// than a desync hunt when someone joins with a stale build. The
/// registry-level mod-set check is a separate, later gate.
// …0003: NpcDetails gained `stats` (2026-07).
// …0004: storage zones — StorageEditBatch/StorageFullSync (2026-07, S1).
// …0005: WorldItem gained `count` — piles ride the same replication (2026-07, S2).
// …0006: container stock — ContainerUpdate/ContainersFullSync (2026-07, S3).
// (S4 finite-food adds no wire messages — the bush transform rides
//  BlockEdit, the eat draw-down rides ContainerUpdate, and the new
//  berry/bush registrations are caught by the mod-set gate — so the id
//  stays 0006. Save format is likewise unchanged: new blocks/items are
//  appended and remapped by id, so v17 saves load without migration.)
pub const NETCODE_PROTOCOL_ID: u64 = 0xB10C_6A31_0000_0006;

/// Which address the server socket binds. Inserted by
/// `run_server_inner`; the dedicated CLI can override the default.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ServerBindAddr(pub SocketAddr);

/// Best-effort local LAN address, for "friends can join at ..." UI.
/// A UDP socket "connected" to a public address reveals which
/// interface the OS would route through — no packet is actually sent
/// (UDP connect only sets the default destination). `None` when
/// offline or sandboxed.
pub fn local_lan_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

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
            // the netcode connect until the user starts a game. Identity
            // loads at build time (main thread, before any UI) so a
            // locked-id fallback is decided before the menu even shows.
            NetMode::Client => {
                app.insert_resource(crate::identity::load_or_create());
                app.add_systems(OnEnter(AppState::InGame), start_netcode_client);
                // Two-step client teardown on quit-to-menu: trigger the
                // graceful Disconnect at the state boundary, despawn the
                // connection entity once lightyear has marked it
                // `Disconnected` (a frame later, after the disconnect
                // packets have been flushed). The entity must be gone
                // before the next session — `start_netcode_client`'s
                // exists-guard would otherwise refuse to spawn a fresh
                // client and the new world would connect to nothing.
                app.add_systems(OnExit(AppState::InGame), disconnect_client_on_exit);
                app.add_systems(
                    Update,
                    despawn_disconnected_clients.run_if(in_state(AppState::MainMenu)),
                );
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

        app.add_channel::<ChunkChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::ServerToClient);

        app.add_channel::<StateSyncChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        app.add_channel::<PeriodicSyncChannel>(ChannelSettings {
            mode: ChannelMode::SequencedUnreliable,
            ..default()
        })
        .add_direction(NetworkDirection::ServerToClient);

        app.register_message::<BlockEdit>()
            .add_direction(NetworkDirection::Bidirectional);
        // Targeted reply to a refused request (reach gate etc.); feeds
        // the rejection-toast UI on the requesting client only.
        app.register_message::<crate::protocol::ActionRejected>()
            .add_direction(NetworkDirection::ServerToClient);
        // Mod-requested worldspace toasts (engine.ui.toast), broadcast
        // to everyone.
        app.register_message::<crate::protocol::WorldToast>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<PlanEdit>()
            .add_direction(NetworkDirection::Bidirectional);
        app.register_message::<PlanEditBatch>()
            .add_direction(NetworkDirection::Bidirectional);
        app.register_message::<PlanFullSync>()
            .add_direction(NetworkDirection::ServerToClient);
        // Storage-zone designation: batch deltas + connect-time full
        // sync (S1 of the storage arc).
        app.register_message::<crate::protocol::StorageEditBatch>()
            .add_direction(NetworkDirection::Bidirectional);
        app.register_message::<crate::protocol::StorageFullSync>()
            .add_direction(NetworkDirection::ServerToClient);
        // Room-state mirror: recognition deltas + connect-time full sync.
        app.register_message::<crate::protocol::RoomSync>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<crate::protocol::RoomRemove>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<crate::protocol::RoomsFullSync>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<ChunkSnapshot>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<ChunkUnload>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<ModSetManifest>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<WorldClockSync>()
            .add_direction(NetworkDirection::ServerToClient);
        // Debug-only client→server requests. No auth gate yet — see the
        // doc comments on `DebugSetClock` / `DebugBumpNeed`.
        app.register_message::<DebugAdvanceTime>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DebugBumpNeed>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DebugFillNearestPlan>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DebugSpawnTools>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DebugSpawnWorkbench>()
            .add_direction(NetworkDirection::ClientToServer);
        // NPC inspection RPC. Targeted reply — server uses the
        // requesting connection entity's MessageSender so other
        // clients don't see the response.
        app.register_message::<RequestNpcDetails>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<NpcDetails>()
            .add_direction(NetworkDirection::ServerToClient);
        // Carry I/O. Client requests; server is the source of truth.
        app.register_message::<PickupRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DropRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DropToolRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DepositRequest>()
            .add_direction(NetworkDirection::ClientToServer);
        // Phase 6b craft-order messages. Retired the Phase 6a
        // `CraftRequest` instant-craft path.
        app.register_message::<QueueOrder>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<CancelOrder>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<DepositToStation>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<WorkStart>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<WorkStop>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<StationUpdate>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<StationsFullSync>()
            .add_direction(NetworkDirection::ServerToClient);
        // Container stock mirror (storage arc S3): per-cell deltas +
        // connect-time full sync, same lane as stations.
        app.register_message::<crate::containers::ContainerUpdate>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<crate::containers::ContainersFullSync>()
            .add_direction(NetworkDirection::ServerToClient);

        // Player-avatar replication. Server owns the avatar entities; the
        // marker tells receivers "attach a mesh," and `AvatarPose` is the
        // per-tick state. We deliberately don't replicate `Transform` — the
        // 40-byte rotation+scale baggage isn't used.
        // See networking-design: state for entities, events for the grid.
        app.component::<Actor>().replicate();
        app.component::<Avatar>().replicate();
        app.component::<Npc>().replicate();
        app.component::<NpcId>().replicate();
        app.component::<crate::npc::NpcKind>().replicate();
        app.component::<NpcAnimOverride>().replicate();
        app.component::<NpcPath>().replicate();
        app.component::<crate::civilization::ClusterBboxReplica>().replicate();
        // Loose items in the world. No prediction (clients never drive
        // item motion) and no interpolation (movement is occasional
        // settle snaps, not continuous per-tick motion worth lerping).
        // The initial replicate carries the spawn position; later
        // server-side settles (fall onto exposed ground, rise out of a
        // newly-placed block) each replicate as one delta, which the
        // client's `sync_world_item_transform` copies to the render
        // Transform.
        app.component::<WorldItem>().replicate();
        // Actor carry stack. Replicated to every client so the owner
        // can read their own state for HUD; no prediction because
        // pickup/drop are discrete server-authoritative events, not
        // continuous-per-frame updates worth rolling back.
        app.component::<Carrying>().replicate();
        // Actor tool slot. Separate from carry — tools enable actions,
        // resources get hauled. Same no-prediction reasoning as
        // Carrying (pickup/swap are discrete server events).
        app.component::<EquippedTool>().replicate();
        // AvatarPose participates in both prediction (owner rolls back when
        // server disagrees) and interpolation (remote viewers lerp between
        // server samples instead of snapping every 50 ms).
        app.component::<AvatarPose>()
            .replicate()
            .predict()
            .add_linear_interpolation();
        // Velocity, ground state, and movement mode are simulation-only —
        // remote viewers don't need them, but the predicted owner does
        // (rollback restarts the controller from these values, so they
        // must be in the prediction history).
        app.component::<AvatarVelocity>().replicate().predict();
        app.component::<AvatarOnGround>().replicate().predict();
        app.component::<MovementMode>().replicate().predict();

        // Per-tick input replication. Adds `ActionState<MovementIntent>` and
        // the buffering machinery on both sides. Phase 2.4 hangs the
        // avatar entity off this; this registration alone is inert.
        app.add_plugins(input::native::InputPlugin::<MovementIntent>::default());
    }
}

fn start_netcode_server(mut commands: Commands, bind: Option<Res<ServerBindAddr>>) {
    use lightyear::prelude::server::{NetcodeConfig, NetcodeServer, ServerUdpIo};

    let addr = bind.map(|b| b.0).unwrap_or(DEFAULT_BIND_ADDR);
    let server = commands
        .spawn((
            NetcodeServer::new(NetcodeConfig {
                protocol_id: NETCODE_PROTOCOL_ID,
                ..default()
            }),
            LocalAddr(addr),
            ServerUdpIo::default(),
        ))
        .id();
    commands.trigger(Start { entity: server });
    info!("netcode server listening on {addr}");
}

/// Ask lightyear to close the session's connection when leaving InGame.
/// Idempotent: lightyear's disconnect observer ignores links that are
/// already `Disconnected` (e.g. after the mod-mismatch gate already
/// triggered one).
fn disconnect_client_on_exit(clients: Query<Entity, With<Client>>, mut commands: Commands) {
    for entity in clients.iter() {
        commands.trigger(Disconnect { entity });
    }
}

/// Remove dead connection entities while at the menu. Gated on
/// `Disconnected` rather than despawning at the state boundary so the
/// transport gets a frame to flush the disconnect packets first (only a
/// courtesy to remote servers — a hosted server is already gone by now).
fn despawn_disconnected_clients(
    clients: Query<Entity, (With<Client>, With<Disconnected>)>,
    mut commands: Commands,
) {
    for entity in clients.iter() {
        info!("despawning disconnected netcode client {entity:?}");
        commands.entity(entity).despawn();
    }
}

fn start_netcode_client(
    mut commands: Commands,
    target: Res<JoinTarget>,
    identity: Res<crate::identity::ClientIdentity>,
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

    // Persistent per-install id (see `identity.rs`): collision-proof
    // across machines AND stable across launches, which is what keys
    // per-player persistence on the server. A second instance in the
    // same directory gets an ephemeral id via the identity module's
    // file-lock fallback.
    let auth = Authentication::Manual {
        server_addr,
        client_id: identity.0,
        private_key: Key::default(),
        protocol_id: NETCODE_PROTOCOL_ID,
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
            ReplicationReceiver,
            // lightyear 0.28: prediction is only wired for links that carry
            // a PredictionManager (its on_insert registers the
            // PredictionResource the rollback systems unwrap — without it
            // the first predicted entity panics the app).
            PredictionManager::default(),
            client,
            UdpIo::default(),
        ))
        .id();
    commands.trigger(Connect { entity });
    info!("netcode client connecting to {server_addr}");
}
