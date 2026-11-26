//! Port of byblos `inspect.go`: what every page of a document contains, without rendering
//! anything.
//!
//! Go's `Inspect`, `InspectContext` and `InspectDocument` are one function here, [`inspect`],
//! which returns the [`DocumentInfo`] (Go's `Inspect` returns its `pages`). The context becomes
//! the `cancel` flag, checked where Go checks `ctx`: at entry and at each page boundary, and
//! per token inside the content walk.
//!
//! CANCELLATION LATENCY: as in Go, [`crate::pdfdoc::open_budgeted`] runs before the page loop
//! and is one parse of the whole document, so the per-page check bounds the walk by one page
//! but not the call.
//!
//! Go's `image.Rectangle` is [`crate::image::Rect`]: integer, in PDF default user space (points,
//! origin lower-left, y increasing upward), not screen coordinates.

use crate::content::{self, BBox, Placement, Scan};
use crate::error::{Error, Result};
use crate::go::{math, sort};
use crate::image::Rect;
use crate::limits::{self, Limits, check_cancel};
use crate::pdfdoc::{self, Doc, DocEnv, is_wrong_password};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// `ImageRef`: one painting of an image on a page.
///
/// `bounds` is where the image is actually VISIBLE: the bounding box of `placement` intersected
/// with any `W`/`W*` clip path or form `/BBox` in effect at the `Do`. It is never empty for a
/// placement that can paint: one narrower than a point on an axis is widened to the smallest
/// integer rectangle that contains it, so read a scale off `placement`, not off `bounds`.
///
/// `placement` is the matrix the image was painted with, PDF order `[a b c d e f]`
/// (ISO 32000-1 8.3.3), unclipped.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageRef {
    pub bounds: Rect,
    pub placement: [f64; 6],
    /// `placement`'s rotation about the page's axes, in degrees, positive counter-clockwise:
    /// `atan2(b, a)`. Signed, where Go's `skewDegrees` is not.
    pub placement_deg: f64,
    /// Pixel dimensions of the stored raster.
    pub width: i64,
    pub height: i64,
    /// 1 bit per component, or an image mask.
    pub bitonal: bool,
    /// The image dictionary's `/ColorSpace` when it is a bare name (`DeviceGray`, `DeviceRGB`,
    /// `DeviceCMYK`), resolved through the cross-reference table; empty for an array, a missing
    /// entry or an inline image.
    pub color_space: String,
    /// `/BitsPerComponent`, 0 when absent.
    pub bpc: i64,
    /// The codec the stored raster declares (`JBIG2Decode`, `DCTDecode`, `FlateDecode`, ...);
    /// empty when it declares none. The codec, not the transport wrapper. The name's bytes as
    /// written, as in Go, where a name is a `string` that need not be UTF-8.
    pub filter: Vec<u8>,
    /// The image XObject's object number: per object, not per painting. Negative for a direct
    /// object.
    pub obj_nr: i64,
    /// `replace_images` will accept this image: false for an `/SMask`, a `/Mask`, an
    /// `/ImageMask` and a direct object.
    pub substitutable: bool,
    /// A `BI ... ID ... EI` operator rather than a `Do`. `width`, `height`, `filter` and
    /// `obj_nr` are then zero or empty and `substitutable` is false.
    pub inline: bool,
}

/// `PageInfo`: one page.
///
/// `bounds` is the CropBox, or the MediaBox when there is none, in the convention of
/// [`ImageRef::bounds`]. `rotate` is the effective `/Rotate` resolved through page-tree
/// inheritance, normalised into `[0, 360)` (so -90 reads back as 270) and not guaranteed to be a
/// multiple of 90; it is not applied to `bounds`. `text_chars` counts the bytes shown by the
/// text operators, forms included: stored code units, not Unicode code points.
/// `diagnostics` holds what byblos worked around on this page; a page carrying one was still
/// read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageInfo {
    /// 1-based.
    pub index: usize,
    pub bounds: Rect,
    pub rotate: i64,
    pub images: Vec<ImageRef>,
    pub text_chars: usize,
    pub diagnostics: Vec<Diagnostic>,
}

/// `Severity`: how much of a [`PageInfo`] to believe when byblos worked around something on the
/// page. Poppler's distinction: both categories are worked around, neither removes a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Severity {
    /// Worked around; the page's numbers are probably right.
    #[default]
    Warning,
    /// Worked around; the page's numbers are probably WRONG, and wrong LOW: a content stream
    /// that stops early paints fewer images and shows less text than the page holds.
    Error,
}

