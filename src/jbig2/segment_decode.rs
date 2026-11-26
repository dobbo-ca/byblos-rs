//! The segment parser, the resource budgets and the stream decoder
//! (`internal/jbig2/segment_decode.go`).
//!
//! # Resource bounds
//!
//! Region dimensions are 32-bit (T.88 7.4.1), so a hostile header can ask for 2^64 pixels, and
//! the bitmap is allocated from that header before a coded bit is read. The acceptance bar is
//! proportionality, not smallness: the worst stream these budgets admit costs about what
//! decoding one maximal legitimate page costs, and anything whose decode cost exceeds the
//! pixels it can hand back by more than a small factor is refused from the headers. Seven
//! rules; the constants are a product decision and the only thing to retune, and they are kept
//! as Go has them.
//!
//! 1. Output: the page is at most [`MAX_PAGE_PIXELS`] (1<<26, 67,108,864). It admits A4, US
//!    Letter and US Legal at 600 dpi and A4 at 800 dpi; it refuses 1200 dpi and 600-dpi A3.
//! 2. Work: the regions sum to at most `MAX_PAGE_PIXELS`. Decode time is linear in this.
//! 3. Usefulness: a region that overhangs its page is clipped (T.88 6.2.4), so the regions may
//!    sum to at most [`MAX_REGION_OVERDRAW`] (4) times the page plus [`OVERDRAW_FLOOR_PIXELS`]
//!    (1<<16, an absolute cost, so that legal clips on small pages are not refused). A page of
//!    unknown height is sized from its regions, so this is evaluated after the height is
//!    resolved.
//! 4. Memory: the page's packed bytes plus every region's packed bytes are at most
//!    [`MAX_STREAM_BITMAP_BYTES`] (16 MiB). Pixels understate memory by up to 8x because rows
//!    are padded to a byte.
//! 5. Header cost: at most [`MAX_STREAM_SEGMENTS`] (1<<16) segments, checked while they are
//!    parsed, and at most [`MAX_STREAM_REFS`] referred-to numbers in total.
//! 6. Symbol and instance counts, from the headers: see `budget_symbol`.
//! 7. Symbol work, charged during decoding: see `budget_symbol`.
//!
//! There is no bound derivable from the input length: under TPGDON a blank 8192x65536 region
//! codes in seven bytes, so the only honest bound is a constant on what the headers claim.

use super::bitmap::{mask_padding, new_bitmap};
use super::budget_symbol::{MAX_STREAM_SYMBOLS, StreamBudget};
use super::generic_decode::decode_generic_region;
use super::segment::{
    NOMINAL_AT_TEMPLATE0, SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION, SEG_TYPE_PAGE_INFORMATION,
};
use super::symbol_decode::decode_symbol_dict;
use super::symbol_header::{
    SEG_TYPE_IMMEDIATE_LOSSLESS_TEXT_REGION, SEG_TYPE_IMMEDIATE_TEXT_REGION,
    SEG_TYPE_INTERMEDIATE_TEXT_REGION, SEG_TYPE_SYMBOL_DICTIONARY, parse_symbol_dict_header,
    parse_text_region_header,
};
use super::text_decode::decode_text_region;
use super::{be32, with_segment};
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

/// Intermediate generic region (T.88 7.3).
pub const SEG_TYPE_INTERMEDIATE_GENERIC_REGION: u8 = 36;
/// Immediate generic region.
pub const SEG_TYPE_IMMEDIATE_GENERIC_REGION: u8 = 38;
/// End of page.
pub const SEG_TYPE_END_OF_PAGE: u8 = 49;
/// End of stripe.
pub const SEG_TYPE_END_OF_STRIPE: u8 = 50;
/// End of file.
pub const SEG_TYPE_END_OF_FILE: u8 = 51;
/// Profiles.
pub const SEG_TYPE_PROFILES: u8 = 52;
/// Tables.
pub const SEG_TYPE_TABLES: u8 = 53;
/// Extension.
pub const SEG_TYPE_EXTENSION: u8 = 62;

/// Rule 1 and 2: the most pixels of a page, and of all the regions of a stream.
pub const MAX_PAGE_PIXELS: i64 = 1 << 26;
/// Rule 4: the most packed bitmap bytes of a stream.
pub const MAX_STREAM_BITMAP_BYTES: i64 = 16 << 20;
/// Rule 3: the regions may sum to this many times the page.
pub const MAX_REGION_OVERDRAW: i64 = 4;
/// Rule 3: the floor under the overdraw ratio, in pixels.
pub const OVERDRAW_FLOOR_PIXELS: i64 = 1 << 16;
/// Rule 5: the most segments of a stream.
pub const MAX_STREAM_SEGMENTS: usize = 1 << 16;
/// Rule 5: the most referred-to segment numbers of a stream, four per segment at the cap.
pub const MAX_STREAM_REFS: usize = 4 * MAX_STREAM_SEGMENTS;

