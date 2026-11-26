//! The resource budget for symbol mode (`internal/jbig2/budget_symbol.go`), and the one place
//! in this package where a bound is enforced during decoding rather than from the headers.
//!
//! A symbol dictionary states no sizes in its header: symbol heights and widths are arithmetic
//! deltas inside the coded data, so what it costs is only knowable while decoding. A text
//! region's cost is the sum of the areas of the symbols it places, and those live in another
//! segment, or another stream. So the budget is split by when it can be evaluated: the symbol
//! and instance counts from the headers in `plan_stream`, and the symbol pixels, symbol bytes
//! and placement pixels here, checked before each allocation and each placement.
//!
//! Symbol bitmap pixels are charged into the same pixel budget as region decoding (one MQ
//! decision per pixel) and symbol bytes into the same byte budget. Placement is not an MQ
//! decision, costs about a sixth as much, and gets its own total: at most
//! `MAX_REGION_OVERDRAW` times the page plus `OVERDRAW_FLOOR_PIXELS`.

use super::generic_decode::add_decoded_pixels;
use super::segment_decode::{
    MAX_PAGE_PIXELS, MAX_REGION_OVERDRAW, MAX_STREAM_BITMAP_BYTES, OVERDRAW_FLOOR_PIXELS,
};
use crate::error::{Error, Result};

/// Bounds the symbols one stream may decode, over every dictionary in it. A header rule:
/// SDNUMNEWSYMS is four bytes, so a 14-byte segment can ask for four billion bitmap slots.
/// 1<<16 is an order of magnitude past the largest alphabet any script puts on a page.
pub const MAX_STREAM_SYMBOLS: i64 = 1 << 16;

/// Bounds the symbol instances one stream may place, over every text region in it. A header
/// rule for the same reason: SBNUMINSTANCES is four bytes, and an instance costs about thirty
/// MQ decisions in coordinate and symbol-ID decoding before anything is drawn. 1<<21 keeps that
/// per-instance work at about one maximal page's worth of decisions.
pub const MAX_STREAM_INSTANCES: i64 = 1 << 21;

fn bad(s: String) -> Error {
    Error::Malformed(s)
}

/// `streamBudget`: the running totals across a whole stream. One dictionary or region cannot
/// be judged on its own, because a stream may hold any number of individually affordable ones.
///
/// `pixels` and `bm_bytes` continue the totals `plan_stream` already charged for the page and
/// the region segments rather than starting from zero.
#[derive(Debug, Default, Clone)]
pub struct StreamBudget {
    /// MQ decisions, against `MAX_PAGE_PIXELS`.
    pub pixels: i64,
    /// Retained packed bitmap bytes, against `MAX_STREAM_BITMAP_BYTES`.
    pub bm_bytes: i64,
    /// Symbols decoded, against `MAX_STREAM_SYMBOLS`.
    pub symbols: i64,
    /// Instances placed, against `MAX_STREAM_INSTANCES`.
    pub instances: i64,
    /// Placement pixels, against `placement_limit`.
    pub placement: i64,
    /// The page's pixels, resolved once the page size is known.
    pub page_pixels: i64,
    /// `MAX_REGION_OVERDRAW * page_pixels + OVERDRAW_FLOOR_PIXELS`.
    pub placement_limit: i64,
}

impl StreamBudget {
    /// `chargeSymbols`.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the stream would declare more than `MAX_STREAM_SYMBOLS`.
    pub fn charge_symbols(&mut self, n: i64) -> Result<()> {
        if n < 0 || self.symbols.saturating_add(n) > MAX_STREAM_SYMBOLS {
            return Err(bad(format!(
                "jbig2: the stream's symbol dictionaries declare more than {MAX_STREAM_SYMBOLS} symbols; the limit \
                 is an order of magnitude past the largest alphabet a script puts on a page"
            )));
        }
        self.symbols += n;
        Ok(())
    }

    /// `chargeInstances`.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the stream would declare more than `MAX_STREAM_INSTANCES`.
    pub fn charge_instances(&mut self, n: i64) -> Result<()> {
        if n < 0 || self.instances.saturating_add(n) > MAX_STREAM_INSTANCES {
            return Err(bad(format!(
                "jbig2: the stream's text regions declare more than {MAX_STREAM_INSTANCES} symbol instances; each \
                 one costs coordinate decoding before it draws anything"
            )));
        }
        self.instances += n;
        Ok(())
    }

    /// `chargeBitmap`: charges one symbol bitmap against the pixel and memory budgets. Called
    /// with the dimensions before the bitmap exists.
    ///
    /// # Errors
    /// [`Error::Malformed`] for a non-positive or over-cap dimension or an exhausted budget.
    pub fn charge_bitmap(&mut self, w: i64, h: i64) -> Result<()> {
        if w <= 0 || h <= 0 || w > MAX_PAGE_PIXELS || h > MAX_PAGE_PIXELS {
            return Err(bad(format!("jbig2: symbol is {w}x{h}")));
        }
        // w and h are each at most 2^26, so the products cannot overflow i64.
        self.pixels += w * h;
        self.bm_bytes += (w as u64).div_ceil(8) as i64 * h;
        if self.pixels > MAX_PAGE_PIXELS || self.bm_bytes > MAX_STREAM_BITMAP_BYTES {
            return Err(bad(format!(
                "jbig2: decoding a {w}x{h} symbol takes the stream to {} pixels in {} bytes; the budget for one \
                 stream is {MAX_PAGE_PIXELS} pixels in {MAX_STREAM_BITMAP_BYTES} bytes",
                self.pixels, self.bm_bytes
            )));
        }
        Ok(())
    }

    /// `chargePlacement`: charges one symbol instance's area, and counts it into the decode
    /// counter: placing a symbol is per-pixel work the counter exists to make visible, even
    /// though it is not an MQ decision.
    ///
    /// # Errors
    /// [`Error::Malformed`] when the placed area passes the placement limit.
    pub fn charge_placement(&mut self, w: i64, h: i64) -> Result<()> {
        // A symbol was charged by charge_bitmap, so w * h is within MAX_PAGE_PIXELS.
        self.placement = self.placement.saturating_add(w.saturating_mul(h));
        if self.placement > self.placement_limit {
            return Err(bad(format!(
                "jbig2: the stream's text regions want to draw {} pixels of symbol onto a page that can show {}; \
                 the budget admits {MAX_REGION_OVERDRAW}x the page plus {OVERDRAW_FLOOR_PIXELS} pixels, and \
                 everything past that is drawn over or off the edge",
                self.placement, self.page_pixels
            )));
        }
        add_decoded_pixels(w * h);
        Ok(())
    }
}
