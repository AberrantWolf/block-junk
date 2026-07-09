---
name: lightyear-028
description: Project-local cheat sheet for lightyear 0.28.x networking patterns (server/client plugin groups, ProtocolPlugin pattern, replication on the bevy_replicon backend, prediction, transports). Use whenever wiring or editing lightyear code in this repo. The library reshapes its API every minor version, so anything you remember from an older version is probably wrong.
user-invocable: false
---

# lightyear 0.28 — survival notes for this project

lightyear churns its API faster than Bevy itself. The big 0.27/0.28 move: **the replication backend is now `bevy_replicon`** (lightyear keeps its higher-level prediction/interpolation/visibility APIs on top). Most of our 0.26-era code survived; the things that didn't are called out below. Tick type went u16 → u32.

When something in this file looks wrong against a build error, the cached source at `~/.cargo/registry/src/index.crates.io-*/lightyear*-0.28.*/` (and `bevy_replicon-0.41.*`) is authoritative. Update this file after fixing.

The canonical reference is the upstream `examples/` directory — fetch with:
```sh
gh api 'repos/cBournhonesque/lightyear/contents/examples/<name>/src/<file>.rs?ref=0.28.0' --jq '.content' | base64 -d
```
(`examples/common/src/client.rs` + `server.rs` hold the canonical connection-entity spawns.)

## What changed 0.26 → 0.28 (the short list)

