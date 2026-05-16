//! Per-block textures and the chunk-rendering material extension.
//!
//! Every registered block slot gets a 16×16 RGBA8 image at client startup,
//! generated procedurally from the block's `color` and `pattern` fields.
//! That image is the *base* color — the chunk fragment shader samples it,
//! then composites a per-block stack of mask+ramp **layers** on top before
//! handing the result to PBR lighting.
//!
//! Layers, masks, and ramps are all **mod-defined**. Mods register named
//! [`MaskDef`]s and [`RampDef`]s via `engine.masks.register` /
//! `engine.ramps.register`, and reference them by id in each
//! [`BlockDef.layers`] entry. The engine bakes those defs into texture-
//! array atlases at boot and resolves the ids to slot indices that the
//! shader can use directly. A stack with `num_layers = 0` reproduces the
//! pre-layer look exactly, so blocks without configured layers are
//! unaffected.
//!
//! Resources bound by [`BlockTextureExt`]:
//!
//! - The base block atlas (`texture_2d_array`, one layer per slot,
//!   Nearest + Repeat).
//! - A *mask atlas* (`texture_2d_array`, R8 grayscale, Linear + Repeat) —
//!   tile-able patterns baked from each registered mask's [`MaskSource`].
//! - A *ramp atlas* (`texture_2d_array`, RGBA8, Linear + Clamp) — 1-pixel-
//!   tall color gradients interpolated from each registered ramp's stops.
//! - A storage buffer of `LayerStack` (one entry per block slot).
//!
//! If a mod registers zero masks or zero ramps, the atlases are built with
//! a single placeholder layer so the wgpu bindings remain valid; in that
//! state no block can have layers (validation rejects refs to non-existent
//! ids), so the placeholder is never sampled.

use std::collections::HashMap;

use bevy::asset::{RenderAssetUsages, embedded_asset};
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::pbr::{ExtendedMaterial, MaterialExtension, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, Extent3d, ShaderType, TextureDimension, TextureFormat, TextureViewDescriptor,
    TextureViewDimension,
};
use bevy::render::storage::ShaderStorageBuffer;
use bevy::shader::ShaderRef;
use block_junk_mod_api::textures::{MaskDef, MaskId, MaskSource, RampDef, RampId};

use crate::blocks::{BlockRegistry, BlockSlot, BootstrapError};

/// 16×16 is enough resolution for distinct pixel-art patterns while
/// keeping the per-block memory tiny (~1 KB per layer). The chunk
/// fragment shader samples this with Nearest filtering so we get a
/// crisp pixel-art look at any view distance.
pub const TEX_SIZE: u32 = 16;

/// Per-side resolution for entries in the mask atlas. 64 is a sweet spot:
/// big enough that one repeat across two world cells (`scale = 2.0`)
/// doesn't read as obvious pixels, small enough that 8 layers fit in
/// 32 KB if we ever go that wide.
pub const MASK_SIZE: u32 = 64;

/// Width of a ramp strip. Each ramp is a `RAMP_SIZE × 1` RGBA8 layer.
pub const RAMP_SIZE: u32 = 64;

/// How many layers a single block's stack can carry. Cap is shared with
/// the WGSL `array<Layer, MAX_LAYERS_PER_BLOCK>` declaration in
/// `block_textures.wgsl` — keep them in sync. Mod-side `BlockDef.layers`
/// entries beyond this cap are silently truncated; we log a warning so
/// authors notice rather than discover it visually.
pub const MAX_LAYERS_PER_BLOCK: usize = 4;

/// Embedded shader path. Lives next to this file so the binary is
/// self-contained — same pattern as `preview.wgsl`.
const SHADER_PATH: &str = "embedded://block_junk/block_textures.wgsl";

/// Built-in patterns. Each is a small deterministic function over
/// (x, y, base_color) → RGBA. Adding a new pattern means a new variant
/// here + a `match` arm in `pixel_for`.
#[derive(Clone, Copy, Debug)]
pub enum Pattern {
    Smooth,
    Noise,
    Speckle,
    Planks,
    Leaves,
    Door,
    Checker,
}