/// The symbols a dictionary exports, shared (not copied) between the dictionaries that
/// re-export them and the text regions that place them.
pub type Symbols = Vec<Rc<Bitmap>>;

fn bad(s: String) -> Error {
    Error::Malformed(s)
}

fn unsupported(s: String) -> Error {
    Error::UnsupportedJbig2Feature(s)
}

/// `segment`: one parsed T.88 7.2 segment header together with the slice of the input holding
/// its data. The data is not copied. `ref_first` and `ref_count` are a range in the parsed
/// stream's shared refs slab, not a slice, to keep the descriptor small.
#[derive(Debug, Clone, Copy)]
pub struct Segment<'a> {
    pub number: u32,
    pub typ: u8,
    pub data: &'a [u8],
    pub ref_first: u32,
    pub ref_count: u32,
}

/// `parsedStream`: the segment descriptors and the slab their referred-to segment numbers live
/// in.
#[derive(Debug)]
pub struct ParsedStream<'a> {
    pub segs: Vec<Segment<'a>>,
    pub refs: Vec<u32>,
}

impl ParsedStream<'_> {
    /// `refsOf`: the segment numbers `sg` refers to, in the order the header lists them. The
    /// order is load-bearing: a text region's symbol list is the exported symbols of its
    /// referred-to dictionaries concatenated in exactly this order (T.88 7.4.3.1.7).
    pub fn refs_of(&self, sg: &Segment) -> &[u32] {
        let first = sg.ref_first as usize;
        self.refs
            .get(first..first + sg.ref_count as usize)
            .unwrap_or(&[])
    }
}

