//! GPU side of the texture system (feature `render`): the chunk material
//! extension, the baked-tile texture array, and the storage-buffer layouts
//! the shader walks. Shared by the game client and texture-studio so both
//! render through the *same* shader — the studio preview is the real thing.
//!
//! Data flow:
//! 1. [`crate::bake::bake_set`] → `Vec<BakedTexture>` (CPU tiles).
//! 2. [`build_gpu_textures`] packs every layer tile into one fixed-slot
//!    `texture_2d_array` Image and emits [`TexInfoGpu`] / [`LayerGpu`]
//!    tables, plus an id → texture-index map.
//! 3. The consumer builds a `Vec<BlockTexGpu>` (one per block slot,
//!    resolving texture ids per face) and assembles a [`ChunkMaterial`].
//!
//! The shader samples tiles with `textureLoad` (no sampler): layer tiles
//! of different world periods have different pixel extents inside a fixed
//! array slot, so manual wrap + nearest fetch is both necessary and
//! exactly the chunky pixel look we want.
//!
//! TODO(texture-aliasing): textureLoad means no mips — distant terrain
//! will shimmer at high frequencies, same as the old Nearest atlas. If it
//! bothers, bake mip chains per tile and select level by derivative.

use std::collections::HashMap;

use bevy::asset::{RenderAssetUsages, embedded_asset};
use bevy::image::Image;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{
    ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline,
    MaterialPlugin,
};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, CompareFunction, Extent3d, RenderPipelineDescriptor, ShaderType,
    SpecializedMeshPipelineError, TextureDimension, TextureFormat, TextureViewDescriptor,
    TextureViewDimension,
};
use bevy::render::storage::ShaderBuffer;
use bevy::shader::{ShaderRef, load_shader_library};

use crate::bake::BakedTexture;

/// Sentinel for "this face has no texture; use the fallback color".
pub const NO_TEXTURE: u32 = u32::MAX;

const SHADER_PATH: &str = "embedded://block_junk_textures/render/chunk_material.wgsl";

/// Per-block-slot face textures + fallback. Mirrors WGSL `BlockTex`.
#[derive(ShaderType, Clone, Copy, Debug)]
pub struct BlockTexGpu {
    /// Texture index for +Y faces, [`NO_TEXTURE`] for none.
    pub top: u32,
    /// Texture index for ±X / ±Z faces.
    pub side: u32,
    /// Texture index for -Y faces.
    pub bottom: u32,
    pub _pad: u32,
    /// Display-space RGBA used when a face has no texture.
    pub fallback: Vec4,
}

impl BlockTexGpu {
    pub fn untextured(color: [f32; 3]) -> Self {
        Self {
            top: NO_TEXTURE,
            side: NO_TEXTURE,
            bottom: NO_TEXTURE,
            _pad: 0,
            fallback: Vec4::new(color[0], color[1], color[2], 1.0),
        }
    }
}

/// Per-texture record. Mirrors WGSL `TexInfo`.
#[derive(ShaderType, Clone, Copy, Debug, Default)]
pub struct TexInfoGpu {
    pub layer_start: u32,
    pub layer_count: u32,
    /// 0 disables the finish pass.
    pub finish_brightness: f32,
    pub finish_scale: f32,
    pub finish_seed: u32,
}

/// Per-baked-layer record. Mirrors WGSL `LayerInfo`.
#[derive(ShaderType, Clone, Copy, Debug)]
pub struct LayerGpu {
    pub array_index: u32,
    /// Used pixel extent inside the (possibly larger) array slot.
    pub size_px: u32,
    /// World repeat in blocks.
    pub period: f32,
    /// [`crate::doc::BlendMode::index`].
    pub blend: u32,
    pub opacity: f32,
}

/// Everything [`build_gpu_textures`] produces. The `tiles` Image is ready
/// to add to `Assets<Image>`; the tables go into `ShaderBuffer`s.
pub struct GpuTextureSet {
    pub tiles: Image,
    pub textures: Vec<TexInfoGpu>,
    pub layers: Vec<LayerGpu>,
    /// Texture id → index into `textures` (what block defs resolve to).
    pub index_by_id: HashMap<String, u32>,
    /// Array slot side length in pixels (max baked tile size).
    pub slot_size: u32,
}

