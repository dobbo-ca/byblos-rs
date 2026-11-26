//! Tests ported from `internal/jbig2/*_test.go`. Left out, because they need tools or the PDF
//! layer: `compare_test.go` (ImageMagick, tiffdump), `pdfembed_test.go` (pdfimages),
//! `TestRoundTripBitIdentical` and the other jbig2dec cross-checks (jbig2dec), the `-update`
//! golden regeneration of `roundtrip_test.go`, the `runtime.MemStats` allocation bounds of the
//! budget tests, `budget_600dpi_test.go` and `encode_decode_boundary_test.go` (tens of millions
//! of pixels per case, meant for the Go CI lane), and `symbol_test.go`'s jbig2dec re-check.

use super::arith_int::{decode_iaid, decode_int, new_int_contexts};
use super::bitmap::{mask_padding, new_bitmap, row_equal_above};
use super::generic::{SLTP_CONTEXT_TEMPLATE0, context_template0, encode_generic_region};
use super::generic_decode::{decode_generic_region, decoded_pixels};
use super::generic_general::{
    GENERIC_TEMPLATES, NOMINAL_AT, SLTP_CONTEXTS, TemplatePixel, decode_generic_region_general,
};
use super::mq::{Contexts, Encoder, QE_TABLE};
use super::mq_decode::Decoder;
use super::segment::{
    NOMINAL_AT_TEMPLATE0, SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION, SEG_TYPE_PAGE_INFORMATION,
    generic_region_segment_data, page_info_segment_data, segment_header,
};
use super::segment_decode::{
    MAX_PAGE_PIXELS, MAX_STREAM_BITMAP_BYTES, MAX_STREAM_REFS, MAX_STREAM_SEGMENTS,
    SEG_TYPE_IMMEDIATE_GENERIC_REGION, SEG_TYPE_INTERMEDIATE_GENERIC_REGION,
    decode_generic_region_segment, parse_segments,
};
use super::symbol_header::{
    REF_CORNER_BOTTOM_LEFT, REF_CORNER_BOTTOM_RIGHT, REF_CORNER_TOP_LEFT, REF_CORNER_TOP_RIGHT,
    SEG_TYPE_IMMEDIATE_TEXT_REGION, SEG_TYPE_SYMBOL_DICTIONARY, sym_code_len,
};
use super::text_decode::place_symbol;
use super::{decode_embedded_stream, decode_embedded_stream_with_globals, embedded_stream};
use crate::bitmap::Bitmap;
use crate::error::Error;
use crate::limits::Limits;
use std::sync::atomic::AtomicBool;

// ---------------------------------------------------------------------------------------------
// Helpers and fixtures (fixtures_test.go, roundtrip_test.go, symbol_test.go).
// ---------------------------------------------------------------------------------------------

static NO_CANCEL: AtomicBool = AtomicBool::new(false);

fn lim() -> Limits {
    Limits::default()
}

fn nb(w: usize, h: usize) -> Bitmap {
    new_bitmap(w, h, &lim()).unwrap()
}

fn put(b: &mut Bitmap, x: usize, y: usize, v: u8) {
    b.set(x as i64, y as i64, v);
}

fn px(b: &Bitmap, x: usize, y: usize) -> u8 {
    b.get(x as i64, y as i64)
}

fn enc_region(b: &mut Bitmap, tpgdon: bool) -> Vec<u8> {
    encode_generic_region(b, tpgdon, &lim(), &NO_CANCEL).unwrap()
}

fn emb(b: &mut Bitmap) -> Vec<u8> {
    embedded_stream(b, &lim(), &NO_CANCEL).unwrap()
}

fn page_size(s: &[u8]) -> Result<(usize, usize), Error> {
    super::page_size(s, &lim())
}

fn dec(s: &[u8]) -> Result<Bitmap, Error> {
    decode_embedded_stream(s, &lim(), &NO_CANCEL)
}

fn dec_region(data: &[u8], w: usize, h: usize, tpgdon: bool) -> Bitmap {
    decode_generic_region(data, w, h, tpgdon, &lim(), &NO_CANCEL).unwrap()
}

fn region_seg_data(b: &mut Bitmap, x: u32, y: u32, tpgdon: bool) -> Vec<u8> {
    generic_region_segment_data(b, x, y, tpgdon, &lim(), &NO_CANCEL).unwrap()
}

fn page_info(w: usize, h: usize) -> Vec<u8> {
    page_info_segment_data(w as u32, h as u32)
}

fn dec_segment(d: &[u8]) -> Result<(Bitmap, super::segment_decode::RegionInfo), Error> {
    decode_generic_region_segment(d, &lim(), &NO_CANCEL)
}

/// The text of an error without the variant's own prefix.
fn msg(e: &Error) -> String {
    match e {
        Error::Malformed(s) | Error::UnsupportedJbig2Feature(s) => s.clone(),
        o => o.to_string(),
    }
}

fn is_unsupported(e: &Error) -> bool {
    matches!(e, Error::UnsupportedJbig2Feature(_))
}

fn ink_count(b: &Bitmap) -> usize {
    b.pix.iter().map(|v| v.count_ones() as usize).sum()
}

fn set_byte(s: &[u8], off: usize, v: u8) -> Vec<u8> {
    let mut out = s.to_vec();
    out[off] = v;
    out
}

fn set_u32(s: &[u8], off: usize, v: u32) -> Vec<u8> {
    let mut out = s.to_vec();
    out[off..off + 4].copy_from_slice(&v.to_be_bytes());
    out
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// T.88 Figure H.6: a 54x44 rectangle with a 2-pixel border.
fn figure_h6() -> Bitmap {
    let mut b = nb(54, 44);
    for y in 0..44 {
        for x in 0..54 {
            if !(2..42).contains(&y) || !(2..52).contains(&x) {
                put(&mut b, x, y, 1);
            }
        }
    }
    b
}

/// A deterministic pseudo-random bitmap: the worst case for an adaptive arithmetic coder.
fn noise_bitmap(w: usize, h: usize, seed: u32) -> Bitmap {
    let mut b = nb(w, h);
    let mut s = seed;
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            if (s >> 29) & 1 == 1 {
                put(&mut b, x, y, 1);
            }
        }
    }
    b
}

/// A page that behaves like a scan of text: wide white margins, blank inter-line gaps, and rows
/// of small ink blocks standing in for glyphs.
fn text_page_bitmap(w: usize, h: usize) -> Bitmap {
    let mut b = nb(w, h);
    let mut s: u32 = 20260727;
    let mut next = |n: usize| -> usize {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        (s >> 16) as usize % n
    };
    const MARGIN_X: usize = 40;
    const MARGIN_Y: usize = 60;
    const LINE_STEP: usize = 26;
    const GLYPH_W: usize = 6;
    const GLYPH_H: usize = 11;
    const GLYPH_GAP: usize = 2;
    const WORD_GAP: usize = 7;
    let mut top = MARGIN_Y;
    while top + GLYPH_H < h - MARGIN_Y {
        let mut x = MARGIN_X;
        while x < w - MARGIN_X - GLYPH_W {
            let word = 2 + next(7);
            let mut i = 0;
            while i < word && x < w - MARGIN_X - GLYPH_W {
                for gy in 0..GLYPH_H {
                    for gx in 0..GLYPH_W {
                        // Hollow the middle so glyphs are strokes, not blocks.
                        if gy == 0
                            || gy == GLYPH_H - 1
                            || gx == 0
                            || gx == GLYPH_W - 1
                            || (gy + gx).is_multiple_of(5)
                        {
                            put(&mut b, x + gx, top + gy, 1);
                        }
                    }
                }
                x += GLYPH_W + GLYPH_GAP;
                i += 1;
            }
            x += WORD_GAP;
        }
        top += LINE_STEP;
    }
    b
}

/// The corpus every downstream test iterates over: non-byte-aligned widths, single-pixel
/// dimensions, all-background, all-ink and pure noise.
fn fixture_bitmaps() -> Vec<(&'static str, Bitmap)> {
    let mut all = nb(200, 120);
    for y in 0..120 {
        for x in 0..200 {
            put(&mut all, x, y, 1);
        }
    }
    vec![
        ("border", figure_h6()),
        ("empty", nb(200, 120)),
        ("full", all),
        ("noise", noise_bitmap(101, 73, 12345)),
        ("odd", noise_bitmap(13, 11, 99)),
        ("single", nb(1, 1)),
        ("column", noise_bitmap(1, 500, 7)),
        ("row", noise_bitmap(500, 1, 11)),
        ("text", text_page_bitmap(640, 480)),
    ]
}

fn read_fixture(name: &str) -> Vec<u8> {
    let p = format!("{}/tests/data/jbig2/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading fixture {p}: {e}"))
}

/// Parses a binary PBM (P4). In PBM a 1 bit is black, which is also the ink convention here.
fn read_pbm(b: &[u8]) -> Bitmap {
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0;
    let is_space = |c: u8| matches!(c, b' ' | b'\n' | b'\t' | b'\r');
    while fields.len() < 3 {
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        if i < b.len() && b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < b.len() && !is_space(b[i]) {
            i += 1;
        }
        assert!(
            start != i,
            "PBM header ran out after {} fields",
            fields.len()
        );
        fields.push(String::from_utf8(b[start..i].to_vec()).unwrap());
    }
    i += 1; // the single whitespace byte after the height
    assert_eq!(fields[0], "P4");
    let w: usize = fields[1].parse().unwrap();
    let h: usize = fields[2].parse().unwrap();
    let mut bm = nb(w, h);
    let want = bm.stride * h;
    assert!(b.len() - i >= want, "PBM carries too few bytes of raster");
    bm.pix.copy_from_slice(&b[i..i + want]);
    bm
}

fn assert_bitmaps_identical(name: &str, got: &Bitmap, want: &Bitmap) {
    assert_eq!(
        (got.width, got.height),
        (want.width, want.height),
        "{name}: decoded size"
    );
    let mut diff = 0;
    let mut first = (0, 0);
    for y in 0..want.height {
        for x in 0..want.width {
            if px(got, x, y) != px(want, x, y) {
                if diff == 0 {
                    first = (x, y);
                }
                diff += 1;
            }
        }
    }
    assert_eq!(
        diff, 0,
        "{name}: round trip is NOT lossless, {diff} pixels differ, first at {first:?}"
    );
}

fn golden(name: &str) -> Vec<u8> {
    read_fixture(&format!("{name}.jb2"))
}

// ---------------------------------------------------------------------------------------------
// bitmap_test.go
// ---------------------------------------------------------------------------------------------

#[test]
fn bitmap_set_get_round_trip() {
    let mut b = nb(13, 5);
    assert_eq!(b.stride, 2);
    assert_eq!(b.pix.len(), 10);
    let pts = [(0, 0), (12, 4), (7, 2), (8, 0)];
    for p in pts {
        put(&mut b, p.0, p.1, 1);
    }
    for p in pts {
        assert_eq!(px(&b, p.0, p.1), 1, "Get({},{})", p.0, p.1);
    }
    assert_eq!(px(&b, 1, 1), 0);
    put(&mut b, 7, 2, 0);
    assert_eq!(px(&b, 7, 2), 0);
}

/// T.88 6.2.5.2: pixels outside the bitmap are 0. No replication, no wrap.
#[test]
fn bitmap_out_of_bounds_is_zero() {
    let mut b = nb(4, 4);
    for y in 0..4 {
        for x in 0..4 {
            put(&mut b, x, y, 1);
        }
    }
    for p in [(-1, 0), (0, -1), (4, 0), (0, 4), (-1, -1), (99, 99)] {
        assert_eq!(b.get(p.0, p.1), 0, "Get({},{})", p.0, p.1);
    }
}

#[test]
fn bitmap_packing_is_msb_first() {
    let mut b = nb(8, 1);
    put(&mut b, 0, 0, 1);
    assert_eq!(b.pix[0], 0x80);
    put(&mut b, 7, 0, 1);
    assert_eq!(b.pix[0], 0x81);
}

#[test]
fn bitmap_mask_padding_clears_bits_past_width() {
    let mut b = nb(5, 2);
    b.pix[0] = 0xFF;
    b.pix[1] = 0xFF;
    mask_padding(&mut b);
    assert_eq!(b.pix[0], 0xF8);
    assert_eq!(b.pix[1], 0xF8);
    for x in 0..5 {
        assert_eq!(px(&b, x, 0), 1, "MaskPadding cleared visible pixel ({x},0)");
    }
}

