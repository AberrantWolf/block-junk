# block-junk design reference

Durable, committed design notes — the counterpart to the per-session
`memory/` store. Anything that stays true across sessions and is worth
finding again lives here.

## Contents

- **[meshes/](meshes/)** — the 3D asset catalog: what's imported, where
  each mesh is used, what the source KayKit packs offer, the import
  pipeline, and the character rig's animation clips. Start at
  [meshes/README.md](meshes/README.md).

## Conventions

- Source art packs live **outside the repo** at
  `~/Desktop/The Complete KayKit Collection v5/` (KayKit) and
  `~/Desktop/Kenney Game Assets All-in-1 3.5.0/` (Kenney). Only the
  meshes we actually import are committed under `mods/vanilla/models/`.
- Modeling / coordinate-frame rules for authoring or correcting a mesh
  live in [`../mods/MODELING.md`](../mods/MODELING.md); the mesh catalog
  here does not duplicate them, it links to them.
