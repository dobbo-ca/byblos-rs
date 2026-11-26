//! Text for the page rasteriser: port of byblos `internal/render/text.go`, plus `fillT1Glyph`
//! from `type1.go` (it drives the Type 1 interpreter through the renderer's path machinery).
//!
//! A [`Font`] is what the caller resolves from the page's `/Font` resources: the raw program
//! (an sfnt, a bare CFF or a Type 1) and the PDF-side widths. The program is parsed here, after
//! the pre-parse gates of `glyf.rs` and `cff.rs`, and a program none of the three parsers
//! accepts degrades to widths-only: the glyphs skip, but the advance still moves the text
//! matrix. Parsed programs are cached by content, so a resource dictionary aliasing one program
//! under many `Tf` names pays the parse once.

use super::cff::cff_to_sfnt;
use super::glyf::parse_glyf_index;
use super::page::{Budget, GState, Path, Point, Renderer, path_points};
use super::sfnt::fixed::{Int26_6, Point26_6};
use super::sfnt::{self, GlyphIndex, SegmentOp};
use super::substitute::{substitute_face, substitute_program};
use super::type1::{T1Error, T1Font, T1Sink, parse_type1};
use crate::content::Matrix;
use crate::error::Result;
use crate::limits::Limits;
use std::collections::HashMap;
use std::rc::Rc;

/// A simple or composite embedded font, resolved by the caller from the page's `/Font`
/// resources (Go's `render.Font`).
#[derive(Debug, Clone, Default)]
pub struct Font {
    /// The raw font program: `/FontFile2` sfnt bytes, `/FontFile3 /Subtype /Type1C` bare CFF
    /// bytes or a `/FontFile` Type 1, told apart by their headers. A program none of the parsers
    /// accepts makes every glyph skip cleanly; the widths still advance. With no program at all
    /// a bundled substitute face is picked from `base_font` and `flags`.
    pub program: Vec<u8>,
    /// The font dict's `/FirstChar` and `/Widths`, in thousandths of text space (ISO 32000-1
    /// 9.2.4). A code outside the array advances by `/MissingWidth`'s default, 0. A dict with no
    /// `/Widths` at all (the standard-14 shape) advances by the resolved program's own metrics.
    pub first_char: i64,
    pub widths: Vec<f64>,
    /// The font dict's `/BaseFont` and its `/FontDescriptor`'s `/Flags`. They are read only when
    /// `program` is empty, to pick a bundled substitute face.
    pub base_font: Vec<u8>,
    pub flags: i64,
    /// A composite font: `/Subtype /Type0` with an Identity-H `/Encoding` whose descendant
    /// carries the program (a CID-keyed CFF). The string is then decoded as 2-byte big-endian
    /// codes, each the CID itself, and `w`/`dw` replace `first_char`/`widths`. The caller must
    /// not set it for an embedded-CMap encoding, a predefined CJK CMap or Identity-V.
    pub type0: bool,
    /// The descendant CIDFont's `/W`, flattened to CID to width in thousandths, and its `/DW`
    /// already resolved: the font's `/DW` when present, else the spec default 1000. The caller
    /// must set `dw` for a Type0 font.
    pub w: HashMap<u16, f64>,
    pub dw: f64,
}

/// `FontFor`: resolves a `Tf` operand to an embedded font. `None` skips that font's shows
/// entirely, advance included; a supplied but unparsable program keeps its widths advancing.
pub type FontFor<'a> = dyn FnMut(&[u8]) -> Option<Font> + 'a;

/// The cache key of a parsed font program: the bundled face it was taken from, or its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ProgKey {
    Face(&'static str),
    Bytes(Vec<u8>),
}

/// `fontProg`: one parsed font program.
#[derive(Default)]
pub(super) struct FontProg {
    fnt: Option<sfnt::Font>,
    t1: Option<T1Font>,
    upem: f64,
    /// Per-GID gated charstring work (CFF only); `fill_gid` charges it.
    gwork: Vec<i64>,
    /// The CID-keyed CFF's charset as a CID map; `None` for any other program.
    cid2gid: Option<HashMap<u16, u16>>,
}

