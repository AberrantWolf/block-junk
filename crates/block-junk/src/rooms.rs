//! Engine-side room registry **and** detector.
//!
//! * [`RoomPatternRegistry`] — built from mod-registered patterns; a static
//!   catalogue read at match time.
//! * [`RoomMap`] — live state: every detected region's floor cells, matched
//!   pattern, computed signature, and a reverse cell→id index for
//!   invalidation.
//! * [`DetectionDirty`] — queue of recently edited cells with timestamps;
//!   the detector drains entries older than [`DEBOUNCE`] and re-runs
//!   detection in the affected neighbourhood.
//!
//! Detection runs synchronously on the server tick. The flood-fill is
//! capped at [`FLOOD_CAP`] cells, which keeps the work bounded — moves
//! to `AsyncComputeTaskPool` if profiling shows it pays.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use block_junk_mod_api::rooms::{
    BBox, Constraint, FloorComposition, FloorKind, PatternDomain, RoomEvent, RoomId, RoomPattern,
    RoomPatternId, RoomSignature, TagCount,
};
use block_junk_mod_api::shared::BlockPos;
use thiserror::Error;

use crate::blocks::{BlockRegistry, BlockSlot};
use crate::protocol::BlockEdit;
use crate::server::ChunkMap;
use crate::voxel::{Chunk, chunk_local_to_world, world_to_chunk};

/// Hard upper bound on floor-fill cells. Anything bigger is "outdoors" or
/// "unclassifiably huge" and isn't tracked as a room.
pub const FLOOD_CAP: u32 = 4096;
/// Player height in cells. A floor cell needs this many cells of clear
/// (air or `support_in_cell`) space starting at the floor cell itself.
const PLAYER_HEIGHT: i32 = 2;
/// Limit when probing column heights. Past this we declare the column
/// "open to sky" and the room has no roof.
const ROOF_PROBE_CAP: i32 = 1024;
/// Quiet period after the most recent edit before detection runs. Keeps
/// per-edit thrash from emitting `Created/Destroyed` storms during a
/// player's place-or-break burst.
const DEBOUNCE: Duration = Duration::from_millis(250);

#[allow(dead_code)]
const _: () = {
    // Compile-time sanity: PLAYER_HEIGHT is used in the floor-cell predicate
    // below; if it's not at least 1 the algorithm is meaningless.
    assert!(PLAYER_HEIGHT >= 1);
};

// ---------- pattern registry (existing) ----------

#[derive(Debug, Error)]
pub enum RoomBootstrapError {
    #[error("duplicate room pattern id {0}")]
    Duplicate(RoomPatternId),
    #[error("pattern {child} declares unknown parent {parent}")]
    UnknownParent {
        child: RoomPatternId,
        parent: RoomPatternId,
    },
    #[error(
        "pattern {child} (domain={child_domain:?}) inherits from {parent} (domain={parent_domain:?})"
    )]
    DomainMismatch {
        child: RoomPatternId,
        child_domain: PatternDomain,
        parent: RoomPatternId,
        parent_domain: PatternDomain,
    },
    #[error("cycle in pattern parent chain involving {0}")]
    Cycle(RoomPatternId),
}

#[derive(Resource)]
pub struct RoomPatternRegistry {
    patterns: Vec<RoomPattern>,
    by_id: HashMap<RoomPatternId, usize>,
    /// Depth of each pattern in its inheritance tree. Roots are 0; used by
    /// the matcher to pick the *deepest* matching node.
    depths: Vec<u32>,
}

