//! Go's concrete image types, as one closed [`Image`] enum.
//!
//! Go's `image.Image` is an interface and byblos switches on the concrete type: the sampler
//! fast paths, `x/image/draw`'s scaler arms and the PNG reader's result type all depend on it.
//! Each variant here keeps Go's `Pix`, `Stride` and `Rect` fields with Go's layout: pixel
//! (x, y) of a one-byte-per-pixel type starts at `pix[(y - rect.y0) * stride + (x - rect.x0)]`,
//! and 16-bit channels are big-endian.
//!
//! `At` is [`Image::at`], which returns the zero colour of the type outside `rect` (as Go's
//! `GrayAt` and friends do), and [`Color::rgba`] is `Color.RGBA()`.
//!
//! The codecs live in submodules, added by later waves: `png_read`, `png_write`,
//! `jpeg_write`, `draw` and `tiff`. [`decode()`] (Go's `image.Decode`) and [`decode_config()`] are in `decode`.
//!
//! Go BSD licence: see NOTICE.

pub mod color;
pub mod decode;
pub mod draw;
pub mod jpeg_write;
pub mod png_read;
pub mod png_write;
pub mod tiff;

pub use color::Color;
pub use decode::{ColorModel, Config, decode, decode_config};

use crate::error::{Error, Result};
use crate::limits::Limits;

/// `image.Rectangle`: `(x0, y0)` inclusive and `(x1, y1)` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

impl Rect {
    /// `image.Rect`: the corners are put in order, as Go does.
    pub fn new(x0: i64, y0: i64, x1: i64, y1: i64) -> Rect {
        Rect {
            x0: x0.min(x1),
            y0: y0.min(y1),
            x1: x0.max(x1),
            y1: y0.max(y1),
        }
    }

    /// `Dx`.
    pub fn dx(&self) -> i64 {
        self.x1.saturating_sub(self.x0)
    }

    /// `Dy`.
    pub fn dy(&self) -> i64 {
        self.y1.saturating_sub(self.y0)
    }

    /// `Empty`.
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    /// `Point{x, y}.In(r)`.
    pub fn contains(&self, x: i64, y: i64) -> bool {
        (self.x0..self.x1).contains(&x) && (self.y0..self.y1).contains(&y)
    }
}

/// Go's `PixOffset` for a type with `bpp` bytes per pixel, or `None` outside `rect`.
fn pix_offset(rect: &Rect, stride: usize, bpp: usize, x: i64, y: i64) -> Option<usize> {
    if !rect.contains(x, y) {
        return None;
    }
    let row = usize::try_from(y.checked_sub(rect.y0)?).ok()?;
    let col = usize::try_from(x.checked_sub(rect.x0)?).ok()?;
    row.checked_mul(stride)?.checked_add(col.checked_mul(bpp)?)
}

/// `pixelBufferLength`: the stride and byte length of a `bpp` image of `rect`, checked against
/// `limits.max_alloc` and allocated zeroed.
fn alloc_pix(rect: &Rect, bpp: usize, limits: &Limits) -> Result<(Vec<u8>, usize)> {
    let over = Error::Alloc {
        got: u64::MAX,
        max: limits.alloc_limit(),
    };
    let dx = usize::try_from(rect.dx()).map_err(|_| over.clone())?;
    let dy = usize::try_from(rect.dy()).map_err(|_| over.clone())?;
    let stride = dx.checked_mul(bpp).ok_or_else(|| over.clone())?;
    let len = stride.checked_mul(dy).ok_or(over)?;
    Ok((limits.alloc_zeroed(len)?, stride))
}

