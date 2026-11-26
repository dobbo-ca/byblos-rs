//! Tests of `doc`, `limits`, `catalog`, `encryptinfo` and `properties`: ports of
//! `internal/pdfdoc/{pdfdoc,limits,contents}_test.go` and `inspect_encrypted_test.go` that run
//! without external tools.
//!
//! What is not ported:
//!
//! * The tests that read a named document of Go's `internal/corpus` generator are `#[ignore]`d
//!   and read `tests/data/corpus/<name>.pdf`. The names: `scan`, `scan-rotated`, `mixed`,
//!   `indirect-kids`, `scan-in-form`, `overlay-text`, `dup-raster`, `jbig2`, `stacked-alpha`,
//!   `stacked-smask`, `born-digital`, `blank-page`, `malformed`. The oracle's `-gen` makes them.
//! * `TestCatchPanic*` test Go's `recover`; Rust has no panic to catch, and the mapping of
//!   `pdf::Error::Panic` is tested through [`seam`].
//! * The `limits_test.go` tests that run on a memory pool or measure Go's `TotalAlloc`
//!   (`TestOpenBudgeted*`, `TestPaddedFiles*`, the `OpenBudgeted` budget cases) have no Rust
//!   equivalent; `max_alloc` is tested by the crate's `Limits` tests. The object-stream-count
//!   tests (`TestObjectStreamCountIsHeldByPdfcpu`, `TestCumulative*`, `TestOneHugeObject*`,
//!   `TestRepeated*`, `TestHostileScalars*`, `TestSelfDrawingForm*`) test pdf-go's own limits
//!   through generated multi-megabyte inputs and belong to the parity run of pdf-rs.
//! * `inspect_encrypted_test.go` needs `InspectDocument` (wave 3) and the encrypted fixtures of
//!   `tests/data/encrypt-census`.
//! * `TestRawImageFlateWrappedDCTOfATinyImage` needs Go's `image/jpeg` encoder.
//! * `contract_test.go`, `straighten_test.go` and the others belong to the files that own them.

use super::annots::build_pdf;
use super::doc::seam;
use super::*;
use crate::content::{self, IDENTITY};
use crate::error::Error;
use crate::limits::Limits;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

fn open_default(data: &[u8]) -> Doc {
    match open(data, &Limits::default(), &never()) {
        Ok(d) => d,
        Err(e) => panic!("open: {e}"),
    }
}

fn pdf_of(bodies: &[&str]) -> Vec<u8> {
    build_pdf(&bodies.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

/// `build_pdf` for bodies that hold binary stream data.
fn pdf_of_bytes(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, b) in bodies.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        buf.extend_from_slice(b);
        buf.extend_from_slice(b"\nendobj\n");
    }
    let start = buf.len();
    buf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
    );
    for off in offsets {
        buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            bodies.len() + 1,
            start
        )
        .as_bytes(),
    );
    buf
}

/// An object body: a dictionary (without its `/Length`) and a stream of `data`.
fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut b = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    b.extend_from_slice(data);
    b.extend_from_slice(b"\nendstream");
    b
}

/// A zlib stream of `data` as stored blocks (at most 65535 bytes), and its Adler-32.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= 0xffff);
    let mut z = vec![0x78, 0x01, 0x01];
    let n = data.len() as u16;
    z.extend_from_slice(&n.to_le_bytes());
    z.extend_from_slice(&(!n).to_le_bytes());
    z.extend_from_slice(data);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

