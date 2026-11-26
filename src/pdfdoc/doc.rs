//! Port of `internal/pdfdoc/pdfdoc.go`: [`Doc`], the seam byblos keeps between itself and the
//! PDF library (pdf-rs here, pdfcpu in Go).
//!
//! Everything above this module speaks in the types declared here. The notes on pdfcpu in Go's
//! package comment carry over to pdf-rs where they are about behaviour it keeps from pdf-go:
//!
//! * `ctx.xref.page_count` is zero after a read; `ensure_page_count` populates it.
//! * `StreamDict::content` is empty until a decode.
//! * The dictionary facts of an image (width, height, bits per component) come from the image
//!   XObject's own stream dictionary.
//! * pdf-rs hands JBIG2 and JPX streams back as they are, so callers check the file type.
//! * pdf-rs reads `/Kids` with `Dict::array_entry`, which does not follow a reference (pdf-go's
//!   `ArrayEntry`): a `/Pages` node holding an indirect `/Kids` looks childless and `page_dict`
//!   returns that node as the dictionary of every page, with no error. [`open`] repairs the
//!   tree before anything walks it; see [`normalize_page_tree`].
//!
//! # Differences from Go
//!
//! * `Doc` is a struct, not an interface; the write half (`replace_image`, `write`,
//!   `add_font_resource`, `append_content`, `wrap_content`) and `annots` are inherent methods
//!   that `write.rs`, `text.rs` and `annots.rs` add in their own `impl Doc` blocks. The fields
//!   they need are `pub(crate)`.
//! * `content.Env` is a trait taking `&self`, and Go's `XObject`, `ExtGStateOpaque` and `Font`
//!   change the document, so the trait is implemented by [`DocEnv`], a `RefCell` around a
//!   `&mut Doc`. [`Doc::xobject`], [`Doc::ext_gstate_opaque`] and [`Doc::font`] are the same
//!   lookups as methods.
//! * Go's `Open`, `OpenLimits` and `OpenBudgeted` are [`open`] and [`open_budgeted`]. Go's
//!   process-wide memory pool is gone (docs/go-differences.md): `OpenBudgeted`'s reservations
//!   are not made, and its adaptation of the caps to the pool's size does not happen; what stays
//!   is the object cap lowered to the file's own object count.
//! * `catchPanic` turned a pdfcpu panic into [`Error::Malformed`]. pdf-rs returns
//!   `pdf::Error::Panic` where pdf-go panics, and [`seam`] makes the same `Malformed` of it.
//! * pdf-rs charges what it allocates to the ledger of the document (`ctx.xref.mem()`); byblos
//!   charges its own input-sized buffers (page content, form content, font programs, number
//!   arrays) to the same ledger.

use super::limits::{image_decode_bytes, is_limit, objects_in};
use crate::content::{self, BBox, Env, IDENTITY, XObject};
use crate::dct;
use crate::error::{Error, Result};
use crate::limits::{self, Limits, check_cancel};
use pdf::filter::{CCITT_FAX, DCT, FLATE, JBIG2, JPX};
use pdf::model::{Context, XRefTable};
use pdf::object::{Array, Dict, Object, Ref, StreamDict};
use pdf::{Bytes, Held};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// A cancel flag that is never set, for the Go calls that take no `context`.
pub(crate) static NEVER: AtomicBool = AtomicBool::new(false);

/// `defaultPageWidthPt`, `defaultPageHeightPt` are US Letter, the box every reader falls back
/// to for a page that declares none. Poppler reports exactly this for the byb-8ly documents,
/// which is a convention it applies and not a fact it read out of them.
const DEFAULT_PAGE_WIDTH_PT: f64 = 612.0;
const DEFAULT_PAGE_HEIGHT_PT: f64 = 792.0;

/// `maxCIDWidthEntries` bounds how many CID to width entries a `/W` array can flatten to. The
/// CIDs of a render font are `u16`, so 65536 (the whole CID space) is already generous. Without
/// a bound the range form (ISO 32000-1 9.7.4.3) `cFirst cLast w` lets about 20 bytes of input
/// (`/W [0 2147483647 500]`) claim 2^31 map entries.
const MAX_CID_WIDTH_ENTRIES: i64 = 65536;

/// The bytes charged for the window of the DEFLATE reader that [`Doc::repair_trailer`] runs.
const INFLATE_WINDOW_BYTES: u64 = 1 << 20;

/// `Rect` is a rectangle in PDF default user space: points, origin lower-left, y increasing
/// upward.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub llx: f64,
    pub lly: f64,
    pub urx: f64,
    pub ury: f64,
}

/// `ImageInfo` describes an image XObject as its own dictionary declares it.
///
/// `smask`, `mask` and `image_mask` are the three ways an image can fail to be an opaque
/// rectangle of pixels. They are recorded as presence, not as content: what the mask actually
/// does needs a renderer, and for deciding whether this image hides what is painted under it,
/// "it has one" is the whole answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageInfo {
    pub name: String,
    pub obj_nr: i64,
    /// Pixels.
    pub width: i64,
    pub height: i64,
    /// `/BitsPerComponent`; 0 when absent.
    pub bpc: i64,
    /// `/ImageMask`: a stencil painted in the fill colour.
    pub image_mask: bool,
    /// `/SMask`: a soft mask supplies per-pixel alpha.
    pub smask: bool,
    /// `/Mask`: a stencil mask or a colour-key range.
    pub mask: bool,
    /// `/Decode` is present: the samples need a remap.
    pub decode: bool,

    /// `/Decode`'s elements when every one of them is a number, and `None` otherwise: either
    /// the entry is absent, or it is present in a shape this cannot read.
    ///
    /// `decode` and this field answer different questions and a caller must pick the one it
    /// means. "Is there a remap at all" is `decode`, and an entry whose numbers could not be
    /// read is still a remap. "What is the remap" is this, and `None` means there is none to be
    /// had, never that the samples pass through unchanged.
    ///
    /// The values are as written; the default array for an absent entry depends on the colour
    /// space and the bit depth (ISO 32000-1 table 39), so it is not supplied. The array of an
    /// indirect entry is shared between the images that name it.
    pub decode_array: Option<Arc<[f64]>>,

    /// The image codec the stream declares: `/Filter` when it is a name, the LAST entry when it
    /// is an array, and empty when `/Filter` is absent (an unencoded stream) or is neither of
    /// those shapes.
    ///
    /// Last, not first, because a filter array is the DECODE order (ISO 32000-1 section 7.4):
    /// in `/Filter [/ASCII85Decode /JBIG2Decode]` the ASCII85 stage only unwraps the transport
    /// encoding and the image codec is `JBIG2Decode`.
    ///
    /// It is a declaration, like everything else here: [`Doc::raw_image`] still reports what
    /// pdf-rs actually made of the bytes. The two disagree when a file lies about its own
    /// encoding.
    ///
    /// The name's bytes as written, like Go's `string`: a name that is not UTF-8 stays as it is.
    pub filter: Vec<u8>,

    /// `/ColorSpace` when it is a plain name (`DeviceGray`, `DeviceRGB`, `DeviceCMYK`), and
    /// empty when it is an array or an indirect reference (Indexed, ICCBased, Separation,
    /// DeviceN). Recompression uses it as an eligibility test, not as colour management:
    /// replacing a `/Separation` with `/DeviceGray` inverts the ink convention, so anything that
    /// is not a device name is left alone.
    pub color_space: String,
}

/// `Page` is one page's geometry, content, and resource scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    /// 1-based.
    pub index: usize,
    pub media_box: Rect,
    /// Equals `media_box` when the page declares none.
    pub crop_box: Rect,
    pub rotate: i64,
    /// Decoded, concatenated.
    pub content: Vec<u8>,
    /// Resource scope handle for `content::Env`.
    pub scope: i64,
    /// The document declared no `/MediaBox` anywhere in this page's inheritance chain, so
    /// `media_box` is the US Letter convention rather than anything the file states.
    pub media_box_defaulted: bool,
    /// At least one of this page's content streams failed its integrity check and `content`
    /// holds the bytes that decoded anyway. See [`Doc::page_contents`].
    ///
    /// It does NOT say bytes were lost. A corrupted Adler-32 trailer alone still yields the
    /// whole stream, because zlib checks the trailer only after the last deflate block; a
    /// mid-stream corruption really does truncate. What the flag states in both cases is that
    /// `content` does not match what the file attests to.
    pub content_recovered: bool,
}

/// `RenderFont` is the renderer's view of one font dict: the fields of `render::text::Font`.
/// `program` is the decoded `/FontFile`, `/FontFile2` or `/FontFile3` stream, empty when the
/// dict embeds none, which is the case the substitution stage serves. `type0`, `w` and `dw`
/// mirror the render font's fields: a composite `/Subtype /Type0` font's program and widths
/// live on its descendant CIDFont, not the Type0 dict itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderFont {
    pub base_font: Vec<u8>,
    /// `/FontDescriptor /Flags`.
    pub flags: i64,
    pub first_char: i64,
    pub widths: Vec<f64>,
    pub program: Vec<u8>,
    pub type0: bool,
    pub w: HashMap<u16, f64>,
    pub dw: f64,
}

/// One resource scope: the resource dictionary of a page or form and the scope it inherits
/// from (-1 for a page scope).
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub(crate) res: Option<Dict>,
    pub(crate) parent: i64,
}

/// `Doc` is a parsed PDF. Go keeps it behind an interface to keep pdfcpu out of every other
/// package; here the struct is the seam, and `ctx` is only for the `pdfdoc` modules.
#[derive(Debug)]
pub struct Doc {
    pub(crate) ctx: Context,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) images: BTreeMap<i64, ImageInfo>,
    /// Image stream dicts, keyed like `images`.
    pub(crate) streams: BTreeMap<i64, StreamDict>,
    /// The xref identity of those streams, for writing.
    pub(crate) refs: BTreeMap<i64, Ref>,
    /// Synthetic ids for direct (non-indirect) objects.
    pub(crate) next_id: i64,
    /// Embedded fonts, keyed by `TrueTypeFont.BaseFont`.
    pub(crate) font_refs: BTreeMap<String, Ref>,
    /// Pages a font was bound to through the `/Resources` they inherit, with the page tree node
    /// that holds it: each takes that node's `/Resources` when the document is written
    /// (`text.rs`, `bind_font`).
    pub(crate) res_adopters: Vec<(Ref, Ref)>,
    /// The charge for the copies of `/Resources` the last write gave those pages.
    pub(crate) adopted_held: Option<Held>,
    /// `RenderFont` results, keyed like `images`. `None` is a font that did not resolve.
    font_cache: BTreeMap<i64, Option<RenderFont>>,
    /// `numberArray` results for indirect arrays, keyed by object number.
    num_arrays: BTreeMap<i64, Option<Arc<[f64]>>>,

    pub(crate) lim: Limits,
    /// Decoded non-image stream bytes of the current page.
    pub(crate) page_bytes: i64,
    /// Decoded forms already counted in `page_bytes`, shared by every `Do` of one form.
    page_forms: BTreeMap<i64, content::Stream>,
    /// The ledger charge of the form copies in `page_forms`.
    page_held: Vec<Held>,
    /// The first limit breach a lookup that returns no error had to swallow.
    pub(crate) limit_err: Option<Error>,
    /// The ledger charge of the number arrays the image records share.
    held: Vec<Held>,
}

