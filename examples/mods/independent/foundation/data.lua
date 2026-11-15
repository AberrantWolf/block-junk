local model = "mods://foundation/models/fixture.gltf"
engine.items.register {id="foundation:stone",display_name="Stone piece",mesh=model}
engine.items.register {id="foundation:brick",display_name="Brick",mesh=model}
local function solid(id, color)
    engine.blocks.register {id="foundation:"..id,display_name=id,flags={solid=true,placeable=true,room_boundary=true,support_below=true},color=color,
        structure={solid_cells={{0,0,0}},supports={{0,0,0}},shelter_cells={{0,0,0}}},drops={{item="foundation:stone",count=1}}}
end
solid("stone",{.4,.5,.6})
solid("soil",{.4,.3,.2})
solid("grass",{.3,.6,.3})
solid("wood",{.5,.3,.1})
solid("leaves",{.2,.5,.2})
local function furnishing(id, interaction, extra)
    local def = {id="foundation:"..id,display_name=id,flags={solid=true,placeable=true},color={.7,.5,.3},mesh=model,
        structure={},materials={{item="foundation:stone",count=1}},facility_slots={{key="primary",pose={0,1,0},yaw=0,approaches={{1,0,0},{-1,0,0},{0,0,1},{0,0,-1}},exclusive=true,interactions={interaction}}}}
    for key,value in pairs(extra or {}) do def[key]=value end
    def.facility_actions = {[interaction] = interaction == "crafting" and {kind="crafting"} or interaction == "storage" and {kind="storage"} or {kind="timed",duration_secs=1}}
    def.settlement = (id=="bed" or id=="bench") and "seed" or "joiner"
    engine.blocks.register(def)
end
furnishing("bed","sleep")
furnishing("chair","dining",{tags={"foundation:seat"}})
furnishing("table","serving",{tags={"foundation:table"}})
furnishing("storage","storage",{container={capacity_bulk=16,accepts={}}})
furnishing("bench","crafting",{station_tag="foundation:crafting",station_tier=1})
engine.recipes.register {id="foundation:brick",display_name="Shape brick",inputs={{item="foundation:stone",count=1}},output={item="foundation:brick",count=1},duration_secs=2,station="foundation:crafting",tier=1}
engine.animations.register {id="foundation:idle",asset=model,clip_index=0}
engine.npcs.register {id="foundation:settler",display_name="Settler",movement_profile="foundation:humanoid",job_classes={"construction","hauling","crafting"},models={model},animations={idle="foundation:idle",walk="foundation:idle",work="foundation:idle"}}
engine.movement.register {id="foundation:humanoid",height=1.8,radius=.3,step_up=1,max_drop=3,stairs=true}
engine.settlements.register {id="foundation:colony",outdoor_link_range=24,movement_profile="foundation:humanoid"}
engine.worlds.register {id="foundation:plain",display_name="Independent plain",terrain={stone="foundation:stone",dirt="foundation:soil",grass="foundation:grass",wood="foundation:wood",leaves="foundation:leaves",gravel="foundation:stone",forage="engine:air"},generation={algorithm="sine_heightmap",features={tree_one_in=0,forage_one_in=0,gravel_one_in=0,tree_height={4,5},canopy_radius=2,gravel_radius={1,2}},version=1,seed=7,base_height=8,amplitude={0,0},frequency={0,0},soil_depth=3,trees=false,forage=false,gravel=false},player_spawn={0,12,0},population={{kind="foundation:settler",position={3,12,0}}},movement_profile="foundation:humanoid",settlement="foundation:colony"}

engine.state.register(1, { completed = 0 })

-- A two-cell passage with its own lintel; opening affects traversal only.
engine.blocks.register {
    id="foundation:door",display_name="Door",flags={solid=true,placeable=true},color={.55,.4,.25},
    portal_visuals={{portal="passage",hidden_cells_when_open={{0,0,0},{0,1,0}}}},
    footprint={{0,0,0},{0,1,0},{0,2,0}},materials={{item="foundation:stone",count=2}},
    structure={solid_cells={{0,2,0}},shelter_cells={{0,2,0}},portals={{key="passage",
        faces={{cell={0,0,0},normal={1,0,0},window=false}},threshold={{0,0,0}},
        volume={{0,0,0},{0,1,0}},initially_open=true}}}
}

-- Independent additive functions, with no definitions borrowed from vanilla.
local function count(interaction, adjacent, capacity)
    return {kind="observe",query={kind="slots",interaction=interaction,capacity=capacity or "usable",adjacent_to_tag=adjacent}}
end
for _,entry in ipairs({{"sleeping","sleep"},{"dining","dining","foundation:table"},{"crafting","crafting"},{"storage","storage"}}) do
    local value=count(entry[2],entry[3])
    engine.capabilities.register {id="foundation:"..entry[1],display_name=entry[1],requirements={{key="facility",description="At least one accessible "..entry[1].." place",condition={kind="compare",operation="at_least",left=value,right={kind="number",value=1}}}},capacity=value,attributes={installed=count(entry[2],entry[3],"installed"),usable=value,available=count(entry[2],entry[3],"available")}}
end
