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
use crate::protocol::CellEdit;
use crate::voxel::{Chunk, ChunkMap, world_to_chunk};

/// Hard upper bound on floor-fill cells. Anything bigger is "outdoors" or
/// "unclassifiably huge" and isn't tracked as a room.
pub const FLOOD_CAP: u32 = 4096;
/// Limit when probing column heights. Past this we declare the column
/// "open to sky" and the room has no roof.
const ROOF_PROBE_CAP: i32 = 1024;
/// Quiet period after the most recent edit before detection runs. Keeps
/// per-edit thrash from emitting `Created/Destroyed` storms during a
/// player's place-or-break burst.
const DEBOUNCE: Duration = Duration::from_millis(250);

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
    /// Volumetric AABB: floor footprint XZ × Y from floor up to the
    /// ceiling (or the topmost wall layer for open-roof rooms). Used to
    /// invalidate the room when an edit lands inside its volume — a roof
    /// block placed at Y=floor+2 isn't in `cell_to_room` (which only
    /// holds floor cells), so without bbox tracking we'd never re-detect.
    bbox_min: IVec3,
    bbox_max: IVec3,
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

    /// Iterate every detected region that currently has a matched
    /// pattern. Yields `(room id, deepest pattern id, anchor cell)`,
    /// where the anchor is whichever floor cell sits closest to the
    /// region's geometric centroid — a sensible "go to this room"
    /// target that's guaranteed to be walkable (it *is* a floor cell).
    ///
    /// Unmatched regions (detected but no pattern fits the signature)
    /// are skipped — they're internal bookkeeping for invalidation and
    /// of no use to a planner looking for a target.
    pub fn iter_matched(&self) -> impl Iterator<Item = (RoomId, &RoomPatternId, IVec3)> + '_ {
        self.rooms.iter().filter_map(|(&id, room)| {
            let pattern = room.pattern.as_ref()?;
            let anchor = floor_anchor(&room.floor_cells)?;
            Some((id, pattern, anchor))
        })
    }
}

/// Floor cell nearest the geometric centroid of `cells`. Returns
/// `None` only on empty input (cells are integer-cell positions and
/// every region tracked by `RoomMap` has at least one floor cell). The
/// centroid itself may not be a floor cell in L-shaped or U-shaped
/// rooms — picking the nearest existing floor cell instead keeps the
/// target walkable regardless of shape.
fn floor_anchor(cells: &[IVec3]) -> Option<IVec3> {
    if cells.is_empty() {
        return None;
    }
    let mut sum = IVec3::ZERO;
    for c in cells {
        sum += *c;
    }
    let n = cells.len() as i32;
    let centroid = sum / n;
    cells
        .iter()
        .min_by_key(|c| {
            let d = **c - centroid;
            d.x.abs() + d.y.abs() + d.z.abs()
        })
        .copied()
}

#[derive(Resource, Default)]
pub struct DetectionDirty {
    cells: Vec<(IVec3, Instant)>,
}

impl DetectionDirty {
    /// Mark a world cell dirty for re-detection. Honours the same
    /// [`DEBOUNCE`] window as edit-driven marking — used by the save
    /// loader to prime re-detection of rooms that existed at save time
    /// (RoomMap itself is runtime-only).
    pub fn push(&mut self, cell: IVec3, at: Instant) {
        self.cells.push((cell, at));
    }
}

// ---------- systems ----------

