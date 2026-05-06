-- Server-only script: registers a hook that fires after every successful
-- place-or-break edit applied to the authoritative world.

engine.on_block_placed(function(event)
    print(string.format(
        "[hello/server] block_placed: %s = %s",
        fmt_pos(event.pos), event.block
    ))
end)
