//! Wave 1d: port of byblos `upgrade.go`: which capabilities, gained since a document was
//! processed, would change its output. The rules read the recorded
//! [`Provenance`](crate::provenance::Provenance); a capability may have a rule long before it
//! has an implementation, so shipping one needs no change to this table.
//!
//! Go's `capabilityRules` map of closures is [`capability_rule`], a `match` over the names, and
//! `capabilityIssue` is [`capability_issue`] (it is what `NotImplemented.Issue` reads).
//!
//! `Provenance::capabilities` and `Provenance::pages` are `Option<Vec<_>>` (Go nil slices); `None`
//! reads as empty.

use crate::provenance::{PageProvenance, Provenance};

/// A rule: whether *gaining* a capability would change the output for a document with the
/// given recorded provenance.
pub type Rule = fn(&Provenance) -> bool;

/// `capabilityRules`: the condition under which gaining capability `c` would change the output
/// for a document with the given recorded provenance (goal G3). `None` means `c` has no rule,
/// which [`upgrade_candidates`] reports as a candidate: missing a real upgrade is worse than
/// one wasted re-run.
pub fn capability_rule(c: &str) -> Option<Rule> {
    let rule: Rule = match c {
        // Inspection and extraction do not alter output.
        "inspect" | "extract-raster" => never,

        // Construction happens before a document exists. No recorded provenance can describe a
        // document that would be improved by gaining it: the file was already built by
        // something, and rebuilding it from images Byblos no longer holds is not an upgrade
        // path.
        "build-pdf" => never,

        // A document that already got jbig2-generic cannot benefit from gaining it. A
        // document that got ccitt-g4 can: same losslessness, better ratio.
        "jbig2-generic" => |p| any_page_applied(p, "ccitt-g4"),

        // The intended next capability, the encoder (not the symbol dictionary decoder, which
        // advances decode-jbig2). It keys on `any_page_applied`, so it fires only on pages
        // byblos itself wrote as generic-region JBIG2 and would now write smaller;
        // decode-jbig2 keys on `any_page_diverted`, so it fires on pages byblos could not
        // read.
        "jbig2-symbol" => |p| any_page_applied(p, "jbig2-generic"),

        // A compatibility fallback with strictly worse compression. Gaining it never improves
        // a document; it is only ever chosen deliberately.
        "ccitt-g4" => never,

        // A renderer turns pages Byblos understood but could not reduce to one page-covering
        // raster into processable ones, and nothing else: tiled rasters,
        // image-plus-vector-overlay, mixed content.
        //
        // "unsupported-codec" is deliberately NOT here (byb-97q): that class means the page
        // was understood and its raster could not be decoded, and a renderer cannot read those
        // bytes either; a decoder can. A partial placement (PageGeometry.CoversPage false) is
        // deliberately NOT a term either (byb-b5.1): 63.7% of extracted pages carry
        // covers_page=false and the gain is zero, a renderer emits the same pixels plus
        // synthesized white. DroppedAnnots, by contrast, IS included: rendering appearance
        // streams into the raster would change the output for those pages.
        "render" => |p| {
            p.pages.iter().flatten().any(|pg| pg.dropped_annots > 0)
                || any_page_diverted(p, &["not-single-raster"])
        },

        // Raster decoders, one capability per codec. byb-z8j gave the divert class a
        // codec-specific form per codec, so each rule also keys on its own
        // "unsupported-codec-<codec>" class. The coarse "unsupported-codec" stays in every
        // rule: a document an older build wrote, or one whose codec this build could not
        // name, carries only the coarse class.
        //
        // The names are "decode-<codec>" and not "<codec>-decode" on purpose: "jbig2-decode"
        // would match the "jbig2-" prefix that page-cleanup below tests for.
        "decode-jbig2" => {
            |p| any_page_diverted(p, &["unsupported-codec", "unsupported-codec-jbig2"])
        }
        "decode-jpx" => |p| any_page_diverted(p, &["unsupported-codec", "unsupported-codec-jpx"]),
        "decode-tiff" => |p| any_page_diverted(p, &["unsupported-codec", "unsupported-codec-tiff"]),

        // Output-side capabilities: gaining one does not improve a document that was already
        // processed with it, and we cannot tell from the record whether a page that missed it
        // had content it would have helped. Conservatively never: a false positive here means
        // re-processing the whole archive.
        "quantize-png" | "downsample" | "jpeg-recompress" | "text-layer" => never,

        // Linearization is a whole-file property, so its rule reads `optimized` rather than
        // any page's record. Provenance records no "the caller asked for linearization" flag,
        // so this also nominates documents whose caller deliberately chose not to linearize:
        // the conservative direction, a wasted re-run beats a hidden upgrade.
        "linearize" => |p| p.optimized != "rewritten-linearized",

        // Despeckling and border removal apply to any page whose raster Byblos actually
        // handled. Every prefix ends in "-" so that a future capability whose name merely
        // starts with one of these words does not match.
        "page-cleanup" => |p| {
            any_page_applied_prefix(
                p,
                &["jbig2-", "ccitt-", "quantize-", "downsample-", "jpeg-"],
            )
        },

        // PDF/A conformance is a property of the whole file, so any document can be converted.
        "pdfa" => always,

        _ => return None,
    };
    Some(rule)
}