| 0.26 | 0.28 |
|---|---|
| `ReplicationSender::new(interval, SendUpdatesMode::…, bool)` | `ReplicationSender` **unit marker**; `SendUpdatesMode` is gone; send interval = app-wide `ReplicationMetadata::new(interval)` **resource** (+ per-channel `ChannelSettings.send_frequency`) |
| `ReplicationReceiver::default()` (config struct) | `ReplicationReceiver` unit marker (same spawn spot) |
| prediction auto-wired on the client | **client entity must carry `PredictionManager::default()`** — see trap below |
| `app.register_component::<C>()` `.add_prediction()` | deprecated → `app.component::<C>().replicate()` `.predict()` (`.add_linear_interpolation()` unchanged) |
| `Predicted { confirmed_entity }`, `Interpolated { confirmed_entity }` | both are **unit structs** now; the fields are gone |
| `Confirmed` marker | **removed**. Receive-side is replicon's `client::Remote` (+ `ConfirmHistory`); `Replicated` is a deprecated alias for `Remote` |
| `*Set` system sets (`InputSet`, `ConnectionSet`, …) | renamed `*Systems` (old names deprecated aliases) |
| `Tick(u16)` | `Tick(u32)` — wraparound worries gone |
| `#[derive(Message)]` proc-macro exported (collided with bevy's) | **no derive exported** — bevy's `#[derive(Message)]` works under both preludes now (see bevy-019 skill) |

**Runtime trap (cost us a panic, 2026-07-02): a client connection entity without `PredictionManager` panics the app** the moment the first `PredictionTarget` entity replicates in: `PredictionManager::on_insert` is what registers the `PredictionResource` that `lightyear_prediction`'s receive path unwraps (`registry.rs` ~1215). Compiles fine without it. Always include `PredictionManager::default()` in the client spawn (network.rs does).

## Cargo features

`default = ["std", "client", "server", "replication", "prediction", "interpolation"]` — abstractions but **no transport/connection layer**. Opt into:

| Use case | Add features |
|---|---|
| Networked play (LAN/internet, UDP) | `netcode`, `udp` |
| Web client | `webtransport` (or `websocket`) |
| Steam | `steam` |
| Per-tick input replication (native) | `input_native` |
| Render-rate smoothing of fixed-tick state | `frame_interpolation` |

block-junk uses `netcode + udp + input_native + frame_interpolation`. Solo play runs a server thread + client App over UDP-localhost (always-client architecture) — no `crossbeam`.

## Plugin registration order matters (unchanged)

```rust
use lightyear::prelude::client::ClientPlugins;
use lightyear::prelude::server::ServerPlugins;

let tick_duration = Duration::from_secs_f64(1.0 / 60.0);
app.add_plugins(DefaultPlugins);
app.add_plugins(ClientPlugins { tick_duration }); // and/or ServerPlugins
app.add_plugins(MyProtocolPlugin);                 // AFTER lightyear plugins
app.add_plugins(MyGameplayPlugin);                 // AFTER protocol
```

Adding both `ClientPlugins` and `ServerPlugins` to one app is still supported (host mode); the shared sub-plugins dedupe (`is_unique: false`).

## Protocol plugin: messages, channels, components

Messages and channels are unchanged from 0.26; component registration is the new fluent API:

```rust
use lightyear::prelude::*;

impl Plugin for MyProtocolPlugin {
    fn build(&self, app: &mut App) {
        // Messages — unchanged.
        app.register_message::<MyMessage>()
            .add_direction(NetworkDirection::Bidirectional);
        // (entity refs inside? impl MapEntities + .add_map_entities())

        // Channels — unchanged. ChannelSettings { mode, send_frequency, priority }.
        app.add_channel::<GameChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);

        // Components — NEW fluent API (old register_component is a
        // deprecated shim over this):
        app.component::<MyComponent>().replicate();
        app.component::<AvatarPose>()
            .replicate()
            .predict()                     // was .add_prediction()
            .add_linear_interpolation();   // unchanged
        app.component::<AvatarVelocity>().replicate().predict();

        // Input plugin (requires `input_native` feature) — unchanged.
        app.add_plugins(input::native::InputPlugin::<MovementIntent>::default());
    }
}
```

Other registration verbs on `ComponentRegistration`: `.replicate_once()`, `.replicate_diff()` (delta compression), `.replicate_with_priority(n)`. Prediction chain: `.predict().with_rollback_condition(f)`. There's also `app.resource::<R>()` — resources now replicate like components, no special casing.

Networked entity-events (new): `app.register_event::<E>()` + `.add_direction(..)`, send via `EventSender<E>::trigger::<Channel>(event)` component, receive with an `On<RemoteEvent<E>>` observer.

## Server lifecycle (entity-driven; unchanged shape)

```rust
use lightyear::prelude::server::*;
use lightyear::prelude::*;

fn startup(mut commands: Commands) {
    let server = commands
        .spawn((
            NetcodeServer::new(NetcodeConfig::default()), // server-side config type
            LocalAddr(SERVER_ADDR),
            ServerUdpIo::default(),
        ))
        .id();
    commands.trigger(Start { entity: server });
}
```

Connection observers (unchanged shapes; `ReplicationSender` is now bare):

```rust
fn handle_new_client(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(ReplicationSender);
}

fn handle_connected(
    trigger: On<Add, Connected>,
    query: Query<&RemoteId, With<ClientOf>>,
    mut commands: Commands,
) { /* spawn avatar for this client */ }
```

**Send interval**: `app.insert_resource(ReplicationMetadata::new(REPLICATION_INTERVAL))` after the lightyear plugins (they insert a Default = send every tick). server.rs::ServerPlugin does this — 50 ms / 20 Hz per networking-design.

**Headless server app note**: Bevy 0.19 requires `StatesPlugin` explicitly with `MinimalPlugins` (see bevy-019 skill) — the dedicated server panicked without it.

## Client lifecycle

```rust
use lightyear::prelude::client::*;
use lightyear::prelude::*;

fn startup(mut commands: Commands) -> Result {
    let auth = Authentication::Manual {
        server_addr: SERVER_ADDR,
        client_id,               // stable per-install identity
        private_key: Key::default(),
        protocol_id: NETCODE_PROTOCOL_ID,
    };
    let client = commands
        .spawn((
            Client::default(),
            LocalAddr(CLIENT_ADDR),
            PeerAddr(SERVER_ADDR),
            Link::new(None),
            ReplicationReceiver::default(),
            PredictionManager::default(), // REQUIRED for prediction — see trap above
            NetcodeClient::new(auth, NetcodeConfig::default())?, // client-side config type
            UdpIo::default(),
        ))
        .id();
    commands.trigger(Connect { entity: client });
    Ok(())
}
```

`protocol_id` and `private_key` must match the server's `NetcodeConfig` or the handshake is rejected before any app code runs — block-junk keys both to `network::NETCODE_PROTOCOL_ID` as a cheap wire-version gate.

## Replication

Send side unchanged from 0.26:

```rust
use lightyear::prelude::*; // Replicate/targets are top-level prelude

commands.spawn((
    MyComponent { .. },
    Replicate::to_clients(NetworkTarget::All),
    PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
    InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
    ControlledBy { owner: link_entity, lifetime: Lifetime::Persistent },
));
```

- `Replicate` = `ReplicationTarget<()>`; remove it to pause replication; remove **before** despawning if the despawn shouldn't replicate.
- `NetworkTarget` variants unchanged: `All`, `None`, `Single`, `AllExceptSingle`, `Only(Vec)`, `AllExcept(Vec)` — over `PeerId` now.
- `ControlledBy` is a proper Bevy relationship now (`ControlledByRemote` on the owner side), requires `Controlled`.
- `Lifetime` variants: `SessionBased` (default) | `Persistent` (`Once` is gone).
- Receive side: owner's copy gets `Predicted` (unit), other clients get `Interpolated` (unit), plain replicated entities get `client::Remote`. There is **no `Confirmed` entity/marker** anymore — for confirmed-tick bookkeeping use `ConfirmHistory`.
- Rooms (if we ever need interest management beyond AoI): `RoomPlugin`, `Rooms` component on the replicated entity (`Rooms::single(id)`), ids from the `RoomAllocator` resource.

## Prediction + interpolation + input replication (simple_box pattern)

Same shape as 0.26 end to end — input type (`Serialize+Deserialize+Clone+PartialEq+Reflect+Debug+Default+MapEntities`, `Default` = "no input held"), minimal replicated state component with `Ease` for interpolated types, shared movement fn on server (`Without<Predicted>` for host-mode safety) and client (`With<Predicted>`) in `FixedUpdate`, client writes `ActionState<I>` in `FixedPreUpdate`:

```rust
app.add_systems(
    FixedPreUpdate,
    buffer_input.in_set(lightyear::prelude::input::client::InputSystems::WriteClientInputs),
);

fn handle_predicted_spawn(trigger: On<Add, Predicted>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(InputMarker::<MovementIntent>::default());
}
```

0.28 additions to know:
- **Server bounds accepted input ticks** to a window around its tick automatically (anti-DoS). Wildly out-of-range inputs are dropped silently.
- **`ValidateInputs` seam (opt-in)**: `app.add_input_validator(system)` (`input::server::InputValidationAppExt`) runs in `PreUpdate/InputSystems::ValidateInputs`; ready-made `authorize_controlled_targets::<S>` strips input targets the sender doesn't own. Worth wiring when we harden multiplayer.
- Input buffering only starts after timeline sync — early inputs during connect are dropped by design.
- `InputConfig` fields: `packet_redundancy` (default 5), `send_interval`, `rebroadcast_inputs`, `lag_compensation`.

Frame-smooth rendering of fixed-tick state: `frame_interpolation` feature; **NOT in the main prelude** — `use lightyear::frame_interpolation::prelude::*;` (`FrameInterpolationPlugin::<C>`, `FrameInterpolate<C>`, `FrameInterpolationSystems`, plus `SkipFrameInterpolation` for teleports). Needs an interpolation fn registered (`.add_linear_interpolation()`).

## Path-resolution traps (verified in 0.28.0)

| Item | Lives in | Notes |
|---|---|---|
| `Server`, `LinkOf`, `Link`, `Linked` | top-level prelude | `LinkOf { server }`/`Server { links }` are a relationship pair now |
| `Client`, `Connect`, `Connected`, `Disconnect` | top-level prelude | shared despite the names |
| `Start`, `Started`, `Stop`, `Stopped` | `prelude::server` only | `Start { entity }` field is `entity` |
| `LocalAddr`, `PeerAddr` | top-level prelude | from `aeronet_io` |
| `Authentication` | top-level prelude | from `lightyear_netcode` |
| `UdpIo` | top-level prelude | `ServerUdpIo` in `prelude::server` |
| `NetcodeServer` / `NetcodeClient` | `prelude::server` / `prelude::client` | |
| **`NetcodeConfig`** | **both `prelude::client` AND `prelude::server` — different types!** | server one has `protocol_id`/`private_key`; client one has `token_expire_secs`. Scope-import inside the fn. |
| `Replicate`, `NetworkTarget`, `NetworkDirection` | top-level prelude | |
| `PredictionManager`, `PredictionTarget`, `InterpolationTarget` | top-level prelude | |
| `Remote` (receive-side marker) | `prelude::client` | top-level `Replicated` is a deprecated alias |
| `ReplicationMetadata`, `ReplicationSender`, `ReplicationReceiver`, `ConfirmHistory` | top-level prelude | |
| `MessageSender<M>`, `MessageReceiver<M>`, `ServerMultiMessageSender` | top-level prelude | |
| `EventSender<E>`, `AppTriggerExt`, `RemoteEvent` | top-level prelude | networked entity-events |
| `input::native::{InputPlugin, ActionState, InputMarker}` | `lightyear::prelude::input::native` | client/server `InputSystems` under `input::client` / `input::server` |
| frame interpolation | `lightyear::frame_interpolation::prelude` | NOT in main prelude |

**Rule of thumb**: top-level prelude first; `prelude::server` for server lifecycle + `ServerUdpIo`/`NetcodeServer`; `prelude::client` for `NetcodeClient`, client `NetcodeConfig`, and `Remote`.

**Mixing broadcast + targeted replies in one system is still safe.** `ServerMultiMessageSender` is a SystemParam over `Query<(&mut MessageManager, &mut Transport)>` — it does NOT touch `MessageSender<M>` components, so one system can take both without a B0001 conflict. Send API: `sender.send::<M, C>(&msg, &server, &NetworkTarget)`. Used by the reach-gate handlers.

## Common gotchas

- **`ProtocolPlugin` registered before `ClientPlugins`/`ServerPlugins`**: silent breakage. Required order.
- **Client entity without `PredictionManager`**: runtime panic on first predicted entity (see top). The matching server-side pattern (bare `ReplicationSender` on `LinkOf`) is fine.
- **`Replicate` without registering the components**: replication runs but components don't appear client-side. Register in the protocol plugin.
- **Entity references in messages without `MapEntities`**: deserializes as the wrong entity. `impl MapEntities` + `.add_map_entities()`.
- **Input handling on `Update` instead of `FixedUpdate`**: prediction needs deterministic input timing.
- **Host mode fires `Connected` for the local client too** — observers run for both local and remote clients.
- **`ControlledBy { lifetime: Lifetime::SessionBased }` (default) despawns controlled entities BEFORE `On<Remove, ClientOf>` observers run**: a disconnect observer that persists the departing player's state reads nothing — entity's gone. Use `Lifetime::Persistent` + manual despawn. block-junk's `register_new_client`/`forget_disconnected_client` does this; re-verified working on 0.28 (2026-07-02).
- **Netcode client id**: `Query<&RemoteId>` on the connection entity; match `PeerId::Netcode(u64)`. Still readable inside `On<Remove, ClientOf>`.
- **Programmatic disconnect**: `commands.trigger(Disconnect { entity })` on the `Client` entity. block-junk uses this for the mod-set-mismatch gate and for quit-to-menu (`network.rs::disconnect_client_on_exit`).
- **Disconnect lifecycle (verified in 0.28 source + at runtime 2026-07-08)**: `Connecting`/`Connected`/`Disconnected` state markers live in the top-level prelude. lightyear_netcode's `Disconnect` observer inserts `Disconnected` immediately (and is a no-op on already-`Disconnected` links, so re-triggering is safe); `NetcodeClient` is `#[require(Disconnected)]`, i.e. a fresh client entity starts `Disconnected` until `Connect`. Clean session-close pattern: trigger `Disconnect` at the state boundary (`OnExit`), then despawn the `Client` entity one frame later gated on `With<Disconnected>` — that frame's PostUpdate flush gets the disconnect packets out. Despawn the old client entity (don't reuse it) before reconnecting; `CLIENT_ADDR` binds port 0 (ephemeral), so despawn/respawn has no rebind hazard.
- **Deprecated aliases compile silently** (`register_component`, `add_prediction`, `InputSet`, `Replicated`, …) — treat new deprecation warnings after a bump as the migration checklist.

## Where to look when stuck

1. **Build errors**: cached source path above.
2. **Runtime confusion**: upstream `examples/` at ref `0.28.0` — `simple_box` (prediction shape), `examples/common/src/{client,server}.rs` (canonical connection-entity spawns).
3. **Behavioral docs**: <https://cbournhonesque.github.io/lightyear/book/>, but verify against Cargo.lock's version.
