//! Wave 1e: port of byblos golang.org/x/image/font/sfnt (v0.47.0: `sfnt.go`, `truetype.go`,
//! `postscript.go`, `cmap.go`, plus the parse-time half of `gpos.go`) and
//! x/image/math/fixed (`fixed.go`, in [`fixed`]).
//!
//! Only what `internal/render/text.go` uses is ported: [`Font::parse`], [`Font::units_per_em`],
//! [`Font::glyph_index`], [`Font::glyph_advance`] (hinting is always `HintingNone`),
//! [`Font::load_glyph`] (to [`Segment`]s) and [`Buffer`]. Every error `Parse` can return is
//! kept, because a parse failure decides whether byblos skips a font.
//!
//! Not ported, because byblos never reaches them: collections and dfont (`ParseCollection`),
//! `ParseReaderAt`, `Name`, `GlyphName` and the built-in post names (`data.go`), `Kern`,
//! `Bounds`, `GlyphBounds`, `Metrics`, `WriteSourceTo`. The `kern` and `GPOS` tables are still
//! walked and validated exactly as `Parse` does, so they fail a parse the same way, but the
//! kerning lookups they would build are not kept.
//!
//! Differences from Go (docs/go-differences.md):
//!
//! * [`Font`] owns a copy of the font program, so it has no lifetime.
//! * Allocations sized by the font are charged against `Limits::max_alloc` before they are made
//!   ([`Error::Resource`]), and [`Buffer`] stops a glyph that expands past
//!   `Limits::max_path_points` segments with `Limit("path-points")`. Go has neither.
//! * Where Go would panic on an out-of-range slice index in the GPOS walk, the parse fails with
//!   [`Error::Invalid`] instead.
//! * `Buffer` is required (Go accepts a nil one) and the `Buffer` argument of `GlyphIndex` and
//!   `GlyphAdvance` is dropped, since a `[]byte` source needs no scratch space.

pub mod fixed;

mod cmap;
mod gpos;
mod postscript;
mod truetype;

use crate::limits::Limits;
use fixed::{Int26_6, Point26_6};
use std::fmt;

/// `Units`: an integral number of font units.
pub type Units = i32;

/// `GlyphIndex`: a glyph index in a [`Font`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlyphIndex(pub u16);

// Limits that are not part of the specifications but of the Go implementation.
pub(crate) const MAX_CMAP_SEGMENTS: u32 = 20000;
pub(crate) const MAX_NUM_SUBROUTINES: i32 = 40000;
pub(crate) const MAX_COMPOUND_RECURSION_DEPTH: u32 = 8;
pub(crate) const MAX_COMPOUND_STACK_SIZE: u32 = 64;
pub(crate) const MAX_GLYPH_DATA_LENGTH: u32 = 64 * 1024;
pub(crate) const MAX_HINT_BITS: i32 = 256;
pub(crate) const MAX_NUM_FONT_DICTS: i32 = 256;
const MAX_NUM_TABLES: usize = 256;
// (MAX_TABLE_OFFSET + MAX_TABLE_LENGTH) does not overflow an i32.
const MAX_TABLE_LENGTH: u32 = 1 << 29;
const MAX_TABLE_OFFSET: u32 = 1 << 29;

