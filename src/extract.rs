//! Wave 3: port of byblos extract.go, plus the PDF-facing half of jbig2.go
//! (`decodeJBIG2Placement`, `grayLevels`, `graySample`, `grayImage`), which only this file calls.
//!
//! [`extract_page_raster`] returns the one visible raster of a page, or refuses the page with an
//! [`Error::Refused`] that carries the reason. Whether a page is one raster is decided from what
//! the content stream and the image dictionaries prove (painting order, geometry, opacity) and
//! never by rendering: that is what makes the project tractable (design spec section 2).
//!
//! The refusals and the stats they feed:
//!
//! * a page that is not one visible image: [`Error::NotSingleRaster`] under the refusal, and
//!   `count_divert(reason)`;
//! * a raster in a codec byblos cannot decode: [`Error::UnsupportedImageCodec`] under the
//!   refusal, also a divert;
//! * a page or file that cannot be read, or a limit or budget breach: the error as it is, and
//!   `count_failure()`.
//!
//! # Differences from Go
//!
//! * Go reads the page twice (`d.Page`, then `inspectPage`, which calls `d.Page` again); the
//!   page is read once here and its scope is walked, which is the same page.
//! * `fmt.Errorf("%w: ...", err)` is [`Error::Wrapped`], so the text and the `is_*` chain of
//!   Go's errors are kept.
//! * `reserveDCT` charged `Need()` to the process memory pool. Here the call's own ledger is
//!   what jpeg-rs is allowed to use (`max_alloc` less what the parse holds), and only the
//!   refusals that Go reports as a `LimitError` or a plain error are kept ([`reserve_dct`]).
//! * Go's `image.Decode` fallback is [`image::decode`] (pdf-rs `gojpeg`, the PNG reader and the
//!   TIFF reader); WebP is not decoded (docs/go-differences.md).

use crate::bitmap::Bitmap;
use crate::content::{self, BBox, Matrix, Paint, Placement, Quad, Scan, Stream};
use crate::dct::{self, Frame};
use crate::error::{Error, Result};
use crate::go::math;
use crate::image::{self, Gray, Image, Rect};
use crate::jbig2;
use crate::limits::{self, Limits, check_cancel};
use crate::notimplemented::NotImplemented;
use crate::pdfdoc::doc::NEVER;
use crate::pdfdoc::{self, Doc, DocEnv, ImageInfo};
use crate::provenance::{COVER_TOLERANCE_PT, PageGeometry, PageProvenance};
use crate::stats::{count_attempt, count_divert, count_extracted, count_failure};
use crate::upgrade::capability_issue;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
mod tests;

/// `MaxSkewDeg`: how far a placement's axes may lie from the page's before the image is
/// treated as rotated or sheared.
///
/// It is an angle and not a matrix entry (byb-b1.2): a tolerance on the off-diagonal terms,
/// which are in points, is an exact-zero test at page scale and rejected all 147 sub-degree
/// scanner deskews measured. Two degrees clears the widest of them (1.09). It is also the
/// straighten envelope: a `StraightenSpec::deg` wider than this leaves a placement byblos then
/// diverts as rotated, so an editor offering a straighten slider has to clamp to it.
pub const MAX_SKEW_DEG: f64 = 2.0;

/// `mrcPatchAreaFrac`: how much of the page a non-bitonal placement must cover before it counts
/// as an MRC patch rather than a stamp or a logo. Measured over 169,376 sample pages: the
/// smallest patch that fires `mrc-layers` anywhere is 0.020194 of the page, the largest inset
/// that must not is 0.017130 (`classify_mrc_patch_floor_bracket` pins both ends).
const MRC_PATCH_AREA_FRAC: f64 = 0.02;

/// `mrcBaseAreaFrac`: how much of the page a bitonal placement must cover to be the base layer
/// of an MRC page. An area fraction and not `covers`, because Google Books places the base at
/// its own resolution, about ten points short on every edge (94.7% of the page).
const MRC_BASE_AREA_FRAC: f64 = 0.90;

/// `paintTolerancePt`: how far a STROKE may stray outside the raster and still count as hidden
/// by it. A thousandth of a point forgives arithmetic and nothing else; it is deliberately not
/// [`COVER_TOLERANCE_PT`], which would cancel the spread a 2-point pen adds to the stroke's box.
const PAINT_TOLERANCE_PT: f64 = 1e-3;

/// `paintFillTolerancePt`: the same allowance for a FILL. A fill's box is its path and nothing
/// inflates it, so there is no spread to cancel. One point is [`COVER_TOLERANCE_PT`]'s value
/// for its reason (two producers disagree about where a page's edge is); poppler picked it:
/// of 191 pages it releases none carries a mark outside the raster, and the first page that
/// needs more (1.6831 pt) paints a visibly dark border.
const PAINT_FILL_TOLERANCE_PT: f64 = 1.0;

/// `PageRaster`: a page's raster and where it sits on the page.
///
/// The raster is not always the whole page: 132 measured pages place it at its own resolution
/// on a nominal Letter box (2384x3321 pixels at 302 DPI is 568.37 x 791.76 points on a 612x792
/// MediaBox) and those extract. `bounds` and `page` are in PDF default user space (points,
/// origin lower-left, y up), the same convention as `PageInfo.bounds`; [`Rect`] is only a
/// convenient integer rectangle, not screen coordinates.
///
/// `bounds` is where the raster is VISIBLE: the placement's own extent intersected with any
/// `W`/`W*` clip path or form `/BBox` in effect (byb-b1.12). `image` is always the full,
/// uncropped raster as stored. `bounds` is never empty beside a returned image (byb-62t): a
/// placement narrower than a point on an axis is the smallest integer rectangle that contains
/// it, which overstates the extent by up to a point per edge, so do not derive a DPI from it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRaster {
    pub image: Image,
    /// Where the raster lands.
    pub bounds: Rect,
    /// The page's CropBox.
    pub page: Rect,
    /// The placement's true quadrilateral: the unit square mapped through its CTM, in ring
    /// order (0,0) (1,0) (1,1) (0,1), in the same unrounded PDF-space points `bounds`'s corners
    /// were rounded from. `None` when the placement carries no rotation or shear, which makes it
    /// the same shape as `bounds`. [`PageRaster::covers_page`] uses it to tell a
    /// rotation-inflated bounding box from the page it only appears to cover (byb-2mt).
    pub raster_quad: Option<[f64; 8]>,
    /// The PDF object number of the image XObject this raster came from: what `replace_images`
    /// keys its substitution map on. `classify` picks the LAST placement in paint order, so on a
    /// stacked page this is the visible top layer, not the hidden under-layer. Negative for a
    /// direct object, which has no cross-reference entry to write back to. It names the
    /// XObject, not the placement: one image painted on several pages is substituted once.
    pub obj_nr: i64,
    /// How many annotations on this page paint and are not in `image`. Annotations live beside
    /// the content stream, so no raster contains them. Non-zero means the caller holds an image
    /// that is missing ink a reader would see; what to do is the caller's policy, which is why
    /// this is reported rather than diverted (6 such pages in 18,610 extracted).
    pub dropped_annots: i64,
    /// The source XObject DECLARED one bit per component, or was an image mask. A declaration,
    /// never a measurement: Ghostscript keys its mono downsampling off the declared depth, and a
    /// pixel sniff measured 13 dB under byblos's own oracle (byb-plj). `image` is a widened
    /// 8-bit image either way. A miss leaves it false, the safe direction.
    pub bitonal: bool,
}

