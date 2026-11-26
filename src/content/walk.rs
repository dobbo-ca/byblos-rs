//! The content-stream walker: port of `internal/content/walk.go`.
//!
//! [`walk`] interprets a decoded content stream, resolving resource names through an [`Env`] and
//! recursing into Form XObjects. It records image placements, inline images, paths, shown
//! strings and counters in a [`Scan`]. The geometry types ([`Matrix`], [`BBox`], [`Quad`]) live
//! here too. Go's `Box` is [`BBox`] (it would shadow `std::boxed::Box`).
//!
//! Differences from Go, all in how Rust owns data:
//!
//! * The cancel flag replaces Go's `context.Context`; it is checked once per token.
//! * The caps come from the `limits` argument. Go reads them from an optional `Limits()` method
//!   of the `Env`.
//! * Names (resource names, colour space names) are byte strings, as in Go and pdf-rs.
//! * A decoded stream is an `Arc<Vec<u8>>` ([`Stream`]), so that a shown string without escapes
//!   can alias the stream's buffer ([`Raw`]) as it does in Go: every draw of one form shares the
//!   form's one buffer.
//! * Form recursion is also capped at [`MAX_NATIVE_DEPTH`] levels, whatever `max_form_depth`
//!   says, because Rust's call stack is fixed where Go's grows.
//! * Go converts a float to `int` for `Tr`; out of range (or NaN) amd64 gives `i64::MIN`, which
//!   is reproduced explicitly because Rust's `as` saturates.

use super::lexer::{Kind, Lexer, Token};
use crate::error::Result;
use crate::go::math::hypot;
use crate::limits::{self, Limits, check_cancel};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// A decoded content stream, shareable so shown strings can alias it.
pub type Stream = Arc<Vec<u8>>;

/// The most nested Form XObject walks, whatever `Limits::max_form_depth` is.
pub const MAX_NATIVE_DEPTH: i64 = 64;

/// Go's `min` builtin on floats: NaN wins, and -0 is less than +0.
fn fmin(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b { a } else { b }
}

/// Go's `max` builtin on floats: NaN wins, and +0 is greater than -0.
fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b { a } else { b }
}

/// A PDF transformation matrix `[a b c d e f]` in the row-vector convention of ISO 32000-1
/// section 8.3.3:
///
/// ```text
/// [ a b 0 ]
/// [ c d 0 ]
/// [ e f 1 ]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Matrix(pub [f64; 6]);

/// The identity transform.
pub const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

impl Matrix {
    /// `Mul`: the matrix that applies `self` first and then `n`.
    pub fn mul(&self, n: &Matrix) -> Matrix {
        let (m, n) = (&self.0, &n.0);
        Matrix([
            m[0] * n[0] + m[1] * n[2],
            m[0] * n[1] + m[1] * n[3],
            m[2] * n[0] + m[3] * n[2],
            m[2] * n[1] + m[3] * n[3],
            m[4] * n[0] + m[5] * n[2] + n[4],
            m[4] * n[1] + m[5] * n[3] + n[5],
        ])
    }

    /// Maps a point through the matrix.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let m = &self.0;
        (x * m[0] + y * m[2] + m[4], x * m[1] + y * m[3] + m[5])
    }

    /// The bounding box of the unit square mapped through the matrix. An image XObject always
    /// occupies the unit square in its own space (ISO 32000-1 section 8.9.5.2), so this is
    /// exactly where the image lands.
    pub fn unit_square_box(&self) -> BBox {
        let (x0, y0) = self.apply(0.0, 0.0);
        let (x1, y1) = self.apply(1.0, 0.0);
        let (x2, y2) = self.apply(0.0, 1.0);
        let (x3, y3) = self.apply(1.0, 1.0);
        BBox {
            llx: fmin(fmin(x0, x1), fmin(x2, x3)),
            lly: fmin(fmin(y0, y1), fmin(y2, y3)),
            urx: fmax(fmax(x0, x1), fmax(x2, x3)),
            ury: fmax(fmax(y0, y1), fmax(y2, y3)),
        }
    }

    /// The unit square's four corners mapped through the matrix, in ring order. `unit_square_box`
    /// is the same mapping with the ring order lost to an axis-aligned box.
    pub fn unit_square_quad(&self) -> Quad {
        let (x0, y0) = self.apply(0.0, 0.0);
        let (x1, y1) = self.apply(1.0, 0.0);
        let (x2, y2) = self.apply(1.0, 1.0);
        let (x3, y3) = self.apply(0.0, 1.0);
        Quad([x0, y0, x1, y1, x2, y2, x3, y3])
    }
}

/// An axis-aligned rectangle in PDF user space: points, origin lower-left, y increasing upward
/// (Go's `Box`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BBox {
    pub llx: f64,
    pub lly: f64,
    pub urx: f64,
    pub ury: f64,
}

impl BBox {
    /// The axis-aligned device-space bounding box of this box's four corners mapped through
    /// `m`. Unlike `unit_square_box`, the box need not be the unit square: a form's /BBox is an
    /// arbitrary rectangle in its own space.
    pub fn map_through(&self, m: &Matrix) -> BBox {
        let (x0, y0) = m.apply(self.llx, self.lly);
        let (x1, y1) = m.apply(self.urx, self.lly);
        let (x2, y2) = m.apply(self.llx, self.ury);
        let (x3, y3) = m.apply(self.urx, self.ury);
        BBox {
            llx: fmin(fmin(x0, x1), fmin(x2, x3)),
            lly: fmin(fmin(y0, y1), fmin(y2, y3)),
            urx: fmax(fmax(x0, x1), fmax(x2, x3)),
            ury: fmax(fmax(y0, y1), fmax(y2, y3)),
        }
    }
}

/// A quadrilateral's four vertices, x then y, in ring order: (0,0) (1,0) (1,1) (0,1) mapped
/// through some matrix. Unlike [`BBox`], it is not axis-aligned: a rotated matrix leaves it a
/// true parallelogram rather than collapsing it to a bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Quad(pub [f64; 8]);