impl Pattern {
    fn parse(s: Option<&str>) -> Self {
        match s {
            None | Some("noise") => Pattern::Noise,
            Some("smooth") => Pattern::Smooth,
            Some("speckle") => Pattern::Speckle,
            Some("planks") => Pattern::Planks,
            Some("leaves") => Pattern::Leaves,
            Some("door") => Pattern::Door,
            Some("checker") => Pattern::Checker,
            // Unknown pattern: fall back to noise rather than panic. A
            // mod author's typo shouldn't break boot.
            Some(_) => Pattern::Noise,
        }
    }
}

/// Compact numeric handle for a registered mask, assigned in registration
/// order. Mods never see this — they reference masks by [`MaskId`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaskSlot(pub u16);

/// Compact numeric handle for a registered ramp.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RampSlot(pub u16);

/// Engine-side registry of mod-registered masks. Same shape as
/// [`BlockRegistry`]: slot order = registration order, ids interned to
/// compact slot indices that the shader can sample directly.
#[derive(Resource)]
pub struct MaskRegistry {
    defs_by_slot: Vec<MaskDef>,
    slot_by_id: HashMap<MaskId, MaskSlot>,
}

impl MaskRegistry {
    pub fn build(pending: Vec<MaskDef>) -> Result<Self, BootstrapError> {
        if pending.len() > u16::MAX as usize {
            return Err(BootstrapError::MaskSlotOverflow {
                slots: pending.len(),
            });
        }
        let mut slot_by_id = HashMap::with_capacity(pending.len());
        for (i, def) in pending.iter().enumerate() {
            // Validate source parameters now so a misconfigured mask is
            // a load error, not a confusing "all black" texture later.
            match &def.source {
                MaskSource::Worley { cells } => {
                    if *cells == 0 {
                        return Err(BootstrapError::MaskWorleyCellsInvalid {
                            mask: def.id.clone(),
                            cells: *cells,
                        });
                    }
                }
                // `MaskSource` is `#[non_exhaustive]`: future variants
                // skip per-variant validation here. Boot-time API-version
                // compat check should keep mods + engine in sync on the
                // set of variants in practice.
                _ => {}
            }
            if slot_by_id
                .insert(def.id.clone(), MaskSlot(i as u16))
                .is_some()
            {
                return Err(BootstrapError::DuplicateMaskId(def.id.clone()));
            }
        }
        Ok(Self {
            defs_by_slot: pending,
            slot_by_id,
        })
    }

    pub fn slot_of(&self, id: &MaskId) -> Option<MaskSlot> {
        self.slot_by_id.get(id).copied()
    }

    pub fn slot_count(&self) -> usize {
        self.defs_by_slot.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (MaskSlot, &MaskDef)> + '_ {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, d)| (MaskSlot(i as u16), d))
    }
}

/// Engine-side registry of mod-registered color ramps.
#[derive(Resource)]
pub struct RampRegistry {
    defs_by_slot: Vec<RampDef>,
    slot_by_id: HashMap<RampId, RampSlot>,
}

impl RampRegistry {
    pub fn build(pending: Vec<RampDef>) -> Result<Self, BootstrapError> {
        if pending.len() > u16::MAX as usize {
            return Err(BootstrapError::RampSlotOverflow {
                slots: pending.len(),
            });
        }
        let mut slot_by_id = HashMap::with_capacity(pending.len());
        for (i, def) in pending.iter().enumerate() {
            if def.stops.len() < 2 {
                return Err(BootstrapError::RampTooFewStops {
                    ramp: def.id.clone(),
                    stops: def.stops.len(),
                });
            }
            if slot_by_id
                .insert(def.id.clone(), RampSlot(i as u16))
                .is_some()
            {
                return Err(BootstrapError::DuplicateRampId(def.id.clone()));
            }
        }
        Ok(Self {
            defs_by_slot: pending,
            slot_by_id,
        })
    }

    pub fn slot_of(&self, id: &RampId) -> Option<RampSlot> {
        self.slot_by_id.get(id).copied()
    }

