//! Ports of `internal/content/walk_test.go`, `caps_test.go`, `walkctx_test.go` and
//! `quad_test.go`.
//!
//! Not ported: `TestWalkChecksGrowWithOperatorCount` counts calls of a `context.Context`'s
//! `Err`, and the cancel flag is a plain `AtomicBool` that cannot be counted; the per-token
//! check is covered by `stops_when_cancelled`.

use super::*;
use crate::error::Error;
use std::collections::HashMap;

/// A fake resource tree: one map of name to XObject per scope, plus the /ExtGState names that
/// introduce transparency (Go's `gsEnv`) and the font names that resolve (Go's `fontEnv`).
#[derive(Default)]
struct MapEnv {
    scopes: Vec<HashMap<String, XObject>>,
    transparent: Vec<&'static str>,
    fonts: HashMap<&'static str, i64>,
}

fn name(n: &[u8]) -> &str {
    std::str::from_utf8(n).unwrap_or("")
}

impl Env for MapEnv {
    fn xobject(&self, scope: i64, n: &[u8]) -> Option<XObject> {
        let scope = usize::try_from(scope).ok()?;
        self.scopes.get(scope)?.get(name(n)).cloned()
    }
    fn ext_gstate_opaque(&self, _scope: i64, n: &[u8]) -> bool {
        !self.transparent.contains(&name(n))
    }
    fn font(&self, _scope: i64, n: &[u8]) -> Option<i64> {
        self.fonts.get(name(n)).copied()
    }
}

fn img(id: i64) -> XObject {
    XObject {
        image: true,
        id,
        ..XObject::default()
    }
}

fn form(content: &str, matrix: Matrix, scope: i64) -> XObject {
    XObject {
        content: stream(content),
        matrix,
        scope,
        ..XObject::default()
    }
}

fn form_bb(content: &str, matrix: Matrix, scope: i64, bbox: BBox) -> XObject {
    XObject {
        bbox: Some(bbox),
        ..form(content, matrix, scope)
    }
}

fn env(scopes: Vec<Vec<(&str, XObject)>>) -> MapEnv {
    MapEnv {
        scopes: scopes
            .into_iter()
            .map(|s| s.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
            .collect(),
        ..MapEnv::default()
    }
}

fn image_env(id: i64) -> MapEnv {
    env(vec![vec![("Im0", img(id))]])
}

/// Go's `mapEnv{{}}`: one scope, nothing in it.
fn empty() -> MapEnv {
    env(vec![vec![]])
}

fn bx(llx: f64, lly: f64, urx: f64, ury: f64) -> BBox {
    BBox { llx, lly, urx, ury }
}

fn stream(s: &str) -> Stream {
    Arc::new(s.as_bytes().to_vec())
}

fn run_with(src: &str, e: &dyn Env, lim: &Limits) -> (Scan, Result<()>) {
    walk(&AtomicBool::new(false), &stream(src), 0, e, lim)
}

/// Walks `src` and fails the test on any error.
fn run(src: &str, e: &dyn Env) -> Scan {
    let (s, r) = run_with(src, e, &Limits::default());
    assert_eq!(r, Ok(()), "walk({src:?}) failed");
    s
}

/// The name of a limit error, or "" for anything else.
fn limit_name(r: &Result<()>) -> &'static str {
    match r {
        Err(Error::Limit { name, .. }) => name,
        _ => "",
    }
}

fn box_eq(got: &BBox, llx: f64, lly: f64, urx: f64, ury: f64) {
    const EPS: f64 = 1e-9;
    assert!(
        (got.llx - llx).abs() <= EPS
            && (got.lly - lly).abs() <= EPS
            && (got.urx - urx).abs() <= EPS
            && (got.ury - ury).abs() <= EPS,
        "box = {got:?}; want {{{llx} {lly} {urx} {ury}}}"
    );
}

fn matrix_eq(got: &Matrix, want: &Matrix) {
    for i in 0..6 {
        assert!(
            (got.0[i] - want.0[i]).abs() <= 1e-9,
            "matrix = {got:?}; want {want:?}"
        );
    }
}

#[test]
fn matrix_mul_is_apply_then_apply() {
    let scale = Matrix([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]);
    let mv = Matrix([1.0, 0.0, 0.0, 1.0, 5.0, 5.0]);
    // move first, then scale: (1,1) -> (6,6) -> (12,12)
    assert_eq!(mv.mul(&scale).apply(1.0, 1.0), (12.0, 12.0));
}

#[test]
fn matrix_unit_square_box_handles_negative_scale() {
    // A y-flip: the unit square maps to y in [-1, 0].
    box_eq(
        &Matrix([1.0, 0.0, 0.0, -1.0, 0.0, 0.0]).unit_square_box(),
        0.0,
        -1.0,
        1.0,
        0.0,
    );
}

#[test]
fn go_float_min_max() {
    assert!(fmin(f64::NAN, 1.0).is_nan() && fmax(1.0, f64::NAN).is_nan());
    assert!(fmin(0.0, -0.0).is_sign_negative() && fmin(-0.0, 0.0).is_sign_negative());
    assert!(fmax(0.0, -0.0).is_sign_positive() && fmax(-0.0, 0.0).is_sign_positive());
    assert_eq!((fmin(1.0, 2.0), fmax(1.0, 2.0)), (1.0, 2.0));
}

#[test]
fn single_image_placement() {
    let s = run("q 612 0 0 792 0 0 cm /Im0 Do Q", &image_env(7));
    assert_eq!(s.images.len(), 1, "{:?}", s.images);
    assert!(s.images[0].id == 7 && s.images[0].name == b"Im0");
    box_eq(&s.images[0].bbox, 0.0, 0.0, 612.0, 792.0);
}

#[test]
fn nested_ctm() {
    let s = run(
        "q 2 0 0 2 10 10 cm q 1 0 0 1 5 5 cm /Im0 Do Q Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    // inner translate(5,5) composed under outer scale(2) translate(10,10)
    box_eq(&s.images[0].bbox, 20.0, 20.0, 22.0, 22.0);
}

#[test]
fn q_restores_the_ctm() {
    let s = run("q 10 0 0 10 0 0 cm /Im0 Do Q /Im0 Do", &image_env(1));
    assert_eq!(s.images.len(), 2);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 10.0, 10.0);
    box_eq(&s.images[1].bbox, 0.0, 0.0, 1.0, 1.0);
}

/// An unbalanced Q must not panic or corrupt the CTM stack.
#[test]
fn unbalanced_restore() {
    let s = run("Q Q q 5 0 0 5 0 0 cm /Im0 Do Q Q Q", &image_env(1));
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 5.0, 5.0);
}

#[test]
fn counts_shown_characters() {
    let s = run(
        "BT /F1 12 Tf (Hello) Tj [ (wor) -120 (ld) ] TJ ET",
        &empty(),
    );
    assert_eq!(s.text_chars, 10);
    assert_eq!(s.text_ops, 2);
}

/// The quote operators show text too, and the double-quote form takes two numeric operands
/// before the string.
#[test]
fn counts_quote_operators() {
    let s = run("BT (ab) ' 1 2 (cde) \" ET", &empty());
    assert!(s.text_chars == 5 && s.text_ops == 2);
}

