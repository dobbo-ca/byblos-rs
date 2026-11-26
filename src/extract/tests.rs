//! Ports of extract_test.go, extract_quad_test.go, extract_subpoint_test.go,
//! extract_bitonal_test.go and extract_dct_sampling_test.go.
//!
//! Everything that needs a corpus PDF (the Go `internal/corpus` generator is not ported) reads
//! `tests/data/corpus/<name>.pdf` and is `#[ignore]`d until the Go oracle's `-gen` has written
//! it. The Go tests that go through `Inspect`, `RecordExtraction` or `BuildFromPages` use
//! [`extract_page`]'s record and `scan_page` here, which are the same facts.

use super::*;
use crate::build::{BuildPage, build_pdf};
use crate::content::{Color, Env, IDENTITY, XObject};
use crate::embed::embed_jpeg;
use crate::encoded::{ColorSpace, EncodedImage};
use crate::go::fmt::format_f;
use crate::go::math::{cos, sin};
use crate::stats::TEST_LOCK;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::PoisonError;

/// `corpus.ScanImageW` and `ScanImageH`.
const SCAN_W: i64 = 306;
const SCAN_H: i64 = 396;

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

/// Runs `f` while holding the lock that serializes the tests that touch the process-global
/// extract counters (stats.rs), so none of them sees another's counts.
fn locked<T>(f: impl FnOnce() -> T) -> T {
    let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// `pageEnv`: the resource tree the classify tests walk against. `/Im0` is an image (id 1),
/// `/Im1` a second one (id 2), `/Fm0` a form carrying `form` at scope 1, `/GS1` an opaque
/// graphics state and `/GS0` a transparent one.
struct PageEnv {
    form: &'static str,
}

impl Env for PageEnv {
    fn xobject(&self, _scope: i64, name: &[u8]) -> Option<XObject> {
        match name {
            b"Im0" => Some(XObject {
                image: true,
                id: 1,
                ..XObject::default()
            }),
            b"Im1" => Some(XObject {
                image: true,
                id: 2,
                ..XObject::default()
            }),
            b"Fm0" if !self.form.is_empty() => Some(XObject {
                content: Arc::new(self.form.as_bytes().to_vec()),
                matrix: IDENTITY,
                scope: 1,
                ..XObject::default()
            }),
            _ => None,
        }
    }

    fn ext_gstate_opaque(&self, _scope: i64, name: &[u8]) -> bool {
        name == b"GS1"
    }

    fn font(&self, _scope: i64, _name: &[u8]) -> Option<i64> {
        None
    }
}

fn walk_page(src: &str, form: &'static str) -> Scan {
    let (s, r) = content::walk(
        &never(),
        &Arc::new(src.as_bytes().to_vec()),
        0,
        &PageEnv { form },
        &Limits::default(),
    );
    r.unwrap();
    s
}

fn scan_with(f: impl FnOnce(&mut Scan)) -> Scan {
    let mut s = Scan::default();
    f(&mut s);
    s
}

fn images(p: Vec<Placement>) -> Scan {
    scan_with(|s| s.images = p)
}

fn page_rect(llx: f64, lly: f64, urx: f64, ury: f64) -> pdfdoc::Rect {
    pdfdoc::Rect { llx, lly, urx, ury }
}

fn bx(llx: f64, lly: f64, urx: f64, ury: f64) -> BBox {
    BBox { llx, lly, urx, ury }
}

/// `placement`: derives the CTM from the box the way the walker does, so a case cannot
/// accidentally describe a geometry the two disagree about.
fn placement(id: i64, b: BBox, opaque: bool) -> Placement {
    Placement {
        name: format!("Im{id}").into_bytes(),
        id,
        bbox: b,
        opaque,
        ctm: Matrix([b.urx - b.llx, 0.0, 0.0, b.ury - b.lly, b.llx, b.lly]),
        ..Placement::default()
    }
}

fn one(b: BBox) -> Vec<Placement> {
    vec![placement(1, b, true)]
}

/// `plainFacts`: every image is an ordinary opaque 8-bit greyscale raster.
fn plain_facts(_: i64) -> Option<ImageInfo> {
    Some(grey())
}

/// `facts`: one dictionary per image id; anything else is unknown, which is itself a case.
fn facts(m: Vec<(i64, ImageInfo)>) -> impl Fn(i64) -> Option<ImageInfo> {
    let m: HashMap<i64, ImageInfo> = m.into_iter().collect();
    move |id| m.get(&id).cloned()
}

fn bitonal() -> ImageInfo {
    ImageInfo {
        bpc: 1,
        ..ImageInfo::default()
    }
}

fn grey() -> ImageInfo {
    ImageInfo {
        bpc: 8,
        ..ImageInfo::default()
    }
}

/// `placementOf`: a placement from `m` with its box derived the way the walker does.
fn placement_of(m: Matrix) -> Vec<Placement> {
    vec![Placement {
        name: b"Im0".to_vec(),
        id: 1,
        ctm: m,
        bbox: m.unit_square_box(),
        ..Placement::default()
    }]
}

/// `deskewedPlacement`: a page-covering raster rotated by `deg`, the way a deskewing scanner
/// writes one: the pixels are the raw scan and the rotation lives in the matrix.
fn deskewed_placement(deg: f64) -> Vec<Placement> {
    let r = deg * std::f64::consts::PI / 180.0;
    placement_of(Matrix([
        612.0 * cos(r),
        612.0 * sin(r),
        -792.0 * sin(r),
        792.0 * cos(r),
        0.0,
        0.0,
    ]))
}

/// A 90-degree rotation: a and d are zero, b and c are not.
fn rotated_placement() -> Vec<Placement> {
    vec![Placement {
        name: b"Im0".to_vec(),
        id: 1,
        ctm: Matrix([0.0, 792.0, -612.0, 0.0, 612.0, 0.0]),
        bbox: bx(0.0, 0.0, 612.0, 792.0),
        ..Placement::default()
    }]
}

fn classify_plain(page: &pdfdoc::Rect, s: &Scan) -> (usize, &'static str) {
    classify(page, s, &plain_facts)
}

// A zero-valued raster, which is what every error return hands back in Go. Rust has no zero
// `Image`, so it is an empty Gray.
fn raster(bounds: Rect, page: Rect, quad: Option<[f64; 8]>) -> PageRaster {
    PageRaster {
        image: Image::Gray(Gray {
            pix: Vec::new(),
            stride: 0,
            rect: Rect::default(),
        }),
        bounds,
        page,
        raster_quad: quad,
        obj_nr: 0,
        dropped_annots: 0,
        bitonal: false,
    }
}

// The zero value must not claim to be the full page. It is what every error return hands back,
// and `Rectangle.In` answers true for an empty receiver, so the natural implementation reports a
// page nobody extracted as covered.
#[test]
fn zero_page_raster_does_not_cover_the_page() {
    assert!(!raster(Rect::default(), Rect::default(), None).covers_page());
    // The same trap one step in: a real raster, but the page box never filled.
    assert!(!raster(Rect::new(0, 0, 306, 792), Rect::default(), None).covers_page());
}

#[test]
fn covers_page_by_bounds_and_quad() {
    let page = Rect::new(0, 0, 612, 792);
    assert!(raster(Rect::new(0, 0, 612, 792), page, None).covers_page());
    assert!(raster(Rect::new(-5, -5, 700, 800), page, None).covers_page());
    assert!(!raster(Rect::new(0, 0, 568, 792), page, None).covers_page());
    // A rotated placement whose bounding box covers the page but whose true quad does not
    // (byb-2mt): the matrix of byblos's own straighten at 1.9 degrees.
    let q = Matrix([611.6635, 20.2910, -26.2589, 791.5646, 13.2977, -9.9278]).unit_square_quad();
    let bounds = box_rect(
        &Matrix([611.6635, 20.2910, -26.2589, 791.5646, 13.2977, -9.9278]).unit_square_box(),
    );
    assert!(!raster(bounds, page, Some(q.0)).covers_page());
}

#[test]
fn divert_class_covers_every_reason() {
    for (reason, class) in [
        ("no-image", "not-single-raster"),
        ("has-text", "not-single-raster"),
        ("multiple-images", "not-single-raster"),
        ("inline-image", "not-single-raster"),
        ("vector-paint", "not-single-raster"),
        ("shading", "not-single-raster"),
        ("unresolved-xobject", "not-single-raster"),
        ("rotated-placement", "not-single-raster"),
        ("flipped-placement", "not-single-raster"),
        // byb-b1.3 stopped classify emitting this one; a provenance record written by an
        // earlier release carries it and divert_class is what reads those.
        ("not-page-covering", "not-single-raster"),
        ("clipped-away", "not-single-raster"),
        ("transparent-overlay", "not-single-raster"),
        ("mrc-layers", "not-single-raster"),
        // The coarse legacy class, which must still match all three decode-* rules.
        ("unsupported-codec", "unsupported-codec"),
        ("unsupported-codec-jbig2", "unsupported-codec-jbig2"),
        ("unsupported-codec-jpx", "unsupported-codec-jpx"),
        // pdf-rs names this file type "tif", never "tiff".
        ("unsupported-codec-tif", "unsupported-codec-tiff"),
        // An unrecognised codec suffix is still a codec problem, not a rendering one.
        ("unsupported-codec-cmyk", "unsupported-codec"),
    ] {
        assert_eq!(divert_class(reason), class, "{reason}");
    }
    assert_eq!(divert_class("something-new"), "not-single-raster");
}

// byb-b1.2: the old gate compared m[1] and m[2] (points) against 1e-6; at page scale that is an
// exact-zero test. An angle does not depend on the size of the raster.
#[test]
fn skew_degrees_is_scale_invariant() {
    let deg = 0.5;
    let r = deg * std::f64::consts::PI / 180.0;
    for scale in [1.0, 72.0, 612.0, 5000.0] {
        let m = Matrix([
            scale * cos(r),
            scale * sin(r),
            -scale * sin(r),
            scale * cos(r),
            0.0,
            0.0,
        ]);
        let got = skew_degrees(&m);
        assert!((got - deg).abs() <= 1e-9, "scale {scale}: {got}");
    }
}

// govdocs1/005393.pdf p91: no text, no paint, one image, and a deskew rotation two orders of
// magnitude larger than the old tolerance could see.
#[test]
fn skew_degrees_on_the_measured_page() {
    let m = Matrix([560.65283, -0.56462, 0.76572, 760.3374, 14.97417, 16.36581]);
    let got = skew_degrees(&m);
    assert!((got - 0.0577).abs() <= 0.0005, "{got}");
    assert!(got <= MAX_SKEW_DEG);
}

struct Case {
    name: &'static str,
    scan: Scan,
    /// `None`: every image is an ordinary opaque 8-bit raster.
    info: Option<Vec<(i64, ImageInfo)>>,
    want_idx: usize,
    want: &'static str,
}

fn case(name: &'static str, scan: Scan, want: &'static str) -> Case {
    Case {
        name,
        scan,
        info: None,
        want_idx: 0,
        want,
    }
}

fn degenerate(ctm: Matrix, b: BBox, clip: Option<BBox>) -> Scan {
    images(vec![Placement {
        name: b"Im0".to_vec(),
        id: 1,
        ctm,
        bbox: b,
        clip,
        ..Placement::default()
    }])
}

#[test]
fn classify_table() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let full = bx(0.0, 0.0, 612.0, 792.0);
    let half = bx(0.0, 0.0, 306.0, 792.0);
    let right_half = bx(306.0, 0.0, 612.0, 792.0);
    let patch = bx(12.0, 2.0, 365.0, 617.0);
    // 0.7% of the page, under the MRC floor.
    let speck = bx(0.0, 0.0, 60.0, 60.0);
    // The base as Google Books places it: short of the page box on every edge, covering 94.8%.
    let inset_base = bx(14.5, 2.0, 597.5, 790.0);
    let layers2 = |a: Placement, b: Placement| images(vec![a, b]);
    let p = placement;
    let both_opaque = |first: BBox, second: BBox| layers2(p(1, first, true), p(2, second, true));

    let mut cases = vec![
        case("clean scan", images(one(full)), ""),
        case("no image at all", Scan::default(), "no-image"),
        case(
            "inked text present",
            scan_with(|s| {
                s.images = one(full);
                s.text_ops = 1;
                s.inked_text_ops = 1;
            }),
            "has-text",
        ),
        // The point of byb-b1.1: an OCR layer shows text operators and deposits no ink.
        case(
            "invisible text only",
            scan_with(|s| {
                s.images = one(full);
                s.text_ops = 4;
            }),
            "",
        ),
        case(
            "inline image",
            scan_with(|s| {
                s.images = one(full);
                s.inline_imgs = 1;
            }),
            "inline-image",
        ),
        // The raster is at index 0, so a paint at index 1 lands on top of it.
        case(
            "painted path over the raster",
            scan_with(|s| {
                s.images = one(full);
                s.paints = vec![Paint {
                    op: "f",
                    bbox: full,
                    fill: Color::default(),
                    stroke: Color::default(),
                    index: 1,
                    clip: None,
                    ctm: Matrix::default(),
                }];
            }),
            "vector-paint",
        ),
        case(
            "shading",
            scan_with(|s| {
                s.images = one(full);
                s.shading_ops = 1;
            }),
            "shading",
        ),
        case(
            "unresolved name",
            scan_with(|s| {
                s.images = one(full);
                s.unresolved = vec![b"X".to_vec()];
            }),
            "unresolved-xobject",
        ),
        case(
            "rotated placement",
            images(rotated_placement()),
            "rotated-placement",
        ),
        // byb-b1.2: of 159 off-axis placements measured, 147 were scanner deskew.
        case(
            "median scanner deskew",
            images(deskewed_placement(0.13)),
            "",
        ),
        case(
            "the widest deskew measured",
            images(deskewed_placement(1.09)),
            "",
        ),
        case(
            "a rotation exactly at the tolerance",
            images(deskewed_placement(MAX_SKEW_DEG)),
            "",
        ),
        case(
            "a rotation past the tolerance",
            images(deskewed_placement(2.5)),
            "rotated-placement",
        ),
        // A shear is not a rotation: the x axis is square to the page and the y axis is not.
        case(
            "sheared placement",
            images(placement_of(Matrix([612.0, 0.0, 80.0, 792.0, 0.0, 0.0]))),
            "rotated-placement",
        ),
        // A negative scale term mirrors without skew, and the box reports the same page-covering
        // box for all three.
        case(
            "vertically flipped",
            images(placement_of(Matrix([612.0, 0.0, 0.0, -792.0, 0.0, 792.0]))),
            "flipped-placement",
        ),
        case(
            "horizontally flipped",
            images(placement_of(Matrix([-612.0, 0.0, 0.0, 792.0, 612.0, 0.0]))),
            "flipped-placement",
        ),
        case(
            "flipped on both axes",
            images(placement_of(Matrix([
                -612.0, 0.0, 0.0, -792.0, 612.0, 792.0,
            ]))),
            "flipped-placement",
        ),
        // A degenerate zero-scale CTM with no clip reports its own cause, not a clip's.
        case(
            "degenerate placement with no clip reports its own cause, not a clip",
            degenerate(Matrix([0.0; 6]), bx(0.0, 0.0, 0.0, 0.0), None),
            "flipped-placement",
        ),
        // The same zero-area box with a clip: now a clip really did narrow it away.
        case(
            "degenerate placement with a clip reports clipped-away",
            degenerate(
                Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]),
                bx(0.0, 0.0, 0.0, 0.0),
                Some(bx(700.0, 800.0, 700.0, 800.0)),
            ),
            "clipped-away",
        ),
        // A sub-point-tall clip is NOT clipped away (byb-62t): poppler inks a stripe the length
        // of the page.
        case(
            "a sub-point clip sliver is a visible stripe and extracts",
            degenerate(
                Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]),
                bx(0.0, 100.0, 612.0, 100.4),
                Some(bx(0.0, 100.0, 612.0, 100.4)),
            ),
            "",
        ),
        // A clip DISJOINT from the placement leaves zero extent on both axes.
        case(
            "a clip disjoint from the placement leaves no extent and diverts",
            degenerate(
                Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]),
                bx(612.0, 0.0, 612.0, 0.0),
                Some(bx(700.0, 0.0, 800.0, 792.0)),
            ),
            "clipped-away",
        ),
        // A clip that narrowed nothing does not steal the CTM's cause.
        case(
            "a clip that narrowed nothing does not steal the CTM's cause",
            degenerate(
                Matrix([0.0; 6]),
                bx(0.0, 0.0, 0.0, 0.0),
                Some(bx(0.0, 0.0, 612.0, 792.0)),
            ),
            "flipped-placement",
        ),
        // byb-b1.3: a lone raster is the page whatever fraction of the box it occupies.
        case(
            "a natural-DPI placement 43.6 points short still extracts",
            images(one(bx(0.0, 0.0, 568.3708, 791.7616))),
            "",
        ),
        case(
            "a lone raster over half the page is still the page",
            images(one(half)),
            "",
        ),
        case(
            "half a point of slack is tolerated",
            images(one(bx(0.5, 0.5, 611.5, 791.5))),
            "",
        ),
        // A stack is a different question: an under-layer can reach past the top one.
        case(
            "a stacked pair that does not cover the page",
            both_opaque(half, half),
            "multiple-images",
        ),
        // The measured Internet Archive shape.
        Case {
            want_idx: 1,
            ..case("stacked pair takes the top", both_opaque(full, full), "")
        },
        Case {
            want_idx: 2,
            ..case(
                "three stacked layers take the last",
                images(vec![p(1, full, true), p(2, full, true), p(3, full, true)]),
                "",
            )
        },
        // Genuine tiling, which the relaxation must not swallow.
        case(
            "side-by-side rasters divert",
            both_opaque(half, right_half),
            "multiple-images",
        ),
        case(
            "a base the top does not contain diverts",
            both_opaque(bx(-20.0, -20.0, 632.0, 812.0), full),
            "multiple-images",
        ),
        case(
            "a stamp painted over the cover diverts",
            both_opaque(full, speck),
            "multiple-images",
        ),
        // Transparency: the occlusion argument needs the top layer to be opaque.
        case(
            "a top painted under transparency cannot be assumed to occlude",
            layers2(p(1, full, true), p(2, full, false)),
            "transparent-overlay",
        ),
    ];

    let with_info = |c: Case, info: Vec<(i64, ImageInfo)>, want: &'static str| Case {
        info: Some(info),
        want,
        ..c
    };
    let stack = || both_opaque(full, full);
    cases.extend([
        with_info(
            case("a top with an /SMask diverts", stack(), ""),
            vec![
                (1, grey()),
                (
                    2,
                    ImageInfo {
                        bpc: 8,
                        smask: true,
                        ..ImageInfo::default()
                    },
                ),
            ],
            "transparent-overlay",
        ),
        with_info(
            case("a top with a /Mask diverts", stack(), ""),
            vec![
                (1, grey()),
                (
                    2,
                    ImageInfo {
                        bpc: 8,
                        mask: true,
                        ..ImageInfo::default()
                    },
                ),
            ],
            "transparent-overlay",
        ),
        // A stencil mask paints the fill colour through the mask, so what is under it shows
        // through the unset bits.
        with_info(
            case("an image-mask top diverts", stack(), ""),
            vec![
                (1, grey()),
                (
                    2,
                    ImageInfo {
                        image_mask: true,
                        ..ImageInfo::default()
                    },
                ),
            ],
            "transparent-overlay",
        ),
        with_info(
            case(
                "an unreadable image dictionary is not assumed opaque",
                stack(),
                "",
            ),
            Vec::new(),
            "transparent-overlay",
        ),
        // The MRC guard. Google Books emits a bitonal page-covering base plus a smaller
        // non-bitonal patch, and on 153 measured pages the base is blank.
        with_info(
            case(
                "a bitonal base with a non-bitonal patch diverts",
                both_opaque(full, patch),
                "",
            ),
            vec![(1, bitonal()), (2, grey())],
            "mrc-layers",
        ),
        // The guard runs first on purpose: this page would otherwise reduce to its top layer.
        with_info(
            case("the guard beats the take-the-top rule", stack(), ""),
            vec![(1, bitonal()), (2, grey())],
            "mrc-layers",
        ),
        // The measured geometry: a base placed short of the page box is still an MRC base.
        with_info(
            case(
                "a base placed short of the page box is still an MRC base",
                both_opaque(inset_base, patch),
                "",
            ),
            vec![(1, bitonal()), (2, grey())],
            "mrc-layers",
        ),
        with_info(
            case(
                "a bitonal page-covering raster alone is not an MRC page",
                images(one(full)),
                "",
            ),
            vec![(1, bitonal())],
            "",
        ),
        with_info(
            case(
                "a non-bitonal speck is too small to be an MRC patch",
                both_opaque(full, speck),
                "",
            ),
            vec![(1, bitonal()), (2, grey())],
            "multiple-images",
        ),
    ]);

    for tc in &cases {
        let (idx, got) = match &tc.info {
            Some(m) => classify(&page, &tc.scan, &facts(m.clone())),
            None => classify_plain(&page, &tc.scan),
        };
        assert_eq!(got, tc.want, "{}", tc.name);
        if got.is_empty() {
            assert_eq!(idx, tc.want_idx, "{}: index", tc.name);
        }
    }
}

