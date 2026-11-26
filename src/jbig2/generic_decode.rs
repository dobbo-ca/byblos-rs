//! The generic region decoder for GBTEMPLATE 0 with nominal AT pixels, the exact inverse of
//! [`encode_generic_region`](super::generic::encode_generic_region)
//! (`internal/jbig2/generic_decode.go`).

use super::bitmap::new_bitmap;
use super::generic::{SLTP_CONTEXT_TEMPLATE0, context_template0};
use super::mq_decode::Decoder;
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::sync::atomic::AtomicBool;

#[cfg(test)]
thread_local! {
    static DECODED_PIXELS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// Go's `decodedPixels` counter, the running total of MQ decisions and symbol placements, for
/// the tests that bound decode cost. It is per thread, so parallel tests do not see each
/// other's work; take a delta around the call under test.
#[cfg(test)]
pub fn decoded_pixels() -> i64 {
    DECODED_PIXELS.with(std::cell::Cell::get)
}

/// Adds `n` to the decode counter. Compiled out of non-test builds.
#[cfg(test)]
pub fn add_decoded_pixels(n: i64) {
    DECODED_PIXELS.with(|c| c.set(c.get() + n));
}

/// Adds `n` to the decode counter. Compiled out of non-test builds.
#[cfg(not(test))]
pub fn add_decoded_pixels(_n: i64) {}

/// `decodeGenericRegion`: the generic region decoding procedure of T.88 6.2.5.7 for
/// GBTEMPLATE 0 with nominal AT pixels. It owns its decoder and its contexts, because T.88
/// 7.4.6.4 resets the statistics at the start of every generic region segment.
///
/// Under TPGDON each row is preceded by an SLTP bit; when the running LTP flag is 1 the row is
/// not coded at all and repeats the row above. Row 0 repeats the implicit all-zero row above
/// the bitmap, which a fresh bitmap already provides.
///
/// There is no way for this to detect a stream it has mis-parsed: the MQ decoder yields a
/// decision for any input. The caller has to reject everything it cannot code for before
/// reaching here, see `decode_generic_region_segment`.
///
/// # Errors
/// [`Error::Malformed`] for a zero dimension, [`Error::Alloc`], [`Error::OutOfMemory`] or
/// [`Error::Cancelled`].
pub fn decode_generic_region(
    data: &[u8],
    w: usize,
    h: usize,
    tpgdon: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Bitmap> {
    if w == 0 || h == 0 {
        return Err(Error::Malformed(format!(
            "jbig2: generic region is {w}x{h}; dimensions must be positive"
        )));
    }
    let mut b = new_bitmap(w, h, limits)?;
    let mut cx = vec![0u8; 1 << 16];
    let mut d = Decoder::new(data);

    let mut work: i64 = 0;
    let r = decode_rows(&mut b, &mut cx, &mut d, tpgdon, cancel, &mut work);
    add_decoded_pixels(work);
    r?;
    Ok(b)
}

fn decode_rows(
    b: &mut Bitmap,
    cx: &mut [u8],
    d: &mut Decoder,
    tpgdon: bool,
    cancel: &AtomicBool,
    work: &mut i64,
) -> Result<()> {
    let stride = b.stride;
    let mut ltp = 0u8;
    for y in 0..b.height {
        check_cancel(cancel)?;
        if tpgdon {
            *work += 1;
            ltp ^= d.decode(cx, SLTP_CONTEXT_TEMPLATE0);
            if ltp == 1 {
                if y > 0 {
                    b.pix.copy_within((y - 1) * stride..y * stride, y * stride);
                }
                continue;
            }
        }
        *work += b.width as i64;
        let yi = y as i64;
        for x in 0..b.width as i64 {
            if d.decode(cx, context_template0(b, x, yi)) != 0 {
                b.set(x, yi, 1);
            }
        }
    }
    Ok(())
}
