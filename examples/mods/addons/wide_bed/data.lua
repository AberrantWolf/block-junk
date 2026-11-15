-- These contracts are reported by validate-mods --json. They describe the
-- independent base before/after this add-on, not the entire content fingerprint.
engine.world_upgrades.register {
    id = "wide_bed:foundation_v1",
    blocks = {{
        block = "foundation:bed",
        from_contract = "7909e8b3f1d2ee512c124fdd9bcc26635a710b9f52d60a95508966c6604d96a2",
        to_contract = "547bbdde56995cbb1ff00406063cf5611668ac74705c3b2d26353c2d278ff8cc",
        footprint = "rebuild_at_anchor",
        slot_renames = { primary = "sleeping" },
    }},
}