impl Quad {
    /// Twice the quad's signed area: positive for a counter-clockwise ring, negative for
    /// clockwise, zero for a degenerate (collinear or zero-area) one.
    pub fn shoelace(&self) -> f64 {
        let q = &self.0;
        let mut two_a = 0.0;
        for i in 0..4 {
            let j = (i + 1) % 4;
            two_a += q[2 * i] * q[2 * j + 1] - q[2 * j] * q[2 * i + 1];
        }
        two_a
    }

    /// Whether every point in `pts` (x,y pairs) lies inside the quad, allowing each up to `tol`
    /// points of perpendicular distance outside an edge. It is the one half-plane test the box
    /// and quad containment wrappers build on.
    ///
    /// A zero-area quad (collinear or coincident vertices) contains nothing: at page scale
    /// |2*area| is on the order of 1e6 pt^2, so an exact-zero result is float noise only when
    /// the quad was already degenerate, and false matches the package's other degenerate
    /// guards.
    ///
    /// The winding direction is measured, not assumed. A reflected matrix (a mirrored
    /// placement) produces a clockwise ring, and testing against a fixed counter-clockwise
    /// assumption would invert every mirror's containment answer.
    fn contains_points(&self, pts: &[f64], tol: f64) -> bool {
        let q = &self.0;
        let two_a = self.shoelace();
        if two_a == 0.0 {
            return false;
        }
        let s = if two_a < 0.0 { -1.0 } else { 1.0 };
        for i in 0..4 {
            let j = (i + 1) % 4;
            let (px, py) = (q[2 * i], q[2 * i + 1]);
            let (ex, ey) = (q[2 * j] - px, q[2 * j + 1] - py);
            let l = hypot(ex, ey);
            if l == 0.0 {
                return false;
            }
            for p in pts.chunks_exact(2) {
                let (tx, ty) = (p[0], p[1]);
                let cross = ex * (ty - py) - ey * (tx - px);
                // Dividing by l turns cross (a scaled area) into a perpendicular distance in
                // points, which is what makes tol mean points rather than something that
                // scales with the edge's length.
                if s * cross / l < -tol {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `b`'s four corners all lie inside the quad, within `tol` points.
    pub fn contains_box(&self, b: &BBox, tol: f64) -> bool {
        self.contains_points(
            &[b.llx, b.lly, b.urx, b.lly, b.urx, b.ury, b.llx, b.ury],
            tol,
        )
    }

    /// Whether `o`'s four vertices all lie inside the quad, within `tol` points.
    pub fn contains_quad(&self, o: &Quad, tol: f64) -> bool {
        self.contains_points(&o.0, tol)
    }
}

/// A resolved /XObject resource. `id` is caller-assigned identity echoed back in
/// [`Placement::id`]; pdfdoc uses the PDF object number, so that an image named Im0 inside a
/// form is never confused with the page's own Im0. The matrix defaults to all zeros, as Go's
/// zero value does: a form sets it (`IDENTITY` when the form has no /Matrix).
#[derive(Debug, Clone, Default)]
pub struct XObject {
    pub image: bool,
    pub id: i64,
    /// Form only: the decoded content stream.
    pub content: Stream,
    /// Form only: its /Matrix, `IDENTITY` when absent.
    pub matrix: Matrix,
    /// Form only: the scope handle for its own resources.
    pub scope: i64,
    /// A form's /BBox, in the form's own coordinate system, or `None` when the form has none
    /// (malformed, or unread by the caller). `None` applies no BBox-derived clipping at all:
    /// it does not mean "clip to nothing".
    pub bbox: Option<BBox>,
}

/// Resolves resource names encountered during a walk (Go's `Env` interface). Scopes are opaque
/// handles into the caller's resource tree; the caller chooses the numbering and [`walk`] only
/// passes them back.
pub trait Env {
    /// The named /XObject in `scope`, or `None` when it does not resolve.
    fn xobject(&self, scope: i64, name: &[u8]) -> Option<XObject>;

    /// Whether the named /ExtGState leaves painting fully opaque: no /ca or /CA below 1 and no
    /// soft mask. A name that does not resolve must be reported as not opaque: a walk cannot
    /// vouch for a dictionary it could not read.
    fn ext_gstate_opaque(&self, scope: i64, name: &[u8]) -> bool;

    /// Resolves the named /Font resource to a caller-assigned identity, echoed back in
    /// [`TextShow::font_id`]; pdfdoc uses the PDF object number. A name that does not resolve
    /// reports `None`, and shows made with it carry font id 0: the show is still recorded, so
    /// "text whose font I could not read" stays visible rather than silently empty.
    fn font(&self, scope: i64, name: &[u8]) -> Option<i64>;
}

/// An [`Env`] that resolves nothing, for a walk of a stream with no resources (Go passes a nil
/// `Env`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullEnv;

impl Env for NullEnv {
    fn xobject(&self, _scope: i64, _name: &[u8]) -> Option<XObject> {
        None
    }
    fn ext_gstate_opaque(&self, _scope: i64, _name: &[u8]) -> bool {
        false
    }
    fn font(&self, _scope: i64, _name: &[u8]) -> Option<i64> {
        None
    }
}

/// One painting of an image XObject, or of an inline image (see `inline`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Placement {
    /// Resource name at the point of use, for diagnostics.
    pub name: Vec<u8>,
    pub id: i64,
    pub ctm: Matrix,
    pub bbox: BBox,
    /// A `BI ... ID ... EI` operator rather than a `Do` naming an image XObject. An inline image
    /// has no resource name and no object number (`name` is empty and `id` is 0), because
    /// ISO 32000-1 8.9.7 embeds it in the content stream itself; the box is still the CTM's unit
    /// square, the same convention 8.9.5.1 gives an XObject image.
    pub inline: bool,
    /// The clip in effect at the moment of painting (the running intersection of every `W/W* n`
    /// path and Form /BBox on the graphics state, in device space), or `None` when nothing
    /// clipped this placement. `bbox` is already the clip intersected with
    /// `ctm.unit_square_box()`, so a caller wanting to know whether the clip narrowed anything
    /// compares `bbox` against that.
    pub clip: Option<BBox>,
    /// The graphics state at the moment of painting, and nothing else: no /ca, /CA or soft mask
    /// was in effect. The image's own /SMask, /Mask and /ImageMask are dictionary facts a walk
    /// never sees.
    pub opaque: bool,
    /// This painting's position in the page's painting order, counted across images, paths and
    /// shown strings together.
    pub index: i64,
}

/// The colour a paint operator was issued with, exactly as the content stream stated it: the
/// operands of the last colour operator, and the name of the colour space when `cs` or `CS`
/// named one. It is deliberately not resolved to device values (see Go's note on `Color`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Color {
    /// `DeviceGray`, `DeviceRGB`, `DeviceCMYK`, or the cs/CS resource name.
    pub space: Vec<u8>,
    /// As written; empty when a space was named but no colour set.
    pub comps: Vec<f64>,
}

/// One path-painting operator: where its path landed in device space, and the colours in force
/// when it was issued.
#[derive(Debug, Clone, PartialEq)]
pub struct Paint {
    /// The painting operator, for diagnostics.
    pub op: &'static str,
    /// Device-space bounds of the path, stroke spread included.
    pub bbox: BBox,
    pub fill: Color,
    pub stroke: Color,
    /// Painting order across the page, shared with [`Placement::index`].
    pub index: i64,
    /// The clip in effect at the moment of painting, in device space, or `None` when nothing
    /// clipped this path. It is the same running intersection [`Placement::clip`] records,
    /// captured at the path's own moment rather than the raster's. `bbox` is deliberately NOT
    /// narrowed by it; see [`Paint::ink`].
    pub clip: Option<BBox>,
    /// The graphics-state matrix in effect when the path was painted. `bbox` is only ever an
    /// axis-aligned bounding box of the path's true (possibly rotated) shape, so a caller
    /// comparing it against a rotated placement's true quadrilateral needs to know whether this
    /// path shares that rotation.
    pub ctm: Matrix,
}

/// The painting operators that lay ink along the path, as well as or instead of inside it.
fn is_stroking_op(op: &str) -> bool {
    matches!(op, "S" | "s" | "B" | "B*" | "b" | "b*")
}

impl Paint {
    /// Whether this operator lays ink along the path as well as, or instead of, inside it. A
    /// stroke's `bbox` includes ink the path itself does not bound, while a fill's `bbox` is
    /// the path.
    pub fn strokes(&self) -> bool {
        is_stroking_op(self.op)
    }

    /// The part of the path that can deposit marks: `bbox` intersected with the clip that was in
    /// force, and whether anything is left.
    ///
    /// A clip DISJOINT from the path removes the path entirely, whatever operator painted it.
    /// Nothing is left to mark, so a stroke gets no exception here.
    ///
    /// A path that bounds no area on its OWN is a degenerate fill (`0 842 m 0 842 l f`) and
    /// marks nothing either. The exception is a stroke: `record_paint` has already spread a
    /// stroke's box by half the line width, so a stroked path is zero-area only when the pen is
    /// zero wide, and ISO 32000-1 8.4.3.2 makes THAT the thinnest line the device can render, a
    /// hairline, which marks.
    ///
    /// Testing disjointness against `bbox` rather than against the intersection is what keeps
    /// the two apart: `intersect_box` clamps a disjoint pair to a degenerate box, so by the time
    /// it has run, "clipped away" and "a hairline" look identical.
    pub fn ink(&self) -> (BBox, bool) {
        let b = self.bbox;
        let area = |x: &BBox| x.urx > x.llx && x.ury > x.lly;
        let Some(c) = self.clip else {
            return (b, self.strokes() || area(&b));
        };
        if c.urx <= b.llx || c.llx >= b.urx || c.ury <= b.lly || c.lly >= b.ury {
            return (intersect_box(&c, &b), false);
        }
        let ink = intersect_box(&c, &b);
        (ink, self.strokes() || area(&ink))
    }
}

/// A shared byte string: a slice of a decoded stream's buffer when the shown string had no
/// escapes (every draw of one form shares the form's one buffer), else its own bytes.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    buf: Stream,
    start: usize,
    end: usize,
}

impl Raw {
    /// A string with its own bytes.
    pub fn owned(v: Vec<u8>) -> Self {
        let end = v.len();
        Raw {
            buf: Arc::new(v),
            start: 0,
            end,
        }
    }

