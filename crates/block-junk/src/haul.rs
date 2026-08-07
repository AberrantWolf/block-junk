//! Server-only state for NPC hauling. Holds two coupled tables:
//!
//! - The per-NPC assignment map — what each NPC is currently
//!   delivering, where, and what tool/items they've reserved for it.
//! - The reservation table — which `WorldItem` entity has been
//!   earmarked by which NPC.
//!
//! These two MUST stay coupled: a reservation without an owning
//! assignment is a leak (the item stays locked, no one can claim it),
//! and an assignment without its reservations is broken (the brain
//! walks to a freed item that another NPC may grab first). The
//! previous design exposed them as two separate `Resource`s and
//! relied on a `release_haul_for` helper to clear them together —
//! convention rather than enforcement. [`HaulStore`] now wraps both
//! behind a private API so the only way to mutate them is through
//! methods that maintain the coupling invariant.
//!
//! Neither survives save/load. NPCs reset to `Goal::Idle` on load and
//! the scheduler re-pairs from scratch.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::items::ItemSlot;
use crate::npc::NpcId;

/// Where a reserved batch of units comes from. Ground items (loose
/// drops + tidied piles) are `WorldItem` entities; container stock (S3)
/// is a count inside the container block at a cell — no entity exists
/// until the units come back out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HaulSource {
    /// A loose [`crate::protocol::WorldItem`] entity (single drop or
    /// pile).
    Ground(Entity),
    /// Stock inside the container block at this cell.
    Container(IVec3),
}

/// One item source the scheduler has earmarked for a haul run. Caches
/// `item` + `translation` so the brain doesn't need to query the
/// (possibly already-despawned) source on every leg — the cached
/// fields are also the fallback used when the source is gone by the
/// time the NPC arrives.
#[derive(Clone, Copy, Debug)]
pub struct ReservedItem {
    pub source: HaulSource,
    pub item: ItemSlot,
    pub translation: Vec3,
    /// How many units to withdraw from this source on the pickup leg —
    /// `min(target's remaining need, carry room, what's there)`. Loose
    /// items reserve `1`. The whole source is reserved regardless of
    /// `count` (one NPC works a stack/container at a time).
    pub count: u32,
}

impl ReservedItem {
    /// The `WorldItem` entity behind a ground reservation; `None` for
    /// container stock. Tool reservations are always ground items.
    pub fn ground_entity(&self) -> Option<Entity> {
        match self.source {
            HaulSource::Ground(e) => Some(e),
            HaulSource::Container(_) => None,
        }
    }
}

/// Where the next deposit leg of a haul cycle delivers materials.
/// Build plans and craft stations are both single-cell deposit
/// targets the scheduler can route to; the brain picks the right
/// arrival action ([`crate::npc::ArrivalAction::DepositAtPlan`] vs
/// [`crate::npc::ArrivalAction::DepositAtStation`]) from this.
///
/// Tool prereqs only apply to Plan targets (the *work* needs a tool;
/// delivery doesn't). Station haul never carries a `pending_tool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HaulTarget {
    Plan(IVec3),
    Station(IVec3),
    /// A designated storage cell the NPC is tidying loose items into
    /// (S2). The deposit merges the carry into a pile at this cell.
    Storage(IVec3),
    /// A container block the NPC is tidying loose items into (S3).
    /// The deposit adds the carry to the container's stock.
    Container(IVec3),
}

impl HaulTarget {
    /// The world cell of the deposit target — useful when the caller
    /// only needs the location (distance check, path target), not the
    /// kind.
    pub fn cell(&self) -> IVec3 {
        match self {
            HaulTarget::Plan(c)
            | HaulTarget::Station(c)
            | HaulTarget::Storage(c)
            | HaulTarget::Container(c) => *c,
        }
    }
}

/// What one NPC is currently hauling. `target` is the deposit
/// destination (Build plan or craft station); `queue` is the
/// remaining items the scheduler has reserved for this run, in
/// pickup order (front first). After every pickup the brain pops the
/// front; when the queue empties the brain walks to the target to
/// deposit, and on deposit the assignment is released (the scheduler
/// will hand out a fresh one next tick if the target still has unmet
/// demand).
///
/// `pending_tool` covers Phase 5b's "fetch the right tool first"
/// branch — when the scheduler picks an NPC for a plan whose
/// `work_action.required_tool` isn't satisfied by the NPC's
/// `EquippedTool`, it also reserves a matching tool nearby and
/// records it here. The brain walks to the tool first
/// (`pick_next_haul_leg` checks this field before anything else),
/// equips it via swap, clears the field, then continues with the
/// material queue. `None` ⇒ no tool prereq. Station targets always
/// carry `None` here — recipe tool gates are enforced when the
/// engine schedules the *work*, not the haul.
#[derive(Clone, Debug)]
pub struct HaulAssignment {
    pub target: HaulTarget,
    pub queue: Vec<ReservedItem>,
    pub pending_tool: Option<ReservedItem>,
}

/// Server-only paired state for NPC hauling. Wraps the assignment
/// map and the world-item reservation table behind a private API so
/// the two can't drift out of sync — the previous version exposed
/// them as two `Resource`s and relied on a `release_haul_for` helper
/// being called by every cleanup path. With this wrapper there is no
/// way to release one without releasing the other.
///
/// The scheduler (in this module) has full access to the inner
/// tables via `pub(super)` getters because its commit flow is
/// atomic-on-success-with-rollback and needs to reserve items
/// one-by-one before assembling the final assignment. External
/// callers (the brain tick in npc.rs) only see the safe public API.
#[derive(Resource, Default, Debug)]
pub struct HaulStore {
    assignments: HashMap<NpcId, HaulAssignment>,
    reservations: HashMap<Entity, NpcId>,
    /// Container cells earmarked for withdrawal, keyed whole-cell (one
    /// NPC works a container at a time — same coarseness as the
    /// whole-stack rule for piles, fine at co-op scale). Deposits
    /// don't reserve: adding stock can't invalidate anyone.
    container_reservations: HashMap<IVec3, NpcId>,
    /// Targets whose last haul leg failed pathfinding, mapped to the
    /// `Time::elapsed_secs` timestamp when a retry is allowed. The
    /// scheduler skips memoized targets so one walled-off plan (or an
    /// item in a pit) can't pin its nearest hauler in a deterministic
    /// assign→path-fail→release loop forever. Entries expire lazily on
    /// check; the world edit that makes a target reachable again is
    /// picked up on the first retry after expiry.
    unreachable_until: HashMap<HaulTarget, f32>,
}

