//! The pdf-rs half of Annex F linearization: port of `internal/pdfdoc/linearize.go` (byb-1y7).
//!
//! `crate::linearize` owns everything Annex F and holds no PDF parser; this file is the
//! translation layer. It turns a parsed context into the neutral [`Graph`] that module
//! describes, serializes each object under the renumbering it decides, and hands the bytes back.
//! The serialization helpers ([`serialize`], [`stream_body`], [`renumber_object`],
//! [`live_objects`], [`reachable`], [`refs_of`], [`trailer_id`]) are shared with
//! `deterministic.rs` and `selfwrite.rs`, as in Go.
//!
//! # Assumptions about `mod.rs` and `doc.rs` (written concurrently)
//!
//! * `Doc::open_limits(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Doc>` is Go's
//!   `OpenLimits` (it normalizes the page tree), and `Doc` has `pub(super) ctx:
//!   pdf::model::Context`. [`linearize`] is the only user; [`linearize_context`] needs no `Doc`.
//!
//! # Differences from Go
//!
//! * `auditSerializable` is not ported. pdf-rs's `Object` has none of the values Go's
//!   `Dict.PDFString` drops silently (`*IndirectRef`, `Rectangle`, `LazyObjectStreamObject`), so
//!   the audit could never fail; a nested stream, which both serializers drop, passes Go's audit
//!   as well.
//! * The input and the output are slices. Object bodies and the graph's tables are charged to
//!   the document's ledger (`max_alloc`); the layout planner and the writer charge a ledger of
//!   their own, sized to what is left of `max_alloc` after the bodies.
//! * The page-tree walk is bounded by pdf-rs's recursion limit (Go's native stack is not).

use super::Doc;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use crate::linearize::{Graph, Meta, malformed, plan_layout, wrap};
use pdf::Held;
use pdf::model::{Context, EntryObject, XRefTable};
use pdf::object::string::hex_decode;
use pdf::object::{Dict, Object, Ref};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// `inheritable`: the page attributes ISO 32000-1 Table 30 lets a `/Pages` node hold on behalf
/// of its descendants.
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// `openDocKeys`: the catalog entries a reader consults before it can display anything. Annex
/// F.3.5 puts what they reach in part 4.
const OPEN_DOC_KEYS: [&[u8]; 5] = [
    b"ViewerPreferences",
    b"PageMode",
    b"Threads",
    b"OpenAction",
    b"AcroForm",
];

/// `fmt.Errorf("<prefix>: %w", e)` for a pdf-rs error.
pub(crate) fn pdf_err(e: pdf::Error, prefix: &str) -> Error {
    wrap(Error::from(e), prefix)
}

/// `Linearize`: a linearized ("fast web view") copy of `data`. It is the last thing that may
/// touch the bytes: anything that re-serializes the document afterwards silently removes the
/// linearization again.
///
/// # Errors
/// The errors of `Doc::open_limits`, and those of [`linearize_context`].
pub fn linearize(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut d = Doc::open_limits(data, lim, cancel)?;
    linearize_context(&mut d.ctx, lim, cancel)
}

/// [`linearize`] over an opened context.
///
/// # Errors
/// [`Error::Malformed`] for an encrypted document, a missing catalog or page tree, a document
/// without pages, or a partition that loses an object; the errors of
/// [`crate::linearize::plan_layout`] and [`crate::linearize::write`]; [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn linearize_context(ctx: &mut Context, lim: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let xt = &mut ctx.xref;
    if xt.encrypt.is_some() {
        // pdf-rs decrypts strings and streams on read. Re-emitting them under the input's
        // /Encrypt dictionary would produce a document that opens as garbage, and stripping the
        // encryption would change what the document IS.
        return Err(malformed(
            "byblos/pdfdoc: linearize: the document is encrypted",
        ));
    }
    if xt.root.is_none() {
        return Err(malformed(
            "byblos/pdfdoc: linearize: the document has no catalog",
        ));
    }

    let (g, _graph_held) = build_graph(xt, cancel)?;
    // The document's ledger holds the parse and the graph; the planner's and the writer's own
    // ledgers get what is left of it.
    let mut rest = *lim;
    rest.max_alloc = xt
        .mem()
        .left()
        .min(lim.alloc_limit().saturating_sub(xt.mem().used()))
        .max(1);
    let plan = plan_layout(&g, &rest, cancel).map_err(|e| wrap(e, "byblos/pdfdoc: linearize"))?;
    let (bodies, _held) = serialize_all(xt, &plan.renumber, cancel)?;
    let [id0, id1] = trailer_id(xt);
    let meta = Meta {
        version: xt
            .version_string()
            .map_err(|e| malformed(format!("byblos/pdfdoc: linearize: {e}")))?,
        id: [id0.unwrap_or_default(), id1.unwrap_or_default()],
    };
    // `write` charges a fresh ledger; shrink it by what the document and the bodies hold (the
    // bodies are charged to the document's ledger) so one call stays within `max_alloc`.
    rest.max_alloc = xt
        .mem()
        .left()
        .min(lim.alloc_limit().saturating_sub(xt.mem().used()))
        .max(1);
    let mut out = Vec::new();
    crate::linearize::write(&mut out, &plan, &bodies, &meta, &rest, cancel)
        .map_err(|e| wrap(e, "byblos/pdfdoc: linearize"))?;
    Ok(out)
}

