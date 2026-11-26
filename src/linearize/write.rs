//! Port of byblos `internal/linearize/write.go`: assembling the file (ISO 32000-1:2008
//! Annex F.3).
//!
//! THE CHICKEN AND EGG. The linearization parameter dictionary states the total file length,
//! the offset and length of the hint stream, and the offset of the end of the first-page
//! section, none of which are known until the file has been laid out, and the dictionary is
//! itself part of the layout. Both cross-reference sections have the same problem.
//!
//! The reference implementation resolves it by emitting the whole file twice. Byblos emits it
//! once, because it emits classic cross-reference tables and no object streams, and that
//! removes every source of pass-to-pass drift qpdf has to absorb. What is left is:
//!
//! * Every unknown value sits in a slot whose WIDTH is known before the first byte is written:
//!   20 bytes per cross-reference entry (7.5.4), 201 bytes for the parameter dictionary, 21
//!   for the front trailer's `/Prev`, and a `/Size` that is known from the plan. So
//!   back-patching never changes a byte count.
//! * The hint stream is left out of the layout entirely and spliced in afterwards at a
//!   recorded offset, and every offset at or past that point is corrected by exactly its
//!   length. That is sound because Annex F.4 defines every offset STORED IN A HINT TABLE as
//!   measured with the hint stream absent, so the hint tables are computed from the pre-splice
//!   layout and the splice does not invalidate them.
//!
//! There is therefore no fixed point to iterate to: the finished length is the pre-splice
//! length plus the hint stream's length, and that is the value written into `/L`.
//!
//! The hint stream is deflated with Go's zlib at level 6 (`pdf::filter::goflate::ZlibWriter`).

use super::hints::{Hints, OutlineHint, PageHint, SharedHint, encode_hints};
use super::layout::Plan;
use super::malformed;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use pdf::Mem;
use pdf::filter::goflate::ZlibWriter;
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::AtomicBool;

/// `linDictSlot` is the fixed size of part 2, copied from the reference implementation: 200
/// bytes of content padded with spaces, then a newline. Measured on two independently produced
/// linearized files, the parameter dictionary starts at offset 15 and the next part starts at
/// 216.
const LIN_DICT_SLOT: usize = 201;

/// `prevSlot` is the fixed width of the front trailer's `/Prev` value.
const PREV_SLOT: usize = 21;

/// `xrefEntry` is the size ISO 32000-1 7.5.4 fixes for a cross-reference entry: exactly 20
/// bytes, including the two-character end-of-line.
const XREF_ENTRY: usize = 20;

/// An empty cross-reference entry, patched later.
const EMPTY_ENTRY: &[u8; XREF_ENTRY] = b"0000000000 00000 n \n";

/// `Meta`: what the assembled file needs that is not structural.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Meta {
    /// For example `"1.5"`; the `%PDF-` header. `"1.4"` when empty.
    pub version: String,
    /// The trailer `/ID`; omitted when the first half is empty.
    pub id: [Vec<u8>; 2],
}

/// The file under construction and where each object landed.
struct Layout<'a> {
    buf: Vec<u8>,
    starts: BTreeMap<i64, i64>,
    ends: BTreeMap<i64, i64>,
    bodies: &'a BTreeMap<i64, Vec<u8>>,
    cancel: &'a AtomicBool,
}

impl Layout<'_> {
    fn pos(&self) -> i64 {
        self.buf.len() as i64
    }

    fn push(&mut self, b: &[u8]) -> Result<()> {
        self.buf.try_reserve(b.len())?;
        self.buf.extend_from_slice(b);
        Ok(())
    }

    /// `emit`: `N 0 obj`, the body, `endobj`.
    fn emit(&mut self, n: i64) -> Result<()> {
        check_cancel(self.cancel)?;
        let bodies = self.bodies;
        let Some(body) = bodies.get(&n) else {
            return Err(malformed(format!("linearize: no body for object {n}")));
        };
        self.starts.insert(n, self.pos());
        self.push(format!("{n} 0 obj\n").as_bytes())?;
        self.push(body)?;
        self.push(b"\nendobj\n")?;
        self.ends.insert(n, self.pos());
        Ok(())
    }

    fn emit_all(&mut self, ns: &[i64]) -> Result<()> {
        for &n in ns {
            self.emit(n)?;
        }
        Ok(())
    }
}