impl HaulStore {
    /// Read the assignment for `npc`, or `None` if they don't have
    /// one. The brain tick calls this on every tick to know whether
    /// to plan a haul leg or fall through to the planner.
    pub fn assignment_of(&self, npc: NpcId) -> Option<&HaulAssignment> {
        self.assignments.get(&npc)
    }

    /// True if `npc` has an active assignment.
    pub fn has_assignment(&self, npc: NpcId) -> bool {
        self.assignments.contains_key(&npc)
    }

    /// True if `source` is currently reserved by anyone other than
    /// `npc`. Used by the scheduler + arrival handlers to detect
    /// races against another NPC.
    pub fn reservation_taken_by_other(&self, source: HaulSource, npc: NpcId) -> bool {
        let holder = match source {
            HaulSource::Ground(entity) => self.reservations.get(&entity),
            HaulSource::Container(cell) => self.container_reservations.get(&cell),
        };
        match holder {
            Some(holder) => holder.0 != npc.0,
            None => false,
        }
    }

    /// Atomically clear `npc`'s assignment and release every
    /// reservation it held. Every cleanup path on the brain side
    /// calls this — abandon-on-stuck, planner-error, NPC-despawn,
    /// preempt-on-need. The two-table coupling is enforced here.
    /// `retain` belt-and-braces catches any stray
    /// reservations not tracked by the assignment (shouldn't happen,
    /// but cheap to defend).
    pub fn release_for_npc(&mut self, npc: NpcId) {
        if let Some(assignment) = self.assignments.remove(&npc) {
            for item in &assignment.queue {
                self.release_reservation_internal(item.source, npc);
            }
            if let Some(tool) = &assignment.pending_tool {
                self.release_reservation_internal(tool.source, npc);
            }
        }
        self.reservations.retain(|_, holder| holder.0 != npc.0);
        self.container_reservations
            .retain(|_, holder| holder.0 != npc.0);
    }

    /// Record that pathing toward `target` just failed; the scheduler
    /// won't offer it again until `retry_at` (elapsed seconds).
    pub fn memo_unreachable(&mut self, target: HaulTarget, retry_at: f32) {
        self.unreachable_until.insert(target, retry_at);
    }

    /// Read-only variant of [`Self::is_unreachable`] for snapshot
    /// building, which only holds `&HaulStore`. Skips the lazy prune —
    /// the scheduler's mutable checks keep the table from growing.
    pub fn is_unreachable_peek(&self, target: HaulTarget, now: f32) -> bool {
        self.unreachable_until
            .get(&target)
            .is_some_and(|&retry_at| now < retry_at)
    }

    /// True while `target` is memoized as unreachable. Expired entries
    /// are pruned on check.
    pub fn is_unreachable(&mut self, target: HaulTarget, now: f32) -> bool {
        match self.unreachable_until.get(&target) {
            Some(&retry_at) if now < retry_at => true,
            Some(_) => {
                self.unreachable_until.remove(&target);
                false
            }
            None => false,
        }
    }

    /// Remove the queued reservation matching `source` (NPC may
    /// arrive at a source that's no longer what the scheduler saw due
    /// to a race; the brain calls this to skip past stale entries).
    pub fn drop_queue_entry(&mut self, npc: NpcId, source: HaulSource) {
        if let Some(assignment) = self.assignments.get_mut(&npc) {
            assignment.queue.retain(|r| r.source != source);
        }
        // Release reservation regardless of whether queue had it (a
        // canonical "remove this stale ref" call).
        self.release_reservation_internal(source, npc);
    }

    /// Clear `npc`'s pending_tool slot and release its reservation.
    /// Called from the PickupTool arrival once the tool is equipped.
    pub fn clear_pending_tool(&mut self, npc: NpcId) {
        if let Some(assignment) = self.assignments.get_mut(&npc)
            && let Some(tool) = assignment.pending_tool.take()
        {
            self.release_reservation_internal(tool.source, npc);
        }
    }

    // --- internals; only the scheduler in this module reaches in. ---

    pub(super) fn try_reserve_internal(&mut self, source: HaulSource, npc: NpcId) -> bool {
        match source {
            HaulSource::Ground(entity) => match self.reservations.get(&entity) {
                Some(holder) if holder.0 == npc.0 => true,
                Some(_) => false,
                None => {
                    self.reservations.insert(entity, npc);
                    true
                }
            },
            HaulSource::Container(cell) => match self.container_reservations.get(&cell) {
                Some(holder) if holder.0 == npc.0 => true,
                Some(_) => false,
                None => {
                    self.container_reservations.insert(cell, npc);
                    true
                }
            },
        }
    }

    pub(super) fn release_reservation_internal(&mut self, source: HaulSource, npc: NpcId) {
        match source {
            HaulSource::Ground(entity) => {
                if let Some(holder) = self.reservations.get(&entity)
                    && holder.0 == npc.0
                {
                    self.reservations.remove(&entity);
                }
            }
            HaulSource::Container(cell) => {
                if let Some(holder) = self.container_reservations.get(&cell)
                    && holder.0 == npc.0
                {
                    self.container_reservations.remove(&cell);
                }
            }
        }
    }

