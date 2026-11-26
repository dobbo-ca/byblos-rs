//! The page rasteriser: port of byblos `internal/render/render.go`.
//!
//! [`page`] interprets the path, colour, clip, image and text operators of a decoded content
//! stream and rasterises them onto a white, opaque RGBA canvas. It reads the same tokens as
//! `content::walk` but keeps the real path (every subpath, every flattened curve point, the fill
//! rule) because it paints at each operator. Fills sample pixel centres against half-open
//! scanline spans; nonzero sums signed crossings, even-odd takes their parity.
//!
//! The page has no PDF parser behind it: images and fonts are looked up through the
//! [`ImageFor`] and [`FontFor`] closures the caller supplies (`None` renders without them). The
//! text half of the renderer (`text.go`) is in `text.rs`.
//!
//! Untrusted input: the page box and scale come out of the file, so raster dimensions are
//! checked against the limits before the canvas is allocated; flattened path points and scanline
//! work are charged against budgets as they accrue.
//!
//! Float-to-int conversions go through [`go_int`], which gives Go's amd64 result for a NaN or an
//! out-of-range value (the minimum integer), because the clamps of the Go code are written
//! against that behaviour and the parity oracle runs on amd64.

use super::sfnt;
use super::text::{FontFor, FontProg, ProgKey, TextFont};
use crate::content::{BBox, IDENTITY, Kind, Lexer, Matrix, Token};
use crate::error::{Error, Result};
use crate::go::{math as gomath, sort::slice_by};
use crate::image::{Image, Rect, Rgba};
use crate::limits::{Limits, check, check_cancel};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

/// `maxCurveDepth`: bounds recursive Bezier subdivision to 2^8 = 256 segments per curve, so a
/// hostile stream of `c` operators amplifies its own byte count by a bounded constant.
const MAX_CURVE_DEPTH: u32 = 8;

/// `flatTol`: the flattening tolerance in device pixels.
const FLAT_TOL: f64 = 0.2;

/// `maxCanvasPixels`: the hard ceiling on the canvas size (16 GiB of RGBA), whatever
/// `Limits::max_raster_pixels` says.
const MAX_CANVAS_PIXELS: i64 = 1 << 32;

/// A decoded image XObject ready for `Do` to sample (Go's `render.Image`). Decoding is the
/// caller's. For a stencil (`/ImageMask true`), `data` is the decoded 1-bit gray by the usual
/// convention: luminance below one half is sample value 0, the value that marks the page under
/// the default `/Decode` (ISO 32000-1 8.9.6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderImage {
    pub data: Image,
    /// `/ImageMask`: paint the fill colour through the stencil.
    pub stencil: bool,
    /// Stencil `/Decode [1 0]`: paint where the sample is 1, not 0.
    pub invert: bool,
}

/// `ImageFor`: resolves a `Do` operand to a decoded image. `None` skips the draw cleanly (an
/// unresolved name, a form XObject, a codec byblos does not decode). It is called once per `Do`,
/// so implementations must cache by name or bound their own decode work.
pub type ImageFor<'a> = dyn FnMut(&[u8]) -> Option<RenderImage> + 'a;

/// What [`page`] needs to know about the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageParams {
    /// The page box in PDF points (user space, y up).
    pub bbox: BBox,
    /// The page's `/Rotate`: the degrees the raster turns clockwise for display (ISO 32000-1
    /// table 30). It swaps the canvas for 90 and 270.
    pub rotate: i64,
    /// Device pixels per point. The canvas is the box's size times `scale`, with device row 0 at
    /// the top of the page.
    pub scale: f64,
}

/// `Page` and `PageLimits`: interprets the content stream `src` and rasterises it onto a white,
/// opaque canvas. `images` resolves `Do` operands and `fonts` resolves `Tf` operands; either may
/// be `None`.
///
/// # Errors
/// `Limit` (`raster-side`, `raster-pixels`, `q-depth`, `path-points`, `fill-work`),
/// `Alloc`/`OutOfMemory` for the canvas, `Cancelled`, a lexer error, and `Malformed` for a scale
/// or page box that cannot be rasterised or a `/Rotate` that is not a multiple of 90. Go returns
/// the partly painted canvas with the error; this returns only the error.
pub fn page<'a>(
    src: &[u8],
    params: &PageParams,
    images: Option<&'a mut ImageFor<'a>>,
    fonts: Option<&'a mut FontFor<'a>>,
    limits: &'a Limits,
    cancel: &AtomicBool,
) -> Result<Rgba> {
    let (w, h, fw, fh) = raster_size(&params.bbox, params.scale, limits)?;
    let rot = ((params.rotate % 360) + 360) % 360;
    if rot % 90 != 0 {
        return Err(Error::Malformed(format!(
            "render: /Rotate {} is not a multiple of 90",
            params.rotate
        )));
    }
    let (w, h) = if rot == 90 || rot == 270 {
        (h, w)
    } else {
        (w, h)
    };
    let mut img = Rgba::new(Rect::new(0, 0, w, h), limits)?;
    img.pix.fill(0xff);
    let mut r = Renderer::new(limits, img, images, fonts);
    // User space to device space: scale, then flip y so user URY lands on device row 0, then turn
    // the whole device space for /Rotate. Row-vector convention, like content::Matrix.
    let s = params.scale;
    let base = Matrix([s, 0.0, 0.0, -s, -params.bbox.llx * s, params.bbox.ury * s])
        .mul(&display_turn(rot, fw, fh));
    // ISO 32000-1 8.4.1's initial graphics state for the parts tracked here: DeviceGray black
    // for both colours, line width 1.
    let ink = ColorState {
        space: Space::Gray,
        rgba: [0, 0, 0, 255],
    };
    r.run(
        src,
        GState {
            ctm: base,
            line_width: 1.0,
            fill: ink,
            stroke: ink,
            clip: None,
            tm: IDENTITY,
            tlm: IDENTITY,
            font: None,
            font_size: 0.0,
            char_sp: 0.0,
            word_sp: 0.0,
            hscale: 1.0,
            leading: 0.0,
            rise: 0.0,
            tr: 0,
        },
        cancel,
    )?;
    Ok(r.img)
}