// An image of `$bpp` bytes per pixel with Go's `Pix`, `Stride`, `Rect` fields.
macro_rules! pix_image {
    ($(#[$meta:meta])* $name:ident, $bpp:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            pub pix: Vec<u8>,
            pub stride: usize,
            pub rect: Rect,
        }

        impl $name {
            /// Bytes per pixel.
            pub const BYTES_PER_PIXEL: usize = $bpp;

            /// An all-zero image of `rect` (Go's `New*`), its buffer checked against
            /// `limits.max_alloc` before it is allocated.
            ///
            /// # Errors
            /// [`Error::Alloc`] if the size overflows or is over `max_alloc`,
            /// [`Error::OutOfMemory`] if the allocator refuses.
            pub fn new(rect: Rect, limits: &Limits) -> Result<Self> {
                let (pix, stride) = alloc_pix(&rect, $bpp, limits)?;
                Ok(Self { pix, stride, rect })
            }

            /// `PixOffset`: the index of pixel (x, y) in `pix`, or `None` outside `rect`.
            pub fn pix_offset(&self, x: i64, y: i64) -> Option<usize> {
                pix_offset(&self.rect, self.stride, $bpp, x, y)
            }

            /// The bytes of pixel (x, y), or `None` outside `rect`.
            pub fn pixel(&self, x: i64, y: i64) -> Option<&[u8]> {
                let i = self.pix_offset(x, y)?;
                self.pix.get(i..i.checked_add($bpp)?)
            }
        }
    };
}

pix_image!(
    /// `image.Gray`.
    Gray, 1
);
pix_image!(
    /// `image.Gray16`: 16-bit big-endian samples.
    Gray16, 2
);
pix_image!(
    /// `image.RGBA`: alpha-premultiplied, one byte per channel.
    Rgba, 4
);
pix_image!(
    /// `image.RGBA64`: alpha-premultiplied, 16-bit big-endian channels.
    Rgba64, 8
);
pix_image!(
    /// `image.NRGBA`: non-premultiplied, one byte per channel.
    Nrgba, 4
);
pix_image!(
    /// `image.NRGBA64`: non-premultiplied, 16-bit big-endian channels.
    Nrgba64, 8
);
pix_image!(
    /// `image.CMYK`: C, M, Y, K bytes.
    Cmyk, 4
);

/// `image.Paletted`: one palette index per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paletted {
    pub pix: Vec<u8>,
    pub stride: usize,
    pub rect: Rect,
    pub palette: Vec<Color>,
}

impl Paletted {
    /// `NewPaletted`: every index 0, buffer checked as in [`Gray::new`].
    ///
    /// # Errors
    /// As [`Gray::new`].
    pub fn new(rect: Rect, palette: Vec<Color>, limits: &Limits) -> Result<Self> {
        let (pix, stride) = alloc_pix(&rect, 1, limits)?;
        Ok(Self {
            pix,
            stride,
            rect,
            palette,
        })
    }

    /// `PixOffset`.
    pub fn pix_offset(&self, x: i64, y: i64) -> Option<usize> {
        pix_offset(&self.rect, self.stride, 1, x, y)
    }
}

/// `image.YCbCrSubsampleRatio`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsample {
    S444,
    S422,
    S420,
    S440,
    S411,
    S410,
}

/// `image.YCbCr`: planar, with the decoder's own padding of the planes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YCbCr {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub y_stride: usize,
    pub c_stride: usize,
    pub ratio: Subsample,
    pub rect: Rect,
}

impl YCbCr {
    /// `YOffset`, or `None` outside `rect`.
    pub fn y_offset(&self, x: i64, y: i64) -> Option<usize> {
        pix_offset(&self.rect, self.y_stride, 1, x, y)
    }

    /// `COffset`, or `None` outside `rect`. Go's integer division truncates toward zero, as
    /// Rust's does.
    pub fn c_offset(&self, x: i64, y: i64) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        let (rx, ry) = (self.rect.x0, self.rect.y0);
        let (col, row) = match self.ratio {
            Subsample::S422 => (x / 2 - rx / 2, y - ry),
            Subsample::S420 => (x / 2 - rx / 2, y / 2 - ry / 2),
            Subsample::S440 => (x - rx, y / 2 - ry / 2),
            Subsample::S411 => (x / 4 - rx / 4, y - ry),
            Subsample::S410 => (x / 4 - rx / 4, y / 2 - ry / 2),
            Subsample::S444 => (x - rx, y - ry),
        };
        usize::try_from(row)
            .ok()?
            .checked_mul(self.c_stride)?
            .checked_add(usize::try_from(col).ok()?)
    }
}

