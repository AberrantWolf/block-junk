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
use block_junk_mod_api::animations::AnimationDef;
use block_junk_mod_api::npcs::{NeedDef, NpcKindDef, WorkDefaults};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NpcBootstrapError {
    #[error("duplicate npc kind id {0}")]
    DuplicateKind(String),
    #[error("duplicate need id {0}")]
    DuplicateNeed(String),
    #[error("npc kind {kind} references unregistered need {need}")]
    KindNeedsUnknownNeed { kind: String, need: String },
    #[error("duplicate animation id {0}")]
    DuplicateAnimation(String),
    #[error("npc kind {kind} animations.{slot} references unregistered animation {anim}")]
    KindAnimationUnknown {
        kind: String,
        slot: &'static str,
        anim: String,
    },
    #[error("work defaults reference unregistered need {0}")]
    WorkDefaultsUnknownNeed(String),
    #[error("work defaults duration_secs must be > 0, got {0}")]
    WorkDefaultsBadDuration(f32),
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
        animations: &AnimationRegistry,
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
            for (slot, anim) in [
                ("idle", &def.animations.idle),
                ("walk", &def.animations.walk),
                ("work", &def.animations.work),
            ] {
                if !animations.contains(anim) {
                    return Err(NpcBootstrapError::KindAnimationUnknown {
                        kind: def.id.0.clone(),
                        slot,
                        anim: anim.clone(),
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

/// Engine-side fallback for the work-action pipeline. Built from the
/// optional `engine.npcs.set_work_defaults` call in a mod's `data.lua`,
/// or [`WorkDefaults::default`] (no need restore, 4s duration) when
/// no mod sets one. Read by the brain at WorkPlan goal commit when
/// the targeted block's own `work_action` is `None`, and by the
/// snapshot builder so planners can score plans by payoff.
#[derive(Resource, Debug, Default)]
pub struct WorkDefaultsRes(pub WorkDefaults);

impl WorkDefaultsRes {
    /// Cross-validate the (optionally Lua-supplied) defaults against
    /// the finished [`NeedRegistry`] and wrap into the resource type.
    /// Same "loud at boot" pattern as the other registries — a
    /// reference to an unregistered need fails the load.
    pub fn build(
        pending: Option<WorkDefaults>,
        needs: &NeedRegistry,
    ) -> Result<Self, NpcBootstrapError> {
        let defaults = pending.unwrap_or_default();
        if defaults.duration_secs <= 0.0 {
            return Err(NpcBootstrapError::WorkDefaultsBadDuration(
                defaults.duration_secs,
            ));
        }
        if let Some(nr) = &defaults.need_restore
            && !needs.contains(&nr.need)
        {
            return Err(NpcBootstrapError::WorkDefaultsUnknownNeed(nr.need.clone()));
        }
        Ok(Self(defaults))
    }
}

/// Registered animation clips, keyed by id. Both sides build identical
/// copies — the client uses them to load assets + build the unified
/// [`bevy::animation::AnimationGraph`]; the server uses them only to
/// validate references from [`NpcKindDef::animations`] +
/// [`UseSlot.animation`].
#[derive(Resource, Default, Debug)]
pub struct AnimationRegistry {
    defs: HashMap<String, AnimationDef>,
    /// Insertion order, so the client can build its AnimationGraph
    /// deterministically (slot index matches order).
    order: Vec<String>,
}

impl AnimationRegistry {
    pub fn build(pending: Vec<AnimationDef>) -> Result<Self, NpcBootstrapError> {
        let mut defs = HashMap::with_capacity(pending.len());
        let mut order = Vec::with_capacity(pending.len());
        for def in pending {
            let key = def.id.0.clone();
            order.push(key.clone());
            if defs.insert(key.clone(), def).is_some() {
                return Err(NpcBootstrapError::DuplicateAnimation(key));
            }
        }
        Ok(Self { defs, order })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.defs.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&AnimationDef> {
        self.defs.get(id)
    }

    /// Registered ids in insertion order. The client's AnimationGraph
    /// build follows this so the cached id → AnimationNodeIndex map is
    /// deterministic across runs and across both sides.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &AnimationDef)> + '_ {
        self.order
            .iter()
            .map(move |id| (id, self.defs.get(id).expect("order entry must resolve")))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }
}
