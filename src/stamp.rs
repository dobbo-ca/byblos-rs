//! Wave 3: port of byblos `stamp.go`.
//!
//! [`stamp_text_layer`] writes an invisible OCR-style text layer into a PDF: PDF text rendering
//! mode 3 (invisible), through [`crate::glyphless`]'s empty-outline font, so a reader can select
//! and search text that paints nothing (design spec section 7).
//!
//! Go's `StampTextLayer`, `StampTextLayerContext` and `StampTextLayerLimits` are this one
//! function: it takes `&Limits` and a cancel flag, reads the document from a byte slice and
//! returns the stamped document. A cancelled call returns no bytes.
//!
//! # Differences from Go
//!
//! * Go wraps each error with `byblos: stamp text layer: page N word M: `. A typed error (limit,
//!   alloc, cancel, [`Error::UnstampableRune`], a pdf-rs error) is returned as it is, so its
//!   class survives; the two plain errors Go builds here keep the full Go text.
//! * `cancel` is also passed to the open, the page tail and the write, which Go runs without
//!   a context. A cancel there stops sooner; it does not change the output.
//! * `PositionedWord.text` is a `String`, so Go's invalid UTF-8 (a U+FFFD rune, never stampable)
//!   cannot occur.
//! * The words and the content stream of a page are charged to the document's `max_alloc`
//!   ledger before they are allocated.

use crate::error::{Error, Result};
use crate::glyphless;
use crate::go::fmt::format_f;
use crate::image::Rect;
use crate::limits::{Limits, check_cancel};
use crate::optimize::write_env;
use crate::pdfdoc::{self, text::TrueTypeFont};
use pdf::GoTime;
use std::sync::atomic::AtomicBool;

/// `PositionedWord`: one recognized word and where it sits on the page.
///
/// `bounds` is in POINTS, in PDF default user space (origin lower-left, y increasing upward: the
/// same convention as `PageInfo.Bounds` and `PageRaster.Bounds`), with the byb-b1.2 residual
/// affine ALREADY APPLIED and `/Rotate` NOT applied. [`stamp_text_layer`] places text at
/// `bounds` directly; it applies no transform of its own.
///
/// * Points, not the raster's pixel space, so a caller reading `PageRaster.Bounds` and a caller
///   writing `PositionedWord.bounds` speak the same unit.
/// * The affine must already be applied: at the measured max 1.09 degree scanner deskew that is
///   about 11.6 pt of drift across a 612 pt page. [`stamp_text_layer`] cannot check this.
/// * `/Rotate` is NOT applied, and must not be: it is a display attribute that already rotates
///   stamped text along with the page's own content, so applying it here would rotate it twice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PositionedWord {
    pub text: String,
    pub bounds: Rect,
}

/// `TextLayer`: the recognized text for one document, one page at a time. `pages[i]` is page
/// `i+1`'s words; a page with no recognized text is an empty vector, and [`stamp_text_layer`]
/// does not touch such a page at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextLayer {
    pub pages: Vec<Vec<PositionedWord>>,
}

/// `glyphlessBaseFont` names the embedded font object. Every page shares one font object, keyed
/// by this name (`Doc::add_font_resource` memoizes on `TrueTypeFont::base_font`).
const GLYPHLESS_BASE_FONT: &str = "BbyblosGlyphless";

/// `glyphlessTrueTypeFont`: the [`TrueTypeFont`] value for [`glyphless::FONT`].
///
/// `/Ascent 800` and `/Descent -200` are not free parameters: they sum to
/// [`glyphless::UNITS_PER_EM`], so the em box spans `[baseline-0.2*fs, baseline+0.8*fs]`, exactly
/// `[bounds.y0, bounds.y1]` for the `ty` [`prepare_word`] computes. Changing either value
/// independently breaks that identity.
pub fn glyphless_true_type_font() -> TrueTypeFont {
    let widths = (glyphless::FIRST_RUNE..=glyphless::LAST_RUNE)
        // Every rune in range is covered.
        .map(|r| glyphless::width(r).map_or(0, i64::from))
        .collect();
    TrueTypeFont {
        base_font: GLYPHLESS_BASE_FONT.to_string(),
        program: glyphless::FONT.to_vec(),
        first_char: i64::from(u32::from(glyphless::FIRST_RUNE)),
        widths,
        // Bit 6, Nonsymbolic: required alongside /Encoding /WinAnsiEncoding.
        flags: 32,
        font_bbox: [0, 0, 0, 0],
        italic_angle: 0,
        ascent: 800,
        descent: -200,
        cap_height: 700,
        stem_v: 80,
    }
}