/// What `sfnt` can fail with. [`Error::NotFound`] and [`Error::ColoredGlyph`] are Go's
/// `ErrNotFound` and `ErrColoredGlyph`; every other Go error is [`Error::Invalid`] with its
/// message (the constants below name them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `ErrNotFound`: the requested value was not found.
    NotFound,
    /// `ErrColoredGlyph`: the glyph is not a monochrome vector glyph.
    ColoredGlyph,
    /// An unexported Go error variable (`errInvalidFont` and so on), by its message.
    Invalid(&'static str),
    /// The two `fmt.Errorf` errors of the PostScript interpreter: an operator byte (after the
    /// escape byte 12 if `escaped`) that no context knows.
    UnrecognizedOperator { escaped: bool, byte: u8 },
    /// A byblos limit or allocation refusal (`Alloc`, `OutOfMemory`, `Limit`), which Go has no
    /// counterpart for.
    Resource(crate::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => f.write_str("sfnt: not found"),
            Error::ColoredGlyph => f.write_str("sfnt: colored glyph"),
            Error::Invalid(s) => f.write_str(s),
            Error::UnrecognizedOperator {
                escaped: true,
                byte,
            } => {
                write!(f, "sfnt: unrecognized CFF 2-byte operator (12 {byte})")
            }
            Error::UnrecognizedOperator {
                escaped: false,
                byte,
            } => write!(f, "sfnt: unrecognized CFF 1-byte operator ({byte})"),
            Error::Resource(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::collections::TryReserveError> for Error {
    fn from(_: std::collections::TryReserveError) -> Self {
        Error::Resource(crate::Error::OutOfMemory)
    }
}

impl From<Error> for crate::Error {
    /// Resource errors keep their meaning; every other sfnt error is an untyped Go error, which
    /// the oracle classes as `error`.
    fn from(e: Error) -> Self {
        match e {
            Error::Resource(r) => r,
            other => crate::Error::Malformed(other.to_string()),
        }
    }
}

/// `Result` with the sfnt [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

pub const INVALID_BOUNDS: Error = Error::Invalid("sfnt: invalid bounds");
pub const INVALID_CFF_TABLE: Error = Error::Invalid("sfnt: invalid CFF table");
pub const INVALID_CMAP_TABLE: Error = Error::Invalid("sfnt: invalid cmap table");
pub const INVALID_FONT: Error = Error::Invalid("sfnt: invalid font");
pub const INVALID_GPOS_TABLE: Error = Error::Invalid("sfnt: invalid GPOS table");
pub const INVALID_GLYPH_DATA: Error = Error::Invalid("sfnt: invalid glyph data");
pub const INVALID_GLYPH_DATA_LENGTH: Error = Error::Invalid("sfnt: invalid glyph data length");
pub const INVALID_HEAD_TABLE: Error = Error::Invalid("sfnt: invalid head table");
pub const INVALID_HHEA_TABLE: Error = Error::Invalid("sfnt: invalid hhea table");
pub const INVALID_HMTX_TABLE: Error = Error::Invalid("sfnt: invalid hmtx table");
pub const INVALID_KERN_TABLE: Error = Error::Invalid("sfnt: invalid kern table");
pub const INVALID_LOCA_TABLE: Error = Error::Invalid("sfnt: invalid loca table");
pub const INVALID_LOCATION_DATA: Error = Error::Invalid("sfnt: invalid location data");
pub const INVALID_MAXP_TABLE: Error = Error::Invalid("sfnt: invalid maxp table");
pub const INVALID_OS2_TABLE: Error = Error::Invalid("sfnt: invalid OS/2 table");
pub const INVALID_POST_TABLE: Error = Error::Invalid("sfnt: invalid post table");
pub const INVALID_SINGLE_FONT: Error =
    Error::Invalid("sfnt: invalid single font (data is a font collection)");
pub const INVALID_SOURCE_DATA: Error = Error::Invalid("sfnt: invalid source data");
pub const INVALID_TABLE_OFFSET: Error = Error::Invalid("sfnt: invalid table offset");
pub const INVALID_TABLE_TAG_ORDER: Error = Error::Invalid("sfnt: invalid table tag order");
pub const UNSUPPORTED_CFF_FD_SELECT_TABLE: Error =
    Error::Invalid("sfnt: unsupported CFF FDSelect table");
pub const UNSUPPORTED_CFF_VERSION: Error = Error::Invalid("sfnt: unsupported CFF version");
pub const UNSUPPORTED_CLASS_DEF_FORMAT: Error =
    Error::Invalid("sfnt: unsupported class definition format");
pub const UNSUPPORTED_CMAP_ENCODINGS: Error = Error::Invalid("sfnt: unsupported cmap encodings");
pub const UNSUPPORTED_COMPOUND_GLYPH: Error = Error::Invalid("sfnt: unsupported compound glyph");
pub const UNSUPPORTED_COVERAGE_FORMAT: Error = Error::Invalid("sfnt: unsupported coverage format");
pub const UNSUPPORTED_EXTENSION_POS_FORMAT: Error =
    Error::Invalid("sfnt: unsupported extension positioning format");
pub const UNSUPPORTED_GPOS_TABLE: Error = Error::Invalid("sfnt: unsupported GPOS table");
pub const UNSUPPORTED_GLYPH_DATA_LENGTH: Error =
    Error::Invalid("sfnt: unsupported glyph data length");
pub const UNSUPPORTED_KERN_TABLE: Error = Error::Invalid("sfnt: unsupported kern table");
pub const UNSUPPORTED_NUMBER_OF_CMAP_SEGMENTS: Error =
    Error::Invalid("sfnt: unsupported number of cmap segments");
pub const UNSUPPORTED_NUMBER_OF_FONT_DICTS: Error =
    Error::Invalid("sfnt: unsupported number of font dicts");
pub const UNSUPPORTED_NUMBER_OF_HINTS: Error = Error::Invalid("sfnt: unsupported number of hints");
pub const UNSUPPORTED_NUMBER_OF_SUBROUTINES: Error =
    Error::Invalid("sfnt: unsupported number of subroutines");
pub const UNSUPPORTED_NUMBER_OF_TABLES: Error =
    Error::Invalid("sfnt: unsupported number of tables");
pub const UNSUPPORTED_POST_TABLE: Error = Error::Invalid("sfnt: unsupported post table");
pub const UNSUPPORTED_REAL_NUMBER_ENCODING: Error =
    Error::Invalid("sfnt: unsupported real number encoding");
pub const UNSUPPORTED_TABLE_OFFSET_LENGTH: Error =
    Error::Invalid("sfnt: unsupported table offset or length");
pub const UNSUPPORTED_TYPE2_CHARSTRING: Error =
    Error::Invalid("sfnt: unsupported Type 2 Charstring");
/// Go panics (an index out of range) where the Rust port returns this; see the module docs.
pub const GO_PANIC: Error = Error::Invalid("sfnt: index out of range (a Go panic)");

/// Go's `u16`: the big-endian `u16` at the start of `b`. Callers pass a slice whose length was
/// checked by a `view`; a shorter one reads as 0 rather than panicking.
pub(crate) fn u16(b: &[u8]) -> u16 {
    match b {
        [a, c, ..] => u16::from_be_bytes([*a, *c]),
        _ => 0,
    }
}

/// Go's `u32`, with the same contract as [`u16`].
pub(crate) fn u32(b: &[u8]) -> u32 {
    match b {
        [a, c, d, e, ..] => u32::from_be_bytes([*a, *c, *d, *e]),
        _ => 0,
    }
}

/// `b[i..]` for a slice index Go would panic on when it is out of range.
pub(crate) fn tail(b: &[u8], i: i64) -> Result<&[u8]> {
    usize::try_from(i)
        .ok()
        .and_then(|i| b.get(i..))
        .ok_or(GO_PANIC)
}

/// Go's `u16(b[i:])` with the panic turned into [`GO_PANIC`].
pub(crate) fn try_u16(b: &[u8], i: i64) -> Result<u16> {
    let t = tail(b, i)?;
    if t.len() < 2 {
        return Err(GO_PANIC);
    }
    Ok(u16(t))
}

/// Go's `u32(b[i:])` with the panic turned into [`GO_PANIC`].
pub(crate) fn try_u32(b: &[u8], i: i64) -> Result<u32> {
    let t = tail(b, i)?;
    if t.len() < 4 {
        return Err(GO_PANIC);
    }
    Ok(u32(t))
}

/// `source.view` for a `[]byte` source: the `length` bytes at `offset`.
pub(crate) fn view(src: &[u8], offset: i64, length: i64) -> Result<&[u8]> {
    let end = offset.checked_add(length).ok_or(INVALID_BOUNDS)?;
    if offset < 0 || offset > end {
        return Err(INVALID_BOUNDS);
    }
    let (Ok(lo), Ok(hi)) = (usize::try_from(offset), usize::try_from(end)) else {
        return Err(INVALID_BOUNDS);
    };
    src.get(lo..hi).ok_or(INVALID_BOUNDS)
}

/// `source.varLenView`: a sub-table of `static_length` bytes plus `n * item_length`, where `n`
/// is the `u16` at `count_offset`. Returns the bytes and `n`.
pub(crate) fn var_len_view(
    src: &[u8],
    offset: i64,
    static_length: i64,
    count_offset: i64,
    item_length: i64,
) -> Result<(&[u8], i64)> {
    if offset < 0 || offset > offset + static_length {
        return Err(INVALID_BOUNDS);
    }
    if count_offset < 0 || count_offset + 1 >= static_length {
        return Err(INVALID_BOUNDS);
    }
    let head = view(src, offset, static_length)?;
    let count = i64::from(try_u16(head, count_offset)?);
    let all = view(src, offset, static_length + count * item_length)?;
    Ok((all, count))
}

/// A section of the font data.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Table {
    pub(crate) offset: u32,
    pub(crate) length: u32,
}

impl Table {
    /// `source.u16`: the `u16` at the relative offset `i`.
    pub(crate) fn u16(&self, src: &[u8], i: i64) -> Result<u16> {
        if i < 0 || i64::from(self.length) < i + 2 {
            return Err(INVALID_BOUNDS);
        }
        Ok(u16(view(src, i64::from(self.offset) + i, 2)?))
    }

    /// `source.u32`: the `u32` at the relative offset `i`.
    pub(crate) fn u32(&self, src: &[u8], i: i64) -> Result<u32> {
        if i < 0 || i64::from(self.length) < i + 4 {
            return Err(INVALID_BOUNDS);
        }
        Ok(u32(view(src, i64::from(self.offset) + i, 4)?))
    }
}

/// The per-parse allocation budget: font-sized buffers are charged against `max_alloc` before
/// they are allocated.
pub(crate) struct Budget(u64);

impl Budget {
    /// Charges `bytes`, or an [`Error::Resource`] if the budget is spent.
    pub(crate) fn charge(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.0 {
            return Err(Error::Resource(crate::Error::Alloc {
                got: bytes,
                max: self.0,
            }));
        }
        self.0 -= bytes;
        Ok(())
    }

    /// An empty `Vec<T>` with room for `n` items, or an [`Error::Resource`].
    pub(crate) fn vec<T>(&mut self, n: usize) -> Result<Vec<T>> {
        self.charge((n as u64).saturating_mul(std::mem::size_of::<T>().max(1) as u64))?;
        let mut v = Vec::new();
        v.try_reserve_exact(n)?;
        Ok(v)
    }
}

/// `scale`: `x` divided by `units_per_em`, rounded to the nearest 1/64th.
fn scale(x: Int26_6, units_per_em: Units) -> Int26_6 {
    let half = units_per_em / 2;
    let x = if x.0 >= 0 {
        x.0.wrapping_add(half)
    } else {
        x.0.wrapping_sub(half)
    };
    Int26_6(x.checked_div(units_per_em).unwrap_or(0))
}

/// `Segment`: one segment of a vector path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// The operator.
    pub op: SegmentOp,
    /// Up to three (x, y) coordinates. The Y axis increases down.
    pub args: [Point26_6; 3],
}

/// `SegmentOp`: a vector path segment's operator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SegmentOp {
    #[default]
    MoveTo,
    LineTo,
    QuadTo,
    CubeTo,
}

