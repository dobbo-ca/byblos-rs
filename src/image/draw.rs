//! Wave 1c: the `golang.org/x/image/draw` kernel scaler, as `downsample.go` calls it:
//! `draw.CatmullRom.Scale(dst, dstRect, img, img.Bounds(), draw.Src, nil)` and
//! `draw.NearestNeighbor.Scale(...)`, with a fresh `*image.Gray` or `*image.RGBA` destination.
//!
//! Go files ported: `draw/scale.go` (`Kernel`, `newDistrib`, `ftou`, the Catmull-Rom kernel) and
//! the arms of the generated `draw/impl.go` that this call reaches: `kernelScaler.Scale`,
//! `scaleX_Gray|NRGBA|RGBA|YCbCr444|422|420|440|RGBA64Image|Image`, `scaleY_RGBA_Src`,
//! `scaleY_RGBA64Image_Src` (a `*image.Gray` destination), and `nnInterpolator.Scale` with its
//! `scale_*_Src` arms.
//!
//! Collapsed arms. Every Go arm reads a source pixel as `At(x, y).RGBA()` (its type-specific fast
//! paths inline the same integer arithmetic), so one fetch replaces them: [`Image::rgba_at`], with `fill_row` inlining the
//! Gray, Rgba and Nrgba paths.
//! The one place the arms differ in the result is the alpha of the horizontal pass: the `Gray`
//! and `YCbCr` fast paths store an exact `1` where the generic paths store a sum of float
//! products. That is kept ([`opaque_fast_path`]). The generic `Image` arm and the `RGBA64Image`
//! arm are the same arithmetic, because `RGBA64At` and `At().RGBA()` agree for every type in
//! [`Image`].
//!
//! Scope. The destination is the whole of `dst` (Go's `dr == dst.Bounds()`), the source is the
//! whole of `src` (`sr == src.Bounds()`), `op` is `Src` and `opts` is nil. For the nearest
//! neighbour interpolator Go turns a same-size scale into `Copy`; here the same-size scale runs
//! the scaling arithmetic, which picks source pixel `(dx, dy)` and so gives the same pixels.
//! `downsample.go` never calls it with equal sizes.
//!
//! Float semantics are Go's: float64 throughout, no fused multiply-add, the same operation order.
//! `ftou` converts like Go on amd64 (an out-of-range float becomes `i32::MIN`).
//!
//! Go BSD licence: see NOTICE.

use crate::error::{Error, Result};
use crate::image::color::gray_from_rgba;
use crate::image::{Gray, Image, Rect, Rgba, Subsample};
use crate::limits::{Limits, check_cancel};
use pdf::{Held, Mem};
use std::sync::atomic::AtomicBool;

/// The two interpolators `downsample.go` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolator {
    /// `draw.NearestNeighbor`.
    NearestNeighbor,
    /// `draw.CatmullRom`.
    CatmullRom,
}

/// The destination of a scale: `*image.Gray` or `*image.RGBA`, the two types `downsample.go`
/// allocates.
#[derive(Debug)]
pub enum Dst<'a> {
    Gray(&'a mut Gray),
    Rgba(&'a mut Rgba),
}

impl Dst<'_> {
    fn rect(&self) -> Rect {
        match self {
            Dst::Gray(g) => g.rect,
            Dst::Rgba(r) => r.rect,
        }
    }

    /// Stores row `dy` of 16-bit channels (r, g, b, a): `uint8(c >> 8)` per channel for `RGBA`,
    /// `Gray.SetRGBA64` for `Gray`.
    fn put_row(&mut self, dy: usize, row: &[[u16; 4]]) {
        match self {
            Dst::Gray(g) => {
                let off = dy.saturating_mul(g.stride);
                if let Some(d) = g.pix.get_mut(off..off.saturating_add(row.len())) {
                    for (p, c) in d.iter_mut().zip(row) {
                        *p = gray_from_rgba(u32::from(c[0]), u32::from(c[1]), u32::from(c[2]));
                    }
                }
            }
            Dst::Rgba(r) => {
                let off = dy.saturating_mul(r.stride);
                if let Some(d) = r.pix.get_mut(off..off.saturating_add(row.len() * 4)) {
                    for (p, c) in d.chunks_exact_mut(4).zip(row) {
                        p[0] = (c[0] >> 8) as u8;
                        p[1] = (c[1] >> 8) as u8;
                        p[2] = (c[2] >> 8) as u8;
                        p[3] = (c[3] >> 8) as u8;
                    }
                }
            }
        }
    }
}

