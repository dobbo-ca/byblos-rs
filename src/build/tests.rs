//! Port of byblos `build_test.go` and the BuildPDF half of `build_codecs_test.go` that need no
//! PDF reader: they check the bytes `build_pdf` writes or the error it returns.
//!
//! Not ported, because they read the built PDF back with `ExtractPageRaster`, `Inspect`,
//! `pdfdoc.Validate`, `pdfimages` or `pdftoppm` (the PDF layer and external tools): the
//! round-trip assertions of `TestBuildPDFRoundTripsThroughExtract`,
//! `TestBuildPDFOrientationIsTopDown`, `TestBuildPDFDPIMatchesPdfimages`,
//! `TestBuiltPDFValidates`, `TestBuiltPDFPixelsMatchPdfimages`, the QuantizeIndexed tests
//! (`TestBuildPDFAcceptsIndexedImage`, `...NonPowerOfTwoPalette`) and the pixel half of
//! `TestBuildPDFCarries16BitFlate`. `TestBuildPDFRejectsIndexPastPalette/png-predicted` needs
//! Go's PNG encoder to pick mixed row filters; `TestBuildPDFIndexedHonoursEveryPNGRowFilter`
//! covers the same ground with explicit filters. The JBIG2 payload is a fixed byte string, not
//! `jbig2.EncodeGenericRegion` output: the test only needs bytes to carry. `FuzzBuildPDF` is
//! in the fuzz crate.

use super::*;
use crate::encoded::{ColorSpace, DecodeParms};
use pdf::filter::goflate::ZlibWriter;