    pub fn slot_count(&self) -> usize {
        self.defs_by_slot.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (RampSlot, &RampDef)> + '_ {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, d)| (RampSlot(i as u16), d))
    }
}

/// One mask+ramp layer composited over the base block color in the
/// chunk fragment shader. Layout matches the WGSL `Layer` struct in
/// `block_textures.wgsl` exactly — encase scalar layout, no padding
/// needed.
#[derive(ShaderType, Clone, Copy, Default, Debug)]
pub struct Layer {
    /// Index into the mask atlas (`mask_atlas[mask_slot]`).
    pub mask_slot: u32,
    /// Index into the ramp atlas. The shader samples this at U = mask
    /// value, so the ramp paints depth/shading within the masked area.
    pub ramp_slot: u32,
    /// World-units per mask repeat. Larger = bigger features. The shader
    /// computes `mask_uv = face_uv / scale`.
    pub scale: f32,
    /// Smoothstep midpoint applied to the raw mask value to derive the
    /// blend coverage.
    pub threshold: f32,
    /// Smoothstep half-width. 0 = hard edge (cartoon); 0.2 = soft.
    pub softness: f32,
}

/// One block's layer stack. Layers composite top-to-bottom: layer
/// `i+1` paints over layer `i`. Slots with `num_layers = 0` skip the
/// loop entirely and look identical to the pre-layer base color.
#[derive(ShaderType, Clone, Copy, Default, Debug)]
pub struct LayerStack {
    pub num_layers: u32,
    pub layers: [Layer; MAX_LAYERS_PER_BLOCK],
}

/// Resource holding every block's pre-baked texture, plus the mask /
/// ramp atlases and the per-slot layer stack buffer used by the chunk
/// material.
#[derive(Resource, Clone)]
pub struct BlockTextures {
    /// Texture-2D-array Image holding one layer per registered slot.
    /// Bound by [`BlockTextureExt`] for the chunk material.
    pub array_handle: Handle<Image>,
    /// Stand-alone 2D handles per slot, indexed by `BlockSlot.0 as usize`.
    /// Used by the hotbar `ImageNode`s.
    pub icons: Vec<Handle<Image>>,
    /// Grayscale mask atlas. R8, one layer per registered mask.
    pub mask_atlas: Handle<Image>,
    /// Color-ramp atlas. RGBA8, one `RAMP_SIZE × 1` layer per ramp.
    pub ramp_atlas: Handle<Image>,
    /// Per-slot layer stacks, indexed by `BlockSlot.0`. Empty default for
    /// slots that don't configure layers.
    pub stacks: Handle<ShaderStorageBuffer>,
}

/// Material extension that adds the texture-2D-array binding on top of
/// the standard PBR material. The chunk fragment shader at
/// `block_textures.wgsl` reads the slot id from the per-vertex colour's
/// alpha, samples this array, composites the per-slot layer stack, and
/// overrides `pbr_input.material.base_color` before running the standard
/// PBR lighting pass — so day/night, ambient, and shadows still apply.
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct BlockTextureExt {
    // Bindings start at 100 to leave 0..99 to the base StandardMaterial,
    // matching the convention in Bevy's extended_material example.
    #[texture(100, dimension = "2d_array")]
    #[sampler(101)]
    pub atlas: Handle<Image>,
    #[texture(102, dimension = "2d_array")]
    #[sampler(103)]
    pub mask_atlas: Handle<Image>,
    #[texture(104, dimension = "2d_array")]
    #[sampler(105)]
    pub ramp_atlas: Handle<Image>,
    #[storage(106, read_only)]
    pub stacks: Handle<ShaderStorageBuffer>,
}

impl MaterialExtension for BlockTextureExt {
    fn fragment_shader() -> ShaderRef {
        SHADER_PATH.into()
    }
    fn deferred_fragment_shader() -> ShaderRef {
        SHADER_PATH.into()
    }
}

/// Type alias for the full chunk material (StandardMaterial + our
/// extension). Spawn / asset-add against this exact path so the type id
/// matches what the renderer expects.
pub type ChunkMaterial = ExtendedMaterial<StandardMaterial, BlockTextureExt>;

pub struct BlockTexturesPlugin;

