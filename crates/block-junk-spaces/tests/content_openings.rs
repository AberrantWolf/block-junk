use block_junk_spaces::{content::*, fixtures, openings::*, *};

fn framed(width: i32, height: i32) -> Scene {
    fixtures::framed_opening(width, height)
}

fn portal() -> SpatialDef {
    SpatialDef {
        id: "test:door".into(),
        structural_cells: vec![],
        furnishing_cells: vec![],
        tags: vec![],
        steps: vec![],
        partitions: (0..2)
            .map(|y| LocalPartition {
                a: Cell(-1, y, 0),
                b: Cell(0, y, 0),
                kind: Partition::Portal { open: true },
            })
            .collect(),
    }
}

#[test]
fn bare_frames_of_different_widths_and_heights_create_explainable_portals() {
    for width in [1, 2, 4] {
        for height in [2, 3, 5] {
            let scene = framed(width, height);
            let (prepared, openings) = with_inferred_openings(&scene);
            assert_eq!(
                openings.len(),
                1,
                "{width} wide, {height} high: {openings:?}"
            );
            let opening = &openings[0];
            assert_eq!(opening.floor_cells.len(), width as usize);
            assert_eq!(opening.height, height);
            assert!(opening.evidence.contains(&Cell(4, height + 1, 2)));
            assert!(opening.evidence.contains(&Cell(4, 1, 1)));
            let before = detect(&scene);
            let after = detect(&prepared);
            assert_eq!(
                before.at_floor(Cell(2, 1, 2)).unwrap().id,
                before.at_floor(Cell(6, 1, 2)).unwrap().id
            );
            assert_ne!(
                after.at_floor(Cell(2, 1, 2)).unwrap().id,
                after.at_floor(Cell(6, 1, 2)).unwrap().id
            );
            assert_eq!(
                infer_openings(&prepared).len(),
                0,
                "preparation is idempotent"
            );
        }
    }
}

#[test]
fn damaging_a_jamb_or_lintel_removes_recognition_on_rebuild() {
    for damage in [Cell(4, 2, 1), Cell(4, 3, 2)] {
        let mut scene = framed(2, 2);
        assert_eq!(infer_openings(&scene).len(), 1);
        scene.set_voxel(damage, Voxel::Air).unwrap();
        let (prepared, openings) = with_inferred_openings(&scene);
        assert!(openings.is_empty());
        let result = detect(&prepared);
        assert_eq!(
            result.at_floor(Cell(2, 1, 2)).unwrap().id,
            result.at_floor(Cell(6, 1, 2)).unwrap().id
        );
    }
}

#[test]
fn unknown_frame_and_unsupported_approach_do_not_prove_a_doorway() {
    for (cell, value) in [(Cell(4, 3, 2), Voxel::Unknown), (Cell(3, 0, 2), Voxel::Air)] {
        let mut scene = framed(2, 2);
        scene.set_voxel(cell, value).unwrap();
        assert!(infer_openings(&scene).is_empty());
    }
}

#[test]
fn floor_height_window_is_rejected_and_asymmetric_stepped_arch_is_supported() {
    let mut slit = framed(2, 2);
    for z in 2..=3 {
        slit.set_voxel(Cell(4, 2, z), Voxel::Solid).unwrap();
    }
    assert!(infer_openings(&slit).is_empty());
    let mut arch = framed(3, 3);
    arch.set_voxel(Cell(4, 3, 2), Voxel::Solid).unwrap();
    let openings = infer_openings(&arch);
    assert_eq!(openings.len(), 1);
    assert_eq!(openings[0].profiles, vec![vec![2, 3, 3]]);
}

#[test]
fn long_roofed_corridor_is_not_sliced_at_every_cell() {
    let scene = fixtures::room(1, 12, 2, Some(3));
    assert!(infer_openings(&scene).is_empty());
}

#[test]
fn furniture_is_not_frame_evidence_and_does_not_hide_a_real_frame() {
    let mut scene = framed(2, 2);
    scene
        .add_placement(Placement::new(1, [Cell(4, 1, 2), Cell(4, 2, 2)], ["shelf"]).unwrap())
        .unwrap();
    assert_eq!(
        infer_openings(&scene).len(),
        1,
        "architecture is independent of furnishings"
    );
    scene.set_voxel(Cell(4, 2, 1), Voxel::Air).unwrap();
    scene
        .add_placement(Placement::new(2, [Cell(4, 2, 1)], ["decorative_jamb"]).unwrap())
        .unwrap();
    assert!(infer_openings(&scene).is_empty());
}

#[test]
fn authored_window_takes_precedence_over_automatic_portal() {
    let mut scene = framed(1, 2);
    let mut def = portal();
    for p in &mut def.partitions {
        p.kind = Partition::Window;
    }
    def.instantiate(&mut scene, Cell(4, 1, 2), Facing::North, 1)
        .unwrap();
    let (prepared, openings) = with_inferred_openings(&scene);
    assert!(openings.is_empty());
    assert_eq!(
        prepared.partition(Cell(3, 1, 2), Cell(4, 1, 2)),
        Some(Partition::Window)
    );
}

