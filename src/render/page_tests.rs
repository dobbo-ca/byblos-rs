//! Ports of `internal/render/render_test.go`, `imagedraw_test.go` and `caps_test.go`.
//!
//! Left out: `oracle_test.go` (compares against `pdftoppm` and builds PDFs through pdfdoc) and
//! `byb6ty_sample_test.go` (needs the `BYBLOS_SAMPLE` corpus and pdfdoc).

use super::*;
use crate::image::{Gray, Nrgba};
use std::sync::atomic::Ordering;

const BOX100: BBox = BBox {
    llx: 0.0,
    lly: 0.0,
    urx: 100.0,
    ury: 100.0,
};

type Px = [u8; 4];
const WHITE: Px = [255, 255, 255, 255];
const BLACK: Px = [0, 0, 0, 255];
const RED: Px = [255, 0, 0, 255];
const BLUE: Px = [0, 0, 255, 255];
const GREEN: Px = [0, 255, 0, 255];

fn params(bbox: BBox) -> PageParams {
    PageParams {
        bbox,
        rotate: 0,
        scale: 1.0,
    }
}

/// `Page` on `box100` at scale 1 with no resolvers, under `lim`.
fn run_limits(src: &str, lim: &Limits) -> Result<Rgba> {
    let cancel = AtomicBool::new(false);
    page(src.as_bytes(), &params(BOX100), None, None, lim, &cancel)
}

/// `render100`: a 100x100pt page rendered at scale 1: one device pixel per point, device row 0 at
/// user y=100.
fn render100(src: &str) -> Rgba {
    let img = run_limits(src, &Limits::default()).unwrap();
    assert_eq!(img.rect, Rect::new(0, 0, 100, 100), "bounds");
    img
}

fn pixel_at(img: &Rgba, x: i64, y: i64) -> Px {
    match img.pixel(x, y) {
        Some(p) => [p[0], p[1], p[2], p[3]],
        None => [0; 4],
    }
}

/// `assertRect`: exactly the device pixels with x in [x0,x1) and y in [y0,y1) are `col`, and
/// every other pixel is white. The fill rule is pixel-centre inclusion against half-open device
/// intervals, so an axis-aligned rectangle's coverage is exactly predictable.
fn assert_rect(img: &Rgba, x0: i64, x1: i64, y0: i64, y1: i64, col: Px) {
    assert_regions(img, &[(x0, x1, y0, y1, col)]);
}

/// `assertRegions`: every pixel inside a region is that region's colour and every pixel outside
/// all of them is white. Regions must not overlap.
fn assert_regions(img: &Rgba, regions: &[(i64, i64, i64, i64, Px)]) {
    let mut bad = Vec::new();
    for y in 0..100 {
        for x in 0..100 {
            let want = regions
                .iter()
                .find(|r| (r.0..r.1).contains(&x) && (r.2..r.3).contains(&y))
                .map_or(WHITE, |r| r.4);
            let got = pixel_at(img, x, y);
            if got != want {
                bad.push((x, y, got, want));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} wrong pixels, first: {:?}",
        bad.len(),
        bad.first()
    );
}

fn limit_name(r: &Result<Rgba>) -> Option<&'static str> {
    match r {
        Err(Error::Limit { name, .. }) => Some(*name),
        _ => None,
    }
}

#[test]
fn fill_rect_exact_pixels() {
    // User rect (10,20)-(40,60) maps to device x [10,40), rows [40,80) after the y-flip.
    let img = render100("1 0 0 rg 10 20 30 40 re f");
    assert_rect(&img, 10, 40, 40, 80, RED);
}

// Two nested rectangles with the same ring direction, painted once with f and once with f*:
// nonzero fills the inner rectangle (winding 2), even-odd leaves it a hole (parity 2).
#[test]
fn winding_rules() {
    let src = "0 0 1 rg 20 20 60 60 re 35 35 30 30 re ";
    let nonzero = render100(&format!("{src}f"));
    let evenodd = render100(&format!("{src}f*"));
    assert_eq!(pixel_at(&nonzero, 50, 50), BLUE, "nonzero: inner pixel");
    assert_eq!(pixel_at(&evenodd, 50, 50), WHITE, "even-odd: inner pixel");
    assert_eq!(pixel_at(&nonzero, 25, 50), BLUE, "nonzero: ring pixel");
    assert_eq!(pixel_at(&evenodd, 25, 50), BLUE, "even-odd: ring pixel");
    assert_eq!(pixel_at(&evenodd, 10, 50), WHITE, "even-odd: outside pixel");
}

