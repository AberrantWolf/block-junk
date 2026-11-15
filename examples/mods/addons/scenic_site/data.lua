-- Authored environmental evidence, deliberately independent of fluids or terrain.
engine.blocks.register {
    id="scenic_site:view",display_name="Scenic interest marker",
    flags={placeable=true},color={.2,.7,.9},structure={},
    environmental_attributes={scenic_interest=1},
}
engine.blocks.register {
    id="scenic_site:seat",display_name="Scenic seat",
    flags={placeable=true},color={.7,.5,.2},structure={},
    facility_slots={{key="rest",pose={0,1,0},yaw=0,approaches={{1,0,0},{-1,0,0},{0,0,1},{0,0,-1}},exclusive=true,interactions={"scenic_rest"}}},
    facility_actions={scenic_rest={kind="timed",duration_secs=2}},
}
local function slots(capacity)
    return {kind="observe",query={kind="slots",interaction="scenic_rest",capacity=capacity}}
end
engine.capabilities.register {
    id="scenic_site:rest",display_name="Scenic rest",
    scope={kind="outdoor",anchor_interaction="scenic_rest",range=4,allow_portals=false},
    requirements={{key="view",description="Reach a scenic interest marker within four surface steps",
        condition={kind="compare",operation="at_least",left={kind="observe",query={kind="environment",attribute="scenic_interest"}},right={kind="number",value=1}}}},
    capacity=slots("usable"),attributes={installed=slots("installed"),available=slots("available")},
}
