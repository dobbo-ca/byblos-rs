//! Port of byblos sauvola.go.
//!
//! Sauvola binarisation (J. Sauvola and M. Pietikainen, "Adaptive document image binarization",
//! Pattern Recognition 33(2), 2000): every pixel is compared with a threshold computed from the
//! mean `m` and standard deviation `s` of the odd `WINDOW_SIZE` x `WINDOW_SIZE` neighbourhood
//! centred on it:
//!
//! ```text
//! T(x,y) = m * (1 + k * (s/r - 1))
//! ```
//!
//! `r` is the dynamic range of `s`, fixed at 128 for 8-bit greyscale. A pixel strictly below
//! its local threshold is ink (bit 1), as in [`Bitmap`] (set bit = black), the inverse of
//! `/DeviceGray`.
//!
//! Known and intrinsic: a uniform dark region larger than the window comes out hollow, because
//! inside it `s` is about 0 and `T` tends to `m * (1 - k)`, so no pixel clears it. Text strokes
//! are thinner than the window and are unaffected.
//!
//! The mean and variance come from two integral images (sum and sum of squares), so the work
//! per pixel is O(1) whatever the window size. The float arithmetic keeps Go's operation order
//! and Rust never fuses multiply-add, so the output is bit-identical to Go's.

use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::image::Image;
use crate::image::color::{Color, gray_from_rgba};
use crate::limits::{Limits, check_cancel};
use std::sync::atomic::AtomicBool;

const WINDOW_SIZE: usize = 31; // odd, so every window is centred on its pixel
const K: f64 = 0.5;
const R: f64 = 128.0;

/// `color.GrayModel.Convert`: a `Gray` is kept, anything else goes through its 16-bit RGBA.
fn to_gray(c: Color) -> u8 {
    match c {
        Color::Gray(v) => v,
        _ => {
            let (r, g, b, _) = c.rgba();
            gray_from_rgba(r, g, b)
        }
    }
}

/// An `n`-element zeroed `i64` buffer; the caller has already checked `n` against the budget.
fn alloc_i64(n: usize) -> Result<Vec<i64>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)?;
    v.resize(n, 0);
    Ok(v)
}

/// One row of an integral image: `cur[x + 1] = prev[x + 1] + (vals[0] + ... + vals[x])`.
/// `cur[0]` stays 0 (the all-zero border column).
fn integral_row(prev: &[i64], cur: &mut [i64], vals: impl Iterator<Item = i64>) {
    let mut row = 0i64;
    for ((c, p), v) in cur.iter_mut().skip(1).zip(prev.iter().skip(1)).zip(vals) {
        row = row.wrapping_add(v);
        *c = p.wrapping_add(row);
    }
}

