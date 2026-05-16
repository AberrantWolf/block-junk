-- events.lua: server-only runtime hook registrations for vanilla.
-- data.lua already populated the declarative registries (blocks, room
-- patterns, needs, NPC kinds); this file adds the live callbacks that
-- run against the authoritative world.

-- The vanilla:wanderer planner. The engine calls this whenever a
-- wanderer's current goal completes (or on first spawn, before it has
-- a goal). Return value is the next abstract goal the engine should
-- execute. Possible shapes:
--
--   { kind = "idle" }                              — defer to next tick (engine arms a short rest)
--   { kind = "wander", radius_cells = N, timeout_secs = T }
--   { kind = "rest",   duration_secs = T }
--   { kind = "goto",     cell = {x,y,z}, timeout_secs = T }
--   { kind = "interact", cell = {x,y,z}, timeout_secs = T }
--   { kind = "work_plan", cell = {x,y,z}, timeout_secs = T }
--
-- The native brain side already handles path selection, A*, steering,
-- stuck-detection, interaction duration, and physics; the planner only
-- chooses between these primitives. New interaction *kinds* (eat,
-- sleep, enchant, sit) don't need a new primitive — they're all
-- `interact`, distinguished by the block def's `interactable`
-- metadata, which the engine reads when committing the goal.

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

-- Sleep threshold above which a tired NPC will *consider* heading for
-- a bed — actual sleep only fires when it's also night out (gated by
-- snapshot.is_night below). With `restores = 0.7` per bed and threshold
-- 0.25, an NPC that's slept will go well below the trigger, but an NPC
-- that hasn't slept in a day will be visibly tired by the time night
-- rolls around. The spawn default is 0.15, so a fresh NPC won't head
-- straight for a bed on session start.
local SLEEP_THRESHOLD = 0.25

-- Work threshold above which an NPC will pick up the nearest player-
-- tagged plan. With `restores = 0.35` per completed action and threshold
-- 0.3, a villager who's just finished a tag won't immediately re-trigger
-- (post-completion deficit ~= 0), but a few minutes later they're back
-- in the queue. Ordered below sleep (rest is mandatory) and below
-- hunger (eating is more urgent), but above the idle wander/visit
-- rhythm — work is what a villager *chooses* to do over wandering.
local WORK_THRESHOLD = 0.3

-- Probability of "go visit a room" after a rest, when at least one
-- matched room is reachable in the snapshot. 0.5 makes the room
-- behavior frequent enough to demo, but still leaves wander time so
-- demolished rooms are noticed by the next planner call.
local VISIT_PROBABILITY = 0.5

-- Pick the nearest interactable whose `need_restore.need` is
-- `need_id`, if any. `snapshot.nearby_interactions` is engine-sorted
-- nearest-first and already filters out exclusive entries claimed by
-- another NPC, so the first match is one this NPC could actually
-- claim (modulo the rare same-tick race the engine catches at
-- try_claim). Returns the cell table the engine will path to, or
-- nil if no nearby interactable serves this need.
local function nearest_interaction_for(snapshot, need_id)
    for _, n in ipairs(snapshot.nearby_interactions) do
        if n.need == need_id then
            return n.cell
        end
    end
    return nil
end

-- Closest plan cell, ignoring verb (build vs remove). nearby_plans is
-- already engine-sorted nearest-first and filtered against other NPCs'
-- claims. Returns the cell table the engine will path to, or nil if
-- nothing's available.
local function nearest_plan(snapshot)
    if snapshot.nearby_plans and #snapshot.nearby_plans > 0 then
        return snapshot.nearby_plans[1].cell
    end
    return nil
end

engine.npcs.set_planner("vanilla:wanderer", function(snapshot)
    -- snapshot.id                  — stable u64 id of this NPC (table key)
    -- snapshot.kind                 — NpcKindId we registered
    -- snapshot.foot                 — { x, y, z } NPC's current foot cell
    -- snapshot.needs                — table keyed by need id (e.g. needs.hunger)
    -- snapshot.nearby_rooms         — sorted nearest-first list
    -- snapshot.nearby_interactions  — sorted nearest-first list of every
    --                                 reachable interactable; each entry
    --                                 has `need` (string or nil),
    --                                 `restores`, and `exclusive`.

    -- Highest priority: tired-at-night with a free bed reachable.
    -- The night gate is what stops NPCs from collapsing into a bed at
    -- noon the moment they reach the tired threshold — sleep is a
    -- nocturnal action even if they're exhausted earlier in the day.
    -- Ordered above hunger so a tired hungry NPC sleeps now and eats
    -- in the morning (a tired NPC mid-meal at sunrise would be
    -- disorienting to watch; the simple rule reads cleaner).
    --
    -- The planner doesn't distinguish "consume" from "sleep" at the
    -- emit step — both are `interact` against an interactable whose
    -- need_restore.need matches. The engine reads the block def to
    -- find duration, exclusivity, and slot data.
    local sleep_need = snapshot.needs.sleep or 0.0
    if snapshot.is_night and sleep_need >= SLEEP_THRESHOLD then
        local bed_cell = nearest_interaction_for(snapshot, "sleep")
        if bed_cell ~= nil then
            last_action[snapshot.id] = "sleep"
            return {
                kind = "interact",
                cell = bed_cell,
                timeout_secs = 60.0,
            }
        end
        -- Tired at night with no bed: fall through to wander/eat so
        -- the NPC isn't frozen pacing in place. If a player places a
        -- bed nearby, the next planner call will catch it.
    end

    -- Next: hunger that's reached the eat threshold AND a reachable
    -- food source. Returns immediately so this short-circuits the
    -- rest/wander rhythm whenever the NPC has both a deficit and a
    -- way to fix it.
    local hunger = snapshot.needs.hunger or 0.0
    if hunger >= HUNGER_THRESHOLD then
        local food_cell = nearest_interaction_for(snapshot, "hunger")
        if food_cell ~= nil then
            last_action[snapshot.id] = "consume"
            return {
                kind = "interact",
                cell = food_cell,
                timeout_secs = 30.0,
            }
        end
        -- Hungry but no food in sight: fall through to wander/visit so
        -- the NPC keeps exploring; maybe a basket comes into snapshot
        -- range as it moves.
    end

    -- Then: pick up player-tagged work if the deficit is high enough
    -- and there's something to do. Sits below sleep / hunger (those are
    -- survival-flavoured) and above the idle wander rhythm. A villager
    -- with a queue of tags will work through them rather than wander.
    local work = snapshot.needs.work or 0.0
    if work >= WORK_THRESHOLD then
        local plan_cell = nearest_plan(snapshot)
        if plan_cell ~= nil then
            last_action[snapshot.id] = "work"
            return {
                kind = "work_plan",
                cell = plan_cell,
                timeout_secs = 60.0,
            }
        end
    end

    local prev = last_action[snapshot.id]

    -- Always rest after motion (wander, visit, consume, sleep, or work).
    -- Keeps the NPC from looking frantic and stops "complete goal,
    -- immediately repeat" loops. Sleep + work included so an NPC that
    -- just finished one takes a breath before deciding the next thing.
    if prev == "wander"
        or prev == "visit"
        or prev == "consume"
        or prev == "sleep"
        or prev == "work"
    then
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
