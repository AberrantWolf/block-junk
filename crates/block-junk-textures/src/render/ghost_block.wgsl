// Ghost-block fragment shader: a voxel block's real composited texture
// rendered as a dithered screen-door ghost (placement previews and
// committed Build-plan ghosts).
//
// Differences from chunk_material.wgsl:
//  - The block slot comes from the `ghost` uniform, not vertex colors —
//    ghost cubes are plain `Cuboid` meshes with no color attribute.
//  - The composited color is multiplied by `ghost.tint` (white = as-is,
//    red = invalid placement, amber = waiting on materials).
//  - Bayer-dither discard with distance-faded coverage. The front
//    variant renders in the Opaque pass writing real depth; the x-ray
//    variant (see `GhostBlockExt::xray`) renders in Transparent3d with
//    depth compare `Less` so only fragments BEHIND world geometry
//    survive — the sparse behind-wall hint.
//
// Forward-only on purpose: ghosts disable shadows and there is no
// prepass in this game. If a depth/deferred prepass ever lands, the
// dither discard must be mirrored into a prepass fragment shader or
// early-Z will draw the discarded pixels anyway.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
    pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT,
    mesh_view_bindings::view,
}

#import block_junk_textures::composite::{composite_block_color, srgb_to_linear}
#import block_junk_textures::dither::{faded_coverage, dither_discards}

struct GhostParams {
    tint: vec4<f32>,
    slot: u32,
    coverage: f32,
    min_coverage: f32,
    fade_start: f32,
    fade_end: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(104)
var<uniform> ghost: GhostParams;

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    let coverage = faded_coverage(
        in.world_position.xyz,
        view.world_position,
        ghost.coverage,
        ghost.min_coverage,
        ghost.fade_start,
        ghost.fade_end,
    );
    if (dither_discards(in.position.xy, coverage)) {
        discard;
    }

    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let color = composite_block_color(ghost.slot, in.world_position.xyz, in.world_normal)
        * ghost.tint.rgb;
    // Alpha stays 1.0: surviving fragments are fully solid — that's the
    // whole point of screen-door transparency (and it keeps the x-ray
    // variant's alpha-blend pipeline writing crisp pixels).
    pbr_input.material.base_color = vec4<f32>(srgb_to_linear(color), 1.0);

    var out: FragmentOutput;
    if (pbr_input.material.flags & STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
        out.color = apply_pbr_lighting(pbr_input);
    } else {
        out.color = pbr_input.material.base_color;
    }
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
