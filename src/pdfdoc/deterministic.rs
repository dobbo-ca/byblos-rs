//! Deterministic pdf-rs output: port of `internal/pdfdoc/deterministic.go` (byb-c53).
//!
//! pdf-go's writer is nondeterministic three ways: `ensureInfoDict` writes the dates from the
//! clock, `fileID` hashes the clock into `/ID`, and the writer emits referenced objects in map
//! order. [`write_deterministic`] therefore serializes the context itself, with the primitives
//! the Annex F linearizer uses on arbitrary read documents. Objects are renumbered contiguously
//! in ascending source-number order; non-stream objects are packed into one object stream and
//! the trailer is a cross-reference stream. The per-run stamps become pure functions of the
//! input: the dates are the input's own (normalized to the fixed 23-byte form) or
//! [`PINNED_DATE`], and `/ID` keeps the input's first element and derives the second from an
//! MD5 of the emitted body.
//!
//! FAIL OPEN. Anything this writer cannot hold safely (an `/Encrypt` dictionary, a catalog-less
//! table, an object that does not reach the catalog or the info dictionary) falls back to
//! pdf-rs's `write_context`: nondeterministic bytes, but never a corrupted document.
//!
//! # Differences from Go
//!
//! * The clock and random source of the fallback are the caller's [`WriteEnv`]; Go reads
//!   `time.Now`.
//! * Go's `false` for any internal error is `Ok(None)` here, except for [`Error::Alloc`],
//!   [`Error::OutOfMemory`] and [`Error::Cancelled`], which propagate (a fallback would only
//!   run into the same limit).
//! * `auditSerializable` is not ported (see `linearize.rs`).
//! * Every buffer is charged to the context's ledger (`max_alloc`).

use super::linearize::{
    live_objects, load_lazy, reachable, renumber_object, serialize, stream_body, trailer_id,
};
use super::selfwrite::{BINARY_MARKER, Out, PRODUCER};
use crate::error::{Error, Result};
use crate::limits::check_cancel;
use crate::linearize::malformed;
use pdf::crypto::md5;
use pdf::filter::goflate::ZlibWriter;
use pdf::model::{Context, XRefTable};
use pdf::object::date::{date_string, date_time};
use pdf::object::text::{hex_literal_to_string, string_literal_to_string};
use pdf::object::{Dict, Object, Ref};
use pdf::{Held, Mem, WriteEnv};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// `pinnedDate`: the CreationDate/ModDate written when the input had none of its own. The value
/// is arbitrary but load-bearing: changing it changes every produced document's bytes.
pub const PINNED_DATE: &str = "D:20000101000000+00'00'";

/// `writeDeterministic`: serializes `ctx` with deterministic bytes, falling back to pdf-rs's own
/// (nondeterministic) writer when it cannot hold the pen safely.
///
/// # Errors
/// The errors of `pdf::api::write_context` on the fallback path; [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`]; [`Error::Malformed`] for a document with no
/// readable version.
pub fn write_deterministic(
    ctx: &mut Context,
    env: &WriteEnv,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if let Some(b) = deterministic_bytes(ctx, cancel)? {
        return Ok(b);
    }
    // Go's `catchPanic` makes a pdf-go panic in the write `ErrMalformed`.
    pdf::api::write_context(ctx, env, cancel).map_err(|e| match super::doc::seam("write", e) {
        Ok(p) => Error::Pdf(p),
        Err(o) => o,
    })
}

/// `infoDates`: the Info dictionary's CreationDate and ModDate as raw date strings, empty for
/// absent or unreadable.
fn info_dates(xt: &mut XRefTable) -> (String, String) {
    let Some(ir) = xt.info else {
        return (String::new(), String::new());
    };
    let (creation, mod_date) = match xt.dereference_dict_ref(ir) {
        Ok(Some(d)) => (
            d.0.get(&b"CreationDate"[..]).cloned(),
            d.0.get(&b"ModDate"[..]).cloned(),
        ),
        _ => return (String::new(), String::new()),
    };
    (raw_date(xt, creation), raw_date(xt, mod_date))
}

/// `rawDate`: the text of a string or hex string `o` (through a reference), empty otherwise.
fn raw_date(xt: &mut XRefTable, o: Option<Object>) -> String {
    let Some(o) = o else {
        return String::new();
    };
    match xt.dereference(&o) {
        Ok(Object::String(s)) => string_literal_to_string(s).unwrap_or_default(),
        Ok(Object::Hex(h)) => hex_literal_to_string(h).unwrap_or_default(),
        _ => String::new(),
    }
}

