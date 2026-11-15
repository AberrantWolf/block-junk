//! Authoritative construction ownership and material escrow. The spatial Plans
//! store publishes committed project state; this ledger owns identity allocation
//! and durable obligations after a project has been retired.
use crate::{items::ItemSlot, plans::Plans, protocol::PlanState};
use bevy::prelude::*;

#[derive(Clone, Debug)]
pub struct PendingRefund {
    pub project_id: u64,
    pub origin: IVec3,
    pub items: Vec<(ItemSlot, u32)>,
}

#[derive(Resource, Clone, Debug)]
pub struct ConstructionLedger {
    pub jobs: crate::action_jobs::JobLedger,
    leases: std::collections::BTreeMap<u64, (IVec3, crate::reservations::Lease)>,
    pub next_project_id: u64,
    pub pending_refunds: Vec<PendingRefund>,
    retired: Vec<IVec3>,
}
impl Default for ConstructionLedger {
    fn default() -> Self {
        Self {
            jobs: Default::default(),
            leases: Default::default(),
            next_project_id: 1,
            pending_refunds: Vec::new(),
            retired: Vec::new(),
        }
    }
}

impl ConstructionLedger {
    fn fail_actor(&mut self, actor: crate::npc::NpcId, reason: &str) {
        use block_junk_mod_api::authority::{JobState, ReservationOwner};
        let jobs: Vec<_> = self
            .jobs
            .records
            .values()
            .filter(|job| {
                !job.state.terminal() && job.actor == ReservationOwner::Npc { id: actor.0 }
            })
            .map(|job| job.id)
            .collect();
        for id in jobs {
            self.jobs
                .finish(id, JobState::Failed, Some(reason.to_owned()));
        }
    }
    /// Caller validates authoritative geometry first. No changes on conflict or
    /// invalid footprint, including no cancellation of an existing project.
    pub fn designate(&mut self, plans: &mut Plans, anchor: IVec3, mut proposed: PlanState) -> bool {
        proposed.footprint.sort_by_key(|c| (c.x, c.y, c.z));
        proposed.footprint.dedup();
        if proposed.footprint.is_empty()
            || !proposed.footprint.contains(&anchor)
            || proposed.footprint.len() > crate::protocol::MAX_BLOCK_FOOTPRINT_CELLS
        {
            return false;
        }
        if let Some(old) = plans.get(anchor)
            && old.kind == proposed.kind
            && old.footprint == proposed.footprint
        {
            return true;
        }
        if plans.iter().any(|(cell, state)| {
            *cell != anchor
                && state
                    .footprint
                    .iter()
                    .any(|c| proposed.footprint.contains(c))
        }) {
            return false;
        }
        let Some(next) = self.next_project_id.checked_add(1) else {
            return false;
        };
        proposed.project_id = self.next_project_id;
        proposed.revision = 1;
        proposed.progress_secs = 0.0;
        self.cancel(plans, anchor);
        self.next_project_id = next;
        plans.upsert(anchor, proposed, 0);
        true
    }

    pub fn cancel(&mut self, plans: &mut Plans, cell: IVec3) -> bool {
        let Some(anchor) = plans
            .iter()
            .find(|(anchor, state)| **anchor == cell || state.footprint.contains(&cell))
            .map(|(anchor, _)| *anchor)
        else {
            return false;
        };
        let state = plans.delete(&anchor, 0).expect("resolved project");
        self.finish_project(state.token(), false, "project canceled or replaced");
        self.retired.push(anchor);
        let items: Vec<_> = state
            .materials
            .into_iter()
            .filter(|m| m.present > 0)
            .map(|m| (m.item, m.present))
            .collect();
        if !items.is_empty() {
            self.pending_refunds.push(PendingRefund {
                project_id: state.project_id,
                origin: anchor,
                items,
            });
        }
        true
    }
    pub fn finish_project(&mut self, token: (u64, u64), success: bool, reason: &str) {
        use block_junk_mod_api::authority::{JobState, JobTarget};
        let ids: Vec<_> = self
            .jobs
            .records
            .values()
            .filter(|job| {
                !job.state.terminal()
                    && job.target
                        == (JobTarget::Construction {
                            project_id: token.0,
                            project_revision: token.1,
                        })
            })
            .map(|job| job.id)
            .collect();
        for id in ids {
            self.jobs.finish(
                id,
                if success {
                    JobState::Completed
                } else {
                    JobState::Failed
                },
                (!success).then(|| reason.to_owned()),
            );
        }
    }
}