/// `preparedWord`: a word's placement, computed once and independent of the font resource's
/// name on the page it lands on.
struct PreparedWord<'a> {
    text: &'a str,
    fs: f64,
    tz: f64,
    tx: f64,
    ty: f64,
}

/// `image.Rectangle.String`.
fn rect_string(b: &Rect) -> String {
    format!("({},{})-({},{})", b.x0, b.y0, b.x1, b.y1)
}

/// `prepareWord`: validates `w` and computes its placement. `Ok(None)` for an empty word
/// (nothing to stamp), [`Error::Image`] for a non-positive box and
/// [`Error::UnstampableRune`] for a rune outside the font: silently dropping recognized text is
/// the failure class this port rejects.
fn prepare_word(w: &PositionedWord) -> Result<Option<PreparedWord<'_>>> {
    if w.text.is_empty() {
        return Ok(None);
    }
    let b = &w.bounds;
    // Go's Dx/Dy wrap on overflow.
    let dx = b.x1.wrapping_sub(b.x0);
    let dy = b.y1.wrapping_sub(b.y0);
    if dx <= 0 || dy <= 0 {
        return Err(Error::Image(format!(
            "box {} is not positive",
            rect_string(b)
        )));
    }
    let fs = dy as f64;
    // The natural advance, in font units (1000ths of an em).
    let mut nat_fu = 0.0f64;
    for r in w.text.chars() {
        let wd = glyphless::width(r).ok_or(Error::UnstampableRune(r))?;
        nat_fu += f64::from(wd);
    }
    Ok(Some(PreparedWord {
        text: &w.text,
        fs,
        // tz scales the natural advance (nat_fu/1000 * fs points) to the box width; legal
        // outside 0..100 and not clamped, same as poppler and the rest of the PDF ecosystem
        // treat it.
        tz: 100.0 * dx as f64 * 1000.0 / (nat_fu * fs),
        tx: b.x0 as f64,
        // 0.2*fs is -Descent/UnitsPerEm; see glyphless_true_type_font.
        ty: b.y0 as f64 + 0.2 * fs,
    }))
}

impl PreparedWord<'_> {
    /// `ops`: the word's Tf/Tz/Tm/Tj operator group, appended to `buf`. Tm stays a pure
    /// translation and all scaling lives in Tf/Tz, so a Tm bug is obvious rather than absorbed
    /// into a scale factor.
    fn ops(&self, font_name: &str, buf: &mut Vec<u8>) {
        let s = format!(
            "/{font_name} {} Tf\n{} Tz\n1 0 0 1 {} {} Tm\n(",
            format_f(self.fs, 4),
            format_f(self.tz, 4),
            format_f(self.tx, 4),
            format_f(self.ty, 4),
        );
        buf.extend_from_slice(s.as_bytes());
        escape_pdf_string(self.text, buf);
        buf.extend_from_slice(b") Tj\n");
    }
}

/// `escapePDFString`: backslash-escapes the three bytes ISO 32000-1 7.3.4.2 requires inside a
/// literal string, appending the result to `out`.
fn escape_pdf_string(s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        if matches!(b, b'\\' | b'(' | b')') {
            out.push(b'\\');
        }
        out.push(b);
    }
}

