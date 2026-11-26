//! Port of byblos `internal/pdfbuild/pdfbuild_test.go`: pins `validate_page`'s field-by-field
//! checks (non-positive page box and image dimensions, empty data, the FlateDecode BPC
//! allowlist, the zlib header check, the filter allowlist, the DCTDecode and JBIG2Decode
//! filter, BPC and colour-space combinations, the `/Indexed` palette-bounds check) and the
//! pixel-cap bounds. It is a pin on what `validate_page` already rejects, not a spec for what
//! it should.

use super::*;
use crate::encoded::ColorSpace;
use pdf::filter::goflate::ZlibWriter;
use std::time::Instant;

/// `deflate`: zlib-compresses raw sample bytes the way an encoder would, so a test image's data
/// decodes under FlateDecode.
fn deflate_bytes(chunks: &[&[u8]]) -> Vec<u8> {
    let mem = pdf::Mem::with_max(64 << 20);
    let cancel = AtomicBool::new(false);
    let mut out: Vec<u8> = Vec::new();
    let mut zw = ZlibWriter::new(&mem).unwrap();
    for c in chunks {
        zw.write(
            c,
            &mut |b: &[u8]| {
                out.extend_from_slice(b);
                Ok(())
            },
            &cancel,
        )
        .unwrap();
    }
    zw.close(&mut |b: &[u8]| {
        out.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    out
}

fn deflate(raw: &[u8]) -> Vec<u8> {
    deflate_bytes(&[raw])
}

fn cs(name: &str) -> ColorSpace {
    ColorSpace {
        name: name.to_string(),
        ..ColorSpace::default()
    }
}

/// A 2x1 `/Indexed` image, hival 3 (4-entry DeviceRGB palette), samples [0, 3]: the last
/// legitimately at the palette's edge. The "otherwise valid" base for the field-specific error
/// cases below.
fn valid_image() -> EncodedImage {
    EncodedImage {
        width: 2,
        height: 1,
        bpc: 8,
        color_space: ColorSpace {
            name: "Indexed".to_string(),
            base: "DeviceRGB".to_string(),
            hi_val: 3,
            lookup: vec![0; 4 * 3],
        },
        filter: "FlateDecode".to_string(),
        data: deflate(&[0, 3]),
        ..EncodedImage::default()
    }
}

fn page(image: EncodedImage) -> Page<'static> {
    Page {
        image: Cow::Owned(image),
        width_pt: 612.0,
        height_pt: 792.0,
        ..Page::default()
    }
}

#[test]
fn validate_page_table() {
    let with = |f: &dyn Fn(&mut EncodedImage)| {
        let mut img = valid_image();
        f(&mut img);
        page(img)
    };
    // (name, page, substring of the error; "" means validate_page must return Ok)
    let cases: Vec<(&str, Page, &str)> = vec![
        ("valid indexed image", page(valid_image()), ""),
        (
            "page box out of range",
            Page {
                width_pt: -1.0,
                ..page(valid_image())
            },
            "is not in the representable range",
        ),
        (
            "non-positive image dimensions",
            with(&|i| i.width = 0),
            "are not positive",
        ),
        (
            // byb-jo9: Width*BPC wraps past zero in Go.
            "width wraps rowLen past zero (byb-jo9)",
            with(&|i| i.width = 1 << 61),
            "limit encoded-image-pixels exceeded",
        ),
        (
            "width one past the bound (byb-jo9)",
            with(&|i| i.width = (1 << 31) + 1),
            "limit encoded-image-pixels exceeded",
        ),
        (
            // byb-37h: the guard fires on the PRODUCT, not a per-axis check.
            "pixel budget (byb-37h)",
            with(&|i| {
                i.width = (1 << 13) + 1;
                i.height = (1 << 13) + 1; // (2^13+1)^2 > 1<<26
            }),
            "limit encoded-image-pixels exceeded",
        ),
        (
            // Both axes at the largest value a real PNG IHDR can declare.
            "pixel budget product overflow guard (byb-37h)",
            with(&|i| {
                i.width = (1 << 32) - 1;
                i.height = (1 << 32) - 1;
            }),
            "limit encoded-image-pixels exceeded",
        ),
        (
            // An axis large enough that a plain product would wrap: the per-axis bound runs
            // before the product.
            "pixel budget axis overflows uint64 product (byb-37h)",
            with(&|i| {
                i.width = 1 << 61;
                i.height = 8;
            }),
            "limit encoded-image-pixels exceeded",
        ),
        ("empty data", with(&|i| i.data = Vec::new()), "no data"),
        (
            "FlateDecode bad BPC",
            with(&|i| i.bpc = 3),
            "is not one of 1, 2, 4, 8",
        ),
        (
            "FlateDecode unsupported colour space",
            with(&|i| i.color_space = cs("Lab")),
            "is not supported",
        ),
        (
            "FlateDecode data does not decode under its own filter",
            with(&|i| i.data = b"not a zlib stream".to_vec()),
            "does not decode under the filter it declares",
        ),
        (
            "indexed sample past the palette's hival",
            // 9 > HiVal 3
            with(&|i| i.data = deflate(&[0, 9])),
            "past the palette's hival",
        ),
        (
            "unsupported filter",
            with(&|i| i.filter = "LZWDecode".to_string()),
            "filter \"LZWDecode\" is not supported",
        ),
        (
            "DCTDecode bad BPC",
            page(EncodedImage {
                width: 2,
                height: 1,
                bpc: 16,
                color_space: cs("DeviceGray"),
                filter: "DCTDecode".to_string(),
                data: vec![0xff, 0xd8],
                ..EncodedImage::default()
            }),
            "bits per component must be 8",
        ),
        (
            "DCTDecode unsupported colour space",
            page(EncodedImage {
                width: 2,
                height: 1,
                bpc: 8,
                color_space: cs("Lab"),
                filter: "DCTDecode".to_string(),
                data: vec![0xff, 0xd8],
                ..EncodedImage::default()
            }),
            "only DeviceGray/DeviceRGB/DeviceCMYK",
        ),
        (
            "JBIG2Decode bad BPC",
            page(EncodedImage {
                width: 2,
                height: 1,
                bpc: 8,
                color_space: cs("DeviceGray"),
                filter: "JBIG2Decode".to_string(),
                data: vec![0x00],
                ..EncodedImage::default()
            }),
            "bits per component must be 1",
        ),
        (
            "JBIG2Decode unsupported colour space",
            page(EncodedImage {
                width: 2,
                height: 1,
                bpc: 1,
                color_space: cs("DeviceRGB"),
                filter: "JBIG2Decode".to_string(),
                data: vec![0x00],
                ..EncodedImage::default()
            }),
            "colour space must be DeviceGray",
        ),
    ];

    let cancel = AtomicBool::new(false);
    for (name, p, want) in cases {
        let got = validate_page(&p, &Limits::default(), &cancel);
        if want.is_empty() {
            assert!(got.is_ok(), "{name}: validate_page() = {got:?}, want Ok");
            continue;
        }
        let err = got.expect_err(name);
        assert!(
            err.to_string().contains(want),
            "{name}: validate_page() = {:?}, want it to contain {want:?}",
            err.to_string()
        );
    }
}

/// `TestValidatePageRefusesOversizedImageCheaply` pins the CPU half of byb-37h: a
/// 1 x 999,000,000 `/Indexed` image, backed by deflated zeros, used to make `validate_page`
/// spend seconds streaming the row loop 999 million times over before it found a bad sample.
/// The pixel guard must refuse it before the palette check is reached at all. This asserts
/// elapsed time, not just the error: the unfixed code also eventually returns an error (a
/// short stream, once the zeros run out), just after paying the full cost.
///
/// The Go test deflates 256 MiB of zeros; the input only has to be a stream the row loop could
/// run over, so this one is 16 MiB.
#[test]
fn validate_page_refuses_oversized_image_cheaply() {
    let zeros = vec![0u8; 1 << 20];
    let chunks: Vec<&[u8]> = (0..16).map(|_| zeros.as_slice()).collect();
    let z = deflate_bytes(&chunks);

    let p = Page {
        image: Cow::Owned(EncodedImage {
            width: 1,
            height: 999_000_000,
            bpc: 1,
            filter: "FlateDecode".to_string(),
            color_space: ColorSpace {
                name: "Indexed".to_string(),
                base: "DeviceGray".to_string(),
                hi_val: 1,
                lookup: vec![0, 255],
            },
            data: z,
            ..EncodedImage::default()
        }),
        width_pt: 0.001,
        height_pt: 999_000.0,
        ..Page::default()
    };

    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let got = validate_page(&p, &Limits::default(), &cancel);
    let el = start.elapsed();

    let err = got.expect_err("validate_page on a 999M-row declared image");
    assert!(
        err.to_string()
            .contains("limit encoded-image-pixels exceeded"),
        "validate_page() = {:?}",
        err.to_string()
    );
    // The bug measured 3.3s on this input; a generous ceiling over a guard-only refusal.
    assert!(
        el < std::time::Duration::from_millis(500),
        "elapsed {el:?} exceeds the ceiling"
    );
}

#[test]
fn write_one_page_is_a_well_formed_pdf() {
    let cancel = AtomicBool::new(false);
    let mut out: Vec<u8> = Vec::new();
    write(
        &mut out,
        &[page(valid_image())],
        &Limits::default(),
        &cancel,
    )
    .unwrap();
    let s = String::from_utf8_lossy(&out).into_owned();
    assert!(out.starts_with(b"%PDF-1.7\n"));
    assert!(out.ends_with(b"%%EOF\n"));
    assert!(s.contains("/MediaBox [0 0 612 792]"));
    assert!(s.contains(&format!(
        "/ColorSpace [/Indexed /DeviceRGB 3 <{}>]",
        "0".repeat(24)
    )));
    // The xref entry of object 1 (the catalog) points at "1 0 obj".
    let xref = out.windows(6).rposition(|w| w == b"\nxref\n").unwrap() + 1;
    let table = String::from_utf8_lossy(&out[xref..]).into_owned();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[2], "0000000000 65535 f ");
    let off: usize = lines[3][..10].parse().unwrap();
    assert!(out[off..].starts_with(b"1 0 obj\n"));
}

#[test]
fn builder_refuses_misuse() {
    let cancel = AtomicBool::new(false);
    let lim = Limits::default();
    let mut b = Builder::new(Vec::new(), &lim);
    assert!(b.close().is_err());
    assert!(b.close().is_err());
    assert!(b.add_page(&page(valid_image()), &cancel).is_err());
    assert_eq!(b.pages(), 0);
    assert!(write(Vec::new(), &[], &lim, &cancel).is_err());
}

#[test]
fn format_num_rounds_to_six_places() {
    assert_eq!(format_num(612.0), "612");
    assert_eq!(format_num(0.1234567), "0.123457");
    assert_eq!(format_num(1.0 / 3.0), "0.333333");
}
