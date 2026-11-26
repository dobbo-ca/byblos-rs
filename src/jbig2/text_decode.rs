//! The text region decoding procedure of T.88 6.4.5, arithmetic variant
//! (`internal/jbig2/text_decode.go`).
//!
//! A text region places symbols from a dictionary onto a bitmap. It decodes no pixels of its
//! own: every pixel it produces was decoded once, in a symbol dictionary, and is stamped here as
//! many times as it occurs.
//!
//! The coordinate system is the part to get right. Instances are grouped into strips of
//! SBSTRIPS rows. T is the coordinate across strips and S the one along them, and TRANSPOSED
//! swaps which of those is x and which is y. Both are coded as deltas from the previous
//! instance, so a single sign error shifts every glyph after it, and the page still looks like a
//! page.

use super::arith_int::{decode_iaid, decode_int, new_int_contexts};
use super::bitmap::{mask_padding, new_bitmap};
use super::budget_symbol::StreamBudget;
use super::mq_decode::Decoder;
use super::segment_decode::{RegionInfo, Segment, composite};
use super::symbol_header::{
    REF_CORNER_BOTTOM_LEFT, REF_CORNER_BOTTOM_RIGHT, REF_CORNER_TOP_RIGHT,
    parse_text_region_header, sym_code_len,
};
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

fn bad(s: &str) -> Error {
    Error::Malformed(s.to_string())
}

/// `decodeTextRegion`: decodes one text region segment against `syms`, the symbol list its
/// referred-to dictionaries exported.
///
/// # Errors
/// [`Error::Malformed`], [`Error::UnsupportedJbig2Feature`], [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn decode_text_region(
    sg: &Segment,
    syms: &[Rc<Bitmap>],
    budget: &mut StreamBudget,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(Bitmap, RegionInfo)> {
    let h = parse_text_region_header(sg.data)?;
    h.unsupported()?;
    if syms.is_empty() {
        return Err(Error::Malformed(format!(
            "jbig2: text region refers to no symbol dictionary, so it has no symbols to place; segment {}",
            sg.number
        )));
    }
    budget.charge_instances(i64::from(h.num_instances))?;

    let strips: i64 = 1 << h.log_strips;
    let code_len = sym_code_len(syms.len());
    let mut b = new_bitmap(h.info.w, h.info.h, limits)?;
    if h.def_pixel == 1 {
        b.pix.fill(0xFF);
        mask_padding(&mut b);
    }

    let mut d = Decoder::new(sg.data.get(h.data_off..).unwrap_or(&[]));
    let mut iadt = new_int_contexts();
    let mut iafs = new_int_contexts();
    let mut iads = new_int_contexts();
    let mut iait = new_int_contexts();
    let mut iaid = vec![0u8; 1 << (code_len + 1)];

    // T.88 6.4.5 step 1: the first strip's T is coded as a negative offset.
    let Some(t0) = decode_int(&mut d, &mut iadt) else {
        return Err(bad(
            "jbig2: text region: OOB where the initial strip T was expected",
        ));
    };
    let mut strip_t = -t0 * strips;
    let (mut first_s, mut placed) = (0i64, 0i64);
    let num_instances = i64::from(h.num_instances);

    while placed < num_instances {
        let Some(dt) = decode_int(&mut d, &mut iadt) else {
            return Err(bad(
                "jbig2: text region: OOB where a strip T delta was expected",
            ));
        };
        strip_t += dt * strips;

        let Some(dfs) = decode_int(&mut d, &mut iafs) else {
            return Err(bad(
                "jbig2: text region: OOB where a strip's first S was expected",
            ));
        };
        first_s += dfs;
        let mut cur_s = first_s;

        let mut first = true;
        loop {
            if !first {
                // OOB ends the strip (T.88 6.4.5 step 3c ii).
                let Some(ids) = decode_int(&mut d, &mut iads) else {
                    break;
                };
                cur_s += ids + h.ds_offset;
            }
            first = false;
            if placed == num_instances {
                return Err(Error::Malformed(format!(
                    "jbig2: text region declares {} instances and codes more",
                    h.num_instances
                )));
            }
            let mut cur_t = 0i64;
            if strips != 1 {
                let Some(v) = decode_int(&mut d, &mut iait) else {
                    return Err(bad("jbig2: text region: OOB where a curT was expected"));
                };
                cur_t = v;
            }
            let id = decode_iaid(&mut d, &mut iaid, code_len);
            let Some(sym) = syms.get(id) else {
                return Err(Error::Malformed(format!(
                    "jbig2: text region places symbol {id} of a {}-symbol list",
                    syms.len()
                )));
            };
            budget.charge_placement(sym.width as i64, sym.height as i64)?;
            check_cancel(cancel)?;
            cur_s = place_symbol(
                &mut b,
                sym,
                cur_s,
                strip_t + cur_t,
                h.ref_corner,
                h.transposed,
                h.comb_op,
            );
            placed += 1;
        }
    }
    Ok((b, h.info))
}

/// `placeSymbol`: draws `sym` onto `b` with its reference corner at (S, T), or at (T, S) when
/// transposed, and returns the advanced S.
///
/// The pre-advance and post-advance of T.88 6.4.5 steps 3c vi, vii, x and xi are what make S mean
/// the leading edge for a left or top corner and the trailing edge for a right or bottom one.
/// The net effect either way is that the symbol occupies the same run of coordinates starting
/// where S was on entry, which is why both branches compute the same origin from different
/// arithmetic rather than placing the symbol in two different places.
pub fn place_symbol(
    b: &mut Bitmap,
    sym: &Bitmap,
    mut cur_s: i64,
    t: i64,
    ref_corner: u32,
    transposed: bool,
    op: u8,
) -> i64 {
    let (w, h) = (sym.width as i64, sym.height as i64);
    let right = ref_corner == REF_CORNER_TOP_RIGHT || ref_corner == REF_CORNER_BOTTOM_RIGHT;
    let bottom = ref_corner == REF_CORNER_BOTTOM_LEFT || ref_corner == REF_CORNER_BOTTOM_RIGHT;

    if !transposed && right {
        cur_s += w - 1;
    }
    if transposed && bottom {
        cur_s += h - 1;
    }

    let (mut x0, mut y0) = if transposed { (t, cur_s) } else { (cur_s, t) };
    if right {
        x0 -= w - 1;
    }
    if bottom {
        y0 -= h - 1;
    }
    composite(b, sym, x0, y0, op);

    if !transposed && !right {
        cur_s += w - 1;
    }
    if transposed && !bottom {
        cur_s += h - 1;
    }
    cur_s
}
