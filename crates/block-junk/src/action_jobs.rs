//! Durable action outcomes. Transient reservations may be rebuilt, but an
//! accepted action and its result do not disappear when a callback or link fails.
use block_junk_mod_api::authority::{JobRecord, JobState, JobTarget, ReservationOwner};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_RETAINED_JOBS: usize = 4096;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobLedger {
    pub next: u64,
    #[serde(with = "saved_records")]
    pub records: BTreeMap<u64, JobRecord>,
}
impl Default for JobLedger {
    fn default() -> Self {
        Self {
            next: 1,
            records: BTreeMap::new(),
        }
    }
}
impl JobLedger {
    pub fn owned_jobs(&self, owner: &str, after: u64, limit: usize) -> (Vec<JobRecord>, bool) {
        let mut records=self.records.range((std::ops::Bound::Excluded(after),std::ops::Bound::Unbounded)).map(|(_,job)|job).filter(|job|matches!(&job.controller,ReservationOwner::Behavior {mod_id,..} if mod_id==owner));
        let page = records.by_ref().take(limit).cloned().collect();
        (page, records.next().is_some())
    }
    pub fn start(
        &mut self,
        actor: ReservationOwner,
        controller: ReservationOwner,
        target: JobTarget,
    ) -> Result<u64, String> {
        if let Some(job) = self.records.values().find(|job| {
            !job.state.terminal() && (job.actor == actor || job.target.conflicts_with(&target))
        }) {
            return if job.actor == actor && job.controller == controller && job.target == target {
                Ok(job.id)
            } else {
                Err("actor or target already has an active job".into())
            };
        }
        // Native consumers receive results through their existing activity/UI
        // flow. Script-owned outcomes remain until explicitly observed.
        let retained = |job: &JobRecord| {
            !job.state.terminal()
                || (!job.observed && matches!(job.controller, ReservationOwner::Behavior { .. }))
        };
        if self.records.values().filter(|job| retained(job)).count() >= MAX_RETAINED_JOBS {
            return Err("unobserved job result capacity reached".into());
        }
        let next = self
            .next
            .checked_add(1)
            .filter(|_| self.next > 0)
            .ok_or("job identities exhausted")?;
        let id = self.next;
        self.records.retain(|_, job| retained(job));
        self.next = next;
        self.records.insert(
            id,
            JobRecord {
                cause: None,
                id,
                revision: 1,
                actor,
                controller,
                target,
                state: JobState::Working,
                reason: None,
                observed: false,
            },
        );
        Ok(id)
    }
    pub fn finish(&mut self, id: u64, state: JobState, reason: Option<String>) -> bool {
        let Some(job) = self.records.get_mut(&id) else {
            return false;
        };
        if !state.terminal() || job.state.terminal() {
            return false;
        }
        job.state = state;
        job.reason = reason.map(|mut reason| {
            if reason.len() > 1024 {
                let mut end = 1024;
                while !reason.is_char_boundary(end) {
                    end -= 1;
                }
                reason.truncate(end);
            }
            reason
        });
        job.revision += 1;
        true
    }
    pub fn acknowledge(&mut self, id: u64, revision: u64) -> Result<(), String> {
        let job = self
            .records
            .get_mut(&id)
            .ok_or("craft job missing or already retired")?;
        if job.revision != revision || !job.state.terminal() {
            return Err("job outcome is not terminal or revision stale".into());
        }
        job.observed = true;
        Ok(())
    }
    pub fn suspend_for_load(&mut self) {
        for job in self
            .records
            .values_mut()
            .filter(|job| !job.state.terminal())
        {
            job.state = JobState::Interrupted;
            job.reason = Some("session reloaded; reservation must be revalidated".into());
            job.revision += 1;
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.next == 0 || self.records.len() > MAX_RETAINED_JOBS {
            return Err("invalid job ledger bounds".into());
        }
        for (&id, job) in &self.records {
            if id == 0
                || id != job.id
                || id >= self.next
                || !match job.state {
                    JobState::Accepted => job.revision == 1,
                    JobState::Working => (1..=2).contains(&job.revision),
                    state if state.terminal() => (2..=3).contains(&job.revision),
                    _ => false,
                }
                || (job.observed && !job.state.terminal())
                || job
                    .reason
                    .as_ref()
                    .is_some_and(|reason| reason.len() > 1024)
                || matches!(job.actor, ReservationOwner::Behavior { .. })
            {
                return Err(format!("invalid action job {id}"));
            }
            match &job.target {
                JobTarget::CraftOrder {
                    order_id,
                    order_revision,
                } if *order_id > 0 && *order_revision > 0 => {}
                JobTarget::Construction {
                    project_id,
                    project_revision,
                } if *project_id > 0 && *project_revision > 0 => {}
                JobTarget::Interaction {
                    slot,
                    placement_revision,
                    action,
                    ..
                } if slot.placement.0 > 0
                    && !slot.key.is_empty()
                    && slot.key.len() <= 128
                    && *placement_revision > 0
                    && !action.is_empty()
                    && action.len() <= 128 => {}
                _ => return Err(format!("invalid job target {id}")),
            }
        }
        let active: Vec<_> = self
            .records
            .values()
            .filter(|job| !job.state.terminal())
            .collect();
        for (i, a) in active.iter().enumerate() {
            if active[i + 1..]
                .iter()
                .any(|b| a.actor == b.actor || a.target.conflicts_with(&b.target))
            {
                return Err("conflicting active jobs in save".into());
            }
        }
        Ok(())
    }
}

pub fn validate_owners(
    save: &crate::save::SaveFile,
    mods: &block_junk_scripting::ModRegistry,
) -> Result<(), String> {
    let Some(world) = &save.world else {
        return Ok(());
    };
    world.interactions.validate()?;
    if world
        .interactions
        .jobs
        .records
        .values()
        .any(|job| !matches!(job.target, JobTarget::Interaction { .. }))
        || world
            .craft_jobs
            .records
            .values()
            .any(|job| !matches!(job.target, JobTarget::CraftOrder { .. }))
        || save
            .construction
            .jobs
            .records
            .values()
            .any(|job| !matches!(job.target, JobTarget::Construction { .. }))
    {
        return Err("saved job is stored in the wrong service ledger".into());
    }
    let mut active_actors = std::collections::BTreeSet::new();
    for job in world
        .craft_jobs
        .records
        .values()
        .chain(save.construction.jobs.records.values())
        .chain(world.interactions.jobs.records.values())
    {
        if !job.observed && job.cause.is_some_and(|cause| !world.events.contains(cause)) {
            return Err(format!("job {} references missing causal budget", job.id));
        }
        if !job.state.terminal() && !active_actors.insert(job.actor.clone()) {
            return Err("saved actor has conflicting active service jobs".into());
        }
        if let ReservationOwner::Behavior {
            mod_id,
            registration_id,
        } = &job.controller
        {
            if registration_id.is_empty()
                || registration_id.len() > 256
                || !mods.mods.iter().any(|module| module.manifest.id == *mod_id)
            {
                return Err(format!(
                    "action job {} references missing mod/behavior {}:{}",
                    job.id, mod_id, registration_id
                ));
            }
        } else if job.actor != job.controller {
            return Err(format!("action job {} has inconsistent controller", job.id));
        }
        match job.actor {
            ReservationOwner::Npc { id }
                if save.npcs.iter().any(|npc| npc.id == id)
                    || (job.state.terminal() && world.npc_ids.retired.contains(&id)) => {}
            ReservationOwner::Player { id }
                if save.players.iter().any(|player| player.client_id == id) => {}
            _ => return Err(format!("action job {} references missing actor", job.id)),
        }
        if !job.state.terminal() {
            let valid = match &job.target {
                JobTarget::Interaction {
                    slot,
                    placement_revision,
                    ..
                } => world
                    .placements
                    .records
                    .get(&slot.placement)
                    .is_some_and(|p| p.active && p.revision == *placement_revision),
                JobTarget::Construction {
                    project_id,
                    project_revision,
                } => save.plans.iter().any(|(_, project)| {
                    project.project_id == *project_id && project.revision == *project_revision
                }),
                JobTarget::CraftOrder {
                    order_id,
                    order_revision,
                } => save.craft_stations.iter().any(|(_, station)| {
                    station
                        .active_work
                        .as_ref()
                        .is_some_and(|work| work.order_id == *order_id)
                        && station
                            .orders
                            .iter()
                            .any(|order| order.id == *order_id && order.revision == *order_revision)
                }),
            };
            if !valid {
                return Err(format!(
                    "action job {} has no matching material commitment",
                    job.id
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn job_results_survive_reload_and_are_observed_without_replaying_completion() {
        let actor = ReservationOwner::Npc { id: 1 };
        let controller = ReservationOwner::Behavior {
            mod_id: "test".into(),
            registration_id: "planner".into(),
        };
        let target = JobTarget::CraftOrder {
            order_id: 1,
            order_revision: 1,
        };
        let mut jobs = JobLedger::default();
        let id = jobs
            .start(actor.clone(), controller.clone(), target.clone())
            .unwrap();
        assert_eq!(
            jobs.start(actor.clone(), controller.clone(), target.clone())
                .unwrap(),
            id
        );
        assert!(
            jobs.start(
                ReservationOwner::Npc { id: 2 },
                controller.clone(),
                target.clone()
            )
            .is_err()
        );
        let mut restored: JobLedger =
            serde_json::from_slice(&serde_json::to_vec(&jobs).unwrap()).unwrap();
        restored.validate().unwrap();
        restored.suspend_for_load();
        assert_eq!(restored.records[&id].state, JobState::Interrupted);
        let resumed = restored
            .start(actor.clone(), controller.clone(), target.clone())
            .unwrap();
        assert_ne!(id, resumed);
        assert!(restored.finish(resumed, JobState::Completed, None));
        assert!(!restored.finish(resumed, JobState::Completed, None));
        assert!(!restored.records[&id].reason.as_ref().unwrap().is_empty());
        restored.acknowledge(id, 2).unwrap();
        assert_eq!(restored.records[&resumed].state, JobState::Completed);
        restored.acknowledge(resumed, 2).unwrap();
        let next = restored.start(actor, controller, target).unwrap();
        assert!(next > resumed);
        assert!(!restored.records.contains_key(&id));
        restored.validate().unwrap();
    }
}

// Internally tagged API objects require deserialize_any, which the binary save
// codec deliberately does not support. Keep a typed, externally tagged wire
// representation here without changing Lua's readable tagged objects.
mod saved_records {
    use super::*;
    use serde::{Deserializer, Serializer};
    #[derive(Serialize, Deserialize)]
    enum Owner {
        Player(u64),
        Npc(u64),
        Behavior(String, String),
    }
    impl From<ReservationOwner> for Owner {
        fn from(value: ReservationOwner) -> Self {
            match value {
                ReservationOwner::Player { id } => Self::Player(id),
                ReservationOwner::Npc { id } => Self::Npc(id),
                ReservationOwner::Behavior {
                    mod_id,
                    registration_id,
                } => Self::Behavior(mod_id, registration_id),
            }
        }
    }
    impl From<Owner> for ReservationOwner {
        fn from(value: Owner) -> Self {
            match value {
                Owner::Player(id) => Self::Player { id },
                Owner::Npc(id) => Self::Npc { id },
                Owner::Behavior(mod_id, registration_id) => Self::Behavior {
                    mod_id,
                    registration_id,
                },
            }
        }
    }
    #[derive(Serialize, Deserialize)]
    enum Target {
        Interaction(
            block_junk_mod_api::architecture::UseSlotId,
            u64,
            String,
            bool,
        ),
        CraftOrder(u64, u64),
        Construction(u64, u64),
    }
    impl From<JobTarget> for Target {
        fn from(value: JobTarget) -> Self {
            match value {
                JobTarget::Interaction {
                    slot,
                    placement_revision,
                    action,
                    exclusive,
                } => Self::Interaction(slot, placement_revision, action, exclusive),
                JobTarget::CraftOrder {
                    order_id,
                    order_revision,
                } => Self::CraftOrder(order_id, order_revision),
                JobTarget::Construction {
                    project_id,
                    project_revision,
                } => Self::Construction(project_id, project_revision),
            }
        }
    }
    impl From<Target> for JobTarget {
        fn from(value: Target) -> Self {
            match value {
                Target::Interaction(slot, placement_revision, action, exclusive) => {
                    Self::Interaction {
                        slot,
                        placement_revision,
                        action,
                        exclusive,
                    }
                }
                Target::CraftOrder(order_id, order_revision) => Self::CraftOrder {
                    order_id,
                    order_revision,
                },
                Target::Construction(project_id, project_revision) => Self::Construction {
                    project_id,
                    project_revision,
                },
            }
        }
    }
    #[derive(Serialize, Deserialize)]
    struct Record {
        id: u64,
        revision: u64,
        actor: Owner,
        controller: Owner,
        target: Target,
        state: JobState,
        reason: Option<String>,
        observed: bool,
        cause: Option<block_junk_mod_api::authority::EventCause>,
    }
    pub fn serialize<S: Serializer>(
        records: &BTreeMap<u64, JobRecord>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        records
            .iter()
            .map(|(id, job)| {
                (
                    *id,
                    Record {
                        id: job.id,
                        revision: job.revision,
                        actor: job.actor.clone().into(),
                        controller: job.controller.clone().into(),
                        target: job.target.clone().into(),
                        state: job.state,
                        reason: job.reason.clone(),
                        observed: job.observed,
                        cause: job.cause,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<u64, JobRecord>, D::Error> {
        Ok(BTreeMap::<u64, Record>::deserialize(deserializer)?
            .into_iter()
            .map(|(id, job)| {
                (
                    id,
                    JobRecord {
                        id: job.id,
                        revision: job.revision,
                        actor: job.actor.into(),
                        controller: job.controller.into(),
                        target: job.target.into(),
                        state: job.state,
                        reason: job.reason,
                        observed: job.observed,
                        cause: job.cause,
                    },
                )
            })
            .collect())
    }
}
