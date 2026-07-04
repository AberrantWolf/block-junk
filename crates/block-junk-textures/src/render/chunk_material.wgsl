// Chunk fragment shader: composite per-block procedural texture layers,
// then hand off to Bevy's standard PBR lighting.
//
// Per fragment:
//  1. Recover the block slot from the vertex color's alpha (the mesher
//     packs `slot / 255` there).
//  2. Composite the face color via the shared library
//     (`block_junk_textures::composite` — structs, bindings 100-103,
//     and the layer-blend loop live there, shared with the ghost-block
//     material).
//  3. Convert display → linear once and run the PBR path.

#import bevy_pbr::{
    pbr_types,
    pbr_functions::alpha_discard,
    pbr_fragment::pbr_input_from_standard_material,
    decal::clustered::apply_decals,
}

#ifdef PREPASS_PIPELINE
#import bevy_pbr::{
    prepass_io::{VertexOutput, FragmentOutput},
    pbr_deferred_functions::deferred_output,
}
#else
#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
}
#endif

#import block_junk_textures::composite::{composite_block_color, srgb_to_linear}

@fragment
fn fragment(
    vertex_output: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var in = vertex_output;

    // Slot id from the vertex color alpha; `+ 0.5` so f32→u32 rounds.
#ifdef VERTEX_COLORS
    let slot = u32(in.color.a * 255.0 + 0.5);
    // The alpha was overloaded; restore opaque white so vertex-color
    // tinting stays a no-op in the PBR path.
    in.color = vec4<f32>(1.0, 1.0, 1.0, 1.0);
#else
    let slot = 0u;
#endif

    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let color = composite_block_color(slot, in.world_position.xyz, in.world_normal);

    pbr_input.material.base_color = vec4<f32>(srgb_to_linear(color), 1.0);
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);

    apply_decals(&pbr_input);

#ifdef PREPASS_PIPELINE
    let out = deferred_output(in, pbr_input);
#else
    var out: FragmentOutput;
    if (pbr_input.material.flags & STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
        out.color = apply_pbr_lighting(pbr_input);
    } else {
        out.color = pbr_input.material.base_color;
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
#endif

    return out;
}
