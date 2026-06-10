//! texture-studio: the procedural-texture editor for block-junk.
//!
//! `cargo run -p texture-studio [path/to/textures.lua]` (defaults to
//! `mods/vanilla/textures.lua`). Loads the doc, shows an editable layer/
//! step view with live previews, a flat tiling preview, and a 3D sample
//! terrain rendered through the *game's* chunk material — what you see
//! here is exactly what the game bakes at boot. Save rewrites the file
//! in canonical form.

mod terrain;
mod ui;

use std::path::PathBuf;

use bevy::camera::Camera3d;
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll};
use bevy::light::DirectionalLight;
use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use bevy::window::{Window, WindowPlugin};
use bevy_egui::{EguiContexts, EguiPlugin};
use block_junk_textures::render::{
    BlockTexGpu, ChunkMaterial, ChunkMaterialPlugin, BlockTextureExt, build_gpu_textures,
};
use block_junk_textures::{BakedTexture, TextureSetDoc, lua_io, validate_doc};

const DEFAULT_DOC: &str = "mods/vanilla/textures.lua";

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DOC));

    let studio = match lua_io::parse_file(&path) {
        Ok(doc) => Studio::new(doc, path),
        Err(e) => {
            // A missing/broken file shouldn't block the tool — start with
            // an empty doc and surface the error in the UI.
            let mut s = Studio::new(TextureSetDoc::default(), path);
            s.error = Some(format!("load failed: {e}"));
            s
        }
    };

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "texture-studio".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        .add_plugins(ChunkMaterialPlugin)
        .insert_resource(studio)
        .insert_resource(Baked::default())
        .insert_resource(SceneBands::default())
        .insert_resource(ui::FlatPreview::default())
        .insert_resource(ClearColor(Color::srgb(0.48, 0.65, 0.85)))
        .add_systems(Startup, setup_scene)
        .add_systems(
            Update,
            (rebake_if_needed, upload_gpu, orbit_camera, keyboard_shortcuts),
        )
        .add_systems(bevy_egui::EguiPrimaryContextPass, ui::studio_ui)
        .run();
}

/// The editable document + selection + undo state. All mutations go
/// through [`Studio::edit`] so undo/dirty/rebake stay consistent.
#[derive(Resource)]
pub struct Studio {
    pub doc: TextureSetDoc,
    pub path: PathBuf,
    pub dirty: bool,
    pub sel_tex: usize,
    pub sel_layer: usize,
    pub sel_step: Option<usize>,
    pub undo: Vec<TextureSetDoc>,
    pub redo: Vec<TextureSetDoc>,
    /// Validation / bake / io error to surface in the UI.
    pub error: Option<String>,
    /// Transient "saved" / "reloaded" message.
    pub status: Option<String>,
    pub needs_rebake: bool,
    /// Bumped on every doc mutation (edits, undo, redo, reload) —
    /// independent of undo coalescing, so UI caches key off this.
    pub doc_version: u64,
    /// (widget key, time) of the last edit — drag coalescing for undo.
    last_edit: Option<(String, f64)>,
}

impl Studio {
    fn new(doc: TextureSetDoc, path: PathBuf) -> Self {
        Self {
            doc,
            path,
            dirty: false,
            sel_tex: 0,
            sel_layer: 0,
            sel_step: None,
            undo: Vec::new(),
            redo: Vec::new(),
            error: None,
            status: None,
            needs_rebake: true,
            doc_version: 0,
            last_edit: None,
        }
    }

