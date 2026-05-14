//! Cell-grid A* for grounded actors.
//!
//! Used by the NPC brain to find a path of foot cells from where the
//! actor stands to a target. Pure logic — no Bevy systems, queries, or
//! resources here, so this is unit-testable against a hand-rolled grid
//! and the same algorithm runs server-side today + (eventually) for
//! whatever else needs grounded navigation.
//!
//! Movement model: 4-directional (N/S/E/W) with ±1 step-up/down, head
//! clearance enforced, no jumping over gaps. Matches what the player
//! controller can do in `apply_walk_step`.
//!
//! Future hooks already in place:
//! - [`Walkability::cost`] returns 1.0 by default; the planned road-tag
//!   system will reduce cost for road cells, biasing the search toward
//!   roads without changing the algorithm.
//! - For very long routes between distant settlements, a separate
//!   road-graph layer would beat per-cell A*; this module stays the
//!   "local" tier in that hierarchy.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use bevy::math::IVec3;
use bevy::platform::collections::HashMap;

/// What the pathfinder needs to know about the world. Implemented by
/// the game-side wrapper around `ChunkMap`/`Chunk`; tests implement it
/// over a hand-rolled grid.
pub trait Walkability {
    /// True if `cell` is occupied by anything that blocks an actor's
    /// body. Unloaded chunks return `true` so the search doesn't commit
    /// to paths through unknown territory.
    fn is_solid(&self, cell: IVec3) -> bool;

    /// Per-cell movement cost. Default 1.0; future road tags lower it.
    fn cost(&self, _cell: IVec3) -> f32 {
        1.0
    }
}

/// True when an actor with a 1×2-cell body can stand at `foot_cell`:
/// foot cell empty, head cell empty, supporting cell solid.
pub fn standable<W: Walkability>(world: &W, foot: IVec3) -> bool {
    !world.is_solid(foot)
        && !world.is_solid(foot + IVec3::Y)
        && world.is_solid(foot - IVec3::Y)
}

/// String-pulling smoother. Given an A* path that zig-zags through
/// 4-directional cells, drop any waypoint that the straight line from
/// the previous-kept cell to the next cell could clear. Result reads
/// as "diagonals where the world allows them, kinks only where
/// necessary." Fixes the visible wobble of an NPC chasing a stair-
/// stepped path on open ground.
///
/// Step-up / step-down cells are deliberately preserved — vertical
/// transitions need to remain explicit so the NPC brain can detect
/// them and trigger a jump. `line_of_sight` returns false across any
/// Y change, which keeps the surrounding kinks intact.
pub fn smooth_path<W: Walkability>(path: Vec<IVec3>, world: &W) -> Vec<IVec3> {
    if path.len() <= 2 {
        return path;
    }
    let mut out = Vec::with_capacity(path.len());
    out.push(path[0]);
    let mut anchor = path[0];
    let mut i = 1;
    // Loop invariant: we're considering whether path[i] is necessary
    // given that the line from `anchor` to path[i+1] would otherwise
    // bypass it. The last cell is always pushed unconditionally
    // outside the loop, so the loop only inspects interior cells.
    while i < path.len() - 1 {
        if line_of_sight(anchor, path[i + 1], world) {
            // path[i] is redundant — the line from anchor to
            // path[i+1] passes through walkable cells.
            i += 1;
        } else {
            // path[i] is a load-bearing kink. Commit it as the new
            // anchor and keep scanning.
            out.push(path[i]);
            anchor = path[i];
            i += 1;
        }
    }
    out.push(*path.last().expect("len > 2 already checked"));
    out
}

