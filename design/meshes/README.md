# Mesh catalog

What art we have, what we can pull in, and how. Split by source pack so
each file stays short. Update the **Imported + usage** table below
whenever a `.gltf` lands in or leaves `mods/vanilla/models/`.

## Per-pack catalogs

| File | Source pack | Shared texture | Imported yet? | Used for |
|------|-------------|----------------|---------------|----------|
| [resource_bits.md](resource_bits.md) | KayKit Resource Bits 1.0 | `resource_bits_texture.png` | ✅ (texture + some meshes) | items, **piles (S2)**, containers (S3), food (S4) |
| [furniture_bits.md](furniture_bits.md) | KayKit Furniture Bits 1.0 | `furniturebits_texture.png` | ✅ texture; only `bed_single_A` | wood furniture (F1) |
| [dungeon_remastered.md](dungeon_remastered.md) | KayKit Dungeon Remastered 1.1 | `dungeon_texture.png` | ❌ | stone furniture, barrels/crates (S3/F1), modular building (future) |
| [forest_nature.md](forest_nature.md) | KayKit Forest Nature Pack 1.0 | `forest_texture.png` | ❌ | grass only — **no trees/bushes** (see file: S4 needs another source) |
| [rpg_tools.md](rpg_tools.md) | KayKit RPG Tools Bits 1.0 | `tools_bits_texture.png` | ✅ (texture + axe/hammer/pickaxe/anvil) | tools, work props |
| [characters.md](characters.md) | KayKit Adventurers + Character Animations | embedded per-glb | ✅ | NPC bodies + rig animation clips |

Two meshes come from packs not otherwise used: `Toy_Workbench` (KayKit
Mystery Monthly) and — for reference — nothing else. It shares no atlas;
its texture is embedded.

## Import pipeline (verified 2026-07-05)

The KayKit packs ship a `.gltf` variant that drops straight in — **no
conversion, no re-export.** Confirmed byte-identical: the repo's
`Wood_Log_B.gltf` == the pack's `Assets/gltf/Wood_Log_B.gltf`.

To import a mesh `<Name>` from pack `<Pack>`:

1. Copy `<Pack>/Assets/gltf/<Name>.gltf` **and** `<Name>.bin` into
   `mods/vanilla/models/`.
2. Ensure the pack's shared texture PNG is present there once (it is for
   resource/tools/furniture; Dungeon + Forest need their `*_texture.png`
   copied in the first time you import from them). The `.gltf` references
   it by bare relative filename (`"uri": "resource_bits_texture.png"`),
   as it does the `.bin`.
3. Reference it from `data.lua` as
   `mesh = "mods://vanilla/models/<Name>.gltf"`. The client loads it as
   `"{mesh}#Scene0"` — each KayKit file is one scene / one mesh, so
   `#Scene0` is the whole part. Resolution path:
   `ItemSlot → ItemRegistry::def().mesh → asset_server.load("{mesh}#Scene0")`
   in `attach_world_item_visuals` (`client.rs`), and the analogous block-
   entity path in `client_chunks.rs`.

Source packs (outside repo):
`~/Desktop/The Complete KayKit Collection v5/<Pack>/Assets/gltf/`.

## Coordinate frame

KayKit local frames don't match block-junk's ("+X = extends direction,
origin at the cell's bottom-centre"). Small props (loose items, the
berry basket) happen to fit the default rule with no correction.
Multi-cell furniture (`bed_single_A`, `Toy_Workbench`, `anvil`) bake a
node-level transform (rotation + uniform scale + translation) into the
`.gltf` to correct frame + fit the cell footprint — see the heavily-
commented `register{}` blocks in `mods/vanilla/data.lua` and the rules
in [`../../mods/MODELING.md`](../../mods/MODELING.md). Budget a framing
pass when importing any multi-cell furniture.

## Imported + usage (everything in `mods/vanilla/models/`)

| File | Source pack | Registered as | Where |
|------|-------------|---------------|-------|
| `Wood_Log_B.gltf` | Resource Bits | item `vanilla:wood_log` | data.lua |
| `Stone_Chunks_Small.gltf` | Resource Bits | item `vanilla:stone_chunk` | data.lua |
| `Wood_Planks_Stack_Small.gltf` | Resource Bits | item `vanilla:wood_planks` | data.lua |
| `Stone_Bricks_Stack_Small.gltf` | Resource Bits | item `vanilla:stone_brick` | data.lua |
| `axe.gltf` / `hammer.gltf` / `pickaxe.gltf` | RPG Tools Bits | items `vanilla:axe` / `:hammer` / `:pickaxe` | data.lua |
| `anvil.gltf` | RPG Tools Bits | block `vanilla:anvil` (smithing station) | data.lua |
| `Toy_Workbench.gltf` | Mystery Monthly | block `vanilla:workbench` (carpentry) | data.lua |
| `berry_basket.gltf` | Resource Bits (`Food_Basket_A_Berries`) | block `vanilla:berry_basket` | data.lua |
| `bed_single_A.gltf` | Furniture Bits | block `vanilla:bed` | data.lua |
| `characters/Knight,Ranger,Druid,Engineer.glb` | Adventurers | `vanilla:wanderer` body variants | data.lua |
| `characters/Rig_Medium_{General,MovementBasic,Simulation,Tools}.glb` | Character Animations | animation clip libraries | data.lua `engine.animations.register` |
| textures: `resource_bits_texture.png`, `tools_bits_texture.png`, `furniturebits_texture.png`, `helpers_texture.png` | — | shared by the above | — |

Not-yet-imported textures we'll need: `dungeon_texture.png` (Dungeon
Remastered), `forest_texture.png` (Forest Nature).