    /// Commit an assignment for `npc`. The caller (the scheduler)
    /// must have already called `try_reserve_internal` for every
    /// entity referenced by `assignment.queue` and `pending_tool`
    /// before calling this — the reservations are already in the
    /// table by the time we register the owner. If the caller bails
    /// before committing, it's responsible for releasing each
    /// individual reservation via `release_reservation_internal`.
    pub(super) fn commit_assignment(&mut self, npc: NpcId, assignment: HaulAssignment) {
        self.assignments.insert(npc, assignment);
    }
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
#[allow(
    clippy::too_many_arguments,
    reason = "scheduler reaches into many subsystems"
)]
pub fn try_schedule_haul_for_npc(
    npc_id: NpcId,
    npc_kind: &str,
    pose: Vec3,
    carrying: &crate::protocol::Carrying,
    equipped_tool: &crate::protocol::EquippedTool,
    kind_registry: &crate::npc_registry::NpcKindRegistry,
    plans: &crate::plans::Plans,
    stations: &crate::craft_stations::CraftStations,
    block_registry: &crate::blocks::BlockRegistry,
    item_registry: &crate::items::ItemRegistry,
    recipes: &crate::recipes::RecipeRegistry,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
    world_items: &Query<(Entity, &crate::protocol::WorldItem)>,
    storage: &crate::storage::StorageZones,
    containers: &crate::containers::Containers,
    container_index: &crate::containers::ContainerIndex,
    store: &mut HaulStore,
    now_secs: f32,
) -> bool {
    use crate::protocol::PlanKind;

    if store.has_assignment(npc_id) {
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

    // Non-empty carry → deposit-only path. Find the nearest target
    // (Build plan or craft station) that wants exactly this kind and
    // create an assignment with an empty queue; `pick_next_haul_leg`
    // then routes the NPC straight to deposit. Covers the save/load
    // case (carry persists; the pre-save haul assignment doesn't),
    // post-craft "I came home holding the output" scenarios, and any
    // other "NPC was handed an item and now needs somewhere to put it"
    // situation. If no matching target exists, the NPC falls through
    // to the planner (wanders carrying the stack); the next time a
    // matching target appears, this branch picks them up.
    if let (Some(carried_slot), c) = (carrying.item, carrying.count)
        && c > 0
    {
        let mut best: Option<(HaulTarget, i32)> = None;
        for (cell, state) in plans.iter() {
            if !matches!(state.kind, PlanKind::Build { .. }) {
                continue;
            }
            if state.is_satisfied() {
                continue;
            }
            let wants_it = state
                .materials
                .iter()
                .any(|m| m.item == carried_slot && m.needed > m.present);
            if !wants_it {
                continue;
            }
            let dist = chebyshev(*cell, foot);
            if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
                continue;
            }
            if store.is_unreachable(HaulTarget::Plan(*cell), now_secs) {
                continue;
            }
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((HaulTarget::Plan(*cell), dist));
            }
        }
        for (cell, state) in stations.iter() {
            let demand = compute_station_demand(state, recipes, item_registry);
            if demand.get(&carried_slot).copied().unwrap_or(0) == 0 {
                continue;
            }
            let dist = chebyshev(*cell, foot);
            if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
                continue;
            }
            if store.is_unreachable(HaulTarget::Station(*cell), now_secs) {
                continue;
            }
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((HaulTarget::Station(*cell), dist));
            }
        }
        let Some((target, _)) = best else {
            return false;
        };
        info!(
            npc = npc_id.0,
            target = ?target,
            kind = carried_slot.0,
            carry = c,
            "haul assignment (deposit-only) committed",
        );
        store.commit_assignment(
            npc_id,
            HaulAssignment {
                target,
                queue: Vec::new(),
                pending_tool: None,
            },
        );
        return true;
    }

    // Index every available item source by ItemSlot once per call. The
    // pool is shared across this NPC's plan scan + final reservation
    // pass; per-NPC rebuild is O(items + stocked containers) and both
    // are sparse. Each pool entry is (source, position, unit count) —
    // a pile or a container's stock of one kind is a single
    // reservation that yields multiple units. Container stock counts
    // as a source unconditionally (the `accepts` filter gates what
    // goes IN, not what comes out) — without this, stocked containers
    // would deadlock the build/craft economy.
    let mut items_by_slot: std::collections::HashMap<
        crate::items::ItemSlot,
        Vec<(HaulSource, Vec3, u32)>,
    > = std::collections::HashMap::new();
    for (entity, wi) in world_items.iter() {
        items_by_slot.entry(wi.item).or_default().push((
            HaulSource::Ground(entity),
            wi.translation,
            wi.count,
        ));
    }
    for (cell, state) in containers.iter() {
        let centre = cell.as_vec3() + Vec3::splat(0.5);
        for (slot, count) in state.inventory.iter() {
            items_by_slot.entry(*slot).or_default().push((
                HaulSource::Container(*cell),
                centre,
                *count,
            ));
        }
    }

    // Pick the nearest viable target — Build plan or craft station.
    // A target is viable when (a) it has unmet demand for at least one
    // item slot AND there's a reachable matching item nearby, AND (b)
    // for Plan targets only, the plan is workable by this NPC (the
    // NPC has the required tool or a matching one is reachable).
    // Stations have no tool gate at the haul layer — recipe tool
    // requirements are enforced when the *work* is scheduled.
    //
    // For each candidate, also pick the input slot we'd actually fetch
    // (single-stack carry forces one kind per assignment). Picking the
    // slot at target-selection time means the "matchable items" check
    // and the later reservation pass agree on the same kind.
    let mut best: Option<(HaulTarget, crate::items::ItemSlot, u32, i32)> = None;
    // best = (target, kind to fetch, remaining count needed, chebyshev distance)
    for (cell, state) in plans.iter() {
        if !matches!(state.kind, PlanKind::Build { .. }) {
            continue;
        }
        if state.is_satisfied() {
            continue;
        }
        let dist = chebyshev(*cell, foot);
        if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
            continue;
        }
        let chosen = pick_haul_kind(
            state
                .materials
                .iter()
                .map(|m| (m.item, m.needed.saturating_sub(m.present))),
            &items_by_slot,
            pose,
            npc_id,
            store,
        );
        let Some((slot, remaining)) = chosen else {
            continue;
        };
        // Tool gate: either no tool needed, NPC has it, or one is
        // available to fetch. `required_tool_for_plan` reads the
        // live block for Remove plans, the planned block for Build.
        let required =
            required_tool_for_plan(*cell, &state.kind, block_registry, chunks, chunk_map);
        if let Some(tag) = &required {
            let npc_satisfies = item_registry.tool_has_tag(equipped_tool.item, tag);
            if !npc_satisfies
                && find_nearest_unreserved_tool(
                    tag,
                    pose,
                    npc_id,
                    world_items,
                    item_registry,
                    store,
                )
                .is_none()
            {
                // Tool needed but unavailable — plan stays unworkable.
                continue;
            }
        }
        if store.is_unreachable(HaulTarget::Plan(*cell), now_secs) {
            continue;
        }
        if best.map(|(_, _, _, d)| dist < d).unwrap_or(true) {
            best = Some((HaulTarget::Plan(*cell), slot, remaining, dist));
        }
    }
    for (cell, state) in stations.iter() {
        let dist = chebyshev(*cell, foot);
        if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
            continue;
        }
        let demand = compute_station_demand(state, recipes, item_registry);
        if demand.is_empty() {
            continue;
        }
        let chosen = pick_haul_kind(
            demand.iter().map(|(s, c)| (*s, *c)),
            &items_by_slot,
            pose,
            npc_id,
            store,
        );
        let Some((slot, remaining)) = chosen else {
            continue;
        };
        if store.is_unreachable(HaulTarget::Station(*cell), now_secs) {
            continue;
        }
        if best.map(|(_, _, _, d)| dist < d).unwrap_or(true) {
            best = Some((HaulTarget::Station(*cell), slot, remaining, dist));
        }
    }
    let Some((target, item_slot, remaining_for_kind, _)) = best else {
        // No material hauling to do. Before giving up, see whether a
        // work-ready plan is blocked only on this NPC's missing tool —
        // if so, fetch the tool now so the planner can commit the work
        // next tick. Covers Remove plans (never haul targets — nothing
        // to deliver) and satisfied Build plans with a build tool gate.
        if try_schedule_tool_fetch(
            npc_id,
            pose,
            foot,
            equipped_tool,
            plans,
            block_registry,
            item_registry,
            chunks,
            chunk_map,
            world_items,
            store,
            now_secs,
        ) {
            return true;
        }
        // Last-resort idle-haul tier: sweep loose items into containers
        // or storage piles. Real pending work (material haul, tool
        // fetch) always outranks cosmetic tidying — this only fires
        // when there's nothing else useful to do.
        return try_schedule_tidy(
            npc_id,
            pose,
            cap,
            item_registry,
            block_registry,
            world_items,
            storage,
            containers,
            container_index,
            store,
            now_secs,
        );
    };

    // Reserve the tool first (if any). Stations never carry a tool
    // prereq at the haul layer, so this branch only fires for Plans.
    // If reservation fails (raced with another scheduler call), abort
    // this assignment — next tick we repick. Don't pre-reserve any
    // materials until the tool is locked, so a lost race releases
    // zero items.
    let pending_tool = if let HaulTarget::Plan(plan_cell) = target {
        let Some(state) = plans.get(plan_cell) else {
            return false;
        };
        let required_tool_tag =
            required_tool_for_plan(plan_cell, &state.kind, block_registry, chunks, chunk_map);
        if let Some(tag) = &required_tool_tag {
            if item_registry.tool_has_tag(equipped_tool.item, tag) {
                None
            } else {
                let Some((entity, item, translation)) = find_nearest_unreserved_tool(
                    tag,
                    pose,
                    npc_id,
                    world_items,
                    item_registry,
                    store,
                ) else {
                    return false;
                };
                if !store.try_reserve_internal(HaulSource::Ground(entity), npc_id) {
                    return false;
                }
                Some(ReservedItem {
                    source: HaulSource::Ground(entity),
                    item,
                    translation,
                    count: 1,
                })
            }
        } else {
            None
        }
    } else {
        None
    };

    let Some(pool) = items_by_slot.get(&item_slot) else {
        if let Some(tool) = &pending_tool {
            store.release_reservation_internal(tool.source, npc_id);
        }
        return false;
    };

    // Sort by distance so closest sources get reserved first.
    let mut candidates: Vec<(HaulSource, Vec3, u32, f32)> = pool
        .iter()
        .filter_map(|(source, translation, count)| {
            if store.reservation_taken_by_other(*source, npc_id) {
                return None;
            }
            let d = (*translation - pose).length();
            if d > MAX_HAUL_ITEM_RADIUS_M {
                return None;
            }
            Some((*source, *translation, *count, d))
        })
        .collect();
    candidates.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));

    // Reserve nearest sources until we've covered `want` units. A pile
    // or a stocked container (count > 1) is one reservation that
    // yields several units, so a single nearby source can satisfy the
    // whole trip; loose items still reserve one entity per unit.
    let want = cap.min(remaining_for_kind);
    let mut got: u32 = 0;
    let mut queue: Vec<ReservedItem> = Vec::new();
    for (source, translation, stack_count, _) in candidates {
        if got >= want {
            break;
        }
        if store.try_reserve_internal(source, npc_id) {
            let take = (want - got).min(stack_count.max(1));
            queue.push(ReservedItem {
                source,
                item: item_slot,
                translation,
                count: take,
            });
            got += take;
        }
    }
    if queue.is_empty() {
        if let Some(tool) = &pending_tool {
            store.release_reservation_internal(tool.source, npc_id);
        }
        return false;
    }
    info!(
        npc = npc_id.0,
        target = ?target,
        kind = item_slot.0,
        queued = queue.len(),
        tool = ?pending_tool.as_ref().map(|t| t.item.0),
        "haul assignment committed",
    );
    store.commit_assignment(
        npc_id,
        HaulAssignment {
            target,
            queue,
            pending_tool,
        },
    );
    true
}

