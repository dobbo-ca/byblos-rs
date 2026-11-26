//! Port of byblos `provenance.go`, the types and the JSON half: [`Provenance`],
//! [`PageProvenance`], [`PageGeometry`] (with `CoversPage`) and [`PageStraighten`], their
//! `json.Marshal` and `json.Unmarshal` through [`crate::go::json`], and the capability list.
//!
//! The PDF half is [`write_provenance`], [`read_provenance`], [`record_extraction`] and
//! `unionSorted` ([`union_sorted`]); it calls [`Provenance::to_json`] and
//! [`Provenance::from_json`].
//!
//! Go's nil slices and pointers keep their meaning: `capabilities` and `pages` have no
//! `omitempty`, so a nil slice marshals as `null` and an empty one as `[]`. They are
//! `Option<Vec<_>>` for that reason. The fields with `omitempty` treat nil and empty alike and
//! are plain `Vec`s. `Geometry`, `ClipBox`, `RasterQuad` and `Straightened` are `Option` because
//! "never measured" must stay distinguishable from a measured zero.

use crate::content::{BBox, Quad};
use crate::error::{Error, Result};
use crate::go::json::{self, Time, Value, Writer, field_values};
use crate::limits::{Limits, check_cancel};
use crate::linearize::wrap;
use crate::pdfdoc::{self, Doc};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// The Byblos semver recorded in every [`Provenance`] (`Version`). It exists for humans and bug
/// reports; upgrade decisions are driven by capabilities, not by comparing versions.
pub const VERSION: &str = "0.2.0";

/// The Info-dictionary key a [`Provenance`] record is stored under, as JSON (`provenanceKey`).
pub const PROVENANCE_KEY: &str = "byblos-provenance";

/// How far a placement may fall short of the page box and still count as covering it, in points
/// (`coverTolerancePt` of `extract.go`).
pub const COVER_TOLERANCE_PT: f64 = 1.0;

/// What this build can do (`buildCapabilities`). Every entry must also have a rule in
/// `capabilityRules` (`upgrade.go`). Append, never remove: a capability string is a permanent
/// identifier that older documents' provenance refers to. Kept sorted.
pub const BUILD_CAPABILITIES: &[&str] = &[
    "build-pdf",
    "downsample",
    "extract-raster",
    "inspect",
    "jbig2-generic",
    "jpeg-recompress",
    "linearize",
    "quantize-png",
    "text-layer",
];

/// What this build can do, sorted (`Capabilities`).
pub fn capabilities() -> &'static [&'static str] {
    BUILD_CAPABILITIES
}

/// The record Byblos writes into a processed PDF, as JSON under [`PROVENANCE_KEY`]. The PDF is
/// authoritative; any mirror of these fields in a caller's database is a cache.
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    /// JSON `version`.
    pub version: String,
    /// JSON `capabilities`; `None` is Go's nil slice and marshals as `null`.
    pub capabilities: Option<Vec<String>>,
    /// JSON `processed_at`, an RFC 3339 time. [`Time::ZERO`] is Go's zero `time.Time`.
    pub processed_at: Time,
    /// JSON `pages`; `None` is Go's nil slice and marshals as `null`.
    pub pages: Option<Vec<PageProvenance>>,
    /// JSON `optimized` (omitted when empty): which branch `Optimize` took, a whole-document
    /// property. `""` means "not known to have been rewritten by Optimize", never "confirmed
    /// pass-through". `"rewritten"` means pdfcpu's optimized bytes were kept and the input was
    /// not linearized. `"rewritten-delinearized"` means the input carried a linearization
    /// dictionary and the output does not, the one case where the rewrite traded a real
    /// property for bytes. `"rewritten-linearized"` means Optimize was asked to linearize and
    /// did; it is the only value that asserts a positive property of the output, which is why
    /// the upgrade rules key on it. `"passed-through"` is reserved and not yet emitted. A
    /// pass-through run carries the input's record forward untouched, so a stale `"rewritten"`
    /// describes the most recent rewrite, not the most recent call.
    pub optimized: String,
}

impl Default for Provenance {
    /// Go's zero `Provenance`.
    fn default() -> Self {
        Provenance {
            version: String::new(),
            capabilities: None,
            processed_at: Time::ZERO,
            pages: None,
            optimized: String::new(),
        }
    }
}

/// What one page actually received.
///
/// `applied` entries are capability names, optionally with a numeric parameter as a `-N`
/// suffix, such as `downsample-150`. `diverted` is the reason a page was not processed, such as
/// `not-single-raster`, and is empty when the page was handled normally; it is coarser than the
/// classify vocabulary of `extract.go` (the codec case is split into `unsupported-codec-jbig2`,
/// `-jpx` and `-tiff`). A document written before that split carries the older
/// `unsupported-codec`, which must keep matching every decode rule indefinitely.
///
/// `placement`, when present, is the affine the page's raster was painted with, in PDF matrix
/// order `[a b c d e f]`. It is recorded, not applied: a scanner's deskew rotation stays in the
/// placement and the raster is kept as stored. It is empty for an axis-aligned placement.
///
/// `dropped_annots` is how many annotations painted on the page and are not in the stored
/// raster; zero is omitted.
///
/// `geometry` is the raster and page boxes measured at write time. It is `None` for every
/// record this build did not measure, and that must never be read as "the raster covers the
/// page". `straightened` is the absolute rotation byblos applied to the page's content; it is
/// replaced, never unioned into `applied`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageProvenance {
    /// JSON `applied` (omitted when empty).
    pub applied: Vec<String>,
    /// JSON `diverted` (omitted when empty).
    pub diverted: String,
    /// JSON `placement` (omitted when empty).
    pub placement: Vec<f64>,
    /// JSON `dropped_annots` (omitted when zero).
    pub dropped_annots: i64,
    /// JSON `geometry` (omitted when `None`).
    pub geometry: Option<PageGeometry>,
    /// JSON `straightened` (omitted when `None`).
    pub straightened: Option<PageStraighten>,
}

