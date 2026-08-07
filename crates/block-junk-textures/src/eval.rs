//! Layer evaluation: run a step list, produce an RGBA tile.
//!
//! Grey steps fill [`GreyBuf`]s; paint steps composite onto the layer's
//! [`ColorBuf`] canvas. The canvas starts transparent — its final alpha is
//! the layer's coverage, which the shader (and [`crate::bake::flatten`])
//! multiplies into the layer's `opacity` when compositing layers.

use std::collections::HashMap;

use crate::doc::{BlendMode, LayerDef, RampStop};
use crate::error::TexError;
use crate::noise;
use crate::ops::{OpCategory, Params};

/// Square scalar field, row-major, values nominally 0..=1.
#[derive(Clone, Debug)]
pub struct GreyBuf {
    pub size: u32,
    pub data: Vec<f32>,
}

impl GreyBuf {
    pub fn new(size: u32) -> Self {
        Self {
            size,
            data: vec![0.0; (size * size) as usize],
        }
    }

    /// Fill from a function of pixel-centre UV.
    pub fn fill(size: u32, mut f: impl FnMut(f32, f32) -> f32) -> Self {
        let mut buf = Self::new(size);
        for y in 0..size {
            for x in 0..size {
                let u = (x as f32 + 0.5) / size as f32;
                let v = (y as f32 + 0.5) / size as f32;
                buf.data[(y * size + x) as usize] = f(u, v);
            }
        }
        buf
    }

    #[inline]
    pub fn at(&self, x: u32, y: u32) -> f32 {
        self.data[(y * self.size + x) as usize]
    }

    #[inline]
    fn at_wrapped(&self, x: i64, y: i64) -> f32 {
        let s = self.size as i64;
        self.data[((y.rem_euclid(s)) * s + x.rem_euclid(s)) as usize]
    }

    /// Bilinear sample with wrap, `u`/`v` in tile space (any real).
    pub fn sample_wrap(&self, u: f32, v: f32) -> f32 {
        let s = self.size as f32;
        // Pixel centres sit at (i + 0.5) / size.
        let x = u * s - 0.5;
        let y = v * s - 0.5;
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        let (x0, y0) = (x0 as i64, y0 as i64);
        let a = self.at_wrapped(x0, y0);
        let b = self.at_wrapped(x0 + 1, y0);
        let c = self.at_wrapped(x0, y0 + 1);
        let d = self.at_wrapped(x0 + 1, y0 + 1);
        let ab = a + fx * (b - a);
        let cd = c + fx * (d - c);
        ab + fy * (cd - ab)
    }
}

/// Square RGBA field, straight (non-premultiplied) alpha.
#[derive(Clone, Debug)]
pub struct ColorBuf {
    pub size: u32,
    pub data: Vec<[f32; 4]>,
}

impl ColorBuf {
    pub fn new(size: u32) -> Self {
        Self {
            size,
            data: vec![[0.0; 4]; (size * size) as usize],
        }
    }

    /// 8-bit RGBA bytes (sRGB-as-stored: values are treated as display
    /// colors end to end, no gamma conversion on the CPU side).
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for px in &self.data {
            for c in px {
                out.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            }
        }
        out
    }
}

/// Blend `src` over `dst` (both RGB). Mirrors `blend_rgb` in the chunk
/// WGSL — change one, change both.
pub fn blend_rgb(mode: BlendMode, dst: [f32; 3], src: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0; 3];
    for i in 0..3 {
        let d = dst[i];
        let s = src[i];
        out[i] = match mode {
            BlendMode::Normal => s,
            BlendMode::Multiply => d * s,
            BlendMode::Screen => 1.0 - (1.0 - d) * (1.0 - s),
            BlendMode::Overlay => {
                if d < 0.5 {
                    2.0 * d * s
                } else {
                    1.0 - 2.0 * (1.0 - d) * (1.0 - s)
                }
            }
            BlendMode::Add => d + s,
            BlendMode::Subtract => d - s,
            BlendMode::Lighten => d.max(s),
            BlendMode::Darken => d.min(s),
        }
        .clamp(0.0, 1.0);
    }
    out
}

