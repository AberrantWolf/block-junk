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

## Custom `Material` shaders: bind group is **3**, not 2

Pre-0.17 cheat sheets and most blog posts say "material bindings live at `@group(2)`." In 0.18 the index is **3** (`MATERIAL_BIND_GROUP_INDEX = 3` in `bevy_pbr`). Hardcoding `@group(2)` builds fine but blows up at first draw with:

```
Shader global ResourceBinding { group: 2, binding: 0 } is not available in the pipeline layout
Storage class Storage { access: StorageAccess(LOAD) } doesn't match the shader Uniform
```

…because group 2 is now the mesh storage bind group. Fix: use the shader-def Bevy injects per material, which is set to the correct index automatically.

```wgsl
#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> color: vec4<f32>;

@fragment
fn fragment(_in: VertexOutput) -> @location(0) vec4<f32> {
    return color;
}
```

Bevy's preprocessor replaces `#{MATERIAL_BIND_GROUP}` at compile time. This is the only correct way — there is no const value you can `@group(N)` directly.

Also note: Bevy uses **reversed-Z depth**. Default `depth_compare` is `CompareFunction::GreaterEqual` — closer to camera = larger depth value. To pick out fragments BEHIND existing geometry (X-ray / blueprint effects), flip to `Less`, not `Greater`.

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

## Skinned-mesh animation (glTF)

Bevy 0.18 animation rests on three asset types and three components. Skim the cached examples in `~/.cargo/registry/src/index.crates.io-*/bevy-0.18.1/examples/animation/` (especially `animated_mesh_control.rs` and `animation_graph.rs`) before writing new animation code — the API has churned every minor version.

**Asset types** (`bevy_animation` + `bevy_gltf`):
- `AnimationClip` — one named clip; the glTF loader produces one per `gltf.animations[i]`.
- `AnimationGraph` (asset) — a petgraph DAG of clip + blend + add nodes. Construct with `AnimationGraph::from_clips([clip_handles])` → `(graph, Vec<AnimationNodeIndex>)`, or `new()` + `add_clip(handle, weight, parent)`.
- `Gltf` (asset) — top-level handle to a loaded glb. Has `named_animations: HashMap<Box<str>, Handle<AnimationClip>>` and `named_scenes`, so you can look up clips by their Blender name instead of guessing indices. Load it as `asset_server.load::<Gltf>("path.glb")`, but you have to wait for it to finish loading before reading those fields.

**Components**:
- `SceneRoot(Handle<Scene>)` on the entity — Bevy walks the glTF hierarchy and spawns the skinned mesh + bones as children.
- `AnimationPlayer` — **auto-inserted** on a descendant of the scene root once it finishes loading (specifically, on the entity that owns the animation targets). Don't insert it manually. Watch `Added<AnimationPlayer>` to find it.
- `AnimationGraphHandle(Handle<AnimationGraph>)` and `AnimationTransitions` — insert these alongside the `AnimationPlayer` once it appears, then drive playback via the transitions component.

**Retargeting across separate .glb files** works as long as the bone hierarchy/names match. Clips are keyed by `AnimationTargetId`, a stable UUID hashed from the bone path within the glTF. KayKit-style packs that ship one `*_Rig_Medium_*.glb` of animations + multiple character `.glb`s sharing the same skeleton just work — load clips from the animation file, scene from the character file.

**Minimal wiring pattern** (cribbed from `animated_mesh_control.rs`):

```rust
// 1. Build the graph at startup, stash node indices in a Resource.
let (graph, indices) = AnimationGraph::from_clips([
    asset_server.load(GltfAssetLabel::Animation(0).from_asset(anim_glb)), // idle
    asset_server.load(GltfAssetLabel::Animation(1).from_asset(anim_glb)), // walk
]);
let graph_handle = graphs.add(graph);

// 2. Spawn the character scene.
commands.spawn(SceneRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(char_glb))));

// 3. When the AnimationPlayer auto-appears, attach graph + transitions and start.
fn setup_player_once_loaded(
    mut commands: Commands,
    library: Res<AnimationLibrary>,
    mut new_players: Query<(Entity, &mut AnimationPlayer), Added<AnimationPlayer>>,
) {
    for (e, mut player) in &mut new_players {
        let mut transitions = AnimationTransitions::new();
        transitions.play(&mut player, library.idle, Duration::ZERO).repeat();
        commands.entity(e).insert((
            AnimationGraphHandle(library.graph.clone()),
            transitions,
        ));
    }
}

// 4. Crossfade to another clip:
transitions.play(&mut player, library.walk, Duration::from_millis(200)).repeat();
```

