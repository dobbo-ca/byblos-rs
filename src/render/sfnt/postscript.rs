//! Port of golang.org/x/image/font/sfnt `postscript.go`: the CFF table parser, the PostScript
//! interpreter for DICT data, and the Type 2 Charstring interpreter that produces the outlines
//! of OpenType/CFF fonts. `render::cff` wraps a bare CFF in an SFNT container so it comes
//! through here too.
//!
//! The Go interpreter table of operators (`psOperators`) is the match in [`lookup_op`]; each
//! operator with a Go `run` function is an [`Op`], and the ones Go lists without one are
//! [`Op::Nop`] (their arguments are popped and ignored).

use super::fixed::{Int26_6, Point26_6};
use super::{
    Budget, Error, GO_PANIC, GlyphData, GlyphIndex, INVALID_CFF_TABLE, MAX_GLYPH_DATA_LENGTH,
    MAX_HINT_BITS, MAX_NUM_FONT_DICTS, MAX_NUM_SUBROUTINES, Result, SegList, Segment, SegmentOp,
    Table, UNSUPPORTED_CFF_FD_SELECT_TABLE, UNSUPPORTED_CFF_VERSION, UNSUPPORTED_GLYPH_DATA_LENGTH,
    UNSUPPORTED_NUMBER_OF_FONT_DICTS, UNSUPPORTED_NUMBER_OF_HINTS,
    UNSUPPORTED_NUMBER_OF_SUBROUTINES, UNSUPPORTED_REAL_NUMBER_ENCODING,
    UNSUPPORTED_TYPE2_CHARSTRING, u16, u32, view,
};

/// The argument stack size: 5176.CFF.pdf section 4 says an operator may be preceded by up to 48
/// operands, and 5177.Type2.pdf Appendix B says "Argument stack 48".
const PS_ARG_STACK_SIZE: usize = 48;

/// "Subr nesting, stack limit 10".
const PS_CALL_STACK_SIZE: usize = 10;

/// The maximum length in bytes of the "-123.456E-7" representation.
const MAX_REAL_NUMBER_STR_LEN: usize = 64;

/// `len("E-")`, the longest nibble definition.
const MAX_NIBBLE_DEFS_LENGTH: usize = 2;

/// Two-byte operators have an initial escape byte of 12.
const ESCAPE_BYTE: u8 = 12;

/// `bigEndian`: the big-endian integer in `b`, which is 1 to 4 bytes long.
fn big_endian(b: &[u8]) -> Result<u32> {
    match *b {
        [a] => Ok(u32::from(a)),
        [a, c] => Ok((u32::from(a) << 8) | u32::from(c)),
        [a, c, d] => Ok((u32::from(a) << 16) | (u32::from(c) << 8) | u32::from(d)),
        [a, c, d, e] => Ok(u32::from_be_bytes([a, c, d, e])),
        _ => Err(GO_PANIC),
    }
}

/// `fdSelect`: a CFF font's Font Dict Select data.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FdSelect {
    format: u8,
    num_ranges: u16,
    offset: i32,
}

impl FdSelect {
    /// `lookup`: the font dict index of glyph `x`.
    fn lookup(&self, src: &[u8], x: GlyphIndex) -> Result<i32> {
        match self.format {
            0 => {
                let buf = view(src, i64::from(self.offset) + i64::from(x.0), 1)?;
                Ok(i32::from(buf.first().copied().unwrap_or(0)))
            }
            3 => {
                let (mut lo, mut hi) = (0i64, i64::from(self.num_ranges));
                while lo < hi {
                    let i = (lo + hi) / 2;
                    let buf = view(src, i64::from(self.offset) + 3 * i, 3 + 2)?;
                    // buf holds the range [xlo, xhi).
                    if x.0 < u16(buf) {
                        hi = i;
                        continue;
                    }
                    if u16(buf.get(3..).unwrap_or_default()) <= x.0 {
                        lo = i + 1;
                        continue;
                    }
                    return Ok(i32::from(buf.get(2).copied().unwrap_or(0)));
                }
                Err(Error::NotFound)
            }
            _ => Err(Error::NotFound),
        }
    }
}

/// `zeroed`: `n` zero `u32`s, charged to the budget first.
fn zeroed(budget: &mut Budget, n: usize) -> Result<Vec<u32>> {
    let mut v = budget.vec::<u32>(n)?;
    v.resize(n, 0);
    Ok(v)
}

/// `cffParser`: parses the CFF table of an SFNT font.
pub(crate) struct CffParser<'a> {
    src: &'a [u8],
    base: i64,
    offset: i64,
    end: i64,
    buf: &'a [u8],
    psi: Interp<'a>,
    /// Unused segment sink: the DICT contexts never emit segments.
    segs: SegList,
}

