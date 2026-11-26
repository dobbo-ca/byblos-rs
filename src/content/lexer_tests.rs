//! Ports of `internal/content/lexer_test.go`.

use super::*;

/// Drains a lexer, failing the test on any error.
fn lex(src: &[u8]) -> Vec<Token<'_>> {
    let mut l = Lexer::new(src);
    let mut out = Vec::new();
    loop {
        match l.next_token() {
            Ok(Some(t)) => out.push(t),
            Ok(None) => return out,
            Err(e) => panic!("next_token() error = {e} after {} tokens", out.len()),
        }
    }
}

fn kinds(toks: &[Token<'_>]) -> Vec<Kind> {
    toks.iter().map(|t| t.kind).collect()
}

fn count(toks: &[Token<'_>], k: Kind) -> usize {
    toks.iter().filter(|t| t.kind == k).count()
}

#[test]
fn operator_sequence() {
    let got = lex(b"q 612 0 0 792 0 0 cm /Im0 Do Q");
    let want = [
        (Kind::Keyword, 0.0, "q"),
        (Kind::Number, 612.0, ""),
        (Kind::Number, 0.0, ""),
        (Kind::Number, 0.0, ""),
        (Kind::Number, 792.0, ""),
        (Kind::Number, 0.0, ""),
        (Kind::Number, 0.0, ""),
        (Kind::Keyword, 0.0, "cm"),
        (Kind::Name, 0.0, "Im0"),
        (Kind::Keyword, 0.0, "Do"),
        (Kind::Keyword, 0.0, "Q"),
    ];
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (i, (k, n, t)) in want.iter().enumerate() {
        assert!(
            got[i].kind == *k && got[i].num == *n && got[i].text.as_ref() == t.as_bytes(),
            "token {i} = {:?}",
            got[i]
        );
    }
}

#[test]
fn numbers() {
    let got = lex(b"-3 .5 4. +2 0.000");
    let want = [-3.0, 0.5, 4.0, 2.0, 0.0];
    assert_eq!(got.len(), want.len());
    for (i, w) in want.iter().enumerate() {
        assert!(got[i].kind == Kind::Number && got[i].num == *w, "token {i}");
    }
}

/// Real-world content streams contain malformed reals. Viewers take the longest valid prefix
/// rather than abandoning the page; so do we.
#[test]
fn malformed_real_takes_longest_valid_prefix() {
    let got = lex(b"1.-2 cm");
    assert!(
        got.len() == 2 && got[0].kind == Kind::Number && got[0].num == 1.0,
        "{got:?}"
    );
}

/// A number whose text overflows float64 is a Go range error, so it takes the prefix path too:
/// 400 digits is out of range, and the longest in-range prefix (308 digits) wins.
#[test]
fn out_of_range_real_takes_longest_in_range_prefix() {
    let src = "9".repeat(400);
    let got = lex(src.as_bytes());
    assert_eq!(got.len(), 1);
    assert!(got[0].num.is_finite() && got[0].num > 1e307, "{:?}", got[0]);
}

/// Reference: Go's `parsePrefixFloat`, longest prefix first (quadratic).
fn naive_prefix_float(s: &[u8]) -> f64 {
    (1..=s.len())
        .rev()
        .find_map(|i| s.get(..i).and_then(parse_float))
        .unwrap_or(0.0)
}

#[test]
fn prefix_float_matches_longest_prefix_scan() {
    let mut cases: Vec<Vec<u8>> = [
        "1.-2", "-", "+", ".", "-.", "+.5-1", "5.", "-.5.5", "1-1-1", "..", "0.1+2", "-+1",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    cases.push(format!("{}.5-1", "9".repeat(400)).into_bytes());
    cases.push(format!("-{}", "9".repeat(309)).into_bytes());
    cases.push(format!("{}-", "9".repeat(310)).into_bytes());
    for c in &cases {
        let (got, want) = (parse_prefix_float(c), naive_prefix_float(c));
        assert!(
            got == want,
            "{:?}: got {got}, want {want}",
            String::from_utf8_lossy(c)
        );
    }
}

/// A hostile token must not cost quadratic time (this would not finish with the per-prefix scan).
#[test]
fn long_malformed_number_is_linear() {
    let src = "1-".repeat(1 << 20);
    let got = lex(src.as_bytes());
    assert!(got.len() == 1 && got[0].num == 1.0, "{} tokens", got.len());
}

#[test]
fn literal_string_escapes() {
    let got = lex(br"(a\(b\)c\n\101\\z) Tj");
    assert!(got.len() == 2 && got[0].kind == Kind::String, "{got:?}");
    assert_eq!(got[0].text.as_ref(), b"a(b)c\nA\\z");
}

#[test]
fn literal_string_nested_parens() {
    let got = lex(b"((nested) ok) Tj");
    assert!(
        got.len() == 2 && got[0].text.as_ref() == b"(nested) ok",
        "{got:?}"
    );
}

/// A backslash before a newline is a line continuation and contributes nothing.
#[test]
fn literal_string_line_continuation() {
    let got = lex(b"(ab\\\ncd) Tj");
    assert!(got.len() == 2 && got[0].text.as_ref() == b"abcd", "{got:?}");
}

/// A string without escapes or nesting aliases the source; one with escapes does not.
#[test]
fn literal_string_alias() {
    let got = lex(b"xx (abc) (a\\)b) Tj");
    assert!(got[1].alias && got[1].at == 4 && got[1].text.as_ref() == b"abc");
    assert!(!got[2].alias && got[2].text.as_ref() == b"a)b");
}

/// An odd number of hex digits is padded with a trailing zero (ISO 32000-1 section 7.3.4.3).
#[test]
fn hex_string_odd_digit_count() {
    let got = lex(b"<4869 7> Tj");
    assert!(got.len() == 2 && got[0].kind == Kind::String, "{got:?}");
    assert_eq!(got[0].text.as_ref(), [0x48, 0x69, 0x70]);
}

#[test]
fn dict_and_array_delimiters() {
    let got = lex(b"<< /A [1 2] >> BDC");
    assert_eq!(
        kinds(&got),
        [
            Kind::DictOpen,
            Kind::Name,
            Kind::ArrayOpen,
            Kind::Number,
            Kind::Number,
            Kind::ArrayClose,
            Kind::DictClose,
            Kind::Keyword
        ]
    );
}

#[test]
fn name_hex_escape() {
    let got = lex(b"/A#20B Do");
    assert!(
        got.len() == 2 && got[0].kind == Kind::Name && got[0].text.as_ref() == b"A B",
        "{got:?}"
    );
}

#[test]
fn skips_comments() {
    let got = lex(b"% a comment with ( unbalanced\nq % trailing\nQ");
    assert!(
        got.len() == 2 && got[0].text.as_ref() == b"q" && got[1].text.as_ref() == b"Q",
        "{got:?}"
    );
}

/// An inline image's binary payload must never be tokenized: it can contain any byte sequence,
/// including things that look like operators.
#[test]
fn inline_image_is_one_token() {
    let got = lex(b"BI /W 2 /H 2 /BPC 8 /CS /G ID \x00q Q(\xff EI Q");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].kind, Kind::InlineImage);
    assert!(got[1].kind == Kind::Keyword && got[1].text.as_ref() == b"Q");
}

/// `n` bytes that contain no "EI" and end in a non-whitespace byte, so that a scan for a
/// whitespace-preceded EI cannot terminate inside them.
fn samples(n: usize) -> String {
    (0..n).map(|i| (b'A' + (i % 16) as u8) as char).collect()
}

/// ISO 32000-1 8.9.7 puts EI after the sample data. It does not require a whitespace byte in
/// between, and four govdocs1 documents are written without one. For an unfiltered image the
/// sample count is fully determined by /W, /H, /BPC and the /CS component count, so the end of
/// the data is a computed offset rather than a delimiter to search for.
#[test]
fn inline_image_ends_at_computed_length_with_no_whitespace_before_ei() {
    // 4 wide, 2 high, 8 bits, 1 component = 8 sample bytes.
    let src = format!("q\nBI\n/W 4 /H 2 /BPC 8 /CS /G\nID {}EI\nQ", samples(8));
    let got = lex(src.as_bytes());
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[1].kind, Kind::InlineImage);
    assert!(got[2].kind == Kind::Keyword && got[2].text.as_ref() == b"Q");
}

