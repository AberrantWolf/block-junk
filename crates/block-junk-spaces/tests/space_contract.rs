use block_junk_spaces::{fixtures, *};
use std::collections::BTreeSet;

#[test]
fn stepped_hall_and_gallery_are_one_space_with_exact_surfaces_and_volume() {
    let scene = fixtures::gallery_hall();
    let result = detect_architecture(&scene);
    let hall = result.at_floor(Cell(3, 1, 3)).unwrap();
    assert_eq!(hall.id, result.at_floor(Cell(4, 2, 3)).unwrap().id);
    assert_eq!(hall.id, result.at_floor(Cell(1, 5, 3)).unwrap().id);
    assert_eq!(
        hall.floor_surfaces.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 5]
    );
    assert_eq!(hall.structural_area(), 48);
    assert_eq!(hall.volume, Volume::Bounded(186));
    assert_eq!(hall.roof_fraction(), Some(1.0));
    assert_eq!(hall.boundary, BoundaryState::Closed);
}

#[test]
fn roof_added_last_and_skylights_change_coverage_not_hall_membership() {
    let complete = fixtures::gallery_hall();
    let expected: BTreeSet<_> = detect_architecture(&complete)
        .at_floor(Cell(3, 1, 3))
        .unwrap()
        .floor()
        .collect();
    let mut scene = complete.clone();
    for x in 1..=6 {
        for z in 1..=6 {
            scene.set_voxel(Cell(x, 7, z), Voxel::Air).unwrap();
        }
    }
    let open = detect_architecture(&scene);
    let hall = open.at_floor(Cell(3, 1, 3)).unwrap();
    assert_eq!(hall.floor().collect::<BTreeSet<_>>(), expected);
    assert_eq!(hall.volume, Volume::Open);
    scene.set_voxel(Cell(3, 7, 3), Voxel::Solid).unwrap();
    assert_eq!(
        detect_architecture(&scene)
            .at_floor(Cell(3, 1, 3))
            .unwrap()
            .floor()
            .collect::<BTreeSet<_>>(),
        expected
    );
    let mut skylight = complete;
    skylight.set_voxel(Cell(3, 7, 3), Voxel::Air).unwrap();
    let result = detect_architecture(&skylight);
    let hall = result.at_floor(Cell(3, 1, 3)).unwrap();
    assert_eq!(hall.floor().collect::<BTreeSet<_>>(), expected);
    assert_eq!(hall.roof_fraction(), Some(47.0 / 48.0));
    assert_eq!(hall.volume, Volume::Open);
}

#[test]
fn thick_arch_threshold_is_owned_by_neither_room() {
    let scene = fixtures::profiled_opening(&[2, 3, 4, 3, 2], 3);
    let result = detect_architecture(&scene);
    let left = result.at_floor(Cell(3, 1, 4)).unwrap();
    let right = result.at_floor(Cell(7, 1, 4)).unwrap();
    assert_ne!(left.id, right.id);
    assert_eq!(left.structural_area(), 21);
    assert_eq!(right.structural_area(), 21);
    assert_eq!(left.volume, Volume::Bounded(84));
    assert_eq!(right.volume, Volume::Bounded(84));
    let portal = result
        .connections
        .iter()
        .find(|p| p.threshold.len() == 15)
        .unwrap();
    assert_eq!(portal.volume.len(), 42);
    assert!(portal.ends[0].contains(&left.id));
    assert!(portal.ends[1].contains(&right.id));
    for &cell in &portal.threshold {
        assert!(result.at_floor(cell).is_none());
    }
}

