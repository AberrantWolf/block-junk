-- Vanilla content registers from data.lua so both the server and the
-- client end up with the same set of blocks (and the same slot order).
--
-- Order matters: vanilla:empty MUST be the first registration so it lands
-- at slot 0. The engine bails on startup otherwise.

local function register(def)
    -- Defaults so each block table only has to list what's special about it.
    def.flags = def.flags or {}
    def.tags = def.tags or {}
    if def.flags.placeable == nil then def.flags.placeable = true end
    engine.blocks.register(def)
end

register {
    id = "vanilla:empty",
    display_name = "Air",
    flags = { placeable = false },
    color = { 0.0, 0.0, 0.0 },
}

-- Items dropped by destroyed blocks (Phase 1). Both meshes come from
-- KayKit Resource Bits and share the same `resource_bits_texture.png`
-- already in this directory. Registration order doesn't matter for
-- items the way it does for blocks (no slot-0 reservation).
engine.items.register {
    id = "vanilla:wood_log",
    display_name = "Wood Log",
    mesh = "mods://vanilla/models/Wood_Log_B.gltf",
    color = { 0.55, 0.40, 0.22 },
}

engine.items.register {
    id = "vanilla:stone_chunk",
    display_name = "Stone Chunk",
    mesh = "mods://vanilla/models/Stone_Chunks_Small.gltf",
    color = { 0.55, 0.55, 0.58 },
}

-- Tools (Phase 5a). Each tool's `tool_tags` is the engine-side handle
-- a block's `work_action.required_tool` matches against. Meshes are
-- placeholders today — the wood-log mesh stands in for all three so
-- the gameplay loop is testable now; proper KayKit RPG Tools Bits
-- meshes can replace the `mesh` paths without touching anything else.
-- Distinct colors keep them visually separable in the world + HUD.
engine.items.register {
    id = "vanilla:axe",
    display_name = "Axe",
    mesh = "mods://vanilla/models/Wood_Log_B.gltf",
    color = { 0.78, 0.45, 0.18 },
    tool_tags = { "vanilla:axe" },
}

engine.items.register {
    id = "vanilla:hammer",
    display_name = "Hammer",
    mesh = "mods://vanilla/models/Wood_Log_B.gltf",
    color = { 0.45, 0.30, 0.20 },
    tool_tags = { "vanilla:hammer" },
}

engine.items.register {
    id = "vanilla:pickaxe",
    display_name = "Pickaxe",
    mesh = "mods://vanilla/models/Wood_Log_B.gltf",
    color = { 0.38, 0.40, 0.45 },
    tool_tags = { "vanilla:pickaxe" },
}

-- Masks + ramps composited per-block by the chunk fragment shader.
-- Slot order is registration order; refs from `layers` below resolve
-- against these by id at boot. Mods can register their own with
-- `engine.masks.register` / `engine.ramps.register` and reference them
-- from their own blocks; the atlas grows accordingly.

engine.masks.register {
    id = "vanilla:bubbles_large",
    -- Worley with 4 cells per tile = a handful of fat blobs. Good for
    -- grass-rocks scale at scale = 2.0 in world space.
    source = { kind = "worley", cells = 4 },
}

engine.masks.register {
    id = "vanilla:bubbles_small",
    -- Same algorithm at 2x cell count: many smaller blobs. Good for
    -- moss-on-stone speckle at scale = 1.0.
    source = { kind = "worley", cells = 8 },
}

engine.ramps.register {
    id = "vanilla:stone_grey",
    -- Mid-grey → slightly lighter cool grey. Reads as rock-blob
    -- highlights when used over green grass.
    stops = {
        { 0.32, 0.32, 0.34 },
        { 0.58, 0.58, 0.60 },
    },
}

engine.ramps.register {
    id = "vanilla:grass_green",
    -- Dark olive → brighter grass green. Drives the moss-on-stone
    -- effect when stone uses it under bubbles_small.
    stops = {
        { 0.18, 0.32, 0.10 },
        { 0.45, 0.65, 0.20 },
    },
}

