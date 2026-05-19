//! Engine-side recipe registry. Holds the canonical [`RecipeDef`] for
//! every registered recipe and maps between stable [`RecipeId`]
//! strings and the compact [`RecipeSlot`] handle used at runtime.
//!
//! Mirrors the [`ItemRegistry`](crate::items::ItemRegistry) shape one
//! layer up — separate slot space, separate registration call in Lua,
//! separate boot-time validator that cross-checks every input/output
//! item id resolves and every `station` tag has at least one matching
//! block.
//!
//! Empty/none is *not* a reserved slot: the engine queries by
//! station-tag lookup, not by slot, so there's no need for a sentinel.

use std::collections::HashMap;

use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockDef, TagId};
use block_junk_mod_api::items::ItemId;
use block_junk_mod_api::recipes::{RecipeDef, RecipeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::items::ItemRegistry;

/// Compact numeric handle for a registered recipe. Stable for a session.
/// Mods never see this — they use [`RecipeId`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RecipeSlot(pub u16);

/// Upper-clamp on recipe `duration_secs` so a misbehaving mod can't
/// freeze a crafter for an hour. Matches the order-of-magnitude bound
/// on Interactable / WorkAction durations elsewhere in the engine.
pub const MAX_RECIPE_DURATION_SECS: f32 = 120.0;
/// Lower-clamp so a near-zero duration doesn't read as an animation
/// glitch.
pub const MIN_RECIPE_DURATION_SECS: f32 = 0.1;

#[derive(Debug, Error)]
pub enum RecipeBootstrapError {
    #[error("duplicate recipe id {0}")]
    DuplicateRecipeId(RecipeId),
    #[error("recipe registry exceeds u16 slot space ({slots} recipes registered)")]
    SlotOverflow { slots: usize },
    #[error("recipe {recipe} input references unregistered item {item}")]
    InputItemUnknown { recipe: RecipeId, item: ItemId },
    #[error("recipe {recipe} output references unregistered item {item}")]
    OutputItemUnknown { recipe: RecipeId, item: ItemId },
    #[error(
        "recipe {recipe} declares station tag {tag:?} but no registered block carries that station_tag"
    )]
    StationUnreachable { recipe: RecipeId, tag: TagId },
    #[error(
        "recipe {recipe} duration_secs {got} is out of bounds [{min}, {max}]"
    )]
    DurationOutOfRange {
        recipe: RecipeId,
        got: f32,
        min: f32,
        max: f32,
    },
}

/// Finalised recipe registry. Held as a Bevy `Resource` on each side.
#[derive(Resource)]
pub struct RecipeRegistry {
    defs_by_slot: Vec<RecipeDef>,
    slot_by_id: HashMap<RecipeId, RecipeSlot>,
    /// Reverse-index: which recipes run at which station tag. Built
    /// once at boot so per-click lookups in the crafting handler are
    /// O(recipes-at-this-station), not O(all-recipes). Vec rather
    /// than Set since recipe order is the deterministic
    /// "first-match-wins" order players (and the future planner)
    /// see.
    by_station: HashMap<TagId, Vec<RecipeSlot>>,
}

impl RecipeRegistry {
    /// Validate and assign slots to the pending recipe list. Slots
    /// run from 0 in registration order. Cross-checks duration
    /// bounds + item id resolution; station-tag reachability is
    /// checked separately in [`Self::validate_against_blocks`] since
    /// it needs the [`BlockRegistry`](crate::blocks::BlockRegistry)
    /// snapshot.
    pub fn build(
        pending: Vec<RecipeDef>,
        items: &ItemRegistry,
    ) -> Result<Self, RecipeBootstrapError> {
        if pending.len() > u16::MAX as usize {
            return Err(RecipeBootstrapError::SlotOverflow {
                slots: pending.len(),
            });
        }
        let mut slot_by_id = HashMap::with_capacity(pending.len());
        let mut by_station: HashMap<TagId, Vec<RecipeSlot>> = HashMap::new();
        for (i, def) in pending.iter().enumerate() {
            let slot = RecipeSlot(i as u16);
            if slot_by_id.insert(def.id.clone(), slot).is_some() {
                return Err(RecipeBootstrapError::DuplicateRecipeId(def.id.clone()));
            }
            if !def.duration_secs.is_finite()
                || def.duration_secs < MIN_RECIPE_DURATION_SECS
                || def.duration_secs > MAX_RECIPE_DURATION_SECS
            {
                return Err(RecipeBootstrapError::DurationOutOfRange {
                    recipe: def.id.clone(),
                    got: def.duration_secs,
                    min: MIN_RECIPE_DURATION_SECS,
                    max: MAX_RECIPE_DURATION_SECS,
                });
            }
            for input in &def.inputs {
                if items.slot_of(&input.item).is_none() {
                    return Err(RecipeBootstrapError::InputItemUnknown {
                        recipe: def.id.clone(),
                        item: input.item.clone(),
                    });
                }
            }
            if items.slot_of(&def.output.item).is_none() {
                return Err(RecipeBootstrapError::OutputItemUnknown {
                    recipe: def.id.clone(),
                    item: def.output.item.clone(),
                });
            }
            by_station.entry(def.station.clone()).or_default().push(slot);
        }
        Ok(Self {
            defs_by_slot: pending,
            slot_by_id,
            by_station,
        })
    }

    /// Cross-check that every recipe's `station` tag is reachable —
    /// i.e. some registered block carries that station_tag. A recipe
    /// with no matching station is a typo; flag it loudly at boot so
    /// the failure happens before the first crafting click rather
    /// than as "L-click did nothing."
    pub fn validate_against_blocks(
        &self,
        blocks: &[BlockDef],
    ) -> Result<(), RecipeBootstrapError> {
        let mut all_station_tags: std::collections::HashSet<&TagId> =
            std::collections::HashSet::new();
        for b in blocks {
            if let Some(tag) = &b.station_tag {
                all_station_tags.insert(tag);
            }
        }
        for def in &self.defs_by_slot {
            if !all_station_tags.contains(&def.station) {
                return Err(RecipeBootstrapError::StationUnreachable {
                    recipe: def.id.clone(),
                    tag: def.station.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn def(&self, slot: RecipeSlot) -> &RecipeDef {
        &self.defs_by_slot[slot.0 as usize]
    }

    pub fn slot_of(&self, id: &RecipeId) -> Option<RecipeSlot> {
        self.slot_by_id.get(id).copied()
    }

    pub fn slot_count(&self) -> usize {
        self.defs_by_slot.len()
    }

    /// Recipes available at a station with this tag, in registration
    /// order. Empty vec when no recipe targets the tag — same return
    /// shape as "tag is wrong"; the handler treats both the same way
    /// (no craftable action). **Doesn't apply tier filtering** — use
    /// [`Self::at_station_tier`] when you want recipes a specific
    /// station instance can actually perform.
    pub fn at_station(&self, tag: &TagId) -> &[RecipeSlot] {
        self.by_station.get(tag).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Recipes available at a station with this tag whose `tier <=
    /// max_tier`. Returns a fresh Vec; allocation is fine because
    /// this is called at click time, not per-tick. Use this for the
    /// craft-order modal's recipe list and any tier-gated server
    /// validation.
    pub fn at_station_tier(&self, tag: &TagId, max_tier: u8) -> Vec<RecipeSlot> {
        self.at_station(tag)
            .iter()
            .copied()
            .filter(|&slot| self.def(slot).tier <= max_tier)
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (RecipeSlot, &RecipeDef)> {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, def)| (RecipeSlot(i as u16), def))
    }
}
