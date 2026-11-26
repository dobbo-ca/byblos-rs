//! Port of byblos internal/dct/dct.go.
//!
//! Reads a JPEG's frame header, without allocating, so a decode can be sized and refused before
//! any decoder makes a buffer from it.
//!
//! Go's `Reserve` also charged `Need() + extra` to the process-wide memory pool. Rust has no
//! pool: the real allocations are charged to `max_alloc` (docs/go-differences.md), so
//! [`check_frame`] keeps only the refusals that Go reports with a `LimitError` or a plain error.

use crate::error::{Error, Result};
use crate::limits;

/// `Frame`: the first SOF marker's shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Frame {
    pub w: usize,
    pub h: usize,
    /// Component count.
    pub n: usize,
    /// Each component's horizontal and vertical sampling.
    pub samp: [[usize; 2]; 4],
    /// Progressive, or more than one scan: coefficients outlive a scan.
    pub coefs: bool,
}

/// `Parse`: reads the first SOF's frame. Bytes between segments are skipped to the next marker,
/// as image/jpeg and libjpeg do. `None` when no complete SOF precedes the first scan.
pub fn parse(data: &[u8]) -> Option<Frame> {
    let mut i = 2usize;
    while i.checked_add(4)? <= data.len() {
        let (&b0, &m, &l0, &l1) = (
            data.get(i)?,
            data.get(i + 1)?,
            data.get(i + 2)?,
            data.get(i + 3)?,
        );
        if b0 != 0xFF || m == 0xFF {
            i += 1;
        } else if m == 0 || m == 0x01 || (0xD0..=0xD8).contains(&m) {
            i += 2;
        } else if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            let hdr = data.get(i..i.checked_add(10)?)?;
            let mut f = Frame {
                h: (usize::from(hdr[5]) << 8) | usize::from(hdr[6]),
                w: (usize::from(hdr[7]) << 8) | usize::from(hdr[8]),
                n: usize::from(hdr[9]),
                ..Frame::default()
            };
            if !(1..=4).contains(&f.n) {
                return None;
            }
            let comps = data.get(i + 10..i.checked_add(10 + 3 * f.n)?)?;
            for (k, c) in comps.chunks_exact(3).enumerate() {
                let hv = c[1];
                let samp = [usize::from(hv >> 4), usize::from(hv & 15)];
                if !(1..=4).contains(&samp[0]) || !(1..=4).contains(&samp[1]) {
                    return None;
                }
                f.samp[k] = samp;
            }
            // Any SOS after the first is a second scan; a stray FFDA in a segment only makes
            // this true, which over-reserves.
            f.coefs = m & 3 == 2 || count_sos(data.get(i..)?) > 1;
            return Some(f);
        } else if m == 0xDA {
            return None;
        } else {
            i += 2 + ((usize::from(l0) << 8) | usize::from(l1));
        }
    }
    None
}

fn count_sos(b: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == 0xFF && b[i + 1] == 0xDA {
            n += 1;
            i += 1;
        }
        i += 1;
    }
    n
}

impl Frame {
    /// `Need`: the most bytes a decode of the frame holds, by either decoder byblos uses: the
    /// MCU-padded planes, int32 coefficients when they outlive a scan, the RGBA (or CMYK) image
    /// and the copy of it, and fixed state, each buffer rounded up a page. It is an upper bound.
    pub fn need(&self) -> i64 {
        const PAGE: i64 = 8192;
        let (mut max_h, mut max_v) = (1usize, 1usize);
        for k in 0..self.n.min(4) {
            max_h = max_h.max(self.samp[k][0]);
            max_v = max_v.max(self.samp[k][1]);
        }
        let (w, h) = (self.w as i64, self.h as i64);
        // Go's int64 arithmetic wraps; a hand-built Frame must not panic in debug builds.
        let (bw, bh) = (
            (max_h as i64).wrapping_mul(8),
            (max_v as i64).wrapping_mul(8),
        );
        let mxx = w
            .wrapping_add(bw)
            .wrapping_sub(1)
            .checked_div(bw)
            .unwrap_or(0);
        let myy = h
            .wrapping_add(bh)
            .wrapping_sub(1)
            .checked_div(bh)
            .unwrap_or(0);
        let mut need = (1i64 << 16)
            .wrapping_add(w.wrapping_mul(2))
            .wrapping_add(4 * PAGE);
        for k in 0..self.n.min(4) {
            // A plane, as large as the MCU grid allows.
            let grid = mxx.wrapping_mul(myy);
            let plane = grid
                .wrapping_mul((max_h as i64).wrapping_mul(max_v as i64))
                .wrapping_mul(64);
            need = need.wrapping_add(plane).wrapping_add(4 * PAGE);
            if self.coefs {
                let sub = (self.samp[k][0] as i64).wrapping_mul(self.samp[k][1] as i64);
                need = need.wrapping_add(grid.wrapping_mul(sub).wrapping_mul(256));
            }
        }
        let out = w.wrapping_mul(h).wrapping_mul(4);
        need = need.wrapping_add(out.wrapping_mul(2));
        if self.n == 4 {
            need = need.wrapping_add(out);
        }
        need
    }
}