/// `pinDate`: normalizes `raw` to the fixed 23-byte `D:YYYYMMDDHHMMSS+HH'MM'` form (any valid
/// PDF date has one, and it preserves the date's own UTC offset), falling back to
/// [`PINNED_DATE`] when `raw` is empty, unparseable, or, for out-of-range years, would not
/// render at that length.
pub fn pin_date(raw: &str) -> String {
    if !raw.is_empty()
        && let Some(t) = date_time(raw.as_bytes(), true)
    {
        let s = date_string(&t);
        if s.len() == PINNED_DATE.len() {
            return s;
        }
    }
    PINNED_DATE.to_string()
}

/// `pinInfo`: rewrites the Info dictionary in place to its deterministic form: dates pinned,
/// `/Producer` naming what actually writes the bytes. The Info object is created when the
/// document has none, as pdf-go's own writer would have. Idempotent: pinned dates normalize to
/// themselves, so a second write emits the same bytes.
///
/// # Errors
/// A pdf-rs error for an Info entry that is not a dictionary or cannot be read.
fn pin_info(xt: &mut XRefTable) -> std::result::Result<(), pdf::Error> {
    let (creation, mod_date) = info_dates(xt);
    let stamp = |d: &mut Dict| {
        d.set(b"CreationDate", Object::string(pin_date(&creation)));
        d.set(b"ModDate", Object::string(pin_date(&mod_date)));
        d.set(b"Producer", Object::string(PRODUCER));
    };
    match xt.info {
        Some(ir) => match xt.dereference_dict_mut(ir)? {
            Some(d) => stamp(d),
            None => return Err(pdf::Error::Corrupt("info dictionary: <nil>".to_string())),
        },
        None => {
            let mut d = Dict::default();
            stamp(&mut d);
            xt.info = Some(xt.ind_ref_for_new_object(Object::Dict(d))?);
        }
    }
    Ok(())
}

/// `flateRaw`: FlateDecode's encode side, zlib at the default level; deterministic for a given
/// input.
///
/// # Errors
/// [`Error::Alloc`], [`Error::OutOfMemory`], [`Error::Cancelled`].
pub(crate) fn flate_raw(mem: &Mem, b: &[u8], cancel: &AtomicBool) -> Result<(Vec<u8>, Held)> {
    let mut held = mem.charge(0)?;
    let mut out: Vec<u8> = Vec::new();
    let mut zw = ZlibWriter::new(mem)?;
    let mut sink = |c: &[u8]| -> std::result::Result<(), pdf::Error> {
        held.grow_by(c.len() as u64)?;
        out.try_reserve(c.len())?;
        out.extend_from_slice(c);
        Ok(())
    };
    zw.write(b, &mut sink, cancel)?;
    zw.close(&mut sink)?;
    Ok((out, held))
}

/// Upper-case hex of `b`, as `strings.ToUpper(hex.EncodeToString(b))`.
fn upper_hex(b: &[u8]) -> Vec<u8> {
    b.iter()
        .flat_map(|x| format!("{x:02X}").into_bytes())
        .collect()
}

/// Where an object of the output lives: a top-level stream, or packed in the object stream.
enum Slot {
    Top(usize),
    Packed(usize),
}

/// A top-level object: its new number, its body and (once written) its offset.
struct Top {
    nr: i64,
    body: Vec<u8>,
    offset: usize,
}

/// One 7-byte entry of the cross-reference stream (`W [1 4 2]`): the low 32 bits of `mid`.
fn add_entry(entries: &mut Vec<u8>, kind: u8, mid: u64, low: u16) {
    entries.push(kind);
    entries.extend_from_slice(&(mid as u32).to_be_bytes());
    entries.extend_from_slice(&low.to_be_bytes());
}

/// `Ok(None)` for what Go reports as `false`, but not for the errors a fallback would repeat.
fn soft<T>(e: Error) -> Result<Option<T>> {
    match e {
        Error::Alloc { .. } | Error::OutOfMemory | Error::Cancelled => Err(e),
        _ => Ok(None),
    }
}

