//! The studio's egui interface. Lives in `EguiPrimaryContextPass` (egui
//! input collection only happens there — buttons silently dead anywhere
//! else).
//!
//! Layout: top toolbar / left texture list / right inspector (layers,
//! step strip with live thumbnails, OpSpec-driven param editor) / bottom
//! flat tiling preview / floating "scene bands" window for the 3D
//! diorama. The central region is the bevy 3D viewport.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use block_junk_textures::doc::{
    BlendMode, FinishDef, LayerDef, MAX_LAYERS_PER_TEXTURE, MAX_PERIOD, ParamValue, RampStop, Step,
    TextureDef,
};
use block_junk_textures::eval::{ColorBuf, GreyBuf, StepTrace, eval_layer_traced};
use block_junk_textures::ops::{OPS, OpCategory, ParamType, find_op};
use block_junk_textures::{bake_texture, flatten};

use crate::{Baked, SceneBands, Studio};

const THUMB_PX: usize = 56;
const FLAT_PX: usize = 320;

/// Flat-preview controls (span in blocks, pan origin, finish toggle).
#[derive(Resource)]
pub struct FlatPreview {
    pub span: f32,
    pub origin: Vec2,
    pub with_finish: bool,
}

impl Default for FlatPreview {
    fn default() -> Self {
        Self {
            span: 6.0,
            origin: Vec2::ZERO,
            with_finish: true,
        }
    }
}

/// egui-texture cache for thumbnails + flat preview. Rebuilt when the
/// bake generation or the selection changes.
#[derive(Default)]
pub struct PreviewCache {
    thumbs_key: Option<(u64, usize, usize)>,
    thumbs: Vec<egui::TextureHandle>,
    flat_key: Option<u64>,
    flat: Option<egui::TextureHandle>,
    rename_draft: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn studio_ui(
    mut contexts: EguiContexts,
    mut studio: ResMut<Studio>,
    baked: Res<Baked>,
    mut bands: ResMut<SceneBands>,
    mut flat: ResMut<FlatPreview>,
    mut cache: Local<PreviewCache>,
    mut fonts_installed: Local<bool>,
    time: Res<Time>,
) {
    let Ok(ctx) = contexts.ctx_mut() else { return };
    let ctx = ctx.clone();
    if !*fonts_installed {
        // DejaVu fallback so the arrow/triangle/die-face button icons
        // aren't tofu. Once — set_fonts rebuilds the glyph atlas.
        block_junk_textures::egui_fonts::install(&ctx);
        *fonts_installed = true;
    }
    let now = time.elapsed_secs_f64();
    studio.clamp_selection();

    top_bar(&ctx, &mut studio);
    texture_list(&ctx, &mut studio, &mut cache, now);
    inspector(&ctx, &mut studio, &mut cache, now);
    flat_preview(&ctx, &mut studio, &baked, &mut flat, &mut cache);
    bands_window(&ctx, &studio, &mut bands);
}

fn top_bar(ctx: &egui::Context, studio: &mut Studio) {
    egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{}{}",
                    studio.path.display(),
                    if studio.dirty { " *" } else { "" }
                ))
                .monospace(),
            );
            ui.separator();
            if ui.button("Save (⌘S)").clicked() {
                studio.save();
            }
            if ui.button("Reload").clicked() {
                studio.reload();
            }
            ui.separator();
            if ui
                .add_enabled(!studio.undo.is_empty(), egui::Button::new("Undo"))
                .clicked()
            {
                studio.undo_once();
            }
            if ui
                .add_enabled(!studio.redo.is_empty(), egui::Button::new("Redo"))
                .clicked()
            {
                studio.redo_once();
            }
            ui.separator();
            ui.label("orbit: right-drag · zoom: scroll");
            if let Some(status) = &studio.status {
                ui.separator();
                ui.colored_label(egui::Color32::LIGHT_GREEN, status);
            }
        });
        if let Some(err) = &studio.error {
            ui.colored_label(egui::Color32::from_rgb(255, 120, 110), err);
        }
    });
}

