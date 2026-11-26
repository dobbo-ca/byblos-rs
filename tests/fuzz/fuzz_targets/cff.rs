#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::render::cff::{cff_to_sfnt, parse_cff};
use byblos::render::sfnt::fixed::Int26_6;
use byblos::render::sfnt::{Buffer, Font, GlyphIndex};
use libfuzzer_sys::fuzz_target;

// The bare CFF parser and gate, then the wrapped OpenType through the sfnt charstring
// interpreter, every glyph.
fuzz_target!(|data: &[u8]| {
    let _ = parse_cff(data);
    let Some(c) = cff_to_sfnt(data) else { return };
    let lim = common::limits();
    let Ok(f) = Font::parse(&c.otf, &lim) else {
        return;
    };
    let upem = Int26_6(f.units_per_em());
    let mut buf = Buffer::new(&lim);
    for g in 0..f.num_glyphs().min(512) {
        let _ = f.load_glyph(&mut buf, GlyphIndex(g as u16), upem);
    }
});
