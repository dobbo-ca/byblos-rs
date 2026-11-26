//! Tests of the sfnt port. The Go tests of x/image `sfnt_test.go` that exercise the ported API
//! (`TestTrueTypeParse`, `TestGlyphAdvance`, `TestGoRegularGlyphIndex`, `TestGlyphIndex`,
//! `TestPostScriptSegments`, `TestTrueTypeSegments`, `TestPPEM`), plus the byblos fonts.
//!
//! Left out because the API is not ported: `TestTrueTypeParseReaderAt`, `TestBounds`,
//! `TestMetrics`, `TestGlyphBounds`, `TestPostInfo`, `TestGlyphName`, `TestBuiltInPostNames`,
//! the kern and GPOS tests, and the proprietary-font tests (the fonts are not available). The
//! `WriteSourceTo` and `Name` checks inside the ported tests are dropped.

use super::fixed::{Int26_6, Point26_6};
use super::*;

const FONTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sfnt/");
const LIBERATION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/render/fonts/");

fn read(dir: &str, name: &str) -> Vec<u8> {
    std::fs::read(format!("{dir}{name}")).unwrap_or_else(|e| panic!("{dir}{name}: {e}"))
}

fn parse(dir: &str, name: &str) -> Font {
    Font::parse(&read(dir, name), &Limits::default()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn pt(x: i32, y: i32) -> Point26_6 {
    Point26_6 {
        x: Int26_6(x),
        y: Int26_6(y),
    }
}

fn seg(op: SegmentOp, args: &[Point26_6]) -> Segment {
    let mut s = Segment {
        op,
        args: [Point26_6::default(); 3],
    };
    for (d, a) in s.args.iter_mut().zip(args) {
        *d = *a;
    }
    s
}

fn move_to(x: i32, y: i32) -> Segment {
    seg(SegmentOp::MoveTo, &[pt(x, y)])
}

fn line_to(x: i32, y: i32) -> Segment {
    seg(SegmentOp::LineTo, &[pt(x, y)])
}

fn quad_to(xa: i32, ya: i32, xb: i32, yb: i32) -> Segment {
    seg(SegmentOp::QuadTo, &[pt(xa, ya), pt(xb, yb)])
}

fn cube_to(xa: i32, ya: i32, xb: i32, yb: i32, xc: i32, yc: i32) -> Segment {
    seg(SegmentOp::CubeTo, &[pt(xa, ya), pt(xb, yb), pt(xc, yc)])
}

fn translate(dx: i32, dy: i32, mut s: Segment) -> Segment {
    truetype::translate_args(&mut s.args, Int26_6(dx), Int26_6(dy));
    s
}

fn transform(t: [i16; 4], dx: i32, dy: i32, mut s: Segment) -> Segment {
    truetype::transform_args(&mut s.args, t, Int26_6(dx), Int26_6(dy));
    s
}

/// `checkSegmentsEqual`: `got` has its Y axis flipped to compare with the Y-up `want`, and every
/// contour must be closed.
fn check_segments_equal(got: &[Segment], want: &[Segment]) {
    let got: Vec<Segment> = got
        .iter()
        .map(|s| {
            let mut s = *s;
            for a in &mut s.args {
                a.y = Int26_6(a.y.0.wrapping_neg());
            }
            s
        })
        .collect();
    assert_eq!(got, want);

    if got.is_empty() {
        return;
    }
    assert_eq!(
        got[0].op,
        SegmentOp::MoveTo,
        "segments do not start with a moveTo"
    );
    let (mut first, mut last) = (Point26_6::default(), Point26_6::default());
    for (i, g) in got.iter().enumerate() {
        match g.op {
            SegmentOp::MoveTo => {
                if i != 0 {
                    assert_eq!(first, last, "contour before segment {i} not closed");
                }
                first = g.args[0];
                last = g.args[0];
            }
            SegmentOp::LineTo => last = g.args[0],
            SegmentOp::QuadTo => last = g.args[1],
            SegmentOp::CubeTo => last = g.args[2],
        }
    }
    assert_eq!(first, last, "last contour not closed");
}

#[test]
fn true_type_parse() {
    let f = parse(FONTS, "Go-Regular.ttf");
    assert_eq!(f.units_per_em(), 2048);
    // The WGL4 character set has more than 650 characters.
    assert!(f.num_glyphs() > 650, "NumGlyphs: {}", f.num_glyphs());
}

#[test]
fn glyph_advance() {
    let cases: [(&str, [(char, i32); 6]); 3] = [
        (
            "Go-Bold.ttf",
            [
                (' ', 569),
                ('A', 1479),
                ('Á', 1479),
                ('Æ', 2048),
                ('i', 592),
                ('x', 1139),
            ],
        ),
        (
            "Go-Mono.ttf",
            [
                (' ', 1229),
                ('A', 1229),
                ('Á', 1229),
                ('Æ', 1229),
                ('i', 1229),
                ('x', 1229),
            ],
        ),
        (
            "Go-Regular.ttf",
            [
                (' ', 569),
                ('A', 1366),
                ('Á', 1366),
                ('Æ', 2048),
                ('i', 505),
                ('x', 1024),
            ],
        ),
    ];
    for (name, wants) in cases {
        let f = parse(FONTS, name);
        let ppem = Int26_6(f.units_per_em());
        for (r, want) in wants {
            let x = f.glyph_index(r as i32).unwrap();
            let got = f.glyph_advance(x, ppem).unwrap();
            assert_eq!(got, Int26_6(want), "name={name} r={r:?}");
        }
    }
}

#[test]
fn go_regular_glyph_index() {
    let f = parse(FONTS, "Go-Regular.ttf");
    let cases: [(u32, u16); 19] = [
        // Not present in Go Regular.
        (0x001f, 0),
        (0x0200, 0),
        (0x2000, 0),
        (0x0020, 3),
        (0x0021, 4),
        (0x0022, 5),
        (0x0023, 6),
        (0x0024, 7),
        (0x0025, 8),
        (0x0026, 9),
        (0x0027, 10),
        (0x03bd, 413),
        (0x03be, 414),
        (0x03bf, 415),
        (0x03c0, 416),
        (0x03c1, 417),
        (0x03c2, 418),
        // Out of the BMP, and a surrogate.
        (0x1f4a9, 0),
        (0xd800, 0),
    ];
    for (r, want) in cases {
        assert_eq!(
            f.glyph_index(r as i32).unwrap(),
            GlyphIndex(want),
            "r={r:#x}"
        );
    }
}

#[test]
fn glyph_index_cmap_formats() {
    let data = read(FONTS, "cmapTest.ttf");
    // -1: every format; the others: only that cmap format, as Go swaps supportedCmapFormat.
    for format in [None, Some(0u16), Some(4), Some(12)] {
        let mut budget = Budget(Limits::default().alloc_limit());
        let f = Font::initialize(data.clone(), &Limits::default(), &mut budget, format)
            .unwrap_or_else(|e| panic!("format {format:?}: {e}"));

        let cases: [(u32, u16); 30] = [
            // Not present in cmapTest.ttf.
            (0x3f, 0),
            (0xfffd, 0),
            (0x1f4a9, 0),
            (0x2f, 0),
            (0x30, 3),
            (0x31, 4),
            (0x32, 5),
            (0x33, 0),
            (0x40, 0),
            (0x41, 6),
            (0x42, 7),
            (0x43, 0),
            (0x60, 0),
            (0x61, 8),
            (0x62, 0),
            // Of the remaining runes, only U+00FF is in both Mac Roman and the font.
            (0xfe, 0),
            (0xff, 9),
            (0x100, 10),
            (0x101, 11),
            (0x102, 0),
            (0x4e2c, 0),
            (0x4e2d, 12),
            (0x4e2e, 0),
            (0x1f0a0, 0),
            (0x1f0a1, 13),
            (0x1f0a2, 0),
            (0x1f0b0, 0),
            (0x1f0b1, 14),
            (0x1f0b2, 15),
            (0x1f0b3, 0),
        ];
        for (r, mut want) in cases {
            if format == Some(0) && r > 0x7f && r != 0xff {
                // Format 0 with Macintosh Roman can only represent a few non-ASCII runes.
                want = 0;
            }
            if format == Some(4) && r > 0xffff {
                // Format 4 only supports the BMP.
                want = 0;
            }
            assert_eq!(
                f.glyph_index(r as i32).unwrap(),
                GlyphIndex(want),
                "format {format:?} r={r:#x}"
            );
        }
    }
}

fn test_segments(name: &str, wants: &[Vec<Segment>]) {
    let f = parse(FONTS, name);
    let ppem = Int26_6(f.units_per_em());
    assert_eq!(f.num_glyphs(), wants.len());
    let mut b = Buffer::default();
    for (i, want) in wants.iter().enumerate() {
        let got = f.load_glyph(&mut b, GlyphIndex(i as u16), ppem).unwrap();
        check_segments_equal(got, want);
    }
    assert_eq!(
        f.load_glyph(&mut b, GlyphIndex(0xffff), ppem).unwrap_err(),
        Error::NotFound
    );
}

#[test]
fn post_script_segments() {
    let wants = vec![
        // .notdef
        vec![
            move_to(50, 0),
            line_to(450, 0),
            line_to(450, 533),
            line_to(50, 533),
            line_to(50, 0),
            move_to(100, 50),
            line_to(100, 483),
            line_to(400, 483),
            line_to(400, 50),
            line_to(100, 50),
        ],
        // zero
        vec![
            move_to(300, 700),
            cube_to(380, 700, 420, 580, 420, 500),
            cube_to(420, 350, 390, 100, 300, 100),
            cube_to(220, 100, 180, 220, 180, 300),
            cube_to(180, 450, 210, 700, 300, 700),
            move_to(300, 800),
            cube_to(200, 800, 100, 580, 100, 400),
            cube_to(100, 220, 200, 0, 300, 0),
            cube_to(400, 0, 500, 220, 500, 400),
            cube_to(500, 580, 400, 800, 300, 800),
        ],
        // one
        vec![
            move_to(100, 0),
            line_to(300, 0),
            line_to(300, 800),
            line_to(100, 800),
            line_to(100, 0),
        ],
        // Q
        vec![
            move_to(657, 237),
            line_to(289, 387),
            line_to(519, 615),
            line_to(657, 237),
            move_to(792, 169),
            cube_to(867, 263, 926, 502, 791, 665),
            cube_to(645, 840, 380, 831, 228, 673),
            cube_to(71, 509, 110, 231, 242, 93),
            cube_to(369, -39, 641, 18, 722, 93),
            line_to(802, 3),
            line_to(864, 83),
            line_to(792, 169),
        ],
        // uni4E2D
        vec![
            move_to(141, 520),
            line_to(137, 356),
            line_to(245, 400),
            line_to(331, 26),
            line_to(355, 414),
            line_to(463, 434),
            line_to(453, 620),
            line_to(341, 592),
            line_to(331, 758),
            line_to(243, 752),
            line_to(235, 562),
            line_to(141, 520),
        ],
    ];
    test_segments("CFFTest.otf", &wants);
}

/// The one-contour glyph `one` of glyfTest.ttf, passed through `f`.
fn one_contour(f: impl Fn(Segment) -> Segment) -> Vec<Segment> {
    vec![
        f(move_to(205, 0)),
        f(line_to(205, 1638)),
        f(line_to(614, 1638)),
        f(line_to(614, 0)),
        f(line_to(205, 0)),
    ]
}

/// The contour `five`, shared by the compound glyphs.
fn base_contour() -> Vec<Segment> {
    vec![
        move_to(0, 0),
        line_to(0, 100),
        line_to(400, 100),
        line_to(400, 0),
        line_to(0, 0),
    ]
}

fn compound(rest: Vec<Segment>) -> Vec<Segment> {
    let mut v = base_contour();
    v.extend(rest);
    v
}

#[test]
fn true_type_segments() {
    let wants = vec![
        // .notdef
        vec![
            move_to(68, 0),
            line_to(68, 1365),
            line_to(612, 1365),
            line_to(612, 0),
            line_to(68, 0),
            move_to(136, 68),
            line_to(544, 68),
            line_to(544, 1297),
            line_to(136, 1297),
            line_to(136, 68),
        ],
        // .null and nonmarkingreturn: empty glyphs.
        vec![],
        vec![],
        // zero
        vec![
            move_to(614, 1434),
            quad_to(369, 1434, 369, 614),
            quad_to(369, 471, 435, 338),
            quad_to(502, 205, 614, 205),
            quad_to(860, 205, 860, 1024),
            quad_to(860, 1167, 793, 1300),
            quad_to(727, 1434, 614, 1434),
            move_to(614, 1638),
            quad_to(1024, 1638, 1024, 819),
            quad_to(1024, 0, 614, 0),
            quad_to(205, 0, 205, 819),
            quad_to(205, 1638, 614, 1638),
        ],
        // one
        one_contour(|s| s),
        // five
        base_contour(),
        // six
        compound(one_contour(|s| translate(111, 234, s))),
        // seven
        compound(one_contour(|s| {
            transform([1 << 13, 0, 0, 1 << 13], 56, 117, s)
        })),
        // eight
        compound(one_contour(|s| {
            transform([3 << 13, 0, 0, 1 << 13], 56, 117, s)
        })),
        // nine
        compound(one_contour(|s| {
            transform([22381, 8192, 5996, 14188], 237, 258, s)
        })),
    ];
    test_segments("glyfTest.ttf", &wants);
}

#[test]
fn ppem() {
    let f = parse(FONTS, "glyfTest.ttf");
    let mut b = Buffer::default();
    let x = f.glyph_index('1' as i32).unwrap();
    assert_ne!(x, GlyphIndex(0));

    let got = f.load_glyph(&mut b, x, Int26_6(12 << 6)).unwrap();
    check_segments_equal(
        got,
        &[
            move_to(77, 0),
            line_to(77, 614),
            line_to(230, 614),
            line_to(230, 0),
            line_to(77, 0),
        ],
    );
    let got = f.load_glyph(&mut b, x, Int26_6(2048)).unwrap();
    check_segments_equal(got, &one_contour(|s| s));
}

#[test]
fn liberation_fonts_parse_and_load_every_glyph() {
    let mut b = Buffer::default();
    for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
        for family in ["Mono", "Sans", "Serif"] {
            let name = format!("Liberation{family}-{style}.ttf");
            let f = parse(LIBERATION, &name);
            assert_eq!(f.units_per_em(), 2048, "{name}");
            let ppem = Int26_6(f.units_per_em());
            assert!(f.num_glyphs() > 300, "{name}: {}", f.num_glyphs());
            for g in 0..f.num_glyphs() {
                let x = GlyphIndex(g as u16);
                f.load_glyph(&mut b, x, ppem)
                    .unwrap_or_else(|e| panic!("{name} glyph {g}: {e}"));
                f.glyph_advance(x, ppem)
                    .unwrap_or_else(|e| panic!("{name} advance {g}: {e}"));
            }
            let a = f.glyph_index('A' as i32).unwrap();
            assert_ne!(a, GlyphIndex(0), "{name}");
            assert_eq!(f.glyph_index(0x1f4a9).unwrap(), GlyphIndex(0), "{name}");
            if family == "Mono" {
                // Liberation Mono is metric-compatible with Courier New.
                assert_eq!(f.glyph_advance(a, ppem).unwrap(), Int26_6(1229), "{name}");
            }
        }
    }
    // Liberation Sans is metric-compatible with Arial.
    let f = parse(LIBERATION, "LiberationSans-Regular.ttf");
    let ppem = Int26_6(f.units_per_em());
    let adv = |c: char| {
        let x = f.glyph_index(c as i32).unwrap();
        f.glyph_advance(x, ppem).unwrap()
    };
    assert_eq!(adv('A'), Int26_6(1366));
    assert_eq!(adv(' '), Int26_6(569));
}

#[test]
fn glyphless_font_parses() {
    let f = parse(FONTS, "glyphless.ttf");
    assert!(f.units_per_em() > 0);
    let mut b = Buffer::default();
    let ppem = Int26_6(f.units_per_em());
    for g in 0..f.num_glyphs() {
        f.load_glyph(&mut b, GlyphIndex(g as u16), ppem).unwrap();
        f.glyph_advance(GlyphIndex(g as u16), ppem).unwrap();
    }
}

fn parse_err(src: &[u8], l: &Limits) -> Error {
    match Font::parse(src, l) {
        Err(e) => e,
        Ok(_) => panic!("parse succeeded"),
    }
}

#[test]
fn parse_errors() {
    let l = Limits::default();
    assert_eq!(parse_err(&[], &l), INVALID_BOUNDS);
    assert_eq!(parse_err(&[0; 12], &l), INVALID_FONT);
    let mut ttcf = vec![0u8; 12];
    ttcf[..4].copy_from_slice(b"ttcf");
    assert_eq!(parse_err(&ttcf, &l), INVALID_SINGLE_FONT);
    // A valid header with no tables: the head table is missing.
    let mut no_tables = vec![0u8; 12];
    no_tables[..4].copy_from_slice(&[0, 1, 0, 0]);
    assert_eq!(parse_err(&no_tables, &l), INVALID_HEAD_TABLE);
    // Too many tables.
    let mut many = no_tables.clone();
    many[4] = 0x02;
    assert_eq!(parse_err(&many, &l), UNSUPPORTED_NUMBER_OF_TABLES);
}

#[test]
fn allocation_budget() {
    let data = read(FONTS, "Go-Regular.ttf");
    let tiny = Limits {
        max_alloc: 1024,
        ..Limits::default()
    };
    let err = parse_err(&data, &tiny);
    assert!(
        matches!(err, Error::Resource(crate::Error::Alloc { .. })),
        "{err}"
    );
    assert_eq!(crate::Error::from(err).class(), "limit/alloc");
}

#[test]
fn hostile_input_never_panics() {
    let l = Limits::default();
    let data = read(LIBERATION, "LiberationSans-Regular.ttf");
    // Truncations.
    for n in (0..data.len()).step_by(997) {
        let _ = Font::parse(&data[..n], &l);
    }
    // Single byte corruptions, over the table directory and spread over the tables.
    let mut b = Buffer::default();
    for i in (0..data.len()).step_by(211).chain(0..512) {
        for v in [0x00, 0xff, 0x80] {
            let mut d = data.clone();
            d[i] = v;
            if let Ok(f) = Font::parse(&d, &l) {
                let ppem = Int26_6(f.units_per_em());
                for g in (0..f.num_glyphs()).step_by(37) {
                    let _ = f.load_glyph(&mut b, GlyphIndex(g as u16), ppem);
                    let _ = f.glyph_advance(GlyphIndex(g as u16), ppem);
                }
                let _ = f.glyph_index('A' as i32);
            }
        }
    }
    // The same for the CFF and CID-free OpenType font.
    let cff = read(FONTS, "CFFTest.otf");
    for i in 0..cff.len() {
        for v in [0x00, 0xff, 0x0c, 0x1d, 0x1e] {
            let mut d = cff.clone();
            d[i] = v;
            if let Ok(f) = Font::parse(&d, &l) {
                let ppem = Int26_6(f.units_per_em());
                for g in 0..f.num_glyphs() {
                    let _ = f.load_glyph(&mut b, GlyphIndex(g as u16), ppem);
                }
            }
        }
    }
}

#[test]
fn mac_roman_encoding() {
    assert_eq!(cmap::mac_roman_encode(0x41), Some(0x41));
    assert_eq!(cmap::mac_roman_encode(0xff), Some(0xd8));
    assert_eq!(cmap::mac_roman_encode(0x20ac), Some(0xdb));
    assert_eq!(cmap::mac_roman_encode(0x3f0), None);
}

#[test]
fn real_numbers_in_dicts() {
    // A Top DICT: 12.5 (a real), the version operator, then 0x0102 (a shortint) and the
    // CharStrings operator.
    let dict = [0x1e, 0x12, 0xa5, 0xff, 0x00, 0x1c, 0x01, 0x02, 0x11];
    assert_eq!(postscript::run_top_dict(&dict), Ok(0x0102));
    // An unterminated real number.
    assert_eq!(
        postscript::run_top_dict(&[0x1e, 0x12]),
        Err(INVALID_CFF_TABLE)
    );
    // An overflowing one: 1E50 does not fit a float32.
    assert_eq!(
        postscript::run_top_dict(&[0x1e, 0x1b, 0x50, 0xff]),
        Err(INVALID_CFF_TABLE)
    );
    // An unknown operator.
    assert_eq!(
        postscript::run_top_dict(&[0x06]),
        Err(Error::UnrecognizedOperator {
            escaped: false,
            byte: 6
        })
    );
}
