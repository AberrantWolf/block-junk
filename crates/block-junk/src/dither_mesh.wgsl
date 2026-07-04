// Dithered ghost fragment shader for glTF mesh blocks (beds, stations).
// The base StandardMaterial is a CLONE of the mesh's real glTF material,
// so pbr_input_from_standard_material picks up its actual textures and
// colors — the ghost looks like the real thing, screen-doored.
//
// Front variant renders opaque with depth writes; x-ray variant renders
// in Transparent3d with depth compare `Less` (see DitherExt::specialize).
// Forward-only on purpose — mirror the discard into a prepass shader if
// a depth/deferred prepass ever lands.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
    mesh_view_bindings::view,
}

#import block_junk_textures::dither::{faded_coverage, dither_discards}

struct DitherParams {
    tint: vec4<f32>,
    coverage: f32,
    min_coverage: f32,
    fade_start: f32,
    fade_end: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100)
var<uniform> dither: DitherParams;

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    let coverage = faded_coverage(
        in.world_position.xyz,
        view.world_position,
        dither.coverage,
        dither.min_coverage,
        dither.fade_start,
        dither.fade_end,
    );
    if (dither_discards(in.position.xy, coverage)) {
        discard;
    }

    var pbr_input = pbr_input_from_standard_material(in, is_front);
    // Tint the real material's albedo; force alpha 1.0 — surviving
    // fragments are fully solid (and the x-ray blend pipeline then
    // writes crisp pixels).
    pbr_input.material.base_color = vec4<f32>(
        pbr_input.material.base_color.rgb * dither.tint.rgb,
        1.0,
    );

    var out: FragmentOutput;
    if (pbr_input.material.flags & STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
        out.color = apply_pbr_lighting(pbr_input);
    } else {
        out.color = pbr_input.material.base_color;
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