/// `buildGraph`: the parsed context as a [`Graph`].
fn build_graph(xt: &mut XRefTable, cancel: &AtomicBool) -> Result<(Graph, Held)> {
    xt.catalog()
        .map_err(|e| pdf_err(e, "byblos/pdfdoc: linearize: catalog"))?;
    // The live map, the reachable set, the edge lists and the walk's sets are about this many
    // bytes per table entry.
    let held = xt
        .mem()
        .charge((xt.table.len() as u64).saturating_mul(128))?;
    let mut g = Graph {
        catalog: xt.root.map_or(0, |r| r.num),
        info: xt.info.map_or(0, |r| r.num),
        ..Graph::default()
    };

    let root = xt
        .pages()
        .map_err(|e| pdf_err(e, "byblos/pdfdoc: linearize: page tree"))?;
    let mut walk = Walk::default();
    walk_page_tree(xt, &mut walk, root, &Dict::default(), 0)?;
    if walk.pages.is_empty() {
        return Err(malformed(
            "byblos/pdfdoc: linearize: the document has no pages",
        ));
    }
    g.pages = walk.pages;
    g.page_tree = walk.tree;

    // Go reads the catalog after the walk: a catalog that is itself a page-tree node has been
    // rewritten by it.
    let catalog = xt
        .catalog()
        .map_err(|e| pdf_err(e, "byblos/pdfdoc: linearize: catalog"))?
        .clone();
    if let Some(Object::Ref(r)) = catalog.find(b"Outlines") {
        g.outlines = r.num;
    }
    if let Some(m) = catalog.find(b"PageMode") {
        g.use_outlines =
            matches!(xt.dereference(m), Ok(Object::Name(n)) if n.as_slice() == b"UseOutlines");
    }
    for (key, val) in catalog.iter() {
        match key.as_slice() {
            b"Pages" | b"Outlines" | b"Type" => {}
            k if OPEN_DOC_KEYS.contains(&k) => g.open_doc.extend(refs_of(val)),
            _ => g.other.extend(refs_of(val)),
        }
    }
    if g.info != 0 {
        g.other.push(g.info);
    }
    g.open_doc.sort_unstable();
    g.other.sort_unstable();
    g.open_doc.dedup();
    g.other.dedup();

    load_lazy(xt, cancel)?;
    let live = live_objects(xt);
    let page_set: BTreeSet<i64> = g.pages.iter().copied().collect();
    let reach = reachable(&live, &[g.catalog, g.info]);

    for &n in &reach {
        check_cancel(cancel)?;
        let Some(&o) = live.get(&n) else { continue };
        let mut out: Vec<i64> = if page_set.contains(&n) {
            // /Parent points back at the page tree, which Annex F.3.10 puts in part 9, and
            // /Thumb at a thumbnail no reader needs to show the page. Following either from a
            // page drags part-9 material into the first-page section. Both objects are still
            // written; they just do not steer the partition.
            match o {
                Object::Dict(pd) => pd
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_slice(), b"Parent" | b"Thumb"))
                    .flat_map(|(_, v)| refs_of(v))
                    .collect(),
                _ => Vec::new(),
            }
        } else {
            refs_of(o)
        };
        out.retain(|m| reach.contains(m));
        out.sort_unstable();
        out.dedup();
        g.refs.insert(n, out);
    }
    Ok((g, held))
}