    fn push_undo(&mut self) {
        self.undo.push(self.doc.clone());
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn mark_changed(&mut self) {
        self.dirty = true;
        self.needs_rebake = true;
        self.doc_version += 1;
    }

    /// Mutate the doc for a continuous widget (slider/drag). Edits with
    /// the same `key` within 0.75 s coalesce into one undo entry so a
    /// drag doesn't spam history. `now` must be a real timestamp.
    pub fn edit(&mut self, key: &str, now: f64, f: impl FnOnce(&mut TextureSetDoc)) {
        let coalesce = self
            .last_edit
            .as_ref()
            .is_some_and(|(k, t)| k == key && now - t < 0.75);
        if !coalesce {
            self.push_undo();
        }
        self.last_edit = Some((key.to_owned(), now));
        f(&mut self.doc);
        self.mark_changed();
    }

    /// Mutate the doc for a one-shot action (button click, combo pick,
    /// add/remove/reorder). Always its own undo entry.
    pub fn edit_once(&mut self, f: impl FnOnce(&mut TextureSetDoc)) {
        self.push_undo();
        self.last_edit = None;
        f(&mut self.doc);
        self.mark_changed();
    }

    pub fn undo_once(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.doc, prev));
            self.dirty = true;
            self.needs_rebake = true;
            self.doc_version += 1;
            self.last_edit = None;
        }
    }

    pub fn redo_once(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.doc, next));
            self.dirty = true;
            self.needs_rebake = true;
            self.doc_version += 1;
            self.last_edit = None;
        }
    }

    pub fn save(&mut self) {
        if let Err(e) = validate_doc(&self.doc) {
            self.error = Some(format!("not saved: {e}"));
            return;
        }
        let text = lua_io::serialize(&self.doc);
        match std::fs::write(&self.path, text) {
            Ok(()) => {
                self.dirty = false;
                self.status = Some(format!("saved {}", self.path.display()));
            }
            Err(e) => self.error = Some(format!("save failed: {e}")),
        }
    }

    pub fn reload(&mut self) {
        match lua_io::parse_file(&self.path) {
            Ok(doc) => {
                self.doc = doc;
                self.undo.clear();
                self.redo.clear();
                self.dirty = false;
                self.error = None;
                self.needs_rebake = true;
                self.doc_version += 1;
                self.status = Some("reloaded".into());
                self.clamp_selection();
            }
            Err(e) => self.error = Some(format!("reload failed: {e}")),
        }
    }

    pub fn clamp_selection(&mut self) {
        self.sel_tex = self.sel_tex.min(self.doc.textures.len().saturating_sub(1));
        if let Some(tex) = self.doc.textures.get(self.sel_tex) {
            self.sel_layer = self.sel_layer.min(tex.layers.len().saturating_sub(1));
            if let Some(layer) = tex.layers.get(self.sel_layer) {
                if let Some(s) = self.sel_step
                    && s >= layer.steps.len()
                {
                    self.sel_step = None;
                }
            } else {
                self.sel_step = None;
            }
        } else {
            self.sel_step = None;
        }
    }
}

/// Latest successful CPU bake. `generation` bumps on every rebake so the
/// UI preview caches know to refresh.
#[derive(Resource, Default)]
pub struct Baked {
    pub set: Vec<BakedTexture>,
    pub generation: u64,
    pub gpu_dirty: bool,
}

impl Baked {
    pub fn by_id(&self, id: &str) -> Option<&BakedTexture> {
        self.set.iter().find(|b| b.id == id)
    }
}

/// Which texture id each terrain band renders with.
#[derive(Resource)]
pub struct SceneBands {
    pub surface_top: String,
    pub surface_side: String,
    pub soil: String,
    pub rock: String,
    pub accent: String,
    pub dirty: bool,
}

impl Default for SceneBands {
    fn default() -> Self {
        Self {
            surface_top: "vanilla:grass_top".into(),
            surface_side: "vanilla:grass_side".into(),
            soil: "vanilla:dirt".into(),
            rock: "vanilla:stone".into(),
            accent: "vanilla:gravel".into(),
            dirty: false,
        }
    }
}

#[derive(Resource)]
struct SceneMaterial(Handle<ChunkMaterial>);

#[derive(Resource)]
struct Orbit {
    yaw: f32,
    pitch: f32,
    dist: f32,
    target: Vec3,
}

fn setup_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ChunkMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut ambient: ResMut<GlobalAmbientLight>,
) {
    ambient.brightness = 300.0;

    // Placeholder GPU data; the first `upload_gpu` run replaces it.
    let empty = build_gpu_textures(&[]);
    let tiles = images.add(empty.tiles);
    let blocks = buffers.add(ShaderStorageBuffer::from(vec![
        BlockTexGpu::untextured([0.5, 0.5, 0.5]);
        terrain::SLOT_COUNT
    ]));
    let textures = buffers.add(ShaderStorageBuffer::from(vec![
        block_junk_textures::render::TexInfoGpu::default(),
    ]));
    let layers = buffers.add(ShaderStorageBuffer::from(vec![
        block_junk_textures::render::LayerGpu {
            array_index: 0,
            size_px: 1,
            period: 1.0,
            blend: 0,
            opacity: 0.0,
        },
    ]));
    let material = materials.add(ChunkMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.9,
            ..default()
        },
        extension: BlockTextureExt {
            tiles,
            blocks,
            textures,
            layers,
        },
    });
    commands.insert_resource(SceneMaterial(material.clone()));

    commands.spawn((
        Mesh3d(meshes.add(terrain::build_mesh())),
        MeshMaterial3d(material),
        Transform::default(),
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 9_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.9, 0.6, 0.0)),
    ));

    let half = terrain::SIZE as f32 * 0.5;
    commands.insert_resource(Orbit {
        yaw: 0.7,
        pitch: 0.55,
        dist: 55.0,
        target: Vec3::new(half, 7.0, half),
    });
    commands.spawn((Camera3d::default(), Transform::default()));
}