#[test]
fn irregular_and_narrow_spaces_have_no_bounding_box_holes() {
    for (w, d, cut, expected) in [(5, 5, 1, 21), (5, 5, 2, 13), (1, 9, 0, 9)] {
        let mut scene = fixtures::room(w, d, 3, Some(4));
        let mut footprint = BTreeSet::new();
        for x in 1..=w {
            for z in 1..=d {
                let removed = match cut {
                    1 => x >= 4 && z >= 4,
                    2 => (2..=4).contains(&x) && z >= 2,
                    _ => false,
                };
                if removed {
                    for y in 1..=3 {
                        scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                    }
                } else {
                    footprint.insert(Cell(x, 1, z));
                }
            }
        }
        let result = detect_architecture(&scene);
        let space = result.at_floor(Cell(1, 1, 1)).unwrap();
        assert_eq!(space.structural_area(), expected);
        assert_eq!(space.floor().collect::<BTreeSet<_>>(), footprint);
        assert_eq!(space.volume, Volume::Bounded(expected * 3));
    }
}

#[test]
fn dense_furniture_and_stacked_objects_do_not_change_architecture() {
    let mut scene = fixtures::room(6, 6, 3, Some(4));
    let before = detect_architecture(&scene);
    let space = before.at_floor(Cell(1, 1, 1)).unwrap();
    let mut id = 1;
    for x in [1, 3, 5] {
        for z in 1..=6 {
            scene
                .add_placement(
                    Placement::new(id, [Cell(x, 1, z), Cell(x + 1, 1, z)], ["bed"]).unwrap(),
                )
                .unwrap();
            id += 1;
        }
    }
    scene
        .add_placement(Placement::new(id, [Cell(1, 2, 1), Cell(1, 3, 1)], ["decoration"]).unwrap())
        .unwrap();
    let after = detect_architecture(&scene);
    let furnished = after.at_floor(Cell(1, 1, 1)).unwrap();
    assert_eq!(furnished.floor_surfaces, space.floor_surfaces);
    assert_eq!(furnished.volume, space.volume);
    assert_eq!(furnished.placements.len(), 19);
}

#[test]
fn stair_doorway_connects_separate_elevations_even_when_closed() {
    let mut scene = fixtures::stepped_room();
    // Structural divide, with a stair door at z=2. Declare all open air faces
    // across the plane: traversal state never erases the architectural plane.
    for z in 1..=4 {
        for y in 2..=3 {
            scene
                .set_partition(
                    Face::new(Cell(2, y, z), Cell(3, y, z)).unwrap(),
                    if z == 2 {
                        Partition::Portal { open: true }
                    } else {
                        Partition::Wall
                    },
                )
                .unwrap();
        }
    }
    let open = detect_architecture(&scene);
    let left = open.at_floor(Cell(2, 1, 2)).unwrap();
    let right = open.at_floor(Cell(3, 2, 2)).unwrap();
    assert_ne!(left.id, right.id);
    assert_eq!(left.structural_area(), 8);
    assert_eq!(right.structural_area(), 8);
    assert!(
        open.connections.iter().any(|p| p.traversable
            && p.ends[0].contains(&left.id)
            && p.ends[1].contains(&right.id))
    );
    for y in 2..=3 {
        scene
            .set_partition(
                Face::new(Cell(2, y, 2), Cell(3, y, 2)).unwrap(),
                Partition::Portal { open: false },
            )
            .unwrap();
    }
    let closed = detect_architecture(&scene);
    assert_eq!(
        closed.at_floor(Cell(2, 1, 2)).unwrap().floor_surfaces,
        left.floor_surfaces
    );
    assert_eq!(
        closed.at_floor(Cell(3, 2, 2)).unwrap().floor_surfaces,
        right.floor_surfaces
    );
    assert!(closed.connections.iter().all(|p| !p.traversable));
}