/// The shape measured on 050289.pdf page 25: the first image's data ends without whitespace, so
/// a scan runs past its EI and stops at a LATER image's EI. Three images were reported as one,
/// with no error, and the walk resumed in the wrong place.
#[test]
fn inline_images_are_not_merged_at_a_later_images_ei() {
    // First image ends on a sample byte; the second ends on a space, which is the only
    // whitespace-preceded EI in the stream.
    let src = format!(
        "BI /W 8 /H 2 /BPC 8 /CS /G ID {}EI BI /W 4 /H 2 /BPC 8 /CS /G ID {} EI Q",
        samples(16),
        samples(7)
    );
    let got = lex(src.as_bytes());
    assert_eq!(count(&got, Kind::InlineImage), 2, "{got:?}");
    assert_eq!(got.len(), 3, "{got:?}");
}

/// The component count of the colour space is a multiplier on the sample length, so a wrong
/// one puts EI at the wrong offset. Each case here supplies exactly the bytes its dictionary
/// implies and no whitespace-preceded EI anywhere, so a wrong count cannot be rescued by the
/// fallback scan: it becomes an error.
#[test]
fn inline_image_computes_length_per_colour_space() {
    for (name, dict, bytes) in [
        ("gray", "/W 4 /H 2 /BPC 8 /CS /G", 8),
        ("rgb", "/W 4 /H 2 /BPC 8 /CS /RGB", 24),
        ("cmyk", "/W 4 /H 2 /BPC 8 /CS /CMYK", 32),
        (
            "device names in full",
            "/W 4 /H 2 /BPC 8 /CS /DeviceRGB",
            24,
        ),
        (
            "indexed is one sample per pixel",
            "/W 5 /H 2 /BPC 4 /CS [/I /RGB 3 <AABBCC>]",
            6,
        ),
        ("sub-byte rows pad to a byte", "/W 3 /H 4 /BPC 4 /CS /G", 8),
        ("image mask is one bit per sample", "/W 9 /H 2 /IM true", 4),
    ] {
        let src = format!("BI {dict} ID {}EI Q", samples(bytes));
        let got = lex(src.as_bytes());
        assert!(
            got.len() == 2 && got[0].kind == Kind::InlineImage,
            "{name}: got {} tokens {got:?}; want [inline image, Q]",
            got.len()
        );
    }
}