// A 4pt horizontal line at user y=50 from x=10 to x=90: device band rows [48,52), butt caps end
// the ink exactly at x=10 and x=90.
#[test]
fn stroke_line_exact_pixels() {
    let img = render100("0 g 4 w 10 50 m 90 50 l S");
    assert_rect(&img, 10, 90, 48, 52, BLACK);
}

// An L of two 10pt-wide strokes: the join must fill the notch outside the corner point.
#[test]
fn stroke_join_fills_the_corner() {
    let img = render100("0 g 10 w 20 20 m 20 80 l 80 80 l S");
    assert_eq!(pixel_at(&img, 17, 100 - 83), BLACK, "join notch pixel");
    assert_eq!(pixel_at(&img, 14, 100 - 86), WHITE, "beyond-join pixel");
    assert_eq!(pixel_at(&img, 20, 100 - 50), BLACK, "vertical arm");
    assert_eq!(pixel_at(&img, 50, 100 - 80), BLACK, "horizontal arm");
}

// ISO 32000-1 8.4.3.2 makes width 0 the thinnest renderable line, not nothing.
#[test]
fn zero_width_stroke_is_a_hairline() {
    let img = render100("0 g 0 w 10 50.5 m 90 50.5 l S");
    assert_rect(&img, 10, 90, 49, 50, BLACK);
}

#[test]
fn ctm_scales_and_places() {
    let img = render100("q 2 0 0 2 0 0 cm 1 0 0 rg 5 5 10 10 re f Q");
    // User rect after CTM: (10,10)-(30,30); device rows [70,90).
    assert_rect(&img, 10, 30, 70, 90, RED);
}

// The region between a chord and a cubic that dips from y=50 to y=20 at its midpoint.
#[test]
fn bezier_flattening() {
    let img = render100("0 g 20 50 m 80 50 l 80 10 20 10 20 50 c h f");
    for (ux, uy, want) in [
        (50, 30, BLACK), // between curve (y=20 at x=50) and chord (y=50)
        (50, 45, BLACK),
        (50, 15, WHITE), // below the curve
        (15, 30, WHITE), // left of the whole shape
        (50, 55, WHITE), // above the chord
    ] {
        assert_eq!(pixel_at(&img, ux, 100 - uy), want, "user ({ux},{uy})");
    }
}

#[test]
fn device_gray() {
    let img = render100("0.5 g 10 10 20 20 re f");
    assert_eq!(pixel_at(&img, 15, 100 - 15), [128, 128, 128, 255]);
}

#[test]
fn color_space_operators() {
    let img = render100("/DeviceRGB cs 0 0 1 sc 10 10 20 20 re f");
    assert_eq!(pixel_at(&img, 15, 100 - 15), BLUE);
}

#[test]
fn clip_restricts_fill() {
    let img = render100("20 20 40 40 re W n 0 g 0 0 100 100 re f");
    assert_rect(&img, 20, 60, 40, 80, BLACK);
}

#[test]
fn clip_restored_by_q() {
    let img = render100("q 20 20 40 40 re W n Q 1 0 0 rg 10 20 30 40 re f");
    assert_rect(&img, 10, 40, 40, 80, RED);
}

// Page boxes and scales come out of the file, so absurd raster sizes are refused.
#[test]
fn raster_dimensions_clamped() {
    let cancel = AtomicBool::new(false);
    let lim = Limits::default();
    for (name, bbox, scale) in [
        (
            "huge box",
            BBox {
                urx: 1e9,
                ury: 1e9,
                ..BBox::default()
            },
            1.0,
        ),
        ("huge scale", BOX100, 1e7),
        ("zero scale", BOX100, 0.0),
        ("negative scale", BOX100, -1.0),
        ("empty box", BBox::default(), 1.0),
    ] {
        let p = PageParams {
            bbox,
            rotate: 0,
            scale,
        };
        assert!(
            page(b"", &p, None, None, &lim, &cancel).is_err(),
            "{name}: page returned Ok; want a refusal"
        );
    }
}