    /// The bytes `start..end` of `buf`.
    pub fn alias(buf: &Stream, start: usize, end: usize) -> Self {
        Raw {
            buf: Arc::clone(buf),
            start,
            end,
        }
    }

    /// The aliased bytes (empty if the range is out of bounds).
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(self.start..self.end).unwrap_or_default()
    }
}

impl PartialEq for Raw {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

/// One string shown by `Tj`, `TJ`, `'` or `"`: the bytes exactly as stored, and the text state
/// in force when they were shown. The walk decodes nothing; it only records where each string
/// started and with what parameters.
///
/// `trm` is the text matrix composed with the CTM at the START of this string: text-space (0,0)
/// through `trm` is the string's origin. It deliberately does not fold in `size`, `hscale` or
/// `rise`; a consumer wanting the full rendering matrix of ISO 32000-1 9.4.4 builds it from
/// those fields. The walk advances the text matrix through every positioning operator and every
/// TJ numeric adjustment (which is why each element of a TJ carries its own `trm`) but NOT
/// through the glyphs of the string itself: glyph advances need the font's width tables, which
/// live behind `font_id`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextShow {
    /// String bytes as stored, undecoded.
    pub raw: Raw,
    /// Tf resource name, empty when no Tf was seen.
    pub font: Vec<u8>,
    /// [`Env::font`]'s identity for it, 0 when unresolved.
    pub font_id: i64,
    /// Tf's size operand (Tfs).
    pub size: f64,
    /// Tc, in unscaled text space units.
    pub char_spacing: f64,
    /// Tw.
    pub word_spacing: f64,
    /// Tz / 100, initially 1.
    pub hscale: f64,
    /// Ts.
    pub rise: f64,
    /// Rendering mode (Tr); see `inks_glyphs`.
    pub mode: i64,
    pub trm: Matrix,
    /// Painting order, shared with [`Placement`] and [`Paint`].
    pub index: i64,
}