impl Plugin for BlockTexturesPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "block_textures.wgsl");
        app.add_plugins(MaterialPlugin::<ChunkMaterial>::default());

        // Phase 1: pull from the three registries and generate everything
        // CPU-side. Borrows are confined to this block so the asset
        // registries can be mutably borrowed in Phase 2.
        let mut per_slot: Vec<Image> = Vec::new();
        let mut stacks_data: Vec<LayerStack> = Vec::new();
        let slot_count;
        let mask_image;
        let ramp_image;
        {
            let block_reg = app.world().resource::<BlockRegistry>();
            let mask_reg = app.world().resource::<MaskRegistry>();
            let ramp_reg = app.world().resource::<RampRegistry>();

            slot_count = block_reg.slot_count();
            stacks_data.reserve(slot_count);
            for slot_idx in 0..slot_count {
                let def = block_reg.def(BlockSlot(slot_idx as u16));
                let pattern = Pattern::parse(def.pattern.as_deref());
                per_slot.push(generate_texture(def.color, pattern));

                // Resolve this block's layer ids to slot indices. The
                // BlockRegistry::validate_layers call run earlier
                // guarantees every id resolves, so `expect` here is a
                // contract assertion, not a real fallible path.
                let mut stack = LayerStack::default();
                if def.layers.len() > MAX_LAYERS_PER_BLOCK {
                    warn!(
                        block = %def.id,
                        configured = def.layers.len(),
                        cap = MAX_LAYERS_PER_BLOCK,
                        "block has more layers than the shader cap; extras truncated",
                    );
                }
                let take = def.layers.len().min(MAX_LAYERS_PER_BLOCK);
                stack.num_layers = take as u32;
                for (i, layer_def) in def.layers.iter().take(take).enumerate() {
                    let mask_slot = mask_reg
                        .slot_of(&layer_def.mask)
                        .expect("validated by BlockRegistry::validate_layers");
                    let ramp_slot = ramp_reg
                        .slot_of(&layer_def.ramp)
                        .expect("validated by BlockRegistry::validate_layers");
                    stack.layers[i] = Layer {
                        mask_slot: u32::from(mask_slot.0),
                        ramp_slot: u32::from(ramp_slot.0),
                        scale: layer_def.scale,
                        threshold: layer_def.threshold,
                        softness: layer_def.softness,
                    };
                }
                stacks_data.push(stack);
            }

            mask_image = generate_mask_atlas(mask_reg);
            ramp_image = generate_ramp_atlas(ramp_reg);
        }

        // Build the array Image by concatenating each layer's bytes.
        // RGBA8UnormSrgb pixels = 4 bytes each, so each layer contributes
        // TEX_SIZE * TEX_SIZE * 4 bytes; total = slot_count layers.
        let layer_bytes = (TEX_SIZE * TEX_SIZE * 4) as usize;
        let mut array_data = Vec::with_capacity(layer_bytes * slot_count);
        for layer in &per_slot {
            let data = layer
                .data
                .as_ref()
                .expect("generate_texture always populates data");
            array_data.extend_from_slice(data);
        }

        let mut array_image = Image::new(
            Extent3d {
                width: TEX_SIZE,
                height: TEX_SIZE,
                depth_or_array_layers: slot_count as u32,
            },
            TextureDimension::D2,
            array_data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );
        // Repeat sampler: the chunk fragment shader passes world-space
        // coordinates as UVs (which span 0..N across an N-cell merged
        // greedy quad), so the sampler has to wrap each cell. Nearest
        // filtering for the pixel-art look — no muddy interpolation
        // between texels.
        array_image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::Repeat,
            address_mode_v: ImageAddressMode::Repeat,
            address_mode_w: ImageAddressMode::Repeat,
            mag_filter: ImageFilterMode::Nearest,
            min_filter: ImageFilterMode::Nearest,
            mipmap_filter: ImageFilterMode::Nearest,
            ..ImageSamplerDescriptor::default()
        });
        // texture_2d_array view: without this, Bevy creates a D2 view of
        // a multi-layer texture and the shader binding fails to match.
        array_image.texture_view_descriptor = Some(TextureViewDescriptor {
            dimension: Some(TextureViewDimension::D2Array),
            ..TextureViewDescriptor::default()
        });

        // Phase 2: register Image assets. Borrow Assets<Image> mutably,
        // collect handles, drop the borrow before grabbing the storage-
        // buffer asset registry.
        let world = app.world_mut();
        let (array_handle, mask_atlas_handle, ramp_atlas_handle, icons) = {
            let mut images = world.resource_mut::<Assets<Image>>();
            let array_handle = images.add(array_image);
            let mask_atlas_handle = images.add(mask_image);
            let ramp_atlas_handle = images.add(ramp_image);
            let icons: Vec<Handle<Image>> = per_slot
                .into_iter()
                .map(|mut img| {
                    // Nearest filter on the UI side too so 16×16 art reads
                    // as crisp pixels when scaled up to 32×32 in the hotbar.
                    img.sampler = ImageSampler::nearest();
                    images.add(img)
                })
                .collect();
            (array_handle, mask_atlas_handle, ramp_atlas_handle, icons)
        };
        // Phase 3: encase the layer stacks into a storage buffer asset.
        let stacks_handle = world
            .resource_mut::<Assets<ShaderStorageBuffer>>()
            .add(ShaderStorageBuffer::from(stacks_data));
        world.insert_resource(BlockTextures {
            array_handle,
            icons,
            mask_atlas: mask_atlas_handle,
            ramp_atlas: ramp_atlas_handle,
            stacks: stacks_handle,
        });
    }
}

