//! Render the fixed acceptance suite. Optional argument: directory for individual
//! SVG panels, suitable for opening at full size or reviewing geometry edits.
use block_junk_spaces::{Cell, Voxel, detect_architecture, fixtures};
use std::{
    fmt::Write as _,
    io::{self, Write},
};
fn main() -> io::Result<()> {
    let directory = std::env::args().nth(1);
    if let Some(directory) = &directory {
        std::fs::create_dir_all(directory)?;
    }
    let cases = fixtures::contract_cases();
    let rows = cases.len().div_ceil(3);
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1480" height="{}" viewBox="0 0 1480 {}"><rect width="1480" height="{}" fill="#f4f1e9"/><style>text{{font-family:monospace;fill:#263a39}} .title{{font-size:16px;font-weight:bold}} .note{{font-size:12px}}</style><text x="20" y="30" class="title">Architectural spaces: fixed reference contract</text><text x="20" y="55" class="note">Teal: selected space at this elevation. Orange: connection threshold. Gold: unique furnishings. Purple: unknown.</text>"##,
        rows * 410 + 90,
        rows * 410 + 90,
        rows * 410 + 90
    )?;
    for (index, fixture) in cases.iter().enumerate() {
        let result = detect_architecture(&fixture.scene);
        let selected = result.at_floor(fixture.sample).unwrap();
        assert_eq!(selected.structural_area(), fixture.area, "{}", fixture.name);
        assert_eq!(selected.volume, fixture.volume, "{}", fixture.name);
        let bounds = fixture.scene.bounds();
        let size =
            24.min(260 / (bounds.max.0 - bounds.min.0 + 1).max(bounds.max.2 - bounds.min.2 + 1));
        let mut panel = String::new();
        write!(panel,r##"<rect width="470" height="390" rx="8" fill="#fffdf8" stroke="#d9d5cc"/><text x="14" y="25" class="title">{:02}. {}</text><text x="14" y="45" class="note">Floor slice y={}</text>"##,index+1,fixture.name,fixture.sample.1).unwrap();
        for x in bounds.min.0..=bounds.max.0 {
            for z in bounds.min.2..=bounds.max.2 {
                let cell = Cell(x, fixture.sample.1, z);
                let color = match fixture.scene.voxel(cell) {
                    Voxel::Solid => "#374b4a",
                    Voxel::Unknown => "#b993a6",
                    Voxel::Air => {
                        if result
                            .connections
                            .iter()
                            .any(|p| p.threshold.contains(&cell))
                        {
                            "#e8ae5c"
                        } else if result.at_floor(cell).is_some_and(|s| s.id == selected.id) {
                            "#86c4b4"
                        } else if result.at_floor(cell).is_some() {
                            "#d5e4e6"
                        } else {
                            "#eeebe4"
                        }
                    }
                };
                let px = 14 + (x - bounds.min.0) * size;
                let pz = 60 + (z - bounds.min.2) * size;
                write!(
                    panel,
                    r##"<rect x="{px}" y="{pz}" width="{}" height="{}" fill="{color}"/>"##,
                    size - 1,
                    size - 1
                )
                .unwrap();
                if let Some(p) = fixture.scene.placements().find(|p| p.cells.contains(&cell)) {
                    write!(panel,r##"<rect x="{}" y="{}" width="{}" height="{}" fill="#f4d69b"/><text x="{}" y="{}" font-size="9">{}</text>"##,px+2,pz+2,size-5,size-5,px+3,pz+size-5,p.id.0).unwrap();
                }
            }
        }
        for connection in &result.connections {
            for &face in &connection.faces {
                let (a, b) = face.cells();
                if a.1 != fixture.sample.1 {
                    continue;
                }
                let (x1, y1, x2, y2) = if a.0 != b.0 {
                    let x = 14 + (a.0.max(b.0) - bounds.min.0) * size;
                    let y = 60 + (a.2 - bounds.min.2) * size;
                    (x, y, x, y + size)
                } else {
                    let x = 14 + (a.0 - bounds.min.0) * size;
                    let y = 60 + (a.2.max(b.2) - bounds.min.2) * size;
                    (x, y, x + size, y)
                };
                write!(panel,r##"<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="#c57827" stroke-width="3"/>"##).unwrap();
            }
        }
        let side_size = 12.min(145 / (bounds.max.0 - bounds.min.0 + 1));
        write!(
            panel,
            r##"<text x="304" y="60" class="note">Side at z={}</text>"##,
            fixture.sample.2
        )
        .unwrap();
        for x in bounds.min.0..=bounds.max.0 {
            for y in bounds.min.1..=bounds.max.1 {
                let cell = Cell(x, y, fixture.sample.2);
                let color = match fixture.scene.voxel(cell) {
                    Voxel::Solid => "#374b4a",
                    Voxel::Unknown => "#b993a6",
                    Voxel::Air => {
                        if result.connections.iter().any(|p| p.volume.contains(&cell)) {
                            "#e8ae5c"
                        } else if result.at_floor(cell).is_some_and(|s| s.id == selected.id) {
                            "#86c4b4"
                        } else {
                            "#eeebe4"
                        }
                    }
                };
                let px = 304 + (x - bounds.min.0) * side_size;
                let py = 76 + (bounds.max.1 - y) * side_size;
                write!(
                    panel,
                    r##"<rect x="{px}" y="{py}" width="{}" height="{}" fill="{color}"/>"##,
                    side_size - 1,
                    side_size - 1
                )
                .unwrap();
            }
        }
        let surfaces = selected
            .floor_surfaces
            .iter()
            .map(|(y, cells)| format!("y{y}:{}", cells.len()))
            .collect::<Vec<_>>()
            .join(" ");
        write!(panel,r##"<text x="14" y="332" class="note">Area {} / volume {:?} / coverage {:?}</text><text x="14" y="352" class="note">Surfaces: {surfaces}</text><text x="14" y="372" class="note">Boundary {:?}; pending {}; furnishings {}</text>"##,selected.structural_area(),selected.volume,selected.roof_fraction().map(|v|(v*100.0).round() as u32),selected.boundary,selected.pending,selected.placements.len()).unwrap();
        writeln!(
            out,
            r##"<g transform="translate({}, {})">{panel}</g>"##,
            20 + (index % 3) * 490,
            80 + (index / 3) * 410
        )?;
        if let Some(directory) = &directory {
            std::fs::write(
                format!("{directory}/{:02}.svg", index + 1),
                format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" width="470" height="390" viewBox="0 0 470 390"><style>text{{font-family:monospace;fill:#263a39}} .title{{font-size:16px;font-weight:bold}} .note{{font-size:12px}}</style>{panel}</svg>"##
                ),
            )?;
        }
    }
    writeln!(out, "</svg>")
}
