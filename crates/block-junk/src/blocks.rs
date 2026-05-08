//! Engine-side block registry. Owns the canonical [`BlockDef`] for every
//! registered block and maps between stable [`BlockId`] strings and the
//! compact [`BlockSlot`] handle used in chunk storage and on the wire.
//!
//! Slot 0 is reserved for `vanilla:empty` so zeroed memory means air. Slots
//! are assigned in registration order and never reused. [`WorldSlots`] is
//! the persistence shape — kept in memory today, written to disk when
//! world saves land.

use std::collections::HashMap;

use bevy::prelude::*;
use block_junk_mod_api::blocks::{BlockDef, BlockId};
use block_mesh::{MergeVoxel, Voxel, VoxelVisibility};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Compact numeric handle for a registered block. Two bytes per cell in
/// chunk storage, stable for a session (and across sessions once
/// [`WorldSlots`] hits disk). Mods never see this — they use [`BlockId`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct BlockSlot(pub u16);

impl BlockSlot {
    /// The empty/air block. Always slot 0 by registry construction; the
    /// engine refuses to start otherwise.
    pub const EMPTY: BlockSlot = BlockSlot(0);

    pub fn is_empty(self) -> bool {
        self == Self::EMPTY
    }
}

/// Reasons the engine refuses to finalise its block registry. All loud —
/// the engine should not silently boot with a degraded registry.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("vanilla:empty must register first (it occupies slot 0); got {got:?}")]
    EmptyNotFirst { got: Option<BlockId> },
    #[error("duplicate block id {0}")]
    DuplicateBlockId(BlockId),
    #[error("registry exceeds u16 slot space ({slots} blocks registered)")]
    SlotOverflow { slots: usize },
}

/// The live, finalised registry. Held as a Bevy `Resource` on each side.
#[derive(Resource)]
pub struct BlockRegistry {
    defs_by_slot: Vec<BlockDef>,
    slot_by_id: HashMap<BlockId, BlockSlot>,
}

impl BlockRegistry {
    /// Validate and assign slots to the pending block list. Slots run from
    /// 0 in registration order; `vanilla:empty` *must* be first. Returns
    /// the registry plus a [`WorldSlots`] table ready to persist.
    pub fn build(pending: Vec<BlockDef>) -> Result<(BlockRegistry, WorldSlots), BootstrapError> {
        let first = pending.first().map(|d| d.id.clone());
        if first.as_ref().map(BlockId::as_str) != Some("vanilla:empty") {
            return Err(BootstrapError::EmptyNotFirst { got: first });
        }
        if pending.len() > u16::MAX as usize {
            return Err(BootstrapError::SlotOverflow {
                slots: pending.len(),
            });
        }

        let mut slot_by_id = HashMap::with_capacity(pending.len());
        let mut entries = Vec::with_capacity(pending.len());
        for (i, def) in pending.iter().enumerate() {
            let slot = BlockSlot(i as u16);
            if slot_by_id.insert(def.id.clone(), slot).is_some() {
                return Err(BootstrapError::DuplicateBlockId(def.id.clone()));
            }
            entries.push(SlotEntry {
                slot: i as u16,
                id: def.id.clone(),
                status: SlotStatus::Active,
            });
        }

        let registry = BlockRegistry {
            defs_by_slot: pending,
            slot_by_id,
        };
        Ok((registry, WorldSlots { slots: entries }))
    }

    pub fn def(&self, slot: BlockSlot) -> &BlockDef {
        &self.defs_by_slot[slot.0 as usize]
    }

    pub fn slot_of(&self, id: &BlockId) -> Option<BlockSlot> {
        self.slot_by_id.get(id).copied()
    }

    /// Resolve an id that the engine *requires* to exist (e.g. `vanilla:stone`
    /// from terrain gen). Panics with a clear message if missing — that's a
    /// load-order or vanilla-mod-content bug, not a runtime error.
    pub fn require(&self, id: &str) -> BlockSlot {
        self.slot_of(&BlockId::new(id))
            .unwrap_or_else(|| panic!("required block id not registered: {id}"))
    }

    pub fn id_of(&self, slot: BlockSlot) -> &BlockId {
        &self.defs_by_slot[slot.0 as usize].id
    }

    pub fn iter_placeable(&self) -> impl Iterator<Item = BlockSlot> + '_ {
        self.defs_by_slot
            .iter()
            .enumerate()
            .filter(|(_, d)| d.flags.placeable)
            .map(|(i, _)| BlockSlot(i as u16))
    }

    pub fn slot_count(&self) -> usize {
        self.defs_by_slot.len()
    }

    /// Iterate every registered (slot, def) pair in slot order. Used to
    /// build the `BlockManifest` server → client message.
    pub fn iter(&self) -> impl Iterator<Item = (BlockSlot, &BlockDef)> + '_ {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, d)| (BlockSlot(i as u16), d))
    }
}

/// Slots the engine's terrain generator resolves once at startup so it
/// doesn't have to hash strings during chunk gen. Cloned (it's pure u16s)
/// into async terrain tasks; no registry lock-up needed on workers.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainSlots {
    pub empty: BlockSlot,
    pub stone: BlockSlot,
    pub dirt: BlockSlot,
    pub grass: BlockSlot,
}

impl TerrainSlots {
    pub fn from_registry(reg: &BlockRegistry) -> Self {
        Self {
            empty: BlockSlot::EMPTY,
            stone: reg.require("vanilla:stone"),
            dirt: reg.require("vanilla:dirt"),
            grass: reg.require("vanilla:grass"),
        }
    }
}

/// Persistent slot ↔ id table. Currently lives only in memory; once world
/// saves land, this is what gets serialised to disk and diffed on load to
/// preserve slot stability across mod-set changes.
#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct WorldSlots {
    pub slots: Vec<SlotEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotEntry {
    pub slot: u16,
    pub id: BlockId,
    pub status: SlotStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotStatus {
    /// Block is currently registered.
    Active,
    /// Saved id has no current registration. Slot is reserved (never reused);
    /// chunks containing it should render as a placeholder until the missing
    /// mod is restored. (Reachable once world saves are added — kept here so
    /// the type is stable.)
    Missing,
}

/// `block-mesh` voxel input that carries pre-computed visibility. Each
/// chunk converts its `Vec<BlockSlot>` to `Vec<MeshVoxel>` at mesh time
/// using the registry, so non-cube blocks (those with a custom mesh) can
/// be excluded from the greedy cube-meshing pass while still merging
/// correctly within their slot type.
///
/// We don't impl `Voxel` directly on `BlockSlot` because empty-vs-opaque
/// isn't slot-only — a slot with a `mesh` path renders as a separate
/// entity, so its cell should be treated as Empty for the cube mesher.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MeshVoxel {
    pub slot: BlockSlot,
    pub visibility: VoxelVisibility,
}

impl Voxel for MeshVoxel {
    fn get_visibility(&self) -> VoxelVisibility {
        self.visibility
    }
}

impl MergeVoxel for MeshVoxel {
    type MergeValue = u16;
    fn merge_value(&self) -> u16 {
        self.slot.0
    }
}

impl BlockRegistry {
    /// Visibility a slot should have for the voxel cube mesher: Empty
    /// for `vanilla:empty` AND for any slot with a custom mesh (those
    /// render as ECS entities rather than as cube faces).
    pub fn voxel_visibility(&self, slot: BlockSlot) -> VoxelVisibility {
        if slot.is_empty() || self.def(slot).mesh.is_some() {
            VoxelVisibility::Empty
        } else {
            VoxelVisibility::Opaque
        }
    }
}