/// `parseSegments`: walks the sequence of segment headers in the embedded file organization
/// (T.88 7.2, and ISO 32000-1:2008 7.4.7: no file header, and the segment headers are not
/// separated from their data).
///
/// The unknown-data-length form (0xFFFFFFFF, T.88 7.2.7) is reported as an unsupported feature
/// rather than as damage: it is only decodable by running the arithmetic decoder to find the
/// end of the segment, and the stream is intact and legal.
///
/// Rule 5 is enforced here, because what it bounds is the cost of this function. The page
/// association of T.88 7.2.6 is enforced here too: segments that disagree about which page
/// they are on are refused. Page 0 is exempt (it marks a segment associated with no page, as a
/// global or end-of-file segment is). The decoder never routes by the field; a region declaring
/// page 2 under a page information segment declaring page 1 would otherwise be composited onto
/// page 1 with no error anywhere.
///
/// # Errors
/// [`Error::Malformed`] for damage and for a breached rule 5, [`Error::UnsupportedJbig2Feature`]
/// for an unknown data length, [`Error::OutOfMemory`] if the allocator refuses.
pub(crate) fn parse_segments(s: &[u8]) -> Result<ParsedStream<'_>> {
    let mut out: Vec<Segment> = Vec::new();
    let mut ref_slab: Vec<u32> = Vec::new();
    // The page every segment that names one is on, and the segment it was first seen from.
    // Zero means no segment has named a page yet, the value T.88 7.2.6 gives a segment
    // associated with no page.
    let (mut stream_page, mut stream_page_seg) = (0u32, 0u32);
    let mut off = 0usize;
    while off < s.len() {
        let rest = s.get(off..).unwrap_or(&[]);
        // The 11-byte minimum is a T.88 7.2 fact: four bytes of segment number, one of flags,
        // one referred-to count, one page association and four of data length. It also makes
        // the fixed-offset reads below safe.
        if rest.len() < 11 {
            return Err(bad(format!(
                "jbig2: segment header at offset {off} is {} bytes; the minimum is 11",
                rest.len()
            )));
        }
        let num = be32(rest, 0);
        let flags = rest.get(4).copied().unwrap_or(0);
        let typ = flags & 0x3F;
        let mut i = 5usize;

        // Referred-to segment count and retain flags (T.88 7.2.4). Counts up to 4 fit in the
        // top 3 bits of a single byte alongside the retain bits; a value of 7 selects the long
        // form, a 4-byte count whose low 29 bits are the count, followed by
        // ceil((count+1)/8) retain bytes.
        let mut count = usize::from(rest.get(i).copied().unwrap_or(0) >> 5);
        if count == 7 {
            let n = be32(rest, i) & 0x1FFF_FFFF;
            if u64::from(n) > s.len() as u64 {
                return Err(bad(format!(
                    "jbig2: segment {num}: refers to {n} segments in a {}-byte stream",
                    s.len()
                )));
            }
            count = n as usize;
            i += 4 + (count + 8) / 8;
        } else {
            i += 1;
        }

        // Referred-to segment numbers (T.88 7.2.5): sized by this segment's number, not by the
        // numbers being referred to.
        let ref_size = if num > 65536 {
            4
        } else if num > 256 {
            2
        } else {
            1
        };
        let ref_off = i;
        // Page association (T.88 7.2.6): four bytes when bit 6 of the flags is set, one
        // otherwise. Its value is read only after the bounds check below.
        let page_len = if flags & 0x40 != 0 { 4 } else { 1 };
        let hdr_end = count
            .checked_mul(ref_size)
            .and_then(|r| i.checked_add(r))
            .and_then(|p| p.checked_add(page_len));
        let page_off = hdr_end.map(|e| e - page_len);
        let (Some(end), Some(page_off)) = (hdr_end, page_off) else {
            return Err(bad(format!(
                "jbig2: segment {num}: header runs past the end of the stream"
            )));
        };
        i = end;
        if i.checked_add(4).is_none_or(|e| e > rest.len()) {
            return Err(bad(format!(
                "jbig2: segment {num}: header runs past the end of the stream"
            )));
        }

        // The page association is readable now. The only thing this decoder does with the
        // field is refuse a stream whose segments disagree about which page they are on.
        let page = if page_len == 4 {
            be32(rest, page_off)
        } else {
            u32::from(rest.get(page_off).copied().unwrap_or(0))
        };
        if page != 0 {
            if stream_page == 0 {
                stream_page = page;
                stream_page_seg = num;
            } else if page != stream_page {
                return Err(bad(format!(
                    "jbig2: segment {num} is associated with page {page} but segment {stream_page_seg} is \
                     associated with page {stream_page}; an embedded JBIG2 stream carries exactly one page \
                     (ISO 32000-1:2008 7.4.7)"
                )));
            }
        }

        let data_len = be32(rest, i);
        i += 4;
        if data_len == 0xFFFF_FFFF {
            return Err(unsupported(format!(
                "segment {num} has an unknown data length"
            )));
        }
        if u64::from(data_len) > (rest.len() - i) as u64 {
            return Err(bad(format!(
                "jbig2: segment {num} declares {data_len} bytes of data but only {} remain",
                rest.len() - i
            )));
        }
        // Rule 5, charged here because this append is the allocation it bounds.
        if out.len() == MAX_STREAM_SEGMENTS {
            return Err(bad(format!(
                "jbig2: stream carries more than {MAX_STREAM_SEGMENTS} segments; the limit is one region per \
                 row of the tallest page these budgets admit, and no encoder writes a page with more regions \
                 than it has rows"
            )));
        }
        // The referred-to segment numbers, retained because a text region's symbol list is
        // defined by them. Rule 5's other half: maxStreamRefs comes out of T.88 7.2.4, four
        // being the largest count the short form of the referred-to field encodes.
        if ref_slab.len() + count > MAX_STREAM_REFS {
            return Err(bad(format!(
                "jbig2: segment {num}: the stream refers to more than {MAX_STREAM_REFS} segments in total; the \
                 limit is {} per segment at the segment cap, which is the most the short form of T.88 7.2.4's \
                 referred-to field encodes",
                MAX_STREAM_REFS / MAX_STREAM_SEGMENTS
            )));
        }
        let first = ref_slab.len() as u32;
        ref_slab.try_reserve(count)?;
        for k in 0..count {
            let a = ref_off + k * ref_size;
            let p = rest.get(a..a + ref_size).unwrap_or(&[]);
            let mut v = 0u32;
            for &b in p {
                v = (v << 8) | u32::from(b);
            }
            ref_slab.push(v);
        }

        let seg_data = rest.get(i..i + data_len as usize).unwrap_or(&[]);
        out.try_reserve(1)?;
        out.push(Segment {
            number: num,
            typ,
            data: seg_data,
            ref_first: first,
            ref_count: count as u32,
        });
        off += i + data_len as usize;
    }
    if out.is_empty() {
        return Err(bad("jbig2: stream contains no segments".to_string()));
    }
    Ok(ParsedStream {
        segs: out,
        refs: ref_slab,
    })
}

