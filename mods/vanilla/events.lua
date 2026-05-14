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
--
-- The native brain side already handles path selection, A*, steering,
-- stuck-detection, and physics; the planner only chooses between these
-- primitives. Adding new primitives (e.g. "go to room X") requires
-- engine-side support.
--
-- Rest → (wander or visit) → Rest rhythm. State is per-NPC because each
-- wanderer's cadence is independent; snapshot.id is the stable handle
-- the engine guarantees doesn't collide between live NPCs and survives
-- save/load. Once needs/opinions hook in, the planner will score
-- actions against need deficits instead of alternating mechanically.
local last_action = {}

-- Probability of "go visit a room" after a rest, when at least one
-- matched room is reachable in the snapshot. 0.5 makes the room
-- behavior frequent enough to demo, but still leaves wander time so
-- demolished rooms are noticed by the next planner call.
local VISIT_PROBABILITY = 0.5

engine.npcs.set_planner("vanilla:wanderer", function(snapshot)
    -- snapshot.id            — stable u64 id of this NPC (table key)
    -- snapshot.kind          — NpcKindId we registered
    -- snapshot.foot          — { x, y, z } NPC's current foot cell
    -- snapshot.needs         — table keyed by need id (e.g. needs.hunger)
    -- snapshot.nearby_rooms  — sorted nearest-first list of
    --                          { id, pattern, anchor, distance }
    local prev = last_action[snapshot.id]

    -- Always rest after motion (wander or visit). Keeps the NPC from
    -- looking frantic and stops "visit room, complete, immediately
    -- re-visit same room" loops.
    if prev == "wander" or prev == "visit" then
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