/// `displayTurn`: maps the unrotated device space of a page onto the displayed one for
/// `/Rotate`. An exact quarter turn permutes the device grid onto itself: no resampling happens
/// here, only a change of which pixel a mark lands in.
///
/// `fw` and `fh` are the exact unrotated device extents, not the ceiled canvas dimensions:
/// the canvas is rounded up on both axes, and poppler leaves that slack on the far edge of each
/// axis before the turn.
fn display_turn(rot: i64, fw: f64, fh: f64) -> Matrix {
    match rot {
        90 => Matrix([0.0, 1.0, -1.0, 0.0, fh, 0.0]),
        180 => Matrix([-1.0, 0.0, 0.0, -1.0, fw, fh]),
        270 => Matrix([0.0, -1.0, 1.0, 0.0, 0.0, fw]),
        _ => IDENTITY,
    }
}

/// `rasterSize`: the canvas in whole pixels and the exact device extents it was rounded up from.
pub(crate) fn raster_size(bbox: &BBox, scale: f64, lim: &Limits) -> Result<(i64, i64, f64, f64)> {
    if scale.is_nan() || scale <= 0.0 || scale.is_infinite() {
        return Err(Error::Malformed(format!(
            "render: scale {scale} is not a positive finite number"
        )));
    }
    let fw = (bbox.urx - bbox.llx) * scale;
    let fh = (bbox.ury - bbox.lly) * scale;
    if fw.is_nan() || fw < 1.0 || fh.is_nan() || fh < 1.0 {
        return Err(Error::Malformed(format!(
            "render: page box {bbox:?} at scale {scale} rasterises below one pixel"
        )));
    }
    // Compared in float: a float beyond int64 converts to MinInt64 on amd64 and would slip under
    // the cap.
    let v = fmax(fw, fh).ceil();
    if v > lim.raster_side() as f64 {
        return Err(Error::Limit {
            name: "raster-side",
            got: sat_i64(v),
            max: lim.raster_side(),
        });
    }
    let (cw, ch) = (fw.ceil(), fh.ceil());
    // The canvas is cw x ch, not fw*fh, and a caller-raised cap is clamped to the hard ceiling.
    let max_px = lim.raster_pixels().min(MAX_CANVAS_PIXELS);
    let p = cw * ch;
    if p > max_px as f64 {
        return Err(Error::Limit {
            name: "raster-pixels",
            got: sat_i64(p),
            max: max_px,
        });
    }
    Ok((go_int(cw), go_int(ch), fw, fh))
}

/// `satInt64`: converts `v` to `i64`, saturating.
fn sat_i64(v: f64) -> i64 {
    if v >= (1u64 << 63) as f64 {
        i64::MAX
    } else {
        v as i64
    }
}

/// Go's `int(f)` on amd64: a NaN or out-of-range value converts to the minimum integer.
pub(super) fn go_int(v: f64) -> i64 {
    let lim = -(i64::MIN as f64);
    if !(-lim..lim).contains(&v) {
        i64::MIN
    } else {
        v as i64
    }
}

/// `math.Min` as amd64 computes it: -Inf beats NaN, otherwise NaN wins, and -0 is less than +0.
pub(super) fn go_math_min(a: f64, b: f64) -> f64 {
    if a == f64::NEG_INFINITY || b == f64::NEG_INFINITY {
        return f64::NEG_INFINITY;
    }
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b { a } else { b }
}

/// `math.Max` as amd64 computes it: +Inf beats NaN, otherwise NaN wins, and +0 is greater than -0.
pub(super) fn go_math_max(a: f64, b: f64) -> f64 {
    if a == f64::INFINITY || b == f64::INFINITY {
        return f64::INFINITY;
    }
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b { a } else { b }
}

/// Go's `max` builtin on floats: NaN wins, and +0 is greater than -0.
pub(super) fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b { a } else { b }
}

/// `clamp01`.
fn clamp01(v: f64) -> f64 {
    go_math_min(1.0, go_math_max(0.0, v))
}

/// `grayColor`: the last operand as a gray level.
fn gray_color(nums: &[f64]) -> [u8; 4] {
    let g = nums.last().map_or(0.0, |&v| clamp01(v));
    let c = (g * 255.0).round() as u8;
    [c, c, c, 255]
}

/// `rgbColor`: the last three operands.
fn rgb_color(nums: &[f64]) -> [u8; 4] {
    let v = match nums {
        [.., r, g, b] => [clamp01(*r), clamp01(*g), clamp01(*b)],
        _ => [0.0; 3],
    };
    [
        (v[0] * 255.0).round() as u8,
        (v[1] * 255.0).round() as u8,
        (v[2] * 255.0).round() as u8,
        255,
    ]
}

/// `cmykColor`: the naive (1-c)(1-k) conversion, the same shape every viewer uses without an ICC
/// profile. There is no colour management, so it is wrong under a real CMYK profile.
fn cmyk_color(nums: &[f64]) -> [u8; 4] {
    let v = match nums {
        [.., c, m, y, k] => [clamp01(*c), clamp01(*m), clamp01(*y), clamp01(*k)],
        _ => [0.0; 4],
    };
    let (c, m, y, k) = (v[0], v[1], v[2], v[3]);
    [
        ((1.0 - c) * (1.0 - k) * 255.0).round() as u8,
        ((1.0 - m) * (1.0 - k) * 255.0).round() as u8,
        ((1.0 - y) * (1.0 - k) * 255.0).round() as u8,
        255,
    ]
}

/// The colour spaces `cs`/`CS` can name that this renderer tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Space {
    Gray,
    Rgb,
    /// Pattern, Indexed, Separation and DeviceN: their operands are not colour components, so
    /// they leave the colour at the black default.
    Inert,
    /// Any other name: almost always an ICCBased resource, whose model the operand count
    /// recovers (3 is RGB, 4 is CMYK).
    Other,
}