/// `regionInfo`: the region segment information field of T.88 7.4.1: 17 bytes giving the
/// region's size, its position on the page, and the operator by which it combines with what is
/// already there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegionInfo {
    pub w: usize,
    pub h: usize,
    pub x: i64,
    pub y: i64,
    pub op: u8,
}

/// `parseRegionInfo`.
///
/// # Errors
/// [`Error::Malformed`] for a short field, a zero or over-cap size, or an operator above 4.
pub fn parse_region_info(d: &[u8]) -> Result<RegionInfo> {
    if d.len() < 17 {
        return Err(bad(format!(
            "jbig2: region segment info is {} bytes; want 17",
            d.len()
        )));
    }
    let w = be32(d, 0);
    let h = be32(d, 4);
    if w == 0 || h == 0 {
        return Err(bad(format!(
            "jbig2: region is {w}x{h}; dimensions must be positive"
        )));
    }
    if u64::from(w) * u64::from(h) > MAX_PAGE_PIXELS as u64 {
        return Err(bad(format!(
            "jbig2: region is {w}x{h}, {} pixels; the limit is {MAX_PAGE_PIXELS}",
            u64::from(w) * u64::from(h)
        )));
    }
    // Bits 0-2 of the flags byte are the external combination operator; bits 3-7 are reserved.
    // T.88 7.4.1.5 allows only 0-4 there, so 5-7 is a malformed field.
    let op = d.get(16).copied().unwrap_or(0) & 0x07;
    if op > 4 {
        return Err(bad(format!(
            "jbig2: external combination operator {op}; T.88 7.4.1.5 allows 0-4"
        )));
    }
    Ok(RegionInfo {
        w: w as usize,
        h: h as usize,
        x: i64::from(be32(d, 8)),
        y: i64::from(be32(d, 12)),
        op,
    })
}

/// `decodeGenericRegionSegment`: decodes the body of an immediate generic region segment
/// (T.88 7.4.6) and returns the region's bitmap and where it goes.
///
/// Everything this package cannot code for is rejected here, before any bit of the arithmetic
/// stream is read, because the MQ decoder returns a decision for any input: decoding an MMR or
/// template-1 region as if it were template 0 produces noise, not an error.
///
/// # Errors
/// [`Error::Malformed`], [`Error::UnsupportedJbig2Feature`] for MMR, a template other than 0,
/// EXTTEMPLATE or non-nominal AT pixels; [`Error::Alloc`], [`Error::OutOfMemory`],
/// [`Error::Cancelled`].
pub fn decode_generic_region_segment(
    d: &[u8],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(Bitmap, RegionInfo)> {
    let info = parse_region_info(d)?;
    if d.len() < 18 {
        return Err(bad(format!(
            "jbig2: generic region segment is {} bytes; want at least 18",
            d.len()
        )));
    }
    let flags = d.get(17).copied().unwrap_or(0);
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    let tpgdon = flags & 0x08 != 0;
    let ext_template = flags & 0x10 != 0;

    if mmr {
        return Err(unsupported("MMR-coded generic region".to_string()));
    }
    if template != 0 {
        return Err(unsupported(format!(
            "generic region GBTEMPLATE {template} (only 0 is implemented)"
        )));
    }
    if ext_template {
        return Err(unsupported("generic region EXTTEMPLATE".to_string()));
    }

    // AT pixels (T.88 7.4.6.3): four signed byte pairs for GBTEMPLATE 0.
    if d.len() < 26 {
        return Err(bad(format!(
            "jbig2: generic region segment is {} bytes; want at least 26 for the AT field",
            d.len()
        )));
    }
    let at = d.get(18..26).unwrap_or(&[]);
    if at != NOMINAL_AT_TEMPLATE0 {
        return Err(unsupported(format!(
            "generic region AT pixels {at:02X?} are not the nominal {NOMINAL_AT_TEMPLATE0:02X?}"
        )));
    }

    let b = decode_generic_region(
        d.get(26..).unwrap_or(&[]),
        info.w,
        info.h,
        tpgdon,
        limits,
        cancel,
    )?;
    Ok((b, info))
}

/// `composite`: draws `region` onto `page` at (x0, y0) under one of the external combination
/// operators of T.88 7.4.1 and Table 12. Pixels falling outside the page are dropped, which is
/// what T.88 6.2.4 requires of a region that overruns its page. `op` has already been validated
/// as 0-4 by [`parse_region_info`].
pub fn composite(page: &mut Bitmap, region: &Bitmap, x0: i64, y0: i64, op: u8) {
    for y in 0..region.height as i64 {
        let py = y0 + y;
        if !(0..page.height as i64).contains(&py) {
            continue;
        }
        for x in 0..region.width as i64 {
            let px = x0 + x;
            if !(0..page.width as i64).contains(&px) {
                continue;
            }
            let (s, dst) = (region.get(x, y), page.get(px, py));
            let v = match op {
                0 => dst | s,       // OR
                1 => dst & s,       // AND
                2 => dst ^ s,       // XOR
                3 => 1 - (dst ^ s), // XNOR
                _ => s,             // 4, REPLACE
            };
            page.set(px, py, v);
        }
    }
}

/// `DecodeEmbeddedStream`: decodes a JBIG2 bitstream in the embedded file organization, the
/// form ISO 32000-1:2008 7.4.7 requires of the PDF `JBIG2Decode` filter and
/// [`embedded_stream`](super::segment::embedded_stream) produces, and returns the composed page
/// bitmap. A set bit is ink. This is the no-globals case of
/// [`decode_embedded_stream_with_globals`].
///
/// # Errors
/// As [`decode_embedded_stream_with_globals`].
pub fn decode_embedded_stream(s: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Bitmap> {
    decode_embedded_stream_with_globals(&[], s, limits, cancel)
}

/// `globals ++ s`, copied only when there are globals.
fn join<'a>(
    globals: &[u8],
    s: &'a [u8],
    buf: &'a mut Vec<u8>,
    limits: &Limits,
) -> Result<&'a [u8]> {
    if globals.is_empty() {
        return Ok(s);
    }
    let max = limits.alloc_limit();
    let n = match globals.len().checked_add(s.len()) {
        Some(n) if n as u64 <= max => n,
        _ => {
            return Err(Error::Alloc {
                got: (globals.len() as u64).saturating_add(s.len() as u64),
                max,
            });
        }
    };
    buf.try_reserve_exact(n)?;
    buf.extend_from_slice(globals);
    buf.extend_from_slice(s);
    Ok(buf.as_slice())
}

