//! The symbol dictionary decoding procedure of T.88 6.5.5, arithmetic variant
//! (`internal/jbig2/symbol_decode.go`).
//!
//! A symbol dictionary is a list of small bitmaps and nothing else; it paints nothing and is
//! consumed by the text regions that refer to it. Symbols are emitted sorted by height, in runs
//! of equal height (height classes), and only the deltas are coded: the height delta from the
//! previous class, then within a class the width delta from the previous symbol. A class ends
//! on OOB from the width decoder. Nothing in the segment header says how big any symbol is,
//! which is the fact the resource budget has to be built around (see `StreamBudget`).

use super::arith_int::{decode_int, new_int_contexts};
use super::budget_symbol::{MAX_STREAM_SYMBOLS, StreamBudget};
use super::generic_general::decode_generic_region_general;
use super::mq_decode::Decoder;
use super::segment_decode::{Segment, Symbols};
use super::symbol_header::parse_symbol_dict_header;
use crate::bitmap::Bitmap;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

fn bad(s: String) -> Error {
    Error::Malformed(s)
}

/// `decodeSymbolDict`: decodes one symbol dictionary segment and returns the symbols it
/// exports, which is not the same list as the symbols it decodes: a dictionary may re-export
/// symbols it was given as input and may withhold symbols it decoded (T.88 6.5.10).
///
/// `input` is the concatenation of the exported symbols of the dictionaries this one refers to,
/// in the order the header refers to them.
///
/// # Errors
/// [`Error::Malformed`], [`Error::UnsupportedJbig2Feature`], [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn decode_symbol_dict(
    sg: &Segment,
    input: &[Rc<Bitmap>],
    budget: &mut StreamBudget,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Symbols> {
    let h = parse_symbol_dict_header(sg.data)?;
    // Refused a second time, having been refused once from the header walk in `plan_stream`.
    // Not redundant: this function is reachable on its own, and the MQ decoder yields a
    // decision for any input, so a dictionary decoded under the wrong assumption is a page of
    // plausible wrong glyphs rather than an error.
    h.unsupported()?;
    budget.charge_symbols(i64::from(h.num_new))?;

    let mut d = Decoder::new(sg.data.get(h.data_off..).unwrap_or(&[]));
    // One context array for every symbol in the dictionary (T.88 6.5.8.1): the statistics
    // learned on the first glyph are what compress the second.
    let mut gb = vec![0u8; 1 << 16];
    // IADH, IADW and IAEX and no others: the IAAI and IARD* contexts are read only under
    // SDREFAGG, which `unsupported` refused above.
    let mut iadh = new_int_contexts();
    let mut iadw = new_int_contexts();
    let mut iaex = new_int_contexts();

    let num_new = h.num_new as usize;
    let mut new_syms: Symbols = Vec::new();
    new_syms.try_reserve_exact(num_new.min(MAX_STREAM_SYMBOLS as usize))?;
    let mut hc_height: i64 = 0;
    while new_syms.len() < num_new {
        let Some(dh) = decode_int(&mut d, &mut iadh) else {
            return Err(bad(
                "jbig2: symbol dictionary: OOB where a height class delta was expected".to_string(),
            ));
        };
        hc_height += dh;
        if hc_height <= 0 {
            return Err(bad(format!(
                "jbig2: symbol dictionary: height class height is {hc_height}; heights are positive"
            )));
        }
        let (mut sym_width, mut in_class) = (0i64, 0usize);
        // OOB ends the height class (T.88 6.5.5 step 4c ii).
        while let Some(dw) = decode_int(&mut d, &mut iadw) {
            sym_width += dw;
            if sym_width <= 0 {
                return Err(bad(format!(
                    "jbig2: symbol dictionary: symbol width is {sym_width}; widths are positive"
                )));
            }
            if new_syms.len() == num_new {
                return Err(bad(format!(
                    "jbig2: symbol dictionary declares {} new symbols and codes more",
                    h.num_new
                )));
            }
            budget.charge_bitmap(sym_width, hc_height)?;
            check_cancel(cancel)?;
            let b = decode_generic_region_general(
                &mut d,
                &mut gb,
                sym_width as usize,
                hc_height as usize,
                h.template,
                &h.at,
                false,
                limits,
                cancel,
            )?;
            new_syms.push(Rc::new(b));
            in_class += 1;
        }
        // AN EMPTY HEIGHT CLASS IS REFUSED, AND THAT IS WHAT MAKES THIS LOOP PROVABLY
        // TERMINATE. Its only exit is having decoded SDNUMNEWSYMS symbols, and a class that
        // codes none makes no progress towards that. With this check every iteration appends
        // at least one symbol, so the loop runs at most SDNUMNEWSYMS times, a count the header
        // budget already bounds. Without it the iteration count is set by the coded data, which
        // does not run out: the MQ decoder reads 1 bits forever past the end, so what it decodes
        // after the last real byte is a repeating sequence, and a repeating "positive height
        // delta, then OOB" is an infinite loop on an input of under a hundred bytes.
        if in_class == 0 {
            return Err(bad(format!(
                "jbig2: symbol dictionary codes a height class of {hc_height} rows containing no symbols, \
                 having decoded {} of {}",
                new_syms.len(),
                h.num_new
            )));
        }
    }

    export_symbols(&mut d, &mut iaex, input, &new_syms, h.num_ex as usize)
}