/// Build one 16×16 RGBA8 image for a block. The pattern function decides
/// each pixel; `color` is the per-block base shade the pattern is
/// derived from.
fn generate_texture(color: [f32; 3], pattern: Pattern) -> Image {
    let bytes_per_pixel = 4;
    let mut data = vec![0u8; (TEX_SIZE * TEX_SIZE) as usize * bytes_per_pixel];
    for y in 0..TEX_SIZE {
        for x in 0..TEX_SIZE {
            let [r, g, b] = pixel_for(x, y, color, pattern);
            let i = ((y * TEX_SIZE + x) as usize) * bytes_per_pixel;
            data[i] = to_u8(r);
            data[i + 1] = to_u8(g);
            data[i + 2] = to_u8(b);
            data[i + 3] = 255;
        }
    }
    Image::new(
        Extent3d {
            width: TEX_SIZE,
            height: TEX_SIZE,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}

fn to_u8(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Cheap deterministic hash → f32 in [0, 1). Pure bit-mixing — adequate
/// for "give me a different value at each pixel" noise; we don't need
/// statistical noise quality here.
fn hash2d(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(73856093) ^ y.wrapping_mul(19349663);
    h ^= h >> 16;
    h = h.wrapping_mul(2654435769);
    h ^= h >> 16;
    (h & 0xFFFFFF) as f32 / 16_777_216.0
}

fn pixel_for(x: u32, y: u32, color: [f32; 3], pattern: Pattern) -> [f32; 3] {
    match pattern {
        Pattern::Smooth => color,
        Pattern::Noise => {
            // ±12 % multiplicative jitter. Subtle enough that the block
            // still reads as "stone" / "dirt" but breaks the flat-colour
            // look.
            let n = hash2d(x, y);
            let k = 0.88 + n * 0.24;
            scale(color, k)
        }
        Pattern::Speckle => {
            // Stone-ish: most pixels close to base, occasional darker or
            // lighter speck (5 % chance each side).
            let n = hash2d(x, y);
            let k = if n < 0.05 {
                0.65
            } else if n < 0.10 {
                1.20
            } else {
                0.90 + (hash2d(x ^ 0x9E37, y ^ 0x79B9)) * 0.20
            };
            scale(color, k)
        }
        Pattern::Planks => {
            // Horizontal planks ~4 px tall with a 1-px darker mortar
            // line at the boundary. Per-plank lightness varies a touch
            // so the planks read as distinct. Occasional dark knot.
            let plank = y / 4;
            let is_mortar = (y % 4) == 0;
            let plank_jitter = (hash2d(0, plank) - 0.5) * 0.15;
            let knot = hash2d(x ^ 0x1234, plank ^ 0x5678) < 0.04;
            let k = if is_mortar {
                0.55
            } else if knot {
                0.50
            } else {
                1.0 + plank_jitter
            };
            scale(color, k)
        }
        Pattern::Leaves => {
            // Dense small-scale variation: each pixel a coin-flip
            // between two shades. Reads as "lots of small leaves."
            let n = hash2d(x, y);
            let k = if n < 0.5 { 0.78 } else { 1.10 };
            scale(color, k)
        }
        Pattern::Door => {
            // Vertical planks (3-px wide) with a horizontal handle band
            // in the upper-middle. The handle itself is a tiny dark dot
            // on the right edge of the middle plank.
            let plank = x / 3;
            let is_seam = (x % 3) == 0 || (x % 3) == 2 && plank == TEX_SIZE / 3 - 1;
            let handle_band = y == 7 || y == 8;
            let handle_dot = handle_band && x == TEX_SIZE - 3;
            let k = if handle_dot {
                0.30
            } else if is_seam {
                0.65
            } else if handle_band {
                0.95
            } else {
                let jitter = (hash2d(0, plank) - 0.5) * 0.10;
                1.0 + jitter
            };
            scale(color, k)
        }
        Pattern::Checker => {
            let k = if ((x / 2) + (y / 2)) % 2 == 0 {
                0.85
            } else {
                1.15
            };
            scale(color, k)
        }
    }
}

fn scale(color: [f32; 3], k: f32) -> [f32; 3] {
    [color[0] * k, color[1] * k, color[2] * k]
}

/// Build the grayscale mask atlas from the [`MaskRegistry`]. One layer
/// per registered mask, R8Unorm, `MASK_SIZE × MASK_SIZE`. When no masks
/// are registered, ship a single all-zero placeholder layer so the
/// wgpu binding remains valid (no block can reference a non-existent
/// mask, so the placeholder is never sampled).
fn generate_mask_atlas(reg: &MaskRegistry) -> Image {
    let count = reg.slot_count();
    let layer_count = count.max(1) as u32;
    let pixels_per_layer = (MASK_SIZE * MASK_SIZE) as usize;
    let mut data = Vec::with_capacity(pixels_per_layer * layer_count as usize);
    if count == 0 {
        data.resize(pixels_per_layer, 0u8);
    } else {
        for (slot, def) in reg.iter() {
            match &def.source {
                MaskSource::Worley { cells } => {
                    for y in 0..MASK_SIZE {
                        for x in 0..MASK_SIZE {
                            let v =
                                worley_tileable(x, y, MASK_SIZE, *cells, u32::from(slot.0));
                            data.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
                        }
                    }
                }
                // Future variants: emit a black layer so the atlas
                // stays valid. `MaskSource` is `#[non_exhaustive]`; in
                // practice the API-version check rejects mismatched
                // mods at load time.
                _ => {
                    warn!(
                        mask = %def.id,
                        "unknown mask source kind; baking as black placeholder",
                    );
                    let pixels = (MASK_SIZE * MASK_SIZE) as usize;
                    data.extend(std::iter::repeat_n(0u8, pixels));
                }
            }
        }
    }
    let mut img = Image::new(
        Extent3d {
            width: MASK_SIZE,
            height: MASK_SIZE,
            depth_or_array_layers: layer_count,
        },
        TextureDimension::D2,
        data,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        address_mode_w: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..ImageSamplerDescriptor::default()
    });
    img.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..TextureViewDescriptor::default()
    });
    img
}

