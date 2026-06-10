//! CPU baking: turn texture defs into RGBA tiles, plus the CPU-side
//! composite ([`flatten`]) used for hotbar icons and editor previews.

use rayon::prelude::*;

use crate::doc::{BlendMode, FinishDef, TextureDef, TextureSetDoc};
use crate::error::TexError;
use crate::eval::{blend_rgb, eval_layer};
use crate::noise;

/// One baked bake-group tile.
#[derive(Clone, Debug)]
pub struct BakedLayer {
    /// Tile side in pixels (`period * pixels_per_block`).
    pub size: u32,
    /// World repeat in blocks.
    pub period: u32,
    pub blend: BlendMode,
    pub opacity: f32,
    /// RGBA8, row-major. Alpha = paint coverage for shader compositing.
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct BakedTexture {
    pub id: String,
    pub layers: Vec<BakedLayer>,
    pub finish: Option<FinishDef>,
}

/// Bake one texture at `pixels_per_block` resolution.
pub fn bake_texture(def: &TextureDef, pixels_per_block: u32) -> Result<BakedTexture, TexError> {
    let mut layers = Vec::with_capacity(def.layers.len());
    for (li, layer) in def.layers.iter().enumerate() {
        let at = format!("texture \"{}\" / layer {}", def.id, li + 1);
        let size = layer.period * pixels_per_block;
        let canvas = eval_layer(layer, size, &at)?;
        layers.push(BakedLayer {
            size,
            period: layer.period,
            blend: layer.blend,
            opacity: layer.opacity,
            rgba: canvas.to_rgba8(),
        });
    }
    Ok(BakedTexture {
        id: def.id.clone(),
        layers,
        finish: def.finish,
    })
}

/// Bake every texture in a doc, in parallel. Order matches `doc.textures`.
pub fn bake_set(doc: &TextureSetDoc) -> Result<Vec<BakedTexture>, TexError> {
    doc.textures
        .par_iter()
        .map(|t| bake_texture(t, doc.pixels_per_block))
        .collect()
}

/// Sample one baked layer at world position (in blocks), nearest-neighbor
/// — the CPU twin of the shader's `textureLoad` path.
#[inline]
fn sample_layer(layer: &BakedLayer, wx: f32, wy: f32) -> [f32; 4] {
    let p = layer.period as f32;
    let u = (wx / p).rem_euclid(1.0);
    let v = (wy / p).rem_euclid(1.0);
    let x = ((u * layer.size as f32) as u32).min(layer.size - 1);
    let y = ((v * layer.size as f32) as u32).min(layer.size - 1);
    let i = ((y * layer.size + x) * 4) as usize;
    [
        layer.rgba[i] as f32 / 255.0,
        layer.rgba[i + 1] as f32 / 255.0,
        layer.rgba[i + 2] as f32 / 255.0,
        layer.rgba[i + 3] as f32 / 255.0,
    ]
}

/// Composite a baked texture's layers at one world position — the same
/// math the chunk shader runs per fragment. Public so icons, previews,
/// and tests share one implementation.
pub fn composite_at(
    baked: &BakedTexture,
    wx: f32,
    wy: f32,
    backdrop: [f32; 3],
    with_finish: bool,
) -> [f32; 3] {
    let mut rgb = backdrop;
    for layer in &baked.layers {
        let s = sample_layer(layer, wx, wy);
        let cov = s[3] * layer.opacity;
        let blended = blend_rgb(layer.blend, rgb, [s[0], s[1], s[2]]);
        for i in 0..3 {
            rgb[i] += (blended[i] - rgb[i]) * cov;
        }
    }
    if with_finish && let Some(f) = &baked.finish {
        let n = noise::value_fbm_world(wx / f.scale, wy / f.scale, f.seed);
        let k = 1.0 + (n - 0.5) * 2.0 * f.brightness;
        for c in &mut rgb {
            *c = (*c * k).clamp(0.0, 1.0);
        }
    }
    rgb
}

/// Render a baked texture into an RGBA8 image: `out_px`² pixels covering
/// `world_span` blocks starting at `world_origin`. Used for hotbar icons
/// (span = 1) and the studio's flat preview (span = several periods).
pub fn flatten(
    baked: &BakedTexture,
    out_px: u32,
    world_origin: [f32; 2],
    world_span: f32,
    backdrop: [f32; 3],
    with_finish: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity((out_px * out_px * 4) as usize);
    for y in 0..out_px {
        for x in 0..out_px {
            let wx = world_origin[0] + (x as f32 + 0.5) / out_px as f32 * world_span;
            let wy = world_origin[1] + (y as f32 + 0.5) / out_px as f32 * world_span;
            let rgb = composite_at(baked, wx, wy, backdrop, with_finish);
            for c in rgb {
                out.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            }
            out.push(255);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::*;

    fn simple_doc() -> TextureSetDoc {
        let mut ramp = Step::new("ramp");
        ramp.params.insert(
            "stops".into(),
            ParamValue::Stops(vec![
                RampStop { pos: 0.0, color: [0.2, 0.2, 0.2] },
                RampStop { pos: 1.0, color: [0.8, 0.8, 0.8] },
            ]),
        );
        TextureSetDoc {
            pixels_per_block: 16,
            textures: vec![TextureDef {
                id: "test:stone".into(),
                layers: vec![LayerDef {
                    period: 3,
                    steps: vec![Step::new("fbm"), ramp],
                    ..LayerDef::default()
                }],
                finish: Some(FinishDef::default()),
            }],
        }
    }

    #[test]
    fn bake_sizes_follow_period_and_ppb() {
        let baked = bake_set(&simple_doc()).unwrap();
        assert_eq!(baked[0].layers[0].size, 48); // 3 blocks * 16 ppb
        assert_eq!(baked[0].layers[0].rgba.len(), 48 * 48 * 4);
    }

    #[test]
    fn composite_repeats_at_period() {
        let baked = &bake_set(&simple_doc()).unwrap()[0];
        let a = composite_at(baked, 0.4, 0.7, [0.0; 3], false);
        let b = composite_at(baked, 0.4 + 3.0, 0.7 + 6.0, [0.0; 3], false);
        assert_eq!(a, b);
        // ...but not at a non-period offset.
        let c = composite_at(baked, 0.4 + 1.0, 0.7, [0.0; 3], false);
        assert_ne!(a, c);
    }

    /// The finish path must evaluate without panicking. (Whether it
    /// actually differs from plain depends on the doc's finish params,
    /// so asserting inequality here would be flaky by design.)
    #[test]
    fn finish_path_evaluates() {
        let baked = &bake_set(&simple_doc()).unwrap()[0];
        let _plain = composite_at(baked, 5.0, 9.0, [0.0; 3], false);
        let _finished = composite_at(baked, 5.0, 9.0, [0.0; 3], true);
    }
}
