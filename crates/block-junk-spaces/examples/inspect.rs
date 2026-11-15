//! Render the same synthetic scenes exercised by the tests, without a renderer,
//! server, assets, or save file. Usage: cargo run -p block-junk-spaces --example
//! inspect > target/space-fixtures.svg

use block_junk_spaces::{fixtures, *};
use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut canopy = fixtures::room(4, 4, 1, None);
    for x in 1..=4 {
        for z in 1..=4 {
            canopy.set_voxel(Cell(x, 7, z), Voxel::Solid).unwrap();
        }
    }
    let (automatic_door, _) = openings::with_inferred_openings(&fixtures::framed_opening(2, 2));
    let panels = [
        (
            "Beds along a wall",
            "Two beds; structural area remains 16.",
            fixtures::wall_beds(),
            Cell(2, 1, 2),
        ),
        (
            "Partial table contacts",
            "Two distinct tables each touch a seat.",
            fixtures::partial_table_contacts(),
            Cell(2, 1, 2),
        ),
        (
            "Automatic double doorway",
            "Jambs + lintel infer a two-cell-wide portal.",
            automatic_door,
            Cell(2, 1, 2),
        ),
        (
            "Open courtyard",
            "Low walls define an area; sky stays open.",
            fixtures::room(4, 4, 1, None),
            Cell(2, 1, 2),
        ),
        (
            "Floating canopy",
            "Full overhead coverage is not enclosure.",
            canopy,
            Cell(2, 1, 2),
        ),
        (
            "Raised floor patch",
            "Two elevations remain separately addressable.",
            fixtures::stepped_room(),
            Cell(3, 2, 2),
        ),
    ];
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1040" height="1230" viewBox="0 0 1040 1230">
<rect width="1040" height="1230" fill="#f4f1e9"/>
<style>text {{font-family:monospace;fill:#263a39}} .title {{font-size:19px;font-weight:bold}} .note {{font-size:12px}} .metric {{font-size:13px}}</style>
<text x="30" y="34" class="title">Structural spaces — reference fixtures</text>
<text x="30" y="57" class="note">Top-down voxel slices; furniture overlays the structural floor instead of removing it.</text>
<text x="30" y="78" class="note">Teal: selected patch · pale blue: another patch · dark: structure · gold: furnishings · orange: portal</text>"##
    )?;
    for (index, (title, note, scene, sample)) in panels.iter().enumerate() {
        let ox = 20 + (index % 2) * 510;
        let oy = 100 + (index / 2) * 370;
        writeln!(
            out,
            r##"<g transform="translate({ox},{oy})"><rect width="490" height="350" rx="8" fill="#fffdf8" stroke="#d9d5cc"/>
<text x="16" y="28" class="title">{title}</text><text x="16" y="50" class="note">{note}</text>"##
        )?;
        let result = detect(scene);
        let patch = result.at_floor(*sample).unwrap();
        let bounds = scene.bounds();
        let size = 23;
        let gx = 18;
        let gz = 72;
        for x in bounds.min.0..=bounds.max.0 {
            for z in bounds.min.2..=bounds.max.2 {
                let c = Cell(x, sample.1, z);
                let px = gx + (x - bounds.min.0) * size;
                let pz = gz + (z - bounds.min.2) * size;
                let color = match scene.voxel(c) {
                    Voxel::Solid => "#374b4a",
                    Voxel::Unknown => "#b993a6",
                    Voxel::Air => match result.at_floor(c) {
                        Some(p) if p.id == patch.id => "#86c4b4",
                        Some(_) => "#d5e4e6",
                        None => "#edeae3",
                    },
                };
                writeln!(
                    out,
                    r##"<rect x="{px}" y="{pz}" width="{size}" height="{size}" fill="{color}" stroke="#fffdf8" stroke-width="1"/>"##
                )?;
                for placement in scene.placements().filter(|p| p.cells.contains(&c)) {
                    writeln!(
                        out,
                        r##"<rect x="{}" y="{}" width="17" height="17" rx="3" fill="#e2b95e"/><text x="{}" y="{}" class="note">{}</text>"##,
                        px + 3,
                        pz + 3,
                        px + 7,
                        pz + 16,
                        placement.id.0
                    )?;
                }
            }
        }
        for connection in &result.connections {
            if let Some(face) = connection.face {
                let (a, b) = face.cells();
                if a.1 != sample.1 {
                    continue;
                }
                let x = gx + (a.0 - bounds.min.0) * size;
                let z = gz + (a.2 - bounds.min.2) * size;
                let (x1, y1, x2, y2) = if a.0 != b.0 {
                    (x + size, z, x + size, z + size)
                } else {
                    (x, z + size, x + size, z + size)
                };
                writeln!(
                    out,
                    r##"<path d="M{x1},{y1} L{x2},{y2}" stroke="#e47726" stroke-width="4"/>"##
                )?;
            }
        }
        let roof = patch
            .roof_fraction()
            .map(|f| format!("{:.0}%", f * 100.0))
            .unwrap_or("unknown".into());
        let volume = patch
            .interior_volume
            .map(|v| v.to_string())
            .unwrap_or("not enclosed".into());
        writeln!(
            out,
            r##"<text x="18" y="277" class="metric">Floor y={} · area {} · placements {}</text>
<text x="18" y="299" class="metric">Roof {roof} · volume {volume}</text>
<text x="18" y="321" class="metric">Boundary {:?} · table/seat contacts {}</text></g>"##,
            sample.1,
            patch.structural_area(),
            patch.placements.len(),
            patch.boundary,
            result.adjacent_count(scene, patch.id, "table", "seat")
        )?;
    }
    writeln!(out, "</svg>")?;
    out.flush()
}
