//! The symbol dictionary and text region segment headers of T.88 7.4.4 and 7.4.3
//! (`internal/jbig2/symbol_header.go`).

use super::segment_decode::{RegionInfo, parse_region_info};
use super::{be16, be32};
use crate::error::{Error, Result};

/// Symbol dictionary (T.88 7.3).
pub const SEG_TYPE_SYMBOL_DICTIONARY: u8 = 0;
/// Intermediate text region.
pub const SEG_TYPE_INTERMEDIATE_TEXT_REGION: u8 = 4;
/// Immediate text region.
pub const SEG_TYPE_IMMEDIATE_TEXT_REGION: u8 = 6;
/// Immediate lossless text region.
pub const SEG_TYPE_IMMEDIATE_LOSSLESS_TEXT_REGION: u8 = 7;

fn bad(s: String) -> Error {
    Error::Malformed(s)
}

/// `symbolDictHeader`: the symbol dictionary segment header of T.88 7.4.4, everything before
/// the arithmetic data, parsed and no more.
///
/// The Huffman selector fields are read and kept even though nothing decodes a Huffman symbol
/// dictionary: a refusal that cannot say which Huffman shape it met is a refusal nobody can act
/// on.
#[derive(Debug, Clone, Default)]
pub struct SymbolDictHeader {
    /// SDHUFF.
    pub huff: bool,
    /// SDREFAGG.
    pub ref_agg: bool,
    /// SDHUFFDH selector, 0-3.
    pub huff_dh: u32,
    /// SDHUFFDW selector, 0-3.
    pub huff_dw: u32,
    /// SDHUFFBMSIZE selector, 0-1.
    pub huff_bm: u32,
    /// SDHUFFAGGINST selector, 0-1.
    pub huff_agg: u32,
    /// Bitmap coding context used.
    pub ctx_used: bool,
    /// SDTEMPLATE, 0-3.
    pub template: usize,
    /// SDRTEMPLATE, 0-1.
    pub r_template: u32,
    /// SDAT: 4 pairs for template 0, else 1.
    pub at: [[i8; 2]; 4],
    pub n_at: usize,
    /// SDRAT, when `ref_agg` and `r_template == 0`.
    pub rat: [[i8; 2]; 2],
    /// SDNUMEXSYMS.
    pub num_ex: u32,
    /// SDNUMNEWSYMS.
    pub num_new: u32,
    /// Offset in the segment data where the coded data starts.
    pub data_off: usize,
}

fn at_pair(d: &[u8], i: usize) -> [i8; 2] {
    [
        d.get(i).copied().unwrap_or(0) as i8,
        d.get(i + 1).copied().unwrap_or(0) as i8,
    ]
}

/// `parseSymbolDictHeader`: T.88 7.4.4.1 through 7.4.4.5.
///
/// Every length is checked before it is read. The fields are variable-length in two independent
/// ways, the AT field's size depends on SDTEMPLATE and the refinement AT field is present only
/// under SDREFAGG, so a single up-front minimum would be wrong both ways.
///
/// # Errors
/// [`Error::Malformed`] when the segment is too short for a field.
pub(crate) fn parse_symbol_dict_header(d: &[u8]) -> Result<SymbolDictHeader> {
    let mut h = SymbolDictHeader::default();
    if d.len() < 2 {
        return Err(bad(format!(
            "jbig2: symbol dictionary segment is {} bytes; want at least 2 for its flags",
            d.len()
        )));
    }
    let f = u32::from(be16(d, 0));
    h.huff = f & 0x0001 != 0;
    h.ref_agg = f & 0x0002 != 0;
    h.huff_dh = (f >> 2) & 3;
    h.huff_dw = (f >> 4) & 3;
    h.huff_bm = (f >> 6) & 1;
    h.huff_agg = (f >> 7) & 1;
    h.ctx_used = f & 0x0100 != 0;
    // Bit 9, "bitmap coding context retained", is deliberately not kept. A dictionary that
    // retains its statistics for a later segment costs this decoder nothing; what it cannot do
    // is read a retained context, and that is bit 8 above.
    h.template = ((f >> 10) & 3) as usize;
    h.r_template = (f >> 12) & 1;
    let mut i = 2usize;

    // T.88 7.4.4.1.2: the AT field is present only for arithmetic coding, and carries one pair
    // for templates 1-3 against template 0's four.
    if !h.huff {
        h.n_at = if h.template == 0 { 4 } else { 1 };
        if d.len() < i + 2 * h.n_at {
            return Err(bad(format!(
                "jbig2: symbol dictionary segment is {} bytes; want {} for the AT field of template {}",
                d.len(),
                i + 2 * h.n_at,
                h.template
            )));
        }
        for k in 0..h.n_at {
            if let Some(p) = h.at.get_mut(k) {
                *p = at_pair(d, i);
            }
            i += 2;
        }
    }
    if h.ref_agg && h.r_template == 0 {
        if d.len() < i + 4 {
            return Err(bad(format!(
                "jbig2: symbol dictionary segment is {} bytes; want {} for the refinement AT field",
                d.len(),
                i + 4
            )));
        }
        for k in 0..2 {
            if let Some(p) = h.rat.get_mut(k) {
                *p = at_pair(d, i);
            }
            i += 2;
        }
    }
    if d.len() < i + 8 {
        return Err(bad(format!(
            "jbig2: symbol dictionary segment is {} bytes; want {} for its two symbol counts",
            d.len(),
            i + 8
        )));
    }
    h.num_ex = be32(d, i);
    h.num_new = be32(d, i + 4);
    h.data_off = i + 8;
    Ok(h)
}