/// `deterministicBytes`: `ctx` as a complete PDF whose bytes are a pure function of the
/// context's content, or `None` when pdf-rs's own writer must hold the pen instead.
///
/// The layout matches what pdf-go's writer would have used in spirit (every non-stream object
/// packed into one object stream, a cross-reference stream as the trailer), because dropping
/// that compression costs real size. Only the ORDER is different.
///
/// ponytail: the whole output is built in memory, like the design it replaces; stream one
/// object at a time (`selfwrite.rs`'s shape) if a write path ever outgrows the callers that
/// already hold input and output in memory.
fn deterministic_bytes(ctx: &mut Context, cancel: &AtomicBool) -> Result<Option<Vec<u8>>> {
    let xt = &mut ctx.xref;
    if xt.encrypt.is_some() || xt.root.is_none() {
        return Ok(None);
    }
    if pin_info(xt).is_err() {
        return Ok(None);
    }
    if let Err(e) = load_lazy(xt, cancel) {
        return soft(e);
    }
    let (Some(root), Some(info)) = (xt.root, xt.info) else {
        return Ok(None);
    };
    let mem = xt.mem().clone();
    let live = live_objects(xt);
    let keep = reachable(&live, &[root.num, info.num]);
    if !keep.contains(&root.num) || !keep.contains(&info.num) {
        return Ok(None);
    }
    let olds: Vec<i64> = keep.iter().copied().collect();
    let renumber: BTreeMap<i64, i64> = olds
        .iter()
        .enumerate()
        .map(|(i, &old)| (old, i as i64 + 1))
        .collect();

    // Serialize every object, split by where it can live: a stream object must be a top-level
    // object, everything else goes into the object stream (ISO 32000-1 7.5.7).
    let mut held = mem.charge(0)?;
    let mut tops: Vec<Top> = Vec::new();
    let mut packed: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    for (i, old) in olds.iter().enumerate() {
        check_cancel(cancel)?;
        let Some(&o) = live.get(old) else {
            return Ok(None);
        };
        let body = match serialize(&mut held, &renumber_object(o, &renumber)) {
            Ok(b) => b,
            Err(e) => return soft(e),
        };
        let nr = i as i64 + 1;
        if matches!(o, Object::Stream(_)) {
            slots.push(Slot::Top(tops.len()));
            tops.push(Top {
                nr,
                body,
                offset: 0,
            });
        } else {
            slots.push(Slot::Packed(packed.len()));
            packed.push((nr, body));
        }
    }
    if packed.len() > 0xFFFF {
        return Ok(None); // the xref stream's object-stream index field is 2 bytes
    }
    let objstm_nr = olds.len() as i64 + 1;
    let xref_nr = objstm_nr + 1;

    let mut buf = Out::new(&mem)?;
    let version = xt
        .version_string()
        .map_err(|e| malformed(format!("byblos/pdfdoc: write: {e}")))?;
    // Cross-reference and object streams are PDF 1.5 features; a 1.0-1.4 input's content is
    // untouched, but the file that now carries it must declare a version a reader will accept
    // them under.
    let version = if version.is_empty() || version.as_str() < "1.5" {
        "1.5"
    } else {
        version.as_str()
    };
    buf.push(format!("%PDF-{version}\n").as_bytes())?;
    buf.push(BINARY_MARKER)?;

    // The object stream: "nr offset" pairs, then the bodies (7.5.7). Never empty: the catalog
    // and the Info dictionary are always packable.
    let objstm_offset = buf.len();
    let mut plain = Out::new(&mem)?;
    let mut off = 0usize;
    for (nr, body) in &packed {
        plain.push(format!("{nr} {off} ").as_bytes())?;
        off += body.len() + 1;
    }
    let first = plain.len();
    for (_, body) in &packed {
        plain.push(body)?;
        plain.push(b"\n")?;
    }
    let mut d = Dict::default();
    d.insert_name(b"Type", b"ObjStm");
    d.insert_int(b"N", packed.len() as i64);
    d.insert_int(b"First", first as i64);
    d.insert_name(b"Filter", b"FlateDecode");
    let (raw, _raw_held) = flate_raw(&mem, plain.as_slice(), cancel)?;
    drop(plain);
    let body = match stream_body(&mut held, &d, &raw) {
        Ok(b) => b,
        Err(e) => return soft(e),
    };
    drop(raw);
    buf.push(format!("{objstm_nr} 0 obj\n").as_bytes())?;
    buf.push(&body)?;
    buf.push(b"\nendobj\n")?;
    for top in &mut tops {
        check_cancel(cancel)?;
        top.offset = buf.len();
        buf.push(format!("{} 0 obj\n", top.nr).as_bytes())?;
        buf.push(&top.body)?;
        buf.push(b"\nendobj\n")?;
    }
    let start = buf.len();
    if start > 0xFFFF_FFFF {
        return Ok(None); // the xref stream's offset field below is 4 bytes
    }

    // /ID: a deterministic fingerprint. Element two is the md5 of everything emitted so far;
    // element one is the input's own first element when it carried an /ID (unchanged identity),
    // else the same digest.
    let fresh = upper_hex(&md5(&[buf.as_slice()]));
    let mut el0 = fresh.clone();
    if xt.id.is_some()
        && let Some(b) = &trailer_id(xt)[0]
    {
        el0 = upper_hex(b);
    }

    // The cross-reference stream (7.5.8): W [1 4 2], one contiguous subsection covering object
    // 0 through the xref stream itself.
    let size = xref_nr + 1;
    let entry_bytes = usize::try_from(size)
        .ok()
        .and_then(|s| s.checked_mul(7))
        .ok_or(Error::OutOfMemory)?;
    let _entries_held = mem.charge(entry_bytes as u64)?;
    let mut entries: Vec<u8> = Vec::new();
    entries.try_reserve_exact(entry_bytes)?;
    add_entry(&mut entries, 0, 0, 0xFFFF); // object 0: head of an empty free list
    for slot in &slots {
        match slot {
            Slot::Top(t) => {
                let off = tops.get(*t).map_or(0, |t| t.offset);
                add_entry(&mut entries, 1, off as u64, 0);
            }
            Slot::Packed(p) => add_entry(&mut entries, 2, objstm_nr as u64, *p as u16),
        }
    }
    add_entry(&mut entries, 1, objstm_offset as u64, 0); // the object stream
    add_entry(&mut entries, 1, start as u64, 0); // the xref stream itself
    let (xraw, _xraw_held) = flate_raw(&mem, &entries, cancel)?;
    let r = |old: i64| {
        Object::Ref(Ref {
            num: renumber.get(&old).copied().unwrap_or(0),
            generation: 0,
        })
    };
    let mut xd = Dict::default();
    xd.insert_name(b"Type", b"XRef");
    xd.insert_int(b"Size", size);
    xd.set(
        b"W",
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(4),
            Object::Integer(2),
        ]),
    );
    xd.set(
        b"Index",
        Object::Array(vec![Object::Integer(0), Object::Integer(size)]),
    );
    xd.insert_name(b"Filter", b"FlateDecode");
    xd.set(b"Root", r(root.num));
    xd.set(b"Info", r(info.num));
    xd.set(
        b"ID",
        Object::Array(vec![Object::Hex(el0), Object::Hex(fresh)]),
    );
    let xbody = match stream_body(&mut held, &xd, &xraw) {
        Ok(b) => b,
        Err(e) => return soft(e),
    };
    buf.push(format!("{xref_nr} 0 obj\n").as_bytes())?;
    buf.push(&xbody)?;
    buf.push(format!("\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes())?;
    Ok(Some(buf.into_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TestPinDateFallsBackToTheConstant`: every input yields a fixed-length date.
    #[test]
    fn pin_date_falls_back_to_the_constant() {
        for (input, want) in [
            ("", PINNED_DATE),
            ("garbage", PINNED_DATE),
            ("D:20190102030405+01'00'", "D:20190102030405+01'00'"), // already normalized
            ("D:20190102030405Z", "D:20190102030405+00'00'"),       // offset preserved as UTC
            ("D:2019", "D:20190101000000+00'00'"), // partial dates are legal (7.9.4)
        ] {
            let got = pin_date(input);
            assert_eq!(got, want, "pin_date({input:?})");
            assert_eq!(got.len(), PINNED_DATE.len(), "pin_date({input:?})");
        }
    }

    #[test]
    fn upper_hex_is_upper_case() {
        assert_eq!(upper_hex(&[0x00, 0xab, 0xFF]), b"00ABFF".to_vec());
        assert!(upper_hex(&[]).is_empty());
    }

    #[test]
    fn xref_entry_is_seven_bytes_big_endian() {
        let mut e = Vec::new();
        add_entry(&mut e, 2, 0x0102_0304, 0x0506);
        assert_eq!(e, [2, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn flate_raw_is_a_zlib_stream() {
        let mem = Mem::with_max(64 << 20);
        let cancel = AtomicBool::new(false);
        let (out, _held) = flate_raw(&mem, b"hello", &cancel).unwrap();
        // The header, a stored block, the end of the stream and the Adler-32 trailer.
        assert_eq!(out[..2], [0x78, 0x9c]);
        assert_eq!(out[out.len() - 4..], [0x06, 0x2c, 0x02, 0x15]);
    }

    /// `fixturePDF`: a one-page document with an Info dictionary carrying the given dates (none
    /// when `info` is `None`) and a trailer `/ID` with `id` as both elements when it is not
    /// empty.
    fn fixture(info: Option<(&str, &str)>, id: &str) -> Vec<u8> {
        let mut bodies = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << >> >>"
                .to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
        ];
        if let Some((creation, modified)) = info {
            bodies.push(format!(
                "<< /CreationDate ({creation}) /ModDate ({modified}) >>"
            ));
        }
        let mut f = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, b) in bodies.iter().enumerate() {
            offs.push(f.len());
            f.extend_from_slice(format!("{} 0 obj\n{b}\nendobj\n", i + 1).as_bytes());
        }
        let start = f.len();
        f.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offs.len() + 1).as_bytes(),
        );
        for off in &offs {
            f.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        let mut trailer = format!("trailer\n<< /Size {} /Root 1 0 R", offs.len() + 1);
        if info.is_some() {
            trailer.push_str(&format!(" /Info {} 0 R", offs.len()));
        }
        if !id.is_empty() {
            trailer.push_str(&format!(" /ID [<{id}> <{id}>]"));
        }
        trailer.push_str(&format!(" >>\nstartxref\n{start}\n%%EOF\n"));
        f.extend_from_slice(trailer.as_bytes());
        f
    }

    fn read(data: &[u8]) -> Context {
        pdf::api::read(data, &pdf::Config::default(), &AtomicBool::new(false)).unwrap()
    }

    fn write(data: &[u8]) -> Vec<u8> {
        let mut ctx = read(data);
        write_deterministic(&mut ctx, &WriteEnv::parity(), &AtomicBool::new(false)).unwrap()
    }

    /// Two writes of one document are the same bytes, the output reads back, and it is a
    /// PDF 1.5 file (object and cross-reference streams).
    #[test]
    fn write_is_repeatable_and_reads_back() {
        let f = fixture(None, "");
        let a = write(&f);
        assert_eq!(a, write(&f));
        assert!(a.starts_with(b"%PDF-1.5\n"));
        let mut ctx = read(&a);
        ctx.xref.ensure_page_count().unwrap();
        assert_eq!(ctx.xref.page_count, 1);
    }

    /// `TestWritePinsMissingDatesToTheConstant`: no Info dictionary, so both dates are the
    /// constant, not the second the test ran in.
    #[test]
    fn write_pins_missing_dates_to_the_constant() {
        let mut ctx = read(&write(&fixture(None, "")));
        let (creation, modified) = info_dates(&mut ctx.xref);
        assert_eq!(
            (creation.as_str(), modified.as_str()),
            (PINNED_DATE, PINNED_DATE)
        );
    }

    /// `TestWriteKeepsTheInputDocumentsDates`.
    #[test]
    fn write_keeps_the_input_documents_dates() {
        let f = fixture(
            Some(("D:20190102030405+01'00'", "D:20200607080910-05'00'")),
            "",
        );
        let mut ctx = read(&write(&f));
        let (creation, modified) = info_dates(&mut ctx.xref);
        assert_eq!(creation, "D:20190102030405+01'00'");
        assert_eq!(modified, "D:20200607080910-05'00'");
    }

    /// `TestWriteKeepsTheInputDocumentsIDElementZero`: ISO 32000-1 14.4 makes element 0 the
    /// document's permanent identity; only element 1 changes.
    #[test]
    fn write_keeps_the_input_documents_id_element_zero() {
        const ID: &str = "4DB48EDAE40A66E4C5E4F2328B8EB183";
        let f = fixture(
            Some(("D:20190102030405+01'00'", "D:20190102030405+01'00'")),
            ID,
        );
        let ctx = read(&write(&f));
        let [id0, id1] = trailer_id(&ctx.xref);
        assert_eq!(id0.map(|b| upper_hex(&b)), Some(ID.as_bytes().to_vec()));
        assert_ne!(id1.map(|b| upper_hex(&b)), Some(ID.as_bytes().to_vec()));
    }
}