#[allow(
    dead_code,
    reason = "get/depth_of/iter are the surface the room detector will read once it lands"
)]
impl RoomPatternRegistry {
    pub fn build(pending: Vec<RoomPattern>) -> Result<Self, RoomBootstrapError> {
        let mut by_id = HashMap::with_capacity(pending.len());
        for (i, p) in pending.iter().enumerate() {
            if by_id.insert(p.id.clone(), i).is_some() {
                return Err(RoomBootstrapError::Duplicate(p.id.clone()));
            }
        }

        let mut depths = vec![0u32; pending.len()];
        for i in 0..pending.len() {
            let mut depth = 0u32;
            let mut seen: HashSet<RoomPatternId> = HashSet::new();
            seen.insert(pending[i].id.clone());
            let mut current = &pending[i];
            while let Some(parent_id) = &current.parent {
                let &parent_idx =
                    by_id
                        .get(parent_id)
                        .ok_or_else(|| RoomBootstrapError::UnknownParent {
                            child: current.id.clone(),
                            parent: parent_id.clone(),
                        })?;
                let parent = &pending[parent_idx];
                if parent.domain != current.domain {
                    return Err(RoomBootstrapError::DomainMismatch {
                        child: current.id.clone(),
                        child_domain: current.domain,
                        parent: parent.id.clone(),
                        parent_domain: parent.domain,
                    });
                }
                if !seen.insert(parent.id.clone()) {
                    return Err(RoomBootstrapError::Cycle(pending[i].id.clone()));
                }
                depth += 1;
                current = parent;
            }
            depths[i] = depth;
        }

        Ok(Self {
            patterns: pending,
            by_id,
            depths,
        })
    }

    pub fn get(&self, id: &RoomPatternId) -> Option<&RoomPattern> {
        self.by_id.get(id).map(|&i| &self.patterns[i])
    }

    pub fn depth_of(&self, id: &RoomPatternId) -> Option<u32> {
        self.by_id.get(id).map(|&i| self.depths[i])
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RoomPattern> + '_ {
        self.patterns.iter()
    }
}

// ---------- live state ----------

/// Bevy-bus wrapper for [`RoomEvent`]. The mod-api type stays Bevy-free,
/// so we can't put `#[derive(Message)]` on it directly. Engine systems
/// write `RoomEventMsg(ev)` to the local bus; the dispatch system reads
/// these and forwards the inner event to mods.
#[derive(Message, Clone, Debug)]
pub struct RoomEventMsg(pub RoomEvent);

struct Room {
    pattern: Option<RoomPatternId>,
    floor_cells: Vec<IVec3>,
}

#[derive(Resource, Default)]
pub struct RoomMap {
    rooms: HashMap<RoomId, Room>,
    cell_to_room: HashMap<IVec3, RoomId>,
    next_id: u32,
}

impl RoomMap {
    fn alloc(&mut self) -> RoomId {
        let id = RoomId(self.next_id);
        self.next_id += 1;
        id
    }
}

#[derive(Resource, Default)]
pub struct DetectionDirty {
    cells: Vec<(IVec3, Instant)>,
}

// ---------- systems ----------

/// Reads applied `BlockEdit`s from the local server bus and pushes the
/// edited world cells onto the dirty queue. Runs after `receive_block_edits`
/// so it sees fully-applied state.
pub fn mark_dirty_from_edits(
    mut reader: MessageReader<BlockEdit>,
    mut dirty: ResMut<DetectionDirty>,
) {
    let now = Instant::now();
    for edit in reader.read() {
        let world = chunk_local_to_world(edit.coord, edit.pos);
        dirty.cells.push((world, now));
    }
}

