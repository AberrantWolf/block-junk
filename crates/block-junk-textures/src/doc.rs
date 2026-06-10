//! The texture document model — what `textures.lua` parses into and what
//! the editor mutates. Pure data, no evaluation logic.

use std::collections::BTreeMap;

/// Hard cap on a layer's world period in blocks. Slot size in the GPU
/// texture array is `max_period * pixels_per_block` per side, so this cap
/// bounds atlas memory. 8 blocks at 16 ppb = 128×128 per slot.
pub const MAX_PERIOD: u32 = 8;

/// Cap on bake-group layers per texture. Each costs one texture fetch per
/// fragment, so this is a per-pixel cost knob, not a memory knob.
pub const MAX_LAYERS_PER_TEXTURE: usize = 4;

/// Used when a doc doesn't set `pixels_per_block`.
pub const DEFAULT_PIXELS_PER_BLOCK: u32 = 16;

/// One parsed `textures.lua`. `pixels_per_block` is per-doc — a mod that
/// wants chunkier or finer pixel art sets it for its whole file rather
/// than sprinkling resolutions per texture.
#[derive(Clone, Debug, PartialEq)]
pub struct TextureSetDoc {
    pub pixels_per_block: u32,
    pub textures: Vec<TextureDef>,
}

impl Default for TextureSetDoc {
    fn default() -> Self {
        Self {
            pixels_per_block: DEFAULT_PIXELS_PER_BLOCK,
            textures: Vec::new(),
        }
    }
}

impl TextureSetDoc {
    pub fn texture(&self, id: &str) -> Option<&TextureDef> {
        self.textures.iter().find(|t| t.id == id)
    }
}

/// One named texture: an ordered list of bake-group layers plus an
/// optional per-fragment finish.
#[derive(Clone, Debug, PartialEq)]
pub struct TextureDef {
    /// `"namespace:name"`, same convention as block ids.
    pub id: String,
    pub layers: Vec<LayerDef>,
    pub finish: Option<FinishDef>,
}

/// One bake group. Bakes to a `(period * pixels_per_block)²` RGBA tile;
/// the shader samples it in world space (`fract(face_uv / period)`) and
/// composites it over the layers below with `blend` + `opacity` scaled by
/// the tile's own alpha (paint coverage).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerDef {
    /// World repeat in blocks, `1..=MAX_PERIOD`. Coprime periods across
    /// layers multiply the visible repeat distance.
    pub period: u32,
    pub blend: BlendMode,
    pub opacity: f32,
    pub steps: Vec<Step>,
}

impl Default for LayerDef {
    fn default() -> Self {
        Self {
            period: 1,
            blend: BlendMode::Normal,
            opacity: 1.0,
            steps: Vec::new(),
        }
    }
}

/// Cheap per-fragment world-space brightness jitter applied in the chunk
/// shader *after* the baked layers — the "shader finish" half of the
/// hybrid evaluation. Kills any residual tiling that survives the
/// coprime-period trick, at the cost of one 2-octave value noise per
/// fragment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FinishDef {
    /// World units per noise feature. Big (8–24) = broad patchiness.
    pub scale: f32,
    /// ± brightness amplitude. 0.05 reads as subtle ground variation.
    pub brightness: f32,
    pub seed: u32,
}

impl Default for FinishDef {
    fn default() -> Self {
        Self {
            scale: 12.0,
            brightness: 0.05,
            seed: 0,
        }
    }
}

/// One step in a layer. `op` names an [`OpSpec`](crate::ops::OpSpec);
/// `params` holds only the values that differ from the spec defaults
/// (parse fills nothing in — accessors fall back to the spec).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Step {
    pub op: String,
    /// Names this step's outputs for later `input` / `mask` / `a` / `b`
    /// refs (`"name"` = primary output, `"name.channel"` = secondary).
    pub out: Option<String>,
    pub params: BTreeMap<String, ParamValue>,
}

impl Step {
    pub fn new(op: &str) -> Self {
        Self {
            op: op.to_owned(),
            ..Self::default()
        }
    }
}

/// Dynamically-typed param value. The schema (which keys exist, their
/// types, ranges, defaults) lives in the op's [`ParamSpec`](crate::ops::ParamSpec)
/// list — this is just storage.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    F32(f32),
    U32(u32),
    /// Enum option or output reference, depending on the param type.
    Str(String),
    Color([f32; 4]),
    Vec2([f32; 2]),
    Stops(Vec<RampStop>),
}

/// One color-ramp stop. Stops are kept sorted by `pos`; lookup is
/// piecewise-linear with clamp-to-end outside the range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RampStop {
    pub pos: f32,
    pub color: [f32; 3],
}

/// Photoshop-style blend modes, used both by paint steps (within a layer
/// bake) and by the shader compositing baked layers. The Rust and WGSL
/// implementations must stay in sync — see `eval::blend_rgb` and the
/// `blend_rgb` function in `render/chunk_material.wgsl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Add,
    Subtract,
    Lighten,
    Darken,
}

impl BlendMode {
    pub const ALL: [BlendMode; 8] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Add,
        BlendMode::Subtract,
        BlendMode::Lighten,
        BlendMode::Darken,
    ];

    pub fn name(self) -> &'static str {
        match self {
            BlendMode::Normal => "normal",
            BlendMode::Multiply => "multiply",
            BlendMode::Screen => "screen",
            BlendMode::Overlay => "overlay",
            BlendMode::Add => "add",
            BlendMode::Subtract => "subtract",
            BlendMode::Lighten => "lighten",
            BlendMode::Darken => "darken",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == s)
    }

    /// Stable index shared with the WGSL `blend_rgb` switch.
    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|m| *m == self).unwrap() as u32
    }
}
