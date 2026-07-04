---
name: bevy-019
description: Project-local cheat sheet for Bevy 0.19 API specifics. Use whenever writing or editing Bevy code in this repo — the Bevy ecosystem moves fast and most online tutorials/skills target older versions (0.14–0.17) that will mislead. Also covers the voxel-meshing crate split (block-mesh vs fast-surface-nets).
user-invocable: false
---

# Bevy 0.19 — survival notes for this project

Bevy ships breaking changes every ~3 months. Anything you remember from training data or read on a blog is probably wrong. **When uncertain, read the cached source** at `~/.cargo/registry/src/index.crates.io-*/bevy_*-0.19.0/` — it is authoritative.

If you make an API decision based on this file and the build fails, treat that as a sign this file is stale. Update it after fixing.

## What 0.18 → 0.19 renamed (all hit this repo)

| 0.18 | 0.19 | Notes |
|---|---|---|
| `Scene` (asset), `Handle<Scene>` | `WorldAsset`, `Handle<WorldAsset>` | `Scene` is now a **trait** in the new BSN scene system — `Handle<Scene>` fails with E0782 "expected a type, found a trait" |
| `SceneRoot(handle)` | `WorldAssetRoot(handle)` | same tuple shape; glTF `#Scene0` asset labels unchanged |
| `SceneInstanceReady` | `WorldInstanceReady` | import from `bevy::world_serialization::` (was `bevy::scene::`); still `{ entity, instance_id }` |
| `DynamicScene` / `DynamicSceneBuilder` | `DynamicWorld` / `DynamicWorldBuilder` | now require `&TypeRegistry` |
| light `shadows_enabled` | `shadow_maps_enabled` | new siblings: `contact_shadows_enabled`, `soft_shadows_enabled` (point/spot) — all default false |
| `TextFont { font_size: f32 }` | `font_size: FontSize` enum | `FontSize::Px(14.0)`; also `Vw/Vh/VMin/VMax/Rem`; `impl From<f32>` = Px |
| `TextFont.font: Handle<Font>` | `font: FontSource` | `Handle` variant or `Family("FiraMono")` / semantic (`Monospace`) |
| `ShaderStorageBuffer` | `ShaderBuffer` | same module `bevy::render::storage`, same `From<Vec<T>>` |
| `DepthStencilState.depth_compare`, `.depth_write_enabled` | now `Option<_>` | wrap in `Some(..)` in specialize hooks (preview.rs) |
| `Assets::get_mut` → `&mut A` | returns `AssetMut<A>` guard | binding must be `let Some(mut x) = ...`; suppresses spurious `AssetEvent::Modified` |
| `TextLayout::new_with_justify` etc. | `TextLayout::justify()` etc. | `new_with_` prefix dropped |

**Headless apps: `StatesPlugin` is mandatory.** In 0.19 `app.init_state::<S>()` **panics at runtime** ("The StateTransition schedule is missing") if `StatesPlugin` isn't added. `DefaultPlugins` brings it; `MinimalPlugins` does NOT. The dedicated server adds `bevy::state::app::StatesPlugin` explicitly (main.rs::run_dedicated_server). Compiles fine either way — this one only bites when the process starts.

**Asset root resolution:** running a raw binary (not via `cargo run`) resolves `assets/` and `mods://` relative to the *executable's* directory. Set `BEVY_ASSET_ROOT=$PWD` when launching `./target/debug/...` by hand.

## Next-gen scenes (BSN) — exists, we don't use it yet

0.19 ships the `bsn!` macro / `Scene` trait / `SceneComponent` derive (this is why the old `Scene` asset got renamed `WorldAsset`). Opt-in; no `.bsn` asset loader shipped yet. bevy_ui also gained real widgets (`bevy_feathers`, `EditableText` text input, richer text). Our UI stays egui for now — see the 2026-07 upgrade notes in git history for the assessment.

## Module path map (stuff that moved)

`bevy_internal` re-exports each sub-crate as a top-level module of `bevy::`. Useful paths:

| Item | 0.19 path | Older path you might remember |
|---|---|---|
| `Mesh`, `Indices`, `PrimitiveTopology`, `Mesh3d`, `Mesh2d` | `bevy::mesh::*` | `bevy::render::mesh::*` |
| `RenderAssetUsages` | `bevy::asset::RenderAssetUsages` | `bevy::render::render_asset::*` |
| `MeshMaterial3d`, `StandardMaterial` | `bevy::pbr::*` (in prelude) | mostly the same |
| `Camera3d`, `Camera2d` | `bevy::camera::*` (in prelude) | `bevy::core_pipeline::*` |
| `DirectionalLight`, `PointLight`, `AmbientLight` | `bevy::light::*` (in prelude) | `bevy::pbr::*` |
| `WorldAssetRoot`, `WorldInstanceReady`, `DynamicWorld` | `bevy::world_serialization::*` (prelude has most) | `bevy::scene::*` |
| `Window`, `PrimaryWindow`, `CursorOptions`, `CursorGrabMode` | `bevy::window::*` | same module, but **see CursorOptions split below** |
| `AlphaMode` | `bevy_material` (re-exported via pbr prelude) | `bevy::render` |

The `bevy::prelude` is generous — most everyday types are in it. When in doubt, just `use bevy::prelude::*;` and add module-qualified imports for the rest.

## Events → Messages (renamed in 0.17, sticks in 0.19)

"Buffered events" are called "messages." **`EventReader` no longer compiles.**

| 0.16 and earlier | 0.17+ |
|---|---|
| `Event` trait, `#[derive(Event)]` | `Message` trait, `#[derive(Message)]` |
| `EventReader<T>` / `EventWriter<T>` | `MessageReader<T>` / `MessageWriter<T>` |
| `Events<T>` resource | `Messages<T>` resource |
| `app.add_event::<T>()` | `app.add_message::<T>()` |
| `World::send_event` / `Commands::send_event` | `World::write_message` / `Commands::write_message` |

`#[derive(Event)]` and `On`/observer pattern are reserved for **entity-targeted events** (different concept). Don't conflate the two.