impl PageRaster {
    /// `CoversPage`: whether the raster fills the page box. When it is false the caller holds
    /// the scanned area, not the MediaBox, and the difference between `bounds` and `page` is the
    /// part the scan does not cover. Byblos does not pad the raster out to the page: pixels that
    /// were never scanned are the same kind of lie as resampling a deskewed raster.
    ///
    /// An empty `page` is covered by nothing. The `raster_quad` conjunct is checked against the
    /// rounded integer `page`, so on a fractional CropBox the effective slack here can reach
    /// 1.5 pt rather than [`COVER_TOLERANCE_PT`]'s 1.0. That only ever makes this MORE
    /// conservative than `PageGeometry::covers_page`, never less.
    pub fn covers_page(&self) -> bool {
        let (p, b) = (&self.page, &self.bounds);
        if p.is_empty() {
            return false;
        }
        // Go's `Page.In(Bounds)`.
        let inside = b.x0 <= p.x0 && p.x1 <= b.x1 && b.y0 <= p.y0 && p.y1 <= b.y1;
        if !inside {
            return false;
        }
        let Some(q) = &self.raster_quad else {
            return true;
        };
        let page_box = BBox {
            llx: p.x0 as f64,
            lly: p.y0 as f64,
            urx: p.x1 as f64,
            ury: p.y1 as f64,
        };
        Quad(*q).contains_box(&page_box, COVER_TOLERANCE_PT)
    }
}

/// Go's `max` builtin on floats: NaN wins.
fn go_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a > b { a } else { b }
}

/// `skewDegrees`: how far the placement's axes lie from the page's axes, in degrees, taking the
/// larger of the two: zero for an axis-aligned placement, the rotation angle for a rotation, and
/// the worse of the two for a shear. Each axis is measured against its own scale term, so the
/// answer does not depend on the size of the raster. Signs are dropped, which keeps a mirror at
/// zero: `classify` catches that by the sign of the scale terms.
pub fn skew_degrees(m: &Matrix) -> f64 {
    let x = math::atan2(m.0[1].abs(), m.0[0].abs());
    let y = math::atan2(m.0[2].abs(), m.0[3].abs());
    go_max(x, y) * 180.0 / std::f64::consts::PI
}

/// Go's `int(f)` of an amd64 `CVTTSD2SQ`: NaN and anything out of range give the "integer
/// indefinite" value.
fn go_int(f: f64) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        i64::MIN
    } else {
        f as i64
    }
}

/// `round`: `int(math.Round(v))`, half away from zero.
fn round(v: f64) -> i64 {
    go_int(v.round())
}

/// `roundExtent`: projects one axis of a box, widening outward rather than letting a positive
/// extent collapse to nothing (byb-62t). The far edge is `round(lo) + round(hi - lo)`, not
/// `round(hi)`, because rounding both edges independently can move the reported extent a whole
/// point away from the value poppler's page-size arithmetic agrees with (byb-wtp); a positive
/// extent is never reported narrower than 1.
fn round_extent(lo: f64, hi: f64) -> (i64, i64) {
    let (l, h) = (round(lo), round(hi));
    if hi <= lo {
        (l, h)
    } else if l == h {
        (go_int(lo.floor()), go_int(hi.ceil()))
    } else {
        (l, l.wrapping_add(round(hi - lo).max(1)))
    }
}

/// `boxRect`: projects a float box onto the integer rectangle a caller reads as `bounds`. Each
/// edge rounds to nearest, and an axis whose two edges would round together widens outward
/// instead, so an empty rectangle never sits beside raster bytes. A box with no extent on an axis
/// still projects to an empty rectangle: whether anything is visible is a float question that
/// `classify` answers with `marks`.
pub fn box_rect(b: &BBox) -> Rect {
    let (llx, urx) = round_extent(b.llx, b.urx);
    let (lly, ury) = round_extent(b.lly, b.ury);
    Rect::new(llx, lly, urx, ury)
}

/// `rectOf`: projects a page box onto the integer rectangle `PageInfo.bounds` and
/// [`PageRaster::page`] report. It is [`box_rect`]'s rule applied to the page box (byb-67j):
/// poppler reports a sub-point /MediaBox verbatim and renders it to at least one inked pixel, so
/// collapsing it says "no page" about a page poppler draws. A box with no extent still projects
/// to an empty rectangle, which [`PageRaster::covers_page`]'s guard depends on.
pub fn rect_of(r: &pdfdoc::Rect) -> Rect {
    let (llx, urx) = round_extent(r.llx, r.urx);
    let (lly, ury) = round_extent(r.lly, r.ury);
    Rect::new(llx, lly, urx, ury)
}

/// `normalizedBox`: `[llx lly urx ury]` with each pair of corners in canonical (min, max) order.
/// ISO 32000-1 7.9.5 permits either diagonal order and requires a consumer to normalize, and a
/// page whose /CropBox names its corners UR-then-LL would otherwise round-trip into
/// `PageGeometry` inverted.
pub fn normalized_box(mut llx: f64, mut lly: f64, mut urx: f64, mut ury: f64) -> [f64; 4] {
    if llx > urx {
        std::mem::swap(&mut llx, &mut urx);
    }
    if lly > ury {
        std::mem::swap(&mut lly, &mut ury);
    }
    [llx, lly, urx, ury]
}

/// Where the image-dictionary facts of a placement come from: `pdfdoc::Doc::image_info` in
/// `extract_page`, a map in the tests. `None` is a dictionary that could not be read.
type InfoFn<'a> = &'a dyn Fn(i64) -> Option<ImageInfo>;