/// What a content-stream walk observed, including everything reached through Form XObjects.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// In paint order: index 0 is painted first and the last index lands on top. Form XObjects
    /// are walked where they are invoked, so a placement inside a form sits between the
    /// placements around the `Do` that reached it.
    pub images: Vec<Placement>,
    /// A `BI ... EI` placement per inline image, kept separate from `images` so the callers
    /// that treat `images` as "the page's image XObjects" are unaffected. Inspect merges the
    /// two back into paint order via `index`.
    pub inline_images: Vec<Placement>,
    /// Bytes shown by Tj, TJ, ' and ".
    pub text_chars: usize,
    /// Number of text-showing operators.
    pub text_ops: usize,
    /// Only the text-showing operators that were in a rendering mode which actually paints
    /// glyphs. Almost all text on a scanned page is an invisible OCR layer (3 Tr) and deposits
    /// nothing; classification wants this count, not `text_ops`.
    pub inked_text_ops: usize,
    /// In paint order too, one entry per string shown (a TJ contributes one per string element,
    /// because the numeric adjustments between them move the text matrix). The counters above
    /// are unchanged by it. Capped at `Limits::max_text_shows`; the counters keep counting past
    /// the cap.
    pub text_shows: Vec<TextShow>,
    /// In paint order too, interleaving with `images` through the shared index. Clipping alone
    /// does not appear here.
    pub paints: Vec<Paint>,
    /// `sh`.
    pub shading_ops: usize,
    /// `BI ... EI`.
    pub inline_imgs: usize,
    /// `Do` operands that did not resolve.
    pub unresolved: Vec<Vec<u8>>,

    /// Counts painting events, so that `Paint::index` and `Placement::index` can be compared.
    order: i64,
    /// The bytes the walk has charged for what it keeps (`keep`).
    retained: i64,
    /// Form XObject invocations across the WHOLE page, not per nesting level, which is why it
    /// lives here: a form is a fresh walk and the count has to survive that.
    forms: i64,
    lim: Limits,
}

// Element sizes the retained-bytes account charges: Go's 64-bit `unsafe.Sizeof` of each record,
// as constants, so `max_retained_bytes` trips at the same element count as in Go. Placement is
// 136 (string 16, int 8, [6]f64 48, Box 32, bool padded to 8, *Box 8, bool padded to 8, int 8),
// Paint 192, TextShow 152, Box 32, string 16.
const TEXT_SHOW_BYTES: i64 = 152;
const PAINT_BYTES: i64 = 192;
const PLACEMENT_BYTES: i64 = 136;
const BOX_BYTES: i64 = 32;
const STRING_BYTES: i64 = 16;

impl Scan {
    /// Charges `n` bytes the walk is about to retain against the page's one account,
    /// `Limits::max_retained_bytes`. Every record appended to the scan and every slice or string
    /// it holds goes through here before it is kept; past the cap the walk stops with a
    /// permanent limit error. A slice that aliases a buffer the page already holds (a lexed
    /// string without escapes) costs only its record.
    fn keep(&mut self, n: i64) -> Result<()> {
        self.retained += n;
        limits::check("retained-bytes", self.retained, self.lim.retained_bytes())?;
        // What the scan keeps is heap too: it may not outgrow `max_alloc` (a no-op at the
        // defaults, where the retained cap is the smaller). The doc ledger itself is not shared
        // with the walk, so this bounds the scan alone.
        let max = self.lim.alloc_limit();
        if self.retained as u64 > max {
            return Err(crate::Error::Alloc {
                got: self.retained as u64,
                max,
            });
        }
        Ok(())
    }
}

/// What a colour holds beyond its record: the space name and the component array, which cs and
/// sc build fresh on every walk of a form.
fn color_bytes(c: &Color) -> i64 {
    c.space.len() as i64 + 8 * c.comps.len() as i64
}

/// A record's size plus the clip box it points at, which can be its own.
fn with_clip(n: i64, clip: &Option<BBox>) -> i64 {
    if clip.is_some() { n + BOX_BYTES } else { n }
}

/// `append` with a fallible allocation.
fn push<T>(v: &mut Vec<T>, x: T) -> Result<()> {
    v.try_reserve(1)?;
    v.push(x);
    Ok(())
}

/// The part of the PDF graphics state a walk tracks. q and Q save and restore it as a unit,
/// which is what the spec says and what a separate CTM stack would get wrong for opacity.
///
/// Known simplification: opacity only ever falls within a q...Q pair. An /ExtGState that
/// restores /ca to 1 after an earlier one lowered it leaves the state reported as not opaque, so
/// a page doing that diverts. No producer has been seen to do it, and the error is in the safe
/// direction.
///
/// `tr` is the text rendering mode. It lives in the text state (table 104), which is part of
/// the graphics state, so BT/ET does not reset it: BT resets the text matrix only. A Form
/// XObject inherits it at the Do and discards it on return, which passing the state by value
/// into the recursion gives for free.
///
/// `clip` is the running intersection of every clip path (W/W* n) and Form /BBox in effect, in
/// device space, or `None` when nothing has clipped yet. The text state (Tf, Tc, Tw, Tz, TL,
/// Ts) is part of the graphics state too (table 52). The text matrix and line matrix are NOT:
/// the spec scopes them to a BT...ET pair. They live here anyway, because pass-by-value
/// recursion into forms and q/Q nesting then handle them for free (known simplification: a
/// stream that runs `Tm ... q ... Td ... Q` INSIDE one text object gets the Td undone by the Q).
///
/// The colours and the font name are `Rc`, so a q pushes a handle where Go pushes a slice or
/// string header.
#[derive(Clone)]
struct GState {
    ctm: Matrix,
    opaque: bool,
    tr: i64,
    line_width: f64,
    fill: Rc<Color>,
    stroke: Rc<Color>,
    clip: Option<BBox>,