/// The segment list of a [`Buffer`], with its size cap.
pub(crate) struct SegList {
    pub(crate) v: Vec<Segment>,
    max: usize,
}

impl SegList {
    pub(crate) fn new(max: usize) -> SegList {
        SegList { v: Vec::new(), max }
    }

    pub(crate) fn push(&mut self, s: Segment) -> Result<()> {
        if self.v.len() >= self.max {
            return Err(Error::Resource(crate::Error::Limit {
                name: "path-points",
                got: self.v.len() as i64 + 1,
                max: self.max as i64,
            }));
        }
        self.v.try_reserve(1)?;
        self.v.push(s);
        Ok(())
    }
}

/// One component of a TrueType compound glyph.
#[derive(Clone, Copy, Default)]
pub(crate) struct CompoundElem {
    pub(crate) glyph_index: GlyphIndex,
    pub(crate) dx: i16,
    pub(crate) dy: i16,
    pub(crate) has_transform: bool,
    pub(crate) xx: i16,
    pub(crate) xy: i16,
    pub(crate) yx: i16,
    pub(crate) yy: i16,
}

/// `Buffer`: the re-usable scratch space of [`Font::load_glyph`]. The segments it returns are
/// valid until the buffer is used again.
pub struct Buffer {
    pub(crate) segs: SegList,
    pub(crate) compound_stack: [CompoundElem; MAX_COMPOUND_STACK_SIZE as usize],
}