/// True if a body can travel in a straight line from cell `a` to cell
/// `b` without entering any non-standable cell. Same-Y only — vertical
/// transitions stay as kinks so the brain can jump them. Uses
/// Amanatides-Woo grid traversal so every cell the line passes
/// through is checked, including diagonal corner-cuts.
fn line_of_sight<W: Walkability>(a: IVec3, b: IVec3, world: &W) -> bool {
    if a.y != b.y {
        return false;
    }
    if a == b {
        return true;
    }
    let y = a.y;
    let ax = a.x as f32 + 0.5;
    let az = a.z as f32 + 0.5;
    let bx = b.x as f32 + 0.5;
    let bz = b.z as f32 + 0.5;
    let dx = bx - ax;
    let dz = bz - az;

    let mut cell = IVec3::new(ax.floor() as i32, y, az.floor() as i32);
    let end = IVec3::new(bx.floor() as i32, y, bz.floor() as i32);

    let step_x = if dx > 0.0 { 1 } else if dx < 0.0 { -1 } else { 0 };
    let step_z = if dz > 0.0 { 1 } else if dz < 0.0 { -1 } else { 0 };

    // Distance (in t) to the next X / Z grid line. With cells aligned
    // to integer coords and centres at `+0.5`, the next boundary in
    // the positive direction is `cell + 1`; in the negative direction
    // it's `cell` itself.
    let next_x = if dx > 0.0 {
        (cell.x + 1) as f32 - ax
    } else {
        ax - cell.x as f32
    };
    let next_z = if dz > 0.0 {
        (cell.z + 1) as f32 - az
    } else {
        az - cell.z as f32
    };
    let mut t_max_x = if dx.abs() > f32::EPSILON {
        next_x / dx.abs()
    } else {
        f32::INFINITY
    };
    let mut t_max_z = if dz.abs() > f32::EPSILON {
        next_z / dz.abs()
    } else {
        f32::INFINITY
    };
    let t_delta_x = if dx.abs() > f32::EPSILON {
        1.0 / dx.abs()
    } else {
        f32::INFINITY
    };
    let t_delta_z = if dz.abs() > f32::EPSILON {
        1.0 / dz.abs()
    } else {
        f32::INFINITY
    };

    if !standable(world, cell) {
        return false;
    }
    while cell != end {
        // When the line passes exactly through a corner (t_max_x ==
        // t_max_z) we step diagonally and verify both flanking cells.
        // A body that physically traverses the corner clips both, so
        // either being solid must invalidate the line. Without this,
        // smoothing would happily route through a 1-cell-wide opening
        // between two walls.
        if (t_max_x - t_max_z).abs() < f32::EPSILON
            && t_max_x.is_finite()
            && t_max_z.is_finite()
        {
            let flank_x = IVec3::new(cell.x + step_x, y, cell.z);
            let flank_z = IVec3::new(cell.x, y, cell.z + step_z);
            if !standable(world, flank_x) || !standable(world, flank_z) {
                return false;
            }
            t_max_x += t_delta_x;
            t_max_z += t_delta_z;
            cell.x += step_x;
            cell.z += step_z;
        } else if t_max_x < t_max_z {
            t_max_x += t_delta_x;
            cell.x += step_x;
        } else {
            t_max_z += t_delta_z;
            cell.z += step_z;
        }
        if !standable(world, cell) {
            return false;
        }
    }
    true
}

/// Drop straight down from `from` looking for the highest standable
/// foot cell within `max_drop` cells. Used by the brain to project a
/// random XZ target onto the actual surface — picking a random Y rarely
/// lands somewhere standable on its own.
pub fn nearest_standable_below<W: Walkability>(
    world: &W,
    from: IVec3,
    max_drop: i32,
) -> Option<IVec3> {
    for dy in 0..=max_drop {
        let cell = from - IVec3::new(0, dy, 0);
        if standable(world, cell) {
            return Some(cell);
        }
    }
    None
}

/// Find a path of foot cells from `start` to `goal`. Returns the path
/// inclusive of both endpoints, or `None` if `goal` is unreachable
/// within the budget.
///
/// `max_nodes` caps the number of cells expanded — a hard CPU ceiling
/// for unreachable goals. `max_path_len` is the allowed g-score, which
/// for unit-cost cells is just "max number of steps." The wander
/// fallback in the brain handles the `None` case.
pub fn find_path<W: Walkability>(
    start: IVec3,
    goal: IVec3,
    world: &W,
    max_nodes: usize,
    max_path_len: usize,
) -> Option<Vec<IVec3>> {
    if start == goal {
        return Some(vec![start]);
    }
    if !standable(world, start) || !standable(world, goal) {
        return None;
    }

    let mut open: BinaryHeap<Frontier> = BinaryHeap::new();
    let mut g_score: HashMap<IVec3, f32> = HashMap::default();
    let mut came_from: HashMap<IVec3, IVec3> = HashMap::default();
    g_score.insert(start, 0.0);
    open.push(Frontier {
        cell: start,
        f: heuristic(start, goal),
    });
    let max_g = max_path_len as f32;
    let mut expanded = 0usize;

    while let Some(Frontier { cell, .. }) = open.pop() {
        if cell == goal {
            return Some(reconstruct(&came_from, cell));
        }
        expanded += 1;
        if expanded > max_nodes {
            return None;
        }
        let g_cell = g_score[&cell];
        for (next, step_cost) in step_neighbours(world, cell) {
            let tentative = g_cell + step_cost;
            if tentative > max_g {
                continue;
            }
            let prev = g_score.get(&next).copied().unwrap_or(f32::INFINITY);
            // Strictly better tentative cost — `<` (not `<=`) so we
            // don't bounce equivalent ties through the heap forever.
            if tentative < prev {
                g_score.insert(next, tentative);
                came_from.insert(next, cell);
                let f = tentative + heuristic(next, goal);
                open.push(Frontier { cell: next, f });
            }
        }
    }
    None
}

