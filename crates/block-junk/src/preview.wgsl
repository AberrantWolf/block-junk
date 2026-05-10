// Placement-preview fragment shader. Outputs the material's `color`
// uniform unchanged; the meaningful work lives on the pipeline:
//
//   - The "front" pass uses standard depth-test (LessEqual), alpha
//     blend. Fragments in front of geometry render as a translucent
//     ghost of the would-be entity.
//
//   - The "back" pass uses reversed depth-test (Greater), multiply
//     blend. Fragments BEHIND geometry render as a darkening factor on
//     top of whatever's already in the colour attachment, giving an
//     X-ray "blueprint shadow" feel without a separate depth-prepass
//     sample.
//
// Both passes share this shader; the per-pass pipeline state diverges
// in `Material::specialize` on the Rust side.

#import bevy_pbr::forward_io::VertexOutput

@group(2) @binding(0) var<uniform> color: vec4<f32>;

@fragment
fn fragment(_in: VertexOutput) -> @location(0) vec4<f32> {
    return color;
}
