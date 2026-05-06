-- Loaded into both sides' Lua states (separately — no cross-side runtime sharing).
-- Use this for constants, pure helpers, or type-like tables.

function fmt_pos(p)
    return string.format("(%d, %d, %d)", p.x, p.y, p.z)
end
