//! The invisible-text write seam: port of `internal/pdfdoc/text.go` (byb-b4 part B).
//!
//! `stamp` builds operator bytes and a [`TrueTypeFont`]; everything that touches pdf-rs types
//! (allocating the font objects, binding them into a page's `/Font` resources, splicing a new
//! content stream into `/Contents`) happens here. [`Doc::add_font_resource`],
//! [`Doc::append_content`] and [`Doc::wrap_content`] are on [`Doc`] next to the other write
//! methods; they need the page tree `Doc::open` normalised.
//!
//! # Assumptions about `doc.rs` (written concurrently)
//!
//! * `Doc` is the document type of `super`, with `pub(super) ctx: pdf::model::Context` (Go's
//!   `d.ctx`, so `self.ctx.xref` is Go's `d.ctx.XRefTable`) and
//!   `pub(super) font_refs: std::collections::BTreeMap<String, pdf::object::Ref>` (Go's
//!   `d.fontRefs`, empty at open).
//! * `Doc::page_contents(&mut self, pd: &pdf::object::Dict, cancel: &AtomicBool) ->
//!   Result<(Vec<u8>, bool)>` is Go's `pageContents` (decoded `/Contents`, recovered flag).
//!
//! # Differences from Go
//!
//! * `XRefTable::page_dict` returns a copy of the page dictionary, not the stored map. Every
//!   change is written back through `dereference_dict_mut` / `entry_mut`.
//! * Go's `PageDict` hands a page without its own `/Resources` the ANCESTOR's stored `/Resources`
//!   map (pdf-go `xreftable.go:2164-2170`), and `bindFont` stores that map in the page: the
//!   binding lands in the ancestor (direct, or the indirect `/Resources` object) and every page
//!   that adopted the map shares it, so a second inheriting page gets `BbGl1` and the first page
//!   has it too. Here the binding is made in the ancestor through
//!   `InheritedPageAttrs::resources_owner`, and each adopting page takes a copy of the
//!   ancestor's final `/Resources` when the document is written
//!   ([`Doc::store_adopted_resources`]). A `/Resources` or `/Font` that is an indirect object is
//!   changed in place, as in Go.
//! * `net_ctm` keeps the last six numbers and charges its `q` stack to `max_alloc`; Go grows
//!   both without a bound. The result is the same.

use super::Doc;
use crate::content::{IDENTITY, Kind, Lexer, Matrix};
use crate::error::{Error, Result};
use crate::go::fmt::format_f;
use crate::limits::check_cancel;
use pdf::model::{EntryObject, XRefTable};
use pdf::object::{Dict, Object, Ref};
use pdf::{Held, Mem};
use std::sync::atomic::AtomicBool;

/// `TrueTypeFont`: a simple, single-byte-encoded TrueType font to embed.
///
/// Exactly the fields a simple (non-Type0) `/FontDescriptor` and `/Font` dictionary need.
/// `widths` is in 1000ths of text space (ISO 32000-1 9.2.4), indexed from `first_char`, one
/// entry per character code up to `LastChar` (`first_char + widths.len() - 1`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrueTypeFont {
    /// `/BaseFont` and `/FontDescriptor` `/FontName`.
    pub base_font: String,
    /// The sfnt bytes; becomes `/FontFile2` with `/Length1` = `program.len()`.
    pub program: Vec<u8>,
    pub first_char: i64,
    pub widths: Vec<i64>,
    pub flags: i64,
    pub font_bbox: [i64; 4],
    pub italic_angle: i64,
    pub ascent: i64,
    pub descent: i64,
    pub cap_height: i64,
    pub stem_v: i64,
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The 1-based page number as pdf-rs takes it.
fn page_nr(n: usize) -> i64 {
    to_i64(n)
}

/// `fmt.Errorf("byblos/pdfdoc: page %d <what>: %w", n, err)`.
fn page_err(n: usize, what: &str, e: pdf::Error) -> Error {
    Error::from(e.context(format!("byblos/pdfdoc: page {n} {what}")))
}

fn font_err(f: &TrueTypeFont, e: pdf::Error) -> Error {
    Error::from(e.context(format!("byblos/pdfdoc: font {}", f.base_font)))
}

fn gone(what: &str) -> Error {
    Error::Malformed(format!("{what} dictionary is no longer there"))
}

fn direct_contents(n: usize) -> Error {
    Error::Malformed(format!(
        "page {n} has a direct /Contents stream, which is malformed"
    ))
}

/// Where a dictionary lives: inline in its parent, or as an indirect object.
#[derive(Clone, Copy)]
enum Loc {
    Direct,
    At(Ref),
}

