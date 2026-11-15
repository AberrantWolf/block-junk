//! Common reservation semantics for engine services. A multi-target acquisition
//! is atomic; a stale release cannot revoke a later acquisition by the same owner.
use block_junk_mod_api::authority::ReservationOwner;
use std::{collections::HashMap, hash::Hash};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub generation: u64,
    pub owner: ReservationOwner,
}
#[derive(Clone, Debug)]
pub struct ReservationTable<K> {
    next: u64,
    holders: HashMap<K, Lease>,
}
impl<K> Default for ReservationTable<K> {
    fn default() -> Self {
        Self {
            next: 1,
            holders: HashMap::new(),
        }
    }
}
impl<K: Eq + Hash + Clone> ReservationTable<K> {
    pub fn acquire_all(&mut self, keys: &[K], owner: ReservationOwner) -> Option<Vec<Lease>> {
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        if unique.len() != keys.len()
            || keys
                .iter()
                .any(|key| self.holder(key).is_some_and(|lease| lease.owner != owner))
        {
            return None;
        }
        let needed = keys
            .iter()
            .filter(|key| !self.holders.contains_key(*key))
            .count() as u64;
        self.next.checked_add(needed)?;
        Some(
            keys.iter()
                .map(|key| {
                    self.acquire(key.clone(), owner.clone())
                        .expect("validated atomic reservation set")
                })
                .collect(),
        )
    }
    pub fn holder(&self, key: &K) -> Option<&Lease> {
        self.holders.get(key)
    }
    pub fn acquire(&mut self, key: K, owner: ReservationOwner) -> Option<Lease> {
        if let Some(lease) = self.holders.get(&key) {
            return (lease.owner == owner).then(|| lease.clone());
        }
        let next = self.next.checked_add(1)?;
        let lease = Lease {
            generation: self.next,
            owner,
        };
        self.next = next;
        self.holders.insert(key, lease.clone());
        Some(lease)
    }
    pub fn release(&mut self, key: &K, lease: &Lease) -> bool {
        if self.holders.get(key) != Some(lease) {
            return false;
        }
        self.holders.remove(key);
        true
    }
    pub fn revoke(&mut self, key: &K) {
        self.holders.remove(key);
    }
    pub fn release_owner(&mut self, owner: &ReservationOwner) {
        self.holders.retain(|_, lease| &lease.owner != owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_and_foreign_releases_preserve_later_ownership() {
        let owner = ReservationOwner::Npc { id: 1 };
        let mut table = ReservationTable::default();
        let old = table.acquire(42, owner.clone()).unwrap();
        assert_eq!(table.acquire(42, owner.clone()), Some(old.clone()));
        assert!(
            table
                .acquire(42, ReservationOwner::Player { id: 1 })
                .is_none()
        );
        assert!(table.release(&42, &old));
        let current = table.acquire(42, owner.clone()).unwrap();
        assert_ne!(current.generation, old.generation);
        assert!(!table.release(&42, &old));
        table.release_owner(&ReservationOwner::Player { id: 1 });
        assert_eq!(table.holder(&42), Some(&current));
        table.release_owner(&owner);
        assert!(table.holder(&42).is_none());
    }
    #[test]
    fn contention_and_exhaustion_do_not_partially_reserve_a_set() {
        let mut table = ReservationTable::default();
        let owner = ReservationOwner::Behavior {
            mod_id: "test".into(),
            registration_id: "planner".into(),
        };
        table.acquire(2, ReservationOwner::Npc { id: 1 }).unwrap();
        assert!(table.acquire_all(&[1, 2], owner.clone()).is_none());
        assert!(table.holder(&1).is_none());
        table.next = u64::MAX - 1;
        assert!(table.acquire_all(&[3, 4], owner.clone()).is_none());
        assert!(table.holder(&3).is_none());
        assert!(table.acquire_all(&[3, 3], owner).is_none());
    }
}