// The conversions from the image types of jpeg-rs `$j` (also the types of `gojpeg::GoImage`).
macro_rules! from_jpeg {
    ($j:ident) => {
        impl From<$j::Rect> for Rect {
            fn from(r: $j::Rect) -> Rect {
                Rect {
                    x0: i64::from(r.min_x),
                    y0: i64::from(r.min_y),
                    x1: i64::from(r.max_x),
                    y1: i64::from(r.max_y),
                }
            }
        }

        impl From<$j::Subsample> for Subsample {
            fn from(s: $j::Subsample) -> Subsample {
                match s {
                    $j::Subsample::S444 => Subsample::S444,
                    $j::Subsample::S422 => Subsample::S422,
                    $j::Subsample::S420 => Subsample::S420,
                    $j::Subsample::S440 => Subsample::S440,
                    $j::Subsample::S411 => Subsample::S411,
                    $j::Subsample::S410 => Subsample::S410,
                }
            }
        }

        from_jpeg!($j, Gray);
        from_jpeg!($j, Rgba);
        from_jpeg!($j, Cmyk);

        impl From<$j::YCbCr> for YCbCr {
            fn from(i: $j::YCbCr) -> YCbCr {
                YCbCr {
                    y: i.y,
                    cb: i.cb,
                    cr: i.cr,
                    y_stride: i.y_stride,
                    c_stride: i.c_stride,
                    ratio: i.ratio.into(),
                    rect: i.rect.into(),
                }
            }
        }
    };
    ($j:ident, $t:ident) => {
        impl From<$j::$t> for $t {
            fn from(i: $j::$t) -> $t {
                $t {
                    pix: i.pix,
                    stride: i.stride,
                    rect: i.rect.into(),
                }
            }
        }
    };
}

from_jpeg!(jpeg);

/// The concrete `image.Image` types byblos produces or consumes. `Gray` and `Rgba` come from
/// jpeg-rs, `YCbCr`, `Cmyk` and the rest from Go's `image/jpeg` and `image/png` ports and the
/// TIFF reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    Gray(Gray),
    Gray16(Gray16),
    Rgba(Rgba),
    Rgba64(Rgba64),
    Nrgba(Nrgba),
    Nrgba64(Nrgba64),
    Cmyk(Cmyk),
    Paletted(Paletted),
    YCbCr(YCbCr),
}

impl Image {
    /// `Bounds`.
    pub fn bounds(&self) -> Rect {
        match self {
            Image::Gray(i) => i.rect,
            Image::Gray16(i) => i.rect,
            Image::Rgba(i) => i.rect,
            Image::Rgba64(i) => i.rect,
            Image::Nrgba(i) => i.rect,
            Image::Nrgba64(i) => i.rect,
            Image::Cmyk(i) => i.rect,
            Image::Paletted(i) => i.rect,
            Image::YCbCr(i) => i.rect,
        }
    }