/// Idle-haul "tidy" scheduler (S2 piles, S3 containers): sweep loose,
/// pileable items that aren't already sitting in a storage cell into
/// a home, grouping by kind. Destination preference, per the S3
/// design: **an accepting container with room beats any pile** —
/// containers are deliberate player furniture, piles the open-air
/// fallback. Within each class, nearest wins; among piles, topping up
/// an existing same-kind pile beats starting a fresh cell.
///
/// Reserves the loose items (single-kind, up to carry cap and the
/// destination's remaining room) and commits a
/// [`HaulTarget::Container`] or [`HaulTarget::Storage`] assignment —
/// the deposit leg ([`crate::npc::ArrivalAction::DepositAtContainer`] /
/// [`crate::npc::ArrivalAction::DepositAtStorage`]) stows the carry.
///
/// Runs only after material haul + tool fetch found nothing, so real
/// work always wins. Assumes an empty carry (the deposit-only branch
/// upstream handles a carrying NPC).
#[allow(
    clippy::too_many_arguments,
    reason = "shares the scheduler's subsystem spread"
)]
fn try_schedule_tidy(
    npc_id: NpcId,
    pose: Vec3,
    cap: u32,
    item_registry: &crate::items::ItemRegistry,
    block_registry: &crate::blocks::BlockRegistry,
    world_items: &Query<(Entity, &crate::protocol::WorldItem)>,
    storage: &crate::storage::StorageZones,
    containers: &crate::containers::Containers,
    container_index: &crate::containers::ContainerIndex,
    store: &mut HaulStore,
    now_secs: f32,
) -> bool {
    let storage_cells: std::collections::HashSet<IVec3> = storage.iter().copied().collect();
    if storage_cells.is_empty() && container_index.len() == 0 {
        return false;
    }

    // One pass over world items: an item in a storage cell contributes to
    // that cell's occupancy (a potential merge destination); a pileable
    // item elsewhere is loose and sweepable.
    let mut loose_by_kind: std::collections::HashMap<
        crate::items::ItemSlot,
        Vec<(Entity, Vec3, f32)>,
    > = std::collections::HashMap::new();
    // cell -> (kind, total units). A cell tracks the first kind seen;
    // tidy keeps cells single-kind, so this only diverges under a manual
    // player dump of mixed kinds onto one cell.
    let mut occupancy: std::collections::HashMap<IVec3, (crate::items::ItemSlot, u32)> =
        std::collections::HashMap::new();
    for (entity, wi) in world_items.iter() {
        let cell = wi.translation.floor().as_ivec3();
        if storage_cells.contains(&cell) {
            let occ = occupancy.entry(cell).or_insert((wi.item, 0));
            if occ.0 == wi.item {
                occ.1 += wi.count;
            }
            continue;
        }
        if !item_registry.def(wi.item).is_pileable() {
            continue;
        }
        if store.reservation_taken_by_other(HaulSource::Ground(entity), npc_id) {
            continue;
        }
        let d = (wi.translation - pose).length();
        if d > MAX_HAUL_ITEM_RADIUS_M {
            continue;
        }
        loose_by_kind
            .entry(wi.item)
            .or_default()
            .push((entity, wi.translation, d));
    }
    if loose_by_kind.is_empty() {
        return false;
    }

    // Tidy the kind of the nearest loose item.
    let mut best_kind: Option<(crate::items::ItemSlot, f32)> = None;
    for (slot, items) in &loose_by_kind {
        let nearest = items
            .iter()
            .map(|(_, _, d)| *d)
            .fold(f32::INFINITY, f32::min);
        if best_kind.map(|(_, bd)| nearest < bd).unwrap_or(true) {
            best_kind = Some((*slot, nearest));
        }
    }
    let (kind, _) = best_kind.unwrap();

    // Destination class 1 — containers: nearest placed container that
    // accepts this kind and has bulk room for at least one unit.
    // Skips containers memoized unreachable (walled-off) so the tidy
    // tier honors the same backoff every other haul target gets.
    let mut container_dest: Option<(IVec3, u32, f32)> = None; // (cell, room_units, dist)
    for (cell, slot) in container_index.iter() {
        let Some(cfg) = block_registry.def(slot).container.as_ref() else {
            continue; // index/registry drift — stale entry, skip
        };
        if !item_registry.def(kind).accepted_by(&cfg.accepts) {
            continue;
        }
        if store.is_unreachable(HaulTarget::Container(cell), now_secs) {
            continue;
        }
        let d = (cell.as_vec3() + Vec3::splat(0.5) - pose).length();
        if d > MAX_HAUL_ITEM_RADIUS_M {
            continue;
        }
        let room = crate::containers::room_for(containers.get(cell), cfg, item_registry, kind);
        if room == 0 {
            continue;
        }
        if container_dest.map(|(_, _, bd)| d < bd).unwrap_or(true) {
            container_dest = Some((cell, room, d));
        }
    }

    // Destination class 2 — open piles on storage cells: prefer an
    // existing same-kind pile with room (nearest); else an empty
    // storage cell (nearest). Cells occupied by another kind or
    // already full are skipped. Only consulted when no container
    // takes the kind.
    let (target, room) = if let Some((cell, room, _)) = container_dest {
        (HaulTarget::Container(cell), room)
    } else {
        let capacity = item_registry.def(kind).pile_capacity();
        let mut dest: Option<(IVec3, u32, bool, f32)> = None; // (cell, existing, has_pile, dist)
        for cell in &storage_cells {
            let existing = match occupancy.get(cell) {
                None => 0,
                Some((k, c)) if *k == kind && *c < capacity => *c,
                Some(_) => continue, // another kind, or same kind but full
            };
            if store.is_unreachable(HaulTarget::Storage(*cell), now_secs) {
                continue;
            }
            let has_pile = existing > 0;
            let d = (cell.as_vec3() + Vec3::splat(0.5) - pose).length();
            let better = match dest {
                None => true,
                Some((_, _, dest_has, dd)) => {
                    if has_pile != dest_has {
                        has_pile // topping up an existing pile beats a fresh cell
                    } else {
                        d < dd
                    }
                }
            };
            if better {
                dest = Some((*cell, existing, has_pile, d));
            }
        }
        let Some((dest_cell, dest_existing, _, _)) = dest else {
            return false;
        };
        (
            HaulTarget::Storage(dest_cell),
            capacity.saturating_sub(dest_existing),
        )
    };

    // Reserve loose items of `kind` nearest-first, up to the smaller of
    // carry cap and the destination's remaining room (so we never
    // over-fill it).
    let room = room.min(cap);
    if room == 0 {
        return false;
    }
    let mut cands = loose_by_kind.remove(&kind).unwrap();
    cands.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut queue: Vec<ReservedItem> = Vec::new();
    for (entity, translation, _) in cands {
        if queue.len() as u32 >= room {
            break;
        }
        if store.try_reserve_internal(HaulSource::Ground(entity), npc_id) {
            queue.push(ReservedItem {
                source: HaulSource::Ground(entity),
                item: kind,
                translation,
                count: 1,
            });
        }
    }
    if queue.is_empty() {
        return false;
    }
    info!(
        npc = npc_id.0,
        target = ?target,
        kind = kind.0,
        queued = queue.len(),
        "tidy assignment committed",
    );
    store.commit_assignment(
        npc_id,
        HaulAssignment {
            target,
            queue,
            pending_tool: None,
        },
    );
    true
}