/// `DereferenceDict(o)` with the error ignored (Go `res, _ :=`): where the dictionary is, or
/// `None` for null, a wrong type, or a failed lookup.
fn dict_loc(xt: &mut XRefTable, o: &Object) -> Option<Loc> {
    match o {
        Object::Dict(_) => Some(Loc::Direct),
        Object::Ref(r) => matches!(xt.dereference_dict_ref(*r), Ok(Some(_))).then_some(Loc::At(*r)),
        _ => None,
    }
}

/// The page's `/Resources`, to change.
fn res_mut<'a>(xt: &'a mut XRefTable, pd: &'a mut Dict, loc: Loc) -> Result<&'a mut Dict> {
    match loc {
        Loc::Direct => match pd.find_mut(b"Resources") {
            Some(Object::Dict(d)) => Ok(d),
            _ => Err(gone("/Resources")),
        },
        Loc::At(r) => xt
            .dereference_dict_mut(r)?
            .ok_or_else(|| gone("/Resources")),
    }
}

/// The `/Font` dictionary of the page's `/Resources`, to change.
fn fonts_mut<'a>(
    xt: &'a mut XRefTable,
    pd: &'a mut Dict,
    res_loc: Loc,
    fonts_loc: Loc,
) -> Result<&'a mut Dict> {
    match fonts_loc {
        Loc::At(r) => xt.dereference_dict_mut(r)?.ok_or_else(|| gone("/Font")),
        Loc::Direct => match res_mut(xt, pd, res_loc)?.find_mut(b"Font") {
            Some(Object::Dict(d)) => Ok(d),
            _ => Err(gone("/Font")),
        },
    }
}

/// What an indirect `/Contents` resolves to (ISO 32000-1 table 30 allows a stream or an array
/// of streams).
enum Target {
    /// The array, copied with room for two more elements, and the charge for the copy.
    Array(Vec<Object>, Held),
    Stream,
    Other(&'static str),
}

/// Go's `%T` of the types a `/Contents` can wrongly resolve to.
fn kind_name(o: &Object) -> &'static str {
    match o {
        Object::Null => "<nil>",
        Object::Bool(_) => "Boolean",
        Object::Integer(_) => "Integer",
        Object::Float(_) => "Float",
        Object::Name(_) => "Name",
        Object::String(_) => "StringLiteral",
        Object::Hex(_) => "HexLiteral",
        Object::Ref(_) => "IndirectRef",
        Object::Array(_) => "Array",
        Object::Dict(_) => "Dict",
        Object::Stream(_) => "StreamDict",
    }
}

/// Charges a copy of `n` elements (plus two spare) of an input-sized array or dictionary.
fn charge_elems(mem: &Mem, n: usize) -> Result<Held> {
    let bytes = n
        .saturating_add(2)
        .saturating_mul(std::mem::size_of::<Object>());
    Ok(mem.charge(bytes as u64)?)
}

/// Charges a deep clone of `o`: every nested array and dictionary entry, not only the top level.
fn charge_clone(mem: &Mem, o: &Object) -> Result<Held> {
    Ok(mem.charge(o.copy_bytes())?)
}

fn contents_target(xt: &mut XRefTable, r: Ref) -> Result<Target> {
    let mem: Mem = xt.mem().clone();
    let ro = Object::Ref(r);
    let t = match xt.dereference(&ro) {
        Ok(t) => t,
        Err(e) => return Err(Error::from(e.context("dereference /Contents"))),
    };
    match t {
        Object::Array(a) => {
            let held = charge_elems(&mem, a.len())?;
            let mut v = Vec::new();
            v.try_reserve_exact(a.len().saturating_add(2))?;
            v.extend(a.iter().cloned());
            Ok(Target::Array(v, held))
        }
        Object::Stream(_) => Ok(Target::Stream),
        other => Ok(Target::Other(kind_name(other))),
    }
}

fn not_stream_or_array(name: &str) -> Error {
    Error::Malformed(format!(
        "/Contents indirect reference resolves to {name}, not a stream or array"
    ))
}

/// Writes the extended array back into the xref entry of `r`. Returning it is not enough:
/// `dereference` hands out the table entry's own object (Go's `appendToIndirectContents`).
fn set_array_entry(xt: &mut XRefTable, r: Ref, v: Vec<Object>) -> Result<()> {
    let entry = xt
        .entry_mut(r.num)
        .ok_or_else(|| Error::Malformed("/Contents array has no cross-reference entry".into()))?;
    entry.object = Some(EntryObject::Obj(Object::Array(v)));
    Ok(())
}

/// `appendToIndirectContents`: the page's new `/Contents` when it must change (`None` when the
/// array object itself was extended).
fn append_to_indirect_contents(xt: &mut XRefTable, r: Ref, add: Ref) -> Result<Option<Object>> {
    match contents_target(xt, r)? {
        Target::Array(mut v, _held) => {
            v.push(Object::Ref(add));
            set_array_entry(xt, r, v)?;
            Ok(None)
        }
        Target::Stream => Ok(Some(Object::Array(vec![Object::Ref(r), Object::Ref(add)]))),
        Target::Other(name) => Err(not_stream_or_array(name)),
    }
}