#[test]
fn courtyard_and_canopy_keep_boundary_separate_from_volume_and_shelter() {
    let mut scene = fixtures::room(4, 4, 1, None);
    let open = detect_architecture(&scene);
    let courtyard = open.at_floor(Cell(2, 1, 2)).unwrap();
    assert_eq!(courtyard.structural_area(), 16);
    assert_eq!(courtyard.boundary, BoundaryState::Closed);
    assert_eq!(courtyard.roof_fraction(), Some(0.0));
    assert_eq!(courtyard.volume, Volume::Open);
    for x in 1..=4 {
        for z in 1..=4 {
            scene.set_voxel(Cell(x, 7, z), Voxel::Solid).unwrap();
        }
    }
    let covered = detect_architecture(&scene);
    let courtyard = covered.at_floor(Cell(2, 1, 2)).unwrap();
    assert_eq!(courtyard.structural_area(), 16);
    assert_eq!(courtyard.boundary, BoundaryState::Closed);
    assert_eq!(courtyard.roof_fraction(), Some(1.0));
    assert_eq!(courtyard.volume, Volume::Open);
}

#[test]
fn outdoor_stair_access_does_not_merge_terrace_with_lower_ground() {
    let mut scene = Scene::new(Bounds {
        min: Cell(-2, 0, -2),
        max: Cell(7, 7, 7),
    })
    .unwrap();
    for x in -2..=7 {
        for z in -2..=7 {
            scene.set_voxel(Cell(x, 0, z), Voxel::Solid).unwrap();
            scene.set_known_sky(x, z, true).unwrap();
        }
    }
    for x in 2..=4 {
        for z in 2..=4 {
            scene.set_voxel(Cell(x, 1, z), Voxel::Solid).unwrap();
        }
    }
    scene
        .add_surface_link(Cell(1, 1, 3), Cell(2, 2, 3))
        .unwrap();
    let result = detect_architecture(&scene);
    let ground = result.at_floor(Cell(1, 1, 3)).unwrap();
    let terrace = result.at_floor(Cell(2, 2, 3)).unwrap();
    assert_ne!(ground.id, terrace.id);
    assert_eq!(terrace.structural_area(), 9);
    assert!(result.connections.iter().any(|p| p.traversable
        && p.ends[0].contains(&ground.id)
        && p.ends[1].contains(&terrace.id)));
}

