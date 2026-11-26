//! Port of byblos `embed_test.go`, the Embed and EXIF half of `build_codecs_test.go`, and
//! `orientation_test.go`. Fixtures are `tests/data/img2pdf` (img2pdf 0.6.3's `oracle.json` and
//! the files beside it).
//!
//! Not ported:
//!
//! * The round trips through `ExtractPageRaster` (the PDF reader): the tests that built a PDF
//!   and read its pixels back now check that `build_pdf` accepts the embedded image and carries
//!   its bytes.
//! * `TestEmbedPNGRefusesOversizedImage`'s `runtime.MemStats` cost assertion (there is no Rust
//!   counterpart), and `TestEXIFRotationPNGTextBomb`'s. Both keep their error assertions.
//!   `hostilePNG` builds its IDAT by deflating 2 GiB of zeros; here it is arbitrary bytes, which
//!   `embed_png` never inflates.
//! * `FuzzBuildPDF`, `FuzzEmbedJP2` and `FuzzEXIFRotation` (the fuzz crate covers them).

use super::*;
use crate::build::{BuildPage, build_pdf, transcode_png};
use crate::go::json::{self, Value};
use crate::image::{Gray, Image, Rect, jpeg_write};
use pdf::filter::goflate::ZlibWriter;
use std::fs;

const AMBIGUOUS: Orientation = Orientation {
    rotate: 0,
    ambiguous: true,
};