/// Tileable Worley noise: 1.0 near the cell-point, fades to 0.0 as the
/// distance approaches `cell_size`. Cell points are placed by hash so
/// the result is deterministic; neighbour cells (including wraparound)
/// are checked so the tile seams disappear.
fn worley_tileable(x: u32, y: u32, size: u32, cells: u32, seed_layer: u32) -> f32 {
    let cell_size = size as f32 / cells as f32;
    let px = x as f32 + 0.5;
    let py = y as f32 + 0.5;
    let cx = (px / cell_size).floor() as i32;
    let cy = (py / cell_size).floor() as i32;
    let cells_i = cells as i32;
    let seed_x = seed_layer
        .wrapping_mul(73856093)
        .wrapping_add(0x9E3779B9);
    let seed_y = seed_layer
        .wrapping_mul(19349663)
        .wrapping_add(0x517CC1B7);
    let mut min_d2 = f32::INFINITY;
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            let nbr_cx = cx + dx;
            let nbr_cy = cy + dy;
            let wrap_cx = nbr_cx.rem_euclid(cells_i) as u32;
            let wrap_cy = nbr_cy.rem_euclid(cells_i) as u32;
            // Hash the *wrapped* cell coord so cell (-1, k) and cell
            // (cells-1, k) generate the same in-cell point — that's
            // what makes the tile edges line up.
            let h_x = hash2d_seed(wrap_cx, wrap_cy, seed_x);
            let h_y = hash2d_seed(wrap_cx, wrap_cy, seed_y);
            let pt_x = (nbr_cx as f32 + h_x) * cell_size;
            let pt_y = (nbr_cy as f32 + h_y) * cell_size;
            let dxw = pt_x - px;
            let dyw = pt_y - py;
            let d2 = dxw * dxw + dyw * dyw;
            if d2 < min_d2 {
                min_d2 = d2;
            }
        }
    }
    let d = min_d2.sqrt();
    // Bubbles: high near cell points, fade to 0 by ~60 % of cell_size.
    1.0 - (d / (cell_size * 0.6)).clamp(0.0, 1.0)
}