/// `classify`: the index of the placement that is the page's raster and `""`, or the reason the
/// page cannot be treated as one. The first matching reason is the one reported and it should
/// be the most informative (a born-digital page has both no image and text; `"no-image"` says
/// more). The returned strings are the keys of the divert counters: do not rename them casually.
///
/// Only text that deposits ink is a reason to divert: rendering mode 3 paints no glyphs and an
/// invisible OCR layer is what nearly every scan pipeline ships. Known simplification: z-order
/// is ignored, so text painted BEFORE an opaque page-covering image diverts too, which is the
/// safe direction.
///
/// A lone raster is not asked to cover the page (byb-b1.3): every arm before it has established
/// that no inked text, visible path, shading, inline image or unresolved XObject marks the
/// page, so nothing can put ink outside the placement and that raster IS the page at any
/// coverage. [`PageRaster::covers_page`] is what tells the caller. The claim is exactly "the
/// content stream": annotations and text-clipping modes sit outside it.
///
/// A stack still has to be covered: an under-layer reaching past the top one is exactly the ink
/// the argument says cannot exist. `covers` is deliberately AABB-only, because a rotated
/// placement's true quadrilateral falls several points short of an axis-aligned page at even a
/// fraction of a degree, which would divert every rotated stack (byb-2mt); `contains` is the
/// site that needs the true shape.
pub fn classify(page: &pdfdoc::Rect, s: &Scan, image_info: InfoFn<'_>) -> (usize, &'static str) {
    if s.images.is_empty() {
        return (0, "no-image");
    }
    if s.inked_text_ops > 0 {
        return (0, "has-text");
    }
    if s.inline_imgs > 0 {
        return (0, "inline-image");
    }
    // Not every painted path is content: a background wash laid down before the raster and then
    // covered by it is invisible (byb-b1.5: 126 scan-shaped pages). Every placement is a
    // candidate cover, not just the top one (byb-7aq).
    if !paints_hidden(&s.images, &s.paints, image_info) {
        return (0, "vector-paint");
    }
    if s.shading_ops > 0 {
        return (0, "shading");
    }
    if !s.unresolved.is_empty() {
        return (0, "unresolved-xobject");
    }
    let reason = mrc_layers(page, &s.images, image_info);
    if !reason.is_empty() {
        return (0, reason);
    }

    // Scan.images is in paint order, so the last placement is the one on top, and it is the only
    // candidate: an earlier raster is not what the page shows, and a raster painted after the
    // candidate would be lost by returning it.
    let top = s.images.len() - 1;
    let t = &s.images[top];
    // A clip can narrow the top placement's box to zero area; nothing is visible there, and
    // extracting would hand back the raster against a raster_box that claims an empty rectangle.
    // The test is on the FLOAT extent (byb-62t): a 0.4 pt sliver is a visible stripe poppler
    // inks, and diverting it loses a page poppler renders. The condition is "a clip actually
    // NARROWED this placement", the same test the ClipBox recording site uses, so a page-sized
    // `re W n` does not steal the reason of a zero-area box some other cause produced.
    if clip_narrowed(t) && !marks(&t.bbox) {
        if top > 0 {
            return (0, "multiple-images");
        }
        return (0, "clipped-away");
    }
    let reason = placement_reason(t);
    if !reason.is_empty() {
        // On a layered page the informative fact is that the layers did not reduce to one
        // raster, not which geometry test the top layer failed.
        if top > 0 {
            return (0, "multiple-images");
        }
        return (0, reason);
    }
    if top > 0 {
        if !covers(&t.bbox, page) {
            return (0, "multiple-images");
        }
        // 16,241 measured Internet Archive pages paint two page-covering images at the
        // identical CTM, the second hiding the first. That reduces to one raster only while the
        // top layer really is an opaque cover over everything below it.
        if !opaque_cover(t, image_info) {
            return (0, "transparent-overlay");
        }
        for under in &s.images[..top] {
            if !contains(&t.bbox, &under.bbox) {
                return (0, "multiple-images");
            }
            // Quad against quad, not quad against box: a rotated top can never contain a
            // rotated under-layer's BOX even at the identical matrix, while for identical
            // matrices the two quads are the same quad.
            if !axis_aligned(&t.ctm)
                && !t
                    .ctm
                    .unit_square_quad()
                    .contains_quad(&under.ctm.unit_square_quad(), COVER_TOLERANCE_PT)
            {
                return (0, "multiple-images");
            }
        }
    }
    (top, "")
}

/// `clipNarrowed`: whether a clip (`W`/`W* n`, or a form /BBox) actually cut this placement
/// below its own unclipped extent, as opposed to merely being in effect when it was painted.
/// One definition for `classify`'s clipped-away guard and the ClipBox recording site: if they
/// disagree, the divert reason and the stored geometry tell different stories.
///
/// It compares against `ctm.unit_square_box()` because an image XObject always occupies the
/// unit square in its own space (ISO 32000-1 8.9.5.2).
pub fn clip_narrowed(p: &Placement) -> bool {
    p.clip.is_some() && p.bbox != p.ctm.unit_square_box()
}

/// `axisAligned`: both off-diagonal terms are exactly zero. The single definition of "the true
/// quadrilateral can differ from the axis-aligned box", shared by every site that asks.
pub fn axis_aligned(m: &Matrix) -> bool {
    m.0[1] == 0.0 && m.0[2] == 0.0
}

/// `sameRotation`: whether `a` and `b` turn their x-axis the same way (same direction,
/// negligible angle between them). It compares directions, so it never unwraps `atan2`. Used
/// only by [`ink_hidden`]: a path's box is an axis-aligned bounding box of its true, possibly
/// rotated shape, so the box's own corners are real points of the path only when the path was
/// not rotated relative to the raster it is tested against.
fn same_rotation(a: &Matrix, b: &Matrix) -> bool {
    let (an, bn) = (math::hypot(a.0[0], a.0[1]), math::hypot(b.0[0], b.0[1]));
    if an == 0.0 || bn == 0.0 {
        return false;
    }
    let cross = a.0[0] * b.0[1] - a.0[1] * b.0[0];
    let dot = a.0[0] * b.0[0] + a.0[1] * b.0[1];
    dot > 0.0 && cross.abs() / (an * bn) < 1e-6
}

/// `placementReason`: why a placement cannot stand as the page's raster, or `""`. Everything it
/// asks is about orientation, nothing about size.
fn placement_reason(p: &Placement) -> &'static str {
    let m = &p.ctm.0;
    // A sub-degree rotation is scanner deskew: the raster is the raw skewed scan and the matrix
    // is recorded in provenance (byb-b1.2).
    if skew_degrees(&p.ctm) > MAX_SKEW_DEG {
        return "rotated-placement";
    }
    // A negative scale mirrors the raster without any skew, and the box cannot see it either:
    // `unit_square_box` takes min/max over the four corners. Requiring both scales strictly
    // positive also rules out a degenerate zero-scale placement.
    if m[0] <= 0.0 || m[3] <= 0.0 {
        return "flipped-placement";
    }
    ""
}

/// `opaqueCover`: whether `p` can be said to hide whatever is painted under it. Three of the
/// four ways it cannot are dictionary facts (/ImageMask, /SMask, /Mask) and the fourth is the
/// graphics state the walk recorded. An image whose dictionary could not be read is not opaque:
/// guessing the other way is the one error here that returns a wrong page instead of a divert.
fn opaque_cover(p: &Placement, image_info: InfoFn<'_>) -> bool {
    if !p.opaque {
        return false;
    }
    matches!(image_info(p.id), Some(i) if !i.image_mask && !i.smask && !i.mask)
}