/// `DecodeEmbeddedStreamWithGlobals`: decodes an embedded JBIG2 bitstream together with the
/// page-0 segments a PDF `/JBIG2Globals` object carries for it.
///
/// The two streams are one stream: T.88 knows nothing of the split, the segments are the
/// globals' followed by the page's. Concatenating the bytes rather than merging two parses
/// keeps rule 5 in one place.
///
/// It decodes immediate generic regions (GBTEMPLATE 0, nominal AT), arithmetically coded symbol
/// dictionaries and immediate text regions. Everything else (Huffman symbol coding, refinement,
/// halftones, MMR, the other templates, non-nominal AT pixels, intermediate regions) is
/// [`Error::UnsupportedJbig2Feature`]: a stream this package half-understands would yield a
/// raster that is wrong without being detectably wrong.
///
/// Allocations are charged against `limits.max_alloc` per bitmap, on top of the constant caps
/// above; the cancel flag is checked per row, per symbol and per region.
///
/// # Errors
/// [`Error::Malformed`], [`Error::UnsupportedJbig2Feature`], [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn decode_embedded_stream_with_globals(
    globals: &[u8],
    s: &[u8],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Bitmap> {
    let mut buf = Vec::new();
    let s = join(globals, s, &mut buf, limits)?;
    let mut p = plan_stream(s)?;

    let mut decoded: Vec<(Bitmap, RegionInfo)> = Vec::new();
    decoded.try_reserve_exact(p.region_count)?;
    // Exported symbols, keyed by the dictionary segment that exported them. A text region names
    // its dictionaries by segment number and by nothing else, so this is the only way back from
    // a reference to a symbol list.
    let mut exported: HashMap<u32, Symbols> = HashMap::new();

    for sg in &p.work {
        check_cancel(cancel)?;
        match sg.typ {
            SEG_TYPE_SYMBOL_DICTIONARY => {
                let input = symbols_for(&p.parsed, sg, &exported)?;
                let syms = decode_symbol_dict(sg, &input, &mut p.budget, limits, cancel)
                    .map_err(|e| with_segment(sg.number, e))?;
                if exported.contains_key(&sg.number) {
                    return Err(bad(format!(
                        "jbig2: two symbol dictionaries are both numbered {}; T.88 7.2.2 numbers the segments \
                         of a file uniquely, and a text region names its dictionary by that number alone",
                        sg.number
                    )));
                }
                exported.insert(sg.number, syms);
            }
            SEG_TYPE_IMMEDIATE_TEXT_REGION | SEG_TYPE_IMMEDIATE_LOSSLESS_TEXT_REGION => {
                let syms = symbols_for(&p.parsed, sg, &exported)?;
                let r = decode_text_region(sg, &syms, &mut p.budget, limits, cancel)
                    .map_err(|e| with_segment(sg.number, e))?;
                decoded.push(r);
            }
            _ => {
                let r = decode_generic_region_segment(sg.data, limits, cancel)
                    .map_err(|e| with_segment(sg.number, e))?;
                decoded.push(r);
            }
        }
    }

    let mut out = new_bitmap(p.page_w, p.page_h, limits)?;
    if p.page_default == 1 {
        out.pix.fill(0xFF);
        mask_padding(&mut out);
    }
    for (b, info) in &decoded {
        check_cancel(cancel)?;
        composite(&mut out, b, info.x, info.y, info.op);
    }
    Ok(out)
}