/// `wrapIndirectContents`: as [`append_to_indirect_contents`], bracketing with `before` and
/// `after`.
fn wrap_indirect_contents(
    xt: &mut XRefTable,
    r: Ref,
    before: Ref,
    after: Ref,
) -> Result<Option<Object>> {
    match contents_target(xt, r)? {
        Target::Array(t, _held) => {
            let _copy = charge_elems(&xt.mem().clone(), t.len())?;
            let mut v = Vec::new();
            v.try_reserve_exact(t.len().saturating_add(2))?;
            v.push(Object::Ref(before));
            v.extend(t);
            v.push(Object::Ref(after));
            set_array_entry(xt, r, v)?;
            Ok(None)
        }
        Target::Stream => Ok(Some(Object::Array(vec![
            Object::Ref(before),
            Object::Ref(r),
            Object::Ref(after),
        ]))),
        Target::Other(name) => Err(not_stream_or_array(name)),
    }
}

/// `Contents` array of `c` (a direct array) with `head` and `tail` around it.
fn bracket(mem: &Mem, head: Option<Ref>, c: &[Object], tail: Ref) -> Result<Object> {
    let _held = charge_elems(mem, c.len())?;
    let mut v = Vec::new();
    v.try_reserve_exact(c.len().saturating_add(2))?;
    v.extend(head.map(Object::Ref));
    v.extend(c.iter().cloned());
    v.push(Object::Ref(tail));
    Ok(Object::Array(v))
}

impl Doc {
    /// `AddFontResource`: embeds `f` as an indirect font object the first time it is called
    /// for a given `f.base_font` on this document, then binds it into page `n`'s `/Font`
    /// resource dictionary under a name no other font on that page uses, and returns that name.
    ///
    /// # Errors
    /// The pdf-rs errors of encoding the font file and of the page lookup, and
    /// [`Error::Alloc`] when `max_alloc` is exceeded.
    pub fn add_font_resource(
        &mut self,
        n: usize,
        f: &TrueTypeFont,
        cancel: &AtomicBool,
    ) -> Result<String> {
        let font = match self.font_refs.get(&f.base_font) {
            Some(r) => *r,
            None => {
                let r = self.embed_font(f, cancel)?;
                self.font_refs.insert(f.base_font.clone(), r);
                r
            }
        };
        self.bind_font(n, font, cancel)
    }

    /// The font objects of `f`: `/FontFile2` stream, `/FontDescriptor`, `/Font`.
    fn embed_font(&mut self, f: &TrueTypeFont, cancel: &AtomicBool) -> Result<Ref> {
        let xt = &mut self.ctx.xref;
        let mem: Mem = xt.mem().clone();

        let program = mem
            .bytes_from(&f.program, f.program.len())
            .map_err(|e| font_err(f, e))?;
        let mut fsd = xt.new_stream_dict_for_buf(program);
        // /Length1 is the length of the UNCOMPRESSED font program (ISO 32000-1 table 126). It
        // must be set before encode(), which compresses the content and leaves Length1 alone.
        fsd.dict.insert_int(b"Length1", to_i64(f.program.len()));
        fsd.encode(&mem, cancel)
            .map_err(|e| font_err(f, e.context("encode font file")))?;
        let font_file = xt
            .ind_ref_for_new_object(Object::Stream(Box::new(fsd)))
            .map_err(|e| font_err(f, e))?;

        // The widths array is caller-sized: charge it before building it.
        let width_bytes = f.widths.len().saturating_mul(std::mem::size_of::<Object>());
        let held = mem.charge(width_bytes as u64).map_err(|e| font_err(f, e))?;
        let mut widths = Vec::new();
        widths.try_reserve_exact(f.widths.len())?;
        widths.extend(f.widths.iter().map(|&w| Object::Integer(w)));
        drop(held);
        let bbox: Vec<Object> = f.font_bbox.iter().map(|&v| Object::Integer(v)).collect();

        let name = || Object::Name(f.base_font.as_bytes().to_vec());
        let mut descr = Dict::default();
        descr.set(b"Type", Object::Name(b"FontDescriptor".to_vec()));
        descr.set(b"FontName", name());
        descr.set(b"Flags", Object::Integer(f.flags));
        descr.set(b"FontBBox", Object::Array(bbox));
        descr.set(b"ItalicAngle", Object::Integer(f.italic_angle));
        descr.set(b"Ascent", Object::Integer(f.ascent));
        descr.set(b"Descent", Object::Integer(f.descent));
        descr.set(b"CapHeight", Object::Integer(f.cap_height));
        descr.set(b"StemV", Object::Integer(f.stem_v));
        descr.set(b"FontFile2", Object::Ref(font_file));
        let descr_ref = xt
            .ind_ref_for_new_object(Object::Dict(descr))
            .map_err(|e| font_err(f, e))?;

        let last_char = f
            .first_char
            .checked_add(to_i64(f.widths.len()))
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| {
                Error::Malformed(format!("font {}: /LastChar overflows", f.base_font))
            })?;
        let mut font = Dict::default();
        font.set(b"Type", Object::Name(b"Font".to_vec()));
        font.set(b"Subtype", Object::Name(b"TrueType".to_vec()));
        font.set(b"BaseFont", name());
        font.set(b"FirstChar", Object::Integer(f.first_char));
        font.set(b"LastChar", Object::Integer(last_char));
        font.set(b"Widths", Object::Array(widths));
        font.set(b"Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        font.set(b"FontDescriptor", Object::Ref(descr_ref));
        xt.ind_ref_for_new_object(Object::Dict(font))
            .map_err(|e| font_err(f, e))
    }

