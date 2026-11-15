-- Balance is entirely mod data. Any base exposing the "sleep" interaction can
-- use this add-on; it contains no terrain, model or vanilla content references.
local area_per_sleeper = 32
engine.capabilities.register {
    id="area_comfort:sleeping",display_name="Spacious sleeping",
    requirements={},
    capacity={kind="calculate",operation="minimum",operands={
        {kind="observe",query={kind="slots",interaction="sleep",capacity="usable"}},
        {kind="calculate",operation="floor",operands={
            {kind="calculate",operation="divide",operands={
                {kind="observe",query={kind="measure",measurement="structural_area"}},
                {kind="number",value=area_per_sleeper},
            }},
        }},
    }},
}