fn lim() -> Limits {
    Limits::default()
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

/// `zlib.NewWriter` over `raw`.
fn zlib(raw: &[u8]) -> Vec<u8> {
    let mem = pdf::Mem::with_max(256 << 20);
    let cancel = no_cancel();
    let mut out: Vec<u8> = Vec::new();
    let mut zw = ZlibWriter::new(&mem).unwrap();
    zw.write(
        raw,
        &mut |b: &[u8]| {
            out.extend_from_slice(b);
            Ok(())
        },
        &cancel,
    )
    .unwrap();
    zw.close(&mut |b: &[u8]| {
        out.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    out
}

fn fixture(rel: &str) -> Vec<u8> {
    let p = format!("{}/tests/data/img2pdf/{rel}", env!("CARGO_MANIFEST_DIR"));
    fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn replace_first(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let i = find(data, old).expect("pattern not found");
    let mut out = data[..i].to_vec();
    out.extend_from_slice(new);
    out.extend_from_slice(&data[i + old.len()..]);
    out
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

// --- PNG builders --------------------------------------------------------------------------

/// `writeHostileChunk`: one length-prefixed, CRC-terminated PNG chunk.
fn png_chunk(typ: &str, data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(typ.as_bytes());
    out.extend_from_slice(data);
    let crc = crc32(&out[4..]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn ihdr(w: u32, h: u32, depth: u8, ctype: u8, interlace: u8) -> Vec<u8> {
    let mut b = w.to_be_bytes().to_vec();
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[depth, ctype, 0, 0, interlace]);
    png_chunk("IHDR", &b)
}

/// A PNG with the given header, extra chunks between IHDR and IDAT, and one IDAT holding the
/// zlib stream of `rows` (filter bytes included).
fn png_file(
    (w, h, depth, ctype): (u32, u32, u8, u8),
    interlace: u8,
    extra: &[Vec<u8>],
    rows: &[u8],
) -> Vec<u8> {
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(ihdr(w, h, depth, ctype, interlace));
    for c in extra {
        out.extend_from_slice(c);
    }
    out.extend(png_chunk("IDAT", &zlib(rows)));
    out.extend(png_chunk("IEND", &[]));
    out
}

/// Rows of `w` samples (one byte each), each behind filter type 0.
fn rows8(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Vec<u8> {
    let mut out = Vec::new();
    for y in 0..h {
        out.push(0);
        for x in 0..w {
            out.push(f(x, y));
        }
    }
    out
}

/// `smallPNG`: a 4x4 8-bit grey PNG.
fn small_png() -> Vec<u8> {
    png_file((4, 4, 8, 0), 0, &[], &rows8(4, 4, |x, y| (x + y) as u8))
}

/// `pngBeforeIDAT`: `small_png` with `chunks` written before its IDAT.
fn png_before_idat(chunks: &[Vec<u8>]) -> Vec<u8> {
    let d = small_png();
    let i = find(&d, b"IDAT").unwrap() - 4;
    let mut out = d[..i].to_vec();
    for c in chunks {
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&d[i..]);
    out
}

/// `insertPHYs`: splices a pHYs chunk right after the IHDR chunk.
fn insert_phys(png: &[u8], ppux: u32, ppuy: u32) -> Vec<u8> {
    let ihdr_end = 8 + 8 + 13 + 4;
    let mut payload = ppux.to_be_bytes().to_vec();
    payload.extend_from_slice(&ppuy.to_be_bytes());
    payload.push(1); // unit specifier: metre
    let mut out = png[..ihdr_end].to_vec();
    out.extend(png_chunk("pHYs", &payload));
    out.extend_from_slice(&png[ihdr_end..]);
    out
}

fn png_text_chunk(typ: &str, key: &str, text: &[u8]) -> Vec<u8> {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(&[0, 0]);
    data.extend(zlib(text));
    png_chunk(typ, &data)
}

fn build_one(img: EncodedImage, dpi: f64) -> Vec<u8> {
    build_pdf(
        &[BuildPage {
            image: img,
            dpi,
            ..BuildPage::default()
        }],
        &lim(),
        &no_cancel(),
    )
    .unwrap()
}

/// `imageDictString`.
fn image_dict(pdf: &[u8], filter: &str) -> String {
    let marker = format!("/Filter /{filter}");
    let idx = find(pdf, marker.as_bytes()).expect("no object with that filter");
    let start = pdf[..idx].windows(2).rposition(|w| w == b"<<").unwrap();
    let end = find(&pdf[idx..], b">>").unwrap();
    String::from_utf8_lossy(&pdf[start..idx + end + 2]).into_owned()
}

/// `pageDict`: the text of the first `/Type /Page` dictionary.
fn page_dict(pdf: &[u8]) -> String {
    let i = find(pdf, b"/Type /Page ").expect("no page dictionary");
    let n = find(&pdf[i..], b">> >>").unwrap();
    String::from_utf8_lossy(&pdf[i..i + n]).into_owned()
}

fn gray_jpeg(w: i64, h: i64, quality: i32, f: impl Fn(i64, i64) -> u8) -> Vec<u8> {
    let mut g = Gray::new(Rect::new(0, 0, w, h), &lim()).unwrap();
    for y in 0..h {
        for x in 0..w {
            g.pix[(y as usize) * g.stride + x as usize] = f(x, y);
        }
    }
    jpeg_write::encode(&Image::Gray(g), quality, &lim().mem(), &no_cancel()).unwrap()
}

fn is_limit(e: &Error, name: &str) -> bool {
    matches!(e, Error::Limit { name: n, .. } if *n == name)
}

// --- embed_test.go -------------------------------------------------------------------------

#[test]
fn embed_png_grayscale_carries_the_idat_into_build_pdf() {
    let (w, h) = (17usize, 11usize);
    let rows = rows8(w, h, |x, y| ((x * 7 + y * 13) % 256) as u8);
    let png = png_file((w as u32, h as u32, 8, 0), 0, &[], &rows);
    let (enc, dpi) = embed_png(&png, &lim()).unwrap();
    assert_eq!(
        dpi,
        Resolution::default(),
        "source PNG declares no pHYs chunk"
    );
    assert_eq!((enc.width, enc.height, enc.bpc), (w as i64, h as i64, 8));
    assert_eq!(enc.color_space.name, "DeviceGray");
    assert_eq!(enc.filter, "FlateDecode");
    assert_eq!(enc.data, zlib(&rows), "the IDAT payload is lifted verbatim");
    let parms = enc.decode_parms.unwrap();
    assert_eq!(
        (
            parms.predictor,
            parms.colors,
            parms.bits_per_component,
            parms.columns
        ),
        (15, 1, 8, w as i64)
    );
    let out = build_pdf(
        &[BuildPage {
            image: enc.clone(),
            width_pt: 72.0,
            height_pt: 72.0,
            ..BuildPage::default()
        }],
        &lim(),
        &no_cancel(),
    )
    .unwrap();
    assert!(out.starts_with(b"%PDF"));
    assert!(find(&out, &enc.data).is_some());
}

#[test]
fn embed_png_truecolour() {
    let (w, h) = (13usize, 9usize);
    let mut rows = Vec::new();
    for y in 0..h {
        rows.push(0);
        for x in 0..w {
            rows.extend_from_slice(&[(x * 17) as u8, (y * 23) as u8, (x + y) as u8]);
        }
    }
    let png = png_file((w as u32, h as u32, 8, 2), 0, &[], &rows);
    let (enc, _) = embed_png(&png, &lim()).unwrap();
    assert_eq!(enc.color_space.name, "DeviceRGB");
    assert_eq!(enc.decode_parms.unwrap().colors, 3);
    build_one(enc, 72.0);
}

#[test]
fn embed_png_declares_dpi_from_phys() {
    let png = insert_phys(&small_png(), 11811, 11811);
    let (_, dpi) = embed_png(&png, &lim()).unwrap();
    // 11811 px/m = 299.9994, as img2pdf rounds it.
    assert_eq!(dpi, Resolution { x: 300.0, y: 300.0 });
}

/// `TestEmbedPNGRefusesOversizedImage` (byb-37h): an IHDR declaring far more pixels than the
/// cap is refused from the header, before the image is built.
#[test]
fn embed_png_refuses_oversized_image() {
    let mut png = PNG_SIGNATURE.to_vec();
    png.extend(ihdr(2_147_483_647, 1, 8, 3, 0));
    png.extend(png_chunk("PLTE", &[0, 0, 0, 255, 255, 255]));
    png.extend(png_chunk("IDAT", &[7u8; 100]));
    png.extend(png_chunk("IEND", &[]));
    let err = embed_png(&png, &lim()).unwrap_err();
    assert!(is_limit(&err, "encoded-image-pixels"), "{err:?}");
    assert_eq!(err.class(), "limit/encoded-image-pixels");
}

#[test]
fn embed_png_refuses_alpha() {
    let rows = rows8(16, 4, |x, _| x as u8);
    for ctype in [4u8, 6] {
        let png = png_file((4, 4, 8, ctype), 0, &[], &rows);
        assert!(embed_png(&png, &lim()).is_err(), "colour type {ctype}");
    }
}

#[test]
fn embed_png_refuses_sixteen_bit() {
    let png = png_file((4, 4, 16, 0), 0, &[], &rows8(8, 4, |_, _| 0));
    assert!(embed_png(&png, &lim()).is_err());
}

#[test]
fn embed_png_refuses_interlaced() {
    let png = png_file((4, 4, 8, 0), 1, &[], &rows8(4, 4, |_, _| 0));
    let err = embed_png(&png, &lim()).unwrap_err();
    assert!(err.to_string().contains("interlaced"), "{err}");
}

#[test]
fn embed_png_indexed() {
    let plte = png_chunk("PLTE", &[10, 20, 30, 200, 100, 50, 0, 0, 0]);
    let png = png_file(
        (6, 5, 8, 3),
        0,
        &[plte],
        &rows8(6, 5, |x, y| ((x + y) % 3) as u8),
    );
    let (enc, _) = embed_png(&png, &lim()).unwrap();
    assert_eq!(enc.color_space.name, "Indexed");
    assert_eq!(enc.color_space.base, "DeviceRGB");
    assert_eq!(enc.color_space.hi_val, 2);
    assert_eq!(enc.color_space.lookup, [10, 20, 30, 200, 100, 50, 0, 0, 0]);
    build_one(enc, 72.0);
}

#[test]
fn embed_png_refuses_trns() {
    let plte = png_chunk("PLTE", &[0, 0, 0, 255, 255, 255]);
    let trns = png_chunk("tRNS", &[0]);
    let png = png_file((4, 4, 8, 3), 0, &[plte, trns], &rows8(4, 4, |_, _| 0));
    let err = embed_png(&png, &lim()).unwrap_err();
    assert!(err.to_string().contains("tRNS"), "{err}");
}

#[test]
fn embed_png_refuses_bad_bit_depth_colour_type_combo() {
    // Colour type 2 never allows bit depth 1.
    let png = png_file((4, 4, 1, 2), 0, &[], &rows8(2, 4, |_, _| 0));
    assert!(embed_png(&png, &lim()).is_err());
}

#[test]
fn embed_png_refuses_bad_crc_before_idat() {
    let exif = png_chunk("eXIf", b"MM\x00\x2a\x00\x00\x00\x08\x00\x00");
    let good = png_before_idat(std::slice::from_ref(&exif));
    embed_png(&good, &lim()).unwrap();
    let mut bad = good;
    let i = find(&bad, b"eXIf").unwrap();
    bad[i + 4 + 10 + 1] ^= 1; // a CRC byte of the eXIf chunk
    let err = embed_png(&bad, &lim()).unwrap_err();
    assert!(err.to_string().contains("bad CRC"), "{err}");
}

#[test]
fn embed_png_refuses_garbage() {
    for data in [
        &b""[..],
        b"\x89PNG",
        b"not a png at all",
        &small_png()[..40],
    ] {
        assert!(embed_png(data, &lim()).is_err());
    }
}

#[test]
fn embed_jpeg_carries_the_source_bytes_verbatim() {
    let (w, h) = (21i64, 15i64);
    let jpg = gray_jpeg(w, h, 90, |x, y| ((x * 11 + y * 3) % 256) as u8);
    let (enc, dpi) = embed_jpeg(&jpg, &lim()).unwrap();
    assert_eq!(
        dpi,
        Resolution::default(),
        "Go's jpeg writer emits no JFIF APP0 segment"
    );
    assert_eq!((enc.width, enc.height, enc.bpc), (w, h, 8));
    assert_eq!(enc.color_space.name, "DeviceGray");
    assert_eq!(enc.filter, "DCTDecode");
    assert_eq!(enc.data, jpg);
    let out = build_one(enc, 72.0);
    assert!(find(&out, &jpg).is_some());
}

/// `TestJPEGDPIReadsTheDeclaredDensity`: a JFIF APP0 segment built by hand.
#[test]
fn jpeg_dpi_reads_the_declared_density() {
    let jfif = |units: u8, density: u16| {
        let mut seg = b"JFIF\x00".to_vec();
        seg.extend_from_slice(&[1, 2, units]);
        seg.extend_from_slice(&density.to_be_bytes());
        seg.extend_from_slice(&density.to_be_bytes());
        seg.extend_from_slice(&[0, 0]); // thumbnail w/h
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0];
        out.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        out.extend(seg);
        out
    };
    let dpi = |d: &[u8]| jpeg_dpi(d, &lim()).unwrap();
    assert_eq!(dpi(&jfif(1, 300)), Resolution { x: 300.0, y: 300.0 });
    assert_eq!(dpi(&jfif(2, 100)), Resolution { x: 254.0, y: 254.0 });
    assert_eq!(dpi(&jfif(0, 4)), Resolution::default());
}

/// `TestEmbedJPEGRefusesLossless`: the smallest possible SOF3 frame header.
#[test]
fn embed_jpeg_refuses_lossless() {
    let data = [
        0xFF, 0xD8, // SOI
        0xFF, 0xC3, 0x00, 0x0B, 0x08, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x11, 0x00, // SOF3
        0xFF, 0xDA, 0x00, 0x02, // SOS (empty)
    ];
    assert!(embed_jpeg(&data, &lim()).is_err());
}

#[test]
fn embed_jpeg_colour() {
    let mut g = crate::image::Rgba::new(Rect::new(0, 0, 19, 12), &lim()).unwrap();
    for y in 0..12usize {
        for x in 0..19usize {
            let o = y * g.stride + x * 4;
            g.pix[o..o + 4].copy_from_slice(&[(x * 13) as u8, (y * 21) as u8, (x + y) as u8, 255]);
        }
    }
    let jpg = jpeg_write::encode(&Image::Rgba(g), 95, &lim().mem(), &no_cancel()).unwrap();
    let (enc, _) = embed_jpeg(&jpg, &lim()).unwrap();
    assert_eq!(enc.color_space.name, "DeviceRGB");
}

// --- build_codecs_test.go ------------------------------------------------------------------

/// `img2pdfEntry`.
#[derive(Default, Clone)]
struct Entry {
    mediabox: Vec<f64>,
    rotate: i64,
    width: i64,
    height: i64,
    bpc: i64,
    decode: Vec<f64>,
    error: String,
}

fn number(v: &Value) -> f64 {
    match v {
        Value::Number(s) => s.parse().unwrap(),
        other => panic!("not a number: {other:?}"),
    }
}

/// `img2pdfOracle` and `orientationOracle`: `dir` is "" or "r4/" or "r5/".
fn oracle(dir: &str) -> Vec<(String, Entry)> {
    let raw = fixture(&format!("{dir}oracle.json"));
    let v = json::parse(&raw, &lim()).unwrap();
    let Value::Object(members) = &v else {
        panic!("oracle is not an object");
    };
    members
        .iter()
        .map(|(name, e)| {
            let Value::Object(fields) = e else {
                panic!("{name}: not an object");
            };
            let mut en = Entry::default();
            for (k, val) in fields {
                let list = |v: &Value| match v {
                    Value::Array(items) => items.iter().map(number).collect::<Vec<f64>>(),
                    _ => Vec::new(),
                };
                match k.as_str() {
                    "mediabox" => en.mediabox = list(val),
                    "decode" => en.decode = list(val),
                    "rotate" => en.rotate = number(val) as i64,
                    "width" => en.width = number(val) as i64,
                    "height" => en.height = number(val) as i64,
                    "bpc" => en.bpc = number(val) as i64,
                    "error" => {
                        if let Value::String { text, .. } = val {
                            en.error = text.clone();
                        }
                    }
                    _ => {}
                }
            }
            (name.clone(), en)
        })
        .collect()
}

fn oracle_entry(dir: &str, name: &str) -> Entry {
    oracle(dir)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e)
        .unwrap_or_default()
}

#[test]
fn embed_jpeg_cmyk_matches_img2pdf() {
    let mut adobe_ycck = fixture("cmyk.jpg");
    // The Adobe APP14 transform byte: 0 CMYK, 2 YCCK.
    let i = find(&adobe_ycck, b"Adobe").unwrap();
    adobe_ycck[i + 11] = 2;
    for (name, data, want) in [
        (
            "cmyk.jpg",
            fixture("cmyk.jpg"),
            oracle_entry("", "cmyk.jpg"),
        ),
        (
            "cmyk-noadobe.jpg",
            fixture("cmyk-noadobe.jpg"),
            oracle_entry("", "cmyk-noadobe.jpg"),
        ),
        // img2pdf treats both Adobe transforms alike.
        ("ycck", adobe_ycck, oracle_entry("", "cmyk.jpg")),
    ] {
        let (img, _) = embed_jpeg(&data, &lim()).unwrap();
        assert_eq!(img.color_space.name, "DeviceCMYK", "{name}");
        assert_eq!(img.filter, "DCTDecode", "{name}");
        assert_eq!(
            (img.bpc, img.width, img.height),
            (want.bpc, want.width, want.height),
            "{name}"
        );
        assert_eq!(img.data, data, "{name}");
        assert_eq!(img.decode, want.decode, "{name}");
        let out = build_one(img, 72.0);
        let d = image_dict(&out, "DCTDecode");
        assert!(d.contains("/ColorSpace /DeviceCMYK"), "{name}: {d}");
        assert_eq!(
            d.contains("/Decode [1 0 1 0 1 0 1 0]"),
            !want.decode.is_empty(),
            "{name}: {d}"
        );
        assert!(
            find(&out, &data).is_some(),
            "{name}: the JPEG was not carried verbatim"
        );
    }
}

#[test]
fn build_pdf_refuses_bad_decode_array() {
    let (mut img, _) = embed_jpeg(&fixture("cmyk.jpg"), &lim()).unwrap();
    for d in [
        vec![1.0, 0.0],
        vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0],
        vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, f64::NAN],
    ] {
        img.decode = d.clone();
        let page = BuildPage {
            image: img.clone(),
            dpi: 72.0,
            ..BuildPage::default()
        };
        assert!(
            build_pdf(&[page], &lim(), &no_cancel()).is_err(),
            "Decode {d:?} was written"
        );
    }
}

#[test]
fn embed_jp2_matches_img2pdf() {
    for name in ["rgb.jp2", "gray.jp2"] {
        let data = fixture(name);
        let want = oracle_entry("", name);
        let (img, _) = embed_jp2(&data, &lim()).unwrap();
        assert_eq!(img.filter, "JPXDecode");
        assert_eq!(
            (img.bpc, img.width, img.height),
            (want.bpc, want.width, want.height),
            "{name}"
        );
        assert_eq!(img.data, data);
        let cs = if name == "rgb.jp2" {
            "DeviceRGB"
        } else {
            "DeviceGray"
        };
        assert_eq!(img.color_space.name, cs);
        let out = build_one(img, 96.0);
        assert!(
            find(&out, &data).is_some(),
            "the JP2 file was not carried verbatim"
        );
        let d = image_dict(&out, "JPXDecode");
        assert!(d.contains(&format!("/ColorSpace /{cs}")), "{d}");
    }
}

#[test]
fn embed_jp2_refuses_what_img2pdf_refuses() {
    assert!(!oracle_entry("", "cmyk.jp2").error.is_empty());
    assert!(embed_jp2(&fixture("cmyk.jp2"), &lim()).is_err());
}

/// `jp2`: assembles a JP2 file from boxes, so the parser's edges are testable without
/// codestream bytes.
fn jp2(w: u32, h: u32, bpc: u8, enum_cs: u32, extra: &[u8]) -> Vec<u8> {
    let jbox = |typ: &str, body: &[&[u8]]| {
        let b = body.concat();
        let mut out = ((8 + b.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(typ.as_bytes());
        out.extend(b);
        out
    };
    let mut ihdr = h.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&[0, 3, bpc, 7, 0, 0]);
    let mut colr = vec![1, 0, 0];
    colr.extend_from_slice(&enum_cs.to_be_bytes());
    let jp2h = jbox(
        "jp2h",
        &[&jbox("ihdr", &[&ihdr]), &jbox("colr", &[&colr]), extra],
    );
    [
        jbox("jP  ", &[&[0x0D, 0x0A, 0x87, 0x0A]]),
        jbox("ftyp", &[b"jp2 ", &[0; 4], b"jp2 "]),
        jp2h,
        jbox("jp2c", &[&[0xFF, 0x4F, 0xFF, 0xD9]]),
    ]
    .concat()
}

#[test]
fn embed_jp2_parses_boxes() {
    // A resc box: 1969 / 1 pixels per metre vertically (50 dpi) and 3937 / 1 horizontally
    // (100 dpi), 10^0.
    let mut resc = 1969u16.to_be_bytes().to_vec();
    resc.extend_from_slice(&[0, 1, 0x0F, 0x61, 0, 1, 0, 0]);
    let mut res = (8u32 + 8 + 10).to_be_bytes().to_vec();
    res.extend_from_slice(b"res ");
    res.extend_from_slice(&(8u32 + 10).to_be_bytes());
    res.extend_from_slice(b"resc");
    res.extend(resc);

    let (img, dpi) = embed_jp2(&jp2(30, 20, 7, 17, &res), &lim()).unwrap();
    assert_eq!((img.width, img.height, img.bpc), (30, 20, 8));
    assert_eq!(img.color_space.name, "DeviceGray");
    assert_eq!(
        dpi,
        Resolution { x: 100.0, y: 50.0 },
        "3937 and 1969 px/m, as img2pdf rounds them"
    );
    let ok = jp2(1, 1, 7, 16, &[]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("no-signature", ok[12..].to_vec()),
        ("truncated", ok[..40].to_vec()),
        (
            "icc",
            replace_first(&ok, &[1, 0, 0, 0, 0, 0, 16], &[2, 0, 0, 0, 0, 0, 16]),
        ),
        ("cmyk", jp2(1, 1, 7, 12, &[])),
        ("sycc", jp2(1, 1, 7, 18, &[])),
        ("zero-width", jp2(0, 1, 7, 16, &[])),
        ("per-comp-bpc", jp2(1, 1, 0xFF, 16, &[])),
    ];
    for (name, data) in cases {
        assert!(embed_jp2(&data, &lim()).is_err(), "{name}: accepted");
    }
}

#[test]
fn embed_jp2_limits_before_anything() {
    // A forged ihdr: 4 billion square pixels in a 100-byte file.
    let err = embed_jp2(&jp2(1 << 31, 1 << 31, 7, 16, &[]), &lim()).unwrap_err();
    assert!(is_limit(&err, "encoded-image-pixels"), "{err:?}");
    let small = Limits {
        max_encoded_image_pixels: 99,
        ..Limits::default()
    };
    let err = embed_jp2(&jp2(100, 100, 7, 16, &[]), &small).unwrap_err();
    assert!(
        is_limit(&err, "encoded-image-pixels"),
        "configured cap not applied: {err:?}"
    );
}

#[test]
fn build_pdf_refuses_jpx_that_is_not_jp2() {
    let img = EncodedImage {
        width: 1,
        height: 1,
        bpc: 8,
        color_space: ColorSpace {
            name: "DeviceRGB".to_string(),
            ..ColorSpace::default()
        },
        filter: "JPXDecode".to_string(),
        data: vec![0],
        ..EncodedImage::default()
    };
    let page = BuildPage {
        image: img,
        dpi: 72.0,
        ..BuildPage::default()
    };
    assert!(build_pdf(&[page], &lim(), &no_cancel()).is_err());
}

/// `withOrientation`: splices an Exif APP1 holding only the orientation tag after the SOI of a
/// small JPEG.
fn with_orientation(o: u16, little: bool) -> Vec<u8> {
    let jpg = gray_jpeg(8, 8, 75, |_, _| 0);
    let mut tiff: Vec<u8> = if little {
        b"II\x2a\x00".to_vec()
    } else {
        b"MM\x00\x2a".to_vec()
    };
    let u16b = |v: u16| {
        if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let u32b = |v: u32| {
        if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    tiff.extend_from_slice(&u32b(8));
    tiff.extend_from_slice(&u16b(1));
    tiff.extend_from_slice(&u16b(0x0112));
    tiff.extend_from_slice(&u16b(3));
    tiff.extend_from_slice(&u32b(1));
    tiff.extend_from_slice(&u16b(o));
    tiff.extend_from_slice(&[0; 6]);
    let seg = [EXIF_PREFIX, &tiff].concat();
    let mut app1 = vec![0xFF, 0xE1];
    app1.extend_from_slice(&((2 + seg.len()) as u16).to_be_bytes());
    app1.extend(seg);
    [&jpg[..2], &app1[..], &jpg[2..]].concat()
}

#[test]
fn exif_rotation_matches_img2pdf() {
    for o in 0..=9u16 {
        let want = oracle_entry("", &format!("exif-{o}.jpg"));
        for little in [false, true] {
            let got = exif_rotation(&with_orientation(o, little), &lim());
            if !want.error.is_empty() {
                assert!(
                    got.is_err(),
                    "orientation {o} (little={little}): got {got:?}; img2pdf refuses ({})",
                    want.error
                );
                continue;
            }
            assert_eq!(
                got.as_ref().ok().copied(),
                Some(want.rotate as i32),
                "orientation {o} (little={little}): img2pdf wrote /Rotate {}",
                want.rotate
            );
        }
    }
    // The pillow-made file itself, and a file with no Exif at all.
    assert_eq!(exif_rotation(&fixture("exif-6.jpg"), &lim()), Ok(90));
    assert_eq!(exif_rotation(&gray_jpeg(4, 4, 75, |_, _| 0), &lim()), Ok(0));
}

#[test]
fn exif_rotation_mirrored_is_a_typed_refusal() {
    for o in [2u16, 4, 5, 7] {
        let err = exif_rotation(&with_orientation(o, false), &lim()).unwrap_err();
        assert!(err.is_mirrored_orientation(), "orientation {o}: {err:?}");
        assert_eq!(err.class(), "mirrored-orientation");
    }
}

/// Files whose XMP names `tiff:Orientation` outside well-formed XML (a bare
/// `tiff:Orientation="6"`): img2pdf reads them through Pillow's regex, the closed XMP rule
/// calls them ambiguous (rotation 0). r4's `xmp-attr9.jpg` is well formed but says 9: also
/// ambiguous.
const XMP_NON_XML: &[&str] = &[
    "xmp-attr2.jpg",
    "xmp-attr6.jpg",
    "xmp-attr8-300dpi.jpg",
    "xmp-attr9.jpg",
    "xmp6-exif-noorient.jpg",
    "png-xmp2.png",
    "png-xmp6.png",
    "rgba-png-xmp2.png",
    "rgba-png-xmp6.png",
];

/// `exifVariants`: the oracle-only Exif files, `exif-6.jpg` with its orientation entry
/// replaced as `gen.py` does. `oracleInput` is the input an `oracle.json` entry was made from.
fn oracle_input(name: &str) -> Vec<u8> {
    let p = format!("{}/tests/data/img2pdf/{name}", env!("CARGO_MANIFEST_DIR"));
    if let Ok(b) = fs::read(p) {
        return b;
    }
    let repl: Vec<u8> = match name {
        "exif-long.jpg" => vec![1, 0x12, 0, 4, 0, 0, 0, 1, 0, 0, 0, 6],
        "exif-short0.jpg" => vec![1, 0x12, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0],
        "exif-short2.jpg" => vec![1, 0x12, 0, 3, 0, 0, 0, 2, 0, 6, 0, 1],
        _ => {
            let o: u8 = name
                .strip_prefix("exif-")
                .and_then(|s| s.strip_suffix(".jpg"))
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("no input for oracle entry {name}"));
            vec![1, 0x12, 0, 3, 0, 0, 0, 1, 0, o, 0, 0]
        }
    };
    replace_first(
        &fixture("exif-6.jpg"),
        &[1, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0],
        &repl,
    )
}

/// `TestEmbedPageBoxMatchesImg2pdf`: builds a page from every accepted oracle entry the way a
/// caller does (the resolution Embed returns, 96 when it declares none, `exif_rotation`) and
/// holds its MediaBox and `/Rotate` to what img2pdf wrote; an entry img2pdf refuses must be
/// refused.
#[test]
fn embed_page_box_matches_img2pdf() {
    for (name, mut want) in oracle("") {
        let data = oracle_input(&name);
        let embedded = if name.ends_with(".jpg") {
            embed_jpeg(&data, &lim())
        } else if name.ends_with(".jp2") {
            embed_jp2(&data, &lim())
        } else if name.starts_with("rgba-") {
            transcode_png(&data[..], &lim(), &no_cancel())
        } else if name.ends_with(".png") {
            embed_png(&data, &lim())
        } else {
            panic!("no Embed function for oracle entry {name}");
        };
        let mut rotate = 0;
        let (mut err, mut built) = (None, None);
        match embedded {
            Ok(v) => built = Some(v),
            Err(e) => err = Some(e),
        }
        if err.is_none() {
            match exif_rotation(&data, &lim()) {
                Ok(r) => rotate = r,
                Err(e) => err = Some(e),
            }
        }
        if XMP_NON_XML.contains(&name.as_str()) {
            // ambiguous by rule: rotation 0, not img2pdf's
            assert!(
                err.is_none() && rotate == 0,
                "{name}: rotation {rotate}, {err:?}; want 0, nil"
            );
            if !want.error.is_empty() {
                continue;
            }
            want.rotate = 0;
        }
        if !want.error.is_empty() {
            assert!(
                err.is_some(),
                "{name}: accepted; img2pdf refuses it ({})",
                want.error
            );
            continue;
        }
        if let Some(e) = err {
            panic!("{name}: {e}");
        }
        let (img, mut res) = built.unwrap();
        if res == Resolution::default() {
            res = Resolution { x: 96.0, y: 96.0 };
        }
        let out = build_pdf(
            &[BuildPage {
                image: img,
                dpi: res.x,
                dpi_y: res.y,
                rotate: i64::from(rotate),
                ..BuildPage::default()
            }],
            &lim(),
            &no_cancel(),
        )
        .unwrap_or_else(|e| panic!("{name}: build_pdf: {e}"));
        let d = page_dict(&out);
        let i = find(d.as_bytes(), b"/MediaBox [")
            .unwrap_or_else(|| panic!("{name}: no MediaBox in {d}"));
        let rest = &d[i + "/MediaBox [".len()..];
        let boxed: Vec<f64> = rest[..rest.find(']').unwrap()]
            .split_whitespace()
            .map(|f| f.parse().unwrap())
            .collect();
        assert_eq!(
            boxed.len(),
            want.mediabox.len(),
            "{name}: MediaBox {boxed:?}; img2pdf wrote {:?}",
            want.mediabox
        );
        for (g, w) in boxed.iter().zip(&want.mediabox) {
            assert!(
                (g - w).abs() <= 1e-4,
                "{name}: MediaBox {boxed:?}; img2pdf wrote {:?} (from {res:?} dpi)",
                want.mediabox
            );
        }
        if name == "exif-dup-1then6.jpg" || name == "exif-two-segments.jpg" {
            // ambiguous: rotation 0 by rule
            assert!(
                !d.contains("/Rotate"),
                "{name}: ambiguous file got a rotated page {d}"
            );
            continue;
        }
        let got = d.contains(&format!("/Rotate {}", want.rotate));
        assert!(
            got == (want.rotate != 0) && !(want.rotate == 0 && d.contains("/Rotate")),
            "{name}: page {d}; img2pdf wrote /Rotate {}",
            want.rotate
        );
    }
}

/// `TestEXIFRotationOracleFiles`: `exif_rotation` against img2pdf on every `exif-*` oracle
/// entry, the entries Pillow reads leniently (LONG, empty, two values) among them.
#[test]
fn exif_rotation_oracle_files() {
    for (name, want) in oracle("") {
        if !name.starts_with("exif-") {
            continue;
        }
        let data = oracle_input(&name);
        let got = exif_rotation(&data, &lim());
        if name == "exif-dup-1then6.jpg" || name == "exif-two-segments.jpg" {
            // repeated metadata is ambiguous, rotation 0
            assert_eq!(exif_orientation(&data, &lim()), Ok(AMBIGUOUS), "{name}");
            continue;
        }
        if !want.error.is_empty() {
            assert!(
                got.is_err(),
                "{name}: got {got:?}; img2pdf refuses ({})",
                want.error
            );
        } else {
            assert_eq!(got, Ok(want.rotate as i32), "{name}");
        }
    }
}

// --- orientation_test.go -------------------------------------------------------------------

/// `orientationOf`: what a caller learns from a file: `exif_orientation`, and the Embed
/// function's own refusal if it has one.
fn orientation_of(name: &str, data: &[u8]) -> Result<Orientation> {
    let o = exif_orientation(data, &lim())?;
    if name.starts_with("rgba-") {
        // embed_png refuses alpha
    } else if name.ends_with(".png") {
        embed_png(data, &lim())?;
    } else {
        embed_jpeg(data, &lim())?;
    }
    Ok(o)
}

#[test]
fn orientation_round4_matches_img2pdf() {
    for (name, want) in oracle("r4/") {
        let data = fixture(&format!("r4/{name}"));
        let r = orientation_of(&name, &data);
        if name == "xmp-attr9.jpg" {
            assert_eq!(r, Ok(AMBIGUOUS), "{name}: want ambiguous");
        } else if name.starts_with("rgba-")
            && !want.error.is_empty()
            && !want.error.contains("TIFF")
        {
            // pHYs, page size: embed_png's and build_pdf's refusals, tested on the grey twin
        } else if name.contains("aspect-big") {
            // build_pdf's page size refusal, not orientation
        } else if !want.error.is_empty() {
            assert!(
                r.is_err(),
                "{name}: got {r:?}; img2pdf refuses ({})",
                want.error
            );
        } else if name == "png-exif-afteridat6.png" || name == "rgba-png-exif-afteridat6.png" {
            // img2pdf reads an eXIf after the IDAT; the rule does not.
            assert_eq!(r, Ok(AMBIGUOUS), "{name}: want ambiguous");
        } else {
            assert_eq!(
                r,
                Ok(Orientation {
                    rotate: want.rotate as i32,
                    ambiguous: false
                }),
                "{name}: img2pdf wrote /Rotate {}",
                want.rotate
            );
        }
    }
}

/// Ambiguous: rotation 0 and `ambiguous`. Every other r5 entry img2pdf accepts is well formed
/// and keeps img2pdf's rotation; the rest are parked.
const R5_AMBIGUOUS: &[&str] = &[
    "pre-exif6-post-exif1.png",
    "pre-exif1-post-exif6.png",
    "pre-exifnoor-post-xmp6.png",
    "pre-exifnoor-post-text-exif6.png",
    "text-xmp-noor-then-itxt-badutf8-6.png",
    "itxt-rawprof-badlang6.png",
    "post-badtype-then-exif6.png",
    "post-exif6-badcrc.png",
];

#[test]
fn orientation_round5() {
    for (name, want) in oracle("r5/") {
        let data = fixture(&format!("r5/{name}"));
        let r = orientation_of(&name, &data);
        let base = name.strip_prefix("rgba-").unwrap_or(&name);
        if !want.error.is_empty() {
            assert!(
                r.is_err(),
                "{name}: got {r:?}; img2pdf refuses ({})",
                want.error
            );
        } else if R5_AMBIGUOUS.contains(&base) {
            assert_eq!(r, Ok(AMBIGUOUS), "{name}: want rotation 0 and ambiguous");
        } else {
            assert_eq!(
                r,
                Ok(Orientation {
                    rotate: want.rotate as i32,
                    ambiguous: false
                }),
                "{name}: img2pdf wrote /Rotate {}",
                want.rotate
            );
        }
    }
}

/// `TestEXIFRotationPNGTextBomb`: files of zTXt chunks that each inflate to a megabyte must
/// not be inflated in full: every key is held to `max_png_text_bytes`.
#[test]
fn exif_rotation_png_text_bomb() {
    let zeros = vec![0u8; 1 << 20];
    let other = png_text_chunk("zTXt", "Comment", &zeros);
    let wanted = png_text_chunk("zTXt", "XML:com.adobe.xmp", &zeros);
    let other: Vec<Vec<u8>> = (0..200).map(|_| other.clone()).collect();
    let wanted: Vec<Vec<u8>> = (0..200).map(|_| wanted.clone()).collect();
    let err = exif_rotation(&png_before_idat(&other), &lim()).unwrap_err();
    assert!(is_limit(&err, "png-text-bytes"), "unread keys: {err:?}");
    let small = Limits {
        max_png_text_bytes: 8 << 20,
        ..Limits::default()
    };
    let err = exif_rotation(&png_before_idat(&wanted), &small).unwrap_err();
    assert!(is_limit(&err, "png-text-bytes"), "{err:?}");
}

/// 70 zTXt chunks of 1 MB (a key nobody reads) are 70 MB of text: img2pdf refuses past 64 MiB,
/// so does `exif_orientation`.
#[test]
fn orientation_text_over_budget_parks() {
    let zeros = vec![0u8; 1_000_000];
    let chunks: Vec<Vec<u8>> = (0..70u8)
        .map(|k| {
            let key = format!(
                "C{}{}",
                char::from(b'A' + k % 26),
                char::from(b'a' + k / 26)
            );
            png_text_chunk("zTXt", &key, &zeros)
        })
        .collect();
    let err = exif_orientation(&png_before_idat(&chunks), &lim()).unwrap_err();
    assert!(is_limit(&err, "png-text-bytes"), "{err:?}");
}

/// `tiffOrient`: a little-endian TIFF whose IFD0 has one SHORT Orientation entry per value.
fn tiff_orient(vals: &[u16]) -> Vec<u8> {
    tiff_tag(0x0112, vals)
}

/// `tiffTag`: `tiff_orient` for any tag.
fn tiff_tag(tag: u16, vals: &[u16]) -> Vec<u8> {
    let mut b = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    b.extend_from_slice(&(vals.len() as u16).to_le_bytes());
    for v in vals {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&[0, 0]);
    }
    b.extend_from_slice(&[0, 0, 0, 0]);
    b
}

/// `jpegApp1`.
fn jpeg_app1(payloads: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8];
    for p in payloads {
        out.extend_from_slice(&[0xff, 0xe1, ((p.len() + 2) >> 8) as u8, (p.len() + 2) as u8]);
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

fn xmp_packet(o: u8) -> Vec<u8> {
    cat(&[
        XAP_PREFIX,
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/'><tiff:Orientation>{}</tiff:Orientation></rdf:Description></x:xmpmeta>",
            char::from(b'0' + o)
        )
        .as_bytes(),
    ])
}

fn exif_seg(vals: &[u16]) -> Vec<u8> {
    cat(&[EXIF_PREFIX, &tiff_orient(vals)])
}

/// `tiffDup`: a little-endian TIFF whose IFD0 holds Orientation SHORT=6 and then a second
/// 0x0112 entry of the given type, count and value/offset.
fn tiff_dup(typ: u16, count: u32, val: u32) -> Vec<u8> {
    let mut b = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0];
    b.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
    b.extend_from_slice(&[0x12, 0x01]);
    b.extend_from_slice(&typ.to_le_bytes());
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(&val.to_le_bytes());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b
}

/// `tiffDupAfterOutside`: a little-endian TIFF whose IFD0 holds Orientation SHORT=6, an ASCII
/// entry whose values lie outside the block, and then a second Orientation (SHORT=1), whole
/// or, if truncated, cut after its tag, type and count.
fn tiff_dup_after_outside(truncated: bool) -> Vec<u8> {
    let mut b = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 3, 0];
    b.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
    b.extend_from_slice(&[0x0f, 0x01, 2, 0, 100, 0, 0, 0, 0xf0, 0xff, 0, 0]);
    b.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
    if truncated {
        b.truncate(b.len() - 6);
        return b;
    }
    b.extend_from_slice(&[0, 0, 0, 0]);
    b
}

fn exif_jpeg(tiff: &[u8]) -> Vec<u8> {
    jpeg_app1(&[&cat(&[EXIF_PREFIX, tiff])])
}

fn orient(data: &[u8]) -> Result<Orientation> {
    exif_orientation(data, &lim())
}

fn rot(rotate: i32) -> Result<Orientation> {
    Ok(Orientation {
        rotate,
        ambiguous: false,
    })
}

/// Repeated orientation metadata is ambiguous whatever the values or the order (img2pdf
/// follows Pillow and rotates by one of them).
#[test]
fn orientation_repeated_sources_are_ambiguous() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "two-xmp-1then6",
            jpeg_app1(&[&xmp_packet(1), &xmp_packet(6)]),
        ),
        (
            "two-xmp-6then1",
            jpeg_app1(&[&xmp_packet(6), &xmp_packet(1)]),
        ),
        (
            "two-xmp-6then6",
            jpeg_app1(&[&xmp_packet(6), &xmp_packet(6)]),
        ),
        (
            "two-exif-6then1",
            jpeg_app1(&[&exif_seg(&[6]), &exif_seg(&[1])]),
        ),
        (
            "two-exif-1then6",
            jpeg_app1(&[&exif_seg(&[1]), &exif_seg(&[6])]),
        ),
        ("jpeg-dup-tag", jpeg_app1(&[&exif_seg(&[1, 6])])),
        ("jpeg-dup-tag-6-1", jpeg_app1(&[&exif_seg(&[6, 1])])),
        ("jpeg-dup-tag-6-6", jpeg_app1(&[&exif_seg(&[6, 6])])),
        (
            "png-dup-tag",
            png_before_idat(&[png_chunk("eXIf", &tiff_orient(&[1, 6]))]),
        ),
        // a repeat Pillow would skip (unknown type, no values, values outside the block)
        // still counts.
        ("jpeg-dup-badtype", exif_jpeg(&tiff_dup(99, 1, 0))),
        ("jpeg-dup-count0", exif_jpeg(&tiff_dup(3, 0, 0))),
        ("jpeg-dup-outside", exif_jpeg(&tiff_dup(3, 100, 0xfff0))),
        // the repeat sits after an entry whose values lie outside the block, or is cut short
        // by the block end.
        (
            "jpeg-dup-after-outside",
            exif_jpeg(&tiff_dup_after_outside(false)),
        ),
        (
            "png-dup-after-outside",
            png_before_idat(&[png_chunk("eXIf", &tiff_dup_after_outside(false))]),
        ),
        (
            "jpeg-dup-truncated",
            exif_jpeg(&tiff_dup_after_outside(true)),
        ),
        (
            "png-dup-truncated",
            png_before_idat(&[png_chunk("eXIf", &tiff_dup_after_outside(true))]),
        ),
        (
            "png-dup-badtype",
            png_before_idat(&[png_chunk("eXIf", &tiff_dup(99, 1, 0))]),
        ),
        (
            "png-dup-count0",
            png_before_idat(&[png_chunk("eXIf", &tiff_dup(3, 0, 0))]),
        ),
    ];
    for (name, data) in cases {
        assert_eq!(
            orient(&data),
            Ok(AMBIGUOUS),
            "{name}: want rotation 0 and ambiguous"
        );
    }
    // one source each stays Pillow's.
    assert_eq!(
        orient(&jpeg_app1(&[&exif_seg(&[6]), &xmp_packet(6)])),
        rot(90),
        "single sources"
    );
}

