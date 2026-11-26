//! Annotations as a page declares them (Go: internal/pdfdoc/annots.go).
//!
//! Like `ImageInfo`, this records declarations rather than content. Whether the annotation's
//! appearance stream actually deposits ink where `/Rect` says it does needs a renderer, and
//! design spec section 2 puts that out of scope; for deciding whether byblos is dropping
//! something a viewer would show, "it declares a normal appearance and a box" is the whole
//! answer.
//!
//! Assumes `crate::pdfdoc::{Doc, Rect}` (the port of `doc` and `Rect` of pdfdoc.go): `Doc` has
//! the parsed document in its `ctx: pdf::model::Context` field, `Rect` has `llx`, `lly`, `urx`
//! and `ury`.

use super::{Doc, Rect};
use crate::error::{Error, Result};
use pdf::model::XRefTable;
use pdf::object::{Dict, Object};
use std::sync::atomic::AtomicBool;

/// One annotation as its own dictionary declares it, in the same default user space as the
/// page's `MediaBox`: points, origin lower-left.
#[derive(Debug, Clone, PartialEq)]
pub struct Annot {
    /// `/Subtype`; empty when absent.
    pub subtype: String,
    /// `/Rect`, normalised so `llx`, `lly` really are the minimum corner.
    pub rect: Rect,
    /// A well-formed 4-number `/Rect` was present.
    pub has_rect: bool,
    /// `/F`; ISO 32000-1 Table 165. 0 when absent.
    pub flags: i64,
    /// `/AP` present.
    pub has_ap: bool,
    /// `/AP /N` resolves to a stream, or `/AS` selects one that does.
    pub has_apn: bool,
    /// `/OC` present: visibility depends on optional-content state.
    pub has_oc: bool,
}

/// The subtypes a viewer is expected to draw from the dictionary when `/AP` is missing. Link is
/// absent on purpose: its visible border is legacy behaviour that modern viewers suppress.
const VIEWER_SYNTHESISES: [&str; 13] = [
    "Square",
    "Circle",
    "Line",
    "Polygon",
    "PolyLine",
    "Ink",
    "FreeText",
    "Text",
    "Highlight",
    "Underline",
    "StrikeOut",
    "Squiggly",
    "Caret",
];

impl Annot {
    /// Whether this annotation puts marks on the page.
    ///
    /// It is deliberately conservative in one direction only: every reason to say no is a fact
    /// in the dictionary, and anything unrecognised counts as painting. Over-counting shows up
    /// as a page needlessly flagged; under-counting is the silent data loss this measurement
    /// exists to find.
    pub fn paints(&self) -> bool {
        self.reason().is_empty()
    }

    /// Names why the annotation deposits nothing, or `""` when it paints.
    pub fn reason(&self) -> &'static str {
        if self.subtype.is_empty() {
            "no-subtype"
        } else if self.subtype == "Popup" {
            // A /Popup is the note window of its parent markup annotation. It is never drawn
            // as part of the page.
            "popup"
        } else if self.flags & 2 != 0 {
            // ISO 32000-1 Table 165. Bit 2 (value 2) Hidden: do not render at all.
            "hidden"
        } else if self.flags & 32 != 0 {
            // Bit 6 (value 32) NoView: rendered when printing, never on screen. It still
            // belongs in an archival raster, so it is separated rather than dismissed.
            "noview-print-only"
        } else if !self.has_rect {
            "no-rect"
        } else if self.rect.urx <= self.rect.llx || self.rect.ury <= self.rect.lly {
            "zero-area-rect"
        } else if self.has_apn {
            "" // paints
        } else if self.has_ap {
            "ap-without-n"
        } else if VIEWER_SYNTHESISES.contains(&self.subtype.as_str()) {
            // Without an appearance stream a viewer may synthesise one from the annotation's
            // own properties. Whether it does is a viewer decision, so these are counted
            // apart from the certain cases.
            "no-ap-viewer-synthesised"
        } else {
            "no-ap"
        }
    }
}

/// `deref`: the object `o` is or refers to, as an owned copy; an error or a missing entry is
/// the null object (the Go helper returns nil for both).
pub(super) fn deref_obj(xt: &mut XRefTable, o: Option<&Object>) -> Object {
    match o {
        None => Object::Null,
        Some(o) => xt.dereference(o).cloned().unwrap_or(Object::Null),
    }
}

/// `dict[key]`: the raw entry, without a dereference.
fn raw<'a>(dict: &'a Dict, key: &[u8]) -> Option<&'a Object> {
    dict.0.get(key)
}