/// Production command boundary: cancel/replacement releases worker and haul
/// reservations before another system can acquire the replacement project.
#[derive(bevy::ecs::system::SystemParam)]
pub struct ConstructionService<'w> {
    plans: ResMut<'w, Plans>,
    ledger: ResMut<'w, ConstructionLedger>,
    claims: ResMut<'w, crate::plan_claims::PlanClaims>,
    haul: ResMut<'w, crate::haul::HaulStore>,
}
impl std::ops::Deref for ConstructionService<'_> {
    type Target = Plans;
    fn deref(&self) -> &Plans {
        &self.plans
    }
}
impl ConstructionService<'_> {
    pub fn actor_is_hauling(&self, id: crate::npc::NpcId) -> bool {
        self.haul.has_assignment(id)
    }
    pub fn release_actor(&mut self, id: crate::npc::NpcId) {
        self.ledger.fail_actor(id, "construction actor removed");
        self.claims.release_all_for(id);
        self.haul.release_for_npc(id);
    }
    pub fn disable_mod_jobs(&mut self, owner: &str, reason: &str) {
        use block_junk_mod_api::authority::ReservationOwner;
        let jobs: Vec<_> = self.ledger.jobs.records.values().filter(|job|!job.state.terminal() && matches!(&job.controller,ReservationOwner::Behavior {mod_id,..} if mod_id==owner)).map(|job|job.id).collect();
        for id in jobs {
            self.ledger.jobs.finish(
                id,
                block_junk_mod_api::authority::JobState::Failed,
                Some(reason.to_owned()),
            );
            if let Some((cell, lease)) = self.ledger.leases.remove(&id) {
                self.claims.release_owned(cell, &lease);
            }
        }
    }

    pub fn designate(&mut self, anchor: IVec3, proposed: PlanState) -> bool {
        let accepted = self.ledger.designate(&mut self.plans, anchor, proposed);
        self.release_retired();
        accepted
    }
    pub fn cancel(&mut self, cell: IVec3) -> bool {
        let canceled = self.ledger.cancel(&mut self.plans, cell);
        self.release_retired();
        canceled
    }
    fn release_retired(&mut self) {
        for cell in self.ledger.retired.drain(..) {
            self.claims.revoke(cell);
            self.haul
                .release_target(crate::haul::HaulTarget::Plan(cell));
        }
    }
}

