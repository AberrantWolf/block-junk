# KayKit Furniture Bits 1.0

Wood furniture for **F1**. Shared texture `furniturebits_texture.png`
(imported; alt palettes `_alt_A/B/C` exist). Multi-cell pieces need a
frame-correction pass on import (see meshes/README → Coordinate frame).

**F1 imports (2026-07-09)** — all with TRS baked into the gltf root
node so the mesh sits on its footprint box (bed_single_A precedent):

| mesh | raw size | bake | footprint |
|---|---|---|---|
| `chair_A/B/C`✅ | 0.75×1.26×0.85 | rot +90°Y (face +X) | 1 |
| `chair_stool`✅ | 0.75×0.50 | none | 1 |
| `table_small`✅ | 1×1×1 | none | 1 |
| `table_medium`✅ | 2×1×2 | +0.5 x/z | 2×2 |
| `table_medium_long`✅ | 3×1×2 | +1.0 x, +0.5 z | 3×2 |
| `table_low`✅ | 2.4×0.5×1.5 | ×5/6, +0.5 x | 2×1 |
| `cabinet_small`✅ | 1×1×1 | none | 1 |
| `cabinet_medium`✅ | 2×1×1 | +0.5 x | 2×1 |
| `desk`✅ | 3×1×1.5 | ×0.75, +0.5 x | 2×1 |
| `book_set`✅ | 0.78×0.5×0.37 | +0.25 y (origin was mid) | 1 |
| `bed_single_A`✅ `bed_single_B`✅ | 1.6×1×3 | ×(0.625,1,⅔), rot −90°Y, +0.5 x | 2×1 |
| `bed_double_A`✅ | 3.1×1×3 | ×(0.645,1,⅔), rot −90°Y, +0.5 x/z | 2×2 |
| `rug_rectangle_A`✅ `rug_oval_A`✅ | 3×0.1×2 | ×(⅔,1,0.5) → exact 2×1 | 2×1 |

`shelf_A_*`/`shelf_B_*` probed and **rejected**: they're wall-mount
brackets (0.4 m tall, origin at the mount plane) and we have no
wall-mount mechanics — DR bookcases serve as the F1 shelf blocks.

## Fantasy-appropriate subset (what we'd use)

**Seating** (tag `vanilla:seat`, A2) — `chair_A`, `chair_A_wood`,
`chair_B`, `chair_B_wood`, `chair_C`, `chair_stool`, `chair_stool_wood`,
`chair_desk_A`, `chair_desk_B`, `armchair`, `armchair_pillows`, `couch`,
`couch_pillows`.

**Tables** — `table_small`, `table_medium`, `table_medium_long`,
`table_low`, `table_low_decorated`.

**Storage furniture** — `cabinet_small`(`_decorated`),
`cabinet_medium`(`_decorated`), `shelf_A_small`, `shelf_A_big`,
`shelf_B_small`(`_decorated`), `shelf_B_large`(`_decorated`).

**Beds** — `bed_single_A`✅, `bed_single_B`, `bed_double_A`,
`bed_double_B` (2-wide; foot-anchored footprint like the current bed).

**Desks / decor** — `desk`(`_decorated`), `desk_large`(`_decorated`),
`book_set`, `book_single`, `pillow_A`, `pillow_B`.

**Rugs** (flat, `nav_passable` decor) — `rug_oval_A/B`,
`rug_rectangle_A/B`, `rug_rectangle_stripes_A/B`.

## Skip

Modern/office props don't fit the setting and several have no supporting
system: `monitor`, `keyboard`, `mouse`, `mousepad_*`, `gameconsole_handheld`,
`cup_pencils`, `mug_*`, and all `lamp_*` (no light system —
per storage-arc, lamps are deferred). `cactus_*` and `pictureframe_*`
(no wall-mount mechanics) are optional decor at best.

## Room-typing hooks (F1)

Seats tagged `vanilla:seat`; new room types **dining** (table + ≥2 seats)
and **study** (desk + shelf) join the existing bedroom/workshop patterns.
Suggested costs: chairs 2 planks, tables 3–4, cabinet/shelf 4, beds
4 planks + a future textile.