/// Orientation metadata img2pdf reads one way and a reader of the whole file another is
/// ambiguous too: XMP Descriptions that disagree, an IFD0 entry with values outside the block
/// before a valid Orientation, a repeated XMP tag 700.
#[test]
fn orientation_conflicts_are_ambiguous() {
    let xmp2 = |a: char, b: char| {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/'><tiff:Orientation>{a}</tiff:Orientation></rdf:Description><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/'><tiff:Orientation>{b}</tiff:Orientation></rdf:Description></x:xmpmeta>"
        )
    };
    let outside_then_orient = [
        b'I', b'I', 42, 0, 8, 0, 0, 0, 2, 0, 0x0f, 0x01, 2, 0, 100, 0, 0, 0, 0xf0, 0xff, 0, 0,
        0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0,
    ];
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "jpeg-xmp-disagree",
            jpeg_app1(&[&cat(&[XAP_PREFIX, xmp2('1', '6').as_bytes()])]),
        ),
        (
            "png-xmp-disagree",
            png_before_idat(&[png_text_chunk(
                "zTXt",
                "XML:com.adobe.xmp",
                xmp2('1', '6').as_bytes(),
            )]),
        ),
        ("jpeg-outside-then-6", exif_jpeg(&outside_then_orient)),
        (
            "png-outside-then-6",
            png_before_idat(&[png_chunk("eXIf", &outside_then_orient)]),
        ),
    ];
    for (name, data) in cases {
        assert_eq!(
            orient(&data),
            Ok(AMBIGUOUS),
            "{name}: want rotation 0 and ambiguous"
        );
    }
    // agreeing Descriptions and a single tag 700 stay Pillow's.
    assert_eq!(
        orient(&jpeg_app1(&[&cat(&[
            XAP_PREFIX,
            xmp2('6', '6').as_bytes()
        ])])),
        rot(90),
        "agreeing XMP"
    );
    assert_eq!(
        orient(&exif_jpeg(&tiff_tag(700, &[1]))),
        rot(0),
        "single tag 700"
    );
}

