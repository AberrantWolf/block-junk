use block_junk_spaces::{fixtures, *};
use std::collections::BTreeSet;

const FLOOR: Cell = Cell(2, 1, 2);

#[test]
fn furnishing_a_wall_does_not_change_structural_area_or_volume() {
    let before = detect(&fixtures::room(4, 4, 2, Some(3)));
    let scene = fixtures::wall_beds();
    let after = detect(&scene);
    let a = before.at_floor(FLOOR).unwrap();
    let b = after.at_floor(FLOOR).unwrap();
    assert_eq!(a.floor, b.floor);
    assert_eq!(b.structural_area(), 16);
    assert_eq!(b.interior_volume, Some(32));
    assert_eq!(a.roofs, b.roofs);
    assert_eq!(
        after.tagged_placements(&scene, b.id, "bed"),
        BTreeSet::from([PlacementId(1), PlacementId(2)])
    );
}

#[test]
fn furniture_wall_through_the_middle_does_not_split_a_space() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    for x in 1..=4 {
        scene
            .add_placement(Placement::new(x as u64, [Cell(x, 1, 2)], ["shelf"]).unwrap())
            .unwrap();
    }
    let result = detect(&scene);
    assert_eq!(
        result.at_floor(Cell(1, 1, 1)).unwrap().id,
        result.at_floor(Cell(4, 1, 4)).unwrap().id
    );
    assert_eq!(result.at_floor(FLOOR).unwrap().structural_area(), 16);
}

#[test]
fn partial_contacts_count_two_distinct_tables() {
    let scene = fixtures::partial_table_contacts();
    let result = detect(&scene);
    let id = result.at_floor(FLOOR).unwrap().id;
    assert_eq!(result.adjacent_count(&scene, id, "table", "seat"), 2);
    assert_eq!(result.adjacent_count(&scene, id, "seat", "table"), 2);
    assert_eq!(
        result.adjacent_placements(&scene, id, "table", "seat"),
        BTreeSet::from([
            (PlacementId(1), PlacementId(3)),
            (PlacementId(2), PlacementId(4))
        ])
    );
}

#[test]
fn multiple_contacts_and_matching_tags_do_not_self_count() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    scene
        .add_placement(
            Placement::new(1, [Cell(1, 1, 1), Cell(2, 1, 1)], ["table", "seat"]).unwrap(),
        )
        .unwrap();
    let result = detect(&scene);
    let id = result.at_floor(FLOOR).unwrap().id;
    assert_eq!(result.adjacent_count(&scene, id, "table", "seat"), 0);
    scene
        .add_placement(Placement::new(2, [Cell(1, 1, 2), Cell(2, 1, 2)], ["seat"]).unwrap())
        .unwrap();
    let result = detect(&scene);
    assert_eq!(result.adjacent_count(&scene, id, "table", "seat"), 1);
}

#[test]
fn rotation_and_vertical_footprints_are_still_single_placements() {
    let mut scene = fixtures::room(4, 4, 3, Some(4));
    scene
        .add_placement(Placement::new(1, [Cell(1, 1, 1), Cell(1, 1, 2)], ["bed"]).unwrap())
        .unwrap();
    scene
        .add_placement(Placement::new(2, [Cell(4, 1, 1), Cell(4, 2, 1)], ["shelf"]).unwrap())
        .unwrap();
    let result = detect(&scene);
    let patch = result.at_floor(FLOOR).unwrap();
    assert_eq!(patch.structural_area(), 16);
    assert_eq!(patch.placements.len(), 2);
}

#[test]
fn roof_probe_dependencies_include_empty_space_above_low_walls() {
    let mut scene = fixtures::room(4, 4, 1, None);
    let before = detect(&scene);
    let patch = before.at_floor(FLOOR).unwrap();
    assert_eq!(patch.roof_fraction(), Some(0.0));
    assert_eq!(patch.boundary, BoundaryState::Closed);
    assert_eq!(patch.interior_volume, None);
    assert!(patch.dependencies.contains(&Cell(2, 7, 2)));
    assert!(
        patch
            .dependencies
            .contains(&Cell(2, scene.bounds().max.1 + 1, 2))
    );
    for x in 1..=4 {
        for z in 1..=4 {
            scene.set_voxel(Cell(x, 7, z), Voxel::Solid).unwrap();
        }
    }
    let after = detect(&scene);
    let roofed = after.at_floor(FLOOR).unwrap();
    assert_eq!(patch.floor, roofed.floor);
    assert_eq!(roofed.roof_fraction(), Some(1.0));
    assert_eq!(
        roofed.interior_volume, None,
        "floating roof is coverage, not full-height enclosure"
    );
    assert!(roofed.exposed_side_faces.unwrap() > 0);
}

