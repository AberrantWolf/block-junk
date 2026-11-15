//! Durable architectural identity and explicit split/merge lineage. Derived
//! topology is disposable; retired records remain addressable by their owner.
use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpaceId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorRun {
    pub start: [i32; 3],
    pub len: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership(pub Vec<FloorRun>);
impl Membership {
    pub fn from_cells(cells: impl IntoIterator<Item = [i32; 3]>) -> Result<Self, String> {
        let sorted: BTreeSet<_> = cells.into_iter().map(|[x, y, z]| (y, z, x)).collect();
        let mut runs: Vec<FloorRun> = Vec::new();
        for (y, z, x) in sorted {
            if let Some(last) = runs.last_mut()
                && last.start[1] == y
                && last.start[2] == z
                && i64::from(last.start[0]) + i64::from(last.len) == i64::from(x)
            {
                last.len += 1;
                continue;
            }
            runs.push(FloorRun {
                start: [x, y, z],
                len: 1,
            });
        }
        let result = Self(runs);
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), String> {
        let mut previous: Option<&FloorRun> = None;
        if self.0.is_empty() {
            return Err("architectural membership cannot be empty".into());
        }
        for run in &self.0 {
            if run.len == 0
                || run.start.iter().any(|n| n.unsigned_abs() > 1_000_000)
                || i64::from(run.start[0]) + i64::from(run.len) - 1 > 1_000_000
            {
                return Err("invalid architectural membership run".into());
            }
            if let Some(old) = previous {
                let a = (old.start[1], old.start[2]);
                let b = (run.start[1], run.start[2]);
                if a > b
                    || (a == b
                        && i64::from(old.start[0]) + i64::from(old.len) >= i64::from(run.start[0]))
                {
                    return Err("membership runs must be sorted, disjoint and coalesced".into());
                }
            }
            previous = Some(run);
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        for run in &self.0 {
            for n in run.start {
                hash.update(&n.to_le_bytes());
            }
            hash.update(&run.len.to_le_bytes());
        }
        *hash.finalize().as_bytes()
    }
    pub fn intersects(&self, other: &Self) -> bool {
        let (mut a, mut b) = (0, 0);
        while a < self.0.len() && b < other.0.len() {
            let left = &self.0[a];
            let right = &other.0[b];
            let lc = (left.start[1], left.start[2]);
            let rc = (right.start[1], right.start[2]);
            if lc < rc {
                a += 1;
                continue;
            }
            if rc < lc {
                b += 1;
                continue;
            }
            if i64::from(left.start[0]) + i64::from(left.len) <= i64::from(right.start[0]) {
                a += 1;
            } else if i64::from(right.start[0]) + i64::from(right.len) <= i64::from(left.start[0]) {
                b += 1;
            } else {
                return true;
            }
        }
        false
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRecord {
    pub id: SpaceId,
    pub revision: u64,
    pub active: bool,
    pub membership: Membership,
    pub membership_fingerprint: [u8; 32],
    pub content_fingerprint: [u8; 32],
    pub predecessors: BTreeSet<SpaceId>,
    pub successors: BTreeSet<SpaceId>,
    pub resolved_state_owners: BTreeSet<String>,
}
#[derive(Clone, Debug)]
pub struct SpaceDraft {
    pub membership: Membership,
    /// Canonical geometry, measurements, connections and contributing facilities.
    pub content_fingerprint: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpaceEvent {
    Created(SpaceId),
    Updated(SpaceId),
    Removed(SpaceId),
    Split {
        predecessor: SpaceId,
        successors: BTreeSet<SpaceId>,
    },
    Merged {
        predecessors: BTreeSet<SpaceId>,
        successor: SpaceId,
    },
}
#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct SpaceIdentities {
    pub next: u64,
    pub revision: u64,
    pub records: BTreeMap<SpaceId, SpaceRecord>,
}
impl Default for SpaceIdentities {
    fn default() -> Self {
        Self {
            next: 1,
            revision: 0,
            records: BTreeMap::new(),
        }
    }
}
impl SpaceIdentities {
    pub fn validate(&self) -> Result<(), String> {
        if self.next == 0 {
            return Err("space identity allocator exhausted".into());
        }
        let mut occupied = BTreeMap::<(i32, i32), Vec<(i32, i64)>>::new();
        for (id, record) in &self.records {
            record.membership.validate()?;
            if id.0 == 0
                || id.0 >= self.next
                || record.id != *id
                || record.revision == 0
                || record.membership_fingerprint != record.membership.fingerprint()
                || (record.active
                    && (!record.successors.is_empty() || !record.resolved_state_owners.is_empty()))
                || record
                    .resolved_state_owners
                    .iter()
                    .any(|owner| owner.is_empty() || owner.len() > 128)
            {
                return Err("invalid durable space identity record".into());
            }
            if record.active {
                for run in &record.membership.0 {
                    occupied
                        .entry((run.start[1], run.start[2]))
                        .or_default()
                        .push((run.start[0], run.start[0] as i64 + run.len as i64));
                }
            }
            for predecessor in &record.predecessors {
                if predecessor >= id
                    || !self
                        .records
                        .get(predecessor)
                        .is_some_and(|p| !p.active && p.successors.contains(id))
                {
                    return Err("invalid predecessor lineage".into());
                }
            }
            for successor in &record.successors {
                if successor <= id
                    || !self
                        .records
                        .get(successor)
                        .is_some_and(|s| s.predecessors.contains(id))
                {
                    return Err("invalid successor lineage".into());
                }
            }
        }
        for runs in occupied.values_mut() {
            runs.sort_unstable();
            if runs.windows(2).any(|pair| pair[0].1 > pair[1].0 as i64) {
                return Err("active space identities overlap".into());
            }
        }
        Ok(())
    }
    /// `scope` contains exactly the old identities covered by a coherent region
    /// rebuild. Unsubscribing is never a removal and must not call this method.
    pub fn reconcile(
        &mut self,
        scope: &BTreeSet<SpaceId>,
        drafts: &[SpaceDraft],
    ) -> Result<(Vec<SpaceId>, Vec<SpaceEvent>), String> {
        self.validate()?;
        for draft in drafts {
            draft.membership.validate()?;
        }
        for (i, draft) in drafts.iter().enumerate() {
            if drafts[..i]
                .iter()
                .any(|other| draft.membership.intersects(&other.membership))
            {
                return Err("new architectural memberships overlap".into());
            }
        }
        for id in scope {
            if !self.records.get(id).is_some_and(|record| record.active) {
                return Err("rebuild scope has missing or retired identity".into());
            }
        }
        for record in self
            .records
            .values()
            .filter(|record| record.active && !scope.contains(&record.id))
        {
            if drafts
                .iter()
                .any(|draft| draft.membership.intersects(&record.membership))
            {
                return Err("rebuild omitted an overlapping active identity".into());
            }
        }
        let mut staged = self.clone();
        let mut old_to_new = BTreeMap::<SpaceId, BTreeSet<usize>>::new();
        let mut new_to_old = vec![BTreeSet::<SpaceId>::new(); drafts.len()];
        for id in scope {
            for (index, draft) in drafts.iter().enumerate() {
                if self.records[id].membership.intersects(&draft.membership) {
                    old_to_new.entry(*id).or_default().insert(index);
                    new_to_old[index].insert(*id);
                }
            }
        }
        let mut assignments = vec![SpaceId(0); drafts.len()];
        let mut events = Vec::new();
        let mut visited = BTreeSet::new();
        let mut order: Vec<_> = (0..drafts.len()).collect();
        order.sort_by_key(|index| drafts[*index].membership.fingerprint());
        for seed in order {
            if !visited.insert(seed) {
                continue;
            }
            let mut group = BTreeSet::from([seed]);
            let mut previous = BTreeSet::new();
            let mut queue = VecDeque::from([seed]);
            while let Some(index) = queue.pop_front() {
                for id in &new_to_old[index] {
                    if previous.insert(*id) {
                        for &next in &old_to_new[id] {
                            if visited.insert(next) {
                                group.insert(next);
                                queue.push_back(next);
                            }
                        }
                    }
                }
            }
            if group.len() == 1 && previous.len() == 1 {
                let id = *previous.first().unwrap();
                let record = staged.records.get_mut(&id).unwrap();
                let draft = &drafts[seed];
                if record.membership != draft.membership
                    || record.content_fingerprint != draft.content_fingerprint
                {
                    record.revision = record
                        .revision
                        .checked_add(1)
                        .ok_or("space revision exhausted")?;
                    record.membership = draft.membership.clone();
                    record.membership_fingerprint = draft.membership.fingerprint();
                    record.content_fingerprint = draft.content_fingerprint;
                    events.push(SpaceEvent::Updated(id));
                }
                assignments[seed] = id;
                continue;
            }
            let mut order: Vec<_> = group.iter().copied().collect();
            order.sort_by_key(|index| drafts[*index].membership.fingerprint());
            for index in order {
                let id = SpaceId(staged.next);
                staged.next = staged
                    .next
                    .checked_add(1)
                    .ok_or("space identity allocator exhausted")?;
                let draft = &drafts[index];
                staged.records.insert(
                    id,
                    SpaceRecord {
                        id,
                        revision: 1,
                        active: true,
                        membership: draft.membership.clone(),
                        membership_fingerprint: draft.membership.fingerprint(),
                        content_fingerprint: draft.content_fingerprint,
                        predecessors: new_to_old[index].clone(),
                        successors: BTreeSet::new(),
                        resolved_state_owners: BTreeSet::new(),
                    },
                );
                assignments[index] = id;
                if new_to_old[index].len() > 1 {
                    events.push(SpaceEvent::Merged {
                        predecessors: new_to_old[index].clone(),
                        successor: id,
                    });
                } else if previous.is_empty() {
                    events.push(SpaceEvent::Created(id));
                }
            }
            for &id in &previous {
                let record = staged.records.get_mut(&id).unwrap();
                record.active = false;
                record.revision = record
                    .revision
                    .checked_add(1)
                    .ok_or("space revision exhausted")?;
                record.successors = old_to_new[&id]
                    .iter()
                    .map(|index| assignments[*index])
                    .collect();
                if record.successors.len() > 1 {
                    events.push(SpaceEvent::Split {
                        predecessor: id,
                        successors: record.successors.clone(),
                    });
                }
            }
        }
        for id in scope.iter().filter(|id| !old_to_new.contains_key(id)) {
            let record = staged.records.get_mut(id).unwrap();
            record.active = false;
            record.revision = record
                .revision
                .checked_add(1)
                .ok_or("space revision exhausted")?;
            events.push(SpaceEvent::Removed(*id));
        }
        if !events.is_empty() {
            staged.revision = staged
                .revision
                .checked_add(1)
                .ok_or("spatial revision exhausted")?;
        }
        staged.validate()?;
        *self = staged;
        Ok((assignments, events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft(cells: &[[i32; 3]], content: u8) -> SpaceDraft {
        SpaceDraft {
            membership: Membership::from_cells(cells.iter().copied()).unwrap(),
            content_fingerprint: [content; 32],
        }
    }
    #[test]
    fn compact_membership_excludes_holes_and_rejects_noncanonical_runs() {
        let shape = Membership::from_cells([[0, 0, 0], [1, 0, 0], [3, 0, 0], [0, 1, 0]]).unwrap();
        assert_eq!(shape.0.len(), 3);
        assert!(!shape.intersects(&Membership::from_cells([[2, 0, 0]]).unwrap()));
        assert!(shape.intersects(&Membership::from_cells([[3, 0, 0]]).unwrap()));
        assert!(
            Membership(vec![
                FloorRun {
                    start: [0, 0, 0],
                    len: 1
                },
                FloorRun {
                    start: [1, 0, 0],
                    len: 1
                }
            ])
            .validate()
            .is_err()
        );
    }
    #[test]
    fn updates_preserve_identity_splits_and_merges_retain_retired_lineage() {
        let mut ledger = SpaceIdentities::default();
        let initial = draft(&[[0, 0, 0], [1, 0, 0], [2, 0, 0]], 1);
        let (ids, events) = ledger
            .reconcile(&BTreeSet::new(), std::slice::from_ref(&initial))
            .unwrap();
        let first = ids[0];
        assert_eq!(events, vec![SpaceEvent::Created(first)]);
        assert!(
            ledger
                .reconcile(&BTreeSet::from([first]), std::slice::from_ref(&initial))
                .unwrap()
                .1
                .is_empty()
        );
        let mut expanded = draft(&[[0, 0, 0], [1, 0, 0], [2, 0, 0], [3, 0, 0]], 1);
        assert_eq!(
            ledger
                .reconcile(&BTreeSet::from([first]), std::slice::from_ref(&expanded))
                .unwrap()
                .1,
            vec![SpaceEvent::Updated(first)]
        );
        expanded.content_fingerprint = [2; 32];
        assert_eq!(
            ledger
                .reconcile(&BTreeSet::from([first]), &[expanded])
                .unwrap()
                .1,
            vec![SpaceEvent::Updated(first)]
        );
        let children = [draft(&[[0, 0, 0]], 1), draft(&[[2, 0, 0], [3, 0, 0]], 1)];
        let (ids, events) = ledger
            .reconcile(&BTreeSet::from([first]), &children)
            .unwrap();
        let child_ids: BTreeSet<_> = ids.into_iter().collect();
        assert_eq!(
            events,
            vec![SpaceEvent::Split {
                predecessor: first,
                successors: child_ids.clone()
            }]
        );
        assert!(!ledger.records[&first].active);
        let retired = ledger.records[&first].clone();
        let (ids, events) = ledger.reconcile(&child_ids, &[initial]).unwrap();
        assert_eq!(
            events,
            vec![SpaceEvent::Merged {
                predecessors: child_ids,
                successor: ids[0]
            }]
        );
        assert_eq!(ledger.records[&first], retired);
        let bytes = bincode::serde::encode_to_vec(&ledger, bincode::config::standard()).unwrap();
        let (loaded, _): (SpaceIdentities, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.records, ledger.records);
        let before = ledger.records.clone();
        assert!(ledger.reconcile(&BTreeSet::new(), &children).is_err());
        assert_eq!(ledger.records, before);
    }
    #[test]
    fn creation_order_does_not_change_ids_and_inactive_demand_does_not_remove_spaces() {
        let a = draft(&[[0, 0, 0]], 1);
        let b = draft(&[[10, 0, 0]], 1);
        let mut first = SpaceIdentities::default();
        let mut second = SpaceIdentities::default();
        first
            .reconcile(&BTreeSet::new(), &[a.clone(), b.clone()])
            .unwrap();
        second.reconcile(&BTreeSet::new(), &[b, a]).unwrap();
        assert_eq!(first.records, second.records);
        let records = first.records.clone();
        assert!(first.reconcile(&BTreeSet::new(), &[]).unwrap().1.is_empty());
        assert_eq!(first.records, records);
    }
}