/// `pdfcpuLimit` is the limit error for a refusal by one of pdf-rs's cumulative limits (object
/// stream entries, decoded bytes per parse) or its per-stream decode cap, and `None` for any
/// other error.
pub(crate) fn pdf_limit(lim: &Limits, e: &pdf::Error) -> Option<Error> {
    use pdf::{Error as P, LimitKind as K};
    let le = |name: &'static str, max: i64| Error::Limit {
        name,
        got: max.saturating_add(1),
        max,
    };
    match e.root() {
        P::Limit(K::ObjectBytes) => return Some(le("object-bytes", lim.object_bytes())),
        P::Limit(K::StreamTotal) => {
            return Some(le("file-stream-bytes", lim.max_stream_total_bytes));
        }
        P::Limit(K::Parsed) => {
            return Some(le("parsed-bytes", lim.parsed_bytes(lim.objects())));
        }
        _ => {}
    }
    match (e.is_limit_exceeded(), e.is_decode_limit_exceeded()) {
        (true, true) => Some(le("stream-bytes", lim.total_decode_bytes())),
        (true, false) => Some(le("objects", lim.object_stream_entries())),
        (false, true) => Some(le("stream-bytes", lim.stream_bytes())),
        (false, false) => None,
    }
}

/// The seam of every pdf-rs call: what Go's `catchPanic` and the budget errors do before an
/// error is looked at.
///
/// A `pdf::Error::Panic` (a site where pdf-go panics) is [`Error::Malformed`] naming `what`.
/// A refused allocation, an allocator failure and a set cancel flag keep their meaning and are
/// `Err`, as is the `Malformed`; any other error is `Ok`, for the caller to classify.
pub(crate) fn seam(what: &str, e: pdf::Error) -> std::result::Result<pdf::Error, Error> {
    if let pdf::Error::Panic(m) = e.root() {
        return Err(Error::Malformed(format!(
            "byblos/pdfdoc: malformed document: {what}: {m}"
        )));
    }
    match Error::from(e) {
        Error::Pdf(p) => Ok(p),
        other => Err(other),
    }
}

/// `fmt.Errorf("<msg>: %w", err)` over a pdf-rs error, after [`seam`] and `pdfcpuLimit`: the
/// error is the limit when it is one, else [`Error::Pdf`] with `msg` in front.
pub(crate) fn pdf_err(lim: &Limits, what: &str, msg: impl Into<Box<str>>, e: pdf::Error) -> Error {
    pdf_err_x(lim, what, msg, e, false)
}

/// [`pdf_err`], with `xref_range` set only for `openSized`'s read, the one place Go turns the
/// untyped xref range error into the `objects` limit.
fn pdf_err_x(
    lim: &Limits,
    what: &str,
    msg: impl Into<Box<str>>,
    e: pdf::Error,
    xref_range: bool,
) -> Error {
    match seam(what, e) {
        Err(o) => o,
        Ok(p) => {
            if let Some(l) = pdf_limit(lim, &p) {
                return l;
            }
            // Go matched the untyped xref range error by its text.
            if xref_range && matches!(p.root(), pdf::Error::XRefObjectRange) {
                return Error::Limit {
                    name: "objects",
                    got: lim.objects().saturating_add(1),
                    max: lim.objects(),
                };
            }
            Error::Pdf(p.context(msg))
        }
    }
}

/// `fmt.Errorf("<msg>: %w", err)` over a pdf-rs error with no limit mapping: the shape of every
/// wrap of Go's write half.
pub(crate) fn wrapped(what: &str, msg: impl Into<Box<str>>, e: pdf::Error) -> Error {
    match seam(what, e) {
        Err(o) => o,
        Ok(p) => Error::Pdf(p.context(msg)),
    }
}

/// `defaultConfig`: the pdf-rs configuration of every entry point that has no document yet
/// (Go's `defaultConfig` also used the package defaults for every limit; with
/// `Limits::default()` these are the same).
pub(crate) fn default_config(lim: &Limits) -> pdf::Config {
    lim.pdf_config(lim.objects())
}

