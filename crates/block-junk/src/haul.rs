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

/// Where the next deposit leg of a haul cycle delivers materials.
/// Build plans and craft stations are both single-cell deposit
/// targets the scheduler can route to; the brain picks the right
/// arrival action ([`crate::npc::ArrivalAction::DepositAtPlan`] vs
/// [`crate::npc::ArrivalAction::DepositAtStation`]) from this.
///
/// Tool prereqs only apply to Plan targets (the *work* needs a tool;
/// delivery doesn't). Station haul never carries a `pending_tool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaulTarget {
    Plan(IVec3),
    Station(IVec3),
}

impl HaulTarget {
    /// The world cell of the deposit target — useful when the caller
    /// only needs the location (distance check, path target), not the
    /// kind.
    pub fn cell(&self) -> IVec3 {
        match self {
            HaulTarget::Plan(c) | HaulTarget::Station(c) => *c,
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
        if let Some(tool) = assignment.pending_tool {
            reservations.release(tool.entity, npc);
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
#[allow(clippy::too_many_arguments, reason = "scheduler reaches into many subsystems")]
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
    assignments: &mut HaulAssignments,
    reservations: &mut WorldItemReservations,
) -> bool {
    use crate::protocol::PlanKind;

    if assignments.contains(npc_id) {
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
        assignments.insert(
            npc_id,
            HaulAssignment {
                target,
                queue: Vec::new(),
                pending_tool: None,
            },
        );
        return true;
    }

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
            state.materials.iter().map(|m| (m.item, m.needed.saturating_sub(m.present))),
            &items_by_slot,
            pose,
            npc_id,
            reservations,
        );
        let Some((slot, remaining)) = chosen else {
            continue;
        };
        // Tool gate: either no tool needed, NPC has it, or one is
        // available to fetch. `required_tool_for_plan` reads the
        // live block for Remove plans, the planned block for Build.
        let required = required_tool_for_plan(
            *cell,
            &state.kind,
            block_registry,
            chunks,
            chunk_map,
        );
        if let Some(tag) = &required {
            let npc_satisfies = item_registry.tool_has_tag(equipped_tool.item, tag);
            if !npc_satisfies
                && find_nearest_unreserved_tool(
                    tag,
                    pose,
                    npc_id,
                    world_items,
                    item_registry,
                    reservations,
                )
                .is_none()
            {
                // Tool needed but unavailable — plan stays unworkable.
                continue;
            }
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
            reservations,
        );
        let Some((slot, remaining)) = chosen else {
            continue;
        };
        if best.map(|(_, _, _, d)| dist < d).unwrap_or(true) {
            best = Some((HaulTarget::Station(*cell), slot, remaining, dist));
        }
    }
    let Some((target, item_slot, remaining_for_kind, _)) = best else {
        return false;
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
        let required_tool_tag = required_tool_for_plan(
            plan_cell,
            &state.kind,
            block_registry,
            chunks,
            chunk_map,
        );
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
                    reservations,
                ) else {
                    return false;
                };
                if !reservations.try_reserve(entity, npc_id) {
                    return false;
                }
                Some(ReservedItem {
                    entity,
                    item,
                    translation,
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
            reservations.release(tool.entity, npc_id);
        }
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
        if let Some(tool) = &pending_tool {
            reservations.release(tool.entity, npc_id);
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
    assignments.insert(
        npc_id,
        HaulAssignment {
            target,
            queue,
            pending_tool,
        },
    );
    true
}

/// Chebyshev (chessboard) distance between two cells. Same metric
/// used everywhere else in the scheduler for "is this within radius."
fn chebyshev(a: IVec3, b: IVec3) -> i32 {
    (a.x - b.x).abs().max((a.y - b.y).abs()).max((a.z - b.z).abs())
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
    items_by_slot: &std::collections::HashMap<
        crate::items::ItemSlot,
        Vec<(Entity, Vec3)>,
    >,
    pose: Vec3,
    npc_id: NpcId,
    reservations: &WorldItemReservations,
) -> Option<(crate::items::ItemSlot, u32)> {
    let mut chosen: Option<(crate::items::ItemSlot, u32)> = None;
    for (slot, remaining) in demands {
        if remaining == 0 {
            continue;
        }
        let Some(pool) = items_by_slot.get(&slot) else {
            continue;
        };
        let reachable = pool.iter().any(|(entity, translation)| {
            if reservations.is_taken_by_other(*entity, npc_id) {
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

/// What tool tag (if any) the plan at `cell` requires its worker to
/// hold. Build plans gate on the *block being placed*, Remove plans
/// on the *live block being destroyed* — same convention the click
/// resolver uses on the client. `None` ⇒ no tool required for this
/// plan.
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
        .required_tool
        .clone()
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
    reservations: &WorldItemReservations,
) -> Option<(Entity, crate::items::ItemSlot, Vec3)> {
    let mut best: Option<(Entity, crate::items::ItemSlot, Vec3, f32)> = None;
    for (entity, wi) in world_items.iter() {
        if reservations.is_taken_by_other(entity, npc_id) {
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
                target: HaulTarget::Plan(IVec3::ZERO),
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
                pending_tool: None,
            },
        );

        release_haul_for(NPC_1, &mut assignments, &mut reservations);

        assert!(!assignments.contains(NPC_1));
        // Both reservations are now free for anyone else.
        assert!(reservations.try_reserve(a, NPC_2));
        assert!(reservations.try_reserve(b, NPC_2));
    }

    #[test]
    fn release_haul_for_also_drops_pending_tool() {
        let mut assignments = HaulAssignments::default();
        let mut reservations = WorldItemReservations::default();
        let tool = entity(10);
        reservations.try_reserve(tool, NPC_1);
        assignments.insert(
            NPC_1,
            HaulAssignment {
                target: HaulTarget::Plan(IVec3::ZERO),
                queue: vec![],
                pending_tool: Some(ReservedItem {
                    entity: tool,
                    item: ItemSlot(0),
                    translation: Vec3::ZERO,
                }),
            },
        );
        release_haul_for(NPC_1, &mut assignments, &mut reservations);
        assert!(!assignments.contains(NPC_1));
        // Tool reservation freed too — another NPC can claim it.
        assert!(reservations.try_reserve(tool, NPC_2));
    }
}
