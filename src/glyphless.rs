//! Port of byblos internal/glyphless/glyphless.go (gen.go is not ported) and its embedded
//! glyphless.ttf, plus glyphless_test.go minus its poppler test.
//!
//! A minimal TrueType font whose glyphs carry no outlines: every character prints nothing but
//! occupies exactly the horizontal space of a normal proportional glyph. It is the asset for
//! writing invisible text (PDF text render mode 3, an OCR-style layer). The `.ttf` is a
//! checked-in asset generated once by Go's gen.go; nothing here builds a font at runtime.

/// The font's design grid. It is 1000, so a glyph's PDF `/Widths` entry (1000ths of text space,
/// ISO 32000-1 9.2.4) equals its hmtx advance with no conversion.
pub const UNITS_PER_EM: u16 = 1000;

/// First covered codepoint: space.
pub const FIRST_RUNE: char = ' ';
/// Last covered codepoint: tilde. The font covers printable ASCII only.
pub const LAST_RUNE: char = '~';

/// The font's glyph count: one per covered rune plus glyph 0, which is `.notdef`.
pub const NUM_GLYPHS: u16 = LAST_RUNE as u16 - FIRST_RUNE as u16 + 2;

/// The TrueType font program.
pub const FONT: &[u8] = include_bytes!("glyphless/glyphless.ttf");

/// Each covered rune's advance in font units, indexed by rune - `FIRST_RUNE`. Adobe's published
/// Helvetica AFM metrics (Core 14 set).
#[rustfmt::skip]
const WIDTHS: [u16; (LAST_RUNE as usize) - (FIRST_RUNE as usize) + 1] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // ' ' .. '/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // '0' .. '?'
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // '@' .. 'O'
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // 'P' .. '_'
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // '`' .. 'o'
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // 'p' .. '~'
];

/// Returns `r`'s glyph index in [`FONT`]. Glyphs are assigned in rune order starting at 1
/// (0 is `.notdef`).
pub fn glyph_id(r: char) -> Option<u16> {
    let off = (r as u32).checked_sub(FIRST_RUNE as u32)?;
    if r > LAST_RUNE {
        return None;
    }
    Some(off as u16 + 1)
}

