//! Ports of byblos `downsample_test.go`. Left out: the PDF round trip of
//! `TestDownsampleDeclaredBPC1KeepsABilevelSourceBilevel` (it runs the bilevel raster directly
//! instead of building, inspecting and extracting a PDF), and `downsample_oracle_test.go`
//! (Ghostscript). `TestDownsampleNilImageErrors` has no Rust counterpart: an `&Image` cannot be
//! nil.

use super::*;
use crate::quantize::test_corpus::{
    IMAGE_H, IMAGE_W, Lcg, lim, new_rgba, no_cancel, scanpage, set_rgba,
};
use std::collections::HashSet;

fn down(img: &Image, bpc: Option<u8>, src: f64, dst: f64) -> Result<Image> {
    downsample(img, bpc, src, dst, &lim(), &no_cancel())
}

fn red(img: &Image, x: i64, y: i64) -> u32 {
    img.rgba_at(x, y).0 >> 8
}

/// `bitonalScanpage`: Scanpage forced to pure black and white, as an RGBA image.
fn bitonal_scanpage() -> Image {
    let src = scanpage();
    let mut out = new_rgba(IMAGE_W, IMAGE_H);
    for y in 0..IMAGE_H {
        for x in 0..IMAGE_W {
            let (r, g, b, _) = src.rgba_at(x, y);
            // Rec. 601 luma, the weighting a scanner's bitonal threshold uses.
            let luma =
                (299.0 * f64::from(r >> 8) + 587.0 * f64::from(g >> 8) + 114.0 * f64::from(b >> 8))
                    / 1000.0;
            let v = if luma >= 128.0 { 255 } else { 0 };
            set_rgba(&mut out, x, y, [v, v, v, 255]);
        }
    }
    Image::Rgba(out)
}

/// The distinct 8-bit red values of `img`.
fn distinct_levels(img: &Image) -> HashSet<u32> {
    let b = img.bounds();
    let mut levels = HashSet::new();
    for y in b.y0..b.y1 {
        for x in b.x0..b.x1 {
            levels.insert(red(img, x, y));
        }
    }
    levels
}

fn empty_image() -> Image {
    Image::Rgba(new_rgba(0, 0))
}

#[test]
fn empty_bounds_errors() {
    assert!(matches!(
        down(&empty_image(), None, 300.0, 150.0),
        Err(Error::Malformed(_))
    ));
}

#[test]
fn bad_dpi_errors() {
    let img = scanpage();
    for (src, dst) in [
        (0.0, 150.0),
        (-300.0, 150.0),
        (300.0, 0.0),
        (300.0, -150.0),
        (f64::NAN, 150.0),
        (300.0, f64::NAN),
        (f64::INFINITY, 150.0),
        (300.0, f64::INFINITY),
    ] {
        assert!(down(&img, None, src, dst).is_err(), "{src} -> {dst}");
    }
}

#[test]
fn no_op_when_src_not_greater_than_dst() {
    let img = scanpage();
    for (src, dst) in [(150.0, 300.0), (150.0, 150.0)] {
        assert_eq!(down(&img, None, src, dst).unwrap(), img, "{src} -> {dst}");
    }
}

#[test]
fn output_dimensions() {
    let out = down(&scanpage(), None, 300.0, 150.0).unwrap();
    let b = out.bounds();
    assert_eq!((b.dx(), b.dy()), (300, 400));
}

#[test]
fn ratio_rounding_to_identical_dimensions_is_no_op() {
    let img = scanpage();
    assert_eq!(down(&img, None, 300.0, 299.9).unwrap(), img);
}

#[test]
fn gray_survives_as_gray() {
    let mut g = Gray::new(Rect::new(0, 0, IMAGE_W, IMAGE_H), &lim()).unwrap();
    for y in 0..IMAGE_H {
        for x in 0..IMAGE_W {
            let o = g.pix_offset(x, y).unwrap();
            g.pix[o] = ((x + y) % 256) as u8;
        }
    }
    let out = down(&Image::Gray(g), None, 300.0, 150.0).unwrap();
    assert!(matches!(out, Image::Gray(_)));
}

#[test]
fn other_types_come_back_as_rgba() {
    assert!(matches!(
        down(&scanpage(), None, 300.0, 150.0).unwrap(),
        Image::Rgba(_)
    ));
}

#[test]
fn solid_region_survives() {
    let out = down(&scanpage(), None, 300.0, 150.0).unwrap();
    // (10, 200) maps to source row ~400, well inside the paper region.
    let (r, g, b, _) = out.rgba_at(10, 200);
    let (want_r, want_g, want_b) = (250i64, 246i64, 236i64);
    let tol = 10;
    assert!((i64::from(r >> 8) - want_r).abs() <= tol);
    assert!((i64::from(g >> 8) - want_g).abs() <= tol);
    assert!((i64::from(b >> 8) - want_b).abs() <= tol);
}