// A curve storm cannot allocate unbounded flattened points.
#[test]
fn path_point_budget() {
    let src = format!(
        "0 g 10 50 m {}f",
        "80 10 20 10 20 50 c 80 10 20 10 20 50 c ".repeat(8)
    );
    let lim = Limits {
        max_path_points: 64,
        ..Limits::default()
    };
    assert!(
        run_limits(&src, &lim).is_err(),
        "page accepted a path beyond the point budget"
    );
}

// Scanline work is charged as it accrues, so a hostile edge pile is cut off.
#[test]
fn fill_work_budget() {
    let mut src = String::from("0 g 0 0 m ");
    for i in 0..64 {
        src.push_str(&format!("{i} 100 l {} 0 l ", i + 1));
    }
    src.push('f');
    let lim = Limits {
        max_fill_work: 16,
        ..Limits::default()
    };
    assert!(
        run_limits(&src, &lim).is_err(),
        "page accepted fill work beyond the budget"
    );
}

// The operator loop is interruptible, like walk's.
#[test]
fn context_cancellation() {
    let cancel = AtomicBool::new(false);
    cancel.store(true, Ordering::Relaxed);
    let src = "10 20 30 40 re f ".repeat(100);
    let r = page(
        src.as_bytes(),
        &params(BOX100),
        None,
        None,
        &Limits::default(),
        &cancel,
    );
    assert_eq!(r.err(), Some(Error::Cancelled));
}

// G/RG/CS/SCN drive the stroke colour, not the fill colour.
#[test]
fn stroke_color_operators() {
    let img = render100("1 0 0 RG 4 w 10 50 m 90 50 l S");
    assert_rect(&img, 10, 90, 48, 52, RED);
    let img = render100("/DeviceRGB CS 0 0 1 SCN 4 w 10 50 m 90 50 l S");
    assert_rect(&img, 10, 90, 48, 52, BLUE);
    let img = render100("0.5 G 4 w 10 50 m 90 50 l S");
    assert_rect(&img, 10, 90, 48, 52, [128, 128, 128, 255]);
}

// B both fills (with the fill colour) and strokes (with the stroke colour).
#[test]
fn fill_stroke_operator_b() {
    let img = render100("1 0 0 rg 0 0 1 RG 8 w 20 20 60 60 re B");
    for (ux, uy, want, what) in [
        (50, 50, RED, "interior fill"),
        (20, 50, BLUE, "stroke centred on the left edge"),
        (17, 50, BLUE, "stroke spread outside the edge"),
        (10, 50, WHITE, "beyond the stroke"),
    ] {
        assert_eq!(pixel_at(&img, ux, 100 - uy), want, "{what} at ({ux},{uy})");
    }
}

// b* fills even-odd (the inner rectangle is a hole) and strokes the path.
#[test]
fn even_odd_fill_stroke_operator() {
    let img = render100("1 0 0 rg 4 w 20 20 60 60 re 35 35 30 30 re b*");
    for (ux, uy, want, what) in [
        (50, 50, WHITE, "even-odd hole"),
        (27, 50, RED, "ring fill"),
        (20, 50, BLACK, "stroked outer edge"),
        (35, 50, BLACK, "stroked inner edge"),
    ] {
        assert_eq!(pixel_at(&img, ux, 100 - uy), want, "{what} at ({ux},{uy})");
    }
}

// s closes the subpath before stroking, so the closing diagonal of the L is inked; the same path
// under S leaves it bare.
#[test]
fn close_stroke_operator() {
    let img = render100("0 g 4 w 20 20 m 20 80 l 80 80 l s");
    assert_eq!(pixel_at(&img, 50, 50), BLACK, "closing segment midpoint");
    let img = render100("0 g 4 w 20 20 m 20 80 l 80 80 l S");
    assert_eq!(pixel_at(&img, 50, 50), WHITE, "S must not close");
}