impl Buffer {
    /// A buffer that refuses a glyph of more than twice `limits.path_points()` segments (a simple
    /// glyph has up to points + contours segments, and byblos gates on points).
    pub fn new(limits: &Limits) -> Buffer {
        Buffer {
            segs: SegList::new(
                usize::try_from(limits.path_points())
                    .unwrap_or(usize::MAX)
                    .saturating_mul(2),
            ),
            compound_stack: [CompoundElem::default(); MAX_COMPOUND_STACK_SIZE as usize],
        }
    }

    /// The segments of the last successful [`Font::load_glyph`].
    pub fn segments(&self) -> &[Segment] {
        &self.segs.v
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::new(&Limits::default())
    }
}

/// The glyph data locations of a font (`glyphData`).
#[derive(Default)]
pub(crate) struct GlyphData {
    /// The data of glyph `i` is `src[locations[i]..locations[i + 1]]`.
    pub(crate) locations: Vec<u32>,
    /// For PostScript fonts, the bytecode of subroutine `i` is `src[x[i]..x[i + 1]]`.
    pub(crate) gsubrs: Vec<u32>,
    pub(crate) single_subrs: Vec<u32>,
    /// `Some` (possibly empty) for a CID-keyed CFF font, as Go's non-nil slice.
    pub(crate) multi_subrs: Option<Vec<Vec<u32>>>,
    pub(crate) fd_select: postscript::FdSelect,
}

/// The tables `initializeTables` finds.
#[derive(Default)]
pub(crate) struct Tables {
    pub(crate) cmap: Table,
    pub(crate) head: Table,
    pub(crate) hhea: Table,
    pub(crate) hmtx: Table,
    pub(crate) maxp: Table,
    pub(crate) os2: Table,
    pub(crate) post: Table,
    pub(crate) glyf: Table,
    pub(crate) loca: Table,
    pub(crate) cff: Table,
    pub(crate) cblc: Table,
    pub(crate) gpos: Table,
    pub(crate) kern: Table,
}