fn texture_list(ctx: &egui::Context, studio: &mut Studio, cache: &mut PreviewCache, now: f64) {
    egui::SidePanel::left("textures")
        .default_width(210.0)
        .show(ctx, |ui| {
            ui.heading("Textures");
            ui.horizontal(|ui| {
                ui.label("pixels/block");
                let mut ppb = studio.doc.pixels_per_block;
                if ui
                    .add(egui::DragValue::new(&mut ppb).range(4..=64))
                    .changed()
                {
                    studio.edit("ppb", now, |doc| doc.pixels_per_block = ppb);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                let ids: Vec<String> = studio.doc.textures.iter().map(|t| t.id.clone()).collect();
                for (i, id) in ids.iter().enumerate() {
                    if ui.selectable_label(studio.sel_tex == i, id).clicked() {
                        studio.sel_tex = i;
                        studio.sel_layer = 0;
                        studio.sel_step = None;
                        cache.rename_draft = None;
                    }
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("+ new").clicked() {
                    let id = unique_id(&studio.doc.textures, "vanilla:new_texture");
                    studio.edit_once(|doc| {
                        doc.textures.push(starter_texture(&id));
                    });
                    studio.sel_tex = studio.doc.textures.len() - 1;
                    studio.sel_layer = 0;
                    studio.sel_step = None;
                    cache.rename_draft = None;
                }
                let has_sel = !studio.doc.textures.is_empty();
                if ui
                    .add_enabled(has_sel, egui::Button::new("duplicate"))
                    .clicked()
                {
                    let sel = studio.sel_tex;
                    let mut copy = studio.doc.textures[sel].clone();
                    copy.id = unique_id(&studio.doc.textures, &copy.id);
                    studio.edit_once(|doc| doc.textures.insert(sel + 1, copy));
                    studio.sel_tex = sel + 1;
                    cache.rename_draft = None;
                }
                if ui
                    .add_enabled(has_sel, egui::Button::new("delete"))
                    .clicked()
                {
                    let sel = studio.sel_tex;
                    studio.edit_once(|doc| {
                        doc.textures.remove(sel);
                    });
                    studio.clamp_selection();
                    cache.rename_draft = None;
                }
            });
        });
}

fn inspector(ctx: &egui::Context, studio: &mut Studio, cache: &mut PreviewCache, now: f64) {
    egui::SidePanel::right("inspector")
        .default_width(360.0)
        .show(ctx, |ui| {
            if studio.doc.textures.is_empty() {
                ui.label("No textures — add one on the left.");
                return;
            }
            let t = studio.sel_tex;
            egui::ScrollArea::vertical().show(ui, |ui| {
                // ---- id rename (applied on focus loss / enter) --------
                ui.horizontal(|ui| {
                    ui.label("id");
                    let draft = cache
                        .rename_draft
                        .get_or_insert_with(|| studio.doc.textures[t].id.clone());
                    let resp = ui.text_edit_singleline(draft);
                    if resp.lost_focus() {
                        let new_id = draft.clone();
                        if new_id != studio.doc.textures[t].id {
                            studio.edit_once(|doc| {
                                doc.textures[t].id = new_id;
                            });
                        }
                        cache.rename_draft = None;
                    }
                });

                // ---- finish ------------------------------------------
                ui.separator();
                let finish = studio.doc.textures[t].finish;
                let mut on = finish.is_some();
                if ui
                    .checkbox(&mut on, "finish (world-space brightness jitter)")
                    .changed()
                {
                    studio.edit_once(|doc| {
                        doc.textures[t].finish = on.then(FinishDef::default);
                    });
                }
                if let Some(f) = finish {
                    let mut scale = f.scale;
                    let mut brightness = f.brightness;
                    let mut seed = f.seed;
                    ui.horizontal(|ui| {
                        ui.label("scale");
                        let c1 = ui.add(egui::Slider::new(&mut scale, 2.0..=32.0)).changed();
                        ui.label("±");
                        let c2 = ui
                            .add(egui::Slider::new(&mut brightness, 0.0..=0.2))
                            .changed();
                        let c3 = ui.add(egui::DragValue::new(&mut seed)).changed();
                        if c1 || c2 || c3 {
                            studio.edit("finish", now, |doc| {
                                doc.textures[t].finish = Some(FinishDef {
                                    scale,
                                    brightness,
                                    seed,
                                });
                            });
                        }
                    });
                }

                // ---- layers ------------------------------------------
                ui.separator();
                ui.horizontal(|ui| {
                    ui.heading("Layers");
                    let can_add = studio.doc.textures[t].layers.len() < MAX_LAYERS_PER_TEXTURE;
                    if ui
                        .add_enabled(can_add, egui::Button::new("+ layer"))
                        .clicked()
                    {
                        studio.edit_once(|doc| {
                            doc.textures[t].layers.push(starter_layer());
                        });
                        studio.sel_layer = studio.doc.textures[t].layers.len() - 1;
                        studio.sel_step = None;
                    }
                });
                let layer_count = studio.doc.textures[t].layers.len();
                for l in 0..layer_count {
                    let layer = &studio.doc.textures[t].layers[l];
                    let (period, blend, opacity) = (layer.period, layer.blend, layer.opacity);
                    let selected = studio.sel_layer == l;
                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(
                                selected,
                                format!("layer {} · period {}", l + 1, period),
                            )
                            .clicked()
                        {
                            studio.sel_layer = l;
                            studio.sel_step = None;
                        }
                        if ui.small_button("↑").clicked() && l > 0 {
                            studio.edit_once(|doc| {
                                doc.textures[t].layers.swap(l, l - 1);
                            });
                            studio.sel_layer = l - 1;
                        }
                        if ui.small_button("↓").clicked() && l + 1 < layer_count {
                            studio.edit_once(|doc| {
                                doc.textures[t].layers.swap(l, l + 1);
                            });
                            studio.sel_layer = l + 1;
                        }
                        if ui.small_button("✕").clicked() && layer_count > 1 {
                            studio.edit_once(|doc| {
                                doc.textures[t].layers.remove(l);
                            });
                            studio.clamp_selection();
                        }
                    });
                    if selected {
                        ui.indent("layer-fields", |ui| {
                            let mut p = period;
                            let mut o = opacity;
                            ui.horizontal(|ui| {
                                ui.label("period");
                                if ui.add(egui::Slider::new(&mut p, 1..=MAX_PERIOD)).changed() {
                                    studio.edit("layer-period", now, |doc| {
                                        doc.textures[t].layers[l].period = p;
                                    });
                                }
                                ui.label("opacity");
                                if ui.add(egui::Slider::new(&mut o, 0.0..=1.0)).changed() {
                                    studio.edit("layer-opacity", now, |doc| {
                                        doc.textures[t].layers[l].opacity = o;
                                    });
                                }
                            });
                            blend_combo(ui, &format!("layer-blend-{l}"), blend, |mode| {
                                studio.edit_once(|doc| {
                                    doc.textures[t].layers[l].blend = mode;
                                });
                            });
                        });
                    }
                }

                // ---- steps -------------------------------------------
                ui.separator();
                let l = studio.sel_layer.min(layer_count.saturating_sub(1));
                ui.horizontal(|ui| {
                    ui.heading(format!("Steps (layer {})", l + 1));
                    ui.menu_button("+ step", |ui| {
                        for (label, cat) in [
                            ("generators", OpCategory::GreyGen),
                            ("filters", OpCategory::GreyFilter),
                            ("paint", OpCategory::Paint),
                        ] {
                            ui.label(egui::RichText::new(label).small().weak());
                            for op in OPS.iter().filter(|o| o.category == cat) {
                                if ui.button(op.name).on_hover_text(op.doc).clicked() {
                                    let step = starter_step(op.name);
                                    studio.edit_once(|doc| {
                                        doc.textures[t].layers[l].steps.push(step);
                                    });
                                    studio.sel_step =
                                        Some(studio.doc.textures[t].layers[l].steps.len() - 1);
                                    ui.close();
                                }
                            }
                            ui.separator();
                        }
                    });
                });

                step_strip(ui, ctx, studio, cache, t, l);

                // ---- selected step params ----------------------------
                if let Some(s) = studio.sel_step {
                    if s < studio.doc.textures[t].layers[l].steps.len() {
                        ui.separator();
                        step_params(ui, studio, t, l, s, now);
                    }
                }
            });
        });
}