fn space_of(name: &[u8]) -> Space {
    match name {
        b"DeviceGray" => Space::Gray,
        b"DeviceRGB" => Space::Rgb,
        b"Pattern" | b"Indexed" | b"Separation" | b"DeviceN" => Space::Inert,
        _ => Space::Other,
    }
}

/// `colorState`: a colour space and the device colour its operands resolved to at set time.
#[derive(Clone, Copy, Debug)]
pub(super) struct ColorState {
    space: Space,
    pub(super) rgba: [u8; 4],
}

impl ColorState {
    /// `setComps`: applies `sc`/`scn` operands under the current space. There is no 1-operand
    /// case under an unknown space: a single operand is a palette index or a subtractive tint
    /// there, and guessing gray measurably regressed real documents.
    fn set_comps(&mut self, nums: &[f64]) {
        match self.space {
            Space::Gray => self.rgba = gray_color(nums),
            Space::Rgb => self.rgba = rgb_color(nums),
            Space::Inert => {}
            Space::Other => match nums.len() {
                3 => self.rgba = rgb_color(nums),
                4 => self.rgba = cmyk_color(nums),
                _ => {}
            },
        }
    }
}

/// `point`: a device-space point.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Point {
    pub(super) x: f64,
    pub(super) y: f64,
}

/// `subpath`: a run of flattened device-space points. `closed` records an explicit `h` (or
/// `re`), which matters for stroking.
#[derive(Debug, Clone, Default)]
pub(super) struct Subpath {
    pts: Vec<Point>,
    closed: bool,
}

/// `clipRect`: the device-space clip rectangle.
#[derive(Clone, Copy, Debug)]
pub(super) struct ClipRect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// `gstate`: the graphics state this renderer tracks, saved and restored as a unit by `q`/`Q`.
/// The text parameters are graphics state (ISO 32000-1 table 52); `tm` and `tlm` are strictly
/// BT..ET-scoped but live here, so that `q`/`Q` nesting handles them and `BT` resets both.
#[derive(Clone)]
pub(super) struct GState {
    pub(super) ctm: Matrix,
    line_width: f64,
    pub(super) fill: ColorState,
    stroke: ColorState,
    /// The device-space clip, or `None` when nothing has clipped yet. Rectangular only: `W`/`W*`
    /// contribute the path's device bounding box.
    pub(super) clip: Option<ClipRect>,
    pub(super) tm: Matrix,
    tlm: Matrix,
    /// `Tf` resolution; `None` skips shows entirely.
    pub(super) font: Option<Rc<TextFont>>,
    pub(super) font_size: f64,
    pub(super) char_sp: f64,
    pub(super) word_sp: f64,
    pub(super) hscale: f64,
    leading: f64,
    pub(super) rise: f64,
    pub(super) tr: i64,
}

impl GState {
    /// `nextLine`: `Td`, `TD`, `T*` and the implicit advance of `'` and `"` move the line matrix
    /// and the text matrix follows it (ISO 32000-1 9.4.2).
    fn next_line(&mut self, tx: f64, ty: f64) {
        self.tlm = Matrix([1.0, 0.0, 0.0, 1.0, tx, ty]).mul(&self.tlm);
        self.tm = self.tlm;
    }
}

/// The per-call budgets: flattened path points held for the current path, and scanline work
/// spent (one unit per active edge per scanline and one per painted pixel).
pub(super) struct Budget {
    points: i64,
    max_points: i64,
    fill_work: i64,
    max_fill: i64,
}

impl Budget {
    fn new(lim: &Limits) -> Budget {
        Budget {
            points: 0,
            max_points: lim.path_points(),
            fill_work: 0,
            max_fill: lim.fill_work(),
        }
    }

    /// `charge`: `n` more flattened points held.
    fn charge(&mut self, n: i64) -> Result<()> {
        self.points = self.points.saturating_add(n);
        check("path-points", self.points, self.max_points)
    }

    /// `chargeFill`: `n` units against the per-page fill budget.
    pub(super) fn charge_fill(&mut self, n: i64) -> Result<()> {
        self.fill_work = self.fill_work.saturating_add(n);
        check("fill-work", self.fill_work, self.max_fill)
    }
}

/// `path`: the current path under construction, all points already in device space (affine maps
/// preserve Beziers, so mapping control points through the CTM and flattening in device space is
/// exact and keeps the tolerance in pixels).
#[derive(Debug, Default)]
pub(super) struct Path {
    pub(super) subs: Vec<Subpath>,
    pub(super) cur: Point,
    has: bool,
    open: bool,
}

/// `pathPoints`.
pub(super) fn path_points(subs: &[Subpath]) -> i64 {
    subs.iter().map(|s| s.pts.len() as i64).sum()
}

impl Path {
    /// `reset`: empties the path and returns its points to the budget.
    pub(super) fn reset(&mut self, bud: &mut Budget) {
        bud.points -= path_points(&self.subs);
        *self = Path::default();
    }

    pub(super) fn move_to(&mut self, bud: &mut Budget, pt: Point) -> Result<()> {
        bud.charge(1)?;
        self.subs.push(Subpath {
            pts: vec![pt],
            closed: false,
        });
        self.cur = pt;
        self.has = true;
        self.open = true;
        Ok(())
    }

    pub(super) fn line_to(&mut self, bud: &mut Budget, pt: Point) -> Result<()> {
        if !self.has {
            return Ok(());
        }
        bud.charge(1)?;
        if !self.open {
            // After h, a construction operator continues from the closed subpath's start (ISO
            // 32000-1 8.5.2) in a fresh subpath.
            self.subs.push(Subpath {
                pts: vec![self.cur],
                closed: false,
            });
            self.open = true;
            bud.charge(1)?;
        }
        if let Some(s) = self.subs.last_mut() {
            s.pts.push(pt);
        }
        self.cur = pt;
        Ok(())
    }

    pub(super) fn curve_to(
        &mut self,
        bud: &mut Budget,
        c1: Point,
        c2: Point,
        end: Point,
    ) -> Result<()> {
        if !self.has {
            return Ok(());
        }
        let mut pts = Vec::new();
        flatten_cubic(&mut pts, self.cur, c1, c2, end, MAX_CURVE_DEPTH);
        for pt in pts {
            self.line_to(bud, pt)?;
        }
        Ok(())
    }

