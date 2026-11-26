//! Wave 3: port of byblos optimize.go: [`optimize`], a structural rewrite of a PDF that
//! never returns more bytes than it was given (unless it linearizes), with optional JPEG
//! recompression.
//!
//! # Differences from Go
//!
//! * One function for `Optimize`, `OptimizeContext` and `OptimizeLimits`: it takes
//!   `&Limits` and a cancel flag. A cancelled call returns [`Error::Cancelled`] and no bytes.
//! * [`OptimizeOptions::now`] is the clock of the `processed_at` stamp (`None`: the system
//!   clock); the parity oracle passes 2000-01-01T00:00:00Z. A pinned clock also selects the
//!   parity random source of pdf-rs's writer (`WriteEnv::parity`), which only the encrypted
//!   fallback of the writer reads.
//! * Go reserved the JPEG decode from the global pool (`reserveDCT`); the pool is gone, so
//!   only the refusals that are not about memory are kept ([`crate::dct::check_frame`]), and
//!   the decode and encode charge the document's own `max_alloc` ledger.
//! * Go's recompression pass uses `context.Background()` for the document it opens and for
//!   `RawImage`; here `cancel` reaches them too, which only makes the call stop sooner.
//!
//! Go's size and linearization policy is kept as it is. [`optimize`] returns the smaller of the
//! input and the rewritten document, and records the branch in `Provenance.optimized`. With
//! [`OptimizeOptions::linearize`] the rule is suspended (linearization costs bytes, and it is
//! the point of the call) and linearization runs last, because anything that serializes the
//! document afterwards removes it again.

use crate::dct;
use crate::encoded::{ColorSpace, EncodedImage};
use crate::error::{Error, Result};
use crate::image::{Image, jpeg_write};
use crate::limits::{Limits, check_cancel, mul_sat};
use crate::linearize::wrap;
use crate::pdfdoc::doc::open_with_mem;
use crate::pdfdoc::optimize::optimize_with_mem;
use crate::pdfdoc::{self, Doc};
use crate::provenance::{
    PageProvenance, Provenance, VERSION, clock, default_caps, read_provenance_with_mem,
    write_provenance_at,
};
use crate::substitute::scan_page;
use pdf::filter::gojpeg;
use pdf::{GoTime, WriteEnv};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// `OptimizeOptions`: what [`optimize`] does beyond the structural rewrite.
#[derive(Debug, Clone, Copy, Default)]
pub struct OptimizeOptions {
    /// Requests a linearized ("fast web view") output, written by byblos's own Annex F writer.
    ///
    /// The output is larger than the same rewrite without it, and usually larger than the
    /// input: the never-larger-than-input rule is suspended for this branch and only for it.
    /// Linearization runs last, and nothing may re-serialize the document after it.
    pub linearize: bool,
    /// Re-encodes every DCTDecode image XObject at [`OptimizeOptions::jpeg_quality`]. It is lossy
    /// and the only lossy thing [`optimize`] does.
    ///
    /// It touches only an image that pdf-rs renders as file type `jpg` (at most three
    /// components), whose `/ColorSpace` is the name `/DeviceGray` or `/DeviceRGB`, with no
    /// `/SMask`, `/Mask`, `/ImageMask` or `/Decode`. Everything else is left alone, with no
    /// error. A re-encoded stream replaces the old one only when it is strictly smaller. The
    /// pages that earned it carry `jpeg-recompress-<quality>` in their `applied`.
    pub recompress_jpeg: bool,
    /// 1 to 100, checked when `recompress_jpeg` is set. 0 is an error, not a default.
    pub jpeg_quality: i64,
    /// The clock of `processed_at`. `None` is the system clock.
    pub now: Option<GoTime>,
}

/// How much of a file can hold the linearization parameter dictionary: ISO 32000-1:2008 Annex
/// F.2.2 puts it first in the file and entirely within the first 1024 bytes.
pub const LINEARIZATION_WINDOW: usize = 1024;