/// `Scaler.Scale(dst, dst.Bounds(), src, src.Bounds(), draw.Src, nil)`.
///
/// The horizontal-pass buffer (`dw * sh` four-float entries), the weight tables and the per-row
/// scratch buffers are allocated, each charged against `limits.max_alloc` first. Go checks its context once
/// before the scale; `cancel` is also checked once per row of each pass.
///
/// # Errors
/// [`Error::Alloc`] if a buffer is over `max_alloc` or a dimension does not fit Go's `int32`,
/// [`Error::OutOfMemory`] if the allocator refuses, [`Error::Cancelled`] if `cancel` is set.
pub fn scale(
    mut dst: Dst<'_>,
    src: &Image,
    interp: Interpolator,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let dr = dst.rect();
    let sr = src.bounds();
    // Go: `adr.Empty() || sr.Empty()` returns without drawing.
    if dr.is_empty() || sr.is_empty() {
        return Ok(());
    }
    let (dw, dh) = (dim(dr.dx(), limits)?, dim(dr.dy(), limits)?);
    let (sw, sh) = (dim(sr.dx(), limits)?, dim(sr.dy(), limits)?);
    match interp {
        Interpolator::NearestNeighbor => {
            scale_nn(&mut dst, src, sr, (dw, dh), (sw, sh), limits, cancel)
        }
        Interpolator::CatmullRom => {
            scale_kernel(&mut dst, src, sr, (dw, dh), (sw, sh), limits, cancel)
        }
    }
}

/// A dimension as Go's `int32(dr.Dx())`, refused when it does not fit.
fn dim(v: i64, limits: &Limits) -> Result<i32> {
    i32::try_from(v).map_err(|_| Error::Alloc {
        got: u64::try_from(v).unwrap_or(u64::MAX),
        max: limits.alloc_limit(),
    })
}