/// `textFont`: one `Tf` resolution, the parsed program plus the PDF-side widths.
pub(super) struct TextFont {
    prog: Rc<FontProg>,
    first: i64,
    pub(super) width: Vec<f64>,
    type0: bool,
    wmap: HashMap<u16, f64>,
    dw: f64,
}

/// Parses `src` as an sfnt into `prog`; a parse failure leaves the font widths-only.
fn parse_sfnt(src: &[u8], prog: &mut FontProg, lim: &Limits) {
    if let Ok(f) = sfnt::Font::parse(src, lim) {
        prog.upem = f64::from(f.units_per_em());
        prog.fnt = Some(f);
    }
}

/// Parses a font program, refusing before allocating and before `sfnt::Font::parse`: parse
/// itself loads whole glyphs (`x` and `H`, the OS/2 metrics fallback), so a hostile compound
/// bomb must be caught first (`glyf.rs`). A program whose glyphs cannot all be bounded degrades
/// to widths-only exactly like an unparsable one.
fn build_prog(program: &[u8], lim: &Limits) -> FontProg {
    let mut prog = FontProg::default();
    if let Some(g) = parse_glyf_index(program) {
        if g.bounded_by(lim.path_points()) {
            parse_sfnt(program, &mut prog, lim);
        }
    } else if let Some(c) = cff_to_sfnt(program) {
        // A bare CFF, gated and wrapped by cff.rs into the container sfnt parses. `cid2gid` is
        // `Some` only for a CID-keyed CFF.
        parse_sfnt(&c.otf, &mut prog, lim);
        prog.gwork = c.gate_work;
        prog.cid2gid = c.cid2gid;
    } else {
        // A classic Type 1. Anything none of the three parsers accept stays widths-only.
        prog.t1 = parse_type1(program);
    }
    prog
}