For mouse motion specifically, **prefer `Res<AccumulatedMouseMotion>`** (resource summing this frame's delta) over `MessageReader<MouseMotion>` — fewer lifetimes, simpler code, same data.

**The old lightyear `Message`-derive collision is RESOLVED as of lightyear 0.28** (verified by compile experiment 2026-07-02): lightyear no longer exports a `#[derive(Message)]` proc-macro, only a blanket `Message` trait. `#[derive(Message)]` + `MessageReader<T>` now work in files that glob-import `lightyear::prelude::*`. New in-process buffered events can use bevy's native message system; the `PendingToasts`-style `Vec<T>`-in-a-`Resource` queues (worldspace_toast.rs) are a historical workaround, fine to leave but not required for new code. Only naming the bare `Message` *trait* (e.g. `T: Message`) is still ambiguous under both globs — qualify it if you ever need to.

## Window split: `CursorOptions` is its own component (unchanged from 0.18)

`Window.cursor_options` does not exist. The window entity carries both `Window` and a separate `CursorOptions` component.

```rust
use bevy::window::{CursorOptions, CursorGrabMode, PrimaryWindow};
fn lock_cursor(mut c: Query<&mut CursorOptions, With<PrimaryWindow>>) {
    if let Ok(mut c) = c.single_mut() {
        c.grab_mode = CursorGrabMode::Locked;
        c.visible = false;
    }
}
```

When customizing the primary window, set `WindowPlugin { primary_cursor_options: Some(CursorOptions { .. }), .. }` — it's a sibling of `primary_window`, not nested inside it.

## Input feature flags (unchanged from 0.18)

Mouse, keyboard, gamepad, and touch are **opt-in features** of the `bevy` crate. The `default` feature set does **not** enable them.

```toml
bevy = { version = "0.19", features = ["mouse", "keyboard"] }
# add "gamepad", "touch", "gestures" as needed
```

Without these, input plugins won't register at runtime. Types still compile (they live in `bevy_input` unconditionally), so this fails silently. Note 0.19 also moved `audio` into default features and stopped implying `ui` from `2d`/`3d` — if a build ever loses UI rendering after feature pruning, check that.

## Bundles → required components (since 0.15)

`SpriteBundle`, `Camera3dBundle`, `PbrBundle`, etc. are **gone**. Spawn the components directly; required-components fill in the rest.

```rust
commands.spawn((
    Mesh3d(mesh_handle),
    MeshMaterial3d(material_handle),
    Transform::from_xyz(0.0, 0.0, 0.0),
));
```

`Mesh3d` and `MeshMaterial3d` are tuple-struct wrappers around handles, not bundles.

## Time API

`Time::delta_seconds()` → `Time::delta_secs()` (since ~0.16). Same for `elapsed_secs`, etc. The `_seconds` variants are gone, not deprecated.

## Query::single returns Result

`Query::single()` and `Query::single_mut()` return `Result<_, QuerySingleError>`. Use `let Ok(x) = q.single() else { return; };` — never `.unwrap()` in shipped code.

## Mesh construction (unchanged from 0.18)

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

`Assets<Mesh>` retains `RENDER_WORLD`-only meshes after data extraction — you don't need `MAIN_WORLD` unless you actually read mesh data CPU-side.

## Custom `Material` shaders: use the `MATERIAL_BIND_GROUP` shader-def

Verified still true in 0.19: `MATERIAL_BIND_GROUP_INDEX = 3` in `bevy_pbr`, injected as the `#{MATERIAL_BIND_GROUP}` shader-def. Hardcoding `@group(2)` builds fine but blows up at first draw ("ResourceBinding { group: 2 } is not available in the pipeline layout") because group 2 is the mesh storage bind group.

```wgsl
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> color: vec4<f32>;
```

Also note: Bevy uses **reversed-Z depth**. Default `depth_compare` is `Some(CompareFunction::GreaterEqual)` — closer to camera = larger depth value. To pick out fragments BEHIND existing geometry (X-ray / blueprint effects), flip to `Less`, not `Greater`. In 0.19 `depth_compare` and `depth_write_enabled` are `Option`s — `Some(..)`-wrap what you set (see preview.rs).

Note: 0.19 replaced the render *graph* with ECS schedules (render passes are systems now). Only matters if we ever write a custom render pass — `Material` impls are unaffected.

## bevy_ui: BorderRadius is a `Node` field, not a Component (unchanged)

```rust
commands.spawn(Node {
    border_radius: BorderRadius::all(Val::Px(6.0)),
    ..default()
});
```

`BackgroundColor` and `BorderColor` are still standalone components — only `BorderRadius` was folded into `Node`.

## `SystemParam`, Bundle, and tuple caps (unchanged from 0.18)

1. **A single system can take at most 16 system params.** The 17th gives a cryptic `no method named "in_set" found for fn item …` at the call site. Fix: `#[derive(SystemParam)]` struct, or extract gating into `run_if`.
2. **`commands.spawn(...)` tuples cap at 15 elements.** 16th gives `(...) is not a Bundle`. Fix: nest tuple-of-tuples (Bundle is recursive). Used by the NPC spawn paths in `npc.rs` and `server.rs`.
3. **`add_systems(Update, (a, b, c, …))` tuples cap at ~5–8 systems** depending on per-system signature complexity. Fix: split into multiple `.add_systems` calls (server.rs `Simulation` set does this on purpose).

Caps 1 and 3 surface as the same E0599 "no method named in_set" pattern. Cap 2 surfaces as "is not a Bundle". Count params/tuple size before chasing other leads.

## bevy_ui: `Text` and `ImageNode` constructors

`Text::new(impl Into<String>)` and `ImageNode::new(Handle<Image>)`; each auto-inserts `Node` via required components; spawn an explicit `Node` alongside for sizing. `TextFont`/`TextColor` are sibling components. **0.19: `font_size` takes `FontSize::Px(18.0)`, not a bare f32.**

```rust
commands.spawn((
    Text::new("Select"),
    TextFont { font_size: FontSize::Px(18.0), ..default() },
    TextColor(Color::WHITE),
));
```

## DirectionalLight as component

```rust
commands.spawn((
    DirectionalLight {
        illuminance: 10_000.0,
        shadow_maps_enabled: true, // was `shadows_enabled` pre-0.19
        ..default()
    },
    Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, 0.4, 0.0)),
));
```

0.19 added `contact_shadows_enabled` (cheap screen-space contact shadows; off by default) — candidate for a visual-quality pass someday.

## Voxel meshing crate split (bevy-independent)

| Crate | Algorithms |
|---|---|
| `block-mesh` (0.2) | `greedy_quads`, `visible_block_faces` — faceted/blocky |
| `fast-surface-nets` (0.2) | `surface_nets` — smooth, what we want |

Both re-export `ndshape`. `f32` implements `SignedDistance`; `Sd8`/`Sd16` are the memory-tight alternatives.

```rust
use fast_surface_nets::{surface_nets, SurfaceNetsBuffer};
use fast_surface_nets::ndshape::{ConstShape, ConstShape3u32};
type ChunkShape = ConstShape3u32<34, 34, 34>; // 32 + 2 padding
```

## Skinned-mesh animation (glTF)

Same three-asset/three-component shape as 0.18 (`AnimationClip`, `AnimationGraph`, `Gltf`; `WorldAssetRoot` + auto-inserted `AnimationPlayer` + `AnimationGraphHandle`/`AnimationTransitions`). Skim `~/.cargo/registry/src/index.crates.io-*/bevy-0.19.0/examples/animation/` before writing new animation code.

Key gotchas (all still true in 0.19, re-verified at runtime 2026-07-02):
- `AnimationTransitions` owns clip switching — don't call `player.play()` directly alongside it.
- The `AnimationPlayer` lives on a *descendant* of the `WorldAssetRoot` bearer; wait for `Added<AnimationPlayer>`.
- glTF origin at feet vs `AvatarPose.translation` at eyes — offset the scene Transform.
- `Children::iter()` yields `Entity` by value — `for child in children.iter()`, not `for &child`.
- Required-component trap: `Mesh3d` requires Transform/Visibility; a parent that loses its renderable loses those silently. Insert `Transform::default()` + `Visibility::default()` explicitly on positioned non-renderable parents.

### Cross-file animation retargeting (separate character + animation glbs)

Unchanged in shape, one algorithm note. The Bevy loader only rigs (`AnimationPlayer` + `AnimationTargetId` + `AnimatedBy`) files that contain their own animations (`bevy_gltf-0.19.0/src/loader/mod.rs` ~552-570 and ~1544-1560). KayKit-style packs (character glb with zero clips + rig glbs) need the manual rigging pass in `client.rs::setup_npc_skeleton_anim`: on `WorldInstanceReady`, walk the tree, insert `AnimationTargetId::from_names(path)` + `AnimatedBy(rig_root)` per named node, player+graph on the rig root.

- **0.19 changed the `AnimationTargetId` hash to blake3** (was UUID/SHA1). Doesn't break the pass — clips and targets are both hashed at runtime by the same bevy version — but IDs are NOT stable across bevy versions, so never persist them.
- Path semantics unchanged: from the animation-root node down, inclusive of its own name.
- The loader still spawns an unnamed coordinate-conversion wrapper above the glTF's named roots — find the rig root by `Name`, not positional child.
- `AnimationTargetId`/`AnimatedBy` are not in the prelude: `use bevy::animation::{AnimatedBy, AnimationTargetId};`.
- Symptom of a rigging miss is unchanged: mesh upright but all body parts collapsed at one point.

## bevy_egui 0.40 / egui 0.34

**Version pins (2026-07):** bevy_egui 0.40.x ↔ bevy 0.19 + egui 0.34. bevy_egui **0.41 jumps to egui 0.35** — available (bevy-inspector-egui was removed 2026-07-03, nothing pins us anymore) but taking it is a small migration pass of its own. `block-junk-textures`' direct `egui` dep must track bevy_egui's pin or Context types won't unify.

### Interactive UI must run in `EguiPrimaryContextPass` (unchanged)

Pointer/click collection only happens in that schedule. A `Window::show` in plain `Update` renders but every `clicked()` is silently false. Read-only overlays get away with `Update`; anything with buttons/sliders/text goes in the pass:

```rust
app.add_systems(bevy_egui::EguiPrimaryContextPass, my_panel_ui.run_if(in_state(AppState::InGame)));
```

In-repo: `menu.rs::pause_menu_ui`, `debug.rs::debug_panel_ui`.

### egui 0.34 changes that hit this repo

- `ctx.style()` → `ctx.global_style()`, `ctx.set_style(..)` → `ctx.set_global_style(..)` (ui_theme.rs).
- **Top-level panels off a bare `Context` are deprecated.** Build a root `Ui` over the background layer and `show_inside` it — helper `ui_theme::root_ui(ctx, "id")`:
  ```rust
  let mut root = crate::ui_theme::root_ui(ctx, "main_menu_root");
  egui::CentralPanel::default().show_inside(&mut root, |ui| { ... });
  ```
- `SidePanel`/`TopBottomPanel` → unified `egui::Panel::{left,right,top,bottom}(id)`; `.default_width/.default_height` → `.default_size`. See texture-studio `ui.rs` for the full four-panel layout.
- `ctx.wants_pointer_input()` → `ctx.egui_wants_pointer_input()` (same for keyboard) — the input-gating pattern from plain Update systems still works.
- `Ui` now derefs to `Context`; `SelectableLabel` removed; new `Popup`/`Tooltip` APIs; `ui.close()` closes the containing popup/menu (unchanged).
- Font backend swapped to skrifa + vello_cpu (hinting!). `FontDefinitions`/`set_fonts` API unchanged — the vendored DejaVu fallback (`block_junk_textures::egui_fonts::install(ctx)`) works as before; still BMP-only symbol coverage, pick BMP icons.
- `EguiContexts::ctx_mut()` returns `Result` (unchanged from 0.39).
- Styling: `CornerRadius` (u8), `visuals.window_corner_radius`, widget tiers at `visuals.widgets.{noninteractive,inactive,hovered,active}` — all unchanged from egui 0.33.
- CPU preview images: `ctx.load_texture(name, ColorImage::…, TextureOptions::NEAREST)` unchanged.
- bevy_egui 0.40 scaling: zoom now via `native_pixels_per_point`/`set_zoom_factor` — don't call `set_pixels_per_point`.

## Shader-library + ghost-material learnings (2026-07)

- **Import-only WGSL libraries**: give the file `#define_import_path my_crate::my_lib`, register with `bevy::shader::load_shader_library!(app, "path/lib.wgsl")` (embeds + permanently loads; a bare `embedded_asset!` is NOT enough — nothing would load it). Cross-crate imports work (block-junk imports `block_junk_textures::dither`); a library may declare bind-group bindings, but then every importer's `AsBindGroup` must bind the same resources at the same slot numbers (chunk material + ghost material share `composite.wgsl`'s @100-103 this way).
- **Per-instance pipeline variants on one material type**: `#[bind_group_data(MyKey)]` on the `AsBindGroup` type + `impl From<&MyExt> for MyKey`; `specialize` reads `key.bind_group_data` — used for the ghost materials' front (opaque, depth write) vs x-ray (Blend → Transparent3d, `depth_compare: Some(Less)`, no write) split. For `ExtendedMaterial`, which pass a mesh renders in comes from the *base* StandardMaterial's `alpha_mode` — keep it consistent with the key.
- **Screen-door (Bayer-dither discard) transparency**: render opaque + `discard` on a Bayer threshold. Depth writes make ghosts self-occlude correctly (no alpha accumulation). Fragment pixel coords = `in.position.xy` (`@builtin(position)`); camera pos = `view.world_position` via `#import bevy_pbr::mesh_view_bindings::view`. Opaque ghosts cast shadows — add `bevy::light::NotShadowCaster`. Forward-only: if a depth/deferred prepass is ever added, mirror the discard into a prepass fragment shader or early-Z resurrects the discarded pixels.
- **Uniform structs**: mirror field order Rust↔WGSL and pad explicitly to 16-byte multiples (`_pad0: f32...`) rather than trusting implicit layout agreement.
- **Save-format gotcha (project, not Bevy)**: the save blob is bincode = positional; `#[serde(default)]` does NOT provide backward compat there. Any field added to a saved struct needs a `SAVE_VERSION` bump + a verbatim `SaveFileVn` migration struct (see save.rs v12/v13 pattern).