    pub(super) fn close(&mut self) {
        if self.open
            && let Some(s) = self.subs.last_mut()
        {
            if s.pts.len() > 1 {
                s.closed = true;
            }
            if let Some(first) = s.pts.first() {
                self.cur = *first;
            }
            self.open = false;
        }
    }
}

/// `flattenCubic`: appends line-segment endpoints approximating the cubic `p0..p3`. Subdivision
/// stops when both control points lie within [`FLAT_TOL`] of the chord, or at depth 0 (the cap
/// that bounds a hostile curve's cost).
fn flatten_cubic(dst: &mut Vec<Point>, p0: Point, c1: Point, c2: Point, p3: Point, depth: u32) {
    if depth == 0 || cubic_flat(p0, c1, c2, p3) {
        dst.push(p3);
        return;
    }
    // de Casteljau split at t = 1/2.
    let m = |a: Point, b: Point| Point {
        x: (a.x + b.x) / 2.0,
        y: (a.y + b.y) / 2.0,
    };
    let (ab, bc, cd) = (m(p0, c1), m(c1, c2), m(c2, p3));
    let (abc, bcd) = (m(ab, bc), m(bc, cd));
    let mid = m(abc, bcd);
    flatten_cubic(dst, p0, ab, abc, mid, depth - 1);
    flatten_cubic(dst, mid, bcd, cd, p3, depth - 1);
}

fn cubic_flat(p0: Point, c1: Point, c2: Point, p3: Point) -> bool {
    // Distance of each control point from the chord p0-p3, against FLAT_TOL.
    let (dx, dy) = (p3.x - p0.x, p3.y - p0.y);
    let l = gomath::hypot(dx, dy);
    if l == 0.0 {
        return gomath::hypot(c1.x - p0.x, c1.y - p0.y) <= FLAT_TOL
            && gomath::hypot(c2.x - p0.x, c2.y - p0.y) <= FLAT_TOL;
    }
    let d1 = (dx * (p0.y - c1.y) - dy * (p0.x - c1.x)).abs();
    let d2 = (dx * (p0.y - c2.y) - dy * (p0.x - c2.x)).abs();
    d1 <= FLAT_TOL * l && d2 <= FLAT_TOL * l
}

/// `edge`: one non-horizontal line segment, stored top-down in device space.
#[derive(Clone, Copy, Debug)]
struct Edge {
    ytop: f64,
    ybot: f64,
    x: f64,
    dxdy: f64,
    /// +1 when the segment ran downward, -1 upward.
    dir: i64,
}

fn add_edge(edges: &mut Vec<Edge>, a: Point, b: Point) {
    if a.y == b.y {
        return;
    }
    let (a, b, dir) = if a.y > b.y { (b, a, -1) } else { (a, b, 1) };
    edges.push(Edge {
        ytop: a.y,
        ybot: b.y,
        x: a.x,
        dxdy: (b.x - a.x) / (b.y - a.y),
        dir,
    });
}

#[derive(Clone, Copy)]
struct Crossing {
    x: f64,
    dir: i64,
}

/// The colour, clip and half-width one stroke is drawn with.
#[derive(Clone, Copy)]
struct StrokeCtx {
    half: f64,
    clip: ClipRect,
    col: [u8; 4],
}

/// Sets pixel (x, y) of `img`; a pixel outside the canvas is ignored, as Go's `SetRGBA` does.
fn put(img: &mut Rgba, x: i64, y: i64, c: [u8; 4]) {
    if let Some(o) = img.pix_offset(x, y)
        && let Some(p) = img.pix.get_mut(o..o + 4)
    {
        p.copy_from_slice(&c);
    }
}

/// `renderer`: the canvas, the resolvers and the per-call budgets and caches.
pub(super) struct Renderer<'a> {
    pub(super) lim: &'a Limits,
    pub(super) img: Rgba,
    images: Option<&'a mut ImageFor<'a>>,
    pub(super) fonts: Option<&'a mut FontFor<'a>>,
    /// `Tf` resolutions; a `None` entry is a name known to be unusable.
    pub(super) fonts_by: HashMap<Vec<u8>, Option<Rc<TextFont>>>,
    /// Parsed font programs by content.
    pub(super) progs_by: HashMap<ProgKey, Rc<FontProg>>,
    pub(super) bud: Budget,
    /// Bytes charged against `Limits::max_render_font_bytes` so far.
    pub(super) font_budget: i64,
    /// The glyph scratch buffer, shared by every font.
    pub(super) buf: sfnt::Buffer,
}

