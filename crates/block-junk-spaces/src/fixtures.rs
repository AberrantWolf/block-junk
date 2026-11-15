//! Small, editable structural scenes shared by tests and the SVG inspection tool.
//! Builders contain no expected memberships: the detector derives those from voxels.

use crate::{Bounds, Cell, Face, Partition, Placement, Scene, Voxel};

/// Interior x/z are 1..=width/depth; floor is y=1. All observed columns
/// explicitly have known sky above the observation domain.
pub fn room(width: i32, depth: i32, wall_height: i32, roof_y: Option<i32>) -> Scene {
    assert!(width > 0 && depth > 0 && wall_height > 0);
    let top = roof_y.unwrap_or(8).max(wall_height + 1) + 2;
    let bounds = Bounds {
        min: Cell(-1, 0, -1),
        max: Cell(width + 2, top, depth + 2),
    };
    let mut scene = Scene::new(bounds).unwrap();
    for x in bounds.min.0..=bounds.max.0 {
        for z in bounds.min.2..=bounds.max.2 {
            scene.set_voxel(Cell(x, 0, z), Voxel::Solid).unwrap();
            scene.set_known_sky(x, z, true).unwrap();
        }
    }
    for x in 0..=width + 1 {
        for z in 0..=depth + 1 {
            if x == 0 || x == width + 1 || z == 0 || z == depth + 1 {
                for y in 1..=wall_height {
                    scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
            }
            if let Some(y) = roof_y {
                scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
            }
        }
    }
    scene
}

pub fn wall_beds() -> Scene {
    let mut scene = room(4, 4, 2, Some(3));
    for (id, x) in [(1, 1), (2, 3)] {
        scene
            .add_placement(Placement::new(id, [Cell(x, 1, 1), Cell(x + 1, 1, 1)], ["bed"]).unwrap())
            .unwrap();
    }
    scene
}

pub fn partial_table_contacts() -> Scene {
    let mut scene = room(4, 4, 2, Some(3));
    for (id, z) in [(1, 1), (2, 4)] {
        scene
            .add_placement(Placement::new(id, [Cell(2, 1, z), Cell(3, 1, z)], ["table"]).unwrap())
            .unwrap();
    }
    for (id, c) in [(3, Cell(2, 1, 2)), (4, Cell(3, 1, 3))] {
        scene
            .add_placement(Placement::new(id, [c], ["seat"]).unwrap())
            .unwrap();
    }
    scene
}

pub fn double_door(open: bool) -> Scene {
    let mut scene = room(7, 4, 2, Some(3));
    for z in 1..=4 {
        for y in 1..=2 {
            if z == 2 || z == 3 {
                scene
                    .set_partition(
                        Face::new(Cell(3, y, z), Cell(4, y, z)).unwrap(),
                        Partition::Portal { open },
                    )
                    .unwrap();
            } else {
                scene.set_voxel(Cell(4, y, z), Voxel::Solid).unwrap();
            }
        }
    }
    scene
}

pub fn stepped_room() -> Scene {
    let mut scene = room(4, 4, 3, Some(4));
    for x in 3..=4 {
        for z in 1..=4 {
            scene.set_voxel(Cell(x, 1, z), Voxel::Solid).unwrap();
        }
    }
    scene
        .add_surface_link(Cell(2, 1, 2), Cell(3, 2, 2))
        .unwrap();
    scene
}

/// Two areas joined by an unannotated rectangular opening in a thin wall.
/// The recognizer, not the fixture, must derive the portal faces.
pub fn framed_opening(width: i32, height: i32) -> Scene {
    let mut scene = room(7, width + 2, height, Some(height + 1));
    for z in 1..=width + 2 {
        for y in 1..=height {
            if z == 1 || z == width + 2 {
                scene.set_voxel(Cell(4, y, z), Voxel::Solid).unwrap();
            }
        }
    }
    scene
}

/// A flat or stepped structural arch through an arbitrary finite wall thickness.
/// Heights are measured above y=1; the first threshold slice is x=4, z=2.
pub fn profiled_opening(profile: &[i32], depth: i32) -> Scene {
    assert!(!profile.is_empty() && depth > 0 && profile.iter().all(|h| *h >= 2));
    let width = profile.len() as i32;
    let height = *profile.iter().max().unwrap();
    let mut scene = room(depth + 6, width + 2, height, Some(height + 1));
    for x in 4..4 + depth {
        for z in 1..=width + 2 {
            for y in 1..=height {
                if z == 1 || z == width + 2 || y > profile[(z - 2) as usize] {
                    scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
            }
        }
    }
    scene
}

pub fn gallery_hall() -> Scene {
    let mut scene = room(6, 6, 6, Some(7));
    for x in 4..=6 {
        for z in 1..=6 {
            scene.set_voxel(Cell(x, 1, z), Voxel::Solid).unwrap();
        }
    }
    for x in 1..=2 {
        for z in 1..=6 {
            scene.set_voxel(Cell(x, 4, z), Voxel::Solid).unwrap();
        }
    }
    scene
}

pub fn balcony() -> Scene {
    let bounds = Bounds {
        min: Cell(-2, 0, -2),
        max: Cell(10, 10, 7),
    };
    let mut scene = Scene::new(bounds).unwrap();
    for x in -2..=10 {
        for z in -2..=7 {
            scene.set_voxel(Cell(x, 0, z), Voxel::Solid).unwrap();
            scene.set_known_sky(x, z, true).unwrap();
        }
    }
    for x in 0..=5 {
        for z in 0..=5 {
            scene.set_voxel(Cell(x, 3, z), Voxel::Solid).unwrap();
            scene.set_voxel(Cell(x, 7, z), Voxel::Solid).unwrap();
            if x == 0 || x == 5 || z == 0 || z == 5 {
                for y in 4..=6 {
                    scene.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
            }
        }
    }
    for x in 6..=8 {
        for z in 1..=3 {
            scene.set_voxel(Cell(x, 3, z), Voxel::Solid).unwrap();
        }
    }
    for y in 4..=5 {
        scene.set_voxel(Cell(5, y, 2), Voxel::Air).unwrap();
    }
    scene
}

pub fn continuing_cave(ledge: bool) -> Scene {
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
            if ledge && (6..=8).contains(&x) {
                scene.set_voxel(Cell(x, 1, z), Voxel::Solid).unwrap();
            }
        }
    }
    if ledge {
        scene
            .add_surface_link(Cell(5, 1, 3), Cell(6, 2, 3))
            .unwrap();
    }
    scene
}

pub fn rotate_cell(mut cell: Cell, turns: usize) -> Cell {
    for _ in 0..turns % 4 {
        cell = Cell(-cell.2, cell.1, cell.0);
    }
    cell
}

/// Rebuild the same authored scene in another orientation/insertion order.
pub fn permuted(scene: &Scene, turns: usize, reverse: bool) -> Scene {
    let a = rotate_cell(scene.bounds.min, turns);
    let b = rotate_cell(scene.bounds.max, turns);
    let mut copy = Scene::new(Bounds {
        min: Cell(a.0.min(b.0), a.1, a.2.min(b.2)),
        max: Cell(a.0.max(b.0), b.1, a.2.max(b.2)),
    })
    .unwrap();
    let mut voxels: Vec<_> = scene.voxels.iter().collect();
    if reverse {
        voxels.reverse();
    }
    for (&cell, &voxel) in voxels {
        copy.set_voxel(rotate_cell(cell, turns), voxel).unwrap();
    }
    for &(x, z) in &scene.sky_columns {
        let c = rotate_cell(Cell(x, 0, z), turns);
        copy.set_known_sky(c.0, c.2, true).unwrap();
    }
    for (&face, &partition) in &scene.partitions {
        let (a, b) = face.cells();
        copy.set_partition(
            Face::new(rotate_cell(a, turns), rotate_cell(b, turns)).unwrap(),
            partition,
        )
        .unwrap();
    }
    for placement in scene.placements.values() {
        copy.add_placement(
            Placement::new(
                placement.id.0,
                placement.cells.iter().map(|&c| rotate_cell(c, turns)),
                placement.tags.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    }
    for link in &scene.links {
        copy.add_surface_link(rotate_cell(link.a, turns), rotate_cell(link.b, turns))
            .unwrap();
    }
    copy
}

pub fn two_storeys(open: bool) -> Scene {
    let mut scene = room(6, 4, 6, Some(7));
    for x in 1..=6 {
        for z in 1..=4 {
            scene.set_voxel(Cell(x, 3, z), Voxel::Solid).unwrap();
        }
    }
    for x in 1..=3 {
        scene.set_voxel(Cell(x, 3, 1), Voxel::Air).unwrap();
    }
    scene.set_voxel(Cell(2, 1, 1), Voxel::Solid).unwrap();
    for y in 1..=2 {
        scene.set_voxel(Cell(3, y, 1), Voxel::Solid).unwrap();
    }
    for x in 1..=4 {
        for y in 4..=6 {
            scene.set_voxel(Cell(x, y, 2), Voxel::Solid).unwrap();
        }
    }
    scene.set_voxel(Cell(4, 6, 1), Voxel::Solid).unwrap();
    for x in 1..=3 {
        scene
            .add_surface_link(Cell(x, x, 1), Cell(x + 1, x + 1, 1))
            .unwrap();
    }
    for y in 4..=5 {
        scene
            .set_partition(
                Face::new(Cell(3, y, 1), Cell(4, y, 1)).unwrap(),
                Partition::Portal { open },
            )
            .unwrap();
    }
    scene
}

pub struct ContractFixture {
    pub name: &'static str,
    pub scene: Scene,
    pub sample: Cell,
    pub area: usize,
    pub volume: crate::Volume,
}

/// Fixed acceptance cases shared with the inspection renderer. Expectations are
/// literals derived from authored geometry, never outputs of the detector.
pub fn contract_cases() -> Vec<ContractFixture> {
    use crate::Volume::{Bounded, Open, Unknown};
    let case = |name, scene, sample, area, volume| ContractFixture {
        name,
        scene,
        sample,
        area,
        volume,
    };
    let mut l = room(5, 5, 3, Some(4));
    let mut u = l.clone();
    for x in 1..=5 {
        for z in 1..=5 {
            for y in 1..=3 {
                if x >= 4 && z >= 4 {
                    l.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
                if (2..=4).contains(&x) && z >= 2 {
                    u.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
                }
            }
        }
    }
    let mut dense = room(6, 6, 3, Some(4));
    let mut id = 1;
    for x in [1, 3, 5] {
        for z in 1..=6 {
            dense
                .add_placement(
                    Placement::new(id, [Cell(x, 1, z), Cell(x + 1, 1, z)], ["bed"]).unwrap(),
                )
                .unwrap();
            id += 1;
        }
    }
    dense
        .add_placement(Placement::new(id, [Cell(1, 2, 1), Cell(1, 3, 1)], ["decoration"]).unwrap())
        .unwrap();
    let mut stepped_roof = room(3, 3, 5, None);
    for x in 1..=3 {
        for z in 1..=3 {
            stepped_roof
                .set_voxel(Cell(x, x + 3, z), Voxel::Solid)
                .unwrap();
        }
    }
    let mut skylight = room(4, 4, 3, Some(4));
    skylight.set_voxel(Cell(2, 4, 2), Voxel::Air).unwrap();
    let mut roof_last = gallery_hall();
    for x in 1..=6 {
        for z in 1..=6 {
            roof_last.set_voxel(Cell(x, 7, z), Voxel::Air).unwrap();
        }
    }
    let courtyard = room(4, 4, 1, None);
    let mut canopy = courtyard.clone();
    for x in 1..=4 {
        for z in 1..=4 {
            canopy.set_voxel(Cell(x, 7, z), Voxel::Solid).unwrap();
        }
    }
    let mut damaged = profiled_opening(&[2, 3, 4, 3, 2], 3);
    for y in 1..=4 {
        damaged.set_voxel(Cell(5, y, 1), Voxel::Air).unwrap();
    }
    let mut unknown = profiled_opening(&[2, 3, 4, 3, 2], 3);
    unknown.set_voxel(Cell(5, 2, 4), Voxel::Unknown).unwrap();
    let mut windows = double_door(false);
    for partition in windows.partitions.values_mut() {
        *partition = Partition::Window;
    }
    let mut crossing = room(7, 5, 2, Some(3));
    for (x, z) in [(4, 1), (4, 5), (2, 3), (6, 3)] {
        for y in 1..=2 {
            crossing.set_voxel(Cell(x, y, z), Voxel::Solid).unwrap();
        }
    }
    let mut parapet = courtyard.clone();
    parapet
        .add_surface_link(Cell(1, 1, 2), Cell(0, 2, 2))
        .unwrap();
    let mut terrace = Scene::new(Bounds {
        min: Cell(-2, 0, -2),
        max: Cell(7, 7, 7),
    })
    .unwrap();
    for x in -2..=7 {
        for z in -2..=7 {
            terrace.set_voxel(Cell(x, 0, z), Voxel::Solid).unwrap();
            terrace.set_known_sky(x, z, true).unwrap();
        }
    }
    for x in 2..=4 {
        for z in 2..=4 {
            terrace.set_voxel(Cell(x, 1, z), Voxel::Solid).unwrap();
        }
    }
    terrace
        .add_surface_link(Cell(1, 1, 3), Cell(2, 2, 3))
        .unwrap();
    vec![
        case(
            "Rectangle",
            room(4, 4, 3, Some(4)),
            Cell(2, 1, 2),
            16,
            Bounded(48),
        ),
        case("L-shaped interior", l, Cell(1, 1, 1), 21, Bounded(63)),
        case("U-shaped interior", u, Cell(1, 1, 1), 13, Bounded(39)),
        case(
            "Narrow interior",
            room(1, 9, 3, Some(4)),
            Cell(1, 1, 1),
            9,
            Bounded(27),
        ),
        case(
            "Beds against walls",
            wall_beds(),
            Cell(2, 1, 2),
            16,
            Bounded(32),
        ),
        case(
            "Dense and stacked furnishings",
            dense,
            Cell(1, 1, 1),
            36,
            Bounded(108),
        ),
        case("Stepped roof", stepped_roof, Cell(2, 1, 2), 9, Bounded(36)),
        case("Skylight", skylight, Cell(2, 1, 2), 16, Open),
        case(
            "Tall vault",
            room(3, 3, 8, Some(9)),
            Cell(2, 1, 2),
            9,
            Bounded(72),
        ),
        case(
            "Hall before roof completion",
            roof_last,
            Cell(3, 1, 3),
            48,
            Open,
        ),
        case("Low-wall courtyard", courtyard, Cell(2, 1, 2), 16, Open),
        case(
            "Floating canopy over courtyard",
            canopy,
            Cell(2, 1, 2),
            16,
            Open,
        ),
        case(
            "Thin rectangular passage",
            profiled_opening(&[3, 3, 3], 1),
            Cell(3, 1, 3),
            15,
            Bounded(45),
        ),
        case(
            "Thick rectangular passage",
            profiled_opening(&[3, 3, 3], 3),
            Cell(3, 1, 3),
            15,
            Bounded(45),
        ),
        case(
            "Thick stepped arch",
            profiled_opening(&[2, 3, 4, 3, 2], 3),
            Cell(3, 1, 4),
            21,
            Bounded(84),
        ),
        case(
            "Damaged frame joins interiors",
            damaged,
            Cell(3, 1, 4),
            58,
            Bounded(214),
        ),
        case("Authored windows", windows, Cell(2, 1, 2), 12, Bounded(24)),
        case(
            "Corridor without invented doors",
            room(2, 10, 3, Some(4)),
            Cell(1, 1, 5),
            20,
            Bounded(60),
        ),
        case(
            "Ambiguous crossing frames",
            crossing,
            Cell(3, 1, 3),
            31,
            Bounded(62),
        ),
        case(
            "Stepped hall",
            stepped_room(),
            Cell(2, 1, 2),
            16,
            Bounded(40),
        ),
        case(
            "Atrium and gallery: lower floor",
            gallery_hall(),
            Cell(3, 1, 3),
            48,
            Bounded(186),
        ),
        case(
            "Atrium and gallery: upper floor",
            gallery_hall(),
            Cell(1, 5, 3),
            48,
            Bounded(186),
        ),
        case(
            "Storeys: lower and stairwell",
            two_storeys(true),
            Cell(1, 1, 1),
            24,
            Bounded(57),
        ),
        case(
            "Storeys: upper behind closed door",
            two_storeys(false),
            Cell(4, 4, 1),
            17,
            Bounded(50),
        ),
        case(
            "Parapet with stair access",
            parapet,
            Cell(0, 2, 2),
            20,
            Open,
        ),
        case(
            "Balcony beyond a doorway",
            balcony(),
            Cell(6, 4, 2),
            9,
            Open,
        ),
        case("Outdoor terrace", terrace, Cell(2, 2, 3), 9, Open),
        case(
            "Cave continuation is unknown",
            continuing_cave(false),
            Cell(6, 1, 3),
            65,
            Unknown,
        ),
        case(
            "Cave ledge is pending",
            continuing_cave(true),
            Cell(7, 2, 3),
            15,
            Unknown,
        ),
        case(
            "Unknown geometry in a frame",
            unknown,
            Cell(3, 1, 4),
            57,
            Unknown,
        ),
    ]
}