    /// Text matrix and line matrix; BT resets both.
    tm: Matrix,
    tlm: Matrix,
    /// Tf resource name.
    font: Rc<[u8]>,
    /// [`Env::font`]'s identity for `font`, 0 when unresolved.
    font_id: i64,
    /// Tfs.
    font_size: f64,
    /// Tc.
    char_sp: f64,
    /// Tw.
    word_sp: f64,
    /// Tz / 100, initially 1.
    hscale: f64,
    /// TL.
    leading: f64,
    /// Ts.
    rise: f64,
}

impl GState {
    /// ISO 32000-1 section 8.4.1's initial graphics state, for the parts tracked here. The line
    /// width matters: a stroke that never sets one still spreads half a point either side of
    /// its path, not nothing.
    fn initial() -> Self {
        GState {
            ctm: IDENTITY,
            opaque: true,
            tr: 0,
            line_width: 1.0,
            fill: Rc::default(),
            stroke: Rc::default(),
            clip: None,
            tm: IDENTITY,
            tlm: IDENTITY,
            font: Rc::from(&b""[..]),
            font_id: 0,
            font_size: 0.0,
            char_sp: 0.0,
            word_sp: 0.0,
            hscale: 1.0,
            leading: 0.0,
            rise: 0.0,
        }
    }

    /// Moves to the start of the next line, offset by (tx, ty) from the start of the current
    /// one: Td, TD, T*, and the implicit advance of ' and " (ISO 32000-1 9.4.2). The line
    /// matrix moves and the text matrix follows it.
    fn next_line(&mut self, tx: f64, ty: f64) {
        self.tlm = Matrix([1.0, 0.0, 0.0, 1.0, tx, ty]).mul(&self.tlm);
        self.tm = self.tlm;
    }
}

/// Accumulates the device-space bounds of the path under construction.
///
/// A curve contributes its control points rather than the curve itself. A Bezier lies inside
/// the convex hull of its control points, so this can only over-estimate, and over-estimating
/// makes a page divert rather than extract, the safe direction for a rule whose job is to prove
/// a paint is hidden.
#[derive(Default)]
struct PathBox {
    bbox: BBox,
    set: bool,
}

impl PathBox {
    fn add(&mut self, (x, y): (f64, f64)) {
        if !self.set {
            self.bbox = BBox {
                llx: x,
                lly: y,
                urx: x,
                ury: y,
            };
            self.set = true;
            return;
        }
        self.bbox.llx = fmin(self.bbox.llx, x);
        self.bbox.lly = fmin(self.bbox.lly, y);
        self.bbox.urx = fmax(self.bbox.urx, x);
        self.bbox.ury = fmax(self.bbox.ury, y);
    }

    fn reset(&mut self) {
        *self = PathBox::default();
    }
}

/// Whether text shown in rendering mode `tr` deposits any ink. ISO 32000-1 table 106: 3 is
/// invisible and 7 adds to the clipping path without painting; every other listed mode fills,
/// strokes, or both. A mode outside the table is treated as inking, because diverting a page
/// byblos does not understand is the safe direction.
fn inks_glyphs(tr: i64) -> bool {
    tr != 3 && tr != 7
}

/// Interprets a decoded content stream, resolving resource names in `scope` through `env`. The
/// caps come from `limits`.
///
/// The Form XObject /BBox, mapped through its /Matrix and the enclosing CTM, and any clip path
/// set with W/W* n, narrow every placement painted inside them to the intersection with the
/// clip. `Paint::bbox` is never narrowed by a clip: the clip in force is recorded beside it
/// (`Paint::clip`) and `Paint::ink` composes the two, so the record stays literal and the
/// visibility question is answered where it is asked.
///
/// Known simplification: a text clipping mode (4 Tr through 7 Tr) adds glyph outlines to the
/// clipping path, but the walk only tracks the mode for `inks_glyphs`; it never folds a text clip
/// into the clip. This overstates what is visible, the same safe direction as every other gap.
///
/// `cancel` is checked at the operator loop boundary, once per token, because the pathological
/// stream this bounds is pathological in tokens: a stream of four million operands with no
/// operator would otherwise be uninterruptible.
///
/// THE SCAN IS ALWAYS RETURNED, error or not. A content stream that stops half-way still
/// describes the part of the page it reached, and that partial answer is what poppler paints for
/// such a page. A caller that cannot use a partial page checks the error.
///
/// Limits: retained records are charged to `max_retained_bytes`, and the form-depth, inline-images
/// and other Go caps apply; the scan's retained bytes also stay within `max_alloc` (see `Scan::keep`); `max_threads` is not used.
///
/// # Errors
///
/// [`crate::Error::Limit`] over a cap, [`crate::Error::Cancelled`] when the cancel flag is set,
/// [`crate::Error::Malformed`] for a bad stream and [`crate::Error::OutOfMemory`] when a buffer
/// cannot grow. The `Scan` is returned in every case.
pub fn walk(
    cancel: &AtomicBool,
    src: &Stream,
    scope: i64,
    env: &dyn Env,
    limits: &Limits,
) -> (Scan, Result<()>) {
    let mut s = Scan {
        lim: *limits,
        ..Scan::default()
    };
    let err = walk_stream(cancel, src, scope, env, GState::initial(), 0, &mut s);
    (s, err)
}

