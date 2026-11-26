//! The straighten contract and the lossless content matrix (byb-16j.4, byb-16j.2), design spec
//! sections 2-4 (Go: internal/pdfdoc/straighten.go).
//!
//! Assumes `crate::pdfdoc::Doc` (pdfdoc.go, not written yet) with its `ctx:
//! pdf::model::Context` field, `Doc::open_limits` and a `Doc::from_context(ctx, &Limits)`
//! constructor (Go: `&doc{ctx: ctx}`); `Doc::wrap_content` is in text.rs. Go's `matrix` is
//! `crate::content::Matrix`.

use super::Doc;
use super::buildpages::PageSource;
use crate::Limits;
use crate::content::Matrix;
use crate::error::{Error, Result};
use crate::go::fmt::format_f;
use crate::go::math::{cos, sin};
use crate::limits::check_cancel;
use pdf::model::{Context, XRefTable};
use std::sync::atomic::AtomicBool;

/// An absolute correction to apply to one page's content.
///
/// `deg` is the rotation byblos applies, in degrees, positive COUNTER-CLOCKWISE in PDF default
/// user space. That is the same signed convention as `skew.Estimate.Deg`
/// (internal/skew/skew.go:70-74), pinned there by `TestSignIsUserSpace`, and it is stated in
/// exactly one place for both sides of the repository boundary.
///
/// It is ABSOLUTE. It is the angle from the ORIGINAL page, never a delta on whatever is already
/// applied. kleio redelivers a job at least once (ocr.go:55-60), so a transform that composes
/// with what is already there corrupts the page on a retry. This is the same argument byb-yul.4
/// settled for `PageSource::rotate`.
///
/// ABSOLUTE IS ENFORCED, NOT ASSUMED. This module only applies the angle it is given; it has no
/// way to see a document's own provenance. The enforcement lives one layer up, in the root
/// `build_from_pages`: the rotation it hands down to this module is `deg` minus whatever the
/// source page's provenance already records as `Straightened.Deg`, defaulting to zero, and the
/// record it writes afterwards is always the total (`deg`), never the increment actually
/// applied.
///
/// To straighten a page flat: measure the raster's content angle D with internal/skew, read the
/// placement angle p from `ImageRef.PlacementDeg`, and set `deg = -(p + D)`. The two cancel on a
/// page that already reads straight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StraightenSpec {
    pub deg: f64,
    /// `[llx lly urx ury]` in the source page's unrotated PDF user space (design spec section
    /// 6). It is refused when `Some`: cropping is not implemented in this version, and refusing
    /// is what lets the field exist in the contract now without a caller silently getting a
    /// page that ignored it.
    pub crop: Option<[f64; 4]>,
}

