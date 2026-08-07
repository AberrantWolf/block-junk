---
name: bevy-019
description: Project-local Bevy 0.19 API and architecture guide for block-junk. Use whenever writing, editing, or reviewing Bevy code in this repository; older Bevy examples are routinely incompatible. Covers schedules, messages and observers, required components, rendering, animation, UI, and the repo's Bevy integration traps.
user-invocable: false
---

# Bevy 0.19 in block-junk

This workspace pins Bevy 0.19. Treat cached 0.19 source and the compiling code in
`crates/block-junk` as authoritative. Do not copy 0.18-or-earlier examples without
checking them.

Cached sources:

```sh
rg "pub (struct|enum|fn)|pub use" ~/.cargo/registry/src/index.crates.io-*/bevy*-0.19.0/src
```

## Repository invariants

- The client uses `DefaultPlugins`; the headless server uses `MinimalPlugins`,
  `TransformPlugin`, and `StatesPlugin`.
- Gameplay systems belong to `GameSet::{Input, Simulation, PostSimulation}`.
  Client gameplay is state-gated on `AppState::InGame`.
- The client and server are separate Bevy `App`s even in solo play. Never share
  resources or entities across them.
- Session-local entities use `DespawnOnExit(AppState::InGame)`. Plugin-private
  indexes/resources need plugin-owned `OnExit` reset systems as well.
- Prefer the existing plugin and `SystemParam` boundaries. Bevy system and tuple
  trait limits still produce misleading `IntoSystem`/`.in_set` errors when a
  signature grows too large.

## Buffered messages versus observers

Buffered in-process events use Bevy messages:

```rust
#[derive(Message)]
struct CellChanged(IVec3);

app.add_message::<CellChanged>();

fn send(mut writer: MessageWriter<CellChanged>) {
    writer.write(CellChanged(IVec3::ZERO));
}
```

Entity lifecycle reactions use observers and `On<Add, T>` / `On<Remove, T>`.

Lightyear also exports a network `Message` derive. In modules that glob-import
`lightyear::prelude::*`, avoid ambiguous local derives. Existing server-local
messages such as `CellEdit` demonstrate the working import pattern. For simple UI
spawn queues, a `Resource<Vec<T>>` such as `PendingToasts` is also appropriate.

## Components and queries

Bundles such as `PbrBundle` and `Camera3dBundle` are gone. Spawn components and let
required components fill in defaults:

```rust
commands.spawn((
    Mesh3d(mesh),
    MeshMaterial3d(material),
    Transform::from_xyz(0.0, 1.0, 0.0),
));
```

`Query::single()` and `single_mut()` return `Result`; shipped systems should use
`let Ok(value) = ... else { return; }`, not unwrap.

`CursorOptions` is a component on the window entity. Query it with
`With<PrimaryWindow>`; it is not a `Window` field.

Mouse and keyboard plugins are feature-gated. This repo enables both explicitly:

```toml
bevy = { version = "0.19", features = ["mouse", "keyboard"] }
```

Prefer `AccumulatedMouseMotion` and `AccumulatedMouseScroll` resources to reading
raw mouse messages.

## Time, transforms, and schedules

- Use `Time::delta_secs()` and `elapsed_secs()`.
- Run deterministic movement and prediction in `FixedUpdate`; gather replicated
  client input in `FixedPreUpdate`.
- Rendering synchronization may belong in `PostUpdate`, after lightyear frame
  interpolation. See `client.rs::sync_avatar_transforms`.
- Commands are deferred. If system B must query entities spawned by system A in
  the same schedule, chain the systems or stage the data outside ECS and apply it
  after reconciliation. Inserting an entity id into a resource does not make the
  entity queryable before deferred commands flush.

## Meshes and materials

```rust
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};

let mut mesh = Mesh::new(
    PrimitiveTopology::TriangleList,
    RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
);
mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
mesh.insert_indices(Indices::U32(indices));
```

Custom material shaders must use Bevy's injected bind-group placeholder:

```wgsl
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: MyParams;
```

Do not hardcode group 2 or 3. Bevy uses reversed-Z depth; behind-geometry effects
need the opposite comparison from conventional depth buffers. Existing dither and
crosshatch materials are the local references.

In Bevy 0.19:

- Ambient light is the `GlobalAmbientLight` resource.
- `DirectionalLight` uses `shadow_maps_enabled`.
- Zero-length storage buffers are rejected by wgpu; insert a dummy element when a
  GPU table can be empty.
- `Rgba8Unorm` is used by the procedural texture pipeline so shader layer blending
  matches CPU previews; convert to linear at the PBR boundary.

Voxel terrain uses `block-mesh` for blocky greedy meshing. Do not replace it with
`fast-surface-nets` unless the intended output is smooth isosurfaces.

## UI and egui

`BorderRadius` is a `Node` field. `Text`, `TextFont`, `TextColor`, and `ImageNode`
are sibling components.

The workspace resolves `bevy_egui 0.40.1` with `egui 0.34.3`:

- Interactive egui widgets run in `EguiPrimaryContextPass`.
- `EguiContexts::ctx_mut()` returns `Result`.
- Use `egui::Button::image(...).selected(...)`, not deprecated `ImageButton`.
- Use `CornerRadius`, `Frame::corner_radius`, and `Margin` APIs from egui 0.34.
- Install the vendored DejaVu fallback through
  `block_junk_textures::egui_fonts::install(ctx)`.

## glTF animation

The client loads visible bodies with `WorldAssetRoot` and builds an
`AnimationGraph` from registered clips. `AnimationPlayer` appears on a descendant
after scene loading; observe its addition, then insert `AnimationGraphHandle` and
`AnimationTransitions` there.

KayKit character files in this repo have a compatible skeleton but may have no
animation tracks of their own. `preview.rs::WorldAssetPlugin` repairs animation
target components for cross-file retargeting. Reuse that path rather than adding a
second scene-loading workaround.

## Verification

For framework changes, run:

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

When an API assumption fails, inspect the exact 0.19 cached source, fix the code,
and update this skill in the same change.