    /// Binds `font` into page `n`'s `/Font` resources under a free `BbGl<i>` name.
    ///
    /// The page dictionary is a copy (see the module doc), so the lookup is by location: a
    /// `/Resources` or `/Font` that is an indirect object is changed where it is stored, a
    /// direct one in the copy, which is then written back.
    ///
    /// Resource consolidation is off (`page_dict` never does it): consolidating prunes the
    /// resources down to what the page's own content names and never walks a form XObject or
    /// an annotation appearance, so it would drop entries other content still needs.
    fn bind_font(&mut self, n: usize, font: Ref, cancel: &AtomicBool) -> Result<String> {
        let xt = &mut self.ctx.xref;
        let mem: Mem = xt.mem().clone();
        let (mut pd, mut pref, inh) = xt
            .page_dict(page_nr(n), cancel)
            .map_err(|e| page_err(n, "dict", e))?;

        // The page's own /Resources, if it has one; otherwise adopt the inherited one rather
        // than creating an empty own dictionary, which would discard whatever the page inherits.
        let mut res_obj = pd.find(b"Resources").cloned().unwrap_or(Object::Null);
        let mut _res_held = charge_clone(&mem, &res_obj)?;
        // Go adopts the ancestor's stored /Resources map: bind in the ancestor, and give the page
        // that map when the document is written. Only the node's /Resources is read and written
        // back, so only it is copied.
        let mut adopter = None;
        let mut _owner_held = None;
        if dict_loc(xt, &res_obj).is_none()
            && let Some(owner) = inh.resources_owner
            && let Ok(Some(od)) = xt.dereference_dict_ref(owner)
        {
            let r = od.find(b"Resources").unwrap_or(&Object::Null);
            _owner_held = Some(charge_clone(&mem, r)?);
            _res_held = charge_clone(&mem, r)?;
            res_obj = r.clone();
            pd = Dict::default();
            pd.set(b"Resources", res_obj.clone());
            adopter = Some((pref, owner));
            pref = owner;
        }
        let res_loc = match dict_loc(xt, &res_obj) {
            Some(l) => l,
            None => {
                let adopted = inh.resources.unwrap_or_default();
                pd.set(b"Resources", Object::Dict(adopted));
                Loc::Direct
            }
        };
        let fonts_obj = res_mut(xt, &mut pd, res_loc)?
            .find(b"Font")
            .cloned()
            .unwrap_or(Object::Null);
        let _fonts_held = charge_clone(&mem, &fonts_obj)?;
        let fonts_loc = match dict_loc(xt, &fonts_obj) {
            Some(l) => l,
            None => {
                res_mut(xt, &mut pd, res_loc)?.set(b"Font", Object::Dict(Dict::default()));
                Loc::Direct
            }
        };

        let name = {
            let fonts = fonts_mut(xt, &mut pd, res_loc, fonts_loc)?;
            let mut i = 0usize;
            loop {
                let name = format!("BbGl{i}");
                if fonts.find(name.as_bytes()).is_none() {
                    fonts.set(name.as_bytes(), Object::Ref(font));
                    break name;
                }
                i += 1;
            }
        };

        if let (Loc::Direct, Some(res)) = (res_loc, pd.0.remove(&b"Resources"[..])) {
            let page = xt.dereference_dict_mut(pref)?.ok_or_else(|| gone("page"))?;
            page.set(b"Resources", res);
        }
        if let Some(a) = adopter
            && !self.res_adopters.contains(&a)
        {
            self.res_adopters.try_reserve(1)?;
            self.res_adopters.push(a);
        }
        Ok(name)
    }

