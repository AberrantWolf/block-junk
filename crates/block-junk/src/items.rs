//! Engine-side item registry. Owns the canonical [`ItemDef`] for every
//! registered item and maps between stable [`ItemId`] strings and the
//! compact [`ItemSlot`] handle used on the wire and in carry / drop
//! payloads.
//!
//! Mirrors the [`BlockRegistry`](crate::blocks::BlockRegistry) shape one
//! layer up — separate slot space, separate registration call in Lua,
//! separate boot-time validator. Empty/none is *not* a reserved slot: an
//! actor with no carry stack uses `None`, not a sentinel item.

use std::collections::HashMap;

use bevy::prelude::*;
use block_junk_mod_api::blocks::BlockDef;
use block_junk_mod_api::items::{ItemDef, ItemId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Compact numeric handle for a registered item. Two bytes per carry
/// entry, stable for a session. Mods never see this — they use [`ItemId`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ItemSlot(pub u16);

#[derive(Debug, Error)]
pub enum ItemBootstrapError {
    #[error("duplicate item id {0}")]
    DuplicateItemId(ItemId),
    #[error("item registry exceeds u16 slot space ({slots} items registered)")]
    SlotOverflow { slots: usize },
    #[error("block {block} drops references unregistered item {item}")]
    DropItemUnknown { block: String, item: ItemId },
    #[error("block {block} drops entry for item {item} has count = 0; remove the entry or set count > 0")]
    DropCountZero { block: String, item: ItemId },
}

/// Finalised item registry. Held as a Bevy `Resource` on each side.
#[derive(Resource)]
pub struct ItemRegistry {
    defs_by_slot: Vec<ItemDef>,
    slot_by_id: HashMap<ItemId, ItemSlot>,
}

impl ItemRegistry {
    /// Validate and assign slots to the pending item list. Slots run from
    /// 0 in registration order. Unlike [`crate::blocks::BlockRegistry`],
    /// no slot is reserved — slot 0 is whatever item registered first.
    pub fn build(pending: Vec<ItemDef>) -> Result<Self, ItemBootstrapError> {
        if pending.len() > u16::MAX as usize {
            return Err(ItemBootstrapError::SlotOverflow {
                slots: pending.len(),
            });
        }
        let mut slot_by_id = HashMap::with_capacity(pending.len());
        for (i, def) in pending.iter().enumerate() {
            let slot = ItemSlot(i as u16);
            if slot_by_id.insert(def.id.clone(), slot).is_some() {
                return Err(ItemBootstrapError::DuplicateItemId(def.id.clone()));
            }
        }
        Ok(Self {
            defs_by_slot: pending,
            slot_by_id,
        })
    }

    pub fn def(&self, slot: ItemSlot) -> &ItemDef {
        &self.defs_by_slot[slot.0 as usize]
    }

    pub fn slot_of(&self, id: &ItemId) -> Option<ItemSlot> {
        self.slot_by_id.get(id).copied()
    }

    pub fn id_of(&self, slot: ItemSlot) -> &ItemId {
        &self.defs_by_slot[slot.0 as usize].id
    }

    pub fn slot_count(&self) -> usize {
        self.defs_by_slot.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ItemSlot, &ItemDef)> {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, def)| (ItemSlot(i as u16), def))
    }

    /// Cross-check every `BlockDef.drops` entry against this registry.
    /// Runs at boot after both registries are built so neither side
    /// loads with a dangling drop reference. Empty-vec drops are
    /// always valid; this only catches typos and stale ids.
    pub fn validate_block_drops(
        &self,
        blocks: &[BlockDef],
    ) -> Result<(), ItemBootstrapError> {
        for def in blocks {
            for drop in &def.drops {
                if self.slot_of(&drop.item).is_none() {
                    return Err(ItemBootstrapError::DropItemUnknown {
                        block: def.id.to_string(),
                        item: drop.item.clone(),
                    });
                }
                if drop.count == 0 {
                    return Err(ItemBootstrapError::DropCountZero {
                        block: def.id.to_string(),
                        item: drop.item.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}