impl Renderer<'_> {
    /// `resolveFont`: resolves and caches a `Tf` operand. A `None` cache entry records a name
    /// `FontFor` could not supply, so a stream that repeats the `Tf` pays once.
    pub(super) fn resolve_font(&mut self, name: &[u8]) -> Option<Rc<TextFont>> {
        if let Some(f) = self.fonts_by.get(name) {
            return f.clone();
        }
        let src = self.fetch_font(name);
        let Some(src) = src else {
            self.fonts_by.insert(name.to_vec(), None);
            return None;
        };
        // For an embedded program the key is its bytes. With none, a bundled face is
        // substituted by name, else by descriptor flags; the face path is the key, so the file
        // is read only on a miss. The deferred faces leave the program empty and degrade to
        // widths-only.
        let mut program: &[u8] = &src.program;
        let key = if program.is_empty() {
            let face = substitute_face(&src.base_font, src.flags);
            let key = ProgKey::Face(face.unwrap_or(""));
            if !self.progs_by.contains_key(&key)
                && face.is_some()
                && let Some(b) = substitute_program(&src.base_font, src.flags)
            {
                program = b;
            }
            key
        } else {
            let mut k = Vec::new();
            if k.try_reserve_exact(program.len()).is_err() {
                self.fonts_by.insert(name.to_vec(), None);
                return None;
            }
            k.extend_from_slice(program);
            ProgKey::Bytes(k)
        };
        let prog = match self.progs_by.get(&key) {
            Some(p) => Rc::clone(p),
            None => {
                let p = Rc::new(build_prog(program, self.lim));
                self.progs_by.insert(key, Rc::clone(&p));
                p
            }
        };
        // Charged once per NAME resolved, whether or not the program was already cached: the
        // caller's decode is repeated for every name, and /Widths and /W are never deduped, so
        // they are charged the same way, per name.
        self.font_budget = self
            .font_budget
            .saturating_add(program.len() as i64)
            .saturating_add(8 * src.widths.len() as i64)
            .saturating_add(16 * src.w.len() as i64);
        if self.font_budget > self.lim.render_font_bytes() {
            self.fonts_by.insert(name.to_vec(), None);
            return None;
        }
        let tf = Rc::new(TextFont {
            prog,
            first: src.first_char,
            width: src.widths,
            type0: src.type0,
            wmap: src.w,
            dw: src.dw,
        });
        self.fonts_by.insert(name.to_vec(), Some(Rc::clone(&tf)));
        Some(tf)
    }

    /// The caller's font for `name`, or `None` when there is no resolver, the name is empty, the
    /// font budget is already spent (refused before even asking the caller, so a long run of
    /// names past the limit cannot keep paying the caller's decode cost) or the caller has none.
    fn fetch_font(&mut self, name: &[u8]) -> Option<Font> {
        if name.is_empty() || self.font_budget >= self.lim.render_font_bytes() {
            return None;
        }
        let resolve = self.fonts.as_mut()?;
        resolve(name)
    }

    /// `showText`: shows one string operand's codes. For each code the glyph is inked under the
    /// text rendering matrix, then the text matrix advances by the PDF width plus spacing (ISO
    /// 32000-1 9.4.4). A simple font's codes are the bytes; a Type0 font's are 2-byte big-endian
    /// CIDs.
    pub(super) fn show_text(&mut self, gs: &mut GState, raw: &[u8]) -> Result<()> {
        let Some(f) = gs.font.clone() else {
            return Ok(());
        };
        let ink = inks_text(gs.tr);
        if f.type0 {
            // The Identity CMap: the code is the CID (9.7.6.2). Word spacing never applies (it is
            // defined for the single-byte code 32 only, 9.3.3), and a dangling odd byte has no
            // code to belong to, so it is dropped.
            for pair in raw.chunks_exact(2) {
                if let [a, b] = pair {
                    let cid = u16::from_be_bytes([*a, *b]);
                    if ink {
                        self.fill_cid_glyph(gs, &f, cid)?;
                    }
                    advance(gs, f.w0_cid(cid), false);
                }
            }
            return Ok(());
        }
        for &code in raw {
            if ink {
                self.fill_glyph(gs, &f, code)?;
            }
            // Word spacing applies to single-byte code 32 (9.3.3).
            advance(gs, f.w0(code), code == b' ');
        }
        Ok(())
    }

    /// `fillGlyph`: rasterises one glyph outline through the same path machinery as the path
    /// operators. Flattened points charge the path budget and the fill budget, and the fill is
    /// the scanline filler under the nonzero rule. Any failure short of a budget (no glyph for
    /// the code, a load error, a non-finite device coordinate) skips the glyph cleanly.
    fn fill_glyph(&mut self, gs: &GState, f: &TextFont, code: u8) -> Result<()> {
        if f.prog.t1.is_some() {
            return self.fill_t1_glyph(gs, f, code);
        }
        let Some(fnt) = f.prog.fnt.as_ref() else {
            return Ok(());
        };
        match fnt.glyph_index(code_rune(code)) {
            Ok(gi) if gi.0 != 0 => self.fill_gid(gs, f, gi),
            _ => Ok(()),
        }
    }

    /// `fillCIDGlyph`: CID to GID through the CID-keyed CFF's charset, then the shared GID path.
    /// A Type0 font whose program is anything else inks nothing, deliberately: that program's
    /// glyphs were work-gated for the 256 single-byte codes alone.
    fn fill_cid_glyph(&mut self, gs: &GState, f: &TextFont, cid: u16) -> Result<()> {
        let Some(map) = f.prog.cid2gid.as_ref() else {
            return Ok(());
        };
        if f.prog.fnt.is_none() {
            return Ok(());
        }
        match map.get(&cid) {
            Some(&gid) if gid != 0 => self.fill_gid(gs, f, GlyphIndex(gid)),
            _ => Ok(()),
        }
    }

    /// `fillGID`: loads and fills one glyph by index.
    fn fill_gid(&mut self, gs: &GState, f: &TextFont, gi: GlyphIndex) -> Result<()> {
        // Charge the charstring work the load is about to execute (measured by cff.rs's gate)
        // before executing it. A glyph can run ~64 KB of charstring yet emit zero segments, so
        // the per-point charge below would price its re-shows at nothing.
        if let Some(&w) = f.prog.gwork.get(usize::from(gi.0)) {
            self.bud.charge_fill(w)?;
        }
        let mut pth = Path::default();
        let res = self.fill_gid_path(gs, f, gi, &mut pth);
        pth.reset(&mut self.bud);
        res
    }

    fn fill_gid_path(
        &mut self,
        gs: &GState,
        f: &TextFont,
        gi: GlyphIndex,
        pth: &mut Path,
    ) -> Result<()> {
        let Some(fnt) = f.prog.fnt.as_ref() else {
            return Ok(());
        };
        // ppem = upem hands segment coordinates back in raw font units with no rounding, keeping
        // the only scaling in float space under glyph_matrix.
        let ppem = Int26_6(f.prog.upem as i32);
        let segs = match fnt.load_glyph(&mut self.buf, gi, ppem) {
            Ok(s) => s,
            // Resource errors (allocation, segment caps) abort the render; the rest skip the glyph.
            Err(sfnt::Error::Resource(e)) => return Err(e),
            Err(_) => return Ok(()),
        };
        let m = glyph_matrix(gs, f.prog.upem, -1.0);
        let mut ok = true;
        for seg in segs {
            if !ok {
                break;
            }
            let [a0, a1, a2] = seg.args;
            match seg.op {
                SegmentOp::MoveTo => {
                    let p = dev(&m, a0, &mut ok);
                    pth.move_to(&mut self.bud, p)?;
                }
                SegmentOp::LineTo => {
                    let p = dev(&m, a0, &mut ok);
                    pth.line_to(&mut self.bud, p)?;
                }
                SegmentOp::QuadTo => {
                    // Exact degree elevation to the cubic the flattener speaks:
                    // c1 = p0 + 2/3(q-p0), c2 = p2 + 2/3(q-p2), computed in device space.
                    let q = dev(&m, a0, &mut ok);
                    let end = dev(&m, a1, &mut ok);
                    let p0 = pth.cur;
                    let c1 = Point {
                        x: p0.x + 2.0 * (q.x - p0.x) / 3.0,
                        y: p0.y + 2.0 * (q.y - p0.y) / 3.0,
                    };
                    let c2 = Point {
                        x: end.x + 2.0 * (q.x - end.x) / 3.0,
                        y: end.y + 2.0 * (q.y - end.y) / 3.0,
                    };
                    pth.curve_to(&mut self.bud, c1, c2, end)?;
                }
                SegmentOp::CubeTo => {
                    // sfnt emits cubics for CFF outlines, already in the form the flattener
                    // speaks.
                    let c1 = dev(&m, a0, &mut ok);
                    let c2 = dev(&m, a1, &mut ok);
                    let end = dev(&m, a2, &mut ok);
                    pth.curve_to(&mut self.bud, c1, c2, end)?;
                }
            }
        }
        if !ok {
            return Ok(());
        }
        self.bud.charge_fill(path_points(&pth.subs))?;
        let clip = self.device_clip(gs.clip);
        self.fill_subpaths(&pth.subs, false, clip, gs.fill.rgba)
    }

    /// `fillT1Glyph`: rasterises one Type 1 glyph through the same path machinery as
    /// [`Renderer::fill_glyph`]: interpreter work and flattened points charge the budgets, and
    /// the fill is the scanline filler under the nonzero rule. Any failure short of a budget
    /// skips the glyph cleanly.
    fn fill_t1_glyph(&mut self, gs: &GState, f: &TextFont, code: u8) -> Result<()> {
        let mut pth = Path::default();
        let res = self.fill_t1_path(gs, f, code, &mut pth);
        pth.reset(&mut self.bud);
        res
    }

    fn fill_t1_path(&mut self, gs: &GState, f: &TextFont, code: u8, pth: &mut Path) -> Result<()> {
        let Some(t1) = f.prog.t1.as_ref() else {
            return Ok(());
        };
        let Some(cs) = t1.glyph(code) else {
            return Ok(());
        };
        // Type 1 charstring space is y-up already: no flip.
        let m = glyph_matrix(gs, t1.upem, 1.0);
        let mut work: i64 = 0;
        let res = {
            let mut sink = PathSink {
                m,
                pth,
                bud: &mut self.bud,
            };
            t1.interpret(cs, &mut sink, &mut work)
        };
        // Charge the interpreter's executed bytes per SHOW, like the CFF gate total: a stream
        // re-showing one expensive glyph stays bounded per page.
        self.bud.charge_fill(work)?;
        match res {
            Err(T1Error::Skip) => return Ok(()),
            Err(T1Error::Sink(e)) => return Err(e),
            Ok(()) => {}
        }
        self.bud.charge_fill(path_points(&pth.subs))?;
        let clip = self.device_clip(gs.clip);
        self.fill_subpaths(&pth.subs, false, clip, gs.fill.rgba)
    }
}

