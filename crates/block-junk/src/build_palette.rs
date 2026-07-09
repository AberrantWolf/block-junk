//! Build-palette overlay: the full catalogue of placeable blocks in a
//! category-tabbed egui grid, opened with B in Plan mode. The digit
//! hotbar stops being "the whole registry" and becomes the player's
//! pinned favourites ([`HotbarPins`]) — F1 furniture tripled the
//! placeable count past what nine digits and a wheel can address.
//!
//! Interactions:
//!   - L-click a tile → select it (drives the Plan-mode Build ghost,
//!     same [`SelectedBlock`] the hotbar writes). The window stays
//!     open for browsing; B or Esc closes.
//!   - R-click a tile → toggle it into/out of the hotbar pins (max 9;
//!     pinned tiles show their digit).
//!
//! Lifecycle follows the craft modal's pattern exactly: a state
//! resource holds `open`, `sync_build_palette_capture` mirrors it into
//! [`UiCaptures`] (the cursor/input SSOT — this module never touches
//! the window), `handle_escape` owns the Esc close, and the draw
//! system runs in `EguiPrimaryContextPass` (clicks silently fall
//! through in plain `Update`).
//!
//! Categories are derived from block metadata rather than a def field:
//! stations (station_tag) > storage (container or the vanilla:storage
//! tag) > furniture (any other mesh block) > blocks (voxel cubes).
//! Good enough at ~40 placeables; a mod-facing category field can
//! replace it if the heuristic ever misfiles something important.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiTextureHandle, egui};

use crate::block_textures::BlockTextures;
use crate::blocks::BlockRegistry;
use crate::client::{HotbarPins, PlaceablePalette, SelectedBlock, short_label};
use crate::items::ItemRegistry;
use crate::menu::AppState;
use crate::player_mode::PlayerMode;
use crate::protocol::GameSet;
use crate::ui_capture::{UiCapture, UiCaptures};

/// Tab names, in display order. Index 0 is the synthetic "All" tab;
/// the rest line up with [`category_of`]'s return value + 1.
const TABS: [&str; 5] = ["All", "Blocks", "Furniture", "Storage", "Stations"];

/// Hotbar capacity — one pin per digit key.
pub const MAX_PINS: usize = 9;

#[derive(Resource, Default)]
pub struct BuildPaletteState {
    pub open: bool,
    /// Active tab index into [`TABS`].
    pub tab: usize,
}

/// Which category tab (1-based within [`TABS`]) a placeable belongs to.
fn category_of(def: &block_junk_mod_api::blocks::BlockDef) -> usize {
    if def.station_tag.is_some() {
        4
    } else if def.container.is_some() || def.tags.iter().any(|t| t.as_str() == "vanilla:storage") {
        3
    } else if def.mesh.is_some() {
        2
    } else {
        1
    }
}

/// B toggles the palette. Opening requires Plan mode and no other
/// overlay holding the cursor; closing works whenever the palette is
/// the thing that's open (the capture set is non-empty *because of
/// us*, so `is_captured` can't gate the close path).
fn toggle_on_key(
    keys: Res<ButtonInput<KeyCode>>,
    captures: Res<UiCaptures>,
    mode: Res<PlayerMode>,
    mut state: ResMut<BuildPaletteState>,
) {
    if !keys.just_pressed(KeyCode::KeyB) {
        return;
    }
    if state.open {
        state.open = false;
        return;
    }
    if captures.is_captured() || !matches!(*mode, PlayerMode::Plan) {
        return;
    }
    state.open = true;
}