impl GpuTextureSet {
    pub fn index_of(&self, id: &str) -> Option<u32> {
        self.index_by_id.get(id).copied()
    }
}

/// Pack baked textures into the fixed-slot tile array + lookup tables.
pub fn build_gpu_textures(baked: &[BakedTexture]) -> GpuTextureSet {
    let slot_size = baked
        .iter()
        .flat_map(|t| t.layers.iter().map(|l| l.size))
        .max()
        .unwrap_or(1)
        .max(1);

    let layer_count: usize = baked.iter().map(|t| t.layers.len()).sum();
    let slot_bytes = (slot_size * slot_size * 4) as usize;
    // At least one (transparent) array layer so the binding stays valid
    // even with zero textures loaded.
    let mut data = vec![0u8; slot_bytes * layer_count.max(1)];

    let mut textures = Vec::with_capacity(baked.len());
    let mut layers = Vec::with_capacity(layer_count);
    let mut index_by_id = HashMap::with_capacity(baked.len());

    for tex in baked {
        index_by_id.insert(tex.id.clone(), textures.len() as u32);
        let layer_start = layers.len() as u32;
        for l in &tex.layers {
            let array_index = layers.len() as u32;
            // Blit the tile into the top-left of its slot.
            let base = array_index as usize * slot_bytes;
            for row in 0..l.size {
                let src = (row * l.size * 4) as usize;
                let dst = base + (row * slot_size * 4) as usize;
                data[dst..dst + (l.size * 4) as usize]
                    .copy_from_slice(&l.rgba[src..src + (l.size * 4) as usize]);
            }
            layers.push(LayerGpu {
                array_index,
                size_px: l.size,
                period: l.period as f32,
                blend: l.blend.index(),
                opacity: l.opacity,
            });
        }
        let finish = tex.finish.unwrap_or(crate::doc::FinishDef {
            brightness: 0.0,
            ..Default::default()
        });
        textures.push(TexInfoGpu {
            layer_start,
            layer_count: tex.layers.len() as u32,
            finish_brightness: finish.brightness,
            finish_scale: finish.scale.max(1e-3),
            finish_seed: finish.seed,
        });
    }

    // Rgba8Unorm (NOT Srgb): the shader blends layers in display space so
    // CPU previews/icons match the composite exactly, then converts the
    // final color to linear once before PBR. An Srgb view would decode
    // per-fetch and change the blend math.
    let mut tiles = Image::new(
        Extent3d {
            width: slot_size,
            height: slot_size,
            depth_or_array_layers: layer_count.max(1) as u32,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    tiles.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..TextureViewDescriptor::default()
    });

    GpuTextureSet {
        tiles,
        textures,
        layers,
        index_by_id,
        slot_size,
    }
}

/// Material extension over StandardMaterial. Bindings start at 100 per
/// the extended-material convention (0..99 belong to the base material).
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct BlockTextureExt {
    #[texture(100, dimension = "2d_array")]
    pub tiles: Handle<Image>,
    #[storage(101, read_only)]
    pub blocks: Handle<ShaderBuffer>,
    #[storage(102, read_only)]
    pub textures: Handle<ShaderBuffer>,
    #[storage(103, read_only)]
    pub layers: Handle<ShaderBuffer>,
}

impl MaterialExtension for BlockTextureExt {
    fn fragment_shader() -> ShaderRef {
        SHADER_PATH.into()
    }
    fn deferred_fragment_shader() -> ShaderRef {
        SHADER_PATH.into()
    }
}

pub type ChunkMaterial = ExtendedMaterial<StandardMaterial, BlockTextureExt>;

const GHOST_SHADER_PATH: &str = "embedded://block_junk_textures/render/ghost_block.wgsl";