/// Horizontal strip of per-step thumbnails with select / reorder / delete.
fn step_strip(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    studio: &mut Studio,
    cache: &mut PreviewCache,
    t: usize,
    l: usize,
) {
    // Refresh the thumbnail cache when the doc or the selection moved.
    // doc_version bumps on EVERY mutation (including undo-coalesced ones
    // and undo/redo themselves), so reorders invalidate reliably.
    let key = (studio.doc_version, t, l);
    if cache.thumbs_key != Some(key) {
        cache.thumbs_key = Some(key);
        cache.thumbs.clear();
        let layer = &studio.doc.textures[t].layers[l];
        let size = (layer.period * studio.doc.pixels_per_block).max(8);
        if let Ok((_, traces)) = eval_layer_traced(layer, size, "preview") {
            for (i, trace) in traces.iter().enumerate() {
                let img = match trace {
                    StepTrace::Grey(g) => grey_image(g, THUMB_PX),
                    StepTrace::Canvas(c) => canvas_image(c, THUMB_PX),
                };
                cache.thumbs.push(ctx.load_texture(
                    format!("thumb-{i}"),
                    img,
                    egui::TextureOptions::NEAREST,
                ));
            }
        }
    }

    let steps: Vec<(String, Option<String>)> = studio.doc.textures[t].layers[l]
        .steps
        .iter()
        .map(|s| (s.op.clone(), s.out.clone()))
        .collect();
    egui::ScrollArea::horizontal()
        .id_salt("step-strip")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let count = steps.len();
                for (s, (op, out)) in steps.iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.set_width(THUMB_PX as f32 + 8.0);
                        let selected = studio.sel_step == Some(s);
                        let label = match out {
                            Some(o) => format!("{op}→{o}"),
                            None => op.clone(),
                        };
                        if let Some(tex) = cache.thumbs.get(s) {
                            let resp = ui.add(
                                egui::Button::image(egui::load::SizedTexture::new(
                                    tex.id(),
                                    [THUMB_PX as f32, THUMB_PX as f32],
                                ))
                                .selected(selected),
                            );
                            if resp.clicked() {
                                studio.sel_step = Some(s);
                            }
                        } else if ui.selectable_label(selected, "·").clicked() {
                            studio.sel_step = Some(s);
                        }
                        ui.label(egui::RichText::new(label).small());
                        ui.horizontal(|ui| {
                            if ui.small_button("◀").clicked() && s > 0 {
                                studio.edit_once(|doc| {
                                    doc.textures[t].layers[l].steps.swap(s, s - 1);
                                });
                                studio.sel_step = Some(s - 1);
                            }
                            if ui.small_button("▶").clicked() && s + 1 < count {
                                studio.edit_once(|doc| {
                                    doc.textures[t].layers[l].steps.swap(s, s + 1);
                                });
                                studio.sel_step = Some(s + 1);
                            }
                            if ui.small_button("✕").clicked() {
                                studio.edit_once(|doc| {
                                    doc.textures[t].layers[l].steps.remove(s);
                                });
                                studio.clamp_selection();
                            }
                        });
                    });
                }
            });
        });
}