/// `arrayEntry`: reads through an indirect reference, which `Dict::array_entry` does not.
fn array_entry(xt: &mut XRefTable, dict: &Dict, key: &[u8]) -> Vec<Object> {
    match deref_obj(xt, raw(dict, key)) {
        Object::Array(a) => a,
        _ => Vec::new(),
    }
}

/// `intEntry`.
fn int_entry(xt: &mut XRefTable, dict: &Dict, key: &[u8]) -> i64 {
    match deref_obj(xt, raw(dict, key)) {
        Object::Integer(v) => v,
        _ => 0,
    }
}

/// `nameEntry`: a `/Name` through an indirect reference, which `Dict::name_entry` does not
/// follow. Empty when absent or not a name.
fn name_entry(xt: &mut XRefTable, dict: &Dict, key: &[u8]) -> Vec<u8> {
    match deref_obj(xt, raw(dict, key)) {
        Object::Name(v) => v,
        _ => Vec::new(),
    }
}

/// `dictEntry`.
fn dict_entry(xt: &mut XRefTable, dict: &Dict, key: &[u8]) -> Option<Dict> {
    match deref_obj(xt, raw(dict, key)) {
        Object::Dict(d) => Some(d),
        _ => None,
    }
}

/// `rectFromArray`: reads four numbers and normalises them.
///
/// ISO 32000-1 7.9.5: a rectangle may be written with its corners in either order, so a legal
/// `/Rect` can arrive with LL above UR. An un-normalised read reports negative area, which
/// `zero-area-rect` would then miscount as painting nothing. A short array is `None` (Go's
/// `RectForArray` indexes without a length check).
fn rect_from_array(xt: &mut XRefTable, a: &[Object]) -> Option<Rect> {
    let mut v = [0f64; 4];
    for (i, slot) in v.iter_mut().enumerate() {
        match deref_obj(xt, a.get(i)) {
            Object::Float(f) => *slot = f,
            Object::Integer(n) => *slot = n as f64,
            _ => return None,
        }
    }
    Some(Rect {
        llx: v[0].min(v[2]),
        lly: v[1].min(v[3]),
        urx: v[0].max(v[2]),
        ury: v[1].max(v[3]),
    })
}

impl Doc {
    /// Returns the annotations declared by page `n` (1-based).
    ///
    /// `/Annots` is NOT inheritable: ISO 32000-1 Table 30 lists only `/Resources`, `/MediaBox`,
    /// `/CropBox` and `/Rotate`, so the page's own dictionary is the whole answer.
    ///
    /// This is deliberately not a field on `Page`: `extract_page_raster` and the inspection
    /// each read the page, so filling it there would dereference every annotation twice on
    /// every extract, to serve a measurement.
    ///
    /// # Errors
    /// `n` outside `1..=page_count`, and the page tree errors of `page_dict`.
    pub fn annots(&mut self, n: usize) -> Result<Vec<Annot>> {
        let xt = &mut self.ctx.xref;
        let count = xt.page_count;
        let page = i64::try_from(n).unwrap_or(i64::MAX);
        if page < 1 || page > count {
            return Err(Error::Malformed(format!(
                "page {n} out of range 1..{count}"
            )));
        }
        // The Go call passes context.Background() and consolidateRes=false (byb-ged): the
        // inherited attributes are discarded here, so consolidation would only buy a content
        // parse whose result is thrown away. pdf-rs's `page_dict` never consolidates.
        let never = AtomicBool::new(false);
        let (pd, _, _) = xt.page_dict(page, &never)?;
        // A page whose /Annots is a reference is legal and common.
        let arr = array_entry(xt, &pd, b"Annots");
        if arr.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        out.try_reserve_exact(arr.len())?;
        for o in &arr {
            // An entry naming an object that is free in the xref dereferences to null, so
            // presence has to be checked, not just the error.
            let Object::Dict(ad) = deref_obj(xt, Some(o)) else {
                continue;
            };
            let subtype = name_entry(xt, &ad, b"Subtype");
            let mut a = Annot {
                subtype: String::from_utf8_lossy(&subtype).into_owned(),
                rect: Rect {
                    llx: 0.0,
                    lly: 0.0,
                    urx: 0.0,
                    ury: 0.0,
                },
                has_rect: false,
                flags: int_entry(xt, &ad, b"F"),
                has_ap: false,
                has_apn: false,
                has_oc: ad.find(b"OC").is_some(),
            };

            let ra = array_entry(xt, &ad, b"Rect");
            if ra.len() == 4
                && let Some(r) = rect_from_array(xt, &ra)
            {
                a.rect = r;
                a.has_rect = true;
            }
            if let Some(ap) = dict_entry(xt, &ad, b"AP") {
                a.has_ap = true;
                // /N is either the appearance stream itself or a sub-dictionary of named
                // states, in which case /AS says which one is current.
                match deref_obj(xt, raw(&ap, b"N")) {
                    Object::Stream(_) => a.has_apn = true,
                    Object::Dict(v) => {
                        let state = name_entry(xt, &ad, b"AS");
                        if !state.is_empty() {
                            a.has_apn = matches!(deref_obj(xt, raw(&v, &state)), Object::Stream(_));
                        }
                    }
                    _ => {}
                }
            }
            out.push(a);
        }
        Ok(out)
    }
}

