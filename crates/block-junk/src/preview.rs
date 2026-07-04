//! Ghost & overlay materials for planned work: dithered screen-door
//! mesh ghosts (placement preview + committed Build plans on glTF
//! blocks) and the world-space demolition crosshatch (Remove plans).
//!
//! ## Why dithered, not alpha-blended
//!
//! The old preview was two translucent passes (alpha fill + multiply
//! darken). In a field of committed plans those passes accumulated into
//! mud: internal faces showed through, overlapping ghosts stacked
//! alpha, and the darken passes progressively blacked out the wall
//! behind. Screen-door transparency renders OPAQUE and discards
//! fragments in a Bayer pattern — surviving pixels write real depth, so
//! ghosts occlude themselves and each other exactly like solid
//! geometry, at any cluster size. Coverage is per-fragment, giving the
//! distance fade ("plans are harder to see from far away") for free.
//!
//! Each ghost still renders two passes of one material type, split by
//! the `xray` pipeline key:
//!   - front: base `alpha_mode = Opaque`, normal depth test + write.
//!   - x-ray: base `alpha_mode = Blend` (Transparent3d, draws after all
//!     opaques), depth compare flipped to `Less` (reversed-Z "behind
//!     world geometry"), no depth write, sparser coverage — the faint
//!     behind-wall hint that replaced the multiply darken.
//!
//! The voxel-block ghost material (same dither, but compositing the
//! block's real baked textures) lives in `block_junk_textures::render`
//! (`GhostBlockMaterial`) — it needs the tile array bindings. This
//! module owns the glTF-mesh ghost extension and the crosshatch.

use bevy::asset::embedded_asset;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{
    ExtendedMaterial, Material, MaterialExtension, MaterialExtensionKey,
    MaterialExtensionPipeline, MaterialPlugin,
};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, CompareFunction, RenderPipelineDescriptor, ShaderType,
    SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

const DITHER_MESH_SHADER_PATH: &str = "embedded://block_junk/dither_mesh.wgsl";
const CROSSHATCH_SHADER_PATH: &str = "embedded://block_junk/crosshatch.wgsl";

/// Uniform block for [`DitherExt`]. Mirrors WGSL `DitherParams` in
/// `dither_mesh.wgsl` — field order must match. 32 bytes, no padding
/// needed.
#[derive(ShaderType, Clone, Copy, Debug)]
pub struct DitherParams {
    /// Multiplies the glTF material's base color: white = the mesh's
    /// real look, red = invalid placement, amber = waiting on
    /// materials.
    pub tint: Vec4,
    /// Dither coverage in [0,1] up close.
    pub coverage: f32,
    /// Coverage floor the distance fade eases toward.
    pub min_coverage: f32,
    /// Camera distance (m) where the fade begins.
    pub fade_start: f32,
    /// Camera distance (m) where coverage reaches `min_coverage`.
    pub fade_end: f32,
}

impl DitherParams {
    /// Constant-coverage params (no distance fade) — the cursor ghost.
    pub fn fixed(tint: Vec4, coverage: f32) -> Self {
        Self {
            tint,
            coverage,
            min_coverage: coverage,
            fade_start: f32::MAX,
            fade_end: f32::MAX,
        }
    }

    /// Distance-faded params — committed plan ghosts.
    pub fn faded(
        tint: Vec4,
        coverage: f32,
        min_coverage: f32,
        fade_start: f32,
        fade_end: f32,
    ) -> Self {
        Self {
            min_coverage,
            fade_start,
            fade_end,
            ..Self::fixed(tint, coverage)
        }
    }
}

/// Pipeline-key data for [`DitherExt`] — front and x-ray variants
/// specialize into different depth states.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DitherKey {
    pub xray: bool,
}

impl From<&DitherExt> for DitherKey {
    fn from(ext: &DitherExt) -> Self {
        Self { xray: ext.xray }
    }
}

/// Extension turning a cloned glTF `StandardMaterial` into a dithered
/// ghost — the mesh keeps its real textures/colors, gains the Bayer
/// discard + tint + distance fade. See the module docs for the
/// front/x-ray split contract on the base's `alpha_mode`.
#[derive(Asset, AsBindGroup, Clone, TypePath)]
#[bind_group_data(DitherKey)]
pub struct DitherExt {
    #[uniform(100)]
    pub params: DitherParams,
    pub xray: bool,
}