/// OpSpec-driven parameter editor for one step.
fn step_params(ui: &mut egui::Ui, studio: &mut Studio, t: usize, l: usize, s: usize, now: f64) {
    let step = studio.doc.textures[t].layers[l].steps[s].clone();
    let Some(spec) = find_op(&step.op) else {
        ui.colored_label(egui::Color32::RED, format!("unknown op {}", step.op));
        return;
    };
    ui.heading(format!("{} — {}", spec.name, category_name(spec.category)));
    ui.label(egui::RichText::new(spec.doc).small().weak());

    // Named output (grey ops only).
    if spec.category != OpCategory::Paint {
        let mut out = step.out.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("out");
            if ui.text_edit_singleline(&mut out).changed() {
                let v = (!out.is_empty()).then_some(out.clone());
                studio.edit(&format!("out-{t}-{l}-{s}"), now, |doc| {
                    doc.textures[t].layers[l].steps[s].out = v;
                });
            }
            if !spec.outputs.is_empty() {
                ui.label(
                    egui::RichText::new(format!("channels: {}", spec.outputs.join(", ")))
                        .small()
                        .weak(),
                );
            }
        });
    }

    let refs = available_refs(&studio.doc.textures[t].layers[l].steps[..s]);

    for ps in spec.params {
        let key = format!("p-{t}-{l}-{s}-{}", ps.name);
        ui.horizontal(|ui| {
            ui.label(ps.name);
            match ps.ty {
                ParamType::F32 { min, max, default } => {
                    let mut v = match step.params.get(ps.name) {
                        Some(ParamValue::F32(v)) => *v,
                        _ => default,
                    };
                    if ui.add(egui::Slider::new(&mut v, min..=max)).changed() {
                        set_param(
                            studio,
                            &key,
                            Some(now),
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::F32(v),
                        );
                    }
                }
                ParamType::U32 { min, max, default } => {
                    let mut v = match step.params.get(ps.name) {
                        Some(ParamValue::U32(v)) => *v,
                        _ => default,
                    };
                    if ui.add(egui::Slider::new(&mut v, min..=max)).changed() {
                        set_param(
                            studio,
                            &key,
                            Some(now),
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::U32(v),
                        );
                    }
                }
                ParamType::Seed => {
                    let mut v = match step.params.get(ps.name) {
                        Some(ParamValue::U32(v)) => *v,
                        _ => 0,
                    };
                    if ui.add(egui::DragValue::new(&mut v)).changed() {
                        set_param(
                            studio,
                            &key,
                            Some(now),
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::U32(v),
                        );
                    }
                    // ⚄ (U+2684) not 🎲: DejaVu covers BMP die faces but
                    // not emoji-plane glyphs.
                    if ui.button("⚄").clicked() {
                        let r = (now * 1.0e6) as u64 as u32 ^ 0x9E37_79B9;
                        set_param(
                            studio,
                            &key,
                            None,
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::U32(r % 10_000),
                        );
                    }
                }
                ParamType::Enum { options, default } => {
                    let current = match step.params.get(ps.name) {
                        Some(ParamValue::Str(v)) => v.clone(),
                        _ => default.to_owned(),
                    };
                    egui::ComboBox::from_id_salt(&key)
                        .selected_text(&current)
                        .show_ui(ui, |ui| {
                            for opt in options {
                                if ui.selectable_label(current == *opt, *opt).clicked() {
                                    set_param(
                                        studio,
                                        &key,
                                        None,
                                        t,
                                        l,
                                        s,
                                        ps.name,
                                        ParamValue::Str((*opt).to_owned()),
                                    );
                                }
                            }
                        });
                }
                ParamType::Ref { required } => {
                    let current = match step.params.get(ps.name) {
                        Some(ParamValue::Str(v)) => Some(v.clone()),
                        _ => None,
                    };
                    let implicit = if matches!(ps.name, "input" | "a") {
                        "(previous)"
                    } else {
                        "(none)"
                    };
                    let text = current.clone().unwrap_or_else(|| implicit.to_owned());
                    egui::ComboBox::from_id_salt(&key)
                        .selected_text(text)
                        .show_ui(ui, |ui| {
                            if !required
                                && ui.selectable_label(current.is_none(), implicit).clicked()
                            {
                                remove_param(studio, t, l, s, ps.name);
                            }
                            for r in &refs {
                                if ui
                                    .selectable_label(current.as_deref() == Some(r), r)
                                    .clicked()
                                {
                                    set_param(
                                        studio,
                                        &key,
                                        None,
                                        t,
                                        l,
                                        s,
                                        ps.name,
                                        ParamValue::Str(r.clone()),
                                    );
                                }
                            }
                            if refs.is_empty() {
                                ui.label("(name an earlier step with `out` first)");
                            }
                        });
                }
                ParamType::Color { default } => {
                    let mut v = match step.params.get(ps.name) {
                        Some(ParamValue::Color(c)) => *c,
                        _ => default,
                    };
                    if ui.color_edit_button_rgba_unmultiplied(&mut v).changed() {
                        set_param(
                            studio,
                            &key,
                            Some(now),
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::Color(v),
                        );
                    }
                }
                ParamType::Vec2 { default } => {
                    let mut v = match step.params.get(ps.name) {
                        Some(ParamValue::Vec2(o)) => *o,
                        _ => default,
                    };
                    let c1 = ui
                        .add(
                            egui::DragValue::new(&mut v[0])
                                .speed(0.01)
                                .range(-1.0..=1.0),
                        )
                        .changed();
                    let c2 = ui
                        .add(
                            egui::DragValue::new(&mut v[1])
                                .speed(0.01)
                                .range(-1.0..=1.0),
                        )
                        .changed();
                    if c1 || c2 {
                        set_param(
                            studio,
                            &key,
                            Some(now),
                            t,
                            l,
                            s,
                            ps.name,
                            ParamValue::Vec2(v),
                        );
                    }
                }
                ParamType::Stops => {
                    stops_editor(ui, studio, &key, now, t, l, s, ps.name, &step);
                }
            }
        });
        if !ps.doc.is_empty() {
            ui.label(egui::RichText::new(ps.doc).small().weak());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stops_editor(
    ui: &mut egui::Ui,
    studio: &mut Studio,
    key: &str,
    now: f64,
    t: usize,
    l: usize,
    s: usize,
    name: &str,
    step: &Step,
) {
    let mut stops = match step.params.get(name) {
        Some(ParamValue::Stops(v)) => v.clone(),
        _ => vec![
            RampStop {
                pos: 0.0,
                color: [0.0; 3],
            },
            RampStop {
                pos: 1.0,
                color: [1.0; 3],
            },
        ],
    };
    ui.vertical(|ui| {
        let mut changed = false;
        let mut remove: Option<usize> = None;
        for (i, stop) in stops.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut stop.pos)
                            .speed(0.01)
                            .range(0.0..=1.0),
                    )
                    .changed();
                changed |= ui.color_edit_button_rgb(&mut stop.color).changed();
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove
            && stops.len() > 1
        {
            stops.remove(i);
            changed = true;
        }
        if ui.small_button("+ stop").clicked() {
            let last = *stops.last().unwrap();
            stops.push(RampStop {
                pos: (last.pos + 0.25).min(1.0),
                color: last.color,
            });
            changed = true;
        }
        if changed {
            stops.sort_by(|a, b| a.pos.total_cmp(&b.pos));
            set_param(
                studio,
                key,
                Some(now),
                t,
                l,
                s,
                name,
                ParamValue::Stops(stops),
            );
        }
    });
}

