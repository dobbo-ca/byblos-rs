//! The colours of Go's `image/color` that byblos reaches, with the exact integer arithmetic of
//! their `RGBA()` methods: 16-bit channels, alpha-premultiplied, in `0..=0xffff`.
//!
//! Go BSD licence: see NOTICE.

/// One of Go's concrete `color.Color` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// `color.Gray`.
    Gray(u8),
    /// `color.Gray16`.
    Gray16(u16),
    /// `color.RGBA`: alpha-premultiplied.
    Rgba { r: u8, g: u8, b: u8, a: u8 },
    /// `color.RGBA64`: alpha-premultiplied.
    Rgba64 { r: u16, g: u16, b: u16, a: u16 },
    /// `color.NRGBA`: non-premultiplied.
    Nrgba { r: u8, g: u8, b: u8, a: u8 },
    /// `color.NRGBA64`: non-premultiplied.
    Nrgba64 { r: u16, g: u16, b: u16, a: u16 },
    /// `color.CMYK`.
    Cmyk { c: u8, m: u8, y: u8, k: u8 },
    /// `color.YCbCr`.
    YCbCr { y: u8, cb: u8, cr: u8 },
}

/// `yy1 + k * c` followed by the clamp that `ycbcr.go` writes for the 16-bit result:
/// `r >>= 8` when the value is in range, else `^(r >> 31) & 0xffff`.
fn clamp16(v: i32) -> u32 {
    if (v as u32) & 0xff00_0000 == 0 {
        (v >> 8) as u32
    } else {
        (!(v >> 31) & 0xffff) as u32
    }
}

/// `color.YCbCr.RGBA`.
fn ycbcr_rgba(y: u8, cb: u8, cr: u8) -> (u32, u32, u32, u32) {
    // The fixed-point constants are those of ycbcr.go.
    let yy1 = i32::from(y) * 0x10101;
    let cb1 = i32::from(cb) - 128;
    let cr1 = i32::from(cr) - 128;
    let r = clamp16(yy1 + 91881 * cr1);
    let g = clamp16(yy1 - 22554 * cb1 - 46802 * cr1);
    let b = clamp16(yy1 + 116130 * cb1);
    (r, g, b, 0xffff)
}

impl Color {
    /// `Color.RGBA()`: the alpha-premultiplied 16-bit channels `(r, g, b, a)`.
    pub fn rgba(self) -> (u32, u32, u32, u32) {
        match self {
            Color::Gray(y) => {
                let y = u32::from(y) * 0x101;
                (y, y, y, 0xffff)
            }
            Color::Gray16(y) => {
                let y = u32::from(y);
                (y, y, y, 0xffff)
            }
            Color::Rgba { r, g, b, a } => (
                u32::from(r) * 0x101,
                u32::from(g) * 0x101,
                u32::from(b) * 0x101,
                u32::from(a) * 0x101,
            ),
            Color::Rgba64 { r, g, b, a } => {
                (u32::from(r), u32::from(g), u32::from(b), u32::from(a))
            }
            Color::Nrgba { r, g, b, a } => {
                let a32 = u32::from(a);
                let pm = |c: u8| u32::from(c) * 0x101 * a32 / 0xff;
                (pm(r), pm(g), pm(b), a32 * 0x101)
            }
            Color::Nrgba64 { r, g, b, a } => {
                let a32 = u32::from(a);
                let pm = |c: u16| u32::from(c) * a32 / 0xffff;
                (pm(r), pm(g), pm(b), a32)
            }
            Color::Cmyk { c, m, y, k } => {
                let w = 0xffff - u32::from(k) * 0x101;
                (
                    (0xffff - u32::from(c) * 0x101) * w / 0xffff,
                    (0xffff - u32::from(m) * 0x101) * w / 0xffff,
                    (0xffff - u32::from(y) * 0x101) * w / 0xffff,
                    0xffff,
                )
            }
            Color::YCbCr { y, cb, cr } => ycbcr_rgba(y, cb, cr),
        }
    }
}

