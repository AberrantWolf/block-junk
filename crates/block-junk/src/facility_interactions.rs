//! Named-slot action commitments. Goal timers never authorize effects: completion
//! must still own this job and its original placement revision and reservation.
use bevy::prelude::*;
use block_junk_mod_api::{
    architecture::{FacilityAction, FacilitySlot, UseSlotId},
    authority::{EventCause, JobRecord, JobState, JobTarget, ReservationOwner},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    #[serde(skip)]
    pub completion_queued: bool,
    pub remaining_secs: f32,
    pub approach: [i32; 3],
}
#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct FacilityInteractions {
    pub jobs: crate::action_jobs::JobLedger,
    pub progress: BTreeMap<u64, Progress>,
    #[serde(skip)]
    leases: BTreeMap<u64, crate::reservations::Lease>,
}

pub struct InteractionRequest<'a> {
    pub placement: &'a crate::placements::PlacementRecord,
    pub slot_key: &'a str,
    pub action: &'a str,
    pub actor: ReservationOwner,
    pub controller: ReservationOwner,
    pub cause: Option<EventCause>,
    pub approach: IVec3,
}

pub fn definition<'a>(
    placement: &crate::placements::PlacementRecord,
    key: &str,
    action: &str,
    blocks: &'a crate::blocks::BlockRegistry,
) -> Result<(&'a FacilitySlot, &'a FacilityAction), String> {
    if !placement.active {
        return Err("interaction placement retired".into());
    }
    let block = blocks
        .slot_of(&placement.definition)
        .ok_or("interaction definition missing")?;
    let slot = blocks.architecture[usize::from(block.0)][placement.orientation as usize]
        .slots
        .iter()
        .find(|slot| slot.key == key)
        .ok_or("named use slot missing")?;
    if !slot.interactions.iter().any(|value| value == action) {
        return Err("slot does not support the requested action".into());
    }
    let action = blocks
        .def(block)
        .facility_actions
        .get(action)
        .ok_or("facility action missing")?;
    Ok((slot, action))
}
impl FacilityInteractions {
    pub fn active_for(&self, actor: u64) -> Option<&JobRecord> {
        self.jobs
            .records
            .values()
            .find(|job| !job.state.terminal() && job.actor == ReservationOwner::Npc { id: actor })
    }
    pub fn prepare(
        &mut self,
        request: InteractionRequest<'_>,
        blocks: &crate::blocks::BlockRegistry,
        claims: &mut crate::interactables::InteractionClaims,
    ) -> Result<JobRecord, String> {
        let (slot, action) =
            definition(request.placement, request.slot_key, request.action, blocks)?;
        let FacilityAction::Timed { duration_secs, .. } = action else {
            return Err(
                "crafting and storage require their parameterized transaction commands".into(),
            );
        };
        let anchor = IVec3::from_array(request.placement.anchor);
        if !slot
            .approaches
            .iter()
            .any(|offset| anchor + IVec3::from_array(*offset) == request.approach)
        {
            return Err("interaction approach is not authored for this slot".into());
        }
        let target = JobTarget::Interaction {
            slot: UseSlotId {
                placement: request.placement.id,
                key: slot.key.clone(),
            },
            placement_revision: request.placement.revision,
            action: request.action.into(),
            exclusive: slot.exclusive,
        };
        let resume = self
            .jobs
            .records
            .values()
            .rev()
            .find(|job| {
                job.actor == request.actor
                    && job.controller == request.controller
                    && job.target == target
                    && job.state == JobState::Interrupted
                    && job.reason.as_deref()
                        == Some("session reloaded; reservation must be revalidated")
                    && self.progress.contains_key(&job.id)
            })
            .map(|job| {
                (
                    job.id,
                    self.progress[&job.id].remaining_secs.min(*duration_secs),
                )
            });
        let mut jobs = self.jobs.clone();
        let before = jobs.next;
        let id = jobs.start(request.actor.clone(), request.controller, target.clone())?;
        if id != before {
            return Ok(jobs.records[&id].clone());
        }
        let lease = if slot.exclusive {
            let JobTarget::Interaction { slot, .. } = &target else {
                unreachable!()
            };
            Some(
                claims
                    .claim_slot(anchor, slot.clone(), request.actor)
                    .ok_or("use slot reserved")?,
            )
        } else {
            None
        };
        let job = jobs.records.get_mut(&id).expect("accepted job");
        job.state = JobState::Accepted;
        job.cause = request.cause;
        let job = job.clone();
        self.jobs = jobs;
        if let Some((old, _)) = resume {
            self.progress.remove(&old);
        }
        self.progress
            .retain(|id, _| self.jobs.records.contains_key(id));
        self.progress.insert(
            id,
            Progress {
                completion_queued: false,
                remaining_secs: resume.map_or(*duration_secs, |(_, remaining)| remaining),
                approach: request.approach.to_array(),
            },
        );
        if let Some(lease) = lease {
            self.leases.insert(id, lease);
        }
        Ok(job)
    }
    pub fn validate_target<'a>(
        &self,
        id: u64,
        placements: &crate::placements::Placements,
        blocks: &'a crate::blocks::BlockRegistry,
        claims: &crate::interactables::InteractionClaims,
    ) -> Result<(&'a FacilitySlot, &'a FacilityAction), String> {
        let job = self
            .jobs
            .records
            .get(&id)
            .filter(|job| !job.state.terminal())
            .ok_or("interaction job missing or terminal")?;
        let JobTarget::Interaction {
            slot,
            placement_revision,
            action,
            exclusive,
        } = &job.target
        else {
            return Err("wrong interaction target".into());
        };
        let placement = placements
            .records
            .get(&slot.placement)
            .filter(|p| p.active && p.revision == *placement_revision)
            .ok_or("interaction placement destroyed or changed")?;
        if *exclusive
            && !self
                .leases
                .get(&id)
                .is_some_and(|lease| claims.matches_slot(slot, lease))
        {
            return Err("interaction reservation lost".into());
        }
        definition(placement, &slot.key, action, blocks)
    }
    pub fn working(&mut self, id: u64) {
        if let Some(job) = self.jobs.records.get_mut(&id)
            && job.state == JobState::Accepted
        {
            job.state = JobState::Working;
            job.revision += 1;
        }
    }
    pub fn finish(
        &mut self,
        id: u64,
        state: JobState,
        reason: Option<String>,
        claims: &mut crate::interactables::InteractionClaims,
    ) -> bool {
        if !self.jobs.finish(id, state, reason) {
            return false;
        }
        if let Some(lease) = self.leases.remove(&id)
            && let JobTarget::Interaction { slot, .. } = &self.jobs.records[&id].target
        {
            claims.release_slot(slot, &lease);
        }
        self.progress.remove(&id);
        true
    }
    pub fn interrupt_actor(
        &mut self,
        actor: u64,
        reason: &str,
        claims: &mut crate::interactables::InteractionClaims,
    ) {
        if let Some(id) = self.active_for(actor).map(|job| job.id) {
            self.finish(id, JobState::Interrupted, Some(reason.into()), claims);
        }
    }
}