impl SymbolDictHeader {
    /// `unsupported`: the symbol dictionary variants this package parses and will not decode.
    ///
    /// It is checked from the header walk, before any budget is charged, and the order is not
    /// cosmetic: a Huffman dictionary's header has no AT field, so its two symbol counts sit
    /// eight bytes earlier than an arithmetic one's. Read under the wrong assumption they are
    /// whatever the coded data starts with, and a legal Huffman stream would be reported as one
    /// asking for four billion symbols, which is damage rather than a missing feature.
    ///
    /// # Errors
    /// [`Error::UnsupportedJbig2Feature`] for SDHUFF, SDREFAGG or a retained coding context.
    pub fn unsupported(&self) -> Result<()> {
        if self.huff {
            return Err(Error::UnsupportedJbig2Feature(
                "Huffman-coded symbol dictionary (SDHUFF)".to_string(),
            ));
        }
        if self.ref_agg {
            return Err(Error::UnsupportedJbig2Feature(
                "symbol dictionary using refinement or aggregate coding (SDREFAGG)".to_string(),
            ));
        }
        if self.ctx_used {
            // T.88 7.4.4.1.1 bit 8: the dictionary resumes the arithmetic statistics another
            // segment retained. Nothing here retains any, so decoding it would start from reset
            // statistics and desynchronise on the first symbol.
            return Err(Error::UnsupportedJbig2Feature(
                "symbol dictionary using a retained bitmap coding context".to_string(),
            ));
        }
        Ok(())
    }
}

/// Reference corner: bottom left (T.88 6.4.5 step 3c x). The number is the encoding in the text
/// region flags field.
pub const REF_CORNER_BOTTOM_LEFT: u32 = 0;
/// Reference corner: top left.
pub const REF_CORNER_TOP_LEFT: u32 = 1;
/// Reference corner: bottom right.
pub const REF_CORNER_BOTTOM_RIGHT: u32 = 2;
/// Reference corner: top right.
pub const REF_CORNER_TOP_RIGHT: u32 = 3;

/// `textRegionHeader`: the text region segment header of T.88 7.4.3: the region segment
/// information field, the flags, and the instance count.
#[derive(Debug, Clone, Default)]
pub struct TextRegionHeader {
    pub info: RegionInfo,
    /// SBHUFF.
    pub huff: bool,
    /// SBREFINE.
    pub refine: bool,
    /// LOGSBSTRIPS, 0-3; SBSTRIPS is `1 << log_strips`.
    pub log_strips: u32,
    /// REFCORNER.
    pub ref_corner: u32,
    /// TRANSPOSED.
    pub transposed: bool,
    /// SBCOMBOP.
    pub comb_op: u8,
    /// SBDEFPIXEL.
    pub def_pixel: u8,
    /// SBDSOFFSET, a signed 5-bit field.
    pub ds_offset: i64,
    /// SBRTEMPLATE.
    pub r_template: u32,
    /// The Huffman selector word, present only under SBHUFF.
    pub huff_flags: u16,
    /// SBRAT, when `refine` and `r_template == 0`.
    pub rat: [[i8; 2]; 2],
    /// SBNUMINSTANCES.
    pub num_instances: u32,
    /// Offset in the segment data where the symbol ID codes start.
    pub data_off: usize,
}