fn walk_stream(
    cancel: &AtomicBool,
    src: &Stream,
    scope: i64,
    env: &dyn Env,
    mut gs: GState,
    depth: i64,
    s: &mut Scan,
) -> Result<()> {
    limits::check(
        "form-depth",
        depth,
        s.lim.form_depth().min(MAX_NATIVE_DEPTH),
    )?;
    let mut l = Lexer::new(src.as_slice());
    let mut stack: Vec<GState> = Vec::new();
    let mut path = PathBox::default();
    let mut ops: Vec<Token> = Vec::new();
    // Marks that W or W* was seen since the current path was started: ISO 32000-1 8.5.4, "It
    // merely sets a flag... that shall be examined by the path-painting operator that follows
    // immediately after". It is local like `path`, not part of the graphics state, because it
    // too is current-path state rather than something q/Q ever needs to save.
    let mut pending_clip = false;
    loop {
        check_cancel(cancel)?;
        let Some(tok) = l.next_token()? else {
            // End of stream is the normal exit.
            return Ok(());
        };
        if tok.kind == Kind::InlineImage {
            s.inline_imgs += 1;
            let mut bbox = gs.ctm.unit_square_box();
            if let Some(c) = &gs.clip {
                bbox = intersect_box(c, &bbox);
            }
            limits::check(
                "inline-images",
                s.inline_images.len() as i64 + 1,
                s.lim.inline_images(),
            )?;
            s.keep(with_clip(PLACEMENT_BYTES, &gs.clip))?;
            s.order += 1;
            let p = Placement {
                ctm: gs.ctm,
                bbox,
                clip: gs.clip,
                opaque: gs.opaque,
                index: s.order,
                inline: true,
                ..Placement::default()
            };
            push(&mut s.inline_images, p)?;
            ops.clear();
            continue;
        }
        if tok.kind != Kind::Keyword {
            if (ops.len() as i64) < s.lim.operands() {
                ops.try_reserve(1)?;
                ops.push(tok);
            }
            continue;
        }

        let op: &[u8] = tok.text.as_ref();
        match op {
            b"q" => {
                limits::check("q-depth", stack.len() as i64 + 1, s.lim.gstate_depth())?;
                push(&mut stack, gs.clone())?;
            }
            b"Q" => {
                if let Some(prev) = stack.pop() {
                    gs = prev;
                }
            }
            b"cm" => {
                if let Some(m) = matrix_operands(&ops) {
                    gs.ctm = m.mul(&gs.ctm);
                }
            }
            b"gs" => {
                if let Some(t) = ops.last()
                    && t.kind == Kind::Name
                {
                    gs.opaque = gs.opaque && env.ext_gstate_opaque(scope, &t.text);
                }
            }
            b"Tr" => {
                if let Some(t) = ops.last()
                    && t.kind == Kind::Number
                {
                    // Go on amd64 yields i64::MIN for out-of-range (and NaN) floats.
                    gs.tr =
                        if (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&t.num) {
                            t.num as i64
                        } else {
                            i64::MIN
                        };
                }
            }
            b"w" => {
                if let Some(v) = last_number(&ops) {
                    gs.line_width = v;
                }
            }

            // Text state (ISO 32000-1 9.3, 9.4.2). BT resets the text and line matrices to
            // identity; ET has nothing to undo, because the matrices are only meaningful
            // between the two and the next BT resets them again. The remaining parameters live
            // on the graphics state, so q/Q and form recursion handle them like every other
            // entry.
            b"BT" => {
                gs.tm = IDENTITY;
                gs.tlm = IDENTITY;
            }
            b"Tf" => {
                gs.font = Rc::from(last_name(&ops));
                if let Some(v) = last_number(&ops) {
                    gs.font_size = v;
                }
                gs.font_id = env.font(scope, &gs.font).unwrap_or(0);
            }
            b"Td" => {
                if let [.., tx, ty] = number_operands(&ops).as_slice() {
                    gs.next_line(*tx, *ty);
                }
            }
            b"TD" => {
                // Like Td, but also sets the leading to -ty (table 108).
                if let [.., tx, ty] = number_operands(&ops).as_slice() {
                    gs.leading = -*ty;
                    gs.next_line(*tx, *ty);
                }
            }
            b"Tm" => {
                if let Some(m) = matrix_operands(&ops) {
                    gs.tm = m;
                    gs.tlm = m;
                }
            }
            b"T*" => gs.next_line(0.0, -gs.leading),
            b"TL" => {
                if let Some(v) = last_number(&ops) {
                    gs.leading = v;
                }
            }
            b"Tc" => {
                if let Some(v) = last_number(&ops) {
                    gs.char_sp = v;
                }
            }
            b"Tw" => {
                if let Some(v) = last_number(&ops) {
                    gs.word_sp = v;
                }
            }
            b"Tz" => {
                // The operand is a percentage (table 105).
                if let Some(v) = last_number(&ops) {
                    gs.hscale = v / 100.0;
                }
            }
            b"Ts" => {
                if let Some(v) = last_number(&ops) {
                    gs.rise = v;
                }
            }

            // Colour. The lower-case operator sets the nonstroking colour and the upper-case
            // one the stroking colour (ISO 32000-1 section 8.6.8). cs and CS name a space and
            // reset the colour to that space's initial value, which is why the components are
            // dropped rather than kept.
            b"g" => gs.fill = device_color(b"DeviceGray", &ops),
            b"G" => gs.stroke = device_color(b"DeviceGray", &ops),
            b"rg" => gs.fill = device_color(b"DeviceRGB", &ops),
            b"RG" => gs.stroke = device_color(b"DeviceRGB", &ops),
            b"k" => gs.fill = device_color(b"DeviceCMYK", &ops),
            b"K" => gs.stroke = device_color(b"DeviceCMYK", &ops),
            b"cs" => {
                gs.fill = Rc::new(Color {
                    space: last_name(&ops).to_vec(),
                    comps: Vec::new(),
                });
            }
            b"CS" => {
                gs.stroke = Rc::new(Color {
                    space: last_name(&ops).to_vec(),
                    comps: Vec::new(),
                });
            }
            b"sc" | b"scn" => Rc::make_mut(&mut gs.fill).comps = number_operands(&ops),
            b"SC" | b"SCN" => Rc::make_mut(&mut gs.stroke).comps = number_operands(&ops),

            // Path construction. Every one of these takes its operands as coordinate pairs
            // (m and l one point, v and y two, c three), so pairing them off handles all of
            // them. h closes the subpath and introduces no new point.
            b"m" | b"l" | b"c" | b"v" | b"y" => {
                add_points(&mut path, &number_operands(&ops), &gs.ctm);
            }
            b"re" => add_rect(&mut path, &number_operands(&ops), &gs.ctm),
            // Neither variant paints or modifies the path; the fill rule they name only
            // matters for a self-intersecting path's interior, which a bounding box can never
            // see (ISO 32000-1 8.5.4). Both just arm the flag examined below.
            b"W" | b"W*" => pending_clip = true,
            b"n" => {
                // n ends the path without painting it. W/W* before it still sets the clip from
                // that path, captured in device space now, before reset discards it.
                if pending_clip {
                    gs.clip = intersect_clip_path(gs.clip, &path);
                    pending_clip = false;
                }
                path.reset();
            }

            b"Do" => do_xobject(cancel, &ops, scope, env, gs.clone(), depth, s)?,
            b"Tj" | b"'" | b"\"" => {
                s.text_ops += 1;
                if inks_glyphs(gs.tr) {
                    s.inked_text_ops += 1;
                }
                if op == b"\""
                    && let [.., w, c] = number_operands(&ops).as_slice()
                {
                    // " sets the word and character spacing from its two leading operands,
                    // then behaves as ' (ISO 32000-1 9.4.3 table 109).
                    gs.word_sp = *w;
                    gs.char_sp = *c;
                }
                if op != b"Tj" {
                    // ' and " advance to the next line BEFORE showing.
                    gs.next_line(0.0, -gs.leading);
                }
                if let Some(t) = ops.iter().rev().find(|t| t.kind == Kind::String) {
                    s.text_chars += t.text.len();
                    record_text(s, &gs, src, t)?;
                }
            }
            b"TJ" => {
                s.text_ops += 1;
                if inks_glyphs(gs.tr) {
                    s.inked_text_ops += 1;
                }
                for o in &ops {
                    match o.kind {
                        Kind::String => {
                            s.text_chars += o.text.len();
                            record_text(s, &gs, src, o)?;
                        }
                        Kind::Number => {
                            // A TJ number is subtracted from the displacement, in thousandths
                            // of unscaled text space (9.4.3): it moves the text matrix, scaled
                            // by the font size and horizontal scaling, and leaves the line
                            // matrix alone.
                            gs.tm = Matrix([
                                1.0,
                                0.0,
                                0.0,
                                1.0,
                                -o.num / 1000.0 * gs.font_size * gs.hscale,
                                0.0,
                            ])
                            .mul(&gs.tm);
                        }
                        _ => {}
                    }
                }
            }
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => {
                // The clip takes effect after WHICHEVER path-painting operator follows W/W*,
                // not just n (ISO 32000-1 8.5.4), so this captures it from the path before
                // `record_paint` resets it, and deliberately never narrows the Paint this same
                // operator records.
                //
                // That is why the state is copied. `Paint::clip` records the clip in force when
                // the operator was ISSUED, and a path that paints and clips in one operator is
                // not clipped by its own W. The difference is only visible on a stroke:
                // `record_paint` spreads a stroke's box by half the pen, while the clip comes
                // from the UN-spread path, so clipping the paint by its own W would shave
                // exactly that spread off.
                let painted = gs.clone();
                if pending_clip {
                    gs.clip = intersect_clip_path(gs.clip, &path);
                    pending_clip = false;
                }
                record_paint(s, &mut path, &painted, painting_op(op))?;
            }
            b"sh" => s.shading_ops += 1,
            _ => {}
        }
        ops.clear();
    }
}