#[test]
fn content_portals_rotate_in_all_four_directions() {
    for (facing, a, b) in [
        (Facing::North, Cell(1, 1, 2), Cell(2, 1, 2)),
        (Facing::East, Cell(2, 1, 1), Cell(2, 1, 2)),
        (Facing::South, Cell(3, 1, 2), Cell(2, 1, 2)),
        (Facing::West, Cell(2, 1, 3), Cell(2, 1, 2)),
    ] {
        let mut scene = fixtures::room(4, 4, 2, Some(3));
        portal()
            .instantiate(&mut scene, Cell(2, 1, 2), facing, 1)
            .unwrap();
        assert_eq!(
            scene.partition(a, b),
            Some(Partition::Portal { open: true })
        );
        assert_eq!(
            scene.partition(a.offset(0, 1, 0), b.offset(0, 1, 0)),
            Some(Partition::Portal { open: true })
        );
    }
}

#[test]
fn content_furniture_rotates_without_affecting_structural_floor() {
    let mut def = portal();
    def.partitions.clear();
    def.furnishing_cells = vec![Cell(0, 0, 0), Cell(1, 0, 0)];
    def.tags = vec!["bed".into()];
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    def.instantiate(&mut scene, Cell(1, 1, 1), Facing::East, 42)
        .unwrap();
    let result = detect(&scene);
    assert_eq!(
        result.at_floor(Cell(2, 1, 2)).unwrap().structural_area(),
        16
    );
    assert_eq!(
        scene.placements().next().unwrap().cells,
        std::collections::BTreeSet::from([Cell(1, 1, 1), Cell(1, 1, 2)])
    );
}

#[test]
fn invalid_content_is_atomic_and_names_its_definition() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    let before = detect(&scene);
    let mut def = portal();
    def.structural_cells = vec![Cell(0, 0, 0)];
    let error = def
        .instantiate(&mut scene, Cell(2, 1, 2), Facing::North, 1)
        .unwrap_err();
    assert!(error.contains("test:door"));
    assert_eq!(detect(&scene), before);
    def.partitions.clear();
    def.structural_cells = vec![Cell(0, 0, 0), Cell(0, 0, 0)];
    assert!(
        def.instantiate(&mut scene, Cell(2, 1, 2), Facing::North, 1)
            .is_err()
    );
    assert_eq!(detect(&scene), before);
}

#[test]
fn rotated_content_overflow_is_a_validation_error() {
    let mut def = portal();
    def.partitions.clear();
    def.structural_cells = vec![Cell(i32::MIN, 0, 0)];
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    assert!(
        def.instantiate(&mut scene, Cell(1, 1, 1), Facing::South, 1)
            .unwrap_err()
            .contains("overflow")
    );
}

fn step_def() -> SpatialDef {
    SpatialDef {
        id: "test:step".into(),
        structural_cells: vec![Cell(1, 0, 0)],
        furnishing_cells: vec![],
        tags: vec![],
        partitions: vec![],
        steps: vec![(Cell(0, 0, 0), Cell(1, 1, 0))],
    }
}

#[test]
fn authored_step_geometry_and_connections_rotate_together() {
    for (facing, upper) in [
        (Facing::North, Cell(3, 2, 2)),
        (Facing::East, Cell(2, 2, 3)),
        (Facing::South, Cell(1, 2, 2)),
        (Facing::West, Cell(2, 2, 1)),
    ] {
        let mut scene = fixtures::room(4, 4, 3, Some(4));
        step_def()
            .instantiate(&mut scene, Cell(2, 1, 2), facing, 1)
            .unwrap();
        let result = detect(&scene);
        let low = result.at_floor(Cell(2, 1, 2)).unwrap().id;
        let high = result.at_floor(upper).unwrap().id;
        assert_ne!(low, high);
        assert!(result.connections.iter().any(|c| c.partition.is_none()
            && ((c.a == low && c.b == high) || (c.a == high && c.b == low))));
    }
}

#[test]
fn low_beam_rejects_step_atomically_and_later_obstruction_removes_connection() {
    let mut scene = fixtures::room(4, 4, 3, Some(4));
    scene.set_voxel(Cell(2, 3, 2), Voxel::Solid).unwrap();
    let before = detect(&scene);
    assert!(
        step_def()
            .instantiate(&mut scene, Cell(2, 1, 2), Facing::North, 1)
            .is_err()
    );
    assert_eq!(detect(&scene), before);
    scene.set_voxel(Cell(2, 3, 2), Voxel::Air).unwrap();
    step_def()
        .instantiate(&mut scene, Cell(2, 1, 2), Facing::North, 1)
        .unwrap();
    assert_eq!(detect(&scene).connections.len(), 1);
    scene.set_voxel(Cell(2, 3, 2), Voxel::Solid).unwrap();
    assert!(detect(&scene).connections.is_empty());
}

#[test]
fn partition_blocks_a_step_even_when_both_floor_samples_still_exist() {
    let mut scene = fixtures::stepped_room();
    scene
        .set_partition(
            Face::new(Cell(2, 2, 2), Cell(3, 2, 2)).unwrap(),
            Partition::Wall,
        )
        .unwrap();
    let result = detect(&scene);
    assert!(result.at_floor(Cell(2, 1, 2)).is_some());
    assert!(result.at_floor(Cell(3, 2, 2)).is_some());
    assert!(!result.connections.iter().any(|c| c.partition.is_none()));
}
