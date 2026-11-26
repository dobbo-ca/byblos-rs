//! Ports of `internal/render/text_test.go`.
//!
//! The two `runtime.MemStats` assertions (the compound bomb's allocation ceiling and the 2000-name
//! live-heap ceiling) have no Rust counterpart; the bomb test keeps its page assertions and the
//! retained-bytes test measures the widths the renderer retains instead.

use super::*;
use crate::content::BBox;
use crate::error::Error;
use crate::image::{Rect, Rgba};
use crate::render::page::{PageParams, page};
use std::cell::Cell;
use std::sync::atomic::AtomicBool;

// ---- minimal TrueType builder ----------------------------------------------------------------
//
// A hand-assembled sfnt with real glyf outlines: glyphless.ttf's glyphs are empty, which is
// exactly what a glyph-rendering test cannot use.

/// One glyf contour point in font units; `off` marks an off-curve (quadratic control) point.
#[derive(Clone, Copy)]
struct Gpt {
    x: i16,
    y: i16,
    off: bool,
}

fn gp(x: i16, y: i16) -> Gpt {
    Gpt { x, y, off: false }
}

fn tbe16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn tbe32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

/// Encodes one simple glyph: contours of absolute points, flags uncompressed, every coordinate a
/// full int16 delta.
fn build_glyf(contours: &[Vec<Gpt>]) -> Vec<u8> {
    let mut b = Vec::new();
    tbe16(&mut b, contours.len() as u16);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
    let mut pts = Vec::new();
    let mut ends = Vec::new();
    for c in contours {
        pts.extend_from_slice(c);
        ends.push((pts.len() - 1) as u16);
        for p in c {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
    }
    for v in [min_x, min_y, max_x, max_y] {
        tbe16(&mut b, v as u16);
    }
    for e in ends {
        tbe16(&mut b, e);
    }
    tbe16(&mut b, 0); // instructionLength
    for p in &pts {
        b.push(if p.off { 0x00 } else { 0x01 });
    }
    let mut px = 0i16;
    for p in &pts {
        tbe16(&mut b, p.x.wrapping_sub(px) as u16);
        px = p.x;
    }
    let mut py = 0i16;
    for p in &pts {
        tbe16(&mut b, p.y.wrapping_sub(py) as u16);
        py = p.y;
    }
    while !b.len().is_multiple_of(2) {
        // Short loca stores offset/2: glyph data must stay even.
        b.push(0);
    }
    b
}

/// A compound glyph of `n` copies of glyph `child`, each offset (0,0): flags are
/// ARG_1_AND_2_ARE_WORDS|ARGS_ARE_XY_VALUES plus MORE_COMPONENTS on all but the last record.
fn build_compound(child: u16, n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    tbe16(&mut b, 0xFFFF); // numberOfContours = -1
    for _ in 0..4 {
        tbe16(&mut b, 0); // bbox, unchecked by the loaders exercised here
    }
    for i in 0..n {
        let mut flags: u16 = 0x0001 | 0x0002;
        if i + 1 < n {
            flags |= 0x0020; // MORE_COMPONENTS
        }
        tbe16(&mut b, flags);
        tbe16(&mut b, child);
        tbe16(&mut b, 0); // dx
        tbe16(&mut b, 0); // dy
    }
    b
}

/// Assembles a TrueType font: `glyphs[i]` (raw glyf bytes) becomes glyph i+1, mapped from rune
/// `first_rune + i` (one contiguous cmap format-4 segment), with hmtx advance `advances[i]`.
/// Glyph 0 (.notdef) is empty.
fn build_ttf(upem: u16, first_rune: i32, glyphs: &[Vec<u8>], advances: &[u16]) -> Vec<u8> {
    let num_glyphs = glyphs.len() + 1;

    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    tbe16(&mut loca, 0); // .notdef: empty, offset 0
    tbe16(&mut loca, 0);
    for g in glyphs {
        glyf.extend_from_slice(g);
        tbe16(&mut loca, (glyf.len() / 2) as u16);
    }

    let mut head = Vec::new();
    tbe32(&mut head, 0x0001_0000);
    tbe32(&mut head, 0x0001_0000);
    tbe32(&mut head, 0); // checkSumAdjustment
    tbe32(&mut head, 0x5F0F_3CF5);
    tbe16(&mut head, 0x0003);
    tbe16(&mut head, upem);
    head.extend_from_slice(&[0; 16]); // created/modified
    tbe16(&mut head, 0); // xMin
    tbe16(&mut head, (-200i16) as u16); // yMin
    tbe16(&mut head, upem); // xMax
    tbe16(&mut head, 800); // yMax
    tbe16(&mut head, 0); // macStyle
    tbe16(&mut head, 8); // lowestRecPPEM
    tbe16(&mut head, 2); // fontDirectionHint
    tbe16(&mut head, 0); // indexToLocFormat: short
    tbe16(&mut head, 0); // glyphDataFormat

    let mut hhea = Vec::new();
    tbe32(&mut hhea, 0x0001_0000);
    tbe16(&mut hhea, 800); // ascender
    tbe16(&mut hhea, (-200i16) as u16); // descender
    tbe16(&mut hhea, 0); // lineGap
    tbe16(&mut hhea, 1000); // advanceWidthMax
    tbe16(&mut hhea, 0); // minLeftSideBearing
    tbe16(&mut hhea, 0); // minRightSideBearing
    tbe16(&mut hhea, 0); // xMaxExtent
    tbe16(&mut hhea, 1); // caretSlopeRise
    tbe16(&mut hhea, 0); // caretSlopeRun
    tbe16(&mut hhea, 0); // caretOffset
    for _ in 0..4 {
        tbe16(&mut hhea, 0); // reserved
    }
    tbe16(&mut hhea, 0); // metricDataFormat
    tbe16(&mut hhea, num_glyphs as u16);

    let mut maxp = Vec::new();
    tbe32(&mut maxp, 0x0001_0000);
    tbe16(&mut maxp, num_glyphs as u16);
    tbe16(&mut maxp, 4096); // maxPoints
    tbe16(&mut maxp, 2048); // maxContours
    tbe16(&mut maxp, 0);
    tbe16(&mut maxp, 0);
    tbe16(&mut maxp, 2); // maxZones
    for _ in 0..8 {
        tbe16(&mut maxp, 0);
    }

    let mut hmtx = Vec::new();
    tbe16(&mut hmtx, 0); // .notdef advance
    tbe16(&mut hmtx, 0);
    for &a in advances {
        tbe16(&mut hmtx, a);
        tbe16(&mut hmtx, 0);
    }

    let last_rune = first_rune + glyphs.len() as i32 - 1;
    let mut sub = Vec::new();
    tbe16(&mut sub, 4); // format
    tbe16(&mut sub, 0); // length, patched
    tbe16(&mut sub, 0); // language
    tbe16(&mut sub, 4); // segCountX2 (2 segments)
    tbe16(&mut sub, 4); // searchRange
    tbe16(&mut sub, 1); // entrySelector
    tbe16(&mut sub, 0); // rangeShift
    tbe16(&mut sub, last_rune as u16);
    tbe16(&mut sub, 0xFFFF);
    tbe16(&mut sub, 0); // reservedPad
    tbe16(&mut sub, first_rune as u16);
    tbe16(&mut sub, 0xFFFF);
    tbe16(&mut sub, (1 - first_rune) as i16 as u16); // idDelta: glyphID = rune - firstRune + 1
    tbe16(&mut sub, 1);
    tbe16(&mut sub, 0); // idRangeOffset x2
    tbe16(&mut sub, 0);
    let len = (sub.len() as u16).to_be_bytes();
    sub[2..4].copy_from_slice(&len);
    let mut cmap = Vec::new();
    tbe16(&mut cmap, 0); // version
    tbe16(&mut cmap, 1); // numTables
    tbe16(&mut cmap, 3); // platformID Windows
    tbe16(&mut cmap, 1); // encodingID Unicode BMP
    tbe32(&mut cmap, 12);
    cmap.extend_from_slice(&sub);

    let mut post = Vec::new();
    tbe32(&mut post, 0x0003_0000);
    post.extend_from_slice(&[0; 28]);

    let tables: [(&str, &Vec<u8>); 8] = [
        ("cmap", &cmap),
        ("glyf", &glyf),
        ("head", &head),
        ("hhea", &hhea),
        ("hmtx", &hmtx),
        ("loca", &loca),
        ("maxp", &maxp),
        ("post", &post),
    ];
    let num_tables = tables.len() as u16;
    let mut entry_selector = 0u16;
    while (1u32 << (entry_selector + 1)) <= u32::from(num_tables) {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector) * 16;

    let mut out = Vec::new();
    tbe32(&mut out, 0x0001_0000);
    tbe16(&mut out, num_tables);
    tbe16(&mut out, search_range);
    tbe16(&mut out, entry_selector);
    tbe16(&mut out, num_tables * 16 - search_range);
    let dir_start = out.len();
    out.resize(out.len() + usize::from(num_tables) * 16, 0);
    for (i, (tag, data)) in tables.iter().enumerate() {
        let off = out.len() as u32;
        let entry = dir_start + i * 16;
        out[entry..entry + 4].copy_from_slice(tag.as_bytes());
        out[entry + 8..entry + 12].copy_from_slice(&off.to_be_bytes());
        out[entry + 12..entry + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    out
}

/// A 500x500 square at the glyph origin: axis-aligned, so its device footprint under any
/// translation and scale is hand-computable to the pixel.
fn square_glyph() -> Vec<Vec<Gpt>> {
    vec![vec![gp(0, 0), gp(500, 0), gp(500, 500), gp(0, 500)]]
}

/// 'A' is the square glyph, advance 600 (font units and the PDF /Widths value in thousandths:
/// unitsPerEm is 1000, so the two scales agree).
fn test_font() -> Font {
    Font {
        program: build_ttf(1000, 'A' as i32, &[build_glyf(&square_glyph())], &[600]),
        first_char: 'A' as i64,
        widths: vec![600.0],
        ..Font::default()
    }
}

fn fonts_for(f: Font) -> impl FnMut(&[u8]) -> Option<Font> {
    move |name: &[u8]| (name == b"F1").then(|| f.clone())
}

const BOX100: BBox = BBox {
    llx: 0.0,
    lly: 0.0,
    urx: 100.0,
    ury: 100.0,
};

/// `Page` at scale 1 over `bbox` with only a font resolver.
fn render_text<'a>(
    src: &str,
    bbox: BBox,
    fonts: Option<&'a mut FontFor<'a>>,
    lim: &'a Limits,
) -> Result<Rgba> {
    let cancel = AtomicBool::new(false);
    let p = PageParams {
        bbox,
        rotate: 0,
        scale: 1.0,
    };
    page(src.as_bytes(), &p, None, fonts, lim, &cancel)
}

/// A device-pixel rectangle, half-open: `(x0, y0, x1, y1)`.
type PxRect = (i64, i64, i64, i64);

/// `assertExactPixels`: black exactly inside the union of `rects` and white everywhere else.
fn assert_exact_pixels(img: &Rgba, w: i64, h: i64, rects: &[PxRect]) {
    let mut bad = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let want = rects
                .iter()
                .any(|r| (r.0..r.2).contains(&x) && (r.1..r.3).contains(&y));
            let p = img.pixel(x, y).unwrap();
            let got = p[0] == 0 && p[1] == 0 && p[2] == 0;
            if got != want {
                bad.push((x, y, got, want));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} pixels differ from the hand-computed glyph footprints, first: {:?}",
        bad.len(),
        bad.first()
    );
}

fn limit_name(r: &Result<Rgba>) -> Option<&'static str> {
    match r {
        Err(Error::Limit { name, .. }) => Some(*name),
        _ => None,
    }
}