/// A page's raster and page boxes as measured at write time, each `[llx lly urx ury]` in PDF
/// default user space: points, origin at the lower-left corner, y increasing upward. This is
/// not the `[a b c d e f]` order of [`PageProvenance::placement`].
///
/// A record carrying this value is a measurement, even an all-zero one; `None` in
/// [`PageProvenance::geometry`] means no measurement. A short JSON array for any of the boxes
/// zero-fills the missing elements instead of failing, so `raster_box` and `page_box` must both
/// be written together. `clip_box` is present only when a clip narrowed the placement below its
/// unclipped raster box. `raster_quad` is the placement's true quadrilateral in ring order
/// (0,0) (1,0) (1,1) (0,1), present only for a rotated or sheared placement.
///
/// [`PageGeometry::covers_page`] applies [`COVER_TOLERANCE_PT`] over these exact floats. The
/// live `PageRaster.CoversPage` applies none, over rounded integers, so the two can disagree
/// for a shortfall in the (0.5, 1.0] pt band. That fork is deliberate.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageGeometry {
    /// JSON `raster_box`.
    pub raster_box: [f64; 4],
    /// JSON `page_box`.
    pub page_box: [f64; 4],
    /// JSON `clip_box` (omitted when `None`).
    pub clip_box: Option<[f64; 4]>,
    /// JSON `raster_quad` (omitted when `None`).
    pub raster_quad: Option<[f64; 8]>,
}

impl PageGeometry {
    /// `CoversPage`: whether `raster_box` fills `page_box` within [`COVER_TOLERANCE_PT`] and,
    /// when `raster_quad` is present, whether the true quadrilateral does too.
    ///
    /// A `page_box` with zero or negative width or height, corners swapped included, is covered
    /// by nothing.
    pub fn covers_page(&self) -> bool {
        let (r, p) = (&self.raster_box, &self.page_box);
        if p[2] <= p[0] || p[3] <= p[1] {
            return false;
        }
        // Not `!(a <= b)` forms: a NaN anywhere must answer false, as Go's negated chain does.
        let covers = r[0] <= p[0] + COVER_TOLERANCE_PT
            && r[1] <= p[1] + COVER_TOLERANCE_PT
            && r[2] >= p[2] - COVER_TOLERANCE_PT
            && r[3] >= p[3] - COVER_TOLERANCE_PT;
        if !covers {
            return false;
        }
        match &self.raster_quad {
            None => true,
            Some(q) => {
                let page_box = BBox {
                    llx: p[0],
                    lly: p[1],
                    urx: p[2],
                    ury: p[3],
                };
                Quad(*q).contains_box(&page_box, COVER_TOLERANCE_PT)
            }
        }
    }
}

/// The correction byblos applied to a page's content: degrees, positive counter-clockwise in PDF
/// default user space. It is absolute and replaced, never accumulated, and it is an `Option` in
/// [`PageProvenance`] so that a real 0.0 degree correction differs from "never straightened".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PageStraighten {
    /// JSON `deg`.
    pub deg: f64,
}

impl Provenance {
    /// `json.Marshal(p)`: the bytes `WriteProvenance` stores under [`PROVENANCE_KEY`].
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] (class `error`, like Go's wrapped marshal error) for a value JSON
    /// cannot carry (NaN or an infinity, a time outside year 0 to 9999); [`Error::Alloc`] or
    /// [`Error::OutOfMemory`] when the output does not fit `limits`.
    pub fn to_json(&self, limits: &Limits) -> Result<Vec<u8>> {
        let mut w = Writer::new(limits);
        self.write(&mut w).map_err(marshal_error)?;
        Ok(w.finish())
    }

    /// `json.Unmarshal(data, &p)`: the record stored under [`PROVENANCE_KEY`]. Unknown keys are
    /// ignored and field names match case-insensitively, as in Go. A JSON `null` gives the zero
    /// record.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptProvenance`] for anything Go's `Unmarshal` rejects (bad syntax, a wrongly
    /// typed value, a bad time); [`Error::Alloc`] or [`Error::OutOfMemory`] when the decoded tree
    /// does not fit `limits`.
    pub fn from_json(data: &[u8], limits: &Limits) -> Result<Provenance> {
        let value = json::parse(data, limits).map_err(corrupt_error)?;
        let mut p = Provenance::default();
        // The tree `parse` already charged, and what is left of the limit for the records.
        let max = limits.alloc_limit();
        let tree = (data.len() as u64).saturating_mul(
            (std::mem::size_of::<(String, Value)>() + std::mem::size_of::<usize>()) as u64,
        );
        p.apply(&value, Budget { tree, max })
            .map_err(corrupt_error)?;
        Ok(p)
    }

    fn write(&self, w: &mut Writer) -> std::result::Result<(), json::Error> {
        w.begin_object()?;
        w.key("version")?;
        w.string(&self.version)?;
        w.key("capabilities")?;
        match &self.capabilities {
            None => w.null()?,
            Some(c) => write_strings(w, c)?,
        }
        w.key("processed_at")?;
        w.time(&self.processed_at)?;
        w.key("pages")?;
        match &self.pages {
            None => w.null()?,
            Some(pages) => {
                w.begin_array()?;
                for p in pages {
                    w.item()?;
                    p.write(w)?;
                }
                w.end_array()?;
            }
        }
        if !self.optimized.is_empty() {
            w.key("optimized")?;
            w.string(&self.optimized)?;
        }
        w.end_object()
    }

    fn apply(&mut self, v: &Value, budget: Budget) -> std::result::Result<(), json::Error> {
        let Some(fields) = v.as_object()? else {
            return Ok(());
        };
        for v in field_values(fields, "version") {
            if let Some(s) = v.as_str()? {
                self.version = try_string(s)?;
            }
        }
        for v in field_values(fields, "capabilities") {
            self.capabilities = merge_slice(self.capabilities.take(), v, set_string)?;
        }
        for v in field_values(fields, "processed_at") {
            if let Some(t) = v.as_time()? {
                self.processed_at = t;
            }
        }
        for v in field_values(fields, "pages") {
            if let Value::Array(items) = v {
                let bytes = (items.len() as u64)
                    .saturating_mul(std::mem::size_of::<PageProvenance>() as u64);
                let got = budget.tree.saturating_add(bytes);
                if got > budget.max {
                    return Err(json::Error::Alloc {
                        got,
                        max: budget.max,
                    });
                }
            }
            self.pages = merge_slice(self.pages.take(), v, |e, p: &mut PageProvenance| p.apply(e))?;
        }
        for v in field_values(fields, "optimized") {
            if let Some(s) = v.as_str()? {
                self.optimized = try_string(s)?;
            }
        }
        Ok(())
    }
}

