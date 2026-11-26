//! Port of golang.org/x/image/font/sfnt `cmap.go`: the character map. Only the formats Go
//! supports are read (0, 4, 6 and 12); the platform scan picks the widest supported subtable.

use super::{
    Budget, GlyphIndex, INVALID_CMAP_TABLE, MAX_CMAP_SEGMENTS, Result, Table,
    UNSUPPORTED_CMAP_ENCODINGS, UNSUPPORTED_NUMBER_OF_CMAP_SEGMENTS, u16, u32, view,
};

const PID_UNICODE: u16 = 0;
const PID_MACINTOSH: u16 = 1;
const PID_WINDOWS: u16 = 3;

const PSID_UNICODE2_BMP_ONLY: u16 = 3;
const PSID_UNICODE2_FULL_REPERTOIRE: u16 = 4;
const PSID_MACINTOSH_ROMAN: u16 = 0;
const PSID_WINDOWS_SYMBOL: u16 = 0;
const PSID_WINDOWS_UCS2: u16 = 1;
const PSID_WINDOWS_UCS4: u16 = 10;

/// The runes of Macintosh Roman bytes 0x80..=0xff (x/text `charmap.Macintosh`); bytes below
/// 0x80 are ASCII.
const MAC_ROMAN_HIGH: [u32; 128] = [
    0x00c4, 0x00c5, 0x00c7, 0x00c9, 0x00d1, 0x00d6, 0x00dc, 0x00e1, 0x00e0, 0x00e2, 0x00e4, 0x00e3,
    0x00e5, 0x00e7, 0x00e9, 0x00e8, 0x00ea, 0x00eb, 0x00ed, 0x00ec, 0x00ee, 0x00ef, 0x00f1, 0x00f3,
    0x00f2, 0x00f4, 0x00f6, 0x00f5, 0x00fa, 0x00f9, 0x00fb, 0x00fc, 0x2020, 0x00b0, 0x00a2, 0x00a3,
    0x00a7, 0x2022, 0x00b6, 0x00df, 0x00ae, 0x00a9, 0x2122, 0x00b4, 0x00a8, 0x2260, 0x00c6, 0x00d8,
    0x221e, 0x00b1, 0x2264, 0x2265, 0x00a5, 0x00b5, 0x2202, 0x2211, 0x220f, 0x03c0, 0x222b, 0x00aa,
    0x00ba, 0x03a9, 0x00e6, 0x00f8, 0x00bf, 0x00a1, 0x00ac, 0x221a, 0x0192, 0x2248, 0x2206, 0x00ab,
    0x00bb, 0x2026, 0x00a0, 0x00c0, 0x00c3, 0x00d5, 0x0152, 0x0153, 0x2013, 0x2014, 0x201c, 0x201d,
    0x2018, 0x2019, 0x00f7, 0x25ca, 0x00ff, 0x0178, 0x2044, 0x20ac, 0x2039, 0x203a, 0xfb01, 0xfb02,
    0x2021, 0x00b7, 0x201a, 0x201e, 0x2030, 0x00c2, 0x00ca, 0x00c1, 0x00cb, 0x00c8, 0x00cd, 0x00ce,
    0x00cf, 0x00cc, 0x00d3, 0x00d4, 0xf8ff, 0x00d2, 0x00da, 0x00db, 0x00d9, 0x0131, 0x02c6, 0x02dc,
    0x00af, 0x02d8, 0x02d9, 0x02da, 0x00b8, 0x02dd, 0x02db, 0x02c7,
];

/// `charmap.Macintosh.EncodeRune`: the Macintosh Roman byte of rune `r`, if it has one.
pub(crate) fn mac_roman_encode(r: i32) -> Option<u8> {
    if r < 0x80 {
        // ASCII superset; a negative rune truncates like Go's `byte(r)`.
        return Some(r as u8);
    }
    let r = r as u32;
    MAC_ROMAN_HIGH
        .iter()
        .position(|&x| x == r)
        .map(|i| 0x80 + i as u8)
}

/// `platformEncodingWidth`: the number of bytes per character a Platform ID and Platform
/// Specific ID assume; 0 for an unsupported pair.
fn platform_encoding_width(pid: u16, psid: u16) -> i32 {
    match (pid, psid) {
        (PID_UNICODE, PSID_UNICODE2_BMP_ONLY) => 2,
        (PID_UNICODE, PSID_UNICODE2_FULL_REPERTOIRE) => 4,
        (PID_MACINTOSH, PSID_MACINTOSH_ROMAN) => 1,
        (PID_WINDOWS, PSID_WINDOWS_SYMBOL) => 2,
        (PID_WINDOWS, PSID_WINDOWS_UCS2) => 2,
        (PID_WINDOWS, PSID_WINDOWS_UCS4) => 4,
        _ => 0,
    }
}