/// /W and /H come out of the file, so their product is not bounded by anything the file has to
/// honour. Bounding each one on its own is not enough: 2e9 by 2e9 at 8 bits and 3 components
/// multiplies out past int64.
#[test]
fn inline_image_does_not_overflow_on_huge_dimensions() {
    for (name, dict) in [
        // row x height overflows: the row fits, the page does not.
        (
            "rows overflow int64",
            "/W 2000000000 /H 2000000000 /BPC 8 /CS /RGB",
        ),
        // width x depth x components overflows before a row is even taken.
        (
            "one row overflows int64",
            "/W 2000000000 /H 2 /BPC 2000000000 /CS /CMYK",
        ),
        // Representable, merely far longer than the stream: the length is computed fine and
        // the scan takes over because EI cannot be there.
        (
            "length exceeds the stream",
            "/W 1000000 /H 1000000 /BPC 8 /CS /G",
        ),
    ] {
        let src = format!("BI {dict} ID xx EI Q");
        let got = lex(src.as_bytes());
        assert_eq!(got.len(), 2, "{name}: {got:?}");
        assert_eq!(got[0].kind, Kind::InlineImage, "{name}");
    }
}

/// A filtered image holds encoded bytes, so /W, /H and /BPC say nothing about how many there
/// are and the delimiter scan stays as the fallback. The encoded data here carries a literal
/// "EI" exactly where the unfiltered length would have put the terminator.
#[test]
fn inline_image_with_filter_falls_back_to_scanning_for_ei() {
    let got = lex(b"BI /W 4 /H 2 /BPC 8 /CS /G /F /AHx ID 41424344EI4546> EI Q");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].kind, Kind::InlineImage);
}

/// A filter can still fix the end of the data even though it hides the length. ASCII85 and
/// ASCIIHex both carry an end-of-data marker, and neither marker's bytes belong to its own
/// alphabet, so the first occurrence is the real one (the shape of 250028.pdf page 53).
#[test]
fn inline_image_ends_at_the_filter_eod_marker() {
    // Each payload plants a whitespace-preceded EI, and a bare EOD-looking byte for the other
    // filter, ahead of the real marker.
    for (name, src) in [
        (
            "ascii85 in an array, as 250028 writes it",
            "q BI /W 8 /H 4 /BPC 8 /CS /RGB /F [/A85 /Fl] ID 9jqo^ EI Blbd> z!!~> EI Q",
        ),
        (
            "ascii85 as a bare name",
            "q BI /W 8 /H 4 /BPC 8 /CS /RGB /F /A85 ID 9jqo^ EI Blbd> z!!~> EI Q",
        ),
        (
            "ascii85 spelled in full",
            "q BI /W 8 /H 4 /BPC 8 /CS /RGB /F /ASCII85Decode ID 9jqo^ EI Blbd> z!!~> EI Q",
        ),
        (
            "asciihex",
            "q BI /W 8 /H 4 /BPC 8 /CS /RGB /F /AHx ID 4142 EI 4344> EI Q",
        ),
        (
            "asciihex spelled in full",
            "q BI /W 8 /H 4 /BPC 8 /CS /RGB /F /ASCIIHexDecode ID 4142 EI 4344> EI Q",
        ),
    ] {
        let got = lex(src.as_bytes());
        assert_eq!(got.len(), 3, "{name}: {got:?}");
        assert_eq!(got[1].kind, Kind::InlineImage, "{name}");
        assert!(
            got[2].kind == Kind::Keyword && got[2].text.as_ref() == b"Q",
            "{name}"
        );
    }
}