// Pins `MRC_PATCH_AREA_FRAC` between the two real pages that bracket it: ia-municipaldocume
// 00masgoog.pdf as walked, identical base and watermark, only the inset differs.
#[test]
fn classify_mrc_patch_floor_bracket() {
    let page = page_rect(0.0, 0.0, 420.0, 618.72);
    let base = bx(9.96, 1.56, 410.04, 617.16); // 0.947768 of the page
    let watermark = bx(286.0, 10.0, 370.0001, 26.5726); // 0.005357, bitonal, on top
    let p99_inset = bx(70.44, 88.2, 140.52, 163.08); // 0.020194
    let p101_inset = bx(81.96, 448.2, 148.2, 515.4); // 0.017130
    let info = facts(vec![(1, bitonal()), (2, grey()), (3, bitonal())]);
    for (inset, want) in [(p99_inset, "mrc-layers"), (p101_inset, "multiple-images")] {
        let scan = images(vec![
            placement(1, base, true),
            placement(2, inset, true),
            placement(3, watermark, true),
        ]);
        assert_eq!(classify(&page, &scan, &info).1, want);
    }
}

// byb-b1.5 measured 126 scan-shaped pages diverting as vector-paint: a background wash painted
// before the raster and then covered by it. These go through the real walker, because what is
// under test is the graphics state.
#[test]
fn classify_paint_occlusion() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    const FULL_PAGE_DO: &str = "q 612 0 0 792 0 0 cm /Im0 Do Q\n";
    let wash_form = "1 1 1 scn 0 0 1200 1500 re f\n";
    let cases: Vec<(&str, String, &'static str, &str)> = vec![
        (
            "background wash then the raster",
            format!(
                "/Cs6 cs 1 1 1 scn\n0.029999 0.03009 611.94 791.94 re\nf\n0 0 0 scn\n{FULL_PAGE_DO}"
            ),
            "",
            "",
        ),
        (
            "stroked border over the raster",
            format!("{FULL_PAGE_DO}q 0 0 0 RG 2 w 72 72 468 648 re S Q\n"),
            "",
            "vector-paint",
        ),
        // A wash the raster does not cover is visible content, whatever order it was painted in.
        (
            "wash wider than the raster it precedes",
            "0 0 612 792 re f\nq 412 0 0 592 100 100 cm /Im0 Do Q\n".to_string(),
            "",
            "vector-paint",
        ),
        // Ink spreads half the line width to either side of the path.
        (
            "a stroke whose ink spreads past the raster edge",
            format!("20 w 0 0 612 792 re S\n{FULL_PAGE_DO}"),
            "",
            "vector-paint",
        ),
        (
            "a stroke whose ink stays inside the raster",
            format!("1 w 10 10 592 772 re S\n{FULL_PAGE_DO}"),
            "",
            "",
        ),
        // byb-7aq: the stroke arms the clip with its OWN path, and ISO 32000-1 8.5.4 does not
        // let that clip touch the operator that armed it.
        (
            "a stroke that sets the clip with its own path",
            "q 20 w 100 100 400 600 re W S Q\nq 400 0 0 600 100 100 cm /Im0 Do Q\n".to_string(),
            "",
            "vector-paint",
        ),
        // Clipping alone has never counted as paint.
        (
            "clip only, no paint",
            format!("0 0 612 792 re W n\n{FULL_PAGE_DO}"),
            "",
            "",
        ),
        // A wash inside a Form XObject: its CTM and its place in the painting order both have to
        // survive the recursion.
        (
            "wash inside a form before the raster",
            format!("q 0.5 0 0 0.5 0 0 cm /Fm0 Do Q\n{FULL_PAGE_DO}"),
            wash_form,
            "",
        ),
        (
            "wash inside a form after the raster",
            format!("{FULL_PAGE_DO}q 0.5 0 0 0.5 0 0 cm /Fm0 Do Q\n"),
            wash_form,
            "vector-paint",
        ),
        // byb-e04: a fill gets PAINT_FILL_TOLERANCE_PT and a stroke does not.
        (
            "a fill half a point outside the raster",
            format!("-0.5 -0.5 613 793 re f\n{FULL_PAGE_DO}"),
            "",
            "",
        ),
        (
            "a stroke half a point outside the raster",
            format!("0 w -0.5 -0.5 613 793 re S\n{FULL_PAGE_DO}"),
            "",
            "vector-paint",
        ),
        (
            "a fill two points outside the raster",
            format!("-2 -2 616 796 re f\n{FULL_PAGE_DO}"),
            "",
            "vector-paint",
        ),
        (
            "a fill half a point past one edge only",
            format!("0 0 612.5 792 re f\n{FULL_PAGE_DO}"),
            "",
            "",
        ),
    ];
    for (name, src, form, want) in cases {
        let got = classify_plain(&page, &walk_page(&src, form)).1;
        assert_eq!(got, want, "{name}");
    }
}