/// A private working copy never journals into the authoritative spatial store.
/// The service holds exclusive access until all commands have validated.
pub struct ConstructionTransaction {
    plans: Plans,
    ledger: ConstructionLedger,
    claims: crate::plan_claims::PlanClaims,
    retired: Vec<IVec3>,
}
impl ConstructionTransaction {
    pub fn release_actor(&mut self, id: crate::npc::NpcId) {
        self.ledger.fail_actor(id, "construction actor removed");
        self.claims.release_all_for(id);
    }
    fn release_retired_claims(&mut self) {
        for cell in self.ledger.retired.drain(..) {
            self.claims.revoke(cell);
            self.retired.push(cell);
        }
    }
    pub fn actor_has_job(&self, id: u64) -> bool {
        self.ledger.jobs.records.values().any(|job| {
            !job.state.terminal()
                && job.actor == block_junk_mod_api::authority::ReservationOwner::Npc { id }
        })
    }
    pub fn read_job(&self, id: u64) -> Result<block_junk_mod_api::authority::JobRecord, String> {
        self.ledger
            .jobs
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| "construction job missing or retired".into())
    }
    pub fn owned_jobs(
        &self,
        owner: &str,
        after: u64,
        limit: usize,
    ) -> (Vec<block_junk_mod_api::authority::JobRecord>, bool) {
        self.ledger.jobs.owned_jobs(owner, after, limit)
    }
    pub fn acknowledge_job(&mut self, owner: &str, id: u64, revision: u64) -> Result<(), String> {
        let job = self.read_job(id)?;
        if !matches!(&job.controller, block_junk_mod_api::authority::ReservationOwner::Behavior {mod_id,..} if mod_id == owner)
        {
            return Err("only the controlling mod may acknowledge a job result".into());
        }
        self.ledger.jobs.acknowledge(id, revision)
    }
    pub fn prepare_job(
        &mut self,
        id: u64,
        revision: u64,
        npc_id: u64,
        controller: block_junk_mod_api::authority::ReservationOwner,
        cause: Option<block_junk_mod_api::authority::EventCause>,
    ) -> Result<block_junk_mod_api::authority::JobRecord, String> {
        use block_junk_mod_api::authority::{JobState, JobTarget, ReservationOwner};
        let cell = self.project_anchor(id, revision)?;
        if !self
            .plans
            .get(cell)
            .expect("validated project")
            .is_satisfied()
        {
            return Err("construction materials are incomplete".into());
        }
        let mut claims = self.claims.clone();
        let lease = claims
            .claim_owned(cell, ReservationOwner::Npc { id: npc_id })
            .ok_or("construction project already reserved")?;
        let mut jobs = self.ledger.jobs.clone();
        let before = jobs.next;
        let job_id = jobs.start(
            ReservationOwner::Npc { id: npc_id },
            controller,
            JobTarget::Construction {
                project_id: id,
                project_revision: revision,
            },
        )?;
        self.claims = claims;
        self.ledger.jobs = jobs;
        self.ledger.leases.insert(job_id, (cell, lease));
        let job = self
            .ledger
            .jobs
            .records
            .get_mut(&job_id)
            .expect("accepted construction job");
        if job_id == before {
            job.state = JobState::Accepted;
            job.cause = cause;
        }
        Ok(job.clone())
    }
    pub fn project_anchor(&self, id: u64, revision: u64) -> Result<IVec3, String> {
        self.plans
            .iter()
            .find(|(_, p)| p.token() == (id, revision))
            .map(|(cell, _)| *cell)
            .ok_or_else(|| "project missing or revision stale".into())
    }
    pub fn deliver(
        &mut self,
        id: u64,
        revision: u64,
        carry: &mut crate::protocol::Carrying,
        count: u32,
    ) -> Result<u32, String> {
        let anchor = self.project_anchor(id, revision)?;
        let accepted =
            transfer_carried_materials(&mut self.plans, anchor, (id, revision), carry, count);
        if accepted == 0 {
            return Err("project cannot accept the supplied material".into());
        }
        Ok(accepted)
    }
    pub fn designate(&mut self, anchor: IVec3, state: PlanState) -> Result<(), String> {
        if self.ledger.designate(&mut self.plans, anchor, state) {
            self.release_retired_claims();
            Ok(())
        } else {
            Err("project footprint conflicts with another project".into())
        }
    }
    pub fn resolve_terrain(&mut self, cells: impl Iterator<Item = IVec3>) {
        let cells: std::collections::HashSet<_> = cells.collect();
        let anchors: Vec<_> = self
            .plans
            .iter()
            .filter(|(_, p)| p.footprint.iter().any(|cell| cells.contains(cell)))
            .map(|(anchor, _)| *anchor)
            .collect();
        for anchor in anchors {
            self.ledger.cancel(&mut self.plans, anchor);
        }
        self.release_retired_claims();
    }
    pub fn cancel(&mut self, id: u64, revision: u64) -> Result<(), String> {
        let anchor = self
            .plans
            .iter()
            .find(|(_, p)| p.token() == (id, revision))
            .map(|(c, _)| *c)
            .ok_or("project missing or revision stale")?;
        self.ledger.cancel(&mut self.plans, anchor);
        self.release_retired_claims();
        Ok(())
    }
}