const STEP_DIRS: [IVec3; 4] = [
    IVec3::new(1, 0, 0),
    IVec3::new(-1, 0, 0),
    IVec3::new(0, 0, 1),
    IVec3::new(0, 0, -1),
];

/// Step-up cells cost slightly more than flat steps so the search
/// prefers a longer flat detour over an unnecessary climb.
const STEP_UP_PREMIUM: f32 = 1.4;

/// Walkable neighbours of `from` for a 4-directional walker that can
/// step up or down 1 cell. Yields (destination_cell, step_cost). The
/// destination's per-cell cost (e.g. road bias) is multiplied in by
/// the caller via `world.cost(next)` at the use site.
fn step_neighbours<'a, W: Walkability>(
    world: &'a W,
    from: IVec3,
) -> impl Iterator<Item = (IVec3, f32)> + 'a {
    STEP_DIRS.iter().filter_map(move |&dir| {
        let same = from + dir;
        if standable(world, same) {
            return Some((same, world.cost(same)));
        }
        // Step up: head clearance for the *current* tile (cell two
        // above the foot must be empty so the actor doesn't bonk
        // ascending through it). Destination is one above `same`.
        let up = same + IVec3::Y;
        let head_clear_for_climb = !world.is_solid(from + IVec3::new(0, 2, 0));
        if head_clear_for_climb && standable(world, up) {
            return Some((up, world.cost(up) * STEP_UP_PREMIUM));
        }
        // Step down: same is empty (we walk through it on the way
        // down) and the cell below is standable.
        let down = same - IVec3::Y;
        if !world.is_solid(same) && standable(world, down) {
            return Some((down, world.cost(down)));
        }
        None
    })
}

/// Manhattan distance — admissible for 4-directional grids with unit
/// step costs. The step-up premium is >1, but A* still terminates with
/// an optimal path under an admissible heuristic; the only consequence
/// of a strict-but-not-tight heuristic is more nodes expanded.
fn heuristic(a: IVec3, b: IVec3) -> f32 {
    let d = (a - b).abs();
    (d.x + d.y + d.z) as f32
}

fn reconstruct(came_from: &HashMap<IVec3, IVec3>, mut cell: IVec3) -> Vec<IVec3> {
    let mut path = vec![cell];
    while let Some(&prev) = came_from.get(&cell) {
        path.push(prev);
        cell = prev;
    }
    path.reverse();
    path
}

#[derive(Clone, Copy)]
struct Frontier {
    cell: IVec3,
    f: f32,
}