// The device footprint of every glyph in a Tm/Td/TJ sequence matches the hand-computed origins
// exactly. 100x100 page at scale 1 (device y = 100 - user y), /F1 at 20pt so the square is 10x10pt
// with advance 12pt:
//   glyph 1: Tm origin (10,50)                    -> x 10..20, user y 50..60
//   glyph 2: +advance 12                          -> x 22..32
//   glyph 3: TJ -500 = +500/1000*20 = +10 further -> x 44..54
//   glyph 4: 0 -30 Td from line start (10,50)     -> (10,20), user y 20..30
#[test]
fn text_glyph_origins_exact() {
    let src = "BT /F1 20 Tf 1 0 0 1 10 50 Tm [(AA) -500 (A)] TJ 0 -30 Td (A) Tj ET";
    let mut fonts = fonts_for(test_font());
    let img = render_text(src, BOX100, Some(&mut fonts), &Limits::default()).unwrap();
    assert_exact_pixels(
        &img,
        100,
        100,
        &[
            (10, 40, 20, 50),
            (22, 40, 32, 50),
            (44, 40, 54, 50),
            (10, 70, 20, 80),
        ],
    );
}

// Tc, Tw, Tz and Ts, each hand-computed:
//   line 1 (Tm origin (10,80), Tc 2, Tw 4): "A A"
//     glyph 1 at x 10; advance 12+2=14; space (code 32, outside /Widths, MissingWidth 0) advances
//     0+2+4=6 -> glyph 2 at x 30
//   line 2 (Td to (10,40), 50 Tz, 5 Ts): "AA", squares 5pt wide, raised 5
//     glyph 3 at x 10, user y 45..55; advance (12+2)*0.5=7 -> glyph 4 at x 17
#[test]
fn text_spacing_params_exact() {
    let src = "BT /F1 20 Tf 4 Tw 2 Tc 1 0 0 1 10 80 Tm (A A) Tj 50 Tz 5 Ts 0 -40 Td (AA) Tj ET";
    let mut fonts = fonts_for(test_font());
    let img = render_text(src, BOX100, Some(&mut fonts), &Limits::default()).unwrap();
    assert_exact_pixels(
        &img,
        100,
        100,
        &[
            (10, 10, 20, 20),
            (30, 10, 40, 20),
            (10, 45, 15, 55),
            (17, 45, 22, 55),
        ],
    );
}