/// `Sauvola`: binarises `img` by local adaptive thresholding (window 31, k 0.5, r 128) and
/// returns a [`Bitmap`] with set bits for ink. Any [`Image`] is accepted; it is converted to
/// grey the way Go's `image.Gray.Set` does, reading the pixels at the image's own bounds.
///
/// Limits: the grey copy, the two `(w + 1) x (h + 1)` integral images of `i64` and the output
/// bitmap together are checked against `limits.max_alloc` before any is allocated. `cancel` is
/// checked once per row in each pass.
///
/// # Errors
/// [`Error::Malformed`] if the bounds are empty, [`Error::Alloc`] / [`Error::OutOfMemory`] if
/// the buffers do not fit, [`Error::Cancelled`] if `cancel` is set.
pub fn sauvola(img: &Image, limits: &Limits, cancel: &AtomicBool) -> Result<Bitmap> {
    let b = img.bounds();
    if b.is_empty() {
        return Err(Error::Malformed(format!(
            "byblos: sauvola: image bounds {b:?} are empty"
        )));
    }
    let max = limits.alloc_limit();
    let over = |got: u64| Error::Alloc { got, max };
    let w = usize::try_from(b.dx()).map_err(|_| over(u64::MAX))?;
    let h = usize::try_from(b.dy()).map_err(|_| over(u64::MAX))?;
    let iw = w.checked_add(1).ok_or_else(|| over(u64::MAX))?;
    let ih = h.checked_add(1).ok_or_else(|| over(u64::MAX))?;
    let cells = iw.checked_mul(ih).ok_or_else(|| over(u64::MAX))?;
    let bytes = cells
        .checked_mul(16)
        .zip(w.checked_mul(h))
        .and_then(|(a, g)| a.checked_add(g))
        .and_then(|t| t.checked_add(w.div_ceil(8).checked_mul(h)?))
        .ok_or_else(|| over(u64::MAX))?;
    if bytes as u64 > max {
        return Err(over(bytes as u64));
    }

    // gray is always re-anchored to (0,0): the image's own Bounds().Min need not be (0,0) (a
    // sub-image, for instance), and every read below indexes it as if it were.
    let mut gray = limits.alloc_zeroed(w * h)?;
    for (y, row) in gray.chunks_exact_mut(w).enumerate() {
        check_cancel(cancel)?;
        for (x, g) in row.iter_mut().enumerate() {
            *g = to_gray(img.at(b.x0 + x as i64, b.y0 + y as i64));
        }
    }

    // sum and sum_sq are (w+1) x (h+1) integral images, 1-indexed so row and column 0 are the
    // all-zero border: sum[y][x] is the total (sum_sq the total of squares) of every pixel with
    // row < y and column < x.
    let mut sum = alloc_i64(cells)?;
    let mut sum_sq = alloc_i64(cells)?;
    for (y, grow) in gray.chunks_exact(w).enumerate() {
        check_cancel(cancel)?;
        let vals = || grow.iter().map(|&v| i64::from(v));
        let (done, rest) = sum.split_at_mut((y + 1) * iw);
        integral_row(&done[y * iw..], &mut rest[..iw], vals());
        let (done, rest) = sum_sq.split_at_mut((y + 1) * iw);
        integral_row(&done[y * iw..], &mut rest[..iw], vals().map(|v| v * v));
    }

    let mut out = Bitmap::new(w, h, limits)?;
    let half = WINDOW_SIZE / 2;
    for y in 0..h {
        check_cancel(cancel)?;
        let y0 = y.saturating_sub(half);
        let y1 = h.min(y + half + 1);
        for x in 0..w {
            let x0 = x.saturating_sub(half);
            let x1 = w.min(x + half + 1);
            let n = ((x1 - x0) as i64) * ((y1 - y0) as i64);

            let s = sum[y1 * iw + x1] - sum[y0 * iw + x1] - sum[y1 * iw + x0] + sum[y0 * iw + x0];
            let sq = sum_sq[y1 * iw + x1] - sum_sq[y0 * iw + x1] - sum_sq[y1 * iw + x0]
                + sum_sq[y0 * iw + x0];

            let mean = s as f64 / n as f64;
            let mut variance = sq as f64 / n as f64 - mean * mean;
            if variance < 0.0 {
                variance = 0.0; // guards float rounding at near-uniform windows
            }
            let stddev = variance.sqrt();

            let threshold = mean * (1.0 + K * (stddev / R - 1.0));
            if f64::from(gray[y * w + x]) < threshold {
                out.set(x as i64, y as i64, 1);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Gray, Rect, Rgba};

    fn go() -> (Limits, AtomicBool) {
        (Limits::default(), AtomicBool::new(false))
    }

    fn run(img: &Image) -> Result<Bitmap> {
        let (l, c) = go();
        sauvola(img, &l, &c)
    }

    fn gray_uniform(w: i64, h: i64, v: u8) -> Gray {
        let mut g = Gray::new(Rect::new(0, 0, w, h), &Limits::default()).unwrap();
        g.pix.fill(v);
        g
    }

    fn set_gray(g: &mut Gray, x: i64, y: i64, v: u8) {
        let o = g.pix_offset(x, y).unwrap();
        g.pix[o] = v;
    }

    fn rgba_white(w: i64, h: i64) -> Rgba {
        let mut i = Rgba::new(Rect::new(0, 0, w, h), &Limits::default()).unwrap();
        i.pix.fill(255);
        i
    }

    #[test]
    fn rejects_empty_bounds() {
        let img = Image::Gray(Gray::new(Rect::new(0, 0, 0, 0), &Limits::default()).unwrap());
        assert!(matches!(run(&img), Err(Error::Malformed(_))));
    }

    // A uniform page has zero local variance everywhere, so no pixel crosses its own local
    // threshold: Sauvola must not invent ink out of a blank scan.
    #[test]
    fn uniform_image_produces_no_ink() {
        let bm = run(&Image::Gray(gray_uniform(64, 64, 235))).unwrap();
        assert_eq!((bm.width, bm.height), (64, 64));
        for y in 0..64 {
            for x in 0..64 {
                assert_eq!(bm.get(x, y), 0, "At({x},{y})");
            }
        }
    }

    #[test]
    fn marks_a_small_dark_mark_as_ink() {
        let mut img = gray_uniform(100, 100, 255);
        for y in 47..53 {
            for x in 47..53 {
                set_gray(&mut img, x, y, 0);
            }
        }
        let bm = run(&Image::Gray(img)).unwrap();
        assert_eq!(bm.get(50, 50), 1, "mark");
        assert_eq!(bm.get(5, 5), 0, "background corner");
    }

    #[test]
    fn is_deterministic() {
        let mut img = gray_uniform(50, 37, 128);
        for y in 10..20 {
            for x in 5..45 {
                set_gray(&mut img, x, y, ((x * 7) % 256) as u8);
            }
        }
        let img = Image::Gray(img);
        let first = run(&img).unwrap();
        for i in 0..3 {
            assert_eq!(first, run(&img).unwrap(), "run {i}");
        }
    }

    #[test]
    fn accepts_non_gray_image() {
        assert!(run(&Image::Rgba(rgba_white(20, 20))).is_ok());
    }

    // A sub-image (Bounds().Min not (0,0)) must be read at its own coordinates.
    #[test]
    fn handles_non_zero_origin_bounds() {
        let mut full = rgba_white(20, 20);
        for y in 10..12 {
            for x in 10..12 {
                let o = full.pix_offset(x, y).unwrap();
                full.pix[o..o + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        // image.RGBA.SubImage(5,5,15,15): same stride, Pix starting at the sub-origin.
        let start = full.pix_offset(5, 5).unwrap();
        let sub = Rgba {
            pix: full.pix[start..].to_vec(),
            stride: full.stride,
            rect: Rect::new(5, 5, 15, 15),
        };
        let bm = run(&Image::Rgba(sub)).unwrap();
        assert_eq!(bm.get(5, 5), 1, "mark");
        assert_eq!(bm.get(0, 0), 0, "background");
    }

    // The one fixture that tells an adaptive thresholder from a global one: an illumination
    // ramp (paper 40..180) carrying stroke rows a constant 100 below the local paper, so the
    // ink and paper intensity bands overlap and no single cutoff separates them.
    fn shaded_text_page(w: i64, h: i64) -> (Gray, Vec<bool>) {
        const ILLUM_MIN: i64 = 40;
        const ILLUM_MAX: i64 = 180;
        const INK_DEPTH: i64 = 100;
        const STROKE_PERIOD: i64 = 8;
        const STROKE_HEIGHT: i64 = 2;
        let mut g = gray_uniform(w, h, 0);
        let mut truth = vec![false; (w * h) as usize];
        for y in 0..h {
            let stroke = y % STROKE_PERIOD < STROKE_HEIGHT;
            for x in 0..w {
                let mut v = ILLUM_MIN + (ILLUM_MAX - ILLUM_MIN) * x / (w - 1);
                if stroke {
                    v = (v - INK_DEPTH).max(0);
                    truth[(y * w + x) as usize] = true;
                }
                set_gray(&mut g, x, y, v as u8);
            }
        }
        (g, truth)
    }

    // Counts the pixels where bm disagrees with the truth mask: (invented on paper, missed on
    // a stroke).
    fn mismatches_against(bm: &Bitmap, truth: &[bool]) -> (usize, usize) {
        let (mut invented, mut missed) = (0, 0);
        for y in 0..bm.height {
            for x in 0..bm.width {
                let ink = bm.get(x as i64, y as i64) == 1;
                let want = truth[y * bm.width + x];
                if ink && !want {
                    invented += 1;
                } else if !ink && want {
                    missed += 1;
                }
            }
        }
        (invented, missed)
    }

    // The acceptance gate: the threshold follows the local background. (1) Sauvola is within
    // ERROR_MAX of the truth; (2) no global cutoff of the 257 comes within GLOBAL_FLOOR; (3)
    // Sauvola beats the mark-nothing null by NULL_MARGIN.
    #[test]
    fn separates_text_under_uneven_illumination() {
        const W: i64 = 240;
        const H: i64 = 240;
        const ERROR_MAX: f64 = 0.0025;
        const GLOBAL_FLOOR: f64 = 0.05;
        const NULL_MARGIN: f64 = 20.0;
        let (img, truth) = shaded_text_page(W, H);
        let total = (W * H) as f64;
        let l = Limits::default();

        let bm = run(&Image::Gray(img.clone())).unwrap();
        let (invented, missed) = mismatches_against(&bm, &truth);
        let rate = (invented + missed) as f64 / total;
        assert!(
            rate <= ERROR_MAX,
            "{rate} off truth ({invented} invented, {missed} missed)"
        );

        let mut best = 1.0f64;
        for cut in 0..=256i64 {
            let mut global = Bitmap::new(W as usize, H as usize, &l).unwrap();
            for y in 0..H {
                for x in 0..W {
                    if i64::from(img.pixel(x, y).unwrap()[0]) < cut {
                        global.set(x, y, 1);
                    }
                }
            }
            let (gi, gm) = mismatches_against(&global, &truth);
            best = best.min((gi + gm) as f64 / total);
        }
        assert!(best >= GLOBAL_FLOOR, "a global cutoff reaches {best}");

        let null = Bitmap::new(W as usize, H as usize, &l).unwrap();
        let (ni, nm) = mismatches_against(&null, &truth);
        let null_rate = (ni + nm) as f64 / total;
        assert!(
            rate * NULL_MARGIN <= null_rate,
            "{rate} vs null {null_rate}"
        );
    }

    // Uneven illumination without text must produce no ink at all.
    #[test]
    fn emits_no_ink_on_smooth_shading_alone() {
        const W: i64 = 240;
        const H: i64 = 240;
        let mut img = gray_uniform(W, H, 0);
        for y in 0..H {
            for x in 0..W {
                set_gray(&mut img, x, y, (40 + 140 * x / (W - 1)) as u8);
            }
        }
        let bm = run(&Image::Gray(img)).unwrap();
        assert!(bm.pix.iter().all(|&b| b == 0), "ink on a smooth ramp");
    }

    #[test]
    fn cancel_and_alloc_limits() {
        let img = Image::Gray(gray_uniform(40, 40, 255));
        let (l, c) = go();
        c.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(sauvola(&img, &l, &c), Err(Error::Cancelled));
        let tight = Limits {
            max_alloc: 1000,
            ..Limits::default()
        };
        let c = AtomicBool::new(false);
        assert!(matches!(
            sauvola(&img, &tight, &c),
            Err(Error::Alloc { .. })
        ));
    }
}