## Texture/material learnings from the procedural-texture build (2026-06)

- **`embedded_asset!` in a library crate**: path relative to the invoking file's dir; URL `embedded://<crate_snake_case>/<path-from-src>`.
- **`AsBindGroup` texture without a sampler is valid** — `#[texture(100, dimension = "2d_array")]` with no `#[sampler]` pairs fine with `textureLoad`-only shaders.
- **Storage-buffer bindings must be non-empty** — `ShaderBuffer::from(vec![])` builds a zero-size buffer wgpu rejects at bind time. Push a dummy element. (Renamed from `ShaderStorageBuffer` in 0.19.)
- **`AccumulatedMouseScroll`** resource for zoom — no `MessageReader<MouseWheel>` needed.
- **Ambient light** is the `GlobalAmbientLight` resource (set `.brightness`).
- **Srgb vs blend math**: store color tiles as `Rgba8Unorm` (NOT `…Srgb`); blend in display space; convert to linear once (`srgb_to_linear`) before handing PBR the base color.

## When a check fails

The cached crate source at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/<crate>-<version>/` is the source of truth. Before guessing at an API:

```sh
grep -nE "pub mod|pub use|pub fn|pub struct" $(find ~/.cargo/registry/src -name "<crate>-<version>" -type d)/src/lib.rs
```

Verify, then update this file with what you found.
