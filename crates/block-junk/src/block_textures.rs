//! Engine glue for the procedural texture system.
//!
//! The heavy lifting lives in the `block-junk-textures` crate (op
//! vocabulary, CPU bake, chunk material + WGSL). This module:
//!
//! - loads every mod's `textures.lua` into a [`TextureRegistry`] (both
//!   sides, so validation parity holds; only the client bakes),
//! - bakes the whole set at client boot and uploads the tile array +
//!   storage tables ([`BlockTexturesPlugin`]),
//! - resolves each block's [`BlockTextureRef`] faces to texture indices,
//! - renders hotbar icons by CPU-flattening each block's texture.
//!
//! `textures.lua` is a *pure data* file (`return { ... }`), owned by the
//! texture-studio tool — unlike `data.lua` it carries no engine API, so
//! the studio can rewrite it wholesale.

use std::collections::HashSet;
use std::path::Path;

use bevy::asset::RenderAssetUsages;
use bevy::image::ImageSampler;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::storage::ShaderBuffer;
use block_junk_textures::render::{BlockTexGpu, build_gpu_textures};
use block_junk_textures::{BakedTexture, TexError, TextureSetDoc, bake_set, flatten, lua_io};

pub use block_junk_textures::render::{
    BlockTextureExt, ChunkMaterial, ChunkMaterialPlugin, GhostBlockExt, GhostBlockMaterial,
    GhostParams,
};

use crate::blocks::{BlockRegistry, BlockSlot};

/// Hotbar icon resolution. Icons are a 1-block window into the texture,
/// CPU-flattened with the same composite math the shader runs.
pub const ICON_SIZE: u32 = 32;

/// Parsed `textures.lua` docs from every mod, in deterministic mod order
/// (vanilla first, then by directory name — same rule as script loading).
/// Built on both sides so a broken texture file fails the server too,
/// keeping the "never silently degrade" rule; baking is client-only.
#[derive(Resource)]
pub struct TextureRegistry {
    docs: Vec<TextureSetDoc>,
    ids: HashSet<String>,
}

impl TextureRegistry {
    /// Scan `mods_dir` for `<mod>/textures.lua` files and parse them all.
    /// Missing files are fine (a mod without textures is normal); parse
    /// or validation errors are not.
    pub fn load_from_mods_dir(mods_dir: &Path) -> Result<Self, TexError> {
        let mut dirs: Vec<std::path::PathBuf> = match std::fs::read_dir(mods_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir() && p.join("manifest.toml").exists())
                .collect(),
            Err(_) => Vec::new(),
        };
        // Same deterministic order as ModRegistry::load_dir.
        dirs.sort_by(|a, b| {
            let a_van = a.file_name().and_then(|n| n.to_str()) == Some("vanilla");
            let b_van = b.file_name().and_then(|n| n.to_str()) == Some("vanilla");
            b_van.cmp(&a_van).then_with(|| a.cmp(b))
        });

        let mut docs = Vec::new();
        let mut ids = HashSet::new();
        for dir in dirs {
            let path = dir.join("textures.lua");
            if !path.exists() {
                continue;
            }
            let doc = lua_io::parse_file(&path)?;
            for tex in &doc.textures {
                if !ids.insert(tex.id.clone()) {
                    return Err(TexError::invalid(
                        path.display().to_string(),
                        format!(
                            "texture id \"{}\" already defined by an earlier mod",
                            tex.id
                        ),
                    ));
                }
            }
            docs.push(doc);
        }
        Ok(Self { docs, ids })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn texture_count(&self) -> usize {
        self.ids.len()
    }

    /// CPU-bake every texture, in doc order. Ids are globally unique
    /// (checked at load), so the flat list is unambiguous.
    pub fn bake_all(&self) -> Result<Vec<BakedTexture>, TexError> {
        let mut out = Vec::with_capacity(self.ids.len());
        for doc in &self.docs {
            out.extend(bake_set(doc)?);
        }
        Ok(out)
    }
}

/// Client-side handles for the chunk material + hotbar icons.
#[derive(Resource, Clone)]
pub struct BlockTextures {
    pub tiles: Handle<Image>,
    pub blocks: Handle<ShaderBuffer>,
    pub textures: Handle<ShaderBuffer>,
    pub layers: Handle<ShaderBuffer>,
    /// Per block slot, indexed by `BlockSlot.0 as usize`. Flat-color
    /// swatch for untextured blocks.
    pub icons: Vec<Handle<Image>>,
}