/// The sink the Type 1 interpreter draws into: font-unit outline commands, mapped to device
/// space and appended to the renderer's path.
struct PathSink<'a> {
    m: Matrix,
    pth: &'a mut Path,
    bud: &'a mut Budget,
}

impl PathSink<'_> {
    /// Maps a font-unit point to device space. One finite check covers every hostile text
    /// parameter, at the only place they meet the scanline filler.
    fn dev(&self, fx: f64, fy: f64) -> std::result::Result<Point, T1Error> {
        let (x, y) = self.m.apply(fx, fy);
        if !x.is_finite() || !y.is_finite() {
            return Err(T1Error::Skip);
        }
        Ok(Point { x, y })
    }
}

impl T1Sink for PathSink<'_> {
    fn move_to(&mut self, x: f64, y: f64) -> std::result::Result<(), T1Error> {
        let p = self.dev(x, y)?;
        self.pth.move_to(self.bud, p).map_err(T1Error::Sink)
    }

    fn line_to(&mut self, x: f64, y: f64) -> std::result::Result<(), T1Error> {
        let p = self.dev(x, y)?;
        self.pth.line_to(self.bud, p).map_err(T1Error::Sink)
    }

    fn curve_to(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
    ) -> std::result::Result<(), T1Error> {
        let c1 = self.dev(x1, y1)?;
        let c2 = self.dev(x2, y2)?;
        let end = self.dev(x3, y3)?;
        self.pth
            .curve_to(self.bud, c1, c2, end)
            .map_err(T1Error::Sink)
    }
}

