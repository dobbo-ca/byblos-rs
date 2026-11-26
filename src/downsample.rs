//! Port of byblos `downsample.go`: `Downsample`, `DownsampleLimits`, `DownsampleDeclaredBPC` and
//! `DownsampleDeclaredBPCLimits` as the one function [`downsample`].
//!
//! The image is resampled from `src_dpi` to `dst_dpi` with a Catmull-Rom kernel (the
//! `x/image/draw` scaler, [`crate::image::draw`]). A source declared 1 bit per component is
//! resampled by point subsampling (`draw.NearestNeighbor`) instead, because Catmull-Rom blends a
//! bilevel image into grey levels it cannot hold. The depth is declared, never detected: an
//! [`Image`] cannot say what depth it was declared at, so the caller passes it.
//!
//! The output is a [`Gray`] for a [`Image::Gray`] source and an [`Rgba`] for every other type,
//! as in Go. A no-op (`src_dpi <= dst_dpi`, or a ratio that rounds to the source dimensions)
//! returns a copy of the input, where Go returns the same value.

use crate::error::{Error, Result};
use crate::image::draw::{self, Dst, Interpolator};
use crate::image::{Gray, Image, Rect, Rgba};
use crate::limits::{Limits, check, check_cancel, mul_sat};
use std::sync::atomic::AtomicBool;

/// Go's `image.Rectangle.String`.
fn rect_string(r: &Rect) -> String {
    format!("({},{})-({},{})", r.x0, r.y0, r.x1, r.y1)
}

/// The pixel bytes `img` holds, for charging the copy of a no-op.
fn pixel_bytes(img: &Image) -> usize {
    match img {
        Image::Gray(i) => i.pix.len(),
        Image::Gray16(i) => i.pix.len(),
        Image::Rgba(i) => i.pix.len(),
        Image::Rgba64(i) => i.pix.len(),
        Image::Nrgba(i) => i.pix.len(),
        Image::Nrgba64(i) => i.pix.len(),
        Image::Cmyk(i) => i.pix.len(),
        Image::Paletted(i) => i.pix.len(),
        Image::YCbCr(i) => {
            i.y.len()
                .saturating_add(i.cb.len())
                .saturating_add(i.cr.len())
        }
    }
}

/// The input unchanged (Go returns `img` itself), copied against `limits.max_alloc`.
fn unchanged(img: &Image, limits: &Limits) -> Result<Image> {
    let got = pixel_bytes(img) as u64;
    if got > limits.alloc_limit() {
        return Err(Error::Alloc {
            got,
            max: limits.alloc_limit(),
        });
    }
    Ok(img.clone())
}

/// `limits` with `held` bytes already alive taken off `max_alloc`, so the scaler's own ledger and
/// the destination answer to one overall budget.
fn without(limits: &Limits, held: usize) -> Limits {
    Limits {
        max_alloc: limits.alloc_limit().saturating_sub(held as u64).max(1),
        ..*limits
    }
}

/// `DownsampleDeclaredBPCLimits`: resamples `img` from `src_dpi` to `dst_dpi`.
///
/// `declared_bpc` is the source's `/BitsPerComponent` as its PDF declares it; pass `Some(1)` for
/// an `/ImageMask` stencil. `None` is `Downsample`'s assumption of 8. Only 1, 2, 4, 8 and 16
/// are valid, and 0 (the "no such entry" of a caller that does not know the depth) is refused
/// rather than silently given the contone kernel. Declared depth 1 subsamples; every other
/// depth interpolates with Catmull-Rom.
///
/// `src_dpi <= dst_dpi`, or a ratio that rounds to identical output dimensions, returns the
/// image unchanged. Otherwise the output is `round(w * ratio)` by `round(h * ratio)`, each at
/// least 1.
///
/// Limits: a source over `limits.max_image_pixels` (default 500,000,000) is refused as
/// [`Error::Limit`] named `image-pixels`, from its bounds and before anything is allocated. The
/// output and the scaler's buffers are charged to `limits.max_alloc`. `cancel` is checked on
/// entry and once per row of each scaler pass.
///
/// # Errors
/// [`Error::Malformed`] for an undeclarable depth, empty bounds or a `src_dpi` or `dst_dpi`
/// that is not positive and finite, [`Error::Limit`], [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn downsample(
    img: &Image,
    declared_bpc: Option<u8>,
    src_dpi: f64,
    dst_dpi: f64,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Image> {
    check_cancel(cancel)?;
    // The /BitsPerComponent values an image XObject may declare.
    let bpc = declared_bpc.unwrap_or(8);
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(Error::Malformed(format!(
            "byblos: downsample: declared bits per component {bpc} is not one of 1, 2, 4, 8, 16"
        )));
    }
    let b = img.bounds();
    if b.is_empty() {
        return Err(Error::Malformed(format!(
            "byblos: downsample: image bounds {} are empty",
            rect_string(&b)
        )));
    }
    check(
        "image-pixels",
        mul_sat(b.dx(), b.dy()),
        limits.image_pixels(),
    )?;
    if src_dpi <= 0.0 || !src_dpi.is_finite() {
        return Err(Error::Malformed(format!(
            "byblos: downsample: srcDPI {src_dpi} must be positive and finite"
        )));
    }
    if dst_dpi <= 0.0 || !dst_dpi.is_finite() {
        return Err(Error::Malformed(format!(
            "byblos: downsample: dstDPI {dst_dpi} must be positive and finite"
        )));
    }
    if src_dpi <= dst_dpi {
        return unchanged(img, limits);
    }

    let ratio = dst_dpi / src_dpi;
    let w = ((b.dx() as f64) * ratio).round() as i64;
    let h = ((b.dy() as f64) * ratio).round() as i64;
    let (w, h) = (w.max(1), h.max(1));
    if w == b.dx() && h == b.dy() {
        return unchanged(img, limits);
    }

    let dst_rect = Rect::new(0, 0, w, h);
    let interp = if bpc == 1 {
        Interpolator::NearestNeighbor
    } else {
        Interpolator::CatmullRom
    };
    if matches!(img, Image::Gray(_)) {
        let mut dst = Gray::new(dst_rect, limits)?;
        let rest = without(limits, dst.pix.len());
        draw::scale(Dst::Gray(&mut dst), img, interp, &rest, cancel)?;
        Ok(Image::Gray(dst))
    } else {
        let mut dst = Rgba::new(dst_rect, limits)?;
        let rest = without(limits, dst.pix.len());
        draw::scale(Dst::Rgba(&mut dst), img, interp, &rest, cancel)?;
        Ok(Image::Rgba(dst))
    }
}

#[cfg(test)]
mod tests;