/// Chebyshev (chessboard) distance between two cells. Same metric
/// used everywhere else in the scheduler for "is this within radius."
fn chebyshev(a: IVec3, b: IVec3) -> i32 {
    (a.x - b.x)
        .abs()
        .max((a.y - b.y).abs())
        .max((a.z - b.z).abs())
}

/// Pick which kind to fetch for a haul target with multiple unmet
/// demands. Single-stack carry means every queue entry is the same
/// ItemSlot, so picking once at target-selection time keeps the
/// "matchable items" check and the later reservation pass agreed on
/// the same kind.
///
/// Strategy: greatest remaining count wins (ties to iteration order).
/// Skips kinds with no reachable, unreserved items in the pool — a
/// kind with 10 needed but no items nearby would route the NPC to a
/// dead-end pickup leg.
fn pick_haul_kind(
    demands: impl IntoIterator<Item = (crate::items::ItemSlot, u32)>,
    items_by_slot: &std::collections::HashMap<crate::items::ItemSlot, Vec<(HaulSource, Vec3, u32)>>,
    pose: Vec3,
    npc_id: NpcId,
    store: &HaulStore,
) -> Option<(crate::items::ItemSlot, u32)> {
    let mut chosen: Option<(crate::items::ItemSlot, u32)> = None;
    for (slot, remaining) in demands {
        if remaining == 0 {
            continue;
        }
        let Some(pool) = items_by_slot.get(&slot) else {
            continue;
        };
        let reachable = pool.iter().any(|(source, translation, _count)| {
            if store.reservation_taken_by_other(*source, npc_id) {
                return false;
            }
            (*translation - pose).length() <= MAX_HAUL_ITEM_RADIUS_M
        });
        if !reachable {
            continue;
        }
        if chosen.map(|(_, c)| remaining > c).unwrap_or(true) {
            chosen = Some((slot, remaining));
        }
    }
    chosen
}