/// `ifd0`: a little-endian TIFF whose IFD0 holds the given 12-byte entries, then `data`
/// (offsets in the entries count from the start of the TIFF).
fn ifd0(data: &[u8], entries: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in entries {
        b.extend_from_slice(e);
    }
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(data);
    b
}

fn ent(tag: u16, typ: u16, count: u32, val: u32) -> Vec<u8> {
    let mut e = tag.to_le_bytes().to_vec();
    e.extend_from_slice(&typ.to_le_bytes());
    e.extend_from_slice(&count.to_le_bytes());
    e.extend_from_slice(&val.to_le_bytes());
    e
}

/// Duplicated XMP is ambiguous only when two packets carry an orientation: two without one,
/// next to a valid Orientation=6, are Pillow's and img2pdf's 90.
#[test]
fn orientation_duplicate_xmp() {
    let no_or = cat(&[
        XAP_PREFIX,
        b"<x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta>",
    ]);
    let xml = |o: u8| xmp_packet(o)[XAP_PREFIX.len()..].to_vec();
    let no_xml = no_or[XAP_PREFIX.len()..].to_vec();
    let exif6 = tiff_orient(&[6]);
    // a single-quoted attribute with spaces around the =, which Pillow's regex misses
    let odd = |o: u8| {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/' tiff:Orientation = '{}'/></x:xmpmeta>",
            char::from(b'0' + o)
        )
        .into_bytes()
    };
    // a prefix other than tiff, bound to the tiff namespace
    let pref = |o: u8| {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:t=\"http://ns.adobe.com/tiff/1.0/\" t:Orientation=\"{}\"/></x:xmpmeta>",
            char::from(b'0' + o)
        )
        .into_bytes()
    };
    let pref_pkt = |o: u8| cat(&[XAP_PREFIX, &pref(o)]);
    let cmt =
        b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><!-- tiff:Orientation=\"3\" --></x:xmpmeta>".to_vec();
    let cmt_pkt = cat(&[XAP_PREFIX, &cmt]);
    let odd_pkt = |o: u8| cat(&[XAP_PREFIX, &odd(o)]);
    let exif_seg6 = cat(&[EXIF_PREFIX, &exif6]);
    let ztxt = |body: &[u8]| png_text_chunk("zTXt", "XML:com.adobe.xmp", body);
    let exif_chunk = png_chunk("eXIf", &exif6);
    // IFD0: Orientation 6, then two tag 700 entries
    let tag700 = |a: &[u8], b: &[u8]| {
        let d = (8 + 2 + 3 * 12 + 4) as u32;
        ifd0(
            &cat(&[a, b]),
            &[
                ent(0x112, 3, 1, 6),
                ent(700, 1, a.len() as u32, d),
                ent(700, 1, b.len() as u32, d + a.len() as u32),
            ],
        )
    };
    type Case = (&'static str, Vec<u8>, Vec<u8>, Result<Orientation>);
    let cases: Vec<Case> = vec![
        (
            "two-without-orientation",
            jpeg_app1(&[&exif_seg6, &no_or, &no_or]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&no_xml), ztxt(&no_xml)]),
            rot(90),
        ),
        (
            "two-disagree",
            jpeg_app1(&[&xmp_packet(1), &xmp_packet(6)]),
            png_before_idat(&[ztxt(&xml(1)), ztxt(&xml(6))]),
            Ok(AMBIGUOUS),
        ),
        (
            "two-odd-form-disagree",
            jpeg_app1(&[&odd_pkt(3), &odd_pkt(6)]),
            png_before_idat(&[ztxt(&odd(3)), ztxt(&odd(6))]),
            Ok(AMBIGUOUS),
        ),
        (
            "two-odd-form-agree",
            jpeg_app1(&[&odd_pkt(6), &odd_pkt(6)]),
            png_before_idat(&[ztxt(&odd(6)), ztxt(&odd(6))]),
            Ok(AMBIGUOUS),
        ),
        (
            "exif-two-odd-form",
            jpeg_app1(&[&exif_seg6, &odd_pkt(3), &odd_pkt(3)]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&odd(3)), ztxt(&odd(3))]),
            Ok(AMBIGUOUS),
        ),
        (
            "two-prefixed-disagree",
            jpeg_app1(&[&pref_pkt(3), &pref_pkt(6)]),
            png_before_idat(&[ztxt(&pref(3)), ztxt(&pref(6))]),
            Ok(AMBIGUOUS),
        ),
        (
            "exif-one-prefixed-disagrees",
            jpeg_app1(&[&exif_seg6, &pref_pkt(3)]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&pref(3))]),
            Ok(AMBIGUOUS),
        ),
        (
            "exif-one-odd-form-disagrees",
            jpeg_app1(&[&exif_seg6, &odd_pkt(3)]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&odd(3))]),
            Ok(AMBIGUOUS),
        ),
        (
            "exif-two-comment-only",
            jpeg_app1(&[&exif_seg6, &cmt_pkt, &cmt_pkt]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&cmt), ztxt(&cmt)]),
            Ok(AMBIGUOUS),
        ),
        (
            "two-agree",
            jpeg_app1(&[&xmp_packet(6), &xmp_packet(6)]),
            png_before_idat(&[ztxt(&xml(6)), ztxt(&xml(6))]),
            Ok(AMBIGUOUS),
        ),
        (
            "one-disagrees-with-exif",
            jpeg_app1(&[&exif_seg6, &xmp_packet(3), &no_or]),
            png_before_idat(&[exif_chunk.clone(), ztxt(&xml(3)), ztxt(&no_xml)]),
            Ok(AMBIGUOUS),
        ),
        (
            "tag700-two-without",
            exif_jpeg(&tag700(&no_xml, &no_xml)),
            png_before_idat(&[png_chunk("eXIf", &tag700(&no_xml, &no_xml))]),
            rot(90),
        ),
        (
            "tag700-two-disagree",
            exif_jpeg(&tag700(&xml(1), &xml(6))),
            png_before_idat(&[png_chunk("eXIf", &tag700(&xml(1), &xml(6)))]),
            Ok(AMBIGUOUS),
        ),
    ];
    for (name, jpeg, png, want) in cases {
        assert_eq!(orient(&jpeg), want, "{name}/jpeg");
        assert_eq!(orient(&png), want, "{name}/png");
    }
}