/// The page tree in page order, as `walkPageTree` returns it.
#[derive(Default)]
pub(super) struct Walk {
    seen: BTreeSet<i64>,
    pub(super) pages: Vec<i64>,
    pub(super) tree: Vec<i64>,
}

/// `walkPageTree`: records the page tree in page order and pushes inherited attributes down
/// onto the leaves.
///
/// The push-down is required, not an optimization: Annex F.3.7 puts the page tree in part 9,
/// past `/E`, so a first page whose `/MediaBox` lives on a `/Pages` node would need an object a
/// progressive reader has not received yet. The raw objects are copied rather than anything
/// from `InheritedPageAttrs`.
pub(super) fn walk_page_tree(
    xt: &mut XRefTable,
    w: &mut Walk,
    r: Ref,
    inh: &Dict,
    depth: u64,
) -> Result<()> {
    let n = r.num;
    if !w.seen.insert(n) {
        return Err(malformed(format!(
            "byblos/pdfdoc: linearize: page tree revisits object {n}"
        )));
    }
    xt.check_recursion_depth("page tree", depth)
        .map_err(|e| pdf_err(e, "byblos/pdfdoc: linearize"))?;

    let node = format!("byblos/pdfdoc: linearize: page tree node {n}");
    let Some(d) = xt.dereference_dict_mut(r).map_err(|e| pdf_err(e, &node))? else {
        return Err(malformed(format!("{node}: no dictionary")));
    };
    let Some(kids) = d.find(b"Kids").cloned() else {
        for key in INHERITABLE {
            if d.find(key).is_none()
                && let Some(v) = inh.0.get(key)
            {
                d.set(key, v.clone());
            }
        }
        w.pages.push(n);
        return Ok(());
    };

    let mut down = inh.clone();
    for key in INHERITABLE {
        if let Some(v) = d.find(key).cloned() {
            down.set(key, v);
            // And take it OFF the node. Leaving it would be redundant once every leaf below
            // carries its own copy, and qpdf treats a leftover as an error when it checks a
            // linearized file.
            d.0.remove(key);
        }
    }
    let arr = xt
        .dereference_array(&kids)
        .map_err(|e| pdf_err(e, &format!("byblos/pdfdoc: linearize: /Kids of {n}")))?
        .cloned()
        .unwrap_or_default();
    w.tree.push(n);
    for k in &arr {
        let Object::Ref(kr) = k else {
            return Err(malformed(format!(
                "byblos/pdfdoc: linearize: /Kids of {n} holds a direct object"
            )));
        };
        walk_page_tree(xt, w, *kr, &down, depth + 1)?;
    }
    Ok(())
}

/// Decodes every object still packed in an object stream, so that [`live_objects`] can read the
/// table without `&mut`. This is the `Dereference` Go's `liveObjects` does per entry.
///
/// # Errors
/// The decode error of an object stream, with Go's context; [`Error::Cancelled`].
pub(crate) fn load_lazy(xt: &mut XRefTable, cancel: &AtomicBool) -> Result<()> {
    let lazy: Vec<i64> = xt
        .table
        .iter()
        .filter(|&(&n, e)| n != 0 && !e.free && matches!(e.object, Some(EntryObject::Lazy(_))))
        .map(|(&n, _)| n)
        .collect();
    for n in lazy {
        check_cancel(cancel)?;
        xt.decode_lazy_entry(n, cancel)
            .map_err(|e| pdf_err(e, &format!("byblos/pdfdoc: linearize: object {n}")))?;
    }
    Ok(())
}

/// `liveObjects`: every object the cross-reference table holds, minus the containers and the
/// null objects. Call [`load_lazy`] first.
///
/// An object stream and a cross-reference stream describe how the INPUT was stored; the objects
/// they hold are already present as ordinary entries, and carrying the containers into the
/// output would write the input's storage layout into a file that no longer uses it.
pub(crate) fn live_objects(xt: &XRefTable) -> BTreeMap<i64, &Object> {
    let mut live = BTreeMap::new();
    for (&n, e) in &xt.table {
        if n == 0 || e.free {
            continue;
        }
        if let Some(EntryObject::Obj(o)) = &e.object
            && !o.is_null()
        {
            live.insert(n, o);
        }
    }
    live
}