/// `symbolsFor`: the symbol list a segment's referred-to dictionaries export, concatenated in
/// the order the header refers to them (T.88 7.4.3.1.7). The order is the numbering: referring
/// to dictionaries A then B and B then A produce two different alphabets.
///
/// A reference to a segment that is not a symbol dictionary is ignored rather than refused: a
/// text region legitimately refers to the table segments its Huffman tables live in.
fn symbols_for(
    parsed: &ParsedStream,
    sg: &Segment,
    exported: &HashMap<u32, Symbols>,
) -> Result<Symbols> {
    let mut out: Symbols = Vec::new();
    for n in parsed.refs_of(sg) {
        let Some(syms) = exported.get(n) else {
            continue;
        };
        out.try_reserve(syms.len())?;
        out.extend(syms.iter().cloned());
        if out.len() as i64 > MAX_STREAM_SYMBOLS {
            break;
        }
    }
    if out.len() as i64 > MAX_STREAM_SYMBOLS {
        return Err(bad(format!(
            "jbig2: segment {} refers to {} symbols; the limit is {}",
            sg.number,
            out.len(),
            MAX_STREAM_SYMBOLS
        )));
    }
    Ok(out)
}

/// `PageSize`: the page a stream resolves to, reading only its headers. Every refusal
/// [`decode_embedded_stream`] makes before decoding (a malformed header, a segment type this
/// package will not touch, the resource budgets) it makes here too, at the same cost, because
/// both run `plan_stream`. It lets the PDF layer check a stream against the image dictionary
/// before decoding it.
///
/// # Errors
/// As `plan_stream`.
pub fn page_size(s: &[u8], limits: &Limits) -> Result<(usize, usize)> {
    page_size_with_globals(&[], s, limits)
}

/// `PageSizeWithGlobals`: [`page_size`] over a stream and the page-0 segments a PDF
/// `/JBIG2Globals` object carries for it. The globals matter to the answer, not just to the
/// refusals: a page whose only region is a text region is refused without them.
///
/// # Errors
/// As `plan_stream`.
pub fn page_size_with_globals(globals: &[u8], s: &[u8], limits: &Limits) -> Result<(usize, usize)> {
    let mut buf = Vec::new();
    let s = join(globals, s, &mut buf, limits)?;
    let p = plan_stream(s)?;
    Ok((p.page_w, p.page_h))
}

/// `streamPlan`: everything a stream's headers determine: the page to compose onto, and the
/// segments that will be decoded. Producing one costs header parsing only: no coded data is
/// read and no bitmap is allocated, which is what lets every refusal happen before the
/// expensive part.
#[derive(Debug)]
pub struct StreamPlan<'a> {
    pub page_w: usize,
    pub page_h: usize,
    pub page_default: u8,
    /// Every segment that produces something, symbol dictionaries and the regions composed
    /// onto the page, in stream order, which is the order they must be decoded in.
    pub work: Vec<Segment<'a>>,
    /// How many of `work` are regions.
    pub region_count: usize,
    pub parsed: ParsedStream<'a>,
    /// The header-time totals, carried forward into decoding, where symbol sizes finally
    /// become knowable.
    pub budget: StreamBudget,
}

