# Modeling guidelines for block-junk

Reference for making `.glb` mesh assets that drop into a mod's
`models/` folder and render as block entities (furniture, doors, etc.).

## Units

**1 model unit = 1 metre = 1 cell.** A unit cube fills exactly one
block. Don't scale.

## Coordinate system

Bevy is **right-handed, Y-up**:

| Axis | Direction         |
| ---- | ----------------- |
| +X   | east              |
| +Y   | up                |
| +Z   | south (toward the default camera) |
| −Z   | north             |

**Blender** uses Z-up internally; its glTF exporter remaps for you:

| In Blender | Becomes in Bevy |
| ---------- | --------------- |
| +Z up      | +Y up           |
| +Y forward | −Z (north)      |
| +X right   | +X (east)       |

So model in Blender with Z up, Y "into the screen," and the export
takes care of the rest.

**Blockbench** is already Y-up / −Z-forward (glTF native). No remap.

## Origin and placement

When the player places a block at cell `(X, Y, Z)`, the engine spawns
the entity with translation `(X + 0.5, Y, Z + 0.5)` — i.e. the model's
local origin lands at the **bottom-centre of the cell**.

Build your model so that:

- **Y = 0** is the cell's floor (model bottom rests on it).
- **X = 0, Z = 0** is the cell's horizontal centre.
- The model extends upward in +Y and outward symmetrically in X / Z.

If your cell is `(10, 16, 5)`, your model origin ends up at world
`(10.5, 16, 5.5)`.

## Single-cell models (current — phase 1)

A model that fits in one cell should sit inside this bounding box:

| Axis | Range          |
| ---- | -------------- |
| X    | −0.5 to +0.5   |
| Y    |  0   to +1     |
| Z    | −0.5 to +0.5   |

You can use less than the full cube; the voxel mesher skips the cube
faces of *that cell only*, so adjacent blocks render normally next to
your model.

## Multi-cell models

Multi-cell entities are supported. Declare which cells they occupy with
`footprint` in `register{}`, and the engine will rotate the model + the
occupancy footprint together based on the player's facing (with an
optional Ctrl+MouseWheel manual rotation override at place time).

- The **anchor** cell is where the player clicks (`footprint` entry
  `{0,0,0}`). For furniture it's the **foot** (bed foot, chair anchor,
  etc.).
- In default (unrotated, "east") orientation, the model extends in
  **+X** (east). The engine rotates 90° per cardinal step at place time.
- The cube voxel mesher skips faces of every footprint cell, so the
  model has clean airspace to sit in even at non-anchor cells.

For a **2-long bed** (default orientation, foot at anchor):

| Axis | Range          |
| ---- | -------------- |
| X    | −0.5 to +1.5   | (2 m long: foot half in anchor, full head cell, head half) |
| Y    |  0   to +1     |
| Z    | −0.5 to +0.5   |

```lua
register {
    id = "yourmod:bed",
    -- ...
    mesh = "mods://yourmod/models/bed.glb",
    footprint = { {0, 0, 0}, {1, 0, 0} },
}
```

## entity_aabb (optional but recommended for partial-cell models)

A model that doesn't fill its full footprint cells (a bed that's only
half the cell tall, a chair, a shrine) should declare a tight bounding
box. The raycast does ray-AABB tests on entity cells, so a tight AABB
means a click *above* a half-height bed passes through to whatever's
behind it instead of breaking the bed.

`entity_aabb` is in the same model frame as the geometry: origin at the
anchor's bottom-centre, +X = the default-orientation extends direction,
+Y = up. The engine rotates it together with `footprint` at place time.

```lua
register {
    id = "yourmod:bed",
    -- ...
    footprint = { {0, 0, 0}, {1, 0, 0} },
    entity_aabb = {
        min = { -0.5, 0.0, -0.5 },
        max = {  1.5, 0.5,  0.5 },  -- 2-long, half-height
    },
}
```

If you omit `entity_aabb`, the engine falls back to the cube union of
the footprint (the model is treated as filling every cell completely,
which is the right default for cube-shaped multi-block entities like a
crate stack but wrong for furniture). When in doubt, declare it.

## Materials

glTF carries PBR. In Blender's Principled BSDF / Blockbench's Material
panel, set:

- **Base color** (albedo) — solid color or texture.
- **Metallic** + **Roughness** — most furniture is `metallic = 0`,
  `roughness ≈ 0.7`.
- Optional: normal map, ambient occlusion, emissive.

For a solid-colour test mesh, no textures needed — just set the base
color factor in the material and the .glb embeds it inline.

## Export

Use **.glb** (binary glTF, single file).

**Blender:** `File → Export → glTF 2.0 (.glb/.gltf)` → format:
*glTF Binary (.glb)*. Recommended options:
- Include: Selected Objects (so junk in the scene doesn't ship).
- Transform: +Y Up (default).
- Geometry: Apply Modifiers, Normals, no UVs unless you have textures.

**Blockbench:** `File → Export → glTF Binary`.

Save to `mods/<your-mod>/models/<name>.glb`.

## Declaring the block

In your mod's `shared.lua`:

```lua
register {
    id = "yourmod:bed",
    display_name = "Bed",
    flags = {
        solid = true,
        support_below = true,
        placeable = true,
    },
    color = { 0.4, 0.2, 0.05 },          -- hotbar swatch colour
    mesh = "mods://yourmod/models/bed.glb",
}
```

The `mods://` URL resolves to your mod directory. Server ignores
`mesh` (rendering is client-only); `color` is the swatch shown in the
hotbar UI and doesn't affect the rendered mesh.

## Common gotchas

- **Origin not at floor:** model floats above or sinks below the cell
  floor. Fix: in Blender, select all geometry, `Object → Set Origin →
  Origin to 3D Cursor` with the cursor at the model's bottom-centre.
- **Wrong scale:** model is half-size or twice-size. Fix: a cell is
  exactly 1 unit. If your modeling app uses different units (Blender's
  default is metres but your scene scale might differ), check the
  export settings.
- **Wrong forward axis:** the model is sideways or backwards. Fix:
  re-check the Blender → Bevy axis remap above. The most common error
  is confusing Blender +Y with Bevy +Z (they're related by negation:
  Blender +Y = Bevy −Z).
- **Black model:** material is missing or has no light interaction.
  Fix: set the base color factor explicitly; ensure the Principled
  BSDF is connected to the material output.