/// `reachable`: the set of objects the catalog and the info dictionary can get to. Anything
/// else is a leftover from an incremental update, or the input's own linearization dictionary
/// and hint stream, which are unreferenced by construction and must not be carried into the
/// output.
pub(crate) fn reachable(live: &BTreeMap<i64, &Object>, roots: &[i64]) -> BTreeSet<i64> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<i64> = roots.iter().copied().filter(|&r| r != 0).collect();
    while let Some(n) = stack.pop() {
        if seen.contains(&n) {
            continue;
        }
        let Some(o) = live.get(&n) else { continue };
        seen.insert(n);
        stack.extend(refs_of(o));
    }
    seen
}

/// `refsOf`: the object numbers `o` refers to, at any depth.
///
/// A stream dictionary's `/Length` is deliberately NOT followed. It is the one entry
/// [`stream_body`] rewrites, with a direct integer measured off the bytes actually emitted;
/// following an indirect one would put the integer object it names into the write set and then
/// delete the only reference to it, leaving an orphan that qpdf refuses.
pub(crate) fn refs_of(o: &Object) -> Vec<i64> {
    fn walk(o: &Object, out: &mut Vec<i64>) {
        match o {
            Object::Ref(r) => out.push(r.num),
            Object::Dict(d) => walk_dict(d, false, out),
            Object::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Object::Stream(s) => walk_dict(&s.dict, true, out),
            _ => {}
        }
    }
    fn walk_dict(d: &Dict, is_stream: bool, out: &mut Vec<i64>) {
        for (key, val) in d.iter() {
            if is_stream && key.as_slice() == b"Length" {
                continue;
            }
            walk(val, out);
        }
    }
    let mut out = Vec::new();
    walk(o, &mut out);
    out
}

/// `serializeAll`: every object under the renumbering, keyed by NEW number. The returned
/// charge covers the bodies; hold it until they are used.
fn serialize_all(
    xt: &mut XRefTable,
    renumber: &BTreeMap<i64, i64>,
    cancel: &AtomicBool,
) -> Result<(BTreeMap<i64, Vec<u8>>, Held)> {
    let mut held = xt.mem().charge(0)?;
    let mut out = BTreeMap::new();
    for (&old, &new) in renumber {
        check_cancel(cancel)?;
        let r = Object::Ref(Ref {
            num: old,
            generation: 0,
        });
        let prefix = format!("byblos/pdfdoc: linearize: object {old}");
        let o = xt.dereference(&r).map_err(|e| pdf_err(e, &prefix))?;
        let body =
            serialize(&mut held, &renumber_object(o, renumber)).map_err(|e| wrap(e, &prefix))?;
        out.insert(new, body);
    }
    Ok((out, held))
}

/// `renumberObject`: a deep copy of `o` with every reference rewritten.
///
/// A reference to an object that is not in the map becomes null: ISO 32000-1 7.3.10 says a
/// reference to an undefined object shall be treated as the null object, so this loses nothing,
/// whereas leaving the reference in place would name an object number that no longer exists.
pub(crate) fn renumber_object(o: &Object, renumber: &BTreeMap<i64, i64>) -> Object {
    fn dict(d: &Dict, renumber: &BTreeMap<i64, i64>) -> Dict {
        Dict(
            d.iter()
                .map(|(k, v)| (k.clone(), renumber_object(v, renumber)))
                .collect(),
        )
    }
    match o {
        Object::Ref(r) => match renumber.get(&r.num) {
            Some(&num) => Object::Ref(Ref { num, generation: 0 }),
            None => Object::Null,
        },
        Object::Dict(d) => Object::Dict(dict(d, renumber)),
        Object::Array(a) => Object::Array(a.iter().map(|v| renumber_object(v, renumber)).collect()),
        Object::Stream(sd) => {
            let mut sd = sd.clone();
            sd.dict = dict(&sd.dict, renumber);
            Object::Stream(sd)
        }
        other => other.clone(),
    }
}

/// `serialize`: one object's body, without the `N 0 obj` / `endobj` wrapper. The bytes are
/// charged to `held`.
///
/// The stream shape mirrors pdf-go's `writeStreamObject` exactly (see [`stream_body`]), so an
/// object that goes through here is byte-identical to one pdf-go wrote itself.
///
/// # Errors
/// [`Error::Alloc`] or [`Error::OutOfMemory`].
pub(crate) fn serialize(held: &mut Held, o: &Object) -> Result<Vec<u8>> {
    match o {
        Object::Stream(sd) => stream_body(held, &sd.dict, sd.raw.as_deref().unwrap_or_default()),
        _ => {
            let s = o.pdf_string();
            held.grow_by(s.len() as u64)?;
            Ok(s)
        }
    }
}