/// Per-item-slot deficit for a craft station: how much input is
/// short across the station's not-yet-done queued orders, after
/// accounting for current inventory and any active_work that has
/// already drained its inputs.
///
/// Implementation: for each non-done order, multiply each recipe
/// input by the order's `remaining`. If `active_work` is currently
/// running and matches the order's recipe, one unit's inputs are
/// already mid-craft (drained at WorkStation receive), so that unit
/// doesn't add demand. The result is summed across orders, then
/// the station's current inventory is subtracted.
///
/// Caller uses this to (a) tell whether a station needs hauling at
/// all (empty result ⇒ no), and (b) which item slot to fetch.
pub fn compute_station_demand(
    state: &crate::craft_stations::StationState,
    recipes: &crate::recipes::RecipeRegistry,
    item_registry: &crate::items::ItemRegistry,
) -> std::collections::HashMap<crate::items::ItemSlot, u32> {
    use block_junk_mod_api::recipes::RecipeId;
    let mut required: std::collections::HashMap<crate::items::ItemSlot, u32> =
        std::collections::HashMap::new();
    for order in &state.orders {
        if order.is_done() {
            continue;
        }
        let remaining = order.remaining();
        // If this order is the one in active_work, one of its units
        // has already had its inputs consumed (locked in at WorkStation
        // receive). Skip the inputs for that one unit.
        let in_flight = state
            .active_work
            .as_ref()
            .is_some_and(|aw| aw.recipe_id == order.recipe_id);
        let units_needing_inputs = if in_flight {
            remaining.saturating_sub(1)
        } else {
            remaining
        };
        if units_needing_inputs == 0 {
            continue;
        }
        let recipe_id = RecipeId::new(order.recipe_id.clone());
        let Some(recipe_slot) = recipes.slot_of(&recipe_id) else {
            continue;
        };
        let recipe = recipes.def(recipe_slot);
        for input in &recipe.inputs {
            let Some(input_slot) = item_registry.slot_of(&input.item) else {
                continue;
            };
            *required.entry(input_slot).or_insert(0) += input.count * units_needing_inputs;
        }
    }
    // Subtract inventory; drop entries with zero deficit so callers
    // can treat `is_empty()` as "station has everything it needs."
    let mut deficit: std::collections::HashMap<crate::items::ItemSlot, u32> =
        std::collections::HashMap::new();
    for (slot, qty) in required {
        let have = state.inventory.get(&slot).copied().unwrap_or(0);
        let need = qty.saturating_sub(have);
        if need > 0 {
            deficit.insert(slot, need);
        }
    }
    deficit
}

