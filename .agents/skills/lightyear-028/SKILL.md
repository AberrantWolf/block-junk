---
name: lightyear-028
description: Project-local lightyear 0.28 networking guide for block-junk. Use whenever writing, editing, or reviewing protocol registration, UDP lifecycle, messages, replication, prediction, interpolation, authority, or area-of-interest behavior in this repository. Older lightyear APIs are not reliable references.
user-invocable: false
---

# lightyear 0.28 in block-junk

This workspace pins lightyear 0.28 with `netcode`, `udp`, `input_native`, and
`frame_interpolation`. Use cached 0.28 sources and the current `network.rs` setup as
the source of truth.

```sh
rg "pub (struct|enum|fn)|pub use" ~/.cargo/registry/src/index.crates.io-*/lightyear*-0.28.0/src
```

## Architecture is fixed

- Every player is a network client.
- Solo play starts a headless server `App` on a worker thread and connects the
  normal client over UDP localhost.
- Dedicated and hosted servers use the same netcode/UDP path.
- Do not introduce crossbeam host mode or a shared-world shortcut.
- Never put a Bevy `Entity` in a cross-world contract unless the type implements
  entity mapping. Prefer stable IDs and chunk coordinates.

Read `../networking-design/SKILL.md` for what should cross the boundary, channel
semantics, bandwidth limits, and AoI policy.

## Plugin and lifecycle order

Add the appropriate lightyear plugin group before `NetworkPlugin`, which registers
the protocol:

```rust
app.add_plugins(lightyear::prelude::client::ClientPlugins { tick_duration });
app.add_plugins(NetworkPlugin { mode: NetMode::Client });
```

The server entity uses `NetcodeServer`, `LocalAddr`, and `ServerUdpIo`, then receives
the `Start` trigger. The client entity uses `Client`, `LocalAddr`, `PeerAddr`,
`Link`, `ReplicationReceiver`, `PredictionManager`, `NetcodeClient`, and `UdpIo`,
then receives `Connect`.

`PredictionManager` is required on the client link before predicted entities
arrive; prediction resources are initialized from it.

Connection lifecycle is component-driven. Transport connection is not game readiness:

- `On<Add, Connected>` starts content/identity validation and sends only the
  compatibility challenge.
- A project-owned `GameReady` marker is added only after the client acknowledges
  the expected manifest/challenge. Avatar creation, replication, AoI, and full
  sync observers key off `GameReady`, not raw `Connected`.
- `On<Remove, Connected>` banks player state and releases claims/bookings.
- Trigger `Disconnect` before despawning a client link; wait for `Disconnected` so
  transport packets can flush.

`NetcodeConfig` exists in both client and server preludes. Import it inside the
setup function from the correct side to avoid type ambiguity.

## Channels and messages

```rust
app.add_channel::<GameChannel>(ChannelSettings {
    mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
    ..default()
})
.add_direction(NetworkDirection::Bidirectional);

app.register_message::<GameMessage>()
    .add_direction(NetworkDirection::ClientToServer);
```

Current channel contracts:

- `WorldChannel`: ordered reliable, latency-sensitive requests and replies.
- `ChunkChannel`: ordered reliable snapshots, unloads, and block-edit deltas.
- `StateSyncChannel`: ordered reliable full-sync-plus-delta state.
- `PeriodicSyncChannel`: sequenced unreliable samples where the newest wins.

Cross-channel ordering does not exist. Full sync and dependent deltas must share a
channel. `ChunkSnapshot`, `ChunkUnload`, and applied `BlockEdit` intentionally share
`ChunkChannel`.

Treat all client-to-server payloads as untrusted. Deserialize into wire types, then
validate identity, permissions, finite/range bounds, registry references, reach,
and current world preconditions before mutating authoritative state. Wire handlers
must not index registries with unchecked numeric slots.

## Replicated components

lightyear 0.28 registers component behavior through the Bevy app component builder:

```rust
app.component::<AvatarPose>()
    .replicate()
    .predict()
    .add_linear_interpolation();

app.component::<AvatarVelocity>().replicate().predict();
```

Do not use old `register_component().add_prediction()` examples.

The server adds `Replicate::to_clients(target)` to replicated entities. Owner
prediction and remote interpolation use `PredictionTarget` and
`InterpolationTarget`; `ControlledBy` ties input ownership to the connection
entity.

Each non-host connection link still needs the marker component:

```rust
commands.entity(connection).insert(ReplicationSender);
```

The replication send interval is app-wide in 0.28:

```rust
app.insert_resource(ReplicationMetadata::new(REPLICATION_INTERVAL));
```

Do not construct the old stateful/per-connection sender configuration from 0.26
examples. Insert the 0.28 unit marker only after the connection reaches
`GameReady`; this prevents replicated state from racing content validation.

Use `NetworkTarget` deliberately. `NetworkTarget::All` is correct only for truly
global data. Spatial world entities and deltas must target the connections whose
AoI contains them.

## Input prediction and rendering

Register native input once in the protocol plugin:

```rust
app.add_plugins(input::native::InputPlugin::<MovementIntent>::default());
```

On the client, attach `ActionState<MovementIntent>` and
`InputMarker<MovementIntent>` to the predicted owner entity. Write input in
`FixedPreUpdate` under `client::input::InputSystems::WriteClientInputs`. Run the
same deterministic movement function on the authoritative server entity and the
client's `Predicted` entity in `FixedUpdate`.

For render-frame smoothing, enable `frame_interpolation`, add
`FrameInterpolationPlugin::<T>`, attach `FrameInterpolate<T>` to the rendered
predicted entity, and copy interpolated state to `Transform` after
`FrameInterpolationSystems::Interpolate` in `PostUpdate`.

## Sending patterns

- A client sends through the `MessageSender<M>` on its connection entity.
- A server sends a targeted response through `MessageSender<M>` queried by the
  requesting connection entity.
- `ServerMultiMessageSender` handles broadcast/`NetworkTarget` sends and can coexist
  in a system with targeted `MessageSender<M>` queries.
- Collect receiver iterators before taking other conflicting mutable borrows when
  necessary; incoming messages may be processed more than once per frame.

Messages should carry requests or facts, not client-declared completed outcomes.
For timed work, the server owns progress and emits the resulting authoritative
world delta only after validation and completion.

## Protocol compatibility

`NETCODE_PROTOCOL_ID` gates incompatible wire builds at handshake time. Bump it for
message shape, direction, channel, or replication registration changes. The
`ModSetManifest` is a separate post-connect content compatibility gate; both are
required.

Protocol registration belongs in one shared `ProtocolPlugin` used by client and
server. Never duplicate the registration lists on each side.

## Verification

For every networking change, add tests at both levels:

- Pure validation tests for hostile/malformed payloads and current-world
  preconditions.
- A two-`App` loopback integration test for ordering, connect/disconnect, authority,
  and AoI delivery.

Then run:

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

If an API assumption fails, inspect the exact lightyear 0.28 cached source and
upstream 0.28 examples, then update this skill with the verified pattern.