impl<'a> CffParser<'a> {
    pub(crate) fn new(src: &'a [u8], cff: &Table) -> CffParser<'a> {
        CffParser {
            src,
            base: i64::from(cff.offset),
            offset: i64::from(cff.offset),
            end: i64::from(cff.offset) + i64::from(cff.length),
            buf: &[],
            psi: Interp::new(src, None),
            segs: SegList::new(0),
        }
    }

    /// `parse`: the glyph data of the CFF table, which must hold `num_glyphs` glyphs.
    pub(crate) fn parse(&mut self, num_glyphs: i32, budget: &mut Budget) -> Result<GlyphData> {
        let mut ret = GlyphData::default();

        // Parse the header.
        self.read(4)?;
        if self.buf.first() != Some(&1)
            || self.buf.get(1) != Some(&0)
            || self.buf.get(2) != Some(&4)
        {
            return Err(UNSUPPORTED_CFF_VERSION);
        }

        // Parse the Name INDEX. The Name INDEX in the CFF must contain only one entry.
        {
            let (count, off_size) = self.parse_index_header()?;
            if count != 1 {
                return Err(INVALID_CFF_TABLE);
            }
            let mut loc_buf = [0u32; 2];
            self.parse_index_locations(&mut loc_buf, count, off_size)?;
            self.offset = i64::from(loc_buf[1]);
        }

        // Parse the Top DICT INDEX.
        self.psi.top_dict = TopDictData::default();
        {
            let (count, off_size) = self.parse_index_header()?;
            // The count should match the count of the Name INDEX, which is 1.
            if count != 1 {
                return Err(INVALID_CFF_TABLE);
            }
            let mut loc_buf = [0u32; 2];
            self.parse_index_locations(&mut loc_buf, count, off_size)?;
            self.read(i64::from(loc_buf[1].wrapping_sub(loc_buf[0])))?;
            self.psi.run(Ctx::TopDict, self.buf, 0, 0, &mut self.segs)?;
        }

        // Skip the String INDEX.
        {
            let (count, off_size) = self.parse_index_header()?;
            if count != 0 {
                // Read the last location. Locations are off by 1 byte.
                self.skip(i64::from(count * off_size))?;
                self.read(i64::from(off_size))?;
                let loc = big_endian(self.buf)?.wrapping_sub(1);
                // Check that locations are in bounds.
                if ((self.end - self.offset) as u32) < loc {
                    return Err(INVALID_CFF_TABLE);
                }
                // Skip the index data.
                self.skip(i64::from(loc))?;
            }
        }

        // Parse the Global Subrs [Subroutines] INDEX.
        {
            let (count, off_size) = self.parse_index_header()?;
            if count != 0 {
                if count > MAX_NUM_SUBROUTINES {
                    return Err(UNSUPPORTED_NUMBER_OF_SUBROUTINES);
                }
                ret.gsubrs = zeroed(budget, count as usize + 1)?;
                self.parse_index_locations(&mut ret.gsubrs, count, off_size)?;
            }
        }

        // Parse the CharStrings INDEX, whose location was found in the Top DICT.
        {
            if !self.seek_from_base(self.psi.top_dict.char_strings_offset) {
                return Err(INVALID_CFF_TABLE);
            }
            let (count, off_size) = self.parse_index_header()?;
            if count == 0 || count != num_glyphs {
                return Err(INVALID_CFF_TABLE);
            }
            ret.locations = zeroed(budget, count as usize + 1)?;
            self.parse_index_locations(&mut ret.locations, count, off_size)?;
        }

        if !self.psi.top_dict.is_cid_font {
            // Parse the Private DICT, whose location was found in the Top DICT.
            ret.single_subrs = self.parse_private_dict(
                self.psi.top_dict.private_dict_offset,
                self.psi.top_dict.private_dict_length,
                budget,
            )?;
        } else {
            // Parse the Font Dict Select data, whose location was found in the Top DICT.
            ret.fd_select = self.parse_fd_select(num_glyphs)?;

            // Parse the Font Dicts. Each one contains its own Private DICT.
            if !self.seek_from_base(self.psi.top_dict.fd_array) {
                return Err(INVALID_CFF_TABLE);
            }

            let (count, off_size) = self.parse_index_header()?;
            if count > MAX_NUM_FONT_DICTS {
                return Err(UNSUPPORTED_NUMBER_OF_FONT_DICTS);
            }

            let mut fd_locations = zeroed(budget, count as usize + 1)?;
            self.parse_index_locations(&mut fd_locations, count, off_size)?;

            let mut private_dicts: Vec<(i32, i32)> = budget.vec(count as usize)?;
            for i in 0..count as usize {
                let (Some(&lo), Some(&hi)) = (fd_locations.get(i), fd_locations.get(i + 1)) else {
                    return Err(GO_PANIC);
                };
                let length = hi.wrapping_sub(lo);
                if self.read(i64::from(length)).is_err() {
                    return Err(INVALID_CFF_TABLE);
                }
                self.psi.top_dict = TopDictData::default();
                self.psi.run(Ctx::TopDict, self.buf, 0, 0, &mut self.segs)?;
                private_dicts.push((
                    self.psi.top_dict.private_dict_offset,
                    self.psi.top_dict.private_dict_length,
                ));
            }

            let mut multi: Vec<Vec<u32>> = budget.vec(count as usize)?;
            for (offset, length) in private_dicts {
                multi.push(self.parse_private_dict(offset, length, budget)?);
            }
            ret.multi_subrs = Some(multi);
        }

        Ok(ret)
    }

    /// `parseFDSelect`: the Font Dict Select data (5176.CFF.pdf section 19).
    fn parse_fd_select(&mut self, num_glyphs: i32) -> Result<FdSelect> {
        if !self.seek_from_base(self.psi.top_dict.fd_select) {
            return Err(INVALID_CFF_TABLE);
        }
        self.read(1)?;
        let mut ret = FdSelect {
            format: self.buf.first().copied().unwrap_or(0),
            ..FdSelect::default()
        };
        match ret.format {
            0 => {
                if self.end - self.offset < i64::from(num_glyphs) {
                    return Err(INVALID_CFF_TABLE);
                }
                ret.offset = self.offset as i32;
                Ok(ret)
            }
            3 => {
                self.read(2)?;
                ret.num_ranges = u16(self.buf);
                if self.end - self.offset < 3 * i64::from(ret.num_ranges) + 2 {
                    return Err(INVALID_CFF_TABLE);
                }
                ret.offset = self.offset as i32;
                Ok(ret)
            }
            _ => Err(UNSUPPORTED_CFF_FD_SELECT_TABLE),
        }
    }

    /// `parsePrivateDICT`: parses a Private DICT and returns its local subroutine locations
    /// (empty if it has none).
    fn parse_private_dict(
        &mut self,
        offset: i32,
        length: i32,
        budget: &mut Budget,
    ) -> Result<Vec<u32>> {
        self.psi.private_dict_subrs_offset = 0;
        if length != 0 {
            let full_length = (self.end - self.base) as i32;
            if offset <= 0 || full_length < offset || full_length - offset < length || length < 0 {
                return Err(INVALID_CFF_TABLE);
            }
            self.offset = self.base + i64::from(offset);
            self.read(i64::from(length))?;
            self.psi
                .run(Ctx::PrivateDict, self.buf, 0, 0, &mut self.segs)?;
        }

        // Parse the Local Subrs [Subroutines] INDEX, whose location was found in the Private
        // DICT.
        let mut subrs = Vec::new();
        if self.psi.private_dict_subrs_offset != 0 {
            if !self.seek_from_base(offset.wrapping_add(self.psi.private_dict_subrs_offset)) {
                return Err(INVALID_CFF_TABLE);
            }
            let (count, off_size) = self.parse_index_header()?;
            if count != 0 {
                if count > MAX_NUM_SUBROUTINES {
                    return Err(UNSUPPORTED_NUMBER_OF_SUBROUTINES);
                }
                subrs = zeroed(budget, count as usize + 1)?;
                self.parse_index_locations(&mut subrs, count, off_size)?;
            }
        }
        Ok(subrs)
    }

    /// `read`: views the `n` bytes from the offset in `self.buf` and advances the offset.
    fn read(&mut self, n: i64) -> Result<()> {
        if n < 0 || self.end - self.offset < n {
            return Err(INVALID_CFF_TABLE);
        }
        let buf = view(self.src, self.offset, n);
        self.offset += n;
        self.buf = buf?;
        Ok(())
    }

    fn skip(&mut self, n: i64) -> Result<()> {
        if self.end - self.offset < n {
            return Err(INVALID_CFF_TABLE);
        }
        self.offset += n;
        Ok(())
    }

    fn seek_from_base(&mut self, offset: i32) -> bool {
        if offset < 0 || ((self.end - self.base) as i32) < offset {
            return false;
        }
        self.offset = self.base + i64::from(offset);
        true
    }

    /// `parseIndexHeader`: returns `(count, offSize)`.
    fn parse_index_header(&mut self) -> Result<(i32, i32)> {
        self.read(2)?;
        let count = i32::from(u16(self.buf));
        // An empty INDEX is a count of 0 and no additional fields.
        if count == 0 {
            return Ok((count, 0));
        }
        self.read(1)?;
        let off_size = i32::from(self.buf.first().copied().unwrap_or(0));
        if !(1..=4).contains(&off_size) {
            return Err(INVALID_CFF_TABLE);
        }
        Ok((count, off_size))
    }

    /// `parseIndexLocations`: fills `dst` (`count + 1` long) with absolute locations.
    fn parse_index_locations(&mut self, dst: &mut [u32], count: i32, off_size: i32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if dst.len() != (count + 1) as usize {
            return Err(GO_PANIC);
        }
        self.read(dst.len() as i64 * i64::from(off_size))?;

        let mut buf = self.buf;
        let mut prev = 0u32;
        let size = off_size as usize;
        for (i, d) in dst.iter_mut().enumerate() {
            let loc = big_endian(buf.get(..size).unwrap_or_default())?;
            buf = buf.get(size..).unwrap_or_default();

            // Locations are off by 1 byte: "Offsets in the offset array are relative to the byte
            // that precedes the object data".
            if loc == 0 {
                return Err(INVALID_CFF_TABLE);
            }
            let loc = loc - 1;

            // The first element of the offset array is always 1 before the correction.
            if i == 0 {
                if loc != 0 {
                    return Err(INVALID_CFF_TABLE);
                }
            } else if loc <= prev {
                // Check that locations are increasing.
                return Err(INVALID_CFF_TABLE);
            }

            // Check that locations are in bounds.
            if ((self.end - self.offset) as u32) < loc {
                return Err(INVALID_CFF_TABLE);
            }

            *d = (self.offset as u32).wrapping_add(loc);
            prev = loc;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct CallStackEntry {
    offset: u32,
    length: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    TopDict,
    PrivateDict,
    Type2Charstring,
}

/// `psTopDictData`: the fields specific to the Top DICT context.
#[derive(Clone, Copy, Default)]
struct TopDictData {
    char_strings_offset: i32,
    fd_array: i32,
    fd_select: i32,
    is_cid_font: bool,
    private_dict_offset: i32,
    private_dict_length: i32,
}

/// `psType2CharstringsData`: the fields specific to the Type 2 Charstrings context.
#[derive(Clone, Copy, Default)]
struct Type2Data {
    x: i32,
    y: i32,
    first_x: i32,
    first_y: i32,
    hint_bits: i32,
    seen_width: bool,
    ended: bool,
    glyph_index: GlyphIndex,
    /// The result of the Font Dict Select lookup plus one, so that zero means unused or not yet
    /// evaluated.
    fd_select_index_plus_one: i32,
}

/// The operators that have a Go `run` function, plus [`Op::Nop`] for the ones that only pop
/// their arguments.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Nop,
    CharStrings,
    Private,
    Ros,
    FdArray,
    FdSelect,
    Subrs,
    Stem,
    Vmoveto,
    Rlineto,
    Hlineto,
    Vlineto,
    Rrcurveto,
    Callsubr,
    Return,
    Endchar,
    Mask,
    Rmoveto,
    Hmoveto,
    Rcurveline,
    Rlinecurve,
    Vvcurveto,
    Hhcurveto,
    Vhcurveto,
    Hvcurveto,
    Callgsubr,
    Hflex,
    Hflex1,
}

/// `psOperators`: the number of values an operator pops (-1 is "array" and -2 "delta" as per
/// 5176.CFF.pdf Table 6, which pop everything) and what it does, or `None` for an operator
/// that no context lists.
fn lookup_op(ctx: Ctx, escaped: bool, b: u8) -> Option<(i32, Op)> {
    use Op::*;
    Some(match (ctx, escaped, b) {
        // The Top DICT operators: 5176.CFF.pdf Tables 9 and 10.
        (Ctx::TopDict, false, 0..=4 | 13 | 15 | 16) => (1, Nop),
        (Ctx::TopDict, false, 5 | 14) => (-1, Nop),
        (Ctx::TopDict, false, 17) => (1, CharStrings),
        (Ctx::TopDict, false, 18) => (2, Private),
        (Ctx::TopDict, true, 0..=6 | 8 | 20..=22 | 31..=35 | 38) => (1, Nop),
        (Ctx::TopDict, true, 7) => (-1, Nop),
        (Ctx::TopDict, true, 23) => (-2, Nop),
        (Ctx::TopDict, true, 30) => (3, Ros),
        (Ctx::TopDict, true, 36) => (1, FdArray),
        (Ctx::TopDict, true, 37) => (1, FdSelect),

        // The Private DICT operators: 5176.CFF.pdf Table 23.
        (Ctx::PrivateDict, false, 6..=9) => (-2, Nop),
        (Ctx::PrivateDict, false, 10 | 11 | 20 | 21) => (1, Nop),
        (Ctx::PrivateDict, false, 19) => (1, Subrs),
        (Ctx::PrivateDict, true, 9..=11 | 14 | 17..=19) => (1, Nop),
        (Ctx::PrivateDict, true, 12 | 13) => (-2, Nop),

        // The Type 2 Charstring operators: 5177.Type2.pdf Appendix A.
        (Ctx::Type2Charstring, false, 1 | 3 | 18 | 23) => (-1, Stem),
        (Ctx::Type2Charstring, false, 4) => (-1, Vmoveto),
        (Ctx::Type2Charstring, false, 5) => (-1, Rlineto),
        (Ctx::Type2Charstring, false, 6) => (-1, Hlineto),
        (Ctx::Type2Charstring, false, 7) => (-1, Vlineto),
        (Ctx::Type2Charstring, false, 8) => (-1, Rrcurveto),
        (Ctx::Type2Charstring, false, 10) => (1, Callsubr),
        (Ctx::Type2Charstring, false, 11) => (0, Return),
        (Ctx::Type2Charstring, false, 14) => (-1, Endchar),
        (Ctx::Type2Charstring, false, 19 | 20) => (-1, Mask),
        (Ctx::Type2Charstring, false, 21) => (-1, Rmoveto),
        (Ctx::Type2Charstring, false, 22) => (-1, Hmoveto),
        (Ctx::Type2Charstring, false, 24) => (-1, Rcurveline),
        (Ctx::Type2Charstring, false, 25) => (-1, Rlinecurve),
        (Ctx::Type2Charstring, false, 26) => (-1, Vvcurveto),
        (Ctx::Type2Charstring, false, 27) => (-1, Hhcurveto),
        (Ctx::Type2Charstring, false, 29) => (1, Callgsubr),
        (Ctx::Type2Charstring, false, 30) => (-1, Vhcurveto),
        (Ctx::Type2Charstring, false, 31) => (-1, Hvcurveto),
        (Ctx::Type2Charstring, true, 34) => (7, Hflex),
        (Ctx::Type2Charstring, true, 36) => (9, Hflex1),
        _ => return None,
    })
}

/// `nibbleDefs`: 5176.CFF.pdf Table 5 "Nibble Definitions".
fn nibble_def(nib: u8) -> &'static str {
    match nib {
        0x00 => "0",
        0x01 => "1",
        0x02 => "2",
        0x03 => "3",
        0x04 => "4",
        0x05 => "5",
        0x06 => "6",
        0x07 => "7",
        0x08 => "8",
        0x09 => "9",
        0x0a => ".",
        0x0b => "E",
        0x0c => "E-",
        0x0e => "-",
        _ => "",
    }
}

/// `psInterpreter`: a PostScript interpreter. `src` and `gd` give the Type 2 Charstring
/// operators access to the font (`gd` is `None` while the CFF table itself is being parsed).
/// Cap on the numbers and operators one charstring may execute (not in Go; byblos applies a
/// similar gate before calling sfnt). Real glyphs use a few hundred.
const MAX_CHARSTRING_WORK: u32 = 1 << 20;

struct Interp<'a> {
    ctx: Ctx,
    instructions: &'a [u8],
    instr_offset: u32,
    instr_length: u32,
    arg: [i32; PS_ARG_STACK_SIZE],
    arg_top: i32,
    call: [CallStackEntry; PS_CALL_STACK_SIZE],
    call_top: i32,

    top_dict: TopDictData,
    private_dict_subrs_offset: i32,
    t2: Type2Data,

    src: &'a [u8],
    gd: Option<&'a GlyphData>,
    /// Operations executed so far; bounds the work a hostile charstring can request.
    work: u32,
}

/// `Font.LoadGlyph`'s PostScript arm: runs the charstring `buf` (at `offset`, `length` bytes
/// in the source) of glyph `x`, appending to `segs`. Returns whether it ended with `endchar`.
pub(crate) fn run_type2_charstring(
    src: &[u8],
    gd: &GlyphData,
    segs: &mut SegList,
    x: GlyphIndex,
    buf: &[u8],
    offset: u32,
    length: u32,
) -> Result<bool> {
    let mut p = Interp::new(src, Some(gd));
    p.t2 = Type2Data {
        glyph_index: x,
        ..Type2Data::default()
    };
    p.run(Ctx::Type2Charstring, buf, offset, length, segs)?;
    Ok(p.t2.ended)
}

impl<'a> Interp<'a> {
    fn new(src: &'a [u8], gd: Option<&'a GlyphData>) -> Interp<'a> {
        Interp {
            ctx: Ctx::TopDict,
            instructions: &[],
            instr_offset: 0,
            instr_length: 0,
            arg: [0; PS_ARG_STACK_SIZE],
            arg_top: 0,
            call: [CallStackEntry::default(); PS_CALL_STACK_SIZE],
            call_top: 0,
            top_dict: TopDictData::default(),
            private_dict_subrs_offset: 0,
            t2: Type2Data::default(),
            src,
            gd,
            work: 0,
        }
    }