/// Tool-fetch-only scheduling: when there's no material hauling to
/// do, route a tool-less NPC to a loose tool that unlocks (or speeds
/// up) a work-ready plan. The assignment carries an empty material
/// queue + a `pending_tool`; after the `PickupTool` arrival equips
/// it, `pick_next_haul_leg` sees nothing left to haul and releases,
/// and the planner commits the now-properly-tooled work.
///
/// Greedy, and deliberately deferential: if *any* nearby work-ready
/// plan is already workable at full speed with the current tool (or
/// ungated), we fetch nothing — the planner should drive that work
/// instead of the NPC swapping tools while jobs wait. (The plan we
/// defer to may turn out claimed by another NPC; the next Idle tick
/// re-evaluates. Greedy-per-tick, same trade the material scheduler
/// makes.)
#[allow(
    clippy::too_many_arguments,
    reason = "shares the scheduler's subsystem spread"
)]
fn try_schedule_tool_fetch(
    npc_id: NpcId,
    pose: Vec3,
    foot: IVec3,
    equipped_tool: &crate::protocol::EquippedTool,
    plans: &crate::plans::Plans,
    block_registry: &crate::blocks::BlockRegistry,
    item_registry: &crate::items::ItemRegistry,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
    world_items: &Query<(Entity, &crate::protocol::WorldItem)>,
    store: &mut HaulStore,
    now_secs: f32,
) -> bool {
    use crate::protocol::PlanKind;
    let mut fetch: Option<(IVec3, block_junk_mod_api::blocks::TagId, i32)> = None;
    for (cell, state) in plans.iter() {
        // Only plans that are otherwise ready to work: Remove plans
        // always, Build plans once their materials are delivered.
        if matches!(state.kind, PlanKind::Build { .. }) && !state.is_satisfied() {
            continue;
        }
        let dist = chebyshev(*cell, foot);
        if dist > MAX_HAUL_PLAN_RADIUS_CELLS {
            continue;
        }
        if store.is_unreachable(HaulTarget::Plan(*cell), now_secs) {
            continue;
        }
        let required =
            required_tool_for_plan(*cell, &state.kind, block_registry, chunks, chunk_map);
        let Some(tag) = required else {
            return false; // ungated work available — planner's job
        };
        if item_registry.tool_has_tag(equipped_tool.item, &tag) {
            return false; // properly-tooled work available — planner's job
        }
        if fetch.as_ref().map(|(_, _, d)| dist < *d).unwrap_or(true) {
            fetch = Some((*cell, tag, dist));
        }
    }
    let Some((plan_cell, tag, _)) = fetch else {
        return false;
    };
    let Some((entity, item, translation)) =
        find_nearest_unreserved_tool(&tag, pose, npc_id, world_items, item_registry, store)
    else {
        // No tool to fetch. Soft-gated plans stay visible to the
        // planner (slow bare-hands work); hard-gated ones wait for a
        // tool to exist.
        return false;
    };
    if !store.try_reserve_internal(HaulSource::Ground(entity), npc_id) {
        return false;
    }
    info!(
        npc = npc_id.0,
        plan = ?plan_cell.to_array(),
        tool = item.0,
        required = %tag,
        "haul assignment (tool fetch) committed",
    );
    store.commit_assignment(
        npc_id,
        HaulAssignment {
            target: HaulTarget::Plan(plan_cell),
            queue: Vec::new(),
            pending_tool: Some(ReservedItem {
                source: HaulSource::Ground(entity),
                item,
                translation,
                count: 1,
            }),
        },
    );
    true
}

/// What tool tag (if any) the plan at `cell` requires its worker to
/// hold. Build plans gate on the *block being placed*'s
/// `build_required_tool` (vanilla: always `None` — building is
/// free-handed), Remove plans on the *live block being destroyed*'s
/// `required_tool` — same per-verb convention the click resolver uses
/// on the client. `None` ⇒ no tool required for this plan.
fn required_tool_for_plan(
    cell: IVec3,
    kind: &crate::protocol::PlanKind,
    block_registry: &crate::blocks::BlockRegistry,
    chunks: &Query<&crate::voxel::Chunk>,
    chunk_map: &crate::voxel::ChunkMap,
) -> Option<block_junk_mod_api::blocks::TagId> {
    use crate::protocol::PlanKind;
    let slot = match kind {
        PlanKind::Build { slot, .. } => *slot,
        PlanKind::Remove => {
            let (coord, local) = crate::voxel::world_to_chunk(cell);
            let entity = chunk_map.0.get(&coord)?;
            let chunk = chunks.get(*entity).ok()?;
            let s = chunk.get(local);
            if s.is_empty() {
                return None;
            }
            s
        }
    };
    block_registry
        .def(slot)
        .work_action
        .as_ref()?
        .tool_for(kind.work_verb())
        .cloned()
}

/// Find the nearest unreserved [`WorldItem`] whose def carries
/// `required_tag` in its `tool_tags`. Skips items reserved by
/// another NPC and items beyond `MAX_HAUL_ITEM_RADIUS_M`. Returns
/// (entity, slot, translation) for callers to plug into a
/// `ReservedItem`. Linear scan; tool count is tiny.
fn find_nearest_unreserved_tool(
    required_tag: &block_junk_mod_api::blocks::TagId,
    pose: Vec3,
    npc_id: NpcId,
    world_items: &Query<(Entity, &crate::protocol::WorldItem)>,
    item_registry: &crate::items::ItemRegistry,
    store: &HaulStore,
) -> Option<(Entity, crate::items::ItemSlot, Vec3)> {
    let mut best: Option<(Entity, crate::items::ItemSlot, Vec3, f32)> = None;
    for (entity, wi) in world_items.iter() {
        if store.reservation_taken_by_other(HaulSource::Ground(entity), npc_id) {
            continue;
        }
        let def = item_registry.def(wi.item);
        if !def.tool_tags.iter().any(|t| t == required_tag) {
            continue;
        }
        let d = (wi.translation - pose).length();
        if d > MAX_HAUL_ITEM_RADIUS_M {
            continue;
        }
        if best.map(|(_, _, _, bd)| d < bd).unwrap_or(true) {
            best = Some((entity, wi.item, wi.translation, d));
        }
    }
    best.map(|(e, s, t, _)| (e, s, t))
}

pub struct HaulPlugin;

