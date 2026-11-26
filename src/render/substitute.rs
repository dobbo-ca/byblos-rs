//! Substitute glyphs for non-embedded fonts: port of byblos `internal/render/substitute.go`.
//!
//! A PDF that names a face without embedding a program gets a bundled metric-compatible
//! Liberation face (SIL OFL 1.1; the licence sits beside the TTFs in `fonts/`), chosen by name
//! with the `/FontDescriptor` flag bits as the fallback. The substitute is an ordinary TrueType,
//! so it rides the same pre-parse glyph gate and sfnt path as an embedded one. Symbol and
//! ZapfDingbats have no metric-compatible face and degrade to widths-only.

/// FontDescriptor flag bits (ISO 32000-1 table 123; the spec numbers bits from 1, so Serif is
/// "bit 2" = 1<<1).
pub const FLAG_FIXED_PITCH: i64 = 1;
pub const FLAG_SERIF: i64 = 1 << 1;
pub const FLAG_SYMBOLIC: i64 = 1 << 2;
pub const FLAG_ITALIC: i64 = 1 << 6;

/// One bundled face: its path under `fonts/` (as Go's embed FS names it) and its bytes.
struct Face {
    path: &'static str,
    data: &'static [u8],
}

macro_rules! face {
    ($f:literal) => {
        Face {
            path: concat!("fonts/Liberation", $f, ".ttf"),
            data: include_bytes!(concat!("fonts/Liberation", $f, ".ttf")),
        }
    };
}

/// Indexed `[family][style]`: Sans, Serif, Mono by Regular, Bold, Italic, BoldItalic.
static FACES: [[Face; 4]; 3] = [
    [
        face!("Sans-Regular"),
        face!("Sans-Bold"),
        face!("Sans-Italic"),
        face!("Sans-BoldItalic"),
    ],
    [
        face!("Serif-Regular"),
        face!("Serif-Bold"),
        face!("Serif-Italic"),
        face!("Serif-BoldItalic"),
    ],
    [
        face!("Mono-Regular"),
        face!("Mono-Bold"),
        face!("Mono-Italic"),
        face!("Mono-BoldItalic"),
    ],
];

/// Go's `strings.ToLower` for the characters that matter here: simple case mapping, so U+0130
/// lowers to `i`, where Rust's full mapping would add a combining dot.
fn lower(c: char) -> char {
    if c == '\u{130}' {
        'i'
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn face(base_font: &[u8], flags: i64) -> Option<&'static Face> {
    let name = match base_font.iter().position(|&b| b == b'+') {
        Some(i) => base_font.get(i + 1..).unwrap_or(&[]), // subset tag ("ABCDEF+...")
        None => base_font,
    };
    let name: String = String::from_utf8_lossy(name).chars().map(lower).collect();
    let family = if name.contains("courier") {
        2 // Mono
    } else if name.contains("times") {
        1 // Serif
    } else if name.contains("helvetica") || name.contains("arial") {
        0 // Sans
    } else if name.contains("symbol") || name.contains("dingbat") {
        return None; // DEFERRED: no metric-compatible open face exists
    } else if flags & FLAG_SYMBOLIC != 0 {
        // An unknown-name symbolic face (Wingdings, a TeX math font): Latin glyphs for symbol
        // codes would misrender, so degrade to widths-only like the named symbol faces above.
        return None;
    } else if flags & FLAG_FIXED_PITCH != 0 {
        2
    } else if flags & FLAG_SERIF != 0 {
        1
    } else {
        0
    };
    let bold = name.contains("bold");
    let italic = name.contains("italic") || name.contains("oblique") || flags & FLAG_ITALIC != 0;
    let style = match (bold, italic) {
        (true, true) => 3,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 0,
    };
    FACES.get(family)?.get(style)
}

/// `substituteFace`: maps a `/BaseFont` name plus descriptor flags to a bundled face path
/// (`fonts/LiberationSans-Bold.ttf`), or `None` for the deferred faces. Name wins over flags: the
/// standard-14 names and their everyday aliases (Arial for Helvetica, Times New Roman for Times,
/// Courier New for Courier) carry style in the name; anything unrecognised falls back to the
/// fixed-pitch/serif/italic flag bits.
pub fn substitute_face(base_font: &[u8], flags: i64) -> Option<&'static str> {
    face(base_font, flags).map(|f| f.path)
}

/// `substituteProgram`: the bundled TrueType bytes for a non-embedded font, or `None` for the
/// deferred faces.
pub fn substitute_program(base_font: &[u8], flags: i64) -> Option<&'static [u8]> {
    face(base_font, flags).map(|f| f.data)
}

#[cfg(test)]
mod tests {
    use super::super::glyf::parse_glyf_index;
    use super::*;
    use crate::limits::Limits;