/// A stride larger than the minimal leaves whole bytes past the last visible pixel; MaskPadding
/// must clear them as well as the padding bits of the byte that holds pixel W-1.
#[test]
fn bitmap_mask_padding_handles_non_minimal_stride() {
    let mut b = Bitmap {
        width: 12,
        height: 2,
        stride: 4,
        pix: vec![0xFF; 8],
    };
    mask_padding(&mut b);
    assert_eq!(b.pix, vec![0xFF, 0xF0, 0x00, 0x00, 0xFF, 0xF0, 0x00, 0x00]);
    for x in 0..12 {
        assert_eq!(px(&b, x, 0), 1);
    }
    assert!(row_equal_above(&b, 1));
}

/// Row 0 is compared against an implicit all-zero row above the bitmap.
#[test]
fn bitmap_row_equal_above() {
    let mut b = nb(9, 4);
    put(&mut b, 3, 2, 1);
    put(&mut b, 3, 3, 1);
    let want = [true, true, false, true];
    for (y, w) in want.iter().enumerate() {
        assert_eq!(row_equal_above(&b, y), *w, "RowEqualAbove({y})");
    }
}

/// Go panics; this returns an error.
#[test]
fn new_bitmap_rejects_zero_dimensions() {
    for d in [(0, 5), (5, 0), (0, 0)] {
        assert!(matches!(
            new_bitmap(d.0, d.1, &lim()),
            Err(Error::Malformed(_))
        ));
    }
}

// ---------------------------------------------------------------------------------------------
// mq_test.go and the decoder vector of decode_test.go
// ---------------------------------------------------------------------------------------------

const MQ_IN: [u8; 32] = [
    0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA,
    0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 0xBF, 0x7F, 0xED, 0x90, 0x4F, 0x46, 0xA3, 0xBF,
];
const MQ_OUT: [u8; 30] = [
    0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D, 0xBB, 0x86,
    0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF, 0xFF, 0xAC,
];

fn new_enc() -> Encoder {
    Encoder::new(u64::MAX).unwrap()
}

/// Table E.1 has exactly 47 rows. Index 46 is the fixed 0.5 estimate.
#[test]
fn qe_table_shape() {
    assert_eq!(QE_TABLE.len(), 47);
    for (i, e) in QE_TABLE.iter().enumerate() {
        assert!(
            e.qe != 0 && e.qe <= 0x5601,
            "qeTable[{i}].qe = {:#06x}",
            e.qe
        );
        assert!((e.nmps as usize) < QE_TABLE.len());
        assert!((e.nlps as usize) < QE_TABLE.len());
        assert!(e.swch <= 1);
    }
    // SWITCH is set on exactly the three rows T.88 marks: 0, 6 and 14.
    for i in [0, 6, 14] {
        assert_eq!(QE_TABLE[i].swch, 1, "qeTable[{i}].swch");
    }
    assert_eq!((QE_TABLE[46].nmps, QE_TABLE[46].nlps), (46, 46));
}

/// The T.88 Annex H.2 test sequence: 256 decisions through a single context, exactly 30 bytes.
#[test]
fn mq_conformance_vector() {
    let mut cx: Contexts = vec![0; 1];
    let mut e = new_enc();
    for b in MQ_IN {
        for i in (0..8).rev() {
            e.encode(&mut cx, 0, (b >> i) & 1).unwrap();
        }
    }
    assert_eq!(e.flush().unwrap(), MQ_OUT.to_vec());
}

/// The same vector backwards: the only check on the MQ decoder that does not go through this
/// package's own encoder.
#[test]
fn mq_decode_conformance_vector() {
    let mut cx: Contexts = vec![0; 1];
    let mut d = Decoder::new(&MQ_OUT);
    let mut got = [0u8; 32];
    for g in &mut got {
        let mut v = 0u8;
        for bit in (0..8).rev() {
            v |= d.decode(&mut cx, 0) << bit;
        }
        *g = v;
    }
    assert_eq!(got, MQ_IN);
}

#[test]
fn mq_flush_terminator() {
    let mut cx: Contexts = vec![0; 1];
    let mut e = new_enc();
    for i in 0..40 {
        e.encode(&mut cx, 0, i & 1).unwrap();
    }
    let got = e.flush().unwrap();
    assert!(got.len() >= 2 && got[got.len() - 2] == 0xFF && got[got.len() - 1] == 0xAC);
}

/// Byte stuffing after 0xFF emits only 7 bits, so no 0xFF byte may be followed by a byte above
/// 0x8F inside the coded data.
#[test]
fn mq_no_marker_sequence_in_data() {
    let mut cx: Contexts = vec![0; 1 << 8];
    let mut e = new_enc();
    let mut s: u32 = 1;
    for _ in 0..20000 {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        e.encode(&mut cx, ((s >> 24) & 0xFF) as usize, ((s >> 16) & 1) as u8)
            .unwrap();
    }
    let got = e.flush().unwrap();
    let body = &got[..got.len() - 2];
    for i in 0..body.len() - 1 {
        assert!(
            !(body[i] == 0xFF && body[i + 1] > 0x8F),
            "marker sequence FF {:02X} at offset {i}",
            body[i + 1]
        );
    }
}

#[test]
fn mq_flush_with_no_decisions() {
    let got = new_enc().flush().unwrap();
    assert!(!got.is_empty());
    assert_eq!(&got[got.len() - 2..], &[0xFF, 0xAC]);
}

/// The coded bytes are capped at the encoder's `max`.
#[test]
fn mq_encoder_output_is_capped() {
    let mut cx: Contexts = vec![0; 1 << 8];
    let mut e = Encoder::new(8).unwrap();
    let mut s: u32 = 1;
    let mut err = None;
    for _ in 0..1000 {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        if let Err(x) = e.encode(&mut cx, ((s >> 24) & 0xFF) as usize, ((s >> 16) & 1) as u8) {
            err = Some(x);
            break;
        }
    }
    assert!(matches!(err, Some(Error::Alloc { .. })));
}

// ---------------------------------------------------------------------------------------------
// generic_test.go
// ---------------------------------------------------------------------------------------------

/// Each template position maps to exactly one context bit, in reading order with MSB = top-left.
#[test]
fn context_template0_bit_positions() {
    let cases: [(i64, i64, u32); 16] = [
        (-2, -2, 15),
        (-1, -2, 14),
        (0, -2, 13),
        (1, -2, 12),
        (2, -2, 11),
        (-3, -1, 10),
        (-2, -1, 9),
        (-1, -1, 8),
        (0, -1, 7),
        (1, -1, 6),
        (2, -1, 5),
        (3, -1, 4),
        (-4, 0, 3),
        (-3, 0, 2),
        (-2, 0, 1),
        (-1, 0, 0),
    ];
    for (dx, dy, bit) in cases {
        let mut b = nb(12, 8);
        b.set(5 + dx, 4 + dy, 1);
        assert_eq!(
            context_template0(&b, 5, 4),
            1usize << bit,
            "template pixel ({dx},{dy})"
        );
    }
}

fn all_ink(w: usize, h: usize) -> Bitmap {
    let mut b = nb(w, h);
    for y in 0..h {
        for x in 0..w {
            put(&mut b, x, y, 1);
        }
    }
    b
}

#[test]
fn context_template0_all_ones_interior() {
    assert_eq!(context_template0(&all_ink(9, 5), 4, 3), 0xFFFF);
}

/// At the top-left corner every template pixel is out of bounds, so the context is 0.
#[test]
fn context_template0_corner_is_zero() {
    assert_eq!(context_template0(&all_ink(9, 5), 0, 0), 0);
}

#[test]
fn sltp_context_decomposition() {
    assert_eq!(SLTP_CONTEXT_TEMPLATE0, 0x9B25);
    assert_eq!((SLTP_CONTEXT_TEMPLATE0 >> 11) & 0x1F, 0b10011);
    assert_eq!((SLTP_CONTEXT_TEMPLATE0 >> 4) & 0x7F, 0b0110010);
    assert_eq!(SLTP_CONTEXT_TEMPLATE0 & 0x0F, 0b0101);
}

/// A bitmap built so that ordinary pixel coding at (6, 4) visits exactly 0x9B25, which collides
/// with the SLTP context under TPGDON: a wrong constant becomes a wrong byte.
#[test]
fn sltp_context_is_pinned_against_collision() {
    let mut b = nb(16, 8);
    for (i, v) in [1u8, 0, 0, 1, 1].iter().enumerate() {
        put(&mut b, 4 + i, 2, *v);
    }
    for (i, v) in [0u8, 1, 1, 0, 0, 1, 0].iter().enumerate() {
        put(&mut b, 3 + i, 3, *v);
    }
    for (i, v) in [0u8, 1, 0, 1].iter().enumerate() {
        put(&mut b, 2 + i, 4, *v);
    }
    assert_eq!(context_template0(&b, 6, 4), SLTP_CONTEXT_TEMPLATE0);
    let want = [0xBE, 0x68, 0x17, 0x23, 0x56, 0xE8, 0x8F, 0xBF, 0xFF, 0xAC];
    assert_eq!(enc_region(&mut b, true), want.to_vec());
}

/// The T.88 Annex H.1 segment 11 conformance vector: a 54x44 bitmap coded with GBTEMPLATE 0,
/// TPGDON = 1 and nominal AT pixels produces exactly these nine bytes.
const H1_REGION: [u8; 9] = [0x04, 0xEE, 0xED, 0x87, 0xFB, 0xCB, 0x2B, 0xFF, 0xAC];

#[test]
fn encode_generic_region_annex_h1() {
    assert_eq!(enc_region(&mut figure_h6(), true), H1_REGION.to_vec());
}

#[test]
fn tpgd_shrinks_repeated_rows() {
    let mut b = figure_h6();
    let with = enc_region(&mut b, true).len();
    let without = enc_region(&mut b, false).len();
    assert_eq!(with, 9);
    assert_eq!(without, 20);
}

#[test]
fn encode_generic_region_all_background() {
    assert_eq!(enc_region(&mut nb(16, 4), true), vec![0xB3, 0xFF, 0xAC]);
}

/// Stray padding bits past the row width must not change the output.
#[test]
fn encode_generic_region_ignores_padding_bits() {
    let mut clean = noise_bitmap(13, 11, 99);
    let mut dirty = noise_bitmap(13, 11, 99);
    for y in 0..dirty.height {
        let i = y * dirty.stride + dirty.stride - 1;
        dirty.pix[i] |= 0x07;
    }
    assert_ne!(clean.pix, dirty.pix);
    assert_eq!(enc_region(&mut clean, true), enc_region(&mut dirty, true));
}

/// A stride larger than the minimal is accepted; the trailing bytes are cleared before coding.
#[test]
fn encode_generic_region_ignores_non_minimal_stride() {
    let (w, h) = (100usize, 200usize);
    let build = |stride: usize, junk: bool| {
        let mut b = Bitmap {
            width: w,
            height: h,
            stride,
            pix: vec![0; stride * h],
        };
        for y in 0..h {
            for x in 0..w {
                if !(3..h - 3).contains(&y) || !(3..w - 3).contains(&x) {
                    put(&mut b, x, y, 1);
                }
            }
            if junk {
                for i in w.div_ceil(8)..stride {
                    b.pix[y * stride + i] = 0xA5u8.wrapping_add(y as u8);
                }
            }
        }
        b
    };
    let want = enc_region(&mut build(w.div_ceil(8), false), true);
    let got = enc_region(&mut build(16, true), true);
    assert_eq!(got, want);
}

#[test]
fn encode_generic_region_corpus_well_formed() {
    for (name, mut b) in fixture_bitmaps() {
        let got = enc_region(&mut b, true);
        assert!(got.len() >= 2, "{name}");
        assert_eq!(&got[got.len() - 2..], &[0xFF, 0xAC], "{name}");
    }
}

/// The straightforward implementation: `context_template0` for every pixel.
fn encode_generic_region_reference(b: &mut Bitmap, tpgdon: bool) -> Vec<u8> {
    mask_padding(b);
    let mut cx: Contexts = vec![0; 1 << 16];
    let mut e = new_enc();
    let mut ltp = 0u8;
    for y in 0..b.height {
        if tpgdon {
            let next = u8::from(row_equal_above(b, y));
            e.encode(&mut cx, SLTP_CONTEXT_TEMPLATE0, next ^ ltp)
                .unwrap();
            ltp = next;
            if ltp == 1 {
                continue;
            }
        }
        for x in 0..b.width as i64 {
            e.encode(
                &mut cx,
                context_template0(b, x, y as i64),
                b.get(x, y as i64),
            )
            .unwrap();
        }
    }
    e.flush().unwrap()
}