/// Doc → CPU bake whenever an edit happened. Validation gates the bake;
/// on error we keep the last good bake and show the message.
fn rebake_if_needed(mut studio: ResMut<Studio>, mut baked: ResMut<Baked>) {
    if !studio.needs_rebake {
        return;
    }
    studio.needs_rebake = false;
    if let Err(e) = validate_doc(&studio.doc) {
        studio.error = Some(e.to_string());
        return;
    }
    match block_junk_textures::bake_set(&studio.doc) {
        Ok(set) => {
            baked.set = set;
            baked.generation += 1;
            baked.gpu_dirty = true;
            studio.error = None;
        }
        Err(e) => studio.error = Some(e.to_string()),
    }
}

/// CPU bake (+ band mapping) → fresh tile array, storage buffers, and an
/// in-place ChunkMaterial update.
fn upload_gpu(
    mut baked: ResMut<Baked>,
    mut bands: ResMut<SceneBands>,
    scene_mat: Res<SceneMaterial>,
    mut materials: ResMut<Assets<ChunkMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
) {
    if !baked.gpu_dirty && !bands.dirty {
        return;
    }
    baked.gpu_dirty = false;
    bands.dirty = false;

    let gpu = build_gpu_textures(&baked.set);
    let resolve = |id: &str| gpu.index_of(id).unwrap_or(block_junk_textures::render::NO_TEXTURE);

    let mut blocks = vec![BlockTexGpu::untextured([0.55, 0.55, 0.55]); terrain::SLOT_COUNT];
    blocks[terrain::SLOT_SURFACE as usize] = BlockTexGpu {
        top: resolve(&bands.surface_top),
        side: resolve(&bands.surface_side),
        bottom: resolve(&bands.soil),
        _pad: 0,
        fallback: Vec4::new(0.36, 0.62, 0.30, 1.0),
    };
    blocks[terrain::SLOT_SOIL as usize] = BlockTexGpu {
        top: resolve(&bands.soil),
        side: resolve(&bands.soil),
        bottom: resolve(&bands.soil),
        _pad: 0,
        fallback: Vec4::new(0.45, 0.32, 0.20, 1.0),
    };
    blocks[terrain::SLOT_ROCK as usize] = BlockTexGpu {
        top: resolve(&bands.rock),
        side: resolve(&bands.rock),
        bottom: resolve(&bands.rock),
        _pad: 0,
        fallback: Vec4::new(0.55, 0.55, 0.58, 1.0),
    };
    blocks[terrain::SLOT_ACCENT as usize] = BlockTexGpu {
        top: resolve(&bands.accent),
        side: resolve(&bands.accent),
        bottom: resolve(&bands.accent),
        _pad: 0,
        fallback: Vec4::new(0.48, 0.46, 0.45, 1.0),
    };

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

    let Some(mat) = materials.get_mut(&scene_mat.0) else {
        return;
    };
    mat.extension.tiles = images.add(gpu.tiles);
    mat.extension.blocks = buffers.add(ShaderStorageBuffer::from(blocks));
    mat.extension.textures = buffers.add(ShaderStorageBuffer::from(textures_gpu));
    mat.extension.layers = buffers.add(ShaderStorageBuffer::from(layers_gpu));
}

/// Right-drag orbits, scroll zooms — only when egui isn't using the
/// pointer.
fn orbit_camera(
    mut orbit: ResMut<Orbit>,
    mut cam: Query<&mut Transform, With<Camera3d>>,
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    mut contexts: EguiContexts,
) {
    let egui_wants_pointer = contexts
        .ctx_mut()
        .map(|ctx| ctx.wants_pointer_input())
        .unwrap_or(false);

    if !egui_wants_pointer {
        if buttons.pressed(MouseButton::Right) || buttons.pressed(MouseButton::Middle) {
            orbit.yaw -= motion.delta.x * 0.008;
            orbit.pitch = (orbit.pitch + motion.delta.y * 0.008).clamp(-0.1, 1.5);
        }
        if scroll.delta.y.abs() > 0.0 {
            orbit.dist = (orbit.dist * (1.0 - scroll.delta.y * 0.08)).clamp(8.0, 160.0);
        }
    }

    let Ok(mut t) = cam.single_mut() else { return };
    let rot = Quat::from_euler(EulerRot::YXZ, orbit.yaw, -orbit.pitch, 0.0);
    let pos = orbit.target + rot * Vec3::new(0.0, 0.0, orbit.dist);
    *t = Transform::from_translation(pos).looking_at(orbit.target, Vec3::Y);
}

fn keyboard_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    mut studio: ResMut<Studio>,
    mut contexts: EguiContexts,
) {
    let egui_wants_kb = contexts
        .ctx_mut()
        .map(|ctx| ctx.wants_keyboard_input())
        .unwrap_or(false);
    if egui_wants_kb {
        return;
    }
    let cmd = keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);
    if !cmd {
        return;
    }
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if keys.just_pressed(KeyCode::KeyS) {
        studio.save();
    } else if keys.just_pressed(KeyCode::KeyZ) {
        if shift {
            studio.redo_once();
        } else {
            studio.undo_once();
        }
    }
}