/// Maps one sfnt glyph point (raw font units) to device space, clearing `ok` when the result is
/// not finite.
fn dev(m: &Matrix, p: Point26_6, ok: &mut bool) -> Point {
    let (x, y) = m.apply(f64::from(p.x.0), f64::from(p.y.0));
    // One finite check covers every hostile text parameter at the only place they meet the
    // scanline filler: a NaN or Inf here (a huge Tfs, an Inf TJ adjustment poisoning tm) would
    // otherwise reach the int conversions.
    if !x.is_finite() || !y.is_finite() {
        *ok = false;
    }
    Point { x, y }
}

/// `advance`: moves the text matrix by one code's displacement (9.4.4), so the Tc/Tz arithmetic
/// exists exactly once.
fn advance(gs: &mut GState, w0: f64, word_sp: bool) {
    let mut tx = gs.font_size * w0 + gs.char_sp;
    if word_sp {
        tx += gs.word_sp;
    }
    gs.tm = Matrix([1.0, 0.0, 0.0, 1.0, tx * gs.hscale, 0.0]).mul(&gs.tm);
}

/// `glyphMatrix`: maps glyph coordinates (font units) to device space: scale by 1/unitsPerEm into
/// text space, then the text-space parameters, the text matrix and the CTM (Trm, ISO 32000-1
/// 9.4.4). `y_sign` is -1 for sfnt glyphs (font units y down, `load_glyph`'s orientation) and +1
/// for Type 1 charstring space, which is y up already.
fn glyph_matrix(gs: &GState, upem: f64, y_sign: f64) -> Matrix {
    let params = Matrix([
        gs.font_size * gs.hscale,
        0.0,
        0.0,
        gs.font_size,
        0.0,
        gs.rise,
    ]);
    Matrix([1.0 / upem, 0.0, 0.0, y_sign / upem, 0.0, 0.0])
        .mul(&params)
        .mul(&gs.tm)
        .mul(&gs.ctm)
}

