# KayKit Dungeon Remastered 1.1

Stone furniture + barrels/crates (S3, F1) and a large modular
building set (future). Shared texture `dungeon_texture.png`
(**imported 2026-07-06** with the S3 containers); 6 alt palettes exist
(`alt_texture_1_Golden` … `_6_NightB`). ~250 meshes total; only the
block-junk-relevant subset is curated here (see the pack `contents.png`
for the rest).

**SCALE WARNING:** this pack is authored on a **2 m dungeon grid** —
twice our 1 m cell. Every import needs a 0.5 uniform scale baked into
the gltf root node (`"scale": [0.5, 0.5, 0.5]`), the same
edit-the-node trick the workbench used. Raw probe bounds below are
PRE-scale; halve them for in-game size.

S3 imports (✅): `barrel_large`✅ (raw 1.8×2.0×1.8 → 0.9×1.0×0.9 at
0.5; the `vanilla:barrel` block) and `crates_stacked`✅ (raw
2.1×2.14×2.25 → ~1.05×1.07×1.12; the `vanilla:crate` block — slight
cell overhang, intentional, anvil precedent). Probed but skipped:
`crate_large` (raw 2.0×0.8×1.4 — too squat to read as bulk storage),
`barrel_small` (raw 1.0×1.0×1.0).

## Curated subset

**Stone seating / tables** (F1) — `bench`, `chair`, `stool`,
`stool_round`, `table_small`(`_decorated_A/B/C`),
`table_medium`(`_decorated_A/B`, `_tablecloth`),
`table_long`(`_decorated_A/B/C`, `_tablecloth`),
`table_round_{small,medium,large}`. `column`, `pillar`,
`pillar_decorated` (2 stone_brick each, per settlement plan).

**Containers** (S3) — `barrel_small`, `barrel_large`,
`barrel_large_decorated`, `barrel_small_stack`, `box_small`(`_decorated`),
`box_large`, `box_stacked`, `crate_small`, `crate_large`(`_decorated`),
`crates_stacked`, `chest`, `chest_large`, `chest_gold`,
`chest_large_gold`, `keg`, `keg_decorated`, `bucket`, `bucket_pickaxes`,
`trunk_{small,medium,large}_A/B/C`.

**Shelving / books** — `bookcase_single`(`_decoratedA/B`),
`bookcase_double`(`_decoratedA/B`), `shelf_small`(`_books`,`_candles`),
`shelf_large`, `shelves`(`_decorated`), `book_{brown,grey,tan}`.

**Beds** — `bed_A_single`, `bed_A_double`, `bed_A_stacked`,
`bed_B_single`, `bed_B_double`, `bed_decorated`, `bed_floor`, `bed_frame`.

**Decor** (no supporting systems yet) — `banner_*` (many colors/patterns),
`torch`, `torch_lit`, `torch_mounted` (no light system — decor only),
`candle*`, `rocks`, `rubble_*`, `plate*`, `bottle_*`.

## Modular building set (future — big)

Full walls (`wall`, `wall_corner`, `wall_doorway`, `wall_window_*`,
`wall_arched*`, `wall_half`, `wall_Tsplit`, …), floors (`floor_tile_*`,
`floor_wood_*`, `floor_dirt_*`, `floor_foundation_*`), `stairs*`,
`scaffold_*`, `bar_*` railings. A whole modular-dungeon construction
kit if block-junk ever wants prefab stone architecture beyond the 1m
voxel grid. Not needed now — noted so we remember it's here.
