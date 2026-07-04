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
    #[error("kind {kind} stat {stat:?} is invalid: {problem}")]
    KindStatInvalid {
        kind: String,
        stat: String,
        problem: &'static str,
    },
}

/// Registered NPC kinds, keyed by full string id. Spawned NPCs reference
/// this through the `NpcKind` component (which carries the same string).
#[derive(Resource, Default, Debug)]
pub struct NpcKindRegistry {
    defs: HashMap<String, NpcKindDef>,
}

impl NpcKindRegistry {
    /// Sorted kind ids, for the connect-time mod-set manifest. Sorted
    /// because the backing map has no stable order and both sides must
    /// produce identical lists to compare.
    pub fn ids_sorted(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.defs.keys().cloned().collect();
        ids.sort();
        ids
    }

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
            // Stat declarations: fail loudly on malformed data — a bad
            // range would silently skew every roll of the kind.
            for (i, stat) in def.stats.iter().enumerate() {
                let problem = if stat.id.is_empty() {
                    Some("empty id")
                } else if !stat.min.is_finite() || !stat.max.is_finite() {
                    Some("non-finite range")
                } else if stat.min > stat.max {
                    Some("min > max")
                } else if def.stats[..i].iter().any(|s| s.id == stat.id) {
                    Some("duplicate id")
                } else {
                    None
                };
                if let Some(problem) = problem {
                    return Err(NpcBootstrapError::KindStatInvalid {
                        kind: def.id.0.clone(),
                        stat: stat.id.clone(),
                        problem,
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

    /// "Starting to care" cutoff used by Lua planners at `Goal::Idle`
    /// to decide whether the NPC should pivot from idle rhythm to the
    /// matching interactable. `None` ⇒ no threshold registered;
    /// callers fall back to their own default.
    /// (Forward wiring — the Idle-pivot planner surface hasn't landed.)
    #[allow(dead_code)]
    pub fn urge_threshold(&self, id: &str) -> Option<f32> {
        self.defs.get(id).and_then(|d| d.urge_threshold)
    }

    /// "Drop everything" cutoff used by the engine's preempt check
    /// every brain tick. `None` ⇒ this need never preempts.
    pub fn preempt_threshold(&self, id: &str) -> Option<f32> {
        self.defs.get(id).and_then(|d| d.preempt_threshold)
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

#[cfg(test)]
mod tests {
    use super::*;
    use block_junk_mod_api::npcs::{NeedDef, NeedId};

    fn need(id: &str, decay: f32, urge: Option<f32>, preempt: Option<f32>) -> NeedDef {
        NeedDef {
            id: NeedId::from(id),
            display_name: id.to_string(),
            decay_per_sec: decay,
            urge_threshold: urge,
            preempt_threshold: preempt,
        }
    }

    #[test]
    fn need_registry_returns_per_need_thresholds() {
        let registry = NeedRegistry::build(vec![
            need("hunger", 0.01, Some(0.3), Some(0.6)),
            need("work", 0.02, Some(0.3), None),
        ])
        .unwrap();

        assert_eq!(registry.urge_threshold("hunger"), Some(0.3));
        assert_eq!(registry.preempt_threshold("hunger"), Some(0.6));
        // `work` has urge but no preempt — wanting more work isn't an emergency.
        assert_eq!(registry.urge_threshold("work"), Some(0.3));
        assert_eq!(registry.preempt_threshold("work"), None);
    }

    #[test]
    fn need_registry_returns_none_for_unknown_or_unset() {
        let registry = NeedRegistry::build(vec![need("hunger", 0.01, None, None)]).unwrap();

        // Unset on a known need ⇒ None (callers fall back to a default).
        assert_eq!(registry.urge_threshold("hunger"), None);
        assert_eq!(registry.preempt_threshold("hunger"), None);
        // Unknown need ⇒ None — tolerates stale references the same way
        // `decay_per_sec` does.
        assert_eq!(registry.urge_threshold("missing"), None);
        assert_eq!(registry.preempt_threshold("missing"), None);
    }
}
