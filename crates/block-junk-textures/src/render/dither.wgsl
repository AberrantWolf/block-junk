// Screen-door transparency helpers shared by every ghost material
// (voxel ghost blocks here, mesh-block ghosts in the game crate).
//
// The idea: render OPAQUE and discard fragments whose 4×4 Bayer cell
// value exceeds a coverage in [0,1]. Because surviving fragments write
// real depth, internal faces, back faces, and overlapping ghosts
// occlude each other correctly — none of the alpha-accumulation mud
// translucent ghosts produce. Coverage can vary per fragment, which is
// how the distance fade works: full strength up close, thinning to a
// floor far away ("plans are harder to see from a distance").
//
// Pure functions, no bindings — callers own their uniforms and call
// `discard` themselves (WGSL can't discard from a helper's callee).

#define_import_path block_junk_textures::dither

// Ordered 4×4 Bayer matrix, values in [0, 1). `coverage <= 0` kills
// every fragment, `coverage >= 1` keeps every fragment.
fn bayer4(px: vec2<u32>) -> f32 {
    var m = array<f32, 16>(
         0.0 / 16.0,  8.0 / 16.0,  2.0 / 16.0, 10.0 / 16.0,
        12.0 / 16.0,  4.0 / 16.0, 14.0 / 16.0,  6.0 / 16.0,
         3.0 / 16.0, 11.0 / 16.0,  1.0 / 16.0,  9.0 / 16.0,
        15.0 / 16.0,  7.0 / 16.0, 13.0 / 16.0,  5.0 / 16.0,
    );
    return m[(px.y & 3u) * 4u + (px.x & 3u)];
}

// Distance-faded coverage: `coverage` up close, easing to
// `min_coverage` (the never-zero floor) between fade_start..fade_end
// metres from the camera. Disable the fade by passing
// `min_coverage == coverage` or an unreachable fade_start.
fn faded_coverage(
    frag_world_pos: vec3<f32>,
    view_world_pos: vec3<f32>,
    coverage: f32,
    min_coverage: f32,
    fade_start: f32,
    fade_end: f32,
) -> f32 {
    let dist = distance(frag_world_pos, view_world_pos);
    return mix(coverage, min_coverage, smoothstep(fade_start, fade_end, dist));
}

// True when the fragment at screen pixel `frag_px` should be discarded
// for an effective coverage value.
fn dither_discards(frag_px: vec2<f32>, effective_coverage: f32) -> bool {
    return bayer4(vec2<u32>(frag_px)) >= effective_coverage;
}