const LINEARIZED_MARKER: &[u8] = b"/Linearized";

/// `fmt.Errorf("byblos: optimize: %w", err)`.
fn ctx(e: Error) -> Error {
    wrap(e, "byblos: optimize")
}

/// The pdf-rs write environment for `now`: the real clock and random source, or the pinned
/// instant with the parity random source.
pub(crate) fn write_env(now: Option<GoTime>) -> WriteEnv {
    match now {
        Some(t) => WriteEnv {
            now: t,
            random: pdf::env::parity_random,
        },
        None => WriteEnv::default(),
    }
}

/// `Optimize`: writes a structurally optimized copy of `pdf`.
///
/// The result is the smaller of the input and pdf-rs's rewrite (the input bytes verbatim when the
/// rewrite plus the provenance record would be larger), so with
/// [`OptimizeOptions::linearize`] unset it is never larger than `pdf`. The branch taken is
/// recorded in `Provenance.optimized` (`rewritten`, `rewritten-delinearized` when the rewrite
/// threw away the input's linearization, `rewritten-linearized`); the pass-through branch
/// records nothing, because any in-band write would grow the input. A document with no prior
/// record gets a fresh one with no capabilities: this call extracts and inspects nothing.
///
/// `limits` gates every option: the input is opened under its parse caps first, the
/// recompression pass adds the content-walk caps, and the linearization re-open uses them too.
/// `cancel` is checked between the passes of pdf-rs (a whole pass is the latency) and per page
/// and per image of the recompression.
///
/// # Errors
/// [`Error::Malformed`] when `jpeg_quality` is outside 1..100 with `recompress_jpeg` set; the
/// errors of [`Doc::open_limits`], [`pdfdoc::optimize`], [`read_provenance`](crate::provenance::read_provenance) (a corrupt record
/// counts as none), [`write_provenance_at`] and [`pdfdoc::linearize`], each with the context
/// `byblos: optimize`; [`Error::Limit`], [`Error::Alloc`], [`Error::Cancelled`].
pub fn optimize(
    pdf: &[u8],
    opts: &OptimizeOptions,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if opts.recompress_jpeg && !(1..=100).contains(&opts.jpeg_quality) {
        return Err(Error::Malformed(format!(
            "byblos: optimize: JPEGQuality {} is outside 1..100 (75 is a reasonable default)",
            opts.jpeg_quality
        )));
    }

    // The gate, the recompression and the rewrite share one ledger; the recompressed PDF stays
    // charged to it while it is rewritten.
    let mem = limits.mem();
    // Gate every option on the parse caps: `pdfdoc::optimize` runs under the package defaults.
    open_with_mem(pdf, limits, &mem, cancel)
        .map(drop)
        .map_err(ctx)?;

    let env = write_env(opts.now);
    // `applied` is built from the ORIGINAL bytes, and is discarded with the recompressed
    // candidate when the "never larger than input" comparison falls back to `pdf`.
    let mut applied = BTreeMap::new();
    let recompressed = if opts.recompress_jpeg {
        let (out, a) =
            recompress_jpeg(pdf, opts.jpeg_quality, &env, limits, &mem, cancel).map_err(ctx)?;
        applied = a;
        out
    } else {
        None
    };
    let input = recompressed.as_deref().unwrap_or(pdf);
    let input_held = mem.charge(recompressed.as_ref().map_or(0, Vec::len) as u64)?;

    // Checked here and not again after the rewrite: `read_provenance` opens with the same check.
    check_cancel(cancel)?;
    let rewritten = optimize_with_mem(input, limits, &mem, &env, cancel).map_err(ctx)?;
    drop(input_held);
    drop(recompressed);

    // Read what provenance the input already carried (the rewrite keeps the Info dictionary),
    // set the branch marker, and write it back onto the rewritten bytes.
    // The provenance read shares the ledger, with the rewritten bytes charged while they live.
    let caps = default_caps(&without(limits, rewritten.len()));
    let rewritten_held = mem
        .charge(rewritten.len() as u64)
        .map_err(|e| ctx(e.into()))?;
    let mut prov = match read_provenance_with_mem(&rewritten, &caps, &mem, cancel) {
        Ok(p) => p,
        // A value under the key that is not valid JSON is not this call's fault, and refusing
        // an otherwise valid PDF over it would give up a free pass-through candidate.
        Err(Error::CorruptProvenance) => None,
        Err(e) => return Err(ctx(e)),
    }
    .unwrap_or_else(|| Provenance {
        // No record of ever being processed by byblos, so no capabilities either: this call
        // only rewrites structure.
        version: VERSION.to_string(),
        processed_at: clock(opts.now),
        ..Provenance::default()
    });
    if !applied.is_empty() {
        let pages = prov.pages.get_or_insert_with(Vec::new);
        for (idx, entries) in applied {
            while pages.len() <= idx {
                pages.push(PageProvenance::default());
            }
            if let Some(pp) = pages.get_mut(idx) {
                pp.applied.extend(entries);
            }
        }
        let capabilities = prov.capabilities.get_or_insert_with(Vec::new);
        if !capabilities.iter().any(|c| c == "jpeg-recompress") {
            capabilities.push("jpeg-recompress".to_string());
            capabilities.sort();
        }
    }
    prov.optimized = if opts.linearize {
        "rewritten-linearized"
    } else if is_linearized(pdf) {
        // The rewrite has just thrown away the input's linearization: the one case where taking
        // the smaller candidate is not free.
        "rewritten-delinearized"
    } else {
        "rewritten"
    }
    .to_string();

    let candidate = write_provenance_at(&rewritten, &prov, opts.now, &caps, cancel).map_err(ctx)?;
    drop(rewritten_held);
    drop(rewritten);
    let held = candidate.len();

    if opts.linearize {
        // Last, and nothing may run after it.
        return pdfdoc::linearize(&candidate, &without(limits, held), cancel).map_err(ctx);
    }

    // Whichever candidate is not larger than the ORIGINAL input is returned (not the
    // recompressed `input`: when recompression's own noise pushes the candidate over the
    // original size, the original comes back verbatim and the `applied` entries, which live
    // only in the discarded candidate, are lost with it).
    if candidate.len() <= pdf.len() {
        return Ok(candidate);
    }
    drop(candidate);
    let _held = mem.charge(pdf.len() as u64)?;
    let mut out = Vec::new();
    out.try_reserve_exact(pdf.len())?;
    out.extend_from_slice(pdf);
    Ok(out)
}