// A wash is only invisible while the thing over it is opaque: a stencil paints through its 1
// bits, and the same goes for an /SMask, a /Mask and a lowered /ca.
#[test]
fn classify_paint_under_a_transparent_raster_still_diverts() {
    let src = "1 1 1 scn 0 0 612 792 re f\nq 612 0 0 792 0 0 cm /Im0 Do Q\n";
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    for (name, info) in [
        (
            "stencil image mask",
            ImageInfo {
                image_mask: true,
                ..ImageInfo::default()
            },
        ),
        (
            "soft-masked raster",
            ImageInfo {
                bpc: 8,
                smask: true,
                ..ImageInfo::default()
            },
        ),
        (
            "colour-keyed raster",
            ImageInfo {
                bpc: 8,
                mask: true,
                ..ImageInfo::default()
            },
        ),
    ] {
        let s = walk_page(src, "");
        let got = classify(&page, &s, &facts(vec![(1, info)])).1;
        assert_eq!(got, "vector-paint", "{name}");
    }
    // The graphics-state half: /GS0 lowers /ca, so the wash shows through the raster.
    let s = walk_page(
        "1 1 1 scn 0 0 612 792 re f\nq /GS0 gs 612 0 0 792 0 0 cm /Im0 Do Q\n",
        "",
    );
    assert_eq!(classify_plain(&page, &s).1, "vector-paint");
}

// A page clip that narrows an under-layer's box makes `contains` EASIER to satisfy, and the
// divert-to-extract flip is the correct answer: post byb-b1.12 the under-layer's box is its REAL
// visible extent.
#[test]
fn narrower_under_layer_box_flips_divert_to_extract_correctly() {
    let src =
        "q 0 0 612 792 re W n\nq 700 0 0 900 0 0 cm /Im0 Do Q\nq 612 0 0 792 0 0 cm /Im0 Do Q\nQ\n";
    let s = walk_page(src, "");
    assert_eq!(s.images.len(), 2);
    let clipped = bx(0.0, 0.0, 612.0, 792.0);
    assert_eq!(s.images[0].bbox, clipped);
    assert_eq!(s.images[1].bbox, clipped);
    let (idx, got) = classify_plain(&page_rect(0.0, 0.0, 612.0, 792.0), &s);
    assert_eq!(got, "");
    assert_eq!(idx, 1);
}

// The page byb-b1.5 was measured on, at its measured geometry: 45 pages of govdocs1/005697.pdf
// carry this stream byte for byte. The raster is 610.56 points wide on a 612 point MediaBox and
// the wash is hidden, so the page extracts.
#[test]
fn classify_on_the_measured_wash_page() {
    let src = "/Cs6 cs 1 1 1 scn\n/GS1 gs\n0.029999 0.03009 610.5 791.94 re\nf\n0 0 0 scn\nq\n\
               610.559937 0 0 792.000061 -0.000012 -0.000031 cm\n/Im0 Do\nQ\n";
    let got = classify_plain(&page_rect(0.0, 0.0, 612.0, 792.0), &walk_page(src, "")).1;
    assert_eq!(got, "");
}