/// Wraps every straightened page's content in a rotation about its own CropBox (or MediaBox,
/// absent one) centre.
///
/// It runs AFTER `build_context` has assembled `ctx`'s whole page tree, not during
/// `migrate_page`. `wrap_content` resolves a page number by walking the catalog's /Pages
/// (`page_dict`), and that tree is only a reservation (its Kids are empty) until `build_context`
/// fills the /Pages object in at the very end of the walk.
///
/// # Errors
/// [`Error::Cancelled`], and anything `page_dict` or `wrap_content` returns for page `i + 1`.
pub(super) fn apply_straighten(
    ctx: Context,
    pages: &[PageSource],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Context> {
    if pages.iter().all(|p| p.straighten.is_none()) {
        return Ok(ctx);
    }
    // Go builds `&doc{ctx: ctx}` with zero (default) limits: carry only the budget over.
    let doc_limits = Limits {
        max_alloc: limits.max_alloc,
        max_threads: limits.max_threads,
        ..Limits::default()
    };
    let mut doc = Doc::from_context(ctx, &doc_limits);
    for (i, p) in pages.iter().enumerate() {
        let Some(spec) = &p.straighten else {
            continue;
        };
        check_cancel(cancel)?;
        let n = i + 1;
        let (cx, cy) = page_centre(&mut doc.ctx.xref, n)?;
        let m = rotation_matrix(spec.deg, cx, cy);
        let [ma, mb, mc, md, me, mf] = m.0;
        let before = format!(
            "q\n{} {} {} {} {} {} cm\n",
            format_f(ma, 10),
            format_f(mb, 10),
            format_f(mc, 10),
            format_f(md, 10),
            format_f(me, 10),
            format_f(mf, 10),
        )
        .into_bytes();
        // Leading "\n": pdf-rs's page content (and every reader) concatenates /Contents array
        // members with no separator, so this wrapper's own "Q" would otherwise fuse with
        // whatever byte the LAST existing stream happens to end on: a real shape ("...Do Q"
        // with no trailing newline) that made a straightened export unreadable on its own
        // re-straighten. "before" needs no matching fix: it already ends in "\n" from the cm
        // line above, and it is prepended before the existing content rather than appended
        // after it.
        doc.wrap_content(n, &before, b"\nQ\n", cancel)?;
    }
    Ok(doc.ctx)
}

/// The centre of page `n`'s CropBox, or its MediaBox when it declares no CropBox: the same
/// fallback `Page()` applies, and the box the design spec's section 3 names as the one byblos
/// itself treats as the page (inspect.go:247, extract.go:369, extract.go:523, extract.go:560).
///
/// `migrate_page` has already given every output page a /MediaBox by the time this runs, so a
/// missing box here would be this module's own bug rather than a caller's malformed input.
fn page_centre(xt: &mut XRefTable, n: usize) -> Result<(f64, f64)> {
    let page = i64::try_from(n).unwrap_or(i64::MAX);
    let never = AtomicBool::new(false);
    let (_, _, inh) = xt.page_dict(page, &never)?;
    let Some(b) = inh.crop_box.or(inh.media_box) else {
        return Err(Error::Malformed(format!("page {n} has no page box")));
    };
    Ok(((b.ll.x + b.ur.x) / 2.0, (b.ll.y + b.ur.y) / 2.0))
}

/// The design spec section 3 matrix for angle `t_deg` about centre `(cx, cy)`:
///
/// ```text
/// a =  cos t     c = -sin t     e = cx(1 - cos t) + cy*sin t
/// b =  sin t     d =  cos t     f = cy(1 - cos t) - cx*sin t
/// ```
///
/// `(cx, cy)` is a fixed point of the result: applying it to `(cx, cy)` returns `(cx, cy)`
/// unchanged, whatever `t` is. Rotating about the origin instead was measured to push a
/// placement off the page edge (design spec section 3).
///
/// Go's `float64(...)` around each product stops the compiler from fusing a multiply-add; Rust
/// never fuses, so the same expression is the same bits.
fn rotation_matrix(t_deg: f64, cx: f64, cy: f64) -> Matrix {
    let t = t_deg * std::f64::consts::PI / 180.0;
    let (cos_t, sin_t) = (cos(t), sin(t));
    Matrix([
        cos_t,
        sin_t,
        -sin_t,
        cos_t,
        cx * (1.0 - cos_t) + cy * sin_t,
        cy * (1.0 - cos_t) - cx * sin_t,
    ])
}

#[cfg(test)]
mod tests {
    use super::super::annots::{build_pdf, deref_obj};
    use super::super::buildpages::{build_from_pages, validate_pages};
    use super::*;
    use pdf::object::Object;

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn page(source: usize, deg: Option<f64>) -> PageSource {
        PageSource {
            source,
            page: 1,
            rotate: 0,
            straighten: deg.map(|deg| StraightenSpec { deg, crop: None }),
        }
    }

    /// `mediaAndCropBoxDoc`: a one-page document whose MediaBox and CropBox are DIFFERENT
    /// rectangles, written by hand because this module's own writer has no path that produces
    /// a page whose two boxes disagree.
    fn media_and_crop_box_doc(width: f64, height: f64, crop: [f64; 4]) -> Vec<u8> {
        build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /CropBox [{} {} {} {}] /Contents 4 0 R >>",
                crop[0], crop[1], crop[2], crop[3]
            ),
            "<< /Length 19 >>\nstream\nq 1 0 0 1 0 0 cm Q\nendstream".to_string(),
        ])
    }

    /// `wrapMatrix`: the six numbers of the "before" wrapper stream's `cm` operator that
    /// `apply_straighten` wrote onto page `n` of `out`.
    fn wrap_matrix(out: &[u8], n: i64) -> [f64; 6] {
        let mut d = Doc::open_limits(out, &Limits::default(), &never()).unwrap();
        let xt = &mut d.ctx.xref;
        let (pd, _, _) = xt.page_dict(n, &never()).unwrap();
        let Some(Object::Array(arr)) = pd.0.get(b"Contents".as_slice()) else {
            panic!("page {n} /Contents is not an array");
        };
        let Some(Object::Ref(r)) = arr.first() else {
            panic!("page {n} /Contents[0] is not a reference");
        };
        wrapper_cm(xt, *r, n)
    }

    /// Decodes wrapper stream `r` and returns the numbers of its `cm` line.
    fn wrapper_cm(xt: &mut XRefTable, r: pdf::object::Ref, n: i64) -> [f64; 6] {
        let Object::Stream(sd) = deref_obj(xt, Some(&Object::Ref(r))) else {
            panic!("page {n} wrapper is not a stream");
        };
        let mut sd = *sd;
        xt.decode_stream(&mut sd, &never()).unwrap();
        let ops = String::from_utf8(sd.content.as_ref().unwrap().as_slice().to_vec()).unwrap();
        let (first, rest) = ops.split_once('\n').unwrap();
        assert_eq!(first.trim(), "q", "wrapper stream = {ops:?}");
        let nums: Vec<f64> = rest
            .split_whitespace()
            .take(6)
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(nums.len(), 6, "wrapper stream = {ops:?}");
        assert!(rest.contains(" cm"), "wrapper stream = {ops:?}");
        [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]]
    }

    // TestStraightenDoesNotShareAWrappedContentsArrayAcrossOutputPages: the same source page
    // named twice with different angles, whose /Contents is a reference to an array. The two
    // output pages share one migrated array object unless the wrap copies it first.
    #[test]
    fn does_not_share_a_wrapped_contents_array_across_output_pages() {
        let orig = "q 306 0 0 396 0 0 cm Q\n";
        let src = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_string(),
            "[5 0 R]".to_string(),
            format!("<< /Length {} >>\nstream\n{orig}endstream", orig.len()),
        ]);
        let out = build_from_pages(
            &[&src],
            &[page(0, Some(2.0)), page(0, Some(5.0))],
            &Limits::default(),
            &never(),
        )
        .unwrap();
        let mut d = Doc::open_limits(&out, &Limits::default(), &never()).unwrap();
        let xt = &mut d.ctx.xref;
        for (n, want_deg) in [(1_i64, 2.0_f64), (2, 5.0)] {
            let (pd, _, _) = xt.page_dict(n, &never()).unwrap();
            let Some(c @ Object::Ref(_)) = pd.0.get(b"Contents".as_slice()) else {
                panic!("page {n} /Contents is not a reference");
            };
            let Object::Array(arr) = deref_obj(xt, Some(c)) else {
                panic!("page {n} /Contents does not resolve to an array");
            };
            assert_eq!(
                arr.len(),
                3,
                "page {n}: a length outside before/original/after means it picked up the other page's wrapper"
            );
            let Some(Object::Ref(r)) = arr.first() else {
                panic!("page {n} /Contents[0] is not a reference");
            };
            let m = wrapper_cm(xt, *r, n);
            let got = m[1].atan2(m[0]).to_degrees();
            assert!(
                (got - want_deg).abs() < 1e-6,
                "page {n} angle = {got}, want {want_deg}"
            );
        }
    }

    // The centre is a fixed point of the matrix and the translation terms are nonzero:
    // rotating about the origin was measured to push the raster off the page edge.
    #[test]
    fn rotates_about_the_centre() {
        let [a, b, c, d, e, f] = rotation_matrix(90.0, 306.0, 396.0).0;
        let got_x = 306.0 * a + 396.0 * c + e;
        let got_y = 306.0 * b + 396.0 * d + f;
        assert!((got_x - 306.0).abs() < 1e-6 && (got_y - 396.0).abs() < 1e-6);
        assert!(e.abs() >= 1.0 || f.abs() >= 1.0);
        let [a, b, c, d, e, f] = rotation_matrix(-37.25, 200.0, 250.0).0;
        let got_x = 200.0 * a + 250.0 * c + e;
        let got_y = 200.0 * b + 250.0 * d + f;
        assert!((got_x - 200.0).abs() < 1e-9 && (got_y - 250.0).abs() < 1e-9);
    }

    #[test]
    fn the_matrix_angle_is_the_angle_asked_for() {
        let m = rotation_matrix(0.6, 306.0, 396.0).0;
        let got = crate::go::math::atan2(m[1], m[0]) * 180.0 / std::f64::consts::PI;
        assert!((got - 0.6).abs() < 1e-6, "{got}");
    }

    // validate's two rows: a Some crop (not implemented in this version) and a non-finite
    // angle, which would write a `cm` of non-finite numbers.
    #[test]
    fn refuses_a_crop_and_a_non_finite_angle() {
        let crop = [0.0, 0.0, 100.0, 100.0];
        let cases = [
            (
                "a crop",
                StraightenSpec {
                    deg: 1.0,
                    crop: Some(crop),
                },
                "crop",
            ),
            (
                "a NaN angle",
                StraightenSpec {
                    deg: f64::NAN,
                    crop: None,
                },
                "finite",
            ),
            (
                "a positive infinite angle",
                StraightenSpec {
                    deg: f64::INFINITY,
                    crop: None,
                },
                "finite",
            ),
            (
                "a negative infinite angle",
                StraightenSpec {
                    deg: f64::NEG_INFINITY,
                    crop: None,
                },
                "finite",
            ),
        ];
        for (name, spec, want) in cases {
            let pages = [PageSource {
                source: 0,
                page: 1,
                rotate: 0,
                straighten: Some(spec),
            }];
            let err = validate_pages(&pages).unwrap_err();
            assert!(err.to_string().contains(want), "{name}: {err}");
            // The refusal comes before any source is opened, so a build writes nothing.
            let err = build_from_pages(
                &[b"not a pdf".as_slice()],
                &pages,
                &Limits::default(),
                &never(),
            )
            .unwrap_err();
            assert!(err.to_string().contains(want), "{name}: {err}");
        }
    }

    // Design spec section 3's explicit choice of centre, against a page whose CropBox and
    // MediaBox actually differ.
    #[test]
    fn rotates_about_the_crop_box_not_the_media_box() {
        let src = media_and_crop_box_doc(612.0, 792.0, [100.0, 100.0, 300.0, 400.0]);
        let out = build_from_pages(
            &[&src],
            &[page(0, Some(90.0))],
            &Limits::default(),
            &never(),
        )
        .unwrap();
        let m = wrap_matrix(&out, 1);
        let (cx, cy) = (200.0, 250.0); // centre of CropBox [100 100 300 400]
        let got_x = cx * m[0] + cy * m[2] + m[4];
        let got_y = cx * m[1] + cy * m[3] + m[5];
        assert!(
            (got_x - cx).abs() < 1e-6 && (got_y - cy).abs() < 1e-6,
            "({got_x},{got_y}): it must be the fixed point, not the MediaBox centre (306,396)"
        );
    }

    // applyStraighten's page-number arithmetic against a multi-page build where only the
    // SECOND page asks for a straighten.
    #[test]
    fn applies_to_the_requested_page_not_only_page_1() {
        let src = media_and_crop_box_doc(612.0, 792.0, [0.0, 0.0, 612.0, 792.0]);
        let out = build_from_pages(
            &[&src, &src],
            &[page(0, None), page(1, Some(5.0))],
            &Limits::default(),
            &never(),
        )
        .unwrap();

        let mut d = Doc::open_limits(&out, &Limits::default(), &never()).unwrap();
        let (pd1, _, _) = d.ctx.xref.page_dict(1, &never()).unwrap();
        assert!(
            !matches!(pd1.0.get(b"Contents".as_slice()), Some(Object::Array(_))),
            "page 1 asked for no straighten and must be untouched"
        );

        let m = wrap_matrix(&out, 2);
        let got = crate::go::math::atan2(m[1], m[0]) * 180.0 / std::f64::consts::PI;
        assert!((got - 5.0).abs() < 1e-6, "page 2 wrapper angle = {got}");
    }

    // The page box is untouched, and /Rotate passes through (design spec sections 3 and 4).
    #[test]
    fn keeps_the_page_boxes_and_rotate() {
        let src = media_and_crop_box_doc(612.0, 792.0, [100.0, 100.0, 300.0, 400.0]);
        let pages = [PageSource {
            source: 0,
            page: 1,
            rotate: 90,
            straighten: Some(StraightenSpec {
                deg: 7.0,
                crop: None,
            }),
        }];
        let out = build_from_pages(&[&src], &pages, &Limits::default(), &never()).unwrap();
        let mut d = Doc::open_limits(&out, &Limits::default(), &never()).unwrap();
        let (pd, _, inh) = d.ctx.xref.page_dict(1, &never()).unwrap();
        assert_eq!(inh.rotate, 90);
        let crop = inh.crop_box.unwrap();
        assert_eq!(
            (crop.ll.x, crop.ll.y, crop.ur.x, crop.ur.y),
            (100.0, 100.0, 300.0, 400.0)
        );
        let media = inh.media_box.unwrap();
        assert_eq!((media.ur.x, media.ur.y), (612.0, 792.0));
        assert!(pd.0.contains_key(b"Contents".as_slice()));
    }

    fn corpus(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    // The tests below need corpus PDFs, which the Go internal/corpus generator makes and this
    // port does not.

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_rotates_about_the_page_centre() {
        let src = corpus("scan");
        let out = build_from_pages(
            &[&src],
            &[page(0, Some(90.0))],
            &Limits::default(),
            &never(),
        )
        .unwrap();
        let m = wrap_matrix(&out, 1);
        let (cx, cy) = (306.0, 396.0); // half of the 612x792 corpus page
        let got_x = cx * m[0] + cy * m[2] + m[4];
        let got_y = cx * m[1] + cy * m[3] + m[5];
        assert!((got_x - cx).abs() < 1e-6 && (got_y - cy).abs() < 1e-6);
        assert!(m[4].abs() >= 1.0 || m[5].abs() >= 1.0);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_composes_with_rotate() {
        let src = corpus("scan");
        let pages = [PageSource {
            source: 0,
            page: 1,
            rotate: 90,
            straighten: Some(StraightenSpec {
                deg: 0.6,
                crop: None,
            }),
        }];
        let out = build_from_pages(&[&src], &pages, &Limits::default(), &never()).unwrap();
        let mut d = Doc::open_limits(&out, &Limits::default(), &never()).unwrap();
        let (_, _, inh) = d.ctx.xref.page_dict(1, &never()).unwrap();
        assert_eq!(inh.rotate, 90);
        let m = wrap_matrix(&out, 1);
        let got = crate::go::math::atan2(m[1], m[0]) * 180.0 / std::f64::consts::PI;
        assert!((got - 0.6).abs() < 1e-6, "{got}");
    }
}