/// A tag 700 XMP is read by a reader of the whole file, not by Pillow: it conflicts with the
/// IFD0 Orientation, and a lost one changes nothing.
#[test]
fn orientation_tag700_xmp_and_malformed_orientation() {
    let xmp = |o: &str| {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/'><tiff:Orientation>{o}</tiff:Orientation></rdf:Description></x:xmpmeta>"
        )
        .into_bytes()
    };
    let data_off = (8 + 2 + 3 * 12 + 4) as u32; // after a three-entry IFD0
    let orient6 = ent(0x112, 3, 1, 6);
    let outside = ent(0x10f, 2, 100, 0xfff0);
    let two = (8 + 2 + 2 * 12 + 4) as u32;
    let one = (8 + 2 + 12 + 4) as u32;
    let cases: Vec<(&str, Vec<u8>, Orientation)> = vec![
        (
            "orient6-outside-tag700",
            ifd0(
                &xmp("1"),
                &[
                    orient6.clone(),
                    outside.clone(),
                    ent(700, 1, xmp("1").len() as u32, data_off),
                ],
            ),
            AMBIGUOUS,
        ),
        (
            "orient6-outside-tag700-agrees",
            ifd0(
                &xmp("6"),
                &[
                    orient6.clone(),
                    outside.clone(),
                    ent(700, 1, xmp("6").len() as u32, data_off),
                ],
            ),
            Orientation {
                rotate: 90,
                ambiguous: false,
            },
        ),
        (
            "orient6-tag700-agrees",
            ifd0(
                &xmp("6"),
                &[orient6.clone(), ent(700, 1, xmp("6").len() as u32, two)],
            ),
            Orientation {
                rotate: 90,
                ambiguous: false,
            },
        ),
        (
            "orient6-tag700-says3",
            ifd0(
                &xmp("3"),
                &[orient6.clone(), ent(700, 1, xmp("3").len() as u32, two)],
            ),
            AMBIGUOUS,
        ),
        (
            "no-orient-tag700-says6",
            ifd0(&xmp("6"), &[ent(700, 1, xmp("6").len() as u32, one)]),
            Orientation {
                rotate: 90,
                ambiguous: false,
            },
        ),
        (
            "orient-itself-outside",
            ifd0(&[], &[ent(0x112, 3, 3, 0xfff0)]),
            AMBIGUOUS,
        ),
    ];
    for (name, tiff, want) in cases {
        assert_eq!(orient(&exif_jpeg(&tiff)), Ok(want), "{name}/jpeg");
        assert_eq!(
            orient(&png_before_idat(&[png_chunk("eXIf", &tiff)])),
            Ok(want),
            "{name}/png"
        );
    }
}