/// `mrcLayers`: the two-tier MRC shape Google Books emits, a bitonal page-covering base plus a
/// smaller non-bitonal patch, or `""`. It runs before the take-the-top rule and overrides it: on
/// 153 measured pages of one Internet Archive file the bitonal base is BLANK and the patch
/// carries every word, and only decoding the pixels tells those apart from the pages where the
/// base carries the text. Compositing the layers is a renderer's job.
fn mrc_layers(page: &pdfdoc::Rect, imgs: &[Placement], image_info: InfoFn<'_>) -> &'static str {
    let page_area = area(&BBox {
        llx: page.llx,
        lly: page.lly,
        urx: page.urx,
        ury: page.ury,
    });
    if page_area <= 0.0 {
        return "";
    }
    let (mut base, mut patch) = (false, false);
    for (i, p) in imgs.iter().enumerate() {
        let Some(info) = image_info(p.id) else {
            continue;
        };
        if info.bpc == 1 || info.image_mask {
            // A base is painted under something. A bitonal layer on top is a stencil over the
            // raster below, which is a transparency question, not this one.
            base = base || (i + 1 < imgs.len() && area(&p.bbox) / page_area >= MRC_BASE_AREA_FRAC);
            continue;
        }
        patch = patch || area(&p.bbox) / page_area > MRC_PATCH_AREA_FRAC;
    }
    if base && patch { "mrc-layers" } else { "" }
}

/// `contains`: whether `outer` covers `inner`, within tolerance.
fn contains(outer: &BBox, inner: &BBox) -> bool {
    outer.llx <= inner.llx + COVER_TOLERANCE_PT
        && outer.lly <= inner.lly + COVER_TOLERANCE_PT
        && outer.urx >= inner.urx - COVER_TOLERANCE_PT
        && outer.ury >= inner.ury - COVER_TOLERANCE_PT
}

/// `covers`: whether `b` contains the page box, within tolerance. An image larger than the page
/// is fine: it is simply cropped on display.
fn covers(b: &BBox, page: &pdfdoc::Rect) -> bool {
    contains(
        b,
        &BBox {
            llx: page.llx,
            lly: page.lly,
            urx: page.urx,
            ury: page.ury,
        },
    )
}

fn area(b: &BBox) -> f64 {
    (b.urx - b.llx) * (b.ury - b.lly)
}

/// `marks`: whether a box has the extent to deposit ink at all, positive on both axes. A raster
/// squeezed to nothing on one axis paints nothing, however wide it is on the other. Tested per
/// axis rather than as `area > 0`: it states the claim exactly, does not underflow on a box
/// small on both axes, and stays safe if a box ever arrives inverted.
pub fn marks(b: &BBox) -> bool {
    b.urx > b.llx && b.ury > b.lly
}

/// `paintsHidden`: whether the page's rasters hide every path-painting operator on it. This is a
/// graphics-state test, not a renderer: it asks only what the content stream and the image
/// dictionaries prove (the order operators were issued in, where their paths landed, what was
/// clipping them, whether a raster is see-through).
///
/// `opaque_cover` depends only on the placement, so it is answered ONCE per placement here, not
/// once per (paint, placement) pair: neither `Scan::paints` nor `Scan::images` is capped, and
/// the pairwise form cost 25 seconds on a 4 KB page of 32,000 fills over 32,000 placements
/// (byb-kpi). The loop is still O(paints x placements); this is its constant, not its shape.
///
/// The path side is `Paint::ink` (the part the clip lets through), and the raster side is every
/// placement, because a path is hidden by anything opaque painted over it (byb-7aq).
pub fn paints_hidden(imgs: &[Placement], paints: &[Paint], image_info: InfoFn<'_>) -> bool {
    let covers: Vec<bool> = imgs.iter().map(|p| opaque_cover(p, image_info)).collect();
    for p in paints {
        let (ink, marks) = p.ink();
        if !marks {
            continue;
        }
        let tol = if p.strokes() {
            PAINT_TOLERANCE_PT
        } else {
            PAINT_FILL_TOLERANCE_PT
        };
        if !ink_hidden(&ink, &p.ctm, p.index, tol, imgs, &covers) {
            return false;
        }
    }
    true
}

/// `inkHidden`: whether some opaque placement painted after this ink contains it. `order` is the
/// ink's position in the shared painting order, `tol` is how far it may stray outside a
/// placement and still count as hidden, `ink_ctm` is the matrix the path was painted under and
/// `covers` is `opaque_cover` per placement.
///
/// The box test alone over-forgives a rotated cover: rotation cuts four corner triangles off the
/// true quadrilateral that its bounding box does not, and ink in one of them can pass without
/// being under the raster (byb-2mt). The quad test is skipped for an axis-aligned cover, where
/// the box IS the true shape, and when the ink was painted under the same rotation as the cover
/// (a wash under the same straighten as the raster): the ink box is itself only an AABB then, and
/// testing its corners against the true quad compounds two approximations.
fn ink_hidden(
    ink: &BBox,
    ink_ctm: &Matrix,
    order: i64,
    tol: f64,
    imgs: &[Placement],
    covers: &[bool],
) -> bool {
    for (img, &cover) in imgs.iter().zip(covers) {
        if img.index < order || !cover {
            continue;
        }
        let b = &img.bbox;
        if ink.llx >= b.llx - tol
            && ink.lly >= b.lly - tol
            && ink.urx <= b.urx + tol
            && ink.ury <= b.ury + tol
            && (axis_aligned(&img.ctm)
                || same_rotation(ink_ctm, &img.ctm)
                || img.ctm.unit_square_quad().contains_box(ink, tol))
        {
            return true;
        }
    }
    false
}

/// `divertClass`: maps a fine-grained `classify` (or codec) reason to the class stored in
/// `PageProvenance::diverted`.
///
/// Two vocabularies exist on purpose: the counters want detail, the stored record wants only
/// enough to answer "would re-processing help, and with what capability", which is what
/// `capability_rule` matches on, and a record written today has to stay meaningful when a later
/// release renames a counter key. `unsupported-codec-jbig2` and `-jpx` pass through;
/// `unsupported-codec-tif` (the file type pdf-rs returns for a rendered TIFF, never `tiff`)
/// normalizes to `unsupported-codec-tiff`; anything else that starts with `unsupported-codec`
/// (the coarse legacy string, or a codec name not yet special-cased) collapses to
/// `unsupported-codec`, which every decode rule still matches. A codec problem never falls
/// through to `not-single-raster`, which nominates a renderer (byb-97q). A reason of neither
/// vocabulary falls back to `not-single-raster`: a wasted re-run is cheaper than a hidden
/// upgrade.
pub fn divert_class(reason: &str) -> &'static str {
    match reason {
        "unsupported-codec-jbig2" => "unsupported-codec-jbig2",
        "unsupported-codec-jpx" => "unsupported-codec-jpx",
        "unsupported-codec-tif" => "unsupported-codec-tiff",
        r if r.starts_with("unsupported-codec") => "unsupported-codec",
        _ => "not-single-raster",
    }
}

/// What one page extraction produced, as Go's `(*PageRaster, PageProvenance, error)`: the raster
/// or the error, and the provenance record of what was done. The record is populated on success
/// (`extract-raster`, geometry, placement, dropped annotations) and carries only `diverted` for a
/// refusal; a failure leaves it zero.
pub type Extracted = (Result<PageRaster>, PageProvenance);

fn fail(e: Error) -> Extracted {
    count_failure();
    (Err(e), PageProvenance::default())
}

/// `refuse`: records the divert and wraps `err` with its reason. Every refusal site goes through
/// here, so the reason a caller reads by type and the reason the provenance record carries
/// cannot disagree.
fn refuse(page: usize, reason: &str, err: Error) -> Extracted {
    count_divert(reason);
    let class = divert_class(reason);
    (
        Err(Error::Refused {
            page,
            reason: reason.to_string(),
            class: class.to_string(),
            source: Some(Box::new(err)),
        }),
        PageProvenance {
            diverted: class.to_string(),
            ..PageProvenance::default()
        },
    )
}