fn flat_preview(
    ctx: &egui::Context,
    studio: &mut Studio,
    baked: &Baked,
    flat: &mut FlatPreview,
    cache: &mut PreviewCache,
) {
    egui::TopBottomPanel::bottom("flat")
        .default_height(FLAT_PX as f32 + 56.0)
        .resizable(true)
        .show(ctx, |ui| {
            let mut export = false;
            ui.horizontal(|ui| {
                ui.heading("Tiling preview");
                ui.label("span");
                ui.add(egui::Slider::new(&mut flat.span, 1.0..=21.0).suffix(" blocks"));
                ui.checkbox(&mut flat.with_finish, "finish");
                export = ui
                    .button("Export PNG")
                    .on_hover_text("Render this view at 1024² to texture-exports/")
                    .clicked();
                ui.label(egui::RichText::new("drag to pan").small().weak());
            });

            let Some(tex) = studio.doc.textures.get(studio.sel_tex) else {
                return;
            };
            // The bake set lags the doc while invalid; preview last good.
            let baked_tex = baked
                .by_id(&tex.id)
                .cloned()
                .or_else(|| bake_texture(tex, studio.doc.pixels_per_block).ok());
            let Some(baked_tex) = baked_tex else { return };

            if export {
                match export_png(&baked_tex, flat) {
                    Ok(path) => studio.status = Some(format!("exported {path}")),
                    Err(e) => studio.error = Some(format!("export failed: {e}")),
                }
            }

            let hash = preview_hash(baked.generation, studio.sel_tex, flat);
            if cache.flat_key != Some(hash) {
                cache.flat_key = Some(hash);
                let rgba = flatten(
                    &baked_tex,
                    FLAT_PX as u32,
                    [flat.origin.x, flat.origin.y],
                    flat.span,
                    [0.2, 0.2, 0.22],
                    flat.with_finish,
                );
                let img = egui::ColorImage::from_rgba_unmultiplied([FLAT_PX, FLAT_PX], &rgba);
                cache.flat = Some(ctx.load_texture("flat", img, egui::TextureOptions::NEAREST));
            }
            if let Some(tex_handle) = &cache.flat {
                let size = ui.available_height().min(FLAT_PX as f32).max(64.0);
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::drag());
                egui::Image::new(egui::load::SizedTexture::new(tex_handle.id(), rect.size()))
                    .paint_at(ui, rect);
                if resp.dragged() {
                    let d = resp.drag_delta();
                    flat.origin.x -= d.x / rect.width() * flat.span;
                    flat.origin.y -= d.y / rect.height() * flat.span;
                    cache.flat_key = None;
                }
            }
        });
}