/// `buildPDF` of the Go tests: objects 1..len(bodies) with a correct xref table. A body may
/// contain "stream"/"endstream"; its `/Length` must already be right.
#[cfg(test)]
pub(super) fn build_pdf(bodies: &[String]) -> Vec<u8> {
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
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        bodies.len() + 1,
        start
    ));
    buf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    // The corpus documents carry one annotation and it is a hidden, zero-area /Square with no
    // /AP, so they exercise none of the reads below. These build PDFs literally instead: every
    // case here is a way a dictionary read can be wrong, and most of them fail in the direction
    // that turns an annotation which paints into one that does not, which is silent data loss
    // rather than a visible error.

    /// A one-page document whose /Annots entry is `annots_entry` and whose extra objects start
    /// at object 5. The page is 612x792 with no content.
    fn annot_pdf(annots_entry: &str, extra: &[&str]) -> Vec<u8> {
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources 4 0 R /Annots {annots_entry} >>"
            ),
            "<< >>".to_string(),
        ];
        objs.extend(extra.iter().map(|s| s.to_string()));
        build_pdf(&objs)
    }

    /// A minimal appearance stream: a form XObject painting nothing.
    const FORM_STREAM: &str =
        "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] /Length 0 >>\nstream\n\nendstream";

    fn annots_of(data: &[u8]) -> Vec<Annot> {
        let mut d = Doc::open_limits(data, &Limits::default(), &AtomicBool::new(false)).unwrap();
        d.annots(1).unwrap()
    }

    #[test]
    fn reads_an_indirect_array() {
        let data = annot_pdf(
            "5 0 R",
            &[
                "[6 0 R]",
                "<< /Type /Annot /Subtype /Stamp /Rect [10 10 100 100] /AP << /N 7 0 R >> >>",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert_eq!(got.len(), 1);
        assert!(got[0].has_apn);
        assert!(got[0].paints(), "{}", got[0].reason());
    }

    #[test]
    fn reads_an_indirect_flags_entry() {
        let data = annot_pdf(
            "[5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Stamp /Rect [10 10 100 100] /F 6 0 R /AP << /N 7 0 R >> >>",
                "2",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].flags, 2);
        assert_eq!(got[0].reason(), "hidden");
    }

    #[test]
    fn skips_a_dangling_reference() {
        let data = annot_pdf(
            "[99 0 R 5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Stamp /Rect [10 10 100 100] /AP << /N 6 0 R >> >>",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert_eq!(got.len(), 1);
        assert!(got[0].paints(), "{}", got[0].reason());
    }

    #[test]
    fn malformed_rect_does_not_panic() {
        for rect in ["[10 10]", "[]", "[10 10 100]", "(not an array)"] {
            let dict =
                format!("<< /Type /Annot /Subtype /Stamp /Rect {rect} /AP << /N 6 0 R >> >>");
            let data = annot_pdf("[5 0 R]", &[dict.as_str(), FORM_STREAM]);
            let got = annots_of(&data);
            assert_eq!(got.len(), 1, "{rect}");
            assert!(!got[0].has_rect, "{rect}");
            assert_eq!(got[0].reason(), "no-rect", "{rect}");
        }
    }

    #[test]
    fn normalises_a_reversed_rect() {
        let data = annot_pdf(
            "[5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Stamp /Rect [100 200 10 20] /AP << /N 6 0 R >> >>",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        let want = Rect {
            llx: 10.0,
            lly: 20.0,
            urx: 100.0,
            ury: 200.0,
        };
        assert_eq!(got[0].rect, want);
        assert!(got[0].paints(), "{}", got[0].reason());
    }

    #[test]
    fn resolves_an_appearance_state() {
        let data = annot_pdf(
            "[5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Widget /Rect [10 10 100 100] /AS /On /AP << /N << /On 6 0 R /Off 7 0 R >> >> >>",
                FORM_STREAM,
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert!(got[0].has_apn);
        assert!(got[0].paints(), "{}", got[0].reason());
    }

    #[test]
    fn appearance_state_missing() {
        let data = annot_pdf(
            "[5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Widget /Rect [10 10 100 100] /AS /Missing /AP << /N << /On 6 0 R >> >> >>",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert!(!got[0].has_apn);
        assert_eq!(got[0].reason(), "ap-without-n");
    }

    // The ladder decides what counts as ink. Every entry that is not "" is a fact in the
    // dictionary saying a viewer draws nothing.
    #[test]
    fn reason_ladder() {
        let rect = "/Rect [10 10 100 100]";
        let ap = "/AP << /N 6 0 R >>";
        let cases = [
            (
                "paints",
                format!("<< /Type /Annot /Subtype /Stamp {rect} {ap} >>"),
                "",
            ),
            (
                "no subtype",
                format!("<< /Type /Annot {rect} {ap} >>"),
                "no-subtype",
            ),
            (
                "popup",
                format!("<< /Type /Annot /Subtype /Popup {rect} {ap} >>"),
                "popup",
            ),
            (
                "hidden",
                format!("<< /Type /Annot /Subtype /Stamp /F 2 {rect} {ap} >>"),
                "hidden",
            ),
            (
                "noview",
                format!("<< /Type /Annot /Subtype /Stamp /F 32 {rect} {ap} >>"),
                "noview-print-only",
            ),
            (
                "zero area",
                format!("<< /Type /Annot /Subtype /Stamp /Rect [0 0 0 0] {ap} >>"),
                "zero-area-rect",
            ),
            (
                "no ap",
                format!("<< /Type /Annot /Subtype /Stamp {rect} >>"),
                "no-ap",
            ),
            (
                "viewer synthesised",
                format!("<< /Type /Annot /Subtype /Square {rect} >>"),
                "no-ap-viewer-synthesised",
            ),
            // Hidden outranks a present appearance: the flag is the viewer's instruction and
            // the stream is only what it would have drawn.
            (
                "hidden beats ap",
                format!("<< /Type /Annot /Subtype /Stamp /F 2 {rect} {ap} >>"),
                "hidden",
            ),
            // Bit 1 Invisible is scoped by Table 165 to annotations with no handler, so on a
            // /Stamp it says nothing and must not suppress the ink.
            (
                "invisible bit ignored",
                format!("<< /Type /Annot /Subtype /Stamp /F 1 {rect} {ap} >>"),
                "",
            ),
        ];
        for (name, dict, want) in cases {
            let got = annots_of(&annot_pdf("[5 0 R]", &[dict.as_str(), FORM_STREAM]));
            assert_eq!(got.len(), 1, "{name}");
            assert_eq!(got[0].reason(), want, "{name}");
            assert_eq!(got[0].paints(), want.is_empty(), "{name}");
        }
    }

    // /Annots is not inheritable: ISO 32000-1 Table 30 lists only /Resources, /MediaBox,
    // /CropBox and /Rotate.
    #[test]
    fn are_not_inherited() {
        let data = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /Annots [4 0 R] >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string(),
            "<< /Type /Annot /Subtype /Stamp /Rect [10 10 100 100] /AP << /N 5 0 R >> >>"
                .to_string(),
            FORM_STREAM.to_string(),
        ]);
        assert!(annots_of(&data).is_empty());
    }

    #[test]
    fn page_without() {
        let data = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string(),
        ]);
        assert!(annots_of(&data).is_empty());
    }

    #[test]
    fn page_out_of_range() {
        let data = annot_pdf("[]", &[]);
        let mut d = Doc::open_limits(&data, &Limits::default(), &AtomicBool::new(false)).unwrap();
        for n in [0, 2] {
            let err = d.annots(n).unwrap_err();
            assert!(err.to_string().contains("out of range"), "{n}: {err}");
        }
    }

    // The byb-b1.3 shape: a stamp sitting in the blank strip a natural-DPI raster leaves.
    #[test]
    fn stamp_in_the_natural_dpi_strip() {
        let data = annot_pdf(
            "[5 0 R]",
            &[
                "<< /Type /Annot /Subtype /Stamp /Rect [575 100 605 200] /AP << /N 6 0 R >> >>",
                FORM_STREAM,
            ],
        );
        let got = annots_of(&data);
        assert!(got[0].paints(), "{}", got[0].reason());
        // The raster reaches 568.3708; the annotation starts beyond it and the page runs to 612.
        const RASTER_URX: f64 = 568.3708;
        assert!(got[0].rect.llx > RASTER_URX);
        assert!(got[0].rect.urx <= 612.0);
    }
}