/// A zlib stream (fixed Huffman) of `n` zero bytes: a literal, then matches of distance 1.
/// 258 bytes cost 13 bits, so a bomb of megabytes is a few kilobytes.
fn zlib_zeros(n: usize) -> Vec<u8> {
    struct Bits {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }
    impl Bits {
        /// `n` bits, least significant first.
        fn put(&mut self, v: u32, n: u32) {
            self.acc |= v << self.nbits;
            self.nbits += n;
            while self.nbits >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.nbits -= 8;
            }
        }
        /// A Huffman code of `n` bits, most significant bit first.
        fn code(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.put((v >> i) & 1, 1);
            }
        }
    }
    const BASE: [u32; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const EXTRA: [u32; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    let mut w = Bits {
        out: vec![0x78, 0x01],
        acc: 0,
        nbits: 0,
    };
    w.put(1, 1); // final block
    w.put(1, 2); // fixed Huffman
    if n > 0 {
        w.code(0x30, 8); // literal 0
    }
    let mut r = n.saturating_sub(1);
    while r > 0 {
        if r < 3 {
            w.code(0x30, 8);
            r -= 1;
            continue;
        }
        let mut l = r.min(258);
        if r - l == 1 || r - l == 2 {
            l -= 3;
        }
        let i = BASE.iter().rposition(|&b| (b as usize) <= l).unwrap_or(0);
        let sym = 257 + i as u32;
        if sym < 280 {
            w.code(sym - 256, 7);
        } else {
            w.code(0xC0 + sym - 280, 8);
        }
        w.put(l as u32 - BASE[i], EXTRA[i]);
        w.code(0, 5); // distance 1
        r -= l;
    }
    w.code(0, 7); // end of block
    if w.nbits > 0 {
        w.out.push(w.acc as u8);
    }
    let mut out = w.out;
    // The Adler-32 of n zero bytes: a stays 1, b is n mod 65521.
    let b = (n % 65521) as u32;
    out.extend_from_slice(&((b << 16) | 1).to_be_bytes());
    out
}

#[test]
fn zlib_zeros_inflates_to_zeros() {
    for n in [0usize, 1, 2, 3, 100, 258, 259, 260, 1000, 100_000] {
        let z = zlib_zeros(n);
        let got = deflate::zlib::decode(&z, &deflate::Limits::default(), &never());
        assert_eq!(got.map(|v| v.len()), Ok(n), "n = {n}");
    }
}

// ---------------------------------------------------------------------------------------
// limits.go

#[test]
fn trailer_size_cases() {
    let classic = "%PDF-1.4\n1 0 obj\n<< >>\nendobj\nxref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 424242 /Root 1 0 R >>\nstartxref\n20\n%%EOF\n".to_string();
    let xref_stream = |pad: usize| {
        let head = format!("%PDF-1.5\n{}", "%".repeat(pad));
        format!(
            "{head}\n7 0 obj\n<< /Type /XRef /Size 777777 /W [1 2 1] /Length 0 >>\nstream\n\nendstream\nendobj\nstartxref\n{}\n%%EOF\n",
            head.len() + 1
        )
    };
    let incremental = format!(
        "{classic}xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 424243 /Prev 20 >>\nstartxref\n99\n%%EOF\n"
    );
    // Linearized: startxref points at the first-page xref at the top; the main trailer (small
    // /Size) is last.
    let first = "%PDF-1.4\n1 0 obj\n<< >>\nendobj\n";
    let lin = format!(
        "{first}xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 77 /Prev 99 >>\nstartxref\n0\n%%EOF\n"
    );
    let linearized = format!(
        "{lin}xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 2 >>\nstartxref\n{}\n%%EOF\n",
        first.len()
    );
    // Leading junk shifts every offset the file states.
    let junk_xref = "%PDF-1.5\n7 0 obj\n<< /Type /XRef /Size 888 /W [1 2 1] /Length 0 >>\nstream\n\nendstream\nendobj\nstartxref\n9\n%%EOF\n";
    let cases = [
        ("classic", classic, 424242, true),
        (
            "last trailer of an incremental update wins",
            incremental,
            424243,
            true,
        ),
        ("xref stream in the tail", xref_stream(10), 777777, true),
        (
            "xref stream before the tail",
            xref_stream(200 << 10),
            777777,
            true,
        ),
        (
            "linearized classic: first-page trailer, not the last",
            linearized,
            77,
            true,
        ),
        (
            "xref stream after leading junk",
            format!("{}{junk_xref}", " ".repeat(20)),
            888,
            true,
        ),
        (
            "xref stream after 70 KiB of leading junk",
            format!("{}{junk_xref}", " ".repeat(70 << 10)),
            888,
            true,
        ),
        ("no startxref", "%PDF-1.4\n garbage".to_string(), 0, false),
        ("empty", String::new(), 0, false),
    ];
    for (name, pdf, want, ok) in cases {
        assert_eq!(trailer_size(pdf.as_bytes()), (want, ok), "{name}");
    }
}

// A /Size past i64 is Go's `strconv.ParseInt` range error: the maximum, and not ok.
#[test]
fn trailer_size_past_i64_is_the_maximum() {
    let pdf = "%PDF-1.4\nxref\ntrailer\n<< /Size 99999999999999999999 >>\nstartxref\n9\n%%EOF\n";
    assert_eq!(trailer_size(pdf.as_bytes()), (i64::MAX, false));
}

// pdf-go reads "<k> 5 0 obj", "<k>.5 0 obj" and "<k>0000000005 0 obj" as object k, not 5, so the
// bound counts tokens, not numbers.
#[test]
fn object_bound_counts_every_header_form() {
    for form in [
        "{k} 5 0 obj",
        "{k}.5 0 obj",
        "{k}x5 0 obj",
        "{k}0000000005 0 obj",
    ] {
        let mut sb = String::from("%PDF-1.4\n");
        for k in 1..=1000 {
            sb.push_str(&form.replace("{k}", &k.to_string()));
            sb.push_str("\n<< >>\nendobj\n");
        }
        assert_eq!(object_bound(sb.as_bytes()), 1000, "{form}");
    }
}

// A match that straddles a chunk boundary counts once.
#[test]
fn object_bound_across_chunks() {
    for pad in [
        (64 << 10) - 40,
        (64 << 10) - 3,
        64 << 10,
        (128 << 10) - 300,
        (128 << 10) + 7,
    ] {
        let pdf = format!(
            "{}7 0 obj\n<< /Type /ObjStm /N 4 /First 9 >>\nstream\nendstream\nendobj\n",
            " ".repeat(pad)
        );
        assert_eq!(object_bound(pdf.as_bytes()), 1, "pad {pad}");
    }
}

#[test]
fn image_decode_bytes_matches_go() {
    // 8-bit gray 10 x 10: a row is 10 bytes and a tag byte.
    assert_eq!(image_decode_bytes(10, 10, 8, 1, 1), 110);
    // bpc 0 reads as 8.
    assert_eq!(image_decode_bytes(10, 10, 0, 1, 1), 110);
    // 1-bit: ceil(10 / 8) = 2 bytes and a tag byte.
    assert_eq!(image_decode_bytes(10, 10, 1, 1, 1), 30);
    // A filter chain gets twice that and 64 bytes.
    assert_eq!(image_decode_bytes(10, 10, 8, 1, 2), 2 * 110 + 64);
    // Overflow wraps as Go's int64 does: row wraps to -2, and -2 * MAX wraps to 2.
    assert_eq!(image_decode_bytes(i64::MAX, i64::MAX, 8, 4, 1), 2);
    // A wrapped row alone is <= 0, which a caller refuses.
    assert!(image_decode_bytes(i64::MAX, 1, 8, 4, 1) <= 0);
}

// The caps are pdf-go's, not pdf-rs's 512 MiB and 10M objects.
#[test]
fn default_config_carries_the_limits() {
    let c = doc::default_config(&Limits::default());
    assert_eq!(c.limits.max_decode_bytes, 64 << 20);
    assert_eq!(c.limits.max_object_count, 200_000);
}

// pdf-rs's `Panic` (a site where pdf-go panics) is `Malformed`, as `catchPanic` made it.
#[test]
fn seam_turns_a_panic_into_malformed() {
    match seam("probe", pdf::Error::Panic("pdf-go exploded")) {
        Err(Error::Malformed(m)) => {
            assert!(m.contains("probe") && m.contains("pdf-go exploded"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    // A refusal and a cancel keep their meaning.
    assert_eq!(
        seam("probe", pdf::Error::Cancelled).unwrap_err(),
        Error::Cancelled
    );
    assert!(matches!(
        seam("probe", pdf::Error::Alloc { got: 2, max: 1 }),
        Err(Error::Alloc { got: 2, max: 1 })
    ));
    // Anything else is for the caller to classify.
    assert!(seam("probe", pdf::Error::WrongPassword).is_ok());
}

// A trailer that understates /Size is caught by the object count of the file.
#[test]
fn under_reported_size_is_an_objects_limit() {
    let mut bodies = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [] /Count 0 >>".to_string(),
    ];
    for _ in 0..2000 {
        bodies.push("<< >>".to_string());
    }
    let pdf = build_pdf(&bodies);
    let text = String::from_utf8(pdf).unwrap_or_default();
    let text = text.replacen(&format!("/Size {}", bodies.len() + 1), "/Size 10", 1);
    let lim = Limits {
        max_objects: 1000,
        ..Limits::default()
    };
    match open(text.as_bytes(), &lim, &never()) {
        Err(Error::Limit {
            name: "objects", ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// A catalog, a page tree and a page followed by `n - 3` empty objects, with no xref or trailer.
fn plain_objects(n: usize) -> String {
    let mut b = String::from("%PDF-1.7\n");
    for i in 1..=n {
        let body = match i {
            1 => "<< /Type /Catalog /Pages 2 0 R >>",
            2 => "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3 => "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            _ => "<< >>",
        };
        b.push_str(&format!("{i} 0 obj\n{body}\nendobj\n"));
    }
    b
}

fn want_objects_limit(name: &str, pdf: &[u8], lim: &Limits) {
    match open_budgeted(pdf, lim, &never()) {
        Err(Error::Limit {
            name: "objects", ..
        }) => {}
        other => panic!("{name}: {:?}", other.map(|_| ())),
    }
}

// A trailer is only a claim: whatever /Size it makes (a corrupt xref stream that says 10, a
// startxref that lands on an object with a /Size 10 nearby), the objects in the file are what
// pdf-rs rebuilds and holds.
#[test]
fn object_budget_does_not_rest_on_the_trailer() {
    let lim = Limits {
        max_objects: 1000,
        ..Limits::default()
    };
    let mut xs = plain_objects(5000);
    let at = xs.len();
    xs.push_str(&format!(
        "5001 0 obj\n<< /Type /XRef /Size 10 /Root 1 0 R /W [1 4 2] /Filter /FlateDecode /Length 5 >>\nstream\nxxxxx\nendstream\nendobj\nstartxref\n{at}\n%%EOF\n"
    ));
    want_objects_limit("corrupt xref stream", xs.as_bytes(), &lim);

    let mut sz = plain_objects(5000);
    sz.push_str("%/Size 10\nstartxref\n9\n%%EOF\n");
    want_objects_limit("startxref on an object", sz.as_bytes(), &lim);
}

// pdf-rs's rebuild of a table it cannot read ignores its object cap, so with no usable /Size
// the file's object headers are counted and held to the cap.
#[test]
fn rebuilt_table_is_held_to_the_object_cap() {
    let mut bodies: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>".into(),
    ];
    while bodies.len() < 210_000 {
        bodies.push("<< >>".into());
    }
    let pdf = build_pdf(&bodies);
    let text = String::from_utf8(pdf).unwrap_or_default();
    // No xref, no trailer; and a startxref that lands on an object.
    let no_xref = text.get(..text.find("xref\n0 ").unwrap_or(0)).unwrap_or("");
    let bad_off = text.replacen("startxref\n", "startxref\n9 ", 1);
    want_objects_limit("no xref", no_xref.as_bytes(), &Limits::default());
    want_objects_limit(
        "startxref on an object",
        bad_off.as_bytes(),
        &Limits::default(),
    );
}

// ---------------------------------------------------------------------------------------
// raw images under the declared size

/// A one-page document holding one `/DeviceGray` image XObject named `Im0` whose header says
/// `w` x `h` and whose data is `data`.
fn image_pdf(w: i64, h: i64, data: &[u8], filter: &str) -> Vec<u8> {
    pdf_of_bytes(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << /XObject << /Im0 5 0 R >> >> >>".to_vec(),
        stream_obj("", b"q 100 0 0 100 0 0 cm /Im0 Do Q"),
        stream_obj(
            &format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} /BitsPerComponent 8 /ColorSpace /DeviceGray {filter}"),
            data,
        ),
    ])
}

fn raw_image_of(pdf: &[u8], lim: &Limits) -> Result<(Vec<u8>, &'static str), Error> {
    let mut d = open(pdf, lim, &never())?;
    let p = d.page(1)?;
    let xo = d
        .xobject(p.scope, b"Im0")
        .ok_or_else(|| Error::Malformed("image not resolved".into()))?;
    d.raw_image(&never(), xo.id)
}

// pdf-rs decodes image streams under its own 512 MiB limit unless told otherwise; byblos sets it
// to the image's declared size.
#[test]
fn image_decode_is_bounded_by_the_declared_size() {
    // Declares 10 x 10 and carries 8 MiB of deflated zeros.
    let bomb = image_pdf(10, 10, &zlib_zeros(8 << 20), "/Filter /FlateDecode");
    match raw_image_of(&bomb, &Limits::default()) {
        Err(Error::Limit {
            name: "image-bytes",
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
    // An honest image of the same header decodes.
    let ok = image_pdf(10, 10, &zlib_zeros(100), "/Filter /FlateDecode");
    let (data, file_type) = raw_image_of(&ok, &Limits::default()).unwrap();
    assert_eq!(file_type, "png");
    assert!(data.starts_with(b"\x89PNG"));
}

#[test]
fn image_pixel_cap_is_checked_from_the_header() {
    let pdf = image_pdf(100_000, 100_000, &[0], "");
    match raw_image_of(&pdf, &Limits::default()) {
        Err(Error::Limit {
            name: "image-pixels",
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

// pdf-rs reads a decode limit of 0 as its 512 MiB default and a negative one as none, so a
// header that declares no positive size must not reach it.
#[test]
fn raw_image_forged_dimensions_are_refused_before_decoding() {
    let bomb = zlib_zeros(8 << 20);
    for (name, w, h) in [
        ("negative width", -1, 100_000_000),
        ("zero width", 0, 0),
        ("negative both", -1, -1),
    ] {
        let r = raw_image_of(
            &image_pdf(w, h, &bomb, "/Filter /FlateDecode"),
            &Limits::default(),
        );
        assert!(r.is_err(), "{name}");
    }
}

// pdf-go hands JBIG2 back as opaque bytes with file type "jbig2" and no error. That is not a
// failure at this layer: it is a real payload in a codec this package does not claim to render,
// so `raw_image` reports it faithfully.
#[test]
fn raw_image_reports_jbig2_as_is() {
    let pdf = image_pdf(4, 4, b"\x97JB2\r\n\x1a\n", "/Filter /JBIG2Decode");
    let (data, file_type) = raw_image_of(&pdf, &Limits::default()).unwrap();
    assert_eq!(file_type, "jbig2");
    assert!(!data.is_empty());
}

#[test]
fn raw_image_unknown_id() {
    let mut d = open_default(&image_pdf(4, 4, &[0; 16], ""));
    let err = d.raw_image(&never(), 99999).unwrap_err();
    // ErrUnsupportedCodec is reserved for a real image in a codec pdf-rs will not render; an
    // unknown id is a caller mistake and must not be confused with it.
    assert!(!matches!(err, Error::UnsupportedCodec), "{err}");
}

// ---------------------------------------------------------------------------------------
// pdfdoc.go: pages

/// A one-page document whose page dictionary has `page_extra` after the required entries.
fn one_page(page_extra: &str) -> Vec<u8> {
    pdf_of(&[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        &format!("<< /Type /Page /Parent 2 0 R {page_extra} >>"),
    ])
}

fn rect(llx: f64, lly: f64, urx: f64, ury: f64) -> Rect {
    Rect { llx, lly, urx, ury }
}

// A page that carries no /Contents is legal and empty, not an error.
#[test]
fn page_without_contents_is_empty_not_an_error() {
    let mut d = open_default(&one_page("/MediaBox [0 0 200 300]"));
    let p = d.page(1).unwrap();
    assert!(p.content.is_empty());
    assert_eq!(p.media_box, rect(0.0, 0.0, 200.0, 300.0));
    assert_eq!(p.crop_box, p.media_box, "no /CropBox reports the MediaBox");
    assert_eq!(p.rotate, 0);
    assert!(!p.media_box_defaulted);
}

// A page with no /MediaBox anywhere in its inheritance chain defaults to US Letter, and says so.
#[test]
fn page_without_a_media_box_is_letter_and_says_so() {
    let mut d = open_default(&one_page(""));
    let p = d.page(1).unwrap();
    assert_eq!(p.media_box, rect(0.0, 0.0, 612.0, 792.0));
    assert!(p.media_box_defaulted);
}

#[test]
fn open_reads_rotate_normalised() {
    for (raw, want) in [("90", 90), ("-90", 270), ("450", 90), ("0", 0)] {
        let mut d = open_default(&one_page(&format!("/MediaBox [0 0 10 10] /Rotate {raw}")));
        assert_eq!(d.page(1).unwrap().rotate, want, "/Rotate {raw}");
    }
}

// ISO 32000-1 7.7.3.3 says the CropBox is to be intersected with the MediaBox, and poppler does
// that. A CropBox that overhangs the MediaBox must not enlarge the page's reported bounds. See
// byb-wtp.
#[test]
fn crop_box_is_clamped_to_media_box() {
    let mut d = open_default(&one_page(
        "/MediaBox [0 0 612 792] /CropBox [-100 -100 900 1000]",
    ));
    let p = d.page(1).unwrap();
    assert_eq!(p.media_box, rect(0.0, 0.0, 612.0, 792.0));
    assert_eq!(p.crop_box, rect(0.0, 0.0, 612.0, 792.0));
    let mut d = open_default(&one_page(
        "/MediaBox [0 0 612 792] /CropBox [100 100 300 400]",
    ));
    assert_eq!(
        d.page(1).unwrap().crop_box,
        rect(100.0, 100.0, 300.0, 400.0)
    );
}

// A CropBox that falls entirely outside the MediaBox has no intersection with it. poppler falls
// back to the MediaBox rather than reporting an empty page. See byb-wtp.
#[test]
fn crop_box_outside_media_box_falls_back_to_media_box() {
    let mut d = open_default(&one_page(
        "/MediaBox [0 0 612 792] /CropBox [700 900 1000 1200]",
    ));
    assert_eq!(d.page(1).unwrap().crop_box, rect(0.0, 0.0, 612.0, 792.0));
}

#[test]
fn page_out_of_range() {
    let mut d = open_default(&one_page("/MediaBox [0 0 10 10]"));
    assert_eq!(d.page_count(), 1);
    assert!(d.page(0).is_err());
    assert!(d.page(2).is_err());
    assert!(d.page(1).is_ok());
}

// A truncated file must produce a clean error, never a panic and never a plausible-looking
// parse.
#[test]
fn open_malformed_returns_an_error() {
    for data in [
        &b""[..],
        b"%PDF-1.7\n",
        b"garbage",
        b"%PDF-1.4\n1 0 obj\n<< /Type",
    ] {
        assert!(open(data, &Limits::default(), &never()).is_err());
    }
}

// A /Pages node may hold its /Kids as an indirect reference, and Google Books PDF Converter
// output does. pdf-rs reads /Kids with `Dict::array_entry`, which does not dereference, so such
// a node looks childless and `page_dict` returns it (no /MediaBox, no /Contents) as the
// dictionary of every page. `open` repairs the tree; without that, both pages here fail. See
// byb-5kk.
#[test]
fn page_tree_with_indirect_kids() {
    let data = pdf_of_bytes(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids 7 0 R /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Resources 6 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Resources << /XObject << /Im0 9 0 R >> >> >>".to_vec(),
        stream_obj("", b"/Im0 Do"),
        b"<< /XObject << /Im0 8 0 R >> >>".to_vec(),
        b"[3 0 R 4 0 R]".to_vec(),
        stream_obj("/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 /ColorSpace /DeviceGray", b"abcd"),
        stream_obj("/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 /ColorSpace /DeviceGray", b"efgh"),
    ]);
    let mut d = open_default(&data);
    assert_eq!(d.page_count(), 2);
    let mut ids = Vec::new();
    for n in 1..=2 {
        let p = d.page(n).unwrap();
        assert_eq!(p.media_box, rect(0.0, 0.0, 612.0, 792.0), "page {n}");
        assert!(p.content.starts_with(b"/Im0 Do"), "page {n}");
        // The page's own /Resources is an indirect reference too (page 1), so a repair that
        // fixed only /Kids would still resolve nothing here.
        let xo = d
            .xobject(p.scope, b"Im0")
            .unwrap_or_else(|| panic!("page {n}: Im0 does not resolve in the page scope"));
        ids.push(xo.id);
    }
    // Each page must resolve its own raster: returning the same dictionary for every page is
    // exactly the bug.
    assert_ne!(ids[0], ids[1]);
}

// ---------------------------------------------------------------------------------------
// pdfdoc.go: resources

const IMAGE_DICT: &str =
    "/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 /ColorSpace /DeviceGray";

/// A document whose page has `/Resources << resources >>` and content `content`, with the
/// extra objects from number 5.
fn resource_pdf(resources: &str, content: &[u8], extra: &[Vec<u8>]) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << {resources} >> >>"
        )
        .into_bytes(),
        stream_obj("", content),
    ];
    objs.extend(extra.iter().cloned());
    pdf_of_bytes(&objs)
}

#[test]
fn xobject_resolves_an_image() {
    let data = resource_pdf(
        "/XObject << /Im0 5 0 R >>",
        b"q 100 0 0 100 0 0 cm /Im0 Do Q",
        &[stream_obj(IMAGE_DICT, b"abcd")],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let xo = d.xobject(p.scope, b"Im0").unwrap();
    assert!(xo.image);
    let info = d.image_info(xo.id).unwrap();
    assert_eq!((info.width, info.height, info.bpc), (2, 2, 8));
    assert!(!info.image_mask);
    assert_eq!(info.name, "Im0");
    assert_eq!(info.color_space, "DeviceGray");
    assert!(info.filter.is_empty());
    assert!(d.xobject(p.scope, b"Nope").is_none());
    assert!(d.xobject(9999, b"Im0").is_none(), "an out-of-range scope");
}

// Go's name is a string of bytes: a /Filter that is not UTF-8 is reported as written, not with
// U+FFFD in place of the bad byte (parity: mut-corpus-dup-raster.pdf.f1.pdf.inspect).
#[test]
fn image_filter_keeps_bytes_that_are_not_utf8() {
    let mut img = stream_obj(&format!("{IMAGE_DICT} /Filter /FlateDecXde"), b"abcd");
    let at = img.windows(4).position(|w| w == b"DecX").unwrap() + 3;
    img[at] = 0xEF;
    let data = resource_pdf(
        "/XObject << /Im0 5 0 R >>",
        b"q 100 0 0 100 0 0 cm /Im0 Do Q",
        &[img],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let xo = d.xobject(p.scope, b"Im0").unwrap();
    assert_eq!(d.image_info(xo.id).unwrap().filter, b"FlateDec\xEFde");
}

// A Form XObject must come back decoded, with its own /Matrix and a scope handle that resolves
// the form's own resources.
#[test]
fn xobject_resolves_a_form_with_its_own_scope() {
    let data = resource_pdf(
        "/XObject << /Fm0 5 0 R >>",
        b"/Fm0 Do",
        &[
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /XObject << /Im0 6 0 R >> >>",
                b"q 100 0 0 100 0 0 cm /Im0 Do Q",
            ),
            stream_obj(IMAGE_DICT, b"abcd"),
        ],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let fm = d.xobject(p.scope, b"Fm0").unwrap();
    assert!(!fm.image, "Fm0 resolved as an image; want a form");
    assert!(fm.content.windows(7).any(|w| w == b"/Im0 Do"));
    assert_eq!(fm.matrix, IDENTITY);
    assert_ne!(fm.scope, p.scope, "the form declares its own /Resources");
    assert_eq!(
        fm.bbox,
        Some(content::BBox {
            llx: 0.0,
            lly: 0.0,
            urx: 100.0,
            ury: 100.0
        })
    );
    assert!(d.xobject(fm.scope, b"Im0").is_some());
    // The page's own scope must NOT see the form's Im0.
    assert!(
        d.xobject(p.scope, b"Im0").is_none(),
        "Im0 leaked into the page scope"
    );
    // A form invoked again is decoded once and shares its buffer.
    let again = d.xobject(p.scope, b"Fm0").unwrap();
    assert!(Arc::ptr_eq(&fm.content, &again.content));
}

// A form without its own /Resources inherits the enclosing scope (ISO 32000-1 section 8.10.2).
// The form declares only /Font, so the image must still resolve through the page's resources.
#[test]
fn form_without_xobject_resources_falls_back() {
    let data = resource_pdf(
        "/XObject << /Fm0 5 0 R /Im0 6 0 R >>",
        b"/Fm0 Do",
        &[
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << >> >>",
                b"q Q",
            ),
            stream_obj(IMAGE_DICT, b"abcd"),
        ],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let fm = d.xobject(p.scope, b"Fm0").unwrap();
    assert!(
        d.xobject(fm.scope, b"Im0").is_some(),
        "resource fallback is missing"
    );
}

// A form over the stream cap answers "no" and keeps the breach for `err`, which inspect checks
// after each page.
#[test]
fn form_over_the_stream_cap_is_remembered() {
    let data = resource_pdf(
        "/XObject << /Fm0 5 0 R >>",
        b"/Fm0 Do",
        &[stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 1 1]",
            &[b' '; 4096],
        )],
    );
    let lim = Limits {
        max_stream_bytes: 1024,
        ..Limits::default()
    };
    let mut d = open(&data, &lim, &never()).unwrap();
    let p = d.page(1).unwrap();
    assert!(d.err().is_none());
    assert!(d.xobject(p.scope, b"Fm0").is_none());
    match d.err() {
        Some(Error::Limit {
            name: "stream-bytes",
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

// Whether the upper layer of a stacked page is opaque decides whether it can be said to hide the
// layer below, and /ca lives in an /ExtGState rather than in the image dictionary.
#[test]
fn ext_gstate_opaque() {
    let data = resource_pdf(
        "/ExtGState << /GS0 << /ca 0.5 >> /GS1 << /ca 1 >> /GS2 << /SMask /None >> /GS3 << /SMask << >> >> /GS4 << /CA 0.25 >> /GS5 5 0 R >>",
        b"q Q",
        &[b"<< /ca 0.5 >>".to_vec()],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    assert!(
        !d.ext_gstate_opaque(p.scope, b"GS0"),
        "the state sets /ca 0.5"
    );
    assert!(d.ext_gstate_opaque(p.scope, b"GS1"));
    assert!(
        d.ext_gstate_opaque(p.scope, b"GS2"),
        "/SMask /None is no mask"
    );
    assert!(!d.ext_gstate_opaque(p.scope, b"GS3"), "a soft mask");
    assert!(!d.ext_gstate_opaque(p.scope, b"GS4"), "/CA counts too");
    assert!(
        !d.ext_gstate_opaque(p.scope, b"GS5"),
        "a state through a reference"
    );
    // A name the document does not declare has not been shown to be opaque, and guessing that
    // it is would be the one direction that loses content.
    assert!(!d.ext_gstate_opaque(p.scope, b"Nope"));
}

// A document with no /ExtGState at all must not report its placements as transparent, or every
// clean scan would divert.
#[test]
fn a_walk_over_a_doc_env_sees_an_opaque_placement() {
    let data = resource_pdf(
        "/XObject << /Im0 5 0 R >>",
        b"q 100 0 0 100 0 0 cm /Im0 Do Q",
        &[stream_obj(IMAGE_DICT, b"abcd")],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let env = DocEnv::new(&mut d);
    let (scan, err) = content::walk(
        &never(),
        &Arc::new(p.content.clone()),
        p.scope,
        &env,
        &Limits::default(),
    );
    assert_eq!(err, Ok(()));
    assert_eq!(scan.images.len(), 1);
    assert!(scan.images[0].opaque);
    let id = scan.images[0].id;
    assert_eq!(env.with(|d| d.image_info(id).map(|i| i.width)), Some(2));
    assert!(env.with(|d| d.err().is_none()));
}

// `font` resolves a /Font resource name to the stable id the walk records on each TextShow.
#[test]
fn font_resolves_to_a_stable_id() {
    let data = resource_pdf(
        "/Font << /F1 5 0 R >>",
        b"BT /F1 12 Tf 50 750 Td (hi) Tj ET",
        &[b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec()],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let id = d.font(p.scope, b"F1").unwrap();
    assert_eq!(id, 5);
    assert_eq!(d.font(p.scope, b"F1"), Some(id), "a stable id");
    assert!(d.font(p.scope, b"Nope").is_none());
    let env = DocEnv::new(&mut d);
    let (scan, err) = content::walk(
        &never(),
        &Arc::new(p.content.clone()),
        p.scope,
        &env,
        &Limits::default(),
    );
    assert_eq!(err, Ok(()));
    assert_eq!(scan.text_shows.first().map(|t| t.font_id), Some(id));
}

#[test]
fn render_font_reads_widths_and_refuses_other_encodings() {
    let data = resource_pdf(
        "/Font << /F1 5 0 R /F2 6 0 R /F3 7 0 R /F4 9 0 R >>",
        b"q Q",
        &[
            b"<< /Type /Font /Subtype /TrueType /BaseFont /Foo /FirstChar 32 /Widths [250 500 1.5] /FontDescriptor << /Flags 32 >> >>".to_vec(),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /Bar /Encoding /Identity-H /DescendantFonts [8 0 R] >>".to_vec(),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /Baz /Encoding /Identity-V /DescendantFonts [8 0 R] >>".to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /DW 700 /W [1 [10 20] 5 7 99 100 100 9] >>".to_vec(),
            b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [8 0 R 8 0 R] >>".to_vec(),
        ],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let f1 = d.render_font(p.scope, b"F1").unwrap();
    assert_eq!(f1.base_font, b"Foo");
    assert_eq!((f1.first_char, f1.flags), (32, 32));
    assert_eq!(f1.widths, vec![250.0, 500.0, 1.5]);
    assert!(f1.program.is_empty() && !f1.type0);
    let f2 = d.render_font(p.scope, b"F2").unwrap();
    assert!(f2.type0);
    assert_eq!(f2.dw, 700.0);
    assert_eq!(f2.w.get(&1), Some(&10.0));
    assert_eq!(f2.w.get(&2), Some(&20.0));
    for cid in 5..=7 {
        assert_eq!(f2.w.get(&cid), Some(&99.0), "cid {cid}");
    }
    assert_eq!(f2.w.get(&100), Some(&9.0));
    // Identity-V and a descendant list of two are refused, not resolved wrongly.
    assert!(d.render_font(p.scope, b"F3").is_none());
    assert!(d.render_font(p.scope, b"F4").is_none());
    assert!(d.render_font(p.scope, b"Nope").is_none());
}

// `/Decode` as ImageInfo reports it. The two fields say different things and BOTH are
// load-bearing: `decode` is "the entry is there", which the optimizer's eligibility test wants (an
// entry it cannot read is still an entry it must not drop), and `decode_array` is "and these
// are its numbers", `None` when there are none to have.
#[test]
fn image_info_reads_the_decode_array() {
    let raw = |extra: &str| {
        stream_obj(
            &format!(
                "/Type /XObject /Subtype /Image /Width 4 /Height 4 /BitsPerComponent 1 /ColorSpace /DeviceGray /Filter /JBIG2Decode {extra}"
            ),
            b"\x00\x01\x02\x03",
        )
    };
    let data = pdf_of_bytes(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Absent 4 0 R /Default 5 0 R /Inverted 6 0 R /Float 7 0 R /Indirect 8 0 R /Unreadable 10 0 R >> >> >>".to_vec(),
        raw(""),
        raw("/Decode [0 1]"),
        raw("/Decode [1 0]"),
        raw("/Decode [1.0 0.0]"),
        raw("/Decode 9 0 R"),
        b"[1 0]".to_vec(),
        raw("/Decode [/Bogus 0]"),
    ]);
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    let cases: [(&str, bool, Option<Vec<f64>>); 6] = [
        ("Absent", false, None),
        ("Default", true, Some(vec![0.0, 1.0])),
        ("Inverted", true, Some(vec![1.0, 0.0])),
        ("Float", true, Some(vec![1.0, 0.0])),
        ("Indirect", true, Some(vec![1.0, 0.0])),
        ("Unreadable", true, None),
    ];
    for (name, present, want) in cases {
        let xo = d
            .xobject(p.scope, name.as_bytes())
            .filter(|x| x.image)
            .unwrap_or_else(|| panic!("{name}: not resolved as an image; the fixture is broken"));
        let info = d.image_info(xo.id).unwrap();
        assert_eq!(info.decode, present, "{name}: decode");
        assert_eq!(
            info.decode_array.as_deref().map(<[f64]>::to_vec),
            want,
            "{name}: decode_array"
        );
        assert_eq!(info.filter, b"JBIG2Decode", "{name}");
    }
}

#[test]
fn image_info_reports_transparency_entries() {
    let data = resource_pdf(
        "/XObject << /Im0 5 0 R /Im1 6 0 R /Im2 8 0 R >>",
        b"q Q",
        &[
            stream_obj(IMAGE_DICT, b"abcd"),
            stream_obj(&format!("{IMAGE_DICT} /SMask 7 0 R"), b"abcd"),
            stream_obj(IMAGE_DICT, b"abcd"),
            stream_obj(
                &format!("{IMAGE_DICT} /Mask [0 0] /ImageMask false"),
                b"abcd",
            ),
        ],
    );
    let mut d = open_default(&data);
    let p = d.page(1).unwrap();
    for (name, smask, mask) in [
        ("Im0", false, false),
        ("Im1", true, false),
        ("Im2", false, true),
    ] {
        let xo = d.xobject(p.scope, name.as_bytes()).unwrap();
        let info = d.image_info(xo.id).unwrap();
        assert_eq!((info.smask, info.mask), (smask, mask), "{name}");
    }
}

// ---------------------------------------------------------------------------------------
// contents_test.go: what a page's bytes are

/// A one-page document whose `/Contents` is `contents` and whose streams are the objects from
/// number 4.
fn contents_pdf(contents: &str, streams: &[Vec<u8>]) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {contents} >>")
            .into_bytes(),
    ];
    objs.extend(streams.iter().cloned());
    pdf_of_bytes(&objs)
}

fn page_of(data: &[u8]) -> Page {
    open_default(data).page(1).unwrap()
}

fn flate_stream(z: &[u8]) -> Vec<u8> {
    stream_obj("/Filter /FlateDecode", z)
}

// ISO 32000-1 table 30 concatenates the elements as a single stream with no byte between them.
#[test]
fn clean_contents_array_has_no_separator_and_is_not_recovered() {
    let parts = [
        "BT /F1 12 Tf 50 750 Td (one) Tj ET",
        "BT /F1 12 Tf 50 730 Td (two) Tj ET",
        "BT /F1 12 Tf 50 710 Td (three) Tj ET",
    ];
    let streams: Vec<Vec<u8>> = parts
        .iter()
        .map(|p| flate_stream(&zlib_stored(p.as_bytes())))
        .collect();
    let p = page_of(&contents_pdf("[4 0 R 5 0 R 6 0 R]", &streams));
    assert_eq!(String::from_utf8_lossy(&p.content), parts.concat());
    assert!(!p.content_recovered, "an undamaged /Contents array");
}

#[test]
fn bad_adler32_is_recovered_whole() {
    let want = "BT /F1 12 Tf 50 750 Td (Recoverable text.) Tj ET";
    let mut z = zlib_stored(want.as_bytes());
    if let Some(last) = z.last_mut() {
        *last ^= 0xFF;
    }
    let p = page_of(&contents_pdf("4 0 R", &[flate_stream(&z)]));
    assert_eq!(String::from_utf8_lossy(&p.content), want);
    assert!(p.content_recovered, "the stream failed its checksum");
}

// The defect byb-3iw was filed for: pdf-go's relaxed mode drops the damaged element and returns
// the sibling's bytes with NO error, so the page is reported as fine while most of its content is
// gone. The assertion is a byte count, not an error.
#[test]
fn bad_adler32_in_array_keeps_both_elements() {
    let bad = "BT /F1 12 Tf 50 730 Td (Bad Adler.) Tj ET";
    let good = "BT /F1 12 Tf 50 750 Td (Good sibling.) Tj ET";
    let mut zbad = zlib_stored(bad.as_bytes());
    if let Some(last) = zbad.last_mut() {
        *last ^= 0xFF;
    }
    let p = page_of(&contents_pdf(
        "[4 0 R 5 0 R]",
        &[
            flate_stream(&zbad),
            flate_stream(&zlib_stored(good.as_bytes())),
        ],
    ));
    // The array orders the damaged element first.
    assert_eq!(String::from_utf8_lossy(&p.content), format!("{bad}{good}"));
    assert!(p.content_recovered);
}

// A /Length that over-declares, routinely by swallowing the EOL before `endstream`, leaves
// bytes after the trailer; the trailer is found, not assumed to be the last four bytes.
#[test]
fn bad_adler_with_trailing_byte_is_recovered_whole() {
    let want = "BT /F1 12 Tf 50 750 Td (Trailing byte.) Tj ET";
    let mut z = zlib_stored(want.as_bytes());
    if let Some(last) = z.last_mut() {
        *last ^= 0xFF;
    }
    z.push(b'\n');
    let p = page_of(&contents_pdf("4 0 R", &[flate_stream(&z)]));
    assert_eq!(String::from_utf8_lossy(&p.content), want);
    assert!(p.content_recovered);
}

// A damaged element that inflates to nothing is dropped beside a healthy sibling, not escalated
// into a whole-page error.
#[test]
fn corrupt_element_beside_healthy_sibling_keeps_the_sibling() {
    let good = "BT /F1 12 Tf 50 750 Td (Healthy.) Tj ET";
    // A valid zlib header, then a RESERVED deflate block type.
    let corrupt = flate_stream(&[0x78, 0x01, 0x07, 0x00, 0x00, 0x00, 0x00]);
    let p = page_of(&contents_pdf(
        "[4 0 R 5 0 R]",
        &[corrupt, flate_stream(&zlib_stored(good.as_bytes()))],
    ));
    assert_eq!(String::from_utf8_lossy(&p.content), good);
    assert!(p.content_recovered, "an element was dropped");
}

// ISO 32000-1 7.3.10 makes a reference to a nonexistent object null and 7.3.9 makes null
// equivalent to omitting the entry, so this page is legally blank.
#[test]
fn null_contents_ref_is_blank_not_an_error() {
    let p = page_of(&contents_pdf("99 0 R", &[]));
    assert!(p.content.is_empty());
    assert!(!p.content_recovered);
}

// The boundary the recovery must not cross: a stream that inflates to zero bytes is not a
// recovery, and a page whose every element is unreadable is refused.
#[test]
fn corrupt_content_stream_is_still_refused() {
    let corrupt = flate_stream(&[0x78, 0x01, 0x07, 0x00, 0x00, 0x00, 0x00]);
    let mut d = open_default(&contents_pdf("4 0 R", &[corrupt]));
    let err = d.page(1).unwrap_err();
    assert!(matches!(err, Error::Malformed(_)), "{err}");
    assert_eq!(err.class(), "error");
}

// A /Contents that is neither a stream nor an array is refused.
#[test]
fn contents_that_is_a_number_is_refused() {
    let mut d = open_default(&one_page("/MediaBox [0 0 10 10] /Contents 42"));
    assert!(d.page(1).is_err());
}

// The page-stream cap is over the decoded streams of one page together.
#[test]
fn page_stream_bytes_are_capped_per_page() {
    let big = stream_obj("", &[b' '; 600]);
    let data = contents_pdf("[4 0 R 5 0 R]", &[big.clone(), big]);
    let lim = Limits {
        max_page_stream_bytes: 1000,
        ..Limits::default()
    };
    let mut d = open(&data, &lim, &never()).unwrap();
    match d.page(1) {
        Err(Error::Limit {
            name: "page-stream-bytes",
            max: 1000,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|p| p.content.len())),
    }
}

// ---------------------------------------------------------------------------------------
// catalog.go, encryptinfo.go, properties.go

#[test]
fn catalog_flags() {
    let make = |cat: &str| {
        pdf_of(&[
            &format!("<< /Type /Catalog /Pages 2 0 R {cat} >>"),
            "<< /Type /Pages /Kids [] /Count 0 >>",
            "<< /SigFlags 3 >>",
        ])
    };
    let mut d = open_default(&make(""));
    assert_eq!(d.catalog(), CatalogInfo::default());
    let mut d = open_default(&make("/NeedsRendering true /AcroForm << /SigFlags 1 >>"));
    assert_eq!(
        d.catalog(),
        CatalogInfo {
            has_signature: true,
            needs_rendering: true
        }
    );
    // Bit 1 only: AppendOnly (2) alone is no signature.
    let mut d = open_default(&make("/AcroForm << /SigFlags 2 >>"));
    assert!(!d.catalog().has_signature);
    // An indirect /AcroForm.
    let mut d = open_default(&make("/AcroForm 3 0 R"));
    assert!(d.catalog().has_signature);
}

#[test]
fn encrypt_info_of_a_plain_document() {
    let d = open_default(&one_page("/MediaBox [0 0 10 10]"));
    assert_eq!(d.encrypt_info(), EncryptInfo::default());
    assert!(!is_wrong_password(&Error::Cancelled));
    assert!(is_wrong_password(&Error::Pdf(pdf::Error::WrongPassword)));
    assert!(is_wrong_password(&Error::Pdf(
        pdf::Error::WrongPassword.context("read")
    )));
    assert!(!is_wrong_password(&Error::Pdf(pdf::Error::Lzw)));
}

#[test]
fn open_budgeted_holds_the_object_cap_to_the_file() {
    let data = one_page("/MediaBox [0 0 10 10]");
    let d = open_budgeted(&data, &Limits::default(), &never()).unwrap();
    // 3 objects: the cap is the claim plus an eighth and 64.
    assert_eq!(d.limits().max_objects, 4 + 64); // 4 / 8 == 0
    assert_eq!(d.page_count(), 1);
}

// ---------------------------------------------------------------------------------------
// Tests that read a document of Go's corpus generator (`internal/corpus`). They need
// tests/data/corpus/<name>.pdf from the Go oracle's `-gen`.

fn corpus(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/data/corpus/{name}.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => panic!("{path}: {e}"),
    }
}

fn image0(d: &mut Doc, page: usize) -> i64 {
    let p = d.page(page).unwrap();
    let xo = d.xobject(p.scope, b"Im0").unwrap();
    assert!(xo.image);
    xo.id
}

#[test]
#[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
fn corpus_open_reads_page_geometry() {
    let mut d = open_default(&corpus("scan"));
    assert_eq!(d.page_count(), 1);
    let p = d.page(1).unwrap();
    let want = rect(0.0, 0.0, 612.0, 792.0);
    assert_eq!(p.media_box, want);
    assert_eq!(p.crop_box, want);
    assert_eq!(p.rotate, 0);
    assert!(p.content.windows(7).any(|w| w == b"/Im0 Do"));
    let id = image0(&mut d, 1);
    let info = d.image_info(id).unwrap();
    assert_eq!((info.width, info.height, info.bpc), (306, 396, 8));
    // pdf-go re-renders a Flate-compressed image to PNG; the bytes must decode.
    let (data, ft) = d.raw_image(&never(), id).unwrap();
    assert_eq!(ft, "png");
    assert!(data.starts_with(b"\x89PNG"));
    assert!(d.page(0).is_err() && d.page(2).is_err());
}

#[test]
#[ignore = "needs tests/data/corpus/scan-rotated.pdf from the Go oracle -gen"]
fn corpus_open_reads_rotate() {
    let p = open_default(&corpus("scan-rotated")).page(1).unwrap();
    assert_eq!(p.rotate, 90);
}

#[test]
#[ignore = "needs tests/data/corpus/mixed.pdf from the Go oracle -gen"]
fn corpus_open_multi_page() {
    let mut d = open_default(&corpus("mixed"));
    assert_eq!(d.page_count(), 2);
    assert!(d.page(1).unwrap().content.windows(2).any(|w| w == b"Tj"));
    assert!(
        d.page(2)
            .unwrap()
            .content
            .windows(7)
            .any(|w| w == b"/Im0 Do")
    );
}

#[test]
#[ignore = "needs tests/data/corpus/indirect-kids.pdf from the Go oracle -gen"]
fn corpus_page_tree_with_indirect_kids() {
    let mut d = open_default(&corpus("indirect-kids"));
    assert_eq!(d.page_count(), 2);
    for n in 1..=2 {
        let p = d.page(n).unwrap();
        assert_eq!(p.media_box, rect(0.0, 0.0, 612.0, 792.0), "page {n}");
        assert!(d.xobject(p.scope, b"Im0").is_some(), "page {n}");
    }
    assert_ne!(image0(&mut d, 1), image0(&mut d, 2));
}

#[test]
#[ignore = "needs tests/data/corpus/dup-raster.pdf from the Go oracle -gen"]
fn corpus_raw_image_is_per_page_when_rasters_are_identical() {
    let mut d = open_default(&corpus("dup-raster"));
    let (id1, id2) = (image0(&mut d, 1), image0(&mut d, 2));
    assert_ne!(id1, id2, "the document is meant to use two objects");
    for id in [id1, id2] {
        let (_, ft) = d.raw_image(&never(), id).unwrap();
        assert_eq!(ft, "png");
    }
}

#[test]
#[ignore = "needs tests/data/corpus/jbig2.pdf from the Go oracle -gen"]
fn corpus_raw_image_reports_jbig2_as_is() {
    let mut d = open_default(&corpus("jbig2"));
    let id = image0(&mut d, 1);
    assert_eq!(d.image_info(id).map(|i| i.bpc), Some(1));
    let (data, ft) = d.raw_image(&never(), id).unwrap();
    assert_eq!(ft, "jbig2");
    assert!(!data.is_empty());
}

#[test]
#[ignore = "needs tests/data/corpus/scan-in-form.pdf from the Go oracle -gen"]
fn corpus_xobject_resolves_a_form_with_its_own_scope() {
    let mut d = open_default(&corpus("scan-in-form"));
    let p = d.page(1).unwrap();
    let fm = d.xobject(p.scope, b"Fm0").unwrap();
    assert!(!fm.image);
    assert!(fm.content.windows(7).any(|w| w == b"/Im0 Do"));
    assert!(d.xobject(fm.scope, b"Im0").is_some());
    assert!(d.xobject(p.scope, b"Im0").is_none());
}

#[test]
#[ignore = "needs tests/data/corpus/overlay-text.pdf from the Go oracle -gen"]
fn corpus_form_without_xobject_resources_falls_back() {
    let mut d = open_default(&corpus("overlay-text"));
    let p = d.page(1).unwrap();
    let fm = d.xobject(p.scope, b"Fm0").unwrap();
    assert!(d.xobject(fm.scope, b"Im0").is_some());
}

#[test]
#[ignore = "needs tests/data/corpus/stacked-alpha.pdf from the Go oracle -gen"]
fn corpus_ext_gstate_opaque() {
    let mut d = open_default(&corpus("stacked-alpha"));
    let p = d.page(1).unwrap();
    assert!(!d.ext_gstate_opaque(p.scope, b"GS0"));
    assert!(!d.ext_gstate_opaque(p.scope, b"Nope"));
}

#[test]
#[ignore = "needs tests/data/corpus/stacked-smask.pdf from the Go oracle -gen"]
fn corpus_image_info_reports_transparency_entries() {
    let mut d = open_default(&corpus("stacked-smask"));
    let p = d.page(1).unwrap();
    for (name, want) in [("Im1", true), ("Im0", false)] {
        let xo = d.xobject(p.scope, name.as_bytes()).unwrap();
        let info = d.image_info(xo.id).unwrap();
        assert_eq!(info.smask, want, "{name}");
        assert!(!info.mask, "{name}");
    }
}

#[test]
#[ignore = "needs tests/data/corpus/born-digital.pdf from the Go oracle -gen"]
fn corpus_font_resolves_to_a_stable_id() {
    let mut d = open_default(&corpus("born-digital"));
    let p = d.page(1).unwrap();
    let id = d.font(p.scope, b"F1").unwrap();
    assert_eq!(d.font(p.scope, b"F1"), Some(id));
    assert!(d.font(p.scope, b"Nope").is_none());
}

#[test]
#[ignore = "needs tests/data/corpus/blank-page.pdf from the Go oracle -gen"]
fn corpus_page_whose_contents_decodes_to_nothing_is_blank() {
    let mut d = open_default(&corpus("blank-page"));
    assert_eq!(d.page_count(), 2);
    assert!(d.page(2).unwrap().content.is_empty());
}

#[test]
#[ignore = "needs tests/data/corpus/malformed.pdf from the Go oracle -gen"]
fn corpus_open_malformed_returns_an_error() {
    assert!(open(&corpus("malformed"), &Limits::default(), &never()).is_err());
}
