//! `build_from_pages`: materialise a document from a page sequence (byb-yul.4; Go:
//! internal/pdfdoc/buildpages.go).
//!
//! This is the object-graph migration walk, and byblos owns it because pdfcpu does not offer
//! one: `InsertPages` inserts BLANK pages, `Merge` and `MergeRaw` append WHOLE documents with no
//! position argument, and its own `migrateIndRef` and `migratePageDict` are unexported.
//!
//! IT IS THE SAME WALK EVEN FROM ONE SOURCE: there is no cheaper single-source route that is
//! not in-place splicing, and splicing cannot serve a sequence spanning two documents.
//!
//! WHY A FRESH CATALOG AND NOT A GRAFT ONTO THE FIRST SOURCE'S. The catalog's entries describe
//! the page SET or the page ORDER (/PageLabels, /StructTreeRoot, /Outlines, /OpenAction,
//! /AcroForm, /Names: 61.9% of the 5,063 multi-page documents of the pinned sample carry at
//! least one), and an edit invalidates them silently. Dropping the entry is honest; carrying a
//! stale one is not. See the design spec's 2026-08-13 amendment.
//!
//! Two known defects of the Go measurement (9,798 builds over the pinned sample) that pdf-rs's
//! writer inherits, named rather than left to be rediscovered:
//!
//! * A dangling reference on 11 of 4,899 documents (0.22%): an entry this walk migrated but the
//!   writer does not follow leaves a reference naming an object the file never defines
//!   (vendor-private page keys such as /CREO_Tools, /AAPL:PPK, and /Resources/Properties/MC0).
//!   ISO 32000-1 7.3.10 makes such a reference the null object. This port serialises the output
//!   itself (`selfwrite`), as Go does after byb-yul.6.
//! * An output that pdfcpu's validator refuses, on 28 of 4,899 (0.57%): carried from inputs that
//!   already fail the same validator.
//!
//! OUTPUT OBJECT NUMBERS follow the order objects are first reached. Go copies dictionaries in
//! map iteration order, so its numbering is random per run; this port copies in sorted key order
//! (the `Dict` is a `BTreeMap`), which is what the parity oracle's patched Go does
//! (docs/go-differences.md).
//!
//! Assumes of the sibling ports: `Doc::open_limits(data, &Limits, cancel)` and the `Doc.ctx`
//! field, and `linearize::{Walk, walk_page_tree}` (`pub(super)` there).

use super::Doc;
use super::annots::deref_obj;
use super::doc::open_with_mem;
use super::linearize::{Walk, walk_page_tree};
use super::selfwrite::{PRODUCER, write_document};
use super::straighten::{StraightenSpec, apply_straighten};
use crate::Limits;
use crate::error::{Error, Result};
use crate::go::fmt::format_g;
use crate::limits::check_cancel;
use pdf::doc::create::{create_context, create_xref_table_with_root_dict_with_mem};
use pdf::model::{Context, EntryObject, XRefTable};
use pdf::object::text::escaped_utf16_string;
use pdf::object::{Dict, Object, Ref, StreamDict};
use pdf::{Held, Mem};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// US Letter, the box every reader falls back to (`defaultPageWidthPt` of pdfdoc.go).
const DEFAULT_PAGE_WIDTH_PT: i64 = 612;
const DEFAULT_PAGE_HEIGHT_PT: i64 = 792;

/// Names one page of one document, and the rotation to give it.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSource {
    /// The document to take the page from: an index into the `sources` of the build. Two
    /// entries naming the SAME index are one logical source, opened once and shared (Go:
    /// two entries naming the same `io.ReadSeeker`).
    pub source: usize,
    /// 1-based, in the source.
    pub page: usize,
    /// The ABSOLUTE `/Rotate` to give the page: 0, 90, 180 or 270. It is not added to whatever
    /// the source page declares.
    pub rotate: i32,
    /// A lossless rotation of the page's content, `None` for none. See [`StraightenSpec`] for
    /// the sign convention and the absolute-not-delta contract (byb-16j.4).
    pub straighten: Option<StraightenSpec>,
}

/// Writes a document whose page `i` is `pages[i]`.
///
/// Deleting a page is omitting it from the sequence, reordering is ordering the sequence,
/// inserting is naming a different source, and rotating is a field. The output is a new
/// document every time; no source is modified. Nothing is returned unless the whole document
/// was built.
///
/// # Errors
/// As [`build_from_pages_with_properties`].
pub fn build_from_pages(
    sources: &[&[u8]],
    pages: &[PageSource],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    build_from_pages_with_properties(sources, pages, &BTreeMap::new(), limits, cancel)
}

/// [`build_from_pages`], with `properties` merged into the output's Info dictionary as part of
/// the SAME build and the SAME write (byb-yul.6, Correction 5).
///
/// THIS IS WHY IT EXISTS, RATHER THAN A CALLER STAMPING PROPERTIES ON AFTERWARDS. The caller
/// used to build the document and then pipe the bytes through `write_properties` for its
/// provenance record, and that goes through pdf-go's own read-validate-optimize-write pass, the
/// SAME catalog-traversal writer this module's own writer exists to replace. Measured: every
/// dangling-reference shape the module comment describes came BACK on that second pass.
///
/// `properties` is encoded exactly the way pdf-go's `PropertiesAdd` encodes it
/// (`escaped_utf16_string`), so `read_properties` decodes it back exactly as it always has. A
/// key already claimed by `carry_info`'s allowlist or by the /Producer stamp is silently
/// overwritten by `properties`, last-write-wins; nothing today passes one of those keys here.
/// Keys apply in sorted order.
///
/// Every page's source is opened under `limits`, each distinct one once. The sources and the
/// output share one `max_alloc` ledger.
///
/// # Errors
/// [`Error::Cancelled`]; `Malformed` for an unbuildable sequence (see [`validate_pages`]), a
/// page past the end of its source, an encrypted source, and a source page tree that disagrees
/// with its page count; whatever `Doc::open_limits`, pdf-rs and the writer return.
pub fn build_from_pages_with_properties(
    sources: &[&[u8]],
    pages: &[PageSource],
    properties: &BTreeMap<String, String>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    build_with_mem(sources, pages, properties, limits, &limits.mem(), cancel)
}