/// One transfer primitive for player requests, NPC arrivals and staged scripts.
/// Both sides of the transfer commit together; stale project tokens do nothing.
pub fn transfer_carried_materials(
    plans: &mut Plans,
    cell: IVec3,
    token: (u64, u64),
    carry: &mut crate::protocol::Carrying,
    requested: u32,
) -> u32 {
    if plans
        .get(cell)
        .is_none_or(|project| project.token() != token)
    {
        return 0;
    }
    let Some(item) = carry.item else {
        return 0;
    };
    let accepted = plans.deposit(cell, item, requested.min(carry.count));
    carry.count -= accepted;
    if carry.count == 0 {
        carry.item = None;
    }
    accepted
}
impl ConstructionService<'_> {
    pub fn stage(&self) -> ConstructionTransaction {
        let mut plans = Plans::default();
        for (cell, state) in self.plans.iter() {
            plans.upsert(*cell, state.clone(), 0);
        }
        let mut transaction = ConstructionTransaction {
            plans,
            ledger: self.ledger.clone(),
            claims: self.claims.clone(),
            retired: Vec::new(),
        };
        transaction.release_retired_claims();
        transaction
    }
    /// Infallible after validation, while this service retains exclusive access.
    pub fn commit(&mut self, transaction: ConstructionTransaction) {
        let removed: Vec<_> = self
            .plans
            .iter()
            .filter(|(cell, _)| transaction.plans.get(**cell).is_none())
            .map(|(cell, _)| *cell)
            .collect();
        for cell in removed {
            self.plans.delete(&cell, 0);
        }
        for (cell, state) in transaction.plans.iter() {
            if self.plans.get(*cell) != Some(state) {
                self.plans.upsert(*cell, state.clone(), 0);
            }
        }
        *self.ledger = transaction.ledger;
        *self.claims = transaction.claims;
        for cell in transaction.retired {
            self.haul
                .release_target(crate::haul::HaulTarget::Plan(cell));
        }
    }
}

/// A validated world placement is committed inside this closure. The project is
/// consumed only on success. Repeated or stale completions cannot call it again.
pub fn complete(
    plans: &mut Plans,
    cell: IVec3,
    token: (u64, u64),
    commit: impl FnOnce() -> bool,
) -> bool {
    if !plans
        .get(cell)
        .is_some_and(|state| (state.project_id, state.revision) == token && state.is_satisfied())
    {
        return false;
    }
    if !commit() {
        return false;
    }
    plans.delete(&cell, 0);
    true
}

/// Retry only in loaded geometry. No destination means the durable obligation
/// remains. Deferred spawns are flushed by the schedule before save snapshots.
pub fn deliver_pending_refunds(
    mut ledger: ResMut<ConstructionLedger>,
    sidecars: Query<&'static crate::voxel::ChunkEntities>,
    chunks: Query<&'static crate::voxel::Chunk>,
    map: Res<crate::voxel::ChunkMap>,
    registry: Res<crate::blocks::BlockRegistry>,
    mut commands: Commands,
) {
    let walk = crate::npc::WorldWalk {
        chunks: &chunks,
        sidecars: &sidecars,
        chunk_map: &map,
        registry: &registry,
    };
    let count = ledger
        .pending_refunds
        .len()
        .min(crate::item_placement::DELIVERIES_PER_TICK);
    let candidates: Vec<_> = ledger.pending_refunds.drain(..count).collect();
    for refund in candidates {
        if let Some(translation) = crate::item_placement::destination(&walk, refund.origin) {
            for (item, count) in refund.items {
                crate::item_placement::spawn_stack(&mut commands, item, count, translation);
            }
        } else {
            ledger.pending_refunds.push(refund);
        }
    }
}