/// Client-only: bake the texture set, upload GPU data, render icons.
/// Reads [`BlockRegistry`] + [`TextureRegistry`] at build time, so the
/// scripting plugin must be added first.
pub struct BlockTexturesPlugin;

impl Plugin for BlockTexturesPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ChunkMaterialPlugin);

        let (gpu, blocks_gpu, icon_images) = {
            let block_reg = app.world().resource::<BlockRegistry>();
            let tex_reg = app.world().resource::<TextureRegistry>();
            let baked = tex_reg
                .bake_all()
                .unwrap_or_else(|e| panic!("texture bake failed: {e}"));
            let gpu = build_gpu_textures(&baked);

            let resolve = |id: Option<&str>| -> u32 {
                id.and_then(|id| gpu.index_of(id))
                    .unwrap_or(block_junk_textures::render::NO_TEXTURE)
            };

            let slot_count = block_reg.slot_count();
            let mut blocks_gpu = Vec::with_capacity(slot_count);
            let mut icon_images = Vec::with_capacity(slot_count);
            for slot_idx in 0..slot_count {
                let def = block_reg.def(BlockSlot(slot_idx as u16));
                let mut entry = BlockTexGpu::untextured(def.color);
                if let Some(tex) = &def.texture {
                    let [top, side, bottom] = tex.faces();
                    entry.top = resolve(top);
                    entry.side = resolve(side);
                    entry.bottom = resolve(bottom);
                }
                blocks_gpu.push(entry);

                // Icon: side texture reads most like the in-world block;
                // fall back to top, then to the flat color.
                let icon_id = def.texture.as_ref().and_then(|t| {
                    let [top, side, _] = t.faces();
                    side.or(top).map(str::to_owned)
                });
                let rgba = match icon_id.and_then(|id| baked.iter().find(|b| b.id == id)) {
                    Some(b) => flatten(b, ICON_SIZE, [0.0, 0.0], 1.0, def.color, false),
                    None => flat_icon(def.color),
                };
                icon_images.push(icon_image(rgba));
            }
            (gpu, blocks_gpu, icon_images)
        };

        info!(
            "baked {} texture(s) / {} layer tile(s), slot size {}px",
            gpu.textures.len(),
            gpu.layers.len(),
            gpu.slot_size,
        );

        let world = app.world_mut();
        let (tiles, icons) = {
            let mut images = world.resource_mut::<Assets<Image>>();
            let tiles = images.add(gpu.tiles);
            let icons = icon_images.into_iter().map(|img| images.add(img)).collect();
            (tiles, icons)
        };
        let (blocks, textures, layers) = {
            let mut buffers = world.resource_mut::<Assets<ShaderBuffer>>();
            // Storage buffers must be non-empty for the bindings to build.
            let mut textures_gpu = gpu.textures;
            if textures_gpu.is_empty() {
                textures_gpu.push(Default::default());
            }
            let mut layers_gpu = gpu.layers;
            if layers_gpu.is_empty() {
                layers_gpu.push(block_junk_textures::render::LayerGpu {
                    array_index: 0,
                    size_px: 1,
                    period: 1.0,
                    blend: 0,
                    opacity: 0.0,
                });
            }
            (
                buffers.add(ShaderBuffer::from(blocks_gpu)),
                buffers.add(ShaderBuffer::from(textures_gpu)),
                buffers.add(ShaderBuffer::from(layers_gpu)),
            )
        };
        world.insert_resource(BlockTextures {
            tiles,
            blocks,
            textures,
            layers,
            icons,
        });
    }
}

fn flat_icon(color: [f32; 3]) -> Vec<u8> {
    let px = [
        (color[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        (color[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        (color[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        255,
    ];
    px.repeat((ICON_SIZE * ICON_SIZE) as usize)
}

fn icon_image(rgba: Vec<u8>) -> Image {
    let mut img = Image::new(
        Extent3d {
            width: ICON_SIZE,
            height: ICON_SIZE,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    // Nearest so the pixel-art look survives UI scaling.
    img.sampler = ImageSampler::nearest();
    img
}
