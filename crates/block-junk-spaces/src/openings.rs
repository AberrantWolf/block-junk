//! Structural frame recognition. Passage geometry is separate from either room;
//! inferred partitions are derived evidence, never authored world mutations.
use crate::{Cell, Face, Partition, Scene, Voxel};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opening {
    /// Entire threshold surface, including every slice of a thick wall.
    pub floor_cells: Vec<Cell>,
    /// Minimum clearance anywhere in the passage.
    pub height: i32,
    pub depth: usize,
    /// Clearance profile across each successive slice of wall thickness.
    pub profiles: Vec<Vec<i32>>,
    /// Air belonging to the connection rather than an adjoining room.
    pub volume_cells: BTreeSet<Cell>,
    pub approaches: [Vec<Cell>; 2],
    /// Entry face at the negative side, retained for the simple patch oracle.
    pub faces: Vec<Face>,
    pub evidence: BTreeSet<Cell>,
}

/// Returns recognized geometry without changing the input. Authored partitions
/// always win. The finite oracle scans exhaustively; live dependency tracking
/// must record unsuccessful queries too, not just these positive witnesses.
pub fn infer_openings(scene: &Scene) -> Vec<Opening> {
    let mut openings = Vec::new();
    for seed in scene.bounds().cells() {
        for normal_x in [true, false] {
            if let Some(opening) = candidate(scene, seed, normal_x) {
                openings.push(opening);
            }
        }
    }
    let ambiguous: BTreeSet<usize> = openings
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            openings
                .iter()
                .enumerate()
                .skip(i + 1)
                .filter_map(move |(j, b)| {
                    a.volume_cells
                        .iter()
                        .any(|c| b.volume_cells.contains(c))
                        .then_some([i, j])
                })
                .flatten()
        })
        .collect();
    openings
        .into_iter()
        .enumerate()
        .filter_map(|(i, opening)| (!ambiguous.contains(&i)).then_some(opening))
        .collect()
}

pub fn with_inferred_openings(scene: &Scene) -> (Scene, Vec<Opening>) {
    let openings = infer_openings(scene);
    let mut prepared = scene.clone();
    for opening in &openings {
        for &face in &opening.faces {
            prepared
                .set_partition(face, Partition::Portal { open: true })
                .expect("recognizer checked endpoints");
        }
    }
    (prepared, openings)
}

/// A profile rises towards a central maximum, then falls. Level rectangular
/// openings are included. No symmetry or one-cell increment restriction: the
/// separate connected-lintel check verifies the solid risers between heights.
fn central_arch(profile: &[i32]) -> bool {
    let maximum = *profile.iter().max().expect("nonempty opening");
    let left_middle = (profile.len() - 1) / 2;
    let right_middle = profile.len() / 2;
    if profile[left_middle] != maximum && profile[right_middle] != maximum {
        return false;
    }
    let mut falling = false;
    for pair in profile.windows(2) {
        if pair[1] < pair[0] {
            falling = true;
        }
        if falling && pair[1] > pair[0] {
            return false;
        }
    }
    true
}

