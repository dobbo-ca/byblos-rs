//! The generic region encoder: GBTEMPLATE 0, nominal AT pixels, optional TPGDON
//! (`internal/jbig2/generic.go`).

use super::bitmap::{mask_padding, row_equal_above};
use super::mq::Encoder;
use crate::bitmap::Bitmap;
use crate::error::Result;
use crate::limits::{Limits, check_cancel};
use std::sync::atomic::AtomicBool;

/// The fixed context used to code the SLTP bit under TPGDON with GBTEMPLATE 0 (T.88 6.2.5.7,
/// Figure 8). It always uses the nominal template regardless of where the AT pixels sit.
pub const SLTP_CONTEXT_TEMPLATE0: usize = 0x9B25;

/// `contextTemplate0`: the GBTEMPLATE 0 context for the pixel at (x, y) with the nominal AT
/// pixels of T.88 Table 5 (A1 = (3, -1), A2 = (-3, -1), A3 = (2, -2), A4 = (-2, -2)), which
/// collapses the 16-pixel template into three contiguous runs:
///
/// * row y-2: x-2 ..= x+2 (5 pixels, context bits 15..11)
/// * row y-1: x-3 ..= x+3 (7 pixels, context bits 10..4)
/// * row y: x-4 ..= x-1 (4 pixels, context bits 3..0)
///
/// Bits are gathered in reading order with MSB = top-left. Out-of-bounds pixels read as 0, as
/// T.88 6.2.5.2 requires.
pub fn context_template0(b: &Bitmap, x: i64, y: i64) -> usize {
    let mut cx = 0usize;
    for dx in -2..=2 {
        cx = (cx << 1) | usize::from(b.get(x + dx, y - 2));
    }
    for dx in -3..=3 {
        cx = (cx << 1) | usize::from(b.get(x + dx, y - 1));
    }
    for dx in -4..=-1 {
        cx = (cx << 1) | usize::from(b.get(x + dx, y));
    }
    cx
}

/// `EncodeGenericRegion`: codes `b` as an arithmetic generic region using GBTEMPLATE 0 with
/// nominal AT pixels, and returns the MQ-coded region data.
///
/// The coding is lossless: a conforming decoder reconstructs `b` exactly. When `tpgdon` is
/// true, each row is preceded by an SLTP bit coded in the fixed context
/// [`SLTP_CONTEXT_TEMPLATE0`]; a row identical to the one above is then not coded at all
/// (T.88 6.2.5.5, 6.2.5.7 step 3b).
///
/// The context array is allocated fresh on every call, which is what T.88 7.4.6.4 step 2
/// requires.
///
/// `b` IS WRITTEN TO. The first thing this does is mask `b`'s padding, the bits past `width` in
/// each row's last byte plus any whole trailing bytes of a non-minimal stride. No pixel is
/// lost, but the masking is load-bearing: [`row_equal_above`] compares whole stride bytes, so
/// unmasked padding would cost TPGD its match.
///
/// The coded bytes are capped at `limits.max_alloc`; the cancel flag is checked once per row.
///
/// # Errors
/// [`Error::Alloc`](crate::Error::Alloc), [`Error::OutOfMemory`](crate::Error::OutOfMemory) or
/// [`Error::Cancelled`](crate::Error::Cancelled).
pub fn encode_generic_region(
    b: &mut Bitmap,
    tpgdon: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    mask_padding(b);

    let mut cx = vec![0u8; 1 << 16];
    let mut e = Encoder::new(limits.alloc_limit())?;

    let mut ltp = 0u8;
    for y in 0..b.height {
        check_cancel(cancel)?;
        if tpgdon {
            let next = u8::from(row_equal_above(b, y));
            e.encode(&mut cx, SLTP_CONTEXT_TEMPLATE0, next ^ ltp)?;
            ltp = next;
            if ltp == 1 {
                continue;
            }
        }

        let yi = y as i64;
        // Seed the three template runs at x = 0. Under nominal AT each run is contiguous, so
        // from here on each advances by one pixel per step.
        let (mut run_above2, mut run_above1, mut run_left) = (0usize, 0usize, 0usize);
        for dx in -2..=2 {
            run_above2 = (run_above2 << 1) | usize::from(b.get(dx, yi - 2));
        }
        for dx in -3..=3 {
            run_above1 = (run_above1 << 1) | usize::from(b.get(dx, yi - 1));
        }
        for dx in -4..=-1 {
            run_left = (run_left << 1) | usize::from(b.get(dx, yi));
        }

        for x in 0..b.width as i64 {
            let pix = b.get(x, yi);
            e.encode(
                &mut cx,
                (run_above2 << 11) | (run_above1 << 4) | run_left,
                pix,
            )?;
            run_above2 = ((run_above2 << 1) | usize::from(b.get(x + 3, yi - 2))) & 0x1F;
            run_above1 = ((run_above1 << 1) | usize::from(b.get(x + 4, yi - 1))) & 0x7F;
            run_left = ((run_left << 1) | usize::from(pix)) & 0x0F;
        }
    }
    e.flush()
}