/// `limits` with `held` bytes of an intermediate PDF already alive taken off `max_alloc`: the
/// provenance and linearization stages open their own ledgers, and the stages' outputs are held
/// between them.
fn without(limits: &Limits, held: usize) -> Limits {
    Limits {
        max_alloc: limits.alloc_limit().saturating_sub(held as u64).max(1),
        ..*limits
    }
}

/// Re-encodes every eligible DCTDecode image XObject of `data` at `quality`, once per distinct
/// object even when several pages share it, and returns the resulting document with which
/// 0-based page indices earned a `jpeg-recompress-<quality>` entry. When nothing was
/// substituted the document is `None` (use `data`) and the map is empty, with no write round
/// trip.
fn recompress_jpeg(
    data: &[u8],
    quality: i64,
    env: &WriteEnv,
    lim: &Limits,
    mem: &pdf::Mem,
    cancel: &AtomicBool,
) -> Result<(Option<Vec<u8>>, BTreeMap<usize, Vec<String>>)> {
    let mut d = open_with_mem(data, lim, mem, cancel)
        .map_err(|e| wrap(e, "byblos: optimize: recompress: open"))?;

    let n = d.page_count();
    let mut page_image_ids: Vec<Vec<i64>> = Vec::new();
    page_image_ids.try_reserve_exact(n)?;
    for i in 1..=n {
        check_cancel(cancel)?;
        let scan = scan_page(&mut d, i, cancel)
            .map_err(|e| wrap(e, &format!("byblos: optimize: recompress: page {i}")))?;
        let mut seen = BTreeSet::new();
        let mut ids = Vec::new();
        for pl in &scan.images {
            if seen.insert(pl.id) {
                ids.push(pl.id);
            }
        }
        page_image_ids.push(ids);
    }

    let entry = format!("jpeg-recompress-{quality}");
    // Object id to whether it was actually replaced.
    let mut substituted: BTreeMap<i64, bool> = BTreeMap::new();
    let mut applied: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (i, ids) in page_image_ids.iter().enumerate() {
        let mut page_applied = false;
        for &id in ids {
            check_cancel(cancel)?;
            let did = match substituted.get(&id) {
                Some(&b) => b,
                None => {
                    let b = recompress_one_image(&mut d, id, quality, cancel)?;
                    substituted.insert(id, b);
                    b
                }
            };
            if did && !page_applied {
                applied.entry(i).or_default().push(entry.clone());
                page_applied = true;
            }
        }
    }
    if applied.is_empty() {
        return Ok((None, applied));
    }
    let out = d
        .write(env, cancel)
        .map_err(|e| wrap(e, "byblos: optimize: recompress: write"))?;
    Ok((Some(out), applied))
}