// byb-7aq: govdocs1/050667.pdf page 1. The two `W n` clips bound BOTH the wash and the raster to
// [0 0.06 612 792]; the wash's own path is 0.06 pt taller than the clip that removes the
// overhang.
#[test]
fn classify_wash_clipped_to_the_raster_is_hidden() {
    let src = "q\n1 i \n0 792 612 -792 re\nW n\n0 792.06 612 -792 re\nW n\n0 0 0 0 k\n/GS2 gs\n\
               0 0.059998 612 792 re\nf\nQ\nq\n1 i \n-6.007 799.2 627.007 -808.2 re\nW* n\n\
               0 792 612 -792 re\nW n\n0 792.06 612 -792 re\nW n\n/GS1 gs\nq\n\
               627.814514 0 0 808.76709 -6.267024 -9.32312 cm\n/Im0 Do\nQ\nQ\n";
    let got = classify_plain(&page_rect(0.0, 0.0, 612.0, 792.0), &walk_page(src, "")).1;
    assert_eq!(got, "");
}

// byb-7aq widened which placements can hide a path; these pin the boundary of the widening.
#[test]
fn classify_paint_occlusion_across_placements() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let wash = "1 1 1 scn 0 0 612 792 re f\n";
    let cover0 = "q 612 0 0 792 0 0 cm /Im0 Do Q\n";
    let cover1 = "q 612 0 0 792 0 0 cm /Im1 Do Q\n";
    let plain = grey();
    let masked = ImageInfo {
        bpc: 8,
        smask: true,
        ..ImageInfo::default()
    };
    let cases: Vec<(&str, String, Vec<(i64, ImageInfo)>, &str)> = vec![
        // The reason moves from "vector-paint" to "transparent-overlay", because the wash really
        // is hidden by /Im0 and the overlay is what stops the page reducing to one raster.
        (
            "a wash hidden by an opaque layer under a transparent top",
            format!("{wash}{cover0}{cover1}"),
            vec![(1, plain.clone()), (2, masked.clone())],
            "transparent-overlay",
        ),
        // The skip in ink_hidden is "keep looking", not "give up".
        (
            "a transparent cover and an opaque one that is too small",
            format!("{wash}{cover0}q 100 0 0 100 100 100 cm /Im1 Do Q\n"),
            vec![(1, masked), (2, plain.clone())],
            "vector-paint",
        ),
        // Painting order still governs.
        (
            "a fill painted after every image",
            format!("{cover0}{cover1}1 1 1 scn 0 0 100 100 re f\n"),
            vec![(1, plain.clone()), (2, plain.clone())],
            "vector-paint",
        ),
        // byb-e04 moves a counter: the wash within the fill tolerance is hidden, so the page
        // reports the layered stack that actually stops it reducing to one raster.
        (
            "a wash inside the fill tolerance over a stack that does not reduce",
            format!(
                "1 1 1 scn -0.5 -0.5 613 793 re f\n{cover0}q 100 0 0 100 100 100 cm /Im1 Do Q\n"
            ),
            vec![(1, plain.clone()), (2, plain)],
            "multiple-images",
        ),
    ];
    for (name, src, info, want) in cases {
        let got = classify(&page, &walk_page(&src, ""), &facts(info)).1;
        assert_eq!(got, want, "{name}");
    }
}

// byb-7aq, the second shape: no clip is in force when the wash is painted, and what hides the
// wash is an opaque page-covering image painted AFTER it (govdocs1/050101.pdf page 49).
#[test]
fn classify_wash_hidden_by_an_earlier_opaque_image() {
    let src = "q /Cs1 cs 1 sc 0 768 1024 -768 re f Q\nq 0 0 1024 768 re W n\n\
               q 1024 0 0 768 0 0 cm /Im0 Do Q\nQ\nq -58 0.573425 1138.94 767.427 re W n\n\
               q 1228.8 0 0 985.9 -103.44 -104.6 cm /Im1 Do Q\nQ\n";
    let got = classify_plain(&page_rect(0.0, 0.0, 1024.0, 768.0), &walk_page(src, "")).1;
    assert_eq!(got, "");
}

// byb-kpi: `opaque_cover` depends only on the placement, and it used to be called from inside
// the per-(paint, placement) loop, which made classification quadratic in two attacker-chosen
// counts. Counting the calls is exact where a timing assertion is a flake.
#[test]
fn paints_hidden_answers_cover_once_per_placement() {
    const N_PAINTS: usize = 40;
    const N_IMGS: usize = 25;
    // Only the LAST placement covers the page. The others are opaque, so `opaque_cover` is asked
    // about them, but their boxes do not contain the ink, so the loop runs to the end for every
    // paint.
    let imgs: Vec<Placement> = (0..N_IMGS)
        .map(|i| Placement {
            id: i as i64 + 1,
            opaque: true,
            index: 1000 + i as i64,
            bbox: if i == N_IMGS - 1 {
                bx(0.0, 0.0, 612.0, 792.0)
            } else {
                bx(500.0, 600.0, 510.0, 610.0)
            },
            ctm: Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]),
            ..Placement::default()
        })
        .collect();
    let paints: Vec<Paint> = (0..N_PAINTS)
        .map(|i| Paint {
            op: "f",
            bbox: bx(10.0, 10.0, 20.0, 20.0),
            fill: Color::default(),
            stroke: Color::default(),
            index: i as i64,
            clip: None,
            ctm: IDENTITY,
        })
        .collect();
    let calls = Cell::new(0usize);
    let info = |_: i64| {
        calls.set(calls.get() + 1);
        Some(ImageInfo::default())
    };
    assert!(paints_hidden(&imgs, &paints, &info));
    assert_eq!(
        calls.get(),
        N_IMGS,
        "once per placement; a count near {} means opaque_cover moved back inside the per-paint loop",
        N_IMGS * N_PAINTS
    );
}

// byb-2mt: replacing the axis-aligned bounding-box test with a true-quadrilateral one at the
// covers/contains and vector-ink sites. This is exactly what byblos's own straighten emits for
// `deg: 1.9` on a 612x792 page, written at %.4f (not 2.0 degrees, which that format perturbs past
// MAX_SKEW_DEG and so diverts "rotated-placement", silencing every fixture here).
const ROTATED_1P9: &str = "611.6635 20.2910 -26.2589 791.5646 13.2977 -9.9278";

// Fixture 1: a rotated top placement over a full-page axis-aligned under-layer. It stops at
// `contains`, not `covers`: top's AABB does cover the page, but top's TRUE quadrilateral does
// not reach the under-layer's own page-sized quad.
#[test]
fn classify_rotated_top_over_full_page_underlayer_diverts_on_contains() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let src = format!("q 612 0 0 792 0 0 cm /Im0 Do Q\nq {ROTATED_1P9} cm /Im1 Do Q\n");
    assert_eq!(
        classify_plain(&page, &walk_page(&src, "")).1,
        "multiple-images"
    );
    // The lone control: a lone raster is never asked to cover the page (byb-b1.3).
    let src = format!("q {ROTATED_1P9} cm /Im1 Do Q\n");
    let (idx, got) = classify_plain(&page, &walk_page(&src, ""));
    assert_eq!(got, "");
    assert_eq!(idx, 0);
}

// Fixture 2: vector ink in a corner triangle a rotated raster's true quadrilateral does not
// reach, though its bounding box does. This stops at `ink_hidden`. The fill comes first:
// `ink_hidden` skips any placement with `index < order`.
#[test]
fn classify_vector_ink_hidden_by_rotated_top_only() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let src = format!("0 0 10 10 re f\nq {ROTATED_1P9} cm /Im1 Do Q\n");
    assert_eq!(
        classify_plain(&page, &walk_page(&src, "")).1,
        "vector-paint"
    );
}

// Fixture 3: isolates `contains` from `covers`. An oversized rotated top covers the page either
// way, but a small axis-aligned under-layer in one of the top's cut corners passes the AABB
// `contains` test and fails the true-quad one.
#[test]
fn classify_contains_diverts_on_under_layer_not_covered_by_top_quad() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let src = "q 20 0 0 20 -160 -165 cm /Im0 Do Q\n\
               q 899.5052 29.8397 -36.4707 1099.3952 -125.5172 -168.6174 cm /Im1 Do Q\n";
    assert_eq!(
        classify_plain(&page, &walk_page(src, "")).1,
        "multiple-images"
    );
}

/// `straightenedMatrixString`: the CTM that byblos's own straighten at `deg` leaves on the
/// "scan" corpus placement (`612 0 0 792 0 0`, centre (306, 396)), as the six operands of a `cm`
/// operator at `%.4f`. Go builds the page and reads it back through Inspect; the matrix is
/// computed here from the same rotation (pdfdoc `rotation_matrix`) composed with the placement.
fn straightened_matrix_string(deg: f64) -> String {
    let t = deg * std::f64::consts::PI / 180.0;
    let (c, s) = (cos(t), sin(t));
    let (cx, cy) = (306.0, 396.0);
    let rot = Matrix([
        c,
        s,
        -s,
        c,
        cx * (1.0 - c) + cy * s,
        cy * (1.0 - c) - cx * s,
    ]);
    let m = Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]).mul(&rot);
    m.0.iter()
        .map(|v| format_f(*v, 4))
        .collect::<Vec<_>>()
        .join(" ")
}

// Fixture 4: two placements at the IDENTICAL rotated matrix (the shape 16,241 measured Internet
// Archive pages have) must still reduce to one raster: for identical matrices the under-layer's
// true quad is exactly the top's, so quad-vs-quad containment holds even though quad-vs-box does
// not. 1.0 degree passes the covers gate and so isolates the `contains` site.
#[test]
fn classify_identical_rotated_stack_still_extracts() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    let m = straightened_matrix_string(1.0);
    let src = format!("q {m} cm /Im0 Do Q\nq {m} cm /Im1 Do Q\n");
    let (idx, got) = classify_plain(&page, &walk_page(&src, ""));
    assert_eq!(
        got, "",
        "identical rotated matrices must still reduce to one raster"
    );
    assert_eq!(idx, 1);
}

