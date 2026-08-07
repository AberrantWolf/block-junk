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

/// How many units of a single item the player avatar can hold. About
/// 1.5–2× a vanilla NPC's [`NpcKindDef::carry_capacity`] (currently 3)
/// per the design intent — players feel meaningfully more capable than
/// individual NPCs, but the gap stays narrow enough that hauling a
/// large build by hand reads as a slog instead of a strategy. Engine
/// const for now; lift to per-kind data if a second player kind ever
/// ships.
pub const PLAYER_CARRY_CAPACITY: u32 = 5;

/// How far a loose item scans downward for solid support before giving
/// up and clamping in place. Because unloaded chunks read as solid (see
/// [`crate::pathfinding::Walkability`]), a real fall almost always stops
/// at the loaded/unloaded boundary long before this; the cap only bounds
/// a pathological tall fully-loaded empty column. An item that finds no
/// support within the budget is clamped, never deleted — losing a
/// resource to a bottomless scan is the one outcome we refuse.
pub(crate) const MAX_ITEM_DROP: i32 = 512;

/// How far a buried item rises to escape a cell that just became solid
/// (a block built where the item sat). One chunk's worth of height — past
/// that we leave it embedded and let a later destroy re-settle it, rather
/// than teleporting it an unbounded distance.
pub(crate) const MAX_ITEM_RISE: i32 = 32;

/// Tiny lift off the support face so an item mesh isn't bisected by the
/// top face of the block it rests on. The item's world Y is always
/// `owning_cell.y + ITEM_FLOOR_LIFT`, which keeps `translation.floor()`
/// recovering the owning cell.
pub(crate) const ITEM_FLOOR_LIFT: f32 = 0.05;

/// Deterministic-per-spawn lateral jitter for piles of dropped items, so
/// siblings don't perfectly overlap and a pile reads as a heap. Doesn't
/// need cross-session reproducibility — just spatial variety. Shared by
/// block drops, station-destroy spills, and disconnect drops.
pub(crate) fn drop_jitter(cell: bevy::math::IVec3, unit_index: u32) -> bevy::math::Vec3 {
    let h = (cell.x as i64)
        .wrapping_mul(73_856_093)
        .wrapping_add((cell.y as i64).wrapping_mul(19_349_663))
        .wrapping_add((cell.z as i64).wrapping_mul(83_492_791))
        .wrapping_add(unit_index as i64 * 2_654_435_761) as u64;
    let fx = ((h & 0xFFFF) as f32 / 65535.0 - 0.5) * 0.4;
    let fz = (((h >> 16) & 0xFFFF) as f32 / 65535.0 - 0.5) * 0.4;
    bevy::math::Vec3::new(fx, 0.0, fz)
}

#[derive(Debug, Error)]
pub enum ItemBootstrapError {
    #[error("duplicate item id {0}")]
    DuplicateItemId(ItemId),
    #[error("item registry exceeds u16 slot space ({slots} items registered)")]
    SlotOverflow { slots: usize },
    #[error("block {block} drops references unregistered item {item}")]
    DropItemUnknown { block: String, item: ItemId },
    #[error(
        "block {block} drops entry for item {item} has count = 0; remove the entry or set count > 0"
    )]
    DropCountZero { block: String, item: ItemId },
    #[error("block {block} materials references unregistered item {item}")]
    MaterialItemUnknown { block: String, item: ItemId },
    #[error(
        "block {block} materials entry for item {item} has count = 0; remove the entry or set count > 0"
    )]
    MaterialCountZero { block: String, item: ItemId },
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
        self.try_def(slot)
            .unwrap_or_else(|| panic!("invalid item slot {}", slot.0))
    }

    /// Fallible lookup for values originating on the wire or disk.
    pub fn try_def(&self, slot: ItemSlot) -> Option<&ItemDef> {
        self.defs_by_slot.get(slot.0 as usize)
    }

    pub fn slot_of(&self, id: &ItemId) -> Option<ItemSlot> {
        self.slot_by_id.get(id).copied()
    }

    pub fn id_of(&self, slot: ItemSlot) -> &ItemId {
        self.try_id_of(slot)
            .unwrap_or_else(|| panic!("invalid item slot {}", slot.0))
    }

    pub fn try_id_of(&self, slot: ItemSlot) -> Option<&ItemId> {
        self.try_def(slot).map(|def| &def.id)
    }

    /// Does the item in `slot` carry `tag` in its `tool_tags`? A `None`
    /// slot (empty tool / empty carry) is always false. Used by the
    /// tool-gating path on both client (outline tint) and server (work
    /// validation).
    pub fn tool_has_tag(
        &self,
        slot: Option<ItemSlot>,
        tag: &block_junk_mod_api::blocks::TagId,
    ) -> bool {
        let Some(slot) = slot else {
            return false;
        };
        self.try_def(slot)
            .is_some_and(|def| def.tool_tags.iter().any(|t| t == tag))
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

    /// Cross-check every `BlockDef.drops` and `BlockDef.materials`
    /// entry against this registry. Runs at boot, after `resolve_drops`
    /// has finalised each def, so defaulted drop lists get the same
    /// scrutiny as explicit ones (redundant with the materials check
    /// when defaulted — harmless). Empty drops/materials are always
    /// valid; this only catches typos and stale ids.
    pub fn validate_block_drops(&self, blocks: &[BlockDef]) -> Result<(), ItemBootstrapError> {
        for def in blocks {
            for drop in def.resolved_drops() {
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
            for mat in &def.materials {
                if self.slot_of(&mat.item).is_none() {
                    return Err(ItemBootstrapError::MaterialItemUnknown {
                        block: def.id.to_string(),
                        item: mat.item.clone(),
                    });
                }
                if mat.count == 0 {
                    return Err(ItemBootstrapError::MaterialCountZero {
                        block: def.id.to_string(),
                        item: mat.item.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}