register {
    id = "vanilla:stone",
    display_name = "Stone",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.55, 0.55, 0.58 },
    pattern = "speckle",
    -- Green moss patches scattered across stone. High threshold so most
    -- pixels stay grey; soft edge to read as moss spreading.
    layers = {
        {
            mask = "vanilla:bubbles_small",
            ramp = "vanilla:grass_green",
            scale = 1.0,
            threshold = 0.70,
            softness = 0.20,
        },
    },
    drops = {
        { item = "vanilla:stone_chunk", count = 1 },
    },
    materials = {
        { item = "vanilla:stone_chunk", count = 1 },
    },
    -- Phase 5a: stone needs a pickaxe to work (chop or build). Engine
    -- defaults supply duration_secs + need_restore; we override only
    -- the tool gate.
    work_action = {
        duration_secs = 4.0,
        need_restore = { need = "work", restores = 0.1 },
        required_tool = "vanilla:pickaxe",
    },
}

register {
    id = "vanilla:dirt",
    display_name = "Dirt",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.45, 0.32, 0.20 },
    pattern = "noise",
}

register {
    id = "vanilla:grass",
    display_name = "Grass",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.36, 0.62, 0.30 },
    pattern = "noise",
    -- Grey rock blobs embedded in the grass. Low softness for crisp
    -- cartoon edges; scale = 2.0 so the blobs span ~2 world cells.
    layers = {
        {
            mask = "vanilla:bubbles_large",
            ramp = "vanilla:stone_grey",
            scale = 2.0,
            threshold = 0.62,
            softness = 0.05,
        },
    },
}

register {
    id = "vanilla:wood",
    display_name = "Wood",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.55, 0.40, 0.22 },
    pattern = "planks",
    -- Phase 1: destroying a wood block drops one Wood Log item.
    -- Trees in terrain are stamped from this same block, so chopping
    -- a tree trunk yields wood the same way breaking a placed wood
    -- block does.
    drops = {
        { item = "vanilla:wood_log", count = 1 },
    },
    -- Phase 3: building a wood block costs one Wood Log delivered to
    -- the plan cell. Symmetric with drops — chopping a wood block
    -- returns what it cost.
    materials = {
        { item = "vanilla:wood_log", count = 1 },
    },
    -- Phase 5a: wood needs an axe to chop (or build). Same pattern as
    -- stone — engine defaults for duration + need_restore, override
    -- only the tool gate.
    work_action = {
        duration_secs = 4.0,
        need_restore = { need = "work", restores = 0.1 },
        required_tool = "vanilla:axe",
    },
}

register {
    id = "vanilla:leaves",
    display_name = "Leaves",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.20, 0.50, 0.25 },
    pattern = "leaves",
}

-- Doors. `walkable_boundary` marks them as access points the room
-- detector needs at least one of for a region to count as a real room.
-- They still bound the flood-fill horizontally (`room_boundary`); future
-- NPC AI is what actually walks through them.
register {
    id = "vanilla:door",
    display_name = "Door",
    flags = {
        solid = true,
        room_boundary = true,
        walkable_boundary = true,
        support_below = true,
    },
    color = { 0.40, 0.18, 0.05 },
    pattern = "door",
}