/// `Write`: assembles the linearized file and writes it to `w`.
///
/// `bodies` is keyed by NEW object number and holds each object's serialized bytes WITHOUT the
/// `N 0 obj` / `endobj` wrapper, with every reference already rewritten to new numbers. A
/// stream's body is its dictionary, then `"\nstream\n"`, then the raw payload, then
/// `"\nendstream"`: the shape pdf-go's own writer produces, so an object that round-trips
/// through this package is byte-identical to one that did not.
///
/// The file is built in one buffer, charged to `limits.max_alloc` (the bodies plus the
/// cross-reference tables) before it is allocated; `cancel` is checked per object.
///
/// # Errors
/// [`Error::Malformed`] for an empty first-page section, a first object group that is not
/// contiguous, an object with no body, a parameter dictionary or `/Prev` that does not fit its
/// slot, or a page with no objects of its own; [`Error::Alloc`]; [`Error::Cancelled`]; the write
/// error of `w`.
pub fn write<W: io::Write>(
    w: &mut W,
    p: &Plan,
    bodies: &BTreeMap<i64, Vec<u8>>,
    m: &Meta,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<()> {
    let Some(&first_page_obj) = p.part6.first() else {
        return Err(malformed("linearize: the first-page section is empty"));
    };

    if p.lin_dict < 1 {
        return Err(malformed("linearize: the plan has no parameter dictionary"));
    }
    let second_group = p.lin_dict - 1;
    let mem = limits.mem();
    let body_bytes = bodies
        .values()
        .fold(0u64, |a, b| a.saturating_add(b.len() as u64));
    let est = body_bytes
        .saturating_add((bodies.len() as u64).saturating_mul(64))
        .saturating_add(((p.part4.len() + p.part6.len() + 2) as u64).saturating_mul(20))
        .saturating_add(u64::try_from(second_group).unwrap_or(0).saturating_mul(20))
        .saturating_add(4096);
    let _held = mem.charge(est)?;

    let mut lay = Layout {
        buf: Vec::new(),
        starts: BTreeMap::new(),
        ends: BTreeMap::new(),
        bodies,
        cancel,
    };
    lay.buf
        .try_reserve_exact(usize::try_from(est).map_err(|_| Error::OutOfMemory)?)?;

    // Part 1. The four bytes above 127 are what tells a transfer agent the file is binary
    // (7.5.2).
    let version = if m.version.is_empty() {
        "1.4"
    } else {
        &m.version
    };
    lay.push(format!("%PDF-{version}\n").as_bytes())?;
    lay.push(b"%\xE2\xE3\xCF\xD3\n")?;

    // Part 2, reserved and filled in at the end. It has no body in `bodies` (it is this
    // function's own object), so its offset is recorded by hand.
    let lin_off = lay.pos();
    lay.push(&[b' '; LIN_DICT_SLOT])?;
    lay.starts.insert(p.lin_dict, lin_off);

    // Part 3. F.3.4: one subsection, no free entries, covering exactly the first-group
    // objects, which are one ascending run by construction.
    let mut first_group = vec![p.lin_dict];
    first_group.extend_from_slice(&p.part4);
    first_group.push(p.hint);
    first_group.extend_from_slice(&p.part6);
    for (i, &n) in first_group.iter().enumerate() {
        if p.lin_dict.checked_add(i as i64) != Some(n) {
            return Err(malformed(format!(
                "linearize: the first object group is not contiguous: object {n} is at index {i} of a run starting at {}",
                p.lin_dict
            )));
        }
    }
    let xref3_off = lay.pos();
    lay.push(format!("xref\n{} {}\n", p.lin_dict, first_group.len()).as_bytes())?;
    let mut front_entry = Vec::with_capacity(first_group.len());
    for _ in &first_group {
        front_entry.push(lay.pos());
        lay.push(EMPTY_ENTRY)?;
    }
    let id = id_literal(&m.id);
    let mut trailer = format!("trailer\n<< /Size {} /Root {} 0 R", p.size, p.catalog);
    if p.info != 0 {
        trailer.push_str(&format!(" /Info {} 0 R", p.info));
    }
    trailer.push_str(" /Prev ");
    lay.push(trailer.as_bytes())?;
    let prev_pos = lay.pos();
    lay.push(&[b' '; PREV_SLOT])?;
    if !id.is_empty() {
        lay.push(b" /ID ")?;
        lay.push(id.as_bytes())?;
    }
    // F.3.4 makes this startxref optional and says its value shall be ignored; the reference
    // implementation writes a literal 0 and so does this.
    lay.push(b" >>\nstartxref\n0\n%%EOF\n")?;

    // Part 4.
    lay.emit_all(&p.part4)?;

    // Part 5 goes here, and is spliced in later. Everything from this offset on moves by the
    // hint stream's length.
    let hint_off = lay.pos();

    // Part 6.
    lay.emit_all(&p.part6)?;
    let end_first_page = lay.pos();

    // Parts 7, 8 and 9.
    for grp in &p.part7 {
        lay.emit_all(grp)?;
    }
    lay.emit_all(&p.part8)?;
    lay.emit_all(&p.part9)?;

    // Part 11. F.3.11: one subsection beginning at object 0, covering the second object group
    // only, and a trailer with no /Prev. A reader that knows nothing about linearization sees
    // an ordinary incremental update.
    let xref_main_off = lay.pos();
    lay.push(format!("xref\n0 {}", second_group + 1).as_bytes())?;
    // Table F.1 defines /T for a classic table as the offset of the white-space character
    // preceding the entry for object 0, which is this byte, not the offset of the "xref"
    // line.
    let space_before_zero = lay.pos();
    lay.push(b"\n")?;
    lay.push(b"0000000000 65535 f \n")?;
    let mut main_entry = Vec::new();
    for _ in 0..second_group {
        main_entry.push(lay.pos());
        lay.push(EMPTY_ENTRY)?;
    }
    let mut trailer2 = format!("trailer\n<< /Size {}", second_group + 1);
    if !id.is_empty() {
        trailer2.push_str(" /ID ");
        trailer2.push_str(&id);
    }
    trailer2.push_str(&format!(" >>\nstartxref\n{xref3_off}\n"));
    lay.push(trailer2.as_bytes())?;
    lay.push(b"%%EOF\n")?;
    let pre_splice_len = lay.pos();

    // The hint tables, computed from the layout as it stands (that is, with the hint stream
    // absent), which is exactly how Annex F.4 defines every offset they store.
    let hint_obj = build_hint(
        p,
        &lay.starts,
        &lay.ends,
        hint_off,
        end_first_page,
        &mem,
        cancel,
    )?;
    let hint_len = hint_obj.len() as i64;

    // Splice the hint stream in at its offset: a single buffer instead of Go's second copy.
    let Layout { buf, starts, .. } = lay;
    let mut out = buf;
    out.try_reserve(hint_obj.len())?;
    let at = usize::try_from(hint_off).map_err(|_| Error::OutOfMemory)?;
    drop(out.splice(at..at, hint_obj));

    // Every offset at or past the splice point moves by the hint stream's length. The hint
    // stream itself lands exactly on the splice point.
    let shift = |off: i64| if off >= hint_off { off + hint_len } else { off };
    let start_of = |n: i64| starts.get(&n).copied().unwrap_or(0);

    let lin = format!(
        "{} 0 obj\n<< /Linearized 1 /L {} /H [ {} {} ] /O {} /E {} /N {} /T {} >>\nendobj\n",
        p.lin_dict,
        pre_splice_len + hint_len,
        hint_off,
        hint_len,
        first_page_obj,
        shift(end_first_page),
        p.n_pages,
        shift(space_before_zero)
    );
    if lin.len() > LIN_DICT_SLOT - 1 {
        return Err(malformed(format!(
            "linearize: the parameter dictionary is {} bytes and the reserved slot is {}",
            lin.len(),
            LIN_DICT_SLOT - 1
        )));
    }
    put(&mut out, lin_off, lin.as_bytes())?;
    put(&mut out, lin_off + LIN_DICT_SLOT as i64 - 1, b"\n")?;

    for (i, &n) in first_group.iter().enumerate() {
        let off = if n != p.hint {
            shift(start_of(n))
        } else {
            hint_off
        };
        put_offset(&mut out, front_entry.get(i).copied().unwrap_or(0), off)?;
    }
    let prev = format!("{}", shift(xref_main_off));
    if prev.len() > PREV_SLOT {
        return Err(malformed(format!(
            "linearize: /Prev is {} bytes and the reserved slot is {}",
            prev.len(),
            PREV_SLOT
        )));
    }
    put(&mut out, prev_pos, prev.as_bytes())?;

    for (i, &pos) in main_entry.iter().enumerate() {
        let n = i as i64 + 1;
        put_offset(&mut out, pos + hint_len, shift(start_of(n)))?;
    }

    w.write_all(&out).map_err(|e| malformed(e.to_string()))
}

/// `buildHint`: encodes the primary hint stream and wraps it as an indirect object.
fn build_hint(
    p: &Plan,
    starts: &BTreeMap<i64, i64>,
    ends: &BTreeMap<i64, i64>,
    hint_off: i64,
    end_first_page: i64,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    // An object that was never emitted reads as 0, as a Go map lookup does.
    let start = |n: i64| starts.get(&n).copied().unwrap_or(0);
    let end = |n: i64| ends.get(&n).copied().unwrap_or(0);

    let mut h = Hints {
        first_page_offset: hint_off,
        n_shared_first_page: p.part6.len() as i64,
        ..Hints::default()
    };
    h.pages.push(PageHint {
        n_objects: p.part6.len() as i64,
        length: end_first_page - hint_off,
        shared_ids: Vec::new(),
    });
    for (i, grp) in p.part7.iter().enumerate() {
        let (Some(&first), Some(&last)) = (grp.first(), grp.last()) else {
            return Err(malformed(format!(
                "linearize: page {} has no objects of its own",
                i + 2
            )));
        };
        h.pages.push(PageHint {
            n_objects: grp.len() as i64,
            length: end(last) - start(first),
            shared_ids: p.page_shared.get(i + 1).cloned().unwrap_or_default(),
        });
    }
    for &n in p.part6.iter().chain(&p.part8) {
        h.shared.push(SharedHint {
            group_length: end(n) - start(n),
        });
    }
    if let Some(&first_shared) = p.part8.first() {
        h.first_shared_obj = first_shared;
        h.first_shared_offset = start(first_shared);
    }
    if let Some(&root) = p.outlines.first() {
        // The group's length is measured from the /Outlines root to the far end of the whole
        // group, which is how the reference implementation recomputes it (maxEnd over the
        // outline object set, minus the root's offset). The objects are contiguous, so that
        // is the last one's end.
        let far = p.outlines.iter().map(|&n| end(n)).fold(0, i64::max);
        h.outline = Some(OutlineHint {
            first_object: root,
            first_object_offset: start(root),
            n_objects: p.outlines.len() as i64,
            group_length: far - start(root),
        });
    }

    let hint = encode_hints(&h)?;
    let ctx = |e: Error| super::wrap(e, "linearize: deflating the hint stream");
    let mut z: Vec<u8> = Vec::new();
    let mut zw = ZlibWriter::new(mem).map_err(Error::from).map_err(ctx)?;
    zw.write(
        &hint.payload,
        &mut |b: &[u8]| {
            z.try_reserve(b.len())?;
            z.extend_from_slice(b);
            Ok(())
        },
        cancel,
    )
    .map_err(Error::from)
    .map_err(ctx)?;
    zw.close(&mut |b: &[u8]| {
        z.try_reserve(b.len())?;
        z.extend_from_slice(b);
        Ok(())
    })
    .map_err(Error::from)
    .map_err(ctx)?;

    // F.3.6: every value in the hint stream dictionary is direct, including /Length.
    let mut dict = format!("<< /Filter /FlateDecode /S {}", hint.s);
    if hint.o != 0 {
        dict.push_str(&format!(" /O {}", hint.o));
    }
    dict.push_str(&format!(" /Length {} >>", z.len()));

    let mut obj = format!("{} 0 obj\n{dict}\nstream\n", p.hint).into_bytes();
    obj.try_reserve(z.len() + 32)?;
    obj.extend_from_slice(&z);
    obj.extend_from_slice(b"\nendstream\nendobj\n");
    Ok(obj)
}

/// Overwrites `bytes.len()` bytes of `out` at `pos`.
fn put(out: &mut [u8], pos: i64, bytes: &[u8]) -> Result<()> {
    let start = usize::try_from(pos).map_err(|_| malformed("linearize: patch offset"))?;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| malformed("linearize: patch offset"))?;
    out.get_mut(start..end)
        .ok_or_else(|| malformed("linearize: patch past the end of the file"))?
        .copy_from_slice(bytes);
    Ok(())
}

/// `putOffset`: writes a cross-reference entry's ten-digit offset field in place. The field is
/// fixed width, so this cannot change any other offset.
fn put_offset(out: &mut [u8], pos: i64, off: i64) -> Result<()> {
    put(out, pos, format!("{off:010}").as_bytes())
}

/// `idLiteral`: `[<first><second>]` in hex, the second half defaulting to the first; empty when
/// there is no first half.
fn id_literal(id: &[Vec<u8>; 2]) -> String {
    let [first, second] = id;
    if first.is_empty() {
        return String::new();
    }
    let second = if second.is_empty() { first } else { second };
    format!("[<{}><{}>]", hex_encode(first), hex_encode(second))
}

/// `hex.EncodeToString`.
fn hex_encode(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len().saturating_mul(2));
    for &x in b {
        s.push(char::from(DIGITS[usize::from(x >> 4)]));
        s.push(char::from(DIGITS[usize::from(x & 15)]));
    }
    s
}