fn bands_window(ctx: &egui::Context, studio: &Studio, bands: &mut SceneBands) {
    egui::Window::new("Scene bands")
        // default_pos (not anchor — anchored windows can't be dragged)
        // over the 3D viewport, clear of the side panels.
        .default_pos([230.0, 48.0])
        .default_open(false)
        .show(ctx, |ui| {
            let ids: Vec<String> = studio.doc.textures.iter().map(|t| t.id.clone()).collect();
            for (label, slot) in [
                ("surface top", &mut bands.surface_top),
                ("surface side", &mut bands.surface_side),
                ("soil", &mut bands.soil),
                ("rock", &mut bands.rock),
                ("accent", &mut bands.accent),
            ] {
                ui.horizontal(|ui| {
                    ui.label(label);
                    egui::ComboBox::from_id_salt(label)
                        .selected_text(slot.as_str())
                        .show_ui(ui, |ui| {
                            for id in &ids {
                                if ui.selectable_label(slot == id, id).clicked() {
                                    *slot = id.clone();
                                    bands.dirty = true;
                                }
                            }
                        });
                });
            }
        });
}

/// Render the current preview view (span/origin/finish) at 1024² and
/// save it under `texture-exports/<id>.png` in the working directory.
fn export_png(
    baked: &block_junk_textures::BakedTexture,
    flat: &FlatPreview,
) -> Result<String, String> {
    const PX: u32 = 1024;
    let rgba = flatten(
        baked,
        PX,
        [flat.origin.x, flat.origin.y],
        flat.span,
        [0.2, 0.2, 0.22],
        flat.with_finish,
    );
    let dir = std::path::Path::new("texture-exports");
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.png", baked.id.replace(':', "_")));
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), PX, PX);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

