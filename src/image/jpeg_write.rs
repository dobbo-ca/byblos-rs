//! Go's `image/jpeg` encoder, for `jpeg.Encode(&buf, src, &Options{Quality})` in `optimize.go`.
//! Ports Go 1.27's `writer.go` (the baseline 4:2:0 writer, with its fixed K.3 Huffman tables
//! and the quality-scaled K.1 quantisation tables) and the forward DCT half of `dct.go`
//! (`fdct`, `fdctCols`, `fdctRows`, `dctBox`), together with `color.RGBToYCbCr` from
//! `image/color/ycbcr.go`. The output is byte-identical to Go's.
//!
//! The fast paths are Go's: `Gray` (one component), `Rgba` and `YCbCr` (three components), and
//! every other [`Image`] variant goes through [`Image::at`]. jpeg-rs's encoder matches
//! libjpeg-turbo, not Go, so it cannot be used for this.
//!
//! Go BSD licence: see NOTICE.

use super::{Gray, Image};
use crate::error::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// `block`: an 8x8 input to the DCT, in natural (not zig-zag) order.
type Block = [i32; 64];

/// `huffmanSpec`.
struct HuffSpec {
    /// `count[i]` is the number of codes of length `i + 1` bits.
    count: [u8; 16],
    /// `value[i]` is the decoded value of the i'th codeword.
    value: &'static [u8],
}

/// `unzig`: zig-zag order to natural order.
const UNZIG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// `sosHeaderY`: the SOS marker of a one-component scan.
const SOS_HEADER_Y: [u8; 10] = [0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00];

/// `sosHeaderYCbCr`: the SOS marker of a three-component scan.
const SOS_HEADER_YCBCR: [u8; 14] = [
    0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
];

/// The unscaled quantisation tables in zig-zag order (`unscaledQuant`).
const UNSCALED_QUANT: [[u8; 64]; 2] = [
    [
        16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35,
        37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80,
        109, 81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99,
    ],
    [
        17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ],
];

/// `theHuffmanSpec`: the K.3 tables, luminance DC and AC, then chrominance DC and AC.
static HUFFMAN_SPEC: [HuffSpec; 4] = [
    HuffSpec {
        count: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        value: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    },
    HuffSpec {
        count: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125],
        value: &[
            0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51,
            0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1,
            0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
            0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57,
            0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92,
            0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
            0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8,
            0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    },
    HuffSpec {
        count: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
        value: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    },
    HuffSpec {
        count: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119],
        value: &[
            0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
            0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09,
            0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25,
            0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38,
            0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
            0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
            0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
            0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    },
];