fn hash2d_seed(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = (x.wrapping_add(seed)).wrapping_mul(73856093)
        ^ (y.wrapping_add(seed.wrapping_mul(2654435769))).wrapping_mul(19349663);
    h ^= h >> 16;
    h = h.wrapping_mul(2654435769);
    h ^= h >> 16;
    (h & 0xFFFFFF) as f32 / 16_777_216.0
}

/// Build the color-ramp atlas from the [`RampRegistry`]. Each layer is a
/// `RAMP_SIZE × 1` RGBA8 strip; the shader samples it with U = mask
/// value, V = 0.5. Linear filtering on the sampler smooths between
/// adjacent stops. As with masks, a registry with zero ramps ships a
/// single white placeholder layer so the binding remains valid.
fn generate_ramp_atlas(reg: &RampRegistry) -> Image {
    let count = reg.slot_count();
    let layer_count = count.max(1) as u32;
    let bytes_per_layer = (RAMP_SIZE * 4) as usize;
    let mut data = Vec::with_capacity(bytes_per_layer * layer_count as usize);
    if count == 0 {
        for _ in 0..RAMP_SIZE {
            data.extend_from_slice(&[255, 255, 255, 255]);
        }
    } else {
        for (_slot, def) in reg.iter() {
            for x in 0..RAMP_SIZE {
                let t = x as f32 / (RAMP_SIZE - 1) as f32;
                let [r, g, b] = sample_ramp(&def.stops, t);
                data.push(to_u8(r));
                data.push(to_u8(g));
                data.push(to_u8(b));
                data.push(255);
            }
        }
    }
    let mut img = Image::new(
        Extent3d {
            width: RAMP_SIZE,
            height: 1,
            depth_or_array_layers: layer_count,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        address_mode_w: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..ImageSamplerDescriptor::default()
    });
    img.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..TextureViewDescriptor::default()
    });
    img
}

/// Sample a piecewise-linear ramp at `t ∈ [0, 1]`. Stops are evenly
/// distributed; `RampRegistry::build` enforces `stops.len() >= 2`.
fn sample_ramp(stops: &[[f32; 3]], t: f32) -> [f32; 3] {
    debug_assert!(stops.len() >= 2);
    let n = stops.len();
    let scaled = t.clamp(0.0, 1.0) * (n - 1) as f32;
    let lo = (scaled.floor() as usize).min(n - 2);
    let hi = lo + 1;
    let f = scaled - lo as f32;
    let a = stops[lo];
    let b = stops[hi];
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
}
