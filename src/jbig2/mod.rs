//! A lossless JBIG2 codec: generic regions in both directions, and symbol mode for reading.
//! Wave 1b port of byblos `internal/jbig2/{doc,bitmap,mq,mq_decode,arith_int,generic,
//! generic_decode,generic_general,segment,segment_decode,symbol_header,symbol_decode,
//! text_decode,budget_symbol}.go` (not `profile.go`) and the two codec entry points of
//! `jbig2.go` that need nothing from the PDF layer, [`encode_generic`] and [`decode_generic`].
//!
//! Scope is deliberately narrow. It implements:
//!
//! * the MQ arithmetic coder of ITU-T T.88 (02/2000) Annex E.2 for encoding and E.3 for
//!   decoding, and the arithmetic integer decoding procedure of Annex A;
//! * the generic region coding procedure of T.88 6.2 with GBTEMPLATE 0, nominal AT pixels and
//!   TPGDON, in both directions, and, for decoding only, the same procedure over all four
//!   templates with arbitrary AT pixels, which is what a symbol dictionary codes its symbols
//!   with;
//! * the symbol dictionary procedure of T.88 6.5 and the text region procedure of 6.4,
//!   arithmetic variants, for decoding only;
//! * the segment syntax of T.88 7.2, 7.4.1, 7.4.3, 7.4.4, 7.4.6 and 7.4.8, parsed in the
//!   embedded file organization that ISO 32000-1:2008 7.4.7 requires of the PDF `JBIG2Decode`
//!   filter, and the two segment types of it that the encoder emits.
//!
//! It implements no refinement region, no halftone region, no Huffman symbol coding and no MMR
//! fallback, in either direction, and it writes no symbol dictionary. Every coding procedure is
//! bit-exact by construction: no character can ever be substituted for another. Everything the
//! decoder cannot code for is rejected explicitly, by inspection of the segment headers, before
//! any coded bit is read, as [`Error::UnsupportedJbig2Feature`]: the MQ decoder yields a
//! decision for any input, so without those rejections the failure mode is a wrong raster
//! rather than an error.
//!
//! The decode side enforces Go's constant resource caps ([`segment_decode::MAX_PAGE_PIXELS`],
//! [`segment_decode::MAX_STREAM_BITMAP_BYTES`] and the others in `segment_decode` and
//! `budget_symbol`) and, on top, the per-call `max_alloc` of [`Limits`] on every bitmap and on
//! the coded output. The cancel flag is checked per row, per symbol and per region.
//!
//! Bitmaps are [`crate::Bitmap`]: a set bit is ink, as in JBIG2, so nothing inverts. The helpers
//! `crate::Bitmap` lacks (`new_bitmap`, `mask_padding`, `row_equal_above`) are in [`bitmap`].
//!
//! This package is original work written from the published specifications.

pub mod arith_int;
pub mod bitmap;
pub mod budget_symbol;
pub mod generic;
pub mod generic_decode;
pub mod generic_general;
pub mod mq;
pub mod mq_decode;
pub mod segment;
pub mod segment_decode;
pub mod symbol_decode;
pub mod symbol_header;
pub mod text_decode;

#[cfg(test)]
mod tests;

pub use segment::embedded_stream;
pub use segment_decode::{
    MAX_PAGE_PIXELS, decode_embedded_stream, decode_embedded_stream_with_globals, page_size,
    page_size_with_globals,
};

use crate::bitmap::Bitmap;
use crate::encoded::{ColorSpace, EncodedImage};
use crate::error::{Error, Result};
use crate::limits::Limits;
use std::sync::atomic::AtomicBool;

/// `CapabilityJBIG2Generic`: the provenance capability string recorded for a page compressed
/// with lossless JBIG2 generic region coding.
pub const CAPABILITY_JBIG2_GENERIC: &str = "jbig2-generic";

