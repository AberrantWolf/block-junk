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
-- Same Wander → Rest cadence the Rust engine used to hardcode. State is
-- per-NPC because each one's wanderer rhythm is independent;
-- snapshot.id is the stable handle the engine guarantees doesn't
-- collide between live NPCs and survives save/load.
-- Once needs/opinions hook in, the planner will score actions instead
-- of alternating mechanically.
local last_action = {}

engine.npcs.set_planner("vanilla:wanderer", function(snapshot)
    -- snapshot.id     — stable u64 id of this NPC (use as a table key)
    -- snapshot.kind   — the NpcKindId we registered ("vanilla:wanderer")
    -- snapshot.foot   — { x, y, z } of the NPC's current foot cell
    -- snapshot.needs  — table keyed by need id, e.g. snapshot.needs.hunger
    local prev = last_action[snapshot.id]
    if prev == "wander" then
        last_action[snapshot.id] = "rest"
        -- Random rest in [3, 8] s — matches the cadence the native
        -- fallback used before the planner surface landed.
        return { kind = "rest", duration_secs = 3.0 + math.random() * 5.0 }
    else
        last_action[snapshot.id] = "wander"
        return { kind = "wander", radius_cells = 12, timeout_secs = 12.0 }
    end
end)