impl TextFont {
    /// `w0`: the code's glyph displacement in text space (ISO 32000-1 9.4.4): `/Widths` in
    /// thousandths where the array covers the code, else `/MissingWidth`, whose default is 0.
    /// Deliberately not the font program's own advance where any `/Widths` exist: the PDF widths
    /// override it by spec. A dict with no `/Widths` at all (the standard-14 shape) takes the
    /// resolved program's own advance instead; for a substituted face those are the
    /// metric-compatible widths.
    fn w0(&self, code: u8) -> f64 {
        let i = i64::from(code).wrapping_sub(self.first);
        if let Ok(i) = usize::try_from(i)
            && let Some(w) = self.width.get(i)
        {
            return w / 1000.0;
        }
        if self.width.is_empty() {
            if let Some(t1) = self.prog.t1.as_ref() {
                // The charstring's own hsbw width, in font units.
                return t1.advance(code) / t1.upem;
            }
            // ppem = upem returns the advance in raw font units, the same convention the glyph
            // fill uses for the load.
            if let Some(fnt) = self.prog.fnt.as_ref()
                && let Ok(gi) = fnt.glyph_index(code_rune(code))
                && gi.0 != 0
                && let Ok(adv) = fnt.glyph_advance(gi, Int26_6(self.prog.upem as i32))
            {
                return f64::from(adv.0) / self.prog.upem;
            }
        }
        0.0
    }

    /// `w0CID`: the CID's glyph displacement in text space: `/W` in thousandths where present,
    /// else `/DW` (already resolved by the caller, so an explicit `/DW 0` advances 0, not 1000).
    fn w0_cid(&self, cid: u16) -> f64 {
        match self.wmap.get(&cid) {
            Some(w) => w / 1000.0,
            None => self.dw / 1000.0,
        }
    }
}

/// `winAnsiC1`: codes 0x80..0x9f, where WinAnsi (CP1252) departs from Latin-1's C1 controls, as
/// runes; 0 for the five undefined slots.
const WIN_ANSI_C1: [i32; 32] = [
    0x20ac, 0, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039,
    0x0152, 0, 0x017d, 0, 0, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014, 0x02dc,
    0x2122, 0x0161, 0x203a, 0x0153, 0, 0x017e, 0x0178,
];

/// `codeRune`: maps a single-byte code to a rune: Latin-1, which agrees with WinAnsi/Standard
/// over ASCII, except that 0x80..0x9f take their WinAnsi meanings (smart quotes, dashes, euro).
fn code_rune(code: u8) -> i32 {
    if (0x80..=0x9f).contains(&code)
        && let Some(&r) = WIN_ANSI_C1.get(usize::from(code - 0x80))
        && r != 0
    {
        return r;
    }
    i32::from(code)
}

/// `inksText`: whether rendering mode `tr` deposits ink: the filling modes 0, 2, 4 and 6 (ISO
/// 32000-1 table 106). The stroke-only modes 1 and 5 and the clip-only mode 7 are deferred (their
/// glyphs advance but do not mark), as is the invisible mode 3. A mode outside the table marks
/// nothing.
fn inks_text(tr: i64) -> bool {
    matches!(tr, 0 | 2 | 4 | 6)
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
