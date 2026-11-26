//! Ports of byblos `quantize_test.go` and `quantize_indexed_test.go`, without the tests that
//! need an external tool (pdfimages, qpdf, pngquant) or the PDF layer (`ReplaceImage`), which
//! are left out: `TestQuantizeIndexedRoundTripPixelsSurvivePdfimages`,
//! `...AllBitDepthsSurviveQpdf`, the `ReplaceImage` step of `...ColorSpaceIsValidIndexed`, and
//! `TestQuantizePNGRateDistortionAgainstPngquant`. `TestQuantizeIndexedMultipleIDATNotTruncated`
//! uses a different random fixture (not Go's `math/rand` stream) and so a different IDAT split.

use super::test_corpus::{
    IMAGE_H, IMAGE_W, Lcg, gradient, lim, new_rgba, no_cancel, photo, psnr_rgb, scanjpeg, scanpage,
    set_rgba,
};
use super::*;
use crate::image::{Nrgba, png_read};

fn qpng(img: &Image, n: usize) -> Result<Vec<u8>> {
    quantize_png(img, n, &lim(), &no_cancel())
}

fn qidx(img: &Image, n: usize) -> Result<EncodedImage> {
    quantize_indexed(img, n, &lim(), &no_cancel())
}

fn decode_palette(b: &[u8]) -> Paletted {
    match png_read::decode(b, &lim(), &no_cancel()).unwrap() {
        Image::Paletted(p) => p,
        other => panic!("QuantizePNG output decoded as {other:?}; want Paletted"),
    }
}

