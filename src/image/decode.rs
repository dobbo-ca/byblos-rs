//! Go's `image.Decode` and `image.DecodeConfig` (`image/format.go`) as linked into byblos.
//!
//! The registered formats are JPEG (`image/jpeg`, here pdf-rs `gojpeg`), PNG
//! ([`png_read`](super::png_read)) and TIFF ([`tiff`](super::tiff); `hhrutter/tiff` registers
//! itself through pdf-go). `x/image/webp` is registered too, through pdf-go's `model/image.go`;
//! it is not ported (docs/go-differences.md), so a WebP file gets the error Go gives an unknown
//! format. Go sniffs the formats in registration order, but the magics are disjoint, so the
//! order cannot change the result.
//!
//! Go's `Decode` reads the magic with `Peek`, which fails on an input shorter than the magic:
//! such an input matches no format.

use super::{Color, Image, png_read, tiff};
use crate::error::{Error, Result};
use crate::limits::Limits;
use pdf::filter::gojpeg;
use std::sync::atomic::AtomicBool;

/// `image.Config.ColorModel`: the colour model of the image a decoder would return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorModel {
    /// `color.GrayModel`.
    Gray,
    /// `color.Gray16Model`.
    Gray16,
    /// `color.RGBAModel`.
    Rgba,
    /// `color.RGBA64Model`.
    Rgba64,
    /// `color.NRGBAModel`.
    Nrgba,
    /// `color.NRGBA64Model`.
    Nrgba64,
    /// `color.CMYKModel`.
    Cmyk,
    /// `color.YCbCrModel`.
    YCbCr,
    /// A `color.Palette`.
    Paletted(Vec<Color>),
}

/// `image.Config`: what `DecodeConfig` reads from the headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub model: ColorModel,
    pub width: usize,
    pub height: usize,
}

const JPEG_MAGIC: &[u8] = b"\xff\xd8";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const TIFF_LE_MAGIC: &[u8] = b"II\x2a\x00";
const TIFF_BE_MAGIC: &[u8] = b"MM\x00\x2a";

enum Format {
    Jpeg,
    Png,
    Tiff,
}

/// `image.ErrFormat`.
fn err_format() -> Error {
    Error::Image("image: unknown format".to_string())
}

/// Go's `sniff`.
fn sniff(data: &[u8]) -> Option<Format> {
    if data.starts_with(JPEG_MAGIC) {
        Some(Format::Jpeg)
    } else if data.starts_with(PNG_MAGIC) {
        Some(Format::Png)
    } else if data.starts_with(TIFF_LE_MAGIC) || data.starts_with(TIFF_BE_MAGIC) {
        Some(Format::Tiff)
    } else {
        None
    }
}

/// `image.Decode`: sniffs JPEG, PNG or TIFF and decodes the image, returning the concrete type
/// Go returns. Buffers are charged to `limits.max_alloc` before they are allocated, and `cancel`
/// is checked per row (or per block row for JPEG).
///
/// # Errors
/// `Error::Image("image: unknown format")` for any other input, the decoder's own errors as
/// documented on [`png_read::decode`] and [`tiff::decode`], pdf-rs's `gojpeg` errors as
/// [`Error::Pdf`] (its allocation and cancellation errors as [`Error::Alloc`],
/// [`Error::OutOfMemory`] and [`Error::Cancelled`]).
pub fn decode(data: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Image> {
    match sniff(data).ok_or_else(err_format)? {
        Format::Png => png_read::decode(data, limits, cancel),
        Format::Tiff => tiff::decode(data, limits, cancel),
        Format::Jpeg => {
            let d = gojpeg::decode(data, &limits.mem(), cancel)?;
            Ok(match d.image {
                gojpeg::GoImage::Gray(i) => Image::Gray(i.into()),
                gojpeg::GoImage::YCbCr(i) => Image::YCbCr(i.into()),
                gojpeg::GoImage::Rgba(i) => Image::Rgba(i.into()),
                gojpeg::GoImage::Cmyk(i) => Image::Cmyk(i.into()),
            })
        }
    }
}

/// `image.DecodeConfig`: the colour model and size, from the headers only. It allocates at most
/// a 256-entry palette, which it checks against `limits.max_alloc`.
///
/// # Errors
/// As [`decode`].
pub fn decode_config(data: &[u8], limits: &Limits) -> Result<Config> {
    match sniff(data).ok_or_else(err_format)? {
        Format::Png => png_read::decode_config(data, limits),
        Format::Tiff => tiff::decode_config(data, limits),
        Format::Jpeg => {
            let c = gojpeg::decode_config(data)?;
            Ok(Config {
                model: match c.model {
                    gojpeg::ColorModel::Gray => ColorModel::Gray,
                    gojpeg::ColorModel::YCbCr => ColorModel::YCbCr,
                    gojpeg::ColorModel::Rgba => ColorModel::Rgba,
                    gojpeg::ColorModel::Cmyk => ColorModel::Cmyk,
                },
                width: c.width,
                height: c.height,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(data: &[u8]) -> bool {
        let limits = Limits::default();
        let cancel = AtomicBool::new(false);
        let want = err_format();
        decode(data, &limits, &cancel) == Err(want.clone())
            && decode_config(data, &limits) == Err(want)
    }

    #[test]
    fn unknown_formats() {
        assert!(unknown(b""));
        assert!(unknown(b"GIF89a"));
        // A prefix of a magic is no match: Go's Peek of the whole magic fails.
        assert!(unknown(b"\x89PNG"));
        assert!(unknown(b"\xff"));
        assert!(unknown(b"II*"));
        // WebP is a documented difference: it is sniffed by Go, and unknown here.
        assert!(unknown(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
        assert_eq!(err_format().class(), "error");
        assert_eq!(err_format().to_string(), "image: unknown format");
    }

    #[test]
    fn sniffs_each_format() {
        let limits = Limits::default();
        let cancel = AtomicBool::new(false);
        // A bare PNG signature is PNG: the error is the PNG reader's (a short read).
        let e = decode(PNG_MAGIC, &limits, &cancel).unwrap_err();
        assert_eq!(e.class(), "pdf/unexpected-eof");
        // A bare TIFF header: the offset 0 points into the header, so the IFD is read from there.
        assert!(decode(b"II*\x00\x00\x00\x00\x00", &limits, &cancel).is_err());
        assert!(decode(b"MM\x00*\x00\x00\x00\x08", &limits, &cancel).is_err());
        // A bare SOI: jpeg's own error.
        let e = decode(b"\xff\xd8", &limits, &cancel).unwrap_err();
        assert_ne!(e, err_format());
        assert!(decode_config(b"\xff\xd8", &limits).is_err());
    }
}
