//! Tick-delayed, bounded delivery. Causality follows commands into generated events.
use crate::ModRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const MAX_PENDING_CALLBACKS: usize = 4096;
pub const CALLBACKS_PER_TICK: usize = 16;
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_QUEUED_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CHAIN_DEPTH: u16 = 32;
pub const MAX_CHAIN_WORK: usize = 4096;
pub const MAX_RETAINED_CAUSES: usize = 4096;
pub const MAX_CAUSAL_EDGES: usize = 65536;
pub use block_junk_mod_api::authority::EventCause;
#[derive(Clone, Debug, Serialize)]
pub struct BehaviorFailure {
    pub owner: String,
    pub behavior: String,
    pub cause: Option<EventCause>,
    pub reason: String,
}
struct Delivery {
    owner: String,
    key: String,
    value: serde_json::Value,
    cause: EventCause,
    ready_tick: u64,
    bytes: usize,
}
#[derive(Default)]
pub(crate) struct EventQueue {
    pinned: std::sync::Mutex<std::collections::BTreeSet<u64>>,
    pending: VecDeque<Delivery>,
    tick: u64,
    next_root: u64,
    queued_bytes: usize,
    chain_work: BTreeMap<u64, usize>,
    ancestors: BTreeMap<u64, BTreeSet<u64>>,
}
impl ModRegistry {
    pub fn validate_cause(&self, cause: Option<EventCause>) -> Result<(), String> {
        if cause.is_some_and(|cause| {
            cause.root == 0
                || cause.depth > MAX_CHAIN_DEPTH
                || !self.events.chain_work.contains_key(&cause.root)
        }) {
            return Err("event cause is unknown or retired".into());
        }
        Ok(())
    }
    /// Called only after the authoritative batch commits. Read-only validation
    /// happens before committing while the registry remains exclusively borrowed.
    pub fn pin_cause(&self, cause: EventCause) {
        if self.events.chain_work.contains_key(&cause.root) {
            self.events.pinned.lock().unwrap().insert(cause.root);
        }
    }
    pub fn retain_job_causes(&self, causes: impl Iterator<Item = EventCause>) {
        *self.events.pinned.lock().unwrap() = causes.map(|cause| cause.root).collect();
    }
    /// Queue all subscriptions atomically. Subscription overflow rejects the event
    /// explicitly; the engine never exposes a partially delivered event revision.
    pub fn enqueue_event<T: Serialize>(
        &mut self,
        event: &str,
        value: &T,
        parent: Option<EventCause>,
    ) -> Result<EventCause, String> {
        self.enqueue_joined_event(event, value, &parent.into_iter().collect::<Vec<_>>())
    }
    /// Coalesced authoritative changes retain all contributing causal budgets.
    /// A join receives a new root; every descendant charges its ancestry too.
    pub fn enqueue_joined_event<T: Serialize>(
        &mut self,
        event: &str,
        value: &T,
        parents: &[EventCause],
    ) -> Result<EventCause, String> {
        let value = serde_json::to_value(value).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len();
        if bytes > MAX_EVENT_BYTES {
            return Err("event payload limit exceeded".into());
        }
        let mut roots = BTreeSet::new();
        let mut depth = 0;
        for &parent in parents {
            self.validate_cause(Some(parent))?;
            if parent.depth >= MAX_CHAIN_DEPTH {
                return Err(format!(
                    "event chain {} exceeded depth {MAX_CHAIN_DEPTH}",
                    parent.root
                ));
            }
            roots.insert(parent.root);
            depth = depth.max(parent.depth + 1);
        }
        let new_root = roots.len() != 1;
        let root = if new_root {
            self.events
                .next_root
                .checked_add(1)
                .ok_or("event identity exhausted")?
        } else {
            *roots.first().unwrap()
        };
        let cause = EventCause { root, depth };
        let mut charged = roots.clone();
        for parent in roots {
            charged.extend(
                self.events
                    .ancestors
                    .get(&parent)
                    .into_iter()
                    .flatten()
                    .copied(),
            );
        }
        let edges: usize = self.events.ancestors.values().map(BTreeSet::len).sum();
        if new_root && edges + charged.len() > MAX_CAUSAL_EDGES {
            return Err("event causal ancestry limit exceeded".into());
        }
        let prefix = format!("subscription/{event}/");
        let subscriptions: Vec<_> = self
            .callback_owners
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, owner)| (key.clone(), owner.clone()))
            .collect();
        if self.events.pending.len() + subscriptions.len() > MAX_PENDING_CALLBACKS
            || self.events.queued_bytes + subscriptions.len() * bytes > MAX_QUEUED_EVENT_BYTES
        {
            return Err("event callback queue is full".into());
        }
        let cost = 1 + subscriptions.len();
        if cost > MAX_CHAIN_WORK {
            return Err("event subscription work exceeds chain budget".into());
        }
        for root in &charged {
            if self.events.chain_work[root] + cost > MAX_CHAIN_WORK {
                return Err(format!(
                    "event chain {root} exceeded work budget {MAX_CHAIN_WORK}"
                ));
            }
        }
        // Select a retired leaf before changing any queue or causal counter.
        let retired = if new_root && self.events.chain_work.len() == MAX_RETAINED_CAUSES {
            let pinned = self.events.pinned.lock().unwrap();
            let pending: BTreeSet<_> = self
                .events
                .pending
                .iter()
                .map(|delivery| delivery.cause.root)
                .chain(
                    self.commands
                        .batches
                        .lock()
                        .unwrap()
                        .iter()
                        .filter_map(|batch| batch.cause.map(|cause| cause.root)),
                )
                .collect();
            let ancestors: BTreeSet<_> =
                self.events.ancestors.values().flatten().copied().collect();
            Some(
                self.events
                    .chain_work
                    .keys()
                    .find(|root| {
                        !pinned.contains(root)
                            && !pending.contains(root)
                            && !charged.contains(root)
                            && !ancestors.contains(root)
                    })
                    .copied()
                    .ok_or("event cause capacity is occupied by pending actions")?,
            )
        } else {
            None
        };
        if let Some(retired) = retired {
            self.events.chain_work.remove(&retired);
            self.events.ancestors.remove(&retired);
        }
        if new_root {
            self.events.next_root = root;
            self.events.chain_work.insert(root, cost);
            if !charged.is_empty() {
                self.events.ancestors.insert(root, charged.clone());
            }
        }
        for root in charged {
            *self.events.chain_work.get_mut(&root).unwrap() += cost;
        }
        self.events.queued_bytes += subscriptions.len() * bytes;
        for (key, owner) in subscriptions {
            self.events.pending.push_back(Delivery {
                key,
                owner,
                value: value.clone(),
                cause,
                ready_tick: self.events.tick.saturating_add(1),
                bytes,
            });
        }
        Ok(cause)
    }
    pub fn pending_event_callbacks(&self) -> usize {
        self.events.pending.len()
    }
    pub fn advance_events(&mut self) {
        self.events.tick = self.events.tick.saturating_add(1);
        for _ in 0..CALLBACKS_PER_TICK {
            if self
                .events
                .pending
                .front()
                .is_none_or(|d| d.ready_tick > self.events.tick)
            {
                break;
            }
            let delivery = self.events.pending.pop_front().expect("checked front");
            self.events.queued_bytes -= delivery.bytes;
            let Some(owner) = self
                .mods
                .iter_mut()
                .find(|m| m.manifest.id == delivery.owner && !m.disabled)
            else {
                continue;
            };
            crate::commands::cause(&owner.lua, Some(delivery.cause));
            use mlua::LuaSerdeExt;
            let result = owner
                .lua
                .to_value_with(&delivery.value, crate::lua_event_options())
                .and_then(|value| crate::callbacks::call(&owner.lua, &delivery.key, value));
            crate::commands::cause(&owner.lua, None);
            if let Err(error) = result {
                owner.disabled = true;
                self.failures.push_back(BehaviorFailure {
                    owner: delivery.owner,
                    behavior: delivery.key,
                    cause: Some(delivery.cause),
                    reason: error.to_string(),
                });
            }
        }
    }
    pub fn take_behavior_failures(&mut self) -> Vec<BehaviorFailure> {
        self.failures.drain(..).collect()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedEvents {
    tick: u64,
    next_root: u64,
    chain_work: BTreeMap<u64, usize>,
    ancestors: BTreeMap<u64, BTreeSet<u64>>,
    pending: Vec<SavedDelivery>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedDelivery {
    owner: String,
    key: String,
    json: String,
    cause: EventCause,
    ready_tick: u64,
}
impl SavedEvents {
    pub fn validate(&self) -> Result<(), String> {
        if self.pending.len() > MAX_PENDING_CALLBACKS
            || self.chain_work.len() > MAX_RETAINED_CAUSES
            || self.chain_work.iter().any(|(root, work)| {
                *root == 0 || *root > self.next_root || *work == 0 || *work > MAX_CHAIN_WORK
            })
        {
            return Err("invalid saved event budget bounds".into());
        }
        if self.ancestors.values().map(BTreeSet::len).sum::<usize>() > MAX_CAUSAL_EDGES {
            return Err("saved causal ancestry limit exceeded".into());
        }
        for (root, ancestors) in &self.ancestors {
            if !self.chain_work.contains_key(root) || ancestors.is_empty() {
                return Err("invalid saved causal join".into());
            }
            for ancestor in ancestors {
                if ancestor >= root
                    || !self.chain_work.contains_key(ancestor)
                    || self
                        .ancestors
                        .get(ancestor)
                        .is_some_and(|parents| !parents.is_subset(ancestors))
                {
                    return Err("invalid saved causal ancestry".into());
                }
            }
        }
        let mut bytes = 0usize;
        for delivery in &self.pending {
            if delivery.owner.is_empty()
                || delivery.owner.len() > 128
                || delivery.key.is_empty()
                || delivery.key.len() > 512
                || delivery.json.len() > MAX_EVENT_BYTES
                || !self.contains(delivery.cause)
                || delivery.ready_tick > self.tick.saturating_add(1)
            {
                return Err("invalid saved event delivery".into());
            }
            let value: serde_json::Value =
                serde_json::from_str(&delivery.json).map_err(|e| e.to_string())?;
            crate::state::validate_value(&value)?;
            bytes = bytes
                .checked_add(delivery.json.len())
                .ok_or("saved event bytes overflow")?;
        }
        if bytes > MAX_QUEUED_EVENT_BYTES {
            return Err("saved event queue byte limit exceeded".into());
        }
        Ok(())
    }
    pub fn contains(&self, cause: EventCause) -> bool {
        cause.depth <= MAX_CHAIN_DEPTH && self.chain_work.contains_key(&cause.root)
    }
}
impl ModRegistry {
    pub fn saved_events(&self) -> SavedEvents {
        SavedEvents {
            tick: self.events.tick,
            next_root: self.events.next_root,
            chain_work: self.events.chain_work.clone(),
            ancestors: self.events.ancestors.clone(),
            pending: self
                .events
                .pending
                .iter()
                .map(|delivery| SavedDelivery {
                    owner: delivery.owner.clone(),
                    key: delivery.key.clone(),
                    json: serde_json::to_string(&delivery.value).expect("validated event payload"),
                    cause: delivery.cause,
                    ready_tick: delivery.ready_tick,
                })
                .collect(),
        }
    }
    pub fn restore_events(&mut self, saved: &SavedEvents) -> Result<(), String> {
        if self.runtime_started {
            return Err("restore events before runtime hooks".into());
        }
        saved.validate()?;
        let mut next = EventQueue {
            tick: saved.tick,
            next_root: saved.next_root,
            chain_work: saved.chain_work.clone(),
            ancestors: saved.ancestors.clone(),
            ..Default::default()
        };
        for delivery in &saved.pending {
            if !self
                .mods
                .iter()
                .any(|module| module.manifest.id == delivery.owner)
            {
                return Err(format!(
                    "saved event {} requires subscription owner {}; resolve queued events before replacing it",
                    delivery.key, delivery.owner
                ));
            }
            let value = serde_json::from_str(&delivery.json).map_err(|e| e.to_string())?;
            next.queued_bytes += delivery.json.len();
            next.pending.push_back(Delivery {
                owner: delivery.owner.clone(),
                key: delivery.key.clone(),
                value,
                cause: delivery.cause,
                ready_tick: delivery.ready_tick,
                bytes: delivery.json.len(),
            });
        }
        self.events = next;
        Ok(())
    }
    pub(crate) fn validate_restored_subscriptions(&self) -> Result<(), String> {
        let owners = self.callback_owners.lock().unwrap();
        for delivery in &self.events.pending {
            if owners.get(&delivery.key) != Some(&delivery.owner) {
                return Err(format!(
                    "saved event {} requires subscription owner {}; resolve queued events before replacing it",
                    delivery.key, delivery.owner
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> ModRegistry {
        ModRegistry {
            events: Default::default(),
            failures: Default::default(),
            commands: Default::default(),
            mod_state: Default::default(),
            state_schemas: Default::default(),
            source_fingerprint: 0,
            callback_owners: Default::default(),
            phase: Default::default(),
            runtime_started: false,
            side: block_junk_mod_api::Side::Server,
            mods: Vec::new(),
        }
    }
    #[test]
    fn joined_events_charge_every_parent_and_survive_restore() {
        let mut registry = registry();
        let a = registry.enqueue_event("space_event", &(), None).unwrap();
        let b = registry.enqueue_event("space_event", &(), None).unwrap();
        let joined = registry
            .enqueue_joined_event("space_event", &(), &[a, b])
            .unwrap();
        assert_ne!(joined.root, a.root);
        assert_ne!(joined.root, b.root);
        assert_eq!(joined.depth, 1);
        assert_eq!(registry.events.chain_work[&a.root], 2);
        assert_eq!(registry.events.chain_work[&b.root], 2);
        let saved = registry.saved_events();
        saved.validate().unwrap();
        registry.restore_events(&saved).unwrap();
        registry
            .enqueue_event("space_event", &(), Some(joined))
            .unwrap();
        assert_eq!(registry.events.chain_work[&a.root], 3);
        assert_eq!(registry.events.chain_work[&b.root], 3);
        registry.events.chain_work.insert(a.root, MAX_CHAIN_WORK);
        let before = serde_json::to_value(registry.saved_events()).unwrap();
        assert!(
            registry
                .enqueue_event("space_event", &(), Some(joined))
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(registry.saved_events()).unwrap(),
            before,
            "failed joined delivery must mutate no budgets or callbacks"
        );
    }
    #[test]
    fn malformed_ancestry_and_depth_cannot_reset_a_budget() {
        let mut registry = registry();
        let parent = registry.enqueue_event("space_event", &(), None).unwrap();
        assert!(
            registry
                .enqueue_joined_event(
                    "space_event",
                    &(),
                    &[EventCause {
                        depth: MAX_CHAIN_DEPTH,
                        ..parent
                    }]
                )
                .is_err()
        );
        let mut saved = registry.saved_events();
        saved
            .ancestors
            .insert(parent.root, BTreeSet::from([parent.root]));
        assert!(saved.validate().is_err());
        saved
            .ancestors
            .insert(parent.root, BTreeSet::from([u64::MAX]));
        assert!(saved.validate().is_err());
    }
}