/// [`build_from_pages_with_properties`] on the caller's ledger `mem`.
///
/// # Errors
/// As [`build_from_pages_with_properties`].
pub(crate) fn build_with_mem(
    sources: &[&[u8]],
    pages: &[PageSource],
    properties: &BTreeMap<String, String>,
    limits: &Limits,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    validate(pages, Some(sources.len()))?;
    let mut docs = open_sources(sources, pages, limits, mem, cancel)?;
    for (i, p) in pages.iter().enumerate() {
        let n = docs.get(&p.source).map_or(0, |d| d.ctx.xref.page_count);
        if p.page < 1 || i64::try_from(p.page).unwrap_or(i64::MAX) > n {
            return Err(Error::Malformed(format!(
                "build from pages: page {} takes source page {}, out of range 1..{n}",
                i + 1,
                p.page
            )));
        }
    }
    let ctx = build_context(pages, &mut docs, properties, limits, mem, cancel)?;
    drop(docs);
    let ctx = apply_straighten(ctx, pages, limits, cancel)?;
    write_document(&ctx.xref, cancel)
}

/// Rejects what cannot be resolved without opening a document.
///
/// For a pre-build check: a caller's only way to learn that an edit list is unbuildable without
/// paying for the build. (A `source` index past the end of the `sources` of the build is caught
/// by the build itself, which is the only place that knows how many there are.)
///
/// The rotation check is not defensive tidiness. pdf-go writes /Rotate 45 and /Rotate -90 with
/// no error, and the 45 case produces a file it then refuses on re-read, so an unchecked
/// rotation is a document that fails at the next reader rather than at this call. 360 is
/// rejected with the rest: it is a legal quarter-turn multiple and it is not one of the four
/// values this takes, and silently folding it to 0 would accept 720 too.
///
/// `PageInfo.Rotate` is NOT a safe way to produce this field. It normalizes into [0,360), so a
/// declared -90 reads back as 270, and it does not guarantee a multiple of 90: a declared 45
/// reads back as 45. A caller round-tripping through it must still be told when the value it
/// got cannot be written.
///
/// # Errors
/// `Malformed`, naming the first page that cannot be built.
pub fn validate_pages(pages: &[PageSource]) -> Result<()> {
    validate(pages, None)
}

/// `ValidatePages`, with the source indices checked against `nsources` when it is known.
fn validate(pages: &[PageSource], nsources: Option<usize>) -> Result<()> {
    if pages.is_empty() {
        return Err(Error::Malformed("build from pages: no pages".to_string()));
    }
    for (i, p) in pages.iter().enumerate() {
        let n = i + 1;
        if nsources.is_some_and(|len| p.source >= len) {
            return Err(Error::Malformed(format!(
                "build from pages: page {n} has no source"
            )));
        }
        if !matches!(p.rotate, 0 | 90 | 180 | 270) {
            return Err(Error::Malformed(format!(
                "build from pages: page {n} asks for rotation {}, which is not one of 0, 90, 180, 270",
                p.rotate
            )));
        }
        if let Some(s) = &p.straighten {
            // A non-finite angle produces a `cm` of non-finite numbers, which is written and
            // then refused by the validator: the same shape of failure as /Rotate 45, caught
            // here at the call rather than at the next reader. An angle outside (-180, 180] is
            // NOT refused: the arithmetic takes it modulo 360 by construction (design spec
            // section 2).
            if !s.deg.is_finite() {
                return Err(Error::Malformed(format!(
                    "build from pages: page {n} straighten angle {} is not finite",
                    format_g(s.deg)
                )));
            }
            // Crop is declared in the contract and not implemented in this version (design
            // spec section 6); refusing it is what lets the field exist now without a caller
            // silently getting a page that ignored it.
            if s.crop.is_some() {
                return Err(Error::Malformed(format!(
                    "build from pages: page {n} straighten crop is not implemented"
                )));
            }
        }
    }
    Ok(())
}

/// The opened sources, by their index in the `sources` of the build.
type Docs = BTreeMap<usize, Doc>;

/// Opens each distinct source exactly once, all on `mem`.
///
/// Once, for two reasons. A document opened twice is parsed twice and held in memory twice, and
/// export memory is already the binding constraint. And every object migrated out of it would
/// be migrated twice, so a font or a raster shared by two exported pages would be written to
/// the output twice.
fn open_sources(
    sources: &[&[u8]],
    pages: &[PageSource],
    limits: &Limits,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Docs> {
    let mut out = Docs::new();
    for (i, p) in pages.iter().enumerate() {
        if out.contains_key(&p.source) {
            continue;
        }
        let Some(data) = sources.get(p.source) else {
            return Err(Error::Malformed(format!(
                "build from pages: page {} has no source",
                i + 1
            )));
        };
        check_cancel(cancel)?;
        let d = open_with_mem(data, limits, mem, cancel)?;
        if d.ctx.xref.encrypt.is_some() {
            // The same refusal Linearize makes, for the same reason. pdf-go decrypts strings
            // and streams on READ, so migrating them into an output with no /Encrypt
            // dictionary silently STRIPS the encryption from a document somebody chose to
            // encrypt, and re-emitting them under the source's /Encrypt would produce a file
            // that opens as garbage.
            //
            // A document with a user password never reaches here: opening fails on it. The
            // case this catches is owner-password-only, which opens with no password at all
            // and is the common one.
            return Err(Error::Malformed(format!(
                "build from pages: page {} source: the document is encrypted",
                i + 1
            )));
        }
        out.insert(p.source, d);
    }
    Ok(out)
}

/// One object of one source document: the migration's memo key, so an image or a font two
/// exported pages share is migrated once.
type SrcObj = (usize, i64);

/// Bounds the recursion. A malformed document can hold a reference cycle that the memo does not
/// break (the memo is written before the copy recurses, so a cycle terminates), but a legal
/// document can still nest resource dictionaries deeply, and an unbounded recursion on a hostile
/// file is a stack overflow the process cannot recover from.
const MAX_MIGRATION_DEPTH: u32 = 256;

/// What the copy of one node is charged against `max_alloc`, on top of the length of a name or
/// string it holds: an estimate, because pdf-rs charges only its byte buffers.
const NODE_BYTES: u64 = 64;

/// The entries of the information dictionary an export inherits.
///
/// IT IS AN ALLOWLIST, not a filter, and that is the point. Every entry here describes the
/// document's CONTENT and survives a page being deleted, reordered or rotated. Everything else
/// is excluded by saying nothing:
///
/// * /Producer and the dates name what WROTE the bytes, which is now byblos.
/// * byblos's own provenance record is under a key of its own and is POSITIONAL
///   (`Provenance.Pages` index i describes page i+1), so an export that reorders or omits a page
///   carries a record onto the wrong page. Re-indexing it is the caller's job; inheriting it
///   silently is worse than losing it. See the design spec's G3 obligation.
/// * A custom key means whatever the tool that wrote it meant. Guessing that it survives a page
///   edit is not something this can do.
const INFO_CARRIED: [&str; 5] = ["Title", "Author", "Subject", "Keywords", "Creator"];

/// The state of one build.
struct Migration<'a> {
    dest: XRefTable,
    /// The output's /Pages node, for every /Parent.
    pages: Ref,
    /// Source object -> output object number.
    done: BTreeMap<SrcObj, i64>,
    /// Each source's page leaves, in page order.
    leafs: BTreeMap<usize, Vec<i64>>,
    depth: u32,
    /// Maps a source page leaf to the output page it became, and `tree` holds every object of
    /// every source's page tree, leaves and /Pages nodes alike. Together they stop the walk
    /// from following a reference INTO a page. See [`Migration::reference`].
    exported: BTreeMap<SrcObj, Ref>,
    tree: BTreeSet<SrcObj>,
    /// The estimate of the copied nodes, charged to the output's ledger until the build ends.
    held: Held,
    cancel: &'a AtomicBool,
}

