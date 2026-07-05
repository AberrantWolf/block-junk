# Characters & animation (KayKit Adventurers + Character Animations)

All NPC bodies ride the shared **Rig_Medium** skeleton, so every
registered clip retargets onto every body. Bodies and clip libraries are
separate `.glb`s.

## Bodies (imported, `mods/vanilla/models/characters/`)

`Knight.glb`, `Ranger.glb`, `Druid.glb`, `Engineer.glb` — mesh-only
(**zero embedded animations**; e.g. Knight has 33 nodes, meshes like
`Knight_Body`/`Knight_Head`/`Knight_Helmet`, 0 anims). Assigned to
`vanilla:wanderer` round-robin by NPC id (`models[(id-1) % len]`), so the
first four villagers differ on every machine. Textures embedded;
alt-palette variants exist in the pack for a future skin/palette swap.

### Attachment sockets (A1, done)

The rig exposes `handslot.r` (right hand) and `handslot.l` (left) nodes —
verified present in all four body glbs. `client.rs::setup_npc_skeleton_anim`
caches them on `NpcVisuals` (via `find_named_descendant` after the scene is
ready); `update_npc_held_tools` childs the `EquippedTool` mesh to `handslot.r`
and `update_npc_held_carry` childs the `Carrying` mesh to `handslot.l`
(the old floating cube is now just a fallback when a socket is missing).
Attach a `WorldAssetRoot` child straight to the socket — its own
`WorldInstanceReady` early-returns in the skeleton pass (no `NpcWorldAssetRoot`
marker), so no interference; a slot change despawns + respawns the child.

Meshes don't all fit a hand at identity — KayKit tools do (axe ~1.05 m),
but resource meshes are world-scale (log 1.35 m, plank 1.5 m). Tune per
item via `ItemDef.hold_transform { pos, rot_deg, scale }` in `data.lua`
(default identity); measure a mesh's bounds from its glTF POSITION
accessor `min`/`max`. Same knob for future bush-fruit or any hand prop.

## Animation clip libraries (imported)

KayKit splits clips across four themed rigs. `data.lua`
`engine.animations.register` binds an id → `(asset, clip_index)`.
**Indices are position-dependent — re-probe if the pack revs.**

### Registered today

| id | asset | index | clip |
|----|-------|-------|------|
| `vanilla:idle` | `Rig_Medium_General.glb` | 6 | `Idle_A` |
| `vanilla:walk` | `Rig_Medium_MovementBasic.glb` | 8 | `Walking_A` |
| `vanilla:lie_idle` | `Rig_Medium_Simulation.glb` | 2 | `Lie_Idle` |
| `vanilla:work` | `Rig_Medium_Tools.glb` | 26 | `Working_A` |
| `vanilla:chopping` | `Rig_Medium_Tools.glb` | 1 | `Chopping` (axe) |
| `vanilla:hammering` | `Rig_Medium_Tools.glb` | 12 | `Hammering` (hammer) |
| `vanilla:pickaxing` | `Rig_Medium_Tools.glb` | 19 | `Pickaxing` (pickaxe) |
| `vanilla:sawing` | `Rig_Medium_Tools.glb` | 21 | `Sawing` |

Work clip is picked by the carrier's `EquippedTool` (`ItemDef.work_animation`),
falling back to `vanilla:work`. `Sawing` is registered but unused (no saw item yet).

### Full clip list per library (0-indexed, for future `clip_index`)

**Rig_Medium_General.glb** — `Death_A`, `Death_A_Pose`, `Death_B`,
`Death_B_Pose`, `Hit_A`, `Hit_B`, `Idle_A`, `Idle_B`, `Interact`,
`PickUp`, `Spawn_Air`, `Spawn_Ground`, `T-Pose`, `Throw`, `Use_Item`.

**Rig_Medium_MovementBasic.glb** — `Jump_Full_Long`, `Jump_Full_Short`,
`Jump_Idle`, `Jump_Land`, `Jump_Start`, `Running_A`, `Running_B`,
`T-Pose`, `Walking_A`, `Walking_B`, `Walking_C`.

**Rig_Medium_Simulation.glb** — `Cheering`, `Lie_Down`, `Lie_Idle`,
`Lie_StandUp`, `Push_Ups`, `Sit_Chair_Down`, `Sit_Chair_Idle`,
`Sit_Chair_StandUp`, `Sit_Floor_Down`, `Sit_Floor_Idle`,
`Sit_Floor_StandUp`, `Sit_Ups`, `T-Pose`, `Waving`.

**Rig_Medium_Tools.glb** — `Chop`, `Chopping`, `Dig`, `Digging`,
`Fishing_Bite`, `Fishing_Cast`, `Fishing_Catch`, `Fishing_Idle`,
`Fishing_Reeling`, `Fishing_Struggling`, `Fishing_Tug`, `Hammer`,
`Hammering`, `Holding_A`, `Holding_B`, `Holding_C`, `Lockpick`,
`Lockpicking`, `Pickaxe`, `Pickaxing`, `Saw`, `Sawing`, `T-Pose`,
`Work_A`, `Work_B`, `Work_C`, `Working_A`, `Working_B`, `Working_C`.

(`Verb` vs `Verbing`: the `_ing` clips are looping cycles; the bare verb
is a single swing. Use `_ing` for sustained work, bare for one-shots.)

## Earmarked for planned phases

- **A1 held items + tool anims** — DONE (tool mesh in hand + `Chopping`/
  `Pickaxing`/`Hammering`/`Sawing` per equipped tool). Still available if
  wanted: General `PickUp` as a one-shot on haul pickup (needs one-shot,
  not loop, semantics on the anim override — deferred); `Holding_A/B/C`
  for a nicer idle-carry pose; `Digging` for a future shovel.
- **A2 rest seating** — Simulation `Sit_Chair_{Down,Idle,StandUp}` (with
  a seat use-slot) and `Sit_Floor_*` for ground rest.
- **Future** — `Waving`/`Cheering` social; `Interact`/`Use_Item` for
  container/station use; `Running_*` when NPCs hurry; `Fishing_*` if
  fishing lands; `Hit`/`Death` for any combat/health.
