# KayKit Furniture Bits 1.0

Wood furniture for **F1**. Shared texture `furniturebits_texture.png`
(imported; alt palettes `_alt_A/B/C` exist). Only `bed_single_A` is
imported so far. Multi-cell pieces need a frame-correction pass on
import (see meshes/README → Coordinate frame).

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
