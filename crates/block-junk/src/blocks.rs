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

use crate::block_textures::TextureRegistry;
use block_mesh::{MergeVoxel, Voxel, VoxelVisibility};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::npc_registry::{AnimationRegistry, NeedRegistry};

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
    #[error("block {block} interactable.need_restore references unregistered need {need}")]
    InteractableNeedUnknown { block: BlockId, need: String },
    #[error(
        "block {block} interactable.need_restore.restores = {value}; must be > 0 and ≤ 1 (need values are deficits in [0, 1])"
    )]
    InteractableRestoresOutOfRange { block: BlockId, value: f32 },
    #[error(
        "block {block} interactable.duration_secs = {value}; must be ≥ {min} (exclusive interactions should feel substantive; non-exclusive ones still need long enough to register visually)"
    )]
    InteractableDurationOutOfRange {
        block: BlockId,
        value: f32,
        min: f32,
    },
    #[error("block {block} work_action.need_restore references unregistered need {need}")]
    WorkActionNeedUnknown { block: BlockId, need: String },
    #[error(
        "block {block} work_action.need_restore.restores = {value}; must be > 0 and ≤ 1 (need values are deficits in [0, 1])"
    )]
    WorkActionRestoresOutOfRange { block: BlockId, value: f32 },
    #[error(
        "block {block} work_action.duration_secs = {value}; must be > 0 (use the engine default by omitting work_action)"
    )]
    WorkActionDurationOutOfRange { block: BlockId, value: f32 },
    #[error("block {block} references texture \"{texture}\", which no mod's textures.lua defines")]
    BlockTextureUnknown { block: BlockId, texture: String },
    #[error(
        "block {block} use_slot.approach is empty — at least one standable cell is required, or omit use_slot to fall back to nearest-neighbour"
    )]
    UseSlotApproachEmpty { block: BlockId },
    #[error(
        "block {block} use_slot.approach cell {cell:?} overlaps the block's own footprint — NPCs can't stand inside the block"
    )]
    UseSlotApproachInsideFootprint { block: BlockId, cell: [i32; 3] },
    #[error("block {block} use_slot.animation references unregistered animation {anim}")]
    UseSlotAnimationUnknown { block: BlockId, anim: String },
    #[error("block {block} {field} references unregistered block {target}")]
    BlockRefUnknown {
        block: BlockId,
        field: &'static str,
        target: BlockId,
    },
    #[error(
        "block {block} regrow.after_secs = {value}; must be > 0 (a zero/negative delay would regrow instantly, thrashing the schedule)"
    )]
    RegrowDelayOutOfRange { block: BlockId, value: f32 },
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

    /// Block id per slot, in slot order — the table a `SaveFile` embeds
    /// so saved chunk grids survive registration-order changes.
    pub fn slot_table(&self) -> Vec<String> {
        self.defs_by_slot
            .iter()
            .map(|d| d.id.as_str().to_owned())
            .collect()
    }

    /// Iterate every registered (slot, def) pair in slot order. Used to
    /// build the `BlockManifest` server → client message.
    pub fn iter(&self) -> impl Iterator<Item = (BlockSlot, &BlockDef)> + '_ {
        self.defs_by_slot
            .iter()
            .enumerate()
            .map(|(i, d)| (BlockSlot(i as u16), d))
    }

    /// Cross-validate interactable-bearing blocks against the
    /// [`NeedRegistry`]. Runs from `scripting.rs::load_side` after
    /// both registries exist (the build step here happens before
    /// needs are known, so we can't fold it in).
    ///
    /// Rules:
    /// - `need_restore.need` (if present) is a registered need id.
    /// - `need_restore.restores` is in (0, 1] — zero or negative is
    ///   either a no-op (bug) or makes the need worse (almost
    ///   certainly a bug); >1 is meaningless because the post-action
    ///   clamp is at 0.0 anyway.
    /// - `duration_secs` ≥ 1.0 for exclusive blocks (sleep should
    ///   feel substantive) and ≥ 0.1 otherwise (shorter eats look
    ///   glitchy — they complete inside one fixed tick at 60 Hz).
    ///   Upper bound is enforced by the brain at execution time,
    ///   not here — a mod author who wants a 5-minute "ritual"
    ///   interaction shouldn't be blocked at boot.
    pub fn validate_interactables(&self, needs: &NeedRegistry) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            let Some(i) = &def.interactable else { continue };
            if let Some(nr) = &i.need_restore {
                if !needs.contains(&nr.need) {
                    return Err(BootstrapError::InteractableNeedUnknown {
                        block: def.id.clone(),
                        need: nr.need.clone(),
                    });
                }
                if !(nr.restores > 0.0 && nr.restores <= 1.0) {
                    return Err(BootstrapError::InteractableRestoresOutOfRange {
                        block: def.id.clone(),
                        value: nr.restores,
                    });
                }
            }
            let min_duration = if i.exclusive { 1.0 } else { 0.1 };
            if i.duration_secs < min_duration {
                return Err(BootstrapError::InteractableDurationOutOfRange {
                    block: def.id.clone(),
                    value: i.duration_secs,
                    min: min_duration,
                });
            }
        }
        Ok(())
    }

    /// Cross-validate block-level `work_action` against the
    /// [`NeedRegistry`]. Same shape as `validate_interactables` —
    /// referenced need ids must exist, magnitudes must land in (0, 1],
    /// duration must be positive. Engine-wide
    /// [`WorkDefaults`](block_junk_mod_api::npcs::WorkDefaults) are
    /// validated separately in `WorkDefaultsRes::build`.
    pub fn validate_work_actions(&self, needs: &NeedRegistry) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            let Some(w) = &def.work_action else { continue };
            if let Some(nr) = &w.need_restore {
                if !needs.contains(&nr.need) {
                    return Err(BootstrapError::WorkActionNeedUnknown {
                        block: def.id.clone(),
                        need: nr.need.clone(),
                    });
                }
                if !(nr.restores > 0.0 && nr.restores <= 1.0) {
                    return Err(BootstrapError::WorkActionRestoresOutOfRange {
                        block: def.id.clone(),
                        value: nr.restores,
                    });
                }
            }
            if w.duration_secs <= 0.0 {
                return Err(BootstrapError::WorkActionDurationOutOfRange {
                    block: def.id.clone(),
                    value: w.duration_secs,
                });
            }
        }
        Ok(())
    }

    /// Cross-validate every block's `texture` reference against the
    /// texture registry. A typo or a missing textures.lua entry is a
    /// load-time error rather than a runtime "fallback color" surprise.
    pub fn validate_textures(&self, textures: &TextureRegistry) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            let Some(tex) = &def.texture else { continue };
            for id in tex.ids() {
                if !textures.contains(id) {
                    return Err(BootstrapError::BlockTextureUnknown {
                        block: def.id.clone(),
                        texture: id.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate `use_slot` declarations on blocks that opt into snap-
    /// to-slot positioning. Two rules: there must be at least one
    /// approach cell (an empty list means "no way to start using the
    /// block," which is almost certainly a typo — authors who want
    /// the old "any cardinal neighbour" behaviour omit the whole
    /// `use_slot`), and no approach cell may sit inside the block's
    /// own footprint (an NPC can't stand inside the block they're
    /// trying to use). Approach cells are in default orientation —
    /// the engine rotates them at use time, so the "inside the
    /// footprint" check happens in the same frame.
    /// Validate that any `use_slot.animation` resolves in the
    /// [`AnimationRegistry`]. Runs after the animation registry has
    /// been built so a typo in a slot's animation id fails the boot
    /// loudly instead of silently leaving the NPC in idle when they
    /// snap to the slot.
    pub fn validate_use_slot_animations(
        &self,
        animations: &AnimationRegistry,
    ) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            let Some(slot) = &def.use_slot else { continue };
            let Some(anim) = &slot.animation else {
                continue;
            };
            if !animations.contains(anim) {
                return Err(BootstrapError::UseSlotAnimationUnknown {
                    block: def.id.clone(),
                    anim: anim.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn validate_use_slots(&self) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            let Some(slot) = &def.use_slot else { continue };
            if slot.approach.is_empty() {
                return Err(BootstrapError::UseSlotApproachEmpty {
                    block: def.id.clone(),
                });
            }
            for cell in &slot.approach {
                if def.footprint.iter().any(|f| f == cell) {
                    return Err(BootstrapError::UseSlotApproachInsideFootprint {
                        block: def.id.clone(),
                        cell: *cell,
                    });
                }
            }
        }
        Ok(())
    }

    /// Cross-validate the storage-arc S4 block-reference fields
    /// (`depleted_block`, `regrow.into`) against this registry, and
    /// bound `regrow.after_secs` above zero. A dangling reference is a
    /// data typo that would otherwise panic at first harvest/regrow;
    /// catch it at boot like every other cross-registry link.
    pub fn validate_transitions(&self) -> Result<(), BootstrapError> {
        for def in &self.defs_by_slot {
            if let Some(target) = &def.depleted_block
                && !self.slot_by_id.contains_key(target)
            {
                return Err(BootstrapError::BlockRefUnknown {
                    block: def.id.clone(),
                    field: "depleted_block",
                    target: target.clone(),
                });
            }
            if let Some(regrow) = &def.regrow {
                if !self.slot_by_id.contains_key(&regrow.into) {
                    return Err(BootstrapError::BlockRefUnknown {
                        block: def.id.clone(),
                        field: "regrow.into",
                        target: regrow.into.clone(),
                    });
                }
                if regrow.after_secs <= 0.0 {
                    return Err(BootstrapError::RegrowDelayOutOfRange {
                        block: def.id.clone(),
                        value: regrow.after_secs,
                    });
                }
            }
        }
        Ok(())
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
    pub wood: BlockSlot,
    pub leaves: BlockSlot,
    pub gravel: BlockSlot,
    /// Storage-arc S4 forage terrain: a ripe berry bush stamped in the
    /// air cell above grass. Harvest/eat depletes it to a bare bush
    /// (via `BlockDef.depleted_block`) which regrows back — those
    /// transitions read block defs directly, so only the ripe stamp
    /// needs a resolved slot here.
    pub berry_bush: BlockSlot,
}

impl TerrainSlots {
    pub fn from_registry(reg: &BlockRegistry) -> Self {
        Self {
            empty: BlockSlot::EMPTY,
            stone: reg.require("vanilla:stone"),
            dirt: reg.require("vanilla:dirt"),
            grass: reg.require("vanilla:grass"),
            wood: reg.require("vanilla:wood"),
            leaves: reg.require("vanilla:leaves"),
            gravel: reg.require("vanilla:gravel"),
            berry_bush: reg.require("vanilla:berry_bush"),
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