#[test]
fn unknown_cave_continuation_never_claims_enclosure() {
    let bounds = Bounds {
        min: Cell(0, 0, 0),
        max: Cell(12, 4, 6),
    };
    let mut scene = Scene::new(bounds).unwrap();
    for x in 0..=12 {
        for z in 0..=6 {
            scene.set_voxel(Cell(x, 0, z), Voxel::Solid).unwrap();
            scene.set_voxel(Cell(x, 4, z), Voxel::Solid).unwrap();
            if z == 0 || z == 6 {
                for y in 1..=3 {
                    scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
            }
        }
    }
    let result = detect_architecture(&scene);
    let cave = result.at_floor(Cell(6, 1, 3)).unwrap();
    assert_eq!(cave.volume, Volume::Unknown);
    assert_eq!(cave.boundary, BoundaryState::Unknown);
    assert!(cave.dependencies.contains(&Cell(13, 1, 3)));
}

#[test]
fn gallery_balcony_and_cave_are_stable_under_rotation_and_edit_order() {
    for (scene, sample) in [
        (fixtures::gallery_hall(), Cell(3, 1, 3)),
        (fixtures::balcony(), Cell(6, 4, 2)),
        (fixtures::continuing_cave(true), Cell(7, 2, 3)),
    ] {
        let original = detect_architecture(&scene);
        let expected = original.at_floor(sample).unwrap();
        for turns in 0..4 {
            for reverse in [false, true] {
                let changed = fixtures::permuted(&scene, turns, reverse);
                let result = detect_architecture(&changed);
                let space = result
                    .at_floor(fixtures::rotate_cell(sample, turns))
                    .unwrap();
                assert_eq!(space.structural_area(), expected.structural_area());
                assert_eq!(space.volume, expected.volume);
                assert_eq!(
                    space.floor().collect::<BTreeSet<_>>(),
                    expected
                        .floor()
                        .map(|c| fixtures::rotate_cell(c, turns))
                        .collect()
                );
            }
        }
    }
}

#[test]
fn balcony_and_parapet_are_not_part_of_the_lower_interior() {
    let scene = fixtures::balcony();
    let result = detect_architecture(&scene);
    let room = result.at_floor(Cell(4, 4, 2)).unwrap();
    let balcony = result.at_floor(Cell(6, 4, 2)).unwrap();
    assert_eq!(room.structural_area(), 16);
    assert_eq!(room.volume, Volume::Bounded(48));
    assert_eq!(balcony.structural_area(), 9);
    assert_eq!(balcony.volume, Volume::Open);
    assert_ne!(room.id, balcony.id);
    let mut scene = fixtures::room(4, 4, 1, None);
    scene
        .add_surface_link(Cell(1, 1, 2), Cell(0, 2, 2))
        .unwrap();
    let result = detect_architecture(&scene);
    let courtyard = result.at_floor(Cell(1, 1, 2)).unwrap();
    let parapet = result.at_floor(Cell(0, 2, 2)).unwrap();
    assert_ne!(courtyard.id, parapet.id);
    assert_eq!(courtyard.structural_area(), 16);
    assert_eq!(parapet.structural_area(), 20);
}

#[test]
fn cave_ledge_is_pending_when_the_containing_cave_continues_into_unknown_chunks() {
    let scene = fixtures::continuing_cave(true);
    let result = detect_architecture(&scene);
    let lower = result.at_floor(Cell(5, 1, 3)).unwrap();
    let ledge = result.at_floor(Cell(6, 2, 3)).unwrap();
    assert_ne!(lower.id, ledge.id);
    assert!(lower.pending && ledge.pending);
    assert_eq!(lower.volume, Volume::Unknown);
    assert_eq!(ledge.volume, Volume::Unknown);
}

#[test]
fn actual_storeys_with_a_stairwell_door_preserve_separate_space_membership() {
    for open in [false, true] {
        let scene = fixtures::two_storeys(open);
        let result = detect_architecture(&scene);
        let lower = result.at_floor(Cell(1, 1, 1)).unwrap();
        let upper = result.at_floor(Cell(4, 4, 1)).unwrap();
        assert_ne!(lower.id, upper.id);
        assert_eq!(lower.structural_area(), 24);
        assert_eq!(lower.volume, Volume::Bounded(57));
        assert_eq!(upper.structural_area(), 17);
        assert_eq!(upper.volume, Volume::Bounded(50));
        assert!(result.connections.iter().any(|p| p.traversable == open
            && p.ends[0].contains(&lower.id)
            && p.ends[1].contains(&upper.id)));
    }
}

#[test]
fn rendered_contract_cases_have_fixed_results_in_every_orientation() {
    for fixture in fixtures::contract_cases() {
        for turns in 0..4 {
            for reverse in [false, true] {
                let scene = fixtures::permuted(&fixture.scene, turns, reverse);
                let result = detect_architecture(&scene);
                let space = result
                    .at_floor(fixtures::rotate_cell(fixture.sample, turns))
                    .unwrap();
                assert_eq!(
                    space.structural_area(),
                    fixture.area,
                    "{} turn={turns} reverse={reverse}",
                    fixture.name
                );
                assert_eq!(
                    space.volume, fixture.volume,
                    "{} turn={turns} reverse={reverse}",
                    fixture.name
                );
            }
        }
    }
}

#[test]
fn support_edits_recompute_every_contract_fixture_and_restore_exactly() {
    for fixture in fixtures::contract_cases() {
        let before = detect_architecture(&fixture.scene);
        let mut edited = fixture.scene.clone();
        let support = fixture.sample.offset(0, -1, 0);
        edited.set_voxel(support, Voxel::Air).unwrap();
        assert!(
            detect_architecture(&edited)
                .at_floor(fixture.sample)
                .is_none(),
            "{}",
            fixture.name
        );
        edited
            .set_voxel(support, fixture.scene.voxel(support))
            .unwrap();
        assert_eq!(detect_architecture(&edited), before, "{}", fixture.name);
    }
}