/// `Reserve`, without the budget: reads `data`'s SOF and refuses a frame past `max_pixels`
/// ([`Error::Limit`] `image-pixels`) or one that the image dictionary does not allow. The
/// dictionary allows a frame of at most `dict_pixels` pixels (negative: no dictionary) and
/// `dict_n` components (0: unknown). A smaller frame is allowed and decodes at its own size, as
/// viewers do. Data that is not a JPEG (no `FF D8` prefix) passes with a zero [`Frame`].
///
/// # Errors
/// [`Error::Malformed`] when no frame header precedes the first scan or the dictionary refuses
/// the frame, [`Error::Limit`] when the frame is over `max_pixels`.
pub fn check_frame(data: &[u8], max_pixels: i64, dict_pixels: i64, dict_n: usize) -> Result<Frame> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Ok(Frame::default());
    }
    let f = parse(data)
        .ok_or_else(|| Error::Malformed("dct: no frame header before the first scan".into()))?;
    let px = limits::mul_sat(f.w as i64, f.h as i64);
    limits::check("image-pixels", px, max_pixels)?;
    if (dict_pixels >= 0 && px > dict_pixels) || (dict_n != 0 && f.n != dict_n) {
        return Err(Error::Malformed(format!(
            "dct: frame is {}x{} with {} components; the image dictionary allows {} pixels and {} components",
            f.w, f.h, f.n, dict_pixels, dict_n
        )));
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    // Go's dct_sof_test.go (TestDCT*) needs Context/decodeDCT from the PDF and extract layers;
    // those tests are left out here.
    use super::*;

    // SOI, SOF `m` of w x h with the given (h<<4|v) bytes, then SOS.
    fn jpeg(m: u8, w: u16, h: u16, hv: &[u8], sos: usize) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8, 0xFF, m, 0, 8 + 3 * hv.len() as u8, 8];
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&w.to_be_bytes());
        d.push(hv.len() as u8);
        for (i, &b) in hv.iter().enumerate() {
            d.extend_from_slice(&[i as u8 + 1, b, 0]);
        }
        for _ in 0..sos {
            d.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
        }
        d
    }

    #[test]
    fn parses_baseline_and_progressive() {
        let f = parse(&jpeg(0xC0, 640, 480, &[0x22, 0x11, 0x11], 1)).unwrap();
        assert_eq!((f.w, f.h, f.n), (640, 480, 3));
        assert_eq!(f.samp[0], [2, 2]);
        assert!(!f.coefs);
        assert!(parse(&jpeg(0xC2, 8, 8, &[0x11], 1)).unwrap().coefs);
        assert!(parse(&jpeg(0xC0, 8, 8, &[0x11], 2)).unwrap().coefs);
    }

    #[test]
    fn refuses_bad_frames() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0xFF, 0xD8]), None);
        assert_eq!(parse(&jpeg(0xC0, 8, 8, &[0x51], 1)), None);
        assert_eq!(parse(&jpeg(0xC0, 8, 8, &[0x11], 1)[..12]), None);
        // A scan before any frame.
        assert_eq!(parse(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2, 0, 0]), None);
    }

    #[test]
    fn need_matches_go_arithmetic() {
        let f = parse(&jpeg(0xC0, 16, 16, &[0x11], 1)).unwrap();
        // 65536 + 32 + 32768 + (2*2*1*64 + 32768) + 2*(4*256)
        assert_eq!(f.need(), 65536 + 32 + 32768 + 256 + 32768 + 2048);
    }

    #[test]
    fn check_frame_limits() {
        let d = jpeg(0xC0, 100, 100, &[0x11, 0x11, 0x11], 1);
        assert!(check_frame(&d, 10_000, -1, 0).is_ok());
        let e = check_frame(&d, 9_999, -1, 0).unwrap_err();
        assert_eq!(e.class(), "limit/image-pixels");
        assert!(check_frame(&d, i64::MAX, 9_999, 0).is_err());
        assert!(check_frame(&d, i64::MAX, -1, 1).is_err());
        assert!(check_frame(&d, i64::MAX, 10_000, 3).is_ok());
        assert_eq!(check_frame(b"PNG", 1, 0, 9).unwrap(), Frame::default());
        assert!(check_frame(&[0xFF, 0xD8, 0, 0, 0, 0], 1, 0, 0).is_err());
    }
}
