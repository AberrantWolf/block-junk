# KayKit Dungeon Remastered 1.1

Stone furniture + barrels/crates (S3, F1) and a large modular
building set (future). Shared texture `dungeon_texture.png`
(**imported 2026-07-06** with the S3 containers); 6 alt palettes exist
(`alt_texture_1_Golden` … `_6_NightB`). ~250 meshes total; only the
block-junk-relevant subset is curated here (see the pack `contents.png`
for the rest).

**SCALE — corrected 2026-07-09:** the **architecture** (walls 4×4 m,
floor tiles 4×4 m) and oversized props (barrel_large 2 m tall,
bookcases 3 m, pillar 4 m) are on the 2 m dungeon grid and need a 0.5
scale baked into the gltf root node. But **human-scale props are
already 1 m-grid** — chair (0.75×1.23), stool (0.5 h), table_small
(1×1×1) probe byte-for-byte the same sizes as Furniture Bits. Probe
before assuming either way; the earlier "every import needs 0.5" note
was wrong (barrel/crates just happened to be oversized props).

S3 imports (✅): `barrel_large`✅ (raw 1.8×2.0×1.8 → 0.9×1.0×0.9 at
0.5; the `vanilla:barrel` block) and `crates_stacked`✅ (raw
2.1×2.14×2.25 → ~1.05×1.07×1.12; the `vanilla:crate` block — slight
cell overhang, intentional, anvil precedent). Probed but skipped:
`crate_large` (raw 2.0×0.8×1.4 — too squat to read as bulk storage),
`barrel_small` (raw 1.0×1.0×1.0).

## Curated subset

**Stone seating / tables** (F1) — `bench`✅ (1.75×0.5×0.75 raw,
imported UNSCALED, +0.5 x shift, 2×1 footprint), `chair`, `stool`,
`stool_round`, `table_small`(`_decorated_A/B/C`),
`table_medium`(`_decorated_A/B`, `_tablecloth`),
`table_long`(`_decorated_A/B/C`, `_tablecloth`),
`table_round_{small,medium,large}`. `column`✅ (0.7×1.4 raw, y-stretch
×1.43 → 0.7×2.0, 1×2-tall footprint), `pillar`✅ (1.5×4.0 raw, 0.5
scale → 0.75×2.0, 1×2-tall footprint), `pillar_decorated` (2
stone_brick each, per settlement plan).

**Containers** (S3) — `barrel_small`, `barrel_large`,
`barrel_large_decorated`, `barrel_small_stack`, `box_small`(`_decorated`),
`box_large`, `box_stacked`, `crate_small`, `crate_large`(`_decorated`),
`crates_stacked`, `chest`, `chest_large`, `chest_gold`,
`chest_large_gold`, `keg`, `keg_decorated`, `bucket`, `bucket_pickaxes`,
`trunk_{small,medium,large}_A/B/C`.

**Shelving / books** — `bookcase_single`✅ and `bookcase_double`✅
(2×3×0.5 / 4×3×0.5 raw, 0.5 scale → 1 and 2 wide × 1.5 tall × 0.25
deep, on 1×2-tall / 2×2 footprints; the F1 "shelf" blocks — Furniture
Bits shelves are wall-mount and we have no mount mechanics),
`bookcase_*_decoratedA/B`, `shelf_small`(`_books`,`_candles`),
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