/// The sliding-window context update must equal forming each context from scratch.
#[test]
fn encode_generic_region_matches_reference() {
    for (name, mut b) in fixture_bitmaps() {
        for tpgdon in [true, false] {
            let want = encode_generic_region_reference(&mut b, tpgdon);
            let got = enc_region(&mut b, tpgdon);
            assert_eq!(got, want, "{name} (tpgdon={tpgdon})");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// generic_general_test.go
// ---------------------------------------------------------------------------------------------

/// Compares the CONTEXTS the table walk and the incremental loop form, at every pixel of every
/// fixture, including the margin where the template hangs off the bitmap.
#[test]
fn general_template_agrees_with_the_incremental_loop() {
    let tmpl = GENERIC_TEMPLATES[0];
    let general = |b: &Bitmap, x: i64, y: i64| -> usize {
        let mut cx = 0usize;
        for p in tmpl {
            let (dx, dy) = match *p {
                TemplatePixel::Fixed(dx, dy) => (dx, dy),
                TemplatePixel::At(i) => (i64::from(NOMINAL_AT[i][0]), i64::from(NOMINAL_AT[i][1])),
            };
            cx = (cx << 1) | usize::from(b.get(x + dx, y + dy));
        }
        cx
    };
    for (name, b) in fixture_bitmaps() {
        for y in -3..b.height as i64 + 1 {
            for x in -5..b.width as i64 + 5 {
                assert_eq!(
                    general(&b, x, y),
                    context_template0(&b, x, y),
                    "{name} at ({x},{y})"
                );
            }
        }
    }
}

#[test]
fn sltp_contexts_match_the_template_that_uses_them() {
    assert_eq!(SLTP_CONTEXTS[0], SLTP_CONTEXT_TEMPLATE0);
    assert_eq!(SLTP_CONTEXTS, [0x9B25, 0x0795, 0x00E5, 0x0195]);
}

#[test]
fn general_path_decodes_what_the_incremental_path_decodes() {
    for (name, want) in fixture_bitmaps() {
        for tpgdon in [false, true] {
            let mut src = want.clone();
            let coded = enc_region(&mut src, tpgdon);
            let mut d = Decoder::new(&coded);
            let mut cx: Contexts = vec![0; 1 << 16];
            let got = decode_generic_region_general(
                &mut d,
                &mut cx,
                want.width,
                want.height,
                0,
                &NOMINAL_AT,
                tpgdon,
                &lim(),
                &NO_CANCEL,
            )
            .unwrap();
            assert_eq!(got.pix, want.pix, "{name} tpgdon={tpgdon}");
        }
    }
}

#[test]
fn general_path_rejects_a_bad_template_and_zero_size() {
    let mut d = Decoder::new(&[]);
    let mut cx: Contexts = vec![0; 1 << 16];
    let run = |d: &mut Decoder, cx: &mut Contexts, w, h, t| {
        decode_generic_region_general(d, cx, w, h, t, &NOMINAL_AT, false, &lim(), &NO_CANCEL)
    };
    assert!(run(&mut d, &mut cx, 4, 4, 4).is_err());
    assert!(run(&mut d, &mut cx, 0, 4, 0).is_err());
    assert!(run(&mut d, &mut cx, 4, 0, 0).is_err());
}

// ---------------------------------------------------------------------------------------------
// segment_test.go
// ---------------------------------------------------------------------------------------------

#[test]
fn segment_header_matches_annex_h1() {
    let got = segment_header(11, SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION, 2, 35);
    assert_eq!(
        got,
        vec![
            0x00, 0x00, 0x00, 0x0B, 0x27, 0x00, 0x02, 0x00, 0x00, 0x00, 0x23
        ]
    );
}

#[test]
fn page_info_segment_header_matches_annex_h1() {
    let got = segment_header(8, SEG_TYPE_PAGE_INFORMATION, 2, 19);
    assert_eq!(
        got,
        vec![
            0x00, 0x00, 0x00, 0x08, 0x30, 0x00, 0x02, 0x00, 0x00, 0x00, 0x13
        ]
    );
}

#[test]
fn page_info_segment_data_matches_annex_h1() {
    let got = page_info_segment_data(64, 56);
    let want = vec![
        0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00,
    ];
    assert_eq!(got, want);
    assert_eq!(got.len(), 19);
}

#[test]
fn generic_region_segment_data_matches_annex_h1() {
    let got = region_seg_data(&mut figure_h6(), 4, 11, true);
    let want = vec![
        0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x0B, 0x00, 0x08, 0x03, 0xFF, 0xFD, 0xFF, 0x02, 0xFE, 0xFE, 0xFE, 0x04, 0xEE, 0xED, 0x87,
        0xFB, 0xCB, 0x2B, 0xFF, 0xAC,
    ];
    assert_eq!(got, want);
    assert_eq!(got.len(), 35);
}

/// The embedded stream is exactly two segments, both on page 1, with no JBIG2 file header.
#[test]
fn embedded_stream_structure() {
    let got = emb(&mut figure_h6());
    assert!(!got.starts_with(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]));
    let u32_at = |o: usize| u32::from_be_bytes([got[o], got[o + 1], got[o + 2], got[o + 3]]);
    assert_eq!(u32_at(0), 0);
    assert_eq!(got[4], SEG_TYPE_PAGE_INFORMATION);
    assert_eq!(got[5], 0x00);
    assert_eq!(got[6], 1);
    assert_eq!(u32_at(7), 19);
    let s1 = 11 + 19;
    assert_eq!(u32_at(s1), 1);
    assert_eq!(got[s1 + 4], SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION);
    assert_eq!(got[s1 + 6], 1);
    let body_len = u32_at(s1 + 7) as usize;
    assert_eq!(body_len, got.len() - s1 - 11);
    assert_eq!(got.len(), s1 + 11 + body_len);
}

#[test]
fn embedded_stream_rejects_oversize_bitmap() {
    for w in [1usize << 33, u32::MAX as usize + 1] {
        let mut b = Bitmap {
            width: w,
            height: 1,
            stride: 1,
            pix: vec![0; 1],
        };
        assert!(
            embedded_stream(&mut b, &lim(), &NO_CANCEL).is_err(),
            "width {w}"
        );
    }
}

#[test]
fn embedded_stream_rejects_a_zero_dimension() {
    let mut a = Bitmap {
        width: 16,
        height: 0,
        stride: 2,
        pix: vec![],
    };
    let mut b = Bitmap {
        width: 0,
        height: 4,
        stride: 2,
        pix: vec![0; 8],
    };
    assert!(embedded_stream(&mut a, &lim(), &NO_CANCEL).is_err());
    assert!(embedded_stream(&mut b, &lim(), &NO_CANCEL).is_err());
}

/// The layout checks of `byblos.EncodeJBIG2Generic`, which here guard every index.
#[test]
fn embedded_stream_rejects_an_inconsistent_layout() {
    let mut short_stride = Bitmap {
        width: 16,
        height: 2,
        stride: 1,
        pix: vec![0; 2],
    };
    let mut short_pix = Bitmap {
        width: 16,
        height: 2,
        stride: 2,
        pix: vec![0; 3],
    };
    let mut overflow = Bitmap {
        width: 16,
        height: usize::MAX,
        stride: 2,
        pix: vec![0; 3],
    };
    assert!(embedded_stream(&mut short_stride, &lim(), &NO_CANCEL).is_err());
    assert!(embedded_stream(&mut short_pix, &lim(), &NO_CANCEL).is_err());
    assert!(embedded_stream(&mut overflow, &lim(), &NO_CANCEL).is_err());
}

#[test]
fn encoding_honours_cancel_and_the_alloc_cap() {
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        embedded_stream(&mut figure_h6(), &lim(), &cancelled),
        Err(Error::Cancelled)
    );
    let tiny = Limits {
        max_alloc: 4,
        ..Limits::default()
    };
    assert!(matches!(
        embedded_stream(&mut noise_bitmap(64, 64, 1), &tiny, &NO_CANCEL),
        Err(Error::Alloc { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// decode_test.go
// ---------------------------------------------------------------------------------------------

// The generic region segment is the second EmbeddedStream writes. Its header is 11 bytes and the
// page information segment before it is 11 + 19.
const REGION_DATA_OFFSET: usize = 11 + 19 + 11;
const REGION_INFO_FLAGS_AT: usize = REGION_DATA_OFFSET + 16;
const REGION_FLAGS_AT: usize = REGION_DATA_OFFSET + 17;
const REGION_AT_AT: usize = REGION_DATA_OFFSET + 18;
const PAGE_FLAGS_AT: usize = 11 + 16;

/// Decodes the T.88 Annex H.1 segment 11 vector, which is independent of this package's encoder.
#[test]
fn decode_generic_region_annex_h1() {
    let got = dec_region(&H1_REGION, 54, 44, true);
    assert_bitmaps_identical("annex-h1", &got, &figure_h6());
}

#[test]
fn decode_round_trip_bit_identical() {
    for (name, mut want) in fixture_bitmaps() {
        mask_padding(&mut want);
        let stream = emb(&mut want);
        let got = dec(&stream).unwrap();
        assert_bitmaps_identical(name, &got, &want);
        assert_eq!(got.pix, want.pix, "{name}: padding is dirty");
    }
}

/// The committed goldens are streams a real jbig2dec confirmed lossless when they were written.
#[test]
fn decode_goldens() {
    for (name, mut want) in fixture_bitmaps() {
        mask_padding(&mut want);
        let got = dec(&golden(name)).unwrap();
        assert_bitmaps_identical(name, &got, &want);
    }
}

/// Pins the exact bytes the encoder produces for every fixture.
#[test]
fn encoder_goldens() {
    for (name, mut b) in fixture_bitmaps() {
        mask_padding(&mut b);
        let got = emb(&mut b);
        assert_eq!(got, golden(name), "{name}: encoder output changed");
    }
}

/// TPGDON off: the branch EmbeddedStream never exercises.
#[test]
fn decode_tpgd_off() {
    for (name, mut want) in fixture_bitmaps() {
        mask_padding(&mut want);
        let data = enc_region(&mut want, false);
        let got = dec_region(&data, want.width, want.height, false);
        assert_bitmaps_identical(name, &got, &want);
    }
}

/// Rewrites a page information segment's height to the unknown value 0xFFFFFFFF.
fn striped_stream(s: &[u8]) -> Vec<u8> {
    set_u32(s, 11 + 4, 0xFFFF_FFFF)
}

#[test]
fn decode_unknown_page_height() {
    let mut want = figure_h6();
    let stream = emb(&mut want);
    let got = dec(&striped_stream(&stream)).unwrap();
    assert_bitmaps_identical("unknown-height", &got, &want);
}

/// Placing a region at a non-zero offset, dropping the part outside the page (T.88 6.2.4), and
/// the five external combination operators.
#[test]
fn decode_clips_region_to_page() {
    let mut full = figure_h6();
    let base = emb(&mut full);
    let (page_w_at, page_h_at) = (11, 11 + 4);
    let (region_x_at, region_y_at) = (REGION_DATA_OFFSET + 8, REGION_DATA_OFFSET + 12);

    // A 20x20 page under a 54x44 region.
    let s = set_u32(&set_u32(&base, page_w_at, 20), page_h_at, 20);
    let got = dec(&s).unwrap();
    assert_eq!((got.width, got.height), (20, 20));
    let mut want = nb(20, 20);
    for y in 0..20 {
        for x in 0..20 {
            put(&mut want, x, y, px(&full, x, y));
        }
    }
    assert_bitmaps_identical("clipped", &got, &want);
    assert_eq!(got.pix, want.pix);

    // A 54x44 region at (5,7) on a 64x64 page: placed whole, nothing else touched.
    let s = set_u32(&set_u32(&base, page_w_at, 64), page_h_at, 64);
    let s = set_u32(&set_u32(&s, region_x_at, 5), region_y_at, 7);
    let got = dec(&s).unwrap();
    let mut want = nb(64, 64);
    for y in 0..full.height {
        for x in 0..full.width {
            put(&mut want, x + 5, y + 7, px(&full, x, y));
        }
    }
    assert_bitmaps_identical("offset", &got, &want);

    // The operators of T.88 Table 12, each over both page default pixel values, written as
    // functions of the source pixel evaluated by hand.
    type Op = fn(u8) -> u8;
    let ops: [(&str, u8, Op, Op); 5] = [
        ("or", 0, |s| s, |_| 1),
        ("and", 1, |_| 0, |s| s),
        ("xor", 2, |s| s, |s| 1 - s),
        ("xnor", 3, |s| 1 - s, |s| s),
        ("replace", 4, |s| s, |s| s),
    ];
    for (name, op, on_background, on_ink) in ops {
        for (i, flags) in [0x01u8, 0x01 | 0x04].into_iter().enumerate() {
            let want_fn = if i == 1 { on_ink } else { on_background };
            let s = set_byte(
                &set_byte(&base, REGION_INFO_FLAGS_AT, op),
                PAGE_FLAGS_AT,
                flags,
            );
            let got = dec(&s).unwrap();
            let mut exp = nb(full.width, full.height);
            for y in 0..full.height {
                for x in 0..full.width {
                    put(&mut exp, x, y, want_fn(px(&full, x, y)));
                }
            }
            assert_bitmaps_identical(&format!("{name}/{i}"), &got, &exp);
        }
    }

    // Bits 3-7 of the operator byte are reserved and ignored.
    let got = dec(&set_byte(&base, REGION_INFO_FLAGS_AT, 0xF8)).unwrap();
    assert_bitmaps_identical("reserved-bits-set", &got, &full);
}

/// Every case is a legal stream some other producer could write and this package cannot decode:
/// each must be an unsupported feature, never a wrong raster.
#[test]
fn decode_rejects_rather_than_guesses() {
    let base = emb(&mut figure_h6());
    assert_eq!(base[REGION_FLAGS_AT], 0x08);
    let cases = [
        ("mmr", set_byte(&base, REGION_FLAGS_AT, 0x09)),
        ("template1", set_byte(&base, REGION_FLAGS_AT, 0x0A)),
        ("template2", set_byte(&base, REGION_FLAGS_AT, 0x0C)),
        ("template3", set_byte(&base, REGION_FLAGS_AT, 0x0E)),
        ("ext-template", set_byte(&base, REGION_FLAGS_AT, 0x18)),
        ("non-nominal-at", set_byte(&base, REGION_AT_AT, 0x08)),
        ("intermediate-generic", set_byte(&base, 11 + 19 + 4, 36)),
        ("intermediate-text", set_byte(&base, 11 + 19 + 4, 4)),
        ("refinement-region", set_byte(&base, 11 + 19 + 4, 42)),
        ("halftone-region", set_byte(&base, 11 + 19 + 4, 22)),
        ("pattern-dictionary", set_byte(&base, 11 + 19 + 4, 16)),
    ];
    for (name, stream) in cases {
        let e = dec(&stream).expect_err(name);
        assert!(is_unsupported(&e), "{name}: {e}");
        assert_eq!(e.class(), "jbig2/unsupported");
    }
}

#[test]
fn decode_rejects_malformed() {
    let base = emb(&mut figure_h6());
    let mut unknown_len = base[..7].to_vec();
    unknown_len.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    unknown_len.extend_from_slice(&base[11..]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("header-only", base[..8].to_vec()),
        ("truncated-page-info", base[..11 + 10].to_vec()),
        (
            "truncated-region-header",
            base[..REGION_DATA_OFFSET + 9].to_vec(),
        ),
        ("declared-length-past-end", base[..base.len() - 1].to_vec()),
        (
            "bad-combination-operator",
            set_byte(&base, REGION_INFO_FLAGS_AT, 0x07),
        ),
        (
            "combination-operator-5",
            set_byte(&base, REGION_INFO_FLAGS_AT, 0x05),
        ),
        ("unknown-data-length", unknown_len),
    ];
    for (name, stream) in cases {
        assert!(dec(&stream).is_err(), "{name}");
    }
}

/// The 1-bits-past-the-end rule of the MQ decoder: no cut of the coded data may panic.
#[test]
fn decode_truncated_coded_data_does_not_panic() {
    let mut b = figure_h6();
    let data = enc_region(&mut b, true);
    for cut in (0..=data.len()).rev() {
        let got = dec_region(&data[..cut], b.width, b.height, true);
        assert_eq!((got.width, got.height), (b.width, b.height));
    }
    assert_bitmaps_identical(
        "untruncated",
        &dec_region(&data, b.width, b.height, true),
        &b,
    );
}

#[test]
fn decode_rejects_absurd_region() {
    let base = emb(&mut figure_h6());
    let huge = set_u32(
        &set_u32(&base, REGION_DATA_OFFSET, 0xFFFF),
        REGION_DATA_OFFSET + 4,
        0xFFFF,
    );
    assert!(dec(&huge).is_err());
}

#[test]
fn decode_rejects_absurd_region_offset() {
    let base = emb(&mut figure_h6());
    let s = set_u32(&striped_stream(&base), REGION_DATA_OFFSET + 12, 0xFFFF_FFFF);
    assert!(dec(&s).is_err());
}

#[test]
fn decode_default_pixel_value() {
    let mut b = nb(13, 11); // non-byte-aligned, so the padding rule is live
    let stream = emb(&mut b);
    let got = dec(&set_byte(&stream, PAGE_FLAGS_AT, 0x01 | 0x04)).unwrap();
    for y in 0..got.height {
        for x in 0..got.width {
            assert_eq!(px(&got, x, y), 1, "pixel ({x},{y})");
        }
    }
    let mut want = nb(13, 11);
    want.pix.fill(0xFF);
    mask_padding(&mut want);
    assert_eq!(got.pix, want.pix);
}

/// A stream of n immediate generic region segments, each declaring w x h pixels at (0,0) and
/// carrying no coded data at all: 37 bytes apiece.
fn empty_region_segments(page_w: usize, page_h: usize, w: u32, h: u32, n: usize) -> Vec<u8> {
    let pi = page_info(page_w, page_h);
    let mut out = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    out.extend_from_slice(&pi);
    for i in 0..n {
        let mut d = Vec::with_capacity(26);
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.push(0x00); // external combination operator OR
        d.push(0x08); // MMR=0, GBTEMPLATE=0, TPGDON=1
        d.extend_from_slice(&NOMINAL_AT_TEMPLATE0);
        out.extend_from_slice(&segment_header(
            i as u32 + 1,
            SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
            1,
            d.len(),
        ));
        out.extend_from_slice(&d);
    }
    out
}

/// A generic region segment body of the declared size with no coded data.
fn region_segment_data(w: u32, h: u32) -> Vec<u8> {
    let mut d = Vec::with_capacity(26);
    d.extend_from_slice(&w.to_be_bytes());
    d.extend_from_slice(&h.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&[0x00, 0x08]);
    d.extend_from_slice(&NOMINAL_AT_TEMPLATE0);
    d
}

/// A hostile stream of 8 regions at the per-region cap is refused from the headers, before any
/// region is decoded, and as a resource refusal rather than an unsupported feature.
#[test]
fn decode_bounds_the_whole_stream_not_just_one_region() {
    let s = empty_region_segments(8192, 4096, 8192, 4096, 8);
    assert_eq!(s.len(), 30 + 8 * 37);
    let before = decoded_pixels();
    let e = dec(&s).expect_err("8 regions of half the page budget each");
    assert!(!is_unsupported(&e), "{e}");
    assert!(msg(&e).contains("budget"), "{e}");
    assert_eq!(decoded_pixels() - before, 0);
}

#[test]
fn decode_accepts_several_regions_within_the_budget() {
    let mut tile = figure_h6();
    let pi = page_info(120, 100);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    let at = [(0usize, 0usize), (60, 0), (0, 50), (60, 50)];
    for (i, p) in at.iter().enumerate() {
        let d = region_seg_data(&mut tile, p.0 as u32, p.1 as u32, true);
        s.extend_from_slice(&segment_header(
            i as u32 + 1,
            SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
            1,
            d.len(),
        ));
        s.extend_from_slice(&d);
    }
    let got = dec(&s).unwrap();
    let mut want = nb(120, 100);
    for p in at {
        for y in 0..tile.height {
            for x in 0..tile.width {
                if px(&tile, x, y) != 0 {
                    put(&mut want, x + p.0, y + p.1, 1);
                }
            }
        }
    }
    assert_bitmaps_identical("four-tiles", &got, &want);
}

/// One T.88 7.2 segment header assembled byte by byte in the wide forms the encoder never writes.
#[derive(Default)]
struct HandHeader {
    num: u32,
    typ: u8,
    deferred: bool,
    page4: bool,
    refs: Vec<u32>,
    ref_bytes: usize,
    long_count: bool,
    retain_bytes: usize,
    data: Vec<u8>,
}

impl HandHeader {
    fn bytes(&self) -> Vec<u8> {
        let mut out = self.num.to_be_bytes().to_vec();
        let mut flags = self.typ;
        if self.page4 {
            flags |= 0x40;
        }
        if self.deferred {
            flags |= 0x80;
        }
        out.push(flags);
        if self.long_count {
            // A four-byte field whose top three bits are the 111 selector and whose low 29 bits
            // are the count, then the retain bytes.
            out.extend_from_slice(&(0xE000_0000u32 | self.refs.len() as u32).to_be_bytes());
            out.extend(std::iter::repeat_n(0u8, self.retain_bytes));
        } else {
            out.push((self.refs.len() as u8) << 5);
        }
        for r in &self.refs {
            match self.ref_bytes {
                1 => out.push(*r as u8),
                2 => out.extend_from_slice(&(*r as u16).to_be_bytes()),
                _ => out.extend_from_slice(&r.to_be_bytes()),
            }
        }
        if self.page4 {
            out.extend_from_slice(&1u32.to_be_bytes());
        } else {
            out.push(1);
        }
        out.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }
}

/// The variable-width parts of the T.88 7.2 segment header, on the boundaries.
#[test]
fn parse_segments_header_arithmetic() {
    let payload = vec![0xAA, 0xBB, 0xCC];
    let t = SEG_TYPE_IMMEDIATE_GENERIC_REGION;
    let cases = vec![
        (
            "ref-1-byte-at-256",
            HandHeader {
                num: 256,
                typ: t,
                refs: vec![1],
                ref_bytes: 1,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "ref-2-byte-at-257",
            HandHeader {
                num: 257,
                typ: t,
                refs: vec![1],
                ref_bytes: 2,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "ref-2-byte-at-65536",
            HandHeader {
                num: 65536,
                typ: t,
                refs: vec![1],
                ref_bytes: 2,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "ref-4-byte-at-65537",
            HandHeader {
                num: 65537,
                typ: t,
                refs: vec![1],
                ref_bytes: 4,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "long-count-8-refs",
            HandHeader {
                num: 3,
                typ: t,
                refs: vec![1, 2, 3, 4, 5, 6, 7, 8],
                ref_bytes: 1,
                long_count: true,
                retain_bytes: 2,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "deferred-bit-set",
            HandHeader {
                num: 4,
                typ: t,
                deferred: true,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "page-association-4-byte",
            HandHeader {
                num: 5,
                typ: t,
                page4: true,
                data: payload.clone(),
                ..Default::default()
            },
        ),
        (
            "page-association-4-byte-and-deferred",
            HandHeader {
                num: 6,
                typ: t,
                page4: true,
                deferred: true,
                refs: vec![1],
                ref_bytes: 1,
                data: payload.clone(),
                ..Default::default()
            },
        ),
    ];
    for (name, h) in cases {
        let bytes = h.bytes();
        let parsed = parse_segments(&bytes).expect(name);
        assert_eq!(parsed.segs.len(), 1, "{name}");
        assert_eq!(parsed.segs[0].number, h.num, "{name}");
        assert_eq!(parsed.segs[0].typ, h.typ, "{name}");
        assert_eq!(parsed.segs[0].data, &h.data[..], "{name}");
    }

    // Two wide headers back to back: the offset arithmetic is checked as well as the widths.
    let first = HandHeader {
        num: 70000,
        typ: SEG_TYPE_PAGE_INFORMATION,
        refs: vec![1, 2],
        ref_bytes: 4,
        page4: true,
        data: vec![0x11, 0x22],
        ..Default::default()
    };
    let second = HandHeader {
        num: 70001,
        typ: t,
        refs: (1..=9).collect(),
        ref_bytes: 4,
        long_count: true,
        retain_bytes: 2,
        deferred: true,
        data: vec![0x33],
        ..Default::default()
    };
    let s = cat(&[&first.bytes(), &second.bytes()]);
    let parsed = parse_segments(&s).unwrap();
    assert_eq!(parsed.segs.len(), 2);
    let (a, b) = (parsed.segs[0], parsed.segs[1]);
    assert_eq!(
        (a.number, a.typ, a.data),
        (70000, SEG_TYPE_PAGE_INFORMATION, &[0x11u8, 0x22][..])
    );
    assert_eq!((b.number, b.typ, b.data), (70001, t, &[0x33u8][..]));
}

/// Each stream is exactly ONE unit over the rule it pins; the dimensions are literals.
#[test]
fn resource_budgets_are_pinned_from_above() {
    // Rule 1, the output budget on the page: 8321 x 8065 = 67,108,865 pixels.
    let e = page_size(&empty_region_segments(8321, 8065, 1, 1, 1)).expect_err("page-pixels");
    assert!(msg(&e).contains("budget"), "{e}");
    assert!(msg(&e).contains("67108865 page pixels"), "{e}");

    // The page width guard names the width.
    let mut s = empty_region_segments(1, 1, 1, 1, 1);
    s[11..15].copy_from_slice(&67108865u32.to_be_bytes());
    let e = page_size(&s).expect_err("page-width");
    assert!(msg(&e).contains("pixels wide"), "{e}");

    // Rule 2, the work budget on the sum of the regions.
    let mut s = empty_region_segments(8192, 8191, 8192, 8192, 1);
    s.extend_from_slice(&segment_header(
        2,
        SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
        1,
        26,
    ));
    s.extend_from_slice(&region_segment_data(1, 1));
    let e = page_size(&s).expect_err("pixels");
    assert!(msg(&e).contains("budget"), "{e}");

    // Rule 4, the memory budget: 16,777,217 bytes, one over.
    let e = page_size(&empty_region_segments(4096, 4096, 1, 14680065, 1)).expect_err("bytes");
    assert!(msg(&e).contains("budget"), "{e}");

    // Rule 3, the ratio: regions summing to 4x the page plus four times the floor.
    let mut s = empty_region_segments(2048, 2048, 4096, 4096, 1);
    s.extend_from_slice(&segment_header(
        2,
        SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
        1,
        26,
    ));
    s.extend_from_slice(&region_segment_data(4096, 64));
    let e = page_size(&s).expect_err("overdraw-ratio");
    assert!(msg(&e).contains("budget"), "{e}");
    assert!(msg(&e).contains("admits 4x the page"), "{e}");

    // Rule 3, the floor: a 1x1 page admits 65,540 pixels; 64x1025 asks for 65,600.
    let e = page_size(&empty_region_segments(1, 1, 64, 1025, 1)).expect_err("overdraw-floor");
    assert!(msg(&e).contains("budget"), "{e}");
    assert!(msg(&e).contains("plus 65536 pixels"), "{e}");

    // One region of 67,108,865 pixels is refused from its header.
    assert!(dec_segment(&region_segment_data(5, 13421773)).is_err());
}

/// The other jaw: without it the pins above are satisfied by a budget of zero.
#[test]
fn resource_budgets_are_pinned_from_below() {
    for (name, w, h) in [
        ("400dpi-A4", 3307usize, 4677usize),
        ("400dpi-Letter", 3400, 4400),
        ("300dpi-A4", 2480, 3508),
    ] {
        let s = empty_region_segments(w, h, w as u32, h as u32, 1);
        assert_eq!(page_size(&s).expect(name), (w, h), "{name}");
    }
    // Both budgets to the unit: 8192x8192 under a page-covering region.
    assert_eq!(
        page_size(&empty_region_segments(8192, 8192, 8192, 8192, 1)).unwrap(),
        (8192, 8192)
    );
    // The memory budget from below, on a skinny page-covering region.
    page_size(&empty_region_segments(1, 8000000, 1, 8000000, 1)).unwrap();
    // Four layers over one page, exactly 4x.
    page_size(&empty_region_segments(2048, 2048, 4096, 1024, 4)).unwrap();
    // The floor under the ratio, on shapes a ratio alone gets wrong.
    for (pw, ph, rw, rh) in [
        (20, 20, 54, 44),
        (20, 20, 60, 50),
        (64, 64, 256, 256),
        (100, 100, 300, 300),
    ] {
        page_size(&empty_region_segments(pw, ph, rw, rh, 1))
            .unwrap_or_else(|e| panic!("{rw}x{rh} on {pw}x{ph}: {e}"));
    }
    // And the floor at its exact boundary.
    page_size(&empty_region_segments(1, 1, 4, 16385, 1)).unwrap();
}

/// The same two-segment stream as `empty_region_segments`, with the page association of each
/// segment given explicitly.
fn segments_on_pages(page_info_page: u8, region_page: u8) -> Vec<u8> {
    let pi = page_info(8, 8);
    let mut out = segment_header(0, SEG_TYPE_PAGE_INFORMATION, page_info_page, pi.len());
    out.extend_from_slice(&pi);
    let d = region_segment_data(1, 1);
    out.extend_from_slice(&segment_header(
        1,
        SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
        region_page,
        d.len(),
    ));
    out.extend_from_slice(&d);
    out
}

#[test]
fn decode_refuses_segments_on_different_pages() {
    for (name, a, b, want_err) in [
        ("both-on-page-1", 1u8, 1u8, false),
        ("both-on-page-2", 2, 2, false),
        ("region-on-another-page", 1, 2, true),
        ("page-information-on-another-page", 2, 1, true),
        ("region-associated-with-no-page", 1, 0, false),
        ("page-information-associated-with-no-page", 0, 1, false),
    ] {
        let r = dec(&segments_on_pages(a, b));
        if !want_err {
            let got = r.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((got.width, got.height), (8, 8), "{name}");
            continue;
        }
        let e = r.expect_err(name);
        assert!(msg(&e).contains(&format!("page {a}")), "{name}: {e}");
        assert!(msg(&e).contains(&format!("page {b}")), "{name}: {e}");
        assert!(!is_unsupported(&e), "{name}: {e}");
    }
}

#[test]
fn both_budgets_are_reachable() {
    // black_box keeps clippy from folding these into constant assertions.
    let bytes = std::hint::black_box(MAX_STREAM_BITMAP_BYTES);
    assert!(bytes * 8 > MAX_PAGE_PIXELS);
    assert!(bytes < MAX_PAGE_PIXELS);
}

#[test]
fn decode_refuses_a_page_with_no_height_or_width() {
    let s = empty_region_segments(64, 0, 1, 1, 1);
    assert!(msg(&page_size(&s).expect_err("height")).contains("height"));
    assert!(dec(&s).is_err());
    let s = empty_region_segments(0, 64, 1, 1, 1);
    assert!(msg(&page_size(&s).expect_err("width")).contains("width"));
    assert!(dec(&s).is_err());
}

/// Each header check that cannot be pinned as accept/reject is pinned by its diagnosis.
#[test]
fn header_refusals_are_attributed_to_the_field_that_broke_them() {
    // The memory budget inside the region loop.
    let e = page_size(&empty_region_segments(1, 1, 1, 8388609, 2)).expect_err("regions");
    assert!(msg(&e).contains("the stream's 2 regions"), "{e}");

    // A region dimension of 0, either one.
    let e = dec_segment(&region_segment_data(0, 4)).expect_err("0x4");
    assert!(msg(&e).starts_with("jbig2: region is 0x4"), "{e}");
    let e = dec_segment(&region_segment_data(4, 0)).expect_err("4x0");
    assert!(msg(&e).starts_with("jbig2: region is 4x0"), "{e}");

    // A stream with no segments at all.
    let e = page_size(&[]).expect_err("empty");
    assert!(msg(&e).contains("contains no segments"), "{e}");

    // Regions with no page information segment.
    let d = region_segment_data(8, 8);
    let mut s = segment_header(1, SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION, 1, d.len());
    s.extend_from_slice(&d);
    let e = page_size(&s).expect_err("no page info");
    assert!(msg(&e).contains("no page information segment"), "{e}");

    // The memory budget is charged before the overdraw rule.
    let s = empty_region_segments(1, 16000000, 8192, 8179, 1);
    assert_eq!(s.len(), 67);
    let e = page_size(&s).expect_err("memory before overdraw");
    assert!(
        msg(&e).contains("wants 16000000 page pixels and 24375296 bytes"),
        "{e}"
    );
    assert!(!msg(&e).contains("onto a page that can show"), "{e}");

    // Regions exactly at the memory budget are charged to the page.
    let e = page_size(&empty_region_segments(2048, 2048, 1, 16777216, 1)).expect_err("page");
    assert!(msg(&e).contains("and 17301504 bytes of bitmap"), "{e}");

    // An intermediate generic region is refused by name.
    let pi = page_info(8, 8);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    let d = region_segment_data(8, 8);
    s.extend_from_slice(&segment_header(
        1,
        SEG_TYPE_INTERMEDIATE_GENERIC_REGION,
        1,
        d.len(),
    ));
    s.extend_from_slice(&d);
    let e = page_size(&s).expect_err("intermediate");
    assert!(is_unsupported(&e));
    assert!(msg(&e).contains("intermediate generic region"), "{e}");

    // A long-form referred-to count larger than the stream.
    let s = [
        0x00, 0x00, 0x00, 0x00, 0x30, 0xE0, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
    ];
    let e = parse_segments(&s).expect_err("long count");
    assert!(
        msg(&e).contains("refers to 16777215 segments in a 11-byte stream"),
        "{e}"
    );
}

/// The unknown-data-length form is a legal encoding this package declines, not damage.
#[test]
fn unknown_segment_data_length_is_a_feature_not_damage() {
    let base = emb(&mut figure_h6());
    let s = set_u32(&base, 7, 0xFFFF_FFFF);
    let e = parse_segments(&s).expect_err("unknown length");
    assert!(is_unsupported(&e), "{e}");
    assert!(msg(&e).contains("unknown data length"), "{e}");
    assert!(is_unsupported(&dec(&s).unwrap_err()));
}

/// The budget loop passes over a region that does not parse and keeps charging the ones after.
#[test]
fn the_budget_charges_regions_after_one_that_does_not_parse() {
    let pi = page_info(64, 64);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    for (i, d) in [
        region_segment_data(64, 64),
        region_segment_data(0, 4),
        region_segment_data(8192, 8192),
    ]
    .iter()
    .enumerate()
    {
        s.extend_from_slice(&segment_header(
            i as u32 + 1,
            SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
            1,
            d.len(),
        ));
        s.extend_from_slice(d);
    }
    assert_eq!(s.len(), 141);
    let e = page_size(&s).expect_err("budget");
    assert!(msg(&e).contains("the stream's 3 regions"), "{e}");
    let before = decoded_pixels();
    assert!(dec(&s).is_err());
    assert_eq!(decoded_pixels() - before, 0);
}

#[test]
fn decode_refuses_a_stream_with_no_region_segment() {
    let pi = page_info(8, 8);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    assert_eq!(s.len(), 30);
    let e = page_size(&s).expect_err("no region");
    assert!(msg(&e).contains("no immediate region segment"), "{e}");
    assert!(!is_unsupported(&e));
    assert!(dec(&s).is_err());
}

#[test]
fn decode_refuses_a_second_page_information_segment() {
    let (first, second) = (page_info(8, 8), page_info(4096, 4096));
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, first.len());
    s.extend_from_slice(&first);
    s.extend_from_slice(&segment_header(
        1,
        SEG_TYPE_PAGE_INFORMATION,
        1,
        second.len(),
    ));
    s.extend_from_slice(&second);
    s.extend_from_slice(&segment_header(
        2,
        SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
        1,
        26,
    ));
    s.extend_from_slice(&region_segment_data(8, 8));
    assert_eq!(s.len(), 97);
    let e = page_size(&s).expect_err("two page infos");
    assert!(
        msg(&e).contains("more than one page information segment"),
        "{e}"
    );
    assert!(dec(&s).is_err());
}

/// A pattern whose pixel depends on both coordinates and on neither alone.
fn clip_pattern(w: usize, h: usize) -> Bitmap {
    let mut b = nb(w, h);
    for y in 0..h {
        for x in 0..w {
            if (x * 31 + y * 17) % 7 < 3 {
                put(&mut b, x, y, 1);
            }
        }
    }
    b
}

/// The other jaw of the overdraw rule: a legal document must open, clipped pixel for pixel.
#[test]
fn decode_admits_legal_clip_cases() {
    let cases = [
        (
            "20x20-under-54x44",
            20usize,
            20usize,
            54usize,
            44usize,
            0usize,
            0usize,
        ),
        ("20x20-under-60x50", 20, 20, 60, 50, 0, 0),
        ("64x64-under-256x256", 64, 64, 256, 256, 0, 0),
        ("100x100-under-300x300", 100, 100, 300, 300, 0, 0),
        ("64x64-under-54x44-at-40-50", 64, 64, 54, 44, 40, 50),
    ];
    for (name, pw, ph, rw, rh, x0, y0) in cases {
        let mut region = clip_pattern(rw, rh);
        let pi = page_info(pw, ph);
        let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
        s.extend_from_slice(&pi);
        let d = region_seg_data(&mut region, x0 as u32, y0 as u32, true);
        s.extend_from_slice(&segment_header(
            1,
            SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
            1,
            d.len(),
        ));
        s.extend_from_slice(&d);
        let got = dec(&s).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut want = nb(pw, ph);
        for y in 0..rh {
            for x in 0..rw {
                let (qx, qy) = (x + x0, y + y0);
                if qx < pw && qy < ph && px(&region, x, y) != 0 {
                    put(&mut want, qx, qy, 1);
                }
            }
        }
        assert_bitmaps_identical(name, &got, &want);
        assert_eq!(got.pix, want.pix, "{name}: dirty padding");
    }
}

/// Rule 5 from above: a stream of four times the cap is refused from parsing.
#[test]
fn segment_count_is_bounded() {
    let n = 4 * MAX_STREAM_SEGMENTS;
    let s = empty_region_segments(1024, 65536, 1, 1, n);
    assert_eq!(s.len(), 30 + n * 37);
    let e = page_size(&s).expect_err("4x the cap");
    assert!(msg(&e).contains("more than 65536 segments"), "{e}");
    assert!(!is_unsupported(&e));
}

/// Rule 5 from below, and at its boundary.
#[test]
fn segment_count_bound_admits_legitimate_streams() {
    // A real page striped one row per region round-trips.
    let (w, h) = (200usize, 300usize);
    let mut page = nb(w, h);
    for y in 0..h {
        put(&mut page, (y * 7) % w, y, 1);
        put(&mut page, 0, y, 1);
        put(&mut page, w - 1, y, 1);
    }
    let pi = page_info(w, h);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    for y in 0..h {
        let mut stripe = nb(w, 1);
        let st = page.stride;
        stripe.pix.copy_from_slice(&page.pix[y * st..(y + 1) * st]);
        let d = region_seg_data(&mut stripe, 0, y as u32, true);
        s.extend_from_slice(&segment_header(
            y as u32 + 1,
            SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
            1,
            d.len(),
        ));
        s.extend_from_slice(&d);
    }
    let got = dec(&s).unwrap();
    assert_eq!((got.width, got.height), (w, h));
    assert_eq!(got.pix, page.pix);

    // Exactly the cap is accepted, one past it is refused.
    let s = empty_region_segments(1024, 65536, 1, 1, MAX_STREAM_SEGMENTS - 1);
    assert_eq!(page_size(&s).unwrap(), (1024, 65536));
    let s = empty_region_segments(1024, 65536, 1, 1, MAX_STREAM_SEGMENTS);
    assert_eq!(s.len(), 30 + MAX_STREAM_SEGMENTS * 37);
    let e = page_size(&s).expect_err("one past the cap");
    assert!(msg(&e).contains("more than 65536 segments"), "{e}");

    // The encoder's own output is two segments.
    let parsed = parse_segments(&emb(&mut figure_h6())).unwrap().segs.len();
    assert_eq!(parsed, 2);
}

/// The measuring instrument itself: exact counts, so a counter stuck at 0 fails.
#[test]
fn decoded_pixels_counts_the_decoding_it_reports() {
    let (w, h) = (61usize, 37usize);
    let content = |w: usize, h: usize, seed: usize| {
        let mut b = nb(w, h);
        for y in 0..h {
            for x in 0..w {
                if (x * 7 + y * 13 + seed) % 11 < 4 {
                    put(&mut b, x, y, 1);
                }
            }
        }
        b
    };
    let stream = |regions: &mut [(Bitmap, usize)], page_h: usize, tpgdon: bool| {
        let pi = page_info(w, page_h);
        let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
        s.extend_from_slice(&pi);
        for (i, (b, y)) in regions.iter_mut().enumerate() {
            let d = region_seg_data(b, 0, *y as u32, tpgdon);
            s.extend_from_slice(&segment_header(
                i as u32 + 1,
                SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
                1,
                d.len(),
            ));
            s.extend_from_slice(&d);
        }
        s
    };

    // One region, TPGDON off: one decision per pixel.
    let page = content(w, h, 0);
    let s = stream(&mut [(page.clone(), 0)], h, false);
    let before = decoded_pixels();
    let got = dec(&s).unwrap();
    assert_eq!(got.pix, page.pix);
    assert_eq!(decoded_pixels() - before, (w * h) as i64);

    // TPGDON on charges the per-row SLTP bit. No row here repeats the row above.
    for y in 1..h {
        let st = page.stride;
        assert_ne!(
            page.pix[y * st..(y + 1) * st],
            page.pix[(y - 1) * st..y * st]
        );
    }
    let s = stream(&mut [(page.clone(), 0)], h, true);
    let before = decoded_pixels();
    assert_eq!(dec(&s).unwrap().pix, page.pix);
    assert_eq!(decoded_pixels() - before, (w * h + h) as i64);

    // Two regions sum.
    let s = stream(
        &mut [(content(w, h, 0), 0), (content(w, h, 5), h)],
        2 * h,
        false,
    );
    let before = decoded_pixels();
    let got = dec(&s).unwrap();
    assert_eq!((got.width, got.height), (w, 2 * h));
    assert_eq!(decoded_pixels() - before, (2 * w * h) as i64);
}

/// Cancellation and the per-bitmap `max_alloc` apply to decoding.
#[test]
fn decoding_honours_cancel_and_the_alloc_cap() {
    let stream = emb(&mut figure_h6());
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        decode_embedded_stream(&stream, &lim(), &cancelled),
        Err(Error::Cancelled)
    );
    let tiny = Limits {
        max_alloc: 16,
        ..Limits::default()
    };
    assert!(matches!(
        decode_embedded_stream(&stream, &tiny, &NO_CANCEL),
        Err(Error::Alloc { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// refs_test.go
// ---------------------------------------------------------------------------------------------

/// The three widths a referred-to segment number can take, and the long form of the count, read
/// back as VALUES rather than merely stepped over.
#[test]
fn parse_segments_retains_referred_to_numbers() {
    let t = SEG_TYPE_IMMEDIATE_TEXT_REGION;
    let cases = [
        (
            "one-byte numbers",
            HandHeader {
                num: 200,
                typ: t,
                refs: vec![1, 2, 250],
                ref_bytes: 1,
                data: vec![1],
                ..Default::default()
            },
        ),
        (
            "two-byte numbers",
            HandHeader {
                num: 60000,
                typ: t,
                refs: vec![1, 60000, 65535],
                ref_bytes: 2,
                data: vec![1],
                ..Default::default()
            },
        ),
        (
            "four-byte numbers",
            HandHeader {
                num: 70000,
                typ: t,
                refs: vec![1, 70000, 4_000_000_000],
                ref_bytes: 4,
                data: vec![1],
                ..Default::default()
            },
        ),
        (
            "long-form count",
            HandHeader {
                num: 70001,
                typ: t,
                refs: vec![9, 8, 7, 6, 5, 4, 3, 2, 1],
                ref_bytes: 4,
                long_count: true,
                retain_bytes: 2,
                data: vec![1],
                ..Default::default()
            },
        ),
    ];
    for (name, h) in cases {
        let bytes = h.bytes();
        let p = parse_segments(&bytes).expect(name);
        assert_eq!(p.segs.len(), 1, "{name}");
        // The order is the symbol numbering.
        assert_eq!(p.refs_of(&p.segs[0]), &h.refs[..], "{name}");
    }
}

/// A segment referring to nothing comes back with an empty list and not somebody else's.
#[test]
fn refs_of_is_per_segment_across_a_shared_slab() {
    let first = HandHeader {
        num: 1,
        typ: SEG_TYPE_SYMBOL_DICTIONARY,
        data: vec![1],
        ..Default::default()
    };
    let second = HandHeader {
        num: 2,
        typ: SEG_TYPE_IMMEDIATE_TEXT_REGION,
        refs: vec![1],
        ref_bytes: 1,
        data: vec![2],
        ..Default::default()
    };
    let third = HandHeader {
        num: 3,
        typ: SEG_TYPE_IMMEDIATE_TEXT_REGION,
        refs: vec![1, 2],
        ref_bytes: 1,
        data: vec![3],
        ..Default::default()
    };
    let s = cat(&[&first.bytes(), &second.bytes(), &third.bytes()]);
    let p = parse_segments(&s).unwrap();
    let want: [&[u32]; 3] = [&[], &[1], &[1, 2]];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(p.refs_of(&p.segs[i]), *w, "segment {}", p.segs[i].number);
    }
}

/// Rule 5's other half: eight references per segment need the long form, and the reference
/// bound is what refuses the stream.
#[test]
fn total_referred_to_numbers_are_bounded() {
    let mut s = Vec::new();
    for n in 0..(MAX_STREAM_REFS / 8 + 2) as u32 {
        let ref_bytes = if n > 65536 {
            4
        } else if n > 256 {
            2
        } else {
            1
        };
        let h = HandHeader {
            num: n,
            typ: SEG_TYPE_IMMEDIATE_TEXT_REGION,
            refs: vec![0; 8],
            ref_bytes,
            long_count: true,
            retain_bytes: 2,
            ..Default::default()
        };
        s.extend_from_slice(&h.bytes());
    }
    let e = parse_segments(&s).expect_err("too many refs");
    assert!(msg(&e).contains("refers to more than"), "{e}");
}

// ---------------------------------------------------------------------------------------------
// symbol_encode_test.go: a TEST-ONLY symbol-mode encoder
// ---------------------------------------------------------------------------------------------

/// `intLengths` from the other side: (value bits, offset) of each prefix length.
const INT_RANGES: [(u32, i64); 6] = [(2, 0), (4, 4), (6, 20), (8, 84), (12, 340), (32, 4436)];

/// The inverse of `decode_int`: one arithmetic integer written with the PREV register updated
/// exactly as the decoder updates it.
struct IntEnc {
    cx: Contexts,
}

impl IntEnc {
    fn new() -> IntEnc {
        IntEnc {
            cx: new_int_contexts(),
        }
    }

    fn bit(&mut self, e: &mut Encoder, prev: &mut usize, b: usize) {
        e.encode(&mut self.cx, *prev, b as u8).unwrap();
        *prev = if *prev < 256 {
            (*prev << 1) | b
        } else {
            (((*prev << 1) | b) & 511) | 256
        };
    }

    /// `oob` writes the out-of-band value, the negative zero of T.88 A.2 step 4.
    fn write(&mut self, e: &mut Encoder, v: i64, oob: bool) {
        let mut prev = 1usize;
        let (sign, mag) = if oob {
            (1usize, 0i64)
        } else if v < 0 {
            (1, -v)
        } else {
            (0, v)
        };
        self.bit(e, &mut prev, sign);
        let mut row = INT_RANGES[5];
        let mut sel = 5;
        for (i, r) in INT_RANGES[..5].iter().enumerate() {
            if mag < r.1 + (1 << r.0) {
                row = *r;
                sel = i;
                break;
            }
        }
        for _ in 0..sel {
            self.bit(e, &mut prev, 1);
        }
        if sel != 5 {
            self.bit(e, &mut prev, 0);
        }
        for i in (0..row.0).rev() {
            self.bit(e, &mut prev, (((mag - row.1) >> i) & 1) as usize);
        }
    }
}

fn write_iaid(e: &mut Encoder, cx: &mut [u8], v: usize, code_len: u32) {
    let mut prev = 1usize;
    for i in (0..code_len).rev() {
        let b = (v >> i) & 1;
        e.encode(cx, prev, b as u8).unwrap();
        prev = (prev << 1) | b;
    }
}

/// Codes `b` into an existing encoder and context array, which is what a symbol dictionary does
/// with every symbol it holds. Template 0, nominal AT, TPGDON off.
fn encode_generic_into(e: &mut Encoder, cx: &mut [u8], b: &Bitmap) {
    for y in 0..b.height as i64 {
        for x in 0..b.width as i64 {
            e.encode(cx, context_template0(b, x, y), b.get(x, y))
                .unwrap();
        }
    }
}

/// A symbol dictionary segment BODY holding `syms`, sorted by non-decreasing height. Every
/// symbol is exported.
fn build_symbol_dict(syms: &[Bitmap]) -> Vec<u8> {
    let mut e = new_enc();
    let mut gb: Contexts = vec![0; 1 << 16];
    let (mut iadh, mut iadw, mut iaex) = (IntEnc::new(), IntEnc::new(), IntEnc::new());
    let mut height = 0i64;
    let mut i = 0;
    while i < syms.len() {
        let mut j = i;
        while j < syms.len() && syms[j].height == syms[i].height {
            j += 1;
        }
        iadh.write(&mut e, syms[i].height as i64 - height, false);
        height = syms[i].height as i64;
        let mut width = 0i64;
        for s in &syms[i..j] {
            iadw.write(&mut e, s.width as i64 - width, false);
            width = s.width as i64;
            encode_generic_into(&mut e, &mut gb, s);
        }
        iadw.write(&mut e, 0, true); // OOB ends the height class
        i = j;
    }
    // Export flags: a run of none, then a run of everything.
    iaex.write(&mut e, 0, false);
    iaex.write(&mut e, syms.len() as i64, false);
    let coded = e.flush().unwrap();
    dict_body(syms.len() as u32, syms.len() as u32, &coded)
}

/// A symbol dictionary body: arithmetic, no refinement, template 0, nominal AT.
fn dict_body(num_ex: u32, num_new: u32, coded: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(18 + coded.len());
    d.extend_from_slice(&0u16.to_be_bytes());
    d.extend_from_slice(&NOMINAL_AT_TEMPLATE0);
    d.extend_from_slice(&num_ex.to_be_bytes());
    d.extend_from_slice(&num_new.to_be_bytes());
    d.extend_from_slice(coded);
    d
}

/// One symbol placed at a reference-corner coordinate.
#[derive(Clone, Copy)]
struct Instance {
    id: usize,
    s: i64,
    t: i64,
}

fn inst(id: usize, s: i64, t: i64) -> Instance {
    Instance { id, s, t }
}

/// The coordinate settings a text region can carry, which are precisely the ones no fixture
/// reaches.
#[derive(Clone, Copy, Default)]
struct TextParams {
    w: usize,
    h: usize,
    ref_corner: u32,
    transposed: bool,
    log_strips: u32,
    ds_offset: i64,
    comb_op: u8,
    def_pixel: u8,
}

fn corners(p: &TextParams) -> (bool, bool) {
    let right = p.ref_corner == REF_CORNER_TOP_RIGHT || p.ref_corner == REF_CORNER_BOTTOM_RIGHT;
    let bottom = p.ref_corner == REF_CORNER_BOTTOM_LEFT || p.ref_corner == REF_CORNER_BOTTOM_RIGHT;
    (right, bottom)
}

/// A text region segment BODY placing `insts`, sorted by strip and then by S. It writes the
/// coordinates the DECODER will read: S is coded as a delta from the previous instance's
/// advanced S, and the advance depends on the reference corner (T.88 6.4.5 step 3c run forwards).
fn build_text_region(p: &TextParams, syms: &[Bitmap], insts: &[Instance]) -> Vec<u8> {
    let strips = 1i64 << p.log_strips;
    let code_len = sym_code_len(syms.len());
    let mut e = new_enc();
    let (mut iadt, mut iafs, mut iads, mut iait) =
        (IntEnc::new(), IntEnc::new(), IntEnc::new(), IntEnc::new());
    let mut iaid: Contexts = vec![0; 1 << (code_len + 1)];

    iadt.write(&mut e, 0, false); // the initial strip T, which the decoder negates
    let (mut strip_t, mut first_s) = (0i64, 0i64);
    let (right, bottom) = corners(p);
    let mut i = 0;
    while i < insts.len() {
        let mut j = i;
        let t0 = insts[i].t / strips * strips;
        while j < insts.len() && insts[j].t / strips * strips == t0 {
            j += 1;
        }
        iadt.write(&mut e, (t0 - strip_t) / strips, false);
        strip_t = t0;

        let mut cur_s = 0i64;
        for (k, inn) in insts[i..j].iter().enumerate() {
            let sym = &syms[inn.id];
            let (w, h) = (sym.width as i64, sym.height as i64);
            // The leading edge: what CURS holds when the decoder reads this instance, before
            // its own pre-advance.
            let mut lead = inn.s;
            if !p.transposed && right {
                lead = inn.s - (w - 1);
            }
            if p.transposed && bottom {
                lead = inn.s - (h - 1);
            }
            if k == 0 {
                iafs.write(&mut e, lead - first_s, false);
                first_s = lead;
            } else {
                iads.write(&mut e, lead - cur_s - p.ds_offset, false);
            }
            cur_s = lead;
            if strips != 1 {
                iait.write(&mut e, inn.t - strip_t, false);
            }
            write_iaid(&mut e, &mut iaid, inn.id, code_len);
            // The post-advance the decoder applies before the next delta.
            cur_s += if p.transposed { h - 1 } else { w - 1 };
        }
        iads.write(&mut e, 0, true); // OOB ends the strip
        i = j;
    }
    let coded = e.flush().unwrap();

    let mut flags = 0u16;
    flags |= (p.log_strips as u16) << 2;
    flags |= (p.ref_corner as u16) << 4;
    if p.transposed {
        flags |= 1 << 6;
    }
    flags |= u16::from(p.comb_op) << 7;
    flags |= u16::from(p.def_pixel) << 9;
    flags |= ((p.ds_offset & 0x1F) as u16) << 10;

    let mut d = Vec::with_capacity(23 + coded.len());
    d.extend_from_slice(&(p.w as u32).to_be_bytes());
    d.extend_from_slice(&(p.h as u32).to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.push(0x00); // external combination operator OR
    d.extend_from_slice(&flags.to_be_bytes());
    d.extend_from_slice(&(insts.len() as u32).to_be_bytes());
    d.extend_from_slice(&coded);
    d
}

/// A complete embedded stream: a page information segment, a symbol dictionary, and a text
/// region referring to it.
fn symbol_stream(page_w: usize, page_h: usize, dict: &[u8], text: &[u8]) -> Vec<u8> {
    let pi = page_info(page_w, page_h);
    let mut out = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    out.extend_from_slice(&pi);
    out.extend_from_slice(&segment_header(
        1,
        SEG_TYPE_SYMBOL_DICTIONARY,
        1,
        dict.len(),
    ));
    out.extend_from_slice(dict);
    // A referring header, which `segment_header` does not write: one referred-to segment, its
    // number one byte wide because this segment's number is 2.
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&[SEG_TYPE_IMMEDIATE_TEXT_REGION, 0x20, 0x01, 0x01]);
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text);
    out
}

/// A small asymmetric bitmap: different under a horizontal flip, a vertical flip and a
/// transpose.
fn glyph(w: usize, h: usize, seed: usize) -> Bitmap {
    let mut b = nb(w, h);
    let mut s = (seed as i64).wrapping_mul(2654435761).wrapping_add(1) as u32;
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            if x == 0 || y == 0 || (x + 2 * y + seed).is_multiple_of(5) || s >> 30 == 0 {
                put(&mut b, x, y, 1);
            }
        }
    }
    b
}

/// Paints what the placements mean, independently of the decoder: the reference corner rule of
/// T.88 6.4.5 step 3c ix applied directly, with no delta arithmetic anywhere near it.
fn draw_expected(p: &TextParams, syms: &[Bitmap], insts: &[Instance]) -> Bitmap {
    let mut want = nb(p.w, p.h);
    if p.def_pixel == 1 {
        want.pix.fill(0xFF);
        mask_padding(&mut want);
    }
    let (right, bottom) = corners(p);
    for inn in insts {
        let sym = &syms[inn.id];
        let (w, h) = (sym.width as i64, sym.height as i64);
        let (mut x0, mut y0) = if p.transposed {
            (inn.t, inn.s)
        } else {
            (inn.s, inn.t)
        };
        if right {
            x0 -= w - 1;
        }
        if bottom {
            y0 -= h - 1;
        }
        super::segment_decode::composite(&mut want, sym, x0, y0, p.comb_op);
    }
    want
}

fn test_symbols() -> Vec<Bitmap> {
    vec![
        glyph(5, 7, 1),
        glyph(9, 7, 2),
        glyph(4, 11, 3),
        glyph(12, 11, 4),
    ]
}

fn corner_name(c: u32) -> &'static str {
    ["bottomleft", "topleft", "bottomright", "topright"][c as usize]
}

const ALL_CORNERS: [u32; 4] = [
    REF_CORNER_BOTTOM_LEFT,
    REF_CORNER_TOP_LEFT,
    REF_CORNER_BOTTOM_RIGHT,
    REF_CORNER_TOP_RIGHT,
];

/// Every reference corner, both axes, and multi-row strips. The expected raster is painted by
/// `draw_expected`, which knows nothing of deltas or strips.
#[test]
fn text_region_coordinates_across_every_corner_and_axis() {
    let syms = test_symbols();
    let insts = [
        inst(0, 20, 14),
        inst(1, 30, 14),
        inst(3, 46, 14),
        inst(2, 22, 40),
        inst(0, 30, 40),
        inst(1, 41, 41),
        inst(3, 24, 70),
        inst(2, 44, 71),
    ];
    for corner in ALL_CORNERS {
        for transposed in [false, true] {
            for log_strips in [0u32, 2] {
                let p = TextParams {
                    w: 120,
                    h: 100,
                    ref_corner: corner,
                    transposed,
                    log_strips,
                    ..TextParams::default()
                };
                let name = format!(
                    "{} transposed={transposed} logstrips={log_strips}",
                    corner_name(corner)
                );
                let s = symbol_stream(
                    p.w,
                    p.h,
                    &build_symbol_dict(&syms),
                    &build_text_region(&p, &syms, &insts),
                );
                let got = dec(&s).unwrap_or_else(|e| panic!("{name}: {e}"));
                let want = draw_expected(&p, &syms, &insts);
                assert_eq!(
                    got.pix, want.pix,
                    "{name}: placeSymbol disagrees with T.88 6.4.5 step 3c"
                );
            }
        }
    }
}

/// SBDSOFFSET is a signed five-bit field, so an encoder writing -1 writes 31.
#[test]
fn text_region_applies_a_signed_ds_offset() {
    let syms = test_symbols();
    let insts = [inst(0, 10, 12), inst(1, 24, 12), inst(2, 40, 12)];
    for off in [-16i64, -1, 0, 1, 15] {
        let p = TextParams {
            w: 120,
            h: 40,
            ds_offset: off,
            ..TextParams::default()
        };
        let s = symbol_stream(
            p.w,
            p.h,
            &build_symbol_dict(&syms),
            &build_text_region(&p, &syms, &insts),
        );
        let got = dec(&s).unwrap();
        assert_eq!(
            got.pix,
            draw_expected(&p, &syms, &insts).pix,
            "SBDSOFFSET {off}"
        );
    }
}

/// A symbol dictionary with SEVERAL HEIGHT CLASSES: the height delta is cumulative across
/// classes and the width delta resets inside each one.
#[test]
fn symbol_dictionary_height_classes() {
    let syms = [
        glyph(3, 4, 7),
        glyph(9, 4, 8),
        glyph(5, 9, 9),
        glyph(6, 16, 10),
        glyph(2, 16, 11),
    ];
    let insts = [
        inst(0, 4, 20),
        inst(1, 10, 20),
        inst(2, 22, 20),
        inst(3, 30, 20),
        inst(4, 40, 20),
    ];
    let p = TextParams {
        w: 60,
        h: 30,
        ..TextParams::default()
    };
    let s = symbol_stream(
        p.w,
        p.h,
        &build_symbol_dict(&syms),
        &build_text_region(&p, &syms, &insts),
    );
    assert_eq!(dec(&s).unwrap().pix, draw_expected(&p, &syms, &insts).pix);
}

/// SBDEFPIXEL and SBCOMBOP: what a region looks like before any symbol lands on it, and how
/// each symbol merges with what is there. Overlapping placements, so an operator other than OR
/// shows. (Go also asks jbig2dec; that part is left out.)
#[test]
fn text_region_default_pixel_and_combination_operator() {
    let syms = test_symbols();
    let insts = [
        inst(0, 20, 20),
        inst(1, 23, 20),
        inst(2, 26, 24),
        inst(3, 24, 26),
    ];
    for def_pixel in [0u8, 1] {
        for comb_op in [0u8, 1, 2, 3] {
            let p = TextParams {
                w: 80,
                h: 60,
                def_pixel,
                comb_op,
                ..TextParams::default()
            };
            let s = symbol_stream(
                p.w,
                p.h,
                &build_symbol_dict(&syms),
                &build_text_region(&p, &syms, &insts),
            );
            let got = dec(&s).unwrap();
            assert_eq!(
                got.pix,
                draw_expected(&p, &syms, &insts).pix,
                "SBDEFPIXEL {def_pixel} SBCOMBOP {comb_op}"
            );
        }
    }
}

/// `place_symbol` returns the advanced S for each corner/axis (the post-advance of step 3c).
#[test]
fn place_symbol_returns_the_advanced_s() {
    let sym = glyph(5, 7, 1);
    for (corner, transposed, want) in [
        (REF_CORNER_TOP_LEFT, false, 14),
        (REF_CORNER_TOP_RIGHT, false, 14),
        (REF_CORNER_TOP_LEFT, true, 16),
        (REF_CORNER_BOTTOM_LEFT, true, 16),
    ] {
        let mut page = nb(40, 40);
        assert_eq!(
            place_symbol(&mut page, &sym, 10, 12, corner, transposed, 0),
            want
        );
    }
}

// ---------------------------------------------------------------------------------------------
// symbol_test.go: the jbig2enc fixture and its jbig2dec reference decode
// ---------------------------------------------------------------------------------------------

/// A symbol dictionary in a globals stream and a text region in a page stream must decode to
/// exactly what jbig2dec decodes them to (`symbol-page.pbm`).
#[test]
fn decode_symbol_mode_matches_jbig2dec() {
    let globals = read_fixture("symbol-globals.jb2");
    let page = read_fixture("symbol-page.jb2");
    let want = read_pbm(&read_fixture("symbol-page.pbm"));
    let got = decode_embedded_stream_with_globals(&globals, &page, &lim(), &NO_CANCEL).unwrap();
    assert_eq!((got.width, got.height), (want.width, want.height));
    assert_eq!(
        got.pix,
        want.pix,
        "{} ink vs {} ink",
        ink_count(&got),
        ink_count(&want)
    );
    // The same page size without decoding.
    assert_eq!(
        super::page_size_with_globals(&globals, &page, &lim()).unwrap(),
        (want.width, want.height)
    );
}

/// Without the globals the page stream must FAIL, not decode to a blank page.
#[test]
fn text_region_without_its_globals_is_an_error_not_a_blank_page() {
    assert!(dec(&read_fixture("symbol-page.jb2")).is_err());
}

/// The legal symbol-mode variants byblos declines, mutated into the fixture's own headers.
#[test]
fn symbol_mode_refuses_the_variants_it_does_not_code() {
    let globals = read_fixture("symbol-globals.jb2");
    let page = read_fixture("symbol-page.jb2");
    assert_eq!((globals[11], globals[12]), (0, 0));
    // 11 bytes of page information header, 19 of its data, 11 of the text region header, then
    // 17 of region information: the flags word is at 58 and 59.
    let text_flags_at = 11 + 19 + 12 + 17 + 1;
    assert_eq!((page[text_flags_at - 1], page[text_flags_at]), (0, 0));

    let cases = [
        ("sdhuff", set_byte(&globals, 12, 0x01), page.clone()),
        ("sdrefagg", set_byte(&globals, 12, 0x02), page.clone()),
        (
            "sbhuff",
            globals.clone(),
            set_byte(&page, text_flags_at, 0x01),
        ),
        (
            "sbrefine",
            globals.clone(),
            set_byte(&page, text_flags_at, 0x02),
        ),
    ];
    for (name, g, p) in cases {
        let e = decode_embedded_stream_with_globals(&g, &p, &lim(), &NO_CANCEL).expect_err(name);
        assert!(is_unsupported(&e), "{name}: {e}");
    }
}

/// A text region whose referred-to dictionary is not in the stream is refused, not decoded
/// against an empty symbol list.
#[test]
fn text_region_refers_to_a_segment_that_is_not_there() {
    let syms = test_symbols();
    let p = TextParams {
        w: 120,
        h: 100,
        ..TextParams::default()
    };
    let text = build_text_region(&p, &syms, &[inst(0, 20, 14)]);
    // The stream `symbol_stream` builds, with the dictionary segment left out.
    let pi = page_info(p.w, p.h);
    let mut s = segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len());
    s.extend_from_slice(&pi);
    s.extend_from_slice(&2u32.to_be_bytes());
    s.extend_from_slice(&[SEG_TYPE_IMMEDIATE_TEXT_REGION, 0x20, 0x01, 0x01]);
    s.extend_from_slice(&(text.len() as u32).to_be_bytes());
    s.extend_from_slice(&text);
    let e = dec(&s).expect_err("no dictionary");
    assert!(msg(&e).contains("no symbol dictionary"), "{e}");
}

// ---------------------------------------------------------------------------------------------
// symbol_hang_test.go
// ---------------------------------------------------------------------------------------------

/// The smallest stream that reaches the height class loop: a dictionary declaring one new
/// symbol whose coded data is a height class delta followed by OOB, and then nothing. The
/// decoder reads 1 bits from there, so this asserts termination and the empty-class refusal.
fn hang_stream() -> Vec<u8> {
    let mut e = new_enc();
    let (mut iadh, mut iadw) = (IntEnc::new(), IntEnc::new());
    iadh.write(&mut e, 8, false);
    iadw.write(&mut e, 0, true); // OOB: the class ends having coded no symbol
    let d = dict_body(1, 1, &e.flush().unwrap());
    let syms = [glyph(5, 7, 1)];
    let text = build_text_region(
        &TextParams {
            w: 60,
            h: 40,
            ..TextParams::default()
        },
        &syms,
        &[inst(0, 10, 20)],
    );
    symbol_stream(60, 40, &d, &text)
}

#[test]
fn symbol_dictionary_with_no_symbols_in_a_height_class_terminates() {
    let e = dec(&hang_stream()).expect_err("empty height class");
    assert!(msg(&e).contains("containing no symbols"), "{e}");
}

/// PageSize reads headers only, so it never enters the loop at all.
#[test]
fn symbol_dictionary_hang_is_not_reachable_through_page_size() {
    assert_eq!(page_size(&hang_stream()).unwrap(), (60, 40));
}

// ---------------------------------------------------------------------------------------------
// budget_symbol_test.go
// ---------------------------------------------------------------------------------------------

/// The counter against work it can see independently: the symbols in the dictionary and the
/// instances the text region places are both countable, so the expected total is arithmetic.
#[test]
fn decoded_pixels_counts_symbol_work() {
    let syms = test_symbols();
    let insts = [
        inst(0, 20, 14),
        inst(1, 34, 14),
        inst(3, 60, 14),
        inst(2, 22, 40),
        inst(0, 34, 40),
    ];
    let p = TextParams {
        w: 120,
        h: 100,
        ..TextParams::default()
    };
    let s = symbol_stream(
        p.w,
        p.h,
        &build_symbol_dict(&syms),
        &build_text_region(&p, &syms, &insts),
    );
    let area = |b: &Bitmap| (b.width * b.height) as i64;
    let want_symbol: i64 = syms.iter().map(area).sum();
    let want_placement: i64 = insts.iter().map(|i| area(&syms[i.id])).sum();
    assert_ne!(
        want_symbol, want_placement,
        "the assertion could not tell one half from the other"
    );
    let before = decoded_pixels();
    dec(&s).unwrap();
    assert_eq!(decoded_pixels() - before, want_symbol + want_placement);
}

/// Placement is charged per instance, not per symbol.
#[test]
fn placement_is_charged_per_instance_not_per_symbol() {
    let syms = [glyph(8, 8, 1)];
    let one = [inst(0, 10, 20)];
    let many = [
        inst(0, 10, 20),
        inst(0, 30, 20),
        inst(0, 50, 20),
        inst(0, 70, 20),
    ];
    let p = TextParams {
        w: 120,
        h: 40,
        ..TextParams::default()
    };
    let cost = |insts: &[Instance]| {
        let s = symbol_stream(
            p.w,
            p.h,
            &build_symbol_dict(&syms),
            &build_text_region(&p, &syms, insts),
        );
        let before = decoded_pixels();
        dec(&s).unwrap();
        decoded_pixels() - before
    };
    assert_eq!(cost(&many) - cost(&one), 3 * 8 * 8);
}

/// The two HEADER-time symbol rules are refused before a coded bit is read.
#[test]
fn symbol_header_counts_are_bounded_before_decoding() {
    let syms = test_symbols();
    let p = TextParams {
        w: 120,
        h: 100,
        ..TextParams::default()
    };
    let dict = build_symbol_dict(&syms);
    let text = build_text_region(&p, &syms, &[inst(0, 20, 14), inst(1, 34, 14)]);
    // The dictionary body is a 2-byte flags word, an 8-byte AT field, then SDNUMEXSYMS and
    // SDNUMNEWSYMS; the text region body is 17 bytes of region information, a 2-byte flags
    // word, then SBNUMINSTANCES.
    assert_eq!(
        u32::from_be_bytes([dict[14], dict[15], dict[16], dict[17]]),
        syms.len() as u32
    );
    assert_eq!(
        u32::from_be_bytes([text[19], text[20], text[21], text[22]]),
        2
    );
    for (name, want, s) in [
        (
            "symbols",
            "more than 65536 symbols",
            symbol_stream(p.w, p.h, &set_u32(&dict, 14, 1 << 31), &text),
        ),
        (
            "instances",
            "more than 2097152 symbol instances",
            symbol_stream(p.w, p.h, &dict, &set_u32(&text, 19, 1 << 31)),
        ),
    ] {
        let before = decoded_pixels();
        let e = dec(&s).expect_err(name);
        assert!(msg(&e).contains(want), "{name}: {e}");
        assert_eq!(decoded_pixels() - before, 0, "{name}");
    }
}

/// The DECODE-time rule: a symbol's size is coded inside the data, so a dictionary declaring one
/// symbol can ask for a four-billion-pixel bitmap, and the charge is made before it exists.
#[test]
fn oversized_symbol_is_refused_before_it_is_allocated() {
    let mut e = new_enc();
    let (mut iadh, mut iadw) = (IntEnc::new(), IntEnc::new());
    iadh.write(&mut e, 1 << 16, false); // a 65,536-row height class
    iadw.write(&mut e, 1 << 16, false); // 65,536 wide: 4,294,967,296 pixels
    let d = dict_body(1, 1, &e.flush().unwrap());
    let syms = [glyph(5, 7, 1)];
    let text = build_text_region(
        &TextParams {
            w: 60,
            h: 40,
            ..TextParams::default()
        },
        &syms,
        &[inst(0, 10, 20)],
    );
    let before = decoded_pixels();
    let err = dec(&symbol_stream(60, 40, &d, &text)).expect_err("4,294,967,296 pixels");
    assert!(msg(&err).contains("the budget for one stream is"), "{err}");
    assert_eq!(decoded_pixels() - before, 0);
}

/// The PLACEMENT rule: instance count and symbol size are both modest, their product is not.
#[test]
fn placement_budget_refuses_drawing_far_more_than_the_page_can_show() {
    let syms = [glyph(60, 60, 1)];
    // A 100x100 page admits 4*10,000 + 65,536 = 105,536 placement pixels: 29 instances of a
    // 60x60 symbol. Forty is past it.
    let insts = [inst(0, 10, 20); 40];
    let p = TextParams {
        w: 100,
        h: 100,
        ..TextParams::default()
    };
    let dict = build_symbol_dict(&syms);
    let e = dec(&symbol_stream(
        p.w,
        p.h,
        &dict,
        &build_text_region(&p, &syms, &insts),
    ))
    .expect_err("40 instances");
    assert!(
        msg(&e).contains("pixels of symbol onto a page that can show"),
        "{e}"
    );
    // The same shape inside the budget still decodes: a bound, not a ban.
    dec(&symbol_stream(
        p.w,
        p.h,
        &dict,
        &build_text_region(&p, &syms, &insts[..29]),
    ))
    .unwrap();
}

/// A legitimate dense page is not refused: symbol boxes covering the page about once over.
#[test]
fn placement_budget_admits_a_page_fully_covered_by_glyph_boxes() {
    let syms = [glyph(10, 14, 1)];
    let mut insts = Vec::new();
    let mut y = 14;
    while y < 200 {
        let mut x = 0;
        while x + 10 <= 200 {
            insts.push(inst(0, x, y));
            x += 10;
        }
        y += 14;
    }
    let p = TextParams {
        w: 200,
        h: 200,
        ..TextParams::default()
    };
    let s = symbol_stream(
        p.w,
        p.h,
        &build_symbol_dict(&syms),
        &build_text_region(&p, &syms, &insts),
    );
    dec(&s).unwrap_or_else(|e| panic!("{} glyph boxes refused: {e}", insts.len()));
}

/// The integer decoder against its own writer, across every prefix row, both signs and OOB.
#[test]
fn arithmetic_integers_round_trip() {
    let values = [
        0i64,
        1,
        -1,
        3,
        4,
        -4,
        19,
        20,
        83,
        84,
        339,
        340,
        4435,
        4436,
        -4436,
        100_000,
        -100_000,
        1 << 31,
    ];
    let mut e = new_enc();
    let mut w = IntEnc::new();
    for v in values {
        w.write(&mut e, v, false);
    }
    w.write(&mut e, 0, true);
    let mut iaid_cx: Contexts = vec![0; 1 << 5];
    for v in [0usize, 5, 31, 17] {
        write_iaid(&mut e, &mut iaid_cx, v, 4);
    }
    let coded = e.flush().unwrap();

    let mut d = Decoder::new(&coded);
    let mut cx = new_int_contexts();
    for v in values {
        assert_eq!(decode_int(&mut d, &mut cx), Some(v), "{v}");
    }
    assert_eq!(decode_int(&mut d, &mut cx), None);
    let mut iaid_cx: Contexts = vec![0; 1 << 5];
    // Codes of length 4 carry values 0..15; 31 and 17 above wrapped into the 4-bit range.
    for want in [0usize, 5, 15, 1] {
        assert_eq!(decode_iaid(&mut d, &mut iaid_cx, 4), want);
    }
}
