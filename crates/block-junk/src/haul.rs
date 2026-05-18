//! Server-only reservation + assignment tables for NPC hauling.
//!
//! Two paired resources, mirrored after [`crate::plan_claims::PlanClaims`]:
//!
//! - [`WorldItemReservations`] — which `WorldItem` entity has been
//!   reserved by which NPC. Prevents two NPCs (or the scheduler in two
//!   consecutive ticks) from queuing the same loose item for delivery.
//! - [`HaulAssignments`] — per-NPC "you are delivering these items to
//!   this plan." The brain reads its own assignment on each leg of a
//!   haul cycle to pick the next destination.
//!
//! Neither survives save/load. NPCs reset to `Goal::Idle` on load and
//! the scheduler re-pairs from scratch — reservations stale-released
//! implicitly when the assignment map empties.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::items::ItemSlot;
use crate::npc::NpcId;

/// Reservation table for [`crate::protocol::WorldItem`] entities.
/// Keyed by entity (the loose-item entity itself, not a cell) since
/// items don't live on the 1m grid and several may share a cell.
#[derive(Resource, Default, Debug)]
pub struct WorldItemReservations {
    by_entity: HashMap<Entity, NpcId>,
}

impl WorldItemReservations {
    /// Try to reserve `entity` for `npc`. Succeeds if the slot is free
    /// or already held by the same NPC (re-reserve is idempotent — the
    /// scheduler may re-call when re-evaluating an existing assignment).
    pub fn try_reserve(&mut self, entity: Entity, npc: NpcId) -> bool {
        match self.by_entity.get(&entity) {
            Some(holder) if holder.0 == npc.0 => true,
            Some(_) => false,
            None => {
                self.by_entity.insert(entity, npc);
                true
            }
        }
    }

    /// Release `entity`'s reservation if `npc` holds it. Releasing a
    /// reservation not held by `npc` is silently a no-op — mirrors
    /// [`crate::plan_claims::PlanClaims::release`].
    pub fn release(&mut self, entity: Entity, npc: NpcId) {
        if let Some(holder) = self.by_entity.get(&entity)
            && holder.0 == npc.0
        {
            self.by_entity.remove(&entity);
        }
    }

    /// Drop every reservation held by `npc`. Called on NPC despawn /
    /// haul abandon so a single NPC can't permanently lock items by
    /// failing in some unanticipated way.
    pub fn release_all_for(&mut self, npc: NpcId) {
        self.by_entity.retain(|_, holder| holder.0 != npc.0);
    }

    /// True if `entity` is currently reserved by anyone other than
    /// `npc`. Used by the scheduler to filter "available items" without
    /// taking the reservation — taking happens later, atomically, when
    /// the scheduler commits an assignment.
    pub fn is_taken_by_other(&self, entity: Entity, npc: NpcId) -> bool {
        match self.by_entity.get(&entity) {
            Some(holder) => holder.0 != npc.0,
            None => false,
        }
    }
}

/// One loose item the scheduler has earmarked for delivery to a plan.
/// Caches `item` + `translation` so the brain doesn't need to query the
/// (possibly already-despawned) `WorldItem` entity on every leg — the
/// cached fields are also the fallback used when the entity is gone by
/// the time the NPC arrives.
#[derive(Clone, Copy, Debug)]
pub struct ReservedItem {
    pub entity: Entity,
    pub item: ItemSlot,
    pub translation: Vec3,
}

/// What one NPC is currently hauling. `plan_cell` is the Build plan
/// being filled; `queue` is the remaining items the scheduler has
/// reserved for this run, in pickup order (front first). After every
/// pickup the brain pops the front; when the queue empties the brain
/// walks to the plan to deposit, and on deposit the assignment is
/// released (the scheduler will hand out a fresh one next tick if the
/// plan still has unmet materials).
#[derive(Clone, Debug)]
pub struct HaulAssignment {
    pub plan_cell: IVec3,
    pub queue: Vec<ReservedItem>,
}

/// Per-NPC assignment map. An NPC with an entry here is being driven
/// by the engine's haul scheduler — the Lua planner is bypassed for
/// the duration. Mirrors [`crate::plan_claims::PlanClaims`] in shape.
#[derive(Resource, Default, Debug)]
pub struct HaulAssignments {
    by_npc: HashMap<NpcId, HaulAssignment>,
}

impl HaulAssignments {
    pub fn get(&self, npc: NpcId) -> Option<&HaulAssignment> {
        self.by_npc.get(&npc)
    }

    pub fn get_mut(&mut self, npc: NpcId) -> Option<&mut HaulAssignment> {
        self.by_npc.get_mut(&npc)
    }

    pub fn insert(&mut self, npc: NpcId, assignment: HaulAssignment) {
        self.by_npc.insert(npc, assignment);
    }

    pub fn remove(&mut self, npc: NpcId) -> Option<HaulAssignment> {
        self.by_npc.remove(&npc)
    }

    pub fn contains(&self, npc: NpcId) -> bool {
        self.by_npc.contains_key(&npc)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NpcId, &HaulAssignment)> {
        self.by_npc.iter()
    }
}

