// Shared block-texture compositing library: the structs, bindings, and
// layer-blend loop used by every material that renders baked block
// textures (the chunk terrain material and the ghost-block material).
//
// Bindings 100-103 follow the extended-material convention (0..99
// belong to the base StandardMaterial). Every importer's AsBindGroup
// MUST bind the same resources at the same slots — the import is legal
// precisely because the binding numbers agree.
//
// Compositing runs in DISPLAY space (tiles are Rgba8Unorm, not Srgb) so
// it matches the CPU `bake::composite_at` byte-for-byte; callers convert
// the final color to linear once (srgb_to_linear) before PBR. blend_rgb
// and finish_fbm mirror `eval::blend_rgb` / `noise::value_fbm_world` —
// keep all three in sync.

#define_import_path block_junk_textures::composite

struct BlockTex {
    top: u32,
    side: u32,
    bottom: u32,
    _pad: u32,
    fallback: vec4<f32>,
}

struct TexInfo {
    layer_start: u32,
    layer_count: u32,
    finish_brightness: f32,
    finish_scale: f32,
    finish_seed: u32,
}

struct LayerInfo {
    array_index: u32,
    size_px: u32,
    period: f32,
    blend: u32,
    opacity: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100)
var tiles: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101)
var<storage, read> blocks: array<BlockTex>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102)
var<storage, read> textures: array<TexInfo>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103)
var<storage, read> layers: array<LayerInfo>;

const NO_TEXTURE: u32 = 0xffffffffu;

// Project world position onto the face plane (by dominant normal axis).
// World-space UVs make adjacent blocks share one continuous pattern —
// the whole point of the multi-period system.
fn face_uv(world_pos: vec3<f32>, world_normal: vec3<f32>) -> vec2<f32> {
    let n = abs(world_normal);
    if (n.y > 0.5) {
        return vec2<f32>(world_pos.x, world_pos.z);
    } else if (n.x > 0.5) {
        return vec2<f32>(world_pos.z, world_pos.y);
    } else {
        return vec2<f32>(world_pos.x, world_pos.y);
    }
}

// Mirror of eval::blend_rgb. Indices = BlendMode::index() order.
fn blend_rgb(mode: u32, dst: vec3<f32>, src: vec3<f32>) -> vec3<f32> {
    var out: vec3<f32>;
    switch mode {
        case 1u: { out = dst * src; }                          // multiply
        case 2u: { out = 1.0 - (1.0 - dst) * (1.0 - src); }    // screen
        case 3u: {                                             // overlay
            let lo = 2.0 * dst * src;
            let hi = 1.0 - 2.0 * (1.0 - dst) * (1.0 - src);
            out = select(hi, lo, dst < vec3<f32>(0.5));
        }
        case 4u: { out = dst + src; }                          // add
        case 5u: { out = dst - src; }                          // subtract
        case 6u: { out = max(dst, src); }                      // lighten
        case 7u: { out = min(dst, src); }                      // darken
        default: { out = src; }                                // normal
    }
    return clamp(out, vec3<f32>(0.0), vec3<f32>(1.0));
}

// ---- finish noise: mirror of noise::value_fbm_world ------------------

fn hash_u32(v: u32) -> u32 {
    var h = v;
    h ^= h >> 16u;
    h *= 0x7feb352du;
    h ^= h >> 15u;
    h *= 0x846ca68bu;
    h ^= h >> 16u;
    return h;
}

fn hash2(x: u32, y: u32, seed: u32) -> f32 {
    let h = hash_u32(x * 0x9E3779B9u ^ y * 0x85EBCA6Bu ^ seed * 0xC2B2AE35u);
    return f32(h >> 8u) / f32(1u << 24u);
}

fn fade2(t: vec2<f32>) -> vec2<f32> {
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
}

fn value_noise_world(p: vec2<f32>, seed: u32) -> f32 {
    let pi = floor(p);
    let pf = p - pi;
    let x0 = u32(i32(pi.x));
    let y0 = u32(i32(pi.y));
    let x1 = x0 + 1u;
    let y1 = y0 + 1u;
    let s = fade2(pf);
    let a = mix(hash2(x0, y0, seed), hash2(x1, y0, seed), s.x);
    let b = mix(hash2(x0, y1, seed), hash2(x1, y1, seed), s.x);
    return mix(a, b, s.y);
}

fn finish_fbm(p: vec2<f32>, seed: u32) -> f32 {
    let n = value_noise_world(p, seed) * 0.6667
        + value_noise_world(p * 2.0, seed ^ 0x9E37u) * 0.3333;
    return clamp(n, 0.0, 1.0);
}

// Display → linear, applied once after compositing.
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// Composite the display-space color of `slot`'s block at a world-space
// point: pick the face texture by dominant normal, blend the baked
// layers, apply the finish jitter. Returns the fallback color for
// untextured faces. Callers srgb_to_linear the result before PBR.
fn composite_block_color(slot: u32, world_pos: vec3<f32>, world_normal: vec3<f32>) -> vec3<f32> {
    let block = blocks[slot];
    var tex_idx = block.side;
    if (world_normal.y > 0.5) {
        tex_idx = block.top;
    } else if (world_normal.y < -0.5) {
        tex_idx = block.bottom;
    }

    var color = block.fallback.rgb;
    if (tex_idx != NO_TEXTURE) {
        let info = textures[tex_idx];
        let uv = face_uv(world_pos, world_normal);
        for (var i = 0u; i < info.layer_count; i = i + 1u) {
            let layer = layers[info.layer_start + i];
            let t = fract(uv / layer.period);
            let size = f32(layer.size_px);
            let px = min(
                vec2<u32>(t * size),
                vec2<u32>(layer.size_px - 1u, layer.size_px - 1u),
            );
            let s = textureLoad(tiles, px, i32(layer.array_index), 0);
            let cov = s.a * layer.opacity;
            color = mix(color, blend_rgb(layer.blend, color, s.rgb), cov);
        }
        if (info.finish_brightness > 0.0) {
            let n = finish_fbm(uv / info.finish_scale, info.finish_seed);
            let k = 1.0 + (n - 0.5) * 2.0 * info.finish_brightness;
            color = clamp(color * k, vec3<f32>(0.0), vec3<f32>(1.0));
        }
    }
    return color;
}