impl fmt::Display for Severity {
    /// `Severity.String`: `"error"` or `"warning"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        })
    }
}

/// `Diagnostic`: one problem byblos worked around while reading a page. `message` is the
/// underlying error's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
}

/// `DocumentInfo`: the pages, and the facts about the whole file that decide whether it may be
/// processed at all.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocumentInfo {
    pub pages: Vec<PageInfo>,
    /// The document carries `/Encrypt` and opened with the empty password. One that needs a
    /// user password does not open: the error is [`Error::Password`].
    pub encrypted: bool,
    /// `/AcroForm /SigFlags` bit 1 (SignaturesExist) is set.
    pub has_signature: bool,
    /// The catalog's `/NeedsRendering` is true (dynamic XFA).
    pub needs_rendering: bool,
}

/// `InspectDocument`: reports what every page of `data` contains, under `lim`, and the
/// document-level facts. It does not render anything.
///
/// A page byblos had to work around is still reported, with a [`Severity::Error`] diagnostic.
/// A breached cap or budget, and cancellation, fail the call.
///
/// # Errors
/// [`Error::Password`] when the document needs a user password; [`Error::Limit`],
/// [`Error::Alloc`] and [`Error::OutOfMemory`] for a breached cap or budget;
/// [`Error::Cancelled`]; otherwise what [`crate::pdfdoc::open_budgeted`] returns.
pub fn inspect(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<DocumentInfo> {
    check_cancel(cancel)?;
    let mut doc = pdfdoc::open_budgeted(data, lim, cancel).map_err(|e| {
        if is_wrong_password(&e) {
            Error::Password
        } else {
            e
        }
    })?;
    let cat = doc.catalog();
    let page_count = doc.page_count();
    // The records this call builds are charged to the call's ledger until it returns.
    let mut held = doc.ctx.xref.mem().charge(0)?;
    held.grow_by((page_count as u64).saturating_mul(size_of::<PageInfo>() as u64))?;
    let mut di = DocumentInfo {
        pages: Vec::new(),
        encrypted: doc.encrypt_info().encrypted,
        has_signature: cat.has_signature,
        needs_rendering: cat.needs_rendering,
    };
    di.pages.try_reserve_exact(page_count)?;
    let mut images: i64 = 0;
    let dl = *doc.limits();
    for n in 1..=page_count {
        check_cancel(cancel)?;
        let (pi, _, res) = inspect_page(&mut doc, cancel, n, &mut held);
        // `pi` is `None` only when the page dictionary would not resolve; every error after
        // that is a walk error, which carries the page prefix.
        let mut walk_err = pi.is_some();
        let mut err = res.err();
        // A cap or budget refusal is the document's, not a page byblos worked around: it fails
        // the call, whichever layer noticed it.
        if let Some(derr) = doc.err() {
            err = Some(derr.clone());
            walk_err = false;
        }
        let mut pi = pi.unwrap_or_else(|| PageInfo {
            index: n,
            ..PageInfo::default()
        });
        if let Some(e) = err {
            // The walk caps at their defaults stay page diagnostics; a caller who set one
            // asked for the failure.
            if (e.is_limit() && !default_walk_cap(&e, &dl))
                || matches!(e, Error::Alloc { .. } | Error::OutOfMemory)
            {
                return Err(e);
            }
            // Cancellation is not a defect in the document. Reporting it as a page diagnostic
            // would hand back a document that was never read.
            check_cancel(cancel)?;
            let message = if walk_err {
                format!("byblos: page {n}: {e}")
            } else {
                e.to_string()
            };
            held.grow_by(message.len() as u64)?;
            pi.diagnostics.try_reserve_exact(1)?;
            pi.diagnostics.push(Diagnostic {
                severity: Severity::Error,
                message,
            });
        }
        // The per-page caps do not bound what the pages keep together: pages can share one
        // content stream. The same caps hold for the document.
        images = images.saturating_add(i64::try_from(pi.images.len()).unwrap_or(i64::MAX));
        limits::check(
            "images",
            images,
            dl.images().wrapping_add(dl.inline_images()),
        )?;
        di.pages.push(pi);
    }
    Ok(di)
}

/// `defaultWalkCap`: whether `err` is a content-walk nesting cap the caller left at its
/// default, which is the field being `<= 0` (`Limits::default()` leaves these three at 0).
fn default_walk_cap(err: &Error, l: &Limits) -> bool {
    let Error::Limit { name, .. } = err else {
        return false;
    };
    match *name {
        "q-depth" => l.max_gstate_depth <= 0,
        "form-depth" => l.max_form_depth <= 0,
        "form-invocations" => l.max_form_invocations <= 0,
        _ => false,
    }
}

/// `inspectPage`: the page's [`PageInfo`] alongside the raw walk, which `extract_page_raster`
/// needs for classification.
///
/// On a walk failure it returns BOTH the error and what the walk established before it: a
/// content stream that stops early still describes the part of the page it reached. A page
/// whose dictionary will not resolve returns no `PageInfo` and no `Scan`. The walk error is
/// returned unwrapped; Go's `byblos: page N:` prefix is [`inspect`]'s to add where it reports
/// the error as a diagnostic.
pub(crate) fn inspect_page(
    doc: &mut Doc,
    cancel: &AtomicBool,
    n: usize,
    held: &mut pdf::Held,
) -> (Option<PageInfo>, Option<Scan>, Result<()>) {
    let p = match doc.page(n) {
        Ok(p) => p,
        Err(e) => return (None, None, Err(e)),
    };
    let lim = *doc.limits();
    let mut pi = PageInfo {
        index: n,
        bounds: rect_of(
            p.crop_box.llx,
            p.crop_box.lly,
            p.crop_box.urx,
            p.crop_box.ury,
        ),
        rotate: p.rotate,
        ..PageInfo::default()
    };
    let scope = p.scope;
    let env = DocEnv::new(doc);
    let (s, walk_res) = content::walk(cancel, &Arc::new(p.content), scope, &env, &lim);
    pi.text_chars = s.text_chars;
    // `images` and `inline_images` are kept separate on `Scan` so that classify and the other
    // `images` consumers don't see inline images; inspect is the one caller that wants both,
    // back in paint order by index.
    let mut placements: Vec<&Placement> = Vec::new();
    let count = s.images.len().saturating_add(s.inline_images.len());
    let per = size_of::<&Placement>().saturating_add(size_of::<ImageRef>());
    if let Err(e) = held.grow_by((count as u64).saturating_mul(per as u64)) {
        return (Some(pi), Some(s), Err(e.into()));
    }
    if let Err(e) = placements.try_reserve_exact(count) {
        return (Some(pi), Some(s), Err(e.into()));
    }
    placements.extend(s.images.iter());
    placements.extend(s.inline_images.iter());
    sort::slice_by(&mut placements, |a, b| a.index < b.index);
    if let Err(e) = pi.images.try_reserve_exact(placements.len()) {
        return (Some(pi), Some(s), Err(e.into()));
    }
    for pl in placements {
        let mut r = ImageRef {
            bounds: box_rect(&pl.bbox),
            placement: pl.ctm.0,
            placement_deg: placement_deg(&pl.ctm.0),
            inline: pl.inline,
            ..ImageRef::default()
        };
        let filled = env.with(|d| -> Result<()> {
            if let Some(info) = d.image_info(pl.id) {
                held.grow_by(
                    (info.color_space.len() as u64).saturating_add(info.filter.len() as u64),
                )?;
                r.width = info.width;
                r.height = info.height;
                r.bitonal = info.bpc == 1 || info.image_mask;
                r.color_space = info.color_space.clone();
                r.bpc = info.bpc;
                r.filter = info.filter.clone();
                r.obj_nr = info.obj_nr;
                // The refusals of the write seam, read off the same record that supplies
                // `bitonal`. A negative object number is a direct object, which has no
                // cross-reference entry to write a substitution back to.
                r.substitutable = info.obj_nr >= 0 && !info.smask && !info.mask && !info.image_mask;
            }
            Ok(())
        });
        if let Err(e) = filled {
            return (Some(pi), Some(s), Err(e));
        }
        pi.images.push(r);
    }
    (Some(pi), Some(s), walk_res)
}

/// `rectOf`: projects a page box onto the integer rectangle [`PageInfo::bounds`] reports, by
/// [`box_rect`]'s rule. A box with no extent still projects to an empty rectangle.
fn rect_of(llx: f64, lly: f64, urx: f64, ury: f64) -> Rect {
    let (x0, x1) = round_extent(llx, urx);
    let (y0, y1) = round_extent(lly, ury);
    Rect::new(x0, y0, x1, y1)
}

/// `boxRect`: projects a float box onto the integer rectangle a caller reads as
/// [`ImageRef::bounds`]. It never reports an EMPTY rectangle for a box with a positive extent:
/// on an axis whose two edges round together it widens outward to the smallest integer
/// interval that contains the box.
fn box_rect(b: &BBox) -> Rect {
    rect_of(b.llx, b.lly, b.urx, b.ury)
}

/// `placementDeg`: `atan2(b, a)` in degrees.
fn placement_deg(m: &[f64; 6]) -> f64 {
    math::atan2(m[1], m[0]) * 180.0 / std::f64::consts::PI
}

/// `roundExtent`: projects one axis of a box, widening outward rather than letting a positive
/// extent collapse to nothing. The far edge is `round(lo) + round(hi - lo)`, not `round(hi)`,
/// which is what poppler's page-size arithmetic agrees with; and a positive extent is never
/// reported narrower than 1.
fn round_extent(lo: f64, hi: f64) -> (i64, i64) {
    let (l, h) = (round(lo), round(hi));
    if hi <= lo {
        (l, h)
    } else if l == h {
        (go_int(lo.floor()), go_int(hi.ceil()))
    } else {
        let w = round(hi - lo).max(1);
        (l, l.wrapping_add(w))
    }
}

/// `round`: `int(math.Round(v))`.
fn round(v: f64) -> i64 {
    go_int(v.round())
}

/// Go's `int(f)` on amd64: a NaN or a value outside the `int64` range converts to `MinInt64`
/// (`CVTTSD2SQ`'s "integer indefinite"), where Rust's `as` saturates.
fn go_int(f: f64) -> i64 {
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        i64::MIN
    } else {
        f as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    const PAGE_W: i64 = 612;
    const PAGE_H: i64 = 792;
    const SCAN_W: i64 = 306;
    const SCAN_H: i64 = 396;
    const TILE_W: i64 = 153;
    const BORN_DIGITAL_TEXT_CHARS: usize = 44;
    const OVERLAY_TEXT_CHARS: usize = 18;
    const INVISIBLE_TEXT_CHARS: usize = 10;
    const TRUNCATED_CONTENT_STREAM_CHARS: usize = 11;
    const DESKEW_PLACEMENT: [f64; 6] = [617.99841, -1.40220, 1.81061, 797.99795, -3.9, -3.0];
    const MIRROR_PLACEMENT: [f64; 6] = [612.0, 0.0, 0.0, -792.0, 0.0, 792.0];

    fn full_page() -> Rect {
        Rect::new(0, 0, PAGE_W, PAGE_H)
    }

    fn run(data: &[u8]) -> Result<DocumentInfo> {
        inspect(data, &Limits::default(), &AtomicBool::new(false))
    }

    /// A corpus PDF of the Go oracle (`oracle -gen`).
    fn corpus_doc(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("corpus document {path}: {e}"))
    }

    fn inspect_corpus(name: &str) -> Vec<PageInfo> {
        match run(&corpus_doc(name)) {
            Ok(di) => di.pages,
            Err(e) => panic!("inspect({name}) error = {e}"),
        }
    }

    /// `image.Rectangle.In`.
    fn is_in(r: Rect, s: Rect) -> bool {
        r.is_empty() || (s.x0 <= r.x0 && r.x1 <= s.x1 && s.y0 <= r.y0 && r.y1 <= s.y1)
    }

    #[test]
    fn severity_strings() {
        assert_eq!(Severity::Error.to_string(), "error");
        assert_eq!(Severity::Warning.to_string(), "warning");
    }

    #[test]
    fn placement_deg_is_signed_ccw() {
        assert_eq!(placement_deg(&[1.0, 0.0, 0.0, 1.0, 0.0, 0.0]), 0.0);
        assert!((placement_deg(&[0.0, 1.0, -1.0, 0.0, 0.0, 0.0]) - 90.0).abs() < 1e-12);
        assert!((placement_deg(&[0.0, -1.0, 1.0, 0.0, 0.0, 0.0]) + 90.0).abs() < 1e-12);
    }

    #[test]
    fn round_extent_widens_instead_of_collapsing() {
        // Plain rounding.
        assert_eq!(round_extent(0.0, 612.0), (0, 612));
        // Edges that round together widen outward to the containing interval.
        assert_eq!(round_extent(10.6, 10.9), (10, 11));
        assert_eq!(round_extent(10.0, 10.4), (10, 11));
        // Rounded edges on different integers but round(hi-lo) == 0: never narrower than 1.
        assert_eq!(round_extent(10.3, 10.7), (10, 11));
        // The far edge is round(lo) + round(hi-lo): 0.3 and 595.6 are 595 wide, not 596.
        assert_eq!(round_extent(0.3, 595.6), (0, 595));
        // No extent stays empty.
        assert_eq!(round_extent(5.0, 5.0), (5, 5));
        assert_eq!(round_extent(7.2, 3.0), (7, 3));
    }

    #[test]
    fn box_rect_reports_the_sub_point_stripe() {
        // `q .4 0 0 792 10 0 cm /Im0 Do Q`.
        let b = BBox {
            llx: 10.0,
            lly: 0.0,
            urx: 10.4,
            ury: 792.0,
        };
        assert_eq!(box_rect(&b), Rect::new(10, 0, 11, 792));
    }

    #[test]
    fn rect_of_a_zero_box_is_empty() {
        assert!(rect_of(0.0, 0.0, 0.0, 0.0).is_empty());
    }

    #[test]
    fn go_int_matches_amd64_conversion() {
        assert_eq!(go_int(f64::NAN), i64::MIN);
        assert_eq!(go_int(f64::INFINITY), i64::MIN);
        assert_eq!(go_int(1e30), i64::MIN);
        assert_eq!(go_int(-2.9), -2);
    }

    #[test]
    fn default_walk_caps_stay_diagnostics() {
        let q = Error::Limit {
            name: "q-depth",
            got: 2,
            max: 1,
        };
        let img = Error::Limit {
            name: "images",
            got: 2,
            max: 1,
        };
        let mut l = Limits::default();
        assert!(default_walk_cap(&q, &l));
        assert!(!default_walk_cap(&img, &l));
        l.max_gstate_depth = 4;
        assert!(!default_walk_cap(&q, &l));
        l.max_gstate_depth = 1024;
        assert!(!default_walk_cap(&q, &l));
        l.max_gstate_depth = 0;
        assert!(default_walk_cap(&q, &l));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(run(b"not a pdf at all").is_err());
        assert!(run(b"").is_err());
    }

    #[test]
    fn a_set_cancel_flag_cancels() {
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let err = inspect(b"%PDF-1.7\n", &Limits::default(), &cancel).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    // ImageRef.ColorSpace and BPC come from the image object the cross-reference table
    // resolves, not from the file's text: a decoy name in a string or a comment of the
    // dictionary, or a superseded earlier definition of the object, must not be read.
    #[test]
    fn image_ref_colour_space_is_the_parsed_object() {
        fn objs(img: &str) -> Vec<String> {
            vec![
                "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>".to_string(),
                "<< /Length 28 >>\nstream\nq 100 0 0 100 0 0 cm /Im1 Do Q\nendstream".to_string(),
                img.to_string(),
            ]
        }
        fn write(objs: &[String]) -> (Vec<u8>, usize) {
            let mut b = String::from("%PDF-1.7\n");
            let mut offs = Vec::new();
            for (i, o) in objs.iter().enumerate() {
                offs.push(b.len());
                b.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
            }
            let x = b.len();
            b.push_str(&format!(
                "xref\n0 {}\n0000000000 65535 f \n",
                objs.len() + 1
            ));
            for o in &offs {
                b.push_str(&format!("{o:010} 00000 n \n"));
            }
            b.push_str(&format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n",
                objs.len() + 1
            ));
            (b.into_bytes(), x)
        }
        fn img(extra: &str, cs: &str) -> String {
            format!(
                "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 {extra} /ColorSpace {cs} /BitsPerComponent 8 /Length 4 >>\nstream\nabcd\nendstream"
            )
        }
        let incremental = {
            let (mut b, prev) = write(&objs(&img("", "/DeviceRGB")));
            let off = b.len();
            b.extend_from_slice(
                format!("5 0 obj\n{}\nendobj\n", img("", "/DeviceCMYK")).as_bytes(),
            );
            let x = b.len();
            b.extend_from_slice(
                format!(
                    "xref\n5 1\n{off:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /Prev {prev} >>\nstartxref\n{x}\n%%EOF\n"
                )
                .as_bytes(),
            );
            b
        };
        let plain = write(&objs(&img("", "/DeviceGray"))).0;
        let string = write(&objs(&img("/Note (/ColorSpace /DeviceRGB)", "/DeviceCMYK"))).0;
        let comment = write(&objs(&img("% /ColorSpace /DeviceRGB\n", "/DeviceCMYK"))).0;
        let array = write(&objs(&img("", "[/ICCBased 1 0 R]"))).0;
        for (name, data, want) in [
            ("plain", plain, "DeviceGray"),
            ("string", string, "DeviceCMYK"),
            ("comment", comment, "DeviceCMYK"),
            ("incremental", incremental, "DeviceCMYK"),
            ("array", array, ""),
        ] {
            let di = run(&data).unwrap_or_else(|e| panic!("{name}: inspect error {e}"));
            assert_eq!(di.pages.len(), 1, "{name}");
            assert_eq!(di.pages[0].images.len(), 1, "{name}");
            let r = &di.pages[0].images[0];
            assert_eq!(r.color_space, want, "{name}: ColorSpace");
            assert_eq!(r.bpc, 8, "{name}: BPC");
        }
    }

    // The tests below read corpus PDFs that the Go internal/corpus generator makes. The
    // generator is not ported; `oracle -gen` writes them to tests/data/corpus/<name>.pdf.

    #[test]
    #[ignore = "needs tests/data/corpus/born-digital.pdf from the Go oracle -gen"]
    fn born_digital() {
        let pages = inspect_corpus("born-digital");
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!(p.index, 1);
        assert_eq!(p.bounds, full_page());
        assert!(p.images.is_empty());
        assert_eq!(p.text_chars, BORN_DIGITAL_TEXT_CHARS);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn single_image_scan() {
        let pages = inspect_corpus("scan");
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!(p.text_chars, 0);
        assert_eq!(p.images.len(), 1);
        let img = &p.images[0];
        assert_eq!(img.bounds, full_page());
        assert_eq!((img.width, img.height), (SCAN_W, SCAN_H));
        assert!(!img.bitonal, "the corpus scan is 8-bit grey");
    }

    // Bounds is the axis-aligned bounding box of the placement; Placement carries the residual
    // affine.
    #[test]
    #[ignore = "needs tests/data/corpus/{scan,scan-deskewed,scan-mirrored}.pdf from the Go oracle -gen"]
    fn reports_the_placement_matrix() {
        for (doc, want) in [
            ("scan", [612.0, 0.0, 0.0, 792.0, 0.0, 0.0]),
            ("scan-deskewed", DESKEW_PLACEMENT),
            ("scan-mirrored", MIRROR_PLACEMENT),
        ] {
            let p = inspect_corpus(doc).remove(0);
            assert_eq!(p.images.len(), 1, "{doc}");
            assert_eq!(p.images[0].placement, want, "{doc}");
        }
    }

    #[test]
    #[ignore = "needs tests/data/corpus/tiled.pdf from the Go oracle -gen"]
    fn tiled_reports_both_halves() {
        let p = inspect_corpus("tiled").remove(0);
        assert_eq!(p.images.len(), 2);
        let half = PAGE_W / 2;
        assert_eq!(p.images[0].bounds, Rect::new(0, 0, half, PAGE_H));
        assert_eq!(p.images[1].bounds, Rect::new(half, 0, PAGE_W, PAGE_H));
        for img in &p.images {
            assert_eq!((img.width, img.height), (TILE_W, SCAN_H));
        }
    }

    // Both layers of a stacked page are real placements.
    #[test]
    #[ignore = "needs tests/data/corpus/stacked.pdf from the Go oracle -gen"]
    fn stacked_reports_both_layers() {
        let p = inspect_corpus("stacked").remove(0);
        assert_eq!(p.images.len(), 2);
        for img in &p.images {
            assert_eq!(img.bounds, full_page());
        }
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan-in-form.pdf from the Go oracle -gen"]
    fn sees_through_a_form() {
        let p = inspect_corpus("scan-in-form").remove(0);
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].bounds, full_page());
    }

    // A form-borne text overlay is invisible to an image count: text_chars comes from the walk.
    #[test]
    #[ignore = "needs tests/data/corpus/overlay-text.pdf from the Go oracle -gen"]
    fn counts_text_inside_a_form() {
        let p = inspect_corpus("overlay-text").remove(0);
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.text_chars, OVERLAY_TEXT_CHARS);
    }

    // An invisible OCR layer is still text, so it keeps counting.
    #[test]
    #[ignore = "needs tests/data/corpus/invisible-text{,-in-form,-form-inherits,-bracketed}.pdf from the Go oracle -gen"]
    fn counts_invisible_text_as_text() {
        for name in [
            "invisible-text",
            "invisible-text-in-form",
            "invisible-text-form-inherits",
            "invisible-text-bracketed",
        ] {
            let p = inspect_corpus(name).remove(0);
            assert_eq!(p.text_chars, INVISIBLE_TEXT_CHARS, "{name}");
        }
    }

    #[test]
    #[ignore = "needs tests/data/corpus/overlay-vector.pdf from the Go oracle -gen"]
    fn vector_overlay_still_reports_the_image() {
        let p = inspect_corpus("overlay-vector").remove(0);
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.text_chars, 0);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/mixed.pdf from the Go oracle -gen"]
    fn multi_page() {
        let pages = inspect_corpus("mixed");
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].index, pages[1].index), (1, 2));
        assert_eq!(pages[0].text_chars, BORN_DIGITAL_TEXT_CHARS);
        assert!(pages[0].images.is_empty());
        assert_eq!(pages[1].text_chars, 0);
        assert_eq!(pages[1].images.len(), 1);
    }

    // /Rotate is a display attribute: bounds stays the MediaBox and the placement still covers
    // it.
    #[test]
    #[ignore = "needs tests/data/corpus/scan-rotated.pdf from the Go oracle -gen"]
    fn rotated_page_reports_unrotated_bounds() {
        let p = inspect_corpus("scan-rotated").remove(0);
        assert_eq!(p.rotate, 90);
        assert_eq!(p.bounds, full_page());
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].bounds, full_page());
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn page_rotate_with_no_rotate_anywhere_is_zero() {
        assert_eq!(inspect_corpus("scan")[0].rotate, 0);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/rotate-inheritance.pdf from the Go oracle -gen"]
    fn page_rotate_inherits_from_the_pages_node() {
        let pages = inspect_corpus("rotate-inheritance");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].rotate, 90, "inherited from the /Pages node");
        assert_eq!(pages[1].rotate, 180, "its own /Rotate overrides");
    }

    #[test]
    #[ignore = "needs tests/data/corpus/malformed.pdf from the Go oracle -gen"]
    fn malformed_returns_an_error() {
        assert!(run(&corpus_doc("malformed")).is_err());
    }

    // One blank page must not fail the whole document.
    #[test]
    #[ignore = "needs tests/data/corpus/blank-page.pdf from the Go oracle -gen"]
    fn blank_page_does_not_fail_the_document() {
        let pages = inspect_corpus("blank-page");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].images.len(), 1);
        assert_eq!(pages[0].images[0].bounds, full_page());
        let blank = &pages[1];
        assert_eq!(blank.index, 2);
        assert_eq!(blank.bounds, full_page());
        assert!(blank.images.is_empty());
        assert_eq!(blank.text_chars, 0);
    }

    // A content stream that did not decode is a SeverityError diagnostic, not a refused
    // document, and the other pages come back untouched.
    #[test]
    #[ignore = "needs tests/data/corpus/{corrupt-content-stream,corrupt-content-stream-in-array}.pdf from the Go oracle -gen"]
    fn records_a_corrupt_content_stream_instead_of_refusing_the_document() {
        for name in ["corrupt-content-stream", "corrupt-content-stream-in-array"] {
            let pages = inspect_corpus(name);
            assert_eq!(pages.len(), 2, "{name}");
            assert!(pages[0].diagnostics.is_empty(), "{name}: {:?}", pages[0]);
            assert_eq!(pages[0].images.len(), 1, "{name}");
            let d = &pages[1].diagnostics;
            assert_eq!(d.len(), 1, "{name}: {d:?}");
            assert_eq!(d[0].severity, Severity::Error, "{name}");
            assert!(!d[0].message.is_empty(), "{name}");
        }
    }

    // The unreadable page is still reported, in its place.
    #[test]
    #[ignore = "needs tests/data/corpus/mixed-page-two-unreadable.pdf from the Go oracle -gen"]
    fn keeps_an_unparseable_page_in_its_place() {
        let pages = inspect_corpus("mixed-page-two-unreadable");
        assert_eq!(pages.len(), 2);
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(p.index, i + 1);
        }
        assert!(!pages[1].diagnostics.is_empty());
    }

    // A page that fails PARTWAY keeps what it read: an exact count, since "more than zero" would
    // also pass if the walk kept going past the damage.
    #[test]
    #[ignore = "needs tests/data/corpus/page-two-stops-mid-stream.pdf from the Go oracle -gen"]
    fn keeps_what_it_read_before_a_page_failed() {
        let pages = inspect_corpus("page-two-stops-mid-stream");
        assert_eq!(pages.len(), 2);
        let bad = &pages[1];
        assert_eq!(bad.diagnostics.len(), 1);
        assert_eq!(bad.diagnostics[0].severity, Severity::Error);
        assert_eq!(bad.text_chars, TRUNCATED_CONTENT_STREAM_CHARS);
        assert_eq!(bad.bounds.dx(), PAGE_W);
        assert_eq!(bad.bounds.dy(), PAGE_H);
    }

    // Bitonal is the field the JBIG2 path selects on.
    #[test]
    #[ignore = "needs tests/data/corpus/jbig2.pdf from the Go oracle -gen"]
    fn reports_bitonal_for_one_bit_images() {
        let p = inspect_corpus("jbig2").remove(0);
        assert_eq!(p.images.len(), 1);
        assert!(
            p.images[0].bitonal,
            "the jbig2 document is /BitsPerComponent 1"
        );
        assert_eq!((p.images[0].width, p.images[0].height), (SCAN_W, SCAN_H));
    }

    // The bitonal base is NOT page-covering by the containment test, only by area.
    #[test]
    #[ignore = "needs tests/data/corpus/mrc-inset-base.pdf from the Go oracle -gen"]
    fn mrc_inset_base_is_short_of_the_page_box() {
        let p = inspect_corpus("mrc-inset-base").remove(0);
        assert_eq!(p.images.len(), 2);
        let (base, patch) = (&p.images[0], &p.images[1]);
        assert!(base.bitonal && !patch.bitonal);
        assert!(is_in(base.bounds, p.bounds) && base.bounds != p.bounds);
        assert!(is_in(patch.bounds, base.bounds) && patch.bounds != base.bounds);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/dup-raster.pdf from the Go oracle -gen"]
    fn dup_raster_reports_both_pages() {
        let pages = inspect_corpus("dup-raster");
        assert_eq!(pages.len(), 2);
        for p in &pages {
            assert_eq!(p.images.len(), 1, "page {}", p.index);
            assert_eq!(p.images[0].bounds, full_page(), "page {}", p.index);
        }
    }

    // Filter splits "bitonal": already JBIG2, or bitonal under another codec.
    #[test]
    #[ignore = "needs tests/data/corpus/{jbig2,scan}.pdf from the Go oracle -gen"]
    fn reports_the_declared_image_filter() {
        for (doc, want, bitonal) in [
            ("jbig2", "JBIG2Decode", true),
            ("scan", "FlateDecode", false),
        ] {
            let p = inspect_corpus(doc).remove(0);
            assert_eq!(p.images.len(), 1, "{doc}");
            assert_eq!(p.images[0].filter, want.as_bytes(), "{doc}");
            assert_eq!(p.images[0].bitonal, bitonal, "{doc}");
        }
    }

    // byb-js5.6: a page whose only raster is a BI ... EI inline image.
    #[test]
    #[ignore = "needs tests/data/corpus/inline-image-scan.pdf from the Go oracle -gen"]
    fn reports_an_inline_image() {
        let pages = inspect_corpus("inline-image-scan");
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!(p.images.len(), 1);
        let img = &p.images[0];
        assert!(img.inline);
        assert_eq!(img.bounds, full_page());
        assert!(!img.substitutable);
        assert_eq!(img.obj_nr, 0);
        assert_eq!((img.width, img.height), (0, 0));
        assert!(img.filter.is_empty());
    }

    // byb-8ly: a missing /MediaBox defaults to US Letter instead of refusing the document.
    #[test]
    #[ignore = "needs tests/data/corpus/no-media-box.pdf from the Go oracle -gen"]
    fn defaults_a_missing_media_box() {
        let pages = inspect_corpus("no-media-box");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].bounds.dx(), PAGE_W);
        assert_eq!(pages[0].bounds.dy(), PAGE_H);
        assert_eq!(pages[0].images.len(), 1);
    }
}