/// `fmt.Errorf("%w: %s", ErrNotSingleRaster, reason)`.
fn not_single_raster(reason: &str) -> Error {
    Error::Wrapped {
        msg: format!("{}: {reason}", Error::NotSingleRaster),
        sources: vec![Error::NotSingleRaster],
    }
}

/// `fmt.Errorf("%w: <detail>", ErrUnsupportedImageCodec)`, with a `NotImplemented` as a second
/// `%w` in parentheses when the refusal is a property of the build.
fn codec_err(detail: &str, ni: Option<NotImplemented>) -> Error {
    let mut msg = format!("{}: {detail}", Error::UnsupportedImageCodec);
    let mut sources = vec![Error::UnsupportedImageCodec];
    if let Some(n) = ni {
        msg = format!("{msg} ({n})");
        sources.push(Error::NotImplemented(n));
    }
    Error::Wrapped { msg, sources }
}

fn not_implemented(capability: &str, why: &str) -> NotImplemented {
    NotImplemented {
        capability: capability.to_string(),
        why: why.to_string(),
        issue: capability_issue(capability).unwrap_or_default().to_string(),
    }
}

/// The wrap of Go's `fmt.Errorf("byblos: page %d: %w", n, err)`.
fn wrap_page(n: usize, e: Error) -> Error {
    Error::Wrapped {
        msg: format!("byblos: page {n}: {e}"),
        sources: vec![e],
    }
}

/// Errors that are the call's resources running out or being stopped, never a property of the
/// document: they are returned as they are and never become a divert.
fn is_budget(e: &Error) -> bool {
    matches!(
        e,
        Error::Alloc { .. } | Error::OutOfMemory | Error::Cancelled
    )
}

/// `ExtractPageRaster`, `…Context` and `…Limits`: the single raster of the given 1-based page of
/// the PDF in `pdf`.
///
/// A page may reach that through more than one placement. Scanner pipelines routinely paint two
/// page-covering images at the same matrix, the second hiding the first; that page has one
/// visible raster and this returns it (see [`classify`] for the conditions, which are stricter
/// than they look: a layer that is not provably an opaque cover diverts).
///
/// A page's /Rotate is a display attribute and does not affect content space, so a rotated page
/// still extracts cleanly and the returned image is the raster as stored. Orientation within
/// content space is a narrower promise: the raster is the page as it reads, to within
/// [`MAX_SKEW_DEG`]. A scanner's deskew lives inside that tolerance and the page extracts as
/// stored, the residual affine going to the provenance record: resampling a bilevel raster to
/// take out a tenth of a degree would break the lossless promise. Past the tolerance the page
/// diverts, this including a quarter turn and a mirror, because the returned image is consumed
/// (OCR, thumbnails, human review) by code that never reads provenance.
///
/// Vector paint is judged the same way, by what the content stream proves: a path painted before
/// the raster and landing inside its placement box is behind an opaque image and cannot be seen.
/// Nothing is filled, scan-converted or composited, only painting order and a bounding box.
///
/// Limits: the parse and content-walk caps of `limits` (a breach is [`Error::Limit`]), and
/// `max_alloc` for every buffer of the call, the JPEG decode and the JBIG2 page included.
/// `cancel` is checked at entry, after the open, and per token in the content walk. The open
/// itself ignores it, as Go does (`pdfdoc.OpenLimits(context.Background(), ...)`): pdfcpu would
/// add a check per object, which the latency table pins. Cancellation latency is therefore the
/// whole of one page's extraction, and a call abandoned at the first two boundaries stays out of
/// the extraction telemetry: it is not a failed extraction.
///
/// # Errors
/// [`Error::Refused`] (its source is [`Error::NotSingleRaster`] for a page that is not one
/// raster and [`Error::UnsupportedImageCodec`] for a codec byblos cannot decode, the latter
/// holding a [`NotImplemented`] where a future build recovers the page); [`Error::Limit`],
/// [`Error::Alloc`], [`Error::Cancelled`]; [`Error::Malformed`] for a page out of range; and the
/// read errors of `pdfdoc::open`.
pub fn extract_page_raster(
    pdf: &[u8],
    page: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<PageRaster> {
    check_cancel(cancel)?;
    let mut d = match pdfdoc::open(pdf, limits, &NEVER) {
        Ok(d) => d,
        Err(e) => {
            // A caller that cancels mid-open must see a cancellation, not a read failure, and
            // the call must stay out of the counters.
            check_cancel(cancel)?;
            count_attempt();
            count_failure();
            return Err(e);
        }
    };
    check_cancel(cancel)?;
    extract_page(&mut d, page, cancel).0
}

/// `extractPage`: [`extract_page_raster`]'s body once the document is open, plus the record of
/// what it did. It lets `record_extraction` walk a whole document with one `pdfdoc::open` and
/// still get exactly the outcome `extract_page_raster` reports, and it is the only scope where
/// the UNROUNDED floats exist: `PageRaster::bounds` and `page` have already been through
/// rounding, and 568.3708 is not 568. The limits are the document's own.
pub fn extract_page(d: &mut Doc, page: usize, cancel: &AtomicBool) -> Extracted {
    count_attempt();
    let mut p = match d.page(page) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let scan = match scan_page(d, &mut p, page, cancel) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };

    let (idx, reason) = classify(&p.crop_box, &scan, &|id| d.image_info(id).cloned());
    if !reason.is_empty() {
        return refuse(page, reason, not_single_raster(reason));
    }
    let Some(placement) = scan.images.get(idx) else {
        return fail(Error::Malformed(format!(
            "byblos: page {page}: no placement at index {idx}"
        )));
    };

    let (data, file_type) = match d.raw_image(cancel, placement.id) {
        Ok(r) => r,
        // pdf-rs declines to render some filters; pdfdoc turns that into UnsupportedCodec. That
        // is a divert (the page is understood, its codec is not), never a read failure. It
        // declines before naming a file type, so there is no codec name to carry and no
        // NotImplemented to name: the register has no capability for "some codec pdf-rs
        // declined to identify" (byb-bjh).
        Err(e @ Error::UnsupportedCodec) => {
            return refuse(page, "unsupported-codec", codec_err(&e.to_string(), None));
        }
        Err(e) => return fail(e),
    };
    let img = match decode_raster(d, placement.id, &data, file_type, cancel) {
        Decoded::Image(i) => i,
        Decoded::Refuse { reason, err } => return refuse(page, &reason, err),
        Decoded::Fail(e) => return fail(e),
    };

    let mut out = PageRaster {
        image: img,
        bounds: box_rect(&placement.bbox),
        page: rect_of(&p.crop_box),
        raster_quad: None,
        obj_nr: 0,
        dropped_annots: 0,
        bitonal: false,
    };
    if !axis_aligned(&placement.ctm) {
        out.raster_quad = Some(placement.ctm.unit_square_quad().0);
    }
    // The declared depth, from the same dictionary fact classify took and inspect turns into
    // ImageRef.bitonal: a page's PageRaster.bitonal and its ImageRef.bitonal must never
    // disagree, and obj_nr is resolved the way inspect resolves ImageRef.obj_nr, since its whole
    // purpose is to be the handle a caller already holds from there.
    if let Some(info) = d.image_info(placement.id) {
        out.bitonal = info.bpc == 1 || info.image_mask;
        out.obj_nr = info.obj_nr;
    }
    // Only on the success path: a divert has already told the caller it gets no raster, and
    // 97% of a real archive diverts. An unreadable /Annots is not a reason to fail a page whose
    // raster is fine; it leaves the count at zero, which understates the loss.
    if let Ok(annots) = d.annots(page) {
        let n = annots.iter().filter(|a| a.paints()).count();
        out.dropped_annots = i64::try_from(n).unwrap_or(i64::MAX);
    }
    let mut geom = PageGeometry {
        raster_box: normalized_box(
            placement.bbox.llx,
            placement.bbox.lly,
            placement.bbox.urx,
            placement.bbox.ury,
        ),
        page_box: normalized_box(
            p.crop_box.llx,
            p.crop_box.lly,
            p.crop_box.urx,
            p.crop_box.ury,
        ),
        clip_box: None,
        raster_quad: None,
    };
    // ClipBox is recorded only when a clip actually narrowed this placement below its own
    // unclipped extent, and from the clip itself, not from `placement.bbox`, which after
    // byb-b1.12 IS the narrowed box already.
    if clip_narrowed(placement)
        && let Some(c) = &placement.clip
    {
        geom.clip_box = Some(normalized_box(c.llx, c.lly, c.urx, c.ury));
    }
    // Like the placement below, populated only for a rotated or sheared placement.
    if !axis_aligned(&placement.ctm) {
        geom.raster_quad = Some(placement.ctm.unit_square_quad().0);
    }
    let mut rec = PageProvenance {
        applied: vec!["extract-raster".to_string()],
        dropped_annots: out.dropped_annots,
        geometry: Some(geom),
        ..PageProvenance::default()
    };
    // Empty for an axis-aligned placement, which is almost every page. The off-diagonal terms
    // are exactly what skew_degrees reads and classify has rejected anything past MAX_SKEW_DEG,
    // so what survives here is a scanner deskew.
    if !axis_aligned(&placement.ctm) {
        rec.placement = placement.ctm.0.to_vec();
    }
    count_extracted(out.covers_page());
    (Ok(out), rec)
}