/// `Font`: an SFNT font (TTF or OTF; a bare CFF needs the wrapper `render::cff` builds).
pub struct Font {
    src: Vec<u8>,
    hmtx_offset: u32,
    num_hmetrics: i32,
    units_per_em: Units,
    glyph_data: GlyphData,
    glyph_index: cmap::GlyphIndexer,
    is_color_bitmap: bool,
    is_postscript: bool,
}

impl Font {
    /// `Parse`: parses an SFNT font from `src`, which is copied. Like Go it also loads the glyphs
    /// of `x` and `H` when the OS/2 table has no x-height and cap-height, so a malformed one
    /// fails the parse.
    ///
    /// # Errors
    /// Any Go parse error ([`Error::Invalid`]), or [`Error::Resource`] when a buffer the font
    /// asks for is over `max_alloc`.
    pub fn parse(src: &[u8], limits: &Limits) -> Result<Font> {
        let mut budget = Budget(limits.alloc_limit());
        let mut owned = budget.vec::<u8>(src.len())?;
        owned.extend_from_slice(src);
        Font::initialize(owned, limits, &mut budget, None)
    }

    /// `only_format` restricts the cmap formats considered (a test hook; Go swaps its
    /// `supportedCmapFormat` variable).
    fn initialize(
        src: Vec<u8>,
        limits: &Limits,
        budget: &mut Budget,
        only_format: Option<u16>,
    ) -> Result<Font> {
        let (tables, is_postscript) = initialize_tables(&src)?;

        // The order of these steps matters: later ones use what earlier ones found.
        let (index_to_loc_format, units_per_em) = parse_head(&src, &tables.head)?;
        let num_glyphs = parse_maxp(&src, &tables.maxp, is_postscript)?;
        let (glyph_data, is_color_bitmap) = parse_glyph_data(
            &src,
            &tables,
            num_glyphs,
            index_to_loc_format,
            is_postscript,
            budget,
        )?;
        let glyph_index = cmap::parse_cmap(&src, &tables.cmap, only_format, budget)?;
        parse_kern(&src, &tables.kern)?;
        gpos::parse_gpos_kern(&src, &tables.gpos, budget)?;
        let num_hmetrics = parse_hhea(&src, &tables.hhea, num_glyphs)?;
        parse_hmtx(&tables.hmtx, num_glyphs, num_hmetrics)?;
        let has_x_height_cap_height = parse_os2(&src, &tables.os2)?;
        parse_post(&src, &tables.post, num_glyphs)?;

        let f = Font {
            src,
            hmtx_offset: tables.hmtx.offset,
            num_hmetrics,
            units_per_em,
            glyph_data,
            glyph_index,
            is_color_bitmap,
            is_postscript,
        };
        if !has_x_height_cap_height {
            f.init_os2_version_below_2(limits)?;
        }
        Ok(f)
    }

    /// `UnitsPerEm`: the number of font units per em.
    pub fn units_per_em(&self) -> Units {
        self.units_per_em
    }

    /// `NumGlyphs`: the number of glyphs.
    pub fn num_glyphs(&self) -> usize {
        self.glyph_data.locations.len().saturating_sub(1)
    }

    /// `GlyphIndex`: the glyph index of rune `r` (a Go `rune` is an `i32`). Index 0 with no
    /// error means the font has no glyph for it.
    ///
    /// # Errors
    /// [`INVALID_CMAP_TABLE`] or [`INVALID_BOUNDS`] when the cmap table is malformed.
    pub fn glyph_index(&self, r: i32) -> Result<GlyphIndex> {
        self.glyph_index.lookup(&self.src, r)
    }

    /// `GlyphAdvance` with `font.HintingNone`: the advance width of glyph `x` at `ppem` pixels
    /// per em. With `ppem` equal to the units per em the result is in font units.
    ///
    /// # Errors
    /// [`Error::NotFound`] if `x` is out of range.
    pub fn glyph_advance(&self, x: GlyphIndex, ppem: Int26_6) -> Result<Int26_6> {
        if usize::from(x.0) >= self.num_glyphs() {
            return Err(Error::NotFound);
        }
        // The advance of the last record applies to all remaining glyphs.
        let n = (self.num_hmetrics - 1) as u16;
        let x = if x.0 > n { n } else { x.0 };
        let buf = view(&self.src, i64::from(self.hmtx_offset) + 4 * i64::from(x), 2)?;
        let adv = Int26_6(i32::from(u16(buf)));
        Ok(scale(
            Int26_6(adv.0.wrapping_mul(ppem.0)),
            self.units_per_em,
        ))
    }

