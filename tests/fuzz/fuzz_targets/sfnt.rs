#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::render::sfnt::fixed::Int26_6;
use byblos::render::sfnt::{Buffer, Font, GlyphIndex};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    let Ok(f) = Font::parse(data, &lim) else {
        return;
    };
    let upem = Int26_6(f.units_per_em());
    for r in [0x20, 0x41, 0x78, 0xE9, 0x4E2D] {
        let _ = f.glyph_index(r);
    }
    let mut buf = Buffer::new(&lim);
    for g in 0..f.num_glyphs().min(512) {
        let gi = GlyphIndex(g as u16);
        let _ = f.glyph_advance(gi, upem);
        let _ = f.load_glyph(&mut buf, gi, upem);
        let _ = f.load_glyph(&mut buf, gi, Int26_6(12 << 6));
    }
});
