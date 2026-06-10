//! Shared egui font setup (feature `egui-fonts`).
//!
//! egui's bundled fonts have thin symbol coverage — arrows, triangles,
//! die faces, ⌘ and friends render as tofu. We vendor DejaVu Sans
//! (`fonts/DejaVuSans.ttf`, free Bitstream Vera-derived license — see
//! `fonts/LICENSE-DejaVu.txt`, redistributable with source and binaries)
//! and register it as a *fallback*, so the default look is unchanged and
//! only missing glyphs fall through to DejaVu.
//!
//! The bytes are compiled in via `include_bytes!` — nothing to download
//! or install at clone/build time.
//!
//! Glyph budget: DejaVu covers U+2190–U+21FF arrows, U+25A0–U+25FF
//! geometric shapes, U+2680–⚅ die faces, U+2318 ⌘, U+2715 ✕. It does
//! NOT cover emoji-plane glyphs (e.g. 🎲 U+1F3B2) — pick BMP symbols
//! for UI icons.

use std::sync::Arc;

use egui::epaint::text::{FontData, FontDefinitions, FontFamily};

static DEJAVU: &[u8] = include_bytes!("../fonts/DejaVuSans.ttf");

/// Append DejaVu Sans as the last-resort fallback for both families.
/// Call once per egui context (idempotent in effect, but it rebuilds the
/// font atlas — don't call every frame).
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("dejavu".to_owned(), Arc::new(FontData::from_static(DEJAVU)));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("dejavu".to_owned());
    }
    ctx.set_fonts(fonts);
}
