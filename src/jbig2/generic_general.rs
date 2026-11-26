//! The generic region decoding procedure of T.88 6.2.5.7 for all four templates and arbitrary
//! AT pixels, over a decoder and a context array supplied by the caller
//! (`internal/jbig2/generic_general.go`).
//!
//! It exists beside [`decode_generic_region`](super::generic_decode::decode_generic_region)
//! because the two are handed different things. A symbol dictionary decodes every symbol it
//! holds from one arithmetic decoder and one shared context array (T.88 6.5.8.1), so this one
//! is handed both. The cost of generality (walking a template table, up to sixteen
//! [`Bitmap::get`] calls per pixel) is paid where it does not matter: a glyph is a few hundred
//! pixels, and a page-sized region does not come here.

use super::bitmap::new_bitmap;
use super::generic::SLTP_CONTEXT_TEMPLATE0;
use super::generic_decode::add_decoded_pixels;
use super::mq_decode::Decoder;
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::sync::atomic::AtomicBool;

/// One entry of a generic region template: a fixed offset from the pixel being coded, or the AT
/// pixel of that index.
#[derive(Debug, Clone, Copy)]
pub enum TemplatePixel {
    Fixed(i64, i64),
    At(usize),
}

use TemplatePixel::{At, Fixed};

/// The four templates of T.88 figures 4 to 7, listed most significant context bit first. The
/// bit order is the order of T.88 6.2.5.7's context formation, and it agrees pixel for pixel
/// with [`context_template0`](super::generic::context_template0) on template 0 with nominal AT.
pub static GENERIC_TEMPLATES: [&[TemplatePixel]; 4] = [
    // Template 0: 16 pixels, four AT.
    &[
        At(3),
        Fixed(-1, -2),
        Fixed(0, -2),
        Fixed(1, -2),
        At(2),
        At(1),
        Fixed(-2, -1),
        Fixed(-1, -1),
        Fixed(0, -1),
        Fixed(1, -1),
        Fixed(2, -1),
        At(0),
        Fixed(-4, 0),
        Fixed(-3, 0),
        Fixed(-2, 0),
        Fixed(-1, 0),
    ],
    // Template 1: 13 pixels, one AT.
    &[
        Fixed(-1, -2),
        Fixed(0, -2),
        Fixed(1, -2),
        Fixed(2, -2),
        Fixed(-2, -1),
        Fixed(-1, -1),
        Fixed(0, -1),
        Fixed(1, -1),
        Fixed(2, -1),
        At(0),
        Fixed(-3, 0),
        Fixed(-2, 0),
        Fixed(-1, 0),
    ],
    // Template 2: 10 pixels, one AT.
    &[
        Fixed(-1, -2),
        Fixed(0, -2),
        Fixed(1, -2),
        Fixed(-2, -1),
        Fixed(-1, -1),
        Fixed(0, -1),
        Fixed(1, -1),
        At(0),
        Fixed(-2, 0),
        Fixed(-1, 0),
    ],
    // Template 3: 10 pixels, one AT, and only two rows.
    &[
        Fixed(-3, -1),
        Fixed(-2, -1),
        Fixed(-1, -1),
        Fixed(0, -1),
        Fixed(1, -1),
        At(0),
        Fixed(-4, 0),
        Fixed(-3, 0),
        Fixed(-2, 0),
        Fixed(-1, 0),
    ],
];

/// The fixed contexts the SLTP bit is coded in under TPGDON, one per template (T.88 6.2.5.7).
pub static SLTP_CONTEXTS: [usize; 4] = [SLTP_CONTEXT_TEMPLATE0, 0x0795, 0x00E5, 0x0195];

/// The AT field of T.88 Table 5 as signed pairs, the same eight bytes `NOMINAL_AT_TEMPLATE0`
/// holds.
pub static NOMINAL_AT: [[i8; 2]; 4] = [[3, -1], [-3, -1], [2, -2], [-2, -2]];

/// `decodeGenericRegionGeneral`: decodes `w` x `h` pixels into a fresh bitmap using `template`
/// and `at`, taking its decisions from `d` and its statistics from `cx`. It charges its pixels
/// to the decode counter exactly as `decode_generic_region` does.
///
/// # Errors
/// [`Error::Malformed`] for a zero dimension or a template outside 0-3, [`Error::Alloc`],
/// [`Error::OutOfMemory`] or [`Error::Cancelled`].
pub fn decode_generic_region_general(
    d: &mut Decoder,
    cx: &mut [u8],
    w: usize,
    h: usize,
    template: usize,
    at: &[[i8; 2]; 4],
    tpgdon: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Bitmap> {
    if w == 0 || h == 0 {
        return Err(Error::Malformed(format!(
            "jbig2: generic region is {w}x{h}; dimensions must be positive"
        )));
    }
    let Some(tmpl) = GENERIC_TEMPLATES.get(template) else {
        return Err(Error::Malformed(format!(
            "jbig2: generic region template {template}; T.88 6.2.5.3 defines 0-3"
        )));
    };
    let mut b = new_bitmap(w, h, limits)?;

    let mut work: i64 = 0;
    let r = decode_rows(&mut b, d, cx, tmpl, template, at, tpgdon, cancel, &mut work);
    add_decoded_pixels(work);
    r?;
    Ok(b)
}

fn decode_rows(
    b: &mut Bitmap,
    d: &mut Decoder,
    cx: &mut [u8],
    tmpl: &[TemplatePixel],
    template: usize,
    at: &[[i8; 2]; 4],
    tpgdon: bool,
    cancel: &AtomicBool,
    work: &mut i64,
) -> Result<()> {
    let stride = b.stride;
    let sltp = SLTP_CONTEXTS.get(template).copied().unwrap_or(0);
    let mut ltp = 0u8;
    for y in 0..b.height {
        check_cancel(cancel)?;
        if tpgdon {
            *work += 1;
            ltp ^= d.decode(cx, sltp);
            if ltp == 1 {
                if y > 0 {
                    b.pix.copy_within((y - 1) * stride..y * stride, y * stride);
                }
                continue;
            }
        }
        *work += b.width as i64;
        let yi = y as i64;
        for x in 0..b.width as i64 {
            let mut ctx = 0usize;
            for p in tmpl {
                let (dx, dy) = match *p {
                    Fixed(dx, dy) => (dx, dy),
                    At(i) => {
                        let a = at.get(i).copied().unwrap_or([0, 0]);
                        (i64::from(a[0]), i64::from(a[1]))
                    }
                };
                ctx = (ctx << 1) | usize::from(b.get(x + dx, yi + dy));
            }
            if d.decode(cx, ctx) != 0 {
                b.set(x, yi, 1);
            }
        }
    }
    Ok(())
}