fn ref_to(n: i64) -> Ref {
    Ref {
        num: n,
        generation: 0,
    }
}

fn source(docs: &mut Docs, si: usize) -> Result<&mut Doc> {
    docs.get_mut(&si)
        .ok_or_else(|| Error::Malformed(format!("build from pages: source {si} is not open")))
}

fn build_context(
    pages: &[PageSource],
    docs: &mut Docs,
    properties: &BTreeMap<String, String>,
    limits: &Limits,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context> {
    // Go's defaultConfig(): the package defaults, not the caller's caps. The output is held to
    // the caller's memory budget, the ledger it shares with the sources, and thread count only.
    let conf_limits = Limits {
        max_alloc: limits.max_alloc,
        max_threads: limits.max_threads,
        ..Limits::default()
    };
    let cfg = conf_limits.pdf_config(conf_limits.objects());
    let xt = create_xref_table_with_root_dict_with_mem(&cfg.limits, mem)?;
    let held = xt.mem().charge(0)?;
    let mut m = Migration {
        dest: xt,
        pages: ref_to(0),
        done: BTreeMap::new(),
        leafs: BTreeMap::new(),
        depth: 0,
        exported: BTreeMap::new(),
        tree: BTreeSet::new(),
        held,
        cancel,
    };
    // The /Pages node is reserved before anything is migrated, because every migrated page's
    // /Parent has to name it.
    let pages_nr = m.dest.insert_object(Object::Null)?;
    m.pages = ref_to(pages_nr);

    // Every output page is given its object number BEFORE any page is migrated, because a link
    // annotation on page 1 may name page 9 and has to resolve to the page 9 this build will
    // produce, not to the one it came from.
    let mut kids: Vec<Object> = Vec::new();
    kids.try_reserve_exact(pages.len())?;
    for p in pages {
        check_cancel(cancel)?;
        m.ensure_leaves(docs, p.source)?;
        let leaf = m.leaf(p.source, p.page)?;
        let nr = m.dest.insert_object(Object::Null)?;
        kids.push(Object::Ref(ref_to(nr)));
        // FIRST occurrence wins. A source page named twice becomes two output pages, and a
        // destination that names it lands on the earlier one, which is what a reader jumping
        // to "that page" expects.
        m.exported.entry((p.source, leaf)).or_insert(ref_to(nr));
    }
    for (i, p) in pages.iter().enumerate() {
        check_cancel(cancel)?;
        let out = match kids.get(i) {
            Some(Object::Ref(r)) => r.num,
            _ => {
                return Err(Error::Malformed(format!(
                    "build from pages: page {} lost its reservation",
                    i + 1
                )));
            }
        };
        m.migrate_page(docs, p, out)?;
    }

    let count = kids.len();
    let mut pages_dict = Dict::default();
    pages_dict.set(b"Type", Object::name("Pages"));
    pages_dict.set(b"Kids", Object::Array(kids));
    pages_dict.set(b"Count", Object::Integer(count as i64));
    m.set(pages_nr, Object::Dict(pages_dict))?;
    let pages_ref = m.pages;
    m.dest.catalog_mut()?.set(b"Pages", Object::Ref(pages_ref));
    m.dest.page_count = count as i64;

    m.carry_info(docs)?;
    // OUTSIDE carry_info's early returns (Correction 6): carry_info gives up entirely on a
    // multi-source export or a source with no /Info, and doing the producer stamp only inside
    // it was measured to reach 90.5% of single-source exports and ZERO multi-source ones. Every
    // export gets a /Producer, carried info or not, one source or many.
    m.stamp_producer()?;
    m.set_properties(properties)?;
    let Migration { dest, .. } = m;
    Ok(create_context(dest, cfg))
}

impl Migration<'_> {
    /// Fills a reserved output object.
    ///
    /// Go's `auditSerializable`, which refused any value pdfcpu's serializer would drop on the
    /// floor, is gone: pdf-rs's `Object` enum has no such value (docs/scope.md, Wave 2).
    fn set(&mut self, nr: i64, o: Object) -> Result<()> {
        match self.dest.entry_mut(nr) {
            Some(e) => {
                e.object = Some(EntryObject::Obj(o));
                Ok(())
            }
            None => Err(Error::Malformed(format!(
                "output object {nr} has no cross-reference entry"
            ))),
        }
    }

    /// The output's Info dictionary, creating an empty one if `carry_info` left none, which it
    /// does whenever the export has no single defensible source to carry a title from.
    /// /Producer and properties both need a dictionary to land in regardless.
    fn ensure_info_dict(&mut self) -> Result<&mut Dict> {
        let nr = match self.dest.info {
            Some(r) => r.num,
            None => {
                let nr = self.dest.insert_object(Object::Null)?;
                self.set(nr, Object::Dict(Dict::default()))?;
                self.dest.info = Some(ref_to(nr));
                nr
            }
        };
        match self.dest.entry_mut(nr) {
            Some(e) => match &mut e.object {
                Some(EntryObject::Obj(Object::Dict(d))) => Ok(d),
                _ => Err(Error::Malformed(format!(
                    "output info object {nr} is not a dictionary"
                ))),
            },
            None => Err(Error::Malformed(format!(
                "output info object {nr} has no cross-reference entry"
            ))),
        }
    }

    /// Gives every export a /Producer naming what wrote it. Unlike pdf-go's own, this does not
    /// stamp /CreationDate or /ModDate: the output's object numbering is not promised stable,
    /// and a clock reading would make two builds of the identical sequence differ for no
    /// reason connected to their content.
    fn stamp_producer(&mut self) -> Result<()> {
        let d = self.ensure_info_dict()?;
        d.set(b"Producer", Object::string(PRODUCER));
        Ok(())
    }

    /// Merges `properties` into the output's Info dictionary, encoded exactly the way pdf-go's
    /// `PropertiesAdd` encodes them, so `read_properties` decodes them back exactly as it
    /// always has (byb-yul.6, Correction 5).
    fn set_properties(&mut self, properties: &BTreeMap<String, String>) -> Result<()> {
        if properties.is_empty() {
            return Ok(());
        }
        let d = self.ensure_info_dict()?;
        for (k, v) in properties {
            let s = escaped_utf16_string(v.as_bytes())?;
            d.set(k.as_bytes(), Object::String(s));
        }
        Ok(())
    }

    /// Gives the output an information dictionary, when there is one defensible answer.
    ///
    /// ONE SOURCE ONLY. With two, "whose title is this" has no answer that does not depend on
    /// the page order, and picking the first page's source would mean that moving an imported
    /// page to the front silently changes what the document claims to be. An export that mixes
    /// sources carries nothing, which is a state a reader can see.
    fn carry_info(&mut self, docs: &mut Docs) -> Result<()> {
        if docs.len() != 1 {
            return Ok(());
        }
        let Some(d) = docs.values_mut().next() else {
            return Ok(());
        };
        let xt = &mut d.ctx.xref;
        let Some(info) = xt.info else {
            return Ok(());
        };
        // An /Info that does not resolve is the source's problem and not something to fail an
        // export over; the export simply carries none.
        let src = match xt.dereference_dict_ref(info) {
            Ok(Some(src)) => src.clone(),
            _ => return Ok(()),
        };
        let mut out = Dict::default();
        for key in INFO_CARRIED {
            let Some(v) = src.find(key.as_bytes()) else {
                continue;
            };
            // A text string is written either way (ISO 32000-1 7.9.2.2), and a UTF-16BE title
            // is normally the hex form.
            if let s @ (Object::String(_) | Object::Hex(_)) = deref_obj(xt, Some(v)) {
                out.set(key.as_bytes(), s);
            }
        }
        if out.is_empty() {
            return Ok(());
        }
        let nr = self.dest.insert_object(Object::Null)?;
        self.set(nr, Object::Dict(out))?;
        self.dest.info = Some(ref_to(nr));
        Ok(())
    }

    /// Copies one source page into the output as a page of its own.
    ///
    /// The page dictionary is NOT memoized, though everything it reaches is. Two entries naming
    /// the same source page at two rotations are two pages of the output and need two
    /// dictionaries; sharing one would give both the second rotation. They still share the
    /// content stream, the raster and the fonts underneath, because those go through `migrate`.
    fn migrate_page(&mut self, docs: &mut Docs, p: &PageSource, nr: i64) -> Result<()> {
        self.ensure_leaves(docs, p.source)?;
        let src = self.leaf(p.source, p.page)?;
        let o = deref_source(docs, p.source, ref_to(src), self.cancel)?;
        let Object::Dict(pd) = o else {
            return Err(Error::Malformed(format!(
                "source page object {src} is not a dictionary"
            )));
        };

        let mut out = Dict::default();
        for (key, val) in pd.iter() {
            // /Parent names the SOURCE page tree, and the output's is assigned below.
            // `reference` would return null for it in any case; skipping it here says why
            // rather than leaving a null to be explained.
            if key.as_slice() == b"Parent" {
                continue;
            }
            let cp = if key.as_slice() == b"Contents" {
                // See copy_contents_field: a straighten wrap can mutate this field's array
                // object in place, so it must never be the SAME migrated object two output
                // pages share.
                self.copy_contents_field(docs, p.source, val)?
            } else {
                self.copy(docs, p.source, val)?
            };
            out.0.insert(key.clone(), cp);
        }
        out.set(b"Type", Object::name("Page"));
        out.set(b"Parent", Object::Ref(self.pages));
        // ABSOLUTE, not additive: the caller states the rotation the exported page must have,
        // and the source page's own /Rotate is replaced rather than combined. kleio redelivers
        // a job at least once, so a rotation that accumulated on each delivery would turn a
        // retry into a defect.
        out.set(b"Rotate", Object::Integer(i64::from(p.rotate)));
        if out.find(b"MediaBox").is_none() {
            // The source declared no /MediaBox anywhere in this page's inheritance chain.
            // byblos reads such a page as US Letter and says so through
            // `Page::media_box_defaulted` (byb-8ly, nine govdocs1 files); writing no box at
            // all would emit a document that byblos accepted on the way in and that every
            // validator refuses on the way out.
            out.set(
                b"MediaBox",
                Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(DEFAULT_PAGE_WIDTH_PT),
                    Object::Integer(DEFAULT_PAGE_HEIGHT_PT),
                ]),
            );
        }
        self.set(nr, Object::Dict(out))
    }

    /// Records `si`'s page leaves in page order, having pushed every inherited attribute down
    /// onto them (once per source: the walk changes the SOURCE context in place).
    ///
    /// The push-down is required, not an optimization. A /Pages node may legally hold
    /// /Resources, /MediaBox, /CropBox and /Rotate on behalf of its descendants (ISO 32000-1
    /// table 30), and the output's page tree is a new node that holds none of them. Measured on
    /// a document whose /Pages node carries /CropBox [100 100 300 400]: byblos reads page 1 as
    /// (100,100)-(300,400), and pdf-go's own `Collect` reports (0,0)-(612,792) after extracting
    /// it, because `addPages` bakes Resources, Parent, MediaBox and conditionally Rotate onto
    /// the migrated leaf and never CropBox.
    fn ensure_leaves(&mut self, docs: &mut Docs, si: usize) -> Result<()> {
        if self.leafs.contains_key(&si) {
            return Ok(());
        }
        let xt = &mut source(docs, si)?.ctx.xref;
        let root = xt.pages()?;
        let mut walk = Walk::default();
        walk_page_tree(xt, &mut walk, root, &Dict::default(), 0)?;
        let Walk {
            pages: leaves,
            tree: nodes,
            ..
        } = walk;
        if i64::try_from(leaves.len()).unwrap_or(i64::MAX) != xt.page_count {
            return Err(Error::Malformed(format!(
                "source page tree walks {} pages, and the document reports {}",
                leaves.len(),
                xt.page_count
            )));
        }
        for &n in leaves.iter().chain(nodes.iter()) {
            self.tree.insert((si, n));
        }
        self.leafs.insert(si, leaves);
        Ok(())
    }

    /// The object number of page `page` (1-based) of source `si`; `ensure_leaves` came first.
    fn leaf(&self, si: usize, page: usize) -> Result<i64> {
        page.checked_sub(1)
            .and_then(|i| self.leafs.get(&si)?.get(i))
            .copied()
            .ok_or_else(|| {
                Error::Malformed(format!("build from pages: source {si} has no page {page}"))
            })
    }

    /// Resolves one indirect reference of a source document.
    ///
    /// IT STOPS AT A PAGE, and that is the whole reason this is not just `migrate`. /Parent is
    /// not the only way a page is reachable: a /Link annotation's /Dest names a page, a /GoTo
    /// action's /D names a page, and an article thread's beads name pages. Following one
    /// migrates the destination page's dictionary, its content stream and its resources into a
    /// document with no room for them: an invisible copy of the page the caller deleted.
    ///
    /// A destination page that IS in this build resolves to the output page it became, so a
    /// link between two exported pages still works. A page that is not becomes null. ISO
    /// 32000-1 7.3.10 makes a reference to an undefined object the null object, so this loses
    /// only what was already unreachable, whereas leaving the reference would name an object
    /// number the output never defines.
    fn reference(&mut self, docs: &mut Docs, si: usize, n: i64) -> Result<Object> {
        let key = (si, n);
        if let Some(r) = self.exported.get(&key) {
            return Ok(Object::Ref(*r));
        }
        if self.tree.contains(&key) {
            return Ok(Object::Null);
        }
        Ok(Object::Ref(self.migrate(docs, si, n)?))
    }

    /// Migrates a page's /Contents field like `copy` does for every other field, with one
    /// exception: when it is an indirect reference to an ARRAY, that array is migrated FRESH
    /// for this page rather than shared through `migrate`'s memo.
    ///
    /// `wrap_indirect_contents` (text.go) extends an indirect-ref-to-array /Contents IN PLACE,
    /// through the xref table entry. `migrate`'s memoization is keyed on the SOURCE object
    /// number, so two entries of one page list naming the same source page (two different
    /// output page dicts) would otherwise both resolve /Contents to the identical migrated
    /// array object, and straightening either page would silently apply BOTH pages' wrappers
    /// to it. Giving each page its own array object here removes the sharing that in-place
    /// mutation cannot survive; the array's own elements still go through the ordinary
    /// memoized `copy`, so the underlying content streams remain shared. An
    /// indirect-ref-to-STREAM /Contents is unaffected for the same reason: the stream case of
    /// `wrap_content` never mutates the stream in place, so sharing it is safe.
    fn copy_contents_field(&mut self, docs: &mut Docs, si: usize, val: &Object) -> Result<Object> {
        let Object::Ref(r) = val else {
            return self.copy(docs, si, val);
        };
        if self.tree.contains(&(si, r.num)) {
            return Ok(Object::Null);
        }
        let Object::Array(arr) = deref_source(docs, si, *r, self.cancel)? else {
            return self.copy(docs, si, val);
        };
        let mut out: Vec<Object> = Vec::new();
        out.try_reserve_exact(arr.len())?;
        for e in &arr {
            out.push(self.copy(docs, si, e)?);
        }
        let nr = self.dest.insert_object(Object::Null)?;
        self.set(nr, Object::Array(out))?;
        Ok(Object::Ref(ref_to(nr)))
    }

    /// Copies the source object `n` of source `si` into the output, once.
    fn migrate(&mut self, docs: &mut Docs, si: usize, n: i64) -> Result<Ref> {
        let key = (si, n);
        if let Some(nw) = self.done.get(&key) {
            return Ok(ref_to(*nw));
        }
        check_cancel(self.cancel)?;
        let o = deref_source(docs, si, ref_to(n), self.cancel)?;
        let nr = self.dest.insert_object(Object::Null)?;
        // Memoized BEFORE the copy recurses, so a reference cycle terminates here rather than
        // running the stack out.
        self.done.insert(key, nr);

        let cp = self.copy(docs, si, &o)?;
        self.set(nr, cp)?;
        Ok(ref_to(nr))
    }

    /// Deep-copies one object, migrating every indirect reference it holds.
    fn copy(&mut self, docs: &mut Docs, si: usize, o: &Object) -> Result<Object> {
        self.depth += 1;
        if self.depth > MAX_MIGRATION_DEPTH {
            self.depth -= 1;
            return Err(Error::Malformed(format!(
                "object graph nests deeper than {MAX_MIGRATION_DEPTH}"
            )));
        }
        let r = self.copy_node(docs, si, o);
        self.depth -= 1;
        r
    }

    fn copy_node(&mut self, docs: &mut Docs, si: usize, o: &Object) -> Result<Object> {
        self.held.grow_by(NODE_BYTES)?;
        match o {
            Object::Ref(r) => self.reference(docs, si, r.num),
            Object::Dict(d) => Ok(Object::Dict(self.copy_dict(docs, si, d)?)),
            Object::Array(a) => {
                let mut out: Vec<Object> = Vec::new();
                out.try_reserve_exact(a.len())?;
                for e in a {
                    out.push(self.copy(docs, si, e)?);
                }
                Ok(Object::Array(out))
            }
            Object::Stream(s) => Ok(Object::Stream(Box::new(self.copy_stream(docs, si, s)?))),
            Object::Name(b) | Object::String(b) | Object::Hex(b) => {
                self.held.grow_by(b.len() as u64)?;
                Ok(o.clone())
            }
            Object::Null | Object::Bool(_) | Object::Integer(_) | Object::Float(_) => Ok(o.clone()),
        }
    }

    /// Entries are copied in key order, which is what numbers the objects they reach.
    fn copy_dict(&mut self, docs: &mut Docs, si: usize, d: &Dict) -> Result<Dict> {
        let mut out = Dict::default();
        for (k, v) in d.iter() {
            self.held.grow_by(k.len() as u64)?;
            let cp = self.copy(docs, si, v)?;
            out.0.insert(k.clone(), cp);
        }
        Ok(out)
    }

    /// Copies a stream object, payload verbatim.
    ///
    /// The encoded bytes are carried across untouched (the clone shares them, and their
    /// charge): nothing here re-encodes, and a JBIG2 or JPX payload pdf-rs cannot decode
    /// migrates as well as a Flate one.
    ///
    /// /Length is forced DIRECT and recomputed from the payload. An indirect /Length would
    /// otherwise migrate an integer object of its own, and the writer checks the payload
    /// against the stream length: the dictionary entry and the field are written from
    /// different places and both have to say the same thing.
    fn copy_stream(&mut self, docs: &mut Docs, si: usize, s: &StreamDict) -> Result<StreamDict> {
        let dict = self.copy_dict(docs, si, &s.dict)?;
        let mut out = s.clone();
        out.dict = dict;
        let n = s.raw.as_ref().map_or(0, |b| b.len()) as i64;
        out.dict.set(b"Length", Object::Integer(n));
        out.stream_length = Some(n);
        out.stream_length_obj_nr = None;
        Ok(out)
    }
}