    /// `viewGlyphData`: the data of glyph `x` and its offset and length in the source.
    fn view_glyph_data(&self, x: GlyphIndex) -> Result<(&[u8], u32, u32)> {
        let xx = usize::from(x.0);
        if self.num_glyphs() <= xx {
            return Err(Error::NotFound);
        }
        let (Some(&i), Some(&j)) = (
            self.glyph_data.locations.get(xx),
            self.glyph_data.locations.get(xx + 1),
        ) else {
            return Err(Error::NotFound);
        };
        if j < i {
            return Err(INVALID_GLYPH_DATA_LENGTH);
        }
        if j - i > MAX_GLYPH_DATA_LENGTH {
            return Err(UNSUPPORTED_GLYPH_DATA_LENGTH);
        }
        let buf = view(&self.src, i64::from(i), i64::from(j - i))?;
        Ok((buf, i, j - i))
    }

    /// `LoadGlyph`: the vector segments of glyph `x` at `ppem` pixels per em, in `b`. The Y axis
    /// of the result increases down. With `ppem` equal to the units per em the coordinates are
    /// raw font units with no rounding.
    ///
    /// # Errors
    /// [`Error::NotFound`] for an index out of range, [`Error::ColoredGlyph`] for a bitmap
    /// font, the glyph data errors, or [`Error::Resource`] past the buffer's segment cap.
    pub fn load_glyph<'b>(
        &self,
        b: &'b mut Buffer,
        x: GlyphIndex,
        ppem: Int26_6,
    ) -> Result<&'b [Segment]> {
        b.segs.v.clear();
        if self.is_color_bitmap {
            return Err(Error::ColoredGlyph);
        }
        if self.is_postscript {
            let (buf, offset, length) = self.view_glyph_data(x)?;
            let ended = postscript::run_type2_charstring(
                &self.src,
                &self.glyph_data,
                &mut b.segs,
                x,
                buf,
                offset,
                length,
            )?;
            if !ended {
                return Err(INVALID_CFF_TABLE);
            }
        } else {
            truetype::load_glyf(self, b, x, 0, 0)?;
        }

        // Scale the segments, and flip the Y coordinates: OpenType's Y axis increases up.
        for seg in &mut b.segs.v {
            for a in &mut seg.args {
                a.x = scale(Int26_6(a.x.0.wrapping_mul(ppem.0)), self.units_per_em);
                a.y = Int26_6(
                    scale(Int26_6(a.y.0.wrapping_mul(ppem.0)), self.units_per_em)
                        .0
                        .wrapping_neg(),
                );
            }
        }
        Ok(&b.segs.v)
    }

    /// `glyphTopOS2` and `initOS2VersionBelow2`: Go loads the glyphs of `x` and `H` to find the
    /// x-height and cap-height, and fails the parse if loading one fails. The heights themselves
    /// are not used by byblos, so only the errors are kept.
    fn init_os2_version_below_2(&self, limits: &Limits) -> Result<()> {
        let ppem = Int26_6(self.units_per_em);
        let mut b = Buffer::new(limits);
        for r in ['x', 'H'] {
            match self.glyph_index(r as i32) {
                Err(e) if e != Error::NotFound => return Err(e),
                Err(_) | Ok(GlyphIndex(0)) => {}
                Ok(ind) => {
                    self.load_glyph(&mut b, ind, ppem)?;
                }
            }
        }
        Ok(())
    }
}