// The operand-to-control-point mapping of v and y (ISO 32000-1 table 59), by pixel identity with
// the equivalent c operator.
#[test]
fn v_y_curve_operators() {
    let y_img = render100("0 g 10 10 m 10 90 90 10 y h f");
    let y_ref = render100("0 g 10 10 m 10 90 90 10 90 10 c h f");
    let v_img = render100("0 g 10 10 m 10 90 90 10 v h f");
    let v_ref = render100("0 g 10 10 m 10 10 10 90 90 10 c h f");
    assert_eq!(y_img.pix, y_ref.pix, "y does not match its c equivalent");
    assert_eq!(v_img.pix, v_ref.pix, "v does not match its c equivalent");
    assert_ne!(y_img.pix, v_img.pix, "the two mappings have collapsed");
    assert_eq!(
        pixel_at(&y_img, 30, 100 - 30),
        BLACK,
        "y curve fill interior"
    );
}

// Painted pixels are charged against max_fill_work, not just active edges. A full-canvas
// rectangle charges 3 edge units per row (300 total, under this budget) but 100 pixels per row.
#[test]
fn fill_pixel_work_budget() {
    let lim = Limits {
        max_fill_work: 350,
        ..Limits::default()
    };
    assert!(run_limits("0 g 0 0 100 100 re f", &lim).is_err());
}

// Stroke rasterisation goes through the same budgeted scanline filler as fills.
#[test]
fn stroke_work_budget() {
    let lim = Limits {
        max_fill_work: 16,
        ..Limits::default()
    };
    assert!(run_limits("0 g 4 w 10 10 m 90 90 l S", &lim).is_err());
}

// Plain garbage operators are simply ignored and the canvas comes back with no error.
#[test]
fn malformed_stream_still_returns_canvas() {
    let img = render100("nonsense ops here 1 0 0 rg 10 20 30 40 re f");
    assert_rect(&img, 10, 40, 40, 80, RED);
}

// A rect 10^31 points wide puts the right-hand crossing past 2^63, where amd64's conversion
// wraps to the minimum integer: the fill must still paint the whole canvas.
#[test]
fn fill_huge_span_still_paints() {
    let huge = format!("1{}", "0".repeat(31));
    let img = render100(&format!("0 g 0 0 {huge} 100 re f"));
    let inked = img.pix.chunks_exact(4).filter(|p| p[0] < 128).count();
    assert_eq!(inked, 100 * 100, "the whole canvas must be black");
}

#[test]
fn page_refuses_deep_gstate_stack() {
    let deep = 1024;
    let lim = Limits::default();
    let r = run_limits(&"q ".repeat(deep + 1), &lim);
    assert_eq!(limit_name(&r), Some("q-depth"));
    assert!(run_limits(&"q ".repeat(deep), &lim).is_ok());
}

// An unknown colour space (every ICCBased resource name in practice) recovers its model from the
// operand count: 3 means RGB, 4 means CMYK.
#[test]
fn set_comps_operand_count_fallback() {
    for (name, src, want) in [
        (
            "3 operands is RGB",
            "/Cs6 cs 0 0 1 scn 0 0 100 100 re f",
            BLUE,
        ),
        (
            "4 operands is CMYK",
            "/Cs6 cs 0 1 1 0 scn 0 0 100 100 re f",
            RED,
        ),
        // C=M=Y=0, K=1 pins the (1-k) term specifically.
        (
            "4 operands respects K",
            "/Cs6 cs 0 0 0 1 scn 0 0 100 100 re f",
            BLACK,
        ),
    ] {
        let img = render100(src);
        assert_eq!(pixel_at(&img, 50, 50), want, "{name}");
    }
}

// A single scn operand under an unresolved space must not be guessed as gray: it is a palette
// index or a subtractive tint.
#[test]
fn set_comps_excludes_single_operand() {
    let img = render100("/Cs6 cs 1 scn 0 0 100 100 re f");
    assert_eq!(pixel_at(&img, 50, 50), BLACK);
}

#[test]
fn set_comps_excludes_indexed() {
    let img = render100("/Indexed cs 1 scn 0 0 100 100 re f");
    assert_eq!(pixel_at(&img, 50, 50), BLACK);
}

