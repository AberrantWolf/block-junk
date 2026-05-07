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

-- Seed room patterns. The detector isn't wired yet (next chunk of work);
-- these prove the registry's parent-chain and domain validation.

engine.rooms.register {
    id = "vanilla:enclosed_space",
    display_name = "Enclosed space",
    domain = "volumetric",
    constraints = {
        { kind = "floor_area", min = 4, max = 4096 },
        { kind = "headroom", min = 2 },
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
        { kind = "floor_area", max = 50 },
        { kind = "floor_fraction", surface = "solid", min = 0.8 },
    },
}
