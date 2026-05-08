-- Vanilla content registers from shared.lua so both the server and the
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
register {
    id = "vanilla:bed",
    display_name = "Bed",
    flags = {
        solid = true,
        support_below = true,
    },
    color = { 0.40, 0.18, 0.05 },
    mesh = "mods://vanilla/models/bed.glb",
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