fn rgb(c: Color) -> (u8, u8, u8) {
    let (r, g, b, _) = c.rgba();
    ((r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8)
}

fn empty_image() -> Image {
    Image::Rgba(new_rgba(0, 0))
}

fn translucent_image() -> Image {
    let mut img = Nrgba::new(Rect::new(0, 0, 4, 4), &lim()).unwrap();
    for p in img.pix.chunks_exact_mut(4) {
        p.copy_from_slice(&[100, 100, 100, 128]);
    }
    Image::Nrgba(img)
}

fn is_malformed_with(e: &Error, prefix: &str) -> bool {
    matches!(e, Error::Malformed(m) if m.starts_with(prefix))
}

#[test]
fn empty_bounds_error() {
    assert!(qpng(&empty_image(), 16).is_err());
    assert!(qidx(&empty_image(), 16).is_err());
}

#[test]
fn colors_out_of_range_error() {
    for n in [0, 1, 257] {
        assert!(qpng(&scanpage(), n).is_err(), "png {n}");
        assert!(qidx(&photo(), n).is_err(), "indexed {n}");
    }
}

#[test]
fn colors_at_bounds_succeed() {
    for n in [2, 256] {
        qpng(&photo(), n).unwrap();
        qidx(&photo(), n).unwrap();
    }
}

#[test]
fn non_opaque_input_errors() {
    assert!(qpng(&translucent_image(), 16).is_err());
    assert!(qidx(&translucent_image(), 16).is_err());
}

#[test]
fn core_errors_carry_the_shared_prefix() {
    const PREFIX: &str = "byblos: quantize:";
    assert!(is_malformed_with(
        &qpng(&empty_image(), 16).unwrap_err(),
        PREFIX
    ));
    assert!(is_malformed_with(&qpng(&photo(), 300).unwrap_err(), PREFIX));
    assert!(is_malformed_with(
        &qidx(&empty_image(), 16).unwrap_err(),
        PREFIX
    ));
    assert!(is_malformed_with(&qidx(&photo(), 300).unwrap_err(), PREFIX));
}

#[test]
fn cancelled_flag_is_honoured() {
    let c = AtomicBool::new(true);
    assert_eq!(
        quantize_png(&scanpage(), 16, &lim(), &c),
        Err(Error::Cancelled)
    );
}

#[test]
fn memory_budget_is_enforced() {
    let l = Limits {
        max_alloc: 1000,
        ..Limits::default()
    };
    assert!(matches!(
        quantize_png(&scanpage(), 16, &l, &no_cancel()),
        Err(Error::Alloc { .. })
    ));
}

#[test]
fn output_is_a_valid_paletted_png() {
    let out = qpng(&photo(), 64).unwrap();
    let pi = decode_palette(&out);
    assert!(pi.palette.len() <= 64);
    assert_eq!((pi.rect.dx(), pi.rect.dy()), (IMAGE_W, IMAGE_H));
}

#[test]
fn palette_size_respected() {
    let pi = decode_palette(&qpng(&photo(), 8).unwrap());
    assert!(pi.palette.len() <= 8);
}

#[test]
fn bit_depth_by_palette_size() {
    for (n, want) in [(2, 1u8), (4, 2), (16, 4), (256, 8)] {
        let out = qpng(&photo(), n).unwrap();
        assert!(out.len() >= 25);
        // The IHDR is the first chunk: 8 signature bytes, 4 length, 4 type, then the depth is the
        // ninth data byte.
        assert_eq!(out[24], want, "colours {n}");
    }
}

#[test]
fn lossless_on_few_colour_image() {
    let img = scanpage();
    let pi = decode_palette(&qpng(&img, 256).unwrap());
    assert_eq!(psnr_rgb(&img, &Image::Paletted(pi)), f64::INFINITY);
}

#[test]
fn deterministic() {
    assert_eq!(qpng(&photo(), 32).unwrap(), qpng(&photo(), 32).unwrap());
}

/// The pixel count of every palette index of a paletted PNG.
fn palette_index_counts(b: &[u8]) -> Vec<usize> {
    let pi = decode_palette(b);
    let mut counts = vec![0; pi.palette.len()];
    for y in 0..pi.rect.dy() {
        for x in 0..pi.rect.dx() {
            counts[usize::from(pi.pix[pi.pix_offset(x, y).unwrap()])] += 1;
        }
    }
    counts
}

#[test]
fn palette_in_population_order() {
    let images = [
        ("Scanpage", scanpage()),
        ("Photo", photo()),
        ("Gradient", gradient()),
        ("Scanjpeg", scanjpeg()),
    ];
    for (name, img) in &images {
        for n in [8, 16, 32, 64, 128, 256] {
            let counts = palette_index_counts(&qpng(img, n).unwrap());
            for i in 1..counts.len() {
                assert!(
                    counts[i] <= counts[i - 1],
                    "{name}/{n}: index {i} has population {}, more than index {}'s {}: {counts:?}",
                    counts[i],
                    i - 1,
                    counts[i - 1]
                );
            }
        }
    }
}

/// The four equal-population tie cases: `first` must sort before `second` under ascending
/// (R, G, B) tuple order. They are chosen so that median cut's own split geometry orders the
/// pair the other way, and so that each channel of the comparator is pinned.
fn tie_cases() -> [(&'static str, [u8; 3], [u8; 3]); 4] {
    [
        ("TieAtR", [10, 10, 250], [200, 10, 10]),
        ("TieAtG", [50, 10, 250], [50, 200, 10]),
        ("TieAtB", [50, 50, 10], [50, 50, 250]),
        ("RAndGDisagree", [10, 200, 100], [200, 10, 100]),
    ]
}

/// 25 pixels in a row: 10 of `first`, 10 of `second`, 5 green.
fn tie_image(first: [u8; 3], second: [u8; 3]) -> Image {
    let green = [100, 200, 100];
    let mut img = new_rgba(25, 1);
    for x in 0..25 {
        let c = match x {
            0..=9 => first,
            10..=19 => second,
            _ => green,
        };
        set_rgba(&mut img, x, 0, [c[0], c[1], c[2], 255]);
    }
    Image::Rgba(img)
}

#[test]
fn png_population_ties_break_by_rgb() {
    for (name, first, second) in tie_cases() {
        let out = qpng(&tie_image(first, second), 3).unwrap();
        let pi = decode_palette(&out);
        assert_eq!(pi.palette.len(), 3, "{name}");
        assert_eq!(palette_index_counts(&out), [10, 10, 5], "{name}");
        let got: Vec<_> = pi.palette.iter().map(|&c| rgb(c)).collect();
        let want = [
            (first[0], first[1], first[2]),
            (second[0], second[1], second[2]),
            (100, 200, 100),
        ];
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn indexed_population_ties_break_by_rgb() {
    for (name, first, second) in tie_cases() {
        let enc = qidx(&tie_image(first, second), 3).unwrap();
        assert_eq!(enc.color_space.hi_val, 2, "{name}");
        let want: Vec<u8> = [first, second, [100, 200, 100]].concat();
        assert_eq!(enc.color_space.lookup, want, "{name}");
    }
}

/// The PSNR `QuantizePNG(Scanjpeg, 64)` produced on main before the palette reorder landed.
#[test]
fn quality_unaffected_by_palette_order() {
    let img = scanjpeg();
    let pi = decode_palette(&qpng(&img, 64).unwrap());
    let got = psnr_rgb(&img, &Image::Paletted(pi));
    assert!((got - 55.224754).abs() <= 5e-7, "PSNR {got}");
}

/// QuantizePNG's PSNR against the original at every point of the colour ladder, measured in Go.
/// Scanpage is absent: it has four colours, so every entry from 4 up is lossless.
const LADDER: &[(&str, usize, f64)] = &[
    ("Gradient", 2, 13.733233480),
    ("Gradient", 4, 17.665946005),
    ("Gradient", 8, 20.534671243),
    ("Gradient", 16, 23.703440693),
    ("Gradient", 32, 26.334065911),
    ("Gradient", 64, 29.684394883),
    ("Gradient", 128, 32.099877360),
    ("Gradient", 256, 35.685541661),
    ("Photo", 2, 10.473265000),
    ("Photo", 4, 12.437580927),
    ("Photo", 8, 16.275573921),
    ("Photo", 16, 17.905666806),
    ("Photo", 32, 19.867158042),
    ("Photo", 64, 22.741621252),
    ("Photo", 128, 24.879549545),
    ("Photo", 256, 27.064433252),
    ("Scanjpeg", 2, 28.657285296),
    ("Scanjpeg", 4, 31.147341919),
    ("Scanjpeg", 8, 42.627377572),
    ("Scanjpeg", 16, 46.633672156),
    ("Scanjpeg", 32, 51.680758833),
    ("Scanjpeg", 64, 55.224754167),
    ("Scanjpeg", 128, 58.758481362),
    ("Scanjpeg", 256, 62.210347508),
];

fn ladder_image(name: &str) -> Image {
    match name {
        "Gradient" => gradient(),
        "Photo" => photo(),
        "Scanjpeg" => scanjpeg(),
        _ => scanpage(),
    }
}

#[test]
fn png_psnr_ladder_pinned() {
    for &(name, n, want) in LADDER {
        let img = ladder_image(name);
        let got = psnr_rgb(
            &img,
            &Image::Paletted(decode_palette(&qpng(&img, n).unwrap())),
        );
        assert!(
            (got - want).abs() <= 5e-7,
            "{name}/{n}: PSNR {got}, want {want}"
        );
    }
}

// --- quantize_indexed ---

#[test]
fn indexed_color_space_is_valid_indexed() {
    let img = photo();
    let want_hi_val = decode_palette(&qpng(&img, 64).unwrap()).palette.len() as i64 - 1;
    let enc = qidx(&img, 64).unwrap();
    assert_eq!(enc.color_space.name, "Indexed");
    assert_eq!(enc.color_space.base, "DeviceRGB");
    assert_eq!(enc.color_space.hi_val, want_hi_val);
    assert_eq!(enc.color_space.lookup.len() as i64, (want_hi_val + 1) * 3);
    assert_eq!((enc.width, enc.height), (IMAGE_W, IMAGE_H));
}

#[test]
fn indexed_lookup_matches_quantize_png_palette() {
    let images = [
        ("Scanpage", scanpage()),
        ("Photo", photo()),
        ("Gradient", gradient()),
        ("Scanjpeg", scanjpeg()),
    ];
    for (name, img) in &images {
        for n in [8, 16, 32, 64, 128, 256] {
            let pal = decode_palette(&qpng(img, n).unwrap()).palette;
            let enc = qidx(img, n).unwrap();
            assert_eq!(enc.color_space.hi_val + 1, pal.len() as i64, "{name}/{n}");
            for (i, &c) in pal.iter().enumerate() {
                let (r, g, b) = rgb(c);
                assert_eq!(
                    enc.color_space.lookup[i * 3..i * 3 + 3],
                    [r, g, b],
                    "{name}/{n} entry {i}"
                );
            }
        }
    }
}

/// Inflates `data` as a zlib stream.
fn inflate(data: &[u8]) -> Vec<u8> {
    deflate::zlib::decode(data, &deflate::Limits::default(), &no_cancel()).unwrap()
}

#[test]
fn indexed_data_is_not_a_png_file() {
    let enc = qidx(&photo(), 64).unwrap();
    assert!(
        !enc.data
            .starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
    );
    assert_eq!(enc.filter, "FlateDecode");
    let p = enc.decode_parms.unwrap();
    let raw = inflate(&enc.data);
    let row_bytes = ((p.columns * p.bits_per_component + 7) / 8) as usize;
    assert_eq!(raw.len(), (row_bytes + 1) * enc.height as usize);
    for row in raw.chunks_exact(row_bytes + 1) {
        assert!(row[0] <= 4, "filter type {}", row[0]);
    }
}

#[test]
fn indexed_data_is_compressed() {
    let enc = qidx(&photo(), 64).unwrap();
    let p = enc.decode_parms.unwrap();
    let row_bytes = (p.columns * p.bits_per_component + 7) / 8;
    let raw_size = (row_bytes + 1) * enc.height;
    assert!(
        enc.data.len() as i64 <= raw_size / 2,
        "data is {} bytes of {raw_size}",
        enc.data.len()
    );
}

#[test]
fn indexed_decode_parms() {
    let img = photo();
    for (colors, want_bpc) in [(2, 1), (4, 2), (16, 4), (17, 8), (256, 8)] {
        let enc = qidx(&img, colors).unwrap();
        let p = enc.decode_parms.unwrap();
        assert_eq!(p.predictor, 15, "{colors}");
        assert_eq!(p.colors, 1, "{colors}");
        assert_eq!(p.bits_per_component, want_bpc, "{colors}");
        assert_eq!(p.columns, enc.width, "{colors}");
        assert_eq!(enc.bpc, want_bpc, "{colors}");
    }
}

#[test]
fn palette_bit_depth_boundaries() {
    let got: Vec<i64> = [1, 2, 3, 4, 5, 16, 17, 256]
        .iter()
        .map(|&n| palette_bit_depth(n))
        .collect();
    assert_eq!(got, [1, 1, 2, 2, 4, 4, 8, 8]);
}

/// A 200x200 image whose 256 colours are assigned at random, so the index stream is
/// high-entropy and the PNG encoder splits its output into more than one IDAT chunk.
fn random_indexed_fixture() -> Image {
    let mut rng = Lcg(1);
    let pal: Vec<[u8; 4]> = (0..256)
        .map(|_| {
            [
                rng.below(256) as u8,
                rng.below(256) as u8,
                rng.below(256) as u8,
                255,
            ]
        })
        .collect();
    let mut img = new_rgba(200, 200);
    for y in 0..200 {
        for x in 0..200 {
            set_rgba(&mut img, x, y, pal[rng.below(256) as usize]);
        }
    }
    Image::Rgba(img)
}

#[test]
fn indexed_multiple_idat_not_truncated() {
    let img = random_indexed_fixture();
    // The fixture must split into several IDAT chunks, or the test checks nothing.
    let png = qpng(&img, 256).unwrap();
    let idats = walk_png_chunks(&png)
        .unwrap()
        .iter()
        .filter(|c| c.typ == *b"IDAT")
        .count();
    assert!(idats >= 2, "{idats} IDAT chunks");

    let enc = qidx(&img, 256).unwrap();
    let p = enc.decode_parms.unwrap();
    let raw = inflate(&enc.data);
    let row_bytes = ((p.columns * p.bits_per_component + 7) / 8) as usize;
    assert_eq!(raw.len(), (row_bytes + 1) * enc.height as usize);
}

/// `paethPredictor`.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (i32::from(a), i32::from(b), i32::from(c));
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// `decodeIndexedRaster`: paints `enc` as a PDF consumer does, using nothing of the PNG reader:
/// inflate, undo the PNG row filters, unpack the samples MSB first and look each up.
fn decode_indexed_raster(enc: &EncodedImage) -> Image {
    let p = enc.decode_parms.unwrap();
    assert_eq!(p.predictor, 15);
    let bpc = p.bits_per_component as usize;
    assert!([1, 2, 4, 8].contains(&bpc));
    let raw = inflate(&enc.data);
    let (w, h) = (enc.width as usize, enc.height as usize);
    let row_bytes = (w * bpc).div_ceil(8);
    assert_eq!(raw.len(), (row_bytes + 1) * h);

    let mut out = new_rgba(enc.width, enc.height);
    let mut prev = vec![0u8; row_bytes];
    let mut cur = vec![0u8; row_bytes];
    let mask = ((1u16 << bpc) - 1) as u8;
    let per_byte = 8 / bpc;
    for y in 0..h {
        let off = y * (row_bytes + 1);
        let ft = raw[off];
        cur.copy_from_slice(&raw[off + 1..off + 1 + row_bytes]);
        for i in 0..row_bytes {
            // One component of at most 8 bits: the filter offset is one byte.
            let (a, c) = if i >= 1 {
                (cur[i - 1], prev[i - 1])
            } else {
                (0, 0)
            };
            let b = prev[i];
            cur[i] = match ft {
                0 => cur[i],
                1 => cur[i].wrapping_add(a),
                2 => cur[i].wrapping_add(b),
                3 => cur[i].wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => cur[i].wrapping_add(paeth(a, b, c)),
                _ => panic!("row {y} has filter type {ft}"),
            };
        }
        for x in 0..w {
            let idx = usize::from((cur[x / per_byte] >> (8 - bpc * (x % per_byte + 1))) & mask);
            let lk = &enc.color_space.lookup[idx * 3..idx * 3 + 3];
            set_rgba(&mut out, x as i64, y as i64, [lk[0], lk[1], lk[2], 255]);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    Image::Rgba(out)
}

#[test]
fn indexed_psnr_ladder_pinned() {
    for &(name, n, want) in LADDER {
        let img = ladder_image(name);
        let got = psnr_rgb(&img, &decode_indexed_raster(&qidx(&img, n).unwrap()));
        assert!(
            (got - want).abs() <= 5e-7,
            "{name}/{n}: PSNR {got}, want {want}"
        );
    }
}

#[test]
fn walk_png_chunks_framing() {
    let png = qpng(&scanpage(), 4).unwrap();
    let chunks = walk_png_chunks(&png).unwrap();
    assert_eq!(&chunks[0].typ, b"IHDR");
    assert_eq!(&chunks.last().unwrap().typ, b"IEND");
    assert!(chunks.iter().all(|c| c.crc_ok));
    assert!(matches!(
        walk_png_chunks(b"short"),
        Err(Error::Malformed(_))
    ));
    assert!(matches!(
        walk_png_chunks(&png[..png.len() - 20]),
        Err(Error::Malformed(_))
    ));
}
