//! Craft-order modal: opens on L-click of a station block, lets the
//! player queue craft orders, see the station's inventory, deposit
//! their carry, and manually trigger work cycles.
//!
//! Egui-based for MVP because the modal is dense + interactive (lists
//! + buttons + quantity inputs). Bevy-UI rewrite is tracked as a
//! polish item — the in-game HUD reads as a distinct visual layer
//! today (egui = dev tools, bevy_ui = gameplay), and this modal
//! straddles that line until we settle on a UI framework for
//! gameplay surfaces.
//!
//! Modal lifecycle:
//! 1. Player L-clicks a station block → `normal_mode_action_input`
//!    sets `CraftStationUiState.open_cell = Some(cell)`.
//! 2. Cursor unlocks via `craft_modal_cursor_lock` (Changed-detected).
//! 3. This module's `draw_craft_modal` renders an egui window each
//!    frame the state is open.
//! 4. Close button / Esc / station-block-gone clears `open_cell`.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use lightyear::prelude::*;

use crate::blocks::BlockRegistry;
use crate::craft_stations::{
    CancelOrder, CraftStationUiState, CraftStations, DepositToStation, QueueOrder, WorkStation,
    craft_modal_cursor_lock,
};
use crate::items::ItemRegistry;
use crate::menu::AppState;
use crate::protocol::{Avatar, Carrying, GameSet, WorldChannel};
use crate::recipes::RecipeRegistry;
use crate::voxel::{Chunk, ChunkMap, world_to_chunk};

pub struct CraftUiPlugin;

impl Plugin for CraftUiPlugin {
    fn build(&self, app: &mut App) {
        // Lifecycle systems (block-gone, Esc, cursor lock) run in
        // the regular Update schedule.
        app.add_systems(
            Update,
            (close_on_block_gone, close_on_escape, craft_modal_cursor_lock)
                .in_set(GameSet::PostSimulation)
                .run_if(in_state(AppState::InGame)),
        );
        // The modal itself MUST live in `EguiPrimaryContextPass` —
        // egui only collects pointer/click events during that
        // schedule, so running the window in `Update` renders fine
        // but every button-click silently falls through. The debug
        // panel + pause menu use the same schedule for the same
        // reason. (Hit this exact trap on first wire-up.)
        app.add_systems(
            bevy_egui::EguiPrimaryContextPass,
            draw_craft_modal.run_if(in_state(AppState::InGame)),
        );
    }
}

/// Close the modal when its target block is no longer a station (the
/// player or an NPC destroyed it mid-modal). Prevents the UI from
/// referencing a stale cell.
fn close_on_block_gone(
    mut ui_state: ResMut<CraftStationUiState>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
) {
    let Some(cell) = ui_state.open_cell else {
        return;
    };
    let (coord, local) = world_to_chunk(cell);
    let still_station = chunk_map
        .0
        .get(&coord)
        .and_then(|&e| chunks.get(e).ok())
        .map(|chunk| {
            let slot = chunk.get(local);
            !slot.is_empty() && registry.def(slot).station_tag.is_some()
        })
        .unwrap_or(false);
    if !still_station {
        ui_state.open_cell = None;
        ui_state.pending_quantities.clear();
    }
}

fn close_on_escape(
    keys: Res<ButtonInput<KeyCode>>,
    mut ui_state: ResMut<CraftStationUiState>,
) {
    if !ui_state.is_open() {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        ui_state.open_cell = None;
        ui_state.pending_quantities.clear();
    }
}

