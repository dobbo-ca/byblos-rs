//! byblos's own serializer for a context this package built itself: port of
//! `internal/pdfdoc/selfwrite.go` (`writeDocument`, `auditTable`; byb-yul.6), the replacement of
//! `api.WriteContext` for `BuildFromPages`.
//!
//! The table already holds exactly the write set, so the document is written by walking the
//! table once, not by traversing the graph: header, one object per object number in numeric
//! order, a single `0 N` classic cross-reference subsection with a proper free list, and a
//! trailer with `/Size`, `/Root` and `/Info`. There is no `/ID`.
//!
//! # Differences from Go
//!
//! * The output is a `Vec<u8>` (docs/go-differences.md: PDF inputs and outputs are slices), so
//!   "streams one object at a time" now means only that one object body exists besides the
//!   output. The audit that can refuse the whole write still runs before the first byte.
//! * `auditSerializable` is not ported: pdf-rs's `Object` has no variant that `Dict.PDFString`
//!   discards (Go's `*IndirectRef`, `Rectangle`, `LazyObjectStreamObject`), and a nested stream,
//!   which both drop, passes Go's audit too. A lazy table entry, which Go's audit refuses, is
//!   refused here by [`audit_table`].
//! * Every byte of the output, every object body and the offset table is charged to the table's
//!   ledger (`max_alloc`) before it is allocated.

use super::linearize::serialize;
use crate::error::{Error, Result};
use crate::limits::check_cancel;
use crate::linearize::malformed;
use pdf::model::{EntryObject, XRefTable};
use pdf::object::Object;
use pdf::{Held, Mem};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// `binaryMarker`: ISO 32000-1 section 7.5.2's comment line of four bytes above 128, right
/// after the header.
pub const BINARY_MARKER: &[u8] = b"%\xE2\xE3\xCF\xD3\n";

/// `producer`: this package's own `/Producer` stamp (Correction 6).
pub const PRODUCER: &str = "byblos";

/// The xref offset field is 10 ASCII digits (ISO 32000-1 7.5.4): `1e10` and past it overflows.
const MAX_OFFSET: usize = 10_000_000_000;

/// A growing output buffer whose every byte is charged to a ledger before it is allocated.
pub(crate) struct Out {
    v: Vec<u8>,
    held: Held,
}

impl Out {
    pub(crate) fn new(mem: &Mem) -> Result<Out> {
        Ok(Out {
            v: Vec::new(),
            held: mem.charge(0)?,
        })
    }

    /// Appends `b`.
    ///
    /// # Errors
    /// [`Error::Alloc`] or [`Error::OutOfMemory`].
    pub(crate) fn push(&mut self, b: &[u8]) -> Result<()> {
        self.held.grow_by(b.len() as u64)?;
        self.v.try_reserve(b.len())?;
        self.v.extend_from_slice(b);
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.v.len()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.v
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.v
    }
}

/// What [`audit_table`] found.
struct Audit<'a> {
    /// Every in-use object, by number.
    filled: BTreeMap<i64, &'a Object>,
    /// Every free object number above 0, ascending. Practically always empty for a table this
    /// package builds.
    free_nrs: Vec<i64>,
    /// The highest object number in the table.
    max: i64,
}

