//! Durable, bounded mod-event delivery for coherent architectural revisions.
//! Geometry can wait for delivery capacity; it never drops a split or fabricates
//! a new causal budget after a delayed computation.
use crate::space_identity::{SpaceEvent, SpaceId, SpaceIdentities};
use bevy::prelude::*;
use block_junk_mod_api::{
    architecture::{SpaceChangeKind, SpaceLifecycleEvent},
    authority::EventCause,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
type Coord = [i32; 3];
const MAX_BATCHES: usize = 128;
const MAX_CAUSAL_EDGES: usize = 65536;
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Batch {
    causes: Vec<EventCause>,
    events: VecDeque<SpaceLifecycleEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryFailure {
    pub event: SpaceLifecycleEvent,
    pub reason: String,
}
#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpatialJournal {
    pub revision: u64,
    changed: BTreeMap<Coord, BTreeMap<u64, u16>>,
    pending: VecDeque<Batch>,
    pub failures: VecDeque<DeliveryFailure>,
    #[serde(skip)]
    pub demand: BTreeSet<Coord>,
}
pub struct Change {
    cause: Option<EventCause>,
    chunks: BTreeSet<Coord>,
}
impl SpatialJournal {
    pub fn set_demand(&mut self, demand: BTreeSet<Coord>) {
        self.changed
            .retain(|coord, _| demand.iter().any(|d| d[0] == coord[0] && d[2] == coord[2]));
        self.demand = demand;
    }
    pub fn prepare(
        &self,
        cells: impl IntoIterator<Item = IVec3>,
        cause: Option<EventCause>,
    ) -> Result<Change, String> {
        let mut chunks = BTreeSet::new();
        if let Some(cause) = cause {
            for cell in cells {
                let coord = crate::voxel::world_to_chunk(cell).0.0.to_array();
                // Roof probes can depend on edits above the demanded chunk.
                if self
                    .demand
                    .iter()
                    .any(|d| d[0] == coord[0] && d[2] == coord[2])
                {
                    chunks.insert(coord);
                }
            }
            let edges: usize = self.changed.values().map(BTreeMap::len).sum();
            let added = chunks
                .iter()
                .filter(|coord| {
                    !self
                        .changed
                        .get(*coord)
                        .is_some_and(|roots| roots.contains_key(&cause.root))
                })
                .count();
            if edges + added > MAX_CAUSAL_EDGES {
                return Err("spatial causal backlog is full; wait for pending geometry".into());
            }
        }
        Ok(Change { cause, chunks })
    }
    pub fn commit_change(&mut self, change: Change) {
        if let Some(cause) = change.cause {
            for coord in change.chunks {
                self.changed
                    .entry(coord)
                    .or_default()
                    .entry(cause.root)
                    .and_modify(|depth| *depth = (*depth).max(cause.depth))
                    .or_insert(cause.depth);
            }
        }
    }
    pub fn has_capacity(&self) -> bool {
        self.pending.len() < MAX_BATCHES
    }
    pub fn causes(&self) -> impl Iterator<Item = EventCause> + '_ {
        self.changed
            .values()
            .flat_map(|roots| {
                roots
                    .iter()
                    .map(|(&root, &depth)| EventCause { root, depth })
            })
            .chain(
                self.pending
                    .iter()
                    .flat_map(|batch| batch.causes.iter().copied()),
            )
    }
    pub fn publish(
        &mut self,
        domain: &BTreeSet<Coord>,
        events: &[SpaceEvent],
        identities: &SpaceIdentities,
    ) -> u64 {
        assert!(self.has_capacity());
        self.revision = self
            .revision
            .checked_add(1)
            .expect("spatial revision exhausted");
        let coords: Vec<_> = self
            .changed
            .keys()
            .filter(|coord| domain.iter().any(|d| d[0] == coord[0] && d[2] == coord[2]))
            .copied()
            .collect();
        let mut causes = BTreeMap::<u64, u16>::new();
        for coord in coords {
            for (root, depth) in self.changed.remove(&coord).unwrap() {
                causes
                    .entry(root)
                    .and_modify(|old| *old = (*old).max(depth))
                    .or_insert(depth);
            }
        }
        let events = events
            .iter()
            .map(|event| {
                let (kind, id) = match event {
                    SpaceEvent::Created(id) => (SpaceChangeKind::Created, *id),
                    SpaceEvent::Updated(id) => (SpaceChangeKind::Updated, *id),
                    SpaceEvent::Removed(id) => (SpaceChangeKind::Removed, *id),
                    SpaceEvent::Split { predecessor, .. } => (SpaceChangeKind::Split, *predecessor),
                    SpaceEvent::Merged { successor, .. } => (SpaceChangeKind::Merged, *successor),
                };
                let record = &identities.records[&id];
                SpaceLifecycleEvent {
                    kind,
                    spatial_revision: self.revision,
                    space_id: id.0,
                    space_revision: record.revision,
                    predecessors: record.predecessors.len() as u64,
                    successors: record.successors.len() as u64,
                }
            })
            .collect::<VecDeque<_>>();
        if !events.is_empty() {
            self.pending.push_back(Batch {
                causes: causes
                    .into_iter()
                    .map(|(root, depth)| EventCause { root, depth })
                    .collect(),
                events,
            });
        }
        self.revision
    }
    pub fn validate(
        &self,
        events: &block_junk_scripting::events::SavedEvents,
        identities: &SpaceIdentities,
    ) -> Result<(), String> {
        if self.pending.len() > MAX_BATCHES
            || self.failures.len() > 128
            || self.changed.values().map(BTreeMap::len).sum::<usize>() > MAX_CAUSAL_EDGES
            || self
                .changed
                .keys()
                .flatten()
                .any(|n| n.unsigned_abs() > 31250)
        {
            return Err("invalid spatial event backlog bounds".into());
        }
        if self.causes().any(|cause| !events.contains(cause)) {
            return Err("spatial event references an unavailable causal budget".into());
        }
        let mut previous_revision = 0;
        for batch in &self.pending {
            if batch.events.is_empty()
                || batch.causes.len() > block_junk_scripting::events::MAX_RETAINED_CAUSES
                || batch.events.len() > identities.records.len().saturating_mul(2)
                || batch
                    .causes
                    .windows(2)
                    .any(|pair| pair[0].root >= pair[1].root)
            {
                return Err("invalid spatial event batch".into());
            }
            let revision = batch.events[0].spatial_revision;
            if revision <= previous_revision {
                return Err("spatial event batches are not ordered".into());
            }
            previous_revision = revision;
            for event in &batch.events {
                if event.spatial_revision == 0
                    || event.spatial_revision != revision
                    || event.spatial_revision > self.revision
                    || !identities
                        .records
                        .get(&SpaceId(event.space_id))
                        .is_some_and(|record| {
                            event.space_revision > 0
                                && event.space_revision <= record.revision
                                && event.predecessors <= record.predecessors.len() as u64
                                && event.successors <= record.successors.len() as u64
                        })
                {
                    return Err("spatial event references an invalid revision or identity".into());
                }
            }
        }
        if self.failures.iter().any(|failure| {
            failure.reason.len() > 4096
                || failure.event.spatial_revision == 0
                || failure.event.spatial_revision > self.revision
                || !identities
                    .records
                    .contains_key(&SpaceId(failure.event.space_id))
        }) {
            return Err("invalid spatial event delivery failure".into());
        }
        Ok(())
    }
}
pub fn dispatch(
    mut journal: ResMut<SpatialJournal>,
    mut mods: ResMut<crate::scripting::ServerMods>,
) {
    for _ in 0..16 {
        let Some(batch) = journal.pending.front() else {
            break;
        };
        let event = batch.events.front().unwrap();
        match mods
            .0
            .enqueue_joined_event("space_event", event, &batch.causes)
        {
            Ok(_) => {}
            Err(reason) if reason == "event callback queue is full" => break,
            Err(mut reason) => {
                let event = event.clone();
                warn!(%reason,space=event.space_id,"spatial mod event rejected");
                if reason.len() > 4096 {
                    let mut end = 4096;
                    while !reason.is_char_boundary(end) {
                        end -= 1;
                    }
                    reason.truncate(end);
                }
                if journal.failures.len() == 128 {
                    journal.failures.pop_front();
                }
                journal
                    .failures
                    .push_back(DeliveryFailure { event, reason });
            }
        }
        let batch = journal.pending.front_mut().unwrap();
        batch.events.pop_front();
        if batch.events.is_empty() {
            journal.pending.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space_identity::{Membership, SpaceDraft};
    use block_junk_scripting::{LoadContext, ModRegistry};
    fn draft(cells: &[[i32; 3]], content: u8) -> SpaceDraft {
        SpaceDraft {
            membership: Membership::from_cells(cells.iter().copied()).unwrap(),
            content_fingerprint: [content; 32],
        }
    }
    #[test]
    fn durable_space_events_deliver_once_after_restore_with_joined_causes() {
        let root = tempfile::tempdir().unwrap();
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/mods/independent/foundation");
        std::os::unix::fs::symlink(base, root.path().join("foundation")).unwrap();
        let observer = root.path().join("observer");
        std::fs::create_dir(&observer).unwrap();
        std::fs::write(observer.join("manifest.toml"),"id='observer'\nname='Observer'\nversion='1.0.0'\napi_version='0.6.0'\n[dependencies]\nfoundation='^1.0.0'\n").unwrap();
        std::fs::write(observer.join("events.lua"),"engine.on_space_event(function(event) engine.commands.push{kind='read_space',space_id=event.space_id} end)").unwrap();
        let mut mods = ModRegistry::load_dir(
            block_junk_mod_api::Side::Server,
            root.path(),
            &LoadContext::new(),
        )
        .unwrap();
        let a = mods.enqueue_event("block_placed", &(), None).unwrap();
        let b = mods.enqueue_event("block_placed", &(), None).unwrap();
        let mut journal = SpatialJournal::default();
        journal.set_demand(BTreeSet::from([[0, 0, 0]]));
        for cause in [a, b] {
            let change = journal.prepare([IVec3::ONE], Some(cause)).unwrap();
            journal.commit_change(change);
        }
        let mut identities = SpaceIdentities::default();
        let (ids, events) = identities
            .reconcile(&BTreeSet::new(), &[draft(&[[1, 1, 1], [2, 1, 1]], 1)])
            .unwrap();
        journal.publish(&BTreeSet::from([[0, 0, 0]]), &events, &identities);
        let saved_events = mods.saved_events();
        journal.validate(&saved_events, &identities).unwrap();
        let bytes = bincode::serde::encode_to_vec(&journal, bincode::config::standard()).unwrap();
        let (journal, _): (SpatialJournal, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        let mut restored = ModRegistry::load_composed(
            block_junk_mod_api::Side::Server,
            root.path(),
            &LoadContext::new(),
            Default::default(),
        )
        .unwrap();
        restored.restore_events(&saved_events).unwrap();
        restored.activate_runtime().unwrap();
        let mut app = App::new();
        app.insert_resource(journal)
            .insert_resource(crate::scripting::ServerMods(restored))
            .add_systems(Update, dispatch);
        app.update();
        assert!(app.world().resource::<SpatialJournal>().pending.is_empty());
        app.world_mut()
            .resource_mut::<crate::scripting::ServerMods>()
            .0
            .advance_events();
        {
            let mods = &app.world().resource::<crate::scripting::ServerMods>().0;
            let batches = mods.commands.batches.lock().unwrap();
            assert_eq!(batches.len(), 1);
            assert!(
                matches!(&batches[0].commands[0],block_junk_scripting::commands::Command::ReadSpace{space_id} if *space_id==ids[0].0)
            );
            assert_eq!(batches[0].cause.unwrap().depth, 1);
        }
        app.update();
        app.world_mut()
            .resource_mut::<crate::scripting::ServerMods>()
            .0
            .advance_events();
        assert_eq!(
            app.world()
                .resource::<crate::scripting::ServerMods>()
                .0
                .commands
                .batches
                .lock()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn delivery_backpressure_retains_whole_revision_batches() {
        let mut journal = SpatialJournal::default();
        let mut identities = SpaceIdentities::default();
        let (ids, events) = identities
            .reconcile(&BTreeSet::new(), &[draft(&[[1, 1, 1]], 1)])
            .unwrap();
        for _ in 0..MAX_BATCHES {
            journal.publish(&BTreeSet::new(), &events, &identities);
        }
        assert!(!journal.has_capacity());
        assert_eq!(journal.pending.len(), MAX_BATCHES);
        assert!(
            journal
                .pending
                .iter()
                .all(|batch| batch.events[0].space_id == ids[0].0)
        );
    }

    #[test]
    fn malformed_saved_batches_reject_without_changing_the_journal() {
        let mut identities = SpaceIdentities::default();
        let (_, events) = identities
            .reconcile(&BTreeSet::new(), &[draft(&[[1, 1, 1]], 1)])
            .unwrap();
        let mut journal = SpatialJournal::default();
        journal.publish(&BTreeSet::new(), &events, &identities);
        journal.publish(&BTreeSet::new(), &events, &identities);
        let budgets = Default::default();
        journal.validate(&budgets, &identities).unwrap();
        let mut unordered = journal.clone();
        unordered.pending.swap(0, 1);
        assert!(unordered.validate(&budgets, &identities).is_err());
        let mut invented = journal.clone();
        invented.pending[0].events[0].successors = 1;
        assert!(invented.validate(&budgets, &identities).is_err());
        let mut repeated = journal.clone();
        repeated.pending[0]
            .events
            .extend(vec![journal.pending[0].events[0].clone(); 2]);
        assert!(repeated.validate(&budgets, &identities).is_err());
        journal.validate(&budgets, &identities).unwrap();
    }
}
