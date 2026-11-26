//! Bitmap helpers of `internal/jbig2/bitmap.go`.
//!
//! The bitmap type itself is [`crate::Bitmap`]: the same packed MSB-first layout, a set bit is
//! ink. Go's `Get` is [`Bitmap::get`] and `Set` is [`Bitmap::set`]; what is here is what
//! `crate::Bitmap` does not have.

use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::Limits;

/// `NewBitmap`: an all-background bitmap. Go panics on a non-positive dimension; this returns
/// an error instead, because a zero-pixel region is not representable in a JBIG2 region segment.
///
/// # Errors
/// [`Error::Malformed`] on a zero dimension, [`Error::Alloc`] or [`Error::OutOfMemory`] from
/// [`Bitmap::new`].
pub fn new_bitmap(w: usize, h: usize, limits: &Limits) -> Result<Bitmap> {
    if w == 0 || h == 0 {
        return Err(Error::Malformed(format!(
            "jbig2: new_bitmap({w}, {h}): dimensions must be positive"
        )));
    }
    Bitmap::new(w, h, limits)
}

/// `MaskPadding`: zeroes everything in a row past the last visible pixel, the padding bits of
/// the byte that holds pixel `width - 1` and every whole byte between there and `stride`.
///
/// `RowEqualAbove` compares whole strides, so stray bits there make two visually identical rows
/// compare unequal and TPGD stops firing. A bitmap whose `pix` is shorter than `stride * height`
/// is masked as far as it goes.
pub fn mask_padding(b: &mut Bitmap) {
    if b.width == 0 || b.stride == 0 {
        return;
    }
    // The byte holding the last visible pixel.
    let last = (b.width - 1) / 8;
    let rem = b.width % 8;
    let mask = if rem != 0 { 0xFFu8 << (8 - rem) } else { 0xFF };
    for row in b.pix.chunks_mut(b.stride).take(b.height) {
        if let Some(v) = row.get_mut(last) {
            *v &= mask;
        }
        if let Some(tail) = row.get_mut(last + 1..) {
            tail.fill(0);
        }
    }
}

/// `RowEqualAbove`: whether row `y` is identical to row `y - 1`. Row 0 is compared against the
/// implicit all-zero row above the bitmap, matching the out-of-bounds rule. This is the
/// predicate that drives TPGD. A row outside `pix` is not equal to anything.
pub fn row_equal_above(b: &Bitmap, y: usize) -> bool {
    let Some(start) = y.checked_mul(b.stride) else {
        return false;
    };
    let Some(cur) = start
        .checked_add(b.stride)
        .and_then(|e| b.pix.get(start..e))
    else {
        return false;
    };
    if y == 0 {
        return cur.iter().all(|&v| v == 0);
    }
    b.pix
        .get(start - b.stride..start)
        .is_some_and(|prev| prev == cur)
}