/// Two ASCII85 images in a row must stay two. The first payload holds no whitespace-preceded EI
/// at all, so a scan runs into the second image.
#[test]
fn adjacent_filtered_inline_images_are_not_merged() {
    let src =
        "BI /W 4 /H 2 /CS /G /F /A85 ID 9jqo^Blbd~>EI BI /W 4 /H 2 /CS /G /F /A85 ID F*)HD~> EI Q";
    let got = lex(src.as_bytes());
    assert_eq!(count(&got, Kind::InlineImage), 2, "{got:?}");
}

/// The marker fixes the end only if EI is actually there. A file that carries the marker's
/// bytes inside its data gets the search, exactly as before.
#[test]
fn inline_image_falls_back_when_the_eod_marker_is_not_followed_by_ei() {
    let got = lex(b"BI /W 4 /H 2 /CS /G /F /AHx ID 4142>4344 EI Q");
    assert!(
        got.len() == 2 && got[0].kind == Kind::InlineImage,
        "{got:?}"
    );
}

/// A filter with no end-of-data marker leaves the scan as the only option. The outermost
/// filter is the one that decides, and it is the first array entry.
#[test]
fn inline_image_with_a_filter_that_has_no_eod_still_scans() {
    for (name, src) in [
        (
            "flate alone",
            &b"BI /W 4 /H 2 /CS /G /F /Fl ID \x01\x02~>\x03 EI Q"[..],
        ),
        (
            "flate outermost",
            &b"BI /W 4 /H 2 /CS /G /F [/Fl /A85] ID \x01\x02~>\x03 EI Q"[..],
        ),
    ] {
        let got = lex(src);
        assert!(
            got.len() == 2 && got[0].kind == Kind::InlineImage,
            "{name}: {got:?}"
        );
    }
}

#[test]
fn errors() {
    for (name, src) in [
        ("unterminated literal string", &b"(abc"[..]),
        ("unterminated hex string", &b"<4869"[..]),
        ("stray close paren", &b") Tj"[..]),
        ("inline image without EI", &b"BI /W 1 ID \x00\x01"[..]),
    ] {
        let mut l = Lexer::new(src);
        loop {
            match l.next_token() {
                Ok(None) => panic!("{name}: reached EOF without an error"),
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
    }
}

/// The lexer must always make progress, or a malformed stream becomes a hang.
#[test]
fn always_advances() {
    let mut l = Lexer::new(b"q\x00\x00 Q");
    let mut prev = None;
    for _ in 0..100 {
        if !matches!(l.next_token(), Ok(Some(_))) {
            return;
        }
        assert!(Some(l.pos) > prev, "lexer did not advance past {prev:?}");
        prev = Some(l.pos);
    }
}

/// `FuzzLexer` as a seeded test: the lexer yields at most one token per input byte plus one.
#[test]
fn fuzz_seeds_terminate() {
    let seeds: [&[u8]; 5] = [
        b"q 612 0 0 792 0 0 cm /Im0 Do Q",
        b"BT (a) Tj ET",
        b"BI /W 1 ID \x00 EI",
        b"BI /W 2000000000 /H 2000000000 /BPC 8 /CS /RGB ID xx EI Q",
        b"<< /A [1 2] >> BDC",
    ];
    for s in seeds {
        let mut l = Lexer::new(s);
        let mut ended = false;
        for _ in 0..=s.len() + 1 {
            match l.next_token() {
                Ok(Some(_)) => {}
                _ => {
                    ended = true;
                    break;
                }
            }
        }
        assert!(
            ended,
            "lexer produced more tokens than input bytes for {s:?}"
        );
    }
}