#[derive(Message, Clone, Debug)]
pub struct InteractionCompleted {
    pub job_id: u64,
    pub actor: crate::npc::NpcId,
}

pub fn approach<W: crate::pathfinding::Walkability>(
    slot: &FacilitySlot,
    anchor: IVec3,
    foot: IVec3,
    world: &W,
) -> Option<(IVec3, Option<crate::npc::UseSlotSnap>)> {
    let stand = slot
        .approaches
        .iter()
        .map(|offset| anchor + IVec3::from_array(*offset))
        .filter(|cell| crate::pathfinding::standable(world, *cell))
        .min_by_key(|cell| {
            let d = *cell - foot;
            d.x.abs() + d.y.abs() + d.z.abs()
        })?;
    let snap = slot.pose.map(|pose| crate::npc::UseSlotSnap {
        translation: anchor.as_vec3()
            + Vec3::new(
                0.5,
                crate::physics::EYE_OFFSET_FROM_CENTRE + crate::physics::PLAYER_HALF_EXTENTS.y,
                0.5,
            )
            + Vec3::from_array(pose),
        yaw: slot.yaw,
    });
    Some((stand, snap))
}

impl FacilityInteractions {
    /// Loaded jobs have no live lease. Retain their exit context until the first
    /// pre-update so a snapped body does not restart policy inside furniture.
    pub fn loaded_occupancy(&self, npc: u64) -> Option<FacilityOccupancy> {
        self.progress.iter().find_map(|(id, progress)| {
            let job = self.jobs.records.get(id)?;
            (!job.state.terminal() && job.actor == ReservationOwner::Npc { id: npc }).then_some(
                FacilityOccupancy {
                    job_id: *id,
                    approach: IVec3::from_array(progress.approach),
                },
            )
        })
    }

