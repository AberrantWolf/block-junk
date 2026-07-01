-- data.lua: loaded into both sides' Lua states (separately — no cross-
-- side runtime sharing). Use for declarative registrations, constants,
-- and pure helpers that events.lua wants to reuse.

function fmt_pos(p)
    return string.format("(%d, %d, %d)", p.x, p.y, p.z)
end

-- engine.ui.on_inspect: append a line to the hover-inspect panel.
-- `target.kind` is "block" | "item" | "npc" | "plan"; `target.id` is
-- the registry id (block/item/npc kind, or the build-target block of a
-- plan); `target.pos` is the hovered cell for blocks/plans, nil
-- otherwise. Return a string to show it, nil to add nothing.
-- Registering here in data.lua is fine on both sides — only the
-- client ever invokes the callback.
engine.ui.on_inspect(function(target)
    if target.kind == "block" and target.id == "vanilla:workbench" then
        return "[hello] a fine workbench indeed"
    end
    return nil
end)