/// Mirror `state.open` into the capture SSOT, craft-modal style. Also
/// force-closes if the mode somehow leaves Plan while open (belt and
/// braces — Tab is in-world input and gated off while captured).
fn sync_build_palette_capture(
    mode: Res<PlayerMode>,
    mut state: ResMut<BuildPaletteState>,
    mut captures: ResMut<UiCaptures>,
) {
    if state.open && !matches!(*mode, PlayerMode::Plan) {
        state.open = false;
    }
    // Only touch the captures set on an actual state change. An
    // unconditional acquire/release flags `UiCaptures` changed EVERY
    // frame (any &mut method through ResMut trips change detection,
    // mutation or not), which makes apply_cursor_mode re-lock +
    // recentre the cursor and re-arm DiscardNextMotion each frame —
    // discarding every mouse delta and killing mouse-look. Same guard
    // as sync_craft_modal_capture.
    if !state.is_changed() {
        return;
    }
    if state.open {
        captures.acquire(UiCapture::BuildPalette);
    } else {
        captures.release(UiCapture::BuildPalette);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "overlay pulls from many subsystems"
)]
fn draw_build_palette(
    mut contexts: EguiContexts,
    mut state: ResMut<BuildPaletteState>,
    palette: Res<PlaceablePalette>,
    mut pins: ResMut<HotbarPins>,
    mut selected: ResMut<SelectedBlock>,
    blocks: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
    textures: Res<BlockTextures>,
) {
    if !state.open {
        return;
    }
    // Register voxel icon textures with egui before borrowing the ctx.
    // add_image dedupes by asset id, so per-frame registration is a
    // HashMap lookup, not a leak.
    let icon_ids: Vec<Option<egui::TextureId>> = palette
        .0
        .iter()
        .map(|slot| {
            let def = blocks.def(*slot);
            if def.mesh.is_some() {
                None
            } else {
                textures
                    .icons
                    .get(slot.0 as usize)
                    .map(|h| contexts.add_image(EguiTextureHandle::Strong(h.clone())))
            }
        })
        .collect();
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };

    let mut open = true;
    egui::Window::new("Build palette")
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(480.0)
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (i, name) in TABS.iter().enumerate() {
                    if ui.selectable_label(state.tab == i, *name).clicked() {
                        state.tab = i;
                    }
                }
            });
            ui.separator();

            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (idx, slot) in palette.0.iter().enumerate() {
                        let def = blocks.def(*slot);
                        if state.tab != 0 && category_of(def) != state.tab {
                            continue;
                        }
                        let is_selected = selected.0 == Some(idx);
                        let tile = egui::Vec2::splat(48.0);
                        let resp = match icon_ids[idx] {
                            Some(tex) => {
                                let img = egui::Image::new(egui::load::SizedTexture::new(
                                    tex,
                                    [34.0, 34.0],
                                ));
                                ui.add_sized(
                                    tile,
                                    egui::Button::image(img).selected(is_selected),
                                )
                            }
                            None => {
                                // Mesh blocks: their look is the gltf; use
                                // the same short label the hotbar shows,
                                // over the def's swatch colour.
                                let [r, g, b] = def.color;
                                let text = egui::RichText::new(short_label(&def.display_name))
                                    .strong()
                                    .color(egui::Color32::WHITE);
                                ui.add_sized(
                                    tile,
                                    egui::Button::new(text)
                                        .fill(egui::Color32::from_rgb(
                                            (r * 200.0) as u8,
                                            (g * 200.0) as u8,
                                            (b * 200.0) as u8,
                                        ))
                                        .selected(is_selected),
                                )
                            }
                        };
                        // Pin digit badge, top-left of the tile.
                        if let Some(pos) = pins.0.iter().position(|&p| p == idx) {
                            ui.painter().text(
                                resp.rect.left_top() + egui::vec2(4.0, 2.0),
                                egui::Align2::LEFT_TOP,
                                format!("{}", pos + 1),
                                egui::FontId::proportional(11.0),
                                egui::Color32::YELLOW,
                            );
                        }
                        let cost = if def.materials.is_empty() {
                            "free".to_owned()
                        } else {
                            def.materials
                                .iter()
                                .map(|m| {
                                    let name = items
                                        .slot_of(&m.item)
                                        .map(|s| items.def(s).display_name.clone())
                                        .unwrap_or_else(|| m.item.to_string());
                                    format!("{}× {name}", m.count)
                                })
                                .collect::<Vec<_>>()
                                .join(" + ")
                        };
                        let resp = resp.on_hover_text(format!(
                            "{}\ncost: {cost}\nL-click select · R-click pin",
                            def.display_name
                        ));
                        if resp.clicked() {
                            selected.0 = if is_selected { None } else { Some(idx) };
                        }
                        if resp.secondary_clicked() {
                            if let Some(pos) = pins.0.iter().position(|&p| p == idx) {
                                pins.0.remove(pos);
                            } else if pins.0.len() < MAX_PINS {
                                pins.0.push(idx);
                            }
                        }
                    }
                });
            });
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "L-click: select · R-click: pin to hotbar (digits 1-9) · B / Esc: close",
                )
                .small()
                .weak(),
            );
        });
    if !open {
        state.open = false;
    }
}

pub struct BuildPalettePlugin;

impl Plugin for BuildPalettePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BuildPaletteState>();
        app.add_systems(
            Update,
            (toggle_on_key, sync_build_palette_capture)
                .chain()
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
        app.add_systems(
            bevy_egui::EguiPrimaryContextPass,
            draw_build_palette.run_if(in_state(AppState::InGame)),
        );
    }
}