/// `inspectPage` without the `PageInfo` that extraction never reads: walks the page's content
/// under its own resource scope. A walk failure is the error, wrapped with the page number; the
/// partial scan Go also returns is not used by any caller here.
fn scan_page(d: &mut Doc, p: &mut pdfdoc::Page, n: usize, cancel: &AtomicBool) -> Result<Scan> {
    let lim = d.lim;
    let src: Stream = Arc::new(std::mem::take(&mut p.content));
    let env = DocEnv::new(d);
    let (scan, err) = content::walk(cancel, &src, p.scope, &env, &lim);
    match err {
        Ok(()) => Ok(scan),
        Err(e) => Err(wrap_page(n, e)),
    }
}

/// How the codec switch of `extractPage` ended.
enum Decoded {
    Image(Image),
    /// A divert: the page is understood, its codec is not.
    Refuse {
        reason: String,
        err: Error,
    },
    /// A limit, budget or cancellation: counted as a failure, never a divert.
    Fail(Error),
}

/// The codec switch of `extractPage`: decodes the raster `data` of image `id`, whose file type
/// pdf-rs inferred.
fn decode_raster(
    d: &mut Doc,
    id: i64,
    data: &[u8],
    file_type: &str,
    cancel: &AtomicBool,
) -> Decoded {
    // One ledger per call: every decode below sees only what the parse and the raw `data` it was
    // handed leave (0 would mean the default, so keep at least 1).
    let lim = Limits {
        max_alloc: d
            .ctx
            .xref
            .mem()
            .left()
            .saturating_sub(data.len() as u64)
            .max(1),
        ..d.lim
    };
    match file_type {
        // pdf-rs returns JBIG2 as opaque bytes rather than erroring, so nothing upstream has
        // looked at them and `image::decode` would not recognise them either. byblos decodes
        // them itself: the generic-region subset `encode_generic` writes (byb-riy) and the
        // arithmetic symbol mode most archive scanners emit (byb-9v0). What still diverts is the
        // rest, under the SAME reason string as before: the taxonomy does not gain a term for
        // it, since a page that needs a fuller decoder and one that needs any decoder nominate
        // the same capability, decode-jbig2.
        //
        // The symbol dictionary a text region places is very often not in this stream at all: a
        // bulk scanner writes one per DOCUMENT and points every page at it through
        // /DecodeParms /JBIG2Globals. Without it such a page decodes to a blank raster rather
        // than failing, so the globals are fetched here, and a failure to read an entry that IS
        // there is a diverted page rather than a silent one. No NotImplemented on that failure
        // (byb-bjh): an unresolvable /JBIG2Globals is a property of THIS document, so no future
        // capability recovers the page.
        "jbig2" => {
            let globals = match d.raw_image_globals(id) {
                Ok(g) => g,
                Err(e) if is_budget(&e) => return Decoded::Fail(e),
                Err(e) => {
                    return Decoded::Refuse {
                        reason: "unsupported-codec-jbig2".to_string(),
                        err: codec_err(&format!("jbig2: {e}"), None),
                    };
                }
            };
            let globals = globals.as_deref().unwrap_or_default();
            match decode_jbig2_placement(data, globals, d.image_info(id), id, &lim, cancel) {
                Ok(img) => Decoded::Image(img),
                Err(e) if is_budget(&e) => Decoded::Fail(e),
                // A coding mode byblos has not built is a property of the BUILD and damaged
                // bytes are a property of the DOCUMENT. The first is recovered by a future
                // byblos for every document at once, the second by nothing: a caller that
                // re-processes on the wrong one wastes a pass over the archive or never
                // re-processes at all. Measured: 36 of 37 diverts reaching here were a coding
                // mode. No fileType is interpolated: every error here already opens with
                // "jbig2:".
                Err(e @ Error::UnsupportedJbig2Feature(_)) => Decoded::Refuse {
                    reason: "unsupported-codec-jbig2".to_string(),
                    err: codec_err(
                        &jbig2_text(&e),
                        Some(not_implemented(
                            "decode-jbig2",
                            "the stream uses a JBIG2 coding mode this build does not decode",
                        )),
                    ),
                },
                Err(e) => Decoded::Refuse {
                    reason: "unsupported-codec-jbig2".to_string(),
                    err: codec_err(&jbig2_text(&e), None),
                },
            }
        }
        // As above for the opaque bytes, and unlike jbig2 there is no byblos JPEG 2000 code in
        // either direction, so every one of these diverts: the unconditional case, which a
        // future build recovers for all of them together. The reason carries the file type so
        // `divert_class` can emit a codec-specific class and `capability_rule` can nominate
        // decode-jpx without also nominating decode-jbig2 (byb-z8j).
        "jpx" => Decoded::Refuse {
            reason: "unsupported-codec-jpx".to_string(),
            err: codec_err(
                "jpx",
                Some(not_implemented(
                    "decode-jpx",
                    "byblos has no JPEG2000 code in either direction and x/image ships no decoder",
                )),
            ),
        },
        _ => {
            let r = reserve_dct(data, d, id).and_then(|f| {
                if data.starts_with(&[0xFF, 0xD8]) {
                    decode_dct(data, &f, lim.image_pixels(), lim.max_alloc, &lim, cancel)
                } else {
                    image::decode(data, &lim, cancel)
                }
            });
            match r {
                Ok(img) => Decoded::Image(img),
                Err(e) if matches!(e, Error::Limit { .. }) || is_budget(&e) => Decoded::Fail(e),
                // The reason carries fileType so the divert nominates one decoder rather than
                // all three (byb-z8j). This is NOT "CMYK rasters divert": pdf-rs renders a CMYK
                // raster to TIFF (file type `tif`) and the TIFF reader decodes it. A TIFF
                // reaches here only when that reader REJECTS it (an unsupported compression),
                // which is why decode-tiff is rare in practice rather than dead. No
                // NotImplemented either (byb-bjh): the decoder cannot tell an unsupported
                // compression from a corrupt file, so naming the capability would be a guess.
                Err(e) => Decoded::Refuse {
                    reason: format!("unsupported-codec-{file_type}"),
                    err: codec_err(&format!("{file_type}: {e}"), None),
                },
            }
        }
    }
}

