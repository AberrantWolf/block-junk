use block_junk_spaces::{fixtures, openings::infer_openings, *};
use std::collections::BTreeSet;

fn rotated(scene: &Scene, turns: usize, reverse: bool) -> Scene {
    let transform = |mut cell: Cell| {
        for _ in 0..turns {
            cell = Cell(-cell.2, cell.1, cell.0);
        }
        cell
    };
    let corners = scene.bounds().cells().map(transform).collect::<Vec<_>>();
    let bounds = Bounds {
        min: Cell(
            corners.iter().map(|c| c.0).min().unwrap(),
            scene.bounds().min.1,
            corners.iter().map(|c| c.2).min().unwrap(),
        ),
        max: Cell(
            corners.iter().map(|c| c.0).max().unwrap(),
            scene.bounds().max.1,
            corners.iter().map(|c| c.2).max().unwrap(),
        ),
    };
    let mut copy = Scene::new(bounds).unwrap();
    let mut cells = scene.bounds().cells().collect::<Vec<_>>();
    if reverse {
        cells.reverse();
    }
    for cell in cells {
        copy.set_voxel(transform(cell), scene.voxel(cell)).unwrap();
    }
    copy
}

#[test]
fn thin_and_thick_rectangles_and_stepped_arches_have_exact_thresholds() {
    for profile in [
        vec![2],
        vec![3, 3],
        vec![2, 3, 4, 3, 2],
        vec![2, 3, 3, 3, 2],
        vec![2, 4, 4, 2],
    ] {
        for depth in [1, 2, 4] {
            let scene = fixtures::profiled_opening(&profile, depth);
            for turns in 0..4 {
                for reverse in [false, true] {
                    let copy = rotated(&scene, turns, reverse);
                    let openings = infer_openings(&copy);
                    assert_eq!(
                        openings.len(),
                        1,
                        "profile={profile:?}, depth={depth}, rotation={turns}, reverse={reverse}"
                    );
                    let opening = &openings[0];
                    assert_eq!(opening.depth, depth as usize);
                    assert_eq!(opening.floor_cells.len(), profile.len() * depth as usize);
                    assert_eq!(
                        opening.volume_cells.len(),
                        profile.iter().sum::<i32>() as usize * depth as usize
                    );
                    assert_eq!(opening.approaches[0].len(), profile.len());
                    assert_eq!(opening.approaches[1].len(), profile.len());
                    assert!(
                        opening
                            .approaches
                            .iter()
                            .flatten()
                            .all(|c| !opening.floor_cells.contains(c))
                    );
                    assert!(
                        opening
                            .volume_cells
                            .iter()
                            .all(|c| copy.voxel(*c) == Voxel::Air)
                    );
                    assert!(
                        opening
                            .evidence
                            .iter()
                            .any(|c| copy.voxel(*c) == Voxel::Solid)
                    );
                }
            }
        }
    }
}

#[test]
fn valleys_off_center_peaks_and_disconnected_headers_are_not_arches() {
    for profile in [vec![3, 2, 3], vec![4, 3, 2, 2, 2], vec![2, 4, 2, 4, 2]] {
        assert!(
            infer_openings(&fixtures::profiled_opening(&profile, 2)).is_empty(),
            "{profile:?}"
        );
    }
    let mut scene = fixtures::profiled_opening(&[2, 4, 2], 2);
    scene.set_voxel(Cell(4, 4, 2), Voxel::Air).unwrap();
    assert!(
        infer_openings(&scene).is_empty(),
        "corner contact does not supply a connected header"
    );
}

#[test]
fn thick_passage_rebuild_observes_damage_blockage_unknown_and_authored_windows() {
    let base = fixtures::profiled_opening(&[2, 3, 4, 3, 2], 3);
    for (cell, voxel) in [
        (Cell(5, 2, 1), Voxel::Air),
        (Cell(5, 1, 4), Voxel::Solid),
        (Cell(5, 2, 4), Voxel::Unknown),
    ] {
        let mut changed = base.clone();
        changed.set_voxel(cell, voxel).unwrap();
        assert!(infer_openings(&changed).is_empty(), "{cell:?} = {voxel:?}");
        changed.set_voxel(cell, base.voxel(cell)).unwrap();
        assert_eq!(infer_openings(&changed), infer_openings(&base));
    }
    let mut changed = base.clone();
    changed
        .set_partition(
            Face::new(Cell(6, 1, 4), Cell(7, 1, 4)).unwrap(),
            Partition::Window,
        )
        .unwrap();
    assert!(infer_openings(&changed).is_empty());
}

#[test]
fn furnishing_a_threshold_does_not_supply_or_destroy_structural_evidence() {
    let mut scene = fixtures::profiled_opening(&[2, 3, 2], 3);
    let before = infer_openings(&scene);
    scene
        .add_placement(Placement::new(1, [Cell(5, 1, 3), Cell(5, 2, 3)], ["statue"]).unwrap())
        .unwrap();
    assert_eq!(infer_openings(&scene), before);
    let cells: BTreeSet<_> = before[0].floor_cells.iter().copied().collect();
    assert_eq!(cells.len(), 9);
}

#[test]
fn intersecting_frames_require_authored_separation() {
    let mut scene = fixtures::room(7, 5, 2, Some(3));
    for (x, z) in [(4, 1), (4, 5), (2, 3), (6, 3)] {
        for y in 1..=2 {
            scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
        }
    }
    assert!(infer_openings(&scene).is_empty());
}