/// Returns `r`'s advance width in font units ([`UNITS_PER_EM`] == 1000), the value in the
/// font's hmtx table for `r`'s glyph.
pub fn width(r: char) -> Option<u16> {
    let off = (r as u32).checked_sub(FIRST_RUNE as u32)?;
    WIDTHS.get(off as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const REQUIRED_TABLES: [&str; 9] = [
        "cmap", "glyf", "head", "hhea", "hmtx", "loca", "maxp", "name", "post",
    ];

    fn be16(b: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([b[at], b[at + 1]])
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    struct Table {
        checksum: u32,
        offset: usize,
        data: &'static [u8],
    }

    fn parse_sfnt(raw: &'static [u8]) -> (u32, HashMap<String, Table>) {
        assert!(raw.len() >= 12, "font is {} bytes, too short", raw.len());
        let scaler = be32(raw, 0);
        let n = be16(raw, 4) as usize;
        let mut tables = HashMap::new();
        for i in 0..n {
            let off = 12 + i * 16;
            assert!(off + 16 <= raw.len(), "directory entry {i} runs past end");
            let tag = String::from_utf8_lossy(&raw[off..off + 4]).into_owned();
            let t_off = be32(raw, off + 8) as usize;
            let t_len = be32(raw, off + 12) as usize;
            assert!(t_off + t_len <= raw.len(), "table {tag:?} runs past end");
            tables.insert(
                tag,
                Table {
                    checksum: be32(raw, off + 4),
                    offset: t_off,
                    data: &raw[t_off..t_off + t_len],
                },
            );
        }
        (scaler, tables)
    }

    fn table_checksum(data: &[u8]) -> u32 {
        let mut sum = 0u32;
        for chunk in data.chunks(4) {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            sum = sum.wrapping_add(u32::from_be_bytes(word));
        }
        sum
    }

    #[test]
    fn sfnt_header_and_directory() {
        let (scaler, tables) = parse_sfnt(FONT);
        assert_eq!(scaler, 0x0001_0000);
        for tag in REQUIRED_TABLES {
            assert!(
                tables.contains_key(tag),
                "required table {tag:?} is missing"
            );
        }
    }

    #[test]
    fn table_checksums() {
        let (_, tables) = parse_sfnt(FONT);
        for (tag, tbl) in &tables {
            let mut data = tbl.data.to_vec();
            if tag == "head" {
                // The directory records head's checksum with checkSumAdjustment zeroed.
                data[8..12].fill(0);
            }
            assert_eq!(table_checksum(&data), tbl.checksum, "table {tag:?}");
        }
    }

    #[test]
    fn head_checksum_adjustment() {
        assert_eq!(FONT.len() % 4, 0);
        let (_, tables) = parse_sfnt(FONT);
        let head = &tables["head"];
        let adjustment = be32(head.data, 8);
        let mut raw = FONT.to_vec();
        raw[head.offset + 8..head.offset + 12].fill(0);
        let sum = table_checksum(&raw);
        assert_eq!(0xB1B0_AFBAu32.wrapping_sub(sum), adjustment);
    }

    #[test]
    fn glyph_outlines_are_empty() {
        let (_, tables) = parse_sfnt(FONT);
        let loc_format = be16(tables["head"].data, 50) as i16;
        let num_glyphs = be16(tables["maxp"].data, 4) as usize;
        assert_eq!(num_glyphs, NUM_GLYPHS as usize);
        let loca = tables["loca"].data;
        let offset = |i: usize| -> u32 {
            match loc_format {
                0 => u32::from(be16(loca, i * 2)) * 2,
                1 => be32(loca, i * 4),
                f => panic!("indexToLocFormat = {f}, want 0 or 1"),
            }
        };
        if loc_format == 0 {
            assert_eq!(loca.len(), (num_glyphs + 1) * 2);
        }
        for i in 0..num_glyphs {
            assert_eq!(
                offset(i),
                offset(i + 1),
                "glyph {i} has a non-empty outline"
            );
        }
        assert_eq!(tables["glyf"].data.len(), 0);
    }

    /// Lookup over the (3,1) format 4 cmap subtable.
    fn cmap_format4(data: &'static [u8]) -> impl Fn(u16) -> Option<u16> {
        let n = be16(data, 2) as usize;
        let mut sub_off = None;
        for i in 0..n {
            let rec = 4 + i * 8;
            if be16(data, rec) == 3 && be16(data, rec + 2) == 1 {
                sub_off = Some(be32(data, rec + 4) as usize);
            }
        }
        let sub = &data[sub_off.expect("no (3,1) subtable")..];
        assert_eq!(be16(sub, 0), 4, "subtable format");
        let seg_x2 = be16(sub, 6) as usize;
        let end_at = 14;
        let start_at = end_at + seg_x2 + 2;
        let delta_at = start_at + seg_x2;
        let range_at = delta_at + seg_x2;
        move |c: u16| {
            for i in 0..seg_x2 / 2 {
                let end = be16(sub, end_at + i * 2);
                let start = be16(sub, start_at + i * 2);
                if c < start || c > end {
                    continue;
                }
                let delta = be16(sub, delta_at + i * 2);
                let range_offset = be16(sub, range_at + i * 2) as usize;
                if range_offset == 0 {
                    let gid = c.wrapping_add(delta);
                    return if gid == 0 { None } else { Some(gid) };
                }
                let at = range_at + i * 2 + range_offset + usize::from(c - start) * 2;
                let gid = be16(sub, at);
                return if gid == 0 {
                    None
                } else {
                    Some(gid.wrapping_add(delta))
                };
            }
            None
        }
    }

    #[test]
    fn width_table_maps_its_codepoints() {
        let (_, tables) = parse_sfnt(FONT);
        let hmtx = tables["hmtx"].data;
        let lookup = cmap_format4(tables["cmap"].data);
        for r in FIRST_RUNE..=LAST_RUNE {
            let want_gid = glyph_id(r).expect("covered");
            let got_gid =
                lookup(r as u16).unwrap_or_else(|| panic!("cmap has no mapping for {r:?}"));
            assert_eq!(got_gid, want_gid, "{r:?}");
            let want_width = width(r).expect("covered");
            let gid = got_gid as usize;
            assert!(gid * 4 + 4 <= hmtx.len(), "hmtx too short for glyph {gid}");
            assert_eq!(be16(hmtx, gid * 4), want_width, "{r:?}");
        }
        assert!(glyph_id('\u{1f}').is_none());
        assert!(lookup(0x1f).is_none());
    }
}