// ------------------------------------------------------------ helpers

/// `now: Some(t)` = continuous widget (coalescing undo); `None` = one-
/// shot pick (its own undo entry).
fn set_param(
    studio: &mut Studio,
    key: &str,
    now: Option<f64>,
    t: usize,
    l: usize,
    s: usize,
    name: &str,
    value: ParamValue,
) {
    let name = name.to_owned();
    let apply = move |doc: &mut block_junk_textures::TextureSetDoc| {
        doc.textures[t].layers[l].steps[s]
            .params
            .insert(name, value);
    };
    match now {
        Some(now) => studio.edit(key, now, apply),
        None => studio.edit_once(apply),
    }
}

fn remove_param(studio: &mut Studio, t: usize, l: usize, s: usize, name: &str) {
    let name = name.to_owned();
    studio.edit_once(|doc| {
        doc.textures[t].layers[l].steps[s].params.remove(&name);
    });
}

fn blend_combo(
    ui: &mut egui::Ui,
    id: &str,
    current: BlendMode,
    mut on_change: impl FnMut(BlendMode),
) {
    ui.horizontal(|ui| {
        ui.label("blend");
        egui::ComboBox::from_id_salt(id)
            .selected_text(current.name())
            .show_ui(ui, |ui| {
                for mode in BlendMode::ALL {
                    if ui.selectable_label(current == mode, mode.name()).clicked() {
                        on_change(mode);
                    }
                }
            });
    });
}