/// `nnInterpolator.Scale`: source pixel `((2*dx + 1) * sw / (2*dw), (2*dy + 1) * sh / (2*dh))`.
fn scale_nn(
    dst: &mut Dst<'_>,
    src: &Image,
    sr: Rect,
    (dw, dh): (i32, i32),
    (sw, sh): (i32, i32),
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<()> {
    let mem = limits.mem();
    let (dw, dh) = (u64::from(dw.unsigned_abs()), u64::from(dh.unsigned_abs()));
    let (sw, sh) = (u64::from(sw.unsigned_abs()), u64::from(sh.unsigned_abs()));
    let (dw2, dh2) = (dw * 2, dh * 2);
    let mut sxs: Vec<i64> = Vec::new();
    let _sxs_held = reserve(&mut sxs, dw as usize, &mem)?;
    sxs.extend((0..dw).map(|dx| sr.x0.saturating_add(((2 * dx + 1) * sw / dw2) as i64)));
    let mut row: Vec<[u16; 4]> = Vec::new();
    let _row_held = reserve(&mut row, sxs.len(), &mem)?;
    for dy in 0..dh {
        check_cancel(cancel)?;
        let sy = sr.y0.saturating_add(((2 * dy + 1) * sh / dh2) as i64);
        row.clear();
        row.extend(sxs.iter().map(|&sx| {
            let (r, g, b, a) = src.rgba_at(sx, sy);
            [r as u16, g as u16, b as u16, a as u16]
        }));
        dst.put_row(dy as usize, &row);
    }
    Ok(())
}

/// `Kernel.Support` of `draw.CatmullRom`.
const CATMULL_ROM_SUPPORT: f64 = 2.0;

/// `draw.CatmullRom.At`: the cubic BC-spline kernel with B = 0, C = 0.5.
fn catmull_rom(t: f64) -> f64 {
    if t < 1.0 {
        return (1.5 * t - 2.5) * t * t + 1.0;
    }
    ((-0.5 * t + 2.5) * t - 4.0) * t + 2.0
}

/// `source`: a range of contribs, their inverse total weight, and that divided by 0xffff.
#[derive(Debug, Clone, Copy)]
struct Source {
    i: usize,
    j: usize,
    inv_total_weight: f64,
    inv_total_weight_ffff: f64,
}

/// `contrib`: the weight of a column or row.
#[derive(Debug, Clone, Copy)]
struct Contrib {
    coord: usize,
    weight: f64,
}

/// `distrib`: how source columns (or rows) are distributed over destination ones.
#[derive(Debug)]
struct Distrib {
    sources: Vec<Source>,
    contribs: Vec<Contrib>,
    /// The charges of `sources` and `contribs`.
    _held: [Held; 2],
}

/// `abs`: like `math.Abs`, but without a care for negative zero, infinities or NaNs.
fn abs(f: f64) -> f64 {
    if f < 0.0 { -f } else { f }
}

/// `ftou`: the range [0.0, 1.0] to [0, 0xffff]. Go's float to `int32` conversion on amd64
/// yields `0x80000000` for NaN and for anything out of range.
fn ftou(f: f64) -> u16 {
    let x = 65535.0 * f + 0.5;
    let i = if x.is_nan() || x >= 2_147_483_648.0 || x <= -2_147_483_649.0 {
        i32::MIN
    } else {
        x as i32
    };
    if i > 0xffff {
        0xffff
    } else if i > 0 {
        i as u16
    } else {
        0
    }
}

/// A buffer of `n` entries of `T` charged to `mem` and reserved; the charge lasts as long as the
/// returned [`Held`].
fn reserve<T>(v: &mut Vec<T>, n: usize, mem: &Mem) -> Result<Held> {
    let held = mem.charge((n as u64).saturating_mul(std::mem::size_of::<T>() as u64))?;
    v.try_reserve_exact(n)?;
    Ok(held)
}

/// `newDistrib` for the Catmull-Rom kernel: `sw` source columns (or rows) over `dw` destination
/// ones.
fn new_distrib(dw: i32, sw: i32, mem: &Mem) -> Result<Distrib> {
    let scale = f64::from(sw) / f64::from(dw);
    let mut half_width = CATMULL_ROM_SUPPORT;
    let mut kernel_arg_scale = 1.0;
    // When shrinking, broaden the effective kernel support so that every source pixel is visited.
    if scale > 1.0 {
        half_width *= scale;
        kernel_arg_scale = 1.0 / scale;
    }

    // Per destination column: the lower and upper bound of the source range and its centre.
    let ndst = usize::try_from(dw).unwrap_or(0);
    let mut bounds: Vec<(i32, i32, f64)> = Vec::new();
    let _bounds_held = reserve(&mut bounds, ndst, mem)?;
    let mut n: usize = 0;
    for x in 0..dw {
        let center = (f64::from(x) + 0.5) * scale - 0.5;
        let i = ((center - half_width).floor() as i32).max(0);
        // Rust saturates here where Go on amd64 gives i32::MIN; only for sw near 2^31.
        let mut j = (center + half_width).ceil() as i32;
        if j > sw {
            j = sw;
            if j < i {
                j = i;
            }
        }
        bounds.push((i, j, center));
        n = n.saturating_add(usize::try_from(j.saturating_sub(i)).unwrap_or(0));
    }

    let mut sources: Vec<Source> = Vec::new();
    let sources_held = reserve(&mut sources, ndst, mem)?;
    let mut contribs: Vec<Contrib> = Vec::new();
    let contribs_held = reserve(&mut contribs, n, mem)?;
    for &(i, j, center) in &bounds {
        let mut total_weight = 0.0;
        let l = contribs.len();
        for coord in i..j {
            let t = abs((center - f64::from(coord)) * kernel_arg_scale);
            if t >= CATMULL_ROM_SUPPORT {
                continue;
            }
            let weight = catmull_rom(t);
            if weight == 0.0 {
                continue;
            }
            total_weight += weight;
            contribs.push(Contrib {
                coord: usize::try_from(coord).unwrap_or(0),
                weight,
            });
        }
        let total_weight = 1.0 / total_weight;
        sources.push(Source {
            i: l,
            j: contribs.len(),
            inv_total_weight: total_weight,
            inv_total_weight_ffff: total_weight / 65535.0,
        });
    }
    Ok(Distrib {
        sources,
        contribs,
        _held: [sources_held, contribs_held],
    })
}

/// Whether Go's horizontal pass for `src` is a fast path that stores alpha as the constant `1`:
/// `scaleX_Gray` and the four `scaleX_YCbCr*` arms. The other YCbCr ratios take the generic arm.
fn opaque_fast_path(src: &Image) -> bool {
    match src {
        Image::Gray(_) => true,
        Image::YCbCr(y) => matches!(
            y.ratio,
            Subsample::S444 | Subsample::S422 | Subsample::S420 | Subsample::S440
        ),
        _ => false,
    }
}

/// `kernelScaler.Scale`: `scaleX_*` into the temporary buffer, then `scaleY_*` into `dst`.
fn scale_kernel(
    dst: &mut Dst<'_>,
    src: &Image,
    sr: Rect,
    (dw, dh): (i32, i32),
    (sw, sh): (i32, i32),
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<()> {
    let mem = limits.mem();
    let horizontal = new_distrib(dw, sw, &mem)?;
    let vertical = new_distrib(dh, sh, &mem)?;
    let dwu = usize::try_from(dw).unwrap_or(0);
    let shu = usize::try_from(sh).unwrap_or(0);
    let len = dwu.saturating_mul(shu);
    let mut tmp: Vec<[f64; 4]> = Vec::new();
    let _tmp_held = reserve(&mut tmp, len, &mem)?;

    // scaleX distributes the source image's columns over the temporary image. Each source row is
    // converted to f64 channels once, not once per weight that reads it.
    let opaque = opaque_fast_path(src);
    let mut srow: Vec<[f64; 4]> = Vec::new();
    let _srow_held = reserve(&mut srow, usize::try_from(sw).unwrap_or(0), &mem)?;
    for y in 0..shu {
        check_cancel(cancel)?;
        fill_row(src, sr, sr.y0.saturating_add(y as i64), sw, &mut srow);
        tmp.extend(horizontal.sources.iter().map(|s| {
            let (mut pr, mut pg, mut pb, mut pa) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for c in horizontal.contribs.get(s.i..s.j).unwrap_or_default() {
                if let Some(p) = srow.get(c.coord) {
                    pr += p[0] * c.weight;
                    pg += p[1] * c.weight;
                    pb += p[2] * c.weight;
                    pa += p[3] * c.weight;
                }
            }
            let k = s.inv_total_weight_ffff;
            [pr * k, pg * k, pb * k, if opaque { 1.0 } else { pa * k }]
        }));
    }

    // scaleY distributes the temporary image's rows over the destination image. The sums run
    // row by row, each column still adding its contributions in order, so they equal Go's.
    let mut acc: Vec<[f64; 4]> = Vec::new();
    let _acc_held = reserve(&mut acc, dwu, &mem)?;
    acc.resize(dwu, [0.0; 4]);
    let mut out: Vec<[u16; 4]> = Vec::new();
    let _out_held = reserve(&mut out, dwu, &mem)?;
    for (dy, s) in vertical.sources.iter().enumerate() {
        check_cancel(cancel)?;
        acc.fill([0.0; 4]);
        for c in vertical.contribs.get(s.i..s.j).unwrap_or_default() {
            let start = c.coord.saturating_mul(dwu);
            let Some(trow) = tmp.get(start..start.saturating_add(dwu)) else {
                continue;
            };
            for (a, p) in acc.iter_mut().zip(trow) {
                a[0] += p[0] * c.weight;
                a[1] += p[1] * c.weight;
                a[2] += p[2] * c.weight;
                a[3] += p[3] * c.weight;
            }
        }
        let k = s.inv_total_weight;
        out.clear();
        out.extend(acc.iter().map(|&[mut pr, mut pg, mut pb, pa]| {
            if pr > pa {
                pr = pa;
            }
            if pg > pa {
                pg = pa;
            }
            if pb > pa {
                pb = pa;
            }
            [ftou(pr * k), ftou(pg * k), ftou(pb * k), ftou(pa * k)]
        }));
        dst.put_row(dy, &out);
    }
    Ok(())
}

/// Row `sy` of `src` (columns `sr.x0..sr.x0 + sw`) as `At(x, y).RGBA()` channels in f64, with
/// the integer arithmetic of the `scaleX_Gray|RGBA|NRGBA` arms inlined for those types.
fn fill_row(src: &Image, sr: Rect, sy: i64, sw: i32, row: &mut Vec<[f64; 4]>) {
    row.clear();
    let n = usize::try_from(sw).unwrap_or(0);
    let ch = |v: u8| f64::from(u32::from(v) * 0x101);
    match src {
        Image::Gray(g) => {
            if let Some(px) = g
                .pix_offset(sr.x0, sy)
                .and_then(|o| g.pix.get(o..o.checked_add(n)?))
            {
                row.extend(px.iter().map(|&v| {
                    let y = ch(v);
                    [y, y, y, 65535.0]
                }));
                return;
            }
        }
        Image::Rgba(i) => {
            if let Some(px) = i
                .pix_offset(sr.x0, sy)
                .and_then(|o| i.pix.get(o..o.checked_add(n.checked_mul(4)?)?))
            {
                row.extend(
                    px.chunks_exact(4)
                        .map(|p| [ch(p[0]), ch(p[1]), ch(p[2]), ch(p[3])]),
                );
                return;
            }
        }
        Image::Nrgba(i) => {
            if let Some(px) = i
                .pix_offset(sr.x0, sy)
                .and_then(|o| i.pix.get(o..o.checked_add(n.checked_mul(4)?)?))
            {
                row.extend(px.chunks_exact(4).map(|p| {
                    let a = u32::from(p[3]);
                    let pm = |c: u8| f64::from(u32::from(c) * 0x101 * a / 0xff);
                    [pm(p[0]), pm(p[1]), pm(p[2]), f64::from(a * 0x101)]
                }));
                return;
            }
        }
        _ => {}
    }
    row.extend((0..i64::from(sw)).map(|x| {
        let (r, g, b, a) = src.rgba_at(sr.x0.saturating_add(x), sy);
        [f64::from(r), f64::from(g), f64::from(b), f64::from(a)]
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(w: i64, h: i64, pix: &[u8]) -> Gray {
        let mut g = Gray::new(Rect::new(0, 0, w, h), &Limits::default()).unwrap();
        g.pix.copy_from_slice(pix);
        g
    }

    #[test]
    fn nearest_neighbour_picks_the_centre_pixel() {
        // dx = 0: (1 * 4) / 4 = 1; dx = 1: (3 * 4) / 4 = 3.
        let src = Image::Gray(gray(4, 1, &[10, 20, 30, 40]));
        let mut dst = Gray::new(Rect::new(0, 0, 2, 1), &Limits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        scale(
            Dst::Gray(&mut dst),
            &src,
            Interpolator::NearestNeighbor,
            &Limits::default(),
            &cancel,
        )
        .unwrap();
        assert_eq!(dst.pix, [20, 40]);
    }

    #[test]
    fn catmull_rom_keeps_a_flat_gray_flat() {
        let src = Image::Gray(gray(4, 4, &[100; 16]));
        let mut dst = Gray::new(Rect::new(0, 0, 2, 2), &Limits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        scale(
            Dst::Gray(&mut dst),
            &src,
            Interpolator::CatmullRom,
            &Limits::default(),
            &cancel,
        )
        .unwrap();
        assert_eq!(dst.pix, [100; 4]);
    }

    #[test]
    fn catmull_rom_keeps_a_flat_rgba_flat() {
        let mut s = Rgba::new(Rect::new(0, 0, 4, 4), &Limits::default()).unwrap();
        for px in s.pix.chunks_exact_mut(4) {
            px.copy_from_slice(&[10, 20, 30, 255]);
        }
        let mut dst = Rgba::new(Rect::new(0, 0, 2, 2), &Limits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        scale(
            Dst::Rgba(&mut dst),
            &Image::Rgba(s),
            Interpolator::CatmullRom,
            &Limits::default(),
            &cancel,
        )
        .unwrap();
        assert_eq!(dst.pix, [10, 20, 30, 255].repeat(4));
    }

    #[test]
    fn ftou_clamps() {
        assert_eq!(ftou(-1.0), 0);
        assert_eq!(ftou(0.0), 0);
        assert_eq!(ftou(1.0), 0xffff);
        assert_eq!(ftou(2.0), 0xffff);
        assert_eq!(ftou(f64::NAN), 0);
        assert_eq!(ftou(f64::INFINITY), 0);
    }

    #[test]
    fn cancelled_flag_stops_the_scale() {
        let src = Image::Gray(gray(2, 2, &[1, 2, 3, 4]));
        let mut dst = Gray::new(Rect::new(0, 0, 1, 1), &Limits::default()).unwrap();
        let cancel = AtomicBool::new(true);
        let r = scale(
            Dst::Gray(&mut dst),
            &src,
            Interpolator::CatmullRom,
            &Limits::default(),
            &cancel,
        );
        assert_eq!(r, Err(Error::Cancelled));
    }
}