/// The text Go's internal/jbig2 error would print: `jbig2: segment N: jbig2: unsupported
/// feature: X` for a coding mode, every other error already opens with `jbig2:`.
fn jbig2_text(e: &Error) -> String {
    match e {
        Error::Malformed(s) if s.starts_with("jbig2:") => s.clone(),
        Error::UnsupportedJbig2Feature(s) => match s.strip_prefix("segment ") {
            Some(rest) => match rest.split_once(": ") {
                Some((n, f)) => format!("jbig2: segment {n}: jbig2: unsupported feature: {f}"),
                None => format!("jbig2: unsupported feature: {s}"),
            },
            None => format!("jbig2: unsupported feature: {s}"),
        },
        other => other.to_string(),
    }
}

/// `reserveDCT` without the budget: the SOF of `data` against the image dictionary of image
/// `id` of `d` under `d`'s pixel cap. The cap is [`Error::Limit`] `image-pixels`, and a frame
/// the dictionary does not allow, or no frame header at all, is [`Error::Malformed`]. A frame of
/// at most the dictionary's pixels and the dictionary's component count (when it names a device
/// space) is allowed and decodes at its own size, as viewers do. Data that is not a JPEG passes
/// with a zero [`Frame`].
///
/// # Errors
/// As [`dct::check_frame`].
pub fn reserve_dct(data: &[u8], d: &Doc, id: i64) -> Result<Frame> {
    let (mut dict_pixels, mut n) = (-1i64, 0usize);
    if let Some(info) = d.image_info(id) {
        dict_pixels = limits::mul_sat(info.width, info.height);
        n = match info.color_space.as_str() {
            "DeviceGray" => 1,
            "DeviceRGB" => 3,
            "DeviceCMYK" => 4,
            _ => 0,
        };
    }
    dct::check_frame(data, d.lim.image_pixels(), dict_pixels, n)
}

