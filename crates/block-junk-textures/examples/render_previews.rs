//! Flatten every texture in a `textures.lua` to PNG — quick visual check
//! without booting the game or the studio.
//!
//!     cargo run -p block-junk-textures --example render_previews -- \
//!         mods/vanilla/textures.lua /tmp/tex-previews [span_blocks]
//!
//! Each texture renders once at `span` world blocks (default: its layers'
//! combined LCM period, clamped to 12) so the multi-period composite and
//! the finish jitter are visible.

use std::path::PathBuf;

use block_junk_textures::{bake_texture, flatten, lua_io};

fn main() {
    let mut args = std::env::args().skip(1);
    let doc_path = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "mods/vanilla/textures.lua".into()),
    );
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/tex-previews".into()));
    let span_override: Option<f32> = args.next().and_then(|s| s.parse().ok());

    let doc = lua_io::parse_file(&doc_path).expect("parse textures.lua");
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    for tex in &doc.textures {
        let baked = bake_texture(tex, doc.pixels_per_block).expect("bake");
        let lcm = tex
            .layers
            .iter()
            .map(|l| l.period as u64)
            .fold(1u64, |acc, p| acc * p / gcd(acc, p));
        let span = span_override.unwrap_or((lcm as f32).min(12.0).max(2.0));
        let px = 512u32;
        let rgba = flatten(&baked, px, [0.0, 0.0], span, [0.3, 0.3, 0.3], true);

        let name = tex.id.replace(':', "_");
        let path = out_dir.join(format!("{name}.png"));
        let file = std::fs::File::create(&path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), px, px);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        println!("{} → {} ({span} blocks)", tex.id, path.display());
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