#[test]
fn pitched_roof_volume_is_measured_not_area_times_median() {
    let mut scene = fixtures::room(4, 4, 5, None);
    for x in 1..=4 {
        let ceiling = if x <= 2 { 3 } else { 5 };
        for z in 1..=4 {
            // Fill above the roof to form a sealed pitched/stepped solid roof.
            for y in ceiling..=5 {
                scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
            }
        }
    }
    let result = detect(&scene);
    let patch = result.at_floor(FLOOR).unwrap();
    assert_eq!(patch.roof_fraction(), Some(1.0));
    assert_eq!(patch.interior_volume, Some(8 * 2 + 8 * 4));
    assert_eq!(patch.exposed_side_faces, Some(0));
}

#[test]
fn skylight_is_known_open_and_unloaded_roof_is_unknown() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    scene.set_voxel(Cell(2, 3, 2), Voxel::Air).unwrap();
    let result = detect(&scene);
    let patch = result.at_floor(FLOOR).unwrap();
    assert_eq!(patch.roof_fraction(), Some(15.0 / 16.0));
    assert_eq!(patch.interior_volume, None);
    scene.set_voxel(Cell(2, 4, 2), Voxel::Unknown).unwrap();
    let result = detect(&scene);
    assert_eq!(result.at_floor(FLOOR).unwrap().roof_fraction(), None);
}

#[test]
fn unknown_ground_or_neighbor_is_not_a_confirmed_boundary() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    scene.set_voxel(Cell(0, 1, 2), Voxel::Unknown).unwrap();
    let result = detect(&scene);
    assert_eq!(
        result.at_floor(FLOOR).unwrap().boundary,
        BoundaryState::Unknown
    );
    assert_eq!(result.at_floor(FLOOR).unwrap().interior_volume, None);
    scene.set_voxel(Cell(0, 1, 2), Voxel::Air).unwrap();
    scene.set_voxel(Cell(0, 0, 2), Voxel::Unknown).unwrap();
    let result = detect(&scene);
    assert_eq!(
        result.at_floor(FLOOR).unwrap().boundary,
        BoundaryState::Unknown
    );
}

#[test]
fn open_and_closed_double_doors_preserve_footprints_but_change_traversal() {
    let closed = detect(&fixtures::double_door(false));
    let open = detect(&fixtures::double_door(true));
    for cell in [Cell(2, 1, 2), Cell(6, 1, 2)] {
        assert_eq!(
            closed.at_floor(cell).unwrap().floor,
            open.at_floor(cell).unwrap().floor
        );
    }
    assert_ne!(
        open.at_floor(Cell(2, 1, 2)).unwrap().id,
        open.at_floor(Cell(6, 1, 2)).unwrap().id
    );
    assert_eq!(open.connections.len(), 2);
    assert!(
        open.connections
            .iter()
            .all(|c| c.partition.unwrap().traversable())
    );
    assert!(
        closed
            .connections
            .iter()
            .all(|c| !c.partition.unwrap().traversable())
    );
    assert_eq!(closed.at_floor(FLOOR).unwrap().exposed_side_faces, Some(0));
    assert_eq!(open.at_floor(FLOOR).unwrap().interior_volume, Some(24));
    assert_eq!(open.at_floor(FLOOR).unwrap().exposed_side_faces, Some(4));
}

#[test]
fn removing_a_partition_merges_spaces_regardless_of_opening_width() {
    let mut scene = fixtures::double_door(true);
    for y in 1..=2 {
        for z in 2..=3 {
            scene.remove_partition(Face::new(Cell(3, y, z), Cell(4, y, z)).unwrap());
        }
    }
    let result = detect(&scene);
    assert_eq!(
        result.at_floor(FLOOR).unwrap().id,
        result.at_floor(Cell(6, 1, 2)).unwrap().id
    );
    assert_eq!(result.at_floor(FLOOR).unwrap().structural_area(), 26);
}

#[test]
fn window_partitions_separate_spaces_without_creating_a_traversable_portal() {
    let mut scene = fixtures::double_door(true);
    for y in 1..=2 {
        for z in 2..=3 {
            scene
                .set_partition(
                    Face::new(Cell(3, y, z), Cell(4, y, z)).unwrap(),
                    Partition::Window,
                )
                .unwrap();
        }
    }
    let result = detect(&scene);
    assert_ne!(
        result.at_floor(FLOOR).unwrap().id,
        result.at_floor(Cell(6, 1, 2)).unwrap().id
    );
    assert!(
        result
            .connections
            .iter()
            .all(|c| !c.partition.unwrap().traversable())
    );
}