/// `auditTable`: walks `xt.table` once, before anything is written.
///
/// An object whose entry is filled but whose object is null is a hard error, not a written
/// null: it is either a slot the migration reserved and a bug left unfilled, or a legal null
/// from a source that named an object it never defines, and refusing is safer than a quietly
/// wrong file. An object stream or a cross-reference stream is a container of the input, not a
/// logical object: its slot is treated as free.
fn audit_table(xt: &XRefTable) -> Result<Audit<'_>> {
    let mut filled = BTreeMap::new();
    let mut free_nrs = Vec::new();
    let mut max = 0i64;
    for (&n, e) in &xt.table {
        if n <= 0 {
            continue;
        }
        if n > max {
            max = n;
        }
        if e.free {
            free_nrs.push(n);
            continue;
        }
        match &e.object {
            Some(EntryObject::ObjStm(_) | EntryObject::XRefStm(_)) => free_nrs.push(n),
            Some(EntryObject::Obj(o)) if !o.is_null() => {
                filled.insert(n, o);
            }
            Some(EntryObject::Lazy(_)) => {
                return Err(malformed(format!(
                    "byblos/pdfdoc: linearize: object {n} holds a types.LazyObjectStreamObject, \
                     which pdfcpu's serializer discards silently"
                )));
            }
            _ => {
                return Err(malformed(format!(
                    "byblos/pdfdoc: write: object {n} was reserved but never filled"
                )));
            }
        }
    }
    // Every number from 1 to max is either filled or free: a classic xref table has no third
    // state. The table's keys are ascending, so the first gap is the first key that jumps.
    let mut expect = 1i64;
    for &n in xt.table.keys() {
        if n <= 0 {
            continue;
        }
        if n != expect {
            return Err(malformed(format!(
                "byblos/pdfdoc: write: object {expect} has no cross-reference entry"
            )));
        }
        expect += 1;
    }
    Ok(Audit {
        filled,
        free_nrs,
        max,
    })
}