-- KayKit "bed_single_A" mesh. The asset's local frame is the KayKit
-- convention (extends ±Z, centred at origin, ~1.6×1×3 m, pillow at
-- -Z), which doesn't match block-junk's "default-orientation extends
-- +X." The gltf carries a node-level transform that bakes in the
-- correction: a -90° Y rotation (KayKit -Z pillow → engine +X head),
-- an X-axis scale of 0.625 (so the 1.6 m width fits in a 1 m cell),
-- a Z-axis scale of 2/3 (so the 3 m length fits in 2 cells = 2 m),
-- and a +0.5 m X translation (centres the 2 m bed across the
-- anchor + head cells, pillow at the head cell's far edge). The engine's existing spawn path
-- (`SceneRoot` + `Transform::from_rotation_y(orientation.yaw())`)
-- handles place-time rotation against this baked-in default.
--
-- footprint = two cells east of the anchor (foot, head). The engine
-- rotates this together with the mesh at non-default orientations.
--
-- entity_aabb is in default-orientation model space (origin at the
-- anchor's bottom-centre, +X = extends direction, +Y = up) and tracks
-- the visible mesh after the gltf-level scale + translation. A tight
-- box matters for the place/break raycast: a click *above* the bed
-- shouldn't break it.
register {
    id = "vanilla:bed",
    display_name = "Bed",
    flags = {
        solid = true,
        support_below = true,
    },
    color = { 0.40, 0.18, 0.05 },
    mesh = "mods://vanilla/models/bed_single_A.gltf",
    footprint = { {0, 0, 0}, {1, 0, 0} },
    entity_aabb = {
        min = { -0.5, 0.0, -0.5 },
        max = {  1.5, 1.0,  0.5 },
    },
    -- The first exclusive interactable. `restores = 0.7` means a
    -- full sleep brings a tiredness deficit down by 70% — bedtime
    -- mostly resets the need but doesn't completely max it, so a
    -- long day still ends with the NPC visibly tired before bed.
    -- `duration_secs = 25` is long enough to read as "they're
    -- asleep" but short enough that a player watching the world
    -- doesn't lose interest before the cycle completes.
    -- `exclusive = true` is what makes a bed a bed rather than a
    -- berry basket: only one NPC claims it at a time, and the
    -- engine maintains the claim table.
    interactable = {
        need_restore = { need = "sleep", restores = 0.7 },
        duration_secs = 25.0,
        exclusive = true,
    },
    -- Snap-to-slot positioning for the sleep action. Without this the
    -- brain would try to derive "stand atop the foot cell + face the
    -- bed's extends axis" through the regular walk/collide pipeline,
    -- which is awkward (the lying animation pivots around the rig's
    -- origin, not whichever cell A* happened to land them on). With
    -- this, the engine pathfinds to one of `approach`, then on
    -- arrival snaps pose.translation to anchor + rotated(pose) and
    -- pose.yaw to orientation.yaw() + yaw, and marks the NPC
    -- kinematic for the sleep duration so the physics tick and
    -- soft-actor-separation pass don't budge them.
    --
    -- `pose` is in model space whose origin is the anchor cell's
    -- bottom-CENTRE (X/Z = cell centre, Y = cell floor). For a lying
    -- pose, the rig's "feet plane" (model origin) is the centre of
    -- the body's lying volume — not the mattress surface — because
    -- the KayKit lie clip pivots around the rig's hip plane, with
    -- the body's mass distributed roughly half above / half below.
    --
    -- `pose = (0.0, 0.5, 0.0)` places that pivot at the anchor
    -- cell's centre in X/Z and at half the bed's height in Y. With
    -- yaw = π/2 below, the lying body extends along the bed's +X
    -- extends axis (foot cell → head cell), centred on the bed's
    -- geometry. The engine adds the standing eye-offset to derive
    -- pose.translation, so authors don't have to think about
    -- Bevy's eye-vs-feet pose convention.
    --
    -- `yaw = π/2` orients the body along the bed's extends axis
    -- with the head at the +X (head-of-bed) end. The KayKit lying
    -- clip extends the body in the rig's +Z direction (opposite of
    -- standing-forward), so we face the NPC at -X = west, which
    -- then maps the rig's +Z to world +X.
    --
    -- `approach` covers the three cells around the foot end of the
    -- bed (West/North/South neighbours) plus the three around the
    -- head (East/North/South neighbours). NPCs that walk up from
    -- any side can reach a slot to start sleeping; the two cells the
    -- bed itself occupies are deliberately omitted (the validator
    -- would reject them anyway). The same cells are reused as
    -- ejection targets when the NPC wakes — they go back out the
    -- way they came in.
    use_slot = {
        pose = { 0.0, 0.5, 0.0 },
        yaw = 1.5707963,
        approach = {
            { -1, 0, 0 },   -- West of foot
            { 0, 0, -1 },   -- North of foot
            { 0, 0, 1 },    -- South of foot
            { 2, 0, 0 },    -- East of head
            { 1, 0, -1 },   -- North of head
            { 1, 0, 1 },    -- South of head
        },
        -- Body clip played while the NPC is locked to this slot.
        -- vanilla:lie_idle is the KayKit "Lie_Idle" — extends the
        -- body horizontally around the rig's origin, which the
        -- engine snap places on the mattress.
        animation = "vanilla:lie_idle",
    },
}

-- Seed room patterns. The detector isn't wired yet (next chunk of work);
-- these prove the registry's parent-chain and domain validation.

engine.rooms.register {
    id = "vanilla:enclosed_space",
    display_name = "Enclosed space",
    domain = "volumetric",
    constraints = {
        { kind = "floor_area", min = 4, max = 4096 },
        -- enclosure_height counts the layers above the floor where the
        -- walls actually extend. Min 1 = at least the floor layer is
        -- bounded by walls. A 1-high wall ring satisfies this; a row of
        -- isolated blocks does not (no perimeter coverage).
        { kind = "enclosure_height", min = 1 },
        -- Every room needs an explicit access point. Without this,
        -- accidental enclosures (a divot in the terrain, a hole the
        -- player dug for fun) would all register as rooms. Children
        -- inherit this so walled_yard and small_house both require it.
        { kind = "door_count", min = 1 },
    },
}

engine.rooms.register {
    id = "vanilla:walled_yard",
    display_name = "Walled yard",
    parent = "vanilla:enclosed_space",
    domain = "volumetric",
    constraints = {
        { kind = "has_roof", required = false },
        { kind = "floor_fraction", surface = "solid", min = 0.6 },
    },
}

engine.rooms.register {
    id = "vanilla:small_house",
    display_name = "Small house",
    parent = "vanilla:enclosed_space",
    domain = "volumetric",
    priority = 1,
    constraints = {
        { kind = "has_roof", required = true },
        { kind = "enclosure_height", min = 2 },
        { kind = "floor_area", max = 50 },
        { kind = "floor_fraction", surface = "solid", min = 0.8 },
    },
}

-- A berry basket — the first non-exclusive interactable. Block-
-- entity like the bed (1-cell footprint, custom mesh), and tagged
-- with `interactable` metadata so the engine indexes it for NPC
-- snapshots. The mesh's local frame puts the basket centred at the
-- anchor's bottom (Y in [0, 0.67], X/Z in ±0.43), so the default
-- bottom-centre modelling rule fits without translation.
--
-- `restores = 0.4` ⇒ one interaction brings a hunger deficit down by
-- 0.4 (40% of the 0..1 scale). With decay 1/300 ⇒ ~2 minutes of decay
-- before the same need climbs back. `duration_secs = 2.0` is enough
-- visible standstill for a player to see "the NPC stopped to eat";
-- shorter looks like a teleport, longer feels ritualistic.
--
-- No `use_slot` ⇒ NPCs walk to any standable neighbour and eat
-- from there, supporting use-from-any-angle. `exclusive = false`
-- ⇒ a queue of hungry villagers can pull from the same basket
-- from different sides at the same time.
register {
    id = "vanilla:berry_basket",
    display_name = "Berry basket",
    flags = {
        solid = true,
        support_below = true,
    },
    color = { 0.6, 0.25, 0.35 },
    mesh = "mods://vanilla/models/berry_basket.gltf",
    entity_aabb = {
        min = { -0.43, 0.0, -0.43 },
        max = {  0.43, 0.67, 0.43 },
    },
    interactable = {
        need_restore = { need = "hunger", restores = 0.4 },
        duration_secs = 2.0,
        exclusive = false,
    },
}

-- Needs the engine itself doesn't know about — it just decays whatever
-- needs are registered at the supplied rate. 1/300 ⇒ ~5 minutes from
-- 0 to critical. The planner consumes this need via berry baskets;
-- decay was tuned slow enough that NPCs don't look frantic, fast
-- enough that a typical play session sees a few hunger cycles.

engine.needs.register {
    id = "hunger",
    display_name = "Hunger",
    decay_per_sec = 1.0 / 300.0,
}

-- Sleep / tiredness. Decays slower than hunger because a day/night
-- cycle is the natural beat for it: at DAY_LENGTH_SECS = 600 (10 real
-- minutes) and decay 1/450, an NPC starting at 0 tiredness reaches the
-- typical eat threshold (~0.3, picked low so behaviour fires often)
-- well before the second night, leaving room for them to also do other
-- things during the day. The planner gates the actual sleep action on
-- it being night, so a tired NPC at noon still wanders rather than
-- collapsing into the nearest bed.
engine.needs.register {
    id = "sleep",
    display_name = "Tiredness",
    decay_per_sec = 1.0 / 450.0,
}

-- Work / purpose. Drives NPCs to pick up player-tagged plans (build,
-- demolish). Rises faster than sleep but slower than hunger — an NPC
-- with no plans available will see the deficit max out, but as long as
-- the player has work for them they'll churn through it. 1/240 ⇒ ~4
-- minutes from 0 to 1; threshold 0.3 means an NPC picks up the first
-- plan after ~70 seconds of idle, and a busy session keeps them at
-- moderate baseline as they work tags down.
engine.needs.register {
    id = "work",
    display_name = "Purpose",
    decay_per_sec = 1.0 / 240.0,
}

-- Work-action balance defaults. Read by the engine at WorkPlan goal
-- commit whenever the targeted block has no `work_action` of its own.
-- `restores = 0.35` matches the floor a sleeper restores — one
-- completed tag moves a villager from "looking for purpose" back
-- toward content. `duration_secs = 4.0` is long enough for a work
-- animation to read on screen without making each plan feel like a
-- chore. Individual blocks can override either knob via their own
-- `work_action = { ... }` table.
engine.npcs.set_work_defaults {
    need_restore = { need = "work", restores = 0.1 },
    duration_secs = 4.0,
}

-- Animation clips. The client loads each one's glTF asset at session
-- start, indexed via `clip_index` into that file's clip list, and
-- builds a unified AnimationGraph keyed by id. KayKit ships rig
-- clips split across themed glbs (General / MovementBasic /
-- Simulation / Tools); we register the four we currently use plus
-- any others a mod wants to reference.
--
-- Probed clip indices for the frozen KayKit pack (regenerate with
-- a glTF parser if the pack ever ships a new revision):
--   Rig_Medium_General.glb[6]       = "Idle_A"
--   Rig_Medium_MovementBasic.glb[8] = "Walking_A"
--   Rig_Medium_Simulation.glb[2]    = "Lie_Idle"
--   Rig_Medium_Tools.glb[26]        = "Working_A"
engine.animations.register {
    id = "vanilla:idle",
    asset = "mods://vanilla/models/characters/Rig_Medium_General.glb",
    clip_index = 6,
}

engine.animations.register {
    id = "vanilla:walk",
    asset = "mods://vanilla/models/characters/Rig_Medium_MovementBasic.glb",
    clip_index = 8,
}

engine.animations.register {
    id = "vanilla:lie_idle",
    asset = "mods://vanilla/models/characters/Rig_Medium_Simulation.glb",
    clip_index = 2,
}

engine.animations.register {
    id = "vanilla:work",
    asset = "mods://vanilla/models/characters/Rig_Medium_Tools.glb",
    clip_index = 26,
}

-- The smoke-test NPC kind. The planner that drives it lives in
-- events.lua; this block is just the declarative half (which side both
-- the client and server need to agree on for any future networked kind
-- table). default_needs is what each new NPC of this kind starts with;
-- spawning a partial deficit means the eat behaviour is observable
-- within ~30 s of session start rather than after the full decay
-- runway.
--
-- `animations.{idle, walk, work}` drive the client's animation
-- selection: idle/walk via velocity hysteresis when no goal-set
-- override is active, work when the NPC is pursuing a player plan.
-- Use-slot interactions (sleeping in a bed) override these with
-- the slot's `animation` field — see vanilla:bed below.
engine.npcs.register {
    id = "vanilla:wanderer",
    display_name = "Wanderer",
    default_needs = {
        hunger = 0.2,
        -- Spawn slightly tired so the first night triggers visible bed
        -- behaviour without waiting a full decay runway. Threshold +
        -- this baseline mean an NPC spawned at sunrise won't sleep
        -- until evening, but one spawned just before night may head
        -- straight for a bed.
        sleep = 0.15,
        -- Spawn with some pent-up purpose so a freshly-loaded session
        -- with a pre-tagged plan gets observable NPC activity within
        -- ~30 s rather than after the full 4-minute decay runway.
        work = 0.2,
    },
    animations = {
        idle = "vanilla:idle",
        walk = "vanilla:walk",
        work = "vanilla:work",
    },
}