/// Dereferences `r` in source `si`; an undefined reference is the null object.
fn deref_source(docs: &mut Docs, si: usize, r: Ref, cancel: &AtomicBool) -> Result<Object> {
    let xt = &mut source(docs, si)?.ctx.xref;
    let target = Object::Ref(r);
    Ok(xt.deref_c(&target, cancel)?.clone())
}

#[cfg(test)]
mod tests {
    use super::super::annots::build_pdf;
    use super::*;

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn lim() -> Limits {
        Limits::default()
    }

    fn page(source: usize, page: usize) -> PageSource {
        PageSource {
            source,
            page,
            rotate: 0,
            straighten: None,
        }
    }

    fn build(sources: &[&[u8]], pages: &[PageSource]) -> Vec<u8> {
        build_from_pages(sources, pages, &lim(), &never()).unwrap()
    }

    fn open(out: &[u8]) -> Doc {
        Doc::open_limits(out, &lim(), &never()).unwrap()
    }

    /// A two-page document whose /Pages node carries the /CropBox on behalf of both leaves,
    /// which ISO 32000-1 table 30 permits and no corpus document exercises.
    fn inherited_crop_box_doc(crop: [i64; 4]) -> Vec<u8> {
        let body = "0 0 1 rg 120 120 100 100 re f\n";
        build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 612 792] /CropBox [{} {} {} {}] >>",
                crop[0], crop[1], crop[2], crop[3]
            ),
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()),
            "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
        ])
    }

    /// A two-page document whose page 1 carries a /Link annotation destinationing page 2: the
    /// shape that makes a page reachable from something other than the page tree.
    fn link_to_page_two_doc() -> Vec<u8> {
        let one = "0 0 1 rg 10 10 50 50 re f\n";
        let two = "%PAGETWOMARKER\n0 1 0 rg 20 20 30 30 re f\n";
        build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 612 792] >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Annots [7 0 R] >>".to_string(),
            format!("<< /Length {} >>\nstream\n{one}\nendstream", one.len()),
            "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{two}\nendstream", two.len()),
            "<< /Type /Annot /Subtype /Link /Rect [10 10 60 60] /Border [0 0 0] /Dest [5 0 R /Fit] >>"
                .to_string(),
        ])
    }

    /// A two-page document carrying an information dictionary (object 7, named by the trailer).
    fn titled_doc(title: &str) -> Vec<u8> {
        let one = "0 0 1 rg 10 10 50 50 re f\n";
        let data = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 612 792] >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{one}\nendstream", one.len()),
            "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
            format!(
                "<< /Title ({title}) /Author (Ada) /Subject (Subj) /Keywords (kw) /Producer (fixture) >>"
            ),
        ]);
        // The trailer of build_pdf names no /Info.
        String::from_utf8(data)
            .unwrap()
            .replace("/Root 1 0 R >>", "/Root 1 0 R /Info 7 0 R >>")
            .into_bytes()
    }

    /// A one-page document with no /Info.
    fn plain_doc() -> Vec<u8> {
        build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>".to_string(),
            "<< /Type /Page /Parent 2 0 R >>".to_string(),
        ])
    }

    fn info_of(out: &[u8]) -> Option<Dict> {
        let mut d = open(out);
        let xt = &mut d.ctx.xref;
        let info = xt.info?;
        xt.dereference_dict_ref(info).unwrap().cloned()
    }

    fn count_objects(out: &[u8], pred: impl Fn(&Dict) -> bool) -> usize {
        let mut d = open(out);
        let xt = &mut d.ctx.xref;
        let nums: Vec<i64> = xt.table.keys().copied().filter(|&n| n != 0).collect();
        let mut n = 0;
        for num in nums {
            let target = Object::Ref(ref_to(num));
            let Ok(o) = xt.dereference(&target) else {
                continue;
            };
            let dict = match o {
                Object::Dict(d) => d,
                Object::Stream(s) => &s.dict,
                _ => continue,
            };
            if pred(dict) {
                n += 1;
            }
        }
        n
    }

    fn is_type(name: &'static str) -> impl Fn(&Dict) -> bool {
        move |d| d.type_() == Some(name.as_bytes())
    }

    // The entries that describe the DOCUMENT rather than its pages. byblos stores provenance in
    // the information dictionary, and a fresh context has no /Info, so a build that said
    // nothing here would silently discard the title, the author and the provenance of every
    // export.
    #[test]
    fn carries_the_info_dict_of_a_single_source() {
        let src = titled_doc("A Title");
        // ONE source, named twice: that is how a caller says "these two pages come from the
        // same document".
        let out = build(&[&src], &[page(0, 2), page(0, 1)]);
        let info = info_of(&out).unwrap();
        for (key, want) in [
            ("Title", "A Title"),
            ("Author", "Ada"),
            ("Subject", "Subj"),
            ("Keywords", "kw"),
        ] {
            assert_eq!(
                info.string_entry(key.as_bytes()),
                Some(want.as_bytes()),
                "/Info /{key}"
            );
        }
        // /Producer names what WROTE these bytes, and that is byblos, not the source's.
        assert_ne!(info.string_entry(b"Producer"), Some(b"fixture".as_slice()));
    }

    // With two sources there is no defensible answer to "whose title is this".
    #[test]
    fn drops_the_info_dict_when_sources_disagree() {
        let (a, b) = (titled_doc("First"), titled_doc("Second"));
        let out = build(&[&a, &b], &[page(0, 1), page(1, 1)]);
        let info = info_of(&out).unwrap();
        assert!(info.string_entry(b"Title").is_none());
    }

    // Correction 6: the producer stamp reaches multi-source exports and sources with no /Info.
    #[test]
    fn stamps_producer_even_without_a_carried_info_dict() {
        let (a, b) = (titled_doc("First"), titled_doc("Second"));
        let out = build(&[&a, &b], &[page(0, 1), page(1, 1)]);
        let info = info_of(&out).unwrap();
        assert!(!info.string_entry(b"Producer").unwrap().is_empty());

        let plain = plain_doc();
        let out = build(&[&plain], &[page(0, 1)]);
        let info = info_of(&out).unwrap();
        assert!(!info.string_entry(b"Producer").unwrap().is_empty());
    }

    // Correction 5: the property is folded into the ONE build and the ONE write, and the raw
    // dict value is what `escaped_utf16_string` produces (a plain-UTF-8 value would round-trip
    // through a reader just as happily, so the raw value is what is checked).
    #[test]
    fn with_properties_encodes_utf16() {
        let plain = plain_doc();
        let want = r#"{"note":"café"}"#;
        let mut props = BTreeMap::new();
        props.insert("byblos-test-key".to_string(), want.to_string());
        let out =
            build_from_pages_with_properties(&[&plain], &[page(0, 1)], &props, &lim(), &never())
                .unwrap();
        let info = info_of(&out).unwrap();
        let raw = info.string_entry(b"byblos-test-key").unwrap();
        assert_eq!(
            raw,
            escaped_utf16_string(want.as_bytes()).unwrap().as_slice()
        );
        assert!(!info.string_entry(b"Producer").unwrap().is_empty());
    }

    #[test]
    fn takes_pages_in_the_order_given_and_names_a_page_twice() {
        let src = titled_doc("T");
        let out = build(&[&src], &[page(0, 2), page(0, 1), page(0, 2)]);
        let d = open(&out);
        assert_eq!(d.ctx.xref.page_count, 3);
        assert_eq!(count_objects(&out, is_type("Page")), 3);
    }

    // Rotate REPLACES the source page's own value and is never added to it.
    #[test]
    fn applies_absolute_rotation() {
        let src = plain_doc();
        for want in [0, 90, 180, 270] {
            let pages = [PageSource {
                rotate: want,
                ..page(0, 1)
            }];
            let out = build(&[&src], &pages);
            let mut d = open(&out);
            let (_, _, inh) = d.ctx.xref.page_dict(1, &never()).unwrap();
            assert_eq!(inh.rotate, i64::from(want));
        }
    }

    // The loss pdf-go's own path takes and byblos must not: an attribute left on the old /Pages
    // node is simply gone from the output's new node.
    #[test]
    fn pushes_an_inherited_crop_box_down() {
        let data = inherited_crop_box_doc([100, 100, 300, 400]);
        let out = build(&[&data], &[page(0, 1)]);
        let mut d = open(&out);
        let (_, _, inh) = d.ctx.xref.page_dict(1, &never()).unwrap();
        let crop = inh.crop_box.unwrap();
        assert_eq!(
            (crop.ll.x, crop.ll.y, crop.ur.x, crop.ur.y),
            (100.0, 100.0, 300.0, 400.0)
        );
    }

    // A link annotation's /Dest names a PAGE; migrating it naively pulls the destination page
    // back in.
    #[test]
    fn does_not_resurrect_a_page_through_an_annotation() {
        let data = link_to_page_two_doc();
        let mut src = open(&data);
        assert_eq!(src.annots(1).unwrap().len(), 1);

        let out = build(&[&data], &[page(0, 1)]);
        assert_eq!(count_objects(&out, is_type("Page")), 1);
        assert!(!out.windows(13).any(|w| w == b"PAGETWOMARKER"));
    }

    // The other half of the rule: when the destination page IS in the build, the annotation
    // names the output page it became.
    #[test]
    fn keeps_a_link_between_two_exported_pages() {
        let data = link_to_page_two_doc();
        // Reversed on purpose, so a destination that was merely copied through would land on
        // the wrong page.
        let out = build(&[&data], &[page(0, 2), page(0, 1)]);
        let mut d = open(&out);
        assert_eq!(d.annots(2).unwrap().len(), 1);
        assert_eq!(count_objects(&out, is_type("Page")), 2);

        let xt = &mut d.ctx.xref;
        let root = xt.pages().unwrap();
        let kids = match xt
            .dereference_dict_ref(root)
            .unwrap()
            .unwrap()
            .find(b"Kids")
        {
            Some(Object::Array(k)) => k.clone(),
            other => panic!("/Kids is {other:?}"),
        };
        let want_page_1: Ref = match kids[0] {
            Object::Ref(r) => r,
            _ => panic!("/Kids[0] is not a reference"),
        };
        let Object::Dict(pd) = deref_obj(xt, Some(&kids[1])) else {
            panic!("page dict");
        };
        let Object::Array(annots) = deref_obj(xt, pd.0.get(b"Annots".as_slice())) else {
            panic!("annots");
        };
        let Object::Dict(ad) = deref_obj(xt, annots.first()) else {
            panic!("annot dict");
        };
        let Object::Array(dest) = deref_obj(xt, ad.0.get(b"Dest".as_slice())) else {
            panic!("dest");
        };
        match dest.first() {
            Some(Object::Ref(r)) => assert_eq!(r.num, want_page_1.num),
            other => panic!("the migrated /Dest names {other:?}, not a page"),
        }
    }

    // Two entries naming one source page: the page DICTIONARY is duplicated, everything under
    // it is shared.
    #[test]
    fn taking_one_page_twice_shares_its_content() {
        let data = link_to_page_two_doc();
        let pages = [
            page(0, 2),
            PageSource {
                rotate: 90,
                ..page(0, 2)
            },
        ];
        let out = build(&[&data], &pages);
        assert_eq!(count_objects(&out, is_type("Page")), 2);
        // One content stream, shared: the source's page 2 has one.
        let streams = count_objects(&out, |d| d.has_entry(b"Length"));
        assert_eq!(streams, 1);
    }

    #[test]
    fn refuses_what_cannot_be_built() {
        let src = plain_doc();
        let cases: [(&str, Vec<PageSource>, &str); 7] = [
            ("an empty page sequence", vec![], "no pages"),
            (
                "a page below the source's range",
                vec![page(0, 0)],
                "out of range",
            ),
            (
                "a page past the source's range",
                vec![page(0, 9)],
                "out of range",
            ),
            (
                "a negative rotation, which PageInfo.Rotate would have normalized away",
                vec![PageSource {
                    rotate: -90,
                    ..page(0, 1)
                }],
                "rotation",
            ),
            (
                "a rotation that is not a quarter turn",
                vec![PageSource {
                    rotate: 45,
                    ..page(0, 1)
                }],
                "rotation",
            ),
            (
                "a rotation of 360, a quarter-turn multiple and still not one of the four",
                vec![PageSource {
                    rotate: 360,
                    ..page(0, 1)
                }],
                "rotation",
            ),
            ("a source that is not there", vec![page(5, 1)], "no source"),
        ];
        for (name, pages, want) in cases {
            let err = build_from_pages(&[&src], &pages, &lim(), &never()).unwrap_err();
            assert!(err.to_string().contains(want), "{name}: {err}");
        }
    }

    #[test]
    fn refuses_an_encrypted_source() {
        // owner-password-only: opens with no password, so the refusal is Build's own.
        let src = std::fs::read(format!(
            "{}/tests/data/encrypt-census/owner-password-only.pdf",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let err = build_from_pages(&[&src], &[page(0, 1)], &lim(), &never()).unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
    }

    #[test]
    fn refuses_an_unreadable_source() {
        let src = plain_doc();
        let bad = b"%PDF-1.7\nnot a document".as_slice();
        let err = build_from_pages(&[&src, bad], &[page(0, 1), page(1, 1)], &lim(), &never());
        assert!(err.is_err());
    }

    #[test]
    fn a_set_cancel_flag_stops_the_build() {
        let src = plain_doc();
        let cancel = AtomicBool::new(true);
        let err = build_from_pages(&[&src], &[page(0, 1)], &lim(), &cancel).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    fn corpus(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    // The tests below need corpus PDFs, which the Go internal/corpus generator makes and this
    // port does not.

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf from the Go oracle -gen"]
    fn corpus_takes_one_page() {
        let book = corpus("booklet");
        let out = build(&[&book], &[page(0, 3)]);
        assert_eq!(open(&out).ctx.xref.page_count, 1);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf from the Go oracle -gen"]
    fn corpus_reorders_and_deletes() {
        let book = corpus("booklet");
        let out = build(&[&book], &[page(0, 8), page(0, 2), page(0, 8)]);
        assert_eq!(open(&out).ctx.xref.page_count, 3);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf and mixed.pdf from the Go oracle -gen"]
    fn corpus_imports_from_a_second_source() {
        let (book, other) = (corpus("booklet"), corpus("mixed"));
        let out = build(&[&book, &other], &[page(0, 3), page(1, 2), page(0, 5)]);
        assert_eq!(open(&out).ctx.xref.page_count, 3);
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan-rotated.pdf from the Go oracle -gen"]
    fn corpus_absolute_rotation_replaces_the_source_rotate() {
        let rotated = corpus("scan-rotated"); // one page, declaring /Rotate 90
        for want in [0, 90, 180, 270] {
            let pages = [PageSource {
                rotate: want,
                ..page(0, 1)
            }];
            let out = build(&[&rotated], &pages);
            let (_, _, inh) = open(&out).ctx.xref.page_dict(1, &never()).unwrap();
            assert_eq!(inh.rotate, i64::from(want));
        }
    }

    // Booklet pages 2 upward share ONE body font object; migrating it per page would multiply
    // every shared font, raster and colour space by the number of pages that use it.
    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf from the Go oracle -gen"]
    fn corpus_migrates_a_shared_resource_once() {
        let book = corpus("booklet");
        let out = build(&[&book], &[page(0, 2), page(0, 3), page(0, 4)]);
        assert_eq!(count_objects(&out, is_type("Font")), 1);
        assert_eq!(
            count_objects(&out, |d| d.subtype() == Some(b"Image".as_slice())),
            3
        );
    }

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf from the Go oracle -gen"]
    fn corpus_taking_one_page_twice_shares_its_raster() {
        let book = corpus("booklet");
        let pages = [
            page(0, 4),
            PageSource {
                rotate: 90,
                ..page(0, 4)
            },
        ];
        let out = build(&[&book], &pages);
        assert_eq!(count_objects(&out, is_type("Page")), 2);
        assert_eq!(
            count_objects(&out, |d| d.subtype() == Some(b"Image".as_slice())),
            1
        );
    }

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf and malformed.pdf from the Go oracle -gen"]
    fn corpus_refuses_a_page_from_an_unreadable_source() {
        let (book, bad) = (corpus("booklet"), corpus("malformed"));
        let err = build_from_pages(&[&book, &bad], &[page(0, 1), page(1, 1)], &lim(), &never());
        assert!(err.is_err());
    }
}