/// `supportedCmapFormat`.
fn supported_cmap_format(format: u16, pid: u16, psid: u16) -> bool {
    match format {
        0 => pid == PID_MACINTOSH && psid == PSID_MACINTOSH_ROMAN,
        4 | 6 | 12 => true,
        _ => false,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Entry16 {
    end: u16,
    start: u16,
    delta: u16,
    offset: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct Entry32 {
    start: u32,
    end: u32,
    delta: u32,
}

/// The glyph index function of a font (`glyphIndexFunc`), one arm per cmap format.
pub(crate) enum GlyphIndexer {
    Format0(Vec<u8>),
    Format4 {
        entries: Vec<Entry16>,
        indexes_base: u32,
        indexes_length: u32,
    },
    Format6 {
        first_code: u16,
        entries: Vec<u16>,
    },
    Format12(Vec<Entry32>),
}

impl GlyphIndexer {
    /// The glyph index of rune `r`; 0 if the font has none.
    pub(crate) fn lookup(&self, src: &[u8], r: i32) -> Result<GlyphIndex> {
        match self {
            GlyphIndexer::Format0(table) => {
                let Some(x) = mac_roman_encode(r) else {
                    // The rune is not representable in Macintosh Roman.
                    return Ok(GlyphIndex(0));
                };
                Ok(GlyphIndex(u16::from(
                    table.get(usize::from(x)).copied().unwrap_or(0),
                )))
            }
            GlyphIndexer::Format4 {
                entries,
                indexes_base,
                indexes_length,
            } => {
                if (r as u32) > 0xffff {
                    return Ok(GlyphIndex(0));
                }
                let c = r as u16;
                let (mut i, mut j) = (0usize, entries.len());
                while i < j {
                    let h = i + (j - i) / 2;
                    let Some(entry) = entries.get(h) else {
                        return Ok(GlyphIndex(0));
                    };
                    if c < entry.start {
                        j = h;
                    } else if entry.end < c {
                        i = h + 1;
                    } else if entry.offset == 0 {
                        return Ok(GlyphIndex(c.wrapping_add(entry.delta)));
                    } else {
                        // Go converts the (negative) int to uint32, wrapping.
                        let t = (h as i64 - entries.len() as i64
                            + i64::from(c.wrapping_sub(entry.start)))
                            as u32;
                        let offset = u32::from(entry.offset).wrapping_add(t.wrapping_mul(2));
                        if offset > *indexes_length || offset.wrapping_add(2) > *indexes_length {
                            return Err(INVALID_CMAP_TABLE);
                        }
                        let x = view(src, i64::from(indexes_base.wrapping_add(offset)), 2)?;
                        return Ok(GlyphIndex(u16(x)));
                    }
                }
                Ok(GlyphIndex(0))
            }
            GlyphIndexer::Format6 {
                first_code,
                entries,
            } => {
                let r16 = r as u16;
                if r16 < *first_code {
                    return Ok(GlyphIndex(0));
                }
                let c = usize::from(r16 - *first_code);
                Ok(GlyphIndex(entries.get(c).copied().unwrap_or(0)))
            }
            GlyphIndexer::Format12(entries) => {
                let c = r as u32;
                let (mut i, mut j) = (0usize, entries.len());
                while i < j {
                    let h = i + (j - i) / 2;
                    let Some(entry) = entries.get(h) else {
                        return Ok(GlyphIndex(0));
                    };
                    if c < entry.start {
                        j = h;
                    } else if entry.end < c {
                        i = h + 1;
                    } else {
                        return Ok(GlyphIndex(
                            c.wrapping_sub(entry.start).wrapping_add(entry.delta) as u16,
                        ));
                    }
                }
                Ok(GlyphIndex(0))
            }
        }
    }
}

/// `parseCmap`: scans the subtables, picks the widest supported one and reads it.
pub(crate) fn parse_cmap(
    src: &[u8],
    cmap: &Table,
    only_format: Option<u16>,
    budget: &mut Budget,
) -> Result<GlyphIndexer> {
    const HEADER_SIZE: u32 = 4;
    const ENTRY_SIZE: u32 = 8;
    if cmap.length < HEADER_SIZE {
        return Err(INVALID_CMAP_TABLE);
    }
    let num_subtables = u32::from(cmap.u16(src, 2)?);
    if cmap.length < HEADER_SIZE + ENTRY_SIZE * num_subtables {
        return Err(INVALID_CMAP_TABLE);
    }

    let mut best_width = 0;
    let mut best_offset = 0u32;
    let mut best_length = 0u32;
    let mut best_format = 0u16;

    // Scan all of the subtables, picking the widest supported one.
    for i in 0..num_subtables {
        let buf = view(
            src,
            i64::from(cmap.offset) + i64::from(HEADER_SIZE + ENTRY_SIZE * i),
            i64::from(ENTRY_SIZE),
        )?;
        let pid = u16(buf);
        let psid = u16(buf.get(2..).unwrap_or_default());
        let width = platform_encoding_width(pid, psid);
        if width <= best_width {
            continue;
        }
        let offset = u32(buf.get(4..).unwrap_or_default());

        if offset > cmap.length - 4 {
            return Err(INVALID_CMAP_TABLE);
        }
        let buf = view(src, i64::from(cmap.offset + offset), 4)?;
        let format = u16(buf);
        if !supported_cmap_format(format, pid, psid) || !only_format.is_none_or(|f| f == format) {
            continue;
        }
        let length = u32::from(u16(buf.get(2..).unwrap_or_default()));

        best_width = width;
        best_offset = offset;
        best_length = length;
        best_format = format;
    }

    if best_width == 0 {
        return Err(UNSUPPORTED_CMAP_ENCODINGS);
    }
    make_cached_glyph_index(src, cmap, best_offset, best_length, best_format, budget)
}

fn make_cached_glyph_index(
    src: &[u8],
    cmap: &Table,
    offset: u32,
    length: u32,
    format: u16,
    budget: &mut Budget,
) -> Result<GlyphIndexer> {
    match format {
        0 => format0(src, cmap, offset, length, budget),
        4 => format4(src, cmap, offset, budget),
        6 => format6(src, cmap, offset, budget),
        12 => format12(src, cmap, offset, budget),
        // parseCmap only selects the formats above.
        _ => Err(INVALID_CMAP_TABLE),
    }
}

fn format0(
    src: &[u8],
    cmap: &Table,
    offset: u32,
    length: u32,
    budget: &mut Budget,
) -> Result<GlyphIndexer> {
    if length != 6 + 256 || offset + length > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(length))?;
    let mut table = budget.vec::<u8>(256)?;
    table.extend_from_slice(buf.get(6..).unwrap_or_default());
    Ok(GlyphIndexer::Format0(table))
}

fn format4(src: &[u8], cmap: &Table, offset: u32, budget: &mut Budget) -> Result<GlyphIndexer> {
    const HEADER_SIZE: u32 = 14;
    if offset + HEADER_SIZE > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(HEADER_SIZE))?;
    let mut offset = offset + HEADER_SIZE;

    let mut seg_count = u16(buf.get(6..).unwrap_or_default());
    if seg_count & 1 != 0 {
        return Err(INVALID_CMAP_TABLE);
    }
    seg_count /= 2;
    if u32::from(seg_count) > MAX_CMAP_SEGMENTS {
        return Err(UNSUPPORTED_NUMBER_OF_CMAP_SEGMENTS);
    }

    let n = usize::from(seg_count);
    let e_length = 8 * u32::from(seg_count) + 2;
    if offset + e_length > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(e_length))?;
    offset += e_length;

    let mut entries = budget.vec::<Entry16>(n)?;
    for i in 0..n {
        let at = |base: usize| u16(buf.get(base + 2 * i..).unwrap_or_default());
        entries.push(Entry16 {
            end: at(0),
            start: at(2 * n + 2),
            delta: at(4 * n + 2),
            offset: at(6 * n + 2),
        });
    }
    Ok(GlyphIndexer::Format4 {
        entries,
        indexes_base: cmap.offset + offset,
        indexes_length: cmap.length - offset,
    })
}