/// `parseTextRegionHeader`.
///
/// # Errors
/// [`Error::Malformed`] when the region information or a field does not fit the segment.
pub(crate) fn parse_text_region_header(d: &[u8]) -> Result<TextRegionHeader> {
    let mut h = TextRegionHeader {
        info: parse_region_info(d)?,
        ..TextRegionHeader::default()
    };
    if d.len() < 19 {
        return Err(bad(format!(
            "jbig2: text region segment is {} bytes; want at least 19 for its flags",
            d.len()
        )));
    }
    let f = u32::from(be16(d, 17));
    h.huff = f & 0x0001 != 0;
    h.refine = f & 0x0002 != 0;
    h.log_strips = (f >> 2) & 3;
    h.ref_corner = (f >> 4) & 3;
    h.transposed = f & 0x0040 != 0;
    h.comb_op = ((f >> 7) & 3) as u8;
    h.def_pixel = ((f >> 9) & 1) as u8;
    // SBDSOFFSET is five bits in two's complement (T.88 7.4.3.1.1), so the range is -16..15 and
    // the sign extension is not optional: an encoder writing -1 writes 31, and reading it as 31
    // shifts every symbol after the first in a strip 32 pixels to the right.
    h.ds_offset = i64::from((f >> 10) & 0x1F);
    if h.ds_offset > 15 {
        h.ds_offset -= 32;
    }
    h.r_template = (f >> 15) & 1;
    let mut i = 19usize;

    if h.huff {
        if d.len() < i + 2 {
            return Err(bad(format!(
                "jbig2: text region segment is {} bytes; want {} for its Huffman flags",
                d.len(),
                i + 2
            )));
        }
        h.huff_flags = be16(d, i);
        i += 2;
    }
    if h.refine && h.r_template == 0 {
        if d.len() < i + 4 {
            return Err(bad(format!(
                "jbig2: text region segment is {} bytes; want {} for the refinement AT field",
                d.len(),
                i + 4
            )));
        }
        for k in 0..2 {
            if let Some(p) = h.rat.get_mut(k) {
                *p = at_pair(d, i);
            }
            i += 2;
        }
    }
    if d.len() < i + 4 {
        return Err(bad(format!(
            "jbig2: text region segment is {} bytes; want {} for its instance count",
            d.len(),
            i + 4
        )));
    }
    h.num_instances = be32(d, i);
    h.data_off = i + 4;
    Ok(h)
}

impl TextRegionHeader {
    /// `unsupported`: the text region variants this package parses and will not decode. Same
    /// ordering argument as the symbol dictionary's: a Huffman text region carries a selector
    /// word the arithmetic one does not, so every field after it is displaced.
    ///
    /// # Errors
    /// [`Error::UnsupportedJbig2Feature`] for SBHUFF or SBREFINE.
    pub fn unsupported(&self) -> Result<()> {
        if self.huff {
            return Err(Error::UnsupportedJbig2Feature(
                "Huffman-coded text region (SBHUFF)".to_string(),
            ));
        }
        if self.refine {
            return Err(Error::UnsupportedJbig2Feature(
                "text region using refinement coding (SBREFINE)".to_string(),
            ));
        }
        Ok(())
    }
}

/// `symCodeLen`: SBSYMCODELEN of T.88 6.4.6, the number of bits an arithmetically coded symbol
/// ID occupies, `ceil(log2(n))` over the symbol list.
///
/// `n == 1` gives zero bits, and that is the specification rather than an unhandled edge: with
/// one symbol in the list there is nothing to distinguish, and `decode_iaid` reads no bits.
/// Encoders are known to write one bit there instead, and this decoder does not try to detect
/// which; guessing wrong desynchronises every decision after it.
pub fn sym_code_len(n: usize) -> u32 {
    let mut bits = 0u32;
    while (1usize << bits) < n {
        bits += 1;
    }
    bits
}
