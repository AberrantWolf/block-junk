//! Front and plan views of the exact opening-contract fixtures.
use block_junk_spaces::{Cell, Voxel, fixtures, openings::infer_openings};
use std::io::{self, Write};
fn main() -> io::Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1100" height="800" viewBox="0 0 1100 800"><rect width="1100" height="800" fill="#f4f1e9"/><style>text{{font-family:monospace;fill:#263a39}} .title{{font-size:20px;font-weight:bold}} .note{{font-size:13px}}</style><text x="24" y="35" class="title">Passages own their threshold geometry</text><text x="24" y="60" class="note">Front view above; plan view below. Orange belongs to the connection, not either room.</text>"##
    )?;
    for (i, (title, profile, depth)) in [
        ("Thin rectangle", vec![3, 3, 3], 1),
        ("Thick rectangle", vec![3, 3, 3], 3),
        ("Thick stepped arch", vec![2, 3, 4, 3, 2], 3),
    ]
    .iter()
    .enumerate()
    {
        let scene = fixtures::profiled_opening(profile, *depth);
        let openings = infer_openings(&scene);
        let opening = &openings[0];
        let ox = 24 + i * 360;
        writeln!(
            out,
            r##"<g transform="translate({ox},92)"><rect width="338" height="670" rx="8" fill="#fffdf8" stroke="#d9d5cc"/><text x="16" y="30" class="title">{title}</text><text x="16" y="58" class="note">Front / structural profile</text>"##
        )?;
        let height = *profile.iter().max().unwrap();
        for z in 1..=profile.len() as i32 + 2 {
            for y in 0..=height + 1 {
                let cell = Cell(4, y, z);
                let color = if scene.voxel(cell) == Voxel::Solid {
                    "#374b4a"
                } else if opening.volume_cells.contains(&cell) {
                    "#e8ae5c"
                } else {
                    "#d5e4e6"
                };
                let x = 18 + (z - 1) * 38;
                let py = 76 + (height + 1 - y) * 38;
                writeln!(
                    out,
                    r##"<rect x="{x}" y="{py}" width="36" height="36" fill="{color}"/>"##
                )?;
            }
        }
        writeln!(
            out,
            r##"<text x="16" y="335" class="note">Plan / full wall thickness</text>"##
        )?;
        for x in 3..=4 + depth {
            for z in 1..=profile.len() as i32 + 2 {
                let cell = Cell(x, 1, z);
                let color = if scene.voxel(cell) == Voxel::Solid {
                    "#374b4a"
                } else if opening.floor_cells.contains(&cell) {
                    "#e8ae5c"
                } else {
                    "#86c4b4"
                };
                let px = 18 + (z - 1) * 38;
                let py = 355 + (x - 3) * 38;
                writeln!(
                    out,
                    r##"<rect x="{px}" y="{py}" width="36" height="36" fill="{color}"/>"##
                )?;
            }
        }
        writeln!(
            out,
            r##"<text x="16" y="585" class="note">Threshold area: {} cells</text><text x="16" y="610" class="note">Passage volume: {} cells</text><text x="16" y="635" class="note">Minimum clearance: {}</text></g>"##,
            opening.floor_cells.len(),
            opening.volume_cells.len(),
            opening.height
        )?;
    }
    writeln!(out, "</svg>")
}