    /// `At`: the colour of pixel (x, y). Outside `rect` it is the zero colour of the type, as
    /// Go returns (a `Paletted` returns its first entry). A `Paletted` with an empty palette, or
    /// an index past the palette (Go returns nil or panics), reads as transparent black.
    pub fn at(&self, x: i64, y: i64) -> Color {
        let zero = Color::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
        let be16 = |p: &[u8], i: usize| u16::from_be_bytes([p[i], p[i + 1]]);
        match self {
            Image::Gray(i) => Color::Gray(i.pixel(x, y).map_or(0, |p| p[0])),
            Image::Gray16(i) => Color::Gray16(i.pixel(x, y).map_or(0, |p| be16(p, 0))),
            Image::Rgba(i) => i.pixel(x, y).map_or(zero, |p| Color::Rgba {
                r: p[0],
                g: p[1],
                b: p[2],
                a: p[3],
            }),
            Image::Rgba64(i) => i.pixel(x, y).map_or(
                Color::Rgba64 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                |p| Color::Rgba64 {
                    r: be16(p, 0),
                    g: be16(p, 2),
                    b: be16(p, 4),
                    a: be16(p, 6),
                },
            ),
            Image::Nrgba(i) => i.pixel(x, y).map_or(
                Color::Nrgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                |p| Color::Nrgba {
                    r: p[0],
                    g: p[1],
                    b: p[2],
                    a: p[3],
                },
            ),
            Image::Nrgba64(i) => i.pixel(x, y).map_or(
                Color::Nrgba64 {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                |p| Color::Nrgba64 {
                    r: be16(p, 0),
                    g: be16(p, 2),
                    b: be16(p, 4),
                    a: be16(p, 6),
                },
            ),
            Image::Cmyk(i) => i.pixel(x, y).map_or(
                Color::Cmyk {
                    c: 0,
                    m: 0,
                    y: 0,
                    k: 0,
                },
                |p| Color::Cmyk {
                    c: p[0],
                    m: p[1],
                    y: p[2],
                    k: p[3],
                },
            ),
            Image::Paletted(i) => {
                let idx = i
                    .pix_offset(x, y)
                    .and_then(|o| i.pix.get(o))
                    .map_or(0, |&v| usize::from(v));
                i.palette.get(idx).copied().unwrap_or(zero)
            }
            Image::YCbCr(i) => {
                let c = i
                    .y_offset(x, y)
                    .zip(i.c_offset(x, y))
                    .and_then(|(yo, co)| Some((*i.y.get(yo)?, *i.cb.get(co)?, *i.cr.get(co)?)));
                let (y, cb, cr) = c.unwrap_or((0, 0, 0));
                Color::YCbCr { y, cb, cr }
            }
        }
    }

    /// `img.At(x, y).RGBA()`.
    pub fn rgba_at(&self, x: i64, y: i64) -> (u32, u32, u32, u32) {
        self.at(x, y).rgba()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    #[test]
    fn rect_is_canonical() {
        let r = Rect::new(5, 7, 1, 2);
        assert_eq!(
            r,
            Rect {
                x0: 1,
                y0: 2,
                x1: 5,
                y1: 7
            }
        );
        assert_eq!((r.dx(), r.dy()), (4, 5));
        assert!(!r.is_empty());
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert!(r.contains(1, 2) && r.contains(4, 6));
        assert!(!r.contains(5, 6) && !r.contains(4, 7) && !r.contains(0, 2));
    }

    #[test]
    fn gray_at_and_zero_outside() {
        let mut g = Gray::new(Rect::new(2, 3, 5, 5), &limits()).unwrap();
        assert_eq!((g.stride, g.pix.len()), (3, 6));
        let o = g.pix_offset(3, 4).unwrap();
        assert_eq!(o, 4);
        g.pix[o] = 0x40;
        let img = Image::Gray(g);
        assert_eq!(img.at(3, 4), Color::Gray(0x40));
        assert_eq!(img.rgba_at(3, 4), (0x4040, 0x4040, 0x4040, 0xffff));
        // Outside the rectangle: the zero Gray, which is opaque black.
        assert_eq!(img.at(0, 0), Color::Gray(0));
        assert_eq!(img.rgba_at(5, 3), (0, 0, 0, 0xffff));
        assert_eq!(img.bounds(), Rect::new(2, 3, 5, 5));
    }

    #[test]
    fn sixteen_bit_types_are_big_endian() {
        let mut g = Gray16::new(Rect::new(0, 0, 2, 1), &limits()).unwrap();
        g.pix[2] = 0x12;
        g.pix[3] = 0x34;
        assert_eq!(Image::Gray16(g).at(1, 0), Color::Gray16(0x1234));

        let mut n = Nrgba64::new(Rect::new(0, 0, 1, 1), &limits()).unwrap();
        n.pix.copy_from_slice(&[0, 1, 0, 2, 0, 3, 0x80, 0]);
        assert_eq!(
            Image::Nrgba64(n).at(0, 0),
            Color::Nrgba64 {
                r: 1,
                g: 2,
                b: 3,
                a: 0x8000
            }
        );
    }

    #[test]
    fn rgba_family() {
        let mut r = Rgba::new(Rect::new(0, 0, 2, 2), &limits()).unwrap();
        let o = r.pix_offset(1, 1).unwrap();
        assert_eq!(o, 12);
        r.pix[o..o + 4].copy_from_slice(&[10, 20, 30, 40]);
        let img = Image::Rgba(r);
        assert_eq!(
            img.at(1, 1),
            Color::Rgba {
                r: 10,
                g: 20,
                b: 30,
                a: 40
            }
        );
        assert_eq!(img.rgba_at(1, 1), (2570, 5140, 7710, 10280));
        assert_eq!(img.rgba_at(-1, 0), (0, 0, 0, 0));

        let mut n = Nrgba::new(Rect::new(0, 0, 1, 1), &limits()).unwrap();
        n.pix.copy_from_slice(&[255, 0, 0, 128]);
        // color.NRGBA.RGBA premultiplies: 0xffff * 128 / 0xff.
        assert_eq!(
            Image::Nrgba(n).rgba_at(0, 0),
            (0xffff * 128 / 0xff, 0, 0, 0x8080)
        );

        let mut c = Cmyk::new(Rect::new(0, 0, 1, 1), &limits()).unwrap();
        c.pix.copy_from_slice(&[0, 0, 0, 255]);
        assert_eq!(Image::Cmyk(c).rgba_at(0, 0), (0, 0, 0, 0xffff));
    }

    #[test]
    fn paletted_at() {
        let pal = vec![
            Color::Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 255,
            },
            Color::Rgba {
                r: 9,
                g: 9,
                b: 9,
                a: 255,
            },
        ];
        let mut p = Paletted::new(Rect::new(0, 0, 2, 1), pal.clone(), &limits()).unwrap();
        p.pix[1] = 1;
        p.pix[0] = 7; // past the palette
        let img = Image::Paletted(p);
        assert_eq!(img.at(1, 0), pal[1]);
        assert_eq!(
            img.at(0, 0),
            Color::Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0
            }
        );
        // Outside: the first palette entry.
        assert_eq!(img.at(5, 5), pal[0]);
    }

