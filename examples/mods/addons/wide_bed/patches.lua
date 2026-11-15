-- One furnishing identity survives; its footprint and slot name change.
engine.blocks.patch {
    id = "foundation:bed",
    footprint = {{0,0,0},{0,0,1}},
    facility_slots = {{
        key = "sleeping", pose = {0,1,0}, yaw = 0,
        approaches = {{1,0,0},{-1,0,0}},
        exclusive = true, interactions = {"sleep"},
    }},
}