/// Drains debounced dirty entries and runs detection. Emits `RoomEvent`s
/// onto the local server bus; the `dispatch_room_events` system in
/// `scripting.rs` forwards them to mod hooks.
pub fn process_dirty(
    mut dirty: ResMut<DetectionDirty>,
    chunks: Query<&Chunk>,
    chunk_map: Res<ChunkMap>,
    block_registry: Res<BlockRegistry>,
    pattern_registry: Res<RoomPatternRegistry>,
    mut rooms: ResMut<RoomMap>,
    mut events: MessageWriter<RoomEventMsg>,
) {
    if dirty.cells.is_empty() {
        return;
    }
    let now = Instant::now();
    let most_recent = dirty.cells.iter().map(|(_, t)| *t).max().unwrap();
    if now.duration_since(most_recent) < DEBOUNCE {
        return;
    }
    let edited: Vec<IVec3> = dirty.cells.drain(..).map(|(c, _)| c).collect();

    let get_block = |w: IVec3| -> BlockSlot {
        let (coord, local) = world_to_chunk(w);
        chunk_map
            .0
            .get(&coord)
            .and_then(|&entity| chunks.get(entity).ok())
            .map(|chunk| chunk.get(local))
            .unwrap_or(BlockSlot::EMPTY)
    };

    // Each edit's 6-neighbourhood is the candidate seed set. An edit at E
    // can change floor-cell status of cells in E ± {X, Y, Z}: removing E
    // changes whether cells at E ± Y have headroom/support; adding E
    // changes cells next to E that previously fanned through E's location.
    let mut seeds: HashSet<IVec3> = HashSet::with_capacity(edited.len() * 7);
    for &c in &edited {
        seeds.insert(c);
        for dir in [IVec3::X, -IVec3::X, IVec3::Y, -IVec3::Y, IVec3::Z, -IVec3::Z] {
            seeds.insert(c + dir);
        }
    }

    // Any room whose floor includes any seed cell is invalidated, then
    // rebuilt from the new fill (if its cells are still a valid room).
    let mut to_invalidate: HashSet<RoomId> = HashSet::new();
    for s in &seeds {
        if let Some(&id) = rooms.cell_to_room.get(s) {
            to_invalidate.insert(id);
        }
    }

    // Collect the new fills before mutating `rooms`. The `visited` set is
    // shared across every seed in this batch — both as the within-fill
    // dedup AND as the across-fill "this region was already explored"
    // marker. So a fill that bails at the cap (outdoor leak) leaves the
    // walked cells marked, and the next sibling seed in the same outdoor
    // region skips immediately instead of rewalking 4096 cells.
    let mut new_fills: Vec<Vec<IVec3>> = Vec::new();
    let mut visited: HashSet<IVec3> = HashSet::new();
    for &s in &seeds {
        if visited.contains(&s) {
            continue;
        }
        if !is_floor_cell(s, &get_block, &block_registry) {
            continue;
        }
        if let Some(cells) =
            flood_fill_floor(s, &get_block, &block_registry, FLOOD_CAP, &mut visited)
        {
            new_fills.push(cells);
        }
    }

    // 3-way diff: a region whose floor_cells set is unchanged is the
    // *same* room (Changed at most). Match by minimum corner — cheap and
    // unambiguous given a fixed canonical ordering.
    let mut survivors: HashMap<IVec3, RoomId> = HashMap::new();
    for &id in &to_invalidate {
        if let Some(room) = rooms.rooms.get(&id) {
            if let Some(min) = canonical_min(&room.floor_cells) {
                survivors.insert(min, id);
            }
        }
    }

    // Pre-compute pattern matches & signatures for new fills, then apply.
    struct Pending {
        cells: Vec<IVec3>,
        canonical: IVec3,
        signature: RoomSignature,
        pattern: Option<RoomPatternId>,
    }
    let mut pending: Vec<Pending> = Vec::with_capacity(new_fills.len());
    for cells in new_fills {
        let canonical = canonical_min(&cells).expect("flood-fill produced an empty cell list");
        let signature = compute_signature(&cells, &get_block, &block_registry);
        let pattern = match_pattern(&signature, &pattern_registry);
        pending.push(Pending {
            cells,
            canonical,
            signature,
            pattern,
        });
    }

    // For each invalidated room: if its canonical key matches a pending
    // fill, the room *kept its identity* (its anchor cell is still part
    // of the new region) and we emit Changed instead of Destroyed+Created.
    // Cell sets aren't required to match exactly — a single edit usually
    // trades a cell at Y for a new one at Y+1 (block placed) or vice
    // versa, which would otherwise flicker as Destroyed+Created every
    // time the player toggles a single block.
    let mut changed_pairs: HashMap<IVec3, RoomId> = HashMap::new();
    for id in &to_invalidate {
        let Some(room) = rooms.rooms.get(id) else {
            continue;
        };
        let Some(canon) = canonical_min(&room.floor_cells) else {
            continue;
        };
        if pending.iter().any(|p| p.canonical == canon) {
            changed_pairs.insert(canon, *id);
        }
    }

    // Apply Destroyed for every invalidated room that didn't survive.
    for id in &to_invalidate {
        if changed_pairs.values().any(|v| v == id) {
            continue;
        }
        if let Some(room) = rooms.rooms.remove(id) {
            for c in &room.floor_cells {
                if rooms.cell_to_room.get(c).copied() == Some(*id) {
                    rooms.cell_to_room.remove(c);
                }
            }
            events.write(RoomEventMsg(RoomEvent::Destroyed { room: *id }));
            info!(?id, "room destroyed");
        }
    }

    // Apply Changed (for matched survivors) and Created (for the rest).
    for p in pending {
        let mut keep_id = changed_pairs.get(&p.canonical).copied();
        let from_pattern = if let Some(id) = keep_id {
            // Pull the previous pattern out of the map so we can compare.
            rooms.rooms.get(&id).and_then(|r| r.pattern.clone())
        } else {
            None
        };

        if keep_id.is_none() {
            keep_id = Some(rooms.alloc());
        }
        let id = keep_id.unwrap();

        // Re-stamp cell_to_room (cells haven't moved for Changed, but it's
        // the same code path).
        for &c in &p.cells {
            rooms.cell_to_room.insert(c, id);
        }

        let was_changed = changed_pairs.values().any(|v| *v == id);
        let event = if was_changed {
            if from_pattern == p.pattern {
                // Same cells, same matched pattern, same signature
                // *modulo predicate fields* — we still rebuild the Room
                // record so tag counts etc. update, but skip the event.
                None
            } else {
                Some(RoomEvent::Changed {
                    room: id,
                    from: from_pattern.clone(),
                    to: p.pattern.clone(),
                    signature: p.signature.clone(),
                })
            }
        } else {
            Some(RoomEvent::Created {
                room: id,
                pattern: p.pattern.clone(),
                signature: p.signature.clone(),
            })
        };

        rooms.rooms.insert(
            id,
            Room {
                pattern: p.pattern.clone(),
                floor_cells: p.cells,
            },
        );

        if let Some(ev) = event {
            match &ev {
                RoomEvent::Created { pattern, .. } => {
                    info!(?id, ?pattern, "room created")
                }
                RoomEvent::Changed { from, to, .. } => {
                    info!(?id, ?from, ?to, "room changed")
                }
                RoomEvent::Destroyed { .. } => unreachable!(),
            }
            events.write(RoomEventMsg(ev));
        }
    }
}