/// XMP is parsed as XML: references are decoded, the default namespace and elements count, and
/// a packet that is not XML but names Orientation is ambiguous.
#[test]
fn orientation_xmp_parsed() {
    let ns = "http://ns.adobe.com/tiff/1.0/";
    let pk = |body: &str| {
        cat(&[
            XAP_PREFIX,
            format!("<x:xmpmeta xmlns:x='adobe:ns:meta/'>{body}</x:xmpmeta>").as_bytes(),
        ])
    };
    let exif6 = cat(&[EXIF_PREFIX, &tiff_orient(&[6])]);
    let r90 = Orientation {
        rotate: 90,
        ambiguous: false,
    };
    let cases: Vec<(&str, Vec<u8>, Orientation)> = vec![
        (
            "char-ref-agrees",
            pk(&format!(
                "<rdf:Description xmlns:tiff='{ns}' tiff:Orientation='&#54;'/>"
            )),
            r90,
        ),
        (
            "char-ref-disagrees",
            pk(&format!(
                "<rdf:Description xmlns:tiff='{ns}' tiff:Orientation='&#51;'/>"
            )),
            AMBIGUOUS,
        ),
        (
            "default-ns-element",
            pk(&format!(
                "<Description xmlns='{ns}'><Orientation>3</Orientation></Description>"
            )),
            AMBIGUOUS,
        ),
        (
            "element-spaced",
            pk(&format!(
                "<rdf:Description xmlns:t='{ns}'><t:Orientation> 6 </t:Orientation></rdf:Description>"
            )),
            r90,
        ),
        (
            "two-values",
            pk(&format!(
                "<rdf:Description xmlns:t='{ns}' t:Orientation='6'><t:Orientation>3</t:Orientation></rdf:Description>"
            )),
            AMBIGUOUS,
        ),
        (
            "other-ns",
            pk("<rdf:Description xmlns:t='urn:other' t:Orientation='3'/>"),
            AMBIGUOUS,
        ),
        (
            "comment-only",
            pk("<!-- tiff:Orientation=\"3\" -->"),
            AMBIGUOUS,
        ),
        (
            "unparsable-with-orientation",
            pk("<rdf:Description tiff:Orientation='3'>"),
            AMBIGUOUS,
        ),
        ("unparsable-without", pk("<rdf:Description>"), r90),
    ];
    for (name, xmp, want) in cases {
        assert_eq!(orient(&jpeg_app1(&[&exif6, &xmp])), Ok(want), "{name}");
    }
}

