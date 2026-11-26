//! Wave 3: port of byblos editpages.go: [`build_from_pages`], a document whose page `i` is
//! `pages[i]`, and a provenance record that describes it.
//!
//! It is the whole edit vocabulary in one call: deleting a page is omitting it from the
//! sequence, reordering is ordering the sequence, inserting is naming a different source, and
//! rotating is a field. The object-graph migration is `pdfdoc`'s
//! ([`crate::pdfdoc::buildpages`]); this file is the other half, carrying each page's provenance
//! record to its NEW index. That is not a formality: `Provenance::pages` is positional with no
//! page identity, so an export that left the slice alone would describe every page after the
//! first edit wrongly.
//!
//! # Differences from Go
//!
//! * One function for `BuildFromPages`, `BuildFromPagesContext` and `BuildFromPagesLimits`; the
//!   clock of `processed_at` is the `now` argument (`None`: the system clock).
//! * Go identifies a source by `io.ReadSeeker` identity; [`PageSource::source`] is an index into
//!   the `sources` slice, and equal indices are one shared source. Go's nil `Source` is an index
//!   past the end of `sources`.
//! * Go's `clonePageProvenance` is `Clone` here: every field is owned, so a page named twice in
//!   a sequence shares nothing with its source's record.
//! * Object numbers: see docs/go-differences.md (sorted copy order).

pub use crate::pdfdoc::buildpages::PageSource;
pub use crate::pdfdoc::straighten::StraightenSpec;