// ---------- helpers ----------

/// Canonical key for a floor-cell set: the lexicographically minimum cell.
/// Stable across re-detection as long as the set itself doesn't change.
fn canonical_min(cells: &[IVec3]) -> Option<IVec3> {
    cells
        .iter()
        .copied()
        .min_by_key(|c| (c.y, c.x, c.z))
}

/// Cell `c` qualifies as a floor cell if a player can stand there: the
/// cell itself is air (or carries `support_in_cell`), there are
/// [`PLAYER_HEIGHT`] passable cells starting at `c`, and the cell below
/// either has `support_below` or `c` itself has `support_in_cell`.
fn is_floor_cell(
    c: IVec3,
    get_block: &impl Fn(IVec3) -> BlockSlot,
    reg: &BlockRegistry,
) -> bool {
    let here_slot = get_block(c);
    let here_def = reg.def(here_slot);
    let here_passable = here_slot.is_empty() || here_def.flags.support_in_cell;
    if !here_passable {
        return false;
    }
    // Headroom: PLAYER_HEIGHT consecutive passable cells starting at c.
    for dy in 1..PLAYER_HEIGHT {
        let s = get_block(c + IVec3::new(0, dy, 0));
        let d = reg.def(s);
        let passable = s.is_empty() || d.flags.support_in_cell;
        if !passable {
            return false;
        }
    }
    if here_def.flags.support_in_cell {
        return true;
    }
    let below = get_block(c - IVec3::Y);
    reg.def(below).flags.support_below
}