// ---- imagedraw_test.go ------------------------------------------------------------------------

fn set_px(pix: &mut [u8], stride: usize, x: usize, y: usize, c: Px) {
    let o = y * stride + x * 4;
    pix[o..o + 4].copy_from_slice(&c);
}

/// A 2x2 source image with four distinct opaque colours. Source row 0 (red, blue) is the top of
/// the image, which the unit-square convention puts at v near 1.
fn two_by_two() -> Image {
    let mut im = Nrgba::new(Rect::new(0, 0, 2, 2), &Limits::default()).unwrap();
    let stride = im.stride;
    set_px(&mut im.pix, stride, 0, 0, RED);
    set_px(&mut im.pix, stride, 1, 0, BLUE);
    set_px(&mut im.pix, stride, 0, 1, BLACK);
    set_px(&mut im.pix, stride, 1, 1, GREEN);
    Image::Nrgba(im)
}

fn plain(data: Image) -> RenderImage {
    RenderImage {
        data,
        stencil: false,
        invert: false,
    }
}

fn render_images(src: &str, imgs: &HashMap<&str, RenderImage>) -> Rgba {
    let cancel = AtomicBool::new(false);
    let lim = Limits::default();
    let mut f =
        |name: &[u8]| -> Option<RenderImage> { imgs.get(std::str::from_utf8(name).ok()?).cloned() };
    page(
        src.as_bytes(),
        &params(BOX100),
        Some(&mut f),
        None,
        &lim,
        &cancel,
    )
    .unwrap()
}

fn one_image(data: Image) -> HashMap<&'static str, RenderImage> {
    HashMap::from([("Im0", plain(data))])
}

// The unit square under `40 0 0 40 10 20 cm` is user (10,20)-(50,60), device x [10,50) rows
// [40,80). Source row 0 is the top of that square, so red/blue land on the upper rows.
#[test]
fn image_draw_exact_pixels() {
    let img = render_images("q 40 0 0 40 10 20 cm /Im0 Do Q", &one_image(two_by_two()));
    assert_regions(
        &img,
        &[
            (10, 30, 40, 60, RED),
            (30, 50, 40, 60, BLUE),
            (10, 30, 60, 80, BLACK),
            (30, 50, 60, 80, GREEN),
        ],
    );
}

// `0 40 -40 0 60 20 cm` maps (u,v) to user (60-40v, 20+40u), a 90-degree rotation.
#[test]
fn image_rotation_90() {
    let img = render_images("q 0 40 -40 0 60 20 cm /Im0 Do Q", &one_image(two_by_two()));
    assert_regions(
        &img,
        &[
            (20, 40, 60, 80, RED),
            (20, 40, 40, 60, BLUE),
            (40, 60, 60, 80, BLACK),
            (40, 60, 40, 60, GREEN),
        ],
    );
}

// A negative horizontal scale mirrors the image.
#[test]
fn image_flip() {
    let img = render_images("q -40 0 0 40 50 20 cm /Im0 Do Q", &one_image(two_by_two()));
    assert_regions(
        &img,
        &[
            (30, 50, 40, 60, RED),
            (10, 30, 40, 60, BLUE),
            (30, 50, 60, 80, BLACK),
            (10, 30, 60, 80, GREEN),
        ],
    );
}

// `40 0 40 40 10 20 cm` shears x by v: probes sit at quadrant centres of the sheared
// parallelogram.
#[test]
fn image_shear() {
    let img = render_images("q 40 0 40 40 10 20 cm /Im0 Do Q", &one_image(two_by_two()));
    for (ux, uy, want, what) in [
        (50, 50, RED, "u=.25 v=.75"),
        (70, 50, BLUE, "u=.75 v=.75"),
        (30, 30, BLACK, "u=.25 v=.25"),
        (50, 30, GREEN, "u=.75 v=.25"),
        (15, 55, WHITE, "left of the sheared top edge"),
    ] {
        assert_eq!(pixel_at(&img, ux, 100 - uy), want, "{what} at ({ux},{uy})");
    }
}

