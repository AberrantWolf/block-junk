---
name: lightyear-026
description: Project-local cheat sheet for lightyear 0.26.x networking patterns (server/client plugin groups, ProtocolPlugin pattern, replication, transports). Use whenever wiring or editing lightyear code in this repo. The library reshapes its API every minor version, so anything you remember from an older version is probably wrong.
user-invocable: false
---

# lightyear 0.26 — survival notes for this project

lightyear is the only crate in this project that churns its API faster than Bevy itself. The 0.26 line moved a *lot* of things: lifecycle is now component-driven (you spawn `Server`/`Client` entities and trigger observers), not resource-driven. Don't pattern-match against older docs.

When something in this file looks wrong against a build error, the cached source at `~/.cargo/registry/src/index.crates.io-*/lightyear*-0.26.*/` is authoritative. Update this file after fixing.

The canonical reference is the upstream `examples/` directory — when the source path is unclear, fetch a relevant example with `gh api repos/cBournhonesque/lightyear/contents/examples/<name>/src/<file>.rs --jq '.content' | base64 -d`.

## Cargo features

`default = ["std", "client", "server", "replication", "prediction", "interpolation"]` — gives you the abstractions but **no actual transport or connection layer**. You must opt into one or both:

| Use case | Add features |
|---|---|
| Friends-mode networked play (LAN/internet, UDP) | `netcode`, `udp` |
| Host mode in-process (server + client in one binary, talking via channels) | `crossbeam` |
| Web client | `webtransport` (or `websocket`) |
| Steam Networking Sockets | `steam` |

`netcode` is the connection layer (handshake, encryption, client IDs) that sits on top of an unreliable IO like UDP. Crossbeam is both the IO and the connection.

For block-junk we currently use `crossbeam` (host mode) and will add `netcode + udp` for friends-mode.

## Plugin registration order matters

```rust
use lightyear::prelude::client::ClientPlugins;
use lightyear::prelude::server::ServerPlugins;
use core::time::Duration;

let tick_duration = Duration::from_secs_f64(1.0 / 60.0);
app.add_plugins(DefaultPlugins);
app.add_plugins(ClientPlugins { tick_duration }); // and/or ServerPlugins
app.add_plugins(ServerPlugins { tick_duration });
app.add_plugins(MyProtocolPlugin);                 // AFTER lightyear plugins
app.add_plugins(MyGameplayPlugin);                 // AFTER protocol
```

**Adding both `ClientPlugins` and `ServerPlugins` to the same app is supported** — that's host mode. The shared sub-plugins use `is_unique: false` and dedupe internally.

## Protocol plugin: messages, channels, components

A `ProtocolPlugin` is just a Bevy `Plugin` that registers types. It must be added *after* the lightyear plugin groups.

```rust
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MyMessage(pub u32);

pub struct GameChannel;

impl Plugin for MyProtocolPlugin {
    fn build(&self, app: &mut App) {
        // Messages — discrete payloads sent over a channel.
        app.register_message::<MyMessage>()
            .add_direction(NetworkDirection::Bidirectional);
            // alternatives: ClientToServer, ServerToClient

        // Channels — define reliability/ordering. A message can be sent
        // on any channel that allows its direction.
        app.add_channel::<GameChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            ..default()
        })
        .add_direction(NetworkDirection::Bidirectional);
        // ChannelMode variants: UnorderedUnreliable, SequencedUnreliable,
        // OrderedReliable, UnorderedReliable, SequencedReliable.

        // Components — anything you want replicated must be registered.
        app.register_component::<MyComponent>();

        // If a Component or Message contains an Entity, it needs entity
        // mapping so remote-world entities resolve to local ones:
        // 1. impl bevy::ecs::entity::MapEntities for the type
        // 2. app.add_map_entities::<T>()
    }
}
```

## Server lifecycle (entity-driven, not resource-driven)