fn flood_fill_floor(
    seed: IVec3,
    get_block: &impl Fn(IVec3) -> BlockSlot,
    reg: &BlockRegistry,
    cap: u32,
    visited: &mut HashSet<IVec3>,
) -> Option<Vec<IVec3>> {
    debug_assert!(is_floor_cell(seed, get_block, reg));
    let mut queue: VecDeque<IVec3> = VecDeque::new();
    let mut out: Vec<IVec3> = Vec::new();
    queue.push_back(seed);
    visited.insert(seed);
    while let Some(c) = queue.pop_front() {
        if (out.len() as u32) >= cap {
            return None;
        }
        out.push(c);
        // 4 cardinal horizontals × {-1, 0, +1} Y. The ±1 Y step lets the
        // fill cross uneven terrain and stair-shaped geometry: the
        // destination still has to be a floor cell on its own (which
        // requires headroom + support), so wall tops in normal rooms
        // (≥2-high walls) don't get traversed because the cells directly
        // above the floor inside the room have no `support_below` and so
        // aren't candidates. Edge cases that *do* leak (1-high "fences",
        // wall-flush ramps onto wall tops) hit the floor cap and bail.
        for [dx, dz] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
            for dy in [-1, 0, 1] {
                let n = c + IVec3::new(dx, dy, dz);
                if !visited.insert(n) {
                    continue;
                }
                if is_floor_cell(n, get_block, reg) {
                    queue.push_back(n);
                }
            }
        }
    }
    Some(out)
}

fn compute_signature(
    floor_cells: &[IVec3],
    get_block: &impl Fn(IVec3) -> BlockSlot,
    reg: &BlockRegistry,
) -> RoomSignature {
    let n = floor_cells.len() as f32;
    let mut min = floor_cells[0];
    let mut max = floor_cells[0];
    for &c in &floor_cells[1..] {
        min = min.min(c);
        max = max.max(c);
    }

    let mut comp = FloorComposition::default();
    for &c in floor_cells {
        let here = get_block(c);
        if reg.def(here).flags.support_in_cell {
            comp.support_in_cell += 1.0;
            continue;
        }
        let below = get_block(c - IVec3::Y);
        let bd = reg.def(below);
        if bd.flags.solid {
            comp.solid += 1.0;
        } else if bd.flags.support_below {
            comp.water_below += 1.0;
        }
    }
    if n > 0.0 {
        comp.solid /= n;
        comp.water_below /= n;
        comp.support_in_cell /= n;
    }

    let mut min_hr: u32 = u32::MAX;
    let mut max_hr: u32 = 0;
    let mut volume: u32 = 0;
    let mut all_roofed = true;
    let mut tag_counts: HashMap<_, u32> = HashMap::new();
    for &c in floor_cells {
        // Walk straight up from the floor cell, collecting tags as we go,
        // stopping at the first `room_boundary` (ceiling) we hit. If we
        // run off the probe cap, the column is open to sky.
        let mut hr: u32 = 0;
        let mut roofed = false;
        for dy in 0..ROOF_PROBE_CAP {
            let cell = c + IVec3::new(0, dy, 0);
            let slot = get_block(cell);
            let def = reg.def(slot);
            // The floor cell itself counts as a passable air cell, but its
            // tags shouldn't count (it's the floor, not the volume).
            // Only collect tags from cells *above* the floor.
            if dy > 0 {
                if !slot.is_empty() {
                    if def.flags.room_boundary {
                        roofed = true;
                        break;
                    }
                    for tag in &def.tags {
                        *tag_counts.entry(tag.clone()).or_insert(0) += 1;
                    }
                    if !def.flags.support_in_cell {
                        // A non-boundary, non-traversable block (a chair,
                        // a chest) occupies the cell but doesn't end the
                        // column. Continue past it.
                    }
                }
            }
            // For headroom purposes, count cells from dy=0 that are
            // passable. The floor cell at dy=0 is passable by predicate.
            if slot.is_empty() || def.flags.support_in_cell {
                hr += 1;
            } else {
                // Hit a non-boundary, non-traversable block — counts as
                // ceiling for column termination but doesn't increment hr.
                roofed = true;
                break;
            }
        }
        if !roofed {
            all_roofed = false;
        }
        min_hr = min_hr.min(hr);
        max_hr = max_hr.max(hr);
        volume = volume.saturating_add(hr);
    }
    if floor_cells.is_empty() {
        min_hr = 0;
    }

    RoomSignature {
        domain: PatternDomain::Volumetric,
        bbox: BBox {
            min: BlockPos {
                x: min.x,
                y: min.y,
                z: min.z,
            },
            max: BlockPos {
                x: max.x,
                y: max.y,
                z: max.z,
            },
        },
        cell_count: floor_cells.len() as u32,
        volume: Some(volume),
        min_headroom: Some(min_hr),
        max_headroom: Some(max_hr),
        has_roof: Some(all_roofed),
        door_count: None, // wall-walk for door counting isn't done yet
        floor_composition: Some(comp),
        tag_counts: tag_counts
            .into_iter()
            .map(|(tag, count)| TagCount { tag, count })
            .collect(),
    }
}