/// `planStream`: parses the headers and applies the budget rules that need no coded data.
///
/// # Errors
/// [`Error::Malformed`] for damage or a breached budget rule, [`Error::UnsupportedJbig2Feature`]
/// for a segment type or feature this package will not decode.
pub(crate) fn plan_stream(s: &[u8]) -> Result<StreamPlan<'_>> {
    let parsed = parse_segments(s)?;

    // Region segments are collected rather than decoded so that a page information segment
    // declaring the unknown height 0xFFFFFFFF (T.88 7.4.8.2, a striped page) can be sized from
    // the regions that will land on it.
    let (mut page_w, mut page_h): (i64, i64) = (0, 0);
    let mut page_default = 0u8;
    let mut saw_page_info = false;
    let mut page_known_height = true;
    // Symbol dictionaries and the regions that consume them go in one list in stream order,
    // because that order is the only thing that sequences them.
    let mut work: Vec<Segment> = Vec::new();
    work.try_reserve_exact(parsed.segs.len())?;
    let mut region_count = 0usize;
    let (mut symbols_declared, mut instances_declared) = (0i64, 0i64);

    for sg in &parsed.segs {
        match sg.typ {
            SEG_TYPE_PAGE_INFORMATION => {
                if saw_page_info {
                    return Err(unsupported(
                        "more than one page information segment".to_string(),
                    ));
                }
                saw_page_info = true;
                if sg.data.len() < 19 {
                    return Err(bad(format!(
                        "jbig2: page information segment is {} bytes; want 19",
                        sg.data.len()
                    )));
                }
                let w = be32(sg.data, 0);
                let mut h = be32(sg.data, 4);
                // Bit 2 of the flags byte is the page's default pixel value.
                page_default = (sg.data.get(16).copied().unwrap_or(0) >> 2) & 1;
                if h == 0xFFFF_FFFF {
                    page_known_height = false;
                    h = 0;
                }
                if w == 0 {
                    return Err(bad(
                        "jbig2: page information segment declares a width of 0".to_string()
                    ));
                }
                if i64::from(w) > MAX_PAGE_PIXELS {
                    return Err(bad(format!(
                        "jbig2: page is {w} pixels wide; the limit is {MAX_PAGE_PIXELS}"
                    )));
                }
                page_w = i64::from(w);
                page_h = i64::from(h);
            }

            SEG_TYPE_IMMEDIATE_GENERIC_REGION
            | SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION
            | SEG_TYPE_IMMEDIATE_TEXT_REGION
            | SEG_TYPE_IMMEDIATE_LOSSLESS_TEXT_REGION => {
                region_count += 1;
                work.push(*sg);
            }

            SEG_TYPE_SYMBOL_DICTIONARY => {
                // Header-time half of the symbol budget. The count is four bytes and the slots
                // are allocated from it, so it is refused here rather than on the first symbol.
                let h =
                    parse_symbol_dict_header(sg.data).map_err(|e| with_segment(sg.number, e))?;
                h.unsupported().map_err(|e| with_segment(sg.number, e))?;
                symbols_declared += i64::from(h.num_new);
                work.push(*sg);
            }

            SEG_TYPE_END_OF_PAGE
            | SEG_TYPE_END_OF_STRIPE
            | SEG_TYPE_END_OF_FILE
            | SEG_TYPE_PROFILES
            | SEG_TYPE_TABLES
            | SEG_TYPE_EXTENSION => {
                // Carry no page content this decoder needs. End-of-stripe would matter to a
                // striped page, but region segments carry absolute Y coordinates, so the page
                // extent is already recoverable without it.
            }

            SEG_TYPE_INTERMEDIATE_GENERIC_REGION => {
                // Composed into an auxiliary buffer by a later refinement or text region, never
                // onto the page. Treating it as immediate would put a region on the page that
                // the producer did not intend to be there.
                return Err(unsupported(format!(
                    "intermediate generic region (segment {})",
                    sg.number
                )));
            }

            SEG_TYPE_INTERMEDIATE_TEXT_REGION => {
                return Err(unsupported(format!(
                    "intermediate text region (segment {})",
                    sg.number
                )));
            }

            t => {
                return Err(unsupported(format!(
                    "segment type {t} (segment {})",
                    sg.number
                )));
            }
        }
    }

    if !saw_page_info {
        return Err(bad(
            "jbig2: stream has no page information segment".to_string()
        ));
    }
    if region_count == 0 {
        return Err(bad(
            "jbig2: stream has no immediate region segment".to_string()
        ));
    }
    // Text regions declare their instance count in the header, so the other header-time half of
    // the symbol budget is charged from the same walk.
    for sg in &work {
        if sg.typ != SEG_TYPE_IMMEDIATE_TEXT_REGION
            && sg.typ != SEG_TYPE_IMMEDIATE_LOSSLESS_TEXT_REGION
        {
            continue;
        }
        let h = parse_text_region_header(sg.data).map_err(|e| with_segment(sg.number, e))?;
        h.unsupported().map_err(|e| with_segment(sg.number, e))?;
        instances_declared += i64::from(h.num_instances);
    }

    // Everything below is read from region headers. No coded data is touched and no bitmap is
    // allocated, so a stream that cannot be afforded is refused for the price of parsing 26
    // bytes per region. Region info that does not parse is passed over rather than reported
    // here: the decode loop reaches it in stream order and says what is actually wrong with it.
    let (mut region_pixels, mut bm_bytes) = (0i64, 0i64);
    for sg in &work {
        if sg.typ == SEG_TYPE_SYMBOL_DICTIONARY {
            // It has no region information field and paints nothing; its cost is charged
            // during decoding.
            continue;
        }
        let Ok(info) = parse_region_info(sg.data) else {
            continue;
        };
        region_pixels += info.w as i64 * info.h as i64;
        bm_bytes += info.w.div_ceil(8) as i64 * info.h as i64;
        if region_pixels > MAX_PAGE_PIXELS || bm_bytes > MAX_STREAM_BITMAP_BYTES {
            return Err(bad(format!(
                "jbig2: segment {}: the stream's {region_count} regions want to decode {region_pixels} pixels \
                 into {bm_bytes} bytes; the budget for one stream is {MAX_PAGE_PIXELS} pixels in \
                 {MAX_STREAM_BITMAP_BYTES} bytes",
                sg.number
            )));
        }
        // An unknown page height is resolved from the region headers, not from the decoded
        // regions: a region placed at y = 0xFFFFFFFF asks for a four-billion-row page, and that
        // has to be refused before the regions under it are decoded.
        if !page_known_height {
            let bottom = info.y + info.h as i64;
            if bottom > page_h {
                page_h = bottom;
            }
        }
    }
    if page_h <= 0 {
        return Err(bad(format!("jbig2: page height resolves to {page_h}")));
    }

    // Rules 1 and 4. The page bitmap is held alongside every region bitmap and is the only
    // thing the caller gets back, so it is capped on its own pixels and charged into the shared
    // memory budget. It is charged last so that a stream whose regions alone are unaffordable is
    // reported against the region that broke the budget. page_w is at most 2^26 and page_h at
    // most 2^32 + 2^26, so the products fit i64.
    let page_pixels = page_w * page_h;
    bm_bytes += (page_w as u64).div_ceil(8) as i64 * page_h;
    if page_pixels > MAX_PAGE_PIXELS || bm_bytes > MAX_STREAM_BITMAP_BYTES {
        return Err(bad(format!(
            "jbig2: a {page_w}x{page_h} page under {region_count} region(s) wants {page_pixels} page pixels and \
             {bm_bytes} bytes of bitmap; the budget for one stream is {MAX_PAGE_PIXELS} pixels in \
             {MAX_STREAM_BITMAP_BYTES} bytes"
        )));
    }

    // Rule 3, run here because a page of unknown height is not sized until the loop above has
    // finished. Every region pixel outside the page is decoded and then discarded.
    if region_pixels > MAX_REGION_OVERDRAW * page_pixels + OVERDRAW_FLOOR_PIXELS {
        return Err(bad(format!(
            "jbig2: a {page_w}x{page_h} page under {region_count} region(s) wants to decode {region_pixels} \
             pixels onto a page that can show {page_pixels}; the budget admits {MAX_REGION_OVERDRAW}x the page \
             plus {OVERDRAW_FLOOR_PIXELS} pixels, and everything past the page's edges is decoded and then \
             discarded"
        )));
    }

    // The two header-time symbol rules, charged into the same budget the decode-time rules
    // continue, so a stream cannot spend each in full.
    let mut budget = StreamBudget {
        pixels: region_pixels,
        bm_bytes,
        page_pixels,
        placement_limit: MAX_REGION_OVERDRAW * page_pixels + OVERDRAW_FLOOR_PIXELS,
        ..StreamBudget::default()
    };
    budget.charge_symbols(symbols_declared)?;
    budget.charge_instances(instances_declared)?;
    // Charged for the refusal only: each dictionary and region charges its own share again as
    // it is decoded, and the running totals must not count them twice.
    budget.symbols = 0;
    budget.instances = 0;

    Ok(StreamPlan {
        page_w: page_w as usize,
        page_h: page_h as usize,
        page_default,
        work,
        region_count,
        parsed,
        budget,
    })
}
