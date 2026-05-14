-- data.lua: loaded into both sides' Lua states (separately — no cross-
-- side runtime sharing). Use for declarative registrations, constants,
-- and pure helpers that events.lua wants to reuse.

function fmt_pos(p)
    return string.format("(%d, %d, %d)", p.x, p.y, p.z)
end