    #[test]
    fn ycbcr_420_offsets_and_at() {
        let y = YCbCr {
            y: (0..16).collect(),
            cb: vec![128, 128, 128, 128],
            cr: vec![128, 128, 128, 128],
            y_stride: 4,
            c_stride: 2,
            ratio: Subsample::S420,
            rect: Rect::new(0, 0, 4, 4),
        };
        assert_eq!(y.y_offset(3, 2), Some(11));
        assert_eq!(y.c_offset(3, 2), Some(3));
        assert_eq!(y.c_offset(4, 0), None);
        let img = Image::YCbCr(y);
        // y = 5 at (1, 1), neutral chroma: grey of 5 * 0x10101 >> 8.
        assert_eq!(img.rgba_at(1, 1), (0x0505, 0x0505, 0x0505, 0xffff));
    }

    #[test]
    fn size_is_checked_against_max_alloc() {
        let l = Limits {
            max_alloc: 100,
            ..Limits::default()
        };
        assert!(Gray::new(Rect::new(0, 0, 10, 10), &l).is_ok());
        assert!(matches!(
            Rgba::new(Rect::new(0, 0, 10, 10), &l),
            Err(Error::Alloc { got: 400, max: 100 })
        ));
        assert!(matches!(
            Rgba64::new(Rect::new(0, 0, i64::MAX, i64::MAX), &l),
            Err(Error::Alloc { .. })
        ));
    }
}
