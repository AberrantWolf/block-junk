//! Engine-side NPC kind + need registries.
//!
//! Mirrors the [`BlockRegistry`](crate::blocks::BlockRegistry) pattern: the
//! scripting host accumulates Lua-registered defs into a `LoadContext`,
//! the engine drains that buffer into the resources here, and brain /
//! spawn code reads from these resources at runtime.
//!
//! No slot interning yet — both kinds and needs are keyed by their full
//! string id. The planner-call path needs the string id anyway (it's what
//! the Lua side keys on), and brain ticks only do the lookup once per
//! goal transition rather than once per tick.
//!
//! Today's surface is intentionally thin: spawn lookups, need decay
//! lookups, and "kind X exists." Slot-interning + per-kind cached
//! `Vec<(need_slot, value)>` defaults will land when profiling shows
//! either matters.
//!
//! See also: [`NpcKindDef`], [`NeedDef`].
use std::collections::HashMap;

use bevy::prelude::*;
use block_junk_mod_api::npcs::{NeedDef, NpcKindDef};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NpcBootstrapError {
    #[error("duplicate npc kind id {0}")]
    DuplicateKind(String),
    #[error("duplicate need id {0}")]
    DuplicateNeed(String),
    #[error("npc kind {kind} references unregistered need {need}")]
    KindNeedsUnknownNeed { kind: String, need: String },
}

/// Registered NPC kinds, keyed by full string id. Spawned NPCs reference
/// this through the `NpcKind` component (which carries the same string).
#[derive(Resource, Default, Debug)]
pub struct NpcKindRegistry {
    defs: HashMap<String, NpcKindDef>,
}

impl NpcKindRegistry {
    /// Validate the pending kinds against the finished [`NeedRegistry`] and
    /// build the live resource. Kinds that reference a need that isn't
    /// registered are a load-time error — fail loudly per the project's
    /// "never silently degrade" rule.
    pub fn build(
        pending: Vec<NpcKindDef>,
        needs: &NeedRegistry,
    ) -> Result<Self, NpcBootstrapError> {
        let mut defs = HashMap::with_capacity(pending.len());
        for def in pending {
            for need_id in def.default_needs.keys() {
                if !needs.contains(need_id) {
                    return Err(NpcBootstrapError::KindNeedsUnknownNeed {
                        kind: def.id.0.clone(),
                        need: need_id.clone(),
                    });
                }
            }
            let key = def.id.0.clone();
            if defs.insert(key.clone(), def).is_some() {
                return Err(NpcBootstrapError::DuplicateKind(key));
            }
        }
        Ok(Self { defs })
    }

    pub fn get(&self, id: &str) -> Option<&NpcKindDef> {
        self.defs.get(id)
    }

    pub fn kind_count(&self) -> usize {
        self.defs.len()
    }
}

/// Registered needs, keyed by full string id. The brain tick reads decay
/// rates from here once per NPC per tick; the planner-call path includes
/// the full need map in the snapshot.
#[derive(Resource, Default, Debug)]
pub struct NeedRegistry {
    defs: HashMap<String, NeedDef>,
}

impl NeedRegistry {
    pub fn build(pending: Vec<NeedDef>) -> Result<Self, NpcBootstrapError> {
        let mut defs = HashMap::with_capacity(pending.len());
        for def in pending {
            let key = def.id.0.clone();
            if defs.insert(key.clone(), def).is_some() {
                return Err(NpcBootstrapError::DuplicateNeed(key));
            }
        }
        Ok(Self { defs })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.defs.contains_key(id)
    }

    /// Decay rate per second for `id`. Returns 0 (no decay) for unknown
    /// ids — the brain tolerates a stale need on an NPC whose registry
    /// entry was removed at mod-reload time without panicking.
    pub fn decay_per_sec(&self, id: &str) -> f32 {
        self.defs.get(id).map(|d| d.decay_per_sec).unwrap_or(0.0)
    }

    pub fn need_count(&self) -> usize {
        self.defs.len()
    }
}