You **spawn a server entity** with the right components, then trigger `Start`:

```rust
use lightyear::prelude::server::*;
use lightyear::prelude::*;

fn startup(mut commands: Commands) {
    let server = commands
        .spawn((
            NetcodeServer::new(NetcodeConfig::default()),
            LocalAddr(SERVER_ADDR), // SocketAddr
            ServerUdpIo::default(), // requires `udp` feature
        ))
        .id();
    commands.trigger(Start { entity: server });
}
```

For host mode the components differ — see "Host mode (crossbeam)" below.

When a client link is established the server sees a new entity with `LinkOf` (transport-level link); when the handshake completes that entity gains `Connected`. Use observers to react:

```rust
fn handle_new_client(trigger: On<Add, LinkOf>, mut commands: Commands) {
    // Add ReplicationSender so we can replicate entities to this client.
    commands.entity(trigger.entity).insert(
        ReplicationSender::new(SEND_INTERVAL, SendUpdatesMode::SinceLastAck, false),
    );
}

fn handle_connected(
    trigger: On<Add, Connected>,
    query: Query<&RemoteId, With<ClientOf>>,
    mut commands: Commands,
) {
    let Ok(client_id) = query.get(trigger.entity) else { return; };
    // Spawn replicated game entities for this client here.
}

// Register them:
app.add_observer(handle_new_client);
app.add_observer(handle_connected);
```

## Client lifecycle

```rust
use lightyear::prelude::client::*;
use lightyear::prelude::*;

fn startup(mut commands: Commands) -> Result {
    let auth = Authentication::Manual {
        server_addr: SERVER_ADDR,
        client_id: 0,
        private_key: lightyear::netcode::Key::default(),
        protocol_id: 0,
    };
    let client = commands
        .spawn((
            Client::default(),
            LocalAddr(CLIENT_ADDR),
            PeerAddr(SERVER_ADDR),
            Link::new(None),
            ReplicationReceiver::default(),
            NetcodeClient::new(auth, NetcodeConfig::default())?,
            UdpIo::default(),
        ))
        .id();
    commands.trigger(Connect { entity: client });
    Ok(())
}
```

`protocol_id` and `private_key` must match between client and server. For local dev both default to zero / zeros. For shipped builds set them yourself.

## Host mode (crossbeam)

When server + client run in the same process you can short-circuit UDP via crossbeam. Replace the IO components:

- Server entity: spawn without `ServerUdpIo`; instead pair it to a crossbeam IO setup (see `lightyear_crossbeam`).
- Client entity: replace `UdpIo` with the crossbeam counterpart, and either remove the netcode layer or use a crossbeam-compatible connection.

The `crossbeam` feature must be enabled. Check `lightyear-0.26.4/src/shared.rs` for `#[cfg(feature = "crossbeam")]` to see what plugins land automatically — the crossbeam IO plugin is added to `SharedPlugins` when the feature is on.

## Replication

Add `Replicate` to entities you want the server to push to clients:

```rust
use lightyear::prelude::*; // Replicate is in the top-level prelude
                            // (NOT lightyear::prelude::server, despite the name)

commands.spawn((
    MyComponent { ... },
    Replicate::to_clients(NetworkTarget::All),
    // optional: prediction (rollback) for the owner
    PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
    // optional: interpolation for everyone else
    InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
    // optional: which client "owns" this entity for input
    ControlledBy { owner: link_entity, lifetime: Default::default() },
));
```

Components on the entity are only replicated if you `app.register_component::<T>()` in the protocol plugin.

`NetworkTarget` variants: `All`, `None`, `Single(client_id)`, `AllExceptSingle(client_id)`, `Only(Vec<client_id>)`, `AllExcept(Vec<client_id>)`.

## Path-resolution traps (verified in 0.26.4)

The prelude is split into a top-level part and `client`/`server` submodules. Items aren't always where the name suggests:

| Item | Lives in | Notes |
|---|---|---|
| `Server`, `LinkOf`, `Link`, `Linked` | top-level prelude | shared |
| `Client`, `Connect`, `Connected`, `Disconnect` | top-level prelude | shared (despite the names) |
| `Start`, `Started`, `Stop`, `Stopped` | `prelude::server` only | server lifecycle triggers/markers |
| `LocalAddr`, `PeerAddr` | top-level prelude | from `aeronet_io` |
| `Authentication` | top-level prelude | from `lightyear_netcode`, *not* `prelude::client` |
| `UdpIo` | top-level prelude | from `lightyear_udp` |
| `ServerUdpIo` | `prelude::server` | server-side UDP IO |
| `NetcodeServer` | `prelude::server` | |
| `NetcodeClient` | `prelude::client` | |
| **`NetcodeConfig`** | **both `prelude::client` AND `prelude::server`** — different types! | Must qualify or scope-import. Glob-importing both = E0308 type mismatch. |
| `ServerMultiMessageSender` | top-level prelude | despite the `Server` prefix it's not in `prelude::server` |
| `MessageSender<T>`, `MessageReceiver<T>` | top-level prelude | components on connection entities |
| `Replicate` | top-level prelude | not `prelude::server` |
| `NetworkTarget` | top-level prelude | |
| `NetworkDirection` | top-level prelude | |

**Rule of thumb**: try the top-level prelude first. If something resolves but doesn't compile (or doesn't exist), check `prelude::server` for server-only lifecycle types and `prelude::client` for `Authentication` exceptions and the netcode client config.

When `NetcodeConfig` ambiguity bites, use scoped function-local imports: `use lightyear::prelude::server::NetcodeConfig;` inside `start_netcode_server`, etc.

## Prediction + interpolation + input replication (the simple_box pattern)

This is the canonical shape for "owner predicts, others interpolate, server is authority." Source: upstream `examples/simple_box`. Verify against that example if anything below stops compiling.

### 1. Define an input type

```rust
use bevy::ecs::entity::MapEntities;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq, Clone, Reflect)]
pub struct MyInput { up: bool, down: bool, jump: bool /* ... */ }

impl MapEntities for MyInput {
    fn map_entities<M: EntityMapper>(&mut self, _: &mut M) {}
}
```

Required derives: `Serialize, Deserialize, Clone, PartialEq, Reflect, Debug, Default, MapEntities`. The `Default` value MUST mean "no input held" — the buffer distinguishes that from "missing packet."

### 2. Define a replicated state component

Keep it minimal. **Don't** replicate `Transform` if you only need a 2D position — its 40-byte rotation+scale baggage is dead weight. Implement `Ease` for components that interpolate:

```rust
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq, Reflect)]
pub struct PlayerPosition(pub Vec3);

impl Ease for PlayerPosition {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        FunctionCurve::new(Interval::UNIT, move |t| {
            PlayerPosition(Vec3::lerp(start.0, end.0, t))
        })
    }
}
```

Components that drive prediction-only state (velocity, `on_ground`) can register without `Ease` — register them with `.add_prediction()` only, no `.add_linear_interpolation()`.

### 3. Protocol registration

```rust
use lightyear::prelude::*;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        // input plugin (REQUIRES the `input_native` cargo feature on lightyear)
        app.add_plugins(input::native::InputPlugin::<MyInput>::default());

        // components — chained methods register prediction/interpolation
        app.register_component::<PlayerPosition>()
            .add_prediction()
            .add_linear_interpolation();

        // velocity — predicted but not interpolated (remote viewers don't need it)
        app.register_component::<Velocity>().add_prediction();
    }
}
```

### 4. Server: spawn the controlled entity

In the `Connected` observer:

```rust
use lightyear::prelude::server::*;

let avatar = commands.spawn((
    PlayerPosition(Vec3::ZERO),
    Replicate::to_clients(NetworkTarget::All),
    PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
    InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
    ControlledBy { owner: link_entity, lifetime: Default::default() },
)).id();
```