/// `Open` / `OpenLimits`: parses `data` under the caps of `lim`. It does not run pdf-rs's
/// validator: real scanner output exercises relaxed paths, and rejecting a readable file helps
/// nobody. The validation gate belongs to `optimize` and to the caller's policy.
///
/// `data` is read once, here. Nothing below re-reads it, so a file this function accepts cannot
/// later be rejected by a validator byblos opted out of.
///
/// The object cap is checked from [`objects_in`] before pdf-rs parses anything; the other caps
/// of `lim` go to pdf-rs as `Limits::pdf_config` writes them. `cancel` interrupts the read.
///
/// # Errors
/// [`Error::Limit`] over a cap (`objects`, `pages`, and the cumulative ones pdf-rs enforces),
/// [`Error::Alloc`], [`Error::Cancelled`], [`Error::Malformed`] where pdf-go panics, and
/// [`Error::Pdf`] for any other read error (a wrong password is `pdf::Error::WrongPassword`,
/// see [`super::is_wrong_password`]).
pub fn open(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Doc> {
    open_with_mem(data, lim, &lim.mem(), cancel)
}

/// [`open`] charging `mem` instead of a fresh ledger of `lim.max_alloc`: every document one call
/// opens on `mem` shares its peak.
///
/// # Errors
/// As [`open`].
pub(crate) fn open_with_mem(
    data: &[u8],
    lim: &Limits,
    mem: &pdf::Mem,
    cancel: &AtomicBool,
) -> Result<Doc> {
    open_sized(data, lim, objects_in(data), mem, cancel)
}

/// `OpenBudgeted` without the pool: [`open`] with the object cap held to the object count the
/// file itself claims (the larger of its trailer `/Size` and its count of `obj` tokens, plus an
/// eighth and 64), because pdf-rs rebuilds the table by scanning whenever it distrusts the
/// trailer and that scan ignores its object-count cap. A linearized file's last trailer can sit
/// a few entries under the real count, so the cap has a margin; it is the most an understated
/// `/Size` parses.
///
/// Go also reserved the object table, the decode slot and the record caps from the global pool
/// and lowered the stream caps to what the pool left. Rust has no pool: `max_alloc` is the
/// per-call ledger that pdf-rs and byblos charge as they allocate.
///
/// # Errors
/// As [`open`].
pub fn open_budgeted(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Doc> {
    let size = objects_in(data);
    limits::check("objects", size, lim.objects())?;
    let mut l = *lim;
    l.max_objects = size
        .saturating_add(size / 8)
        .saturating_add(64)
        .min(lim.objects());
    open_sized(data, &l, size, &lim.mem(), cancel)
}

/// `openSized` is [`open_with_mem`] given the object count [`objects_in`] found.
fn open_sized(
    data: &[u8],
    lim: &Limits,
    size: i64,
    mem: &pdf::Mem,
    cancel: &AtomicBool,
) -> Result<Doc> {
    limits::check("objects", size, lim.objects())?;
    // pdf-go decodes object streams and image streams under `MaxDecodeBytes`, which defaults to
    // 512 MiB; `raw_image` raises it per image to that image's declared size. The object-count
    // cap is the backstop for a trailer that understates /Size. The cumulative limits are
    // pdf-go's: it reads an object stream's /N by its own rules, which no scan here tracks, so
    // it counts the entries and the decoded bytes of the parse itself and refuses at the limit.
    let conf = lim.pdf_config(size);
    let mut ctx = pdf::api::read_with_mem(data, &conf, mem, cancel)
        .map_err(|e| pdf_err_x(lim, "open", "byblos/pdfdoc: read", e, true))?;
    ctx.xref
        .ensure_page_count()
        .map_err(|e| pdf_err(lim, "open", "byblos/pdfdoc: page count", e))?;
    limits::check("pages", ctx.xref.page_count, lim.pages())?;
    if let Ok(root) = ctx.xref.pages() {
        normalize_page_tree(&mut ctx.xref, root);
    }
    Ok(Doc::new(ctx, lim))
}

/// `normalizePageTree` replaces an indirect `/Kids` with the array it refers to, throughout the
/// page tree.
///
/// ISO 32000-1 section 7.3.10 allows any object that is not a stream to be indirect, and Google
/// Books PDF Converter writes `/Kids` that way. pdf-rs reads `/Kids` with `Dict::array_entry`,
/// which returns `None` for an indirect reference instead of following it, so such a node has
/// no children as far as the walk is concerned. The walk treats a childless node as the page it
/// was looking for: `page_dict` then returns the `/Pages` node itself for every page number,
/// with no error, and that dictionary has neither `/MediaBox` nor `/Contents`.
///
/// Repairing at open rather than reacting to the symptom in [`Doc::page`] is deliberate. A
/// missing `/MediaBox` is only how the damage shows when the bad node is the root; an
/// intermediate `/Pages` node that happens to carry inheritable attributes yields a page dict
/// that looks entirely plausible and is silently the wrong page.
///
/// The repair is load-bearing for the writer too: it serializes THIS context, and on the
/// `indirect-kids` corpus document a bare read-then-write yields 594 bytes and zero pages, with
/// a nil error and a clean re-read.
///
/// Go recurses; this walks with a stack, so a hostile tree cannot overflow the call stack. Each
/// node is handled at most once, which is all that Go's `seen` map was for.
pub(crate) fn normalize_page_tree(xt: &mut XRefTable, root: Ref) {
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(r) = stack.pop() {
        if !seen.insert(r.num) {
            // A cycle, or a node reachable twice. pdf-rs rejects both when it walks the tree;
            // this pass only has to avoid looping before it gets there.
            continue;
        }
        let kids = match xt.dereference_dict_ref(r) {
            Ok(Some(d)) => match d.find(b"Kids") {
                Some(k) => k.clone(),
                None => continue, // a leaf page
            },
            _ => continue,
        };
        let arr: Array = match xt.dereference_array(&kids) {
            Ok(Some(a)) => a.clone(),
            _ => continue,
        };
        if let Ok(Some(d)) = xt.dereference_dict_mut(r) {
            d.set(b"Kids", Object::Array(arr.clone()));
        }
        // The kids are pushed last first, so they are handled in order, as Go's loop does.
        for k in arr.iter().rev() {
            if let Object::Ref(kr) = k {
                stack.push(*kr);
            }
        }
    }
}

fn rect_of(r: &pdf::object::Rect) -> Rect {
    Rect {
        llx: r.ll.x,
        lly: r.ll.y,
        urx: r.ur.x,
        ury: r.ur.y,
    }
}

/// Go's `max` builtin on floats: NaN wins, and +0 is greater than -0.
fn gmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b { a } else { b }
}

/// Go's `min` builtin on floats: NaN wins, and -0 is less than +0.
fn gmin(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && a == b {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b { a } else { b }
}

/// `intersectRect` returns the overlap of `a` and `b`, and `None` if they don't overlap at all.
fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect {
        llx: gmax(a.llx, b.llx),
        lly: gmax(a.lly, b.lly),
        urx: gmin(a.urx, b.urx),
        ury: gmin(a.ury, b.ury),
    };
    if r.llx >= r.urx || r.lly >= r.ury {
        return None;
    }
    Some(r)
}

/// `lastFilter` is the name of `sd`'s last filter, empty for none.
fn last_filter(sd: &StreamDict) -> &[u8] {
    match sd.filter_pipeline.last() {
        Some(f) => f.name.as_slice(),
        None => &[],
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A buffer that grows in charged steps, for the concatenation of a page's content streams.
struct Concat {
    v: Vec<u8>,
    held: Held,
}

impl Concat {
    fn new(xt: &XRefTable) -> Result<Concat> {
        Ok(Concat {
            v: Vec::new(),
            held: xt.mem().charge(0)?,
        })
    }

    fn extend(&mut self, p: &[u8]) -> Result<()> {
        self.held.grow_by(p.len() as u64)?;
        self.v.try_reserve(p.len())?;
        self.v.extend_from_slice(p);
        Ok(())
    }

    /// The bytes; the charge is released here, because the page leaves the call.
    fn into_vec(self) -> Vec<u8> {
        self.v
    }
}

/// The Adler-32 of RFC 1950.
struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    fn new() -> Adler32 {
        Adler32 { a: 1, b: 0 }
    }

    fn update(&mut self, p: &[u8]) {
        // 5552 is the most bytes whose sums fit a u32 before the modulus.
        for chunk in p.chunks(5552) {
            for &x in chunk {
                self.a += u32::from(x);
                self.b += self.a;
            }
            self.a %= 65521;
            self.b %= 65521;
        }
    }

    fn sum(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

impl Doc {
    /// `OpenLimits`: [`open`] as an associated function.
    ///
    /// # Errors
    /// As [`open`].
    pub fn open_limits(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Doc> {
        open(data, lim, cancel)
    }

    /// `&doc{ctx: ctx}`: a document over a context that was built or read elsewhere, with
    /// nothing resolved yet. The page tree is NOT normalized (see [`normalize_page_tree`]);
    /// [`open`] does that for a context it reads.
    pub fn from_context(ctx: Context, lim: &Limits) -> Doc {
        Doc::new(ctx, lim)
    }

    fn new(ctx: Context, lim: &Limits) -> Doc {
        Doc {
            ctx,
            scopes: Vec::new(),
            images: BTreeMap::new(),
            streams: BTreeMap::new(),
            refs: BTreeMap::new(),
            next_id: -1,
            font_refs: BTreeMap::new(),
            res_adopters: Vec::new(),
            adopted_held: None,
            font_cache: BTreeMap::new(),
            num_arrays: BTreeMap::new(),
            lim: *lim,
            page_bytes: 0,
            page_forms: BTreeMap::new(),
            page_held: Vec::new(),
            limit_err: None,
            held: Vec::new(),
        }
    }

    /// `PageCount`.
    pub fn page_count(&self) -> usize {
        usize::try_from(self.ctx.xref.page_count).unwrap_or(0)
    }

    /// Records a swallowed error that is a cap or budget refusal, for [`Doc::err`].
    pub(crate) fn note(&mut self, e: pdf::Error) {
        if let Err(o) = seam("lookup", e) {
            self.note_err(o);
        }
    }

    /// As [`Doc::note`] for a byblos error: only the first limit breach is kept.
    pub(crate) fn note_err(&mut self, e: Error) {
        if is_limit(&e) && self.limit_err.is_none() {
            self.limit_err = Some(e);
        }
    }

    /// `Page` reads page `n` (1-based): its boxes, rotation, resource scope and decoded content.
    ///
    /// # Errors
    /// [`Error::Malformed`] for a page out of range, with no dictionary, or whose content
    /// streams are all unreadable; [`Error::Limit`] over a stream cap; [`Error::Alloc`];
    /// [`Error::Pdf`] for any other error.
    pub fn page(&mut self, n: usize) -> Result<Page> {
        let what = format!("page {n}");
        let pc = self.ctx.xref.page_count;
        let ni = i64::try_from(n).unwrap_or(i64::MAX);
        if n < 1 || ni > pc {
            return Err(Error::Malformed(format!("page {n} out of range 1..{pc}")));
        }
        // consolidateRes=false (pdf-rs has no other mode), for the reasons text.go's
        // AddFontResource already gives, plus one it could not see (byb-ged). true does two
        // things, and byblos wants neither:
        //
        //   - It PRUNES the returned resource dict to the names pdf-go found in the page's OWN
        //     content stream, never descending into a form's content to do it. This dict
        //     becomes Page.scope, so an image only a Form XObject paints is deleted from the
        //     scope the form resolves through, and inspect reports a page with fewer images and
        //     no error at all. Measured: govdocs1 500973.pdf page 1 reported 1 image where
        //     pdfimages lists 5.
        //   - It rejects the page outright when the content names a resource category the dict
        //     does not declare ("missing required resource subdict"), and on "corrupt BI
        //     expression" and "corrupt page content". Because inspect walks every page, that
        //     loses the WHOLE document: 8 of 4,840 govdocs1 files, all of which poppler reads.
        //
        // What false gives up is pdf-go's non-standard MERGE of a page's own /Resources with an
        // ancestor /Pages node's. ISO 32000-1 7.7.3.4 makes /Resources inheritable, not
        // additive: a page that declares one gets its own, and poppler agrees.
        let (pd, _, inh) = self.ctx.xref.page_dict(ni, &NEVER).map_err(|e| {
            // Object streams are decoded when first read, so the cumulative limits can end
            // a page.
            pdf_err(&self.lim, &what, format!("byblos/pdfdoc: page {n} dict"), e)
        })?;

        // A page with no /MediaBox anywhere in its inheritance chain is malformed: ISO 32000-1
        // 7.7.3.3 makes it a required inheritable attribute. Byblos used to refuse the whole
        // document, and byb-8ly measured what that cost: nine of the 4,840 govdocs1 sample
        // files, every one PDF 1.0, none using object streams, with the string "MediaBox"
        // absent from the file's bytes altogether. Poppler reads all 9.
        //
        // It defaults instead, to the US Letter box poppler defaults to, and SAYS SO through
        // `media_box_defaulted`: 612x792 is a convention, not a measurement of the document,
        // and a caller deciding whether a raster covers the page is entitled to know its page
        // box was supplied rather than read.
        let defaulted = inh.media_box.is_none();
        let mb = inh.media_box.unwrap_or_else(|| {
            pdf::object::Rect::for_dim(DEFAULT_PAGE_WIDTH_PT, DEFAULT_PAGE_HEIGHT_PT)
        });

        // A form Do appends a scope each, so keeping them for the document grows with pages x
        // draws. A Page's scope is valid until the next page call.
        self.scopes.clear();
        let scope = self.add_scope(inh.resources, -1);
        let media_box = rect_of(&mb);
        let mut p = Page {
            index: n,
            media_box,
            crop_box: media_box,
            rotate: inh.rotate.rem_euclid(360),
            content: Vec::new(),
            scope,
            media_box_defaulted: defaulted,
            content_recovered: false,
        };
        // ISO 32000-1 7.7.3.3: the CropBox is to be intersected with the MediaBox, not taken
        // verbatim; poppler does this too. An empty intersection (the two boxes don't overlap
        // at all) falls back to the MediaBox rather than reporting a degenerate page.
        if let Some(cb) = &inh.crop_box
            && let Some(c) = intersect_rect(rect_of(cb), media_box)
        {
            p.crop_box = c;
        }

        // A page with no /Contents is legal and empty, and so is a page whose /Contents is
        // there and resolves to zero bytes: an ordinary blank page, which is a duplex scan's
        // back side, a section separator, or a form page left deliberately empty.
        let (c, recovered) = self
            .page_contents(&pd, &NEVER)
            .map_err(|e| self.page_err(n, "content", e))?;
        p.content = c;
        p.content_recovered = recovered;
        Ok(p)
    }

    /// The wrap of Go's `fmt.Errorf("byblos/pdfdoc: page %d content: %w", n, err)`, after
    /// `pdfcpuLimit`.
    fn page_err(&self, n: usize, what: &str, e: Error) -> Error {
        match e {
            Error::Pdf(p) => match pdf_limit(&self.lim, &p) {
                Some(l) => l,
                None => Error::Pdf(p.context(format!("byblos/pdfdoc: page {n} {what}"))),
            },
            Error::Malformed(s) => Error::Malformed(format!("byblos/pdfdoc: page {n} {what}: {s}")),
            other => other,
        }
    }

    /// `pageContents` decodes a page's content streams, and is the ONE place that decides what
    /// a page's bytes are. `page`, `append_content` and `content_depth` all route through it.
    ///
    /// It replaces a direct page-content call of pdf-rs, which byblos cannot use, for a reason
    /// byb-3iw measured over every page of 4,840 documents. Under the relaxed validation mode
    /// pdf-go's `decodeContentStream` SWALLOWS a flate failure: the stream contributes zero
    /// bytes and the page is reported as fine. When `/Contents` is a single stream that is
    /// `ErrNoContent`, which byblos used to re-decode separately to tell a blank page from a
    /// shredded one. But when `/Contents` is an ARRAY and a sibling element decodes, the sum is
    /// non-zero, the call returns success, and the damaged element is dropped in silence. That
    /// is the corpus's 150277 p25 (5,452 bytes to its last 469, -91.4%) and it is invisible
    /// from outside the library, so the decode has to happen here.
    ///
    /// The bool reports that at least one stream failed its integrity check and byblos kept
    /// the bytes that decoded anyway. It does NOT mean bytes were lost: a corrupted Adler-32
    /// trailer alone still yields the complete stream. What it means is that the content does
    /// not match what the file attests, which is what a caller needs to know either way.
    ///
    /// The elements are concatenated with NO separator between them, which is what pdf-go does
    /// and what every other reader does. Inserting a newline here would change the content
    /// bytes of every clean multi-element page in the corpus.
    ///
    /// The result is charged to the ledger while it is built; the charge is released on return.
    /// `cancel` interrupts the stream decodes.
    ///
    /// # Errors
    /// [`Error::Malformed`] for a `/Contents` that is neither a stream nor an array and for a
    /// page whose every element is unreadable (it is SHREDDED, not blank); a limit or
    /// allocation refusal; [`Error::Pdf`] for a dereference error.
    pub(crate) fn page_contents(
        &mut self,
        pd: &Dict,
        cancel: &AtomicBool,
    ) -> Result<(Vec<u8>, bool)> {
        // A new page: the running total of decoded non-image streams starts over.
        self.page_bytes = 0;
        self.page_forms.clear();
        self.page_held.clear();
        if pd.find(b"Contents").is_none() {
            return Ok((Vec::new(), false));
        }
        // Go checks with Find but reads the exact key.
        let null = Object::Null;
        let contents = pd.0.get(b"Contents".as_slice()).unwrap_or(&null);
        let o = self
            .ctx
            .xref
            .dereference(contents)
            .cloned()
            .map_err(|e| conv("page content", e))?;

        // /Contents may be an indirect reference to an object that does not exist, or to a
        // null. ISO 32000-1 7.3.10 makes a reference to a nonexistent object null, and 7.3.9
        // makes null equivalent to omitting the entry, so this is a legally blank page and not
        // an error.
        // /Contents is one stream or an array of them (ISO 32000-1 table 30).
        let objs: Array = match o {
            Object::Null => return Ok((Vec::new(), false)),
            Object::Array(a) => a,
            s @ Object::Stream(_) => vec![s],
            _ => {
                return Err(Error::Malformed(
                    "page content must be stream dict or array".into(),
                ));
            }
        };

        let mut buf = Concat::new(&self.ctx.xref)?;
        let mut recovered = false;
        let mut dropped = 0usize;
        let total = objs.len();
        for obj in &objs {
            if obj.is_null() {
                continue;
            }
            let sd = self
                .ctx
                .xref
                .dereference(obj)
                .cloned()
                .map_err(|e| conv("page content", e))?;
            // A null entry, or an element that is not a stream at all. pdf-go skips both
            // rather than failing the page, and so does this.
            let Object::Stream(s) = sd else {
                continue;
            };
            let mut s = *s;
            if let Err(e) = self.decode(&mut s, true, cancel) {
                // A cap or budget refusal is the document's, not damage in one element. A
                // `Malformed` is a pdf-go panic, which ends the page.
                if is_limit(&e) || !matches!(e, Error::Pdf(_)) {
                    return Err(e);
                }
                // A DAMAGED ELEMENT MUST NOT FAIL THE WHOLE PAGE. That is what v0.13 did, in
                // relaxed mode, and a version bump is not licence to lose a page byblos used to
                // read: measured, refusing here dropped a healthy sibling's 39 bytes and broke
                // AppendContent on documents v0.13 stamped fine.
                //
                // Two shapes, two answers. A stream whose deflate data is INTACT and whose
                // Adler-32 trailer is not decodes correctly once the trailer is repaired. A
                // stream whose deflate data is itself corrupt has no trailer worth repairing,
                // so it is DROPPED, which is again what v0.13's relaxed mode did.
                //
                // (A decode limit is not damage either, but `decode` has turned it into a
                // limit error above, so no second decode of the same bomb is ever started.)
                let (repaired, ok) = self.decode_recovering(&s, cancel)?;
                if ok {
                    if let Some(r) = repaired {
                        buf.extend(&r)?;
                    }
                } else {
                    dropped += 1;
                }
                recovered = true;
                continue;
            }
            if let Some(c) = &s.content {
                buf.extend(c)?;
            }
        }
        if !buf.v.is_empty() {
            return Ok((buf.into_vec(), recovered));
        }
        if dropped > 0 {
            // Every element was unreadable, so this page is SHREDDED, not blank, and byblos
            // refuses it exactly as it did before. Telling the two apart is the whole reason
            // this function exists: pdf-go reports both with the identical `ErrNoContent` and
            // the identical zero bytes.
            return Err(Error::Malformed(format!(
                "content stream decode: {dropped} of {total} streams unreadable"
            )));
        }
        // A legal blank page: a duplex scan's back side, a section separator, a form page left
        // deliberately empty. `recovered` is returned rather than a literal false: a page whose
        // only stream failed its integrity check AND inflated to zero bytes did have a
        // recovery.
        Ok((Vec::new(), recovered))
    }

    /// `decodeRecovering` decodes `s` ONE FILTER AT A TIME, repairing a flate layer's Adler-32
    /// trailer when that is the only thing wrong with it.
    ///
    /// One filter at a time is the whole point, and it is what six successive regressions
    /// converged on: pdf-go v0.15 propagates a checksum error and then DISCARDS the bytes it was
    /// handed. Earlier attempts repaired ONE layer and asked the library to decode the rest of
    /// the pipeline in one call. Each was refuted by a shape it could not express: a filter
    /// CHAIN where the flate layer is not the outermost, a chain whose SECOND flate layer was
    /// the damaged one, a chain with BOTH flate layers damaged.
    ///
    /// Walking the pipeline removes the guess. Each filter is applied on its own, so a damaged
    /// flate layer is repaired at the point it is reached, N of them are repaired
    /// independently, and pdf-rs still applies every filter and every `/DecodeParms` predictor
    /// itself. Nothing here reimplements a PDF filter.
    ///
    /// The bool is false when some layer cannot be decoded even after a repair, and its element
    /// is then DROPPED rather than failing the page; see [`Doc::page_contents`].
    fn decode_recovering(
        &mut self,
        s: &StreamDict,
        cancel: &AtomicBool,
    ) -> Result<(Option<Bytes>, bool)> {
        let max = self
            .lim
            .stream_bytes()
            .min(self.lim.page_stream_bytes() - self.page_bytes);
        if max <= 0 {
            return Ok((None, false));
        }
        let mem = self.ctx.xref.mem().clone();
        let mut data: Option<Bytes> = s.raw.clone();
        for f in &s.filter_pipeline {
            // One filter, over the bytes the previous one produced. DecodeParms travels with
            // the filter, so a predictor is applied by pdf-rs on the layer that declares it.
            let mut step = s.clone();
            step.raw = data.clone();
            step.filter_pipeline = vec![f.clone()];
            step.content = None;
            let e = match step.decode_with_limit(max, &mem, cancel) {
                Ok(()) => {
                    data = step.content.clone();
                    continue;
                }
                Err(e) => e,
            };
            let e = seam("page content", e)?;
            // Only a flate layer can be repaired; any other filter failing is a drop.
            if f.name.as_slice() != FLATE {
                return Ok((None, false));
            }
            // A DECODE LIMIT is not a damaged checksum, and attempting a repair on one is both
            // pointless and expensive: repairing the trailer cannot make the stream smaller, so
            // the retry would inflate the same bomb a second time and hit the same cap.
            if e.is_decode_limit_exceeded() {
                return Ok((None, false));
            }
            let Some(repaired) =
                self.repair_trailer(data.as_deref().unwrap_or(&[]), max, cancel)?
            else {
                return Ok((None, false));
            };
            step.raw = Some(repaired);
            step.content = None;
            if let Err(e) = step.decode_with_limit(max, &mem, cancel) {
                seam("page content", e)?;
                return Ok((None, false));
            }
            data = step.content.clone();
        }
        Ok((data, true))
    }

    /// `repairTrailer` returns `zstream` with its Adler-32 trailer replaced by the one its
    /// deflate data implies, and everything past the trailer removed.
    ///
    /// `None` when the deflate data does not inflate (that stream is damaged under the trailer
    /// and there is nothing here to repair), or when it is not a zlib stream this can rebuild.
    /// `max` bounds the inflate, which only has to be as large as a decode that follows may
    /// produce: a stream that inflates to more fails that decode on its limit anyway.
    fn repair_trailer(
        &self,
        zstream: &[u8],
        max: i64,
        cancel: &AtomicBool,
    ) -> Result<Option<Bytes>> {
        // A zlib stream is a 2-byte header, the deflate data, and a 4-byte Adler-32 of the
        // UNCOMPRESSED bytes (RFC 1950). FDICT, header bit 5, prepends a dictionary id this
        // does not handle; PDF producers do not set it.
        const ZLIB_HEADER: usize = 2;
        const ADLER_LEN: usize = 4;
        if zstream.len() < ZLIB_HEADER + ADLER_LEN || zstream.get(1).is_none_or(|b| b & 0x20 != 0) {
            return Ok(None);
        }

        // THE TRAILER IS NOT NECESSARILY THE LAST FOUR BYTES. A /Length that over-declares,
        // routinely by swallowing the EOL before `endstream`, leaves those extra bytes in the
        // raw data, and pdf-go's relaxed mode repairs a SHORT /Length but not a long one.
        // Patching at len-4 then wrote the correct checksum to the wrong offsets.
        //
        // So the end of the deflate data is FOUND, not assumed: the DEFLATE reader takes from
        // its source only the bytes it consumes, so whatever the source has left after the last
        // deflate block begins with the real trailer.
        let mem = self.ctx.xref.mem();
        let _window = mem.charge(INFLATE_WINDOW_BYTES)?;
        let limits = deflate::Limits {
            max_output: usize::try_from(max).unwrap_or(usize::MAX),
            max_alloc: INFLATE_WINDOW_BYTES as usize,
        };
        let mut rest: &[u8] = zstream.get(ZLIB_HEADER..).unwrap_or(&[]);
        let mut sum = Adler32::new();
        {
            // Inflating directly skips the checksum entirely rather than ignoring a failure
            // reported after the fact. Data that is ITSELF corrupt fails here: that rejects a
            // reserved deflate block type. A stream that inflates to ZERO bytes without error is
            // NOT rejected: it is a legally empty content stream carrying a bad checksum, and
            // refusing it would report a blank page as shredded.
            //
            // THE INFLATED BYTES ARE NEVER HELD. Only their Adler-32 is wanted, so they are
            // streamed through the hash. Holding them would allocate the whole inflated size on
            // a path that is reached on a DECODE FAILURE.
            let mut rd = deflate::Reader::new(&mut rest, &limits, cancel);
            let mut chunk = [0u8; 32 << 10];
            loop {
                match rd.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => sum.update(chunk.get(..n).unwrap_or(&[])),
                    Err(e) => {
                        return match e.get_ref().and_then(|g| g.downcast_ref::<deflate::Error>()) {
                            Some(deflate::Error::Cancelled) => Err(Error::Cancelled),
                            Some(deflate::Error::AllocLimit) => {
                                Err(Error::Alloc { got: 0, max: 0 })
                            }
                            _ => Ok(None),
                        };
                    }
                }
            }
        }
        // First byte past the deflate data.
        let end = zstream.len().saturating_sub(rest.len());
        if rest.len() < ADLER_LEN {
            // THIS GUARD IS REACHED, and it is the difference between dropping an element and
            // failing. With a /Predictor, pdf-go uses `decodePostProcessRows`, which returns
            // an unexpected-EOF error unswallowed when the final row is PARTIAL, so a predictor
            // stream whose last row is short and whose trailer is cut to 1-3 bytes arrives
            // here.
            return Ok(None);
        }

        // Anything past the trailer is not part of the zlib stream and is dropped with it, so
        // the repaired stream is exactly header + data + checksum.
        let n = end + ADLER_LEN;
        let held = mem.charge(n as u64)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(n)?;
        raw.extend_from_slice(zstream.get(..n).unwrap_or(&[]));
        let s = sum.sum();
        // RFC 1950 stores the Adler-32 most significant byte first.
        for (i, b) in s.to_be_bytes().iter().enumerate() {
            if let Some(slot) = raw.get_mut(end + i) {
                *slot = *b;
            }
        }
        Ok(Some(Bytes::new(raw, n, Some(held))))
    }

    /// `addScope` appends a resource scope and returns its handle.
    ///
    /// Scopes live for one page: `page` resets them. Resolving the same form twice on a page
    /// appends duplicate entries, bounded by the walk's form cap.
    pub(crate) fn add_scope(&mut self, res: Option<Dict>, parent: i64) -> i64 {
        self.scopes.push(Scope { res, parent });
        self.scopes.len() as i64 - 1
    }

    /// `XObject` resolves the named `/XObject` resource in `scope`: an image (recorded for
    /// [`Doc::image_info`] and [`Doc::raw_image`]) or a Form (decoded once per page, with a
    /// scope of its own). `None` when the name does not resolve or is neither.
    ///
    /// A form over a stream cap answers `None` and keeps the first such breach for
    /// [`Doc::err`].
    pub fn xobject(&mut self, sc: i64, name: &[u8]) -> Option<XObject> {
        let obj = self.lookup_resource(sc, b"XObject", name)?;
        let id = self.identify(&obj);
        let sd = match self.ctx.xref.dereference_stream_dict(&obj) {
            Ok((Some(sd), _)) => sd,
            Ok(_) => return None,
            Err(e) => {
                self.note(e);
                return None;
            }
        };
        let sub = sd.dict.subtype().map(<[u8]>::to_vec);
        match sub.as_deref() {
            Some(b"Image") => {
                let cs_name = match sd.dict.find(b"ColorSpace") {
                    Some(Object::Name(n)) => lossy(n),
                    _ => String::new(),
                };
                let info = ImageInfo {
                    name: lossy(name),
                    obj_nr: id,
                    width: self.int_entry(&sd.dict, b"Width"),
                    height: self.int_entry(&sd.dict, b"Height"),
                    bpc: self.int_entry(&sd.dict, b"BitsPerComponent"),
                    image_mask: self.bool_entry(&sd.dict, b"ImageMask"),
                    smask: sd.dict.find(b"SMask").is_some(),
                    mask: sd.dict.find(b"Mask").is_some(),
                    decode: sd.dict.find(b"Decode").is_some(),
                    decode_array: self.number_array(&sd.dict, b"Decode"),
                    filter: self.filter_name(&sd.dict),
                    color_space: cs_name,
                };
                self.images.insert(id, info);
                // Keep the stream dictionary. `raw_image` renders from this rather than asking
                // pdf-rs which objects a page uses, because that answer comes from the
                // optimize pass, which deduplicates identical rasters.
                self.streams.insert(id, sd);
                // And its xref identity, which `replace_image` needs to write the substituted
                // stream back: the stream dict is a copy, not a handle.
                if let Object::Ref(r) = &obj {
                    self.refs.insert(id, *r);
                }
                Some(XObject {
                    image: true,
                    id,
                    ..XObject::default()
                })
            }
            Some(b"Form") => {
                let mut sd = sd;
                // A form invoked many times on a page is decoded once and counts once; every
                // Do shares that buffer, so a form that draws itself holds one decode, not one
                // per nesting level.
                let cached = if id >= 0 {
                    self.page_forms.get(&id).cloned()
                } else {
                    None
                };
                let stream = match cached {
                    Some(c) => c,
                    None => {
                        if let Err(e) = self.decode(&mut sd, true, &NEVER) {
                            self.note_err(e);
                            return None;
                        }
                        let stream = match self.stream_of(sd.content.as_deref().unwrap_or(&[])) {
                            Ok(s) => s,
                            Err(e) => {
                                self.note_err(e);
                                return None;
                            }
                        };
                        if id >= 0 {
                            self.page_forms.insert(id, stream.clone());
                        }
                        stream
                    }
                };
                let mut m = IDENTITY;
                let arr = self.array_entry(&sd.dict, b"Matrix");
                if arr.len() == 6 {
                    for (i, o) in arr.iter().enumerate() {
                        match self.number(o) {
                            Some(v) => {
                                if let Some(slot) = m.0.get_mut(i) {
                                    *slot = v;
                                }
                            }
                            None => {
                                m = IDENTITY;
                                break;
                            }
                        }
                    }
                }
                let mut bbox: Option<BBox> = None;
                let arr = self.array_entry(&sd.dict, b"BBox");
                if arr.len() == 4 {
                    let mut vals = [0f64; 4];
                    let mut ok = true;
                    for (slot, o) in vals.iter_mut().zip(arr.iter()) {
                        match self.number(o) {
                            Some(v) => *slot = v,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        bbox = Some(BBox {
                            llx: vals[0],
                            lly: vals[1],
                            urx: vals[2],
                            ury: vals[3],
                        });
                    }
                }
                // A form without its own /Resources inherits the enclosing resource dictionary
                // (ISO 32000-1 section 8.10.2), which the scope's parent chain provides.
                let res = self.dict_entry(&sd.dict, b"Resources");
                let form_scope = self.add_scope(res, sc);
                Some(XObject {
                    content: stream,
                    matrix: m,
                    scope: form_scope,
                    bbox,
                    ..XObject::default()
                })
            }
            _ => None,
        }
    }

    /// A copy of `c` for a form's walk, charged to the ledger until the next page.
    fn stream_of(&mut self, c: &[u8]) -> Result<content::Stream> {
        let held = self.ctx.xref.mem().charge(c.len() as u64)?;
        let mut v = Vec::new();
        v.try_reserve_exact(c.len())?;
        v.extend_from_slice(c);
        self.page_held.push(held);
        Ok(Arc::new(v))
    }

    /// `lookupResource` walks the scope chain so a form that declares only `/Font` still
    /// resolves images through its parent. `category` is a resource dictionary key: `XObject`,
    /// `ExtGState`.
    fn lookup_resource(&mut self, sc: i64, category: &[u8], name: &[u8]) -> Option<Object> {
        let mut i = sc;
        while i >= 0 {
            let idx = usize::try_from(i).ok()?;
            let scope = self.scopes.get(idx)?;
            if let Some(res) = &scope.res
                && let Some(c) = res.0.get(category)
                && let Ok(Some(cat)) = self.ctx.xref.dereference_dict(c)
                && let Some(o) = cat.find(name)
            {
                return Some(o.clone());
            }
            i = scope.parent;
        }
        None
    }

    /// `ExtGStateOpaque` reports whether the named graphics state leaves painting fully opaque.
    ///
    /// A name that does not resolve, or a value that is not a dictionary, is reported as not
    /// opaque. That is the direction that costs a divert rather than a wrong document: the
    /// caller uses this to decide whether an image hides what is painted beneath it.
    pub fn ext_gstate_opaque(&mut self, sc: i64, name: &[u8]) -> bool {
        let Some(obj) = self.lookup_resource(sc, b"ExtGState", name) else {
            return false;
        };
        let Object::Dict(gs) = self.deref(&obj) else {
            return false;
        };
        // /ca is the non-stroking alpha and /CA the stroking one. An image is painted with /ca,
        // but a state that sets either is not one to reason about occlusion from.
        for key in [b"ca".as_slice(), b"CA".as_slice()] {
            if let Some(o) = gs.0.get(key)
                && self.number(o).is_some_and(|v| v < 1.0)
            {
                return false;
            }
        }
        // /SMask /None is how a graphics state says it has no soft mask. Anything else is a
        // mask, and reading it would mean rendering it.
        if let Some(sm) = gs.find(b"SMask") {
            match self.deref(sm) {
                Object::Name(n) if n == b"None" => {}
                _ => return false,
            }
        }
        true
    }

    /// `Font` resolves the named `/Font` resource to a stable id, the font half of
    /// `content::Env`. The id is the font dict's object number when the resource is an indirect
    /// reference, and a synthetic negative one otherwise, fresh per call, exactly like the
    /// XObject path above. Every `/Font` in the corpus is indirect, so the direct-dict case is
    /// unexercised; if a real archive ever shows direct font dicts, that call site is where a
    /// per-(scope, name) cache would go, so repeated `Tf` ops naming the same dict agree.
    pub fn font(&mut self, sc: i64, name: &[u8]) -> Option<i64> {
        let obj = self.lookup_resource(sc, b"Font", name)?;
        Some(self.identify(&obj))
    }

    /// `RenderFont` resolves the named `/Font` resource for rendering. A composite
    /// `/Subtype /Type0` font is resolved through its descendant CIDFont when `/Encoding` is
    /// Identity-H, per the render font's own contract that only the Identity CMaps give code
    /// boundaries without decoding an embedded or predefined CMap. Any other `/Encoding` (a CMap
    /// stream, a predefined CJK CMap name, or Identity-V) returns `None` rather than a wrong or
    /// fallback resolution. Identity-V is refused, not resolved with horizontal metrics: the
    /// text show has no vertical mode, so it would ink glyphs where poppler inks none, and a
    /// census of all 5,672 pinned-sample documents finds 0 Identity-V among 4,482 Type0 dicts.
    pub fn render_font(&mut self, sc: i64, name: &[u8]) -> Option<RenderFont> {
        // Memoized by the font dict's object identity, not by (scope, name): a resource dict
        // aliasing one dict under many Tf names would otherwise re-run the full read-to-the-
        // limit decode on EVERY name, including a rejected /FontFile whose program comes back
        // empty, which nothing on the caller side bounds. Keyed by object number (a synthetic
        // id is never reused, so a direct dict just never hits).
        let obj = self.lookup_resource(sc, b"Font", name)?;
        let id = self.identify(&obj);
        if id >= 0
            && let Some(c) = self.font_cache.get(&id)
        {
            return c.clone();
        }
        let r = match self.render_font_inner(sc, name) {
            Ok(rf) => Some(rf),
            Err(e) => {
                self.note_err(e);
                None
            }
        };
        if id >= 0 {
            self.font_cache.insert(id, r.clone());
        }
        r
    }

    fn render_font_inner(&mut self, sc: i64, name: &[u8]) -> Result<RenderFont> {
        let shown = lossy(name);
        let mut rf = RenderFont::default();
        // The same shape as `raw_image_globals`: this dereferences arbitrary objects a font dict
        // names and runs pdf-rs's filter chain over one of them, so a corrupt dict must divert
        // this font, not take the page down.
        let Some(obj) = self.lookup_resource(sc, b"Font", name) else {
            return Err(Error::Malformed(format!("no /Font resource {shown:?}")));
        };
        let Object::Dict(dict) = self.deref(&obj) else {
            return Err(Error::Malformed(format!("/Font {shown:?} is not a dict")));
        };
        // `width_dict` is where /FirstChar, /Widths and /FontDescriptor are read from: the font
        // dict itself, unless it is a Type0 composite, in which case those live on its one
        // descendant CIDFont (ISO 32000-1 9.7.4).
        let mut width_dict = dict.clone();
        if dict.name_entry(b"Subtype") == Some(b"Type0".as_slice()) {
            match self.get_deref(&dict, b"Encoding") {
                Object::Name(n) if n == b"Identity-H" => {}
                _ => {
                    return Err(Error::Malformed(format!(
                        "/Font {shown:?} Type0 /Encoding is not Identity-H"
                    )));
                }
            }
            let desc_fonts = self.array_entry(&dict, b"DescendantFonts");
            if desc_fonts.len() != 1 {
                return Err(Error::Malformed(format!(
                    "/Font {shown:?} /DescendantFonts is not one element"
                )));
            }
            let Some(Object::Dict(desc)) = desc_fonts.first().map(|o| self.deref(o)) else {
                return Err(Error::Malformed(format!(
                    "/Font {shown:?} descendant is not a dict"
                )));
            };
            rf.type0 = true;
            // ISO 32000-1 9.7.4.3 default, overwritten below only if /DW parses as a number.
            rf.dw = 1000.0;
            if let Some(n) = desc.0.get(b"DW".as_slice()).and_then(|o| self.number(o)) {
                rf.dw = n;
            }
            rf.w = self.cid_widths(&desc);
            width_dict = desc;
        }
        if let Some(bf) = dict.name_entry(b"BaseFont") {
            rf.base_font = bf.to_vec();
        }
        if !rf.type0 {
            rf.first_char = self.int_entry(&dict, b"FirstChar");
            rf.widths = self
                .number_array(&dict, b"Widths")
                .map(|a| a.to_vec())
                .unwrap_or_default();
        }
        let Some(fd) = self.dict_entry(&width_dict, b"FontDescriptor") else {
            return Ok(rf);
        };
        rf.flags = self.int_entry(&fd, b"Flags");
        let font_limit = self.lim.font_program_bytes();
        let mem = self.ctx.xref.mem().clone();
        for key in [
            b"FontFile2".as_slice(),
            b"FontFile3".as_slice(),
            b"FontFile".as_slice(),
        ] {
            let Some(o) = fd.find(key) else {
                continue;
            };
            let Ok((Some(mut sd), _)) = self.ctx.xref.dereference_stream_dict(o) else {
                continue;
            };
            if let Err(e) = sd.decode_with_limit(font_limit, &mem, &NEVER) {
                // Damage in one font file is a font without a program; a refused allocation is
                // not.
                match seam("font program", e) {
                    Err(o) => return Err(o),
                    Ok(_) => continue,
                }
            }
            let Some(content) = &sd.content else {
                continue;
            };
            // `decode_with_limit` only consults the limit inside a filter stage: an unfiltered
            // stream (no /Filter, or a preserved image filter) returns its raw bytes untouched,
            // so a literal oversized program must be bounded here too; see
            // `Limits::font_program_bytes`.
            if content.len() as i64 > font_limit {
                continue;
            }
            let held = mem.charge(content.len() as u64)?;
            let mut v = Vec::new();
            v.try_reserve_exact(content.len())?;
            v.extend_from_slice(content);
            drop(held); // the program leaves the call
            rf.program = v;
            break;
        }
        Ok(rf)
    }

    /// `identify` returns a stable id for an XObject: its PDF object number when it is an
    /// indirect reference, and a negative synthetic id otherwise.
    fn identify(&mut self, o: &Object) -> i64 {
        if let Object::Ref(r) = o {
            return r.num;
        }
        let id = self.next_id;
        self.next_id -= 1;
        id
    }

    /// `ImageInfo` returns the dictionary facts for an image resolved by [`Doc::xobject`],
    /// keyed by the id that `xobject` returned.
    pub fn image_info(&self, id: i64) -> Option<&ImageInfo> {
        self.images.get(&id)
    }

    /// `RawImage` renders the image XObject that [`Doc::xobject`] previously resolved as `id`,
    /// and returns its bytes and the file type pdf-rs inferred (`png`, `tif`, `jpg`, `jpx` or
    /// `jbig2`).
    ///
    /// It renders from the stream dictionary this document already holds, on the already-parsed
    /// context. Two consequences worth stating, because the obvious alternative (extracting by
    /// page number) gets both wrong:
    ///
    /// * It is per-image, not per-file. A page-number extract runs validate and optimize on
    ///   every call, so extracting an N-page document would re-read, re-validate and
    ///   re-optimize it N times, and could reject on the validator that `open` deliberately
    ///   skipped.
    /// * It is per-object, not per-deduplicated-object. The optimize pass collapses
    ///   byte-identical image XObjects, so the map a page-number extract returns for page 2 of
    ///   a duplex scan is keyed by page 1's object number.
    ///
    /// The decode is bounded by the image's own header: pixels first, then the pdf-rs decode
    /// limit set to the declared decoded size. A forged header must not reach pdf-rs: it reads
    /// a limit of 0 as its 512 MiB default and a negative one as no limit. pdf-rs charges the
    /// decode's memory to the ledger itself; Go's reservation from the pool is not made.
    ///
    /// # Errors
    /// [`Error::Malformed`] for an id this document has not resolved and invalid dimensions;
    /// [`Error::Limit`] `image-pixels` or `image-bytes`; [`Error::UnsupportedCodec`];
    /// [`Error::Alloc`]; [`Error::Cancelled`]; [`Error::Pdf`] for any other error.
    pub fn raw_image(&mut self, cancel: &AtomicBool, id: i64) -> Result<(Vec<u8>, &'static str)> {
        // The same shape as `raw_image_globals` and `render_font`: this hands an arbitrary image
        // dict to pdf-rs, which reads entries it does not check for, and a corrupt image must
        // divert or skip, not take the process down (byb-11r). Rendering draws EVERY image
        // XObject on the page (byb-547), which made it live: govdocs1/400566.pdf p1 crashed
        // outright, 1 file in the 5,672-file pinned sample.
        let what = format!("rendering image {id}");
        let Some(sd) = self.streams.get(&id).cloned() else {
            return Err(Error::Malformed(format!(
                "image {id} has not been resolved on this document"
            )));
        };
        // The image's own header bounds its decode: pixels first, then pdf-rs's per-stream
        // decode limit set to the declared decoded size.
        let info = self.images.get(&id).cloned().unwrap_or_default();
        if info.width <= 0 || info.height <= 0 {
            return Err(Error::Malformed(format!(
                "image {id}: invalid dimensions {} x {}",
                info.width, info.height
            )));
        }
        limits::check(
            "image-pixels",
            info.width.wrapping_mul(info.height),
            self.lim.image_pixels(),
        )?;
        let mem = self.ctx.xref.mem().clone();
        let last = last_filter(&sd).to_vec();
        let mut bpc = info.bpc;
        // Four components when the colour space does not say.
        let mut n: i64 = 4;
        if info.image_mask {
            bpc = 1;
            n = 1;
        } else if last.as_slice() == CCITT_FAX {
            n = 1;
        } else if let Ok(c) = pdf::image::color_space_components(&mut self.ctx.xref, &sd)
            && c > 0
        {
            n = c;
            // pdf-rs counts the base space; a sample is one index.
            if let Some(a) = self.indexed(&sd) {
                n = 1;
                // The lookup table is a stream pdf-rs inflates when it paints: 256 entries of
                // the base space is all there is.
                let lut = a.get(3).map(|o| self.ctx.xref.dereference_stream_dict(o));
                if let Some(Ok((Some(mut lut), _))) = lut {
                    let cap = c.wrapping_mul(256);
                    if let Err(e) = lut.decode_with_limit(cap, &mem, cancel) {
                        if e.is_decode_limit_exceeded() {
                            return Err(Error::Limit {
                                name: "image-bytes",
                                got: cap.wrapping_add(1),
                                max: cap,
                            });
                        }
                        return Err(wrapped(
                            &what,
                            format!("byblos/pdfdoc: image {id}: lookup table"),
                            e,
                        ));
                    }
                }
            }
        }
        let mut declared =
            image_decode_bytes(info.width, info.height, bpc, n, sd.filter_pipeline.len());
        // The bound is on pixels. When the last filter is a codec, the layers in front of it
        // (Flate around DCT) yield compressed bytes, which for a tiny image outweigh its
        // pixels, so they get a floor.
        let codec = matches!(last.as_slice(), DCT | JPX | JBIG2);
        if codec {
            declared = declared.max(1 << 20);
        }
        if declared <= 0 {
            // Overflow.
            let max = self.lim.stream_bytes();
            return Err(Error::Limit {
                name: "image-bytes",
                got: max.saturating_add(1),
                max,
            });
        }
        // pdf-rs decodes everything but a codec's stream itself and charges its buffers to the
        // ledger; a soft mask is decoded too, under the size its own dictionary allows, so a
        // stream that lies about it is refused.
        if !codec {
            let (sm, sm_cap) = self.soft_mask(&sd, &info);
            check_cancel(cancel)?;
            if let Some(mut sm) = sm {
                match sm.decode_with_limit(sm_cap, &mem, cancel) {
                    Ok(()) => {}
                    Err(e) if e.is_decode_limit_exceeded() => {
                        return Err(Error::Limit {
                            name: "image-bytes",
                            got: sm_cap.saturating_add(1),
                            max: sm_cap,
                        });
                    }
                    Err(e) => {
                        return Err(wrapped(
                            &what,
                            format!("byblos/pdfdoc: image {id}: soft mask"),
                            e,
                        ));
                    }
                }
            }
        }
        let saved = self.ctx.xref.conf.limits.max_decode_bytes;
        self.ctx.xref.conf.limits.max_decode_bytes = declared as u64;
        let r = self.extract_one(cancel, id, &sd, &info, declared);
        self.ctx.xref.conf.limits.max_decode_bytes = saved;
        r
    }

    /// The part of `RawImage` that runs under the raised decode limit: the CMYK DCT gate and
    /// `ExtractImage`.
    fn extract_one(
        &mut self,
        cancel: &AtomicBool,
        id: i64,
        sd: &StreamDict,
        info: &ImageInfo,
        declared: i64,
    ) -> Result<(Vec<u8>, &'static str)> {
        let what = format!("rendering image {id}");
        self.reserve_cmyk_dct(cancel, sd, info, declared)?;
        // The call changes the stream dictionary (its decoded content, the CCITT decode
        // parameters, the DCT component count). Go changes the one the document keeps; the copy
        // is dropped here so the decoded bytes are not held, and charged to the ledger, for the
        // life of the document.
        let mut sd = sd.clone();
        let im = match pdf::image::extract_image(
            &mut self.ctx,
            &mut sd,
            info.name.as_bytes(),
            id,
            cancel,
        ) {
            Ok(im) => im,
            Err(e) => {
                return Err(match seam(&what, e) {
                    Err(o) => o,
                    Ok(p) if p.is_decode_limit_exceeded() => Error::Limit {
                        name: "image-bytes",
                        got: declared.saturating_add(1),
                        max: declared,
                    },
                    Ok(p) => Error::Pdf(p.context(format!("byblos/pdfdoc: rendering image {id}"))),
                });
            }
        };
        // pdf-go signals "I will not render this filter" by returning a nil reader and an empty
        // file type, with a nil error.
        match im {
            Some(im) if !im.file_type.is_empty() => Ok((im.data, im.file_type)),
            _ => Err(Error::UnsupportedCodec),
        }
    }

    /// `indexed` is `sd`'s `/ColorSpace` array when it is an `/Indexed` one, else `None`.
    fn indexed(&mut self, sd: &StreamDict) -> Option<Array> {
        let Object::Array(a) = self.get_deref(&sd.dict, b"ColorSpace") else {
            return None;
        };
        if a.len() < 4 {
            return None;
        }
        match self.deref(a.first()?) {
            Object::Name(n) if n == b"Indexed" => Some(a),
            _ => None,
        }
    }

    /// `softMask` is image `sd`'s `/SMask` stream and the decoded size its own dictionary
    /// allows (8-bit gray unless it says otherwise); `None` and 0 when there is none. A mask of
    /// unusable size is held to the image's.
    fn soft_mask(&mut self, sd: &StreamDict, info: &ImageInfo) -> (Option<StreamDict>, i64) {
        let Some(o) = sd.dict.find(b"SMask") else {
            return (None, 0);
        };
        let sm = match self.ctx.xref.dereference_stream_dict(o) {
            Ok((Some(sm), _)) => sm,
            _ => return (None, 0),
        };
        let mut w = self.int_entry(&sm.dict, b"Width");
        let mut h = self.int_entry(&sm.dict, b"Height");
        if w <= 0 || h <= 0 {
            w = info.width;
            h = info.height;
        }
        let bpc = self.int_entry(&sm.dict, b"BitsPerComponent");
        let mut c = image_decode_bytes(w, h, bpc, 1, sm.filter_pipeline.len());
        // The stages in front hold the codec's bytes.
        if matches!(last_filter(&sm), DCT | JPX | JBIG2) {
            c = c.max(1 << 20);
        }
        (Some(sm), c)
    }

    /// `reserveCMYKDCT` guards the one DCT decode pdf-rs makes itself: a four-component stream
    /// is decoded inside `extract_image`, before any caller sees the bytes, so the SOF check
    /// byblos gives every other DCT decode happens here, on the encoded stream, first. A frame
    /// past the pixel cap is a [`Error::Limit`]; one the dictionary does not allow is
    /// [`Error::UnsupportedCodec`]. The memory reservation of Go is not made: pdf-rs charges
    /// the decode to the ledger.
    fn reserve_cmyk_dct(
        &mut self,
        cancel: &AtomicBool,
        sd: &StreamDict,
        info: &ImageInfo,
        declared: i64,
    ) -> Result<()> {
        let n = sd.filter_pipeline.len();
        if last_filter(sd) != DCT {
            return Ok(());
        }
        // `extract_image` reports a colour space error; and only four components are decoded
        // here.
        if !matches!(
            pdf::image::color_space_components(&mut self.ctx.xref, sd),
            Ok(4)
        ) {
            return Ok(());
        }
        let jpg: Option<Bytes> = if n > 1 {
            // The filters in front of DCT, run on a copy so `sd` is left as it was.
            let mut cp = sd.clone();
            cp.filter_pipeline.truncate(n - 1);
            cp.content = None;
            let mem = self.ctx.xref.mem().clone();
            if let Err(e) = cp.decode_with_limit(declared, &mem, cancel) {
                return Err(wrapped(
                    "rendering image",
                    format!("byblos/pdfdoc: image {}", info.obj_nr),
                    e,
                ));
            }
            cp.content.clone()
        } else {
            sd.raw.clone()
        };
        check_cancel(cancel)?;
        match dct::check_frame(
            jpg.as_deref().unwrap_or(&[]),
            self.lim.image_pixels(),
            info.width.wrapping_mul(info.height),
            4,
        ) {
            Ok(_) => Ok(()),
            Err(e @ Error::Limit { .. }) => Err(e),
            Err(_) => Err(Error::UnsupportedCodec),
        }
    }

    /// `RawImageGlobals` returns the JBIG2 page-0 segments image `id` declares in
    /// `/DecodeParms /JBIG2Globals`, decoded, or `None` when it declares none.
    ///
    /// IT IS SEPARATE FROM `raw_image` BECAUSE THE TWO ARE DIFFERENT STREAMS, and PDF gives them
    /// different lifetimes on purpose. The image stream is one page. The globals stream is
    /// shared: a bulk scanner writes one symbol dictionary for a whole document and every
    /// page's image dictionary points at it, so the same bytes come back for hundreds of ids.
    /// Returning them concatenated with the image would be wrong in both directions: it would
    /// multiply the dictionary by the page count, and it would put page-0 segments in front of
    /// a page-1 stream, which is not what ISO 32000-1:2008 7.4.7 describes.
    ///
    /// ABSENCE IS NOT AN ERROR and unreadability is. A generic-region JBIG2 image legitimately
    /// carries no globals (byblos's own encoder writes none), so `None` is the ordinary answer.
    /// A `/JBIG2Globals` entry that is present and does not resolve to a stream is the opposite
    /// case: those segments are needed, and decoding without them composes a text region against
    /// an empty symbol list and yields a blank page rather than an error.
    ///
    /// # Errors
    /// [`Error::Malformed`] for an id this document has not resolved or a `/JBIG2Globals` that
    /// is not a stream; a limit or allocation refusal; [`Error::Pdf`] for a dereference or
    /// decode error.
    pub fn raw_image_globals(&mut self, id: i64) -> Result<Option<Vec<u8>>> {
        // The one path in this package that dereferences an arbitrary indirect object named by
        // an image dictionary and then runs pdf-rs's filter chain over whatever it finds: a
        // corrupt /JBIG2Globals object must divert one page, not the run.
        let what = format!("reading /JBIG2Globals for image {id}");
        let Some(sd) = self.streams.get(&id).cloned() else {
            return Err(Error::Malformed(format!(
                "image {id} has not been resolved on this document"
            )));
        };
        let Some(parms) = self.decode_parms(&sd.dict) else {
            return Ok(None);
        };
        let Some(obj) = parms.find(b"JBIG2Globals") else {
            return Ok(None);
        };
        let mut gsd = match self.ctx.xref.dereference_stream_dict(obj) {
            Ok((Some(g), _)) => g,
            Ok(_) => {
                return Err(Error::Malformed(format!(
                    "image {id}: /JBIG2Globals does not resolve to a stream"
                )));
            }
            Err(e) => {
                return Err(wrapped(
                    &what,
                    format!("byblos/pdfdoc: image {id}: resolving /JBIG2Globals"),
                    e,
                ));
            }
        };
        if let Err(e) = self.decode(&mut gsd, false, &NEVER) {
            if is_limit(&e) {
                return Err(e);
            }
            return Err(match e {
                Error::Pdf(p) => Error::Pdf(
                    p.context(format!("byblos/pdfdoc: image {id}: decoding /JBIG2Globals")),
                ),
                other => other,
            });
        }
        let Some(c) = &gsd.content else {
            return Ok(None);
        };
        let held = self.ctx.xref.mem().charge(c.len() as u64)?;
        let mut v = Vec::new();
        v.try_reserve_exact(c.len())?;
        v.extend_from_slice(c);
        drop(held); // the globals leave the call
        Ok(Some(v))
    }

    /// `decodeParms` returns the `/DecodeParms` dictionary that belongs to the LAST entry of
    /// `dict`'s `/Filter`: the image codec, by the same argument `filter_name` makes for reading
    /// the last entry rather than the first.
    ///
    /// `/DecodeParms` is parallel to `/Filter` (ISO 32000-1:2008 7.4.1, table 5): a dictionary
    /// when `/Filter` is a name, an array of the same length when `/Filter` is an array, with a
    /// null in the slot of every filter that takes no parameters. A lone dictionary against a
    /// filter array is out of specification and files write it anyway; it is accepted here
    /// because there is only one filter in the chain that takes parameters and only one
    /// dictionary offered, so nothing is being guessed at.
    fn decode_parms(&mut self, dict: &Dict) -> Option<Dict> {
        let arr = match self.get_deref(dict, b"DecodeParms") {
            Object::Array(arr) => arr,
            Object::Dict(p) => return Some(p),
            _ => return None,
        };
        // Index by the filter chain, so a two-filter image takes the second dictionary rather
        // than the first.
        let mut i = arr.len() as i64 - 1;
        if let Object::Array(f) = self.get_deref(dict, b"Filter")
            && f.len() <= arr.len()
        {
            i = f.len() as i64 - 1;
        }
        let i = usize::try_from(i).ok()?;
        match self.deref(arr.get(i)?) {
            Object::Dict(p) => Some(p),
            _ => None,
        }
    }

    // --- dereferencing dictionary readers ---------------------------------------------------
    //
    // `Dict::int_entry`, `dict_entry` and `array_entry` return `None` when the entry is an
    // indirect reference instead of a direct object. An indirect /Resources on a Form XObject
    // is common in real documents and an indirect /Width is legal, and in both cases the
    // failure would be silent wrong data (an empty form scope, or Width 0 flowing into the page
    // info) rather than an error. These read through the reference.

    /// `deref`: the object `o` is or refers to, `Object::Null` when it does not resolve.
    pub(crate) fn deref(&mut self, o: &Object) -> Object {
        match self.ctx.xref.dereference(o) {
            Ok(v) => v.clone(),
            Err(e) => {
                self.note(e);
                Object::Null
            }
        }
    }

    /// `d.deref(dict[key])`: the dereferenced entry, `Object::Null` when absent.
    pub(crate) fn get_deref(&mut self, dict: &Dict, key: &[u8]) -> Object {
        match dict.0.get(key) {
            Some(o) => self.deref(o),
            None => Object::Null,
        }
    }

    /// `number`: the integer or real `o` is or refers to.
    pub(crate) fn number(&mut self, o: &Object) -> Option<f64> {
        match self.ctx.xref.dereference(o) {
            Ok(Object::Integer(i)) => Some(*i as f64),
            Ok(Object::Float(f)) => Some(*f),
            _ => None,
        }
    }

    /// `intEntry`: 0 unless the entry is an integer.
    pub(crate) fn int_entry(&mut self, dict: &Dict, key: &[u8]) -> i64 {
        match self.get_deref(dict, key) {
            Object::Integer(i) => i,
            _ => 0,
        }
    }

    /// `boolEntry`: false unless the entry is a boolean.
    pub(crate) fn bool_entry(&mut self, dict: &Dict, key: &[u8]) -> bool {
        matches!(self.get_deref(dict, key), Object::Bool(true))
    }

    /// `dictEntry`: `None` unless the entry is a dictionary (a stream does not count).
    pub(crate) fn dict_entry(&mut self, dict: &Dict, key: &[u8]) -> Option<Dict> {
        match self.get_deref(dict, key) {
            Object::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// `arrayEntry`: empty unless the entry is an array.
    pub(crate) fn array_entry(&mut self, dict: &Dict, key: &[u8]) -> Array {
        match self.get_deref(dict, key) {
            Object::Array(a) => a,
            _ => Array::new(),
        }
    }

    /// `filterName` reports the image codec `dict` declares; see [`ImageInfo::filter`] for why
    /// an array yields its last entry rather than its first.
    fn filter_name(&mut self, dict: &Dict) -> Vec<u8> {
        match self.get_deref(dict, b"Filter") {
            Object::Name(n) => n,
            Object::Array(v) => match v.last() {
                Some(l) => match self.deref(l) {
                    Object::Name(n) => n,
                    _ => Vec::new(),
                },
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// `numberArray` reads `key` as an array of numbers, and returns `None` unless every
    /// element is one. All-or-nothing on purpose: a partly-read array is a remap a caller
    /// cannot apply, and returning it with holes in it invites applying it anyway.
    ///
    /// An indirect array is converted once and every caller shares the result: the document
    /// parses the array once, but the image and font records outlive the walk, and K images
    /// naming one M-entry array would otherwise hold K*8*M bytes no account sees. A direct
    /// array belongs to one dictionary the parse already counted.
    pub(crate) fn number_array(&mut self, dict: &Dict, key: &[u8]) -> Option<Arc<[f64]>> {
        let arr = self.array_entry(dict, key);
        if arr.is_empty() {
            return None;
        }
        if let Some(Object::Ref(r)) = dict.0.get(key) {
            if let Some(out) = self.num_arrays.get(&r.num) {
                return out.clone();
            }
            let out = self.read_numbers(&arr);
            self.num_arrays.insert(r.num, out.clone());
            return out;
        }
        self.read_numbers(&arr)
    }

    fn read_numbers(&mut self, arr: &[Object]) -> Option<Arc<[f64]>> {
        let held = match self
            .ctx
            .xref
            .mem()
            .charge((arr.len() as u64).saturating_mul(8))
        {
            Ok(h) => h,
            Err(e) => {
                self.note(e);
                return None;
            }
        };
        let mut out: Vec<f64> = Vec::new();
        if out.try_reserve_exact(arr.len()).is_err() {
            return None;
        }
        for o in arr {
            out.push(self.number(o)?);
        }
        self.held.push(held);
        Some(Arc::from(out))
    }

    /// `cidWidths` flattens a descendant CIDFont's `/W` (ISO 32000-1 9.7.4.3) to CID to width.
    /// `/W` interleaves two forms in one array:
    ///
    /// ```text
    /// c [w1 w2 ... wn]   n consecutive CIDs starting at c
    /// cFirst cLast w     every CID in [cFirst, cLast] gets w
    /// ```
    ///
    /// A malformed head (a non-number where a CID is expected) stops the scan rather than
    /// misreading the rest of the array as something it is not.
    ///
    /// `budget`, not the map's length, is what bounds the work: both forms can target the same
    /// CIDs repeatedly (a repeated range, or a shared indirect sub-array referenced many
    /// times), and the map stops growing on a repeat while the loop keeps re-walking every CID
    /// it names. Gating on the length alone lets about 20 bytes of `/W` (a repeated
    /// `0 65534 500` triple, or one shared `9 0 R` sub-array referenced N times) burn
    /// O(N*65536) time for O(1) input growth. `budget` is decremented once per CID visited, by
    /// either form, so total work across the whole array is capped regardless of how many times
    /// a range repeats.
    fn cid_widths(&mut self, dict: &Dict) -> HashMap<u16, f64> {
        let mut out = HashMap::new();
        let arr = self.array_entry(dict, b"W");
        if arr.is_empty() {
            return out;
        }
        // `budget` bounds work, not entries covered, so it must accommodate both:
        // MAX_CID_WIDTH_ENTRIES for the CIDs a single range or sub-array can visit, plus the
        // array's length so that entries after a full-space range (budget otherwise drained to
        // 0) still get their turn to override it.
        let mut budget = MAX_CID_WIDTH_ENTRIES + arr.len() as i64;
        let mut i = 0usize;
        while i < arr.len() && budget > 0 {
            let Some(c) = arr.get(i).and_then(|o| self.number(o)) else {
                break;
            };
            if !(0.0..=65535.0).contains(&c) {
                break;
            }
            i += 1;
            let Some(next) = arr.get(i) else {
                break;
            };
            if let Object::Array(sub) = self.deref(next) {
                for (cid, wo) in (c as i64..).zip(sub.iter()) {
                    if cid > 65535 || budget <= 0 {
                        break;
                    }
                    if let Some(w) = self.number(wo) {
                        out.insert(cid as u16, w);
                    }
                    budget -= 1;
                }
                i += 1;
                continue;
            }
            let Some(mut last) = self.number(next) else {
                break;
            };
            i += 1;
            let Some(w) = arr.get(i).and_then(|o| self.number(o)) else {
                break;
            };
            i += 1;
            // Clamp in float space, before converting to an integer: `last` can be a float
            // wildly out of integer range (e.g. 1e300), and Go's float64 to int conversion of
            // an out-of-range value is implementation-defined (arm64 saturates, amd64 wraps to
            // a negative), so converting first and clamping after gave a different rendered
            // layout per architecture (byb-6z1 review finding 4).
            if last > 65535.0 {
                last = 65535.0;
            }
            let hi = last as i64;
            let mut cid = c as i64;
            while cid <= hi && budget > 0 {
                out.insert(cid as u16, w);
                budget -= 1;
                cid += 1;
            }
        }
        out
    }
}

/// A pdf-rs error as the error of a read that has no message of its own: [`seam`] first.
pub(crate) fn conv(what: &str, e: pdf::Error) -> Error {
    match seam(what, e) {
        Ok(p) => Error::Pdf(p),
        Err(o) => o,
    }
}

/// `content.Env` over a [`Doc`]. The walk calls the environment through `&self`, and Go's
/// lookups change the document (a scope per form, the image records), so the document sits in
/// a `RefCell`. Use [`DocEnv::with`] between the walk's calls, for example to read
/// [`Doc::image_info`] or [`Doc::err`] after it; a call made from inside a lookup is a misuse
/// and answers "no" instead of panicking.
pub struct DocEnv<'a> {
    doc: RefCell<&'a mut Doc>,
}

impl<'a> DocEnv<'a> {
    /// A `DocEnv` over `doc`.
    pub fn new(doc: &'a mut Doc) -> DocEnv<'a> {
        DocEnv {
            doc: RefCell::new(doc),
        }
    }

    /// Runs `f` on the document. It must not run a walk over this `DocEnv`.
    pub fn with<R>(&self, f: impl FnOnce(&mut Doc) -> R) -> R {
        let mut guard = self.doc.borrow_mut();
        let d: &mut Doc = &mut guard;
        f(d)
    }

    /// The document back, once the walk is done.
    pub fn into_doc(self) -> &'a mut Doc {
        self.doc.into_inner()
    }
}

impl Env for DocEnv<'_> {
    fn xobject(&self, scope: i64, name: &[u8]) -> Option<XObject> {
        self.doc.try_borrow_mut().ok()?.xobject(scope, name)
    }

    fn ext_gstate_opaque(&self, scope: i64, name: &[u8]) -> bool {
        match self.doc.try_borrow_mut() {
            Ok(mut d) => d.ext_gstate_opaque(scope, name),
            Err(_) => false,
        }
    }

    fn font(&self, scope: i64, name: &[u8]) -> Option<i64> {
        self.doc.try_borrow_mut().ok()?.font(scope, name)
    }
}
