# KayKit Resource Bits 1.0

The workhorse pack for items, piles, containers, and food. Shared
texture `resource_bits_texture.png` (imported). `✅` = already in
`mods/vanilla/models/`.

## Catalog (by category)

Names are the pack's mesh base names; each exists as `<Name>.gltf` +
`<Name>.bin` in the pack's `Assets/gltf/`.

**Wood** — `Wood_Log_A`, `Wood_Log_B`✅, `Wood_Log_Stack`, `Wood_Plank_A`,
`Wood_Plank_B`, `Wood_Plank_C`, `Wood_Planks_Stack_Small`✅,
`Wood_Planks_Stack_Medium`, `Wood_Planks_Stack_Large`.

**Stone** — `Stone_Brick`, `Stone_Bricks_Stack_Small`✅,
`Stone_Bricks_Stack_Medium`, `Stone_Bricks_Stack_Large`,
`Stone_Chunks_Small`✅, `Stone_Chunks_Large`.

**Containers** (S3) — `Containers_Box_{Small,Medium,Large✅,Large_Dirty}`,
`Containers_Crate_{Small_Green,Small_Grey,Medium_Wood,Medium_Grey,Medium_Tan,Large}`,
`Containers_Pile_{Small,Medium,Large}`✅ (generic loose-heap fallback).
S3 shipped 2026-07-06: `Containers_Box_Large` = the `vanilla:box` block
(native scale, bounds ±0.38 × 0.795 tall). Barrel + crate came from
Dungeon Remastered instead (see that file).

**Food** (S4) — `Food_Basket_A_Berries`✅ (imported as `berry_basket.gltf`),
`Food_Basket_A_Empty`✅ (staged for the deferred empty/full basket swap,
not yet wired), `Food_Basket_B_{Berries,Empty}`,
`Food_Barrel_{Empty,Fish}`, `Food_Berry_Blue`✅ (the `vanilla:berry`
item), `Food_Berry_Orange`, `Food_Apple_{Red,Green}` (apples deferred),
`Food_Cheese`, `Food_Flour`,
`Food_Crate_{Small_Berries,Small_Empty,Large_Apples,Large_Empty}`,
`Food_Pile_{Small,Medium,Large}`✅ (berry pile tiers).

**Metals** (future minerals/bronze arc) — for each of Copper / Iron /
Silver / Gold: `_Bar`, `_Bars`, `_Nugget_{Small,Medium,Large}`,
`_Nuggets`, `_Bars_Stack_{Small,Medium,Large}`.

**Money / Gems / Textiles / Fuel / Parts / Pallets** — present but out of
scope; see the pack `contents.png` if ever needed. `Parts_Pile_*` and
`Gems_Pile_*` are extra generic-heap options.

## Planned pile-tier mapping (S2)

Design rule (locked with user 2026-07-05): **a single unit looks like a
single item; count ≥ 2 switches to a stack tier.** Thresholds, capacity,
and whether an item piles at all are **Lua-configurable** per item
(`ItemDef.pile`), not hardcoded. `bulk` is a separate weight used for
pile capacity and (later) container fill. Global default
`PILE_CAPACITY_BULK = 12` → `capacity_units = floor(12 / bulk)` unless
overridden.

| Item | bulk | cap (units) | count 1 (base `mesh`) | tier meshes (min count → mesh) |
|------|------|-------------|-----------------------|--------------------------------|
| `vanilla:wood_log` | 3 | 4 | `Wood_Log_B` ✅ | 2 → `Wood_Log_Stack` |
| `vanilla:wood_planks` | 2 | 6 | `Wood_Plank_A` *(new base — was Stack_Small)* | 2 → `Wood_Planks_Stack_Small`✅, 4 → `_Medium`, 6 → `_Large` |
| `vanilla:stone_chunk` | 2 | 6 | `Stone_Chunks_Small` ✅ | 3 → `Stone_Chunks_Large` |
| `vanilla:stone_brick` | 2 | 6 | `Stone_Brick` *(new base — was Stack_Small)* | 2 → `Stone_Bricks_Stack_Small`✅, 4 → `_Medium`, 6 → `_Large` |
| `vanilla:axe`/`:hammer`/`:pickaxe` | 6 | 2 | own mesh ✅ | *(TBD: non-piling, or 2 → `Containers_Pile_Small`)* |

Changing planks/bricks base to the single-item mesh means a lone dropped
board/brick reads as one, and only a tidied stack shows the KayKit stack
mesh — the intended "single looks single" behavior.

### Imports needed for S2 (copy `.gltf`+`.bin` from `Assets/gltf/`)

`Wood_Log_Stack`, `Wood_Plank_A`, `Wood_Planks_Stack_Medium`,
`Wood_Planks_Stack_Large`, `Stone_Chunks_Large`, `Stone_Brick`,
`Stone_Bricks_Stack_Medium`, `Stone_Bricks_Stack_Large`,
`Containers_Pile_{Small,Medium,Large}` (generic fallback).

Texture already present, so these are pure file copies.