/// Composite one paint sample onto a canvas pixel: blend the RGB, then
/// lerp by coverage; alpha accumulates with the standard over-operator.
#[inline]
pub fn composite_px(canvas: &mut [f32; 4], src_rgb: [f32; 3], coverage: f32, mode: BlendMode) {
    let cov = coverage.clamp(0.0, 1.0);
    if cov <= 0.0 {
        return;
    }
    let dst = [canvas[0], canvas[1], canvas[2]];
    let blended = blend_rgb(mode, dst, src_rgb);
    for i in 0..3 {
        canvas[i] += (blended[i] - canvas[i]) * cov;
    }
    canvas[3] += (1.0 - canvas[3]) * cov;
}

/// Piecewise-linear ramp lookup. Stops must be sorted by pos (validation
/// guarantees it); clamps outside the range.
pub fn sample_ramp(stops: &[RampStop], t: f32) -> [f32; 3] {
    match stops {
        [] => [t, t, t],
        [only] => only.color,
        _ => {
            if t <= stops[0].pos {
                return stops[0].color;
            }
            for w in stops.windows(2) {
                let (a, b) = (w[0], w[1]);
                if t <= b.pos {
                    let span = (b.pos - a.pos).max(1e-6);
                    let f = (t - a.pos) / span;
                    return [
                        a.color[0] + (b.color[0] - a.color[0]) * f,
                        a.color[1] + (b.color[1] - a.color[1]) * f,
                        a.color[2] + (b.color[2] - a.color[2]) * f,
                    ];
                }
            }
            stops[stops.len() - 1].color
        }
    }
}