/// Malformed XMP that names conflicting orientations stays ambiguous, however it fails to
/// parse.
#[test]
fn orientation_xmp_malformed_conflict() {
    let ns = "http://ns.adobe.com/tiff/1.0/";
    let exif6 = cat(&[EXIF_PREFIX, &tiff_orient(&[6])]);
    let valid = format!(
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='{ns}' tiff:Orientation=\"6\"><tiff:Orientation>3</tiff:Orientation></rdf:Description></x:xmpmeta>"
    );
    let xap = |s: &str| cat(&[XAP_PREFIX, s.as_bytes()]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "no-exif-attrs",
            jpeg_app1(&[&xap("<tiff:Orientation=\"3\"/><tiff:Orientation=\"6\"/>")]),
        ),
        (
            "exif-attrs",
            jpeg_app1(&[
                &exif6,
                &xap("<tiff:Orientation=\"6\"/><tiff:Orientation=\"3\"/>"),
            ]),
        ),
        (
            "exif-attr-element",
            jpeg_app1(&[
                &exif6,
                &xap("<tiff:Orientation=\"6\"/><tiff:Orientation>3</tiff:Orientation>"),
            ]),
        ),
        ("trailing-nul", jpeg_app1(&[&xap(&format!("{valid}\x00"))])),
        (
            "nul-entity-element",
            jpeg_app1(&[&xap(&format!(
                "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='{ns}' tiff:Orientation=\"6\"><tiff:Orientation>&#51;</tiff:Orientation></rdf:Description></x:xmpmeta>\x00"
            ))]),
        ),
        (
            "nul-spaced-element-exif",
            jpeg_app1(&[
                &exif6,
                &xap(&format!(
                    "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='{ns}' tiff:Orientation=\"6\"><tiff:Orientation> 3 </tiff:Orientation></rdf:Description></x:xmpmeta>\x00"
                )),
            ]),
        ),
        (
            "non-xml-mixed-quotes",
            jpeg_app1(&[&xap("<tiff:Orientation=\"6\"/><tiff:Orientation='3'/>")]),
        ),
        (
            "non-xml-mixed-quotes-exif",
            jpeg_app1(&[
                &exif6,
                &xap("<tiff:Orientation=\"6\"/><tiff:Orientation='3'/>"),
            ]),
        ),
        (
            "unbound-prefix",
            jpeg_app1(&[
                &exif6,
                &xap("<x:xmpmeta xmlns:x='adobe:ns:meta/'><a tiff:Orientation=\"3\"/></x:xmpmeta>"),
            ]),
        ),
        (
            "empty-value",
            jpeg_app1(&[
                &exif6,
                &xap(&format!(
                    "<x:xmpmeta xmlns:x='adobe:ns:meta/'><a xmlns:tiff='{ns}' tiff:Orientation=\"\"/></x:xmpmeta>"
                )),
            ]),
        ),
    ];
    for (name, jpeg) in cases {
        assert_eq!(orient(&jpeg), Ok(AMBIGUOUS), "{name}");
    }
}

