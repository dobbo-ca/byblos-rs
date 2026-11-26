//! Port of golang.org/x/image/math/fixed (`fixed.go`): the 26.6 fixed point number and point
//! that `sfnt` hands glyph coordinates back in. Only what the sfnt port and `render::text` use.
//!
//! Go's `Int26_6` is an `int32` that wraps on overflow; the arithmetic here is explicit about
//! that (`wrapping_*`), and the field is public so callers can do the same.

/// A signed 26.6 fixed point number: 26 integer bits and 6 fractional bits, held in an `i32`.
/// With `ppem` equal to the font's units per em, a coordinate is a plain font-unit count.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Int26_6(pub i32);

impl Int26_6 {
    /// Go's `fixed.I`: the integer `i` as a 26.6 number (`i << 6`, wrapping like Go's
    /// conversion).
    pub fn from_int(i: i32) -> Int26_6 {
        Int26_6(i.wrapping_shl(6))
    }

    /// Go's `Floor`.
    pub fn floor(self) -> i32 {
        self.0 >> 6
    }

    /// Go's `Round`.
    pub fn round(self) -> i32 {
        self.0.wrapping_add(0x20) >> 6
    }

    /// Go's `Ceil`.
    pub fn ceil(self) -> i32 {
        self.0.wrapping_add(0x3f) >> 6
    }

    /// Go's `x + y` on `Int26_6` (wraps).
    pub fn wrapping_add(self, y: Int26_6) -> Int26_6 {
        Int26_6(self.0.wrapping_add(y.0))
    }

    /// Go `Mul` (named `mul_round`, not `std::ops::Mul`): the product rounded to nearest, in 64 bits.
    pub fn mul_round(self, y: Int26_6) -> Int26_6 {
        Int26_6(((i64::from(self.0) * i64::from(y.0) + (1 << 5)) >> 6) as i32)
    }
}

/// A point with 26.6 fixed point coordinates.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point26_6 {
    pub x: Int26_6,
    pub y: Int26_6,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(Int26_6::from_int(3), Int26_6(192));
        assert_eq!(Int26_6(-1).floor(), -1);
        assert_eq!(Int26_6(0x1f).round(), 0);
        assert_eq!(Int26_6(0x20).round(), 1);
        assert_eq!(Int26_6(1).ceil(), 1);
        assert_eq!(Int26_6(64).mul_round(Int26_6(96)), Int26_6(96));
        assert_eq!(
            Int26_6(i32::MAX).wrapping_add(Int26_6(1)),
            Int26_6(i32::MIN)
        );
    }
}
