//! Port of byblos `internal/pdfbuild/pngres.go`: the resolution of a PNG as Pillow reads it for
//! img2pdf.

use super::malformed;
use crate::error::Result;

/// `PNGPhys`: collects a PNG's `pHYs` chunks as Pillow reads them for img2pdf: the last chunk
/// of unit 1 (metres) and the last of unit 0 (an aspect ratio only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PngPhys {
    metre: [u32; 2],
    aspect: [u32; 2],
    has_metre: bool,
    has_aspect: bool,
}

impl PngPhys {
    /// `Add`: reads one `pHYs` chunk body, of which only the first 9 bytes count; one with
    /// another unit changes nothing.
    ///
    /// # Errors
    /// [`crate::Error::Malformed`] for a body under 9 bytes (Pillow refuses it).
    pub fn add(&mut self, b: &[u8]) -> Result<()> {
        let Some(&[a0, a1, a2, a3, b0, b1, b2, b3, unit]) = b.first_chunk::<9>() else {
            return Err(malformed("truncated pHYs chunk"));
        };
        let v = [
            u32::from_be_bytes([a0, a1, a2, a3]),
            u32::from_be_bytes([b0, b1, b2, b3]),
        ];
        match unit {
            1 => {
                self.metre = v;
                self.has_metre = true;
            }
            0 => {
                self.aspect = v;
                self.has_aspect = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// `Resolution`: the resolution img2pdf takes from the chunks, in whole pixels per inch
    /// (x, then y): the pixels per metre of unit 1; failing that, for unit 0, img2pdf's 96 dpi
    /// default scaled along the axis the ratio favours. Zero if the chunks give none.
    pub fn resolution(&self) -> (f64, f64) {
        let a = self.aspect;
        if self.has_metre {
            return (
                (f64::from(self.metre[0]) * 0.0254).round_ties_even(),
                (f64::from(self.metre[1]) * 0.0254).round_ties_even(),
            );
        }
        if self.has_aspect && a[0] != 0 && a[1] != 0 {
            if a[0] > a[1] {
                return (
                    ((96 * u64::from(a[0])) as f64 / f64::from(a[1])).round_ties_even(),
                    96.0,
                );
            }
            return (
                96.0,
                ((96 * u64::from(a[1])) as f64 / f64::from(a[0])).round_ties_even(),
            );
        }
        (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metre_wins_over_aspect() {
        let mut p = PngPhys::default();
        // 11811 pixels per metre is 300.0 dpi (299.9994 rounded); aspect 2:1 would say 192 x 96.
        p.add(&[0, 0, 0, 2, 0, 0, 0, 1, 0]).unwrap();
        p.add(&[0, 0, 0x2E, 0x23, 0, 0, 0x2E, 0x23, 1]).unwrap();
        assert_eq!(p.resolution(), (300.0, 300.0));
    }

    #[test]
    fn aspect_only_and_none() {
        let mut p = PngPhys::default();
        assert_eq!(p.resolution(), (0.0, 0.0));
        p.add(&[0, 0, 0, 2, 0, 0, 0, 1, 0]).unwrap();
        assert_eq!(p.resolution(), (192.0, 96.0));
        let mut q = PngPhys::default();
        q.add(&[0, 0, 0, 1, 0, 0, 0, 4, 0]).unwrap();
        assert_eq!(q.resolution(), (96.0, 384.0));
        // Another unit changes nothing; a short body is refused.
        q.add(&[0, 0, 0, 9, 0, 0, 0, 9, 7]).unwrap();
        assert_eq!(q.resolution(), (96.0, 384.0));
        assert!(q.add(&[0; 8]).is_err());
    }
}