/// Atomically drop `npc`'s assignment and release every item it had
/// reserved. The two resources are coupled — an assignment without
/// reservations is meaningless, and orphan reservations would leak
/// items out of the scheduler's pool — so cleanup paths should always
/// call this rather than touching either resource alone.
pub fn release_haul_for(
    npc: NpcId,
    assignments: &mut HaulAssignments,
    reservations: &mut WorldItemReservations,
) {
    if let Some(assignment) = assignments.remove(npc) {
        for item in assignment.queue {
            reservations.release(item.entity, npc);
        }
    }
    // Belt-and-braces: even if the assignment is gone for some other
    // reason (manual mutation, future scheduler path), make sure no
    // stray reservation outlives the assignment.
    reservations.release_all_for(npc);
}

/// Max Chebyshev distance (cells) from an NPC's foot to a plan the
/// scheduler will commit to. Same magnitude as the planner's
/// `SNAPSHOT_PLAN_RADIUS_CELLS` so a hauler doesn't cross-map for one
/// distant build while leaving local items unhauled.
const MAX_HAUL_PLAN_RADIUS_CELLS: i32 = 48;
/// Max euclidean distance (m) from an NPC to a loose item the
/// scheduler will reserve for that NPC. Looser than the plan radius
/// because the NPC walks to the plan after picking up — distant items
/// for a nearby plan are still fine (the cost is the extra walk leg).
const MAX_HAUL_ITEM_RADIUS_M: f32 = 64.0;

/// Per-NPC matchmaker: try to find an unsatisfied Build plan + nearby
/// unreserved [`WorldItem`]s that this single NPC can haul. Returns
/// `true` when an assignment was inserted (the caller then dispatches
/// the first leg); `false` when no viable pairing exists this tick.
///
/// Called from inside the brain tick's Idle-entry branch (NOT a
/// standalone system) because the brain tick is monolithic — an NPC
/// transitions `Working/MoveTo` → `Idle` → `Wander` (or whatever the
/// planner returns) in one iteration. A standalone scheduler in
/// `Update` only ever sees the post-planner goal and would assign
/// nothing. Running per-NPC at the Idle moment is the only place
/// where `Goal::Idle` is observable.
///
/// Greedy. No global optimisation — the goal is "every NPC has
/// something useful to do," not "minimise total haul distance."
pub fn try_schedule_haul_for_npc(
    npc_id: NpcId,
    npc_kind: &str,
    pose: Vec3,
    carrying_is_empty: bool,
    kind_registry: &crate::npc_registry::NpcKindRegistry,
    plans: &crate::plans::Plans,
    world_items: &Query<(Entity, &crate::protocol::WorldItem)>,
    assignments: &mut HaulAssignments,
    reservations: &mut WorldItemReservations,
) -> bool {
    use crate::protocol::PlanKind;

    if assignments.contains(npc_id) {
        return false;
    }
    if !carrying_is_empty {
        // Non-empty carry from a previous assignment or a player
        // hand-off — scheduler doesn't hijack. The brain's planner
        // gets to dispose of it (wander, etc.).
        return false;
    }
    let cap = kind_registry
        .get(npc_kind)
        .map(|d| d.carry_capacity)
        .unwrap_or(3);
    if cap == 0 {
        return false;
    }
    let foot = IVec3::new(
        pose.x.floor() as i32,
        pose.y.floor() as i32,
        pose.z.floor() as i32,
    );

    // Index every loose item by ItemSlot once per call. The pool is
    // shared across this NPC's plan scan + final reservation pass;
    // per-NPC rebuild is O(items) and items are sparse.
    let mut items_by_slot: std::collections::HashMap<
        crate::items::ItemSlot,
        Vec<(Entity, Vec3)>,
    > = std::collections::HashMap::new();
    for (entity, wi) in world_items.iter() {
        items_by_slot
            .entry(wi.item)
            .or_default()
            .push((entity, wi.translation));
    }

    // Pick the nearest unsatisfied Build plan that has at least one
    // reachable matching item.
    let mut best_plan: Option<(IVec3, i32)> = None;
    for (cell, state) in plans.iter() {
        if !matches!(state.kind, PlanKind::Build { .. }) {
            continue;
        }
        if state.is_satisfied() {
            continue;
        }
        let dist = (cell.x - foot.x)
            .abs()
            .max((cell.y - foot.y).abs())
            .max((cell.z - foot.z).abs());
        if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
            continue;
        }
        let has_matchable = state.materials.iter().any(|m| {
            if m.needed <= m.present {
                return false;
            }
            let Some(pool) = items_by_slot.get(&m.item) else {
                return false;
            };
            pool.iter().any(|(entity, translation)| {
                if reservations.is_taken_by_other(*entity, npc_id) {
                    return false;
                }
                let d = (*translation - pose).length();
                d <= MAX_HAUL_ITEM_RADIUS_M
            })
        });
        if !has_matchable {
            continue;
        }
        if best_plan.map(|(_, d)| dist < d).unwrap_or(true) {
            best_plan = Some((*cell, dist));
        }
    }
    let Some((plan_cell, _)) = best_plan else {
        return false;
    };
    let Some(state) = plans.get(plan_cell) else {
        return false;
    };

    // Single-stack carry — every queue entry must be the same ItemSlot.
    // Pick the most-needed remaining kind; ties to iteration order.
    let mut chosen_kind: Option<crate::items::ItemSlot> = None;
    let mut remaining_for_kind: u32 = 0;
    for m in &state.materials {
        let remaining = m.needed.saturating_sub(m.present);
        if remaining == 0 {
            continue;
        }
        if remaining > remaining_for_kind {
            chosen_kind = Some(m.item);
            remaining_for_kind = remaining;
        }
    }
    let Some(item_slot) = chosen_kind else {
        return false;
    };
    let Some(pool) = items_by_slot.get(&item_slot) else {
        return false;
    };

    // Sort by distance so closest items get reserved first.
    let mut candidates: Vec<(Entity, Vec3, f32)> = pool
        .iter()
        .filter_map(|(entity, translation)| {
            if reservations.is_taken_by_other(*entity, npc_id) {
                return None;
            }
            let d = (*translation - pose).length();
            if d > MAX_HAUL_ITEM_RADIUS_M {
                return None;
            }
            Some((*entity, *translation, d))
        })
        .collect();
    candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

    let want = (cap as usize).min(remaining_for_kind as usize);
    let mut queue: Vec<ReservedItem> = Vec::with_capacity(want);
    for (entity, translation, _) in candidates {
        if queue.len() >= want {
            break;
        }
        if reservations.try_reserve(entity, npc_id) {
            queue.push(ReservedItem {
                entity,
                item: item_slot,
                translation,
            });
        }
    }
    if queue.is_empty() {
        return false;
    }
    info!(
        npc = npc_id.0,
        plan = ?plan_cell.to_array(),
        kind = item_slot.0,
        queued = queue.len(),
        "haul assignment committed",
    );
    assignments.insert(
        npc_id,
        HaulAssignment {
            plan_cell,
            queue,
        },
    );
    true
}