/// `StampTextLayer`, `StampTextLayerContext` and `StampTextLayerLimits`: writes `tl` into the
/// pages of the PDF `data` as an invisible text-render-mode-3 layer and returns the result.
///
/// A page whose `tl.pages[i]` is empty (including one beyond the end of a shorter `tl.pages`) is
/// not touched at all: no font resource is added and no content is appended. More `tl.pages`
/// than the document has pages is an error.
///
/// `now` pins the clock of the pdf-rs writer fallback (`None`: the system clock); the parity
/// oracle passes 2000-01-01T00:00:00Z, which also selects the parity random source.
///
/// `cancel` is checked on entry, at the top of the page loop and at the top of the word loop. A
/// cancelled call returns no bytes.
///
/// # Errors
/// [`Error::Cancelled`]; the errors of [`pdfdoc::open`]; [`Error::Image`] for a text layer with
/// more pages than the document or a word whose box is not positive;
/// [`Error::UnstampableRune`] for a rune outside printable ASCII; [`Error::Alloc`]; and the
/// errors of [`pdfdoc::Doc::add_font_resource`], [`pdfdoc::Doc::append_content`] and
/// [`pdfdoc::Doc::write`].
pub fn stamp_text_layer(
    data: &[u8],
    tl: &TextLayer,
    now: Option<GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let mut d = pdfdoc::open(data, limits, cancel)?;
    if tl.pages.len() > d.page_count() {
        return Err(Error::Image(format!(
            "byblos: stamp text layer: text layer has {} pages, document has {}",
            tl.pages.len(),
            d.page_count()
        )));
    }
    let mem = d.ctx.xref.mem().clone();

    let font = glyphless_true_type_font();
    for (i, words) in tl.pages.iter().enumerate() {
        check_cancel(cancel)?;
        if words.is_empty() {
            continue;
        }
        let page_num = i + 1;

        // Validate every word before charging, so a bad word wins over a limit error.
        for (wi, word) in words.iter().enumerate() {
            check_cancel(cancel)?;
            prepare_word(word).map_err(|e| match e {
                Error::Image(m) => Error::Image(format!(
                    "byblos: stamp text layer: page {page_num} word {wi}: {m}"
                )),
                other => other,
            })?;
        }

        // The placements, then the content stream (an escaped word is at most twice its text),
        // are held at once.
        let mut est =
            (words.len() as u64).saturating_mul((std::mem::size_of::<PreparedWord>() + 160) as u64);
        for w in words {
            est = est.saturating_add((w.text.len() as u64).saturating_mul(2));
        }
        let _held = mem.charge(est)?;

        let mut prepared: Vec<PreparedWord> = Vec::new();
        prepared.try_reserve_exact(words.len())?;
        for word in words {
            if let Ok(Some(pw)) = prepare_word(word) {
                prepared.push(pw);
            }
        }
        if prepared.is_empty() {
            continue;
        }

        let font_name = d.add_font_resource(page_num, &font, cancel)?;
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(est.try_into().unwrap_or(usize::MAX))?;
        buf.extend_from_slice(b"BT\n3 Tr\n");
        for pw in &prepared {
            pw.ops(&font_name, &mut buf);
        }
        buf.extend_from_slice(b"ET\n");
        d.append_content(page_num, &buf, cancel)?;
    }

    d.write(&write_env(now), cancel)
}

#[cfg(test)]
mod tests {
    //! Ports of `stamp_test.go` that need no external tool. Left out: every test that runs
    //! `pdftotext`, `pdftoppm` or `pdffonts` (`TestStampTextLayerFontLoadsCleanlyInPoppler`,
    //! `...FontIsEmbeddedAccordingToPdffonts`, `TestStampedTextLeavesTheRasterUnchanged`,
    //! `TestStampedWordsReadInGeometricOrder`, `TestStampedWordBoundsRoundTripThroughBBox` and
    //! its non-uniform variant (the tool-free parts of `TestStampTextLayerArrayContents` and
    //! `...UnwindsUnbalancedGraphicsState` are ported), `...CountersTopLevelCTM`, `...CountersTopLevelYFlip`,
    //! `TestStampedTextIsNotMirroredOnARotatedPage`, `TestStampTextLayerIndirectArrayContents`,
    //! `...PreservesInheritedResourcesUsedOnlyByAForm`).
    use super::*;
    use crate::content::{Kind, Lexer};

