//! Placement-preview rendering: a translucent stand-in of the would-be
//! entity drawn in the world before the player commits to placing it.
//!
//! The look is two passes layered together:
//!   - Front pass: alpha-blended translucent fill. Drawn with Bevy's
//!     standard reversed-Z depth test (`GreaterEqual`), so it shows up
//!     where it would actually be visible at full opacity.
//!   - Back pass: multiplicative darkening. Drawn with depth-test
//!     flipped to `Less`, so only the parts of the preview *behind*
//!     existing geometry survive — they multiply the framebuffer down,
//!     like an X-ray shadow on the wall.
//!
//! Both passes share one fragment shader (`preview.wgsl`) that just
//! outputs the material's uniform `color`; the per-pass divergence lives
//! in `Material::specialize` (depth-compare + blend state) so we don't
//! need a custom render graph node or a depth-prepass sample.
//!
//! Material handles for the four (front/back × valid/invalid) combos are
//! managed by `client.rs` — this module only builds the types and a
//! plugin that registers them with the renderer.

use bevy::asset::embedded_asset;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, CompareFunction,
    RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

/// Path the AssetServer resolves to the embedded `preview.wgsl`. Both
/// materials reference it via `Material::fragment_shader`.
pub const PREVIEW_SHADER_PATH: &str = "embedded://block_junk/preview.wgsl";

/// Front pass: alpha-blended translucent fill, default depth test, no
/// depth-write. The `color` uniform is RGBA; alpha controls how strong
/// the ghost reads.
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct PreviewFront {
    #[uniform(0)]
    pub color: LinearRgba,
}

impl Material for PreviewFront {
    fn fragment_shader() -> ShaderRef {
        PREVIEW_SHADER_PATH.into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
    fn enable_shadows() -> bool {
        false
    }
    fn enable_prepass() -> bool {
        false
    }
    // No `specialize` needed for the front pass: `AlphaMode::Blend`
    // already sets up alpha blending and depth_write=false. We rely on
    // Bevy's reversed-Z default `GreaterEqual` depth-compare, which
    // makes the front pass behave like any other transparent draw.
}

/// Back pass: multiplicative darken behind existing geometry. RGB of the
/// `color` uniform controls how dark; alpha is unused (the multiply
/// blend ignores the alpha channel).
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct PreviewBack {
    #[uniform(0)]
    pub color: LinearRgba,
}

impl Material for PreviewBack {
    fn fragment_shader() -> ShaderRef {
        PREVIEW_SHADER_PATH.into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        // Blend gets us into the Transparent3d pass so the per-fragment
        // pipeline state we install in `specialize` is honoured. The
        // actual blend formula is overridden below.
        AlphaMode::Blend
    }
    fn enable_shadows() -> bool {
        false
    }
    fn enable_prepass() -> bool {
        false
    }
    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // Multiply blend: result_rgb = src_rgb * dst_rgb. The fragment
        // shader writes the darken factor (e.g. (0.6, 0.6, 0.6, 1)) and
        // the framebuffer's existing colour gets multiplied down.
        if let Some(target) = descriptor
            .fragment
            .as_mut()
            .and_then(|f| f.targets.first_mut().and_then(|t| t.as_mut()))
        {
            target.blend = Some(BlendState {
                color: BlendComponent {
                    src_factor: BlendFactor::Dst,
                    dst_factor: BlendFactor::Zero,
                    operation: BlendOperation::Add,
                },
                alpha: BlendComponent {
                    src_factor: BlendFactor::Zero,
                    dst_factor: BlendFactor::One,
                    operation: BlendOperation::Add,
                },
            });
        }
        // Bevy uses reversed-Z depth: closer-to-camera = larger depth
        // value, default compare is `GreaterEqual`. To pick out fragments
        // BEHIND existing geometry we need the inverse — `Less` means
        // "current frag is farther than what's already there." Combined
        // with depth-write off, the back pass leaves no trace in the
        // depth buffer.
        if let Some(ds) = descriptor.depth_stencil.as_mut() {
            ds.depth_compare = CompareFunction::Less;
            ds.depth_write_enabled = false;
        }
        Ok(())
    }
}

/// Plugin: registers both materials with the renderer and embeds the
/// shader so the binary is self-contained (no `assets/shaders/...` to
/// ship at runtime).
pub struct PreviewPlugin;

impl Plugin for PreviewPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "preview.wgsl");
        app.add_plugins(MaterialPlugin::<PreviewFront>::default());
        app.add_plugins(MaterialPlugin::<PreviewBack>::default());
    }
}
