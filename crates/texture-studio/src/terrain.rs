//! The studio's pseudo-terrain sample scene: a little sine-hill voxel
//! field meshed with naive neighbor-culled quads. Slots are fixed bands
//! (surface / soil / rock / accent); the UI maps each band to a texture
//! id, so the same mesh demos any texture set the doc defines.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};

pub const SIZE: i32 = 48;

/// Band slots baked into the mesh's vertex-color alpha. Slot 0 = air.
pub const SLOT_SURFACE: u8 = 1;
pub const SLOT_SOIL: u8 = 2;
pub const SLOT_ROCK: u8 = 3;
pub const SLOT_ACCENT: u8 = 4;
pub const SLOT_COUNT: usize = 5;

fn height(x: i32, z: i32) -> i32 {
    let h = (x as f32 * 0.14).sin() * 3.5 + (z as f32 * 0.09).sin() * 4.0 + 8.0;
    h.floor() as i32
}

/// Hash-blob accent patches on the surface (shows a 4th texture, e.g.
/// gravel, scattered across the hills).
fn is_accent(x: i32, z: i32) -> bool {
    let cx = x.div_euclid(7);
    let cz = z.div_euclid(7);
    let mut h = (cx as u32).wrapping_mul(0x9E37_79B9) ^ (cz as u32).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    // ~1 in 4 patch cells, and only the middle of the cell.
    (h & 3) == 0 && x.rem_euclid(7) >= 2 && x.rem_euclid(7) <= 5 && z.rem_euclid(7) >= 2 && z.rem_euclid(7) <= 5
}

fn slot_at(x: i32, y: i32, z: i32) -> u8 {
    if !(0..SIZE).contains(&x) || !(0..SIZE).contains(&z) || y < 0 {
        // Outside the field reads as solid below the surface line so the
        // outer walls don't render (no quads against off-field cells)…
        // except we DO want the boundary walls visible: treat outside as
        // air instead, giving the diorama a cut-away cake look.
        return 0;
    }
    let h = height(x, z);
    if y >= h {
        0
    } else if y == h - 1 {
        if is_accent(x, z) { SLOT_ACCENT } else { SLOT_SURFACE }
    } else if y >= h - 4 {
        SLOT_SOIL
    } else {
        SLOT_ROCK
    }
}

/// Build the diorama mesh. Same vertex layout as the game's chunk mesh:
/// POSITION + NORMAL + COLOR with the band slot in the color alpha.
pub fn build_mesh() -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[f32; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    // (normal, quad corners CCW from outside)
    const FACES: [([f32; 3], [[f32; 3]; 4]); 6] = [
        // +Y
        ([0.0, 1.0, 0.0], [[0., 1., 0.], [0., 1., 1.], [1., 1., 1.], [1., 1., 0.]]),
        // -Y
        ([0.0, -1.0, 0.0], [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]]),
        // +X
        ([1.0, 0.0, 0.0], [[1., 0., 0.], [1., 1., 0.], [1., 1., 1.], [1., 0., 1.]]),
        // -X
        ([-1.0, 0.0, 0.0], [[0., 0., 0.], [0., 0., 1.], [0., 1., 1.], [0., 1., 0.]]),
        // +Z
        ([0.0, 0.0, 1.0], [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]]),
        // -Z
        ([0.0, 0.0, -1.0], [[0., 0., 0.], [0., 1., 0.], [1., 1., 0.], [1., 0., 0.]]),
    ];

    let max_h = (0..SIZE)
        .flat_map(|x| (0..SIZE).map(move |z| height(x, z)))
        .max()
        .unwrap_or(8);

    for x in 0..SIZE {
        for z in 0..SIZE {
            for y in 0..=max_h {
                let slot = slot_at(x, y, z);
                if slot == 0 {
                    continue;
                }
                for (fi, (normal, corners)) in FACES.iter().enumerate() {
                    let (dx, dy, dz) = match fi {
                        0 => (0, 1, 0),
                        1 => (0, -1, 0),
                        2 => (1, 0, 0),
                        3 => (-1, 0, 0),
                        4 => (0, 0, 1),
                        _ => (0, 0, -1),
                    };
                    if slot_at(x + dx, y + dy, z + dz) != 0 {
                        continue;
                    }
                    let base = positions.len() as u32;
                    for c in corners {
                        positions.push([x as f32 + c[0], y as f32 + c[1], z as f32 + c[2]]);
                        normals.push(*normal);
                        colors.push([1.0, 1.0, 1.0, slot as f32 / 255.0]);
                    }
                    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                }
            }
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}