impl PageProvenance {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), json::Error> {
        w.begin_object()?;
        if !self.applied.is_empty() {
            w.key("applied")?;
            write_strings(w, &self.applied)?;
        }
        if !self.diverted.is_empty() {
            w.key("diverted")?;
            w.string(&self.diverted)?;
        }
        if !self.placement.is_empty() {
            w.key("placement")?;
            write_floats(w, &self.placement)?;
        }
        if self.dropped_annots != 0 {
            w.key("dropped_annots")?;
            w.i64(self.dropped_annots)?;
        }
        if let Some(g) = &self.geometry {
            w.key("geometry")?;
            g.write(w)?;
        }
        if let Some(s) = &self.straightened {
            w.key("straightened")?;
            w.begin_object()?;
            w.key("deg")?;
            w.f64(s.deg)?;
            w.end_object()?;
        }
        w.end_object()
    }

    fn apply(&mut self, v: &Value) -> std::result::Result<(), json::Error> {
        let Some(fields) = v.as_object()? else {
            return Ok(());
        };
        for v in field_values(fields, "applied") {
            self.applied = merge_slice(Some(std::mem::take(&mut self.applied)), v, set_string)?
                .unwrap_or_default();
        }
        for v in field_values(fields, "diverted") {
            if let Some(s) = v.as_str()? {
                self.diverted = try_string(s)?;
            }
        }
        for v in field_values(fields, "placement") {
            self.placement = merge_slice(
                Some(std::mem::take(&mut self.placement)),
                v,
                |e, f: &mut f64| {
                    if let Some(x) = e.as_f64()? {
                        *f = x;
                    }
                    Ok(())
                },
            )?
            .unwrap_or_default();
        }
        for v in field_values(fields, "dropped_annots") {
            if let Some(n) = v.as_i64()? {
                self.dropped_annots = n;
            }
        }
        for v in field_values(fields, "geometry") {
            if matches!(v, Value::Null) {
                self.geometry = None;
            } else {
                self.geometry
                    .get_or_insert_with(Default::default)
                    .apply(v)?;
            }
        }
        for v in field_values(fields, "straightened") {
            if matches!(v, Value::Null) {
                self.straightened = None;
            } else {
                let s = self.straightened.get_or_insert_with(Default::default);
                if let Some(members) = v.as_object()? {
                    for d in field_values(members, "deg") {
                        if let Some(f) = d.as_f64()? {
                            s.deg = f;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl PageGeometry {
    fn write(&self, w: &mut Writer) -> std::result::Result<(), json::Error> {
        w.begin_object()?;
        w.key("raster_box")?;
        write_floats(w, &self.raster_box)?;
        w.key("page_box")?;
        write_floats(w, &self.page_box)?;
        if let Some(c) = &self.clip_box {
            w.key("clip_box")?;
            write_floats(w, c)?;
        }
        if let Some(q) = &self.raster_quad {
            w.key("raster_quad")?;
            write_floats(w, q)?;
        }
        w.end_object()
    }

    fn apply(&mut self, v: &Value) -> std::result::Result<(), json::Error> {
        let Some(fields) = v.as_object()? else {
            return Ok(());
        };
        for v in field_values(fields, "raster_box") {
            if let Some(a) = v.to_array::<f64, 4>(Value::as_f64)? {
                self.raster_box = a;
            }
        }
        for v in field_values(fields, "page_box") {
            if let Some(a) = v.to_array::<f64, 4>(Value::as_f64)? {
                self.page_box = a;
            }
        }
        for v in field_values(fields, "clip_box") {
            self.clip_box = v.to_array::<f64, 4>(Value::as_f64)?;
        }
        for v in field_values(fields, "raster_quad") {
            self.raster_quad = v.to_array::<f64, 8>(Value::as_f64)?;
        }
        Ok(())
    }
}

fn write_strings(w: &mut Writer, items: &[String]) -> std::result::Result<(), json::Error> {
    w.begin_array()?;
    for s in items {
        w.item()?;
        w.string(s)?;
    }
    w.end_array()
}

fn write_floats(w: &mut Writer, items: &[f64]) -> std::result::Result<(), json::Error> {
    w.begin_array()?;
    for f in items {
        w.item()?;
        w.f64(*f)?;
    }
    w.end_array()
}

/// What `Provenance::apply` may spend: the tree `parse` charged plus the record vectors, against
/// the allocation limit.
#[derive(Clone, Copy)]
struct Budget {
    tree: u64,
    max: u64,
}

/// A Go slice decode with `MergeWithLegacySemantics`: element `i` is decoded into the old
/// element `i` when the old slice reaches that far (a `null` element keeps it), else into a zero
/// value. `null` gives `None`.
///
/// Go also reuses old elements past the old length that are still in capacity; that tail is not
/// kept, so a duplicate key after a longer array can differ from Go.
fn merge_slice<T: Default>(
    old: Option<Vec<T>>,
    v: &Value,
    mut f: impl FnMut(&Value, &mut T) -> std::result::Result<(), json::Error>,
) -> std::result::Result<Option<Vec<T>>, json::Error> {
    let items = match v {
        Value::Array(items) => items,
        Value::Null => return Ok(None),
        _ => return Err(json::Error::Type("Go value of type slice".to_string())),
    };
    let mut old = old.unwrap_or_default().into_iter();
    let mut out = Vec::new();
    out.try_reserve_exact(items.len())?;
    for item in items {
        let mut e = old.next().unwrap_or_default();
        f(item, &mut e)?;
        out.push(e);
    }
    Ok(Some(out))
}

fn set_string(e: &Value, s: &mut String) -> std::result::Result<(), json::Error> {
    if let Some(x) = e.as_str()? {
        *s = try_string(x)?;
    }
    Ok(())
}

fn try_string(s: &str) -> std::result::Result<String, json::Error> {
    let mut out = String::new();
    out.try_reserve_exact(s.len())?;
    out.push_str(s);
    Ok(out)
}

/// A marshal failure: resource failures keep their class, the rest is Go's wrapped
/// `byblos: marshal provenance` error.
fn marshal_error(e: json::Error) -> Error {
    match e {
        json::Error::Alloc { got, max } => Error::Alloc { got, max },
        json::Error::OutOfMemory => Error::OutOfMemory,
        e => Error::Malformed(format!("byblos: marshal provenance: {e}")),
    }
}

/// An unmarshal failure: resource failures keep their class, the rest is `errCorruptProvenance`.
fn corrupt_error(e: json::Error) -> Error {
    match e {
        json::Error::Alloc { got, max } => Error::Alloc { got, max },
        json::Error::OutOfMemory => Error::OutOfMemory,
        _ => Error::CorruptProvenance,
    }
}

/// `time.Now()`, or the pinned instant `now` of a call that takes a clock (the parity oracle
/// passes 2000-01-01T00:00:00Z). The system clock is read in UTC with its nanoseconds; Go's
/// `time.Now()` is in the local zone, which is UTC in a container.
pub(crate) fn clock(now: Option<pdf::GoTime>) -> Time {
    match now {
        Some(t) => Time::from_go_time(t),
        None => {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Time {
                unix: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                nanos: d.subsec_nanos(),
                offset_secs: 0,
            }
        }
    }
}

/// The caps of Go's `ReadProvenance` and `WriteProvenance` when a byblos operation calls them
/// internally: the package defaults (Go takes no `Limits` there) with only the per-call budget
/// of `lim`.
pub(crate) fn default_caps(lim: &Limits) -> Limits {
    Limits {
        max_alloc: lim.max_alloc,
        max_threads: lim.max_threads,
        ..Limits::default()
    }
}

/// `unionSorted`: the sorted, deduplicated union of `a` and `b`.
pub fn union_sorted(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    out.extend_from_slice(b);
    out.sort();
    out.dedup();
    out
}

/// `WriteProvenance`: marshals `p` to JSON and stores it under [`PROVENANCE_KEY`] in the Info
/// dictionary of `pdf` (pdf-rs's `properties_add` through [`pdfdoc::write_properties`]), and
/// returns the new document. It needs only an Info dictionary to exist, not an extraction
/// outcome or a text layer.
///
/// The write is one indivisible read, validate, optimize and write pass of pdf-rs; `cancel` is
/// checked before it and by pdf-rs inside it.
///
/// # Errors
/// [`Error::Malformed`] when `p` cannot be marshalled (NaN, an infinity, a time outside year 0
/// to 9999); the errors of [`pdfdoc::write_properties`]; [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn write_provenance(
    pdf: &[u8],
    p: &Provenance,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    write_provenance_at(pdf, p, None, limits, cancel)
}

/// [`write_provenance`] with the fallback writer's clock pinned to `now`; `None` is the real
/// clock and random source.
///
/// # Errors
/// As [`write_provenance`].
pub fn write_provenance_at(
    pdf: &[u8],
    p: &Provenance,
    now: Option<pdf::GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let data = p.to_json(limits)?;
    let json = String::from_utf8(data)
        .map_err(|e| Error::Malformed(format!("byblos: marshal provenance: {e}")))?;
    let props = BTreeMap::from([(PROVENANCE_KEY.to_string(), json)]);
    pdfdoc::write_properties_at(pdf, &props, now, limits, cancel)
        .map_err(|e| wrap(e, "byblos: write provenance"))
}

/// `ReadProvenance`: reads back the record [`write_provenance`] stored under
/// [`PROVENANCE_KEY`]. `Ok(None)` is a document no byblos build has processed: absence is not an
/// error.
///
/// # Errors
/// [`Error::CorruptProvenance`] for a value that is not a valid record (Go's
/// `errCorruptProvenance`); the errors of [`pdfdoc::read_properties`]; [`Error::Alloc`],
/// [`Error::OutOfMemory`], [`Error::Cancelled`].
pub fn read_provenance(
    pdf: &[u8],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Option<Provenance>> {
    read_provenance_with_mem(pdf, limits, &limits.mem(), cancel)
}

/// [`read_provenance`] on the caller's ledger `mem`.
///
/// # Errors
/// As [`read_provenance`].
pub(crate) fn read_provenance_with_mem(
    pdf: &[u8],
    limits: &Limits,
    mem: &pdf::Mem,
    cancel: &AtomicBool,
) -> Result<Option<Provenance>> {
    check_cancel(cancel)?;
    let props = pdfdoc::properties::read_properties_with_mem(pdf, limits, mem, cancel)
        .map_err(|e| wrap(e, "byblos: read provenance"))?;
    match props.get(PROVENANCE_KEY) {
        None => Ok(None),
        Some(raw) => Provenance::from_json(raw.as_bytes(), limits).map(Some),
    }
}

/// One page's extraction, for [`record_extraction`]: the record and the error of
/// `extractPage`, which come together (a diverted page has both).
fn page_record(d: &mut Doc, n: usize, cancel: &AtomicBool) -> (PageProvenance, Option<Error>) {
    let (res, rec) = crate::extract::extract_page(d, n, cancel);
    (rec, res.err())
}

/// `RecordExtraction`: runs extraction over every page of `pdf` and returns the record of what
/// happened, ready for [`write_provenance`]. `pages` has exactly one entry per page, in page
/// order. `now` is the clock for `processed_at` (`None`: the system clock).
///
/// It is the only producer of [`PageGeometry`]. A page that cannot be READ at all aborts the
/// whole call. `capabilities` claims only `extract-raster`; a record the document already
/// carries is merged, not overwritten: `optimized` is kept, the capabilities and each page's
/// `applied` are unions, and `straightened` is carried forward. A record that is not valid JSON
/// counts as no record.
///
/// `cancel` is checked per page; the initial read and open are single uninterruptible passes of
/// pdf-rs apart from its own checks.
///
/// # Errors
/// The errors of [`read_provenance`] (except [`Error::CorruptProvenance`]) and of
/// [`Doc::open_limits`]; for a page that fails without being diverted, its error with the
/// context `byblos: provenance: page N`; [`Error::Cancelled`], [`Error::Alloc`].
pub fn record_extraction(
    pdf: &[u8],
    now: Option<pdf::GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Provenance> {
    check_cancel(cancel)?;
    let old = match read_provenance(pdf, &default_caps(limits), cancel) {
        Ok(p) => p,
        Err(Error::CorruptProvenance) => None,
        Err(e) => return Err(e),
    };
    let mut d = Doc::open_limits(pdf, limits, cancel)?;
    let count = d.page_count();
    let mut capabilities = vec!["extract-raster".to_string()];
    let mut optimized = String::new();
    if let Some(o) = &old {
        optimized.clone_from(&o.optimized);
        capabilities = union_sorted(o.capabilities.as_deref().unwrap_or_default(), &capabilities);
    }
    let mut pages = Vec::new();
    pages.try_reserve_exact(count)?;
    for n in 1..=count {
        check_cancel(cancel)?;
        let (mut rec, err) = page_record(&mut d, n, cancel);
        if let Some(e) = err
            && rec.diverted.is_empty()
        {
            return Err(wrap(e, &format!("byblos: provenance: page {n}")));
        }
        if let Some(op) = old
            .as_ref()
            .and_then(|o| o.pages.as_ref())
            .and_then(|v| v.get(n - 1))
        {
            if rec.diverted.is_empty() {
                rec.applied = union_sorted(&op.applied, &rec.applied);
            }
            // Carried forward even for a diverted page: it is not derivable from the document,
            // and extraction zeroes the other fields of a page that diverts.
            rec.straightened = op.straightened;
        }
        pages.push(rec);
    }
    Ok(Provenance {
        version: VERSION.to_string(),
        capabilities: Some(capabilities),
        processed_at: clock(now),
        pages: Some(pages),
        optimized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits::default()
    }

    fn time(s: &str) -> Time {
        Time::parse_rfc3339(s.as_bytes()).expect("time")
    }

    fn marshal(f: impl FnOnce(&mut Writer) -> std::result::Result<(), json::Error>) -> String {
        let mut w = Writer::new(&lim());
        f(&mut w).expect("marshal");
        String::from_utf8(w.finish()).expect("utf8")
    }

    fn marshal_page(p: &PageProvenance) -> String {
        marshal(|w| p.write(w))
    }

    fn marshal_geometry(g: &PageGeometry) -> String {
        marshal(|w| g.write(w))
    }

    fn unmarshal_page(s: &str) -> PageProvenance {
        let v = json::parse(s.as_bytes(), &lim()).expect("parse");
        let mut p = PageProvenance::default();
        p.apply(&v).expect("apply");
        p
    }

    fn unmarshal_geometry(s: &str) -> PageGeometry {
        let v = json::parse(s.as_bytes(), &lim()).expect("parse");
        let mut g = PageGeometry::default();
        g.apply(&v).expect("apply");
        g
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn capabilities_is_sorted_and_not_empty() {
        let got = capabilities();
        assert!(!got.is_empty());
        assert!(got.is_sorted(), "{got:?}");
    }

    #[test]
    fn capabilities_contains_b1_set() {
        for want in ["extract-raster", "inspect"] {
            assert!(capabilities().contains(&want), "missing {want}");
        }
    }

    #[test]
    fn capabilities_contains_b3_set() {
        for want in ["quantize-png", "downsample", "jpeg-recompress"] {
            assert!(capabilities().contains(&want), "missing {want}");
        }
    }

    #[test]
    fn provenance_json_round_trip() {
        let input = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(capabilities().iter().map(|s| (*s).to_string()).collect()),
            processed_at: time("2026-07-27T12:00:00Z"),
            pages: Some(vec![
                PageProvenance {
                    applied: strings(&["jbig2-generic", "downsample-150"]),
                    ..Default::default()
                },
                PageProvenance {
                    diverted: "not-single-raster".to_string(),
                    ..Default::default()
                },
            ]),
            optimized: String::new(),
        };
        let raw = input.to_json(&lim()).expect("marshal");
        let out = Provenance::from_json(&raw, &lim()).expect("unmarshal");
        assert_eq!(out, input);
        assert_eq!(out.pages.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            out.pages
                .as_ref()
                .and_then(|p| p.get(1))
                .map(|p| p.diverted.as_str()),
            Some("not-single-raster")
        );
    }

    #[test]
    fn provenance_json_bytes() {
        let p = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(strings(&["a"])),
            processed_at: time("2026-07-27T12:00:00Z"),
            pages: Some(vec![PageProvenance::default()]),
            optimized: "rewritten".to_string(),
        };
        assert_eq!(
            String::from_utf8(p.to_json(&lim()).expect("marshal")).expect("utf8"),
            r#"{"version":"0.2.0","capabilities":["a"],"processed_at":"2026-07-27T12:00:00Z","pages":[{}],"optimized":"rewritten"}"#
        );
        // A nil slice is null, an empty one is [], and the zero time is year 1.
        let zero = Provenance::default();
        assert_eq!(
            String::from_utf8(zero.to_json(&lim()).expect("marshal")).expect("utf8"),
            r#"{"version":"","capabilities":null,"processed_at":"0001-01-01T00:00:00Z","pages":null}"#
        );
    }

    #[test]
    fn corrupt_value_is_errcorrupt() {
        for bad in [
            &b"{"[..],
            &b"[]"[..],
            &br#"{"version":1}"#[..],
            &br#"{"processed_at":"x"}"#[..],
        ] {
            assert_eq!(
                Provenance::from_json(bad, &lim()),
                Err(Error::CorruptProvenance),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(
            Provenance::from_json(b"null", &lim()),
            Ok(Provenance::default())
        );
    }

    // The deskew placement of the corpus (internal/corpus DeskewPlacement).
    #[test]
    fn page_provenance_carries_the_placement() {
        let m = [617.99841, -1.40220, 1.81061, 797.99795, -3.9, -3.0];
        let input = PageProvenance {
            applied: strings(&["extract-raster"]),
            placement: m.to_vec(),
            ..Default::default()
        };
        let raw = marshal_page(&input);
        assert_eq!(unmarshal_page(&raw).placement, input.placement, "{raw}");
    }

    #[test]
    fn page_provenance_omits_empty_fields() {
        assert_eq!(marshal_page(&PageProvenance::default()), "{}");
    }

    #[test]
    fn page_provenance_decodes_legacy_record_without_geometry() {
        let out = unmarshal_page(
            r#"{"applied":["extract-raster"],"placement":[1,0,0,1,0,0],"dropped_annots":2}"#,
        );
        assert_eq!(out.geometry, None);
        assert_eq!(out.placement, vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(out.dropped_annots, 2);
    }

    #[test]
    fn page_provenance_geometry_zero_value_is_not_omitted() {
        let raw = marshal_page(&PageProvenance {
            geometry: Some(PageGeometry::default()),
            ..Default::default()
        });
        assert!(raw.contains(r#""geometry""#), "{raw}");
        assert!(unmarshal_page(&raw).geometry.is_some());
    }

    #[test]
    fn page_provenance_decodes_geometry_without_clip_box() {
        let out = unmarshal_page(
            r#"{"applied":["extract-raster"],"geometry":{"raster_box":[0,0,612,792],"page_box":[0,0,612,792]}}"#,
        );
        let g = out.geometry.expect("geometry");
        assert_eq!(g.clip_box, None);
        assert_eq!(g.raster_box, [0.0, 0.0, 612.0, 792.0]);
    }

    #[test]
    fn page_provenance_geometry_clip_box_round_trips() {
        let input = PageProvenance {
            geometry: Some(PageGeometry {
                raster_box: [0.0, 0.0, 612.0, 792.0],
                page_box: [0.0, 0.0, 612.0, 792.0],
                clip_box: Some([0.0, 0.0, 100.0, 100.0]),
                raster_quad: None,
            }),
            ..Default::default()
        };
        let out = unmarshal_page(&marshal_page(&input));
        assert_eq!(
            out.geometry.and_then(|g| g.clip_box),
            Some([0.0, 0.0, 100.0, 100.0])
        );
    }

    #[test]
    fn page_provenance_geometry_clip_box_wire_key_is_clip_box() {
        let g = PageGeometry {
            clip_box: Some([1.0, 2.0, 3.0, 4.0]),
            ..Default::default()
        };
        let raw = marshal_geometry(&g);
        assert!(raw.contains(r#""clip_box":[1,2,3,4]"#), "{raw}");
        assert_eq!(
            unmarshal_geometry(&raw).clip_box,
            Some([1.0, 2.0, 3.0, 4.0])
        );
    }

    #[test]
    fn page_provenance_geometry_omits_clip_box_when_unmeasured() {
        let raw = marshal_geometry(&PageGeometry {
            raster_box: [0.0, 0.0, 612.0, 792.0],
            page_box: [0.0, 0.0, 612.0, 792.0],
            ..Default::default()
        });
        assert!(!raw.contains(r#""clip_box""#), "{raw}");
    }

    #[test]
    fn page_provenance_geometry_clip_box_zero_value_is_not_omitted() {
        let raw = marshal_geometry(&PageGeometry {
            clip_box: Some([0.0; 4]),
            ..Default::default()
        });
        assert!(raw.contains(r#""clip_box""#), "{raw}");
        assert_eq!(unmarshal_geometry(&raw).clip_box, Some([0.0; 4]));
    }

    #[test]
    fn page_provenance_decodes_geometry_without_raster_quad() {
        let out = unmarshal_page(
            r#"{"applied":["extract-raster"],"geometry":{"raster_box":[0,0,612,792],"page_box":[0,0,612,792]}}"#,
        );
        let g = out.geometry.expect("geometry");
        assert_eq!(g.raster_quad, None);
        assert!(g.covers_page());
    }

    #[test]
    fn page_provenance_geometry_short_raster_quad_zero_fills_and_covers_page_is_false() {
        let g = unmarshal_geometry(
            r#"{"raster_box":[0,0,612,792],"page_box":[0,0,612,792],"raster_quad":[1,2,3]}"#,
        );
        assert_eq!(
            g.raster_quad,
            Some([1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert!(!g.covers_page());
    }

    #[test]
    fn page_provenance_geometry_raster_quad_round_trips() {
        let input = PageProvenance {
            geometry: Some(PageGeometry {
                raster_box: [0.0, 0.0, 612.0, 792.0],
                page_box: [0.0, 0.0, 612.0, 792.0],
                clip_box: None,
                raster_quad: Some([0.0, 0.0, 612.0, 0.0, 612.0, 792.0, 0.0, 792.0]),
            }),
            ..Default::default()
        };
        let raw = marshal_page(&input);
        assert!(raw.contains(r#""raster_quad""#), "{raw}");
        assert_eq!(unmarshal_page(&raw), input);
    }

    #[test]
    fn page_provenance_geometry_omits_raster_quad_when_axis_aligned() {
        let raw = marshal_geometry(&PageGeometry {
            raster_box: [0.0, 0.0, 612.0, 792.0],
            page_box: [0.0, 0.0, 612.0, 792.0],
            ..Default::default()
        });
        assert!(!raw.contains(r#""raster_quad""#), "{raw}");
    }

    #[test]
    fn page_straightened_zero_is_not_omitted_and_round_trips() {
        let input = PageProvenance {
            straightened: Some(PageStraighten { deg: 0.0 }),
            ..Default::default()
        };
        let raw = marshal_page(&input);
        assert_eq!(raw, r#"{"straightened":{"deg":0}}"#);
        assert_eq!(unmarshal_page(&raw), input);
    }

    #[test]
    fn page_provenance_decodes_legacy_record_without_straightened() {
        let out = unmarshal_page(r#"{"applied":["extract-raster"]}"#);
        assert_eq!(out.straightened, None);
    }

    #[test]
    fn page_straightened_negative_round_trips() {
        let input = PageProvenance {
            straightened: Some(PageStraighten { deg: -1.7 }),
            ..Default::default()
        };
        assert_eq!(unmarshal_page(&marshal_page(&input)), input);
    }

    #[test]
    fn duplicate_pages_key_merges_into_old_elements() {
        let got = Provenance::from_json(
            br#"{"pages":[{"diverted":"x"}],"pages":[{"applied":["a"]},null]}"#,
            &lim(),
        )
        .expect("unmarshal");
        let pages = got.pages.expect("pages");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].diverted, "x");
        assert_eq!(pages[0].applied, strings(&["a"]));
        assert_eq!(pages[1], PageProvenance::default());
    }

    #[test]
    fn many_empty_pages_hit_the_alloc_limit() {
        let mut data = b"{\"pages\":[".to_vec();
        data.extend(std::iter::repeat_n(&b"{},"[..], 100_000).flatten());
        data.extend_from_slice(b"{}]}");
        let limits = Limits {
            max_alloc: 20_000_000,
            ..Limits::default()
        };
        assert!(matches!(
            Provenance::from_json(&data, &limits),
            Err(Error::Alloc { .. })
        ));
    }

    #[test]
    fn page_geometry_covers_page() {
        let cases: [(&str, [f64; 4], [f64; 4], bool); 8] = [
            (
                "natural-DPI raster leaves a strip: not covered",
                [0.0, 0.0, 568.3708, 791.7616],
                [0.0, 0.0, 612.0, 792.0],
                false,
            ),
            (
                "exactly-filling box: covered",
                [0.0, 0.0, 612.0, 792.0],
                [0.0, 0.0, 612.0, 792.0],
                true,
            ),
            (
                "short by less than the tolerance: covered",
                [0.4, 0.4, 611.6, 791.6],
                [0.0, 0.0, 612.0, 792.0],
                true,
            ),
            (
                "zero PageBox: not covered by anything",
                [0.0, 0.0, 612.0, 792.0],
                [0.0, 0.0, 0.0, 0.0],
                false,
            ),
            (
                "degenerate non-zero PageBox: not covered by anything",
                [0.0, 0.0, 612.0, 792.0],
                [100.0, 100.0, 100.0, 100.0],
                false,
            ),
            (
                "inverted PageBox corners: not covered by anything",
                [0.0, 0.0, 1.0, 1.0],
                [612.0, 792.0, 0.0, 0.0],
                false,
            ),
            (
                "exactly at the tolerance boundary: covered",
                [1.0, 1.0, 611.0, 791.0],
                [0.0, 0.0, 612.0, 792.0],
                true,
            ),
            (
                "just past the tolerance boundary: not covered",
                [1.01, 1.01, 610.99, 790.99],
                [0.0, 0.0, 612.0, 792.0],
                false,
            ),
        ];
        for (name, raster_box, page_box, want) in cases {
            let g = PageGeometry {
                raster_box,
                page_box,
                ..Default::default()
            };
            assert_eq!(g.covers_page(), want, "{name}");
        }
    }

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "{VERSION}");
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()), "{VERSION}");
    }
}

/// Tests of the PDF half: `write_provenance`, `read_provenance`, `record_extraction`.
#[cfg(test)]
mod pdf_tests {
    use super::*;
    use crate::optimize::testpdf::{image_doc, image_doc_bodies, pdf_of};

    const PINNED: pdf::GoTime = pdf::GoTime {
        unix: 946_684_800,
        utc_offset_min: 0,
    };

    fn lim() -> Limits {
        Limits::default()
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // TestWriteReadProvenanceRoundTrip: through a real PDF, so that a pdf-rs level mangling of
    // the property string is seen.
    #[test]
    fn write_read_round_trip() {
        let inp = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(capabilities().iter().map(|s| (*s).to_string()).collect()),
            processed_at: Time::parse_rfc3339(b"2026-07-31T09:00:00Z").expect("time"),
            pages: Some(vec![
                PageProvenance {
                    applied: strings(&["extract-raster"]),
                    geometry: Some(PageGeometry {
                        raster_box: [0.0, 0.0, 568.3708, 791.7616],
                        page_box: [0.0, 0.0, 612.0, 792.0],
                        ..PageGeometry::default()
                    }),
                    ..PageProvenance::default()
                },
                PageProvenance {
                    diverted: "unsupported-codec-jbig2".to_string(),
                    ..PageProvenance::default()
                },
            ]),
            optimized: String::new(),
        };
        let out = write_provenance(&image_doc(1), &inp, &lim(), &never()).expect("write");
        let got = read_provenance(&out, &lim(), &never())
            .expect("read")
            .expect("the record just written");
        assert_eq!(got, inp);
    }

    // TestReadProvenanceAbsent: a never-seen document is `None`, not an error.
    #[test]
    fn read_absent() {
        assert_eq!(read_provenance(&image_doc(1), &lim(), &never()), Ok(None));
    }

    // errCorruptProvenance: a value under the key that is not a record.
    #[test]
    fn read_corrupt_value() {
        let mut bodies = image_doc_bodies(1);
        bodies.push("<< /byblos-provenance (not valid json) >>".to_string());
        let pdf = pdf_of(&bodies, "/Info 6 0 R");
        let e = read_provenance(&pdf, &lim(), &never()).expect_err("corrupt");
        assert_eq!(e, Error::CorruptProvenance);
        assert_eq!(e.class(), "error");
    }

    #[test]
    fn write_replaces_an_earlier_record() {
        let one = Provenance {
            version: "one".to_string(),
            ..Provenance::default()
        };
        let two = Provenance {
            version: "two".to_string(),
            ..Provenance::default()
        };
        let a = write_provenance(&image_doc(1), &one, &lim(), &never()).expect("write");
        let b = write_provenance(&a, &two, &lim(), &never()).expect("write");
        let got = read_provenance(&b, &lim(), &never()).expect("read");
        assert_eq!(got.map(|p| p.version), Some("two".to_string()));
    }

    #[test]
    fn not_a_pdf_and_cancelled() {
        assert!(read_provenance(b"not a pdf", &lim(), &never()).is_err());
        assert!(write_provenance(b"not a pdf", &Provenance::default(), &lim(), &never()).is_err());
        let cancel = AtomicBool::new(true);
        assert_eq!(
            read_provenance(&image_doc(1), &lim(), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(
            write_provenance(&image_doc(1), &Provenance::default(), &lim(), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(
            record_extraction(&image_doc(1), Some(PINNED), &lim(), &cancel),
            Err(Error::Cancelled)
        );
    }

    #[test]
    fn unmarshalable_record_is_refused_before_the_pdf_is_read() {
        let p = Provenance {
            pages: Some(vec![PageProvenance {
                placement: vec![f64::NAN],
                ..PageProvenance::default()
            }]),
            ..Provenance::default()
        };
        let e = write_provenance(b"not a pdf", &p, &lim(), &never()).expect_err("NaN");
        assert!(e.to_string().contains("marshal provenance"), "{e}");
    }

    #[test]
    fn union_sorted_is_sorted_and_deduplicated() {
        assert_eq!(
            union_sorted(&strings(&["b", "a"]), &strings(&["c", "a", "b"])),
            strings(&["a", "b", "c"])
        );
        assert_eq!(union_sorted(&[], &[]), Vec::<String>::new());
    }

    #[test]
    fn clock_is_pinned_or_the_system_clock() {
        assert_eq!(clock(Some(PINNED)), Time::from_go_time(PINNED));
        let t = clock(None);
        assert!(t.unix > 1_700_000_000, "{t:?}");
        assert_eq!(t.offset_secs, 0);
        assert!(t.nanos < 1_000_000_000);
    }

    // TestRecordExtractionIsOneEntryPerPageInOrder / ...ClaimsOnlyExtractRaster /
    // ...StampsVersionAndProcessedAt, on a hand-built document: whatever extraction makes of a
    // page, the record has one entry each and the document-level fields are fixed.
    #[test]
    fn record_extraction_has_one_entry_per_page() {
        let pdf = image_doc(3);
        let p = record_extraction(&pdf, Some(PINNED), &lim(), &never()).expect("record");
        assert_eq!(p.version, VERSION);
        assert_eq!(p.capabilities, Some(strings(&["extract-raster"])));
        assert_eq!(p.processed_at, Time::from_go_time(PINNED));
        assert_eq!(p.pages.map(|v| v.len()), Some(3));
        assert_eq!(p.optimized, "");
    }

    // TestRecordExtractionFailsWhenTheDocumentWontOpen.
    #[test]
    fn record_extraction_fails_when_the_document_wont_open() {
        let e =
            record_extraction(b"not a pdf", Some(PINNED), &lim(), &never()).expect_err("not a pdf");
        assert_ne!(e, Error::Cancelled);
    }

    // TestRecordExtractionPreservesAnExistingRecord: `optimized` is kept, the capabilities are
    // the union, and each page's `applied` is the union of the old page's and this call's; a
    // page's `straightened` is carried forward.
    #[test]
    fn record_extraction_preserves_an_existing_record() {
        let old = Provenance {
            version: "0.0.1".to_string(),
            capabilities: Some(strings(&["linearize", "text-layer"])),
            optimized: "rewritten-linearized".to_string(),
            pages: Some(vec![PageProvenance {
                applied: strings(&["text-layer"]),
                straightened: Some(PageStraighten { deg: -1.25 }),
                ..PageProvenance::default()
            }]),
            ..Provenance::default()
        };
        let pdf = write_provenance(&image_doc(1), &old, &lim(), &never()).expect("seed");
        let p = record_extraction(&pdf, Some(PINNED), &lim(), &never()).expect("record");
        assert_eq!(p.version, VERSION);
        assert_eq!(p.optimized, "rewritten-linearized");
        assert_eq!(
            p.capabilities,
            Some(strings(&["extract-raster", "linearize", "text-layer"]))
        );
        let pages = p.pages.expect("pages");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].straightened, Some(PageStraighten { deg: -1.25 }));
    }

    // The Go tests of record_extraction_test.go that read `internal/corpus` documents
    // (TestRecordExtractionRecordsUnroundedGeometry, ...DivertedPageHasNoGeometry,
    // ...NormalizesAReversedCropBox, ...AbortsOnAnUnreadablePage and the rest) are the oracle's
    // `record` op until the corpus is committed.
    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_records_unrounded_geometry() {
        let path = format!("{}/tests/data/corpus/scan.pdf", env!("CARGO_MANIFEST_DIR"));
        let pdf = std::fs::read(path).expect("fixture");
        let p = record_extraction(&pdf, Some(PINNED), &lim(), &never()).expect("record");
        let pages = p.pages.expect("pages");
        assert_eq!(pages[0].applied, strings(&["extract-raster"]));
        let g = pages[0].geometry.as_ref().expect("geometry");
        assert!(g.covers_page());
    }

    #[test]
    #[ignore = "needs tests/data/corpus/malformed.pdf from the Go oracle -gen"]
    fn corpus_malformed_aborts() {
        let path = format!(
            "{}/tests/data/corpus/malformed.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let pdf = std::fs::read(path).expect("fixture");
        assert!(record_extraction(&pdf, Some(PINNED), &lim(), &never()).is_err());
    }
}