/// `writeDocument`: audits `xt`'s whole table, then serializes it as a classic
/// (non-cross-reference-stream) PDF.
///
/// # Errors
/// [`Error::Malformed`] for an unfilled object, a gap in the object numbers, a missing catalog,
/// or an object that starts at offset 10^10 or later; [`Error::Alloc`], [`Error::OutOfMemory`],
/// [`Error::Cancelled`].
pub fn write_document(xt: &XRefTable, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let audit = audit_table(xt)?;
    let mem = xt.mem();
    let root = xt
        .root
        .ok_or_else(|| malformed("byblos/pdfdoc: write: the document has no catalog"))?;

    let mut out = Out::new(mem)?;
    let version = xt
        .version_string()
        .map_err(|e| malformed(format!("byblos/pdfdoc: write: {e}")))?;
    let version = if version.is_empty() {
        "1.7".to_string()
    } else {
        version
    };
    out.push(format!("%PDF-{version}\n").as_bytes())?;
    out.push(BINARY_MARKER)?;

    let slots = usize::try_from(audit.max).map_err(|_| Error::OutOfMemory)?;
    let _offsets_held = mem.charge((slots as u64).saturating_mul(8))?;
    let mut offsets: Vec<usize> = Vec::new();
    offsets.try_reserve_exact(slots)?;
    let mut scratch = mem.charge(0)?;
    for n in 1..=audit.max {
        check_cancel(cancel)?;
        let Some(o) = audit.filled.get(&n) else {
            offsets.push(0); // a free entry; its xref line is built from free_nrs below.
            continue;
        };
        let body = serialize(&mut scratch, o)
            .map_err(|e| crate::linearize::wrap(e, &format!("byblos/pdfdoc: write: object {n}")))?;
        let pos = out.len();
        if pos >= MAX_OFFSET {
            return Err(malformed(format!(
                "byblos/pdfdoc: write: object {n} starts at offset {pos}, \
                 past the 10-digit xref offset field's limit"
            )));
        }
        offsets.push(pos);
        out.push(format!("{n} 0 obj\n").as_bytes())?;
        out.push(&body)?;
        out.push(b"\nendobj\n")?;
        scratch.shrink_to(0);
    }

    let start = out.len();
    out.push(format!("xref\n0 {}\n", audit.max + 1).as_bytes())?;
    // Object 0 is the free-list head, generation 65535: its "offset" names the first free
    // object, or is 0 when nothing is free.
    let head = audit.free_nrs.first().copied().unwrap_or(0);
    out.push(format!("{head:010} 65535 f \n").as_bytes())?;
    for (n, &off) in (1..=audit.max).zip(&offsets) {
        if audit.filled.contains_key(&n) {
            out.push(format!("{off:010} 00000 n \n").as_bytes())?;
            continue;
        }
        let idx = audit.free_nrs.binary_search(&n).unwrap_or_else(|i| i);
        let next = audit.free_nrs.get(idx + 1).copied().unwrap_or(0);
        out.push(format!("{next:010} 00000 f \n").as_bytes())?;
    }

    let mut trailer = format!("trailer\n<< /Size {} /Root {} 0 R", audit.max + 1, root.num);
    if let Some(info) = xt.info {
        trailer.push_str(&format!(" /Info {} 0 R", info.num));
    }
    trailer.push_str(&format!(" >>\nstartxref\n{start}\n%%EOF\n"));
    out.push(trailer.as_bytes())?;
    Ok(out.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf::object::{Dict, Ref, StreamDict};
    use pdf::{Bytes, Limits};

    /// `freshTable`: a catalog-only table: object 0 is the free head, object 1 is a bare
    /// `<< /Type /Catalog >>`, there is no `/Info`.
    fn fresh() -> XRefTable {
        pdf::api::create_xref_table_with_root_dict(&Limits::default()).unwrap()
    }

    fn dict(key: &[u8], v: Object) -> Object {
        let mut d = Dict::default();
        d.set(key, v);
        Object::Dict(d)
    }

    fn write(xt: &XRefTable) -> Result<Vec<u8>> {
        write_document(xt, &AtomicBool::new(false))
    }

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    #[test]
    fn refuses_a_reserved_but_unfilled_object() {
        let mut xt = fresh();
        xt.insert_object(Object::Null).unwrap();
        let e = write(&xt).unwrap_err();
        assert!(
            e.to_string().contains("reserved but never filled"),
            "error = {e}"
        );
        assert_eq!(e.class(), "error");
    }

    #[test]
    fn size_and_root() {
        let out = text(&write(&fresh()).unwrap());
        assert!(out.contains("/Size 2"), "{out:?}");
        assert!(out.contains("/Root 1 0 R"), "{out:?}");
        assert!(!out.contains("/Info"), "{out:?}");
    }

    #[test]
    fn info_present_when_set() {
        let mut xt = fresh();
        let nr = xt
            .insert_object(dict(b"Title", Object::string(b"hi")))
            .unwrap();
        xt.info = Some(Ref {
            num: nr,
            generation: 0,
        });
        let out = text(&write(&xt).unwrap());
        assert!(out.contains("/Info 2 0 R"), "{out:?}");
    }

    /// ONE `0 N` subsection covering every object from the free head to the highest object
    /// number, not one subsection per contiguous run.
    #[test]
    fn single_xref_subsection_with_free_head() {
        let mut xt = fresh();
        xt.insert_object(dict(b"A", Object::Integer(1))).unwrap();
        let out = text(&write(&xt).unwrap());
        let is_header = |l: &&str| {
            let mut p = l.split(' ');
            let digits = |s: Option<&str>| {
                s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            };
            digits(p.next()) && digits(p.next()) && p.next().is_none()
        };
        let headers: Vec<&str> = out.lines().filter(is_header).collect();
        assert_eq!(headers, ["0 3"], "{out:?}");
        assert!(
            out.contains("xref\n0 3\n0000000000 65535 f \n"),
            "object 0 is not the free head pdf-go itself writes: {out:?}"
        );
    }

    /// The full free-list chain over two non-contiguous free objects: 0 -> 3 -> 5 -> 0.
    #[test]
    fn free_list_chains_multiple_free_entries() {
        let mut xt = fresh();
        for i in 0..5 {
            xt.insert_object(dict(b"N", Object::Integer(i))).unwrap();
        }
        // Objects are 1 (the catalog) through 6. Free 3 and 5.
        for n in [3, 5] {
            let e = xt.entry_mut(n).unwrap();
            e.free = true;
            e.object = None;
        }
        let out = text(&write(&xt).unwrap());
        let lines: Vec<&str> = out.split('\n').collect();
        let start = lines.iter().position(|l| *l == "0 7").unwrap();
        let entries = &lines[start + 1..start + 8];
        assert_eq!(entries[0], "0000000003 65535 f ");
        assert_eq!(entries[3], "0000000005 00000 f ");
        assert_eq!(entries[5], "0000000000 00000 f ");
        for n in [1, 2, 4, 6] {
            assert!(entries[n].ends_with(" n "), "object {n} = {:?}", entries[n]);
        }
    }

    /// An object number in `[1, max]` that is neither filled nor free is refused outright
    /// instead of corrupting the chain.
    #[test]
    fn refuses_a_table_gap_between_filled_and_free() {
        let mut xt = fresh();
        for i in 0..5 {
            xt.insert_object(dict(b"N", Object::Integer(i))).unwrap();
        }
        xt.table.remove(&4);
        let e = write(&xt).unwrap_err();
        assert!(e.to_string().contains("object 4"), "error = {e}");
    }

    #[test]
    fn header_names_the_version() {
        let xt = fresh();
        let out = write(&xt).unwrap();
        let want = format!("%PDF-{}\n", xt.version_string().unwrap());
        assert!(out.starts_with(want.as_bytes()));
        assert!(
            out.windows(BINARY_MARKER.len()).any(|w| w == BINARY_MARKER),
            "the binary comment line (ISO 32000-1 7.5.2) is missing"
        );
    }

    /// `/Length` matches the written payload, and a binary payload (NUL and every high byte)
    /// survives byte for byte through a round trip via pdf-rs's own reader.
    #[test]
    fn stream_length_and_binary_payload() {
        let mut xt = fresh();
        let payload: Vec<u8> = (0..512).map(|i| i as u8).collect();
        let mut d = Dict::default();
        d.insert_name(b"Type", b"XObject");
        let sd = StreamDict {
            dict: d,
            raw: Some(Bytes::new(payload.clone(), payload.len(), None)),
            ..StreamDict::default()
        };
        let nr = xt.insert_object(Object::Stream(Box::new(sd))).unwrap();
        let out = write(&xt).unwrap();

        let mut ctx =
            pdf::api::read(&out, &pdf::Config::default(), &AtomicBool::new(false)).unwrap();
        let r = Object::Ref(Ref {
            num: nr,
            generation: 0,
        });
        let (got, _) = ctx.xref.dereference_stream_dict(&r).unwrap();
        let got = got.unwrap();
        assert!(matches!(
            got.dict.find(b"Length"),
            Some(Object::Integer(512))
        ));
        assert_eq!(got.raw.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn output_over_the_budget_is_refused() {
        let xt = fresh();
        let mut small = Out::new(&pdf::Mem::with_max(4)).unwrap();
        assert!(matches!(
            small.push(b"too long for four bytes"),
            Err(Error::Alloc { .. })
        ));
        // And a cancelled write writes nothing.
        let e = write_document(&xt, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    /// `TestWriteDocumentPassesQpdfCheck`: pdf-rs's reader silently reconstructs a damaged
    /// classic xref table, so a round trip cannot catch a bug in the table this writer owns (the
    /// free list, `/Size`, an offset); qpdf warns instead. Needs `qpdf` on `PATH`.
    #[test]
    #[ignore = "needs qpdf on PATH"]
    fn passes_qpdf_check() {
        let mut xt = fresh();
        let mut d = Dict::default();
        d.insert_name(b"Type", b"Pages");
        d.insert_int(b"Count", 1);
        d.set(
            b"Kids",
            Object::Array(vec![Object::Ref(Ref {
                num: 3,
                generation: 0,
            })]),
        );
        let pages = xt.insert_object(Object::Dict(d)).unwrap();
        let mut page = Dict::default();
        page.insert_name(b"Type", b"Page");
        page.set(
            b"Parent",
            Object::Ref(Ref {
                num: pages,
                generation: 0,
            }),
        );
        page.set(
            b"MediaBox",
            Object::Array([0, 0, 612, 792].map(Object::Integer).to_vec()),
        );
        page.set(b"Resources", Object::Dict(Dict::default()));
        xt.insert_object(Object::Dict(page)).unwrap();
        xt.catalog_mut().unwrap().set(
            b"Pages",
            Object::Ref(Ref {
                num: pages,
                generation: 0,
            }),
        );
        let out = write(&xt).unwrap();

        let path =
            std::env::temp_dir().join(format!("byblos-selfwrite-{}.pdf", std::process::id()));
        std::fs::write(&path, &out).unwrap();
        let report = std::process::Command::new("qpdf")
            .arg("--check")
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(&path);
        let report = report.unwrap();
        let all = format!("{}{}", text(&report.stdout), text(&report.stderr));
        assert!(report.status.success(), "qpdf --check: {all}");
        assert!(!all.contains("WARNING"), "qpdf --check warned:\n{all}");
    }
}
