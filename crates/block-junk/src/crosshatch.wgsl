// Demolition-marker fragment shader: two diagonal stripe sets derived
// purely from WORLD coordinates — stable in 3D, no UVs, identical
// across terrain cubes and mesh-block cells, and continuous across
// adjacent tagged cells (so a cancelled cell reads as a hole in the
// field). Unlit alpha-blend; drawn on a slightly inflated unit cube per
// tagged cell to sit just off the real geometry (no z-fighting, no
// depth bias needed).

#import bevy_pbr::{
    forward_io::VertexOutput,
    mesh_view_bindings::view,
}

struct CrosshatchParams {
    color: vec4<f32>,
    period: f32,
    duty: f32,
    min_alpha: f32,
    fade_start: f32,
    fade_end: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0)
var<uniform> hatch: CrosshatchParams;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let wp = in.world_position.xyz;
    // Two opposing diagonal stripe families → an X-hatch. The plane
    // sums keep the pattern coherent on every face orientation.
    let s1 = fract((wp.x + wp.y + wp.z) / hatch.period);
    let s2 = fract((wp.x - wp.y + wp.z) / hatch.period);
    let stripes = max(step(s1, hatch.duty), step(s2, hatch.duty));
    if (stripes <= 0.0) {
        discard;
    }
    // Distance fade toward the alpha-ratio floor — far-off demolition
    // fields thin to a faint sketch but never vanish.
    let dist = distance(wp, view.world_position);
    let fade = mix(1.0, hatch.min_alpha, smoothstep(hatch.fade_start, hatch.fade_end, dist));
    return vec4<f32>(hatch.color.rgb, hatch.color.a * fade);
}
