-- Server-only script: registers a hook that fires after every successful
-- place-or-break edit applied to the authoritative world.

engine.on_block_placed(function(event)
    print(string.format(
        "[hello/server] block_placed: %s = %s",
        fmt_pos(event.pos), event.block
    ))
end)

-- Hook that fires whenever the room detector's view of the world changes.
-- `event.kind` is "created" / "changed" / "destroyed". `event.pattern` is
-- the deepest matching `RoomPatternId`, or nil if nothing matched.
engine.on_room_event(function(event)
    if event.kind == "created" then
        local pat = event.pattern or "(no match)"
        print(string.format(
            "[hello/server] room_created: room=%d pattern=%s cells=%d",
            event.room, pat, event.signature.cell_count
        ))
    elseif event.kind == "changed" then
        local from = event.from or "(none)"
        local to = event.to or "(none)"
        print(string.format(
            "[hello/server] room_changed: room=%d %s -> %s",
            event.room, from, to
        ))
    elseif event.kind == "destroyed" then
        print(string.format(
            "[hello/server] room_destroyed: room=%d", event.room
        ))
    end
end)