/// `capabilityIssue`: the tracked issue for every capability this build does NOT have. It is
/// the other half of the register: [`capability_rule`] says what gaining a capability would
/// change, and this says where the work lives. `NotImplemented::issue` reads it.
pub fn capability_issue(c: &str) -> Option<&'static str> {
    Some(match c {
        "ccitt-g4" => "byb-v41",
        "decode-jbig2" => "byb-mk8",
        "decode-jpx" => "byb-9bn",
        "decode-tiff" => "byb-2bx",
        "jbig2-symbol" => "byb-v66",
        "page-cleanup" => "byb-09i",
        "pdfa" => "byb-90q",
        "render" => "byb-8b9",
        _ => return None,
    })
}

fn never(_: &Provenance) -> bool {
    false
}

fn always(_: &Provenance) -> bool {
    true
}

fn pages_have(p: &Provenance, f: impl Fn(&PageProvenance) -> bool) -> bool {
    p.pages.iter().flatten().any(f)
}

/// `anyPageApplied`.
fn any_page_applied(p: &Provenance, want: &str) -> bool {
    pages_have(p, |pg| {
        pg.applied.iter().any(|a| applied_capability(a) == want)
    })
}

/// `anyPageAppliedPrefix`.
fn any_page_applied_prefix(p: &Provenance, prefixes: &[&str]) -> bool {
    pages_have(p, |pg| {
        pg.applied
            .iter()
            .any(|a| prefixes.iter().any(|pre| a.starts_with(pre)))
    })
}

/// `anyPageDiverted`.
fn any_page_diverted(p: &Provenance, reasons: &[&str]) -> bool {
    pages_have(p, |pg| reasons.contains(&pg.diverted.as_str()))
}

/// `appliedCapability`: strips a trailing numeric parameter from an `applied` entry:
/// `downsample-150` is the capability `downsample` applied at 150 DPI, while `jbig2-generic` is
/// a capability name in its own right. Only an all-digit final segment is treated as a
/// parameter.
pub fn applied_capability(s: &str) -> &str {
    let Some(i) = s.rfind('-') else {
        return s;
    };
    let tail = s.get(i + 1..).unwrap_or("");
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return s;
    }
    s.get(..i).unwrap_or(s)
}