/// `EncodeJBIG2Generic`: compresses a bitonal bitmap with lossless JBIG2 generic region coding
/// and returns a JBIG2 bitstream in the embedded file organization required by the PDF
/// `JBIG2Decode` filter. The bitmap is copied (against `limits.max_alloc`) because the encoder
/// zeroes padding bits in place; [`embedded_stream`] skips the copy.
///
/// The image XObject that embeds the result takes exactly these entries: `/Width`, `/Height`,
/// `/ColorSpace /DeviceGray`, `/BitsPerComponent 1`, `/Filter /JBIG2Decode`, and no `/Decode`
/// array (the filter already presents a JBIG2 black pixel as the sample that renders black), no
/// `/DecodeParms` and no `/JBIG2Globals`.
///
/// # Errors
/// See [`embedded_stream`].
pub fn encode_generic(b: &Bitmap, limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    // Go's four argument checks, in Go's order and wording, before anything is copied.
    if b.width == 0 || b.height == 0 {
        return Err(Error::Malformed(format!(
            "byblos: EncodeJBIG2Generic: bitmap is {}x{}; dimensions must be positive",
            b.width, b.height
        )));
    }
    if b.stride < b.width.div_ceil(8) {
        return Err(Error::Malformed(format!(
            "byblos: EncodeJBIG2Generic: stride {} is too small for width {}",
            b.stride, b.width
        )));
    }
    let want = (b.stride as u128) * (b.height as u128);
    if want > i64::MAX as u128 {
        return Err(Error::Malformed(format!(
            "byblos: EncodeJBIG2Generic: stride {} and height {} overflow",
            b.stride, b.height
        )));
    }
    if (b.pix.len() as u128) < want {
        return Err(Error::Malformed(format!(
            "byblos: EncodeJBIG2Generic: Pix is {} bytes; want at least {want}",
            b.pix.len()
        )));
    }
    let max = limits.alloc_limit();
    if b.pix.len() as u64 > max {
        return Err(Error::Alloc {
            got: b.pix.len() as u64,
            max,
        });
    }
    let mut pix = Vec::new();
    pix.try_reserve_exact(b.pix.len())?;
    pix.extend_from_slice(&b.pix);
    let mut work = Bitmap {
        width: b.width,
        height: b.height,
        stride: b.stride,
        pix,
    };
    embedded_stream(&mut work, limits, cancel)
}

/// `EncodeJBIG2Image`: [`encode_generic`] wrapped as an [`EncodedImage`] (`/DeviceGray`, one
/// bit per component, `/JBIG2Decode`, no `/Decode`, no `/DecodeParms`).
///
/// # Errors
/// See [`encode_generic`].
pub fn encode_image(b: &Bitmap, limits: &Limits, cancel: &AtomicBool) -> Result<EncodedImage> {
    let data = encode_generic(b, limits, cancel)?;
    Ok(EncodedImage {
        width: b.width as i64,
        height: b.height as i64,
        bpc: 1,
        color_space: ColorSpace {
            name: "DeviceGray".into(),
            ..Default::default()
        },
        filter: "JBIG2Decode".into(),
        data,
        ..Default::default()
    })
}

/// `DecodeJBIG2Generic`: decodes a JBIG2 bitstream in the embedded file organization and
/// returns the page bitmap. A set bit is ink.
///
/// It inverts [`encode_generic`] up to a size: it decodes any page of at most 67,108,864 pixels
/// whose bitmaps pack into 16 MiB, and no stream of more than 65,536 segments. It also decodes
/// arithmetically coded symbol dictionaries with the immediate text regions that place them.
/// Every other coding mode is [`Error::UnsupportedJbig2Feature`] and no bitmap. Page-0 (global)
/// segments are not visible here; use [`decode_embedded_stream_with_globals`].
///
/// # Errors
/// See [`decode_embedded_stream_with_globals`].
pub fn decode_generic(data: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Bitmap> {
    decode_embedded_stream(data, limits, cancel)
}

/// Reads the big-endian `u32` at `o`, or 0 when the slice is too short. Callers have checked
/// the length; the fallback only keeps a logic slip from panicking.
pub(crate) fn be32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o + 4) {
        Some(&[w, x, y, z]) => u32::from_be_bytes([w, x, y, z]),
        _ => 0,
    }
}

/// Reads the big-endian `u16` at `o`; see [`be32`].
pub(crate) fn be16(b: &[u8], o: usize) -> u16 {
    match b.get(o..o + 2) {
        Some(&[x, y]) => u16::from_be_bytes([x, y]),
        _ => 0,
    }
}

/// Go's `fmt.Errorf("jbig2: segment %d: %w", ...)`: the same error class with the segment named.
/// Budget and limit errors pass through unchanged.
pub(crate) fn with_segment(num: u32, e: Error) -> Error {
    match e {
        Error::Malformed(s) => Error::Malformed(format!("jbig2: segment {num}: {s}")),
        Error::UnsupportedJbig2Feature(s) => {
            Error::UnsupportedJbig2Feature(format!("segment {num}: {s}"))
        }
        other => other,
    }
}