fn candidate(scene: &Scene, seed: Cell, normal_x: bool) -> Option<Opening> {
    let mut evidence = BTreeSet::new();
    let mut read = |c| {
        evidence.insert(c);
        scene.voxel(c)
    };
    let tangent = |c: Cell, n| {
        if normal_x {
            c.offset(0, 0, n)
        } else {
            c.offset(n, 0, 0)
        }
    };
    let normal = |c: Cell, n| {
        if normal_x {
            c.offset(n, 0, 0)
        } else {
            c.offset(0, 0, n)
        }
    };
    if read(seed) != Voxel::Air || read(tangent(seed, -1)) != Voxel::Solid {
        return None;
    }
    let tangent_end = if normal_x {
        scene.bounds().max.2 - seed.2
    } else {
        scene.bounds().max.0 - seed.0
    };
    let mut width = 0;
    for offset in 0..=tangent_end {
        match read(tangent(seed, offset)) {
            Voxel::Air => width += 1,
            Voxel::Solid => break,
            Voxel::Unknown => return None,
        }
    }
    if width == 0 {
        return None;
    }
    let jambs = [tangent(seed, -1), tangent(seed, width)];
    if jambs
        .iter()
        .any(|&j| read(j) != Voxel::Solid || read(normal(j, -1)) != Voxel::Air)
    {
        return None;
    }
    // Only the first slice is a candidate. A corridor without both exposed
    // ends cannot become a succession of invented doorways.
    let normal_end = if normal_x {
        scene.bounds().max.0 - seed.0
    } else {
        scene.bounds().max.2 - seed.2
    };
    let mut depth = 0;
    for offset in 0..=normal_end {
        let sides = jambs.map(|j| read(normal(j, offset)));
        match sides {
            [Voxel::Solid, Voxel::Solid] => depth += 1,
            [Voxel::Air, Voxel::Air] => break,
            _ => return None,
        }
    }
    if depth == 0 || jambs.iter().any(|&j| read(normal(j, depth)) != Voxel::Air) {
        return None;
    }
    let mut floor_cells = Vec::new();
    let mut volume_cells = BTreeSet::new();
    let mut profiles = Vec::new();
    for slice in 0..depth {
        let mut profile = Vec::new();
        for offset in 0..width {
            let floor = normal(tangent(seed, offset), slice);
            if read(floor.offset(0, -1, 0)) != Voxel::Solid {
                return None;
            }
            let mut ceiling = None;
            for dy in 0..=scene.bounds().max.1 - floor.1 {
                match read(floor.offset(0, dy, 0)) {
                    Voxel::Air => {
                        volume_cells.insert(floor.offset(0, dy, 0));
                    }
                    Voxel::Solid => {
                        ceiling = Some(dy);
                        break;
                    }
                    Voxel::Unknown => return None,
                }
            }
            let height = ceiling?;
            if height < 2 {
                return None;
            }
            profile.push(height);
            floor_cells.push(floor);
        }
        if !central_arch(&profile) {
            return None;
        }
        for (jamb, height) in [
            (jambs[0], profile[0]),
            (jambs[1], profile[profile.len() - 1]),
        ] {
            for dy in 0..=height {
                if read(normal(jamb, slice).offset(0, dy, 0)) != Voxel::Solid {
                    return None;
                }
            }
        }
        // Adjacent roof voxels must be connected by structural risers, not
        // merely touch at a corner or float at different heights.
        for offset in 0..width - 1 {
            let a = profile[offset as usize];
            let b = profile[offset as usize + 1];
            let lower_column = offset + i32::from(b < a);
            for dy in a.min(b)..=a.max(b) {
                if read(normal(tangent(seed, lower_column), slice).offset(0, dy, 0)) != Voxel::Solid
                {
                    return None;
                }
            }
        }
        profiles.push(profile);
    }
    let approaches = [
        (0..width)
            .map(|offset| normal(tangent(seed, offset), -1))
            .collect::<Vec<_>>(),
        (0..width)
            .map(|offset| normal(tangent(seed, offset), depth))
            .collect::<Vec<_>>(),
    ];
    for floor in approaches.iter().flatten() {
        if read(floor.offset(0, -1, 0)) != Voxel::Solid {
            return None;
        }
        for dy in 0..2 {
            if read(floor.offset(0, dy, 0)) != Voxel::Air {
                return None;
            }
        }
    }
    // Authored geometry anywhere in the passage has precedence, including a
    // declared window or a portal at its far end.
    if scene.partitions.keys().any(|face| {
        let (a, b) = face.cells();
        volume_cells.contains(&a) || volume_cells.contains(&b)
    }) {
        return None;
    }
    let mut faces = Vec::new();
    for offset in 0..width {
        let floor = tangent(seed, offset);
        for dy in 0..profiles[0][offset as usize] {
            let b = floor.offset(0, dy, 0);
            let a = normal(b, -1);
            if read(a) != Voxel::Air {
                return None;
            }
            faces.push(Face::new(a, b).ok()?);
        }
    }
    let height = *profiles.iter().flatten().min()?;
    Some(Opening {
        floor_cells,
        height,
        depth: depth as usize,
        profiles,
        volume_cells,
        approaches,
        faces,
        evidence,
    })
}