/// `initializeTables` for a font at offset 0.
fn initialize_tables(src: &[u8]) -> Result<(Tables, bool)> {
    let buf = view(src, 0, 12)?;
    let mut is_postscript = false;
    match u32(buf) {
        // dfont and ttcf: the data is a collection.
        0x0000_0100 | 0x7474_6366 => return Err(INVALID_SINGLE_FONT),
        0x0001_0000 | 0x7472_7565 => {}
        0x4f54_544f => is_postscript = true,
        _ => return Err(INVALID_FONT),
    }
    let num_tables = usize::from(u16(buf.get(4..).unwrap_or_default()));
    if num_tables > MAX_NUM_TABLES {
        return Err(UNSUPPORTED_NUMBER_OF_TABLES);
    }

    // The table records, 16 bytes each, sorted by tag.
    let records = view(src, 12, 16 * num_tables as i64)?;
    let mut tables = Tables::default();
    let mut final_table_offset: i32 = 0;
    let mut prev_tag = 0u32;
    for (i, b) in records.chunks_exact(16).enumerate() {
        let tag = u32(b);
        if i > 0 && tag <= prev_tag {
            return Err(INVALID_TABLE_TAG_ORDER);
        }
        prev_tag = tag;

        let o = u32(b.get(8..12).unwrap_or_default());
        let n = u32(b.get(12..16).unwrap_or_default());
        if o > MAX_TABLE_OFFSET || n > MAX_TABLE_LENGTH {
            return Err(UNSUPPORTED_TABLE_OFFSET_LENGTH);
        }
        // The checksums are ignored, but all tables begin on four byte boundaries.
        if o & 3 != 0 {
            return Err(INVALID_TABLE_OFFSET);
        }
        if final_table_offset < (o + n) as i32 {
            final_table_offset = (o + n) as i32;
        }

        let t = Table {
            offset: o,
            length: n,
        };
        match tag {
            0x4342_4c43 => tables.cblc = t,
            0x4346_4620 => tables.cff = t,
            0x4f53_2f32 => tables.os2 = t,
            0x636d_6170 => tables.cmap = t,
            0x676c_7966 => tables.glyf = t,
            0x4750_4f53 => tables.gpos = t,
            0x6865_6164 => tables.head = t,
            0x6868_6561 => tables.hhea = t,
            0x686d_7478 => tables.hmtx = t,
            0x6b65_726e => tables.kern = t,
            0x6c6f_6361 => tables.loca = t,
            0x6d61_7870 => tables.maxp = t,
            0x706f_7374 => tables.post = t,
            _ => {}
        }
    }

    if final_table_offset as usize > src.len() {
        return Err(INVALID_SOURCE_DATA);
    }
    Ok((tables, is_postscript))
}

/// `parseHead`: returns `(indexToLocFormat, unitsPerEm)`. The font bounds are not kept.
fn parse_head(src: &[u8], head: &Table) -> Result<(bool, Units)> {
    if head.length != 54 {
        return Err(INVALID_HEAD_TABLE);
    }
    let u = head.u16(src, 18)?;
    if u == 0 {
        return Err(INVALID_HEAD_TABLE);
    }
    // The bounds at offsets 36..44 lie inside the table, so reading them cannot fail.
    let u2 = head.u16(src, 50)?;
    Ok((u2 != 0, Units::from(u)))
}

/// `parseMaxp`: returns the number of glyphs.
fn parse_maxp(src: &[u8], maxp: &Table, is_postscript: bool) -> Result<i32> {
    if is_postscript {
        if maxp.length != 6 {
            return Err(INVALID_MAXP_TABLE);
        }
    } else if maxp.length != 32 {
        return Err(INVALID_MAXP_TABLE);
    }
    Ok(i32::from(maxp.u16(src, 4)?))
}

/// `parseGlyphData`.
fn parse_glyph_data(
    src: &[u8],
    t: &Tables,
    num_glyphs: i32,
    index_to_loc_format: bool,
    is_postscript: bool,
    budget: &mut Budget,
) -> Result<(GlyphData, bool)> {
    let mut ret = GlyphData::default();
    let mut is_color_bitmap = false;
    if is_postscript {
        let mut p = postscript::CffParser::new(src, &t.cff);
        ret = p.parse(num_glyphs, budget)?;
    } else if t.loca.length != 0 {
        ret.locations = truetype::parse_loca(
            src,
            &t.loca,
            t.glyf.offset,
            index_to_loc_format,
            num_glyphs,
            budget,
        )?;
    } else if t.cblc.length != 0 {
        is_color_bitmap = true;
        // The CBLC and CBDT tables are not parsed: the font has empty glyphs.
        let mut v = budget.vec::<u32>(num_glyphs as usize + 1)?;
        v.resize(num_glyphs as usize + 1, 0);
        ret.locations = v;
    }

    if ret.locations.len() != (num_glyphs + 1) as usize {
        return Err(INVALID_LOCATION_DATA);
    }
    Ok((ret, is_color_bitmap))
}

/// `parseHhea`: returns `numHMetrics`. The ascent, descent, line gap and caret slope lie inside
/// the 36-byte table, so reading them cannot fail and they are not kept.
fn parse_hhea(src: &[u8], hhea: &Table, num_glyphs: i32) -> Result<i32> {
    if hhea.length != 36 {
        return Err(INVALID_HHEA_TABLE);
    }
    let u = hhea.u16(src, 34)?;
    if i32::from(u) > num_glyphs || u == 0 {
        return Err(INVALID_HHEA_TABLE);
    }
    Ok(i32::from(u))
}

/// `parseHmtx`. Some fonts in the wild omit the `2*(numGlyphs-numHMetrics)` part
/// (golang/go#28379).
fn parse_hmtx(hmtx: &Table, num_glyphs: i32, num_hmetrics: i32) -> Result<()> {
    if hmtx.length != (4 * num_hmetrics) as u32
        && hmtx.length != (4 * num_hmetrics + 2 * (num_glyphs - num_hmetrics)) as u32
    {
        return Err(INVALID_HMTX_TABLE);
    }
    Ok(())
}