/// Re-encodes the image XObject `id` at `quality` and substitutes it through
/// [`Doc::replace_image`], reporting whether it did.
///
/// Eligibility: pdf-rs renders it as file type `jpg`, its `/ColorSpace` is the name
/// `/DeviceGray` or `/DeviceRGB`, and it carries no `/SMask`, `/Mask`, `/ImageMask` or
/// `/Decode` (the substituted stream drops `/Decode`, which would invert a legal
/// `/Decode [1 0 ...]` image). Everything ineligible, a JPEG the decoder cannot read included, is
/// "not substituted", not an error.
fn recompress_one_image(d: &mut Doc, id: i64, quality: i64, cancel: &AtomicBool) -> Result<bool> {
    let Some(info) = d.image_info(id).cloned() else {
        return Ok(false);
    };
    if info.smask || info.mask || info.image_mask || info.decode {
        return Ok(false);
    }
    let dict_n = match info.color_space.as_str() {
        "DeviceGray" => 1,
        "DeviceRGB" => 3,
        _ => return Ok(false),
    };

    let (data, file_type) = match d.raw_image(cancel, id) {
        Ok(r) => r,
        Err(Error::UnsupportedCodec) => return Ok(false),
        Err(e) => {
            return Err(wrap(
                e,
                &format!("byblos: optimize: recompress: image {id}"),
            ));
        }
    };
    if file_type != "jpg" {
        return Ok(false);
    }

    // A JPEG that Go's `image/jpeg` cannot decode (arithmetic coding, 12-bit precision,
    // lossless, a truncated scan) is ineligible, not broken: one such image must not fail the
    // whole document. `reserveDCT` refused a frame past the pixel cap or one the dictionary does
    // not allow the same way.
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Ok(false);
    }
    let dict_pixels = mul_sat(info.width, info.height);
    if dct::check_frame(&data, d.limits().image_pixels(), dict_pixels, dict_n).is_err() {
        return Ok(false);
    }
    let mem = d.ctx.xref.mem().clone();
    let (src, _held) = match gojpeg::decode(&data, &mem, cancel) {
        Ok(gojpeg::Decoded { image, held }) => (
            match image {
                gojpeg::GoImage::Gray(i) => Image::Gray(i.into()),
                gojpeg::GoImage::YCbCr(i) => Image::YCbCr(i.into()),
                gojpeg::GoImage::Rgba(i) => Image::Rgba(i.into()),
                gojpeg::GoImage::Cmyk(i) => Image::Cmyk(i.into()),
            },
            held,
        ),
        Err(e) => {
            return match Error::from(e) {
                Error::Cancelled => Err(Error::Cancelled),
                _ => Ok(false),
            };
        }
    };
    let q = i32::try_from(quality).unwrap_or(100);
    let buf = jpeg_write::encode(&src, q, &mem, cancel).map_err(|e| {
        wrap(
            e,
            &format!("byblos: optimize: recompress: encode image {id}"),
        )
    })?;
    // The reason this rule is not suspended for the document-level guarantee: a candidate that
    // did not shrink is simply not applied.
    if buf.len() >= data.len() {
        return Ok(false);
    }

    let cs = if matches!(src, Image::Gray(_)) {
        "DeviceGray"
    } else {
        "DeviceRGB"
    };
    drop(src);
    d.replace_image(
        id,
        EncodedImage {
            width: info.width,
            height: info.height,
            bpc: 8,
            color_space: ColorSpace {
                name: cs.to_string(),
                ..ColorSpace::default()
            },
            filter: "DCTDecode".to_string(),
            data: buf,
            ..EncodedImage::default()
        },
    )
    .map_err(|e| {
        wrap(
            e,
            &format!("byblos: optimize: recompress: replace image {id}"),
        )
    })?;
    Ok(true)
}