    fn limits() -> Limits {
        Limits::default()
    }

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// `minimalPDF`: a one-page, 612x792, `/Rotate`-absent PDF whose page content is the given
    /// fragments (one makes `/Contents` a reference, more make it an array). The streams are not
    /// compressed.
    fn minimal_pdf(contents: &[&str]) -> Vec<u8> {
        let n = contents.len();
        let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + i)).collect();
        let entry = if n == 1 {
            refs[0].clone()
        } else {
            format!("[{}]", refs.join(" "))
        };
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> \
                 /Contents {entry} >>"
            ),
        ];
        for c in contents {
            objs.push(format!(
                "<< /Length {} >>\nstream\n{c}\nendstream",
                c.len() + 1
            ));
        }
        let mut b = b"%PDF-1.7\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(b.len());
            b.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let x = b.len();
        let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
        for o in offs {
            tail.push_str(&format!("{o:010} 00000 n \n"));
        }
        tail.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n",
            objs.len() + 1
        ));
        b.extend_from_slice(tail.as_bytes());
        b
    }

    fn stamp(base: &[u8], words: Vec<PositionedWord>) -> Vec<u8> {
        let tl = TextLayer { pages: vec![words] };
        stamp_text_layer(base, &tl, None, &limits(), &never()).unwrap()
    }

    fn word(text: &str, x0: i64, y0: i64, x1: i64, y1: i64) -> PositionedWord {
        PositionedWord {
            text: text.to_string(),
            bounds: Rect::new(x0, y0, x1, y1),
        }
    }

    fn page_content(stamped: &[u8]) -> Vec<u8> {
        let mut d = pdfdoc::open(stamped, &limits(), &never()).unwrap();
        d.page(1).unwrap().content
    }

    /// G: `glyphless_true_type_font().program` is the WHOLE embedded font asset.
    #[test]
    fn font_program_is_the_whole_embedded_font() {
        let f = glyphless_true_type_font();
        assert_eq!(f.program, glyphless::FONT);
        assert_eq!(f.widths.len(), 95);
        assert_eq!(f.ascent - f.descent, i64::from(glyphless::UNITS_PER_EM));
    }

    /// B3: every text-showing operator in the stamped content executes under Tr 3.
    fn check_every_show_is_render_mode_3(base: &[u8]) {
        let stamped = stamp(
            base,
            vec![
                word("Scanned", 72, 700, 149, 712),
                word("twice", 72, 670, 112, 682),
            ],
        );
        let content = page_content(&stamped);
        let mut lex = Lexer::new(&content);
        let (mut tr, mut last, mut saw) = (0i64, 0.0f64, false);
        while let Ok(Some(tok)) = lex.next_token() {
            match tok.kind {
                Kind::Number => last = tok.num,
                Kind::Keyword => match &*tok.text {
                    b"Tr" => tr = last as i64,
                    b"Tj" | b"TJ" | b"'" | b"\"" => {
                        saw = true;
                        assert_eq!(tr, 3, "text-showing operator ran at Tr={tr}");
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        assert!(saw, "stamped content stream shows no text at all");
    }

    #[test]
    fn text_executes_at_render_mode_3() {
        check_every_show_is_render_mode_3(&minimal_pdf(&[""]));
    }

    /// G (byb-9ee): the placement matrix is a pure translation to the baseline that puts the em
    /// box on `bounds`; a flipped d component is caught here, where `pdftotext -bbox` cannot.
    fn check_matrix_does_not_flip_the_y_axis(base: &[u8]) {
        let words = vec![
            word("alpha", 200, 400, 260, 420),
            word("gamma", 72, 96, 112, 108),
        ];
        let stamped = stamp(base, words.clone());
        let content = page_content(&stamped);

        let mut matrices: Vec<(String, [f64; 6])> = Vec::new();
        let (mut nums, mut last_str, mut tm) = (Vec::<f64>::new(), String::new(), None);
        let mut lex = Lexer::new(&content);
        while let Ok(Some(tok)) = lex.next_token() {
            match tok.kind {
                Kind::Number => {
                    nums.push(tok.num);
                    continue;
                }
                Kind::String => last_str = String::from_utf8_lossy(&tok.text).into_owned(),
                Kind::Keyword => match &*tok.text {
                    b"Tm" => {
                        assert!(nums.len() >= 6, "Tm with {} operands, want 6", nums.len());
                        let mut m = [0.0; 6];
                        m.copy_from_slice(&nums[nums.len() - 6..]);
                        tm = Some(m);
                    }
                    b"Tj" => {
                        let m = tm.take().expect("Tj with no Tm before it");
                        matrices.push((last_str.clone(), m));
                    }
                    _ => {}
                },
                _ => {}
            }
            nums.clear();
        }

        let descent_em =
            -(glyphless_true_type_font().descent as f64) / f64::from(glyphless::UNITS_PER_EM);
        for w in &words {
            let (_, m) = matrices
                .iter()
                .find(|(t, _)| *t == w.text)
                .unwrap_or_else(|| panic!("no Tm/Tj pair for {:?}", w.text));
            assert_eq!(m[..4], [1.0, 0.0, 0.0, 1.0], "{:?}: Tm linear part", w.text);
            let fs = w.bounds.dy() as f64;
            let want_tx = w.bounds.x0 as f64;
            let want_ty = w.bounds.y0 as f64 + descent_em * fs;
            assert!(
                (m[4] - want_tx).abs() <= 1e-4,
                "{:?}: tx {} want {want_tx}",
                w.text,
                m[4]
            );
            assert!(
                (m[5] - want_ty).abs() <= 1e-4,
                "{:?}: ty {} want {want_ty}",
                w.text,
                m[5]
            );
        }
    }

    #[test]
    fn matrix_does_not_flip_the_y_axis() {
        check_matrix_does_not_flip_the_y_axis(&minimal_pdf(&[""]));
    }

    fn corpus(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_text_executes_at_render_mode_3() {
        check_every_show_is_render_mode_3(&corpus("scan"));
    }

    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_matrix_does_not_flip_the_y_axis() {
        check_matrix_does_not_flip_the_y_axis(&corpus("scan"));
    }

    /// The pdftotext-free half of `TestStampTextLayerAcrossTheCorpus`: every corpus document
    /// stamps, and `malformed` is refused.
    #[test]
    #[ignore = "needs every tests/data/corpus/<name>.pdf (scan, scan-rotated, ..., malformed) from the Go oracle -gen"]
    fn corpus_stamps_every_document() {
        let tl = TextLayer {
            pages: vec![vec![word("OCR", 30, 30, 60, 42)]],
        };
        let r = stamp_text_layer(&corpus("malformed"), &tl, None, &limits(), &never());
        assert!(r.is_err(), "StampTextLayer(malformed): want an error");
        for name in ["scan", "scan-rotated", "born-digital"] {
            stamp_text_layer(&corpus(name), &tl, None, &limits(), &never()).unwrap();
        }
    }

    /// Stamping a document whose `/Kids` is an indirect reference must reach both pages.
    #[test]
    #[ignore = "needs tests/data/corpus/indirect-kids.pdf from the Go oracle -gen"]
    fn corpus_indirect_kids() {
        let tl = TextLayer {
            pages: vec![
                vec![word("one", 72, 700, 100, 712)],
                vec![word("two", 72, 700, 100, 712)],
            ],
        };
        stamp_text_layer(&corpus("indirect-kids"), &tl, None, &limits(), &never()).unwrap();
    }

    #[test]
    fn array_contents_stamp() {
        let stamped = stamp(
            &minimal_pdf(&["", ""]),
            vec![word("Byblos", 200, 400, 260, 420)],
        );
        assert!(String::from_utf8_lossy(&page_content(&stamped)).contains("(Byblos) Tj"));
    }

    #[test]
    fn unbalanced_graphics_state_is_unwound() {
        let base = minimal_pdf(&["q 1 0 0 1 100 100 cm q 2 0 0 2 0 0 cm\n"]);
        let stamped = stamp(&base, vec![word("Byblos", 200, 400, 260, 420)]);
        let text = String::from_utf8_lossy(&page_content(&stamped)).into_owned();
        // The stamp is wrapped in q ... Q so the unmatched q's of the page cannot move it.
        assert!(text.trim_end().ends_with('Q'));
        assert!(text.contains("(Byblos) Tj"));
    }

    #[test]
    fn prepare_word_places_the_em_box_on_the_bounds() {
        let w = word("OCR", 30, 30, 60, 42);
        let pw = prepare_word(&w).unwrap().unwrap();
        let nat: f64 = "OCR"
            .chars()
            .map(|c| f64::from(glyphless::width(c).unwrap()))
            .sum();
        assert_eq!(pw.fs, 12.0);
        assert_eq!(pw.tx, 30.0);
        assert_eq!(pw.ty, 30.0 + 0.2 * 12.0);
        assert_eq!(pw.tz, 100.0 * 30.0 * 1000.0 / (nat * 12.0));
        let mut buf = Vec::new();
        pw.ops("F", &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("/F 12.0000 Tf\n"));
        assert!(s.ends_with("1 0 0 1 30.0000 32.4000 Tm\n(OCR) Tj\n"));
    }

    #[test]
    fn strings_are_escaped() {
        let mut out = Vec::new();
        escape_pdf_string("a(b)\\c", &mut out);
        assert_eq!(out, b"a\\(b\\)\\\\c");
    }

    #[test]
    fn empty_word_is_skipped_and_empty_page_is_untouched() {
        let base = minimal_pdf(&["0 g"]);
        assert!(prepare_word(&word("", 0, 0, 0, 0)).unwrap().is_none());
        let tl = TextLayer {
            pages: vec![vec![word("", 0, 0, 0, 0)]],
        };
        let out = stamp_text_layer(&base, &tl, None, &limits(), &never()).unwrap();
        assert_eq!(page_content(&out), page_content(&base));
        let tl = TextLayer {
            pages: vec![vec![]],
        };
        let out = stamp_text_layer(&base, &tl, None, &limits(), &never()).unwrap();
        assert_eq!(page_content(&out), page_content(&base));
    }

    #[test]
    fn unstampable_rune_is_refused_not_substituted() {
        let tl = TextLayer {
            pages: vec![vec![word("caf\u{e9}", 10, 10, 50, 22)]],
        };
        let e = stamp_text_layer(&minimal_pdf(&[""]), &tl, None, &limits(), &never()).unwrap_err();
        assert_eq!(e, Error::UnstampableRune('\u{e9}'));
        assert_eq!(e.class(), "unstampable-rune");
    }

    #[test]
    fn a_box_that_is_not_positive_names_the_page_and_word() {
        let tl = TextLayer {
            pages: vec![vec![
                word("ok", 10, 10, 50, 22),
                word("bad", 10, 10, 10, 22),
            ]],
        };
        let e = stamp_text_layer(&minimal_pdf(&[""]), &tl, None, &limits(), &never()).unwrap_err();
        assert!(
            e.to_string()
                .contains("page 1 word 1: box (10,10)-(10,22) is not positive"),
            "{e}"
        );
    }

    #[test]
    fn more_pages_than_the_document_is_an_error() {
        let tl = TextLayer {
            pages: vec![vec![], vec![]],
        };
        let e = stamp_text_layer(&minimal_pdf(&[""]), &tl, None, &limits(), &never()).unwrap_err();
        assert!(
            e.to_string()
                .contains("text layer has 2 pages, document has 1")
        );
    }

    #[test]
    fn a_set_cancel_flag_writes_nothing() {
        let tl = TextLayer {
            pages: vec![vec![word("a", 0, 0, 5, 5)]],
        };
        let cancel = AtomicBool::new(true);
        let e = stamp_text_layer(&minimal_pdf(&[""]), &tl, None, &limits(), &cancel).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }
}