/// `decodeDCT`: decodes a JPEG through jpeg-rs (jpeg-go's port), whose pixels are
/// libjpeg-turbo's (poppler's and ocrmypdf's decoder): a grey stream comes back as
/// [`Image::Gray`], a colour one as [`Image::Rgba`]. What it does not read (CMYK and YCCK,
/// 12-bit, lossless) falls back to [`image::decode`] (pdf-rs `gojpeg`), as before. `f` is the
/// frame [`reserve_dct`] returned; the decode is bounded by `max_pixels` and by `max_alloc`
/// bytes, which the caller sets to what its ledger has left. A stream the decoder reads only
/// partly (truncated entropy data, a fatal marker after the last row) is an error, as in Go,
/// where the image comes back beside it and is dropped.
///
/// # Errors
/// The jpeg-rs errors as [`Error::Pdf`] (its allocation refusals and cancellation as
/// [`Error::Alloc`], [`Error::OutOfMemory`] and [`Error::Cancelled`]), and those of
/// [`image::decode`] for the fallback.
pub fn decode_dct(
    data: &[u8],
    f: &Frame,
    max_pixels: i64,
    max_alloc: u64,
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<Image> {
    let opts = jpeg::DecodeOptions {
        format: if f.n == 1 {
            jpeg::Format::Gray
        } else {
            jpeg::Format::Rgba
        },
        ..jpeg::DecodeOptions::default()
    };
    let jl = jpeg::Limits {
        max_pixels: u64::try_from(max_pixels).unwrap_or(u64::MAX),
        max_alloc,
        max_threads: lim.threads(),
    };
    match jpeg::decode(data, &opts, &jl, cancel) {
        Ok(dec) => {
            if let Some(e) = dec.error {
                return Err(Error::from(pdf::Error::Jpeg(e)));
            }
            Ok(match dec.image {
                jpeg::Image::Gray(g) => Image::Gray(g.into()),
                jpeg::Image::Rgba(r) => Image::Rgba(r.into()),
                jpeg::Image::YCbCr(y) => Image::YCbCr(y.into()),
            })
        }
        Err(jpeg::Error::Unsupported) => image::decode(data, lim, cancel),
        Err(e) => Err(Error::from(pdf::Error::Jpeg(e))),
    }
}

/// `decodeJBIG2Placement`: extractPage's JBIG2 branch. Checks the stream against the image
/// dictionary (`info`, `None` for one that could not be read), decodes it, and hands back a
/// raster in the polarity a renderer would show.
///
/// Two dictionary facts can make a correctly decoded bitmap the wrong answer, and both are read
/// rather than assumed away, because each one's failure mode is a silently inverted or
/// mis-shaped page rather than an error: /Decode remaps samples (on a 1-bit image `[1 0]`
/// inverts it; the array is APPLIED, byb-e7n, see `gray_levels`), and /Width and /Height are
/// what the PDF says the raster is (a page of a different size means the stream is not the one
/// this dictionary describes, or was mis-parsed).
///
/// THE ORDER OF THE CHECKS IS LOAD-BEARING. This runs on bytes from an untrusted PDF, and a
/// JBIG2 generic region is the one codec whose output is not bounded by its input: 67 bytes can
/// ask for half a billion pixels. Decoding first and comparing afterwards pays in full for a
/// stream that was never going to be accepted (1.5 s and 16 MiB for a 1x1 dictionary over an
/// 8191x8191 page); `jbig2::page_size_with_globals` resolves the page from the 26-byte region
/// headers, so the same mismatch is refused in about a microsecond with nothing decoded. The
/// size cap in front of it is the same argument one step earlier: `gray_image` expands the page
/// to one byte per pixel, 8x the packed bitmap, so [`jbig2::MAX_PAGE_PIXELS`] bounds the largest
/// single allocation anywhere in a JBIG2 extraction, and testing the dictionary's own declared
/// dimensions against it refuses an absurd image without opening the stream at all.
///
/// /ImageMask, /SMask and /Mask need no check here: `opaque_cover` already refuses to treat such
/// an image as the page's raster, so `classify` has diverted the page as vector-paint long
/// before this runs.
///
/// Every error it returns opens with `jbig2:`, so `extract_page` can wrap the lot without naming
/// the codec a second time. A refusal for a missing coding mode is
/// [`Error::UnsupportedJbig2Feature`] (what Go's `markUnsupportedJBIG2` attaches; the Rust jbig2
/// errors carry that variant already), and every other error is damage.
///
/// # Errors
/// [`Error::Malformed`] for a dictionary problem or a damaged stream,
/// [`Error::UnsupportedJbig2Feature`], [`Error::Alloc`], [`Error::OutOfMemory`],
/// [`Error::Cancelled`].
pub fn decode_jbig2_placement(
    data: &[u8],
    globals: &[u8],
    info: Option<&ImageInfo>,
    id: i64,
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<Image> {
    let Some(info) = info else {
        return Err(Error::Malformed(format!(
            "jbig2: image {id} has no dictionary to check the decoded raster against"
        )));
    };
    let (ink, background) = gray_levels(info, id)?;
    let px = info.width.wrapping_mul(info.height);
    if px > jbig2::MAX_PAGE_PIXELS {
        return Err(Error::Malformed(format!(
            "jbig2: image {id}'s dictionary declares {}x{}, {px} pixels; byblos renders a \
             bilevel page at one byte per pixel and the limit is {}",
            info.width,
            info.height,
            jbig2::MAX_PAGE_PIXELS
        )));
    }
    let (w, h) = jbig2::page_size_with_globals(globals, data, lim)?;
    if w as i64 != info.width || h as i64 != info.height {
        return Err(Error::Malformed(format!(
            "jbig2: page is {w}x{h} but image {id}'s dictionary says {}x{}",
            info.width, info.height
        )));
    }
    let b = jbig2::decode_embedded_stream_with_globals(globals, data, lim, cancel)?;
    gray_image(&b, ink, background, lim, cancel).map(Image::Gray)
}

/// The `/DeviceGray` sample a 1-bit image carries for an ink pixel when it declares no /Decode
/// array: the JBIG2Decode filter presents a JBIG2 ink pixel as sample 0 (`defaultInk`).
const DEFAULT_INK: u8 = 0x00;
/// The sample of a background pixel under the default array `[0 1]` (`defaultBackground`).
const DEFAULT_BACKGROUND: u8 = 0xFF;

/// `grayLevels`: the `/DeviceGray` samples an ink bit and a background bit must become, applying
/// image `id`'s /Decode array.
///
/// The array is applied at this boundary and not by inverting the bitmap or carrying a polarity
/// flag: the bitmap's convention (a set bit is ink) has nothing to do with /Decode, which remaps
/// the SAMPLE a renderer reads, which is what [`gray_image`] builds. Poppler is the
/// specification (measured with pdftoppm 26.06.0 on a 4x4 raster, `/DeviceGray`, 1 bit): absent
/// and `[0 1]` give ink 0 and background 255, `[1 0]` ink 255 and background 0, `[0.5 1]` ink
/// 128, `[0 0]` gives 0 and 0, `[2 -1]` clamps to 255 and 0. So the rule is a two-entry lookup
/// (sample 0 takes the array's first number, sample 1 its second), not "invert on `[1 0]`".
///
/// Still refused, because the numbers then do not mean grey levels: any colour space other than
/// `/DeviceGray`, INCLUDING an unresolved one (`color_space` is empty for an array or an
/// indirect reference, which is where /Indexed lives: `[1 0]` there swaps two palette entries),
/// an array that is not all numbers, and one that is not two long. All still say "/Decode
/// array": the byb-9v0 census classifies this gate by that phrase.
fn gray_levels(info: &ImageInfo, id: i64) -> Result<(u8, u8)> {
    if !info.decode {
        return Ok((DEFAULT_INK, DEFAULT_BACKGROUND));
    }
    if info.color_space != "DeviceGray" {
        let cs = if info.color_space.is_empty() {
            "an array or an indirect reference".to_string()
        } else {
            format!("/{}", info.color_space)
        };
        return Err(Error::Malformed(format!(
            "jbig2: image {id} carries a /Decode array against {cs}; byblos applies one only in \
             /DeviceGray, where its two numbers ARE the grey levels"
        )));
    }
    let Some(arr) = &info.decode_array else {
        return Err(Error::Malformed(format!(
            "jbig2: image {id} carries a /Decode array whose entries are not all numbers, so \
             there is nothing to apply"
        )));
    };
    match arr.as_ref() {
        [a, b] => Ok((gray_sample(*a), gray_sample(*b))),
        _ => Err(Error::Malformed(format!(
            "jbig2: image {id} carries a /Decode array of {} entries; a one-component image's \
             array has two",
            arr.len()
        ))),
    }
}

/// `graySample`: maps a /DeviceGray component to an 8-bit sample, clamping outside `[0, 1]` as
/// poppler does. The first case is a negated test so that a NaN takes it: these numbers come
/// out of an untrusted file.
fn gray_sample(v: f64) -> u8 {
    if v.is_nan() || v <= 0.0 {
        0x00
    } else if v >= 1.0 {
        0xFF
    } else {
        // In (0, 1): the product is in (0, 255], so the cast is exact.
        (v * 255.0).round() as u8
    }
}

/// `grayImage`: renders `b` as an 8-bit greyscale image, writing `ink` for a set bit and
/// `background` for a clear one. Under the PDF /DeviceGray convention and an absent /Decode
/// array those are 0x00 and 0xFF.
///
/// That inversion is the Bitmap-to-PDF boundary: it is applied here because the result is handed
/// out as an image to code (OCR, thumbnailing, human review) that cannot tell a photographic
/// negative from a page. [`Gray`] rather than a one-bit image over the same buffer costs 8x the
/// memory and buys interoperability with everything that matches on the concrete type for a
/// fast path, including `downsample`. That 8x is why `MAX_PAGE_PIXELS` is the bound that matters
/// on this path.
///
/// # Errors
/// [`Error::Alloc`] or [`Error::OutOfMemory`] for the image, [`Error::Cancelled`] (checked per
/// row).
pub fn gray_image(
    b: &Bitmap,
    ink: u8,
    background: u8,
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<Gray> {
    let w = i64::try_from(b.width).unwrap_or(i64::MAX);
    let h = i64::try_from(b.height).unwrap_or(i64::MAX);
    let mut g = Gray::new(Rect::new(0, 0, w, h), lim)?;
    let stride = g.stride.max(1);
    for (y, row) in g.pix.chunks_mut(stride).enumerate() {
        check_cancel(cancel)?;
        let y = i64::try_from(y).unwrap_or(i64::MAX);
        for (x, px) in row.iter_mut().enumerate() {
            let x = i64::try_from(x).unwrap_or(i64::MAX);
            *px = if b.get(x, y) != 0 { ink } else { background };
        }
    }
    Ok(g)
}