/// `isLinearized`: whether `data` carries a linearization parameter dictionary, that is,
/// whether the file claims "fast web view". It does not parse: Annex F.2.2's placement rule
/// answers from the first [`LINEARIZATION_WINDOW`] bytes with no xref walk, so it also works on
/// documents pdf-rs refuses to read. It reports the claim, not its validity.
pub fn is_linearized(data: &[u8]) -> bool {
    let window = data.get(..LINEARIZATION_WINDOW).unwrap_or(data);
    window
        .windows(LINEARIZED_MARKER.len())
        .any(|w| w == LINEARIZED_MARKER)
}

#[cfg(test)]
pub(crate) mod testpdf {
    //! Hand-built PDFs for the tests of `optimize`, `substitute`, `editpages` and `provenance`:
    //! the Go tests read the documents of `internal/corpus`, which is not ported.

    /// Objects 1..=len(bodies) with a correct xref table; `trailer` is extra trailer entries
    /// (for example `/Info 7 0 R`). Bodies are ASCII, and a stream's `/Length` must be right.
    pub(crate) fn pdf_of(bodies: &[String], trailer: &str) -> Vec<u8> {
        let mut buf = String::from("%PDF-1.7\n");
        let mut offsets = Vec::new();
        for (i, b) in bodies.iter().enumerate() {
            offsets.push(buf.len());
            buf.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, b));
        }
        let start = buf.len();
        buf.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            bodies.len() + 1
        ));
        for off in offsets {
            buf.push_str(&format!("{off:010} 00000 n \n"));
        }
        buf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R {trailer} >>\nstartxref\n{start}\n%%EOF\n",
            bodies.len() + 1
        ));
        buf.into_bytes()
    }

    /// The bodies of an `n`-page document whose pages all paint one 2x2 DeviceGray image
    /// (`/Im0`, object `4 + n`) through one shared content stream (object `3 + n`).
    pub(crate) fn image_doc_bodies(n: usize) -> Vec<String> {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut bodies = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        ];
        for _ in 0..n {
            bodies.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /XObject << /Im0 {} 0 R >> >> >>",
                3 + n,
                4 + n
            ));
        }
        let content = "q 612 0 0 792 0 0 cm /Im0 Do Q\n";
        bodies.push(format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ));
        bodies.push(
            "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray \
             /BitsPerComponent 8 /Length 4 >>\nstream\nABCD\nendstream"
                .to_string(),
        );
        bodies
    }

    /// [`image_doc_bodies`] as a file.
    pub(crate) fn image_doc(n: usize) -> Vec<u8> {
        pdf_of(&image_doc_bodies(n), "")
    }
}

