//! Legacy pattern definitions retained until the capability content port.
//! The simple prototype detector is compiled only as an independent test oracle.
use bevy::prelude::*;
use block_junk_mod_api::rooms::{PatternDomain, RoomPattern, RoomPatternId};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
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

#[cfg(test)]
mod oracle {
    use super::*;
    use crate::blocks::{BlockRegistry, BlockSlot};
    use block_junk_mod_api::blocks::TagId;
    use block_junk_mod_api::rooms::{
        AdjacentPairCount, BBox, Constraint, FloorComposition, FloorKind, RoomSignature, TagCount,
    };
    use block_junk_mod_api::shared::BlockPos;
    use std::collections::VecDeque;
    const FLOOD_CAP: u32 = 4096;
    const ROOF_SCAN_CAP: i32 = 32;
    const HAS_ROOF_MIN_FRACTION: f32 = 0.85;
    fn overlap_keeps_identity(overlap: usize, old_len: usize, new_len: usize) -> bool {
        overlap > 0 && 2 * overlap >= old_len.min(new_len)
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

    /// Does this block bound a room volumetrically? Walls, doors, glass,
    /// furniture. Gated on flags rather than `!is_empty()` so future
    /// passable decor (torches, signs) doesn't read as a wall.
    fn bounds_room(slot: BlockSlot, reg: &BlockRegistry) -> bool {
        if slot.is_empty() {
            return false;
        }
        let f = reg.def(slot).flags;
        f.room_boundary || f.solid
    }

    /// Is `c` a 1-wide gap in a wall run along `axis` (the axis PERPENDICULAR
    /// to travel)? Both flanking cells at floor Y must be `room_boundary`
    /// blocks — walls, doors, terrain — NOT merely solid. Furniture (a bed is
    /// solid but not room_boundary) must not flank a choke, or a bed placed
    /// one cell from a wall would carve the strip behind it into "doorways"
    /// and split the room. A choke cell is a *virtual doorway*: the fill
    /// stops at it instead of leaking through, and — when it has walkable
    /// headroom — it counts toward `door_count`. 2-wide openings are
    /// breaches and still leak.
    fn is_choke_along(
        c: IVec3,
        axis: IVec3,
        get_block: &impl Fn(IVec3) -> BlockSlot,
        reg: &BlockRegistry,
    ) -> bool {
        let wall_flank = |w: IVec3| {
            let slot = get_block(w);
            !slot.is_empty() && reg.def(slot).flags.room_boundary
        };
        wall_flank(c + axis) && wall_flank(c - axis)
    }

    /// Result of one floor flood-fill.
    struct FloorFill {
        cells: Vec<IVec3>,
        /// Virtual-doorway cells the fill stopped at (1-wide wall gaps).
        /// These seal the perimeter check; the walkable subset also counts
        /// as doors.
        boundary_gaps: HashSet<IVec3>,
        /// Subset of `boundary_gaps` with walkable headroom (the cell above
        /// is passable) — an actor-sized doorway, not a floor-level slit.
        doorways: HashSet<IVec3>,
    }

    fn flood_fill_floor(
        seed: IVec3,
        get_block: &impl Fn(IVec3) -> BlockSlot,
        reg: &BlockRegistry,
        cap: u32,
        visited: &mut HashSet<IVec3>,
    ) -> Option<FloorFill> {
        debug_assert!(is_floor_cell(seed, get_block, reg));
        let mut queue: VecDeque<IVec3> = VecDeque::new();
        let mut out: Vec<IVec3> = Vec::new();
        let mut boundary_gaps: HashSet<IVec3> = HashSet::new();
        let mut doorways: HashSet<IVec3> = HashSet::new();
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
                let d = IVec3::new(dx, 0, dz);
                let n = c + d;
                if visited.contains(&n) {
                    continue;
                }
                if !is_floor_cell(n, get_block, reg) {
                    continue;
                }
                // Perpendicular flanks both walls ⇒ virtual doorway: the
                // fill treats it as a boundary rather than leaking into
                // whatever lies beyond (outdoors, the next room). Not
                // inserted into `visited` — the region on the far side must
                // evaluate it independently to count the shared door.
                let perp = IVec3::new(d.z, 0, d.x);
                if is_choke_along(n, perp, get_block, reg) {
                    boundary_gaps.insert(n);
                    let above = get_block(n + IVec3::Y);
                    if above.is_empty() || reg.def(above).flags.support_in_cell {
                        doorways.insert(n);
                    }
                    continue;
                }
                visited.insert(n);
                queue.push_back(n);
            }
        }
        Some(FloorFill {
            cells: out,
            boundary_gaps,
            doorways,
        })
    }

    fn compute_signature(
        fill: &FloorFill,
        get_block: &impl Fn(IVec3) -> BlockSlot,
        reg: &BlockRegistry,
    ) -> RoomSignature {
        let floor_cells = &fill.cells;
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
        // count distinct cells whose block has `walkable_boundary` set, plus
        // the fill's virtual doorways (1-wide wall openings with walkable
        // headroom). The two sets are disjoint — doorway gaps are air cells.
        // Distinct cells, so a door adjacent to multiple floor cells still
        // counts once.
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
        let door_count = (door_cells.len() + fill.doorways.len()) as u32;

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
        // floor's XZ bbox is part of the room's exterior wall ring. A
        // perimeter cell *inside* the bbox is a column / pillar / furniture
        // block sitting on the floor (carved out of a previously-floor cell).
        let external_perimeter: HashSet<(i32, i32)> = perimeter_xz
            .iter()
            .copied()
            .filter(|&(x, z)| x < min.x || x > max.x || z < min.z || z > max.z)
            .collect();
        // The floor must be sealed at its own Y: every perimeter cell either
        // bounds the room (wall, door block, terrain, furniture) or is one of
        // the fill's virtual doorways. Without this, a fill running along a
        // wall top would read as enclosed.
        let perimeter_sealed = perimeter_xz.iter().all(|&(x, z)| {
            bounds_room(get_block(IVec3::new(x, floor_y, z)), reg)
                || fill.boundary_gaps.contains(&IVec3::new(x, floor_y, z))
        });

        // Per-column ceiling probe. Each floor column independently looks up
        // for the first bounding block within ROOF_SCAN_CAP layers — so a
        // pitched roof, a skylight, or the air gap above a door block no
        // longer voids the whole room the way the old "one all-solid layer,
        // walls solid at every layer" walk did.
        let mut ceilings: Vec<u32> = Vec::new();
        if perimeter_sealed {
            for &(x, z) in &floor_xz {
                let ceiling = (1..=ROOF_SCAN_CAP)
                    .find(|&dy| bounds_room(get_block(IVec3::new(x, floor_y + dy, z)), reg));
                if let Some(dy) = ceiling {
                    ceilings.push(dy as u32);
                }
            }
        }
        let roof_fraction = if floor_xz.is_empty() || !perimeter_sealed {
            0.0
        } else {
            ceilings.len() as f32 / floor_xz.len() as f32
        };
        let has_roof = roof_fraction >= HAS_ROOF_MIN_FRACTION;

        // Interior height. Roofed: median clear headroom under the ceiling
        // (median, not min, so one low beam doesn't reclassify the room).
        // Unroofed: minimum wall run along the external perimeter, doorway
        // columns exempt. Unsealed regions read 0 and match nothing that
        // requires enclosure.
        let enclosure_height: u32 = if !perimeter_sealed {
            0
        } else if has_roof {
            let mut sorted = ceilings.clone();
            sorted.sort_unstable();
            sorted[(sorted.len() - 1) / 2]
        } else {
            external_perimeter
                .iter()
                .filter(|&&(x, z)| !fill.boundary_gaps.contains(&IVec3::new(x, floor_y, z)))
                .map(|&(x, z)| {
                    (0..=ROOF_SCAN_CAP)
                        .take_while(|&dy| {
                            bounds_room(get_block(IVec3::new(x, floor_y + dy, z)), reg)
                        })
                        .count() as u32
                })
                .min()
                .unwrap_or(0)
        };
        let volume = enclosure_height.saturating_mul(floor_cells.len() as u32);

        // Interior contents scan for tags: every column of the region —
        // floor columns AND internal-perimeter columns (furniture, pillars;
        // these were carved out of the floor set by the very block we want
        // to count) — from floor Y up through the interior height. Multi-
        // cell block entities are normalised to placement units by their
        // footprint size so a 2-cell bed counts as ONE bed.
        let mut slot_cell_counts: HashMap<BlockSlot, u32> = HashMap::new();
        let mut tagged_cells: HashMap<IVec3, BlockSlot> = HashMap::new();
        let internal_perimeter = perimeter_xz.difference(&external_perimeter);
        let scan_top = enclosure_height.saturating_sub(1) as i32;
        for &(x, z) in floor_xz.iter().chain(internal_perimeter) {
            for dy in 0..=scan_top {
                let cell = IVec3::new(x, floor_y + dy, z);
                let slot = get_block(cell);
                if !slot.is_empty() {
                    *slot_cell_counts.entry(slot).or_insert(0) += 1;
                    if !reg.def(slot).tags.is_empty() {
                        tagged_cells.insert(cell, slot);
                    }
                }
            }
        }
        let mut tag_counts: HashMap<_, u32> = HashMap::new();
        for (slot, cells) in slot_cell_counts {
            let def = reg.def(slot);
            if def.tags.is_empty() {
                continue;
            }
            let units = cells.div_ceil(def.footprint.len().max(1) as u32);
            for tag in &def.tags {
                *tag_counts.entry(tag.clone()).or_insert(0) += units;
            }
        }

        // Tag adjacency: for each ordered tag pair (a, b), how many tag-a
        // placements stand directly next to a tag-b cell (4-dir horizontal,
        // same layer). Counted in cells first — a cell contributes once per
        // (a, b) key no matter how many b neighbours it touches — then
        // normalised to placement units per slot with the same div_ceil rule
        // as tag_counts. Exact for 1-cell a-blocks (seats); a multi-cell
        // a-placement only partially touching b errs LOW, which is the safe
        // direction for pattern matching.
        let mut pair_slot_cells: HashMap<(TagId, TagId), HashMap<BlockSlot, u32>> = HashMap::new();
        for (&cell, &slot_a) in &tagged_cells {
            let mut seen_here: HashSet<(TagId, TagId)> = HashSet::new();
            for dir in [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z] {
                let Some(&slot_b) = tagged_cells.get(&(cell + dir)) else {
                    continue;
                };
                for ta in &reg.def(slot_a).tags {
                    for tb in &reg.def(slot_b).tags {
                        let key = (ta.clone(), tb.clone());
                        if seen_here.insert(key.clone()) {
                            *pair_slot_cells
                                .entry(key)
                                .or_default()
                                .entry(slot_a)
                                .or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        let mut adjacent_pairs: Vec<AdjacentPairCount> = pair_slot_cells
            .into_iter()
            .map(|((a, b), slots)| AdjacentPairCount {
                a,
                b,
                count: slots
                    .into_iter()
                    .map(|(slot, cells)| {
                        cells.div_ceil(reg.def(slot).footprint.len().max(1) as u32)
                    })
                    .sum(),
            })
            .collect();
        adjacent_pairs.sort_by(|l, r| (&l.a.0, &l.b.0).cmp(&(&r.a.0, &r.b.0)));

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
            roof_fraction: Some(roof_fraction),
            door_count: Some(door_count),
            floor_composition: Some(comp),
            tag_counts: tag_counts
                .into_iter()
                .map(|(tag, count)| TagCount { tag, count })
                .collect(),
            adjacent_pairs,
        }
    }

    /// Find the deepest matching pattern (with the parent chain's constraints
    /// also satisfied), breaking ties by `priority` then registration order.
    fn match_pattern(sig: &RoomSignature, registry: &RoomPatternRegistry) -> Option<RoomPatternId> {
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
            Constraint::RoofFraction { min, max } => {
                let v = sig.roof_fraction.unwrap_or(0.0);
                min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
            }
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
            Constraint::AdjacentPair { a, b, min } => {
                let count = sig
                    .adjacent_pairs
                    .iter()
                    .find(|p| &p.a == a && &p.b == b)
                    .map(|p| p.count)
                    .unwrap_or(0);
                count >= *min
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use block_junk_mod_api::blocks::BlockDef;

        // ---------- harness ----------

        /// Named slots for the test registry, in registration order.
        /// Engine air occupies slot zero before these definitions.
        struct Slots {
            stone: BlockSlot,
            door: BlockSlot,
            bed: BlockSlot,
            seat: BlockSlot,
            table: BlockSlot,
        }

        fn test_registry() -> (BlockRegistry, Slots) {
            let defs: Vec<BlockDef> = serde_json::from_value(serde_json::json!([
                {
                    "id": "test:stone",
                    "display_name": "Stone",
                    "flags": { "solid": true, "room_boundary": true, "support_below": true },
                    "color": [0.5, 0.5, 0.5]
                },
                {
                    "id": "test:door",
                    "display_name": "Door",
                    "flags": { "room_boundary": true, "walkable_boundary": true },
                    "color": [0.4, 0.2, 0.1]
                },
                {
                    "id": "test:bed",
                    "display_name": "Bed",
                    "flags": { "solid": true, "support_below": true },
                    "tags": ["vanilla:bed"],
                    "footprint": [[0, 0, 0], [1, 0, 0]],
                    "color": [0.4, 0.2, 0.1]
                },
                {
                    "id": "test:seat",
                    "display_name": "Seat",
                    "flags": { "solid": true, "support_below": true },
                    "tags": ["vanilla:seat"],
                    "color": [0.4, 0.2, 0.1]
                },
                {
                    "id": "test:table",
                    "display_name": "Table",
                    "flags": { "solid": true, "support_below": true },
                    "tags": ["vanilla:table"],
                    "footprint": [[0, 0, 0], [1, 0, 0]],
                    "color": [0.4, 0.2, 0.1]
                }
            ]))
            .expect("test block defs deserialize");
            let (reg, _) = BlockRegistry::build(defs).expect("test registry builds");
            let slots = Slots {
                stone: reg.slot_of(&"test:stone".into()).unwrap(),
                door: reg.slot_of(&"test:door".into()).unwrap(),
                bed: reg.slot_of(&"test:bed".into()).unwrap(),
                seat: reg.slot_of(&"test:seat".into()).unwrap(),
                table: reg.slot_of(&"test:table".into()).unwrap(),
            };
            (reg, slots)
        }

        fn vanilla_like_patterns() -> RoomPatternRegistry {
            use block_junk_mod_api::rooms::Constraint as C;
            let p =
                |id: &str, parent: Option<&str>, priority: i32, constraints: Vec<C>| RoomPattern {
                    id: id.into(),
                    display_name: id.to_string(),
                    parent: parent.map(Into::into),
                    domain: PatternDomain::Volumetric,
                    constraints,
                    priority,
                };
            RoomPatternRegistry::build(vec![
                p(
                    "enclosed_space",
                    None,
                    0,
                    vec![
                        C::FloorArea {
                            min: Some(4),
                            max: Some(4096),
                        },
                        C::EnclosureHeight {
                            min: Some(1),
                            max: None,
                        },
                        C::DoorCount { min: 1, max: None },
                    ],
                ),
                p(
                    "walled_yard",
                    Some("enclosed_space"),
                    0,
                    vec![
                        C::RoofFraction {
                            min: None,
                            max: Some(0.5),
                        },
                        C::FloorFraction {
                            surface: FloorKind::Solid,
                            min: 0.6,
                        },
                    ],
                ),
                p(
                    "small_house",
                    Some("enclosed_space"),
                    1,
                    vec![
                        C::HasRoof { required: true },
                        C::EnclosureHeight {
                            min: Some(2),
                            max: None,
                        },
                        C::FloorArea {
                            min: None,
                            max: Some(50),
                        },
                        C::FloorFraction {
                            surface: FloorKind::Solid,
                            min: 0.8,
                        },
                    ],
                ),
                p(
                    "bedroom",
                    Some("small_house"),
                    2,
                    vec![C::TagCount {
                        tag: block_junk_mod_api::blocks::TagId("vanilla:bed".into()),
                        min: 1,
                        max: None,
                    }],
                ),
                p(
                    "dining",
                    Some("small_house"),
                    3,
                    vec![
                        C::TagCount {
                            tag: block_junk_mod_api::blocks::TagId("vanilla:table".into()),
                            min: 1,
                            max: None,
                        },
                        C::AdjacentPair {
                            a: block_junk_mod_api::blocks::TagId("vanilla:seat".into()),
                            b: block_junk_mod_api::blocks::TagId("vanilla:table".into()),
                            min: 2,
                        },
                    ],
                ),
            ])
            .expect("test patterns build")
        }

        fn pair_count(sig: &RoomSignature, a: &str, b: &str) -> u32 {
            sig.adjacent_pairs
                .iter()
                .find(|p| p.a.as_str() == a && p.b.as_str() == b)
                .map(|p| p.count)
                .unwrap_or(0)
        }

        #[derive(Default)]
        struct World {
            blocks: HashMap<IVec3, BlockSlot>,
        }

        impl World {
            fn set(&mut self, x: i32, y: i32, z: i32, slot: BlockSlot) {
                self.blocks.insert(IVec3::new(x, y, z), slot);
            }

            fn clear(&mut self, x: i32, y: i32, z: i32) {
                self.blocks.remove(&IVec3::new(x, y, z));
            }

            fn getter(&self) -> impl Fn(IVec3) -> BlockSlot + '_ {
                |w| self.blocks.get(&w).copied().unwrap_or(BlockSlot::EMPTY)
            }
        }

        /// Stone ground at y=0 over `0..=5` square, stone wall ring at
        /// `y = 1..=wall_h` on the x/z ∈ {0,5} border. Interior floor cells
        /// are the 4×4 at y=1. No roof, no door — callers add those.
        fn walled_box(world: &mut World, s: &Slots, wall_h: i32) {
            for x in 0..=5 {
                for z in 0..=5 {
                    world.set(x, 0, z, s.stone);
                    let on_ring = x == 0 || x == 5 || z == 0 || z == 5;
                    if on_ring {
                        for y in 1..=wall_h {
                            world.set(x, y, z, s.stone);
                        }
                    }
                }
            }
        }

        fn flat_roof(world: &mut World, s: &Slots, y: i32) {
            for x in 0..=5 {
                for z in 0..=5 {
                    world.set(x, y, z, s.stone);
                }
            }
        }

        /// Run fill + signature + match from an interior seed.
        fn detect(
            world: &World,
            reg: &BlockRegistry,
            patterns: &RoomPatternRegistry,
            seed: IVec3,
        ) -> Option<(RoomSignature, Option<RoomPatternId>)> {
            let get = world.getter();
            let mut visited = HashSet::new();
            let fill = flood_fill_floor(seed, &get, reg, FLOOD_CAP, &mut visited)?;
            let sig = compute_signature(&fill, &get, reg);
            let pattern = match_pattern(&sig, patterns);
            Some((sig, pattern))
        }

        fn pattern_name(p: &Option<RoomPatternId>) -> &str {
            p.as_ref().map(|id| id.as_str()).unwrap_or("<none>")
        }

        const SEED: IVec3 = IVec3::new(2, 1, 2);

        // ---------- enclosure + ceilings ----------

        #[test]
        fn box_with_door_block_is_small_house() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.door_count, Some(1));
            assert_eq!(sig.enclosure_height, Some(2));
            assert_eq!(sig.has_roof, Some(true));
            assert_eq!(pattern_name(&pat), "small_house");
        }

        #[test]
        fn open_doorway_bounds_the_fill_and_counts_as_a_door() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            // 1-wide, 2-high opening in the west wall: no door block at all.
            w.clear(0, 1, 2);
            w.clear(0, 2, 2);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill must not leak");
            assert_eq!(sig.cell_count, 16, "fill stayed inside the room");
            assert_eq!(sig.door_count, Some(1), "virtual doorway counted");
            assert_eq!(pattern_name(&pat), "small_house");
        }

        #[test]
        fn sealed_box_without_any_door_matches_nothing() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.door_count, Some(0));
            assert_eq!(pat, None, "no access point ⇒ not a room");
        }

        #[test]
        fn pitched_roof_is_still_a_house() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            w.set(0, 1, 2, s.door);
            // Two-level "pitched" roof: west half at y=3, east half at y=4.
            for x in 0..=5 {
                for z in 0..=5 {
                    w.set(x, if x <= 2 { 3 } else { 4 }, z, s.stone);
                }
            }
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.has_roof, Some(true), "per-column roof sees the pitch");
            assert_eq!(pattern_name(&pat), "small_house");
        }

        #[test]
        fn skylight_hole_is_still_a_house() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            w.clear(2, 3, 2); // one missing roof block over the interior
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            let fraction = sig.roof_fraction.expect("fraction populated");
            assert!(
                (fraction - 15.0 / 16.0).abs() < 1e-6,
                "15 of 16 columns roofed, got {fraction}"
            );
            assert_eq!(sig.has_roof, Some(true));
            assert_eq!(pattern_name(&pat), "small_house");
        }

        #[test]
        fn air_gap_above_door_block_does_not_unroof_the_house() {
            // The pre-2026-07 layer-walk regression in miniature: door block
            // at floor Y, nothing filling the wall at Y+1 over the door.
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            w.clear(0, 2, 2); // wall opening directly above the door
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.has_roof, Some(true));
            assert_eq!(sig.enclosure_height, Some(2));
            assert_eq!(pattern_name(&pat), "small_house");
        }

        #[test]
        fn crawlspace_is_not_a_room_at_all() {
            // Ceiling directly at head height: sealed and roofed, but no
            // floor cell has standing clearance, so walkable area is 0 and
            // nothing matches. A space the player can't stand in isn't a
            // room — the fix for "low ceilings feel wrong" is the R1
            // feedback/diagnosis surface, not loosening this.
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 1);
            flat_roof(&mut w, &s, 2);
            w.set(0, 1, 2, s.door);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.enclosure_height, Some(1));
            assert_eq!(sig.has_roof, Some(true), "roofed, just too low");
            assert_eq!(sig.walkable_count, Some(0), "nowhere to stand");
            assert_eq!(pat, None);
        }

        #[test]
        fn open_yard_with_low_walls_is_walled_yard() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 1);
            w.clear(0, 1, 2); // open doorway; sky above, so it's walkable
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.roof_fraction, Some(0.0));
            assert_eq!(sig.enclosure_height, Some(1));
            assert_eq!(sig.door_count, Some(1));
            assert_eq!(pattern_name(&pat), "walled_yard");
        }

        #[test]
        fn half_roofed_shell_is_neither_yard_nor_house() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            w.set(0, 1, 2, s.door);
            // Roof over 10 of 16 interior columns (the x ∈ {1,2} rows plus
            // two cells of the x == 3 row): fraction 0.625 — between the
            // yard's ≤ 0.5 and the house's ≥ 0.85.
            for x in 1..=2 {
                for z in 1..=4 {
                    w.set(x, 3, z, s.stone);
                }
            }
            w.set(3, 3, 1, s.stone);
            w.set(3, 3, 2, s.stone);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            let fraction = sig.roof_fraction.expect("fraction populated");
            assert!((fraction - 0.625).abs() < 1e-6, "got {fraction}");
            assert_eq!(sig.has_roof, Some(false));
            assert_eq!(pattern_name(&pat), "enclosed_space");
        }

        #[test]
        fn two_wide_gap_leaks_the_fill() {
            let (reg, s) = test_registry();
            let mut w = World::default();
            // Wide ground plane so the leak has room to exceed the cap.
            for x in -8..=13 {
                for z in -8..=13 {
                    w.set(x, 0, z, s.stone);
                }
            }
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            // 2-wide breach: neither cell is flanked on both sides.
            w.clear(0, 1, 2);
            w.clear(0, 1, 3);
            let get = w.getter();
            let mut visited = HashSet::new();
            let fill = flood_fill_floor(SEED, &get, &reg, 200, &mut visited);
            assert!(fill.is_none(), "2-wide breach must leak to the cap");
        }

        // ---------- furniture typing ----------

        #[test]
        fn bed_in_a_house_makes_a_bedroom() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            // 2-cell bed standing ON the floor (carves 2 floor cells).
            w.set(2, 1, 3, s.bed);
            w.set(3, 1, 3, s.bed);
            let (sig, pat) = detect(&w, &reg, &patterns, SEED).expect("fill succeeds");
            assert_eq!(sig.cell_count, 14, "bed cells left the floor set");
            let beds = sig
                .tag_counts
                .iter()
                .find(|tc| tc.tag.0 == "vanilla:bed")
                .map(|tc| tc.count);
            assert_eq!(beds, Some(1), "2-cell bed counts as ONE bed");
            assert_eq!(pattern_name(&pat), "bedroom");
        }

        #[test]
        fn seats_flanking_a_table_make_a_dining_room() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            // 2-cell table with a seat on the south side of each table cell.
            // Compact cluster — a wall-to-wall furniture row would slice the
            // 4×4 interior into two disconnected strips.
            w.set(2, 1, 2, s.table);
            w.set(3, 1, 2, s.table);
            w.set(2, 1, 3, s.seat);
            w.set(3, 1, 3, s.seat);
            let seed = IVec3::new(1, 1, 1);
            let (sig, pat) = detect(&w, &reg, &patterns, seed).expect("fill succeeds");
            assert_eq!(
                pair_count(&sig, "vanilla:seat", "vanilla:table"),
                2,
                "both seats touch the table"
            );
            assert_eq!(
                pair_count(&sig, "vanilla:table", "vanilla:seat"),
                1,
                "the table itself is ONE placement even with both cells touched"
            );
            assert_eq!(pattern_name(&pat), "dining");
        }

        #[test]
        fn seat_across_the_room_does_not_count_toward_dining() {
            let (reg, s) = test_registry();
            let patterns = vanilla_like_patterns();
            let mut w = World::default();
            walled_box(&mut w, &s, 2);
            flat_roof(&mut w, &s, 3);
            w.set(0, 1, 2, s.door);
            w.set(2, 1, 2, s.table);
            w.set(3, 1, 2, s.table);
            w.set(2, 1, 3, s.seat); // at the table
            w.set(4, 1, 4, s.seat); // corner, not touching
            let seed = IVec3::new(1, 1, 1);
            let (sig, pat) = detect(&w, &reg, &patterns, seed).expect("fill succeeds");
            assert_eq!(pair_count(&sig, "vanilla:seat", "vanilla:table"), 1);
            assert_eq!(
                pattern_name(&pat),
                "small_house",
                "one adjacent seat is not a dining room"
            );
        }

        // ---------- helpers ----------

        #[test]
        fn corridor_cell_reads_as_choke() {
            let (reg, s) = test_registry();
            let mut w = World::default();
            for x in 0..=6 {
                w.set(x, 0, 0, s.stone); // ground
                w.set(x, 1, -1, s.stone); // south wall
                w.set(x, 1, 1, s.stone); // north wall
            }
            let get = w.getter();
            assert!(is_choke_along(IVec3::new(3, 1, 0), IVec3::Z, &get, &reg));
            assert!(!is_choke_along(IVec3::new(3, 1, 0), IVec3::X, &get, &reg));
        }

        #[test]
        fn overlap_identity_thresholds() {
            // Single-cell trim of a 16-cell room keeps identity.
            assert!(overlap_keeps_identity(15, 16, 15));
            // Wholesale replacement (no shared cells) does not.
            assert!(!overlap_keeps_identity(0, 16, 16));
            // A room split in half: the 8-cell survivor keeps identity
            // against the old 16 (overlap 8 ≥ half of min(16, 8)).
            assert!(overlap_keeps_identity(8, 16, 8));
            // Tiny sliver of a big room grabbing its id: 2 of 40 shared,
            // new fill is 30 cells of mostly-new geometry — rejected.
            assert!(!overlap_keeps_identity(2, 40, 30));
        }
    }
}
