-- events.lua: server-only runtime hook registrations for vanilla.
-- data.lua already populated the declarative registries (blocks, room
-- patterns, needs, NPC kinds); this file adds the live callbacks that
-- run against the authoritative world.

-- The vanilla:wanderer planner. The engine calls this whenever a
-- wanderer's current goal completes (or on first spawn, before it has
-- a goal). Return value is the next abstract goal the engine should
-- execute. Possible shapes:
--
--   { kind = "idle" }                            — defer to next tick (engine arms a short rest)
--   { kind = "wander", radius_cells = N, timeout_secs = T }
--   { kind = "rest",   duration_secs = T }
--   { kind = "goto",    cell = {x,y,z}, timeout_secs = T }
--   { kind = "consume", cell = {x,y,z}, timeout_secs = T }
--
-- The native brain side already handles path selection, A*, steering,
-- stuck-detection, consumption duration, and physics; the planner only
-- chooses between these primitives. Adding new primitives requires
-- engine-side support.

-- Per-NPC bookkeeping. Keyed by snapshot.id (the engine guarantees this
-- is stable across save/load and unique between live NPCs); we use it
-- to alternate motion/rest so the cluster doesn't all wander in lockstep
-- and so a freshly-completed visit doesn't immediately re-trigger.
local last_action = {}

-- Hunger threshold above which the NPC heads for food rather than
-- continuing its idle rhythm. With `restores = 0.4` per basket and the
-- threshold here at 0.3, an NPC that's just eaten won't head back for
-- more — the post-meal value (max(0, current - 0.4)) drops well below
-- the trigger. Picked low enough that visible "I'm hungry, going to
-- the basket" behaviour fires regularly in a session, high enough that
-- a freshly-spawned NPC with default deficit 0.2 doesn't pre-empt
-- everything else on tick zero.
local HUNGER_THRESHOLD = 0.3

-- Probability of "go visit a room" after a rest, when at least one
-- matched room is reachable in the snapshot. 0.5 makes the room
-- behavior frequent enough to demo, but still leaves wander time so
-- demolished rooms are noticed by the next planner call.
local VISIT_PROBABILITY = 0.5

-- Pick the nearest consumable that addresses `need_id` above the
-- threshold, if any. `snapshot.nearby_consumables` is engine-sorted
-- nearest-first so the first match is the closest; we don't have to
-- sort or score beyond the need-id filter today. Returns the cell
-- table the engine will path to.
local function nearest_consumable_for(snapshot, need_id)
    for _, c in ipairs(snapshot.nearby_consumables) do
        if c.need == need_id then
            return c.cell
        end
    end
    return nil
end

engine.npcs.set_planner("vanilla:wanderer", function(snapshot)
    -- snapshot.id                — stable u64 id of this NPC (table key)
    -- snapshot.kind              — NpcKindId we registered
    -- snapshot.foot              — { x, y, z } NPC's current foot cell
    -- snapshot.needs             — table keyed by need id (e.g. needs.hunger)
    -- snapshot.nearby_rooms      — sorted nearest-first list
    -- snapshot.nearby_consumables — sorted nearest-first list

    -- Highest priority: hunger that's reached the eat threshold AND a
    -- reachable food source. Returns immediately so this short-circuits
    -- the rest/wander rhythm whenever the NPC has both a deficit and a
    -- way to fix it.
    local hunger = snapshot.needs.hunger or 0.0
    if hunger >= HUNGER_THRESHOLD then
        local food_cell = nearest_consumable_for(snapshot, "hunger")
        if food_cell ~= nil then
            last_action[snapshot.id] = "consume"
            return {
                kind = "consume",
                cell = food_cell,
                timeout_secs = 30.0,
            }
        end
        -- Hungry but no food in sight: fall through to wander/visit so
        -- the NPC keeps exploring; maybe a basket comes into snapshot
        -- range as it moves.
    end

    local prev = last_action[snapshot.id]

    -- Always rest after motion (wander, visit, or consume). Keeps the
    -- NPC from looking frantic and stops "complete goal, immediately
    -- repeat" loops.
    if prev == "wander" or prev == "visit" or prev == "consume" then
        last_action[snapshot.id] = "rest"
        return { kind = "rest", duration_secs = 3.0 + math.random() * 5.0 }
    end

    -- After resting, pick a fresh motion: visit a known room or
    -- wander. Visit only fires when we know of at least one room.
    if #snapshot.nearby_rooms > 0 and math.random() < VISIT_PROBABILITY then
        last_action[snapshot.id] = "visit"
        local target = snapshot.nearby_rooms[1]  -- nearest
        return {
            kind = "goto",
            cell = target.anchor,
            timeout_secs = 60.0,
        }
    end

    last_action[snapshot.id] = "wander"
    return { kind = "wander", radius_cells = 12, timeout_secs = 12.0 }
end)