fn format6(src: &[u8], cmap: &Table, offset: u32, budget: &mut Budget) -> Result<GlyphIndexer> {
    const HEADER_SIZE: u32 = 10;
    if offset + HEADER_SIZE > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(HEADER_SIZE))?;
    let offset = offset + HEADER_SIZE;

    let first_code = u16(buf.get(6..).unwrap_or_default());
    let entry_count = u16(buf.get(8..).unwrap_or_default());

    let e_length = 2 * u32::from(entry_count);
    if offset + e_length > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }

    let mut entries = budget.vec::<u16>(usize::from(entry_count))?;
    if entry_count != 0 {
        let buf = view(src, i64::from(cmap.offset + offset), i64::from(e_length))?;
        for i in 0..usize::from(entry_count) {
            entries.push(u16(buf.get(2 * i..).unwrap_or_default()));
        }
    }
    Ok(GlyphIndexer::Format6 {
        first_code,
        entries,
    })
}

fn format12(src: &[u8], cmap: &Table, offset: u32, budget: &mut Budget) -> Result<GlyphIndexer> {
    const HEADER_SIZE: u32 = 16;
    if offset + HEADER_SIZE > cmap.length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(HEADER_SIZE))?;
    let length = u32(buf.get(4..).unwrap_or_default());
    if cmap.length < offset || length > cmap.length - offset {
        return Err(INVALID_CMAP_TABLE);
    }
    let offset = offset + HEADER_SIZE;

    let num_groups = u32(buf.get(12..).unwrap_or_default());
    if num_groups > MAX_CMAP_SEGMENTS {
        return Err(UNSUPPORTED_NUMBER_OF_CMAP_SEGMENTS);
    }

    let e_length = 12 * num_groups;
    if HEADER_SIZE + e_length != length {
        return Err(INVALID_CMAP_TABLE);
    }
    let buf = view(src, i64::from(cmap.offset + offset), i64::from(e_length))?;

    let mut entries = budget.vec::<Entry32>(num_groups as usize)?;
    for i in 0..num_groups as usize {
        let at = |base: usize| u32(buf.get(base + 12 * i..).unwrap_or_default());
        entries.push(Entry32 {
            start: at(0),
            end: at(4),
            delta: at(8),
        });
    }
    Ok(GlyphIndexer::Format12(entries))
}
