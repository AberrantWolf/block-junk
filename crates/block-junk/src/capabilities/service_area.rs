//! Facility-centered reachability. Range measures horizontal steps plus height
//! changes; architectural thresholds are excluded unless portal traversal is enabled.
use crate::{
    architecture_solver::{Cell, Face, topology::Connection},
    surface_navigation::SurfaceView,
};
use bevy::math::IVec3;
use block_junk_mod_api::architecture::MovementProfile;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceArea {
    pub surfaces: BTreeSet<Cell>,
    pub pending: bool,
}

pub async fn compute(
    view: &SurfaceView,
    starts: &BTreeSet<Cell>,
    profile: &MovementProfile,
    range: u32,
    allow_portals: bool,
    connections: &[Connection],
) -> ServiceArea {
    let mut thresholds = BTreeSet::new();
    let mut faces = BTreeSet::<Face>::new();
    if !allow_portals {
        for connection in connections {
            for cell in &connection.threshold {
                view.budget.step().await;
                thresholds.insert(*cell);
            }
            for face in &connection.faces {
                view.budget.step().await;
                faces.insert(*face);
            }
        }
    }
    let mut result = ServiceArea::default();
    let mut distances = BTreeMap::new();
    let mut queue = BTreeSet::new();
    for &cell in starts {
        if thresholds.contains(&cell) {
            continue;
        }
        match view
            .standable(IVec3::new(cell.0, cell.1, cell.2), profile)
            .await
        {
            Some(true) => {
                distances.insert(cell, 0u32);
                queue.insert((0u32, cell));
            }
            None => result.pending = true,
            Some(false) => {}
        }
    }
    while let Some((distance, cell)) = queue.pop_first() {
        view.budget.step().await;
        if distances.get(&cell) != Some(&distance) {
            continue;
        }
        result.surfaces.insert(cell);
        if distance >= range {
            continue;
        }
        let from = IVec3::new(cell.0, cell.1, cell.2);
        for offset in [IVec3::X, -IVec3::X, IVec3::Z, -IVec3::Z] {
            // A reachable supported floor ends the downward search; it cannot
            // be skipped to reach a disconnected ledge below it.
            let mut reached_floor = false;
            for dy in (0..=profile.step_up as i32).chain((1..=profile.max_drop as i32).map(|n| -n))
            {
                view.budget.step().await;
                if dy < 0 && reached_floor {
                    break;
                }
                let to = from + offset + IVec3::Y * dy;
                let next = Cell(to.x, to.y, to.z);
                let cost = 1 + dy.unsigned_abs();
                if cost > range - distance {
                    continue;
                }
                if thresholds.contains(&next) || faces.contains(&Face::new(cell, next)) {
                    continue;
                }
                if distances
                    .get(&next)
                    .is_some_and(|old| *old <= distance + cost)
                {
                    if dy >= 0 {
                        reached_floor = true;
                    }
                    continue;
                }
                match view.step(from, to, profile).await {
                    Some(true) => {
                        reached_floor = true;
                        {
                            match view
                                .crosses_boundary(from, to, profile, allow_portals)
                                .await
                            {
                                Some(false) => {}
                                Some(true) => continue,
                                None => {
                                    result.pending = true;
                                    continue;
                                }
                            }
                        }
                        distances.insert(next, distance + cost);
                        queue.insert((distance + cost, next));
                    }
                    None => result.pending = true,
                    Some(false) => {}
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        architecture_solver::{Budget, Computation},
        blocks::{BlockRegistry, BlockSlot},
        spatial_geometry::ChunkGeometry,
    };
    use std::{sync::Arc, task::Poll};
    fn fixture() -> (SurfaceView, MovementProfile) {
        let blocks = BlockRegistry::build(serde_json::from_value(serde_json::json!([
            {"id":"test:floor","display_name":"Floor","flags":{"solid":true},"color":[1,1,1]},
            {"id":"test:screen","display_name":"Screen","flags":{},"color":[1,1,1],"structure":{"partitions":[{"cell":[0,0,0],"normal":[1,0,0],"window":false}]}}
        ])).unwrap()).unwrap().0;
        let mut chunk = ChunkGeometry::default();
        chunk.physical.fill(Some(BlockSlot::EMPTY));
        for x in 0..32 {
            for z in 0..32 {
                chunk.physical[ChunkGeometry::cell_index(IVec3::new(x, 0, z))] =
                    Some(blocks.require("test:floor"));
            }
        }
        chunk.dependencies.insert([0, 0, 0], Some(1));
        (
            SurfaceView {
                chunks: BTreeMap::from([([0, 0, 0], Arc::new(chunk))]),
                collision: blocks.collision.clone(),
                architecture: blocks.architecture.clone(),
                budget: Arc::new(Budget::default()),
                dependencies: Default::default(),
            },
            MovementProfile {
                id: "test:adult".into(),
                height: 1.8,
                radius: 0.3,
                step_up: 1,
                max_drop: 3,
                stairs: true,
            },
        )
    }
    fn run(
        view: SurfaceView,
        profile: MovementProfile,
        starts: BTreeSet<Cell>,
        range: u32,
        portals: bool,
        connections: Vec<Connection>,
    ) -> ServiceArea {
        let mut task = Computation::new(view.budget.clone(), async move {
            compute(&view, &starts, &profile, range, portals, &connections).await
        });
        for _ in 0..100_000 {
            let before = task.operations;
            let result = task.advance(31);
            assert!(task.operations - before <= 31);
            if let Poll::Ready(result) = result {
                return result;
            }
        }
        panic!("finite area did not converge")
    }
    #[test]
    fn range_follows_surfaces_and_overlapping_starts_do_not_multiply_area() {
        let (view, profile) = fixture();
        let area = run(
            view,
            profile,
            BTreeSet::from([Cell(16, 1, 16)]),
            2,
            false,
            vec![],
        );
        assert!(!area.pending);
        assert_eq!(area.surfaces.len(), 13);
        assert!(!area.surfaces.contains(&Cell(18, 1, 18)));
    }
    #[test]
    fn inferred_thresholds_block_by_default_and_explicit_portal_traversal_allows_them() {
        let (view, profile) = fixture();
        let threshold = (0..32).map(|z| Cell(17, 1, z)).collect();
        let connection = Connection {
            ends: Default::default(),
            faces: Default::default(),
            threshold,
            volume: Default::default(),
            evidence: Default::default(),
            traversable: true,
            placements: Default::default(),
        };
        let starts = BTreeSet::from([Cell(16, 1, 16)]);
        let blocked = run(
            view.clone(),
            profile.clone(),
            starts.clone(),
            3,
            false,
            vec![connection.clone()],
        );
        assert!(!blocked.pending);
        assert!(blocked.surfaces.iter().all(|cell| cell.0 < 17));
        let allowed = run(view, profile, starts, 3, true, vec![connection]);
        assert!(allowed.surfaces.contains(&Cell(19, 1, 16)));
    }
    #[test]
    fn portal_permission_never_bypasses_an_authored_partition() {
        let (mut view, profile) = fixture();
        let chunk = Arc::make_mut(view.chunks.get_mut(&[0, 0, 0]).unwrap());
        for z in 0..32 {
            chunk.physical[ChunkGeometry::cell_index(IVec3::new(16, 1, z))] = Some(BlockSlot(2));
        }
        let area = run(
            view,
            profile,
            BTreeSet::from([Cell(16, 1, 16)]),
            3,
            true,
            vec![],
        );
        assert!(!area.pending);
        assert!(area.surfaces.iter().all(|cell| cell.0 <= 16));
    }
    #[test]
    fn unknown_continuation_retains_read_dependency_and_pending_result() {
        let (view, profile) = fixture();
        let dependencies = view.dependencies.clone();
        let area = run(
            view,
            profile,
            BTreeSet::from([Cell(31, 1, 16)]),
            2,
            false,
            vec![],
        );
        assert!(area.pending);
        assert!(dependencies.lock().unwrap().contains_key(&[1, 0, 0]));
    }
}