/// `huffmanLUT.init`: for each value, the codeword size in the top 8 bits and the codeword in
/// the low 24.
fn huffman_lut(s: &HuffSpec) -> Vec<u32> {
    let max_value = s.value.iter().copied().max().map_or(0, usize::from);
    let mut lut = vec![0u32; max_value + 1];
    let (mut code, mut k) = (0u32, 0usize);
    for (i, &n) in s.count.iter().enumerate() {
        let n_bits = (i as u32 + 1) << 24;
        for _ in 0..n {
            if let Some(slot) = s.value.get(k).and_then(|&v| lut.get_mut(usize::from(v))) {
                *slot = n_bits | code;
            }
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    lut
}

/// `div`: a / b rounded to the nearest integer, instead of toward zero.
fn div(a: i32, b: i32) -> i32 {
    if a >= 0 {
        (a + (b >> 1)) / b
    } else {
        -((-a + (b >> 1)) / b)
    }
}

/// The number of bits needed to hold `a` (Go's `bitCount` table).
fn bit_count(a: i32) -> u32 {
    32 - (a as u32).leading_zeros()
}

/// `color.RGBToYCbCr`.
fn rgb_to_ycbcr(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r1 = i32::from(r);
    let g1 = i32::from(g);
    let b1 = i32::from(b);
    let yy = (19595 * r1 + 38470 * g1 + 7471 * b1 + (1 << 15)) >> 16;
    let mut cb = -11056 * r1 - 21712 * g1 + 32768 * b1 + (257 << 15);
    if (cb as u32) & 0xff00_0000 == 0 {
        cb >>= 16;
    } else {
        cb = !(cb >> 31);
    }
    let mut cr = 32768 * r1 - 27440 * g1 - 5328 * b1 + (257 << 15);
    if (cr as u32) & 0xff00_0000 == 0 {
        cr >>= 16;
    } else {
        cr = !(cr >> 31);
    }
    (yy as u8, cb as u8, cr as u8)
}

// The 60-bit fixed-point constants of dct.go.
const COS1: u64 = 1130768441178740757;
const SIN1: u64 = 224923827593068887;
const COS3: u64 = 958619196450722178;
const SIN3: u64 = 640528868967736374;
const SQRT2: u64 = 1630477228166597777;
const SQRT2_COS6: u64 = 623956622067911264;
const SQRT2_SIN6: u64 = 1506364539328854985;

/// `c`: the constant `x` rounded to `bits` bits.
fn c(x: u64, bits: u32) -> i32 {
    ((x + (1u64 << (59 - bits))) >> (60 - bits)) as i32
}

/// `dctBox`: a 3-multiply, 3-add rotation and scaling.
fn dct_box(x0: i32, x1: i32, kcos: i32, ksin: i32) -> (i32, i32) {
    let ksum = kcos * (x0 + x1);
    let y0 = ksum + (ksin - kcos) * x1;
    let y1 = ksum - (kcos + ksin) * x0;
    (y0, y1)
}

/// `fdct`: the forward DCT. Inputs are UQ8.0, outputs are Q13.0.
fn fdct(b: &mut Block) {
    fdct_cols(b);
    fdct_rows(b);
}

/// `fdctCols`: the 1D DCT of the columns. Outputs are Q10.18.
fn fdct_cols(b: &mut Block) {
    for i in 0..8 {
        let (x0, x1, x2, x3) = (b[i], b[8 + i], b[16 + i], b[24 + i]);
        let (x4, x5, x6, x7) = (b[32 + i], b[40 + i], b[48 + i], b[56 + i]);

        // Stage 1: four butterflies.
        let (x0, x7) = (x0 + x7, x0 - x7);
        let (x1, x6) = (x1 + x6, x1 - x6);
        let (x2, x5) = (x2 + x5, x2 - x5);
        let (x3, x4) = (x3 + x4, x3 - x4);

        // Stage 2: two boxes and two butterflies.
        let (x4, x7) = dct_box(x4, x7, c(COS3, 18), c(SIN3, 18));
        let (x5, x6) = dct_box(x5, x6, c(COS1, 18), c(SIN1, 18));
        let (x0, x3) = (x0 + x3, x0 - x3);
        let (x1, x2) = (x1 + x2, x1 - x2);

        // Stage 3: one box and three butterflies.
        let (x2, x3) = dct_box(x2, x3, c(SQRT2_COS6, 18), c(SQRT2_SIN6, 18));
        let (x0, x1) = (x0 + x1, x0 - x1);

        b[i] = (x0 - 128 * 8) << 18;
        b[32 + i] = x1 << 18;
        b[16 + i] = x2;
        b[48 + i] = x3;

        let (x4, x6) = (x4 + x6, x4 - x6);
        let (x7, x5) = (x7 + x5, x7 - x5);

        // Stage 4: two sqrt(2) scalings and one butterfly.
        let x5 = (x5 >> 12) * c(SQRT2, 12);
        let x6 = (x6 >> 12) * c(SQRT2, 12);
        let (x7, x4) = (x7 + x4, x7 - x4);

        b[8 + i] = x7;
        b[24 + i] = x5;
        b[40 + i] = x6;
        b[56 + i] = x4;
    }
}

/// `fdctRows`: the 1D DCT of the rows. Inputs are Q10.18, outputs are Q13.0.
fn fdct_rows(b: &mut Block) {
    for x in b.chunks_exact_mut(8) {
        let (x0, x1, x2, x3) = (x[0], x[1], x[2], x[3]);
        let (x4, x5, x6, x7) = (x[4], x[5], x[6], x[7]);

        // Stage 1: four butterflies.
        let (x0, x7) = (x0 + x7, x0 - x7);
        let (x1, x6) = (x1 + x6, x1 - x6);
        let (x2, x5) = (x2 + x5, x2 - x5);
        let (x3, x4) = (x3 + x4, x3 - x4);

        // Stage 2: two boxes and two butterflies.
        let (x4, x7) = dct_box(x4 >> 14, x7 >> 14, c(COS3, 14), c(SIN3, 14));
        let (x5, x6) = dct_box(x5 >> 14, x6 >> 14, c(COS1, 14), c(SIN1, 14));
        let (x0, x3) = (x0 + x3, x0 - x3);
        let (x1, x2) = (x1 + x2, x1 - x2);

        // Stage 3: one box and three butterflies.
        let (x2, x3) = dct_box(x2 >> 14, x3 >> 14, c(SQRT2_COS6, 14), c(SQRT2_SIN6, 14));
        let (x0, x1) = (x0 + x1, x0 - x1);
        let (x4, x6) = (x4 + x6, x4 - x6);
        let (x7, x5) = (x7 + x5, x7 - x5);

        // Stage 4: two sqrt(2) scalings and one butterfly.
        let x5 = (x5 >> 14) * c(SQRT2, 14);
        let x6 = (x6 >> 14) * c(SQRT2, 14);
        let (x7, x4) = (x7 + x4, x7 - x4);

        // Cut from Q13.18 to Q13.0.
        let cut = |v: i32| (v + (1 << 17)) >> 18;
        x[0] = cut(x0);
        x[1] = cut(x7);
        x[2] = cut(x2);
        x[3] = cut(x5);
        x[4] = cut(x1);
        x[5] = cut(x6);
        x[6] = cut(x3);
        x[7] = cut(x4);
    }
}

/// The error for a pixel buffer that is shorter than its bounds say. Go would panic.
fn short_pixels() -> Error {
    Error::Malformed("jpeg: pixel buffer is shorter than the image bounds".to_string())
}

/// `grayToY`: the 8x8 region of `m` whose top-left corner is (px, py), edge pixels repeated.
fn gray_to_y(m: &Gray, px: i64, py: i64, y_block: &mut Block) -> Result<()> {
    let xmax = m.rect.x1.saturating_sub(1);
    let ymax = m.rect.y1.saturating_sub(1);
    for j in 0..8 {
        let sy = py.saturating_add(j as i64).min(ymax);
        for i in 0..8 {
            let sx = px.saturating_add(i as i64).min(xmax);
            let v = m
                .pixel(sx, sy)
                .and_then(|p| p.first())
                .ok_or_else(short_pixels)?;
            y_block[8 * j + i] = i32::from(*v);
        }
    }
    Ok(())
}

/// The Y, Cb, Cr of pixel (x, y): `rgbaToYCbCr` for `Rgba`, `yCbCrToYCbCr` for `YCbCr` and
/// `toYCbCr` (through `At`) for the rest.
fn ycbcr_at(img: &Image, x: i64, y: i64) -> Result<(u8, u8, u8)> {
    let v = match img {
        Image::Rgba(m) => m.pixel(x, y).map(|p| rgb_to_ycbcr(p[0], p[1], p[2])),
        Image::YCbCr(m) => m
            .y_offset(x, y)
            .zip(m.c_offset(x, y))
            .and_then(|(yi, ci)| Some((*m.y.get(yi)?, *m.cb.get(ci)?, *m.cr.get(ci)?))),
        _ => {
            let (r, g, b, _) = img.at(x, y).rgba();
            Some(rgb_to_ycbcr((r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8))
        }
    };
    v.ok_or_else(short_pixels)
}

/// `toYCbCr` and its two specialisations: the 8x8 region of `img` whose top-left corner is
/// (px, py) as Y, Cb and Cr blocks, edge pixels repeated.
fn to_ycbcr(
    img: &Image,
    px: i64,
    py: i64,
    y_block: &mut Block,
    cb_block: &mut Block,
    cr_block: &mut Block,
) -> Result<()> {
    let b = img.bounds();
    let xmax = b.x1.saturating_sub(1);
    let ymax = b.y1.saturating_sub(1);
    for j in 0..8 {
        let sy = py.saturating_add(j as i64).min(ymax);
        for i in 0..8 {
            let sx = px.saturating_add(i as i64).min(xmax);
            let (yy, cb, cr) = ycbcr_at(img, sx, sy)?;
            y_block[8 * j + i] = i32::from(yy);
            cb_block[8 * j + i] = i32::from(cb);
            cr_block[8 * j + i] = i32::from(cr);
        }
    }
    Ok(())
}

/// `scale`: the 16x16 region represented by the 4 `src` blocks, scaled to the 8x8 `dst` block.
fn scale(dst: &mut Block, src: &[Block; 4]) {
    for (i, s) in src.iter().enumerate() {
        let dst_off = ((i & 2) << 4) | ((i & 1) << 2);
        for y in 0..4 {
            for x in 0..4 {
                let j = 16 * y + 2 * x;
                let sum = s[j] + s[j + 1] + s[j + 8] + s[j + 9];
                dst[8 * y + x + dst_off] = (sum + 2) >> 2;
            }
        }
    }
}

/// `encoder`. `err` is the first error met: after it, writes are no-ops, as in Go.
struct Encoder {
    out: Vec<u8>,
    err: Option<Error>,
    /// The call's ledger and the charge for the output grown so far.
    mem: pdf::Mem,
    held: pdf::Held,
    /// Accumulated bits to write to `out`.
    bits: u32,
    n_bits: u32,
    /// The scaled quantisation tables, in zig-zag order.
    quant: [[u8; 64]; 2],
    lut: [Vec<u32>; 4],
}

impl Encoder {
    fn grow(&mut self, extra: usize) {
        // Double, but never past the budget (unless `extra` alone is over it: then the charge
        // fails below).
        let left = usize::try_from(self.mem.left()).unwrap_or(usize::MAX);
        let add = extra.max(self.out.len()).max(4096).min(left).max(extra);
        if let Err(e) = self.held.grow_by(add as u64) {
            self.err = Some(e.into());
            return;
        }
        if let Err(e) = self.out.try_reserve_exact(add) {
            self.err = Some(e.into());
        }
    }

    fn write(&mut self, p: &[u8]) {
        if self.err.is_some() {
            return;
        }
        if self.out.capacity() - self.out.len() < p.len() {
            self.grow(p.len());
            if self.err.is_some() {
                return;
            }
        }
        self.out.extend_from_slice(p);
    }

    fn write_byte(&mut self, b: u8) {
        self.write(&[b]);
    }

    /// The first error met, if any.
    fn failed(&mut self) -> Result<()> {
        match self.err.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// `emit`: the least significant `n_bits` bits of `bits` to the bit-stream.
    fn emit(&mut self, bits: u32, n_bits: u32) {
        let mut n_bits = n_bits + self.n_bits;
        let mut bits = bits.checked_shl(32u32.wrapping_sub(n_bits)).unwrap_or(0) | self.bits;
        while n_bits >= 8 {
            let b = (bits >> 24) as u8;
            self.write_byte(b);
            if b == 0xff {
                self.write_byte(0x00);
            }
            bits <<= 8;
            n_bits -= 8;
        }
        self.bits = bits;
        self.n_bits = n_bits;
    }

    /// `emitHuff`: `value` with the Huffman encoder `h`.
    fn emit_huff(&mut self, h: usize, value: i32) {
        let x = usize::try_from(value)
            .ok()
            .and_then(|v| self.lut[h].get(v))
            .copied()
            .unwrap_or(0);
        self.emit(x & ((1 << 24) - 1), x >> 24);
    }

    /// `emitHuffRLE`: a run of `run_length` copies of `value` with the Huffman encoder `h`.
    fn emit_huff_rle(&mut self, h: usize, run_length: i32, value: i32) {
        let (a, b) = if value < 0 {
            (value.wrapping_neg(), value - 1)
        } else {
            (value, value)
        };
        let n_bits = if a < 0x100 {
            bit_count(a)
        } else {
            8 + bit_count(a >> 8)
        };
        self.emit_huff(h, (run_length << 4) | n_bits as i32);
        if n_bits > 0 {
            self.emit((b as u32) & ((1u32 << n_bits) - 1), n_bits);
        }
    }

    /// `writeMarkerHeader`.
    fn write_marker_header(&mut self, marker: u8, marker_len: usize) {
        self.write(&[
            0xff,
            marker,
            (marker_len >> 8) as u8,
            (marker_len & 0xff) as u8,
        ]);
    }

    /// `writeDQT`: the Define Quantization Table marker.
    fn write_dqt(&mut self) {
        self.write_marker_header(0xdb, 2 + 2 * (1 + 64));
        for i in 0..2 {
            self.write_byte(i as u8);
            let table = self.quant[i];
            self.write(&table);
        }
    }

    /// `writeSOF0`: the Start Of Frame (Baseline Sequential) marker.
    fn write_sof0(&mut self, width: i64, height: i64, n_component: usize) {
        self.write_marker_header(0xc0, 8 + 3 * n_component);
        let size = [
            (height >> 8) as u8,
            (height & 0xff) as u8,
            (width >> 8) as u8,
            (width & 0xff) as u8,
        ];
        let [yh, yl, xh, xl] = size;
        if n_component == 1 {
            // No subsampling for grayscale image.
            self.write(&[8, yh, yl, xh, xl, 1, 1, 0x11, 0x00]);
        } else {
            // 4:2:0 chroma subsampling.
            self.write(&[
                8, yh, yl, xh, xl, 3, 1, 0x22, 0x00, 2, 0x11, 0x01, 3, 0x11, 0x01,
            ]);
        }
    }

    /// `writeDHT`: the Define Huffman Table marker. A grayscale image drops the chrominance
    /// tables.
    fn write_dht(&mut self, n_component: usize) {
        let specs = if n_component == 1 {
            &HUFFMAN_SPEC[..2]
        } else {
            &HUFFMAN_SPEC[..]
        };
        let marker_len = 2 + specs.iter().map(|s| 1 + 16 + s.value.len()).sum::<usize>();
        self.write_marker_header(0xc4, marker_len);
        for (i, s) in specs.iter().enumerate() {
            self.write_byte([0x00, 0x10, 0x01, 0x11][i]);
            self.write(&s.count);
            self.write(s.value);
        }
    }

    /// `writeBlock`: a block of pixel data with the quantisation table `q`, returning the
    /// post-quantised DC value. `b` is in natural order.
    fn write_block(&mut self, b: &mut Block, q: usize, prev_dc: i32) -> i32 {
        fdct(b);
        // Emit the DC delta.
        let dc = div(b[0], 8 * i32::from(self.quant[q][0]));
        self.emit_huff_rle(2 * q, 0, dc - prev_dc);
        // Emit the AC components.
        let h = 2 * q + 1;
        let mut run_length = 0i32;
        for zig in 1..64 {
            let ac = div(b[UNZIG[zig]], 8 * i32::from(self.quant[q][zig]));
            if ac == 0 {
                run_length += 1;
            } else {
                while run_length > 15 {
                    self.emit_huff(h, 0xf0);
                    run_length -= 16;
                }
                self.emit_huff_rle(h, run_length, ac);
                run_length = 0;
            }
        }
        if run_length > 0 {
            self.emit_huff(h, 0x00);
        }
        dc
    }

    /// `writeSOS`: the StartOfScan marker and the scan.
    fn write_sos(&mut self, img: &Image, cancel: &AtomicBool) -> Result<()> {
        let bounds = img.bounds();
        // The blocks are in natural order. DC components are delta-encoded.
        let mut b: Block = [0; 64];
        if let Image::Gray(m) = img {
            self.write(&SOS_HEADER_Y);
            let mut prev_dc_y = 0;
            for y in (bounds.y0..bounds.y1).step_by(8) {
                for x in (bounds.x0..bounds.x1).step_by(8) {
                    gray_to_y(m, x, y, &mut b)?;
                    prev_dc_y = self.write_block(&mut b, 0, prev_dc_y);
                }
                self.failed()?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
            }
        } else {
            self.write(&SOS_HEADER_YCBCR);
            let mut cb: [Block; 4] = [[0; 64]; 4];
            let mut cr: [Block; 4] = [[0; 64]; 4];
            let (mut prev_dc_y, mut prev_dc_cb, mut prev_dc_cr) = (0, 0, 0);
            for y in (bounds.y0..bounds.y1).step_by(16) {
                for x in (bounds.x0..bounds.x1).step_by(16) {
                    for i in 0..4 {
                        let x_off = ((i & 1) * 8) as i64;
                        let y_off = ((i & 2) * 4) as i64;
                        let (px, py) = (x.saturating_add(x_off), y.saturating_add(y_off));
                        to_ycbcr(img, px, py, &mut b, &mut cb[i], &mut cr[i])?;
                        prev_dc_y = self.write_block(&mut b, 0, prev_dc_y);
                    }
                    scale(&mut b, &cb);
                    prev_dc_cb = self.write_block(&mut b, 1, prev_dc_cb);
                    scale(&mut b, &cr);
                    prev_dc_cr = self.write_block(&mut b, 1, prev_dc_cr);
                }
                self.failed()?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
            }
        }
        // Pad the last byte with 1's.
        self.emit(0x7f, 7);
        Ok(())
    }
}

/// `jpeg.Encode(w, img, &Options{Quality: quality})`: `img` as a JPEG 4:2:0 baseline file. The
/// quality is clipped to 1..=100, as Go does. The output is charged to `mem` as it grows (the
/// ledger of the decode that produced `img`, so source and output share one budget); `cancel` is
/// checked once per MCU row.
///
/// # Errors
/// [`Error::Malformed`] ("jpeg: image is too large to encode") when a side is 65 536 or more,
/// and for a pixel buffer shorter than its bounds (Go panics there); [`Error::Cancelled`];
/// [`Error::Alloc`] and
/// [`Error::OutOfMemory`] when the output outgrows the budget or the allocator refuses.
pub fn encode(img: &Image, quality: i32, mem: &pdf::Mem, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let b = img.bounds();
    if b.dx() >= 1 << 16 || b.dy() >= 1 << 16 {
        return Err(Error::Malformed(
            "jpeg: image is too large to encode".to_string(),
        ));
    }
    // Clip quality to [1, 100].
    let quality = quality.clamp(1, 100);
    // Convert from a quality rating to a scaling factor.
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };
    // Initialise the quantisation tables.
    let mut quant = [[0u8; 64]; 2];
    for (dst, src) in quant.iter_mut().zip(UNSCALED_QUANT.iter()) {
        for (d, &s) in dst.iter_mut().zip(src.iter()) {
            let x = (i32::from(s) * scale + 50) / 100;
            *d = x.clamp(1, 255) as u8;
        }
    }
    let mut e = Encoder {
        out: Vec::new(),
        err: None,
        mem: mem.clone(),
        held: mem.charge(0)?,
        bits: 0,
        n_bits: 0,
        quant,
        lut: std::array::from_fn(|i| huffman_lut(&HUFFMAN_SPEC[i])),
    };
    let n_component = if matches!(img, Image::Gray(_)) { 1 } else { 3 };
    // Start Of Image, the quantisation tables, the image dimensions, the Huffman tables, the
    // image data and End Of Image.
    e.write(&[0xff, 0xd8]);
    e.write_dqt();
    e.write_sof0(b.dx(), b.dy(), n_component);
    e.write_dht(n_component);
    e.write_sos(img, cancel)?;
    e.write(&[0xff, 0xd9]);
    e.failed()?;
    Ok(e.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Nrgba, Rect, Rgba, Subsample, YCbCr};
    use crate::limits::Limits;
    use pdf::filter::gojpeg::{GoImage, decode};

    /// `zigzag` of writer_test.go: natural order to zig-zag order.
    const ZIGZAG: [usize; 64] = [
        0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9,
        11, 18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60,
        21, 34, 37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
    ];
    /// `unscaledQuantInNaturalOrder` of writer_test.go (section K.1).
    const UNSCALED_QUANT_NATURAL: [[u8; 64]; 2] = [
        [
            16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57,
            69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55,
            64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100,
            103, 99,
        ],
        [
            17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99,
            99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
            99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        ],
    ];

    fn limits() -> Limits {
        Limits::default()
    }

    /// `jpeg.Decode`, through pdf-rs's port of Go's decoder.
    fn decode_back(bytes: &[u8]) -> Image {
        let l = limits();
        let d = decode(bytes, &l.mem(), &AtomicBool::new(false)).unwrap();
        match d.image {
            GoImage::Gray(i) => Image::Gray(i.into()),
            GoImage::YCbCr(i) => Image::YCbCr(i.into()),
            GoImage::Rgba(i) => Image::Rgba(i.into()),
            GoImage::Cmyk(i) => Image::Cmyk(i.into()),
        }
    }

    /// `averageDelta`: the average delta in RGB space. The two images must have the same bounds.
    fn average_delta(m0: &Image, m1: &Image) -> i64 {
        let b = m0.bounds();
        let (mut sum, mut n) = (0i64, 0i64);
        for y in b.y0..b.y1 {
            for x in b.x0..b.x1 {
                let (r0, g0, b0, _) = m0.rgba_at(x, y);
                let (r1, g1, b1, _) = m1.rgba_at(x, y);
                for (u0, u1) in [(r0, r1), (g0, g1), (b0, b1)] {
                    sum += (i64::from(u0) - i64::from(u1)).abs();
                    n += 1;
                }
            }
        }
        sum / n
    }

    /// A smooth 150 x 103 opaque RGBA image (Go's test reads testdata/video-001.png, which needs
    /// the PNG reader that has not landed). The size is not a multiple of 16, so the edge
    /// replication is exercised.
    fn gradient() -> Rgba {
        let mut m = Rgba::new(Rect::new(0, 0, 150, 103), &limits()).unwrap();
        for y in 0..103usize {
            for x in 0..150usize {
                let o = y * m.stride + 4 * x;
                m.pix[o..o + 4].copy_from_slice(&[x as u8, y as u8, ((x + y) / 2) as u8, 255]);
            }
        }
        m
    }

    #[test]
    fn zig_unzig() {
        for i in 0..64 {
            assert_eq!(UNZIG[ZIGZAG[i]], i, "unzig[zigzag[{i}]]");
            assert_eq!(ZIGZAG[UNZIG[i]], i, "zigzag[unzig[{i}]]");
        }
    }

    #[test]
    fn unscaled_quant() {
        for i in 0..2 {
            for zig in 0..64 {
                assert_eq!(
                    UNSCALED_QUANT[i][zig], UNSCALED_QUANT_NATURAL[i][UNZIG[zig]],
                    "i={i}, zig={zig}"
                );
            }
        }
    }

    #[test]
    fn writer() {
        // Go's testCase tolerances, in 16-bit channel units.
        let cases: [(i32, i64); 6] = [
            (1, 24 << 8),
            (20, 12 << 8),
            (60, 8 << 8),
            (80, 6 << 8),
            (90, 4 << 8),
            (100, 2 << 8),
        ];
        let m0 = Image::Rgba(gradient());
        for (quality, tolerance) in cases {
            let bytes = encode(&m0, quality, &limits().mem(), &AtomicBool::new(false)).unwrap();
            let m1 = decode_back(&bytes);
            assert_eq!(m0.bounds(), m1.bounds(), "quality={quality}");
            let d = average_delta(&m0, &m1);
            assert!(d <= tolerance, "quality={quality}: delta {d} > {tolerance}");
        }
    }

    #[test]
    fn write_grayscale() {
        let mut g = Gray::new(Rect::new(0, 0, 32, 32), &limits()).unwrap();
        for (i, p) in g.pix.iter_mut().enumerate() {
            *p = i as u8;
        }
        let m0 = Image::Gray(g);
        let bytes = encode(&m0, 75, &limits().mem(), &AtomicBool::new(false)).unwrap();
        let m1 = decode_back(&bytes);
        assert_eq!(m0.bounds(), m1.bounds());
        assert!(matches!(m1, Image::Gray(_)), "want a Gray image");
        let d = average_delta(&m0, &m1);
        assert!(d <= 2 << 8, "average delta too high: {d}");
    }

    /// `TestEncodeYCbCr`: an RGBA image and the 4:4:4 YCbCr image of the same colours encode to
    /// the same bytes. A small xorshift stands in for Go's seeded `math/rand`.
    #[test]
    fn encode_ycbcr_matches_rgba() {
        let (w, h) = (64usize, 48usize);
        let rect = Rect::new(0, 0, w as i64, h as i64);
        let mut rgba = Rgba::new(rect, &limits()).unwrap();
        let mut ycc = YCbCr {
            y: vec![0; w * h],
            cb: vec![0; w * h],
            cr: vec![0; w * h],
            y_stride: w,
            c_stride: w,
            ratio: Subsample::S444,
            rect,
        };
        let mut s = 123u32;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 8) as u8
        };
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = (next(), next(), next());
                let o = y * rgba.stride + 4 * x;
                rgba.pix[o..o + 4].copy_from_slice(&[r, g, b, 255]);
                let (yy, cb, cr) = rgb_to_ycbcr(r, g, b);
                ycc.y[y * w + x] = yy;
                ycc.cb[y * w + x] = cb;
                ycc.cr[y * w + x] = cr;
            }
        }
        let a = encode(
            &Image::Rgba(rgba),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let b = encode(
            &Image::YCbCr(ycc),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(a, b);
    }

    /// The generic path (through `At`) gives the bytes of the `Rgba` fast path for opaque pixels.
    #[test]
    fn generic_path_matches_rgba_path() {
        let g = gradient();
        let mut n = Nrgba::new(g.rect, &limits()).unwrap();
        n.pix.copy_from_slice(&g.pix);
        let a = encode(
            &Image::Rgba(g),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let b = encode(
            &Image::Nrgba(n),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(a, b);
    }

    /// The marker segments in order, with their lengths checked against the bytes.
    fn markers(j: &[u8]) -> Vec<(u8, usize)> {
        assert_eq!(&j[..2], &[0xff, 0xd8]);
        let mut out = Vec::new();
        let mut i = 2;
        loop {
            assert_eq!(j[i], 0xff, "marker at {i}");
            let m = j[i + 1];
            if m == 0xd9 {
                assert_eq!(i + 2, j.len(), "EOI is the last two bytes");
                return out;
            }
            let len = (usize::from(j[i + 2]) << 8) | usize::from(j[i + 3]);
            out.push((m, len));
            i += 2 + len;
            if m == 0xda {
                // Entropy-coded data: skip to the EOI (0xff is always stuffed with 0x00).
                while j[i] != 0xff || j[i + 1] != 0xd9 {
                    if j[i] == 0xff {
                        assert_eq!(j[i + 1], 0x00, "unstuffed 0xff at {i}");
                    }
                    i += 1;
                }
            }
        }
    }

    #[test]
    fn color_structure() {
        let bytes = encode(
            &Image::Rgba(gradient()),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let want = vec![
            (0xdb, 132),
            (0xc0, 17),
            (0xc4, 2 + 4 * 17 + 12 + 162 + 12 + 162),
            (0xda, 12),
        ];
        assert_eq!(markers(&bytes), want);
        // SOF0: 8-bit, height 103, width 150, three components with 2x2, 1x1, 1x1 sampling.
        let sof = bytes.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        assert_eq!(
            &bytes[sof + 4..sof + 19],
            &[8, 0, 103, 0, 150, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]
        );
    }

    #[test]
    fn gray_structure() {
        let g = Gray::new(Rect::new(0, 0, 9, 9), &limits()).unwrap();
        let bytes = encode(
            &Image::Gray(g),
            75,
            &limits().mem(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let want = vec![
            (0xdb, 132),
            (0xc0, 11),
            (0xc4, 2 + 2 * 17 + 12 + 162),
            (0xda, 8),
        ];
        assert_eq!(markers(&bytes), want);
    }

    #[test]
    fn quality_is_clipped() {
        let m = Image::Rgba(gradient());
        let q1 = encode(&m, 1, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            encode(&m, 0, &limits().mem(), &AtomicBool::new(false)).unwrap(),
            q1
        );
        assert_eq!(
            encode(&m, -5, &limits().mem(), &AtomicBool::new(false)).unwrap(),
            q1
        );
        let q100 = encode(&m, 100, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            encode(&m, 101, &limits().mem(), &AtomicBool::new(false)).unwrap(),
            q100
        );
        assert_ne!(q1, q100);
    }

    #[test]
    fn quantisation_tables_follow_quality() {
        let g = Image::Gray(Gray::new(Rect::new(0, 0, 8, 8), &limits()).unwrap());
        // Quality 50 is scale 100: the unscaled table. DQT data starts after FFD8 FFDB len(2) id.
        let j = encode(&g, 50, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert_eq!(&j[7..7 + 64], &UNSCALED_QUANT[0]);
        assert_eq!(j[6], 0);
        // Quality 100 is scale 0: every entry clamps to 1.
        let j = encode(&g, 100, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert!(j[7..7 + 64].iter().all(|&q| q == 1));
        // Quality 1 is scale 5000: every entry clamps to 255.
        let j = encode(&g, 1, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert!(j[7..7 + 64].iter().all(|&q| q == 255));
    }

    #[test]
    fn fdct_of_flat_blocks() {
        let mut b: Block = [128; 64];
        fdct(&mut b);
        assert_eq!(b, [0; 64]);
        let mut b: Block = [255; 64];
        fdct(&mut b);
        assert_eq!(b[0], 64 * (255 - 128));
        assert!(b[1..].iter().all(|&v| v == 0));
    }

    #[test]
    fn rgb_to_ycbcr_known_values() {
        assert_eq!(rgb_to_ycbcr(0, 0, 0), (0, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 255, 255), (255, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 0, 0), (76, 85, 255));
    }

    #[test]
    fn empty_image_is_headers_only() {
        let g = Image::Gray(Gray::new(Rect::new(0, 0, 0, 0), &limits()).unwrap());
        let j = encode(&g, 75, &limits().mem(), &AtomicBool::new(false)).unwrap();
        assert_eq!(&j[..2], &[0xff, 0xd8]);
        assert_eq!(&j[j.len() - 2..], &[0xff, 0xd9]);
    }

    #[test]
    fn too_large_is_refused() {
        let img = Image::Gray(Gray {
            pix: Vec::new(),
            stride: 0,
            rect: Rect::new(0, 0, 1 << 16, 1),
        });
        let err = encode(&img, 75, &limits().mem(), &AtomicBool::new(false)).unwrap_err();
        assert_eq!(err.class(), "error");
        assert!(err.to_string().contains("too large to encode"));
    }

    #[test]
    fn short_pixel_buffer_is_an_error_not_a_panic() {
        let img = Image::Gray(Gray {
            pix: vec![0; 3],
            stride: 8,
            rect: Rect::new(0, 0, 8, 8),
        });
        assert!(matches!(
            encode(&img, 75, &limits().mem(), &AtomicBool::new(false)),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn output_is_charged_to_max_alloc() {
        let l = Limits {
            max_alloc: 1000,
            ..Limits::default()
        };
        assert!(matches!(
            encode(
                &Image::Rgba(gradient()),
                100,
                &l.mem(),
                &AtomicBool::new(false)
            ),
            Err(Error::Alloc { .. })
        ));
    }
}