impl Plugin for HaulPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HaulStore>();
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

    fn assignment_with_queue(entities: &[Entity]) -> HaulAssignment {
        HaulAssignment {
            target: HaulTarget::Plan(IVec3::ZERO),
            queue: entities
                .iter()
                .map(|e| ReservedItem {
                    source: HaulSource::Ground(*e),
                    item: ItemSlot(0),
                    translation: Vec3::ZERO,
                    count: 1,
                })
                .collect(),
            pending_tool: None,
        }
    }

    #[test]
    fn first_reserve_succeeds_second_fails() {
        let mut s = HaulStore::default();
        let e = entity(1);
        assert!(s.try_reserve_internal(HaulSource::Ground(e), NPC_1));
        assert!(!s.try_reserve_internal(HaulSource::Ground(e), NPC_2));
    }

    #[test]
    fn reserve_by_same_npc_is_idempotent() {
        let mut s = HaulStore::default();
        let e = entity(1);
        assert!(s.try_reserve_internal(HaulSource::Ground(e), NPC_1));
        assert!(s.try_reserve_internal(HaulSource::Ground(e), NPC_1));
    }

    #[test]
    fn release_reservation_frees_for_other() {
        let mut s = HaulStore::default();
        let e = entity(1);
        s.try_reserve_internal(HaulSource::Ground(e), NPC_1);
        s.release_reservation_internal(HaulSource::Ground(e), NPC_1);
        assert!(s.try_reserve_internal(HaulSource::Ground(e), NPC_2));
    }

    #[test]
    fn release_by_non_owner_is_no_op() {
        let mut s = HaulStore::default();
        let e = entity(1);
        s.try_reserve_internal(HaulSource::Ground(e), NPC_1);
        s.release_reservation_internal(HaulSource::Ground(e), NPC_2);
        assert!(!s.try_reserve_internal(HaulSource::Ground(e), NPC_2));
        assert!(s.try_reserve_internal(HaulSource::Ground(e), NPC_1));
    }

    #[test]
    fn release_for_npc_clears_assignment_and_all_reservations() {
        let mut s = HaulStore::default();
        let a = entity(1);
        let b = entity(2);
        assert!(s.try_reserve_internal(HaulSource::Ground(a), NPC_1));
        assert!(s.try_reserve_internal(HaulSource::Ground(b), NPC_1));
        s.commit_assignment(NPC_1, assignment_with_queue(&[a, b]));
        assert!(s.has_assignment(NPC_1));

        s.release_for_npc(NPC_1);

        assert!(!s.has_assignment(NPC_1));
        // Both reservations are now free for anyone else — coupling
        // enforced regardless of which path the caller took.
        assert!(s.try_reserve_internal(HaulSource::Ground(a), NPC_2));
        assert!(s.try_reserve_internal(HaulSource::Ground(b), NPC_2));
    }

    #[test]
    fn release_for_npc_drops_pending_tool() {
        let mut s = HaulStore::default();
        let tool = entity(10);
        assert!(s.try_reserve_internal(HaulSource::Ground(tool), NPC_1));
        s.commit_assignment(
            NPC_1,
            HaulAssignment {
                target: HaulTarget::Plan(IVec3::ZERO),
                queue: vec![],
                pending_tool: Some(ReservedItem {
                    source: HaulSource::Ground(tool),
                    item: ItemSlot(0),
                    translation: Vec3::ZERO,
                    count: 1,
                }),
            },
        );

        s.release_for_npc(NPC_1);

        assert!(!s.has_assignment(NPC_1));
        assert!(s.try_reserve_internal(HaulSource::Ground(tool), NPC_2));
    }

    #[test]
    fn release_for_npc_only_clears_that_npcs_state() {
        let mut s = HaulStore::default();
        let a = entity(1);
        let b = entity(2);
        s.try_reserve_internal(HaulSource::Ground(a), NPC_1);
        s.try_reserve_internal(HaulSource::Ground(b), NPC_2);
        s.commit_assignment(NPC_1, assignment_with_queue(&[a]));
        s.commit_assignment(NPC_2, assignment_with_queue(&[b]));

        s.release_for_npc(NPC_1);

        assert!(!s.has_assignment(NPC_1));
        assert!(s.has_assignment(NPC_2));
        // NPC_2's reservation survives.
        assert!(!s.try_reserve_internal(HaulSource::Ground(b), NPC_1));
    }

    #[test]
    fn drop_queue_entry_releases_its_reservation() {
        let mut s = HaulStore::default();
        let a = entity(1);
        let b = entity(2);
        s.try_reserve_internal(HaulSource::Ground(a), NPC_1);
        s.try_reserve_internal(HaulSource::Ground(b), NPC_1);
        s.commit_assignment(NPC_1, assignment_with_queue(&[a, b]));

        s.drop_queue_entry(NPC_1, HaulSource::Ground(a));
        // a is free; b is still queued and reserved.
        assert!(s.try_reserve_internal(HaulSource::Ground(a), NPC_2));
        assert!(!s.try_reserve_internal(HaulSource::Ground(b), NPC_2));
        assert_eq!(s.assignment_of(NPC_1).unwrap().queue.len(), 1);
    }

    #[test]
    fn clear_pending_tool_releases_its_reservation() {
        let mut s = HaulStore::default();
        let tool = entity(7);
        s.try_reserve_internal(HaulSource::Ground(tool), NPC_1);
        s.commit_assignment(
            NPC_1,
            HaulAssignment {
                target: HaulTarget::Plan(IVec3::ZERO),
                queue: vec![],
                pending_tool: Some(ReservedItem {
                    source: HaulSource::Ground(tool),
                    item: ItemSlot(0),
                    translation: Vec3::ZERO,
                    count: 1,
                }),
            },
        );

        s.clear_pending_tool(NPC_1);

        // Assignment still exists (might have a queue), but tool is freed.
        assert!(s.has_assignment(NPC_1));
        assert!(s.assignment_of(NPC_1).unwrap().pending_tool.is_none());
        assert!(s.try_reserve_internal(HaulSource::Ground(tool), NPC_2));
    }

    #[test]
    fn unreachable_memo_expires_and_prunes() {
        let mut store = HaulStore::default();
        let target = HaulTarget::Plan(IVec3::new(1, 2, 3));
        store.memo_unreachable(target, 30.0);
        assert!(store.is_unreachable(target, 10.0));
        assert!(store.is_unreachable(target, 29.9));
        // Expiry boundary: at/after retry_at the memo clears...
        assert!(!store.is_unreachable(target, 30.0));
        // ...and stays cleared (entry pruned, not just ignored).
        assert!(!store.is_unreachable(target, 0.0));
        // Other targets unaffected.
        assert!(!store.is_unreachable(HaulTarget::Station(IVec3::ZERO), 0.0));
    }
}