#[cfg(test)]
mod tests {
    use super::testpdf::{image_doc, image_doc_bodies, pdf_of};
    use super::*;
    use std::sync::atomic::Ordering;

    const PINNED: GoTime = GoTime {
        unix: 946_684_800,
        utc_offset_min: 0,
    };

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn run(pdf: &[u8], opts: &OptimizeOptions) -> Result<Vec<u8>> {
        optimize(pdf, opts, &Limits::default(), &never())
    }

    fn pinned() -> OptimizeOptions {
        OptimizeOptions {
            now: Some(PINNED),
            ..OptimizeOptions::default()
        }
    }

    // TestIsLinearizedIgnoresAMarkerPastTheWindow.
    #[test]
    fn is_linearized_ignores_a_marker_past_the_window() {
        let mut buf = "%comment padding\n".repeat(200).into_bytes();
        buf.extend_from_slice(b"/Linearized 1");
        assert!(buf.len() > LINEARIZATION_WINDOW);
        assert!(!is_linearized(&buf));
        // A file shorter than the window must not panic on the slice bound.
        assert!(!is_linearized(b"%PDF-1.7\n"));
        assert!(!is_linearized(b""));
    }

    #[test]
    fn is_linearized_finds_the_marker_inside_the_window() {
        let mut buf = b"%PDF-1.7\n1 0 obj\n<< /Linearized 1 /L 100 >>\nendobj\n".to_vec();
        assert!(is_linearized(&buf));
        // The window is the first 1024 bytes: a marker straddling its end does not count.
        buf = vec![b' '; LINEARIZATION_WINDOW - 5];
        buf.extend_from_slice(LINEARIZED_MARKER);
        assert!(!is_linearized(&buf));
        buf = vec![b' '; LINEARIZATION_WINDOW - LINEARIZED_MARKER.len()];
        buf.extend_from_slice(LINEARIZED_MARKER);
        assert_eq!(buf.len(), LINEARIZATION_WINDOW);
        assert!(is_linearized(&buf));
    }

    // TestOptimizeZeroByteInput, TestOptimizeNonPDFInput.
    #[test]
    fn degenerate_inputs_are_errors() {
        assert!(run(b"", &pinned()).is_err());
        assert!(run(b"not a pdf at all, just text\n", &pinned()).is_err());
    }

    // TestOptimizeRecompressJPEGQualityValidation: checked before any work, 0 is an error.
    #[test]
    fn jpeg_quality_is_validated_before_any_work() {
        let pdf = image_doc(1);
        for q in [0, -1, 101, 1000] {
            let opts = OptimizeOptions {
                recompress_jpeg: true,
                jpeg_quality: q,
                ..pinned()
            };
            let err = run(&pdf, &opts).expect_err("quality out of range");
            assert_eq!(err.class(), "error", "quality {q}");
            assert!(err.to_string().contains("outside 1..100"), "{err}");
        }
        // Not checked when recompression is off.
        let opts = OptimizeOptions {
            jpeg_quality: 0,
            ..pinned()
        };
        assert!(run(&pdf, &opts).is_ok());
    }

    #[test]
    fn a_cancelled_call_returns_no_bytes() {
        let cancel = AtomicBool::new(true);
        let r = optimize(&image_doc(1), &pinned(), &Limits::default(), &cancel);
        assert_eq!(r, Err(Error::Cancelled));
        cancel.store(false, Ordering::Relaxed);
        assert!(optimize(&image_doc(1), &pinned(), &Limits::default(), &cancel).is_ok());
    }