    /// The argument at stack index `i`. Every index the operators use is below the checked
    /// stack top, so the 0 for one out of range is never read.
    fn a(&self, i: i32) -> i32 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.arg.get(i))
            .copied()
            .unwrap_or(0)
    }

    fn has_more_instructions(&self) -> bool {
        if !self.instructions.is_empty() {
            return true;
        }
        self.call
            .iter()
            .take(self.call_top.max(0) as usize)
            .any(|e| e.length != 0)
    }

    /// `run`: runs the instructions in the given context. For the Type 2 Charstring context,
    /// `offset` and `length` give the location of the instructions in the source.
    fn run(
        &mut self,
        ctx: Ctx,
        instructions: &'a [u8],
        offset: u32,
        length: u32,
        segs: &mut SegList,
    ) -> Result<()> {
        self.ctx = ctx;
        self.instructions = instructions;
        self.instr_offset = offset;
        self.instr_length = length;
        self.arg_top = 0;
        self.call_top = 0;

        'outer: while !self.instructions.is_empty() {
            self.work += 1;
            if self.work > MAX_CHARSTRING_WORK {
                return Err(Error::Resource(crate::Error::Limit {
                    name: "charstring-work",
                    got: i64::from(self.work),
                    max: i64::from(MAX_CHARSTRING_WORK),
                }));
            }
            // Push a numeric operand on the stack, if applicable.
            if self.parse_number()? {
                continue;
            }

            // Otherwise, execute an operator.
            let Some((&first, rest)) = self.instructions.split_first() else {
                break;
            };
            let mut b = first;
            self.instructions = rest;

            let mut escaped = false;
            loop {
                if b == ESCAPE_BYTE && !escaped {
                    let Some((&next, rest)) = self.instructions.split_first() else {
                        return Err(INVALID_CFF_TABLE);
                    };
                    b = next;
                    self.instructions = rest;
                    escaped = true;
                    continue;
                }

                let Some((num_pop, op)) = lookup_op(self.ctx, escaped, b) else {
                    return Err(Error::UnrecognizedOperator { escaped, byte: b });
                };
                if self.arg_top < num_pop {
                    return Err(INVALID_CFF_TABLE);
                }
                self.exec(op, segs)?;
                if num_pop < 0 {
                    self.arg_top = 0;
                } else {
                    self.arg_top -= num_pop;
                }
                continue 'outer;
            }
        }
        Ok(())
    }

    /// `parseNumber` (5176.CFF.pdf section 4): pushes a numeric operand if the next bytes are
    /// one. Returns whether it did.
    fn parse_number(&mut self) -> Result<bool> {
        let ins = self.instructions;
        let Some(&b) = ins.first() else {
            return Ok(false);
        };
        let mut number = 0i32;
        let mut has_result = false;
        if b == 28 {
            if ins.len() < 3 {
                return Err(INVALID_CFF_TABLE);
            }
            number = i32::from(u16(ins.get(1..).unwrap_or_default()) as i16);
            has_result = true;
            self.instructions = ins.get(3..).unwrap_or_default();
        } else if b == 29 && self.ctx != Ctx::Type2Charstring {
            if ins.len() < 5 {
                return Err(INVALID_CFF_TABLE);
            }
            number = u32(ins.get(1..).unwrap_or_default()) as i32;
            has_result = true;
            self.instructions = ins.get(5..).unwrap_or_default();
        } else if b == 30 && self.ctx != Ctx::Type2Charstring {
            // A real number: a byte value of 30 followed by a variable-length sequence of
            // nibbles.
            let mut s = [0u8; MAX_REAL_NUMBER_STR_LEN];
            let mut n = 0usize;
            self.instructions = ins.get(1..).unwrap_or_default();
            'real: loop {
                let Some((&byte, rest)) = self.instructions.split_first() else {
                    return Err(INVALID_CFF_TABLE);
                };
                self.instructions = rest;
                // Process the two nibbles, high then low.
                let mut bits = byte;
                for _ in 0..2 {
                    let nib = bits >> 4;
                    bits <<= 4;
                    if nib == 0x0f {
                        let text = std::str::from_utf8(s.get(..n).unwrap_or_default())
                            .map_err(|_| INVALID_CFF_TABLE)?;
                        // ParseFloat(s, 32): a parse failure or an overflow is an error.
                        match text.parse::<f32>() {
                            Ok(f) if f.is_finite() => number = f.to_bits() as i32,
                            _ => return Err(INVALID_CFF_TABLE),
                        }
                        has_result = true;
                        break 'real;
                    }
                    if nib == 0x0d {
                        return Err(INVALID_CFF_TABLE);
                    }
                    if n + MAX_NIBBLE_DEFS_LENGTH > MAX_REAL_NUMBER_STR_LEN {
                        return Err(UNSUPPORTED_REAL_NUMBER_ENCODING);
                    }
                    for &c in nibble_def(nib).as_bytes() {
                        if let Some(slot) = s.get_mut(n) {
                            *slot = c;
                        }
                        n += 1;
                    }
                }
            }
        } else if b < 32 {
            // No-op: an operator.
        } else if b < 247 {
            self.instructions = ins.get(1..).unwrap_or_default();
            number = i32::from(b) - 139;
            has_result = true;
        } else if b < 251 {
            if ins.len() < 2 {
                return Err(INVALID_CFF_TABLE);
            }
            let b1 = ins.get(1).copied().unwrap_or(0);
            self.instructions = ins.get(2..).unwrap_or_default();
            number = i32::from(b - 247) * 256 + i32::from(b1) + 108;
            has_result = true;
        } else if b < 255 {
            if ins.len() < 2 {
                return Err(INVALID_CFF_TABLE);
            }
            let b1 = ins.get(1).copied().unwrap_or(0);
            self.instructions = ins.get(2..).unwrap_or_default();
            number = -i32::from(b - 251) * 256 - i32::from(b1) - 108;
            has_result = true;
        } else if b == 255 && self.ctx == Ctx::Type2Charstring {
            if ins.len() < 5 {
                return Err(INVALID_CFF_TABLE);
            }
            number = u32(ins.get(1..).unwrap_or_default()) as i32;
            has_result = true;
            self.instructions = ins.get(5..).unwrap_or_default();
            // A 16.16 fixed point number, rounded to the closest integer (the stack holds
            // integers only). Not `(number + 0x8000) >> 16`, because of potential overflow.
            number = (number >> 16).wrapping_add(1 & (number >> 15));
        }

        if has_result {
            let top = usize::try_from(self.arg_top).unwrap_or(PS_ARG_STACK_SIZE);
            match self.arg.get_mut(top) {
                Some(slot) => *slot = number,
                None => return Err(INVALID_CFF_TABLE),
            }
            self.arg_top += 1;
        }
        Ok(has_result)
    }

    /// Runs an operator's Go `run` function.
    fn exec(&mut self, op: Op, segs: &mut SegList) -> Result<()> {
        match op {
            Op::Nop => Ok(()),
            Op::CharStrings => {
                self.top_dict.char_strings_offset = self.a(self.arg_top - 1);
                Ok(())
            }
            Op::Private => {
                self.top_dict.private_dict_length = self.a(self.arg_top - 2);
                self.top_dict.private_dict_offset = self.a(self.arg_top - 1);
                Ok(())
            }
            Op::Ros => {
                self.top_dict.is_cid_font = true;
                Ok(())
            }
            Op::FdArray => {
                self.top_dict.fd_array = self.a(self.arg_top - 1);
                Ok(())
            }
            Op::FdSelect => {
                self.top_dict.fd_select = self.a(self.arg_top - 1);
                Ok(())
            }
            Op::Subrs => {
                self.private_dict_subrs_offset = self.a(self.arg_top - 1);
                Ok(())
            }
            Op::Stem => self.t2_stem(),
            Op::Mask => self.t2_mask(),
            Op::Hmoveto => {
                self.t2_read_width(1);
                if self.arg_top != 1 {
                    return Err(INVALID_CFF_TABLE);
                }
                self.move_to(segs, self.a(0), 0)
            }
            Op::Vmoveto => {
                self.t2_read_width(1);
                if self.arg_top != 1 {
                    return Err(INVALID_CFF_TABLE);
                }
                self.move_to(segs, 0, self.a(0))
            }
            Op::Rmoveto => {
                self.t2_read_width(2);
                if self.arg_top != 2 {
                    return Err(INVALID_CFF_TABLE);
                }
                self.move_to(segs, self.a(0), self.a(1))
            }
            Op::Hlineto => self.t2_lineto(segs, false),
            Op::Vlineto => self.t2_lineto(segs, true),
            Op::Rlineto => self.t2_rlineto(segs),
            Op::Rcurveline => self.t2_rcurveline(segs),
            Op::Rlinecurve => self.t2_rlinecurve(segs),
            Op::Hhcurveto => self.t2_curveto(segs, false, false),
            Op::Vvcurveto => self.t2_curveto(segs, false, true),
            Op::Hvcurveto => self.t2_curveto(segs, true, false),
            Op::Vhcurveto => self.t2_curveto(segs, true, true),
            Op::Rrcurveto => self.t2_rrcurveto(segs),
            Op::Hflex => self.t2_hflex(segs),
            Op::Hflex1 => self.t2_hflex1(segs),
            Op::Callgsubr => {
                let gd = self.gd.ok_or(INVALID_CFF_TABLE)?;
                self.t2_call(&gd.gsubrs)
            }
            Op::Callsubr => self.t2_callsubr(),
            Op::Return => self.t2_return(),
            Op::Endchar => self.t2_endchar(segs),
        }
    }

    // The path construction, as `psType2CharstringsData` methods in Go.

    fn close_path(&mut self, segs: &mut SegList) -> Result<()> {
        if self.t2.x != self.t2.first_x || self.t2.y != self.t2.first_y {
            segs.push(point_segment(
                SegmentOp::LineTo,
                self.t2.first_x,
                self.t2.first_y,
            ))?;
        }
        Ok(())
    }

    fn move_to(&mut self, segs: &mut SegList, dx: i32, dy: i32) -> Result<()> {
        self.close_path(segs)?;
        self.t2.x = self.t2.x.wrapping_add(dx);
        self.t2.y = self.t2.y.wrapping_add(dy);
        segs.push(point_segment(SegmentOp::MoveTo, self.t2.x, self.t2.y))?;
        self.t2.first_x = self.t2.x;
        self.t2.first_y = self.t2.y;
        Ok(())
    }

    fn line_to(&mut self, segs: &mut SegList, dx: i32, dy: i32) -> Result<()> {
        self.t2.x = self.t2.x.wrapping_add(dx);
        self.t2.y = self.t2.y.wrapping_add(dy);
        segs.push(point_segment(SegmentOp::LineTo, self.t2.x, self.t2.y))
    }

    fn cube_to(&mut self, segs: &mut SegList, d: [i32; 6]) -> Result<()> {
        let [dxa, dya, dxb, dyb, dxc, dyc] = d;
        self.t2.x = self.t2.x.wrapping_add(dxa);
        self.t2.y = self.t2.y.wrapping_add(dya);
        let a = fixed_point(self.t2.x, self.t2.y);
        self.t2.x = self.t2.x.wrapping_add(dxb);
        self.t2.y = self.t2.y.wrapping_add(dyb);
        let b = fixed_point(self.t2.x, self.t2.y);
        self.t2.x = self.t2.x.wrapping_add(dxc);
        self.t2.y = self.t2.y.wrapping_add(dyc);
        let c = fixed_point(self.t2.x, self.t2.y);
        segs.push(Segment {
            op: SegmentOp::CubeTo,
            args: [a, b, c],
        })
    }

    /// `t2CReadWidth`: reads the optional width adjustment, which is on the bottom of the
    /// argument stack if present. `n_args` is the expected number of arguments; a negative one
    /// means a multiple of 2.
    ///
    /// For a CFF embedded in an SFNT font the widths are in the hmtx table, so the value is
    /// just removed from the stack.
    fn t2_read_width(&mut self, n_args: i32) {
        if self.t2.seen_width {
            return;
        }
        self.t2.seen_width = true;
        if n_args >= 0 {
            if self.arg_top != n_args + 1 {
                return;
            }
        } else if self.arg_top & 1 == 0 {
            return;
        }
        let top = usize::try_from(self.arg_top).unwrap_or(0);
        if (1..=PS_ARG_STACK_SIZE).contains(&top) {
            self.arg.copy_within(1..top, 0);
            self.arg_top -= 1;
        }
    }

    fn t2_stem(&mut self) -> Result<()> {
        self.t2_read_width(-1);
        if self.arg_top % 2 != 0 {
            return Err(INVALID_CFF_TABLE);
        }
        // Update the number of hint bits needed to parse hintmask and cntrmask; the stem hints
        // are otherwise ignored.
        self.t2.hint_bits += self.arg_top / 2;
        if self.t2.hint_bits > MAX_HINT_BITS {
            return Err(UNSUPPORTED_NUMBER_OF_HINTS);
        }
        Ok(())
    }

    fn t2_mask(&mut self) -> Result<()> {
        // If a hintmask is given any arguments, an implicit vstem runs (as FreeType does).
        if self.arg_top != 0 {
            self.t2_stem()?;
        } else if !self.t2.seen_width {
            self.t2.seen_width = true;
        }

        let hint_bytes = usize::try_from(self.t2.hint_bits).unwrap_or(0).div_ceil(8);
        if self.instructions.len() < hint_bytes {
            return Err(INVALID_CFF_TABLE);
        }
        self.instructions = self.instructions.get(hint_bytes..).unwrap_or_default();
        Ok(())
    }

    fn t2_lineto(&mut self, segs: &mut SegList, mut vertical: bool) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 1 {
            return Err(INVALID_CFF_TABLE);
        }
        let mut i = 0;
        while i < self.arg_top {
            let (mut dx, mut dy) = (self.a(i), 0);
            if vertical {
                std::mem::swap(&mut dx, &mut dy);
            }
            self.line_to(segs, dx, dy)?;
            i += 1;
            vertical = !vertical;
        }
        Ok(())
    }

    fn t2_rlineto(&mut self, segs: &mut SegList) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 2 || self.arg_top % 2 != 0 {
            return Err(INVALID_CFF_TABLE);
        }
        let mut i = 0;
        while i < self.arg_top {
            self.line_to(segs, self.a(i), self.a(i + 1))?;
            i += 2;
        }
        Ok(())
    }

    /// `rcurveline` is `{dxa dya dxb dyb dxc dyc}+ dxd dyd`.
    fn t2_rcurveline(&mut self, segs: &mut SegList) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 8 || self.arg_top % 6 != 2 {
            return Err(INVALID_CFF_TABLE);
        }
        let mut i = 0;
        let i_max = self.arg_top - 2;
        while i < i_max {
            self.cube_to(segs, self.six(i))?;
            i += 6;
        }
        self.line_to(segs, self.a(i), self.a(i + 1))
    }

    /// `rlinecurve` is `{dxa dya}+ dxb dyb dxc dyc dxd dyd`.
    fn t2_rlinecurve(&mut self, segs: &mut SegList) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 8 || self.arg_top % 2 != 0 {
            return Err(INVALID_CFF_TABLE);
        }
        let mut i = 0;
        let i_max = self.arg_top - 6;
        while i < i_max {
            self.line_to(segs, self.a(i), self.a(i + 1))?;
            i += 2;
        }
        self.cube_to(segs, self.six(i))
    }

    /// The six arguments from stack index `i`.
    fn six(&self, i: i32) -> [i32; 6] {
        [
            self.a(i),
            self.a(i + 1),
            self.a(i + 2),
            self.a(i + 3),
            self.a(i + 4),
            self.a(i + 5),
        ]
    }

    /// `t2CCurveto`: the hh / vv / hv / vh `xxcurveto` operators. N relative cubic curves need
    /// 6*N control points, but only 4*N+0 or 4*N+1 are used: all (or all but one) of the
    /// tangents are implicitly horizontal or vertical.
    ///
    /// `swap` is whether that constraint alternates along the piecewise curve; `vertical` is
    /// whether the first implicit constraint is vertical.
    fn t2_curveto(&mut self, segs: &mut SegList, swap: bool, mut vertical: bool) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 4 {
            return Err(INVALID_CFF_TABLE);
        }

        let mut i = 0;
        match self.arg_top & 3 {
            0 => {}
            1 => {
                if !swap {
                    i = 1;
                    if vertical {
                        self.t2.x = self.t2.x.wrapping_add(self.a(0));
                    } else {
                        self.t2.y = self.t2.y.wrapping_add(self.a(0));
                    }
                }
            }
            _ => return Err(INVALID_CFF_TABLE),
        }

        while i != self.arg_top {
            i = self.t2_curveto4(segs, swap, vertical, i)?;
            if i < 0 {
                return Err(INVALID_CFF_TABLE);
            }
            if swap {
                vertical = !vertical;
            }
        }
        Ok(())
    }

    /// `t2CCurveto4`: one curve from stack index `i`; returns the next index, or -1.
    fn t2_curveto4(
        &mut self,
        segs: &mut SegList,
        swap: bool,
        vertical: bool,
        mut i: i32,
    ) -> Result<i32> {
        if i + 4 > self.arg_top {
            return Ok(-1);
        }
        let mut dxa = self.a(i);
        let mut dya = 0;
        let dxb = self.a(i + 1);
        let dyb = self.a(i + 2);
        let mut dxc = self.a(i + 3);
        let mut dyc = 0;
        i += 4;

        if vertical {
            std::mem::swap(&mut dxa, &mut dya);
        }

        if swap && i + 1 == self.arg_top {
            dyc = self.a(i);
            i += 1;
        }

        if swap != vertical {
            std::mem::swap(&mut dxc, &mut dyc);
        }

        self.cube_to(segs, [dxa, dya, dxb, dyb, dxc, dyc])?;
        Ok(i)
    }

    fn t2_rrcurveto(&mut self, segs: &mut SegList) -> Result<()> {
        if !self.t2.seen_width || self.arg_top < 6 || self.arg_top % 6 != 0 {
            return Err(INVALID_CFF_TABLE);
        }
        let mut i = 0;
        while i != self.arg_top {
            self.cube_to(segs, self.six(i))?;
            i += 6;
        }
        Ok(())
    }

    // For the flex operators the flex depth is ignored and the segments are always cubic.

    fn t2_hflex(&mut self, segs: &mut SegList) -> Result<()> {
        let a = |i| self.a(i);
        let first = [a(0), 0, a(1), a(2), a(3), 0];
        let second = [a(4), 0, a(5), a(2).wrapping_neg(), a(6), 0];
        self.cube_to(segs, first)?;
        self.cube_to(segs, second)
    }

    fn t2_hflex1(&mut self, segs: &mut SegList) -> Result<()> {
        let dy1 = self.a(1);
        let dy2 = self.a(3);
        let dy5 = self.a(7);
        let dy6 = dy1.wrapping_neg().wrapping_sub(dy2).wrapping_sub(dy5);
        let first = [self.a(0), dy1, self.a(2), dy2, self.a(4), 0];
        let second = [self.a(5), 0, self.a(6), dy5, self.a(8), dy6];
        self.cube_to(segs, first)?;
        self.cube_to(segs, second)
    }

    fn t2_callsubr(&mut self) -> Result<()> {
        let gd = self.gd.ok_or(INVALID_CFF_TABLE)?;
        let mut subrs: &'a [u32] = &gd.single_subrs;
        if let Some(multi) = &gd.multi_subrs {
            if self.t2.fd_select_index_plus_one == 0 {
                let index = gd.fd_select.lookup(self.src, self.t2.glyph_index)?;
                if index < 0 || multi.len() <= index as usize {
                    return Err(INVALID_CFF_TABLE);
                }
                self.t2.fd_select_index_plus_one = index + 1;
            }
            let i = usize::try_from(self.t2.fd_select_index_plus_one - 1).unwrap_or(usize::MAX);
            subrs = multi.get(i).map_or(&[][..], Vec::as_slice);
        }
        self.t2_call(subrs)
    }

    /// `t2CCall`: calls the subroutine whose biased index is on the top of the stack.
    fn t2_call(&mut self, subrs: &[u32]) -> Result<()> {
        if self.call_top as usize == PS_CALL_STACK_SIZE || subrs.is_empty() {
            return Err(INVALID_CFF_TABLE);
        }
        let length = self.instructions.len() as u32;
        let entry = CallStackEntry {
            offset: self
                .instr_offset
                .wrapping_add(self.instr_length)
                .wrapping_sub(length),
            length,
        };
        if let Some(slot) = usize::try_from(self.call_top)
            .ok()
            .and_then(|i| self.call.get_mut(i))
        {
            *slot = entry;
        }
        self.call_top += 1;

        let subr_index = self
            .a(self.arg_top - 1)
            .wrapping_add(subr_bias(subrs.len() - 1));
        if subr_index < 0 || (subrs.len() - 1) as i32 <= subr_index {
            return Err(INVALID_CFF_TABLE);
        }
        let k = subr_index as usize;
        let (Some(&i), Some(&j)) = (subrs.get(k), subrs.get(k + 1)) else {
            return Err(GO_PANIC);
        };
        if j < i {
            return Err(INVALID_CFF_TABLE);
        }
        if j - i > MAX_GLYPH_DATA_LENGTH {
            return Err(UNSUPPORTED_GLYPH_DATA_LENGTH);
        }
        let buf = view(self.src, i64::from(i), i64::from(j - i))?;

        self.instructions = buf;
        self.instr_offset = i;
        self.instr_length = j - i;
        Ok(())
    }

    fn t2_return(&mut self) -> Result<()> {
        if self.call_top <= 0 {
            return Err(INVALID_CFF_TABLE);
        }
        self.call_top -= 1;
        let entry = usize::try_from(self.call_top)
            .ok()
            .and_then(|i| self.call.get(i))
            .copied()
            .unwrap_or_default();
        let buf = view(self.src, i64::from(entry.offset), i64::from(entry.length))?;

        self.instructions = buf;
        self.instr_offset = entry.offset;
        self.instr_length = entry.length;
        Ok(())
    }

    fn t2_endchar(&mut self, segs: &mut SegList) -> Result<()> {
        self.t2_read_width(0);
        if self.arg_top != 0 || self.has_more_instructions() {
            if self.arg_top == 4 {
                // The implicit "seac" command (5177.Type2.pdf Appendix C) is not processed.
                return Err(UNSUPPORTED_TYPE2_CHARSTRING);
            }
            return Err(INVALID_CFF_TABLE);
        }
        self.close_path(segs)?;
        self.t2.ended = true;
        Ok(())
    }
}

fn fixed_point(x: i32, y: i32) -> Point26_6 {
    Point26_6 {
        x: Int26_6(x),
        y: Int26_6(y),
    }
}

fn point_segment(op: SegmentOp, x: i32, y: i32) -> Segment {
    Segment {
        op,
        args: [
            fixed_point(x, y),
            Point26_6::default(),
            Point26_6::default(),
        ],
    }
}

/// `subrBias`: the subroutine index bias as per 5177.Type2.pdf section 4.7.
fn subr_bias(num_subroutines: usize) -> i32 {
    if num_subroutines < 1240 {
        return 107;
    }
    if num_subroutines < 33900 {
        return 1131;
    }
    32768
}

/// Runs `dict` as a Top DICT and returns its CharStrings offset (a test hook).
#[cfg(test)]
pub(crate) fn run_top_dict(dict: &[u8]) -> Result<i32> {
    let mut p = Interp::new(&[], None);
    p.run(Ctx::TopDict, dict, 0, 0, &mut SegList::new(0))?;
    Ok(p.top_dict.char_strings_offset)
}