// Fill modes ink, the deferred stroke-only and clip-only modes and invisible mode do not, and
// every mode still advances (mode 3's second glyph proves the advance by switching back to fill
// mid-string).
#[test]
fn text_render_modes() {
    let cases: [(&str, &str, Vec<PxRect>); 6] = [
        (
            "fill",
            "BT /F1 20 Tf 1 0 0 1 10 50 Tm (A) Tj ET",
            vec![(10, 40, 20, 50)],
        ),
        (
            "fill-stroke",
            "BT /F1 20 Tf 2 Tr 1 0 0 1 10 50 Tm (A) Tj ET",
            vec![(10, 40, 20, 50)],
        ),
        (
            "invisible",
            "BT /F1 20 Tf 3 Tr 1 0 0 1 10 50 Tm (A) Tj ET",
            vec![],
        ),
        (
            "stroke-only-deferred",
            "BT /F1 20 Tf 1 Tr 1 0 0 1 10 50 Tm (A) Tj ET",
            vec![],
        ),
        (
            "clip-only-deferred",
            "BT /F1 20 Tf 7 Tr 1 0 0 1 10 50 Tm (A) Tj ET",
            vec![],
        ),
        (
            "invisible-still-advances",
            "BT /F1 20 Tf 1 0 0 1 10 50 Tm 3 Tr (A) Tj 0 Tr (A) Tj ET",
            vec![(22, 40, 32, 50)],
        ),
    ];
    for (name, src, rects) in cases {
        let mut fonts = fonts_for(test_font());
        let img = render_text(src, BOX100, Some(&mut fonts), &Limits::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_exact_pixels(&img, 100, 100, &rects);
    }
}

// A font FontFor cannot supply, an unparsable font program, and a nil FontFor must all skip the
// glyphs without erroring the page: the path fill after the text must still land.
#[test]
fn text_unresolved_font_skips_cleanly() {
    let src = "BT /F9 20 Tf 1 0 0 1 10 50 Tm (A) Tj ET 30 30 10 10 re f";
    let lim = Limits::default();
    let want = [(30, 60, 40, 70)];

    let mut unresolved = fonts_for(test_font());
    let img = render_text(src, BOX100, Some(&mut unresolved), &lim).unwrap();
    assert_exact_pixels(&img, 100, 100, &want);

    let img = render_text(src, BOX100, None, &lim).unwrap();
    assert_exact_pixels(&img, 100, 100, &want);

    let mut garbage = |_: &[u8]| {
        Some(Font {
            program: b"not an sfnt".to_vec(),
            widths: vec![600.0],
            first_char: 'A' as i64,
            ..Font::default()
        })
    };
    let img = render_text(src, BOX100, Some(&mut garbage), &lim).unwrap();
    assert_exact_pixels(&img, 100, 100, &want);
}

// The PDF /Widths array wins over the font's own hmtx advance (ISO 32000-1 9.2.4). The font says
// 600; /Widths says 900, so glyph 2 lands at 10+18=28, not 10+12=22.
#[test]
fn text_widths_override_font_advance() {
    let mut f = test_font();
    f.widths = vec![900.0];
    let mut fonts = fonts_for(f);
    let img = render_text(
        "BT /F1 20 Tf 1 0 0 1 10 50 Tm (AA) Tj ET",
        BOX100,
        Some(&mut fonts),
        &Limits::default(),
    )
    .unwrap();
    assert_exact_pixels(&img, 100, 100, &[(10, 40, 20, 50), (28, 40, 38, 50)]);
}

// A glyph whose flattened outline exceeds the point budget must trip it as an error, not hang.
// The glyph stays under the budget on disk (800 points, so the expansion gate admits it) and blows
// past it through curve flattening: 400 tall skinny quadratics that, drawn at font size 2000,
// each subdivide several times, past 1000 device points total.
#[test]
fn hostile_glyph_trips_point_budget() {
    let mut c = Vec::new();
    for i in 0..400i16 {
        c.push(gp(2 * i, 0));
        c.push(Gpt {
            x: 2 * i + 1,
            y: 800,
            off: true,
        });
    }
    let f = Font {
        program: build_ttf(1000, 'A' as i32, &[build_glyf(&[c])], &[600]),
        first_char: 'A' as i64,
        widths: vec![600.0],
        ..Font::default()
    };
    let lim = Limits {
        max_path_points: 1000,
        ..Limits::default()
    };
    let big = BBox {
        urx: 5000.0,
        ury: 5000.0,
        ..BBox::default()
    };
    let mut fonts = fonts_for(f);
    let r = render_text(
        "BT /F1 2000 Tf 1 0 0 1 10 50 Tm (A) Tj ET",
        big,
        Some(&mut fonts),
        &lim,
    );
    assert_eq!(limit_name(&r), Some("path-points"), "{:?}", r.err());
}

// A compound-glyph bomb (7 levels of 9 components each, ~9^7 expanded instances from ~1 KB of
// font) must make the font degrade to widths-only without the expansion ever materialising. The
// bomb glyph is deliberately mapped to 'H': sfnt::Font::parse itself loads 'x' and 'H' for its
// OS/2 metrics fallback, so a post-parse check is already too late.
#[test]
fn hostile_compound_glyph_refused_before_parse() {
    let mut glyphs = vec![build_glyf(&square_glyph())]; // glyph 1 ('A'): 4-point leaf
    let mut advances = vec![600u16];
    for i in 0..7u16 {
        // glyphs 2..8 ('B'..'H'): 9 copies of the previous
        glyphs.push(build_compound(i + 1, 9));
        advances.push(600);
    }
    let f = Font {
        program: build_ttf(1000, 'A' as i32, &glyphs, &advances),
        first_char: 'A' as i64,
        widths: vec![600.0],
        ..Font::default()
    };
    let mut fonts = fonts_for(f);
    let img = render_text(
        "BT /F1 20 Tf 1 0 0 1 10 50 Tm (H) Tj ET 30 30 10 10 re f",
        BOX100,
        Some(&mut fonts),
        &Limits::default(),
    )
    .unwrap();
    // The bomb's glyphs skip; the rest of the page still renders.
    assert_exact_pixels(&img, 100, 100, &[(30, 60, 40, 70)]);
}

// A stream re-showing a wholly off-canvas glyph is the one case the scanline accounting never
// sees (its y-range clamps to the canvas), and the path budget is returned after every glyph, so
// the per-glyph fill charge is the only budget that accrues, and it must trip.
#[test]
fn hostile_off_canvas_glyphs_trip_fill_work() {
    let src = format!(
        "BT /F1 20 Tf 1 0 0 1 5000 5000 Tm ({}) Tj ET",
        "A".repeat(300)
    );
    let lim = Limits {
        max_fill_work: 1000,
        ..Limits::default()
    };
    let mut fonts = fonts_for(test_font());
    let r = render_text(&src, BOX100, Some(&mut fonts), &lim);
    assert_eq!(limit_name(&r), Some("fill-work"), "{:?}", r.err());
}

// At upem 2048 (Arial's) a legal glyph coordinate of 20000 font units overflows sfnt's int32
// scaling under a ppem of fixed.I(upem), where Int26_6(upem) returns raw font units exactly.
// Hand computation, 100x100 page at scale 1: square 0..20000 units = 20000/2048 em, at 8pt =
// 78.125pt from Tm origin (10,10): x 10..88.125, user y 10..88.125 -> device y 11.875..90.
#[test]
fn text_large_coords_at_high_upem() {
    let big = vec![vec![gp(0, 0), gp(20000, 0), gp(20000, 20000), gp(0, 20000)]];
    let f = Font {
        program: build_ttf(2048, 'A' as i32, &[build_glyf(&big)], &[600]),
        first_char: 'A' as i64,
        widths: vec![600.0],
        ..Font::default()
    };
    let mut fonts = fonts_for(f);
    let img = render_text(
        "BT /F1 8 Tf 1 0 0 1 10 10 Tm (A) Tj ET",
        BOX100,
        Some(&mut fonts),
        &Limits::default(),
    )
    .unwrap();
    assert_exact_pixels(&img, 100, 100, &[(10, 12, 88, 90)]);
}

// Hostile text parameters that drive a glyph's device coordinates non-finite must skip that
// glyph: no panic, no error, no stray ink, and the rest of the page still renders. 1e308 is
// finite, so it passes the operand parsers; the overflow to Inf happens where the parameters
// multiply into the text rendering matrix.
#[test]
fn text_non_finite_params_skip_glyphs_cleanly() {
    let huge = format!("1{}", "0".repeat(308));
    let cases: [(&str, String, Vec<PxRect>); 2] = [
        // Tfs 1e308 x Tz 1000 overflows the Trm to Inf: the glyph skips, the path fill after ET
        // still lands.
        (
            "inf-trm",
            format!("BT /F1 {huge} Tf 100000 Tz 1 0 0 1 10 50 Tm (A) Tj ET 30 30 10 10 re f"),
            vec![(30, 60, 40, 70)],
        ),
        // A huge TJ adjustment poisons tm for the second show only.
        (
            "huge-tj",
            format!("BT /F1 20 Tf 1 0 0 1 10 50 Tm [(A) {huge} (A)] TJ ET 30 30 10 10 re f"),
            vec![(10, 40, 20, 50), (30, 60, 40, 70)],
        ),
    ];
    for (name, src, rects) in cases {
        let mut fonts = fonts_for(test_font());
        let img = render_text(&src, BOX100, Some(&mut fonts), &Limits::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_exact_pixels(&img, 100, 100, &rects);
    }
}

// q/Q save and restore the text state as part of the graphics state (ISO 32000-1 table 52): a Tf
// inside q..Q must not leak out. Inside q..Q the size is 40 (square 20pt at x 10); after Q it is
// back to 20 (square 10pt), shown at Tm origin (50,50).
#[test]
fn text_state_saved_by_q() {
    let src = "BT /F1 20 Tf q /F1 40 Tf 1 0 0 1 10 50 Tm (A) Tj Q 1 0 0 1 50 50 Tm (A) Tj ET";
    let mut fonts = fonts_for(test_font());
    let img = render_text(src, BOX100, Some(&mut fonts), &Limits::default()).unwrap();
    assert_exact_pixels(&img, 100, 100, &[(10, 30, 30, 50), (50, 40, 60, 50)]);
}

fn small_canvas(lim: &Limits) -> Rgba {
    Rgba::new(Rect::new(0, 0, 1, 1), lim).unwrap()
}

// 2000 Tf names, all aliased to one /Widths-bearing 65536-entry dict (rebuilt on every call, as a
// caller that rebuilds Widths per dict deref would), inking nothing. Without the budget in
// resolve_font this page retains >1 GiB for zero pixels; the widths the renderer retains are
// measured directly where Go measured the heap. (Go's survives every single-guard mutation too:
// either refusal alone stops the retention.)
#[test]
fn resolve_font_bounds_page_retained_bytes() {
    const N: usize = 2000;
    let mut src = String::from("BT\n");
    for i in 0..N {
        src.push_str(&format!("/F{i} 12 Tf\n"));
    }
    src.push_str("ET\n");
    let new_font = |_: &[u8]| {
        Some(Font {
            widths: vec![500.0; 65536],
            base_font: b"Recon".to_vec(),
            ..Font::default()
        })
    };

    let big = BBox {
        urx: 612.0,
        ury: 792.0,
        ..BBox::default()
    };
    let lim = Limits::default();
    let mut f = new_font;
    render_text(&src, big, Some(&mut f), &lim).unwrap();

    let mut f = new_font;
    let mut r = Renderer::new(&lim, small_canvas(&lim), None, Some(&mut f));
    for i in 0..N {
        r.resolve_font(format!("F{i}").as_bytes());
    }
    let retained: usize = r
        .fonts_by
        .values()
        .flatten()
        .map(|tf| 8 * tf.width.len())
        .sum();
    // Generous: 4x the budget, where the unguarded page retains ~1 GiB.
    assert!(retained <= 4 * (16 << 20), "retained {retained} bytes");
}

// One Tf name whose /Widths array alone exceeds the budget crosses it on its first and only call,
// before the pre-fetch check (which only fires once the budget is already exceeded) ever sees it.
// resolve_font must still refuse to retain it.
#[test]
fn resolve_font_refuses_oversized_single_name() {
    let wlen = (16usize << 20) / 8 + 1; // one array alone costs > the budget
    let lim = Limits::default();
    let mut f = |_: &[u8]| {
        Some(Font {
            widths: vec![0.0; wlen],
            base_font: b"Recon".to_vec(),
            ..Font::default()
        })
    };
    let mut r = Renderer::new(&lim, small_canvas(&lim), None, Some(&mut f));
    assert!(
        r.resolve_font(b"F1").is_none(),
        "a {wlen}-entry Widths array alone exceeds the budget"
    );
    assert!(
        matches!(r.fonts_by.get(b"F1".as_slice()), Some(None)),
        "want a cached nil entry, not a retained TextFont"
    );
}

// Once the running total already exceeds the budget, further distinct Tf names must not even
// reach FontFor: the caller has no font cache of its own, so every extra call is a full re-decode
// paid only to be discarded.
#[test]
fn resolve_font_stops_fetching_past_budget() {
    const WLEN: usize = 65536; // 512 KiB/name; ~32 names exhaust a 16 MiB budget
    let calls = Cell::new(0usize);
    let lim = Limits::default();
    let mut f = |_: &[u8]| {
        calls.set(calls.get() + 1);
        Some(Font {
            widths: vec![0.0; WLEN],
            base_font: b"Recon".to_vec(),
            ..Font::default()
        })
    };
    let mut r = Renderer::new(&lim, small_canvas(&lim), None, Some(&mut f));
    for i in 0..2000 {
        r.resolve_font(format!("F{i}").as_bytes());
    }
    let max_calls = (16usize << 20) / (8 * WLEN) + 2; // +2: the crossing call, plus slack
    assert!(
        calls.get() <= max_calls,
        "FontFor called {} times for 2000 distinct names; want <= {max_calls}",
        calls.get()
    );
}

// Charging program bytes only on a program-cache miss prices the retained, deduped parse result,
// not the decode work the caller repeats on every call. A font dict with no /Widths and no /W
// (the standard-14 shape) used to charge exactly 0 per name once its program had been seen once.
// The charge must be per name resolved, not per distinct program parsed.
#[test]
fn resolve_font_charges_program_bytes_per_aliased_name() {
    const PROGRAM_LEN: usize = 15 << 20; // under the budget alone, deliberately
    let calls = Cell::new(0usize);
    let lim = Limits::default();
    let mut f = |_: &[u8]| {
        calls.set(calls.get() + 1);
        Some(Font {
            program: vec![0; PROGRAM_LEN],
            base_font: b"Recon".to_vec(),
            ..Font::default()
        })
    };
    let mut r = Renderer::new(&lim, small_canvas(&lim), None, Some(&mut f));
    for i in 0..2000 {
        r.resolve_font(format!("F{i}").as_bytes());
    }
    // One program's worth of budget admits ~1 name; generous slack.
    assert!(
        calls.get() <= 4,
        "FontFor called {} times for 2000 aliased names with no /Widths or /W; want <= 4",
        calls.get()
    );
}