/// Reads applied per-cell edits from the local server bus and pushes the
/// edited world cells onto the dirty queue. Runs after `receive_block_edits`
/// so it sees fully-applied state. A multi-cell place fires multiple
/// CellEdits in a single tick, all of which land in this debounced
/// queue and resolve into one detection pass.
pub fn mark_dirty_from_edits(
    mut reader: MessageReader<CellEdit>,
    mut dirty: ResMut<DetectionDirty>,
) {
    let now = Instant::now();
    for edit in reader.read() {
        dirty.cells.push((edit.world, now));
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

    // Each edit's horizontal 4-neighbourhood is the candidate seed set.
    // We deliberately *don't* seed at ±Y from the edit: an edit at E that
    // creates a fresh floor cell directly above (because E became a
    // support_below) is real, but isolating that 1-cell "podium-top"
    // region as its own room flickers spurious Created/Destroyed events.
    // The same rule applies for cells above ground inside a yard with
    // uneven terrain — each Y level becomes its own room until a `step`
    // block tag exists to mark explicit Y-traversal points.
    let mut seeds: HashSet<IVec3> = HashSet::with_capacity(edited.len() * 5);
    for &c in &edited {
        seeds.insert(c);
        for dir in [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z] {
            seeds.insert(c + dir);
        }
    }

    // Any room whose floor includes any seed cell is invalidated. Plus
    // any room whose VOLUMETRIC bbox contains any edited cell — without
    // this, an edit above the floor (placing a roof, raising the wall a
    // layer) wouldn't be in `cell_to_room` and the room would never be
    // re-evaluated against deeper patterns like small_house.
    let mut to_invalidate: HashSet<RoomId> = HashSet::new();
    for s in &seeds {
        if let Some(&id) = rooms.cell_to_room.get(s) {
            to_invalidate.insert(id);
        }
    }
    for &edit in &edited {
        for (&id, room) in rooms.rooms.iter() {
            if bbox_contains(room.bbox_min, room.bbox_max, edit) {
                to_invalidate.insert(id);
            }
        }
    }
    // Re-seed from each invalidated room's floor cells so the flood-fill
    // actually re-runs over them. Without this, an above-floor edit would
    // invalidate the room but produce no new fill (no seed reaches the
    // floor cells), and the room would just be Destroyed silently.
    let invalidate_seeds: Vec<IVec3> = to_invalidate
        .iter()
        .filter_map(|id| rooms.rooms.get(id))
        .flat_map(|room| room.floor_cells.iter().copied())
        .collect();
    seeds.extend(invalidate_seeds);

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

    // Pre-compute pattern matches & signatures for new fills, then apply.
    struct Pending {
        cells: Vec<IVec3>,
        canonical: IVec3,
        signature: RoomSignature,
        pattern: Option<RoomPatternId>,
        bbox_min: IVec3,
        bbox_max: IVec3,
    }
    let mut pending: Vec<Pending> = Vec::with_capacity(new_fills.len());
    for cells in new_fills {
        let canonical = canonical_min(&cells).expect("flood-fill produced an empty cell list");
        let signature = compute_signature(&cells, &get_block, &block_registry);
        let pattern = match_pattern(&signature, &pattern_registry);
        // Volumetric bbox covering everything that, if edited, can affect
        // this room's classification:
        //   - Floor footprint extended by 1 in X and Z so the wall ring
        //     (perimeter cells, one outside the floor in each cardinal
        //     direction) is included. Without this, wall edits land
        //     OUTSIDE the floor's XZ extents and never trigger
        //     invalidation.
        //   - One Y below the floor for the support layer (breaking the
        //     ground under the floor invalidates the room).
        //   - enclosure_height layers above the floor, plus 1 slack so a
        //     roof or new wall placed just above the topmost bounded
        //     layer still intersects.
        let height = signature.enclosure_height.unwrap_or(1).max(1);
        let bbox_min = IVec3::new(
            signature.bbox.min.x - 1,
            signature.bbox.min.y - 1,
            signature.bbox.min.z - 1,
        );
        let bbox_max = IVec3::new(
            signature.bbox.max.x + 1,
            signature.bbox.min.y + height as i32 + 1,
            signature.bbox.max.z + 1,
        );
        pending.push(Pending {
            cells,
            canonical,
            signature,
            pattern,
            bbox_min,
            bbox_max,
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
    // Only emit the public event if the room had a matched pattern —
    // unmatched fills are tracked internally for invalidation but stay
    // silent so mods don't see noise from in-progress geometry.
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
            if room.pattern.is_some() {
                events.write(RoomEventMsg(RoomEvent::Destroyed { room: *id }));
                info!(?id, "room destroyed");
            }
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

        // Pattern-transition events. We only surface a public event when
        // the matched pattern changes (or appears, or disappears).
        // Unmatched-only transitions (None ↔ None) and same-pattern
        // updates stay silent — the room is tracked but mods don't care.
        let event = match (from_pattern.as_ref(), p.pattern.as_ref()) {
            (None, None) => None,
            (None, Some(_)) => Some(RoomEvent::Created {
                room: id,
                pattern: p.pattern.clone(),
                signature: p.signature.clone(),
            }),
            (Some(_), None) => Some(RoomEvent::Destroyed { room: id }),
            (Some(f), Some(t)) if f == t => None,
            (Some(_), Some(_)) => Some(RoomEvent::Changed {
                room: id,
                from: from_pattern.clone(),
                to: p.pattern.clone(),
                signature: p.signature.clone(),
            }),
        };

        rooms.rooms.insert(
            id,
            Room {
                pattern: p.pattern.clone(),
                floor_cells: p.cells,
                bbox_min: p.bbox_min,
                bbox_max: p.bbox_max,
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
                RoomEvent::Destroyed { .. } => {
                    info!(?id, "room destroyed (pattern lost)")
                }
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

fn bbox_contains(min: IVec3, max: IVec3, p: IVec3) -> bool {
    p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y && p.z >= min.z && p.z <= max.z
}

/// Cell `c` qualifies as a floor cell if it's a passable air cell whose
/// support comes from below (solid, water) or from in-cell traversal
/// (ladder, rail).
///
/// **Headroom is not checked here.** It used to be — required 2 cells
/// of vertical clearance — but that meant placing a head-height block
/// inside an enclosed room would disqualify the cell below from being a
/// floor cell, which removed it from the floor set, which made the
/// perimeter check at floor Y see an air-perimeter cell (the now-
/// demoted floor) and flunk the room's enclosure entirely. The room is
/// still enclosed; the player just bumps their head. Headroom is a
/// pathing/standability concern, not a room-detection one — it'll
/// belong to NPC AI when that lands.
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
        // Pure 2D fill at the seed Y. ±Y traversal is *intentionally* off
        // for now — with our "any solid block has support_below" tagging,
        // a 1-high wall's top would qualify as a floor cell, and ±Y step
        // would let the fill leap onto wall tops and back down outside.
        // Cost: each Y level becomes its own room when terrain inside an
        // enclosure is uneven. Worth it because 1-high wall enclosures
        // are a much more common user expectation than multi-Y unions.
        // A future `step` block tag (or `wall_only` tag, or a structural
        // wall-detector) can re-enable selective ±Y traversal.
        for [dx, dz] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
            let n = c + IVec3::new(dx, 0, dz);
            if !visited.insert(n) {
                continue;
            }
            if is_floor_cell(n, get_block, reg) {
                queue.push_back(n);
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

    // Door count: walk the floor's horizontal boundary (cells *not* in the
    // fill that are directly adjacent to a floor cell at the same Y) and
    // count distinct cells whose block has `walkable_boundary` set. A
    // single 2-tall door's lower block is at floor-Y, which is what the
    // boundary walk encounters. Distinct cells, so a door adjacent to
    // multiple floor cells still counts once.
    let floor_set: HashSet<IVec3> = floor_cells.iter().copied().collect();
    let mut door_cells: HashSet<IVec3> = HashSet::new();
    for &c in floor_cells {
        for dir in [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z] {
            let n = c + dir;
            if floor_set.contains(&n) {
                continue;
            }
            if reg.def(get_block(n)).flags.walkable_boundary {
                door_cells.insert(n);
            }
        }
    }
    let door_count = door_cells.len() as u32;

    // Bottom-up enclosure walk. From the floor's Y, scan layer by layer
    // upward. A layer counts as "enclosed" iff every perimeter cell at
    // that Y is solid (the walls extend) AND there's still some interior
    // air at that Y (we haven't hit the roof yet). The room ends when
    // either the walls give out (open above) or the interior closes
    // (capped by a roof).
    //
    // This replaces the older per-column headroom probe — the old way
    // confused "walled yard, infinite air column" with "tall hall," and
    // worse, it had no notion of "wall extends here," so a 1-high wall
    // and a 5-high wall produced the same signature. Now the signature
    // tells us the actual built height.
    let floor_y = floor_cells[0].y;
    let floor_xz: HashSet<(i32, i32)> = floor_cells.iter().map(|c| (c.x, c.z)).collect();
    let mut perimeter_xz: HashSet<(i32, i32)> = HashSet::new();
    for &(x, z) in &floor_xz {
        for [dx, dz] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
            let nx = x + dx;
            let nz = z + dz;
            if !floor_xz.contains(&(nx, nz)) {
                perimeter_xz.insert((nx, nz));
            }
        }
    }
    // External vs internal perimeter. A perimeter cell *outside* the
    // floor's XZ bbox is part of the room's exterior wall ring — it must
    // be solid at every Y for the room to be enclosed. A perimeter cell
    // *inside* the bbox is a column / pillar the player has placed on
    // the floor (a single block carved out of a previously-floor cell);
    // requiring walls above it would mean every interior column kicks
    // the room out of small_house back to walled_yard, which is
    // surprising. So we only enforce the exterior perimeter for the
    // bound check at higher Ys.
    let external_perimeter: HashSet<(i32, i32)> = perimeter_xz
        .iter()
        .copied()
        .filter(|&(x, z)| x < min.x || x > max.x || z < min.z || z > max.z)
        .collect();
    // Floor must be enclosed at its OWN Y too. Without this check, a fill
    // that runs along a wall ring (cells whose support is the wall block
    // below them) would count as "enclosed" — its perimeter at floor Y
    // is air on both sides (interior + exterior), not walls. Real rooms
    // have walls (or terrain, or other solids) directly bounding the
    // floor cells in 4-cardinal at the floor's Y. Use the *full* perimeter
    // here (including internal columns) — at floor Y, an internal column
    // is itself solid (it's the placed block), so this still passes for
    // legitimate column placements.
    let perimeter_at_floor_solid = perimeter_xz
        .iter()
        .all(|&(x, z)| !get_block(IVec3::new(x, floor_y, z)).is_empty());
    let mut enclosure_height: u32 = if perimeter_at_floor_solid { 1 } else { 0 };
    let mut has_roof = false;
    let mut tag_counts: HashMap<_, u32> = HashMap::new();
    for dy in 1..ROOF_PROBE_CAP {
        if !perimeter_at_floor_solid {
            // Floor isn't enclosed; don't bother probing higher layers.
            break;
        }
        let y = floor_y + dy;
        // Roof check: every interior column position is solid at this y.
        let interior_all_solid = floor_xz
            .iter()
            .all(|&(x, z)| !get_block(IVec3::new(x, y, z)).is_empty());
        if interior_all_solid {
            has_roof = true;
            break;
        }
        // Bound check: every *exterior* perimeter position is solid at
        // this y (the wall extends up to here). Internal column positions
        // are exempt — they don't need walls above them.
        let bounded = external_perimeter
            .iter()
            .all(|&(x, z)| !get_block(IVec3::new(x, y, z)).is_empty());
        if !bounded {
            break;
        }
        // Layer is enclosed. Collect tags on any solid blocks inside the
        // interior at this y (furniture, decorations).
        for &(x, z) in &floor_xz {
            let slot = get_block(IVec3::new(x, y, z));
            if slot.is_empty() {
                continue;
            }
            let def = reg.def(slot);
            for tag in &def.tags {
                *tag_counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        enclosure_height += 1;
    }
    let volume = enclosure_height.saturating_mul(floor_cells.len() as u32);

    // Walkable cells = floor cells with player-height clearance above.
    // Floor set itself stays geometric (so the room stays enclosed even
    // when the player builds something at head height); this counts the
    // subset that's actually standable, which the FloorArea constraint
    // reads as "minimum room size."
    let walkable_count = floor_cells
        .iter()
        .filter(|&&c| {
            let above = get_block(c + IVec3::Y);
            let above_def = reg.def(above);
            above.is_empty() || above_def.flags.support_in_cell
        })
        .count() as u32;

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
        walkable_count: Some(walkable_count),
        enclosure_height: Some(enclosure_height),
        has_roof: Some(has_roof),
        door_count: Some(door_count),
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
            // Walkable count when present, fall back to geometric for
            // connective-domain signatures or older mod-emitted ones.
            let v = sig.walkable_count.unwrap_or(sig.cell_count);
            min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
        }
        Constraint::EnclosureHeight { min, max } => {
            let v = sig.enclosure_height.unwrap_or(0);
            min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
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
        Constraint::DoorCount { min, max } => {
            let v = sig.door_count.unwrap_or(0);
            v >= *min && max.is_none_or(|m| v <= m)
        }
        // Connective domain — no detector for it yet, so any pattern with
        // an `AdjacentPair` constraint can't match. Returning false keeps
        // the volumetric matcher from accidentally selecting one.
        Constraint::AdjacentPair { .. } => false,
    }
}