    // TestOptimizeNeverLargerThanInput and TestOptimizeOutputValidates, on a hand-built document.
    #[test]
    fn output_is_never_larger_and_still_reads() {
        for n in 1..=3 {
            let pdf = image_doc(n);
            let out = run(&pdf, &pinned()).expect("optimize");
            assert!(
                out.len() <= pdf.len(),
                "n={n}: {} > {}",
                out.len(),
                pdf.len()
            );
            let r = crate::provenance::read_provenance(&out, &Limits::default(), &never());
            assert!(r.is_ok(), "n={n}: {r:?}");
        }
    }

    // TestOptimizeCorruptProvenanceFallsBack: a value under the key that is not JSON must not
    // abort the call.
    #[test]
    fn corrupt_provenance_falls_back() {
        let mut bodies = image_doc_bodies(1);
        bodies.push("<< /byblos-provenance (not valid json) >>".to_string());
        let pdf = pdf_of(&bodies, "/Info 6 0 R");
        let out = run(&pdf, &pinned()).expect("a corrupt record is no record");
        assert!(!out.is_empty());
        assert!(out.len() <= pdf.len());
    }

    // TestOptimizeLinearizeSuspendsTheNeverLargerRule: asking to linearize gets a linearized
    // document, and the record says so.
    #[test]
    fn linearize_runs_last_and_is_recorded() {
        let pdf = image_doc(2);
        let opts = OptimizeOptions {
            linearize: true,
            ..pinned()
        };
        let out = run(&pdf, &opts).expect("optimize");
        assert!(is_linearized(&out), "the output is not linearized");
        assert_eq!(&out[..5], b"%PDF-");
        let p = crate::provenance::read_provenance(&out, &Limits::default(), &never())
            .expect("read")
            .expect("record");
        assert_eq!(p.optimized, "rewritten-linearized");
        assert_eq!(p.version, VERSION);
        assert_eq!(p.processed_at, crate::go::json::Time::from_go_time(PINNED));
        assert_eq!(p.capabilities, None);
    }

    // The three files of the Go tests below need `internal/corpus` documents. The oracle's
    // `-gen` writes them under tests/data/corpus/<name>.pdf.
    fn corpus(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    // TestOptimizePassThroughBranch.
    #[test]
    #[ignore = "needs tests/data/corpus/born-digital.pdf from the Go oracle -gen"]
    fn pass_through_returns_the_input_verbatim() {
        let pdf = corpus("born-digital");
        let out = run(&pdf, &pinned()).expect("optimize");
        assert_eq!(out, pdf);
        let p = crate::provenance::read_provenance(&out, &Limits::default(), &never());
        assert!(p.expect("read").is_none_or(|p| p.optimized != "rewritten"));
    }

    // TestOptimizeNonPassThroughBranch and TestOptimizeRecordsAPlainRewriteWhenNothingWasLost.
    #[test]
    #[ignore = "needs tests/data/corpus/dup-raster.pdf from the Go oracle -gen"]
    fn rewritten_branch_records_a_plain_rewrite() {
        let pdf = corpus("dup-raster");
        let out = run(&pdf, &pinned()).expect("optimize");
        assert!(out.len() < pdf.len());
        let p = crate::provenance::read_provenance(&out, &Limits::default(), &never())
            .expect("read")
            .expect("record");
        assert_eq!(p.optimized, "rewritten");
        // TestOptimizeFreshRecordClaimsNoCapabilities.
        assert!(p.capabilities.as_deref().unwrap_or_default().is_empty());
    }

    // TestOptimizeNeverLargerThanInput over `corpus.All()`: every name of the oracle's corpus
    // listing that is present in tests/data/corpus.
    #[test]
    #[ignore = "needs tests/data/corpus/*.pdf from the Go oracle -gen"]
    fn never_larger_over_the_corpus() {
        let dir = format!("{}/tests/data/corpus", env!("CARGO_MANIFEST_DIR"));
        for e in std::fs::read_dir(dir).expect("corpus dir") {
            let path = e.expect("entry").path();
            if path.extension().is_none_or(|x| x != "pdf")
                || path.file_stem().is_some_and(|s| s == "malformed")
            {
                continue;
            }
            let pdf = std::fs::read(&path).expect("read");
            let out = run(&pdf, &pinned()).expect("optimize");
            assert!(out.len() <= pdf.len(), "{path:?}");
        }
    }

    // TestOptimizeMalformedInput.
    #[test]
    #[ignore = "needs tests/data/corpus/malformed.pdf from the Go oracle -gen"]
    fn malformed_corpus_document_is_an_error() {
        assert!(run(&corpus("malformed"), &pinned()).is_err());
    }

    // TestOptimizeRecompressJPEGNoLongerRefused, ...ShrinksOutput, ...RecordsApplied,
    // ...OutputReReads (the quality-50 recompression of the two-page shared-JPEG document).
    #[test]
    #[ignore = "needs tests/data/corpus/scan-jpeg.pdf (corpus.ScanJPEG) from the Go oracle -gen"]
    fn recompress_jpeg_shrinks_and_records_both_pages() {
        let pdf = corpus("scan-jpeg");
        let opts = OptimizeOptions {
            recompress_jpeg: true,
            jpeg_quality: 50,
            ..pinned()
        };
        let out = run(&pdf, &opts).expect("optimize");
        assert!(out.len() < pdf.len());
        let p = crate::provenance::read_provenance(&out, &Limits::default(), &never())
            .expect("read")
            .expect("record");
        let pages = p.pages.expect("pages");
        assert_eq!(pages.len(), 2);
        for pp in &pages {
            assert_eq!(pp.applied, vec!["jpeg-recompress-50".to_string()]);
        }
        assert!(
            p.capabilities
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|c| c == "jpeg-recompress")
        );
    }