/// `UpgradeCandidates`: the capabilities in `current`, sorted and without repeats, that the
/// document described by `p` does not already have AND that would actually change its output.
/// An empty result means re-processing the document is wasted work.
///
/// A capability with no rule is reported (see [`capability_rule`]). `None` means nothing is
/// known about the document (Go's nil `p`), so every capability is a candidate. docs/scope.md
/// gives `p: &Provenance`; the `Option` is the nil case, which Go tests.
pub fn upgrade_candidates(p: Option<&Provenance>, current: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|c| match p {
            None => true,
            Some(p) => {
                !p.capabilities.iter().flatten().any(|have| have == *c)
                    && capability_rule(c).is_none_or(|rule| rule(p))
            }
        })
        .map(|c| (*c).to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::capabilities;

    fn page(applied: &[&str], diverted: &str) -> PageProvenance {
        PageProvenance {
            applied: applied.iter().map(|s| (*s).to_string()).collect(),
            diverted: diverted.to_string(),
            ..PageProvenance::default()
        }
    }

    fn prov(caps: &[&str], pages: Vec<PageProvenance>) -> Provenance {
        Provenance {
            capabilities: Some(caps.iter().map(|s| (*s).to_string()).collect()),
            pages: Some(pages),
            ..Provenance::default()
        }
    }

    struct Case {
        name: &'static str,
        prov: Option<Provenance>,
        current: &'static [&'static str],
        want: &'static [&'static str],
    }

    #[test]
    fn upgrade_candidates_table() {
        let mut dropped = page(&["extract-raster"], "");
        dropped.dropped_annots = 1;
        const DECODERS: &[&str] = &[
            "decode-jbig2",
            "decode-jpx",
            "decode-tiff",
            "extract-raster",
            "inspect",
            "render",
        ];
        const DECODERS_NO_RENDER: &[&str] = &[
            "decode-jbig2",
            "decode-jpx",
            "decode-tiff",
            "extract-raster",
            "inspect",
        ];
        let er = &["extract-raster", "inspect"];
        let cases = vec![
            // The three cases from design spec section 6.
            Case {
                name: "applied jbig2-generic, new jbig2-symbol: yes, smaller output",
                prov: Some(prov(
                    &["inspect", "jbig2-generic"],
                    vec![page(&["jbig2-generic"], "")],
                )),
                current: &["inspect", "jbig2-generic", "jbig2-symbol"],
                want: &["jbig2-symbol"],
            },
            Case {
                name: "diverted not-single-raster, new render: yes, now processable",
                prov: Some(prov(er, vec![page(&[], "not-single-raster")])),
                current: &["extract-raster", "inspect", "render"],
                want: &["render"],
            },
            // byb-b5.1: a page whose stored raster does not cover its page box is NOT, on its
            // own, a render candidate. (The Go case also sets PageGeometry; no rule reads it.)
            Case {
                name: "partial placement alone is not a render candidate",
                prov: Some(prov(er, vec![page(&["extract-raster"], "")])),
                current: &["extract-raster", "inspect", "render"],
                want: &[],
            },
            // byb-b5.1: DroppedAnnots > 0 DOES make render a candidate.
            Case {
                name: "dropped annotations make render a candidate",
                prov: Some(prov(er, vec![dropped])),
                current: &["extract-raster", "inspect", "render"],
                want: &["render"],
            },
            Case {
                name: "applied jpeg-recompress only, new jbig2-symbol: no bitonal content",
                prov: Some(prov(
                    &["inspect", "jpeg-recompress"],
                    vec![page(&["jpeg-recompress"], "")],
                )),
                current: &["inspect", "jbig2-symbol", "jpeg-recompress"],
                want: &[],
            },
            // Boundary behaviour.
            Case {
                name: "no capability gap at all",
                prov: Some(prov(
                    &["inspect", "jbig2-generic"],
                    vec![page(&["jbig2-generic"], "")],
                )),
                current: &["inspect", "jbig2-generic"],
                want: &[],
            },
            Case {
                name: "numeric parameter suffixes are stripped before matching",
                prov: Some(prov(
                    &["downsample", "inspect", "jpeg-recompress"],
                    vec![page(&["downsample-150", "jpeg-recompress"], "")],
                )),
                current: &["downsample", "inspect", "jpeg-recompress", "page-cleanup"],
                want: &["page-cleanup"],
            },
            Case {
                name: "ccitt-g4 is a compatibility fallback, never an improvement",
                prov: Some(prov(
                    &["inspect", "jbig2-generic"],
                    vec![page(&["jbig2-generic"], "")],
                )),
                current: &["ccitt-g4", "inspect", "jbig2-generic"],
                want: &[],
            },
            Case {
                name: "jbig2-generic improves a document that only got ccitt-g4",
                prov: Some(prov(
                    &["ccitt-g4", "inspect"],
                    vec![page(&["ccitt-g4"], "")],
                )),
                current: &["ccitt-g4", "inspect", "jbig2-generic"],
                want: &["jbig2-generic"],
            },
            // A codec divert is a decoder problem, not a renderer problem (byb-97q).
            Case {
                name: "unsupported-codec is NOT a render candidate",
                prov: Some(prov(er, vec![page(&[], "unsupported-codec")])),
                current: &["extract-raster", "inspect", "render"],
                want: &[],
            },
            Case {
                name: "unsupported-codec nominates every decoder",
                prov: Some(prov(er, vec![page(&[], "unsupported-codec")])),
                current: DECODERS,
                want: &["decode-jbig2", "decode-jpx", "decode-tiff"],
            },
            Case {
                name: "not-single-raster nominates the renderer and no decoder",
                prov: Some(prov(er, vec![page(&[], "not-single-raster")])),
                current: DECODERS,
                want: &["render"],
            },
            Case {
                name: "a document carrying both divert classes wants both",
                prov: Some(prov(
                    er,
                    vec![
                        page(&[], "not-single-raster"),
                        page(&[], "unsupported-codec"),
                    ],
                )),
                current: DECODERS,
                want: &["decode-jbig2", "decode-jpx", "decode-tiff", "render"],
            },
            // byb-z8j: a codec-specific class narrows which decoder is wanted.
            Case {
                name: "jbig2-diverted page wants decode-jbig2 and not decode-jpx or decode-tiff",
                prov: Some(prov(er, vec![page(&[], "unsupported-codec-jbig2")])),
                current: DECODERS_NO_RENDER,
                want: &["decode-jbig2"],
            },
            Case {
                name: "jpx-diverted page wants only decode-jpx",
                prov: Some(prov(er, vec![page(&[], "unsupported-codec-jpx")])),
                current: DECODERS_NO_RENDER,
                want: &["decode-jpx"],
            },
            Case {
                name: "tiff-diverted page wants only decode-tiff",
                prov: Some(prov(er, vec![page(&[], "unsupported-codec-tiff")])),
                current: DECODERS_NO_RENDER,
                want: &["decode-tiff"],
            },
            Case {
                name: "a fully processed document wants no decoder",
                prov: Some(prov(er, vec![page(&["jbig2-generic"], "")])),
                current: DECODERS_NO_RENDER,
                want: &[],
            },
            Case {
                name: "only the diverted page matters, not the handled ones",
                prov: Some(prov(
                    &["extract-raster", "inspect", "jbig2-generic"],
                    vec![page(&["jbig2-generic"], ""), page(&[], "not-single-raster")],
                )),
                current: &["extract-raster", "inspect", "jbig2-generic", "render"],
                want: &["render"],
            },
            Case {
                name: "an unknown new capability is reported: better a wasted re-run than a missed upgrade",
                prov: Some(prov(&["inspect"], vec![page(&["jbig2-generic"], "")])),
                current: &["inspect", "some-future-thing"],
                want: &["some-future-thing"],
            },
            Case {
                name: "nil provenance: nothing is known, so everything is a candidate",
                prov: None,
                current: &["inspect", "render"],
                want: &["inspect", "render"],
            },
            Case {
                name: "results are sorted and deduplicated",
                prov: Some(prov(&[], vec![page(&[], "not-single-raster")])),
                current: &["render", "pdfa", "render"],
                want: &["pdfa", "render"],
            },
        ];
        for tc in &cases {
            let got = upgrade_candidates(tc.prov.as_ref(), tc.current);
            assert_eq!(got, tc.want, "{}", tc.name);
        }
    }

    /// A capability with no rule is silently treated as "always improves", which would make
    /// the reprocess job scan the whole archive. Catch it here instead.
    #[test]
    fn every_capability_has_a_rule() {
        for c in capabilities() {
            assert!(
                capability_rule(c).is_some(),
                "capability {c:?} has no rule in capability_rule"
            );
        }
    }

    #[test]
    fn linearize_reads_optimized() {
        let mut p = prov(&[], vec![]);
        assert_eq!(upgrade_candidates(Some(&p), &["linearize"]), ["linearize"]);
        p.optimized = "rewritten-linearized".to_string();
        assert!(upgrade_candidates(Some(&p), &["linearize"]).is_empty());
    }

    #[test]
    fn applied_capability_table() {
        for (input, want) in [
            ("downsample-150", "downsample"),
            ("jbig2-generic", "jbig2-generic"),
            ("jpeg-recompress", "jpeg-recompress"),
            ("quantize-png-64", "quantize-png"),
            ("trailing-", "trailing-"),
            ("noseparator", "noseparator"),
        ] {
            assert_eq!(applied_capability(input), want, "{input}");
        }
    }
}
