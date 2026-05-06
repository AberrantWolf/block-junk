---
name: bevy-018
description: Project-local cheat sheet for Bevy 0.18 API specifics. Use whenever writing or editing Bevy code in this repo — the Bevy ecosystem moves fast and most online tutorials/skills target older versions (0.14–0.16) that will mislead. Also covers the voxel-meshing crate split (block-mesh vs fast-surface-nets).
user-invocable: false
---

# Bevy 0.18 — survival notes for this project

Bevy ships breaking changes every ~3 months. Anything you remember from training data or read on a blog is probably wrong. **When uncertain, read the cached source** at `~/.cargo/registry/src/index.crates.io-*/bevy_*-0.18.1/` — it is authoritative.

If you make an API decision based on this file and the build fails, treat that as a sign this file is stale. Update it after fixing.

## Module path map (stuff that moved)

`bevy_internal` re-exports each sub-crate as a top-level module of `bevy::`. Useful paths:

| Item | 0.18 path | Older path you might remember |
|---|---|---|
| `Mesh`, `Indices`, `PrimitiveTopology`, `Mesh3d`, `Mesh2d` | `bevy::mesh::*` | `bevy::render::mesh::*` |
| `RenderAssetUsages` | `bevy::asset::RenderAssetUsages` | `bevy::render::render_asset::*` |
| `MeshMaterial3d`, `StandardMaterial` | `bevy::pbr::*` (in prelude) | mostly the same |
| `Camera3d`, `Camera2d` | `bevy::camera::*` (in prelude) | `bevy::core_pipeline::*` |
| `DirectionalLight`, `PointLight`, `AmbientLight` | `bevy::light::*` (in prelude) | `bevy::pbr::*` |
| `MouseMotion`, `MouseButton`, `AccumulatedMouseMotion` | `bevy::input::mouse::*` (in prelude) | same |
| `Window`, `PrimaryWindow`, `CursorOptions`, `CursorGrabMode` | `bevy::window::*` | same module, but **see CursorOptions split below** |

The `bevy::prelude` is generous — most everyday types are in it. When in doubt, just `use bevy::prelude::*;` and add module-qualified imports for the rest.

## Events → Messages (renamed in 0.17, sticks in 0.18)

"Buffered events" are now called "messages." **`EventReader` no longer compiles.**

| 0.16 and earlier | 0.17+ |
|---|---|
| `Event` trait, `#[derive(Event)]` | `Message` trait, `#[derive(Message)]` |
| `EventReader<T>` | `MessageReader<T>` |
| `EventWriter<T>` | `MessageWriter<T>` |
| `Events<T>` resource | `Messages<T>` resource |
| `app.add_event::<T>()` | `app.add_message::<T>()` |
| `World::send_event` / `Commands::send_event` | `World::write_message` / `Commands::write_message` |

`#[derive(Event)]` and `Trigger`/observer pattern are now reserved for **entity-targeted events** (different concept). Don't conflate the two.

For mouse motion specifically, **prefer `Res<AccumulatedMouseMotion>`** (resource summing this frame's delta) over `MessageReader<MouseMotion>` — fewer lifetimes, simpler code, same data.

## Window split: `CursorOptions` is its own component

`Window.cursor_options` no longer exists. The window entity carries both `Window` and a separate `CursorOptions` component.

```rust
// 0.16:
fn lock_cursor(mut w: Query<&mut Window, With<PrimaryWindow>>) {
    w.single_mut().unwrap().cursor_options.grab_mode = CursorGrabMode::Locked;
}

// 0.18:
use bevy::window::{CursorOptions, CursorGrabMode, PrimaryWindow};
fn lock_cursor(mut c: Query<&mut CursorOptions, With<PrimaryWindow>>) {
    if let Ok(mut c) = c.single_mut() {
        c.grab_mode = CursorGrabMode::Locked;
        c.visible = false;
    }
}
```

When customizing the primary window, set `WindowPlugin { primary_cursor_options: Some(CursorOptions { .. }), .. }` — it's a sibling of `primary_window`, not nested inside it.

## Input feature flags (0.18)

Mouse, keyboard, gamepad, and touch are **opt-in features** of the `bevy` crate. The `default` feature set (`["2d", "3d", "ui"]`) does **not** enable them.

```toml
bevy = { version = "0.18", features = ["mouse", "keyboard"] }
# add "gamepad", "touch", "gestures" as needed
```

Without these, input plugins won't register at runtime. Types still compile (they live in `bevy_input` unconditionally), so this fails silently.

## Bundles → required components (since 0.15)

`SpriteBundle`, `Camera3dBundle`, `MaterialMeshBundle`, `PbrBundle`, etc. are **gone**. Spawn the components directly; required-components fill in the rest.

```rust
// Old:
commands.spawn(PbrBundle { mesh, material, transform, ..default() });

// 0.18:
commands.spawn((
    Mesh3d(mesh_handle),
    MeshMaterial3d(material_handle),
    Transform::from_xyz(0.0, 0.0, 0.0),
));
// Camera3d, DirectionalLight, etc. work the same way.
```

`Mesh3d` and `MeshMaterial3d` are tuple-struct wrappers around handles, not bundles.

## Time API

`Time::delta_seconds()` → `Time::delta_secs()` (since ~0.16). Same for `elapsed_secs`, etc. The `_seconds` variants are gone, not deprecated.

## Query::single returns Result

`Query::single()` and `Query::single_mut()` return `Result<_, QuerySingleError>`. Use `let Ok(x) = q.single() else { return; };` or `let-else` — never `.unwrap()` in shipped code.

## Mesh construction

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

Note: in 0.18, `Assets<Mesh>` retains `RENDER_WORLD`-only meshes after data extraction — you don't need to keep `MAIN_WORLD` set unless you actually read mesh data CPU-side.

## DirectionalLight as component

```rust
commands.spawn((
    DirectionalLight {
        illuminance: 10_000.0,
        shadows_enabled: true,
        ..default()
    },
    Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, 0.4, 0.0)),
));
```

`Transform` provides direction; the component itself is just lighting parameters.

## Voxel meshing crate split

This bites every voxel project. There used to be one crate; now there are two:

| Crate | Algorithms |
|---|---|
| `block-mesh` (0.2) | `greedy_quads`, `visible_block_faces` — **faceted/blocky output** |
| `fast-surface-nets` (0.2) | `surface_nets` — **smooth output, what we want** |

For block-junk we use `fast-surface-nets`. Both crates re-export `ndshape` (`ConstShape3u32` etc.) so we still have one ndshape version.

```rust
use fast_surface_nets::{surface_nets, SurfaceNetsBuffer};
use fast_surface_nets::ndshape::{ConstShape, ConstShape3u32};

type ChunkShape = ConstShape3u32<34, 34, 34>; // 32 + 2 padding

let sdf: Vec<f32> = /* negative inside, positive outside */;
let mut buffer = SurfaceNetsBuffer::default();
surface_nets(&sdf, &ChunkShape {}, [0; 3], [33; 3], &mut buffer);
// buffer.positions: Vec<[f32; 3]>, buffer.normals: Vec<[f32; 3]>, buffer.indices: Vec<u32>
```

`f32` implements `SignedDistance` directly. For memory-tight chunks later, `Sd8`/`Sd16` from `fast_surface_nets` are i8/i16 fixed-point alternatives.

## When a check fails

The cached crate source at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/<crate>-<version>/` is the source of truth. Before guessing at an API:

```sh
grep -nE "pub mod|pub use|pub fn|pub struct" $(find ~/.cargo/registry/src -name "<crate>-<version>" -type d)/src/lib.rs
```

Verify, then update this file with what you found.
