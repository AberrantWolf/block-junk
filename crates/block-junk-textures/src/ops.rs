//! The op vocabulary, declared as data.
//!
//! Every op is described by an [`OpSpec`]: its category, named outputs,
//! and a [`ParamSpec`] list with type/range/default/doc per param. The Lua
//! parser validates against this table, the evaluator reads params through
//! it (so missing params fall back to defaults), and texture-studio
//! auto-generates its inspector UI from it. Adding an op = one entry here
//! + one match arm in `eval::eval_grey_op` (or `eval::eval_paint_op`).

use crate::doc::{ParamValue, RampStop, Step};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpCategory {
    /// Produces grey output(s) from nothing (noise, patterns, shapes).
    GreyGen,
    /// Produces grey output(s) from grey input(s).
    GreyFilter,
    /// Composites color onto the layer canvas.
    Paint,
}

#[derive(Clone, Copy, Debug)]
pub enum ParamType {
    F32 { min: f32, max: f32, default: f32 },
    U32 { min: u32, max: u32, default: u32 },
    Enum { options: &'static [&'static str], default: &'static str },
    /// RGBA, components 0..=1. Alpha scales paint coverage.
    Color { default: [f32; 4] },
    /// Reference to a named grey output (`"name"` / `"name.channel"`).
    /// Non-required refs named `input`/`a` default to the previous grey
    /// step's primary output; a non-required `mask`/`b` defaults to "none".
    Ref { required: bool },
    /// Color-ramp stop list. Always required where it appears.
    Stops,
    /// u32, default 0. Split from U32 so the editor can show a 🎲 button.
    Seed,
    Vec2 { default: [f32; 2] },
}

#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct OpSpec {
    pub name: &'static str,
    pub category: OpCategory,
    /// Named outputs; first is the primary (what a bare `"name"` ref or
    /// the implicit prev-chain resolves to). Empty for paint ops.
    pub outputs: &'static [&'static str],
    pub params: &'static [ParamSpec],
    pub doc: &'static str,
}

const fn p(name: &'static str, ty: ParamType, doc: &'static str) -> ParamSpec {
    ParamSpec { name, ty, doc }
}

/// Shared tail params for paint ops.
const BLEND: ParamSpec = p(
    "blend",
    ParamType::Enum {
        options: &[
            "normal", "multiply", "screen", "overlay", "add", "subtract", "lighten", "darken",
        ],
        default: "normal",
    },
    "How this paint combines with what's already on the canvas",
);
const OPACITY: ParamSpec = p(
    "opacity",
    ParamType::F32 { min: 0.0, max: 1.0, default: 1.0 },
    "Overall strength of this paint",
);
const MASK: ParamSpec = p(
    "mask",
    ParamType::Ref { required: false },
    "Grey output gating where this paint lands (1 = full, 0 = none)",
);
const INPUT: ParamSpec = p(
    "input",
    ParamType::Ref { required: false },
    "Grey input; defaults to the previous grey step's output",
);