// Fixture 5's classify half: a page-covering wash under a page-covering raster, both through the
// SAME outer rotation straighten wraps the content in, so the wash's box is the AABB of an
// already-rotated shape. `ink_hidden`'s quad conjunct, applied naively to that box's own
// corners, would reject it. Every degree must still extract.
#[test]
fn classify_background_wash_straightened_still_extracts() {
    let page = page_rect(0.0, 0.0, 612.0, 792.0);
    for deg in [0.0, 0.1, 0.5, 1.0, 1.9] {
        let t = deg * std::f64::consts::PI / 180.0;
        let (c, s) = (cos(t), sin(t));
        let (cx, cy) = (306.0, 396.0);
        let rot = format!(
            "{} {} {} {} {} {}",
            format_f(c, 10),
            format_f(s, 10),
            format_f(-s, 10),
            format_f(c, 10),
            format_f(cx * (1.0 - c) + cy * s, 10),
            format_f(cy * (1.0 - c) - cx * s, 10)
        );
        let src = format!(
            "q\n{rot} cm\n/Cs6 cs 1 1 1 scn\n0 0 612 792 re\nf\n0 0 0 scn\nq 612 0 0 792 0 0 cm /Im0 Do Q\nQ\n"
        );
        let got = classify_plain(&page, &walk_page(&src, "")).1;
        assert_eq!(got, "", "deg={deg}");
    }
}

// extract_subpoint_test.go.
#[test]
fn box_rect_rounds_to_nearest_and_widens_only_to_stay_non_empty() {
    let tests = [
        // An ordinary box still rounds to nearest, and .5 still goes away from zero.
        (
            "a page-covering box is unchanged",
            bx(0.0, 0.0, 612.0, 792.0),
            Rect::new(0, 0, 612, 792),
        ),
        (
            "each edge rounds to nearest",
            bx(10.4, 10.6, 600.4, 700.6),
            Rect::new(10, 11, 600, 701),
        ),
        (
            "a half point rounds away from zero",
            bx(0.5, 1.5, 611.5, 791.5),
            Rect::new(1, 2, 612, 792),
        ),
        // The widening, and only where an axis would otherwise collapse.
        (
            "a sub-point square widens on both axes",
            bx(10.0, 10.0, 10.4, 10.4),
            Rect::new(10, 10, 11, 11),
        ),
        (
            "a sub-point stripe widens only on x",
            bx(10.0, 0.0, 10.4, 792.0),
            Rect::new(10, 0, 11, 792),
        ),
        // Both edges round UP to the same integer: the near edge has to move.
        (
            "a sub-point box between two integers widens the near edge too",
            bx(10.6, 10.6, 10.9, 10.9),
            Rect::new(10, 10, 11, 11),
        ),
        (
            "a sub-point box straddling an integer",
            bx(9.9, 9.9, 10.1, 10.1),
            Rect::new(9, 9, 11, 11),
        ),
        // byb-wtp: round(0.3) = 0 and round(595.6) = 596 independently, a width of 596 where the
        // continuous extent 595.3 rounds to 595.
        (
            "a wide sub-point offset matches poppler's continuous width",
            bx(0.3, 0.0, 595.6, 792.0),
            Rect::new(0, 0, 595, 792),
        ),
        // Edges round apart (10 and 11) but the continuous extent (0.4) rounds to 0.
        (
            "edges round apart but the continuous extent rounds to zero",
            bx(10.3, 0.0, 10.7, 792.0),
            Rect::new(10, 0, 11, 792),
        ),
        // A box with NO extent on an axis marks nothing, and stays empty.
        (
            "a zero-width box stays empty",
            bx(10.0, 10.0, 10.0, 700.0),
            Rect::new(10, 10, 10, 700),
        ),
        (
            "a zero-height box stays empty",
            bx(10.0, 10.0, 600.0, 10.0),
            Rect::new(10, 10, 600, 10),
        ),
        (
            "a point stays empty",
            bx(10.0, 10.0, 10.0, 10.0),
            Rect::new(10, 10, 10, 10),
        ),
    ];
    for (name, b, want) in tests {
        assert_eq!(box_rect(&b), want, "{name}");
    }
}

// byb-67j: `rect_of` projects the PAGE box, and a page box is not a placement box, so the rule
// was re-derived from poppler. A sub-point MediaBox is reported verbatim and rendered to at least
// one inked pixel; collapsing it says "no page" about a page poppler draws.
#[test]
fn rect_of_never_collapses_a_page_box_poppler_renders() {
    let r = |llx, lly, urx, ury| pdfdoc::Rect { llx, lly, urx, ury };
    let tests = [
        (
            "letter is unchanged",
            r(0.0, 0.0, 612.0, 792.0),
            Rect::new(0, 0, 612, 792),
        ),
        (
            "each edge rounds to nearest",
            r(10.4, 10.6, 600.4, 700.6),
            Rect::new(10, 11, 600, 701),
        ),
        // byb-67j's own producer: BuildPDF of a 5x2 px image at 300 DPI.
        (
            "a 5x2 px page at 300 DPI widens only on y",
            r(0.0, 0.0, 1.2, 0.48),
            Rect::new(0, 0, 1, 1),
        ),
        (
            "a sub-point page widens on both axes",
            r(0.0, 0.0, 0.4, 0.4),
            Rect::new(0, 0, 1, 1),
        ),
        (
            "the smallest page poppler still inks widens too",
            r(0.0, 0.0, 0.1, 0.1),
            Rect::new(0, 0, 1, 1),
        ),
        (
            "a sub-point page offset from the origin",
            r(10.6, 10.6, 10.9, 10.9),
            Rect::new(10, 10, 11, 11),
        ),
        // A box with no extent stays empty: `covers_page`'s empty-page guard depends on it.
        (
            "the zero rect stays empty",
            r(0.0, 0.0, 0.0, 0.0),
            Rect::new(0, 0, 0, 0),
        ),
        (
            "a zero-height page stays empty",
            r(0.0, 0.0, 612.0, 0.0),
            Rect::new(0, 0, 612, 0),
        ),
    ];
    for (name, b, want) in tests {
        assert_eq!(rect_of(&b), want, "{name}");
    }
}

#[test]
fn round_extent_survives_non_finite_boxes() {
    // Go's amd64 conversion of NaN or an out-of-range float is the "integer indefinite" value;
    // the point here is that nothing panics or overflows.
    let _ = box_rect(&bx(f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::NAN));
    let _ = box_rect(&bx(1e300, -1e300, 1e300, 1e300));
    let _ = rect_of(&pdfdoc::Rect {
        llx: f64::MAX,
        lly: f64::MIN,
        urx: f64::MAX,
        ury: f64::MAX,
    });
}

#[test]
fn normalized_box_orders_corners() {
    assert_eq!(
        normalized_box(612.0, 792.0, 0.0, 0.0),
        [0.0, 0.0, 612.0, 792.0]
    );
    assert_eq!(
        normalized_box(0.0, 792.0, 612.0, 0.0),
        [0.0, 0.0, 612.0, 792.0]
    );
    assert_eq!(
        normalized_box(0.0, 0.0, 612.0, 792.0),
        [0.0, 0.0, 612.0, 792.0]
    );
}

#[test]
fn gray_levels_apply_the_decode_array() {
    let info = |decode: Option<Vec<f64>>, cs: &str| ImageInfo {
        bpc: 1,
        color_space: cs.to_string(),
        decode: decode.is_some() || cs == "DeviceGray",
        decode_array: decode.map(Arc::from),
        ..ImageInfo::default()
    };
    // No /Decode: the default array [0 1].
    let none = ImageInfo {
        bpc: 1,
        ..ImageInfo::default()
    };
    assert_eq!(gray_levels(&none, 1).unwrap(), (0x00, 0xFF));
    let g = |a: &[f64]| gray_levels(&info(Some(a.to_vec()), "DeviceGray"), 1).unwrap();
    // The rows of the poppler table in `gray_levels`.
    assert_eq!(g(&[0.0, 1.0]), (0x00, 0xFF));
    assert_eq!(g(&[1.0, 0.0]), (0xFF, 0x00));
    assert_eq!(g(&[0.5, 1.0]), (128, 0xFF));
    assert_eq!(g(&[0.0, 0.0]), (0, 0));
    assert_eq!(g(&[2.0, -1.0]), (0xFF, 0x00));
    assert_eq!(g(&[0.25, 0.75]), (64, 191));
    assert_eq!(g(&[0.3333, 0.6667]), (85, 170));
    assert_eq!(g(&[f64::NAN, 1.0]), (0, 0xFF));
    // Refused: a colour space that is not /DeviceGray (an Indexed palette lives behind an array
    // or a reference), an array that is not all numbers, one that is not two long.
    for e in [
        gray_levels(&info(Some(vec![1.0, 0.0]), ""), 7).unwrap_err(),
        gray_levels(&info(Some(vec![1.0, 0.0]), "DeviceRGB"), 7).unwrap_err(),
        gray_levels(&info(None, "DeviceGray"), 7).unwrap_err(),
        gray_levels(&info(Some(vec![1.0, 0.0, 1.0]), "DeviceGray"), 7).unwrap_err(),
    ] {
        assert!(e.to_string().contains("/Decode array"), "{e}");
        assert!(
            matches!(&e, Error::Malformed(m) if m.starts_with("jbig2:")),
            "{e}"
        );
    }
}

