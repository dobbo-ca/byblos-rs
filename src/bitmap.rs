//! `Bitmap`: a 1-bit-per-pixel bilevel image (`bitmap.go`).
//!
//! A set bit (1) is BLACK ink. That matches JBIG2, where 1 is black, and is the inverse of PDF
//! `/DeviceGray`, where 1 is white; any conversion across that boundary inverts.
//!
//! The origin is top-left and y increases downward. Rows are packed MSB-first: pixel (x, y) is
//! bit `7 - (x % 8)` of `pix[y * stride + x / 8]`. Bits past `width` in the final byte of a row
//! are always zero, and [`Bitmap::set`] preserves that, so `==` compares `pix` directly.

use crate::error::{Error, Result};
use crate::image::Rect;
use crate::limits::Limits;

/// A bilevel image owned by byblos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    /// Bytes per row.
    pub stride: usize,
    pub pix: Vec<u8>,
}

impl Bitmap {
    /// `NewBitmap`: a `width` x `height` bitmap with every pixel clear. The buffer is checked
    /// against `limits.max_alloc` before it is allocated.
    ///
    /// # Errors
    /// [`Error::Alloc`] if the buffer size overflows or is over `max_alloc`,
    /// [`Error::OutOfMemory`] if the allocator refuses.
    pub fn new(width: usize, height: usize, limits: &Limits) -> Result<Bitmap> {
        let over = Error::Alloc {
            got: u64::MAX,
            max: limits.alloc_limit(),
        };
        let stride = width.checked_add(7).ok_or_else(|| over.clone())? / 8;
        let len = stride.checked_mul(height).ok_or(over)?;
        Ok(Bitmap {
            width,
            height,
            stride,
            pix: limits.alloc_zeroed(len)?,
        })
    }

    /// `At`: 1 or 0. Pixels outside the bitmap read as 0, as ITU-T T.88 section 6.2.5.2
    /// requires for JBIG2 template gathering.
    pub fn get(&self, x: i64, y: i64) -> u8 {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return 0;
        };
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.pix
            .get(y * self.stride + x / 8)
            .map_or(0, |b| (b >> (7 - x % 8)) & 1)
    }

    /// `Set`: writes 1 when `v` is non-zero and 0 otherwise. Coordinates outside the bitmap are
    /// ignored.
    pub fn set(&mut self, x: i64, y: i64, v: u8) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x >= self.width || y >= self.height {
            return;
        }
        let mask = 1u8 << (7 - x % 8);
        if let Some(b) = self.pix.get_mut(y * self.stride + x / 8) {
            if v != 0 {
                *b |= mask;
            } else {
                *b &= !mask;
            }
        }
    }

    /// `Bounds`: the extent with the origin at (0, 0).
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as i64, self.height as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_bounds() {
        let l = Limits::default();
        let b = Bitmap::new(10, 3, &l).unwrap();
        assert_eq!((b.width, b.height, b.stride, b.pix.len()), (10, 3, 2, 6));
        assert_eq!(b.bounds(), Rect::new(0, 0, 10, 3));
        let e = Bitmap::new(0, 5, &l).unwrap();
        assert_eq!((e.stride, e.pix.len()), (0, 0));
    }

    #[test]
    fn set_get_msb_first_and_out_of_range() {
        let l = Limits::default();
        let mut b = Bitmap::new(10, 2, &l).unwrap();
        b.set(0, 0, 1);
        b.set(9, 1, 7);
        assert_eq!(b.pix, vec![0x80, 0x00, 0x00, 0x40]);
        assert_eq!((b.get(0, 0), b.get(1, 0), b.get(9, 1)), (1, 0, 1));
        // Outside: reads 0, writes ignored.
        assert_eq!(
            (b.get(-1, 0), b.get(0, -1), b.get(10, 0), b.get(0, 2)),
            (0, 0, 0, 0)
        );
        b.set(-1, 0, 1);
        b.set(10, 0, 1);
        b.set(0, 2, 1);
        assert_eq!(b.pix, vec![0x80, 0x00, 0x00, 0x40]);
        b.set(0, 0, 0);
        assert_eq!(b.get(0, 0), 0);
        assert_eq!(b.pix[0], 0);
    }

    #[test]
    fn clone_and_equal() {
        let l = Limits::default();
        let mut a = Bitmap::new(9, 9, &l).unwrap();
        a.set(8, 8, 1);
        let mut c = a.clone();
        assert_eq!(a, c);
        c.set(0, 0, 1);
        assert_ne!(a, c);
        assert_ne!(a, Bitmap::new(9, 8, &l).unwrap());
    }

    #[test]
    fn size_is_checked_against_max_alloc() {
        let l = Limits {
            max_alloc: 100,
            ..Limits::default()
        };
        assert!(Bitmap::new(8, 100, &l).is_ok());
        assert!(matches!(
            Bitmap::new(8, 101, &l),
            Err(Error::Alloc { got: 101, max: 100 })
        ));
        assert!(matches!(
            Bitmap::new(usize::MAX, 1, &l),
            Err(Error::Alloc { .. })
        ));
        assert!(matches!(
            Bitmap::new(usize::MAX / 2, usize::MAX / 2, &l),
            Err(Error::Alloc { .. })
        ));
    }
}