fn lim() -> Limits {
    Limits::default()
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn zlib(raw: &[u8]) -> Vec<u8> {
    let mem = pdf::Mem::with_max(64 << 20);
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

fn cs(name: &str) -> ColorSpace {
    ColorSpace {
        name: name.to_string(),
        ..ColorSpace::default()
    }
}

fn indexed(base: &str, hi_val: i64, lookup: Vec<u8>) -> ColorSpace {
    ColorSpace {
        name: "Indexed".to_string(),
        base: base.to_string(),
        hi_val,
        lookup,
    }
}

/// `grayPattern`: deterministic, non-uniform 8-bit grey samples.
fn gray_pattern(w: usize, h: usize, seed: usize) -> Vec<u8> {
    (0..w * h)
        .map(|i| ((i * 7 + seed * 31) % 251) as u8)
        .collect()
}

/// `flateGrayImage`: a lossless DeviceGray/FlateDecode raster.
fn flate_gray_image(w: usize, h: usize, seed: usize) -> EncodedImage {
    EncodedImage {
        width: w as i64,
        height: h as i64,
        bpc: 8,
        color_space: cs("DeviceGray"),
        filter: "FlateDecode".to_string(),
        data: zlib(&gray_pattern(w, h, seed)),
        ..EncodedImage::default()
    }
}

/// A JBIG2Decode image whose payload is `payload`.
fn jbig2_image(w: i64, h: i64, payload: &[u8]) -> EncodedImage {
    EncodedImage {
        width: w,
        height: h,
        bpc: 1,
        color_space: cs("DeviceGray"),
        filter: "JBIG2Decode".to_string(),
        data: payload.to_vec(),
        ..EncodedImage::default()
    }
}

fn page(image: EncodedImage) -> BuildPage {
    BuildPage {
        image,
        dpi: 300.0,
        ..BuildPage::default()
    }
}

fn build(pages: &[BuildPage]) -> Result<Vec<u8>> {
    build_pdf(pages, &lim(), &no_cancel())
}

fn build_ok(pages: &[BuildPage]) -> Vec<u8> {
    build(pages).unwrap()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains(hay: &[u8], needle: &str) -> bool {
    find(hay, needle.as_bytes()).is_some()
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

/// `TestBuildPDFPlacementIsFitCentered`, on the content stream: a 40x60 image on a 612x792 page
/// is scaled by 13.2 to 528x792 and centred 42 points from the left.
#[test]
fn placement_is_fit_centered() {
    let out = build_ok(&[BuildPage {
        image: flate_gray_image(40, 60, 1),
        width_pt: 612.0,
        height_pt: 792.0,
        ..BuildPage::default()
    }]);
    assert!(contains(&out, "q 528 0 0 792 42 0 cm /Im0 Do Q"));
    assert!(contains(
        page_dict(&out).as_bytes(),
        "/MediaBox [0 0 612 792]"
    ));
}

/// A box derived from DPI is the image at its own resolution, so the image fills it, as
/// img2pdf draws it. With `dpi_y != dpi` the derived box has a different aspect from the
/// pixels; fit-centering would leave bands.
#[test]
fn derived_box_is_filled() {
    let out = build_ok(&[BuildPage {
        image: flate_gray_image(100, 100, 1),
        dpi: 300.0,
        dpi_y: 150.0,
        ..BuildPage::default()
    }]);
    assert!(
        contains(&out, "q 24 0 0 48 0 0 cm"),
        "{}",
        String::from_utf8_lossy(&out[..out.len().min(400)])
    );
}

#[test]
fn derived_box_follows_dpi() {
    let out = build_ok(&[page(flate_gray_image(40, 60, 1))]);
    // 40 * 72 / 300 = 9.6 and 60 * 72 / 300 = 14.4
    assert!(contains(
        page_dict(&out).as_bytes(),
        "/MediaBox [0 0 9.6 14.4]"
    ));
}

#[test]
fn multi_page() {
    let mut pages = Vec::new();
    for f in [0u8, 97, 194] {
        pages.push(page(EncodedImage {
            width: 4,
            height: 4,
            bpc: 8,
            color_space: cs("DeviceGray"),
            filter: "FlateDecode".to_string(),
            data: zlib(&[f; 16]),
            ..EncodedImage::default()
        }));
    }
    let out = build_ok(&pages);
    assert!(contains(&out, "/Count 3"));
    for p in &pages {
        assert!(find(&out, &p.image.data).is_some());
    }
}

/// Any of these slipping through would produce a file whose `/Filter` names a codec byblos
/// cannot cross-check, or a MediaBox no reader can use.
#[test]
fn rejects_unsupported_input() {
    let valid_gray = flate_gray_image(8, 8, 1);
    let img =
        |w: i64, h: i64, bpc: i64, space: ColorSpace, filter: &str, data: Vec<u8>| EncodedImage {
            width: w,
            height: h,
            bpc,
            color_space: space,
            filter: filter.to_string(),
            data,
            ..EncodedImage::default()
        };
    let boxed = |image: EncodedImage, w: f64, h: f64| BuildPage {
        image,
        width_pt: w,
        height_pt: h,
        ..BuildPage::default()
    };
    let with_parms = |mut i: EncodedImage, p: DecodeParms| {
        i.decode_parms = Some(p);
        i
    };
    let rgb_lookup = vec![0u8; 12];
    let cases: Vec<(&str, Vec<BuildPage>)> = vec![
        (
            "ccitt",
            vec![page(img(
                8,
                8,
                1,
                cs("DeviceGray"),
                "CCITTFaxDecode",
                vec![0],
            ))],
        ),
        (
            "lzw",
            vec![page(img(8, 8, 8, cs("DeviceGray"), "LZWDecode", vec![0]))],
        ),
        (
            "no-filter",
            vec![page(img(8, 8, 8, cs("DeviceGray"), "", vec![0]))],
        ),
        (
            "jbig2-wrong-bpc",
            vec![page(img(8, 8, 8, cs("DeviceGray"), "JBIG2Decode", vec![0]))],
        ),
        ("no-pages", vec![]),
        ("nan-box", vec![boxed(valid_gray.clone(), f64::NAN, 100.0)]),
        (
            "inf-box",
            vec![boxed(valid_gray.clone(), f64::INFINITY, 100.0)],
        ),
        ("negative-box", vec![boxed(valid_gray.clone(), -1.0, 100.0)]),
        (
            "no-dpi-no-box",
            vec![BuildPage {
                image: valid_gray.clone(),
                ..BuildPage::default()
            }],
        ),
        // Finite, positive page boxes that format_num's six-decimal-place rounding cannot
        // represent as anything but 0 or +Inf. See pdfbuild's MIN_COORD and MAX_COORD.
        ("tiny-box", vec![boxed(valid_gray.clone(), 1e-7, 1e-7)]),
        ("huge-box", vec![boxed(valid_gray.clone(), 1e303, 1e303)]),
        // An in-range page box and an in-range image size whose fit-centered scale still
        // rounds one placed dimension to 0: caught only by the post-scale check.
        (
            "degenerate-aspect",
            vec![boxed(
                img(
                    100_000,
                    1,
                    8,
                    cs("DeviceGray"),
                    "FlateDecode",
                    zlib(&vec![0u8; 100_000]),
                ),
                1.0,
                1000.0,
            )],
        ),
        // An /Indexed space whose palette does not match its own declaration.
        (
            "indexed-short-lookup",
            vec![page(img(
                8,
                8,
                4,
                indexed("DeviceRGB", 15, vec![1, 2, 3]),
                "FlateDecode",
                zlib(&[0; 32]),
            ))],
        ),
        (
            "indexed-bad-base",
            vec![page(img(
                8,
                8,
                4,
                indexed("DeviceLab", 0, vec![1]),
                "FlateDecode",
                zlib(&[0; 32]),
            ))],
        ),
        // A self-consistent /Indexed declaration whose SAMPLES name a palette entry that does
        // not exist.
        (
            "indexed-index-past-hival",
            vec![page(img(
                8,
                8,
                8,
                indexed("DeviceRGB", 1, vec![0, 0, 0, 255, 255, 255]),
                "FlateDecode",
                zlib(&[200; 64]),
            ))],
        ),
        // /DecodeParms that leave the samples unrecoverable. Every sample is 0, well inside
        // the palette, so only a refusal to guess at the row layout rejects these.
        (
            "indexed-tiff-predictor",
            vec![page(with_parms(
                img(
                    8,
                    8,
                    8,
                    indexed("DeviceRGB", 3, rgb_lookup.clone()),
                    "FlateDecode",
                    zlib(&[0; 64]),
                ),
                DecodeParms {
                    predictor: 2,
                    colors: 1,
                    bits_per_component: 8,
                    columns: 8,
                },
            ))],
        ),
        (
            "indexed-parms-disagree",
            vec![page(with_parms(
                img(
                    8,
                    8,
                    8,
                    indexed("DeviceRGB", 3, rgb_lookup.clone()),
                    "FlateDecode",
                    zlib(&[0; 72]),
                ),
                DecodeParms {
                    predictor: 15,
                    colors: 1,
                    bits_per_component: 8,
                    columns: 4,
                },
            ))],
        ),
        // A well-formed zlib stream that runs out before the rows the image declares.
        (
            "indexed-short-stream",
            vec![page(img(
                8,
                8,
                8,
                indexed("DeviceRGB", 3, rgb_lookup),
                "FlateDecode",
                zlib(&[0; 32]),
            ))],
        ),
    ];
    for (name, pages) in cases {
        assert!(
            build(&pages).is_err(),
            "{name}: build_pdf returned Ok; want a rejection"
        );
    }
}

/// Catches map-ordered dictionary emission or a wall-clock value (a `/CreationDate`, an `/ID`)
/// sneaking into the output.
#[test]
fn is_deterministic() {
    let pages = [page(flate_gray_image(20, 20, 3))];
    assert_eq!(build_ok(&pages), build_ok(&pages));
}

/// Catches a builder that re-compresses, re-frames, or otherwise touches an opaque JBIG2
/// payload, or gets `/BitsPerComponent` wrong for it.
#[test]
fn carries_jbig2_verbatim() {
    let payload: Vec<u8> = (0..200u32).map(|i| (i * 37 + 11) as u8).collect();
    let out = build_ok(&[page(jbig2_image(24, 24, &payload))]);
    assert!(find(&out, &payload).is_some());
    // ISO 32000-1 7.4.7 says JBIG2Decode carries 1-bit DeviceGray.
    let dict = image_dict(&out, "JBIG2Decode");
    assert!(dict.contains("/ColorSpace /DeviceGray"), "{dict}");
    assert!(dict.contains("/BitsPerComponent 1"), "{dict}");
    assert!(
        dict.contains("/Width 24") && dict.contains("/Height 24"),
        "{dict}"
    );
}

/// `/DecodeParms`'s keys are the PNG predictor parameters, meaningful only for FlateDecode. A
/// caller that sets `decode_parms` on a JBIG2 image must not see it emitted.
#[test]
fn omits_decode_parms_for_non_flate() {
    let mut img = jbig2_image(8, 8, &[1, 2, 3, 4]);
    img.decode_parms = Some(DecodeParms {
        predictor: 15,
        colors: 1,
        bits_per_component: 8,
        columns: 8,
    });
    let out = build_ok(&[page(img)]);
    let dict = image_dict(&out, "JBIG2Decode");
    assert!(!dict.contains("/DecodeParms"), "{dict}");
}

/// `TestBuildPDFRejectsFlateDataThatCannotDecode` (byb-vq6): a whole PNG file declared as
/// FlateDecode must be refused at write time.
#[test]
fn rejects_flate_data_that_cannot_decode() {
    let png_file = [
        &b"\x89PNG\r\n\x1a\n"[..],
        &[0, 0, 0, 13],
        b"IHDR",
        &[0u8; 40],
    ]
    .concat();
    let img = EncodedImage {
        width: 40,
        height: 24,
        bpc: 8,
        color_space: cs("DeviceRGB"),
        filter: "FlateDecode".to_string(),
        data: png_file,
        ..EncodedImage::default()
    };
    let err = build(&[page(img)]).unwrap_err();
    assert!(err.to_string().contains("FlateDecode"), "{err}");
}

/// `TestBuildPDFRejectsIndexPastPalette/raw` (byb-5jy): samples of 200 against hival 1.
#[test]
fn rejects_index_past_palette() {
    let img = EncodedImage {
        width: 8,
        height: 8,
        bpc: 8,
        color_space: indexed("DeviceRGB", 1, vec![0, 0, 0, 255, 255, 255]),
        filter: "FlateDecode".to_string(),
        data: zlib(&[200; 64]),
        ..EncodedImage::default()
    };
    let err = build(&[page(img)]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("200") && msg.contains("hival"), "{msg}");
}

/// A row of sub-byte samples whose width is not a whole number of bytes ends in padding bits,
/// which ISO 32000-1 section 8.9.5.1 says a reader ignores. A palette check that read them as
/// samples would refuse this page, whose five real indices are all inside a four-entry palette
/// and whose sixth nibble, pure padding, is 0xF.
#[test]
fn accepts_indexed_row_padding() {
    let rows = [
        0x12, 0x30, 0x2F, // samples 1 2 3 0 2, then 0xF of padding
        0x03, 0x21, 0x3F, // samples 0 3 2 1 3, then 0xF of padding
    ];
    let img = EncodedImage {
        width: 5,
        height: 2,
        bpc: 4,
        color_space: indexed("DeviceRGB", 3, vec![0; 12]),
        filter: "FlateDecode".to_string(),
        data: zlib(&rows),
        ..EncodedImage::default()
    };
    let out = build_ok(&[page(img)]);
    let dict = image_dict(&out, "FlateDecode");
    assert!(dict.contains("[/Indexed /DeviceRGB 3 <"), "{dict}");
}

/// `paethPredictor`: ISO/IEC 15948 section 9.4's PaethPredictor over the byte to the left
/// (a), the byte above (b) and the byte above-left (c).
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let (ai, bi, ci) = (i32::from(a), i32::from(b), i32::from(c));
    let p = ai + bi - ci;
    let (pa, pb, pc) = ((p - ai).abs(), (p - bi).abs(), (p - ci).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// `pngFilterRows`: encodes `rows`, one byte per sample, as a PDF `/Predictor 15` stream: each
/// row prefixed with the filter type in `filters[i]` and filtered under it, the lot deflated.
/// The forward filters are written out from ISO/IEC 15948 section 9.2, so this and pdfbuild's
/// reconstruction are two independent readings of the same spec.
fn png_filter_rows(rows: &[Vec<u8>], filters: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut prev = vec![0u8; rows[0].len()];
    for (i, row) in rows.iter().enumerate() {
        let mut out = vec![0u8; row.len()];
        for j in 0..row.len() {
            // a, b, c: left, above, above-left. bpp is 1 because these are one-component
            // 8-bit rows.
            let (a, c) = if j > 0 {
                (row[j - 1], prev[j - 1])
            } else {
                (0, 0)
            };
            let b = prev[j];
            out[j] = match filters[i] {
                0 => row[j],
                1 => row[j].wrapping_sub(a),
                2 => row[j].wrapping_sub(b),
                3 => row[j].wrapping_sub(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => row[j].wrapping_sub(paeth_predictor(a, b, c)),
                f => panic!("filter type {f} is not one of PNG's 0..4"),
            };
        }
        raw.push(filters[i]);
        raw.extend_from_slice(&out);
        prev = row.clone();
    }
    zlib(&raw)
}

/// `indexedPredictedPage`: wraps a `/Predictor 15` stream of 8-bit samples as a one-page
/// `/Indexed` build with the given hival.
fn indexed_predicted_page(w: i64, h: i64, hival: i64, data: Vec<u8>) -> Vec<BuildPage> {
    vec![page(EncodedImage {
        width: w,
        height: h,
        bpc: 8,
        color_space: indexed("DeviceRGB", hival, vec![0; 3 * (hival + 1).max(0) as usize]),
        filter: "FlateDecode".to_string(),
        decode_parms: Some(DecodeParms {
            predictor: 15,
            colors: 1,
            bits_per_component: 8,
            columns: w,
        }),
        data,
        ..EncodedImage::default()
    })]
}

/// Every one of PNG's five row filters, over random rows, pinned at the exact hival boundary:
/// `build_pdf` must accept a stream whose largest sample IS hival and refuse the identical
/// stream one hival lower. Accepting at the boundary and refusing below it together say the
/// recovered maximum is exactly the true one.
#[test]
fn indexed_honours_every_png_row_filter() {
    const W: usize = 16;
    const H: usize = 5;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for iter in 0..64usize {
        let mut rows: Vec<Vec<u8>> = Vec::new();
        let mut filters = Vec::new();
        let mut max_sample = 0i64;
        for i in 0..H {
            // Rotating the assignment puts every filter type in every row position, so each
            // one is exercised both against the implied zero row above row 1 and against a
            // real row.
            filters.push(((i + iter) % 5) as u8);
            let mut row = Vec::new();
            for _ in 0..W {
                let v = (next() % 90) as u8;
                row.push(v);
                max_sample = max_sample.max(i64::from(v));
            }
            rows.push(row);
        }
        let data = png_filter_rows(&rows, &filters);
        let ok = build(&indexed_predicted_page(
            W as i64,
            H as i64,
            max_sample,
            data.clone(),
        ));
        assert!(
            ok.is_ok(),
            "row set {iter} (filters {filters:?}): build at hival {max_sample}, the true largest sample: {ok:?}"
        );
        let low = build(&indexed_predicted_page(
            W as i64,
            H as i64,
            max_sample - 1,
            data,
        ));
        assert!(
            low.is_err(),
            "row set {iter} (filters {filters:?}): Ok at hival {}, one below the true largest sample {max_sample}",
            max_sample - 1
        );
    }
}

/// The stream of the test above must also be written, not merely accepted.
#[test]
fn indexed_predicted_builds() {
    let (w, h) = (16usize, 5usize);
    let mut max_sample = 0i64;
    let mut rows = Vec::new();
    for i in 0..h {
        let mut row = Vec::new();
        for j in 0..w {
            let v = ((i * 37 + j * 29) % 90) as u8;
            max_sample = max_sample.max(i64::from(v));
            row.push(v);
        }
        rows.push(row);
    }
    let data = png_filter_rows(&rows, &[0, 1, 2, 3, 4]);
    let out = build_ok(&indexed_predicted_page(
        w as i64,
        h as i64,
        max_sample,
        data.clone(),
    ));
    assert!(find(&out, &data).is_some());
    assert!(contains(&out, "/Predictor 15"));
}

#[test]
fn carries_16_bit_flate() {
    for (name, n) in [("DeviceGray", 1usize), ("DeviceRGB", 3)] {
        let (w, h) = (7usize, 5usize);
        let px: Vec<u8> = (0..w * h * n * 2).map(|i| (i * 7 + 3) as u8).collect();
        let img = EncodedImage {
            width: w as i64,
            height: h as i64,
            bpc: 16,
            color_space: cs(name),
            filter: "FlateDecode".to_string(),
            data: zlib(&px),
            ..EncodedImage::default()
        };
        let out = build_ok(&[BuildPage {
            image: img,
            dpi: 72.0,
            ..BuildPage::default()
        }]);
        let d = image_dict(&out, "FlateDecode");
        assert!(d.contains("/BitsPerComponent 16"), "{name}: {d}");
    }
}

#[test]
fn refuses_16_bit_indexed() {
    let img = EncodedImage {
        width: 2,
        height: 2,
        bpc: 16,
        color_space: indexed("DeviceRGB", 0, vec![0, 0, 0]),
        filter: "FlateDecode".to_string(),
        data: zlib(&[0; 8]),
        ..EncodedImage::default()
    };
    assert!(build(&[page(img)]).is_err(), "16-bpc /Indexed was written");
}

#[test]
fn page_rotate() {
    let img = flate_gray_image(10, 20, 1);
    let at = |r: i64| {
        build(&[BuildPage {
            image: img.clone(),
            dpi: 72.0,
            rotate: r,
            ..BuildPage::default()
        }])
    };
    for r in [90, 180, 270] {
        let d = page_dict(&at(r).unwrap());
        assert!(d.contains(&format!("/Rotate {r}")), "Rotate {r}: {d}");
        assert!(d.contains("/MediaBox [0 0 10 20]"), "Rotate {r}: {d}");
    }
    assert!(!page_dict(&at(0).unwrap()).contains("/Rotate"));
    for r in [45, -90, 360, 91] {
        assert!(at(r).is_err(), "Rotate {r} accepted");
    }
}

#[test]
fn page_box_table() {
    let img = flate_gray_image(10, 20, 1);
    let p = |w: f64, h: f64, dpi: f64, dpi_y: f64| BuildPage {
        image: img.clone(),
        width_pt: w,
        height_pt: h,
        dpi,
        dpi_y,
        rotate: 0,
    };
    assert_eq!(page_box(&p(0.0, 0.0, 72.0, 0.0)).unwrap(), (10.0, 20.0));
    assert_eq!(page_box(&p(0.0, 0.0, 72.0, 144.0)).unwrap(), (10.0, 10.0));
    assert_eq!(page_box(&p(5.0, 6.0, 0.0, 0.0)).unwrap(), (5.0, 6.0));
    for bad in [
        p(0.0, 0.0, 0.0, 0.0),
        p(0.0, 0.0, 72.0, -1.0),
        p(0.0, 0.0, f64::NAN, 0.0),
        p(0.0, 0.0, f64::INFINITY, 0.0),
        p(5.0, 0.0, 72.0, 0.0),
        p(0.0, 5.0, 72.0, 0.0),
        p(-1.0, 5.0, 72.0, 0.0),
        p(f64::NAN, 5.0, 72.0, 0.0),
        p(f64::INFINITY, 5.0, 72.0, 0.0),
    ] {
        assert!(page_box(&bad).is_err(), "{bad:?}");
    }
}

/// A box over `max_encoded_image_pixels` is refused as `limit encoded-image-pixels`, before
/// anything is written.
#[test]
fn encoded_image_pixel_cap() {
    let small = Limits {
        max_encoded_image_pixels: 99,
        ..Limits::default()
    };
    let err = build_pdf(&[page(flate_gray_image(40, 60, 1))], &small, &no_cancel()).unwrap_err();
    assert_eq!(err.class(), "limit/encoded-image-pixels", "{err:?}");
}

#[test]
fn cancelled_call_writes_nothing() {
    let cancel = AtomicBool::new(true);
    let err = build_pdf(&[page(flate_gray_image(8, 8, 1))], &lim(), &cancel).unwrap_err();
    assert_eq!(err, Error::Cancelled);
}

#[test]
fn page_errors_name_the_page() {
    let err = build(&[page(flate_gray_image(8, 8, 1)), BuildPage::default()]).unwrap_err();
    assert!(err.to_string().contains("page 2"), "{err}");
    assert_eq!(err.class(), "error");
}

/// A `BuildWriter` run over the same pages writes the bytes `build_pdf` returns.
#[test]
fn build_writer_matches_build_pdf() {
    let pages = [
        page(flate_gray_image(20, 20, 3)),
        page(flate_gray_image(8, 12, 4)),
    ];
    let want = build_ok(&pages);
    let mut w = BuildWriter::new(Vec::new(), &lim());
    for p in &pages {
        w.add_page(p, &no_cancel()).unwrap();
    }
    assert_eq!(w.close().unwrap(), want);
}

#[test]
fn build_writer_page_cap_and_refusals_leave_it_usable() {
    let one = Limits {
        max_pages: 1,
        ..Limits::default()
    };
    let mut w = BuildWriter::new(Vec::new(), &one);
    let good = page(flate_gray_image(8, 8, 1));
    // A refused page writes nothing.
    assert!(w.add_page(&BuildPage::default(), &no_cancel()).is_err());
    assert_eq!(
        w.add_page(&good, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    w.add_page(&good, &no_cancel()).unwrap();
    let err = w.add_page(&good, &no_cancel()).unwrap_err();
    assert_eq!(err.class(), "limit/pages", "{err:?}");
    let out = w.close().unwrap();
    assert_eq!(out, build_ok(&[good]));
}

#[test]
fn build_writer_close_without_pages_is_an_error() {
    let w = BuildWriter::new(Vec::new(), &lim());
    assert!(w.close().is_err());
}

/// `TranscodePNG` on a plain PNG: the resolution is the `pHYs` chunk's (none here), and the
/// image is Flate colour at the PNG's depth.
#[test]
fn transcode_png_reads_a_png() {
    let path = format!(
        "{}/tests/data/png/gray-gradient.png",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(path).unwrap();
    let (img, res) = transcode_png(&data[..], &lim(), &no_cancel()).unwrap();
    assert_eq!(img.filter, "FlateDecode");
    assert_eq!(img.color_space.name, "DeviceGray");
    assert_eq!(res, Resolution::default());
    assert!(build(&[page(img)]).is_ok());
}

#[test]
fn transcode_png_refuses_garbage() {
    let err = transcode_png(&b"not a png"[..], &lim(), &no_cancel()).unwrap_err();
    assert!(err.to_string().contains("TranscodePNG"), "{err}");
}

/// The page writer borrows the image rather than cloning it (Go copies only the slice header).
#[test]
fn to_page_borrows_the_image() {
    let p = BuildPage {
        image: jbig2_image(8, 8, &[1, 2, 3]),
        ..BuildPage::default()
    };
    let page = to_page(&p, 1.0, 1.0);
    assert!(matches!(page.image, Cow::Borrowed(_)));
    assert!(std::ptr::eq(
        page.image.data.as_ptr(),
        p.image.data.as_ptr()
    ));
}

#[test]
fn bad_page_box_is_reported_before_the_output_charge() {
    // As Go: the page boxes are resolved before anything is charged, so an image over
    // max_alloc with no page box is the page-box error, not an allocation error.
    let p = BuildPage {
        image: jbig2_image(8, 8, &vec![0u8; 2 << 20]),
        dpi: 0.0,
        ..BuildPage::default()
    };
    let lim = Limits {
        max_alloc: 1 << 20,
        ..Limits::default()
    };
    let e = build_pdf(&[p], &lim, &no_cancel()).unwrap_err();
    assert!(
        matches!(&e, Error::Malformed(s) if s.contains("no page box given")),
        "{e:?}"
    );
}