pub struct HaulPlugin;

impl Plugin for HaulPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldItemReservations>();
        app.init_resource::<HaulAssignments>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPC_1: NpcId = NpcId(1);
    const NPC_2: NpcId = NpcId(2);

    fn entity(id: u32) -> Entity {
        Entity::from_raw_u32(id).expect("nonzero entity id")
    }

    #[test]
    fn first_reserve_succeeds_second_fails() {
        let mut r = WorldItemReservations::default();
        let e = entity(1);
        assert!(r.try_reserve(e, NPC_1));
        assert!(!r.try_reserve(e, NPC_2));
    }

    #[test]
    fn reserve_by_same_npc_is_idempotent() {
        let mut r = WorldItemReservations::default();
        let e = entity(1);
        assert!(r.try_reserve(e, NPC_1));
        assert!(r.try_reserve(e, NPC_1));
    }

    #[test]
    fn release_frees_for_other() {
        let mut r = WorldItemReservations::default();
        let e = entity(1);
        r.try_reserve(e, NPC_1);
        r.release(e, NPC_1);
        assert!(r.try_reserve(e, NPC_2));
    }

    #[test]
    fn release_by_non_owner_is_no_op() {
        let mut r = WorldItemReservations::default();
        let e = entity(1);
        r.try_reserve(e, NPC_1);
        r.release(e, NPC_2);
        assert!(!r.try_reserve(e, NPC_2));
        assert!(r.try_reserve(e, NPC_1));
    }

    #[test]
    fn release_all_for_drops_only_that_npc() {
        let mut r = WorldItemReservations::default();
        let a = entity(1);
        let b = entity(2);
        r.try_reserve(a, NPC_1);
        r.try_reserve(b, NPC_2);
        r.release_all_for(NPC_1);
        assert!(r.try_reserve(a, NPC_2));
        assert!(!r.try_reserve(b, NPC_1));
    }

    #[test]
    fn release_haul_for_clears_both_resources() {
        let mut assignments = HaulAssignments::default();
        let mut reservations = WorldItemReservations::default();
        let a = entity(1);
        let b = entity(2);
        reservations.try_reserve(a, NPC_1);
        reservations.try_reserve(b, NPC_1);
        assignments.insert(
            NPC_1,
            HaulAssignment {
                plan_cell: IVec3::ZERO,
                queue: vec![
                    ReservedItem {
                        entity: a,
                        item: ItemSlot(0),
                        translation: Vec3::ZERO,
                    },
                    ReservedItem {
                        entity: b,
                        item: ItemSlot(0),
                        translation: Vec3::ZERO,
                    },
                ],
            },
        );

        release_haul_for(NPC_1, &mut assignments, &mut reservations);

        assert!(!assignments.contains(NPC_1));
        // Both reservations are now free for anyone else.
        assert!(reservations.try_reserve(a, NPC_2));
        assert!(reservations.try_reserve(b, NPC_2));
    }
}