    /// Gives every page in `res_adopters` the final `/Resources` of the page tree node it
    /// inherited them from, as a direct dictionary: Go's page holds the node's map itself, so it
    /// is written with every binding made through it, also those of later pages.
    ///
    /// The copies stay in the document, so the returned charge (each copy's deep size) is to be
    /// held for as long as they are: N pages adopting a node whose `/Font` gained a name per page
    /// hold N^2 entries where Go holds one map.
    ///
    /// # Errors
    /// [`Error::Alloc`] for the copies, [`Error::Cancelled`], and the pdf-rs errors of the page
    /// lookup.
    pub(crate) fn store_adopted_resources(&mut self, cancel: &AtomicBool) -> Result<Held> {
        let xt = &mut self.ctx.xref;
        let mut held = xt.mem().hold();
        for &(page, owner) in &self.res_adopters {
            check_cancel(cancel)?;
            // A /Resources that is a reference is followed even when it names the node itself:
            // Go's page then adopts the node's whole dictionary.
            let at = match xt
                .dereference_dict_ref(owner)?
                .and_then(|od| od.find(b"Resources"))
            {
                Some(Object::Ref(r)) => Some(*r),
                Some(Object::Dict(_)) => None,
                _ => continue,
            };
            let res = match at {
                Some(r) => match xt.dereference_dict_ref(r)? {
                    Some(res) => res,
                    None => continue,
                },
                None => match xt
                    .dereference_dict_ref(owner)?
                    .and_then(|od| od.find(b"Resources"))
                {
                    Some(Object::Dict(res)) => res,
                    _ => continue,
                },
            };
            held.grow_by(pdf::object::dict_copy_bytes(res))?;
            let res = res.clone();
            let pd = xt.dereference_dict_mut(page)?.ok_or_else(|| gone("page"))?;
            pd.set(b"Resources", Object::Dict(res));
        }
        Ok(held)
    }

    /// `newContentStream`: `ops` as a new, encoded content stream object.
    fn new_content_stream(&mut self, ops: &[u8], cancel: &AtomicBool) -> Result<Ref> {
        let xt = &mut self.ctx.xref;
        let mem: Mem = xt.mem().clone();
        let buf = mem.bytes_from(ops, ops.len())?;
        let mut sd = xt.new_stream_dict_for_buf(buf);
        sd.encode(&mem, cancel)?;
        Ok(xt.ind_ref_for_new_object(Object::Stream(Box::new(sd)))?)
    }

    /// Sets the page's `/Contents` through its reference.
    fn set_contents(&mut self, pref: Ref, v: Object) -> Result<()> {
        let page = self
            .ctx
            .xref
            .dereference_dict_mut(pref)?
            .ok_or_else(|| gone("page"))?;
        page.set(b"Contents", v);
        Ok(())
    }

    /// `AppendContent`: appends `ops` to page `n`'s content, first countering whatever net
    /// transform the existing content left in effect, so `ops` execute in PDF default user
    /// space regardless of what came before them.
    ///
    /// The net transform is not just the q/Q depth: a `cm` outside any q/Q pair (Chrome/Skia
    /// start with a y-flip) changes the CTM permanently. [`net_ctm`] walks the whole stream and
    /// `ops` are wrapped in `q`, the inverse `cm`, `ops`, `Q`. A new stream is appended to
    /// `/Contents`; the existing streams are never decoded or rewritten.
    ///
    /// # Errors
    /// A page whose content fails to decode (not a blank page), a `/Contents` that is a direct
    /// stream or an indirect non-stream, and the pdf-rs errors of the page lookup.
    pub fn append_content(&mut self, n: usize, ops: &[u8], cancel: &AtomicBool) -> Result<()> {
        let (pd, pref, _) = self
            .ctx
            .xref
            .page_dict(page_nr(n), cancel)
            .map_err(|e| page_err(n, "dict", e))?;

        // A page that decodes to zero bytes leaves the CTM at identity; a page whose streams
        // failed to decode is refused by page_contents. Recovered content is used AS content.
        let mem: Mem = self.ctx.xref.mem().clone();
        let (cur, _) = self.page_contents(&pd, cancel)?;
        let ctm = if cur.is_empty() {
            IDENTITY
        } else {
            net_ctm(&cur, &mem, cancel)?
        };

        let cap = ops.len().saturating_add(256);
        let _held = mem.charge(cap as u64)?;
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(cap)?;
        buf.extend_from_slice(b"q\n");
        // A singular net CTM cannot be inverted; falling through without a correcting cm
        // matches the behaviour from before transform tracking existed.
        if let Some(inv) = invert(&ctm).filter(|inv| *inv != IDENTITY) {
            let s = inv.0.map(|v| format_f(v, 10)).join(" ");
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b" cm\n");
        }
        buf.extend_from_slice(ops);
        buf.extend_from_slice(b"\nQ\n");