// BinaryHeap is a max-heap; reverse the order so smaller f wins. NaN
// f-scores would tie via Equal, which is fine — they shouldn't occur
// (heuristic + g_score are both finite for valid cells).
impl PartialEq for Frontier {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f
    }
}
impl Eq for Frontier {}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::platform::collections::HashSet;

    /// Tiny test world: any cell explicitly listed in `solid` is a
    /// block; everything else is air. Saves the test cases from
    /// listing the entire empty world.
    struct GridWorld {
        solid: HashSet<IVec3>,
    }

    impl GridWorld {
        fn floor_at(y: i32) -> Self {
            // Ground plane from x,z in [-50, 50] at the given y.
            let mut solid = HashSet::default();
            for x in -50..=50 {
                for z in -50..=50 {
                    solid.insert(IVec3::new(x, y, z));
                }
            }
            Self { solid }
        }

        fn add_wall(&mut self, min: IVec3, max: IVec3) {
            for x in min.x..=max.x {
                for y in min.y..=max.y {
                    for z in min.z..=max.z {
                        self.solid.insert(IVec3::new(x, y, z));
                    }
                }
            }
        }
    }

    impl Walkability for GridWorld {
        fn is_solid(&self, cell: IVec3) -> bool {
            self.solid.contains(&cell)
        }
    }

    #[test]
    fn straight_line_on_open_floor() {
        let world = GridWorld::floor_at(0);
        // Feet stand at y=1 (the cell whose bottom is the floor's top face).
        let path = find_path(IVec3::new(0, 1, 0), IVec3::new(5, 1, 0), &world, 1000, 100)
            .expect("reachable");
        assert_eq!(path.first(), Some(&IVec3::new(0, 1, 0)));
        assert_eq!(path.last(), Some(&IVec3::new(5, 1, 0)));
        // Manhattan distance is 5; A* finds an optimal 5-step path
        // (start + 5 steps = 6 cells inclusive).
        assert_eq!(path.len(), 6);
    }

    #[test]
    fn detour_around_wall() {
        let mut world = GridWorld::floor_at(0);
        // 1-cell-thick, 2-cell-tall wall blocking the direct line.
        world.add_wall(IVec3::new(2, 1, -2), IVec3::new(2, 2, 2));
        let path = find_path(IVec3::new(0, 1, 0), IVec3::new(5, 1, 0), &world, 5000, 100)
            .expect("reachable around the wall");
        // Verify every cell on the path is standable.
        for cell in &path {
            assert!(standable(&world, *cell), "{cell:?} should be standable");
        }
        // Manhattan distance is 5; with a 5-tall wall to skirt, we walk
        // out, around, back. Should be longer than 6.
        assert!(path.len() > 6, "path should detour, got {} cells", path.len());
    }

    #[test]
    fn step_up_one_block() {
        // Build a stepped surface: floor at y=0 for x in [0,2], then a
        // single column raising the floor to y=1 at x=3.
        let mut world = GridWorld::floor_at(0);
        world.solid.insert(IVec3::new(3, 1, 0));
        // Foot start is on the lower floor at (0, 1, 0); goal is on
        // top of the stepped column at (3, 2, 0).
        let path = find_path(IVec3::new(0, 1, 0), IVec3::new(3, 2, 0), &world, 1000, 100)
            .expect("step-up reachable");
        assert!(path.contains(&IVec3::new(3, 2, 0)));
    }

    #[test]
    fn unreachable_returns_none() {
        let mut world = GridWorld::floor_at(0);
        // Box the start cell in.
        world.add_wall(IVec3::new(-1, 1, -1), IVec3::new(-1, 3, 1));
        world.add_wall(IVec3::new(1, 1, -1), IVec3::new(1, 3, 1));
        world.add_wall(IVec3::new(0, 1, -1), IVec3::new(0, 3, -1));
        world.add_wall(IVec3::new(0, 1, 1), IVec3::new(0, 3, 1));
        world.add_wall(IVec3::new(0, 3, 0), IVec3::new(0, 3, 0));
        let path = find_path(IVec3::new(0, 1, 0), IVec3::new(5, 1, 0), &world, 1000, 100);
        assert!(path.is_none(), "boxed-in actor cannot path out");
    }

    #[test]
    fn node_budget_caps_search() {
        // Wide-open floor, distant goal — easily searchable, but with
        // a tiny budget the search bails.
        let world = GridWorld::floor_at(0);
        let path = find_path(IVec3::new(0, 1, 0), IVec3::new(40, 1, 0), &world, 8, 200);
        assert!(path.is_none(), "8-node budget too small for 40-cell distance");
    }

    #[test]
    fn smoothing_collapses_open_diagonal() {
        // A* on 4-directional grid produces a stair-stepped diagonal.
        // On open ground every cell is standable, so the smoother
        // should collapse the whole staircase to start + end.
        let world = GridWorld::floor_at(0);
        let staircase: Vec<IVec3> = (0..=4)
            .flat_map(|i| [IVec3::new(i, 1, i), IVec3::new(i + 1, 1, i)])
            .collect();
        let smoothed = smooth_path(staircase, &world);
        assert_eq!(smoothed.first(), Some(&IVec3::new(0, 1, 0)));
        assert_eq!(smoothed.last(), Some(&IVec3::new(5, 1, 4)));
        assert!(
            smoothed.len() < 4,
            "open diagonal should collapse to a tight skeleton, got {smoothed:?}"
        );
    }

    #[test]
    fn smoothing_keeps_kinks_near_walls() {
        // A wall forces the line A→C through a solid cell — the
        // intermediate B must remain.
        let mut world = GridWorld::floor_at(0);
        world.add_wall(IVec3::new(2, 1, 0), IVec3::new(2, 2, 0));
        let path = vec![
            IVec3::new(0, 1, 0),
            IVec3::new(2, 1, 2), // detour around wall
            IVec3::new(4, 1, 0),
        ];
        let smoothed = smooth_path(path.clone(), &world);
        assert_eq!(smoothed, path, "kink around wall must be preserved");
    }

    #[test]
    fn smoothing_preserves_step_ups() {
        // A step-up cell must remain even on otherwise open ground —
        // the brain reads endpoint Y to decide when to jump.
        let mut world = GridWorld::floor_at(0);
        world.solid.insert(IVec3::new(2, 1, 0));
        let path = vec![
            IVec3::new(0, 1, 0),
            IVec3::new(1, 1, 0),
            IVec3::new(2, 2, 0), // step up
            IVec3::new(3, 2, 0),
        ];
        let smoothed = smooth_path(path, &world);
        assert!(
            smoothed.contains(&IVec3::new(2, 2, 0)),
            "step-up cell stripped: {smoothed:?}"
        );
    }

    #[test]
    fn nearest_standable_below_finds_floor() {
        let world = GridWorld::floor_at(5);
        // Drop from y=20, expecting to land at y=6 (floor at y=5, feet
        // stand at the cell above).
        let cell = nearest_standable_below(&world, IVec3::new(0, 20, 0), 30)
            .expect("ground reachable within drop budget");
        assert_eq!(cell, IVec3::new(0, 6, 0));
    }
}