/// `exportSymbols`: the export flag procedure of T.88 6.5.10 over the input and newly decoded
/// symbols; returns the ones flagged for export.
///
/// The flags are coded as alternating run lengths starting with a run of not-exported, so a
/// dictionary that exports everything codes two numbers: a run of zero, then a run of
/// everything. A run of zero is legal and common and cannot be treated as the end of the list,
/// but two of them in a row would advance nothing, which is why the loop bounds its own
/// iterations rather than trusting the stream to terminate it.
fn export_symbols(
    d: &mut Decoder,
    iaex: &mut [u8],
    input: &[Rc<Bitmap>],
    decoded: &[Rc<Bitmap>],
    want: usize,
) -> Result<Symbols> {
    let mut all: Symbols = Vec::new();
    all.try_reserve_exact(input.len() + decoded.len())?;
    all.extend(input.iter().cloned());
    all.extend(decoded.iter().cloned());

    let mut out: Symbols = Vec::new();
    out.try_reserve_exact(want.min(all.len()))?;
    let (mut i, mut exporting) = (0usize, false);
    for _ in 0..2 * all.len() + 2 {
        if i >= all.len() {
            break;
        }
        let Some(run) = decode_int(d, iaex) else {
            return Err(bad(
                "jbig2: symbol dictionary: OOB where an export run length was expected".to_string(),
            ));
        };
        if run < 0 || i as i64 + run > all.len() as i64 {
            return Err(bad(format!(
                "jbig2: symbol dictionary: export run of {run} at {i} over {} symbols",
                all.len()
            )));
        }
        let run = run as usize;
        if exporting {
            out.extend(all.get(i..i + run).unwrap_or(&[]).iter().cloned());
        }
        i += run;
        exporting = !exporting;
    }
    if i < all.len() {
        return Err(bad(format!(
            "jbig2: symbol dictionary: export runs cover {i} of {} symbols",
            all.len()
        )));
    }
    // The count is in the header and the flags are in the coded data, so they are two
    // independent statements of the same fact. A disagreement means one of them was
    // mis-parsed, and continuing would hand every text region that refers to this dictionary a
    // symbol list of the wrong length, which renumbers every glyph rather than failing.
    if out.len() != want {
        return Err(bad(format!(
            "jbig2: symbol dictionary declares {want} exported symbols and its export flags select {}",
            out.len()
        )));
    }
    Ok(out)
}
