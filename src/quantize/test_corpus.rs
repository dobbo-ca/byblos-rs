//! Test fixtures: the synthetic images of byblos `internal/corpus/images.go` (`Gradient`,
//! `Photo`, `Scanpage`, `Scanjpeg`), the `psnrRGB` metric of `psnr_test.go`, and a small
//! deterministic generator that stands in for the seeded `math/rand` of the Go tests.

use crate::image::jpeg_write;
use crate::image::{Image, Rect, Rgba};
use crate::limits::Limits;
use std::sync::atomic::AtomicBool;

pub const IMAGE_W: i64 = 600;
pub const IMAGE_H: i64 = 800;

pub fn lim() -> Limits {
    Limits::default()
}

pub fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

/// A new `w` x `h` RGBA image of `rect` origin (0, 0).
pub fn new_rgba(w: i64, h: i64) -> Rgba {
    Rgba::new(Rect::new(0, 0, w, h), &lim()).unwrap()
}

/// `SetRGBA`: a write outside the bounds is ignored.
pub fn set_rgba(img: &mut Rgba, x: i64, y: i64, c: [u8; 4]) {
    if let Some(o) = img.pix_offset(x, y) {
        img.pix[o..o + 4].copy_from_slice(&c);
    }
}

pub fn gradient() -> Image {
    let mut img = new_rgba(IMAGE_W, IMAGE_H);
    for y in 0..IMAGE_H {
        for x in 0..IMAGE_W {
            let r = (x * 255 / (IMAGE_W - 1)) as u8;
            let g = (y * 255 / (IMAGE_H - 1)) as u8;
            let b = ((x + y) * 255 / (IMAGE_W + IMAGE_H - 2)) as u8;
            set_rgba(&mut img, x, y, [r, g, b, 255]);
        }
    }
    Image::Rgba(img)
}

pub fn photo() -> Image {
    use crate::go::math::sin;
    let mut img = new_rgba(IMAGE_W, IMAGE_H);
    for y in 0..IMAGE_H {
        for x in 0..IMAGE_W {
            let (fx, fy) = (x as f64, y as f64);
            let r = (127.0 + 127.0 * sin(fx / 23.0)) as u8;
            let g = (127.0 + 127.0 * sin(fy / 31.0)) as u8;
            let b = (127.0 + 127.0 * sin((fx + fy) / 17.0)) as u8;
            set_rgba(&mut img, x, y, [r, g, b, 255]);
        }
    }
    Image::Rgba(img)
}

pub fn scanpage() -> Image {
    let mut img = new_rgba(IMAGE_W, IMAGE_H);
    let paper = [250, 246, 236, 255];
    for y in 0..IMAGE_H {
        for x in 0..IMAGE_W {
            set_rgba(&mut img, x, y, paper);
        }
    }
    let band = [225, 220, 205, 255];
    for y in 20..60 {
        for x in 0..IMAGE_W {
            set_rgba(&mut img, x, y, band);
        }
    }
    let ink = [40, 35, 30, 255];
    for line in 0..20 {
        let y = 120 + line * 30;
        if y + 1 >= IMAGE_H {
            break;
        }
        let mut x = 60;
        while x < IMAGE_W - 60 {
            set_rgba(&mut img, x, y, ink);
            set_rgba(&mut img, x, y + 1, ink);
            x += 3;
        }
    }
    let stamp = [30, 60, 160, 255];
    let (cx, cy, radius) = (IMAGE_W - 120, IMAGE_H - 120, 50);
    for y in cy - radius..=cy + radius {
        for x in cx - radius..=cx + radius {
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy <= radius * radius {
                set_rgba(&mut img, x, y, stamp);
            }
        }
    }
    Image::Rgba(img)
}

/// Scanpage round-tripped through JPEG at quality 75, 4:2:0.
pub fn scanjpeg() -> Image {
    let l = lim();
    let c = no_cancel();
    let bytes = jpeg_write::encode(&scanpage(), 75, &l.mem(), &c).unwrap();
    crate::image::decode(&bytes, &l, &c).unwrap()
}

/// `psnrRGB`: PSNR in dB over the three colour channels (+inf for identical images, NaN for
/// different sizes).
pub fn psnr_rgb(a: &Image, b: &Image) -> f64 {
    let (ba, bb) = (a.bounds(), b.bounds());
    if ba.dx() != bb.dx() || ba.dy() != bb.dy() {
        return f64::NAN;
    }
    let mut sum = 0f64;
    let mut n = 0i64;
    for y in 0..ba.dy() {
        for x in 0..ba.dx() {
            let (ar, ag, ab, _) = a.rgba_at(ba.x0 + x, ba.y0 + y);
            let (br, bg, bb2, _) = b.rgba_at(bb.x0 + x, bb.y0 + y);
            let dr = f64::from((ar >> 8) as i32 - (br >> 8) as i32);
            let dg = f64::from((ag >> 8) as i32 - (bg >> 8) as i32);
            let db = f64::from((ab >> 8) as i32 - (bb2 >> 8) as i32);
            sum += dr * dr + dg * dg + db * db;
            n += 3;
        }
    }
    if sum == 0.0 {
        return f64::INFINITY;
    }
    let mse = sum / n as f64;
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// A deterministic 64-bit LCG (Knuth's MMIX constants). The Go tests use `math/rand` with a fixed
/// seed; only that the data is fixed and random-looking matters to the tests that use this.
pub struct Lcg(pub u64);

impl Lcg {
    /// The next value in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}
