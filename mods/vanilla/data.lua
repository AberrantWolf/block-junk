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

register {
    id = "vanilla:stone",
    display_name = "Stone",
    flags = {
        solid = true,
        room_boundary = true,
        support_below = true,
    },
    color = { 0.55, 0.55, 0.58 },
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
}

-- First block-entity test: a placeholder bed mesh. The .glb is a small
-- generated cuboid (1×0.5×2 ish, dark brown) — replace with proper art
-- when ready. The voxel mesher skips this slot's cube faces because
-- mesh is set; the client spawns an ECS entity with a SceneRoot loaded
-- from the path.
--
-- footprint = two cells extending east of the anchor: anchor (foot) at
-- {0,0,0}, head at {1,0,0}. The engine rotates this when the player
-- places at non-default orientations.
--
-- entity_aabb tracks the visible mesh's actual extent so ray tests that
-- pass above the low bed (it's roughly half-cell tall) don't break it.
-- Tune this when the real bed mesh lands. Coordinates are model-space:
-- anchor's bottom-centre at (0,0,0), +X = head, +Y = up.
register {
    id = "vanilla:bed",
    display_name = "Bed",
    flags = {
        solid = true,
        support_below = true,
    },
    color = { 0.40, 0.18, 0.05 },
    mesh = "mods://vanilla/models/bed.glb",
    footprint = { {0, 0, 0}, {1, 0, 0} },
    entity_aabb = {
        min = { -0.5, 0.0, -0.5 },
        max = {  1.5, 0.5,  0.5 },
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
    },
}
