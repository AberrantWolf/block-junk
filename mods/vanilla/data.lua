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
            softness = 0.10,
        },
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
    -- The first sleeper. `restores = 0.7` means a full sleep brings a
    -- tiredness deficit down by 70% — a bedtime mostly resets the need
    -- but doesn't completely max it, so a long day still ends with the
    -- NPC visibly tired before bed. `duration_secs = 25` is long enough
    -- to read as "they're asleep" but short enough that a player
    -- watching the world doesn't lose interest before the cycle
    -- completes. Only one NPC may claim a given bed at a time; the
    -- engine maintains the claim table.
    sleeper = {
        need = "sleep",
        restores = 0.7,
        duration_secs = 25.0,
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

-- A berry basket — the first consumable. Block-entity like the bed
-- (1-cell footprint, but with a custom mesh), and tagged with
-- `consumable` metadata so the engine indexes it for NPC snapshots.
-- The mesh's local frame puts the basket centred at the anchor's
-- bottom (Y in [0, 0.67], X/Z in ±0.43), so the default bottom-centre
-- modelling rule fits without translation.
--
-- `restores = 0.4` ⇒ one consumption brings a hunger deficit down by
-- 0.4 (40% of the 0..1 scale). With decay 1/300 ⇒ ~2 minutes of decay
-- before the same need climbs back. `duration_secs = 2.0` is enough
-- visible standstill for a player to see "the NPC stopped to eat";
-- shorter looks like a teleport, longer feels ritualistic.
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
    consumable = {
        need = "hunger",
        restores = 0.4,
        duration_secs = 2.0,
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

-- The smoke-test NPC kind. The planner that drives it lives in
-- events.lua; this block is just the declarative half (which side both
-- the client and server need to agree on for any future networked kind
-- table). default_needs is what each new NPC of this kind starts with;
-- spawning a partial deficit means the eat behaviour is observable
-- within ~30 s of session start rather than after the full decay
-- runway.
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
}