/// `parseKern`: validates the kern table. The pair count and offset it finds feed `Font.Kern`,
/// which is not ported.
fn parse_kern(src: &[u8], kern: &Table) -> Result<()> {
    if kern.length == 0 {
        return Ok(());
    }
    const HEADER_SIZE: i64 = 4;
    if kern.length < HEADER_SIZE as u32 {
        return Err(INVALID_KERN_TABLE);
    }
    let buf = view(src, i64::from(kern.offset), HEADER_SIZE)?;
    let offset = i64::from(kern.offset) + HEADER_SIZE;
    let length = i64::from(kern.length) - HEADER_SIZE;

    match u16(buf) {
        0 => {
            if u16(buf.get(2..).unwrap_or_default()) == 0 {
                return Ok(());
            }
            // More than one subtable: only the first is used.
            parse_kern_version0(src, offset, length)
        }
        1 => {
            if buf.get(2).copied() != Some(0) || buf.get(3).copied() != Some(0) {
                return Err(UNSUPPORTED_KERN_TABLE);
            }
            // Apple's extended kern table is ignored, as Microsoft's and FreeType's code do.
            Ok(())
        }
        _ => Err(UNSUPPORTED_KERN_TABLE),
    }
}

fn parse_kern_version0(src: &[u8], offset: i64, length: i64) -> Result<()> {
    const HEADER_SIZE: i64 = 6;
    if length < HEADER_SIZE {
        return Err(INVALID_KERN_TABLE);
    }
    let buf = view(src, offset, HEADER_SIZE)?;
    if u16(buf) != 0 {
        return Err(UNSUPPORTED_KERN_TABLE);
    }
    let subtable_length = u16(buf.get(2..).unwrap_or_default());
    if i64::from(subtable_length) < HEADER_SIZE || length < i64::from(subtable_length) {
        return Err(INVALID_KERN_TABLE);
    }
    // Only horizontal kerning is supported.
    if buf.get(5).copied() != Some(0x01) {
        return Err(UNSUPPORTED_KERN_TABLE);
    }
    let offset = offset + HEADER_SIZE;
    let length = length - HEADER_SIZE;
    let subtable_length = subtable_length - HEADER_SIZE as u16;

    match buf.get(4).copied() {
        Some(0) => parse_kern_format0(src, offset, length, subtable_length),
        _ => Err(UNSUPPORTED_KERN_TABLE),
    }
}

fn parse_kern_format0(src: &[u8], offset: i64, length: i64, subtable_length: u16) -> Result<()> {
    const HEADER_SIZE: i64 = 8;
    const ENTRY_SIZE: i64 = 6;
    if length < HEADER_SIZE {
        return Err(INVALID_KERN_TABLE);
    }
    let buf = view(src, offset, HEADER_SIZE)?;
    let num_pairs = i64::from(u16(buf));

    // The subtable length is only a u16, and fonts like Cambria have more than 10k kerning pairs:
    // compare the size with the kern length and its truncation with the subtable length.
    let n = HEADER_SIZE + ENTRY_SIZE * num_pairs;
    if length < n || subtable_length != n as u16 {
        return Err(INVALID_KERN_TABLE);
    }
    Ok(())
}

/// `parseOS2`: returns whether the table has the x-height and cap-height (version 2 and up).
fn parse_os2(src: &[u8], os2: &Table) -> Result<bool> {
    if os2.length == 0 {
        // Apple TrueType fonts might omit the OS/2 table.
        return Ok(false);
    } else if os2.length < 2 {
        return Err(INVALID_OS2_TABLE);
    }
    let vers = os2.u16(src, 0)?;
    if vers < 2 {
        // The original TrueType specification had this table at 68 bytes long.
        if os2.length < 68 {
            return Err(INVALID_OS2_TABLE);
        }
        return Ok(false);
    }
    if os2.length < 96 {
        return Err(INVALID_OS2_TABLE);
    }
    Ok(true)
}

/// `parsePost`: validates the post table. The italic angle and underline fields lie inside the
/// 32-byte header, so reading them cannot fail and they are not kept.
fn parse_post(src: &[u8], post: &Table, num_glyphs: i32) -> Result<()> {
    const HEADER_SIZE: u32 = 32;
    if post.length < HEADER_SIZE {
        return Err(INVALID_POST_TABLE);
    }
    match post.u32(src, 0)? {
        0x10000 | 0x30000 => {}
        0x20000 => {
            if post.length < HEADER_SIZE + 2 + 2 * num_glyphs as u32 {
                return Err(INVALID_POST_TABLE);
            }
        }
        _ => return Err(UNSUPPORTED_POST_TABLE),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