pub const OPS: &[OpSpec] = &[
    // ------------------------------------------------------- generators
    OpSpec {
        name: "fbm",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[
            p("cells", ParamType::U32 { min: 1, max: 64, default: 4 },
              "Base lattice cells per tile; higher = finer features"),
            p("octaves", ParamType::U32 { min: 1, max: 8, default: 4 },
              "Noise layers; each doubles frequency and halves amplitude by gain"),
            p("gain", ParamType::F32 { min: 0.0, max: 1.0, default: 0.5 },
              "Amplitude falloff per octave; higher = rougher"),
            p("seed", ParamType::Seed, "Noise variation"),
        ],
        doc: "Fractal Perlin noise, tileable. The everything-noise: dirt, clouds, grain.",
    },
    OpSpec {
        name: "voronoi",
        category: OpCategory::GreyGen,
        outputs: &["edge", "f1", "cells"],
        params: &[
            p("cells", ParamType::U32 { min: 1, max: 64, default: 6 },
              "Feature points per tile side"),
            p("jitter", ParamType::F32 { min: 0.0, max: 1.0, default: 1.0 },
              "Point randomness; 0 = perfect grid"),
            p("seed", ParamType::Seed, "Point placement variation"),
        ],
        doc: "Cellular noise. edge = bright inside cells, dark at borders (stones!); \
              f1 = distance to nearest point; cells = flat random value per cell.",
    },
    OpSpec {
        name: "white",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[p("seed", ParamType::Seed, "Variation")],
        doc: "Independent random value per pixel. Speckle, static, dithering input.",
    },
    OpSpec {
        name: "gradient",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[p(
            "dir",
            ParamType::Enum { options: &["x", "y", "radial"], default: "y" },
            "Ramp direction across the tile",
        )],
        doc: "0→1 ramp across the tile. Block-aligned (seam lands on the period edge).",
    },
    OpSpec {
        name: "shape",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[
            p("kind", ParamType::Enum { options: &["circle", "box", "diamond"], default: "circle" },
              "Shape function"),
            p("size", ParamType::F32 { min: 0.0, max: 1.0, default: 0.6 },
              "Extent, as a fraction of the tile"),
            p("softness", ParamType::F32 { min: 0.0, max: 1.0, default: 0.05 },
              "Edge falloff width"),
        ],
        doc: "One centred SDF shape, 1 inside fading to 0 at the edge. Combine with \
              transform (scale) for grids of studs, bolts, tiles.",
    },
    OpSpec {
        name: "bricks",
        category: OpCategory::GreyGen,
        outputs: &["field", "cells"],
        params: &[
            p("columns", ParamType::U32 { min: 1, max: 32, default: 4 }, "Bricks per row"),
            p("rows", ParamType::U32 { min: 1, max: 32, default: 8 }, "Rows per tile"),
            p("gap", ParamType::F32 { min: 0.0, max: 0.4, default: 0.06 },
              "Mortar width, as a fraction of brick size"),
            p("bevel", ParamType::F32 { min: 0.0, max: 0.5, default: 0.1 },
              "Falloff from mortar (0) to brick face (1)"),
            p("offset", ParamType::F32 { min: 0.0, max: 1.0, default: 0.5 },
              "Alternate-row horizontal shift; 0.5 = classic running bond"),
            p("seed", ParamType::Seed, "Per-brick random (cells output)"),
        ],
        doc: "Brick/plank grid. field = bevelled height (0 in mortar, 1 on faces); \
              cells = flat random per brick for color variation. Rotate with transform \
              for vertical planks.",
    },
    OpSpec {
        name: "checker",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[p("count", ParamType::U32 { min: 1, max: 32, default: 2 },
                    "Squares per tile side")],
        doc: "Alternating 0/1 squares.",
    },
    OpSpec {
        name: "stripes",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[
            p("count", ParamType::U32 { min: 1, max: 64, default: 4 }, "Stripes per tile"),
            p("duty", ParamType::F32 { min: 0.0, max: 1.0, default: 0.5 },
              "Fraction of each stripe that is 1"),
        ],
        doc: "Horizontal stripes (rotate with transform for vertical). Planks seams, \
              pipes, hazard bands.",
    },
    OpSpec {
        name: "scratches",
        category: OpCategory::GreyGen,
        outputs: &["value"],
        params: &[
            p("count", ParamType::U32 { min: 1, max: 64, default: 12 }, "Number of segments"),
            p("length", ParamType::F32 { min: 0.01, max: 1.0, default: 0.3 },
              "Segment length, tile fraction"),
            p("width", ParamType::F32 { min: 0.001, max: 0.2, default: 0.01 },
              "Stroke width, tile fraction"),
            p("seed", ParamType::Seed, "Placement variation"),
        ],
        doc: "Random thin line segments, wrapped at tile edges. Wear, wood grain \
              accents, wire clutter.",
    },
    // -------------------------------------------------------- filters
    OpSpec {
        name: "levels",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("in_lo", ParamType::F32 { min: 0.0, max: 1.0, default: 0.0 }, "Input black point"),
            p("in_hi", ParamType::F32 { min: 0.0, max: 1.0, default: 1.0 }, "Input white point"),
            p("gamma", ParamType::F32 { min: 0.1, max: 8.0, default: 1.0 },
              "Midtone curve; >1 darkens"),
            p("out_lo", ParamType::F32 { min: 0.0, max: 1.0, default: 0.0 }, "Output black point"),
            p("out_hi", ParamType::F32 { min: 0.0, max: 1.0, default: 1.0 }, "Output white point"),
        ],
        doc: "Remap value range with a gamma curve. The contrast workhorse.",
    },
    OpSpec {
        name: "invert",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[INPUT],
        doc: "1 - value.",
    },
    OpSpec {
        name: "quantize",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("steps", ParamType::U32 { min: 2, max: 32, default: 4 }, "Output levels"),
        ],
        doc: "Posterize to N flat levels. Instant cartoon shading.",
    },
    OpSpec {
        name: "threshold",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("threshold", ParamType::F32 { min: 0.0, max: 1.0, default: 0.5 },
              "Smoothstep midpoint"),
            p("softness", ParamType::F32 { min: 0.0, max: 0.5, default: 0.05 },
              "Half-width; 0 = hard cartoon edge"),
        ],
        doc: "Soft cutoff: 0 below, 1 above. Turns any noise into a mask.",
    },
    OpSpec {
        name: "blur",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("radius", ParamType::F32 { min: 0.0, max: 16.0, default: 2.0 }, "Radius in pixels"),
        ],
        doc: "Box blur, wrapping at tile edges.",
    },
    OpSpec {
        name: "edge",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("strength", ParamType::F32 { min: 0.1, max: 16.0, default: 2.0 },
              "Gradient magnitude multiplier"),
        ],
        doc: "Sobel edge detect. Outlines brick mortar, cell borders, panel seams.",
    },
    OpSpec {
        name: "transform",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("scale", ParamType::U32 { min: 1, max: 16, default: 2 },
              "Repeat the input N× across the tile (integer keeps it tileable)"),
            p("rotate", ParamType::Enum { options: &["0", "90", "180", "270"], default: "0" },
              "Rotation in 90° steps (arbitrary angles would break tiling)"),
            p("offset", ParamType::Vec2 { default: [0.0, 0.0] }, "UV shift, wraps"),
        ],
        doc: "Tile/rotate/shift a grey field.",
    },
    OpSpec {
        name: "warp",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            INPUT,
            p("by", ParamType::Ref { required: true },
              "Grey field that drives the distortion"),
            p("amount", ParamType::F32 { min: 0.0, max: 0.5, default: 0.1 },
              "Max displacement, tile fraction"),
        ],
        doc: "Domain-warp input by another field. THE op that makes procedural \
              stop looking procedural — melts grids, gnarls noise.",
    },
    OpSpec {
        name: "math",
        category: OpCategory::GreyFilter,
        outputs: &["value"],
        params: &[
            p("a", ParamType::Ref { required: false },
              "First operand; defaults to the previous grey step"),
            p("b", ParamType::Ref { required: true }, "Second operand"),
            p("mode", ParamType::Enum {
                options: &["add", "subtract", "multiply", "min", "max", "average", "difference"],
                default: "multiply",
            }, "Combine function (clamped to 0..1)"),
        ],
        doc: "Combine two grey fields.",
    },
    // ---------------------------------------------------------- paint
    OpSpec {
        name: "fill",
        category: OpCategory::Paint,
        outputs: &[],
        params: &[
            p("color", ParamType::Color { default: [0.5, 0.5, 0.5, 1.0] },
              "Paint color; alpha scales coverage"),
            BLEND,
            OPACITY,
            MASK,
        ],
        doc: "Flat color over the canvas (through mask/opacity/blend).",
    },
    OpSpec {
        name: "ramp",
        category: OpCategory::Paint,
        outputs: &[],
        params: &[
            INPUT,
            p("stops", ParamType::Stops,
              "Grey → color mapping; piecewise-linear between stops"),
            BLEND,
            OPACITY,
            MASK,
        ],
        doc: "Colorize a grey field through a gradient. The signature op of the \
              game's masks-and-ramps look.",
    },
];