    // TestOptimizeRecompressJPEGNonDCTPageIsNoOp.
    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn recompress_of_a_document_with_no_jpeg_is_a_no_op() {
        let pdf = corpus("scan");
        let env = write_env(Some(PINNED));
        let (out, applied) = recompress_jpeg(
            &pdf,
            50,
            &env,
            &Limits::default(),
            &Limits::default().mem(),
            &never(),
        )
        .expect("recompress");
        assert!(out.is_none());
        assert!(applied.is_empty());
    }

    // TestOptimizeRecompressJPEGSkipsSMask.
    #[test]
    #[ignore = "needs tests/data/corpus/scan-smask-jpeg.pdf (corpus.ScanSMaskJPEG) from the Go oracle -gen"]
    fn recompress_skips_an_image_with_an_smask() {
        let pdf = corpus("scan-smask-jpeg");
        let env = write_env(Some(PINNED));
        let (out, applied) = recompress_jpeg(
            &pdf,
            10,
            &env,
            &Limits::default(),
            &Limits::default().mem(),
            &never(),
        )
        .expect("recompress");
        assert!(out.is_none());
        assert!(applied.is_empty());
    }

    // TestIsLinearizedSeparatesRealFilesBothWays, negative half, and
    // TestOptimizeRecordsThatTheRewriteDelinearized need a real linearized file (pdfcpu's
    // bookletTest.pdf), which is not committed.
    #[test]
    #[ignore = "needs a real linearized PDF (pdfcpu bookletTest.pdf) in tests/data/real"]
    fn rewrite_of_a_linearized_input_records_delinearized() {
        let path = format!(
            "{}/tests/data/real/bookletTest.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let pdf = std::fs::read(path).expect("fixture");
        assert!(is_linearized(&pdf));
        let out = run(&pdf, &pinned()).expect("optimize");
        assert!(out.len() < pdf.len());
        assert!(!is_linearized(&out));
        let p = crate::provenance::read_provenance(&out, &Limits::default(), &never())
            .expect("read")
            .expect("record");
        assert_eq!(p.optimized, "rewritten-delinearized");
    }
}