/// The static name of a painting operator, for `Paint::op`.
fn painting_op(op: &[u8]) -> &'static str {
    match op {
        b"S" => "S",
        b"s" => "s",
        b"F" => "F",
        b"f*" => "f*",
        b"B" => "B",
        b"B*" => "B*",
        b"b" => "b",
        b"b*" => "b*",
        _ => "f",
    }
}

fn device_color(space: &[u8], ops: &[Token]) -> Rc<Color> {
    Rc::new(Color {
        space: space.to_vec(),
        comps: number_operands(ops),
    })
}

/// Appends one shown string with the text state in force. `raw` is the lexer's string: a slice
/// of the stream being walked when the string has no escapes (so every draw of one form shares
/// the form's one buffer), else a fresh copy, which is charged.
fn record_text(s: &mut Scan, gs: &GState, src: &Stream, t: &Token) -> Result<()> {
    if s.text_shows.len() as i64 >= s.lim.text_shows() {
        return Ok(());
    }
    let mut n = TEXT_SHOW_BYTES + gs.font.len() as i64; // Tf copies the name per call
    if !t.alias {
        n += t.text.len() as i64;
    }
    s.keep(n)?;
    let raw = if t.alias {
        Raw::alias(src, t.at, t.at + t.text.len())
    } else {
        Raw::owned(t.text.to_vec())
    };
    s.order += 1;
    let show = TextShow {
        raw,
        font: gs.font.to_vec(),
        font_id: gs.font_id,
        size: gs.font_size,
        char_spacing: gs.char_sp,
        word_spacing: gs.word_sp,
        hscale: gs.hscale,
        rise: gs.rise,
        mode: gs.tr,
        trm: gs.tm.mul(&gs.ctm),
        index: s.order,
    };
    push(&mut s.text_shows, show)
}

/// Appends the path as painted and clears it, which every painting operator does (ISO 32000-1
/// section 8.5.3).
fn record_paint(s: &mut Scan, path: &mut PathBox, gs: &GState, op: &'static str) -> Result<()> {
    let r = record_paint_inner(s, path, gs, op);
    path.reset();
    r
}

fn record_paint_inner(s: &mut Scan, path: &PathBox, gs: &GState, op: &'static str) -> Result<()> {
    if !path.set {
        // A painting operator with no current path marks nothing, and giving it the empty box
        // would put a rectangle at the origin into the record.
        return Ok(());
    }
    let mut bbox = path.bbox;
    if is_stroking_op(op) {
        // Ink spreads half the line width either side of the path. Without this a border
        // stroked along the raster's own edge would look contained by it, when half the ink
        // actually falls outside.
        let r = gs.line_width * device_scale(&gs.ctm) / 2.0;
        bbox = BBox {
            llx: bbox.llx - r,
            lly: bbox.lly - r,
            urx: bbox.urx + r,
            ury: bbox.ury + r,
        };
    }
    if s.paints.len() as i64 >= s.lim.paints() {
        return Ok(());
    }
    s.keep(with_clip(PAINT_BYTES, &gs.clip) + color_bytes(&gs.fill) + color_bytes(&gs.stroke))?;
    s.order += 1;
    let p = Paint {
        op,
        bbox,
        fill: (*gs.fill).clone(),
        stroke: (*gs.stroke).clone(),
        index: s.order,
        clip: gs.clip,
        ctm: gs.ctm,
    };
    push(&mut s.paints, p)
}