impl MaterialExtension for DitherExt {
    fn fragment_shader() -> ShaderRef {
        DITHER_MESH_SHADER_PATH.into()
    }

    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if key.bind_group_data.xray
            && let Some(ds) = descriptor.depth_stencil.as_mut()
        {
            // Reversed-Z: `Less` keeps fragments FARTHER than the depth
            // buffer — i.e. behind existing geometry. No depth write so
            // the hint pass leaves no trace.
            ds.depth_compare = Some(CompareFunction::Less);
            ds.depth_write_enabled = Some(false);
        }
        Ok(())
    }
}

pub type DitherMeshMaterial = ExtendedMaterial<StandardMaterial, DitherExt>;

/// On a `WorldAssetRoot` whose glTF materials should be re-created as
/// dithered ghosts. The material-swap observer (in `client.rs`) fires
/// on `WorldInstanceReady` for any root carrying this, clones each
/// descendant mesh's real `StandardMaterial` into front + x-ray
/// [`DitherMeshMaterial`] instances with these params, and records the
/// created handles in [`GhostMeshMaterials`] on the root.
#[derive(Component, Clone, Copy)]
pub struct GhostMeshStyle {
    pub front: DitherParams,
    pub xray: DitherParams,
}

/// Handles of the [`DitherMeshMaterial`] assets created for one ghost
/// scene root. The cursor preview mutates their params per frame
/// (valid/invalid re-tint); committed plan ghosts leave them fixed.
#[derive(Component, Default)]
pub struct GhostMeshMaterials {
    pub front: Vec<Handle<DitherMeshMaterial>>,
    pub xray: Vec<Handle<DitherMeshMaterial>>,
}

/// Inserted on a ghost scene root once the material swap completed.
/// Until then the scene stays `Visibility::Hidden` so the player never
/// sees a frame of the original glTF materials.
#[derive(Component)]
pub struct PreviewSceneReady;

/// Uniform block for [`CrosshatchMaterial`]. Mirrors WGSL
/// `CrosshatchParams` in `crosshatch.wgsl` — field order and explicit
/// padding must match (uniform structs round to 16-byte multiples).
#[derive(ShaderType, Clone, Copy, Debug)]
pub struct CrosshatchParams {
    /// Stripe color; alpha is the up-close stripe opacity (~0.5).
    pub color: Vec4,
    /// World-space stripe repeat distance in metres.
    pub period: f32,
    /// Fraction of each period that is stripe (0..1).
    pub duty: f32,
    /// Alpha *ratio* floor the distance fade eases toward (0.2 = fade
    /// to 20% of base opacity, never to nothing).
    pub min_alpha: f32,
    /// Camera distance (m) where the fade begins.
    pub fade_start: f32,
    /// Camera distance (m) where the fade bottoms out.
    pub fade_end: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
}

/// World-space red crosshatch drawn over every cell tagged for
/// demolition: two diagonal stripe sets derived purely from world
/// coordinates — stable in 3D, no UVs, so one shared material covers
/// terrain cubes and mesh-block cells alike, and the pattern lines up
/// seamlessly across adjacent tagged cells. A cancelled cell in a
/// cluster reads as an obvious hole in the pattern field (the per-cell
/// wireframes this replaces couldn't show that: neighbours re-traced
/// the hole's every edge).
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct CrosshatchMaterial {
    #[uniform(0)]
    pub params: CrosshatchParams,
}

impl Material for CrosshatchMaterial {
    fn fragment_shader() -> ShaderRef {
        CROSSHATCH_SHADER_PATH.into()
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
}

/// Plugin: registers the materials and embeds their shaders so the
/// binary is self-contained. The dither helper library
/// (`block_junk_textures::dither`) is registered by
/// `ChunkMaterialPlugin` — added before this by `BlockTexturesPlugin`.
pub struct PreviewPlugin;

impl Plugin for PreviewPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "dither_mesh.wgsl");
        embedded_asset!(app, "crosshatch.wgsl");
        app.add_plugins(MaterialPlugin::<DitherMeshMaterial>::default());
        app.add_plugins(MaterialPlugin::<CrosshatchMaterial>::default());
    }
}