impl<'a> Renderer<'a> {
    /// A renderer over `img`.
    pub(super) fn new(
        lim: &'a Limits,
        img: Rgba,
        images: Option<&'a mut ImageFor<'a>>,
        fonts: Option<&'a mut FontFor<'a>>,
    ) -> Renderer<'a> {
        Renderer {
            lim,
            img,
            images,
            fonts,
            fonts_by: HashMap::new(),
            progs_by: HashMap::new(),
            bud: Budget::new(lim),
            font_budget: 0,
            buf: sfnt::Buffer::new(lim),
        }
    }

    /// `run`: the operator loop. It collects operands, switches on the keyword and resets.
    /// Unknown operators are ignored, and a viewer keeps rendering past what it does not speak.
    fn run(&mut self, src: &[u8], mut gs: GState, cancel: &AtomicBool) -> Result<()> {
        let mut l = Lexer::new(src);
        let mut stack: Vec<GState> = Vec::new();
        let mut pth = Path::default();
        let mut ops: Vec<Token<'_>> = Vec::new();
        // W/W* arm a flag the next painting operator examines (ISO 32000-1 8.5.4).
        let mut pending_clip = false;
        let max_ops = self.lim.operands();
        loop {
            check_cancel(cancel)?;
            let Some(tok) = l.next_token()? else {
                return Ok(());
            };
            if tok.kind != Kind::Keyword {
                if tok.kind != Kind::InlineImage && (ops.len() as i64) < max_ops {
                    ops.push(tok);
                }
                continue;
            }

            let op: &[u8] = &tok.text;
            match op {
                b"q" => {
                    check("q-depth", stack.len() as i64 + 1, self.lim.gstate_depth())?;
                    stack.push(gs.clone());
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
                b"w" => {
                    if let Some(&v) = number_operands(&ops).last() {
                        gs.line_width = v;
                    }
                }

                b"g" => {
                    gs.fill = ColorState {
                        space: Space::Gray,
                        rgba: gray_color(&number_operands(&ops)),
                    };
                }
                b"G" => {
                    gs.stroke = ColorState {
                        space: Space::Gray,
                        rgba: gray_color(&number_operands(&ops)),
                    };
                }
                b"rg" => {
                    gs.fill = ColorState {
                        space: Space::Rgb,
                        rgba: rgb_color(&number_operands(&ops)),
                    };
                }
                b"RG" => {
                    gs.stroke = ColorState {
                        space: Space::Rgb,
                        rgba: rgb_color(&number_operands(&ops)),
                    };
                }
                b"cs" => {
                    // Naming a space resets the colour to its initial value, black for both
                    // device spaces (ISO 32000-1 8.6.8).
                    gs.fill = ColorState {
                        space: space_of(last_name(&ops)),
                        rgba: [0, 0, 0, 255],
                    };
                }
                b"CS" => {
                    gs.stroke = ColorState {
                        space: space_of(last_name(&ops)),
                        rgba: [0, 0, 0, 255],
                    };
                }
                b"sc" | b"scn" => gs.fill.set_comps(&number_operands(&ops)),
                b"SC" | b"SCN" => gs.stroke.set_comps(&number_operands(&ops)),

                // Text: the same operator semantics as walk's text state, feeding glyph
                // outlines to the fill machinery. ET needs no case: BT resets both matrices, and
                // the state parameters persist across text objects (table 104).
                b"BT" => {
                    gs.tm = IDENTITY;
                    gs.tlm = IDENTITY;
                }
                b"Tf" => {
                    gs.font = self.resolve_font(last_name(&ops));
                    if let Some(v) = last_number(&ops) {
                        gs.font_size = v;
                    }
                }
                b"Td" => {
                    if let [.., a, b] = number_operands(&ops).as_slice() {
                        gs.next_line(*a, *b);
                    }
                }
                b"TD" => {
                    if let [.., a, b] = number_operands(&ops).as_slice() {
                        gs.leading = -*b;
                        gs.next_line(*a, *b);
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
                    if let Some(v) = last_number(&ops) {
                        gs.hscale = v / 100.0;
                    }
                }
                b"Ts" => {
                    if let Some(v) = last_number(&ops) {
                        gs.rise = v;
                    }
                }
                b"Tr" => {
                    if let Some(v) = last_number(&ops) {
                        gs.tr = go_int(v);
                    }
                }
                b"Tj" | b"'" | b"\"" => {
                    if matches!(op, b"\"") {
                        // " sets word and character spacing from its two leading operands, then
                        // behaves as ' (ISO 32000-1 table 109).
                        if let [.., a, b] = number_operands(&ops).as_slice() {
                            gs.word_sp = *a;
                            gs.char_sp = *b;
                        }
                    }
                    if !matches!(op, b"Tj") {
                        // ' and " advance to the next line BEFORE showing.
                        gs.next_line(0.0, -gs.leading);
                    }
                    if let Some(o) = ops.iter().rev().find(|o| o.kind == Kind::String) {
                        self.show_text(&mut gs, &o.text)?;
                    }
                }
                b"TJ" => {
                    for o in &ops {
                        match o.kind {
                            Kind::String => self.show_text(&mut gs, &o.text)?,
                            Kind::Number => {
                                // Subtracted from the displacement, in thousandths of unscaled
                                // text space (9.4.3).
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

                b"m" => {
                    if let Some(pt) = last_point(&ops, &gs.ctm) {
                        pth.move_to(&mut self.bud, pt)?;
                    }
                }
                b"l" => {
                    if let Some(pt) = last_point(&ops, &gs.ctm) {
                        pth.line_to(&mut self.bud, pt)?;
                    }
                }
                b"c" | b"v" | b"y" => {
                    curve_op(&mut self.bud, &mut pth, op, &number_operands(&ops), &gs.ctm)?;
                }
                b"re" => rect_op(&mut self.bud, &mut pth, &number_operands(&ops), &gs.ctm)?,
                b"h" => pth.close(),

                b"Do" => self.draw_image(last_name(&ops), &gs)?,

                b"W" | b"W*" => {
                    // Both rules collapse to the same device bounding box here.
                    pending_clip = true;
                }

                b"n" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"S" | b"s" => {
                    // The paint sees the gstate BEFORE its own W installs a clip: a path that
                    // paints and clips in one operator is not clipped by itself.
                    let painted = gs.clone();
                    if pending_clip {
                        gs.clip = intersect_clip(gs.clip, &pth.subs);
                        pending_clip = false;
                    }
                    self.paint(op, &mut pth, &painted)?;
                    pth.reset(&mut self.bud);
                }
                _ => {}
            }
            ops.clear();
        }
    }

    /// `paint`: dispatches one path-painting operator.
    fn paint(&mut self, op: &[u8], p: &mut Path, gs: &GState) -> Result<()> {
        if matches!(op, b"b" | b"b*" | b"s") {
            p.close();
        }
        let (fill, even_odd, stroke) = match op {
            b"f" | b"F" => (true, false, false),
            b"f*" => (true, true, false),
            b"B" | b"b" => (true, false, true),
            b"B*" | b"b*" => (true, true, true),
            b"S" | b"s" => (false, false, true),
            _ => (false, false, false),
        };
        let clip = self.device_clip(gs.clip);
        if fill {
            self.fill_subpaths(&p.subs, even_odd, clip, gs.fill.rgba)?;
        }
        if stroke {
            self.stroke_subpaths(&p.subs, gs, clip)?;
        }
        Ok(())
    }

    /// `deviceClip`: converts the gstate clip to a canvas-bounded pixel-centre test range. With
    /// no clip it is the whole canvas.
    pub(super) fn device_clip(&self, c: Option<ClipRect>) -> ClipRect {
        let full = ClipRect {
            x0: 0.0,
            y0: 0.0,
            x1: self.img.rect.dx() as f64,
            y1: self.img.rect.dy() as f64,
        };
        match c {
            None => full,
            Some(c) => ClipRect {
                x0: go_math_max(full.x0, c.x0),
                y0: go_math_max(full.y0, c.y0),
                x1: go_math_min(full.x1, c.x1),
                y1: go_math_min(full.y1, c.y1),
            },
        }
    }

    /// `fillSubpaths`: scanline-fills the subpaths (each implicitly closed, as every fill
    /// operator does) under the chosen winding rule. A pixel is painted when its centre
    /// (x+0.5, y+0.5) is inside: crossings are counted over the half-open span `[ytop, ybot)`,
    /// which keeps a vertex from counting twice.
    pub(super) fn fill_subpaths(
        &mut self,
        subs: &[Subpath],
        even_odd: bool,
        clip: ClipRect,
        col: [u8; 4],
    ) -> Result<()> {
        let mut edges = Vec::new();
        for s in subs {
            if s.pts.len() < 2 {
                continue;
            }
            for w in s.pts.windows(2) {
                if let [a, b] = w {
                    add_edge(&mut edges, *a, *b);
                }
            }
            if let (Some(last), Some(first)) = (s.pts.last(), s.pts.first()) {
                add_edge(&mut edges, *last, *first);
            }
        }
        self.fill_edges(&mut edges, even_odd, clip, col)
    }

    fn fill_edges(
        &mut self,
        edges: &mut [Edge],
        even_odd: bool,
        clip: ClipRect,
        col: [u8; 4],
    ) -> Result<()> {
        if edges.is_empty() || clip.x1 <= clip.x0 || clip.y1 <= clip.y0 {
            return Ok(());
        }
        slice_by(edges, |a, b| a.ytop < b.ytop);
        let Some(first) = edges.first() else {
            return Ok(());
        };
        let (y_min, mut y_max) = (first.ytop, first.ybot);
        for e in edges.iter() {
            y_max = go_math_max(y_max, e.ybot);
        }
        let y0 = go_int((go_math_max(y_min, clip.y0) - 0.5).ceil());
        let y1 = go_int((go_math_min(y_max, clip.y1) - 0.5).ceil());
        let px0 = go_int((clip.x0 - 0.5).ceil());
        let px1 = go_int((clip.x1 - 0.5).ceil());

        let mut active: Vec<Edge> = Vec::new();
        let mut next = 0;
        let mut xs: Vec<Crossing> = Vec::new();
        for y in y0..y1 {
            let yc = y as f64 + 0.5;
            while let Some(e) = edges.get(next) {
                if e.ytop > yc || e.ytop.is_nan() {
                    break;
                }
                active.push(*e);
                next += 1;
            }
            xs.clear();
            active.retain(|e| {
                if e.ybot <= yc {
                    return false;
                }
                if e.ytop <= yc {
                    xs.push(Crossing {
                        x: e.x + (yc - e.ytop) * e.dxdy,
                        dir: e.dir,
                    });
                }
                true
            });
            self.bud.charge_fill(active.len() as i64 + 1)?;
            if xs.is_empty() {
                continue;
            }
            slice_by(&mut xs, |a, b| a.x < b.x);
            // After processing crossing i, winding describes the interval (xs[i].x,
            // xs[i+1].x). Nonzero fills where the sum of signed crossings is nonzero; even-odd
            // fills where their count is odd.
            let mut winding: i64 = 0;
            for (i, c) in xs.iter().enumerate() {
                if even_odd {
                    winding += 1;
                } else {
                    winding += c.dir;
                }
                let inside = if even_odd {
                    winding % 2 != 0
                } else {
                    winding != 0
                };
                let Some(nx) = xs.get(i + 1) else {
                    continue;
                };
                if !inside {
                    continue;
                }
                // Clamp in floats before the int conversion: a device x past 2^63 would wrap to
                // the minimum integer on amd64 and drop the span.
                let xa = px0.max(go_int((go_math_max(c.x, clip.x0) - 0.5).ceil()));
                let xb = px1.min(go_int((go_math_min(nx.x, clip.x1) - 0.5).ceil()));
                if xb <= xa {
                    continue;
                }
                self.bud.charge_fill(xb - xa)?;
                for x in xa..xb {
                    put(&mut self.img, x, y, col);
                }
            }
        }
        Ok(())
    }

    /// `strokeSubpaths`: widens each subpath's segments into rectangles and adds a join polygon
    /// at every shared vertex. Each polygon is filled the moment it is built: same-colour opaque
    /// fills union pixel-for-pixel with filling the whole pile at once, and the edge buffer
    /// stays small. Caps are butt (the PDF default); joins are octagons circumscribing the round
    /// join's circle. A width of zero is the thinnest renderable line (ISO 32000-1 8.4.3.2), one
    /// device pixel, and any width thinner than a pixel still marks, so the half-width floors at
    /// 0.5.
    fn stroke_subpaths(&mut self, subs: &[Subpath], gs: &GState, clip: ClipRect) -> Result<()> {
        let sc = StrokeCtx {
            half: go_math_max(gs.line_width * device_scale(&gs.ctm) / 2.0, 0.5),
            clip,
            col: gs.stroke.rgba,
        };
        let mut edges: Vec<Edge> = Vec::with_capacity(8);
        for s in subs {
            let n = s.pts.len();
            if n < 2 {
                continue;
            }
            for w in s.pts.windows(2) {
                if let [a, b] = w {
                    self.stroke_quad(&mut edges, *a, *b, &sc)?;
                }
            }
            for w in s.pts.windows(3) {
                if let [_, c, _] = w {
                    self.stroke_join(&mut edges, *c, &sc)?;
                }
            }
            if s.closed
                && let (Some(last), Some(first)) = (s.pts.last(), s.pts.first())
            {
                self.stroke_quad(&mut edges, *last, *first, &sc)?;
                self.stroke_join(&mut edges, *first, &sc)?;
                self.stroke_join(&mut edges, *last, &sc)?;
            }
        }
        Ok(())
    }

    /// `fillRing`: fills the polygon `pts` under the nonzero rule in the stroke colour.
    fn fill_ring(&mut self, edges: &mut Vec<Edge>, pts: &[Point], sc: &StrokeCtx) -> Result<()> {
        edges.clear();
        let n = pts.len();
        for (i, a) in pts.iter().enumerate() {
            if let Some(b) = pts.get((i + 1) % n) {
                add_edge(edges, *a, *b);
            }
        }
        self.fill_edges(edges, false, sc.clip, sc.col)
    }

    /// `quad`: the rectangle of half-width `sc.half` along the segment `a..b`.
    fn stroke_quad(
        &mut self,
        edges: &mut Vec<Edge>,
        a: Point,
        b: Point,
        sc: &StrokeCtx,
    ) -> Result<()> {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let l = gomath::hypot(dx, dy);
        if l == 0.0 {
            return Ok(());
        }
        let (nx, ny) = (-dy / l * sc.half, dx / l * sc.half);
        let q = [
            Point {
                x: a.x + nx,
                y: a.y + ny,
            },
            Point {
                x: b.x + nx,
                y: b.y + ny,
            },
            Point {
                x: b.x - nx,
                y: b.y - ny,
            },
            Point {
                x: a.x - nx,
                y: a.y - ny,
            },
        ];
        self.fill_ring(edges, &q, sc)
    }

    /// `join`: an octagon circumscribing the radius-`half` circle around `c`.
    fn stroke_join(&mut self, edges: &mut Vec<Edge>, c: Point, sc: &StrokeCtx) -> Result<()> {
        let rad = sc.half / gomath::cos(std::f64::consts::PI / 8.0);
        let mut q = [Point::default(); 8];
        for (i, p) in q.iter_mut().enumerate() {
            let a = -2.0 * std::f64::consts::PI * (i as f64) / 8.0;
            *p = Point {
                x: c.x + rad * gomath::cos(a),
                y: c.y + rad * gomath::sin(a),
            };
        }
        self.fill_ring(edges, &q, sc)
    }

    /// `drawImage`: paints an image XObject under the CTM. The image occupies the unit square in
    /// its own space (ISO 32000-1 8.9.5.2), so each destination pixel centre inside the placed
    /// square's device bounding box is mapped back through the CTM's inverse and sampled
    /// nearest-neighbour; the inverse mapping makes rotation, flip and shear exact. Source row 0
    /// is the top of the unit square (v near 1).
    fn draw_image(&mut self, name: &[u8], gs: &GState) -> Result<()> {
        if name.is_empty() {
            return Ok(());
        }
        let Some(resolve) = self.images.as_mut() else {
            return Ok(());
        };
        let Some(im) = resolve(name) else {
            return Ok(()); // unresolved, undecodable, or not an image: skip the draw
        };
        let sb = im.data.bounds();
        let (sw, sh) = (sb.dx(), sb.dy());
        if sw <= 0 || sh <= 0 {
            return Ok(());
        }
        let m = gs.ctm.0;
        let det = m[0] * m[3] - m[1] * m[2];
        if det == 0.0 || det.is_nan() || det.is_infinite() {
            return Ok(()); // a singular CTM places the image with zero area
        }
        // Destination range: the unit square's device bounding box intersected with the clip,
        // under the same pixel-centre convention as fills. The intersection happens in floats,
        // before any int conversion: a huge but finite translation would otherwise convert to
        // the minimum integer on amd64 and hang the loop below. After the guard both bounds sit
        // inside the clip, which deviceClip bounded to the canvas; the comparison also rejects
        // NaN bounds.
        let ub = gs.ctm.unit_square_box();
        let clip = self.device_clip(gs.clip);
        let fx0 = go_math_max(ub.llx, clip.x0);
        let fx1 = go_math_min(ub.urx, clip.x1);
        let fy0 = go_math_max(ub.lly, clip.y0);
        let fy1 = go_math_min(ub.ury, clip.y1);
        let overlaps = fx0 < fx1 && fy0 < fy1;
        if !overlaps {
            return Ok(());
        }
        let x0 = go_int((fx0 - 0.5).ceil());
        let x1 = go_int((fx1 - 0.5).ceil());
        let y0 = go_int((fy0 - 0.5).ceil());
        let y1 = go_int((fy1 - 0.5).ceil());
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        for y in y0..y1 {
            self.bud.charge_fill(x1 - x0)?;
            let dy = y as f64 + 0.5 - m[5];
            for x in x0..x1 {
                // Row-vector inverse: [u v] = [x-e, y-f] * [[a b],[c d]]^-1.
                let dx = x as f64 + 0.5 - m[4];
                let u = (dx * m[3] - dy * m[2]) / det;
                let v = (dy * m[0] - dx * m[1]) / det;
                let inside = (0.0..1.0).contains(&u) && (0.0..1.0).contains(&v);
                if !inside {
                    continue;
                }
                let sx = sb.x0 + go_int(u * sw as f64).min(sw - 1);
                let sy = sb.y0 + go_int((1.0 - v) * sh as f64).min(sh - 1);
                let (sr, sg, sbl, sa) = im.data.rgba_at(sx, sy);
                if im.stencil {
                    // The default /Decode [0 1] paints where the decoded sample is 0; invert
                    // (/Decode [1 0]) paints where it is 1 (ISO 32000-1 8.9.6.2).
                    let one = sr + sg + sbl >= 3 * 0x8000;
                    if one == im.invert {
                        put(&mut self.img, x, y, gs.fill.rgba);
                    }
                    continue;
                }
                self.compose(x, y, (sr, sg, sbl, sa));
            }
        }
        Ok(())
    }

    /// `compose`: source-over composites one premultiplied 16-bit source colour onto the canvas.
    fn compose(&mut self, x: i64, y: i64, src: (u32, u32, u32, u32)) {
        let (sr, sg, sb, sa) = src;
        match sa {
            0 => {}
            0xffff => put(
                &mut self.img,
                x,
                y,
                [(sr >> 8) as u8, (sg >> 8) as u8, (sb >> 8) as u8, 255],
            ),
            _ => {
                let Some(o) = self.img.pix_offset(x, y) else {
                    return;
                };
                let Some(d) = self.img.pix.get(o..o + 4) else {
                    return;
                };
                let d = [d[0], d[1], d[2], d[3]];
                let inv = 0xffff - sa;
                let blend = |s: u32, dc: u8| {
                    (s.wrapping_add(u32::from(dc) * 0x101 * inv / 0xffff) >> 8) as u8
                };
                put(
                    &mut self.img,
                    x,
                    y,
                    [
                        blend(sr, d[0]),
                        blend(sg, d[1]),
                        blend(sb, d[2]),
                        blend(sa, d[3]),
                    ],
                );
            }
        }
    }
}

/// `intersectClip`: the running clip intersected with the device bounding box of the path.
fn intersect_clip(clip: Option<ClipRect>, subs: &[Subpath]) -> Option<ClipRect> {
    let mut b: Option<ClipRect> = None;
    for pt in subs.iter().flat_map(|s| s.pts.iter()) {
        b = Some(match b {
            None => ClipRect {
                x0: pt.x,
                y0: pt.y,
                x1: pt.x,
                y1: pt.y,
            },
            Some(b) => ClipRect {
                x0: go_math_min(b.x0, pt.x),
                y0: go_math_min(b.y0, pt.y),
                x1: go_math_max(b.x1, pt.x),
                y1: go_math_max(b.y1, pt.y),
            },
        });
    }
    let Some(mut b) = b else {
        return clip;
    };
    if let Some(c) = clip {
        b.x0 = go_math_max(b.x0, c.x0);
        b.y0 = go_math_max(b.y0, c.y0);
        b.x1 = go_math_min(b.x1, c.x1);
        b.y1 = go_math_min(b.y1, c.y1);
    }
    Some(b)
}

fn curve_op(bud: &mut Budget, p: &mut Path, op: &[u8], nums: &[f64], m: &Matrix) -> Result<()> {
    let need = if op == b"c" { 6 } else { 4 };
    if nums.len() < need || !p.has {
        return Ok(());
    }
    let Some(n) = nums.get(nums.len() - need..) else {
        return Ok(());
    };
    let pt = |i: usize| -> Point {
        let (x, y) = m.apply(
            n.get(i).copied().unwrap_or(0.0),
            n.get(i + 1).copied().unwrap_or(0.0),
        );
        Point { x, y }
    };
    match op {
        b"c" => p.curve_to(bud, pt(0), pt(2), pt(4)),
        // v: the current point doubles as the first control point.
        b"v" => {
            let cur = p.cur;
            p.curve_to(bud, cur, pt(0), pt(2))
        }
        // y: the endpoint doubles as the second control point.
        _ => p.curve_to(bud, pt(0), pt(2), pt(2)),
    }
}

fn rect_op(bud: &mut Budget, p: &mut Path, nums: &[f64], m: &Matrix) -> Result<()> {
    let [.., x, y, w, h] = nums else {
        return Ok(());
    };
    let (x, y, w, h) = (*x, *y, *w, *h);
    let corner = |ux: f64, uy: f64| -> Point {
        let (dx, dy) = m.apply(ux, uy);
        Point { x: dx, y: dy }
    };
    // re is m l l l h (ISO 32000-1 8.5.2, table 59): a closed subpath whose current point is its
    // start.
    p.move_to(bud, corner(x, y))?;
    for pt in [corner(x + w, y), corner(x + w, y + h), corner(x, y + h)] {
        p.line_to(bud, pt)?;
    }
    p.close();
    Ok(())
}

/// `deviceScale`: how much the CTM magnifies lengths, exact for rotations and uniform scales.
fn device_scale(m: &Matrix) -> f64 {
    (m.0[0] * m.0[3] - m.0[1] * m.0[2]).abs().sqrt()
}

/// `numberOperands`.
fn number_operands(ops: &[Token<'_>]) -> Vec<f64> {
    ops.iter()
        .filter(|o| o.kind == Kind::Number)
        .map(|o| o.num)
        .collect()
}

/// `matrixOperands`: the last six numbers, if all finite.
fn matrix_operands(ops: &[Token<'_>]) -> Option<Matrix> {
    let nums = number_operands(ops);
    let [.., a, b, c, d, e, f] = nums.as_slice() else {
        return None;
    };
    let m = [*a, *b, *c, *d, *e, *f];
    if m.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(Matrix(m))
}

/// `lastNumber`: the last numeric operand. Non-finite values pass: like TJ's raw adjustments
/// they can only poison `tm`, and the glyph fill skips any glyph whose device coordinates come
/// out non-finite.
fn last_number(ops: &[Token<'_>]) -> Option<f64> {
    ops.iter()
        .rev()
        .find(|o| o.kind == Kind::Number)
        .map(|o| o.num)
}

/// `lastName`: the last name operand, or empty.
fn last_name<'t>(ops: &'t [Token<'_>]) -> &'t [u8] {
    ops.iter()
        .rev()
        .find(|o| o.kind == Kind::Name)
        .map_or(&b""[..], |o| o.text.as_ref())
}

/// `lastPoint`: the last two numbers mapped through `m`, unless the result is NaN.
fn last_point(ops: &[Token<'_>], m: &Matrix) -> Option<Point> {
    let nums = number_operands(ops);
    let [.., a, b] = nums.as_slice() else {
        return None;
    };
    let (x, y) = m.apply(*a, *b);
    if x.is_nan() || y.is_nan() {
        return None;
    }
    Some(Point { x, y })
}

#[cfg(test)]
#[path = "page_tests.rs"]
mod tests;