    pub fn suspend_for_load(&mut self) {
        self.jobs.suspend_for_load();
        self.leases.clear();
    }
    pub fn validate(&self) -> Result<(), String> {
        self.jobs.validate()?;
        for (id, progress) in &self.progress {
            if !progress.remaining_secs.is_finite()
                || progress.remaining_secs < 0.
                || progress
                    .approach
                    .iter()
                    .any(|v| v.unsigned_abs() > 1_000_064)
                || !self.jobs.records.get(id).is_some_and(|job| {
                    (!job.state.terminal()
                        || (job.state == JobState::Interrupted
                            && job.reason.as_deref()
                                == Some("session reloaded; reservation must be revalidated")))
                        && matches!(job.target, JobTarget::Interaction { .. })
                })
            {
                return Err("invalid saved interaction progress".into());
            }
        }
        if self.jobs.records.values().any(|job| {
            !matches!(job.target, JobTarget::Interaction { .. })
                || (!job.state.terminal() && !self.progress.contains_key(&job.id))
        }) {
            return Err("interaction job has no matching progress".into());
        }
        Ok(())
    }
    pub fn disable_mod(
        &mut self,
        owner: &str,
        reason: &str,
        claims: &mut crate::interactables::InteractionClaims,
    ) {
        let ids:Vec<_>=self.jobs.records.values().filter(|job|!job.state.terminal() && matches!(&job.controller,ReservationOwner::Behavior{mod_id,..} if mod_id==owner)).map(|job|job.id).collect();
        for id in ids {
            self.finish(id, JobState::Interrupted, Some(reason.into()), claims);
        }
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct ActivationWorld<'w, 's> {
    placements: Res<'w, crate::placements::Placements>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
    sidecars: Query<'w, 's, &'static crate::voxel::ChunkEntities>,
    map: Res<'w, crate::voxel::ChunkMap>,
    access: Res<'w, crate::facility_access::FacilityAccess>,
    preset: Option<Res<'w, crate::scripting::SelectedWorld>>,
    actors: Query<
        'w,
        's,
        (
            Entity,
            &'static crate::npc::NpcId,
            &'static mut crate::npc::Brain,
            &'static mut crate::protocol::AvatarPose,
        ),
        Without<crate::npc::BrainDisabled>,
    >,
}
pub fn activate(
    mut jobs: ResMut<FacilityInteractions>,
    mut claims: ResMut<crate::interactables::InteractionClaims>,
    mut world: ActivationWorld,
    mut commands: Commands,
) {
    let ids: Vec<_> = jobs
        .jobs
        .records
        .values()
        .filter(|job| {
            job.state == JobState::Accepted
                && matches!(job.controller, ReservationOwner::Behavior { .. })
        })
        .map(|job| job.id)
        .take(32)
        .collect();
    let walk = crate::npc::WorldWalk {
        chunks: &world.chunks,
        sidecars: &world.sidecars,
        chunk_map: &world.map,
        registry: &world.blocks,
    };
    for id in ids {
        let outcome = (|| -> Result<(), String> {
            let job = &jobs.jobs.records[&id];
            let ReservationOwner::Npc { id: actor } = job.actor else {
                return Err("interaction requires an NPC actor".into());
            };
            let (slot, action) =
                jobs.validate_target(id, &world.placements, &world.blocks, &claims)?;
            let FacilityAction::Timed { need_restore, .. } = action else {
                return Err("wrong action kind".into());
            };
            let JobTarget::Interaction { slot: slot_id, .. } = &job.target else {
                unreachable!()
            };
            let anchor = IVec3::from_array(world.placements.records[&slot_id.placement].anchor);
            let progress = &jobs.progress[&id];
            let foot = IVec3::from_array(progress.approach);
            crate::facility_access::require_usable_approach(
                &world.access,
                world.placements.records[&slot_id.placement].revision,
                slot_id,
                &world
                    .preset
                    .as_deref()
                    .ok_or("selected world unavailable")?
                    .0
                    .movement_profile,
                foot,
            )?;
            let (entity, _, mut brain, mut pose) = world
                .actors
                .iter_mut()
                .find(|(_, npc, _, _)| npc.0 == actor)
                .ok_or("interaction actor unavailable")?;
            if !matches!(brain.goal, crate::npc::Goal::Idle)
                || !crate::pathfinding::standable(&walk, foot)
                || pose.translation.distance(foot.as_vec3() + Vec3::splat(0.5)) > 2.
            {
                return Err("interaction approach or actor no longer available".into());
            }
            if let Some(offset) = slot.pose {
                pose.translation = anchor.as_vec3()
                    + Vec3::new(
                        0.5,
                        crate::physics::EYE_OFFSET_FROM_CENTRE
                            + crate::physics::PLAYER_HALF_EXTENTS.y,
                        0.5,
                    )
                    + Vec3::from_array(offset);
                pose.yaw = slot.yaw;
                commands
                    .entity(entity)
                    .insert(crate::protocol::KinematicLock);
            }
            commands.entity(entity).insert(FacilityOccupancy {
                job_id: id,
                approach: foot,
            });
            brain.goal = crate::npc::Goal::Interacting {
                remaining_secs: progress.remaining_secs,
                need_restore: need_restore.clone(),
                target_cell: anchor,
                anchor_cell: anchor,
                exclusive: slot.exclusive,
                animation: slot.animation.clone(),
            };
            Ok(())
        })();
        match outcome {
            Ok(()) => jobs.working(id),
            Err(reason) => {
                jobs.finish(id, JobState::Failed, Some(reason), &mut claims);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use block_junk_mod_api::{architecture::PlacementId, blocks::Cardinal};
    fn scene() -> (crate::blocks::BlockRegistry, crate::placements::Placements) {
        let defs=serde_json::from_value(serde_json::json!([{"id":"test:bench","display_name":"Bench","color":[1,1,1],"flags":{},"facility_actions":{"rest":{"kind":"timed","duration_secs":10}},"facility_slots":[
            {"key":"left","yaw":0,"approaches":[[0,0,1]],"exclusive":true,"interactions":["rest"]},
            {"key":"right","yaw":0,"approaches":[[0,0,-1]],"exclusive":true,"interactions":["rest"]},
            {"key":"shared","yaw":0,"approaches":[[1,0,0]],"exclusive":false,"interactions":["rest"]}
        ]}])).unwrap();
        let blocks = crate::blocks::BlockRegistry::build(defs).unwrap().0;
        let mut placements = crate::placements::Placements::default();
        placements
            .index_existing(
                IVec3::ZERO,
                blocks.require("test:bench"),
                Cardinal::East,
                &blocks,
                |_| Some(blocks.require("test:bench")),
            )
            .unwrap();
        (blocks, placements)
    }
    fn request<'a>(
        placement: &'a crate::placements::PlacementRecord,
        slot: &'static str,
        id: u64,
    ) -> InteractionRequest<'a> {
        InteractionRequest {
            placement,
            slot_key: slot,
            action: "rest",
            actor: ReservationOwner::Npc { id },
            controller: ReservationOwner::Behavior {
                mod_id: "test".into(),
                registration_id: "planner".into(),
            },
            cause: None,
            approach: match slot {
                "left" => IVec3::Z,
                "right" => IVec3::NEG_Z,
                _ => IVec3::X,
            },
        }
    }
    #[test]
    fn distinct_slots_share_a_placement_without_double_booking_and_stale_release_is_safe() {
        let (blocks, placements) = scene();
        let p = &placements.records[&PlacementId(1)];
        let mut jobs = FacilityInteractions::default();
        let mut claims = crate::interactables::InteractionClaims::default();
        let left = jobs
            .prepare(request(p, "left", 1), &blocks, &mut claims)
            .unwrap();
        assert_eq!(
            jobs.prepare(request(p, "left", 1), &blocks, &mut claims)
                .unwrap(),
            left
        );
        assert!(
            jobs.prepare(request(p, "left", 2), &blocks, &mut claims)
                .is_err()
        );
        let right = jobs
            .prepare(request(p, "right", 2), &blocks, &mut claims)
            .unwrap();
        assert_ne!(left.id, right.id);
        jobs.prepare(request(p, "shared", 3), &blocks, &mut claims)
            .unwrap();
        jobs.prepare(request(p, "shared", 4), &blocks, &mut claims)
            .unwrap();
        jobs.validate().unwrap();
        jobs.finish(left.id, JobState::Interrupted, None, &mut claims);
        let replacement = jobs
            .prepare(request(p, "left", 5), &blocks, &mut claims)
            .unwrap();
        assert!(!jobs.finish(left.id, JobState::Completed, None, &mut claims));
        jobs.validate_target(replacement.id, &placements, &blocks, &claims)
            .unwrap();
        let mut changed = placements.clone();
        changed.records.get_mut(&p.id).unwrap().revision += 1;
        assert!(
            jobs.validate_target(right.id, &changed, &blocks, &claims)
                .is_err()
        );
        jobs.disable_mod("test", "callback failed", &mut claims);
        assert!(jobs.jobs.records.values().all(|job| job.state.terminal()));
        assert!(!claims.is_taken_by_other(IVec3::ZERO, crate::npc::NpcId(99)));
    }
    #[test]
    fn reload_exits_a_snapped_actor_before_policy_without_losing_progress() {
        let (blocks, placements) = scene();
        let mut jobs = FacilityInteractions::default();
        let mut claims = crate::interactables::InteractionClaims::default();
        let job = jobs
            .prepare(
                request(&placements.records[&PlacementId(1)], "left", 1),
                &blocks,
                &mut claims,
            )
            .unwrap();
        let approach = IVec3::new(4, 1, 4);
        jobs.progress.get_mut(&job.id).unwrap().approach = approach.to_array();
        jobs.progress.get_mut(&job.id).unwrap().remaining_secs = 3.;
        let exit = jobs.loaded_occupancy(1).unwrap();
        jobs.suspend_for_load();
        let blocks = crate::blocks::BlockRegistry::build(
            serde_json::from_value(serde_json::json!([
                {"id":"test:floor","display_name":"Floor","flags":{"solid":true},"color":[1,1,1]}
            ]))
            .unwrap(),
        )
        .unwrap()
        .0;
        let mut chunk = crate::voxel::Chunk {
            revision: 0,
            blocks: vec![
                crate::blocks::BlockSlot::EMPTY;
                (crate::protocol::CHUNK_PADDED as usize).pow(3)
            ],
        };
        chunk.set(
            crate::voxel::world_to_chunk(approach - IVec3::Y).1,
            blocks.require("test:floor"),
        );
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(blocks)
            .insert_resource(jobs)
            .init_resource::<crate::interactables::InteractionClaims>()
            .init_resource::<crate::voxel::ChunkMap>()
            .add_systems(PreUpdate, reconcile_exits);
        let chunk = app.world_mut().spawn(chunk).id();
        app.world_mut()
            .resource_mut::<crate::voxel::ChunkMap>()
            .0
            .insert(crate::protocol::ChunkCoord(IVec3::ZERO), chunk);
        let actor = app
            .world_mut()
            .spawn((
                crate::npc::NpcId(1),
                crate::protocol::AvatarPose {
                    translation: Vec3::splat(8.),
                    yaw: 0.,
                },
                crate::npc::Brain {
                    goal: crate::npc::Goal::Idle,
                    rng: 1,
                    home_facility: None,
                    preempt_cooldown_secs: 0.,
                    policy_results: Vec::new(),
                    active_policy_job: None,
                    awaiting_construction: None,
                },
                exit,
                crate::protocol::KinematicLock,
            ))
            .id();
        app.update();
        assert_eq!(
            app.world()
                .get::<crate::protocol::AvatarPose>(actor)
                .unwrap()
                .translation,
            crate::physics::standing_pose_translation(approach)
        );
        assert!(
            app.world()
                .get::<crate::protocol::KinematicLock>(actor)
                .is_none()
        );
        assert!(app.world().get::<FacilityOccupancy>(actor).is_none());
        assert_eq!(
            app.world().resource::<FacilityInteractions>().progress[&job.id].remaining_secs,
            3.
        );
    }

    #[test]
    fn binary_reload_retains_progress_until_revalidated_and_resumes_it_only_once() {
        let (blocks, placements) = scene();
        let p = &placements.records[&PlacementId(1)];
        let mut jobs = FacilityInteractions::default();
        let mut claims = crate::interactables::InteractionClaims::default();
        let first = jobs
            .prepare(request(p, "left", 1), &blocks, &mut claims)
            .unwrap();
        jobs.working(first.id);
        jobs.progress.get_mut(&first.id).unwrap().remaining_secs = 3.;
        let bytes = bincode::serde::encode_to_vec(&jobs, bincode::config::standard()).unwrap();
        let (mut loaded, _): (FacilityInteractions, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        loaded.validate().unwrap();
        let exit = loaded.loaded_occupancy(1).unwrap();
        assert_eq!(exit.job_id, first.id);
        assert_eq!(
            exit.approach.to_array(),
            loaded.progress[&first.id].approach
        );
        assert!(loaded.loaded_occupancy(2).is_none());
        loaded.suspend_for_load();
        loaded.validate().unwrap();
        assert!(loaded.loaded_occupancy(1).is_none());
        let mut claims = crate::interactables::InteractionClaims::default();
        let resumed = loaded
            .prepare(request(p, "left", 1), &blocks, &mut claims)
            .unwrap();
        assert_eq!(loaded.progress[&resumed.id].remaining_secs, 3.);
        loaded.finish(resumed.id, JobState::Completed, None, &mut claims);
        let next = loaded
            .prepare(request(p, "left", 1), &blocks, &mut claims)
            .unwrap();
        assert_eq!(loaded.progress[&next.id].remaining_secs, 10.);
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct InteractionService<'w> {
    pub jobs: ResMut<'w, FacilityInteractions>,
    pub claims: ResMut<'w, crate::interactables::InteractionClaims>,
}
impl InteractionService<'_> {
    pub fn interrupt_actor(&mut self, id: u64, reason: &str) {
        self.jobs.interrupt_actor(id, reason, &mut self.claims);
    }
    pub fn disable_mod(&mut self, owner: &str, reason: &str) {
        self.jobs.disable_mod(owner, reason, &mut self.claims);
    }
}

/// Retain the original exit surface even if the furnishing is destroyed or the
/// callback is disabled before the goal can perform its normal cleanup.
#[derive(Component)]
pub struct FacilityOccupancy {
    pub job_id: u64,
    pub approach: IVec3,
}
type ExitActors<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static crate::npc::NpcId,
        &'static mut crate::protocol::AvatarPose,
        &'static mut crate::npc::Brain,
        &'static FacilityOccupancy,
        Has<crate::protocol::KinematicLock>,
        Has<crate::npc::BrainDisabled>,
    ),
>;
#[derive(bevy::ecs::system::SystemParam)]
pub struct ExitWorld<'w, 's> {
    chunks: Query<'w, 's, &'static crate::voxel::Chunk>,
    sidecars: Query<'w, 's, &'static crate::voxel::ChunkEntities>,
    map: Res<'w, crate::voxel::ChunkMap>,
    blocks: Res<'w, crate::blocks::BlockRegistry>,
    actors: ExitActors<'w, 's>,
}
pub fn reconcile_exits(
    mut world: ExitWorld,
    mut service: InteractionService,
    mut commands: Commands,
) {
    let walk = crate::npc::WorldWalk {
        chunks: &world.chunks,
        sidecars: &world.sidecars,
        chunk_map: &world.map,
        registry: &world.blocks,
    };
    for (entity, id, mut pose, mut brain, occupancy, locked, disabled) in &mut world.actors {
        let active = service
            .jobs
            .jobs
            .records
            .get(&occupancy.job_id)
            .is_some_and(|job| !job.state.terminal());
        let using = matches!(
            brain.goal,
            crate::npc::Goal::Interacting { .. }
                | crate::npc::Goal::MoveTo {
                    on_arrive: crate::npc::ArrivalAction::Interact { .. },
                    ..
                }
        );
        if active && using && !disabled {
            continue;
        }
        if active {
            service.jobs.finish(
                occupancy.job_id,
                JobState::Interrupted,
                Some("interaction goal ended".into()),
                &mut service.claims,
            );
        }
        if using {
            brain.goal = crate::npc::Goal::Idle;
        }
        if locked {
            let candidates = std::iter::once(occupancy.approach).chain(
                [IVec3::X, IVec3::NEG_X, IVec3::Z, IVec3::NEG_Z, IVec3::Y]
                    .into_iter()
                    .map(|offset| occupancy.approach + offset),
            );
            if !crate::npc::try_eject_to_cells(&mut pose, candidates, &walk) {
                warn!(
                    npc = id.0,
                    "facility exit blocked; releasing body for normal collision recovery"
                );
            }
        }
        commands
            .entity(entity)
            .remove::<(FacilityOccupancy, crate::protocol::KinematicLock)>();
    }
}

pub fn valid_actor_pose(
    slot: &FacilitySlot,
    anchor: IVec3,
    approach: IVec3,
    pose: &crate::protocol::AvatarPose,
) -> bool {
    let expected = slot.pose.map_or_else(
        || crate::physics::standing_pose_translation(approach),
        |offset| {
            anchor.as_vec3()
                + Vec3::new(
                    0.5,
                    crate::physics::EYE_OFFSET_FROM_CENTRE + crate::physics::PLAYER_HALF_EXTENTS.y,
                    0.5,
                )
                + Vec3::from_array(offset)
        },
    );
    pose.translation.is_finite() && pose.translation.distance(expected) <= 0.85
}

/// Timed work advances here for every caller. The brain only travels, displays
/// the mirrored activity and reacts to the authoritative terminal result.
pub fn advance(
    time: Res<Time>,
    mut jobs: ResMut<FacilityInteractions>,
    mut claims: ResMut<crate::interactables::InteractionClaims>,
    mut world: ActivationWorld,
    mut completed: MessageWriter<InteractionCompleted>,
) {
    let dt = time.delta_secs();
    if !dt.is_finite() || dt <= 0. {
        return;
    }
    let ids: Vec<_> = jobs
        .jobs
        .records
        .values()
        .filter(|job| job.state == JobState::Working)
        .map(|job| job.id)
        .collect();
    let walk = crate::npc::WorldWalk {
        chunks: &world.chunks,
        sidecars: &world.sidecars,
        chunk_map: &world.map,
        registry: &world.blocks,
    };
    for id in ids {
        if jobs.progress.get(&id).is_some_and(|p| p.completion_queued) {
            continue;
        }
        let outcome = (|| -> Result<(), String> {
            let job = &jobs.jobs.records[&id];
            let ReservationOwner::Npc { id: actor } = job.actor else {
                return Err("interaction actor invalid".into());
            };
            let (slot, _) = jobs.validate_target(id, &world.placements, &world.blocks, &claims)?;
            let JobTarget::Interaction { slot: slot_id, .. } = &job.target else {
                unreachable!()
            };
            let anchor = IVec3::from_array(world.placements.records[&slot_id.placement].anchor);
            let approach = IVec3::from_array(jobs.progress[&id].approach);
            let (_, _, mut brain, pose) = world
                .actors
                .iter_mut()
                .find(|(_, npc, _, _)| npc.0 == actor)
                .ok_or("interaction actor missing or disabled")?;
            let crate::npc::Goal::Interacting { remaining_secs, .. } = &mut brain.goal else {
                return Err("interaction interrupted".into());
            };
            if !crate::pathfinding::standable(&walk, approach)
                || !valid_actor_pose(slot, anchor, approach, &pose)
            {
                return Err("interaction approach blocked or actor displaced".into());
            }
            let progress = jobs.progress.get_mut(&id).expect("active progress");
            progress.remaining_secs = (progress.remaining_secs - dt).max(0.);
            *remaining_secs = progress.remaining_secs;
            if progress.remaining_secs == 0. {
                progress.completion_queued = true;
                completed.write(InteractionCompleted {
                    job_id: id,
                    actor: crate::npc::NpcId(actor),
                });
            }
            Ok(())
        })();
        if let Err(reason) = outcome {
            jobs.finish(id, JobState::Interrupted, Some(reason), &mut claims);
        }
    }
}