`PredictionTarget` puts `Predicted` on the owner's copy; `InterpolationTarget` puts `Interpolated` on every other client's copy.

### 5. Server: consume inputs in FixedUpdate

```rust
use lightyear::prelude::input::native::*;

fn server_movement(
    mut q: Query<
        (&mut PlayerPosition, &ActionState<MyInput>),
        Without<Predicted>, // host-mode safety: skip the local-client copy
    >,
) {
    for (pos, input) in q.iter_mut() {
        shared_movement(pos, &input.0);
    }
}

app.add_systems(FixedUpdate, server_movement);
```

The `ActionState<MyInput>` component appears automatically on the server's avatar entity once `InputPlugin` is registered.

### 6. Client: write inputs in FixedPreUpdate

```rust
use lightyear::prelude::client::input::*;
use lightyear::prelude::input::native::*;

fn buffer_input(
    mut q: Query<&mut ActionState<MyInput>, With<InputMarker<MyInput>>>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    let Ok(mut state) = q.single_mut() else { return };
    state.0 = MyInput { /* read keys */ };
}

app.add_systems(
    FixedPreUpdate,
    buffer_input.in_set(InputSystems::WriteClientInputs),
);
```

`InputMarker<MyInput>` is what tells the input plugin "this client is the one writing inputs to this entity." Add it in the `Add<Predicted>` observer below.

### 7. Client: predict in FixedUpdate

```rust
fn client_movement(
    mut q: Query<(&mut PlayerPosition, &ActionState<MyInput>), With<Predicted>>,
) {
    for (pos, input) in q.iter_mut() {
        shared_movement(pos, &input.0);
    }
}
```

Same function the server calls. Identical inputs → identical outputs ⇒ no rollback. When server disagrees, lightyear rolls the predicted entity back and replays.

### 8. Client: tag the predicted entity as input source

```rust
fn handle_predicted_spawn(
    trigger: On<Add, Predicted>,
    mut commands: Commands,
) {
    commands.entity(trigger.entity).insert(InputMarker::<MyInput>::default());
}

app.add_observer(handle_predicted_spawn);
```

### 9. Cargo features

`input_native` is **not** in `default`. Add it explicitly: `lightyear = { version = "0.26", features = ["netcode", "udp", "input_native"] }`.

For frame-smooth render of the owner camera (60 Hz physics → variable render rate), also enable `frame_interpolation` and use `lightyear_frame_interpolation` to lerp Transform between FixedUpdate ticks.

## Common gotchas

- **`ProtocolPlugin` registered before `ClientPlugins`/`ServerPlugins`**: silent breakage. Required order.
- **Replicate without registering the components**: replication runs but the components don't appear on the client. Always `register_component` in protocol plugin.
- **Entity references in messages without `MapEntities`**: deserializes as the wrong entity on the receiving side. Implement `MapEntities` and call `add_map_entities::<T>()`.
- **`#[derive(Component)]` on a replicated type but no `Serialize/Deserialize`**: register_component fails to compile. All replicated types need both.
- **Writing input handling on `Update` instead of `FixedUpdate`**: prediction needs deterministic input timing. Move input → state changes into `FixedUpdate`.
- **Server in `host` mode adds two `Connected` observers (one local, one for real clients)**: be aware the same observer fires for the local client too.

## Where to look when stuck

1. **Build errors**: cached source path above.
2. **Runtime confusion**: the upstream `examples/` directory. Useful ones:
   - `simple_setup` — minimum viable client+server wiring
   - `simple_box` — replicated components + per-client predicted entities
   - `client_replication` — client-authoritative replication
   - `replication_groups` — grouping entities for atomic update
3. **Behavioral docs**: <https://cbournhonesque.github.io/lightyear/book/>, but verify against the version in your Cargo.lock.