    /// The standard-14 name mapping, the common aliases, the subset-tag strip, the
    /// descriptor-flag fallback for unknown names, and the DEFERRED faces.
    #[test]
    fn face_mapping() {
        let cases: &[(&str, i64, Option<&str>)] = &[
            ("Helvetica", 0, Some("fonts/LiberationSans-Regular.ttf")),
            ("Helvetica-Bold", 0, Some("fonts/LiberationSans-Bold.ttf")),
            (
                "Helvetica-Oblique",
                0,
                Some("fonts/LiberationSans-Italic.ttf"),
            ),
            (
                "Helvetica-BoldOblique",
                0,
                Some("fonts/LiberationSans-BoldItalic.ttf"),
            ),
            ("Times-Roman", 0, Some("fonts/LiberationSerif-Regular.ttf")),
            ("Times-Bold", 0, Some("fonts/LiberationSerif-Bold.ttf")),
            ("Times-Italic", 0, Some("fonts/LiberationSerif-Italic.ttf")),
            (
                "Times-BoldItalic",
                0,
                Some("fonts/LiberationSerif-BoldItalic.ttf"),
            ),
            ("Courier", 0, Some("fonts/LiberationMono-Regular.ttf")),
            ("Courier-Bold", 0, Some("fonts/LiberationMono-Bold.ttf")),
            (
                "Courier-Oblique",
                0,
                Some("fonts/LiberationMono-Italic.ttf"),
            ),
            (
                "Courier-BoldOblique",
                0,
                Some("fonts/LiberationMono-BoldItalic.ttf"),
            ),
            // The two standard-14 faces with no Liberation equivalent: DEFERRED.
            ("Symbol", 0, None),
            ("ZapfDingbats", 0, None),
            // Aliases and subset tags seen across the corpus.
            ("Arial", 0, Some("fonts/LiberationSans-Regular.ttf")),
            ("ArialMT", 0, Some("fonts/LiberationSans-Regular.ttf")),
            ("Arial-BoldMT", 0, Some("fonts/LiberationSans-Bold.ttf")),
            (
                "ABCDEF+Arial-ItalicMT",
                0,
                Some("fonts/LiberationSans-Italic.ttf"),
            ),
            (
                "TimesNewRoman",
                0,
                Some("fonts/LiberationSerif-Regular.ttf"),
            ),
            (
                "TimesNewRomanPS-BoldMT",
                0,
                Some("fonts/LiberationSerif-Bold.ttf"),
            ),
            ("CourierNew", 0, Some("fonts/LiberationMono-Regular.ttf")),
            // Unknown names fall back by descriptor flags: fixed-pitch bit, serif bit, italic bit
            // (ISO 32000-1 table 123), else sans.
            (
                "Garamond",
                FLAG_SERIF,
                Some("fonts/LiberationSerif-Regular.ttf"),
            ),
            (
                "SomeMono",
                FLAG_FIXED_PITCH,
                Some("fonts/LiberationMono-Regular.ttf"),
            ),
            (
                "Whatever",
                FLAG_ITALIC,
                Some("fonts/LiberationSans-Italic.ttf"),
            ),
            ("Whatever", 0, Some("fonts/LiberationSans-Regular.ttf")),
            ("", 0, Some("fonts/LiberationSans-Regular.ttf")),
            // An unknown-name SYMBOLIC face gets no Latin substitute; a recognised standard-14
            // name still wins over a stray symbolic bit.
            ("Wingdings", FLAG_SYMBOLIC, None),
            (
                "Helvetica",
                FLAG_SYMBOLIC,
                Some("fonts/LiberationSans-Regular.ttf"),
            ),
            // A proportional Monotype face must NOT be claimed for the fixed-pitch family by name.
            (
                "MonotypeCorsiva",
                0,
                Some("fonts/LiberationSans-Regular.ttf"),
            ),
            // Beyond the Go cases: the simple-case-mapping quirk and an invalid UTF-8 name.
            ("\u{130}talic", 0, Some("fonts/LiberationSans-Italic.ttf")),
            ("Arial\u{fffd}", 0, Some("fonts/LiberationSans-Regular.ttf")),
            ("TIMES-BOLD", 0, Some("fonts/LiberationSerif-Bold.ttf")),
        ];
        for &(name, flags, want) in cases {
            assert_eq!(
                substitute_face(name.as_bytes(), flags),
                want,
                "substitute_face({name:?}, {flags:#x})"
            );
        }
        assert_eq!(
            substitute_face(b"Arial\xff", 0),
            Some("fonts/LiberationSans-Regular.ttf")
        );
    }

    /// The bundled faces ride the existing TrueType path: pre-parse glyph gate included.
    #[test]
    fn takes_the_truetype_path() {
        let p = substitute_program(b"Helvetica", 0).expect("no program for Helvetica");
        let g = parse_glyf_index(p).expect("the bundled face is not an indexable TrueType");
        assert!(
            g.bounded_by(Limits::default().path_points()),
            "the bundled face fails the pre-parse gate"
        );
    }

    /// Every one of the 12 family/style combinations resolves to real TrueType bytes.
    #[test]
    fn all_faces_load() {
        for name in [
            "Helvetica",
            "Helvetica-Bold",
            "Helvetica-Oblique",
            "Helvetica-BoldOblique",
            "Times-Roman",
            "Times-Bold",
            "Times-Italic",
            "Times-BoldItalic",
            "Courier",
            "Courier-Bold",
            "Courier-Oblique",
            "Courier-BoldOblique",
        ] {
            let p = substitute_program(name.as_bytes(), 0)
                .unwrap_or_else(|| panic!("no embedded program for {name}"));
            assert!(p.len() > 100_000, "{name}: {} bytes", p.len());
            assert_eq!(&p[..4], &[0, 1, 0, 0], "{name}: not a TrueType");
        }
        assert!(substitute_program(b"Symbol", 0).is_none());
    }
}