        let add = self.new_content_stream(&buf, cancel)?;
        let xt = &mut self.ctx.xref;
        let new_contents = match pd.find(b"Contents") {
            Some(Object::Ref(c)) => append_to_indirect_contents(xt, *c, add)?,
            Some(Object::Array(c)) => {
                let _held = charge_elems(&mem, c.len())?;
                let mut v = Vec::new();
                v.try_reserve_exact(c.len().saturating_add(1))?;
                v.extend(c.iter().cloned());
                v.push(Object::Ref(add));
                Some(Object::Array(v))
            }
            None | Some(Object::Null) => Some(Object::Array(vec![Object::Ref(add)])),
            // ISO 32000-1 7.3.8.1: every stream shall be an indirect object.
            Some(_) => return Err(direct_contents(n)),
        };
        match new_contents {
            Some(v) => self.set_contents(pref, v),
            None => Ok(()),
        }
    }

    /// `WrapContent`: brackets page `n`'s whole content with `before` and `after`, as two new
    /// streams. No existing stream is decoded or rewritten; the new streams are appended (not
    /// merged) at either end of `/Contents`.
    ///
    /// Unlike [`Doc::append_content`] this needs no inverse: a prepend is the outermost
    /// transform, and the trailing `Q` the caller writes into `after` restores whatever
    /// `before`'s own `q` pushed. [`Doc::content_depth`] refuses unbalanced content first.
    ///
    /// # Errors
    /// [`Error::UnbalancedContent`], a content stream the lexer cannot tokenize, a direct
    /// `/Contents` stream, and the pdf-rs errors of the page lookup.
    pub fn wrap_content(
        &mut self,
        n: usize,
        before: &[u8],
        after: &[u8],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let (pd, pref, _) = self
            .ctx
            .xref
            .page_dict(page_nr(n), cancel)
            .map_err(|e| page_err(n, "dict", e))?;

        self.content_depth(n, cancel)?;

        let before_ref = self.new_content_stream(before, cancel)?;
        let after_ref = self.new_content_stream(after, cancel)?;

        let mem: Mem = self.ctx.xref.mem().clone();
        let xt = &mut self.ctx.xref;
        let new_contents = match pd.find(b"Contents") {
            Some(Object::Ref(c)) => wrap_indirect_contents(xt, *c, before_ref, after_ref)?,
            Some(Object::Array(c)) => Some(bracket(&mem, Some(before_ref), c, after_ref)?),
            // No content to rotate, but the wrapper is still written so the record and the
            // file agree.
            None | Some(Object::Null) => Some(Object::Array(vec![
                Object::Ref(before_ref),
                Object::Ref(after_ref),
            ])),
            Some(_) => return Err(direct_contents(n)),
        };
        match new_contents {
            Some(v) => self.set_contents(pref, v),
            None => Ok(()),
        }
    }

    /// `contentDepth`: the net q/Q nesting page `n`'s content leaves behind. A page with no
    /// content leaves none (0).
    ///
    /// Any imbalance, a final depth that is not zero OR a `Q` ever seen with no `q` open
    /// (even if later `q`s bring the count back to zero), is [`Error::UnbalancedContent`]. A
    /// stream the lexer cannot fully tokenize is refused with the lexer's error: its prefix
    /// cannot prove the rest balanced.
    ///
    /// # Errors
    /// As above, plus the errors of the page lookup and of `page_contents`.
    pub fn content_depth(&mut self, n: usize, cancel: &AtomicBool) -> Result<usize> {
        let (pd, _, _) = self
            .ctx
            .xref
            .page_dict(page_nr(n), cancel)
            .map_err(|e| page_err(n, "dict", e))?;
        let (cur, _) = self.page_contents(&pd, cancel)?;
        if cur.is_empty() {
            return Ok(0);
        }
        let (depth, surplus_q) = content_q_depth(&cur, cancel)?;
        if surplus_q || depth != 0 {
            return Err(Error::UnbalancedContent);
        }
        Ok(depth)
    }
}