/// Named outputs of one grey step: `(channel, buffer)` with the primary
/// channel first.
type Outputs = Vec<(&'static str, GreyBuf)>;

struct EvalCtx {
    size: u32,
    named: HashMap<String, Outputs>,
    prev: Option<Outputs>,
}

impl EvalCtx {
    /// Resolve `"name"` / `"name.channel"` against named outputs.
    fn resolve(&self, refstr: &str, at: &str) -> Result<&GreyBuf, TexError> {
        let (name, channel) = match refstr.split_once('.') {
            Some((n, c)) => (n, Some(c)),
            None => (refstr, None),
        };
        let outputs = self.named.get(name).ok_or_else(|| {
            TexError::invalid(at, format!("reference to unknown output \"{name}\""))
        })?;
        match channel {
            None => Ok(&outputs[0].1),
            Some(c) => outputs
                .iter()
                .find(|(n, _)| *n == c)
                .map(|(_, b)| b)
                .ok_or_else(|| {
                    TexError::invalid(at, format!("output \"{name}\" has no channel \"{c}\""))
                }),
        }
    }

    /// The input a filter falls back to when `input`/`a` is omitted.
    fn prev_primary(&self, at: &str) -> Result<&GreyBuf, TexError> {
        self.prev
            .as_ref()
            .map(|o| &o[0].1)
            .ok_or_else(|| TexError::invalid(at, "no previous grey step to use as input"))
    }

    fn input(&self, p: &Params, param: &str, at: &str) -> Result<GreyBuf, TexError> {
        match p.reference(param) {
            Some(r) => self.resolve(r, at).cloned(),
            None => self.prev_primary(at).cloned(),
        }
    }
}

/// What one step produced, for editor thumbnails: grey steps yield their
/// primary output, paint steps a snapshot of the canvas after them.
#[derive(Clone, Debug)]
pub enum StepTrace {
    Grey(GreyBuf),
    Canvas(ColorBuf),
}

/// Evaluate one layer's step list into an RGBA canvas of `size`² pixels.
/// `at_prefix` seeds error paths (e.g. `texture "vanilla:grass" / layer 1`).
pub fn eval_layer(layer: &LayerDef, size: u32, at_prefix: &str) -> Result<ColorBuf, TexError> {
    eval_layer_impl(layer, size, at_prefix, None)
}

/// [`eval_layer`] that also records a per-step [`StepTrace`] (one entry
/// per step, in order) — the editor's live thumbnail strip.
pub fn eval_layer_traced(
    layer: &LayerDef,
    size: u32,
    at_prefix: &str,
) -> Result<(ColorBuf, Vec<StepTrace>), TexError> {
    let mut trace = Vec::with_capacity(layer.steps.len());
    let canvas = eval_layer_impl(layer, size, at_prefix, Some(&mut trace))?;
    Ok((canvas, trace))
}

fn eval_layer_impl(
    layer: &LayerDef,
    size: u32,
    at_prefix: &str,
    mut trace: Option<&mut Vec<StepTrace>>,
) -> Result<ColorBuf, TexError> {
    let mut canvas = ColorBuf::new(size);
    let mut ctx = EvalCtx {
        size,
        named: HashMap::new(),
        prev: None,
    };
    for (i, step) in layer.steps.iter().enumerate() {
        let at = format!("{at_prefix} / step {} ({})", i + 1, step.op);
        let p = Params::new(step)
            .ok_or_else(|| TexError::invalid(&at, format!("unknown op \"{}\"", step.op)))?;
        match p.spec.category {
            OpCategory::GreyGen | OpCategory::GreyFilter => {
                let outputs = eval_grey_op(&p, &ctx, &at)?;
                if let Some(t) = trace.as_deref_mut() {
                    t.push(StepTrace::Grey(outputs[0].1.clone()));
                }
                if let Some(name) = &step.out {
                    ctx.named.insert(name.clone(), outputs.clone());
                }
                ctx.prev = Some(outputs);
            }
            OpCategory::Paint => {
                eval_paint_op(&p, &ctx, &mut canvas, &at)?;
                if let Some(t) = trace.as_deref_mut() {
                    t.push(StepTrace::Canvas(canvas.clone()));
                }
            }
        }
    }
    Ok(canvas)
}

fn eval_grey_op(p: &Params, ctx: &EvalCtx, at: &str) -> Result<Outputs, TexError> {
    let size = ctx.size;
    let spec = p.spec;
    let buf = match spec.name {
        "fbm" => {
            let (cells, octaves, gain, seed) = (
                p.u32("cells"),
                p.u32("octaves"),
                p.f32("gain"),
                p.u32("seed"),
            );
            GreyBuf::fill(size, |u, v| {
                noise::fbm_periodic(u, v, cells, octaves, gain, seed)
            })
        }
        "voronoi" => {
            let (cells, jitter, seed) = (p.u32("cells"), p.f32("jitter"), p.u32("seed"));
            let mut edge = GreyBuf::new(size);
            let mut f1b = GreyBuf::new(size);
            let mut cellb = GreyBuf::new(size);
            for y in 0..size {
                for x in 0..size {
                    let u = (x as f32 + 0.5) / size as f32;
                    let v = (y as f32 + 0.5) / size as f32;
                    let (f1, f2, cr) = noise::worley_periodic(u, v, cells, jitter, seed);
                    let i = (y * size + x) as usize;
                    edge.data[i] = (f2 - f1).clamp(0.0, 1.0);
                    f1b.data[i] = f1.clamp(0.0, 1.0);
                    cellb.data[i] = cr;
                }
            }
            return Ok(vec![("edge", edge), ("f1", f1b), ("cells", cellb)]);
        }
        "white" => {
            let seed = p.u32("seed");
            let mut buf = GreyBuf::new(size);
            for y in 0..size {
                for x in 0..size {
                    buf.data[(y * size + x) as usize] = noise::hash2(x, y, seed);
                }
            }
            buf
        }
        "gradient" => match p.enum_str("dir") {
            "x" => GreyBuf::fill(size, |u, _| u),
            "radial" => GreyBuf::fill(size, |u, v| {
                let dx = (u - 0.5) * 2.0;
                let dy = (v - 0.5) * 2.0;
                (1.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
            }),
            _ => GreyBuf::fill(size, |_, v| v),
        },
        "shape" => {
            let kind = p.enum_str("kind").to_owned();
            let s = p.f32("size");
            let soft = p.f32("softness").max(1.0 / size as f32);
            GreyBuf::fill(size, |u, v| {
                let dx = (u - 0.5).abs() * 2.0;
                let dy = (v - 0.5).abs() * 2.0;
                let d = match kind.as_str() {
                    "box" => dx.max(dy),
                    "diamond" => dx + dy,
                    _ => (dx * dx + dy * dy).sqrt(),
                };
                1.0 - smoothstep(s - soft, s + soft, d)
            })
        }
        "bricks" => {
            let cols = p.u32("columns").max(1) as f32;
            let rows = p.u32("rows").max(1) as f32;
            let gap = p.f32("gap");
            let bevel = p.f32("bevel").max(1e-4);
            let offset = p.f32("offset");
            let seed = p.u32("seed");
            let mut field = GreyBuf::new(size);
            let mut cellb = GreyBuf::new(size);
            for y in 0..size {
                for x in 0..size {
                    let u = (x as f32 + 0.5) / size as f32;
                    let v = (y as f32 + 0.5) / size as f32;
                    let row = (v * rows).floor();
                    let shifted = u + (row as i64 % 2 != 0) as i32 as f32 * (offset / cols);
                    let col = (shifted * cols).floor();
                    let lu = shifted * cols - col;
                    let lv = v * rows - row;
                    // Distance to the nearest brick border, as a fraction
                    // of the brick cell (0 at border, 0.5 at centre).
                    let d = lu.min(1.0 - lu).min(lv.min(1.0 - lv));
                    let i = (y * size + x) as usize;
                    field.data[i] = smoothstep(gap * 0.5, gap * 0.5 + bevel, d);
                    // Wrap col so the cells output tiles like the field.
                    let wc = (col as i64).rem_euclid(cols as i64) as u32;
                    let wr = (row as i64).rem_euclid(rows as i64) as u32;
                    cellb.data[i] = noise::hash2(wc, wr, seed);
                }
            }
            return Ok(vec![("field", field), ("cells", cellb)]);
        }
        "checker" => {
            let n = p.u32("count").max(1) as f32;
            GreyBuf::fill(size, |u, v| {
                (((u * n).floor() + (v * n).floor()) as i64 % 2) as f32
            })
        }
        "stripes" => {
            let n = p.u32("count").max(1) as f32;
            let duty = p.f32("duty");
            GreyBuf::fill(size, |_, v| ((v * n).fract() < duty) as i32 as f32)
        }
        "scratches" => {
            let count = p.u32("count");
            let len = p.f32("length");
            let width = p.f32("width");
            let seed = p.u32("seed");
            // Random segments in tile space. Coverage = max over segments
            // of a soft line profile; distances computed against the 9
            // wrapped copies of each segment so strokes tile seamlessly.
            let segs: Vec<([f32; 2], [f32; 2])> = (0..count)
                .map(|i| {
                    let ax = noise::hash2(i, 0, seed);
                    let ay = noise::hash2(i, 1, seed);
                    let ang = noise::hash2(i, 2, seed) * std::f32::consts::TAU;
                    let l = len * (0.5 + 0.5 * noise::hash2(i, 3, seed));
                    ([ax, ay], [ax + ang.cos() * l, ay + ang.sin() * l])
                })
                .collect();
            GreyBuf::fill(size, |u, v| {
                let mut best = f32::INFINITY;
                for (a, b) in &segs {
                    for wy in -1..=1 {
                        for wx in -1..=1 {
                            let off = [wx as f32, wy as f32];
                            let d = dist_point_segment([u + off[0], v + off[1]], *a, *b);
                            best = best.min(d);
                        }
                    }
                }
                1.0 - smoothstep(width * 0.5, width, best)
            })
        }
        "levels" => {
            let input = ctx.input(p, "input", at)?;
            let (in_lo, in_hi) = (p.f32("in_lo"), p.f32("in_hi"));
            let (out_lo, out_hi) = (p.f32("out_lo"), p.f32("out_hi"));
            let gamma = p.f32("gamma").max(0.01);
            let span = (in_hi - in_lo).max(1e-6);
            map_buf(input, |x| {
                let t = ((x - in_lo) / span).clamp(0.0, 1.0).powf(gamma);
                out_lo + (out_hi - out_lo) * t
            })
        }
        "invert" => map_buf(ctx.input(p, "input", at)?, |x| 1.0 - x),
        "quantize" => {
            let steps = p.u32("steps").max(2) as f32;
            map_buf(ctx.input(p, "input", at)?, move |x| {
                ((x * steps).floor().min(steps - 1.0)) / (steps - 1.0)
            })
        }
        "threshold" => {
            let th = p.f32("threshold");
            let soft = p.f32("softness");
            map_buf(ctx.input(p, "input", at)?, move |x| {
                smoothstep(th - soft, th + soft, x)
            })
        }
        "blur" => box_blur(&ctx.input(p, "input", at)?, p.f32("radius")),
        "edge" => {
            let input = ctx.input(p, "input", at)?;
            let strength = p.f32("strength");
            let mut out = GreyBuf::new(size);
            let s = size as i64;
            for y in 0..s {
                for x in 0..s {
                    let v = |dx: i64, dy: i64| input.at_wrapped(x + dx, y + dy);
                    let gx = (v(1, -1) + 2.0 * v(1, 0) + v(1, 1))
                        - (v(-1, -1) + 2.0 * v(-1, 0) + v(-1, 1));
                    let gy = (v(-1, 1) + 2.0 * v(0, 1) + v(1, 1))
                        - (v(-1, -1) + 2.0 * v(0, -1) + v(1, -1));
                    out.data[(y * s + x) as usize] =
                        ((gx * gx + gy * gy).sqrt() * strength * 0.25).clamp(0.0, 1.0);
                }
            }
            out
        }
        "transform" => {
            let input = ctx.input(p, "input", at)?;
            let scale = p.u32("scale").max(1) as f32;
            let rot = p.enum_str("rotate").to_owned();
            let off = p.vec2("offset");
            GreyBuf::fill(size, |u, v| {
                let (mut su, mut sv) = match rot.as_str() {
                    "90" => (v, 1.0 - u),
                    "180" => (1.0 - u, 1.0 - v),
                    "270" => (1.0 - v, u),
                    _ => (u, v),
                };
                su = su * scale + off[0];
                sv = sv * scale + off[1];
                input.sample_wrap(su, sv)
            })
        }
        "warp" => {
            let input = ctx.input(p, "input", at)?;
            let by_ref = p
                .reference("by")
                .ok_or_else(|| TexError::invalid(at, "warp requires a \"by\" reference"))?;
            let by = ctx.resolve(by_ref, at)?.clone();
            let amount = p.f32("amount");
            GreyBuf::fill(size, |u, v| {
                // Two decorrelated reads of the driver give a 2D vector.
                let dx = by.sample_wrap(u, v) - 0.5;
                let dy = by.sample_wrap(u + 0.37, v + 0.19) - 0.5;
                input.sample_wrap(u + dx * amount * 2.0, v + dy * amount * 2.0)
            })
        }
        "math" => {
            let a = ctx.input(p, "a", at)?;
            let b_ref = p
                .reference("b")
                .ok_or_else(|| TexError::invalid(at, "math requires a \"b\" reference"))?;
            let b = ctx.resolve(b_ref, at)?;
            let mode = p.enum_str("mode").to_owned();
            let mut out = GreyBuf::new(size);
            for i in 0..out.data.len() {
                let (x, y) = (a.data[i], b.data[i]);
                out.data[i] = match mode.as_str() {
                    "add" => x + y,
                    "subtract" => x - y,
                    "min" => x.min(y),
                    "max" => x.max(y),
                    "average" => (x + y) * 0.5,
                    "difference" => (x - y).abs(),
                    _ => x * y,
                }
                .clamp(0.0, 1.0);
            }
            out
        }
        other => {
            return Err(TexError::invalid(
                at,
                format!("grey op \"{other}\" has no evaluator (spec/eval drift)"),
            ));
        }
    };
    Ok(vec![(
        spec.outputs.first().copied().unwrap_or("value"),
        buf,
    )])
}

fn eval_paint_op(
    p: &Params,
    ctx: &EvalCtx,
    canvas: &mut ColorBuf,
    at: &str,
) -> Result<(), TexError> {
    let size = ctx.size;
    let blend = BlendMode::parse(p.enum_str("blend")).unwrap_or_default();
    let opacity = p.f32("opacity");
    let mask = match p.reference("mask") {
        Some(r) => Some(ctx.resolve(r, at)?.clone()),
        None => None,
    };
    match p.spec.name {
        "fill" => {
            let c = p.color("color");
            for y in 0..size {
                for x in 0..size {
                    let i = (y * size + x) as usize;
                    let m = mask.as_ref().map_or(1.0, |m| m.data[i]);
                    composite_px(
                        &mut canvas.data[i],
                        [c[0], c[1], c[2]],
                        c[3] * opacity * m,
                        blend,
                    );
                }
            }
        }
        "ramp" => {
            let input = ctx.input(p, "input", at)?;
            let stops = p.stops("stops");
            for y in 0..size {
                for x in 0..size {
                    let i = (y * size + x) as usize;
                    let m = mask.as_ref().map_or(1.0, |m| m.data[i]);
                    let rgb = sample_ramp(&stops, input.data[i]);
                    composite_px(&mut canvas.data[i], rgb, opacity * m, blend);
                }
            }
        }
        other => {
            return Err(TexError::invalid(
                at,
                format!("paint op \"{other}\" has no evaluator (spec/eval drift)"),
            ));
        }
    }
    Ok(())
}

fn map_buf(mut buf: GreyBuf, f: impl Fn(f32) -> f32) -> GreyBuf {
    for v in &mut buf.data {
        *v = f(*v).clamp(0.0, 1.0);
    }
    buf
}

/// Separable box blur with wrap. Radius in pixels, rounded.
fn box_blur(input: &GreyBuf, radius: f32) -> GreyBuf {
    let r = radius.round() as i64;
    if r <= 0 {
        return input.clone();
    }
    let s = input.size as i64;
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut tmp = GreyBuf::new(input.size);
    for y in 0..s {
        for x in 0..s {
            let mut sum = 0.0;
            for d in -r..=r {
                sum += input.at_wrapped(x + d, y);
            }
            tmp.data[(y * s + x) as usize] = sum * norm;
        }
    }
    let mut out = GreyBuf::new(input.size);
    for y in 0..s {
        for x in 0..s {
            let mut sum = 0.0;
            for d in -r..=r {
                sum += tmp.at_wrapped(x, y + d);
            }
            out.data[(y * s + x) as usize] = sum * norm;
        }
    }
    out
}

#[inline]
fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    if hi <= lo {
        return if x < lo { 0.0 } else { 1.0 };
    }
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn dist_point_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [p[0] - a[0], p[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 <= 1e-12 {
        0.0
    } else {
        ((ap[0] * ab[0] + ap[1] * ab[1]) / len2).clamp(0.0, 1.0)
    };
    let dx = ap[0] - ab[0] * t;
    let dy = ap[1] - ab[1] * t;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{ParamValue, Step};

    fn step(op: &str, params: &[(&str, ParamValue)], out: Option<&str>) -> Step {
        Step {
            op: op.to_owned(),
            out: out.map(str::to_owned),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        }
    }

    fn stops() -> ParamValue {
        ParamValue::Stops(vec![
            RampStop {
                pos: 0.0,
                color: [0.1, 0.1, 0.1],
            },
            RampStop {
                pos: 1.0,
                color: [0.9, 0.9, 0.9],
            },
        ])
    }

    #[test]
    fn layer_eval_is_deterministic_and_opaque() {
        let layer = LayerDef {
            steps: vec![
                step("fbm", &[("seed", ParamValue::U32(3))], Some("n")),
                step(
                    "ramp",
                    &[("input", ParamValue::Str("n".into())), ("stops", stops())],
                    None,
                ),
            ],
            ..LayerDef::default()
        };
        let a = eval_layer(&layer, 32, "test").unwrap();
        let b = eval_layer(&layer, 32, "test").unwrap();
        assert_eq!(a.to_rgba8(), b.to_rgba8());
        // Unmasked ramp paints everywhere → fully opaque canvas.
        assert!(a.data.iter().all(|px| px[3] > 0.999));
    }

    #[test]
    fn implicit_prev_input_chains() {
        let layer = LayerDef {
            steps: vec![
                step("fbm", &[], None),
                step("invert", &[], None), // takes prev implicitly
                step("ramp", &[("stops", stops())], None),
            ],
            ..LayerDef::default()
        };
        eval_layer(&layer, 16, "test").unwrap();
    }

    #[test]
    fn multi_output_channels_resolve() {
        let layer = LayerDef {
            steps: vec![
                step("voronoi", &[], Some("v")),
                step(
                    "ramp",
                    &[
                        ("input", ParamValue::Str("v.cells".into())),
                        ("stops", stops()),
                    ],
                    None,
                ),
            ],
            ..LayerDef::default()
        };
        eval_layer(&layer, 16, "test").unwrap();
    }

    #[test]
    fn unknown_ref_errors_with_path() {
        let layer = LayerDef {
            steps: vec![step(
                "ramp",
                &[
                    ("input", ParamValue::Str("nope".into())),
                    ("stops", stops()),
                ],
                None,
            )],
            ..LayerDef::default()
        };
        let err = eval_layer(&layer, 16, "texture \"t\" / layer 1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope") && msg.contains("step 1"), "{msg}");
    }

    #[test]
    fn mask_gates_paint_coverage() {
        let layer = LayerDef {
            steps: vec![
                step(
                    "gradient",
                    &[("dir", ParamValue::Str("x".into()))],
                    Some("g"),
                ),
                step(
                    "fill",
                    &[
                        ("color", ParamValue::Color([1.0, 0.0, 0.0, 1.0])),
                        ("mask", ParamValue::Str("g".into())),
                    ],
                    None,
                ),
            ],
            ..LayerDef::default()
        };
        let c = eval_layer(&layer, 32, "test").unwrap();
        // Left edge ~0 coverage, right edge ~1.
        assert!(c.data[0][3] < 0.05);
        assert!(c.data[31][3] > 0.9);
    }

    #[test]
    fn blur_preserves_mean() {
        let mut layer = LayerDef {
            steps: vec![
                step("white", &[], None),
                step("blur", &[("radius", ParamValue::F32(3.0))], Some("b")),
            ],
            ..LayerDef::default()
        };
        // Evaluate via the grey path by referencing in a fill mask.
        layer.steps.push(step(
            "fill",
            &[
                ("color", ParamValue::Color([1.0, 1.0, 1.0, 1.0])),
                ("mask", ParamValue::Str("b".into())),
            ],
            None,
        ));
        let c = eval_layer(&layer, 32, "test").unwrap();
        let mean: f32 = c.data.iter().map(|p| p[3]).sum::<f32>() / c.data.len() as f32;
        assert!((mean - 0.5).abs() < 0.1, "mean {mean}");
    }
}