#[test]
fn furniture_relations_do_not_cross_a_partition() {
    let mut scene = fixtures::double_door(true);
    scene
        .add_placement(Placement::new(1, [Cell(3, 1, 2)], ["seat"]).unwrap())
        .unwrap();
    scene
        .add_placement(Placement::new(2, [Cell(4, 1, 2)], ["table"]).unwrap())
        .unwrap();
    let result = detect(&scene);
    let id = result.at_floor(FLOOR).unwrap().id;
    assert_eq!(result.adjacent_count(&scene, id, "seat", "table"), 0);
}

#[test]
fn stepped_floors_connect_without_unioning_levels_or_wall_tops() {
    let scene = fixtures::stepped_room();
    let result = detect(&scene);
    let lower = result.at_floor(FLOOR).unwrap();
    let upper = result.at_floor(Cell(3, 2, 2)).unwrap();
    assert_ne!(lower.id, upper.id);
    assert_eq!(lower.structural_area() + upper.structural_area(), 16);
    assert_eq!(
        result
            .connections
            .iter()
            .filter(|c| c.partition.is_none())
            .count(),
        1
    );
    assert!(!lower.floor.contains(&Cell(0, 4, 2)));
}

#[test]
fn raised_objects_belong_above_the_correct_floor_not_every_overlapping_bbox() {
    let mut scene = fixtures::room(4, 4, 5, Some(6));
    for x in 1..=4 {
        for z in 1..=4 {
            scene.set_voxel(Cell(x, 3, z), Voxel::Solid).unwrap();
        }
    }
    scene
        .add_placement(Placement::new(1, [Cell(2, 4, 2)], ["bed"]).unwrap())
        .unwrap();
    let result = detect(&scene);
    assert!(result.at_floor(FLOOR).unwrap().placements.is_empty());
    assert_eq!(
        result.at_floor(Cell(2, 4, 2)).unwrap().placements,
        BTreeSet::from([PlacementId(1)])
    );
}

#[test]
fn irregular_space_membership_does_not_include_the_missing_corner() {
    let mut scene = fixtures::room(4, 4, 2, Some(3));
    // Cut the northeast 2x2 out with two full-height partition planes.
    for y in 1..=2 {
        for t in 3..=4 {
            scene
                .set_partition(
                    Face::new(Cell(2, y, t), Cell(3, y, t)).unwrap(),
                    Partition::Wall,
                )
                .unwrap();
            scene
                .set_partition(
                    Face::new(Cell(t, y, 2), Cell(t, y, 3)).unwrap(),
                    Partition::Wall,
                )
                .unwrap();
        }
    }
    scene
        .add_placement(Placement::new(1, [Cell(4, 1, 4)], ["bed"]).unwrap())
        .unwrap();
    let result = detect(&scene);
    let l_shape = result.at_floor(FLOOR).unwrap();
    assert_eq!(l_shape.structural_area(), 12);
    assert!(l_shape.placements.is_empty());
    assert_eq!(result.at_floor(Cell(4, 1, 4)).unwrap().placements.len(), 1);
}

#[test]
fn rebuild_is_independent_of_voxel_and_placement_insertion_order() {
    let original = fixtures::partial_table_contacts();
    let mut reverse = Scene::new(original.bounds()).unwrap();
    let cells: Vec<_> = original.bounds().cells().collect();
    for c in cells.into_iter().rev() {
        reverse.set_voxel(c, original.voxel(c)).unwrap();
        reverse.set_known_sky(c.0, c.2, true).unwrap();
    }
    let placements: Vec<_> = original.placements().cloned().collect();
    for p in placements.into_iter().rev() {
        reverse.add_placement(p).unwrap();
    }
    assert_eq!(detect(&original), detect(&reverse));
}

#[test]
fn invalid_placement_and_boundary_inputs_fail_without_mutation() {
    assert!(Placement::new(1, [FLOOR, FLOOR], ["bed"]).is_err());
    assert!(Placement::new(0, [FLOOR], ["bed"]).is_err());
    assert!(Face::new(FLOOR, FLOOR.offset(1, 1, 0)).is_err());
    let mut scene = fixtures::wall_beds();
    let before = detect(&scene);
    let duplicate = Placement::new(1, [Cell(4, 1, 4)], ["bed"]).unwrap();
    assert!(scene.add_placement(duplicate).is_err());
    assert!(scene.set_voxel(Cell(1, 1, 1), Voxel::Solid).is_err());
    assert_eq!(detect(&scene), before);
}

#[test]
fn more_than_old_flood_cap_is_still_a_space() {
    let scene = fixtures::room(65, 64, 1, Some(3));
    let result = detect(&scene);
    assert_eq!(result.at_floor(FLOOR).unwrap().structural_area(), 4160);
    assert_eq!(
        result.at_floor(FLOOR).unwrap().boundary,
        BoundaryState::Closed
    );
}