// /ImageMask true paints the current fill colour where the decoded sample is 0, and invert (a
// /Decode of [1 0]) inverts which samples paint.
#[test]
fn image_mask_stencil() {
    let mut mask = Gray::new(Rect::new(0, 0, 2, 2), &Limits::default()).unwrap();
    mask.pix.copy_from_slice(&[0, 255, 255, 0]);
    let src = "1 0 0 rg q 40 0 0 40 10 20 cm /M Do Q";

    let stencil = |invert| {
        HashMap::from([(
            "M",
            RenderImage {
                data: Image::Gray(mask.clone()),
                stencil: true,
                invert,
            },
        )])
    };
    let img = render_images(src, &stencil(false));
    assert_regions(&img, &[(10, 30, 40, 60, RED), (30, 50, 60, 80, RED)]);

    let inverted = render_images(src, &stencil(true));
    assert_regions(&inverted, &[(30, 50, 40, 60, RED), (10, 30, 60, 80, RED)]);
}

// A name the resolver declines skips the draw and keeps rendering.
#[test]
fn unsupported_image_skips_draw() {
    let img = render_images(
        "q 40 0 0 40 10 20 cm /Im0 Do Q 1 0 0 rg 10 20 30 40 re f",
        &HashMap::new(),
    );
    assert_rect(&img, 10, 40, 40, 80, RED);
}

// A Do with no resolver at all is likewise a clean no-op.
#[test]
fn image_with_nil_resolver() {
    let img = render100("q 40 0 0 40 10 20 cm /Im0 Do Q 1 0 0 rg 10 20 30 40 re f");
    assert_rect(&img, 10, 40, 40, 80, RED);
}

// A singular CTM maps the unit square to zero area: no division by zero, no error.
#[test]
fn image_degenerate_ctm_skips() {
    let img = render_images("q 0 0 0 0 50 50 cm /Im0 Do Q", &one_image(two_by_two()));
    assert_regions(&img, &[]);
}

// The rectangular clip applies to image draws: clip user (20,20)-(40,40) is device x [20,40)
// rows [60,80), the bottom half of the image.
#[test]
fn clip_restricts_image() {
    let img = render_images(
        "20 20 20 20 re W n q 40 0 0 40 10 20 cm /Im0 Do Q",
        &one_image(two_by_two()),
    );
    assert_regions(&img, &[(20, 30, 60, 80, BLACK), (30, 40, 60, 80, GREEN)]);
}

// A half-transparent red source pixel over the white canvas comes out pink by source-over.
#[test]
fn image_alpha_composites() {
    let mut one = Nrgba::new(Rect::new(0, 0, 1, 1), &Limits::default()).unwrap();
    one.pix.copy_from_slice(&[255, 0, 0, 128]);
    let img = render_images(
        "q 40 0 0 40 10 20 cm /Im0 Do Q",
        &one_image(Image::Nrgba(one)),
    );
    assert_eq!(pixel_at(&img, 20, 50), [255, 127, 127, 255]);
    assert_eq!(pixel_at(&img, 5, 50), WHITE);
}

// A finite but huge translation operand places the image far off-canvas; the device bounds are
// intersected in float space before any int conversion, so the draw is skipped, not hung.
#[test]
fn image_huge_translation_skips() {
    let huge = format!("1{}", "0".repeat(300));
    let img = render_images(
        &format!("q 1 0 0 1 {huge} 0 cm /Im0 Do Q"),
        &one_image(two_by_two()),
    );
    assert_regions(&img, &[]);
}

// The premultiplied RGBA source places the same quadrants as the NRGBA one.
#[test]
fn image_rgba_fast_path() {
    let mut im = Rgba::new(Rect::new(0, 0, 2, 2), &Limits::default()).unwrap();
    let stride = im.stride;
    set_px(&mut im.pix, stride, 0, 0, RED);
    set_px(&mut im.pix, stride, 1, 0, BLUE);
    set_px(&mut im.pix, stride, 0, 1, BLACK);
    set_px(&mut im.pix, stride, 1, 1, GREEN);
    let img = render_images(
        "q 40 0 0 40 10 20 cm /Im0 Do Q",
        &one_image(Image::Rgba(im)),
    );
    assert_regions(
        &img,
        &[
            (10, 30, 40, 60, RED),
            (30, 50, 40, 60, BLUE),
            (10, 30, 60, 80, BLACK),
            (30, 50, 60, 80, GREEN),
        ],
    );
}