/// Uniform block for [`GhostBlockExt`]. Mirrors WGSL `GhostParams` in
/// `ghost_block.wgsl` — field order and explicit padding must match.
#[derive(ShaderType, Clone, Copy, Debug)]
pub struct GhostParams {
    /// Multiplies the composited display-space color: white = the
    /// block's real look, red = invalid placement, amber = waiting on
    /// materials.
    pub tint: Vec4,
    /// Block slot whose textures to composite (ghost cubes carry no
    /// vertex-color slot encoding).
    pub slot: u32,
    /// Dither coverage in [0,1] up close.
    pub coverage: f32,
    /// Coverage floor the distance fade eases toward — keep nonzero so
    /// far-away plans stay a faint sketch instead of vanishing.
    pub min_coverage: f32,
    /// Camera distance (m) where the fade begins.
    pub fade_start: f32,
    /// Camera distance (m) where coverage reaches `min_coverage`. Set
    /// `min_coverage == coverage` to disable fading (cursor ghost).
    pub fade_end: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
}

impl GhostParams {
    /// Constant-coverage params (no distance fade) — the cursor ghost.
    pub fn fixed(slot: u32, tint: Vec4, coverage: f32) -> Self {
        Self {
            tint,
            slot,
            coverage,
            min_coverage: coverage,
            fade_start: f32::MAX,
            fade_end: f32::MAX,
            _pad0: 0.0,
            _pad1: 0.0,
            _pad2: 0.0,
        }
    }

    /// Distance-faded params — committed plan ghosts.
    pub fn faded(
        slot: u32,
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
            ..Self::fixed(slot, tint, coverage)
        }
    }
}

/// Pipeline-key data for [`GhostBlockExt`]: the front and x-ray
/// variants of one material type specialize into different pipelines
/// (depth state) keyed by this.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GhostBlockKey {
    pub xray: bool,
}

impl From<&GhostBlockExt> for GhostBlockKey {
    fn from(ext: &GhostBlockExt) -> Self {
        Self { xray: ext.xray }
    }
}

/// Extension rendering a single block's composited texture as a
/// dithered screen-door ghost. Shares the tile array + lookup tables
/// with [`BlockTextureExt`] (same handles, zero extra GPU uploads);
/// adds the per-ghost uniform.
///
/// Two variants of the same type, split by `xray` via the pipeline key:
///   - `xray: false` — base `alpha_mode` MUST be `Opaque`: renders in
///     the Opaque pass, writes depth, ghosts occlude each other.
///   - `xray: true` — base `alpha_mode` MUST be `Blend`: routes to
///     Transparent3d (draws after all opaques), depth compare flipped
///     to `Less` (reversed-Z "behind world geometry"), no depth write —
///     the sparse behind-wall hint.
#[derive(Asset, AsBindGroup, Clone, TypePath)]
#[bind_group_data(GhostBlockKey)]
pub struct GhostBlockExt {
    #[texture(100, dimension = "2d_array")]
    pub tiles: Handle<Image>,
    #[storage(101, read_only)]
    pub blocks: Handle<ShaderBuffer>,
    #[storage(102, read_only)]
    pub textures: Handle<ShaderBuffer>,
    #[storage(103, read_only)]
    pub layers: Handle<ShaderBuffer>,
    #[uniform(104)]
    pub params: GhostParams,
    pub xray: bool,
}

impl MaterialExtension for GhostBlockExt {
    fn fragment_shader() -> ShaderRef {
        GHOST_SHADER_PATH.into()
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

pub type GhostBlockMaterial = ExtendedMaterial<StandardMaterial, GhostBlockExt>;

/// Registers the embedded shaders + the materials. Add once per app
/// (game client and texture-studio both use it).
pub struct ChunkMaterialPlugin;

impl Plugin for ChunkMaterialPlugin {
    fn build(&self, app: &mut App) {
        load_shader_library!(app, "render/composite.wgsl");
        load_shader_library!(app, "render/dither.wgsl");
        embedded_asset!(app, "render/chunk_material.wgsl");
        embedded_asset!(app, "render/ghost_block.wgsl");
        app.add_plugins(MaterialPlugin::<ChunkMaterial>::default());
        app.add_plugins(MaterialPlugin::<GhostBlockMaterial>::default());
    }
}