/// `contentQDepth`: walks `src` the way [`net_ctm`] does, lexing with `crate::content` rather
/// than splitting on whitespace (a naive split miscounts an operator-looking string or inline
/// image payload). Returns the final depth and whether a `Q` was seen at depth zero, which
/// pops state this stream never pushed and can still leave the depth at zero.
///
/// # Errors
/// The lexer's error for a stream it cannot fully tokenize (a stray `)` or `>`, an unterminated
/// string, ...), and [`Error::Cancelled`].
pub(crate) fn content_q_depth(src: &[u8], cancel: &AtomicBool) -> Result<(usize, bool)> {
    let mut l = Lexer::new(src);
    let mut depth = 0usize;
    let mut surplus_q = false;
    let mut tokens = 0u32;
    while let Some(tok) = l.next_token()? {
        tokens = tokens.wrapping_add(1);
        if tokens & 1023 == 0 {
            check_cancel(cancel)?;
        }
        if tok.kind != Kind::Keyword {
            continue;
        }
        match &*tok.text {
            b"q" => depth += 1,
            b"Q" => {
                if depth == 0 {
                    surplus_q = true;
                } else {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }
    Ok((depth, surplus_q))
}

/// `matrix.invert`: the inverse, or `None` if `m` is singular (determinant 0: a degenerate
/// scale to zero, which no matrix undoes).
fn invert(m: &Matrix) -> Option<Matrix> {
    let [a, b, c, d, e, f] = m.0;
    let det = a * d - b * c;
    if det == 0.0 {
        return None;
    }
    let ia = d / det;
    let ib = -b / det;
    let ic = -c / det;
    let id = a / det;
    let ie = -(e * ia + f * ic);
    let iff = -(e * ib + f * id);
    Some(Matrix([ia, ib, ic, id, ie, iff]))
}

/// `netCTM`: walks `src` the way a PDF interpreter would, tracking `q` (push), `Q` (pop) and
/// `cm` (concatenate), and returns the CTM in effect at the end, starting from identity. Other
/// operators never change the CTM and are ignored. A lexer error ends the walk with what was
/// read so far, as in Go.
///
/// The `q` stack is charged to `mem` (its high-water mark, one matrix per level).
///
/// # Errors
/// [`Error::Alloc`], [`Error::OutOfMemory`] and [`Error::Cancelled`].
pub fn net_ctm(src: &[u8], mem: &Mem, cancel: &AtomicBool) -> Result<Matrix> {
    const LEVEL: u64 = std::mem::size_of::<Matrix>() as u64;
    let mut l = Lexer::new(src);
    let mut m = IDENTITY;
    let mut stack: Vec<Matrix> = Vec::new();
    let mut held = mem.charge(0)?;
    let mut high = 0usize;
    // The last six numbers, oldest first, and how many were read since the last operator.
    let mut nums = [0.0f64; 6];
    let mut count = 0usize;
    let mut tokens = 0u32;
    loop {
        tokens = tokens.wrapping_add(1);
        if tokens & 1023 == 0 {
            check_cancel(cancel)?;
        }
        let Ok(Some(tok)) = l.next_token() else {
            return Ok(m);
        };
        match tok.kind {
            Kind::Number => {
                nums.rotate_left(1);
                nums[5] = tok.num;
                count = count.saturating_add(1);
            }
            Kind::Keyword => {
                match &*tok.text {
                    b"cm" => {
                        if count >= 6 {
                            m = Matrix(nums).mul(&m);
                        }
                    }
                    b"q" => {
                        stack.try_reserve(1)?;
                        stack.push(m);
                        if stack.len() > high {
                            high = stack.len();
                            held.grow_by(LEVEL)?;
                        }
                    }
                    b"Q" => {
                        if let Some(p) = stack.pop() {
                            m = p;
                        }
                    }
                    _ => {}
                }
                count = 0;
            }
            _ => count = 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn ctm(src: &str) -> Matrix {
        let mem = Mem::with_max(1 << 20);
        net_ctm(src.as_bytes(), &mem, &no_cancel()).unwrap()
    }

    #[test]
    fn fonts_bind_in_the_node_that_holds_inherited_resources() {
        let pdf = super::super::annots::build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 100 100] \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ]);
        let mut d = Doc::open_limits(&pdf, &crate::Limits::default(), &no_cancel()).unwrap();
        let font = crate::stamp::glyphless_true_type_font();
        // As Go: the second page sees the first page's binding in the shared map.
        assert_eq!(
            d.add_font_resource(1, &font, &no_cancel()).unwrap(),
            "BbGl0"
        );
        assert_eq!(
            d.add_font_resource(2, &font, &no_cancel()).unwrap(),
            "BbGl1"
        );
        d.store_adopted_resources(&no_cancel()).unwrap();
        for num in [2, 3, 4] {
            let node = d
                .ctx
                .xref
                .dereference_dict_ref(Ref { num, generation: 0 })
                .unwrap()
                .unwrap();
            let Some(Object::Dict(res)) = node.find(b"Resources") else {
                panic!("obj {num}: no direct /Resources");
            };
            let Some(Object::Dict(fonts)) = res.find(b"Font") else {
                panic!("obj {num}: no direct /Font");
            };
            for name in [&b"F1"[..], b"BbGl0", b"BbGl1"] {
                assert!(fonts.find(name).is_some(), "obj {num}");
            }
        }
    }

    #[test]
    fn adopted_resources_copies_are_charged_deep() {
        // Every adopting page takes a copy of the node's /Resources, whose /Font gains a name per
        // page: N pages hold N^2 entries, which max_alloc must see.
        const N: usize = 300;
        let kids: Vec<String> = (3..3 + N).map(|i| format!("{i} 0 R")).collect();
        let mut bodies = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {N} /MediaBox [0 0 100 100] \
                 /Resources << /ProcSet [/PDF] >> >>",
                kids.join(" ")
            ),
        ];
        bodies.extend((0..N).map(|_| "<< /Type /Page /Parent 2 0 R >>".to_string()));
        let pdf = super::super::annots::build_pdf(&bodies);
        let font = crate::stamp::glyphless_true_type_font();
        let stamp = |max_alloc| {
            let lim = crate::Limits {
                max_alloc,
                ..crate::Limits::default()
            };
            let mut d = Doc::open_limits(&pdf, &lim, &no_cancel()).unwrap();
            for n in 1..=N {
                d.add_font_resource(n, &font, &no_cancel()).unwrap();
            }
            d.store_adopted_resources(&no_cancel())
        };
        let copies = stamp(0).unwrap().bytes();
        assert!(copies >= (N * N * 100) as u64, "{copies}");
        let e = stamp(copies / 2).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }), "{e:?}");
    }

    #[test]
    fn adopted_resources_follow_a_reference_to_the_node_itself() {
        // Go's page adopts DereferenceDict(2 0 R), the node's own dictionary.
        let pdf = super::super::annots::build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 100 100] /Resources 2 0 R >>"
                .into(),
            "<< /Type /Page /Parent 2 0 R >>".into(),
        ]);
        let mut d = Doc::open_limits(&pdf, &crate::Limits::default(), &no_cancel()).unwrap();
        let font = crate::stamp::glyphless_true_type_font();
        assert_eq!(
            d.add_font_resource(1, &font, &no_cancel()).unwrap(),
            "BbGl0"
        );
        d.store_adopted_resources(&no_cancel()).unwrap();
        let page = d
            .ctx
            .xref
            .dereference_dict_ref(Ref {
                num: 3,
                generation: 0,
            })
            .unwrap()
            .unwrap();
        let Some(Object::Dict(res)) = page.find(b"Resources") else {
            panic!("page: no direct /Resources");
        };
        let Some(Object::Dict(fonts)) = res.find(b"Font") else {
            panic!("page: no direct /Font");
        };
        assert!(fonts.find(b"BbGl0").is_some());
    }

    #[test]
    fn net_ctm_tracks_cm_outside_q() {
        // A Skia-style y-flip outside any q/Q survives to the end.
        let m = ctm("1 0 0 -1 0 792 cm q 2 0 0 2 0 0 cm Q BT ET");
        assert_eq!(m, Matrix([1.0, 0.0, 0.0, -1.0, 0.0, 792.0]));
    }

    #[test]
    fn net_ctm_ignores_operator_text_in_strings() {
        assert_eq!(ctm("(1 0 0 -1 0 792 cm) Tj"), IDENTITY);
    }

    #[test]
    fn net_ctm_needs_six_operands_and_ignores_surplus_q() {
        assert_eq!(ctm("1 2 3 cm Q Q"), IDENTITY);
    }

    #[test]
    fn invert_round_trips_and_refuses_singular() {
        let m = Matrix([1.0, 0.0, 0.0, -1.0, 0.0, 792.0]);
        let inv = invert(&m).unwrap();
        assert_eq!(m.mul(&inv), IDENTITY);
        assert!(invert(&Matrix([0.0; 6])).is_none());
    }

    #[test]
    fn q_depth_counts_nesting_and_surplus_q() {
        let c = no_cancel();
        assert_eq!(content_q_depth(b"q q Q", &c).unwrap(), (1, false));
        assert_eq!(content_q_depth(b"Q q", &c).unwrap(), (1, true));
        assert_eq!(content_q_depth(b"(q) Tj q Q", &c).unwrap(), (0, false));
        // A Q followed by a q washes the count out; the flag still reports it.
        assert_eq!(content_q_depth(b"Q q Q", &c).unwrap(), (0, true));
    }

    #[test]
    fn q_depth_refuses_a_stream_it_cannot_lex() {
        assert!(content_q_depth(b"q ) Q", &no_cancel()).is_err());
    }

    // Go tests that need a corpus PDF and the Doc open/write path of doc.rs; they are written
    // against tests/data/corpus/<name>.pdf once those exist.

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn add_font_resource_embeds_font_file2_hermetically() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn stamped_font_and_content_validate_cleanly() {}

    #[test]
    #[ignore = "needs tests/data/corpus/blank-page.pdf from the Go oracle -gen"]
    fn append_content_on_a_blank_page_succeeds() {}

    #[test]
    #[ignore = "needs tests/data/corpus/corrupt-content-stream.pdf and corrupt-content-stream-in-array.pdf; corpus.CorruptContentStream{,InArray} are exported helpers that -gen does not write by name, so the oracle needs an explicit generator for them; bodies wait for Doc::open in doc.rs"]
    fn append_content_on_a_corrupt_content_stream_is_an_error() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn wrap_content_wraps_a_contents_array() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn wrap_content_refuses_a_direct_contents_stream() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn wrap_content_refuses_unbalanced_content() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn wrap_content_refuses_when_content_fails_to_lex() {}

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn wrap_content_validates() {}
}