// Destination writes go through the same max_fill_work budget as fills.
#[test]
fn image_write_budget() {
    let cancel = AtomicBool::new(false);
    let lim = Limits {
        max_fill_work: 16,
        ..Limits::default()
    };
    let mut f = |_: &[u8]| Some(plain(two_by_two()));
    let r = page(
        b"q 100 0 0 100 0 0 cm /Im0 Do Q",
        &params(BOX100),
        Some(&mut f),
        None,
        &lim,
        &cancel,
    );
    assert_eq!(limit_name(&r), Some("fill-work"));
}

// ---- caps_test.go -----------------------------------------------------------------------------

// The render caps' defaults, restated so a change to them fails here.
#[test]
fn render_cap_defaults() {
    let l = Limits::default();
    assert_eq!(l.raster_side(), 20000);
    assert_eq!(l.raster_pixels(), 1 << 26);
    assert_eq!(l.gstate_depth(), 1024);
    assert_eq!(l.render_font_bytes(), 16 << 20);
    assert_eq!(l.path_points(), 1 << 21);
    assert_eq!(l.fill_work(), 1 << 28);
    assert_eq!(l.operands(), 8192);
}

#[test]
fn render_low_caps_refuse_by_name() {
    let d = Limits::default();
    for (name, src, lim) in [
        (
            "raster-side",
            "",
            Limits {
                max_raster_side: 50,
                ..d
            },
        ),
        (
            "raster-pixels",
            "",
            Limits {
                max_raster_pixels: 5000,
                ..d
            },
        ),
        (
            "q-depth",
            "q q q q",
            Limits {
                max_gstate_depth: 3,
                ..d
            },
        ),
        (
            "path-points",
            "0 0 m 50 50 l 20 80 l f",
            Limits {
                max_path_points: 2,
                ..d
            },
        ),
        (
            "fill-work",
            "0 g 0 0 100 100 re f",
            Limits {
                max_fill_work: 10,
                ..d
            },
        ),
    ] {
        let r = run_limits(src, &lim);
        assert_eq!(limit_name(&r), Some(name), "{name}: err = {:?}", r.err());
    }
}

// A float past int64 converts to the minimum integer on amd64 and must still be refused.
#[test]
fn raster_size_huge_scale_refused() {
    let err = raster_size(&BOX100, i64::MAX as f64, &Limits::default()).unwrap_err();
    match err {
        Error::Limit { name, got, max } => {
            assert_eq!(name, "raster-side");
            assert!(got > max, "got {got} <= max {max}");
        }
        e => panic!("err = {e:?}; want raster-side"),
    }
}

// A caller-raised cap must not turn into a huge allocation, and the cap compares the canvas
// actually allocated: ceil(w)*ceil(h).
#[test]
fn raster_size_raised_caps_and_canvas() {
    let big = Limits {
        max_raster_side: i64::MAX,
        max_raster_pixels: i64::MAX,
        ..Limits::default()
    };
    let square = |urx, ury| BBox {
        urx,
        ury,
        ..BBox::default()
    };
    for b in [
        square(2e9, 2e9),
        square(9.2e18, 1.0),
        square((1u64 << 22) as f64, (1u64 << 22) as f64),
        square((1u64 << 17) as f64, (1u64 << 17) as f64),
    ] {
        match raster_size(&b, 1.0, &big) {
            Err(Error::Limit { name, got, max }) => {
                assert_eq!(name, "raster-pixels", "box {b:?}");
                assert!(got > max, "box {b:?}: got {got} <= max {max}");
            }
            r => panic!("box {b:?}: {r:?}; want raster-pixels"),
        }
    }
    // 8191.5 x 8192.4 is under 1<<26 by area but allocates 8192 x 8193 px.
    let r = raster_size(&square(8191.5, 8192.4), 1.0, &Limits::default());
    assert!(
        matches!(
            r,
            Err(Error::Limit {
                name: "raster-pixels",
                ..
            })
        ),
        "{r:?}"
    );
}