/// `streamBody`: the dictionary, `"\nstream\n"`, the raw (still encoded) payload and
/// `"\nendstream"`.
///
/// `/Length` is written direct and measured off `raw`. An indirect `/Length` would name an
/// object number the renumbering has moved, and pdf-go's own writer refuses a stream whose
/// payload disagrees with it. The bytes are charged to `held` before they are allocated.
///
/// # Errors
/// [`Error::Alloc`] or [`Error::OutOfMemory`].
pub(crate) fn stream_body(held: &mut Held, dict: &Dict, raw: &[u8]) -> Result<Vec<u8>> {
    let mut d = dict.clone();
    d.set(b"Length", Object::Integer(raw.len() as i64));
    let head = d.pdf_string();
    let n = head
        .len()
        .checked_add(raw.len())
        .and_then(|n| n.checked_add(b"\nstream\n\nendstream".len()))
        .ok_or(Error::OutOfMemory)?;
    held.grow_by(n as u64)?;
    let mut out = Vec::new();
    out.try_reserve_exact(n)?;
    out.extend_from_slice(&head);
    out.extend_from_slice(b"\nstream\n");
    out.extend_from_slice(raw);
    out.extend_from_slice(b"\nendstream");
    Ok(out)
}

/// `trailerID`: the input's `/ID` carried forward. It is not regenerated: nothing in this pass
/// changes what the document IS, and a reader that has cached the file by `/ID` should keep
/// matching it. `None` is Go's nil slice (absent, or a form that does not decode), which is not
/// the same as an empty one.
pub(crate) fn trailer_id(xt: &XRefTable) -> [Option<Vec<u8>>; 2] {
    let mut out = [None, None];
    if let Some(id) = &xt.id {
        for (slot, v) in out.iter_mut().zip(id) {
            *slot = match v {
                Object::Hex(h) => hex_decode(h).ok(),
                Object::String(s) => Some(s.clone()),
                _ => None,
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf::object::StreamDict;

    /// A classic PDF of `objects` (object `n` is `objects[n - 1]`), the catalog being object 1.
    fn pdf(objects: &[&str]) -> Vec<u8> {
        let mut f = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(f.len());
            f.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = f.len();
        f.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for o in offsets {
            f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        f.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        f
    }

    fn read(data: &[u8]) -> Context {
        pdf::api::read(data, &pdf::Config::default(), &AtomicBool::new(false)).unwrap()
    }

    fn r(num: i64) -> Ref {
        Ref { num, generation: 0 }
    }

    #[test]
    fn linearizes_a_one_page_document_that_reads_back() {
        let data = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
        ]);
        let cancel = AtomicBool::new(false);
        let out = linearize_context(&mut read(&data), &Limits::default(), &cancel).unwrap();
        assert!(out.starts_with(b"%PDF-"));
        assert!(String::from_utf8_lossy(&out).contains("/Linearized 1 "));
        let mut again = read(&out);
        again.xref.ensure_page_count().unwrap();
        assert_eq!(again.xref.page_count, 1);
        // Two runs are the same bytes.
        assert_eq!(
            out,
            linearize_context(&mut read(&data), &Limits::default(), &cancel).unwrap()
        );
    }

    /// Annex F.3.7 puts the page tree in part 9, so an attribute inherited from a `/Pages` node
    /// is pushed onto the leaf and taken off the node.
    #[test]
    fn pushes_inherited_attributes_down_and_off_the_node() {
        let data = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] /Rotate 90 >>",
            "<< /Type /Page /Parent 2 0 R /Rotate 180 /Resources << >> >>",
        ]);
        let mut ctx = read(&data);
        linearize_context(&mut ctx, &Limits::default(), &AtomicBool::new(false)).unwrap();
        let leaf = ctx
            .xref
            .dereference_dict_mut(r(3))
            .unwrap()
            .unwrap()
            .clone();
        // The leaf's own /Rotate wins; the missing /MediaBox is inherited.
        assert!(matches!(leaf.find(b"Rotate"), Some(Object::Integer(180))));
        assert!(leaf.find(b"MediaBox").is_some());
        let node = ctx
            .xref
            .dereference_dict_mut(r(2))
            .unwrap()
            .unwrap()
            .clone();
        assert!(node.find(b"MediaBox").is_none());
        assert!(node.find(b"Rotate").is_none());
    }

    #[test]
    fn refuses_a_document_with_no_pages() {
        let data = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [] /Count 0 >>",
        ]);
        let e = linearize_context(
            &mut read(&data),
            &Limits::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(e, Error::Malformed(_)), "{e:?}");
    }

    #[test]
    fn cancelled_before_it_starts() {
        let data = pdf(&["<< /Type /Catalog >>"]);
        let e = linearize_context(&mut read(&data), &Limits::default(), &AtomicBool::new(true))
            .unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    /// `refsOf` follows every reference at any depth, but not a stream's `/Length`.
    #[test]
    fn refs_of_skips_the_stream_length() {
        let mut dict = Dict::default();
        dict.set(b"Length", Object::Ref(r(9)));
        dict.set(b"X", Object::Ref(r(7)));
        dict.set(
            b"Y",
            Object::Array(vec![
                Object::Integer(1),
                Object::Dict({
                    let mut d = Dict::default();
                    d.set(b"Length", Object::Ref(r(5)));
                    d
                }),
            ]),
        );
        let stream = Object::Stream(Box::new(StreamDict {
            dict: dict.clone(),
            ..StreamDict::default()
        }));
        // The nested dictionary's /Length is an ordinary entry.
        assert_eq!(refs_of(&stream), vec![7, 5]);
        assert_eq!(refs_of(&Object::Dict(dict)), vec![9, 7, 5]);
        assert!(refs_of(&Object::Integer(3)).is_empty());
    }

    #[test]
    fn renumber_object_rewrites_references_and_nulls_the_unknown() {
        let map = BTreeMap::from([(4, 1), (6, 2)]);
        let mut d = Dict::default();
        d.set(b"A", Object::Ref(r(4)));
        d.set(
            b"B",
            Object::Array(vec![Object::Ref(r(6)), Object::Ref(r(99))]),
        );
        let got = renumber_object(&Object::Dict(d), &map);
        assert_eq!(got.pdf_string(), b"<</A 1 0 R/B[2 0 R null]>>".to_vec());
        assert!(matches!(
            renumber_object(&Object::Ref(r(99)), &map),
            Object::Null
        ));
    }

    #[test]
    fn stream_body_measures_the_length_off_the_payload() {
        let mut dict = Dict::default();
        dict.set(b"Length", Object::Ref(r(5)));
        dict.set(b"Filter", Object::name(b"FlateDecode"));
        let mut held = pdf::Mem::with_max(1 << 20).charge(0).unwrap();
        let body = stream_body(&mut held, &dict, b"abc").unwrap();
        assert_eq!(
            body,
            b"<</Filter/FlateDecode/Length 3>>\nstream\nabc\nendstream".to_vec()
        );
        // A stream with no payload at all is an empty one.
        let sd = Object::Stream(Box::default());
        assert_eq!(
            serialize(&mut held, &sd).unwrap(),
            b"<</Length 0>>\nstream\n\nendstream".to_vec()
        );
        assert_eq!(
            serialize(&mut held, &Object::Null).unwrap(),
            b"null".to_vec()
        );
    }

    #[test]
    fn stream_body_is_refused_over_the_budget() {
        let mut held = pdf::Mem::with_max(8).charge(0).unwrap();
        let e = stream_body(&mut held, &Dict::default(), &[0; 64]).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e:?}");
    }

    #[test]
    fn linearize_context_honours_the_callers_limit_over_a_larger_ledger() {
        let data = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>",
        ]);
        let mut ctx = read(&data);
        let lim = Limits {
            max_alloc: 1,
            ..Limits::default()
        };
        assert!(linearize_context(&mut ctx, &lim, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn trailer_id_reads_hex_and_string_elements() {
        let mut xt = read(&pdf(&["<< /Type /Catalog >>"])).xref;
        assert_eq!(trailer_id(&xt), [None, None]);
        xt.id = Some(vec![Object::Hex(b"00ff".to_vec()), Object::string(b"ab")]);
        assert_eq!(trailer_id(&xt), [Some(vec![0, 0xff]), Some(b"ab".to_vec())]);
        xt.id = Some(vec![Object::Hex(b"0".to_vec())]);
        assert_eq!(trailer_id(&xt), [None, None]);
    }
}