/// How much the CTM magnifies lengths. The square root of the determinant is exact for the
/// rotations and uniform scales a page placement carries, and for an anisotropic one it is the
/// geometric mean rather than the worst axis: close enough for a pen width, and nowhere near the
/// tolerances classification decides on.
fn device_scale(m: &Matrix) -> f64 {
    let m = &m.0;
    (m[0] * m[3] - m[1] * m[2]).abs().sqrt()
}

/// Adds every coordinate pair in `nums`, mapped through `m`.
fn add_points(p: &mut PathBox, nums: &[f64], m: &Matrix) {
    for xy in nums.chunks_exact(2) {
        p.add(m.apply(xy[0], xy[1]));
    }
}

/// Adds the four corners of a `re` rectangle. Width or height may be negative, and taking all
/// four corners covers that without a special case.
fn add_rect(p: &mut PathBox, nums: &[f64], m: &Matrix) {
    let [.., x, y, w, h] = nums else {
        return;
    };
    p.add(m.apply(*x, *y));
    p.add(m.apply(x + w, *y));
    p.add(m.apply(x + w, y + h));
    p.add(m.apply(*x, y + h));
}

fn do_xobject(
    cancel: &AtomicBool,
    ops: &[Token],
    scope: i64,
    env: &dyn Env,
    mut gs: GState,
    depth: i64,
    s: &mut Scan,
) -> Result<()> {
    let Some(last) = ops.last() else {
        return Ok(());
    };
    if last.kind != Kind::Name {
        return Ok(());
    }
    let name: &[u8] = &last.text;
    let Some(xo) = env.xobject(scope, name) else {
        limits::check(
            "unresolved",
            s.unresolved.len() as i64 + 1,
            s.lim.unresolved(),
        )?;
        s.keep(STRING_BYTES + name.len() as i64)?;
        return push(&mut s.unresolved, name.to_vec());
    };
    if xo.image {
        limits::check("images", s.images.len() as i64 + 1, s.lim.images())?;
        let mut bbox = gs.ctm.unit_square_box();
        if let Some(c) = &gs.clip {
            bbox = intersect_box(c, &bbox);
        }
        s.keep(with_clip(PLACEMENT_BYTES, &gs.clip) + name.len() as i64)?;
        s.order += 1;
        let p = Placement {
            name: name.to_vec(),
            id: xo.id,
            ctm: gs.ctm,
            bbox,
            clip: gs.clip,
            opaque: gs.opaque,
            index: s.order,
            inline: false,
        };
        return push(&mut s.images, p);
    }
    gs.ctm = xo.matrix.mul(&gs.ctm);
    if let Some(bb) = &xo.bbox {
        // ISO 32000-1 8.10.2: /BBox trims the form to its boundaries. It lives in the form's
        // own coordinate system, which /Matrix composed with the enclosing CTM maps to device
        // space: the same composition just built above for the recursive walk.
        let mut dev = bb.map_through(&gs.ctm);
        if let Some(c) = &gs.clip {
            dev = intersect_box(c, &dev);
        }
        gs.clip = Some(dev);
    }
    s.forms += 1;
    limits::check("form-invocations", s.forms, s.lim.form_invocations())?;
    walk_stream(cancel, &xo.content, xo.scope, env, gs, depth + 1, s)
}

/// Folds a just-closed clip path into `clip`, in device space (the path's points were already
/// mapped through the CTM as they were added, so no further mapping happens here). A path with
/// no points sets no clip: W with no current path marks nothing (ISO 32000-1 8.5.4).
fn intersect_clip_path(clip: Option<BBox>, path: &PathBox) -> Option<BBox> {
    if !path.set {
        return clip;
    }
    Some(match &clip {
        Some(c) => intersect_box(c, &path.bbox),
        None => path.bbox,
    })
}

/// The intersection of `a` and `b`. The two rectangles may not overlap on an axis, which would
/// ordinarily invert that axis (max > min); this clamps the far edge to the near one instead, so
/// the result always keeps every box in this module's `llx <= urx`, `lly <= ury` invariant. A
/// disjoint clip therefore reports a zero-area box at the near corner, not an inverted one.
pub fn intersect_box(a: &BBox, b: &BBox) -> BBox {
    let (llx, lly) = (fmax(a.llx, b.llx), fmax(a.lly, b.lly));
    let (mut urx, mut ury) = (fmin(a.urx, b.urx), fmin(a.ury, b.ury));
    if urx < llx {
        urx = llx;
    }
    if ury < lly {
        ury = lly;
    }
    BBox { llx, lly, urx, ury }
}

/// Reads the six numbers a cm operator takes.
fn matrix_operands(ops: &[Token]) -> Option<Matrix> {
    let tail = ops.len().checked_sub(6).and_then(|i| ops.get(i..))?;
    let mut m = [0.0; 6];
    for (slot, t) in m.iter_mut().zip(tail) {
        if t.kind != Kind::Number {
            return None;
        }
        *slot = t.num;
    }
    Some(Matrix(m))
}

/// The numeric operands in order. Non-numeric tokens are dropped rather than rejected: a
/// malformed operand list should cost a little precision, not abort the walk of an otherwise
/// readable page.
fn number_operands(ops: &[Token]) -> Vec<f64> {
    ops.iter()
        .filter(|o| o.kind == Kind::Number)
        .map(|o| o.num)
        .collect()
}

fn last_number(ops: &[Token]) -> Option<f64> {
    ops.iter()
        .rev()
        .find(|o| o.kind == Kind::Number)
        .map(|o| o.num)
}

fn last_name<'s>(ops: &'s [Token<'_>]) -> &'s [u8] {
    ops.iter()
        .rev()
        .find(|o| o.kind == Kind::Name)
        .map(|o| o.text.as_ref())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "walk_tests.rs"]
mod tests;
