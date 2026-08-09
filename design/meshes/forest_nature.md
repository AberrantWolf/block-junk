# KayKit Forest Nature Pack 1.0

Shared texture `forest_texture.png` (not imported). **Contents are far
thinner than the name suggests.**

## Everything in the pack

- `Grass_1_Mesh`, `Grass_1_SingleSided_Mesh`, `Grass_2_Mesh`,
  `Grass_2_SingleSided_Mesh` — grass tufts (double- and single-sided).
- `Color1` … `Color8` — material/palette swatch meshes.

That's it. **No trees, no bushes, no rocks, no flowers.**

## ⚠️ Correction to the storage-arc plan (S4)

The storage-arc memory assumes `vanilla:berry_bush` and apple-bearing
fruit trees come from "Forest Nature Pack." **They don't** — this pack is
grass only. S4's foraging terrain needs a different source. Options:

1. **Block-stamped trees (status quo):** trees are already `vanilla:wood`
   + `vanilla:leaves` blocks stamped on terrain. A `vanilla:berry_bush`
   could be a new block with a small custom mesh, and fruit could be a
   `leaves_apple` block variant — no new pack required. Cheapest path,
   fits the existing terrain-gen.
2. **Kenney Nature Kit** (`~/Desktop/Kenney Game Assets All-in-1 3.6.0/
   3D assets/Nature Kit/Models/GLTF format/`): has real `tree_*`,
   `plant_bush*`, `log_stack*`, `rock_*` `.glb`s (different art style —
   flatter/lower-poly than KayKit; check it reads OK next to KayKit).
3. Source a dedicated KayKit foliage pack if one exists in the collection
   (Halloween/Holiday packs have some foliage — check before S4).

**Resolved (S4 build, 2026-07-07):** went with **option 1 + a staged
option 2**. `vanilla:berry_bush` / `vanilla:bare_berry_bush` ship as
solid **textured cube blocks** (green `vanilla:leaves` ↔ brown
`vanilla:wood_bark`) so the terrain mesher renders them with zero extra
plumbing — terrain-generated *mesh* blocks would need sidecar-anchor
population the cube path skips. Kenney Nature Kit `plant_bushDetailed` /
`plant_bushSmall` are imported as `berry_bush.glb` / `berry_bush_bare.glb`
(self-contained, flat-material, ~0.36 m and ~0.21 m tall) and staged in
`mods/vanilla/models/` for a later one-line `mesh =` swap once the
terrain mesh-entity plumbing lands. Art-style clash (Kenney is flatter
than KayKit) is why they're staged, not wired — revisit when polishing.
