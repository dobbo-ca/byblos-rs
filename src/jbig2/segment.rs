//! The encoder side of the segment syntax: segment headers, the page information segment, the
//! generic region segment and [`embedded_stream`] (`internal/jbig2/segment.go`).

use super::generic::encode_generic_region;
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::Limits;
use std::sync::atomic::AtomicBool;

/// Immediate lossless generic region (T.88 7.3).
pub const SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION: u8 = 39;
/// Page information (T.88 7.3).
pub const SEG_TYPE_PAGE_INFORMATION: u8 = 48;

/// The AT pixel field for GBTEMPLATE 0 (T.88 7.4.6.3), eight signed bytes carrying the nominal
/// positions of T.88 Table 5: A1 (3,-1), A2 (-3,-1), A3 (2,-2), A4 (-2,-2).
pub static NOMINAL_AT_TEMPLATE0: [u8; 8] = [0x03, 0xFF, 0xFD, 0xFF, 0x02, 0xFE, 0xFE, 0xFE];

/// `segmentHeader`: a T.88 7.2 segment header with a one-byte page association and no
/// referred-to segments, 11 bytes in all.
///
/// The flags byte is the segment type in bits 0-5, bit 6 clear for a one-byte page association,
/// bit 7 clear for "not deferred, non-retain". The referred-to-segment-count byte is 0x00. The
/// unknown-data-length form is deliberately not used. `data_len` is truncated to 32 bits.
pub fn segment_header(seg_num: u32, seg_type: u8, page_assoc: u8, data_len: usize) -> Vec<u8> {
    let mut h = Vec::with_capacity(11);
    h.extend_from_slice(&seg_num.to_be_bytes());
    h.extend_from_slice(&[seg_type, 0x00, page_assoc]);
    h.extend_from_slice(&(data_len as u32).to_be_bytes());
    h
}

/// `pageInfoSegmentData`: the 19-byte page information segment body (T.88 7.4.8).
///
/// Resolutions are written as 0 ("unknown"). The flags byte is 0x01, "page is eventually
/// lossless": no refinements, default pixel value 0, default combination operator OR, no
/// auxiliary buffers. Striping information is 0x0000: not striped.
pub fn page_info_segment_data(width: u32, height: u32) -> Vec<u8> {
    let mut d = Vec::with_capacity(19);
    d.extend_from_slice(&width.to_be_bytes());
    d.extend_from_slice(&height.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&0u32.to_be_bytes());
    d.extend_from_slice(&[0x01, 0x00, 0x00]);
    d
}

/// `genericRegionSegmentData`: the body of a generic region segment: the 17-byte region segment
/// information field (T.88 7.4.1), the one-byte generic region flags (7.4.6.2), the eight-byte
/// AT field (7.4.6.3) and the MQ-coded region data. Template 0 with TPGDON is flags 0x08.
///
/// # Errors
/// As [`encode_generic_region`].
pub fn generic_region_segment_data(
    b: &mut Bitmap,
    x: u32,
    y: u32,
    tpgdon: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let coded = encode_generic_region(b, tpgdon, limits, cancel)?;
    let mut d = Vec::new();
    d.try_reserve_exact(26 + coded.len())?;
    d.extend_from_slice(&(b.width as u32).to_be_bytes());
    d.extend_from_slice(&(b.height as u32).to_be_bytes());
    d.extend_from_slice(&x.to_be_bytes());
    d.extend_from_slice(&y.to_be_bytes());
    d.push(0x00); // region segment flags: external combination operator OR
    // MMR = 0, GBTEMPLATE = 0.
    d.push(if tpgdon { 0x08 } else { 0x00 });
    d.extend_from_slice(&NOMINAL_AT_TEMPLATE0);
    d.extend_from_slice(&coded);
    Ok(d)
}

/// `EmbeddedStream`: encodes `b` as a complete JBIG2 bitstream in the embedded file
/// organization required by ISO 32000-1:2008 7.4.7 for the PDF `JBIG2Decode` filter: no file
/// header, no end-of-page segment, no end-of-file segment, and every segment associated with
/// page 1.
///
/// The result is exactly two segments, a page information segment and an immediate lossless
/// generic region segment covering the whole page, so the image XObject needs no
/// `/DecodeParms` and no `/JBIG2Globals` stream. `b` is written to: its padding is masked (see
/// [`encode_generic_region`]). The coded bytes are capped at `limits.max_alloc`.
///
/// # Errors
/// [`Error::Malformed`] for a zero or over-32-bit dimension, a stride too small for the width,
/// or a `pix` shorter than `stride * height`; otherwise as [`encode_generic_region`].
pub fn embedded_stream(b: &mut Bitmap, limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    if b.width == 0 || b.height == 0 {
        return Err(Error::Malformed(format!(
            "jbig2: bitmap is {}x{}; dimensions must be positive",
            b.width, b.height
        )));
    }
    if b.width as u64 > u64::from(u32::MAX) || b.height as u64 > u64::from(u32::MAX) {
        return Err(Error::Malformed(format!(
            "jbig2: bitmap is {}x{}; JBIG2 region dimensions are 32-bit",
            b.width, b.height
        )));
    }
    // The layout checks of byblos.EncodeJBIG2Generic, which guard every index below.
    if b.stride < b.width.div_ceil(8) {
        return Err(Error::Malformed(format!(
            "jbig2: stride {} is too small for width {}",
            b.stride, b.width
        )));
    }
    let Some(want) = b.stride.checked_mul(b.height) else {
        return Err(Error::Malformed(format!(
            "jbig2: stride {} and height {} overflow",
            b.stride, b.height
        )));
    };
    if b.pix.len() < want {
        return Err(Error::Malformed(format!(
            "jbig2: Pix is {} bytes; want at least {want}",
            b.pix.len()
        )));
    }

    let pi = page_info_segment_data(b.width as u32, b.height as u32);
    let gr = generic_region_segment_data(b, 0, 0, true, limits, cancel)?;

    let mut out = Vec::new();
    out.try_reserve_exact(22 + pi.len() + gr.len())?;
    out.extend_from_slice(&segment_header(0, SEG_TYPE_PAGE_INFORMATION, 1, pi.len()));
    out.extend_from_slice(&pi);
    out.extend_from_slice(&segment_header(
        1,
        SEG_TYPE_IMMEDIATE_LOSSLESS_GENERIC_REGION,
        1,
        gr.len(),
    ));
    out.extend_from_slice(&gr);
    Ok(out)
}