#[test]
fn gray_image_writes_ink_and_background() {
    let lim = Limits::default();
    let mut b = Bitmap::new(10, 2, &lim).unwrap();
    b.set(0, 0, 1);
    b.set(9, 1, 1);
    let g = gray_image(&b, 0x00, 0xFF, &lim, &never()).unwrap();
    assert_eq!(g.rect, Rect::new(0, 0, 10, 2));
    assert_eq!(g.pix[0], 0x00);
    assert_eq!(g.pix[1], 0xFF);
    assert_eq!(g.pix[10 + 9], 0x00);
    assert_eq!(g.pix.iter().filter(|&&v| v == 0).count(), 2);
    let inv = gray_image(&b, 0xFF, 0x00, &lim, &never()).unwrap();
    assert_eq!(inv.pix[0], 0xFF);
    assert_eq!(inv.pix[1], 0x00);
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        gray_image(&b, 0, 255, &lim, &cancelled).unwrap_err(),
        Error::Cancelled
    );
    let tiny = Limits {
        max_alloc: 4,
        ..Limits::default()
    };
    assert!(matches!(
        gray_image(&b, 0, 255, &tiny, &never()),
        Err(Error::Alloc { .. })
    ));
}

#[test]
fn decode_jbig2_placement_checks_the_dictionary_before_the_stream() {
    let lim = Limits::default();
    let data = b"not a jbig2 stream";
    // No dictionary.
    let e = decode_jbig2_placement(data, &[], None, 3, &lim, &never()).unwrap_err();
    assert!(
        matches!(&e, Error::Malformed(m) if m.starts_with("jbig2: image 3 has no dictionary")),
        "{e}"
    );
    // A declared size over the page cap is refused without opening the stream.
    let big = ImageInfo {
        width: 1 << 14,
        height: 1 << 14,
        bpc: 1,
        ..ImageInfo::default()
    };
    let e = decode_jbig2_placement(data, &[], Some(&big), 3, &lim, &never()).unwrap_err();
    assert!(e.to_string().contains("the limit is 67108864"), "{e}");
    // A /Decode array the build declines.
    let bad = ImageInfo {
        width: 8,
        height: 8,
        bpc: 1,
        decode: true,
        color_space: "DeviceRGB".to_string(),
        ..ImageInfo::default()
    };
    let e = decode_jbig2_placement(data, &[], Some(&bad), 3, &lim, &never()).unwrap_err();
    assert!(e.to_string().contains("/Decode array"), "{e}");
    // Garbage under a fine dictionary is damage, with jbig2's own error.
    let ok = ImageInfo {
        width: 8,
        height: 8,
        bpc: 1,
        ..ImageInfo::default()
    };
    assert!(decode_jbig2_placement(data, &[], Some(&ok), 3, &lim, &never()).is_err());
}

#[test]
fn codec_errors_keep_go_s_text_and_chain() {
    let e = not_single_raster("vector-paint");
    assert_eq!(
        e.to_string(),
        "byblos: page is not a single raster: vector-paint"
    );
    assert!(e.is_not_single_raster());
    assert!(!e.is_unsupported_image_codec());

    let n = not_implemented("decode-jpx", "why");
    assert_eq!(n.issue, "byb-9bn");
    let e = codec_err("jpx", Some(n));
    assert!(
        e.to_string()
            .starts_with("byblos: page raster uses an image codec byblos cannot decode: jpx (")
    );
    assert!(e.is_unsupported_image_codec());
    assert!(e.is_not_implemented());
    assert_eq!(
        e.as_not_implemented().map(|n| n.capability.as_str()),
        Some("decode-jpx")
    );
    assert!(!e.is_not_single_raster());

    let (r, rec) = locked(|| refuse(4, "unsupported-codec-tif", e));
    let r = r.unwrap_err();
    assert_eq!(r.class(), "refused/unsupported-codec-tif");
    assert!(r.is_unsupported_image_codec());
    match &r {
        Error::Refused {
            page,
            reason,
            class,
            ..
        } => {
            assert_eq!(*page, 4);
            assert_eq!(reason, "unsupported-codec-tif");
            assert_eq!(class, "unsupported-codec-tiff");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rec.diverted, "unsupported-codec-tiff");
    assert!(rec.geometry.is_none());
}

/// The zlib stream of one stored block, which needs no deflate encoder.
fn stored_zlib(data: &[u8]) -> Vec<u8> {
    let n = u16::try_from(data.len()).unwrap();
    let mut v = vec![0x78, 0x01, 0x01];
    v.extend_from_slice(&n.to_le_bytes());
    v.extend_from_slice(&(!n).to_le_bytes());
    v.extend_from_slice(data);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    v.extend_from_slice(&((b << 16) | a).to_be_bytes());
    v
}

// byb-67j end to end: BuildPDF of a 5x2 px image at 300 DPI is a 1.2 x 0.48 pt page, byblos's own
// output and the only known producer of one. byb-62t made `bounds` non-empty on it while `page`
// stayed empty, so a raster covering 100% of its page reported bounds not contained in it.
#[test]
fn a_sub_point_page_is_covered_by_its_own_full_page_raster() {
    let (w, h) = (5i64, 2i64);
    let img = EncodedImage {
        width: w,
        height: h,
        bpc: 8,
        color_space: ColorSpace {
            name: "DeviceGray".to_string(),
            ..ColorSpace::default()
        },
        filter: "FlateDecode".to_string(),
        data: stored_zlib(&[0u8; 10]),
        ..EncodedImage::default()
    };
    let lim = Limits::default();
    let doc = build_pdf(
        &[BuildPage {
            image: img,
            dpi: 300.0,
            ..BuildPage::default()
        }],
        &lim,
        &never(),
    )
    .unwrap();
    let r = extract(&doc, 1).unwrap();
    assert!(
        !r.page.is_empty(),
        "page {:?} beside bounds {:?}",
        r.page,
        r.bounds
    );
    assert!(
        r.covers_page(),
        "bounds {:?} against page {:?}; the raster IS the page",
        r.bounds,
        r.page
    );
    // The declared depth is 8, and the object number is a real indirect object.
    assert!(!r.bitonal);
    assert!(r.obj_nr > 0);
    assert_eq!(r.raster_quad, None);
}

#[test]
fn page_out_of_range_and_garbage_are_failures_not_refusals() {
    let lim = Limits::default();
    let e = extract(b"not a pdf", 1).unwrap_err();
    assert!(!matches!(e, Error::Refused { .. }));
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        extract_page_raster(b"not a pdf", 1, &lim, &cancelled).unwrap_err(),
        Error::Cancelled
    );
    let img = EncodedImage {
        width: 1,
        height: 1,
        bpc: 8,
        color_space: ColorSpace {
            name: "DeviceGray".to_string(),
            ..ColorSpace::default()
        },
        filter: "FlateDecode".to_string(),
        data: stored_zlib(&[0]),
        ..EncodedImage::default()
    };
    let doc = build_pdf(
        &[BuildPage {
            image: img,
            width_pt: 10.0,
            height_pt: 10.0,
            ..BuildPage::default()
        }],
        &lim,
        &never(),
    )
    .unwrap();
    for n in [0usize, 2] {
        assert!(extract(&doc, n).is_err(), "page {n}");
    }
    assert!(extract(&doc, 1).is_ok());
}