/// `color.GrayModel.Convert` of a colour with 16-bit channels `r`, `g`, `b` (alpha is ignored,
/// as in Go): the JFIF luma weights 19595, 38470 and 7471 (they sum to 65536), rounded.
pub fn gray_from_rgba(r: u32, g: u32, b: u32) -> u8 {
    ((19595 * r + 38470 * g + 7471 * b + (1 << 15)) >> 24) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_16_bit_channels() {
        assert_eq!(Color::Gray(0xab).rgba(), (0xabab, 0xabab, 0xabab, 0xffff));
        assert_eq!(
            Color::Gray16(0x1234).rgba(),
            (0x1234, 0x1234, 0x1234, 0xffff)
        );
        assert_eq!(
            Color::Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 4
            }
            .rgba(),
            (0x101, 0x202, 0x303, 0x404)
        );
        assert_eq!(
            Color::Rgba64 {
                r: 1,
                g: 2,
                b: 3,
                a: 4
            }
            .rgba(),
            (1, 2, 3, 4)
        );
        // NRGBA: r = (c * 0x101) * a / 0xff.
        assert_eq!(
            Color::Nrgba {
                r: 0xff,
                g: 0x80,
                b: 0,
                a: 0x80
            }
            .rgba(),
            (0xffff * 0x80 / 0xff, 0x8080 * 0x80 / 0xff, 0, 0x8080)
        );
        assert_eq!(
            Color::Nrgba {
                r: 9,
                g: 9,
                b: 9,
                a: 0
            }
            .rgba(),
            (0, 0, 0, 0)
        );
        assert_eq!(
            Color::Nrgba64 {
                r: 0xffff,
                g: 0x8000,
                b: 1,
                a: 0x8000
            }
            .rgba(),
            (0x8000, 0x8000 * 0x8000 / 0xffff, 0, 0x8000)
        );
    }

    #[test]
    fn cmyk() {
        assert_eq!(
            Color::Cmyk {
                c: 0,
                m: 0,
                y: 0,
                k: 0
            }
            .rgba(),
            (0xffff, 0xffff, 0xffff, 0xffff)
        );
        assert_eq!(
            Color::Cmyk {
                c: 0,
                m: 0,
                y: 0,
                k: 0xff
            }
            .rgba(),
            (0, 0, 0, 0xffff)
        );
        assert_eq!(
            Color::Cmyk {
                c: 0xff,
                m: 0,
                y: 0,
                k: 0
            }
            .rgba(),
            (0, 0xffff, 0xffff, 0xffff)
        );
        // w = 0xffff - 0x80*0x101 = 0x7f7f; r = (0xffff - 0x4040) * 0x7f7f / 0xffff.
        let (r, _, _, _) = Color::Cmyk {
            c: 0x40,
            m: 0,
            y: 0,
            k: 0x80,
        }
        .rgba();
        assert_eq!(r, (0xffff - 0x4040) * 0x7f7f / 0xffff);
    }

    // Values of color.YCbCr.RGBA worked out from ycbcr.go: grey (cb = cr = 128) is exact, and
    // the 16-bit result is the 24-bit one shifted.
    #[test]
    fn ycbcr() {
        assert_eq!(
            Color::YCbCr {
                y: 0,
                cb: 128,
                cr: 128
            }
            .rgba(),
            (0, 0, 0, 0xffff)
        );
        assert_eq!(
            Color::YCbCr {
                y: 255,
                cb: 128,
                cr: 128
            }
            .rgba(),
            (0xffff, 0xffff, 0xffff, 0xffff)
        );
        let (r, g, b, a) = Color::YCbCr {
            y: 128,
            cb: 128,
            cr: 128,
        }
        .rgba();
        assert_eq!((r, g, b, a), (0x8080, 0x8080, 0x8080, 0xffff));
        // Clamped high and low.
        let (r, _, _, _) = Color::YCbCr {
            y: 255,
            cb: 128,
            cr: 255,
        }
        .rgba();
        assert_eq!(r, 0xffff);
        let (r, _, _, _) = Color::YCbCr {
            y: 0,
            cb: 128,
            cr: 0,
        }
        .rgba();
        assert_eq!(r, 0);
        // The zero colour that Go's YCbCr.At returns outside the bounds is not black: with
        // cb = cr = 0, g = (22554 + 46802) * 128 >> 8 = 34678; r and b clamp to 0.
        assert_eq!(
            Color::YCbCr { y: 0, cb: 0, cr: 0 }.rgba(),
            (0, 34678, 0, 0xffff)
        );
    }

    #[test]
    fn gray_model() {
        assert_eq!(gray_from_rgba(0, 0, 0), 0);
        assert_eq!(gray_from_rgba(0xffff, 0xffff, 0xffff), 255);
        assert_eq!(gray_from_rgba(0x8000, 0x8000, 0x8000), 128);
        // 0.299 * 255 = 76.2 for pure red.
        assert_eq!(gray_from_rgba(0xffff, 0, 0), 76);
        // 0.587 * 255 = 149.7 for pure green.
        assert_eq!(gray_from_rgba(0, 0xffff, 0), 150);
        // 0.114 * 255 = 29.07 for pure blue.
        assert_eq!(gray_from_rgba(0, 0, 0xffff), 29);
    }
}