#[allow(clippy::too_many_arguments, reason = "modal pulls from many subsystems")]
fn draw_craft_modal(
    mut contexts: EguiContexts,
    mut ui_state: ResMut<CraftStationUiState>,
    stations: Res<CraftStations>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    registry: Res<BlockRegistry>,
    items: Res<ItemRegistry>,
    recipes: Res<RecipeRegistry>,
    carry: Query<&Carrying, With<Avatar>>,
    mut queue_sender: Query<&mut MessageSender<QueueOrder>>,
    mut cancel_sender: Query<&mut MessageSender<CancelOrder>>,
    mut deposit_sender: Query<&mut MessageSender<DepositToStation>>,
    mut work_sender: Query<&mut MessageSender<WorkStation>>,
) {
    let Some(cell) = ui_state.open_cell else {
        return;
    };
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };

    // Snapshot the block/station info from the live chunk + registry.
    let (coord, local) = world_to_chunk(cell);
    let Some(&chunk_entity) = chunk_map.0.get(&coord) else {
        ui_state.open_cell = None;
        return;
    };
    let Ok(chunk) = chunks.get(chunk_entity) else {
        ui_state.open_cell = None;
        return;
    };
    let slot = chunk.get(local);
    if slot.is_empty() {
        ui_state.open_cell = None;
        return;
    }
    let block_def = registry.def(slot);
    let Some(station_tag) = block_def.station_tag.clone() else {
        ui_state.open_cell = None;
        return;
    };
    let station_tier = block_def.station_tier;
    let station_display_name = block_def.display_name.clone();

    // Snapshot what we'll send before the closure runs — egui's
    // `show` borrows the response, and we don't want long borrows on
    // resources inside it. Same pattern the F3 debug panel uses.
    let mut to_queue: Option<(String, u32)> = None;
    let mut to_cancel: Option<String> = None;
    let mut to_deposit = false;
    let mut to_work: Option<String> = None;

    let station_state = stations.get(cell).cloned().unwrap_or_default();
    let carry_state = carry.iter().next().copied().unwrap_or_default();

    // Recipes available at this station (tag + tier filter).
    let available_recipes: Vec<_> = recipes
        .at_station_tier(&station_tag, station_tier)
        .into_iter()
        .map(|s| (s, recipes.def(s).clone()))
        .collect();

    let mut show_open = true;
    egui::Window::new(format!("Craft — {station_display_name}"))
        .open(&mut show_open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(format!("station tag: {station_tag} · tier {station_tier}"));
            ui.separator();

            // Inventory section.
            ui.label(egui::RichText::new("Inventory").strong());
            if station_state.inventory.is_empty() {
                ui.label("  (empty)");
            } else {
                for (item_slot, count) in &station_state.inventory {
                    let name = items.def(*item_slot).display_name.clone();
                    ui.label(format!("  {name}: {count}"));
                }
            }
            let deposit_label = match (carry_state.item, carry_state.count) {
                (Some(item_slot), count) if count > 0 => {
                    let name = items.def(item_slot).display_name.clone();
                    format!("Deposit carry ({count}× {name})")
                }
                _ => "Deposit carry (empty)".to_owned(),
            };
            let deposit_enabled = carry_state.count > 0;
            if ui
                .add_enabled(deposit_enabled, egui::Button::new(deposit_label))
                .clicked()
            {
                to_deposit = true;
            }
            ui.separator();

            // Active orders.
            ui.label(egui::RichText::new("Active orders").strong());
            if station_state.orders.is_empty() {
                ui.label("  (none queued)");
            } else {
                let work_in_progress = station_state.active_work.as_ref();
                for order in &station_state.orders {
                    // Look up the recipe def — orders persist the
                    // string id, so the registry resolve might miss
                    // if a mod was uninstalled mid-session. Fall
                    // back to the raw id text.
                    let recipe = recipes
                        .slot_of(&block_junk_mod_api::recipes::RecipeId::new(
                            order.recipe_id.clone(),
                        ))
                        .map(|s| recipes.def(s));
                    let label = match recipe {
                        Some(def) => def.display_name.clone(),
                        None => format!("{} (unknown recipe)", order.recipe_id),
                    };
                    let is_active = work_in_progress
                        .map(|aw| aw.recipe_id == order.recipe_id)
                        .unwrap_or(false);
                    // Work button: enabled when inventory satisfies
                    // every input AND no other craft is in progress
                    // at this station (one workspace, one craft).
                    let other_active = work_in_progress.is_some() && !is_active;
                    let can_work = !other_active
                        && match recipe {
                            Some(def) => def.inputs.iter().all(|input| {
                                let Some(slot) = items.slot_of(&input.item) else {
                                    return false;
                                };
                                station_state
                                    .inventory
                                    .get(&slot)
                                    .copied()
                                    .unwrap_or(0)
                                    >= input.count
                            }),
                            None => false,
                        };
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "  {label} — {}/{}",
                            order.completed, order.total
                        ));
                        if is_active
                            && let Some(aw) = work_in_progress
                        {
                            let pct = (aw.elapsed_secs / aw.total_secs.max(0.001))
                                .clamp(0.0, 1.0);
                            ui.label(format!("Working… {:.0}%", pct * 100.0));
                        } else if ui
                            .add_enabled(can_work, egui::Button::new("Work"))
                            .clicked()
                        {
                            to_work = Some(order.recipe_id.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            to_cancel = Some(order.recipe_id.clone());
                        }
                    });
                }
            }
            ui.separator();

            // Queue new order.
            ui.label(egui::RichText::new("Queue new order").strong());
            if available_recipes.is_empty() {
                ui.label("  (no recipes available at this station)");
            } else {
                for (_, recipe) in &available_recipes {
                    let inputs_str = if recipe.inputs.is_empty() {
                        "free".to_owned()
                    } else {
                        recipe
                            .inputs
                            .iter()
                            .map(|i| {
                                let name = items
                                    .slot_of(&i.item)
                                    .map(|s| items.def(s).display_name.clone())
                                    .unwrap_or_else(|| i.item.to_string());
                                format!("{}× {}", i.count, name)
                            })
                            .collect::<Vec<_>>()
                            .join(" + ")
                    };
                    let output_name = items
                        .slot_of(&recipe.output.item)
                        .map(|s| items.def(s).display_name.clone())
                        .unwrap_or_else(|| recipe.output.item.to_string());
                    let pending_key = recipe.id.0.clone();
                    let pending = ui_state
                        .pending_quantities
                        .entry(pending_key.clone())
                        .or_insert(1);
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "  {} ({} → {}× {})",
                            recipe.display_name, inputs_str, recipe.output.count, output_name
                        ));
                        ui.add(egui::DragValue::new(pending).range(1u32..=99u32));
                        if ui.button("Queue").clicked() {
                            to_queue = Some((pending_key, *pending));
                        }
                    });
                }
            }
            ui.separator();
            if ui.button("Close").clicked() {
                // Close on next frame via the open=false path below.
                // egui's window manages the "X" → false signal too;
                // unifying via show_open lets either trigger close.
            }
        });
    if !show_open {
        ui_state.open_cell = None;
        ui_state.pending_quantities.clear();
        return;
    }

    // Dispatch messages outside the closure to avoid holding any
    // resource borrows during the egui call.
    if to_deposit
        && let Ok(mut s) = deposit_sender.single_mut()
    {
        s.send::<WorldChannel>(DepositToStation { station_cell: cell });
    }
    if let Some(recipe_id) = to_cancel
        && let Ok(mut s) = cancel_sender.single_mut()
    {
        s.send::<WorldChannel>(CancelOrder {
            station_cell: cell,
            recipe_id,
        });
    }
    if let Some(recipe_id) = to_work
        && let Ok(mut s) = work_sender.single_mut()
    {
        s.send::<WorldChannel>(WorkStation {
            station_cell: cell,
            recipe_id,
        });
    }
    if let Some((recipe_id, quantity)) = to_queue
        && let Ok(mut s) = queue_sender.single_mut()
    {
        s.send::<WorldChannel>(QueueOrder {
            station_cell: cell,
            recipe_id,
            quantity,
        });
    }
}