#[test]
fn non_zero_origin_bounds() {
    let full = scanpage();
    let mut sub = Rgba::new(Rect::new(100, 200, 100 + IMAGE_W, 200 + IMAGE_H), &lim()).unwrap();
    for y in 200..200 + IMAGE_H {
        for x in 100..100 + IMAGE_W {
            let (r, g, b, _) = full.rgba_at(x - 100, y - 200);
            set_rgba(
                &mut sub,
                x,
                y,
                [(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8, 255],
            );
        }
    }
    let out = down(&Image::Rgba(sub), None, 300.0, 150.0).unwrap();
    let b = out.bounds();
    assert_eq!((b.dx(), b.dy()), (300, 400));
}

#[test]
fn declared_bpc1_keeps_a_bilevel_source_bilevel() {
    let src = bitonal_scanpage();
    assert_eq!(distinct_levels(&src).len(), 2);
    let out = down(&src, Some(1), 300.0, 150.0).unwrap();
    let b = out.bounds();
    assert_eq!((b.dx(), b.dy()), (300, 400));
    let levels = distinct_levels(&out);
    assert!(
        levels.len() == 2 && levels.contains(&0) && levels.contains(&255),
        "{levels:?}"
    );
}

#[test]
fn declared_bpc1_subsamples_rather_than_blurs_and_thresholds() {
    const K: i64 = 2;
    const W: i64 = 64;
    let mut rng = Lcg(1);
    let mut src = new_rgba(W, W);
    for y in 0..W {
        for x in 0..W {
            let v = if rng.below(2) == 1 { 255 } else { 0 };
            set_rgba(&mut src, x, y, [v, v, v, 255]);
        }
    }
    let src = Image::Rgba(src);
    let out = down(&src, Some(1), 300.0, 150.0).unwrap();
    let ob = out.bounds();
    assert_eq!((ob.dx(), ob.dy()), (W / K, W / K));
    // True point subsampling copies one source pixel per destination pixel at one phase shared
    // by the whole image; a blur followed by a threshold cannot reproduce that on noise.
    let matched = (0..K).any(|py| {
        (0..K).any(|px| {
            (0..ob.dy())
                .all(|y| (0..ob.dx()).all(|x| red(&out, x, y) == red(&src, K * x + px, K * y + py)))
        })
    });
    assert!(matched, "no fixed phase reproduces the output");
}

#[test]
fn does_not_infer_bilevel_from_pixels() {
    let out = down(&bitonal_scanpage(), None, 300.0, 150.0).unwrap();
    assert!(distinct_levels(&out).len() > 2);
}

#[test]
fn declared_bpc_rejects_an_undeclarable_depth() {
    let img = bitonal_scanpage();
    for bpc in [0u8, 3, 5, 7, 9, 24, 32] {
        assert!(down(&img, Some(bpc), 300.0, 150.0).is_err(), "{bpc}");
    }
    for bpc in [1u8, 2, 4, 8, 16] {
        down(&img, Some(bpc), 300.0, 150.0).unwrap();
    }
}

#[test]
fn declared_bpc1_obeys_downsamples_rules() {
    let img = bitonal_scanpage();
    assert!(down(&empty_image(), Some(1), 300.0, 150.0).is_err());
    for (src, dst) in [
        (0.0, 150.0),
        (-300.0, 150.0),
        (300.0, 0.0),
        (300.0, -150.0),
        (f64::NAN, 150.0),
        (300.0, f64::NAN),
        (f64::INFINITY, 150.0),
        (300.0, f64::INFINITY),
    ] {
        assert!(down(&img, Some(1), src, dst).is_err(), "{src} -> {dst}");
    }
    for (src, dst) in [(150.0, 300.0), (150.0, 150.0), (300.0, 299.9)] {
        assert_eq!(
            down(&img, Some(1), src, dst).unwrap(),
            img,
            "{src} -> {dst}"
        );
    }
    let b = down(&img, Some(1), 300.0, 150.0).unwrap().bounds();
    assert_eq!((b.dx(), b.dy()), (300, 400));
}

#[test]
fn uses_the_contone_kernel_declared_bpc_does() {
    for src in [scanpage(), bitonal_scanpage()] {
        let plain = down(&src, None, 300.0, 150.0).unwrap();
        for bpc in [2u8, 4, 8, 16] {
            assert_eq!(down(&src, Some(bpc), 300.0, 150.0).unwrap(), plain, "{bpc}");
        }
        assert_ne!(down(&src, Some(1), 300.0, 150.0).unwrap(), plain);
    }
}

#[test]
fn source_over_the_pixel_cap_is_a_limit_error() {
    let l = Limits {
        max_image_pixels: 100,
        ..Limits::default()
    };
    let e = downsample(&scanpage(), None, 300.0, 150.0, &l, &no_cancel()).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Limit {
                name: "image-pixels",
                ..
            }
        ),
        "{e:?}"
    );
}

#[test]
fn cancelled_flag_is_honoured() {
    let c = AtomicBool::new(true);
    assert_eq!(
        downsample(&scanpage(), None, 300.0, 150.0, &lim(), &c),
        Err(Error::Cancelled)
    );
}