pub fn find_op(name: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|o| o.name == name)
}

/// Typed parameter access for one step, falling back to the spec default
/// when the param is absent. Wrong-typed values are treated as absent —
/// `validate_doc` rejects those up front, so eval never sees them.
pub struct Params<'a> {
    pub step: &'a Step,
    pub spec: &'static OpSpec,
}

impl<'a> Params<'a> {
    pub fn new(step: &'a Step) -> Option<Self> {
        Some(Self {
            step,
            spec: find_op(&step.op)?,
        })
    }

    fn spec_of(&self, name: &str) -> Option<&'static ParamSpec> {
        self.spec.params.iter().find(|p| p.name == name)
    }

    pub fn f32(&self, name: &str) -> f32 {
        if let Some(ParamValue::F32(v)) = self.step.params.get(name) {
            return *v;
        }
        match self.spec_of(name).map(|p| p.ty) {
            Some(ParamType::F32 { default, .. }) => default,
            _ => 0.0,
        }
    }

    pub fn u32(&self, name: &str) -> u32 {
        if let Some(ParamValue::U32(v)) = self.step.params.get(name) {
            return *v;
        }
        match self.spec_of(name).map(|p| p.ty) {
            Some(ParamType::U32 { default, .. }) => default,
            Some(ParamType::Seed) => 0,
            _ => 0,
        }
    }

    pub fn enum_str(&self, name: &str) -> &str {
        if let Some(ParamValue::Str(v)) = self.step.params.get(name) {
            return v;
        }
        match self.spec_of(name).map(|p| p.ty) {
            Some(ParamType::Enum { default, .. }) => default,
            _ => "",
        }
    }

    /// A `Ref` param's value, or `None` when absent (caller decides the
    /// implicit-prev / no-mask fallback).
    pub fn reference(&self, name: &str) -> Option<&str> {
        match self.step.params.get(name) {
            Some(ParamValue::Str(v)) => Some(v.as_str()),
            _ => None,
        }
    }

    pub fn color(&self, name: &str) -> [f32; 4] {
        if let Some(ParamValue::Color(v)) = self.step.params.get(name) {
            return *v;
        }
        match self.spec_of(name).map(|p| p.ty) {
            Some(ParamType::Color { default }) => default,
            _ => [0.0; 4],
        }
    }

    pub fn vec2(&self, name: &str) -> [f32; 2] {
        if let Some(ParamValue::Vec2(v)) = self.step.params.get(name) {
            return *v;
        }
        match self.spec_of(name).map(|p| p.ty) {
            Some(ParamType::Vec2 { default }) => default,
            _ => [0.0; 2],
        }
    }

    pub fn stops(&self, name: &str) -> Vec<RampStop> {
        if let Some(ParamValue::Stops(v)) = self.step.params.get(name) {
            return v.clone();
        }
        // Stops are required by validation; an unreachable fallback that
        // still renders *something* beats a panic in the editor.
        vec![
            RampStop { pos: 0.0, color: [0.0, 0.0, 0.0] },
            RampStop { pos: 1.0, color: [1.0, 1.0, 1.0] },
        ]
    }
}
