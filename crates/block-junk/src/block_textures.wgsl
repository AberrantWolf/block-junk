// Chunk fragment shader.
//
// Mirrors Bevy's stock `pbr.wgsl` but overrides `pbr_input.material.base_color`
// from a per-block texture-array sample before lighting runs. The slot id
// for each fragment is recovered from the per-vertex colour's alpha
// channel (the mesher writes `slot / 255` there); rgb of the vertex
// colour is unused and kept at 1.0 so the alpha overwrite doesn't leak
// into the lit colour.
//
// Greedy-meshed quads span many cells. We compute UVs in this shader
// from `world_position` projected onto the face plane (selected by
// `world_normal`); with the texture-array sampler set to Repeat +
// Nearest, each cell of the world samples one full copy of the 16×16
// texture — adjacent cells of the same block look identical, which is
// the pixel-art tiling effect we want.
//
// On top of the base sample we composite a per-block stack of mask+ramp
// LAYERS read from a storage buffer indexed by slot. Each layer samples
// a tileable grayscale mask (in world space, at its own scale), turns
// that into a 0..1 coverage with smoothstep, looks up a color in a 1D
// ramp at U = mask value, and `mix`es over the running color. Stacks
// with `num_layers = 0` skip the loop entirely and produce the same
// output as the pre-layer version of this shader.

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

@group(#{MATERIAL_BIND_GROUP}) @binding(100)
var block_atlas: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101)
var block_atlas_sampler: sampler;

// Mask atlas: tileable R8 grayscale patterns. Sampled in world space at
// each layer's own scale; the .r channel is the mask value.
@group(#{MATERIAL_BIND_GROUP}) @binding(102)
var mask_atlas: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103)
var mask_sampler: sampler;

// Ramp atlas: 1-pixel-tall RGBA strips. Sampled with U = mask value
// (clamped) so the ramp paints depth/shading inside the masked region.
@group(#{MATERIAL_BIND_GROUP}) @binding(104)
var ramp_atlas: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105)
var ramp_sampler: sampler;

// Field-for-field mirror of `Layer` / `LayerStack` in `block_textures.rs`.
// Encase scalar layout — no padding fields needed. If this ever drifts,
// the shader will read garbage threshold/scale values; keep them in sync.
struct Layer {
    mask_slot: u32,
    ramp_slot: u32,
    scale: f32,
    threshold: f32,
    softness: f32,
}

struct LayerStack {
    num_layers: u32,
    layers: array<Layer, 4>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(106)
var<storage, read> stacks: array<LayerStack>;

// Pick the two world-space axes that align with the face plane and use
// them as UV. The texture-array sampler's Repeat mode handles tiling
// for greedy quads that span multiple cells.
fn face_uv(world_pos: vec3<f32>, world_normal: vec3<f32>) -> vec2<f32> {
    let n = abs(world_normal);
    if (n.y > 0.5) {
        // Top / bottom: project onto XZ.
        return vec2<f32>(world_pos.x, world_pos.z);
    } else if (n.x > 0.5) {
        // East / west: project onto ZY.
        return vec2<f32>(world_pos.z, world_pos.y);
    } else {
        // North / south: project onto XY.
        return vec2<f32>(world_pos.x, world_pos.y);
    }
}

@fragment
fn fragment(
    vertex_output: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var in = vertex_output;

    // Recover the slot id from the alpha channel that the mesher
    // packed in. `+ 0.5` so the f32→u32 truncation rounds rather than
    // floors and we don't lose the top of the range to FP error.
#ifdef VERTEX_COLORS
    let slot = u32(in.color.a * 255.0 + 0.5);
    // Stop the alpha leaking into the lit colour: the rgb is white in
    // the mesher (so vertex-colour tinting is a no-op), but the alpha
    // was overloaded. Restore opaque.
    in.color = vec4<f32>(1.0, 1.0, 1.0, 1.0);
#else
    let slot = 0u;
#endif

    // Build the standard PbrInput, then patch base_color with our
    // texture-array sample plus the per-slot layer stack. From here PBR
    // lighting / fog / tonemap run exactly as in `pbr.wgsl`.
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    let uv = face_uv(in.world_position.xyz, in.world_normal);
    let base_sample = textureSample(block_atlas, block_atlas_sampler, uv, i32(slot));
    var color = base_sample.rgb;

    // Composite the per-slot layer stack. Each fragment of a single
    // greedy quad has the same `slot` (the mesher writes one slot per
    // quad), so `num_layers` is uniform per primitive — but we use
    // `textureSampleLevel(0)` for masks/ramps to avoid the WGSL
    // non-uniform-derivatives gripe and to make the no-mipmap behavior
    // explicit (we don't generate mips for these atlases).
    let num_layers = stacks[slot].num_layers;
    for (var i = 0u; i < num_layers; i = i + 1u) {
        let layer = stacks[slot].layers[i];
        let s = max(layer.scale, 0.001);
        let mask_uv = uv / s;
        let mask = textureSampleLevel(
            mask_atlas, mask_sampler, mask_uv, i32(layer.mask_slot), 0.0,
        ).r;
        let coverage = smoothstep(
            layer.threshold - layer.softness,
            layer.threshold + layer.softness,
            mask,
        );
        let layer_color = textureSampleLevel(
            ramp_atlas, ramp_sampler, vec2<f32>(mask, 0.5), i32(layer.ramp_slot), 0.0,
        ).rgb;
        color = mix(color, layer_color, coverage);
    }

    pbr_input.material.base_color = vec4<f32>(color, 1.0);

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