/// The closed XMP rule's attack forms: a value Pillow's regex cannot see (padded, an entity),
/// a packet cut off after a conflicting element, and deep nesting.
#[test]
fn orientation_xmp_closed_rule() {
    let ns = "http://ns.adobe.com/tiff/1.0/";
    let tag700 = |v: u8| {
        let x = format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='http://ns.adobe.com/tiff/1.0/' tiff:Orientation='{}'/></x:xmpmeta>",
            char::from(48 + v)
        )
        .into_bytes();
        ifd0(&x, &[ent(700, 1, x.len() as u32, (8 + 2 + 12 + 4) as u32)])
    };
    let pk = |body: &str| {
        cat(&[
            XAP_PREFIX,
            format!("<x:xmpmeta xmlns:x='adobe:ns:meta/'>{body}</x:xmpmeta>").as_bytes(),
        ])
    };
    let desc = |attrs: &str, inner: &str| {
        pk(&format!(
            "<rdf:Description xmlns:tiff='{ns}' {attrs}>{inner}</rdf:Description>"
        ))
    };
    // n elements deep in all (x:xmpmeta included), the value in the innermost
    let nest = |n: usize| {
        cat(&[
            XAP_PREFIX,
            format!(
                "{}<b xmlns:t='{ns}' t:Orientation='&#54;'/>{}",
                "<a>".repeat(n - 2),
                "</a>".repeat(n - 2)
            )
            .as_bytes(),
        ])
    };
    let exif6 = cat(&[EXIF_PREFIX, &tiff_orient(&[6])]);
    let trunc = cat(&[
        XAP_PREFIX,
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:Description xmlns:tiff='{ns}' tiff:Orientation=\"6\"><tiff:Orientation>3</tiff:Orientation>"
        )
        .as_bytes(),
    ]);
    let r90 = Orientation {
        rotate: 90,
        ambiguous: false,
    };
    let cases: Vec<(&str, Vec<u8>, Orientation)> = vec![
        (
            "padded-3-vs-exif6",
            jpeg_app1(&[&exif6, &desc("tiff:Orientation=' 3'", "")]),
            AMBIGUOUS,
        ),
        (
            "padded-3-alone",
            jpeg_app1(&[&desc("tiff:Orientation=' 3'", "")]),
            Orientation {
                rotate: 180,
                ambiguous: false,
            },
        ),
        (
            "entity-element-vs-exif6",
            jpeg_app1(&[
                &exif6,
                &desc("", "<tiff:Orientation>&#51;</tiff:Orientation>"),
            ]),
            AMBIGUOUS,
        ),
        (
            "entity-attr-vs-exif6",
            jpeg_app1(&[&exif6, &desc("tiff:Orientation='&#51;'", "")]),
            AMBIGUOUS,
        ),
        (
            "entity-attr-regex-agrees",
            jpeg_app1(&[
                &exif6,
                &pk(&format!(
                    "<!-- tiff:Orientation=\"6\" --><rdf:Description xmlns:tiff='{ns}' tiff:Orientation='&#54;'/>"
                )),
            ]),
            r90,
        ),
        (
            "entity-attr-regex-says-other",
            jpeg_app1(&[&pk(&format!(
                "<!-- tiff:Orientation=\"6\" --><rdf:Description xmlns:tiff='{ns}' tiff:Orientation='&#51;'/>"
            ))]),
            AMBIGUOUS,
        ),
        ("truncated-conflict", jpeg_app1(&[&trunc]), AMBIGUOUS),
        (
            "truncated-conflict-exif6",
            jpeg_app1(&[&exif6, &trunc]),
            AMBIGUOUS,
        ),
        ("nested-70-agrees", jpeg_app1(&[&nest(70)]), r90),
        (
            "nested-70-agrees-exif6",
            jpeg_app1(&[&exif6, &nest(70)]),
            r90,
        ),
        (
            "nested-over-bound",
            jpeg_app1(&[&nest(XMP_MAX_NESTING + 2)]),
            AMBIGUOUS,
        ),
        (
            "second-packet-without-word-ignored",
            jpeg_app1(&[&desc("tiff:Orientation='6'", ""), &pk("<a/>")]),
            r90,
        ),
        (
            "second-packet-without-word-ignored-exif6",
            jpeg_app1(&[&exif6, &desc("tiff:Orientation='6'", ""), &pk("<a/>")]),
            r90,
        ),
        (
            "nul-padded",
            jpeg_app1(&[&cat(&[&desc("tiff:Orientation='6'", ""), &[0, 0]])]),
            AMBIGUOUS,
        ),
        (
            "plus-6",
            jpeg_app1(&[&desc("tiff:Orientation='+6'", "")]),
            AMBIGUOUS,
        ),
        (
            "zero-padded-6",
            jpeg_app1(&[&desc("tiff:Orientation='06'", "")]),
            AMBIGUOUS,
        ),
        (
            "nbsp-6",
            jpeg_app1(&[&desc("tiff:Orientation='&#160;6'", "")]),
            AMBIGUOUS,
        ),
        (
            "tag700-and-packet",
            jpeg_app1(&[
                &cat(&[EXIF_PREFIX, &tag700(6)]),
                &desc("tiff:Orientation='6'", ""),
            ]),
            AMBIGUOUS,
        ),
    ];
    for (name, jpeg, want) in cases {
        assert_eq!(orient(&jpeg), Ok(want), "{name}");
    }
}

#[test]
fn exif_rotation_never_panics_on_truncations() {
    // A cheap stand-in for FuzzEXIFRotation over the committed fixtures: every prefix of a
    // few files must give a result, not a panic.
    for name in [
        "exif-6.jpg",
        "png-exif6.png",
        "png-xmp6.png",
        "exif-two-segments.jpg",
    ] {
        let data = fixture(name);
        for n in 0..data.len().min(600) {
            let _ = exif_rotation(&data[..n], &lim());
            let _ = embed_jpeg(&data[..n], &lim());
            let _ = embed_png(&data[..n], &lim());
            let _ = embed_jp2(&data[..n], &lim());
        }
    }
}