use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use crate::linearize::wrap;
use crate::pdfdoc;
use crate::provenance::{
    PROVENANCE_KEY, PageProvenance, PageStraighten, Provenance, VERSION, clock, default_caps,
    read_provenance, union_sorted,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// The `PageProvenance::diverted` reason for a page byblos did not process and has no record of,
/// typically a page imported from a document that never went through byblos.
///
/// It is deliberately outside the extraction vocabulary: `upgrade` matches the divert reasons
/// by exact string, so an unrecognised one is inert and nominates nothing. That is the honest
/// state: this page was not diverted BY extraction, it was never offered to it. The zero
/// `PageProvenance` is not an option, as it reads as a page that was handled and had nothing
/// applied.
pub const DIVERTED_NOT_RECORDED: &str = "source-unrecorded";

/// `ValidatePages`: whether [`build_from_pages`] would refuse `pages` before it opened a single
/// source. It is the check a request handler can afford: the sequence is non-empty, every
/// `rotate` is 0, 90, 180 or 270, every `straighten` angle is finite, and no `straighten`
/// carries a `crop` (not implemented).
///
/// It cannot check that a page exists in its source, which needs the document open; `Ok` is
/// "nothing structural is wrong", not "this will build". An encrypted source is likewise refused
/// only at build time. It reads no source, and a `source` index past the end of the slice the
/// build gets is only found by the build.
///
/// # Errors
/// [`Error::Malformed`], naming the first page that cannot be built.
pub fn validate_pages(pages: &[PageSource], _limits: &Limits) -> Result<()> {
    pdfdoc::buildpages::validate_pages(pages)
}

/// `BuildFromPages`: a document whose page `i` is `pages[i]` (a page of `sources[p.source]`),
/// and a provenance record that describes it.
///
/// Each page arrives with its content stream, resources, annotations and inherited attributes
/// pushed down, and its provenance record moved to its new index. The catalog does NOT come
/// with it: outlines, page labels, the structure tree, form fields and named destinations
/// describe the page set or order, and an edit makes them silently wrong.
///
/// The record: `capabilities` is the INTERSECTION of the sources' (one unrecorded source empties
/// it), `optimized` is `rewritten-delinearized` (the output is never linearized; compose with
/// `optimize` for that), and a page with no record is `diverted: source-unrecorded`. A page with
/// a `straighten` gets the ABSOLUTE angle recorded and `straighten` is added to its `applied`;
/// the rotation handed to the migration is the angle minus whatever the source page's record
/// already says, so asking twice for the same angle rotates once.
///
/// Output is not byte-stable across Go runs (object numbering); here it is a function of the
/// inputs. It writes nothing unless the whole document was built; the output is checked with
/// pdf-rs's validator.
///
/// `cancel` is checked per source and per page of the record; the migration and its write are
/// one span.
///
/// # Errors
/// [`Error::Malformed`] for an empty sequence and whatever [`validate_pages`] refuses, a page
/// past the end of its source, an encrypted source and an unbuildable page tree; the errors of
/// `Doc::open_limits`, pdf-rs, the writer and the validator, with the context
/// `byblos: BuildFromPages`; [`Error::Limit`], [`Error::Alloc`], [`Error::Cancelled`].
pub fn build_from_pages(
    sources: &[&[u8]],
    pages: &[PageSource],
    now: Option<pdf::GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if pages.is_empty() {
        return Err(Error::Malformed(
            "byblos: BuildFromPages: no pages".to_string(),
        ));
    }

    // The record is assembled BEFORE the document is built, so that a source whose provenance
    // cannot be read fails the call rather than producing a record that silently describes the
    // wrong pages. It also puts the OLD `straightened.deg` in hand when the absolute-not-delta
    // rule needs it: `deltas[i]` is the increment straighten has to apply to page `i`.
    let (record, deltas) = build_record(sources, pages, now, limits, cancel)?;

    // The absolute-versus-delta translation lives here, the one place that has both the caller's
    // absolute request and the source's prior record. A copy, not a mutation of `pages`.
    let adjusted: Vec<PageSource> = pages
        .iter()
        .zip(&deltas)
        .map(|(p, &delta)| PageSource {
            straighten: p.straighten.map(|s| StraightenSpec { deg: delta, ..s }),
            ..p.clone()
        })
        .collect();

    let data = record.to_json(limits)?;
    let json = String::from_utf8(data).map_err(|e| {
        Error::Malformed(format!("byblos: BuildFromPages: marshal provenance: {e}"))
    })?;
    check_cancel(cancel)?;
    let props = BTreeMap::from([(PROVENANCE_KEY.to_string(), json)]);
    // The sources, the output and the validation share one ledger; the built bytes stay
    // charged to it while they are validated.
    let mem = limits.mem();
    let built =
        pdfdoc::buildpages::build_with_mem(sources, &adjusted, &props, limits, &mem, cancel)
            .map_err(|e| wrap(e, "byblos: BuildFromPages"))?;
    check_cancel(cancel)?;
    let _built = mem.charge(built.len() as u64)?;
    // The gate that folding the provenance write into the build removed: without it a source
    // page carrying an entry the validator refuses (`/PresSteps`) would build and be written
    // with a nil error.
    pdfdoc::write::validate_with_mem(&built, limits, &mem, cancel)
        .map_err(|e| wrap(e, "byblos: BuildFromPages"))?;
    Ok(built)
}

/// `buildRecord`: the exported document's provenance from its sources', and the per-page
/// straighten INCREMENT: `deltas[i]` is `pages[i].straighten.deg` minus whatever page `i`'s
/// source record already has as `straightened.deg`, defaulting to zero for a source with no
/// readable record (what every document byblos has never seen looks like).
fn build_record(
    sources: &[&[u8]],
    pages: &[PageSource],
    now: Option<pdf::GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(Provenance, Vec<f64>)> {
    let caps = default_caps(limits);
    // A source index that has no source (Go's nil `Source`) is skipped here;
    // `build_from_pages_with_properties` refuses it with a message that names the page.
    let mut records: BTreeMap<usize, Option<Provenance>> = BTreeMap::new();
    for p in pages {
        check_cancel(cancel)?;
        let Some(&src) = sources.get(p.source) else {
            continue;
        };
        if records.contains_key(&p.source) {
            continue;
        }
        // A record that is absent, or is present and unparseable, is "no record", the reading
        // `optimize` and `record_extraction` take. A source that cannot be READ at all is a
        // different thing, which the build reports with the page number.
        let got = read_provenance(src, &caps, cancel).ok().flatten();
        records.insert(p.source, got);
    }

    let mut deltas = Vec::new();
    deltas.try_reserve_exact(pages.len())?;
    let mut out_pages = Vec::new();
    out_pages.try_reserve_exact(pages.len())?;
    for p in pages {
        let src = records.get(&p.source).and_then(Option::as_ref);
        // A source with no record, or one whose pages slice is too short to reach this page,
        // cannot say what happened to it.
        let mut pp = match src
            .and_then(|s| s.pages.as_ref())
            .and_then(|v| p.page.checked_sub(1).and_then(|i| v.get(i)))
        {
            Some(rec) => rec.clone(),
            None => PageProvenance {
                diverted: DIVERTED_NOT_RECORDED.to_string(),
                ..PageProvenance::default()
            },
        };

        let mut delta = 0.0;
        if let Some(s) = &p.straighten {
            let old = pp.straightened.map_or(0.0, |st| st.deg);
            delta = s.deg - old;
            pp.applied = union_sorted(&pp.applied, &["straighten".to_string()]);
            pp.straightened = Some(PageStraighten { deg: s.deg });
        }
        deltas.push(delta);
        out_pages.push(pp);
    }

    let out = Provenance {
        version: VERSION.to_string(),
        capabilities: common_capabilities(pages, &records),
        processed_at: utc(clock(now)),
        // Output is never linearized; see `build_from_pages`.
        optimized: "rewritten-delinearized".to_string(),
        pages: Some(out_pages),
    };
    Ok((out, deltas))
}

/// `time.Time.UTC()`: the same instant shown at offset 0.
fn utc(t: crate::go::json::Time) -> crate::go::json::Time {
    crate::go::json::Time {
        offset_secs: 0,
        ..t
    }
}

/// `commonCapabilities`: what EVERY contributing source's build could do.
///
/// The intersection, not the union: `capabilities` has no per-page form, so a document assembled
/// from two builds can only honestly claim what both had. Claiming the union would suppress an
/// upgrade nomination for a capability one contributing build lacked, and `upgrade` takes the
/// opposite bias throughout: reporting a wasted re-run is cheaper than hiding a real upgrade.
/// One unrecorded source therefore empties the claim (`None`, which marshals as `null`).
///
/// `None` and `Some(vec![])` stay apart as Go's nil and empty slice do: a source whose record
/// has `capabilities: null` gives `None`, one with `[]` gives `Some(vec![])`.
fn common_capabilities(
    pages: &[PageSource],
    records: &BTreeMap<usize, Option<Provenance>>,
) -> Option<Vec<String>> {
    let mut out: Option<Vec<String>> = None;
    let mut first = true;
    let mut seen = BTreeSet::new();
    for p in pages {
        // A source with no entry here is Go's nil `Source`.
        let Some(rec) = records.get(&p.source) else {
            continue;
        };
        if !seen.insert(p.source) {
            continue;
        }
        let rec = rec.as_ref()?;
        if first {
            out.clone_from(&rec.capabilities);
            first = false;
            continue;
        }
        let theirs = rec.capabilities.as_deref().unwrap_or_default();
        if let Some(mine) = &mut out {
            mine.retain(|c| theirs.contains(c));
        }
    }
    if let Some(v) = &mut out {
        v.sort();
        v.dedup();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimize::testpdf::{image_doc, image_doc_bodies, pdf_of};
    use crate::provenance::{PageGeometry, write_provenance};
    use std::sync::atomic::Ordering;

    const PINNED: pdf::GoTime = pdf::GoTime {
        unix: 946_684_800,
        utc_offset_min: 0,
    };

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn page(source: usize, page: usize) -> PageSource {
        PageSource {
            source,
            page,
            rotate: 0,
            straighten: None,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// `recordedBooklet`: a four-page document carrying a record whose page entries are told
    /// apart by their `applied` marker, so a re-indexing mistake names the page it took the
    /// record from.
    fn recorded(pages: usize) -> Vec<u8> {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut bodies = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {pages} >>",
                kids.join(" ")
            ),
        ];
        for _ in 0..pages {
            bodies.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << >> >>",
                3 + pages
            ));
        }
        bodies.push("<< /Length 0 >>\nstream\n\nendstream".to_string());
        let plain = pdf_of(&bodies, "");
        let p = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(strings(&["extract-raster", "jbig2-generic"])),
            optimized: "rewritten-linearized".to_string(),
            pages: Some(
                (1..=pages)
                    .map(|i| PageProvenance {
                        applied: vec![format!("marker-{i}")],
                        dropped_annots: i as i64,
                        ..PageProvenance::default()
                    })
                    .collect(),
            ),
            ..Provenance::default()
        };
        let out = write_provenance(&plain, &p, &Limits::default(), &never()).expect("seed");
        // The seed has to be readable, or every assertion below is vacuous.
        let got = read_provenance(&out, &Limits::default(), &never())
            .expect("read")
            .expect("record");
        assert_eq!(got.pages.map(|v| v.len()), Some(pages));
        out
    }

    fn build(sources: &[&[u8]], pages: &[PageSource]) -> Vec<u8> {
        build_from_pages(sources, pages, Some(PINNED), &Limits::default(), &never())
            .expect("build_from_pages")
    }

    fn read(pdf: &[u8]) -> Provenance {
        read_provenance(pdf, &Limits::default(), &never())
            .expect("read")
            .expect("the built document carries no provenance record at all")
    }

    // TestBuildFromPagesCarriesEachPagesProvenanceToItsNewIndex.
    #[test]
    fn carries_each_pages_provenance_to_its_new_index() {
        let src = recorded(6);
        let out = build(
            &[src.as_slice()],
            &[
                page(0, 6),
                page(0, 2),
                PageSource {
                    rotate: 90,
                    ..page(0, 6)
                },
            ],
        );
        let p = read(&out);
        let pages = p.pages.expect("pages");
        assert_eq!(pages.len(), 3);
        for (i, want) in [6usize, 2, 6].into_iter().enumerate() {
            assert_eq!(
                pages[i].applied,
                vec![format!("marker-{want}")],
                "page {}",
                i + 1
            );
            assert_eq!(pages[i].dropped_annots, want as i64);
        }
        assert_eq!(p.processed_at, crate::go::json::Time::from_go_time(PINNED));
    }

    // TestBuildFromPagesRecordsAPageItHasNoRecordFor.
    #[test]
    fn records_a_page_it_has_no_record_for() {
        let recorded = recorded(4);
        let plain = image_doc(1);
        let out = build(
            &[recorded.as_slice(), plain.as_slice()],
            &[page(0, 4), page(1, 1)],
        );
        let p = read(&out);
        let pages = p.pages.expect("pages");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].applied, vec!["marker-4".to_string()]);
        // Not the zero record, and not a reason of the extraction vocabulary.
        assert_eq!(pages[1].diverted, DIVERTED_NOT_RECORDED);
        for known in [
            "not-single-raster",
            "unsupported-codec",
            "unsupported-codec-jbig2",
            "unsupported-codec-jpx",
            "unsupported-codec-tiff",
        ] {
            assert_ne!(pages[1].diverted, known);
        }
    }

    // TestBuildFromPagesRecordsDelinearizedOutput.
    #[test]
    fn records_delinearized_output() {
        let src = recorded(2);
        let out = build(&[src.as_slice()], &[page(0, 1)]);
        let p = read(&out);
        assert_eq!(p.optimized, "rewritten-delinearized");
        assert_eq!(p.version, VERSION);
        assert!(!crate::optimize::is_linearized(&out));
    }

    // TestBuildFromPagesClaimsOnlyCapabilitiesEverySourceHad.
    #[test]
    fn claims_only_capabilities_every_source_had() {
        let recorded = recorded(3);
        let plain = image_doc(1);

        let single = build(&[recorded.as_slice()], &[page(0, 2)]);
        let p = read(&single);
        assert!(
            p.capabilities
                .as_deref()
                .unwrap_or_default()
                .contains(&"jbig2-generic".to_string()),
            "{:?}",
            p.capabilities
        );

        let mixed = build(
            &[recorded.as_slice(), plain.as_slice()],
            &[page(0, 2), page(1, 1)],
        );
        let q = read(&mixed);
        assert_eq!(
            q.capabilities, None,
            "one unrecorded source empties the claim"
        );
    }

    // The intersection, with nil and empty kept apart.
    #[test]
    fn common_capabilities_is_the_intersection() {
        let rec = |c: Option<&[&str]>| {
            Some(Provenance {
                capabilities: c.map(strings),
                ..Provenance::default()
            })
        };
        let records = BTreeMap::from([
            (0, rec(Some(&["b", "a", "c", "a"]))),
            (1, rec(Some(&["c", "b", "z"]))),
            (2, rec(Some(&[]))),
            (3, rec(None)),
            (4, None),
        ]);
        let two = [page(0, 1), page(1, 1), page(0, 2)];
        assert_eq!(
            common_capabilities(&two, &records),
            Some(strings(&["b", "c"]))
        );
        // A source with an empty list gives an empty, not a nil, claim.
        let three = [page(0, 1), page(2, 1)];
        assert_eq!(common_capabilities(&three, &records), Some(Vec::new()));
        // A nil list stays nil; an unrecorded source empties the whole claim.
        assert_eq!(common_capabilities(&[page(3, 1)], &records), None);
        assert_eq!(
            common_capabilities(&[page(0, 1), page(4, 1)], &records),
            None
        );
        // No contributing source at all.
        assert_eq!(common_capabilities(&[page(9, 1)], &records), None);
        // The result is sorted and deduplicated even for one source.
        assert_eq!(
            common_capabilities(&[page(0, 1)], &records),
            Some(strings(&["a", "b", "c"]))
        );
    }

    // The enforced-absolute rule (design spec section 2): the record holds the total, the
    // migration gets the increment.
    #[test]
    fn straighten_is_absolute_in_the_record() {
        let bodies = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> >>".to_string(),
        ];
        let plain = pdf_of(&bodies, "");
        let p = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(Vec::new()),
            pages: Some(vec![PageProvenance {
                applied: strings(&["straighten"]),
                straightened: Some(PageStraighten { deg: 1.5 }),
                ..PageProvenance::default()
            }]),
            ..Provenance::default()
        };
        let src = write_provenance(&plain, &p, &Limits::default(), &never()).expect("seed");
        let want = |deg: f64| {
            let spec = StraightenSpec { deg, crop: None };
            [PageSource {
                straighten: Some(spec),
                ..page(0, 1)
            }]
        };
        let (rec, deltas) = build_record(
            &[src.as_slice()],
            &want(2.0),
            Some(PINNED),
            &Limits::default(),
            &never(),
        )
        .expect("record");
        assert_eq!(deltas, vec![0.5]);
        let pages = rec.pages.expect("pages");
        assert_eq!(pages[0].straightened, Some(PageStraighten { deg: 2.0 }));
        assert_eq!(pages[0].applied, strings(&["straighten"]));
        // The same angle again is a zero increment.
        let (_, deltas) = build_record(
            &[src.as_slice()],
            &want(1.5),
            Some(PINNED),
            &Limits::default(),
            &never(),
        )
        .expect("record");
        assert_eq!(deltas, vec![0.0]);
        // A source with no record has nothing to subtract.
        let none = image_doc(1);
        let (rec, deltas) = build_record(
            &[none.as_slice()],
            &want(-3.0),
            Some(PINNED),
            &Limits::default(),
            &never(),
        )
        .expect("record");
        assert_eq!(deltas, vec![-3.0]);
        let pages = rec.pages.expect("pages");
        assert_eq!(pages[0].diverted, DIVERTED_NOT_RECORDED);
        assert_eq!(pages[0].applied, strings(&["straighten"]));
    }

    // A page's geometry and every other field travel with it (the deep copy of Go's
    // TestClonePageProvenanceSharesNoRasterQuadPointer is `Clone`).
    #[test]
    fn page_records_are_independent_copies() {
        let inp = PageProvenance {
            geometry: Some(PageGeometry {
                raster_quad: Some([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
                clip_box: Some([1.0, 2.0, 3.0, 4.0]),
                ..PageGeometry::default()
            }),
            applied: strings(&["x"]),
            ..PageProvenance::default()
        };
        let mut out = inp.clone();
        assert_eq!(out, inp);
        if let Some(g) = &mut out.geometry
            && let Some(q) = &mut g.raster_quad
        {
            q[0] = 99.0;
        }
        out.applied.push("y".to_string());
        assert_eq!(
            inp.geometry.and_then(|g| g.raster_quad).map(|q| q[0]),
            Some(1.0)
        );
        assert_eq!(inp.applied, strings(&["x"]));
    }

    // TestBuildFromPagesRefusalsReachTheCaller.
    #[test]
    fn refusals_reach_the_caller() {
        let src = recorded(2);
        let lim = Limits::default();
        let cases: Vec<(&str, Vec<PageSource>, &str)> = vec![
            ("no pages", vec![], "no pages"),
            ("a page out of range", vec![page(0, 3)], "out of range"),
            (
                "a rotation that is not a quarter turn",
                vec![PageSource {
                    rotate: 45,
                    ..page(0, 1)
                }],
                "rotation",
            ),
            ("a source that is not there", vec![page(5, 1)], "no source"),
            (
                "a crop",
                vec![PageSource {
                    straighten: Some(StraightenSpec {
                        deg: 1.0,
                        crop: Some([0.0; 4]),
                    }),
                    ..page(0, 1)
                }],
                "crop",
            ),
        ];
        for (name, pages, want) in cases {
            let e = build_from_pages(&[src.as_slice()], &pages, Some(PINNED), &lim, &never())
                .expect_err(name);
            assert!(e.to_string().contains(want), "{name}: {e}");
        }
    }

    // ValidatePages: what it can say without a document.
    #[test]
    fn validate_pages_checks_what_needs_no_document() {
        let lim = Limits::default();
        assert!(validate_pages(&[], &lim).is_err());
        assert!(validate_pages(&[page(0, 1), page(7, 99)], &lim).is_ok());
        let bad = PageSource {
            rotate: 360,
            ..page(0, 1)
        };
        assert!(validate_pages(&[bad], &lim).is_err());
        let nan = PageSource {
            straighten: Some(StraightenSpec {
                deg: f64::NAN,
                crop: None,
            }),
            ..page(0, 1)
        };
        assert!(validate_pages(&[nan], &lim).is_err());
    }

    // TestBuildFromPagesContextIsCheckedBeforeAnyWork.
    #[test]
    fn a_cancelled_call_returns_no_bytes() {
        let src = image_doc(1);
        let cancel = AtomicBool::new(true);
        let r = build_from_pages(
            &[src.as_slice()],
            &[page(0, 1)],
            Some(PINNED),
            &Limits::default(),
            &cancel,
        );
        assert_eq!(r, Err(Error::Cancelled));
        cancel.store(false, Ordering::Relaxed);
        assert!(
            build_from_pages(
                &[src.as_slice()],
                &[page(0, 1)],
                Some(PINNED),
                &Limits::default(),
                &cancel
            )
            .is_ok()
        );
    }

    // TestBuildFromPagesContextRefusesADocumentPdfcpuWouldRefuse: the validator gate that folding
    // the provenance write into the build removed. A page carrying /PresSteps is refused by
    // pdf-go's validator whatever its value.
    #[test]
    fn refuses_a_document_the_validator_would_refuse() {
        let mut bodies = image_doc_bodies(1);
        bodies[2] = "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /MediaBox [0 0 612 792] \
                     /Resources << >> /PresSteps << /S /Fly >> >>"
            .to_string();
        let src = pdf_of(&bodies, "");
        let e = build_from_pages(
            &[src.as_slice()],
            &[page(0, 1)],
            Some(PINNED),
            &Limits::default(),
            &never(),
        )
        .expect_err("a page carrying /PresSteps");
        let msg = e.to_string().to_lowercase();
        assert!(
            msg.contains("pressteps") || msg.contains("presentation steps"),
            "{e}"
        );
    }

    // The tests of Go that build from the corpus `booklet` (8 pages) and `mixed` documents:
    // TestBuildFromPagesCarriesEachPagesProvenanceToItsNewIndex and the others above run on a
    // hand-built document here; these two read the corpus.
    #[test]
    #[ignore = "needs tests/data/corpus/mixed.pdf from the Go oracle -gen"]
    fn corpus_mixed_page_out_of_range() {
        let path = format!("{}/tests/data/corpus/mixed.pdf", env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read(path).expect("fixture");
        let e = build_from_pages(
            &[src.as_slice()],
            &[page(0, 3)],
            Some(PINNED),
            &Limits::default(),
            &never(),
        )
        .expect_err("page 3 of a two-page document");
        assert!(e.to_string().contains("out of range"), "{e}");
    }

    #[test]
    #[ignore = "needs tests/data/corpus/booklet.pdf from the Go oracle -gen"]
    fn corpus_booklet_reorders_with_its_records() {
        let path = format!(
            "{}/tests/data/corpus/booklet.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let plain = std::fs::read(path).expect("fixture");
        let p = Provenance {
            version: VERSION.to_string(),
            capabilities: Some(strings(&["extract-raster", "jbig2-generic"])),
            optimized: "rewritten-linearized".to_string(),
            pages: Some(
                (1..=8)
                    .map(|i| PageProvenance {
                        applied: vec![format!("marker-{i}")],
                        dropped_annots: i,
                        ..PageProvenance::default()
                    })
                    .collect(),
            ),
            ..Provenance::default()
        };
        let src = write_provenance(&plain, &p, &Limits::default(), &never()).expect("seed");
        let out = build(&[src.as_slice()], &[page(0, 6), page(0, 2), page(0, 6)]);
        let got = read(&out).pages.expect("pages");
        let markers: Vec<_> = got.iter().map(|g| g.applied.clone()).collect();
        assert_eq!(
            markers,
            vec![
                strings(&["marker-6"]),
                strings(&["marker-2"]),
                strings(&["marker-6"])
            ]
        );
    }
}