/// `dcOnlyJPEG`: a baseline 3-component JPEG whose blocks carry a DC coefficient only, with Y
/// sampled `hy` x `vy` and Cb, Cr 1x1, so the chroma sampling is 4:4:0 (1x2), 4:1:1 (4x1) or
/// 4:1:0 (4x2) as the arguments say. DC steps differ block to block so upsampling and IDCT
/// differences show.
fn dc_only_jpeg(w: usize, h: usize, hy: usize, vy: usize) -> Vec<u8> {
    fn seg(b: &mut Vec<u8>, marker: u8, body: &[u8]) {
        let n = body.len() + 2;
        b.extend_from_slice(&[0xFF, marker, (n >> 8) as u8, n as u8]);
        b.extend_from_slice(body);
    }
    struct Bits {
        out: Vec<u8>,
        acc: u32,
        n: u32,
    }
    impl Bits {
        fn put(&mut self, v: i32, n: u32) {
            for i in (0..n).rev() {
                self.acc = (self.acc << 1) | ((v >> i) & 1) as u32;
                self.n += 1;
                if self.n == 8 {
                    self.out.push(self.acc as u8);
                    if self.acc == 0xFF {
                        self.out.push(0);
                    }
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
    }

    let mut b = vec![0xFF, 0xD8];
    let mut dqt = vec![0u8];
    dqt.extend_from_slice(&[8u8; 64]);
    seg(&mut b, 0xDB, &dqt);
    seg(
        &mut b,
        0xC0,
        &[
            8,
            (h >> 8) as u8,
            h as u8,
            (w >> 8) as u8,
            w as u8,
            3,
            1,
            ((hy << 4) | vy) as u8,
            0,
            2,
            0x11,
            0,
            3,
            0x11,
            0,
        ],
    );
    let dc_counts: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    let dc_vals: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    let mut dht = vec![0x00u8];
    dht.extend_from_slice(&dc_counts);
    dht.extend_from_slice(&dc_vals);
    seg(&mut b, 0xC4, &dht);
    // Two 2-bit codes: 00 is EOB, 01 is ZRL.
    let mut ac_counts = [0u8; 16];
    ac_counts[1] = 2;
    let mut dht = vec![0x10u8];
    dht.extend_from_slice(&ac_counts);
    dht.extend_from_slice(&[0x00, 0xF0]);
    seg(&mut b, 0xC4, &dht);
    seg(&mut b, 0xDA, &[3, 1, 0, 2, 0, 3, 0, 0, 63, 0]);

    // (code, length) of each DC category.
    let mut dc = [(0u32, 0u32); 12];
    let (mut c, mut k) = (0u32, 0usize);
    for l in 1..=16 {
        for _ in 0..dc_counts[l - 1] {
            dc[usize::from(dc_vals[k])] = (c, l as u32);
            c += 1;
            k += 1;
        }
        c <<= 1;
    }
    let mut bits = Bits {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    let mut pred = [0i32; 3];
    let mut block = |bits: &mut Bits, comp: usize, val: i32| {
        let mut d = val - pred[comp];
        pred[comp] = val;
        let mut cat = 0u32;
        let mut a = d.abs();
        while a > 0 {
            cat += 1;
            a >>= 1;
        }
        let (code, len) = dc[cat as usize];
        bits.put(code as i32, len);
        if d < 0 {
            d += (1 << cat) - 1;
        }
        bits.put(d, cat);
        bits.put(0, 2); // EOB
    };
    let (mw, mh) = (w.div_ceil(8 * hy), h.div_ceil(8 * vy));
    for my in 0..mh {
        for mx in 0..mw {
            for j in 0..vy {
                for i in 0..hy {
                    let (blk_x, blk_y) = (mx * hy + i, my * vy + j);
                    block(&mut bits, 0, ((blk_x * 37 + blk_y * 53) % 97) as i32 - 48);
                }
            }
            block(&mut bits, 1, ((mx * 29 + my * 11) % 41) as i32 - 20);
            block(&mut bits, 2, ((mx * 7 + my * 31) % 37) as i32 - 18);
        }
    }
    bits.put(0x7F, 7); // pad with ones
    b.extend_from_slice(&bits.out);
    b.extend_from_slice(&[0xFF, 0xD9]);
    b
}

// A DCT page is decoded by jpeg-rs (the jpeg-go copy), so its pixels are libjpeg-turbo's (the
// decoder poppler and ocrmypdf use), not image/jpeg's: IDCT and chroma upsampling differ. 4:4:0,
// 4:1:1 and 4:1:0 are the samplings the copy added.
#[test]
fn extract_dct_page_decodes_with_jpeg_rs() {
    let lim = Limits::default();
    for (name, hy, vy) in [
        ("440", 1, 2),
        ("411", 4, 1),
        ("410", 4, 2),
        ("420", 2, 2),
        ("444", 1, 1),
    ] {
        let data = dc_only_jpeg(75, 50, hy, vy);
        let (enc, _) = embed_jpeg(&data, &lim).unwrap();
        let doc = build_pdf(
            &[BuildPage {
                image: enc,
                width_pt: 75.0,
                height_pt: 50.0,
                ..BuildPage::default()
            }],
            &lim,
            &never(),
        )
        .unwrap();
        let pr = extract(&doc, 1).unwrap();
        let want = jpeg::decode(
            &data,
            &jpeg::DecodeOptions {
                format: jpeg::Format::Rgba,
                ..jpeg::DecodeOptions::default()
            },
            &jpeg::Limits::default(),
            &never(),
        )
        .unwrap();
        let jpeg::Image::Rgba(want) = want.image else {
            panic!("{name}: jpeg-rs did not return Rgba");
        };
        let Image::Rgba(got) = &pr.image else {
            panic!("{name}: extract returned {:?}", pr.image.bounds());
        };
        assert_eq!(got.rect, Rect::from(want.rect), "{name}: bounds");
        let bad = got
            .pix
            .iter()
            .zip(&want.pix)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(got.pix.len(), want.pix.len(), "{name}");
        assert_eq!(bad, 0, "{name}: bytes differ from the jpeg-rs decode");
    }
}

// ---- tests that need corpus PDFs ----------------------------------------------------------

fn corpus(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/corpus/{name}.pdf",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn extract(data: &[u8], page: usize) -> Result<PageRaster> {
    locked(|| extract_page_raster(data, page, &Limits::default(), &never()))
}

fn extract_rec(data: &[u8], page: usize) -> Extracted {
    let mut d = pdfdoc::open(data, &Limits::default(), &never()).unwrap();
    locked(|| extract_page(&mut d, page, &never()))
}

/// The object numbers of the images of page 1 in paint order, as `Inspect` reports them.
fn image_obj_nrs(data: &[u8]) -> Vec<i64> {
    let lim = Limits::default();
    let mut d = pdfdoc::open(data, &lim, &never()).unwrap();
    let mut p = d.page(1).unwrap();
    let scan = scan_page(&mut d, &mut p, 1, &never()).unwrap();
    scan.images
        .iter()
        .map(|pl| d.image_info(pl.id).map_or(0, |i| i.obj_nr))
        .collect()
}

fn dims(r: &PageRaster) -> (i64, i64) {
    let b = r.image.bounds();
    (b.dx(), b.dy())
}

#[test]
#[ignore = "needs tests/data/corpus/{scan,scan-rotated,scan-in-form,background-wash}.pdf from the Go oracle -gen"]
fn extract_page_raster_succeeds() {
    for name in ["scan", "scan-rotated", "scan-in-form", "background-wash"] {
        let pr = extract(&corpus(name), 1).unwrap();
        // Every document here is placed across the whole page box.
        assert!(
            pr.covers_page(),
            "{name}: {:?} should fill {:?}",
            pr.bounds,
            pr.page
        );
        assert_eq!(dims(&pr), (SCAN_W, SCAN_H), "{name}");
    }
}

// byb-b1.12's acceptance criterion: a clipped page-covering image reports the visible box and no
// longer reports covers_page.
#[test]
#[ignore = "needs tests/data/corpus/scan-clipped-corner.pdf from the Go oracle -gen"]
fn extract_page_raster_reports_the_clipped_box_not_the_raster_placement() {
    let pr = extract(&corpus("scan-clipped-corner"), 1).unwrap();
    assert_eq!(pr.bounds, Rect::new(0, 0, 100, 100));
    assert!(!pr.covers_page());
}

#[test]
#[ignore = "needs tests/data/corpus/scan-clipped-away.pdf from the Go oracle -gen"]
fn extract_page_raster_diverts_when_the_clip_leaves_nothing_visible() {
    let data = corpus("scan-clipped-away");
    assert!(extract(&data, 1).unwrap_err().is_not_single_raster());
    // RecordExtraction's page record is extract_page's.
    let (r, rec) = extract_rec(&data, 1);
    assert!(r.is_err());
    assert!(
        !rec.diverted.is_empty(),
        "want the page reported as diverted"
    );
    assert!(
        rec.geometry.is_none(),
        "a diverted page carries no geometry"
    );
}

// The crop comes from a Form XObject's own /BBox, read off the real PDF by pdfdoc.
#[test]
#[ignore = "needs tests/data/corpus/scan-cropped-by-form-bbox.pdf from the Go oracle -gen"]
fn extract_page_raster_reports_the_form_bbox_crop() {
    let data = corpus("scan-cropped-by-form-bbox");
    let pr = extract(&data, 1).unwrap();
    assert_eq!(pr.bounds, Rect::new(0, 0, 100, 100));
    assert!(!pr.covers_page());
    let (_, rec) = extract_rec(&data, 1);
    let geom = rec.geometry.unwrap();
    assert_eq!(geom.clip_box, Some([0.0, 0.0, 100.0, 100.0]));
}

// The ClipBox must come from placement.clip, not from the narrowed raster box.
#[test]
#[ignore = "needs tests/data/corpus/scan-clip-narrower-than-raster-box.pdf from the Go oracle -gen"]
fn extract_page_raster_clip_box_is_the_clip_not_the_narrowed_raster_box() {
    let (_, rec) = extract_rec(&corpus("scan-clip-narrower-than-raster-box"), 1);
    let geom = rec.geometry.unwrap();
    assert_eq!(geom.raster_box, [0.0, 0.0, 100.0, 200.0]);
    assert_eq!(geom.clip_box, Some([-50.0, -50.0, 200.0, 200.0]));
}

// Two page-covering placements at the identical CTM are one visible raster: the second.
#[test]
#[ignore = "needs tests/data/corpus/{stacked,stacked-in-form}.pdf from the Go oracle -gen"]
fn extract_page_raster_takes_the_occluding_layer() {
    let first_gray = |seed: i64| (seed * 31 % 251) as u32;
    let (top, base) = (first_gray(5), first_gray(4));
    assert_ne!(top, base);
    for name in ["stacked", "stacked-in-form"] {
        let pr = extract(&corpus(name), 1).unwrap();
        let (r, ..) = pr.image.rgba_at(0, 0);
        assert_eq!(
            r >> 8,
            top,
            "{name}: first pixel; {base} is the occluded base"
        );
    }
}

// PageRaster.obj_nr is the same object number Inspect reports for the image it came from.
#[test]
#[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
fn extract_page_raster_obj_nr_matches_inspect() {
    let data = corpus("scan");
    let before = image_obj_nrs(&data);
    assert_eq!(before.len(), 1, "single-image page");
    assert_eq!(extract(&data, 1).unwrap().obj_nr, before[0]);
}

// On a stacked two-layer page obj_nr is the VISIBLE top image's object number, not the first
// placement's (16,241 Internet Archive pages are shaped like this).
#[test]
#[ignore = "needs tests/data/corpus/{stacked,stacked-in-form}.pdf from the Go oracle -gen"]
fn extract_page_raster_obj_nr_is_the_visible_top_layer() {
    for name in ["stacked", "stacked-in-form"] {
        let data = corpus(name);
        let nrs = image_obj_nrs(&data);
        assert_eq!(nrs.len(), 2, "{name}: base + occluding layer");
        let (under, top) = (nrs[0], nrs[1]);
        assert_ne!(under, top, "{name}");
        let got = extract(&data, 1).unwrap().obj_nr;
        assert_eq!(got, top, "{name}");
        assert_ne!(got, under, "{name}: the HIDDEN under-layer");
    }
}

// The headline correction of byb-b1.1: three shapes of an invisible OCR layer.
#[test]
#[ignore = "needs tests/data/corpus/{invisible-text,invisible-text-in-form,invisible-text-form-inherits,invisible-text-bracketed}.pdf from the Go oracle -gen"]
fn extract_page_raster_extracts_under_an_invisible_text_layer() {
    for name in [
        "invisible-text",
        "invisible-text-in-form",
        "invisible-text-form-inherits",
        "invisible-text-bracketed",
    ] {
        let pr = extract(&corpus(name), 1).unwrap();
        assert_eq!(dims(&pr), (SCAN_W, SCAN_H), "{name}");
    }
}

// byb-b1.2: the stored raster of a deskewed page is returned as stored, not resampled. scan and
// scan-deskewed hold the same raster object, so anything but pixel equality means a rewrite.
#[test]
#[ignore = "needs tests/data/corpus/{scan,scan-deskewed}.pdf from the Go oracle -gen"]
fn extract_page_raster_keeps_a_deskewed_raster_as_stored() {
    let got = extract(&corpus("scan-deskewed"), 1).unwrap().image;
    let want = extract(&corpus("scan"), 1).unwrap().image;
    assert_eq!(got.bounds(), want.bounds());
    assert_eq!(got, want);
}

// byb-b1.3: ia-DTIC_ADA383635.pdf p40 is 568.37 x 791.76 points on a 612x792 MediaBox.
#[test]
#[ignore = "needs tests/data/corpus/scan-natural-dpi.pdf from the Go oracle -gen"]
fn extract_page_raster_accepts_a_natural_dpi_placement() {
    let pr = extract(&corpus("scan-natural-dpi"), 1).unwrap();
    assert!(!pr.covers_page());
    let short = pr.page.dx() - pr.bounds.dx();
    assert!(
        (40..=48).contains(&short),
        "horizontal shortfall {short}; the measured page is 43.6"
    );
    assert_eq!(pr.page.dy() - pr.bounds.dy(), 0);
    assert_eq!(
        dims(&pr),
        (SCAN_W, SCAN_H),
        "returned as stored, not padded to the MediaBox"
    );
}

#[test]
#[ignore = "needs tests/data/corpus/{born-digital,overlay-text,tiled,overlay-vector,stacked-smask,stacked-alpha,mrc,mrc-inset-base,scan-quarter-turn,scan-mirrored}.pdf from the Go oracle -gen"]
fn extract_page_raster_diverts() {
    for (doc, reason) in [
        ("born-digital", "no-image"),
        ("overlay-text", "has-text"),
        ("tiled", "multiple-images"),
        ("overlay-vector", "vector-paint"),
        ("stacked-smask", "transparent-overlay"),
        ("stacked-alpha", "transparent-overlay"),
        ("mrc", "mrc-layers"),
        ("mrc-inset-base", "mrc-layers"),
        // byb-b1.2: a quarter turn and a mirror are exactly correctable, but the image is
        // consumed before anyone reads provenance.
        ("scan-quarter-turn", "rotated-placement"),
        ("scan-mirrored", "flipped-placement"),
    ] {
        let e = extract(&corpus(doc), 1).unwrap_err();
        assert!(e.is_not_single_raster(), "{doc}: {e}");
        assert!(
            e.to_string().contains(reason),
            "{doc}: {e} should name {reason}"
        );
        assert_eq!(e.class(), format!("refused/{reason}"), "{doc}");
    }
}

// The trap ErrUnsupportedImageCodec exists for: pdf-rs returns a JBIG2 payload as opaque bytes
// with no error. The corpus payload is 64 bytes of filler, not a JBIG2 stream at all.
#[test]
#[ignore = "needs tests/data/corpus/jbig2.pdf from the Go oracle -gen"]
fn extract_page_raster_rejects_jbig2() {
    let e = extract(&corpus("jbig2"), 1).unwrap_err();
    assert!(e.is_unsupported_image_codec(), "{e}");
    assert!(
        !e.is_not_single_raster(),
        "a JBIG2 page-covering scan IS a single raster"
    );
    assert!(
        !e.to_string().contains("image: unknown format"),
        "the jbig2 branch was bypassed: {e}"
    );
    assert!(
        e.to_string()
            .starts_with("byblos: page raster uses an image codec byblos cannot decode: jbig2: "),
        "{e}"
    );
}

// The jpx arm refuses on the declared filter alone and wraps a NotImplemented.
#[test]
#[ignore = "needs tests/data/corpus/jpx.pdf from the Go oracle -gen"]
fn extract_page_raster_rejects_jpx() {
    let e = extract(&corpus("jpx"), 1).unwrap_err();
    assert!(e.is_unsupported_image_codec(), "{e}");
    assert!(!e.is_not_single_raster());
    assert_eq!(
        e.as_not_implemented().map(|n| n.capability.as_str()),
        Some("decode-jpx")
    );
    assert!(
        e.to_string()
            .starts_with("byblos: page raster uses an image codec byblos cannot decode: jpx ("),
        "{e}"
    );
}

// Page 2 of the mixed document is a clean scan even though page 1 is not.
#[test]
#[ignore = "needs tests/data/corpus/mixed.pdf from the Go oracle -gen"]
fn extract_page_raster_is_per_page() {
    let data = corpus("mixed");
    assert!(extract(&data, 1).unwrap_err().is_not_single_raster());
    assert!(extract(&data, 2).is_ok());
}

// Extraction is per-object, not merely per-page: both pages hold the same raster bytes as two
// distinct objects that the optimize pass deduplicates.
#[test]
#[ignore = "needs tests/data/corpus/dup-raster.pdf from the Go oracle -gen"]
fn extract_page_raster_handles_deduplicated_rasters() {
    let data = corpus("dup-raster");
    for page in [1, 2] {
        let pr = extract(&data, page).unwrap();
        assert_eq!(dims(&pr), (SCAN_W, SCAN_H), "page {page}");
    }
}

#[test]
#[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
fn extract_page_raster_out_of_range() {
    let data = corpus("scan");
    for n in [0usize, 2] {
        assert!(extract(&data, n).is_err(), "page {n}");
    }
}

#[test]
#[ignore = "needs tests/data/corpus/malformed.pdf from the Go oracle -gen"]
fn extract_page_raster_malformed() {
    assert!(extract(&corpus("malformed"), 1).is_err());
}

// byb-b1.11: the content stream proves nothing marks the strip, and it is the annotation, which is
// not in the content stream at all, that puts ink there.
#[test]
#[ignore = "needs tests/data/corpus/scan-stamped.pdf from the Go oracle -gen"]
fn extract_page_raster_reports_a_stamp_it_cannot_include() {
    let pr = extract(&corpus("scan-stamped"), 1).unwrap();
    assert!(!pr.covers_page(), "the natural-DPI raster leaves a strip");
    assert_eq!(pr.dropped_annots, 1);
}

#[test]
#[ignore = "needs tests/data/corpus/{scan-natural-dpi,scan,scan-deskewed}.pdf from the Go oracle -gen"]
fn extract_page_raster_reports_no_annots_when_there_are_none() {
    for name in ["scan-natural-dpi", "scan", "scan-deskewed"] {
        assert_eq!(
            extract(&corpus(name), 1).unwrap().dropped_annots,
            0,
            "{name}"
        );
    }
}

// The decoy in indirect-kids is hidden, zero-area and has no appearance: a viewer draws nothing.
#[test]
#[ignore = "needs tests/data/corpus/indirect-kids.pdf from the Go oracle -gen"]
fn extract_page_raster_ignores_a_non_painting_annotation() {
    assert_eq!(
        extract(&corpus("indirect-kids"), 1).unwrap().dropped_annots,
        0
    );
}

// byb-xcx: the DECLARED depth, never the pixels. scan-bilevel is the only bitonal corpus document
// that reaches extract_page_raster; jbig2 is bitonal too but diverts as an undecodable codec.
// Go walks corpus.All(); the names here are the documents of that set this port has a fixture
// for, and the test asserts the population so it cannot pass vacuously.
#[test]
#[ignore = "needs tests/data/corpus/{scan-bilevel,scan,scan-rotated,scan-in-form,background-wash}.pdf from the Go oracle -gen"]
fn page_raster_carries_the_declared_bitonal_flag() {
    let (mut bitonal_seen, mut contone_seen) = (0, 0);
    for name in [
        "scan-bilevel",
        "scan",
        "scan-rotated",
        "scan-in-form",
        "background-wash",
    ] {
        let pr = extract(&corpus(name), 1).unwrap();
        let want = name == "scan-bilevel";
        assert_eq!(pr.bitonal, want, "{name}");
        if want {
            bitonal_seen += 1;
        } else {
            contone_seen += 1;
        }
    }
    assert!(bitonal_seen > 0 && contone_seen > 0);
}

// The end-to-end halves of the byb-2mt fixtures: a straightened page of the corpus must still
// extract (BuildFromPages is pdfdoc's; the straightened document is built there).
#[test]
#[ignore = "needs tests/data/corpus/{background-wash,stacked}.pdf from the Go oracle -gen"]
fn straightened_background_wash_and_stack_still_extract() {
    use crate::pdfdoc::buildpages::{PageSource, build_from_pages};
    use crate::pdfdoc::straighten::StraightenSpec;
    let lim = Limits::default();
    for (name, degs) in [
        ("background-wash", vec![0.0, 0.1, 0.5, 1.0, 1.9]),
        ("stacked", vec![1.0]),
    ] {
        let src = corpus(name);
        for deg in degs {
            let page = PageSource {
                source: 0,
                page: 1,
                rotate: 0,
                straighten: Some(StraightenSpec { deg, crop: None }),
            };
            let out = build_from_pages(&[&src], &[page], &lim, &never()).unwrap();
            assert!(
                extract(&out, 1).is_ok(),
                "{name} at {deg} degrees must not divert"
            );
        }
    }
}