#[test]
fn recurses_into_form_xobjects() {
    let e = env(vec![
        vec![(
            "Fm0",
            form(
                "q 100 0 0 100 0 0 cm /Im0 Do Q",
                Matrix([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]),
                1,
            ),
        )],
        vec![("Im0", img(42))],
    ]);
    let s = run("q 0.5 0 0 0.5 0 0 cm /Fm0 Do Q", &e);
    assert!(
        s.images.len() == 1 && s.images[0].id == 42,
        "{:?}",
        s.images
    );
    // form /Matrix scale(2) under page CTM scale(0.5) is the identity.
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// This is the regression the whole classification design rests on: text inside a form must be
/// seen, because the page's own image count cannot see it.
#[test]
fn sees_text_inside_a_form() {
    let e = env(vec![
        vec![
            ("Im0", img(1)),
            ("Fm0", form("BT (Scanned 2026-07-27) Tj ET", IDENTITY, 1)),
        ],
        vec![],
    ]);
    let s = run("q 612 0 0 792 0 0 cm /Im0 Do Q q /Fm0 Do Q", &e);
    assert_eq!(s.images.len(), 1);
    assert!(s.text_chars == 18 && s.text_ops == 1);
}

/// Images is z-ordered: index 0 is painted first and the last index is on top. Classification
/// decides which layer is visible from that order alone, so the order has to survive a Form
/// XObject, which is the one place a walk could plausibly reorder it.
#[test]
fn images_are_in_paint_order() {
    let e = env(vec![
        vec![
            ("ImA", img(1)),
            ("Fm0", form("/ImB Do", IDENTITY, 1)),
            ("ImC", img(3)),
        ],
        vec![("ImB", img(2))],
    ]);
    let s = run("/ImA Do /Fm0 Do /ImC Do", &e);
    let ids: Vec<i64> = s.images.iter().map(|p| p.id).collect();
    assert_eq!(ids, [1, 2, 3], "the order they are painted in");
}

/// Placements are opaque by default and stop being so under an /ExtGState that introduces
/// transparency. Q restores the previous state, so the second placement here is opaque again.
#[test]
fn records_graphics_state_opacity() {
    let mut e = image_env(1);
    e.transparent = vec!["GSa"];
    let s = run("q /GSa gs /Im0 Do Q /Im0 Do", &e);
    assert_eq!(s.images.len(), 2);
    assert!(!s.images[0].opaque, "the state sets transparency");
    assert!(s.images[1].opaque, "Q restores the graphics state");
}

/// Transparency set outside a form applies to what the form paints.
#[test]
fn carries_opacity_into_forms() {
    let mut e = env(vec![
        vec![("Fm0", form("/Im0 Do", IDENTITY, 1))],
        vec![("Im0", img(1))],
    ]);
    e.transparent = vec!["GSa"];
    let s = run("q /GSa gs /Fm0 Do Q", &e);
    assert_eq!(s.images.len(), 1);
    assert!(!s.images[0].opaque, "the enclosing state is transparent");
}

/// The canonical invisible OCR layer, verbatim from ia-DTIC_ADA134285.pdf p20. Rendering mode 3
/// paints no glyphs, so this text deposits no ink at all.
#[test]
fn does_not_count_invisible_text_as_inked() {
    let s = run(
        "BT\n3 Tr /F10 1 Tf\n11.4 0 0 12 119 703.2 Tm (References)Tj\nET",
        &empty(),
    );
    assert_eq!(s.text_ops, 1, "the operator is still there");
    assert_eq!(s.inked_text_ops, 0, "3 Tr paints no glyphs");
}

#[test]
fn counts_visible_text_as_inked() {
    let s = run(
        "BT /F1 12 Tf (Hello) Tj [ (wor) -120 (ld) ] TJ ET",
        &empty(),
    );
    assert_eq!(s.inked_text_ops, 2, "mode 0 fills glyphs");
}

/// ISO 32000-1 table 106. Modes 0-2 fill and/or stroke, 4-6 do the same and also clip, 3 paints
/// nothing and 7 only clips. A mode outside the table is treated as inking, because diverting a
/// page byblos does not understand is the safe direction.
#[test]
fn ink_per_rendering_mode() {
    for (mode, inked) in [
        ("0", 1),
        ("1", 1),
        ("2", 1),
        ("3", 0),
        ("4", 1),
        ("5", 1),
        ("6", 1),
        ("7", 0),
        ("9", 1),
        ("-1", 1),
    ] {
        let s = run(&format!("BT {mode} Tr (x) Tj ET"), &empty());
        assert_eq!(s.inked_text_ops, inked, "Tr {mode}");
    }
}

/// The `3 Tr ... Tj ... 0 Tr` bracket idiom: only the mode in force at each showing operator
/// answers the question, not a per-page set of the Tr values a stream mentions.
#[test]
fn rendering_mode_is_per_operator_not_per_page() {
    let s = run("BT 3 Tr (hidden) Tj 0 Tr ET", &empty());
    assert!(s.text_ops == 1 && s.inked_text_ops == 0);

    let s = run("BT 3 Tr (hidden) Tj 0 Tr (shown) Tj ET", &empty());
    assert!(s.text_ops == 2 && s.inked_text_ops == 1);
}

/// Tr belongs to the text state, which is part of the graphics state, so Q restores it (ISO
/// 32000-1 section 8.4.2 and table 104).
#[test]
fn restores_rendering_mode_on_q() {
    let s = run("q 3 Tr BT (a) Tj ET Q BT (b) Tj ET", &empty());
    assert!(s.text_ops == 2 && s.inked_text_ops == 1);
}

/// BT/ET resets the text matrix, not the text state. A mode set in one text object is still in
/// force in the next.
#[test]
fn rendering_mode_survives_bt_et() {
    let s = run("BT 3 Tr ET BT (a) Tj ET", &empty());
    assert_eq!(s.inked_text_ops, 0, "ET does not reset Tr");
}

/// 14 files and 207 pages of Tesseract /GlyphLessFont text live inside a Form XObject. A regex
/// over the page stream sees no `3 Tr` there and scores the text visible.
#[test]
fn sees_rendering_mode_inside_a_form() {
    let e = env(vec![
        vec![("Fm0", form("BT 3 Tr (References) Tj ET", IDENTITY, 1))],
        vec![],
    ]);
    let s = run("q /Fm0 Do Q", &e);
    assert!(s.text_ops == 1 && s.inked_text_ops == 0);
}

/// A form inherits the graphics state in force at its invocation, so a mode set on the page
/// applies to text the form shows.
#[test]
fn form_inherits_rendering_mode() {
    let e = env(vec![
        vec![("Fm0", form("BT (References) Tj ET", IDENTITY, 1))],
        vec![],
    ]);
    let s = run("q 3 Tr /Fm0 Do Q", &e);
    assert!(s.text_ops == 1 && s.inked_text_ops == 0);
}

/// Do saves and restores the graphics state around a form (ISO 32000-1 section 8.10.1), so a
/// mode the form sets must not leak back onto the page.
#[test]
fn form_rendering_mode_does_not_leak() {
    let e = env(vec![
        vec![("Fm0", form("BT 3 Tr (hidden) Tj ET", IDENTITY, 1))],
        vec![],
    ]);
    let s = run("q /Fm0 Do Q BT (shown) Tj ET", &e);
    assert!(s.text_ops == 2 && s.inked_text_ops == 1);
}

/// The quote operators show text too and must be scored the same way.
#[test]
fn quote_operators_respect_rendering_mode() {
    let s = run("BT 3 Tr (ab) ' 1 2 (cde) \" ET", &empty());
    assert!(s.text_ops == 2 && s.inked_text_ops == 0);
}

#[test]
fn rejects_unbounded_form_recursion() {
    let e = env(vec![vec![("Fm0", form("/Fm0 Do", IDENTITY, 0))]]);
    let (_, r) = run_with("/Fm0 Do", &e, &Limits::default());
    assert_eq!(limit_name(&r), "form-depth", "{r:?}");
}

/// A Limits cap above [`MAX_NATIVE_DEPTH`] still stops a self-referencing form, at the native
/// depth, instead of overflowing the stack.
#[test]
fn native_depth_bounds_a_huge_form_depth_cap() {
    let e = env(vec![vec![("Fm0", form("/Fm0 Do", IDENTITY, 0))]]);
    let lim = Limits {
        max_form_depth: i64::MAX,
        max_form_invocations: i64::MAX,
        ..Limits::default()
    };
    let (_, r) = run_with("/Fm0 Do", &e, &lim);
    assert_eq!(limit_name(&r), "form-depth", "{r:?}");
}

#[test]
fn counts_painting_operators() {
    for (name, src, want) in [
        ("stroke", "72 72 468 648 re S", 1),
        ("fill", "0 0 m 10 10 l f", 1),
        ("even-odd fill", "0 0 10 10 re f*", 1),
        ("close and stroke", "0 0 m 10 10 l b", 1),
        (
            "clip then no-op paint is not painting",
            "0 0 10 10 re W n",
            0,
        ),
        (
            "construction alone is not painting",
            "0 0 m 10 10 l 20 20 l h",
            0,
        ),
    ] {
        let s = run(src, &empty());
        assert_eq!(s.paints.len(), want, "{name}: {:?}", s.paints);
    }
}

/// A paint operator with no current path marks nothing. Recording it would give it the empty
/// box, which a containment test would then have to reason about.
#[test]
fn ignores_a_paint_with_no_path() {
    let s = run("f S B n", &empty());
    assert!(s.paints.is_empty(), "{:?}", s.paints);
}

/// Where a path landed, not how many there were. Every path-construction operator takes its
/// operands as coordinate pairs, and all of them have to reach the box.
#[test]
fn records_the_path_box_in_device_space() {
    for (name, src, llx, lly, urx, ury) in [
        ("rectangle", "10 20 100 200 re f", 10.0, 20.0, 110.0, 220.0),
        ("lines", "10 10 m 50 80 l 30 5 l h f", 10.0, 5.0, 50.0, 80.0),
        (
            "cubic curve bounded by its control points",
            "0 0 m 10 90 20 90 30 0 c f",
            0.0,
            0.0,
            30.0,
            90.0,
        ),
        (
            "under a CTM",
            "q 2 0 0 3 5 7 cm 10 20 100 200 re f Q",
            25.0,
            67.0,
            225.0,
            667.0,
        ),
        // A second subpath extends the same path; both are painted by the one f.
        (
            "two subpaths",
            "0 0 10 10 re 100 100 10 10 re f",
            0.0,
            0.0,
            110.0,
            110.0,
        ),
    ] {
        let s = run(src, &empty());
        assert_eq!(s.paints.len(), 1, "{name}: {:?}", s.paints);
        box_eq(&s.paints[0].bbox, llx, lly, urx, ury);
    }
}

/// The path is reset by the operator that paints or discards it, so a second paint reports only
/// its own path.
#[test]
fn resets_the_path_after_painting() {
    let s = run("0 0 10 10 re f 100 100 10 10 re f", &empty());
    assert_eq!(s.paints.len(), 2);
    box_eq(&s.paints[0].bbox, 0.0, 0.0, 10.0, 10.0);
    box_eq(&s.paints[1].bbox, 100.0, 100.0, 110.0, 110.0);
}

/// n discards the path without painting it, and must clear it too.
#[test]
fn resets_the_path_after_no_op() {
    let s = run("0 0 500 500 re n 10 10 10 10 re f", &empty());
    assert_eq!(s.paints.len(), 1);
    box_eq(&s.paints[0].bbox, 10.0, 10.0, 20.0, 20.0);
}

/// Stroked ink spreads half the line width to either side of the path, in device space, so the
/// CTM scales the spread along with everything else.
#[test]
fn inflates_a_stroke_by_half_its_line_width() {
    let s = run("10 w 100 100 200 200 re S", &empty());
    assert_eq!(s.paints.len(), 1);
    box_eq(&s.paints[0].bbox, 95.0, 95.0, 305.0, 305.0);

    // Under a 2x CTM the same 10-point pen is 20 device points wide.
    let s = run("q 2 0 0 2 0 0 cm 10 w 100 100 200 200 re S Q", &empty());
    box_eq(&s.paints[0].bbox, 190.0, 190.0, 610.0, 610.0);

    // A fill is not inflated: ink stops at the path.
    let s = run("10 w 100 100 200 200 re f", &empty());
    box_eq(&s.paints[0].bbox, 100.0, 100.0, 300.0, 300.0);
}

/// The default line width is 1.0 (ISO 32000-1 section 8.4.3.2), not zero, so a stroke that
/// never sets one still spreads half a point.
#[test]
fn stroke_uses_the_default_line_width() {
    let s = run("100 100 200 200 re S", &empty());
    assert_eq!(s.paints.len(), 1);
    box_eq(&s.paints[0].bbox, 99.5, 99.5, 300.5, 300.5);
}

/// Index orders paints against image placements. Whether a wash is background or overlay is
/// entirely this number.
#[test]
fn orders_paints_against_placements() {
    let s = run(
        "0 0 10 10 re f q 612 0 0 792 0 0 cm /Im0 Do Q 20 20 10 10 re f",
        &image_env(1),
    );
    assert!(s.paints.len() == 2 && s.images.len() == 1);
    assert!(
        s.paints[0].index < s.images[0].index && s.images[0].index < s.paints[1].index,
        "indices are paint {}, image {}, paint {}",
        s.paints[0].index,
        s.images[0].index,
        s.paints[1].index
    );
}

/// A form's contents are painted where the Do appears, so a paint inside one must order against
/// the page's own operators, not after all of them.
#[test]
fn orders_paints_inside_a_form() {
    let e = env(vec![
        vec![
            ("Im0", img(1)),
            ("Fm0", form("0 0 10 10 re f", IDENTITY, 1)),
        ],
        vec![],
    ]);
    let s = run("/Fm0 Do q 612 0 0 792 0 0 cm /Im0 Do Q", &e);
    assert!(s.paints.len() == 1 && s.images.len() == 1);
    assert!(
        s.paints[0].index < s.images[0].index,
        "the form's fill precedes the raster"
    );
}

/// The colour a paint was issued with is recorded but not resolved: `1 1 1 scn` is white in an
/// ICCBased RGB space and nearly black in a three-ink DeviceN.
#[test]
fn records_the_colour_a_paint_was_issued_with() {
    let s = run(
        "/Cs6 cs 1 1 1 scn 0 0 0 RG 4 w 0 0 10 10 re B 0.5 g 0 0 10 10 re f",
        &empty(),
    );
    assert_eq!(s.paints.len(), 2);
    let f = &s.paints[0].fill;
    assert!(
        f.space == b"Cs6" && f.comps.len() == 3 && f.comps[0] == 1.0,
        "{f:?}"
    );
    let k = &s.paints[0].stroke;
    assert!(
        k.space == b"DeviceRGB" && k.comps.len() == 3 && k.comps[0] == 0.0,
        "{k:?}"
    );
    let g = &s.paints[1].fill;
    assert!(
        g.space == b"DeviceGray" && g.comps == [0.5],
        "second fill = {g:?}"
    );
}

/// q and Q save and restore the whole graphics state, not only the CTM. Line width is the one
/// that changes a box, so it is the one asserted on.
#[test]
fn q_restores_line_width_and_colour() {
    let s = run("2 w 0 g q 40 w 1 0 0 rg Q 100 100 200 200 re S", &empty());
    assert_eq!(s.paints.len(), 1);
    box_eq(&s.paints[0].bbox, 99.0, 99.0, 301.0, 301.0);
    assert_eq!(
        s.paints[0].fill.space, b"DeviceGray",
        "the DeviceGray in force before the q"
    );
}

#[test]
fn counts_shading_and_inline_images() {
    let s = run("/Sh0 sh BI /W 1 /H 1 /BPC 8 /CS /G ID \x00 EI", &empty());
    assert_eq!(s.shading_ops, 1);
    assert_eq!(s.inline_imgs, 1);
}

#[test]
fn records_inline_image_placements() {
    let s = run(
        "q 10 0 0 10 5 5 cm BI /W 1 /H 1 /BPC 8 /CS /G ID \x00 EI Q",
        &empty(),
    );
    assert_eq!(s.inline_images.len(), 1);
    let p = &s.inline_images[0];
    assert!(p.inline && p.name.is_empty() && p.id == 0 && p.opaque);
    box_eq(&p.bbox, 5.0, 5.0, 15.0, 15.0);
}

#[test]
fn records_unresolved_xobject_names() {
    let s = run("/Missing Do", &empty());
    assert_eq!(s.unresolved, [b"Missing".to_vec()]);
}

#[test]
fn propagates_lexer_errors() {
    let (_, r) = run_with("(unterminated", &empty(), &Limits::default());
    assert!(r.is_err(), "an unterminated string must be an error");
}

// --- clip paths (W, W*) narrow a placement's Box ----------------------------------------------

/// A clip path set with `re W n` narrows a placement that follows it to the intersection of the
/// clip and the placement's own (unclipped) box.
#[test]
fn clip_path_narrows_the_placement_box() {
    let s = run(
        "q 0 0 100 100 re W n 612 0 0 792 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// W* (even-odd clip) sets the clip from the same current path as W (ISO 32000-1 8.5.4); for
/// bounding-box purposes it must narrow a placement exactly like W does.
#[test]
fn even_odd_clip_narrows_the_placement_box() {
    let s = run(
        "q 0 0 100 100 re W* n 612 0 0 792 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// Q restores the whole graphics state (ISO 32000-1 8.4.2), the clip included.
#[test]
fn q_restores_the_clip() {
    let s = run(
        "q 0 0 50 50 re W n Q q 200 0 0 200 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 200.0, 200.0);
}

/// The mirror of `q_restores_the_clip`: an OUTER clip must survive a nested q/Q that never
/// touches it. q has to actually SAVE the clip on the stack.
#[test]
fn q_saves_the_outer_clip_across_a_nested_pair() {
    let s = run(
        "0 0 100 100 re W n q Q 200 0 0 200 0 0 cm /Im0 Do",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// A clip set inside a form must not corrupt the outer clip once control returns to the page.
#[test]
fn clip_inside_a_form_does_not_corrupt_the_outer_clip() {
    let e = env(vec![
        vec![
            ("Im0", img(1)),
            ("Fm0", form("0 0 50 50 re W n", IDENTITY, 1)),
        ],
        vec![],
    ]);
    // Outer clip 0,0,200,200; the form's own inner clip (0,0,50,50) must not retroactively
    // narrow that outer clip.
    let s = run(
        "0 0 200 200 re W n q /Fm0 Do Q 300 0 0 300 0 0 cm /Im0 Do",
        &e,
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 200.0, 200.0);
}

/// The clip a placement RECORDS must not retroactively change once a later clip op folds in.
#[test]
fn clip_does_not_retroactively_corrupt_an_already_recorded_placement() {
    let s = run(
        "0 0 200 200 re W n q 300 0 0 300 0 0 cm /Im0 Do Q 0 0 50 50 re W n 300 0 0 300 0 0 cm /Im0 Do",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 2);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 200.0, 200.0);
    box_eq(&s.images[1].bbox, 0.0, 0.0, 50.0, 50.0);
    assert_eq!(
        s.images[0].clip,
        Some(bx(0.0, 0.0, 200.0, 200.0)),
        "the second clip op must not retroactively narrow it"
    );
}

/// ISO 32000-1 7.9.5 permits a rectangle array's corners in either diagonal order.
/// `map_through` must normalize over all four mapped corners.
#[test]
fn form_bbox_with_swapped_corners_is_not_collapsed() {
    let e = env(vec![
        vec![(
            "Fm0",
            // [100 100 0 0] names the same rectangle as [0 0 100 100], corners reversed.
            form_bb(
                "q 612 0 0 792 0 0 cm /Im0 Do Q",
                IDENTITY,
                1,
                bx(100.0, 100.0, 0.0, 0.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// A form /Matrix with a rotation must go through the same four-corner normalisation as a
/// swapped-corner /BBox: mapping only the two named corners loses the box on a 90-degree turn.
#[test]
fn form_bbox_with_a_rotated_matrix_is_not_collapsed() {
    let e = env(vec![
        vec![(
            "Fm0",
            // A 90-degree rotation: [0 1 -1 0 0 0].
            form_bb(
                "q 612 0 0 792 0 0 cm /Im0 Do Q",
                Matrix([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]),
                1,
                bx(0.0, 0.0, 100.0, 100.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    // The BBox's four corners rotated 90 degrees land at (0,0), (0,100), (-100,0), (-100,100);
    // the bounding box of all four is (-100,0)-(0,100).
    box_eq(&s.images[0].bbox, -100.0, 0.0, 0.0, 100.0);
}

/// W with no current path marks nothing (ISO 32000-1 8.5.4): it must leave the running clip
/// unchanged, not collapse it to an empty box.
#[test]
fn clip_operator_with_no_current_path_does_not_clip_to_nothing() {
    for (name, src) in [
        (
            "W with nothing painted first",
            "W n 300 0 0 300 0 0 cm /Im0 Do",
        ),
        (
            "W immediately after n already cleared the path",
            "0 0 50 50 re n W n 300 0 0 300 0 0 cm /Im0 Do",
        ),
    ] {
        let s = run(src, &image_env(1));
        assert_eq!(s.images.len(), 1, "{name}");
        box_eq(&s.images[0].bbox, 0.0, 0.0, 300.0, 300.0);
    }
}

/// A form /BBox must INTERSECT the accumulated clip, not replace it.
#[test]
fn form_bbox_intersects_rather_than_replaces_the_running_clip() {
    // page clip narrower than the form BBox
    let e = env(vec![
        vec![(
            "Fm0",
            form_bb(
                "q 1000 0 0 1000 0 0 cm /Im0 Do Q",
                IDENTITY,
                1,
                bx(0.0, 0.0, 500.0, 500.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("0 0 60 60 re W n /Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 60.0, 60.0);

    // nested form BBox wider than the enclosing one
    let e = env(vec![
        vec![(
            "Fm0",
            form_bb("/Fm1 Do", IDENTITY, 1, bx(0.0, 0.0, 200.0, 200.0)),
        )],
        vec![(
            "Fm1",
            form_bb(
                "q 1000 0 0 1000 0 0 cm /Im0 Do Q",
                IDENTITY,
                2,
                bx(0.0, 0.0, 500.0, 500.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 200.0, 200.0);
}

/// Two successive `re W n` clips intersect cumulatively (ISO 32000-1 8.5.4).
#[test]
fn nested_clips_intersect() {
    let s = run(
        "0 0 100 100 re W n 20 20 60 60 re W n q 200 0 0 200 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    // intersect(0,0,100,100) with the 20,20,60,60 re (corners 20,20 to 80,80) is 20,20,80,80.
    box_eq(&s.images[0].bbox, 20.0, 20.0, 80.0, 80.0);
}

/// A clip set inside a Form XObject must not leak back out to the page that invoked it.
#[test]
fn clip_inside_a_form_does_not_leak() {
    let e = env(vec![
        vec![
            ("Im0", img(1)),
            ("Fm0", form("0 0 30 30 re W n", IDENTITY, 1)),
        ],
        vec![],
    ]);
    let s = run("/Fm0 Do q 500 0 0 500 0 0 cm /Im0 Do Q", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 500.0, 500.0);
}

/// A clip is fixed in DEVICE space at the moment W takes effect, not re-derived from its
/// user-space path later.
#[test]
fn clip_is_fixed_in_device_space_not_user_space() {
    let s = run(
        "q 2 0 0 2 0 0 cm 0 0 50 50 re W n 150 0 0 150 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 100.0, 100.0);
}

/// The clip takes effect after WHICHEVER path-painting operator follows W/W*, not just n.
#[test]
fn clip_takes_effect_after_any_painting_operator_not_just_n() {
    let s = run(
        "0 0 50 50 re W f q 200 0 0 200 0 0 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 50.0, 50.0);
}

/// A clip disjoint from a placement empties it out: the collapsing edge is clamped to the near
/// bound instead of inverting, so "empty" is an ordinary zero-area box. The clip is 0,0,10,10;
/// the image lands at 500,500,1000,1000; the pinned result is the point 500,500,500,500.
#[test]
fn disjoint_clip_reports_a_zero_area_box_not_an_inverted_one() {
    let s = run(
        "0 0 10 10 re W n q 500 0 0 500 500 500 cm /Im0 Do Q",
        &image_env(1),
    );
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 500.0, 500.0, 500.0, 500.0);
}

/// A clip must NOT narrow Paint.Box: the record stays literal.
#[test]
fn clip_does_not_narrow_paint_boxes() {
    let s = run("0 0 10 10 re W n 0 0 500 500 re f", &empty());
    assert_eq!(s.paints.len(), 1);
    box_eq(&s.paints[0].bbox, 0.0, 0.0, 500.0, 500.0);
}

/// The other half of that pair: the box is unnarrowed, and the clip that would have narrowed it
/// is recorded beside it.
#[test]
fn records_the_clip_in_force_on_a_paint() {
    let s = run("0 0 10 10 re W n 0 0 500 500 re f", &empty());
    assert_eq!(s.paints.len(), 1);
    let c = s.paints[0]
        .clip
        .expect("the 0 0 10 10 clip that was in force");
    box_eq(&c, 0.0, 0.0, 10.0, 10.0);
}

/// A path painted with nothing clipping it reports no clip, which is a different fact from a
/// clip that happens to cover everything.
#[test]
fn records_no_clip_when_none_placed() {
    let s = run("0 0 500 500 re f", &empty());
    assert_eq!(s.paints.len(), 1);
    assert_eq!(s.paints[0].clip, None);
}

/// ISO 32000-1 8.5.4: a path that paints and clips in one go is not clipped by its own W, and
/// `Paint::clip` must record the clip that was in force when the operator was issued, not the
/// one it installs. Decisive on a stroke, because `record_paint` spreads a stroke's box by half
/// the pen while the clip comes from the un-spread path.
#[test]
fn a_path_is_not_clipped_by_its_own_w() {
    // A 20pt pen down the rectangle's own edge: the ink runs from 90 to 110 on each side, while
    // the path, and so the clip it arms, is 100 to 500.
    let s = run("20 w 100 100 400 600 re W S", &empty());
    assert_eq!(s.paints.len(), 1);
    assert_eq!(
        s.paints[0].clip, None,
        "the clip in force when S was issued"
    );
    box_eq(&s.paints[0].bbox, 90.0, 90.0, 510.0, 710.0);
    let (ink, marks) = s.paints[0].ink();
    assert!(marks, "a 20pt stroke marks the page");
    box_eq(&ink, 90.0, 90.0, 510.0, 710.0);
}

/// The clip a painting operator arms still applies to everything AFTER it.
#[test]
fn a_paths_own_w_clips_what_follows() {
    let s = run("0 0 100 100 re W S\n200 200 300 300 re f", &empty());
    assert_eq!(s.paints.len(), 2);
    let c = s.paints[1]
        .clip
        .expect("the first operator's W clips what follows it");
    box_eq(&c, 0.0, 0.0, 100.0, 100.0);
    assert!(
        !s.paints[1].ink().1,
        "the second fill is clipped entirely away"
    );
}

/// A form's /BBox reaches `Paint::clip` the same way it reaches `Placement::clip`.
#[test]
fn records_a_form_bbox_as_the_paints_clip() {
    let e = env(vec![
        vec![(
            "Fm0",
            form_bb(
                "0 0 500 500 re f",
                IDENTITY,
                1,
                bx(10.0, 10.0, 100.0, 100.0),
            ),
        )],
        vec![],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.paints.len(), 1);
    let c = s.paints[0]
        .clip
        .expect("the form's /BBox clips what is painted inside it");
    box_eq(&c, 10.0, 10.0, 100.0, 100.0);
    box_eq(&s.paints[0].bbox, 0.0, 0.0, 500.0, 500.0);
    let (ink, marks) = s.paints[0].ink();
    assert!(marks, "the fill covers the whole /BBox");
    box_eq(&ink, 10.0, 10.0, 100.0, 100.0);
}

fn paint(op: &'static str, bbox: BBox, clip: Option<BBox>) -> Paint {
    Paint {
        op,
        bbox,
        fill: Color::default(),
        stroke: Color::default(),
        index: 0,
        clip,
        ctm: Matrix::default(),
    }
}

#[test]
fn paint_ink() {
    let clip = Some(bx(0.0, 0.0, 10.0, 10.0));
    for (name, p, want, marks) in [
        // A wash drawn past the page edge and clipped back to it. What it can mark is the
        // clip, not the path.
        (
            "a fill clipped back to the page",
            paint("f", bx(-9.0, -9.0, 500.0, 500.0), clip),
            bx(0.0, 0.0, 10.0, 10.0),
            true,
        ),
        (
            "an unclipped fill is its own box",
            paint("f", bx(1.0, 2.0, 3.0, 4.0), None),
            bx(1.0, 2.0, 3.0, 4.0),
            true,
        ),
        // intersect_box clamps a disjoint pair to a degenerate box rather than inverting it,
        // so a path clipped entirely away bounds no area and marks nothing.
        (
            "a fill clipped entirely away",
            paint("f", bx(100.0, 100.0, 200.0, 200.0), clip),
            bx(100.0, 100.0, 100.0, 100.0),
            false,
        ),
        // `0 842 m 0 842 l f`: a fill of a path with no extent at all.
        (
            "a fill of a path with no extent",
            paint("f", bx(0.0, 842.0, 0.0, 842.0), None),
            bx(0.0, 842.0, 0.0, 842.0),
            false,
        ),
        // The exception: a zero-area stroke box means a zero-width pen, which ISO 32000-1
        // 8.4.3.2 makes a hairline, the thinnest line the device can render, which marks.
        (
            "a zero-width stroke is a hairline, not nothing",
            paint("S", bx(0.0, 5.0, 500.0, 5.0), None),
            bx(0.0, 5.0, 500.0, 5.0),
            true,
        ),
        // A clip disjoint from the path removes the path, and the hairline exception does not
        // reach that far: there is no line left to draw.
        (
            "a stroke clipped entirely away",
            paint("S", bx(699.0, 699.0, 1501.0, 1501.0), clip),
            bx(699.0, 699.0, 699.0, 699.0),
            false,
        ),
        // The hairline again, under a clip that contains it.
        (
            "a zero-width stroke inside its clip still marks",
            paint("S", bx(0.0, 5.0, 500.0, 5.0), clip),
            bx(0.0, 5.0, 10.0, 5.0),
            true,
        ),
        // Half the pen falls outside the clip: what is left still marks.
        (
            "a stroke the clip only partly removes",
            paint("S", bx(-5.0, -5.0, 5.0, 5.0), clip),
            bx(0.0, 0.0, 5.0, 5.0),
            true,
        ),
    ] {
        let (got, m) = p.ink();
        assert_eq!(got, want, "{name}: ink box");
        assert_eq!(m, marks, "{name}: marks");
    }
}

// --- Form /BBox narrows a placement's Box ----------------------------------------------------

/// A form's /BBox crops content painted inside it, including an oversized image (ISO 32000-1
/// 8.10.2).
#[test]
fn form_bbox_crops_the_placement() {
    let e = env(vec![
        vec![(
            "Fm0",
            form_bb(
                "q 1000 0 0 1000 0 0 cm /Im0 Do Q",
                IDENTITY,
                1,
                bx(0.0, 0.0, 50.0, 50.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 50.0, 50.0);
}

/// The /BBox lives in the form's own coordinate system, which is /Matrix composed with the CTM
/// in force at the Do. Matrix translate(5,5), enclosing CTM scale(3): combined is
/// {3 0 0 3 15 15}; BBox 0,0,10,10 maps through that to 15,15,45,45.
#[test]
fn form_bbox_is_mapped_through_matrix_and_ctm() {
    let e = env(vec![
        vec![(
            "Fm0",
            form_bb(
                "q 1000 0 0 1000 0 0 cm /Im0 Do Q",
                Matrix([1.0, 0.0, 0.0, 1.0, 5.0, 5.0]),
                1,
                bx(0.0, 0.0, 10.0, 10.0),
            ),
        )],
        vec![("Im0", img(1))],
    ]);
    let s = run("q 3 0 0 3 0 0 cm /Fm0 Do Q", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 15.0, 15.0, 45.0, 45.0);
}

/// A missing form /BBox applies no BBox-derived clipping at all.
#[test]
fn missing_form_bbox_does_not_clip() {
    let e = env(vec![
        vec![("Fm0", form("q 1000 0 0 1000 0 0 cm /Im0 Do Q", IDENTITY, 1))],
        vec![("Im0", img(1))],
    ]);
    let s = run("/Fm0 Do", &e);
    assert_eq!(s.images.len(), 1);
    box_eq(&s.images[0].bbox, 0.0, 0.0, 1000.0, 1000.0);
}

/// Where the shown string starts in device space: text-space (0,0) through Trm, rise excluded.
fn show_origin(ts: &TextShow, x: f64, y: f64) {
    let (gx, gy) = ts.trm.apply(0.0, 0.0);
    assert!(
        (gx - x).abs() <= 1e-9 && (gy - y).abs() <= 1e-9,
        "origin({:?}) = ({gx}, {gy}); want ({x}, {y})",
        ts.raw.as_bytes()
    );
}

/// Hand-computed Tm/Td/TJ origins, a BT reset between two text objects, and a " with non-zero
/// Tw/Tc. Walk does not know glyph widths, so a string deposits no horizontal advance of its
/// own; the origins are computed from the positioning operators alone.
#[test]
fn tracks_text_positions() {
    let s = run(
        "BT /F1 12 Tf 50 700 Td (One) Tj [ (Two) -1000 (Three) ] TJ ET\n\
         BT /F1 10 Tf 2 0 0 2 100 500 Tm 14 TL (A) Tj 3 4 (Quote) \" ET",
        &empty(),
    );
    assert_eq!(s.text_shows.len(), 5);
    let [one, two, three, a, quote] = &s.text_shows[..] else {
        panic!("want five shows");
    };
    assert!(one.raw.as_bytes() == b"One" && one.font == b"F1" && one.size == 12.0);
    show_origin(one, 50.0, 700.0);
    // A TJ number is subtracted from the displacement, in thousandths of text space scaled by
    // the font size: -1000 moves the next string RIGHT by 12pt (ISO 32000-1 9.4.3).
    show_origin(two, 50.0, 700.0);
    show_origin(three, 62.0, 700.0);
    // BT reset the matrices; Tm then set both, and " advanced by the leading (14, doubled by
    // Tm's scale) before showing.
    assert_eq!(a.size, 10.0);
    show_origin(a, 100.0, 500.0);
    matrix_eq(&a.trm, &Matrix([2.0, 0.0, 0.0, 2.0, 100.0, 500.0]));
    assert!(
        quote.raw.as_bytes() == b"Quote" && quote.word_spacing == 3.0 && quote.char_spacing == 4.0
    );
    show_origin(quote, 100.0, 472.0);
}

/// Td, TD and T* differ only in what they do to the leading, and a walk that conflates them puts
/// every later T* line in the wrong place.
#[test]
fn td_vs_td_vs_t_star() {
    let s = run(
        "BT /F1 10 Tf 0 -20 TD (a) Tj T* (b) Tj 0 -5 Td (c) Tj T* (d) Tj ET",
        &empty(),
    );
    assert_eq!(s.text_shows.len(), 4);
    show_origin(&s.text_shows[0], 0.0, -20.0); // TD moved and set TL=20
    show_origin(&s.text_shows[1], 0.0, -40.0); // T* advanced by TL
    show_origin(&s.text_shows[2], 0.0, -45.0); // Td moved 5 but left TL=20
    show_origin(&s.text_shows[3], 0.0, -65.0); // T* still advances by 20, not 5
}

/// ' advances to the next line BEFORE showing (ISO 32000-1 9.4.3); it is not just a Tj.
#[test]
fn apostrophe_advances_before_showing() {
    let s = run("BT /F1 10 Tf 20 TL 0 -30 Td (x) ' ET", &empty());
    assert_eq!(s.text_shows.len(), 1);
    show_origin(&s.text_shows[0], 0.0, -50.0);
}

/// Positions compose: Trm is Tm composed with the CTM in force, so nesting q/cm/Q around a text
/// object multiplies through, Q restores the outer composition, and BT resets the text matrix
/// without touching the CTM.
#[test]
fn text_positions_compose_under_q_and_bt() {
    let s = run(
        "q 2 0 0 2 10 10 cm \
         BT 1 0 0 1 5 5 Tm (a) Tj ET \
         q 1 0 0 1 100 0 cm BT 1 0 0 1 5 5 Tm (b) Tj ET Q \
         BT (c) Tj ET Q \
         BT (d) Tj ET",
        &empty(),
    );
    assert_eq!(s.text_shows.len(), 4);
    let ctm1 = Matrix([2.0, 0.0, 0.0, 2.0, 10.0, 10.0]);
    let ctm2 = Matrix([1.0, 0.0, 0.0, 1.0, 100.0, 0.0]).mul(&ctm1);
    let tm = Matrix([1.0, 0.0, 0.0, 1.0, 5.0, 5.0]);
    matrix_eq(&s.text_shows[0].trm, &tm.mul(&ctm1));
    matrix_eq(&s.text_shows[1].trm, &tm.mul(&ctm2));
    matrix_eq(&s.text_shows[2].trm, &ctm1); // BT reset Tm; Q restored the CTM
    matrix_eq(&s.text_shows[3].trm, &IDENTITY);
}

/// The text state parameters live in the graphics state (ISO 32000-1 table 52), so Q restores
/// them like everything else.
#[test]
fn q_restores_text_state() {
    let s = run(
        "BT /F1 12 Tf 2 Tc 3 Tw 50 Tz 4 TL 5 Ts \
         q /F2 8 Tf 9 Tc 1 Tw 200 Tz 2 TL 0 Ts Q (a) Tj ET",
        &empty(),
    );
    assert_eq!(s.text_shows.len(), 1);
    let ts = &s.text_shows[0];
    assert!(
        ts.font == b"F1"
            && ts.size == 12.0
            && ts.char_spacing == 2.0
            && ts.word_spacing == 3.0
            && ts.hscale == 0.5
            && ts.rise == 5.0,
        "show = {ts:?}; want F1 12 Tc 2 Tw 3 Tz 50 Ts 5 restored by Q"
    );
}

#[test]
fn resolves_fonts_through_env() {
    let mut e = empty();
    e.fonts.insert("F1", 7);
    let s = run("BT /F1 12 Tf (a) Tj /F9 9 Tf (b) Tj ET", &e);
    assert_eq!(s.text_shows.len(), 2);
    assert_eq!(s.text_shows[0].font_id, 7);
    // A name the env cannot resolve still records the show, with no identity.
    assert!(s.text_shows[1].font == b"F9" && s.text_shows[1].font_id == 0);
}

/// Text shows share the painting order with images and paths, so a renderer can interleave
/// them.
#[test]
fn orders_text_against_placements() {
    let s = run("BT (a) Tj ET /Im0 Do BT (b) Tj ET", &image_env(1));
    assert!(s.text_shows.len() == 2 && s.images.len() == 1);
    assert!(
        s.text_shows[0].index < s.images[0].index && s.images[0].index < s.text_shows[1].index,
        "order = text {}, image {}, text {}",
        s.text_shows[0].index,
        s.images[0].index,
        s.text_shows[1].index
    );
}

/// A shown string without escapes aliases the stream it came from, and one with escapes is
/// decoded.
#[test]
fn shown_strings_alias_or_decode() {
    let s = run("BT (plain) Tj (a\\)b) Tj <4142> Tj ET", &empty());
    let raws: Vec<&[u8]> = s.text_shows.iter().map(|t| t.raw.as_bytes()).collect();
    assert_eq!(raws, [&b"plain"[..], &b"a)b"[..], &b"AB"[..]]);
}

// --- caps (caps_test.go) ---------------------------------------------------------------------

const MAX_TEXT_SHOWS: usize = 1 << 16;
const MAX_PAINTS: usize = 1 << 18;
const MAX_GSTATE_DEPTH: usize = 1024;
const MAX_FORM_INVOCATIONS: usize = 4096;

/// The walk caps' defaults, restated so a change to them fails here.
#[test]
fn cap_defaults() {
    let l = Limits::default();
    assert!(
        l.text_shows() == MAX_TEXT_SHOWS as i64
            && l.paints() == MAX_PAINTS as i64
            && l.gstate_depth() == MAX_GSTATE_DEPTH as i64
            && l.form_invocations() == MAX_FORM_INVOCATIONS as i64
            && l.form_depth() == 8
            && l.operands() == 8192,
        "walk cap defaults moved: {l:?}"
    );
}

#[test]
fn low_caps_refuse_by_name() {
    let form_env = env(vec![vec![("F1", form("0 0 m f ", Matrix::default(), 0))]]);
    let nested = env(vec![vec![
        ("F1", form("/F2 Do", Matrix::default(), 0)),
        ("F2", form("0 0 m f ", Matrix::default(), 0)),
    ]]);
    for (name, src, e, lim) in [
        (
            "q-depth",
            "q q q q",
            &form_env,
            Limits {
                max_gstate_depth: 3,
                ..Limits::default()
            },
        ),
        (
            "form-invocations",
            "/F1 Do /F1 Do /F1 Do",
            &form_env,
            Limits {
                max_form_invocations: 2,
                ..Limits::default()
            },
        ),
        (
            "form-depth",
            "/F1 Do",
            &nested,
            Limits {
                max_form_depth: 1,
                ..Limits::default()
            },
        ),
    ] {
        let (_, r) = run_with(src, e, &lim);
        assert_eq!(limit_name(&r), name, "{r:?}");
    }
}

/// The truncating caps keep today's behaviour at any setting: the walk stops recording and
/// carries on.
#[test]
fn low_record_caps_truncate() {
    let lim = Limits {
        max_paints: 4,
        ..Limits::default()
    };
    let (s, r) = run_with(&"0 0 m 1 1 l S\n".repeat(10), &empty(), &lim);
    assert!(
        r.is_ok() && s.paints.len() == 4,
        "paints = {}",
        s.paints.len()
    );

    let lim = Limits {
        max_text_shows: 3,
        ..Limits::default()
    };
    let src = format!("BT {}ET", "(a) Tj ".repeat(10));
    let (s, r) = run_with(&src, &empty(), &lim);
    assert!(
        r.is_ok() && s.text_shows.len() == 3 && s.text_ops == 10,
        "shows = {} ops = {}",
        s.text_shows.len(),
        s.text_ops
    );
}

/// The retained-bytes account charges Go's record sizes, so the cap trips at Go's element
/// count: 152 bytes per aliased text show.
#[test]
fn retained_bytes_trips_at_go_element_count() {
    let lim = Limits {
        max_retained_bytes: 152 * 3,
        ..Limits::default()
    };
    let src = format!("BT {}ET", "(a) Tj ".repeat(4));
    let (s, r) = run_with(&src, &empty(), &lim);
    assert_eq!(limit_name(&r), "retained-bytes", "{r:?}");
    assert_eq!(
        s.text_shows.len(),
        3,
        "the fourth show is refused before it is kept"
    );
}

/// What the scan keeps is also held to `max_alloc` (L2): 152 bytes per show, so a 300-byte
/// budget refuses the third.
#[test]
fn retained_bytes_stay_within_max_alloc() {
    let lim = Limits {
        max_alloc: 300,
        ..Limits::default()
    };
    let src = format!("BT {}ET", "(a) Tj ".repeat(4));
    let (s, r) = run_with(&src, &empty(), &lim);
    assert!(matches!(r, Err(Error::Alloc { .. })), "{r:?}");
    assert_eq!(s.text_shows.len(), 1);
}

/// A hostile many-Tj stream must not grow TextShows without bound. The counters keep counting
/// past the cap.
#[test]
fn caps_text_shows() {
    const EXTRA: usize = 5;
    let src = format!("BT {}ET", "(a) Tj ".repeat(MAX_TEXT_SHOWS + EXTRA));
    let s = run(&src, &empty());
    assert_eq!(s.text_shows.len(), MAX_TEXT_SHOWS, "the cap");
    assert!(
        s.text_ops == MAX_TEXT_SHOWS + EXTRA && s.text_chars == MAX_TEXT_SHOWS + EXTRA,
        "counting past the cap: ops {} chars {}",
        s.text_ops,
        s.text_chars
    );
}

/// `q` is two bytes and saved a gstate with no bound at all, so a content stream of nothing but
/// `q ` drove peak heap up with a nil error.
#[test]
fn refuses_deep_gstate_stack() {
    let (s, r) = run_with(
        &"q ".repeat(MAX_GSTATE_DEPTH + 1),
        &image_env(1),
        &Limits::default(),
    );
    assert_eq!(limit_name(&r), "q-depth", "{r:?}");
    // The partial scan still comes back: refusing must not become discarding.
    assert_eq!(s.images.len(), 0);
    // One below the cap is still fine, so the bound is where it says it is.
    let (_, r) = run_with(
        &"q ".repeat(MAX_GSTATE_DEPTH),
        &image_env(1),
        &Limits::default(),
    );
    assert_eq!(r, Ok(()), "within the cap");
}

/// DEPTH was bounded at 8 and WIDTH was not, so eight chained forms each invoking the next
/// `fan` times cost fan^7 walks. This is that shape in miniature: 8 levels, fan 6, which is
/// 6^7 = 279,936 walks uncapped.
#[test]
fn refuses_form_invocation_fan_out() {
    const LEVELS: usize = 8;
    const FAN: usize = 6;
    let mut forms = Vec::new();
    for i in 1..=LEVELS {
        let body = if i < LEVELS {
            format!("/F{} Do ", i + 1).repeat(FAN)
        } else {
            "0 0 m f ".to_string()
        };
        forms.push((format!("F{i}"), form(&body, Matrix::default(), 0)));
    }
    let e = MapEnv {
        scopes: vec![forms.into_iter().collect()],
        ..MapEnv::default()
    };
    let (_, r) = run_with("/F1 Do", &e, &Limits::default());
    assert_eq!(limit_name(&r), "form-invocations", "{r:?}");
}

/// A form invoked many times in a FLAT stream is legitimate (a repeated letterhead or stamp),
/// so the cap must bound the tree without refusing that.
#[test]
fn allows_flat_form_repetition() {
    let e = env(vec![vec![("F1", form("0 0 m f ", Matrix::default(), 0))]]);
    let (_, r) = run_with(
        &"/F1 Do ".repeat(MAX_FORM_INVOCATIONS),
        &e,
        &Limits::default(),
    );
    assert_eq!(r, Ok(()), "within the cap");
}

/// Scan.Paints stops growing at the cap; the walk itself carries on and reports no error.
#[test]
fn caps_recorded_paints() {
    let (s, r) = run_with(
        &"0 0 m 1 1 l S\n".repeat(MAX_PAINTS + 100),
        &NullEnv,
        &Limits::default(),
    );
    assert_eq!(r, Ok(()));
    assert_eq!(s.paints.len(), MAX_PAINTS);
}

/// Scan.Images, InlineImages and Unresolved stop the walk with a limit error instead of growing
/// without bound or silently dropping images.
#[test]
fn caps_image_lists() {
    let n = usize::try_from(Limits::default().images()).unwrap_or(0) + 1;
    for (src, cap) in [
        ("/Im0 Do\n", "images"),
        ("/Nope Do\n", "unresolved"),
        ("BI /W 1 /H 1 /BPC 8 /CS /G ID x EI\n", "inline-images"),
    ] {
        let (_, r) = run_with(&src.repeat(n), &image_env(1), &Limits::default());
        assert_eq!(limit_name(&r), cap, "{r:?}");
    }
}

// --- cancellation (walkctx_test.go) ----------------------------------------------------------

/// "q Q" is a matched save/restore pair, so the walk does real work per operator without
/// accumulating state that would blow up memory instead of time.
fn big_stream(n: usize) -> String {
    "q Q\n".repeat(n / 2)
}

/// A set flag stops the walk at the next token and the (empty) scan still comes back.
#[test]
fn stops_when_cancelled() {
    let cancel = AtomicBool::new(true);
    let (s, r) = walk(
        &cancel,
        &stream(&big_stream(100_000)),
        0,
        &NullEnv,
        &Limits::default(),
    );
    assert_eq!(r, Err(Error::Cancelled));
    assert!(s.images.is_empty());
}

/// Threading a flag must not change what a walk reports.
#[test]
fn accepts_an_unset_cancel_flag() {
    let (s, r) = run_with(&"q Q\n".repeat(10), &NullEnv, &Limits::default());
    assert_eq!(r, Ok(()));
    assert!(s.images.is_empty());
}

// --- quads (quad_test.go) --------------------------------------------------------------------

/// The exact matrix byblos's own Straighten emits for `StraightenSpec{Deg: 1.9}` on a 612x792
/// page, rotating about the page centre.
const STRAIGHTEN_MATRIX: Matrix = Matrix([611.6635, 20.2910, -26.2589, 791.5646, 13.2977, -9.9278]);

#[test]
fn unit_square_quad_axis_aligned_matches_box() {
    let m = Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]);
    let b = m.unit_square_box();
    let q = m.unit_square_quad();
    assert!(
        q.contains_box(&b, 0.0),
        "{q:?} does not contain its own box {b:?}"
    );
    // And the reverse: the box, read as a quad in the same ring order, must contain the quad:
    // an axis-aligned quad IS its bounding box.
    let box_quad = Quad([b.llx, b.lly, b.urx, b.lly, b.urx, b.ury, b.llx, b.ury]);
    assert!(box_quad.contains_quad(&q, 0.0));
}

#[test]
fn quad_contains_box_rotated_page() {
    let q = STRAIGHTEN_MATRIX.unit_square_quad();
    let page = bx(0.0, 0.0, 612.0, 792.0);
    assert!(
        !q.contains_box(&page, 1.0),
        "a 1.9deg rotated quad cannot contain the axis-aligned page it grew past"
    );
    // A box centred well inside the rotated quad, small enough that the corner triangles
    // rotation cuts off do not reach it.
    let centred = bx(200.0, 250.0, 400.0, 550.0);
    assert!(
        q.contains_box(&centred, 1.0),
        "{centred:?} should sit inside {q:?}"
    );
}

#[test]
fn quad_winding_both_mirrors() {
    let centred = bx(200.0, 250.0, 400.0, 550.0);
    for (name, m) in [
        (
            "horizontal mirror",
            Matrix([-612.0, 0.0, 0.0, 792.0, 612.0, 0.0]),
        ),
        (
            "vertical mirror",
            Matrix([612.0, 0.0, 0.0, -792.0, 0.0, 792.0]),
        ),
    ] {
        let q = m.unit_square_quad();
        assert!(q.contains_box(&centred, 1.0), "{name}: quad {q:?}");
    }
}

#[test]
fn quad_90_degree_rotation() {
    // a and d are zero, b and c are not: a clean quarter turn.
    let q = Matrix([0.0, 792.0, -612.0, 0.0, 612.0, 0.0]).unit_square_quad();
    let page = bx(0.0, 0.0, 612.0, 792.0);
    assert!(
        q.contains_box(&page, 0.0),
        "a quarter turn of a 612x792 unit square still exactly covers a 612x792 box ({q:?})"
    );
}

#[test]
fn quad_zero_determinant() {
    let q = Matrix([0.0; 6]).unit_square_quad();
    assert!(
        !q.contains_box(&bx(-1.0, -1.0, 1.0, 1.0), 100.0),
        "a collapsed quad has zero area and can contain nothing"
    );
}

/// Two coincident vertices produce a zero-length edge; `l == 0` in `contains_points` must
/// return false rather than divide by zero.
#[test]
fn quad_zero_length_edge() {
    let q = Quad([0.0, 0.0, 0.0, 0.0, 10.0, 10.0, 0.0, 10.0]);
    assert!(!q.contains_box(&bx(1.0, 1.0, 2.0, 2.0), 0.0));
}

#[test]
fn quad_tolerance_at_the_boundary() {
    let q = Matrix([612.0, 0.0, 0.0, 792.0, 0.0, 0.0]).unit_square_quad();
    // A point exactly 2 points outside the right edge: s*cross/l == -2, and the algorithm's own
    // comparison is the strict "< -tol", so tol==2 must pass containment rather than reject it.
    let outside = bx(0.0, 0.0, 614.0, 792.0);
    assert!(
        q.contains_box(&outside, 2.0),
        "boundary is inclusive: < -tol, not <="
    );
    assert!(!q.contains_box(&outside, 1.999));
}

/// `Matrix{300,300,600,600,0,0}` has determinant 0, but its four vertices are NOT coincident:
/// they are four distinct points on the line y=x. Only the shoelace-area guard stands between
/// this and treating a line as an unbounded strip.
#[test]
fn quad_collinear_vertices_rejects_far_point() {
    let q = Matrix([300.0, 300.0, 600.0, 600.0, 0.0, 0.0]).unit_square_quad();
    assert_eq!(q.shoelace(), 0.0, "test setup");
    let far = bx(5000.0, 5000.0, 5001.0, 5001.0);
    assert!(!q.contains_box(&far, 100.0));
}

/// `Quad{0,0, 0,0, 10,0, 5,10}` repeats its first vertex, so edge 0 has zero length, but the
/// other three edges still bound a genuine triangle, and (5,3) sits inside it. Only the `l == 0`
/// guard can reject this point.
#[test]
fn quad_zero_length_edge_rejects_point_inside_triangle() {
    let q = Quad([0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 5.0, 10.0]);
    assert_ne!(q.shoelace(), 0.0, "test setup");
    assert!(!q.contains_box(&bx(5.0, 3.0, 5.0, 3.0), 0.0));
}

/// `tol` is a hair above zero rather than exactly zero: every vertex sits on its own boundary,
/// and the float noise of the matrix multiplications that built the quad can trip a strict
/// "< 0" comparison.
#[test]
fn quad_contains_quad_itself() {
    let q = STRAIGHTEN_MATRIX.unit_square_quad();
    assert!(q.contains_quad(&q, 1e-6));
}