/// Find the deepest matching pattern (with the parent chain's constraints
/// also satisfied), breaking ties by `priority` then registration order.
fn match_pattern(
    sig: &RoomSignature,
    registry: &RoomPatternRegistry,
) -> Option<RoomPatternId> {
    let mut best: Option<(&RoomPattern, u32)> = None;
    'pattern: for pattern in registry.iter() {
        if pattern.domain != sig.domain {
            continue;
        }
        // Walk the inheritance chain; *every* ancestor's constraints must
        // pass before this pattern can match.
        let mut current = pattern;
        loop {
            for c in &current.constraints {
                if !evaluate_constraint(c, sig) {
                    continue 'pattern;
                }
            }
            match &current.parent {
                Some(parent_id) => match registry.get(parent_id) {
                    Some(p) => current = p,
                    None => break, // pre-validated; can't actually happen
                },
                None => break,
            }
        }
        let depth = registry.depth_of(&pattern.id).unwrap_or(0);
        let take = match best {
            None => true,
            Some((b, b_depth)) => {
                depth > b_depth || (depth == b_depth && pattern.priority > b.priority)
            }
        };
        if take {
            best = Some((pattern, depth));
        }
    }
    best.map(|(p, _)| p.id.clone())
}

fn evaluate_constraint(c: &Constraint, sig: &RoomSignature) -> bool {
    match c {
        Constraint::Volume { min, max } => {
            let v = sig.volume.unwrap_or(0);
            min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
        }
        Constraint::FloorArea { min, max } => {
            let v = sig.cell_count;
            min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
        }
        Constraint::Headroom { min, max } => {
            let lo = sig.min_headroom.unwrap_or(0);
            let hi = sig.max_headroom.unwrap_or(0);
            min.is_none_or(|m| lo >= m) && max.is_none_or(|m| hi <= m)
        }
        Constraint::HasRoof { required } => sig.has_roof == Some(*required),
        Constraint::FloorFraction { surface, min } => {
            let fc = sig.floor_composition.unwrap_or_default();
            let v = match surface {
                FloorKind::Solid => fc.solid,
                FloorKind::WaterBelow => fc.water_below,
                FloorKind::SupportInCell => fc.support_in_cell,
            };
            v >= *min
        }
        Constraint::TagCount { tag, min, max } => {
            let count = sig
                .tag_counts
                .iter()
                .find(|tc| &tc.tag == tag)
                .map(|tc| tc.count)
                .unwrap_or(0);
            count >= *min && max.is_none_or(|m| count <= m)
        }
        Constraint::TagFraction { tag, min } => {
            let count = sig
                .tag_counts
                .iter()
                .find(|tc| &tc.tag == tag)
                .map(|tc| tc.count)
                .unwrap_or(0);
            (count as f32) / (sig.cell_count.max(1) as f32) >= *min
        }
        Constraint::ComponentSize { min, max } => {
            let v = sig.cell_count;
            min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
        }
        // Connective domain — no detector for it yet, so any pattern with
        // an `AdjacentPair` constraint can't match. Returning false keeps
        // the volumetric matcher from accidentally selecting one.
        Constraint::AdjacentPair { .. } => false,
    }
}
