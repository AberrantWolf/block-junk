//! Procedural texture system shared by the game and `texture-studio`.
//!
//! A **texture** is a list of bake-group **layers**. Each layer has its own
//! world `period` (how many blocks the baked tile spans before repeating)
//! and bakes independently on the CPU at load; the chunk shader composites
//! the layers per-fragment in world space. Coprime periods (1, 3, 7, …)
//! multiply: the combined pattern only repeats at their LCM, which is what
//! kills the repeating-texture look without per-fragment noise.
//!
//! Inside a layer, an ordered list of **steps** runs top to bottom:
//!
//! - *grey* steps (generators + filters) compute scalar fields. They never
//!   touch the canvas; they exist to be referenced. A step's `out = "name"`
//!   makes its outputs addressable as `"name"` / `"name.channel"`. A
//!   filter's `input` defaults to the previous grey step's primary output,
//!   so simple chains don't need names at all.
//! - *paint* steps (`fill`, `ramp`) composite color onto the layer canvas
//!   through a blend mode, opacity, and optional grey `mask`.
//!
//! That's a DAG serialized as a list — full reuse power, list-shaped
//! editing. The op vocabulary is declared as data ([`ops::OpSpec`]) so the
//! editor UI, the Lua parser, and validation all derive from one table.
//!
//! Everything is deterministic: same doc + same seed params ⇒ identical
//! bytes on every machine.

pub mod bake;
pub mod doc;
pub mod error;
pub mod eval;
pub mod lua_io;
pub mod noise;
pub mod ops;
pub mod validate;

#[cfg(feature = "egui-fonts")]
pub mod egui_fonts;
#[cfg(feature = "render")]
pub mod render;

pub use bake::{BakedLayer, BakedTexture, bake_set, bake_texture, flatten};
pub use doc::{
    BlendMode, DEFAULT_PIXELS_PER_BLOCK, FinishDef, LayerDef, MAX_LAYERS_PER_TEXTURE, MAX_PERIOD,
    ParamValue, RampStop, Step, TextureDef, TextureSetDoc,
};
pub use error::TexError;
pub use eval::{ColorBuf, GreyBuf, StepTrace, eval_layer, eval_layer_traced};
pub use ops::{OPS, OpCategory, OpSpec, ParamSpec, ParamType, find_op};
pub use validate::validate_doc;