**Gotchas**:
- `AnimationTransitions` *owns* clip switching. Don't call `player.play()` directly when transitions are present — the example file warns the two will fight.
- The `AnimationPlayer` lives on a descendant entity, **not** on the entity that owns `SceneRoot`. If you want to drive transitions from elsewhere (e.g. a system that watches the parent's `AvatarVelocity`), cache the player entity on the parent the moment it appears.
- glTFs typically place the model origin at the feet; our `AvatarPose.translation` is the eye position. Offset the scene's `Transform` by `-(EYE_OFFSET_FROM_CENTRE + PLAYER_HALF_EXTENTS.y)` when attaching, or you get floating-shoulder characters.
- Spawning a SceneRoot is asynchronous — the children (and the auto-added `AnimationPlayer`) don't exist on the same frame. Any startup logic that touches them must wait for `Added<AnimationPlayer>`.
- **Iterator items: `Children::iter()` yields `Entity` by value, not `&Entity`. Don't write `for &child in children.iter()` — it won't compile. Just `for child in children.iter()`.**
- **Required-component trap when swapping renderables:** `Mesh3d` auto-inserts `Transform` + `GlobalTransform` + `Visibility` + `InheritedVisibility` + `ViewVisibility` via required components. If you replace a `Mesh3d` insert with something that *doesn't* require Transform (e.g. moving the mesh into a child SceneRoot and leaving only a marker on the parent), the parent loses its Transform silently. Sync systems querying `&mut Transform` skip the entity, child transforms inherit from nothing, and everything renders at world origin. Always insert `Transform::default()` + `Visibility::default()` explicitly on parent entities that need to be positioned but don't bear a renderable.

### Cross-file animation retargeting (separate character + animation glbs)

KayKit-style asset packs ship a character glb (Knight.glb) with the skinned mesh + skeleton but **zero animation tracks**, and bundles of animation clips in separate "rig" glbs (Rig_Medium_*.glb) against the same skeleton. The intuitive setup — spawn the character scene, build an `AnimationGraph` from the rig's clip handles, attach it — **silently produces a body whose parts all collapse to the origin**. The Bevy 0.18 loader only inserts `AnimationPlayer`, `AnimationTargetId`, and `AnimatedBy` when the file being loaded contains its own animations (see `bevy_gltf/src/loader/mod.rs:1016-1027` and `1487-1495`). With no AnimationPlayer driving bones, bone transforms stay at identity; with no `AnimationTargetId` on the bones, the rig clip's curves have nothing to bind to even if you insert an AnimationPlayer manually.

Symptom signature: skinned mesh **upright** (not lying down — that'd be a coordinate-system issue) with **all body parts at the same world position**. That's bind-pose-relative meshes drawn without the inverse-bind correction that the bone hierarchy normally cancels out.

Fix: replay the loader's rigging pass yourself in a `SceneInstanceReady` observer. Walk the scene's entity tree, build a `Vec<Name>` path from the scene-root node down (inclusive of its own name), and on each named entity insert `AnimationTargetId::from_names(path.iter())` + `AnimatedBy(rig_root)`. Insert `AnimationPlayer` + `AnimationGraphHandle` on the rig_root entity (the direct child of the SceneRoot bearer, matching the glb's top-level scene node). Retargeting then works because `AnimationTargetId` is hashed from the bone-name chain — character and rig glbs computing the same chain get the same UUID and the clip's curves bind to your bones. See `client.rs::setup_npc_skeleton_anim` for the in-repo implementation.

`AnimationTargetId` and `AnimatedBy` are **not in the bevy prelude** — import explicitly: `use bevy::animation::{AnimatedBy, AnimationTargetId};`.

One more wrapper trap when reaching into a scene: Bevy's glTF loader spawns an unnamed coordinate-system-conversion entity (`world_root_id`) above the glTF's named top-level nodes. So `SceneRoot bearer → first child` is the unnamed wrapper, *not* the named glTF root. Search descendants by `Name` (`name.as_str() == "Rig_Medium"`) rather than positional `children.iter().next()` — the wrapper count can change with Bevy versions or `convert_coordinates` settings.

### bevy_egui: interactive UI must run in `EguiPrimaryContextPass`

bevy_egui's pointer/click event collection only happens inside the `EguiPrimaryContextPass` schedule. A `Window::show(...)` call placed in regular `Update` (or `FixedUpdate`, or one of the project's `GameSet::*` sets) **will render the window and its buttons, but every `button(...).clicked()` returns false** — the input never reaches egui. Symptom: panel visible, cursor releases as expected, clicks land silently with no log spam to suggest a wiring problem.

Read-only egui systems (like `debug_overlay_ui`) get away with it because they don't read input. Anything with buttons, sliders, or text fields must live in `EguiPrimaryContextPass`:

```rust
app.add_systems(
    bevy_egui::EguiPrimaryContextPass,
    my_panel_ui.run_if(in_state(AppState::InGame)),
);
```

In-repo: `menu.rs::pause_menu_ui` and `debug.rs::debug_panel_ui` both use this schedule. Other system ordering (`GameSet::Input` etc.) still applies to the input *handlers* (keyboard toggles, etc.) — only the egui UI itself needs the special schedule.

## When a check fails

The cached crate source at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/<crate>-<version>/` is the source of truth. Before guessing at an API:

```sh
grep -nE "pub mod|pub use|pub fn|pub struct" $(find ~/.cargo/registry/src -name "<crate>-<version>" -type d)/src/lib.rs
```

Verify, then update this file with what you found.