/// Bind accepted commands only after their transaction's deferred actor creation.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct ConstructionJobsWorld<'w, 's> {
    ledger: ResMut<'w, ConstructionLedger>,
    plans: Res<'w, Plans>,
    claims: ResMut<'w, crate::plan_claims::PlanClaims>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
    map: Res<'w, crate::voxel::ChunkMap>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    items: Res<'w, crate::items::ItemRegistry>,
    defaults: Res<'w, crate::npc_registry::WorkDefaultsRes>,
    actors: Query<'w, 's, ConstructionActor>,
}
type ConstructionActor = (
    &'static crate::npc::NpcId,
    &'static mut crate::npc::Brain,
    &'static crate::protocol::AvatarPose,
    &'static crate::protocol::EquippedTool,
    &'static crate::npc::Needs,
    Has<crate::npc::BrainDisabled>,
);
pub(crate) fn activate_jobs(mut world: ConstructionJobsWorld) {
    use block_junk_mod_api::authority::{JobState, JobTarget, ReservationOwner};
    let pending: Vec<_> = world
        .ledger
        .jobs
        .records
        .values()
        .filter(|job| job.state == JobState::Accepted)
        .take(32)
        .cloned()
        .collect();
    for job in pending {
        let result = (|| -> Result<(), String> {
            let JobTarget::Construction {
                project_id,
                project_revision,
            } = job.target
            else {
                return Err("invalid construction target".into());
            };
            let ReservationOwner::Npc { id } = job.actor else {
                return Err("invalid construction actor".into());
            };
            let (cell, project) = world
                .plans
                .iter()
                .find(|(_, project)| project.token() == (project_id, project_revision))
                .ok_or("project canceled or replaced")?;
            if !project.is_satisfied()
                || !world
                    .ledger
                    .leases
                    .get(&job.id)
                    .is_some_and(|(anchor, lease)| {
                        anchor == cell && world.claims.matches(*cell, lease)
                    })
            {
                return Err("project materials or reservation unavailable".into());
            }
            if project.footprint.iter().any(|cell| {
                world
                    .map
                    .0
                    .get(&crate::voxel::world_to_chunk(*cell).0)
                    .is_none_or(|entity| world.chunks.get(*entity).is_err())
            }) {
                return Err("project geometry unavailable".into());
            }
            let (_, mut brain, pose, tool, needs, disabled) = world
                .actors
                .iter_mut()
                .find(|(npc, ..)| npc.0 == id)
                .ok_or("construction actor removed")?;
            if disabled || !matches!(brain.goal, crate::npc::Goal::Idle) {
                return Err("construction actor interrupted before activation".into());
            }
            if !pose.translation.is_finite()
                || pose.translation.distance(cell.as_vec3() + Vec3::splat(0.5))
                    > crate::protocol::INTERACT_REACH
            {
                return Err("construction actor moved out of reach".into());
            }
            let slot = match project.kind {
                crate::protocol::PlanKind::Build { slot, .. } => slot,
                crate::protocol::PlanKind::Remove => {
                    let (coord, local) = crate::voxel::world_to_chunk(*cell);
                    world
                        .chunks
                        .get(world.map.0[&coord])
                        .map_err(|_| "project geometry unavailable")?
                        .get(local)
                }
            };
            let def = world
                .blocks
                .try_def(slot)
                .filter(|_| !slot.is_empty())
                .ok_or("project block unavailable")?;
            let verb = project.kind.work_verb();
            let multiplier = if let Some(work) = &def.work_action {
                let has_tool = work
                    .tool_for(verb)
                    .is_none_or(|tag| world.items.tool_has_tag(tool.item, tag));
                work.duration_multiplier(verb, has_tool)
                    .ok_or("construction tool requirement failed")?
            } else {
                1.
            };
            let (duration, restore) = crate::npc::resolve_work_action(
                project.kind,
                *cell,
                &world.chunks,
                &world.map,
                &world.blocks,
                &world.defaults.0,
            );
            let worn = world
                .defaults
                .0
                .worn_down
                .as_ref()
                .filter(|rule| needs.0.get(&rule.need).copied().unwrap_or(0.) >= rule.above)
                .map_or(1., |rule| rule.multiplier);
            brain.goal = crate::npc::Goal::Working {
                project_token: project.token(),
                remaining_secs: (duration * multiplier * worn - project.progress_secs).max(0.),
                target_cell: *cell,
                plan_kind: project.kind,
                need_restore: restore,
            };
            Ok(())
        })();
        match result {
            Ok(()) => {
                let job = world
                    .ledger
                    .jobs
                    .records
                    .get_mut(&job.id)
                    .expect("accepted job");
                job.state = JobState::Working;
                job.revision += 1;
            }
            Err(reason) => {
                world
                    .ledger
                    .jobs
                    .finish(job.id, JobState::Failed, Some(reason));
                if let ReservationOwner::Npc { id } = job.actor {
                    let _ = id;
                    if let Some((cell, lease)) = world.ledger.leases.remove(&job.id) {
                        world.claims.release_owned(cell, &lease);
                    }
                }
            }
        }
    }
}
/// Interrupt abandoned work after completion messages have committed in Update.
/// Durable escrow and progress remain on the project for a later attempt.
pub(crate) fn reconcile_jobs(mut world: ConstructionJobsWorld) {
    use block_junk_mod_api::authority::{JobState, JobTarget, ReservationOwner};
    let retired: Vec<_> = world
        .ledger
        .leases
        .keys()
        .copied()
        .filter(|id| {
            world
                .ledger
                .jobs
                .records
                .get(id)
                .is_none_or(|job| job.state.terminal())
        })
        .collect();
    for id in retired {
        if let Some((cell, lease)) = world.ledger.leases.remove(&id) {
            world.claims.release_owned(cell, &lease);
        }
    }
    let active: Vec<_> = world
        .ledger
        .jobs
        .records
        .values()
        .filter(|job| job.state == JobState::Working)
        .cloned()
        .collect();
    for job in active {
        let valid = match (&job.target,&job.actor) {
            (JobTarget::Construction {project_id,project_revision},ReservationOwner::Npc {id}) => world.plans.iter().find(|(_,project)|project.token()==(*project_id,*project_revision)).is_some_and(|(cell,_)|world.claims.holder(*cell)==Some(crate::npc::NpcId(*id))) && world.actors.iter().any(|(npc,brain,_,_,_,disabled)|npc.0==*id && !disabled && matches!(brain.goal,crate::npc::Goal::Working {project_token,..} if project_token==(*project_id,*project_revision))),
            _=>false,
        };
        let valid = valid
            && world
                .ledger
                .leases
                .get(&job.id)
                .is_some_and(|(cell, lease)| world.claims.matches(*cell, lease));
        if !valid {
            world.ledger.jobs.finish(
                job.id,
                JobState::Interrupted,
                Some("construction actor or reservation no longer active".into()),
            );
            if let ReservationOwner::Npc { id } = job.actor {
                let _ = id;
                if let Some((cell, lease)) = world.ledger.leases.remove(&job.id) {
                    world.claims.release_owned(cell, &lease);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MaterialEntry, PlanKind};
    #[test]
    fn replacement_commits_new_job_without_revoking_its_claim_and_failure_preserves_escrow() {
        use block_junk_mod_api::authority::{JobState, ReservationOwner};
        let mut app = App::new();
        app.init_resource::<Plans>()
            .init_resource::<ConstructionLedger>()
            .init_resource::<crate::plan_claims::PlanClaims>()
            .init_resource::<crate::haul::HaulStore>();
        let mut state = bevy::ecs::system::SystemState::<ConstructionService>::new(app.world_mut());
        {
            let mut service = state.get_mut(app.world_mut()).unwrap();
            assert!(service.designate(IVec3::ZERO, proposed(vec![IVec3::ZERO])));
            assert_eq!(service.plans.deposit(IVec3::ZERO, ItemSlot(1), 3), 3);
            let mut transaction = service.stage();
            let controller = ReservationOwner::Behavior {
                mod_id: "test".into(),
                registration_id: "builder".into(),
            };
            let first = transaction
                .prepare_job(1, 1, 10, controller.clone(), None)
                .unwrap();
            assert_eq!(first.state, JobState::Accepted);
            assert_eq!(
                transaction
                    .prepare_job(1, 1, 10, controller.clone(), None)
                    .unwrap()
                    .id,
                first.id
            );
            assert!(
                transaction
                    .prepare_job(1, 1, 20, controller.clone(), None)
                    .is_err()
            );
            service.commit(transaction);
            let mut transaction = service.stage();
            let mut replacement = PlanState::new(PlanKind::Remove, vec![]);
            replacement.footprint = vec![IVec3::ZERO, IVec3::X];
            transaction.designate(IVec3::ZERO, replacement).unwrap();
            let second = transaction.prepare_job(2, 1, 20, controller, None).unwrap();
            service.commit(transaction);
            assert_eq!(
                service.claims.holder(IVec3::ZERO),
                Some(crate::npc::NpcId(20))
            );
            assert_eq!(
                service.ledger.jobs.records[&first.id].state,
                JobState::Failed
            );
            assert_eq!(
                service.ledger.jobs.records[&second.id].state,
                JobState::Accepted
            );
            assert_eq!(
                service.ledger.pending_refunds[0].items,
                vec![(ItemSlot(1), 3)]
            );
            // A later failure abandons its complete staged view.
            let mut rejected = service.stage();
            rejected.cancel(2, 1).unwrap();
            assert!(
                rejected
                    .prepare_job(2, 1, 20, ReservationOwner::Npc { id: 20 }, None)
                    .is_err()
            );
            assert_eq!(
                service.ledger.jobs.records[&second.id].state,
                JobState::Accepted
            );
            service.disable_mod_jobs("test", "planner failed");
            assert_eq!(
                service.ledger.jobs.records[&second.id].state,
                JobState::Failed
            );
            assert_eq!(service.claims.holder(IVec3::ZERO), None);
            assert_eq!(service.plans.get(IVec3::ZERO).unwrap().project_id, 2);
            assert_eq!(
                service.ledger.pending_refunds[0].items,
                vec![(ItemSlot(1), 3)]
            );
            service.ledger.jobs.validate().unwrap();
        }
    }
    fn proposed(cells: Vec<IVec3>) -> PlanState {
        let mut state = PlanState::new(
            PlanKind::Remove,
            vec![MaterialEntry {
                item: ItemSlot(1),
                needed: 3,
                present: 0,
            }],
        );
        state.footprint = cells;
        state
    }
    #[test]
    fn replacement_is_atomic_and_failed_replacement_preserves_paid_project() {
        let mut ledger = ConstructionLedger::default();
        let mut plans = Plans::default();
        let anchor = IVec3::ZERO;
        assert!(ledger.designate(&mut plans, anchor, proposed(vec![anchor])));
        assert_eq!(plans.deposit(anchor, ItemSlot(1), 2), 2);
        let old = plans.get(anchor).unwrap().clone();
        assert!(ledger.designate(&mut plans, IVec3::X, proposed(vec![IVec3::X])));
        assert!(!ledger.designate(&mut plans, anchor, proposed(vec![anchor, IVec3::X])));
        assert_eq!(plans.get(anchor), Some(&old));
        assert!(ledger.pending_refunds.is_empty());
        assert!(ledger.designate(&mut plans, anchor, proposed(vec![anchor, IVec3::Z])));
        assert_ne!(plans.get(anchor).unwrap().token(), old.token());
        assert_eq!(plans.get(anchor).unwrap().materials[0].present, 0);
        assert_eq!(ledger.pending_refunds[0].items, vec![(ItemSlot(1), 2)]);
        assert!(!complete(&mut plans, anchor, old.token(), || panic!(
            "replacement cannot be completed by stale work"
        )));
    }

    #[test]
    fn idempotence_overlap_cancel_and_exactly_once_completion() {
        let mut ledger = ConstructionLedger::default();
        let mut plans = Plans::default();
        let anchor = IVec3::ZERO;
        let proposal = proposed(vec![anchor, IVec3::X]);
        assert!(ledger.designate(&mut plans, anchor, proposal.clone()));
        assert_eq!(plans.deposit(anchor, ItemSlot(1), 2), 2);
        let saved = plans.get(anchor).unwrap().clone();
        assert!(ledger.designate(&mut plans, anchor, proposal));
        assert_eq!(plans.get(anchor), Some(&saved));
        assert!(!ledger.designate(&mut plans, IVec3::X, proposed(vec![IVec3::X])));
        assert!(!complete(
            &mut plans,
            anchor,
            (saved.project_id, saved.revision),
            || panic!("insufficient escrow")
        ));
        assert!(ledger.cancel(&mut plans, IVec3::X));
        assert!(!ledger.cancel(&mut plans, anchor));
        assert_eq!(ledger.pending_refunds.len(), 1);
        assert_eq!(ledger.pending_refunds[0].items, vec![(ItemSlot(1), 2)]);
        assert!(ledger.designate(&mut plans, anchor, proposed(vec![anchor])));
        assert!(!complete(
            &mut plans,
            anchor,
            (saved.project_id, saved.revision),
            || panic!("stale completion")
        ));
        assert_eq!(plans.deposit(anchor, ItemSlot(1), 4), 3);
        let state = plans.get(anchor).unwrap().clone();
        let token = (state.project_id, state.revision);
        assert!(!complete(&mut plans, anchor, token, || false));
        assert_eq!(plans.get(anchor), Some(&state));
        assert!(complete(&mut plans, anchor, token, || true));
        assert!(!complete(&mut plans, anchor, token, || panic!(
            "duplicate completion"
        )));
        assert_eq!(ledger.pending_refunds.len(), 1);
    }
}