/// Refs visible to a step: every earlier named output, plus its channels.
fn available_refs(earlier: &[Step]) -> Vec<String> {
    let mut out = Vec::new();
    for step in earlier {
        let (Some(name), Some(spec)) = (&step.out, find_op(&step.op)) else {
            continue;
        };
        out.push(name.clone());
        for ch in spec.outputs.iter().skip(1) {
            out.push(format!("{name}.{ch}"));
        }
    }
    out
}

fn unique_id(textures: &[TextureDef], base: &str) -> String {
    if !textures.iter().any(|t| t.id == base) {
        return base.to_owned();
    }
    for i in 2.. {
        let candidate = format!("{base}_{i}");
        if !textures.iter().any(|t| t.id == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn starter_texture(id: &str) -> TextureDef {
    TextureDef {
        id: id.to_owned(),
        layers: vec![starter_layer()],
        finish: None,
    }
}

fn starter_layer() -> LayerDef {
    let mut ramp = Step::new("ramp");
    ramp.params.insert(
        "stops".into(),
        ParamValue::Stops(vec![
            RampStop {
                pos: 0.0,
                color: [0.3, 0.3, 0.3],
            },
            RampStop {
                pos: 1.0,
                color: [0.7, 0.7, 0.7],
            },
        ]),
    );
    LayerDef {
        steps: vec![Step::new("fbm"), ramp],
        ..LayerDef::default()
    }
}

fn starter_step(op: &str) -> Step {
    let mut step = Step::new(op);
    if let Some(spec) = find_op(op) {
        for ps in spec.params {
            if matches!(ps.ty, ParamType::Stops) {
                step.params.insert(
                    ps.name.to_owned(),
                    ParamValue::Stops(vec![
                        RampStop {
                            pos: 0.0,
                            color: [0.2, 0.2, 0.2],
                        },
                        RampStop {
                            pos: 1.0,
                            color: [0.8, 0.8, 0.8],
                        },
                    ]),
                );
            }
        }
    }
    step
}

fn category_name(cat: OpCategory) -> &'static str {
    match cat {
        OpCategory::GreyGen => "generator",
        OpCategory::GreyFilter => "filter",
        OpCategory::Paint => "paint",
    }
}

fn preview_hash(generation: u64, sel: usize, flat: &FlatPreview) -> u64 {
    let mut h = generation.wrapping_mul(31).wrapping_add(sel as u64);
    h = h.wrapping_mul(31).wrapping_add(flat.span.to_bits() as u64);
    h = h
        .wrapping_mul(31)
        .wrapping_add(flat.origin.x.to_bits() as u64);
    h = h
        .wrapping_mul(31)
        .wrapping_add(flat.origin.y.to_bits() as u64);
    h.wrapping_mul(31).wrapping_add(flat.with_finish as u64)
}

fn grey_image(buf: &GreyBuf, out: usize) -> egui::ColorImage {
    let mut img = egui::ColorImage::new([out, out], vec![egui::Color32::BLACK; out * out]);
    for y in 0..out {
        for x in 0..out {
            let v = buf.sample_wrap((x as f32 + 0.5) / out as f32, (y as f32 + 0.5) / out as f32);
            let c = (v.clamp(0.0, 1.0) * 255.0) as u8;
            img.pixels[y * out + x] = egui::Color32::from_gray(c);
        }
    }
    img
}

fn canvas_image(buf: &ColorBuf, out: usize) -> egui::ColorImage {
    let mut img = egui::ColorImage::new([out, out], vec![egui::Color32::BLACK; out * out]);
    let size = buf.size as usize;
    for y in 0..out {
        for x in 0..out {
            let sx = (x * size / out).min(size - 1);
            let sy = (y * size / out).min(size - 1);
            let px = buf.data[sy * size + sx];
            // Composite over a checkerboard so coverage reads at a glance.
            let checker = if ((x / 8) + (y / 8)) % 2 == 0 {
                0.25
            } else {
                0.35
            };
            let a = px[3].clamp(0.0, 1.0);
            let c = |i: usize| ((px[i] * a + checker * (1.0 - a)).clamp(0.0, 1.0) * 255.0) as u8;
            img.pixels[y * out + x] = egui::Color32::from_rgb(c(0), c(1), c(2));
        }
    }
    img
}
