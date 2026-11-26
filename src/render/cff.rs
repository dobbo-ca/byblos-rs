//! Bare CFF (Type1C) glyph outlines: port of byblos `internal/render/cff.go`.
//!
//! The CFF blob is wrapped as the `CFF ` table of a minimal OTTO container that sfnt parses
//! ([`cff_wrap_sfnt`]). Before sfnt sees the font, [`CffProgram::walk_glyph`] re-runs the control
//! flow of every reachable glyph's charstring and charges one unit per executed byte against a
//! work budget ([`MAX_CHARSTRING_WORK`]), so a subr call-tree bomb degrades the font to
//! widths-only. CID-keyed fonts (Top DICT `ROS`) are walked with the local subrs of each glyph's
//! font DICT, resolved through FDSelect exactly as sfnt does.
//!
//! The parsers are linear in the input and size their tables by structure counts that are at most
//! `u16` wide; the buffers that scale with the program use fallible reservation ([`try_vec`]), so
//! a refusal degrades to `None` (widths-only) as every other refusal does.

use super::glyf::{be16, be32};
use std::collections::HashMap;

/// `maxCharstringWork`: bounds the charstring bytes one glyph's outline may execute, subr
/// expansion included. sfnt's own per-stream cap is 64 KB. (Go keeps it in a variable so tests
/// can lower it; here the `_with_budget` entry points take it as an argument.)
pub const MAX_CHARSTRING_WORK: i64 = 1 << 16;

/// sfnt's `maxGlyphDataLength`: a stream over it is refused before a byte runs.
const MAX_STREAM: usize = 64 * 1024;

/// An empty `Vec` with room for `n` elements, or `None` when the allocator refuses.
pub fn try_vec<T>(n: usize) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    Some(v)
}

/// `cffProgram`: the parsed skeleton of a bare CFF, enough structure to gate interpreter work and
/// map codes to glyphs. Outline semantics stay in sfnt.
#[derive(Debug, Clone)]
pub struct CffProgram<'a> {
    pub charstrings: Vec<&'a [u8]>,
    pub gsubrs: Vec<&'a [u8]>,
    pub lsubrs: Vec<&'a [u8]>,
    pub code2gid: [u16; 256],
    pub upem: u16,
    /// CID-keyed only: the charset read as a CID map. `Some` exactly when the font is CID-keyed.
    pub cid2gid: Option<HashMap<u16, u16>>,
    /// CID-keyed only: the local subrs of each font DICT, partitioned by `fd_sel`.
    pub fd_subrs: Vec<Vec<&'a [u8]>>,
    pub fd_sel: Vec<i16>,
}

/// `cffToSFNT`'s result: an `sfnt.Parse`-able wrapper plus the gate-measured per-GID charstring
/// work (charged per show), and the charset-as-CID-map for a CID-keyed program.
#[derive(Debug, Clone)]
pub struct CffSfnt {
    pub otf: Vec<u8>,
    pub gate_work: Vec<i64>,
    pub cid2gid: Option<HashMap<u16, u16>>,
}

/// `cffIndex`: reads the INDEX at `p[pos..]` (count, offSize, count+1 one-based offsets, then the
/// data) and returns its items and the position after it. `None` is a malformed INDEX; an empty
/// INDEX is an empty item list.
pub fn cff_index(p: &[u8], pos: usize) -> Option<(Vec<&[u8]>, usize)> {
    // len - pos, not pos + n: a huge pos must not wrap the sum and slip past the bound.
    if p.len() < pos || p.len() - pos < 2 {
        return None;
    }
    let count = usize::from(be16(p, pos));
    if count == 0 {
        return Some((Vec::new(), pos + 2));
    }
    if p.len() - pos < 3 {
        return None;
    }
    let off_size = usize::from(*p.get(pos + 2)?);
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let off_pos = pos + 3;
    let data_pos = off_pos + (count + 1) * off_size;
    if data_pos > p.len() {
        return None;
    }
    let read_off = |i: usize| -> i64 {
        let s = p
            .get(off_pos + i * off_size..off_pos + (i + 1) * off_size)
            .unwrap_or(&[]);
        s.iter().fold(0i64, |v, &b| (v << 8) | i64::from(b))
    };
    let mut prev = read_off(0);
    if prev != 1 {
        return None;
    }
    let base = data_pos as i64 - 1;
    let mut items = try_vec(count)?;
    for i in 1..=count {
        let off = read_off(i);
        if off < prev || base + off > p.len() as i64 {
            return None;
        }
        items.push(p.get((base + prev) as usize..(base + off) as usize)?);
        prev = off;
    }
    Some((items, (base + prev) as usize))
}

/// `cffDict`: parses a DICT into operator -> operands (a 12-escaped operator x is keyed 1200+x).
/// Unknown operators are kept; only malformed operands or a truncated escape refuse. An operator
/// with no operands is present with an empty list (Go's nil slice).
pub fn cff_dict(d: &[u8]) -> Option<HashMap<i32, Vec<f64>>> {
    let mut dict: HashMap<i32, Vec<f64>> = HashMap::new();
    let mut args: Vec<f64> = Vec::new();
    let mut i = 0usize;
    while i < d.len() {
        let b = *d.get(i)?;
        let next = d.get(i + 1).copied();
        match b {
            28 => {
                if i + 3 > d.len() {
                    return None;
                }
                args.push(f64::from(be16(d, i + 1) as i16));
                i += 3;
            }
            29 => {
                if i + 5 > d.len() {
                    return None;
                }
                args.push(f64::from(be32(d, i + 1) as i32));
                i += 5;
            }
            30 => {
                // real: nibble-encoded decimal
                let mut s = String::with_capacity(16);
                i += 1;
                let mut done = false;
                while !done {
                    if i >= d.len() || s.len() > 64 {
                        return None;
                    }
                    let byte = d[i];
                    for nib in [byte >> 4, byte & 0x0f] {
                        match nib {
                            0..=9 => s.push(char::from(b'0' + nib)),
                            0x0a => s.push('.'),
                            0x0b => s.push('E'),
                            0x0c => s.push_str("E-"),
                            0x0e => s.push('-'),
                            0x0f => done = true,
                            _ => return None,
                        }
                        if done {
                            break;
                        }
                    }
                    i += 1;
                }
                // strconv accepts the exact grammar the nibbles produce; a bare or doubled
                // marker errors, and so does an out-of-range value (Go's ErrRange).
                let v: f64 = s.parse().ok()?;
                if v.is_infinite() {
                    return None;
                }
                args.push(v);
            }
            32..=246 => {
                args.push(f64::from(i32::from(b) - 139));
                i += 1;
            }
            247..=250 => {
                let n = i32::from(next?);
                args.push(f64::from(i32::from(b - 247) * 256 + n + 108));
                i += 2;
            }
            251..=254 => {
                let n = i32::from(next?);
                args.push(f64::from(-i32::from(b - 251) * 256 - n - 108));
                i += 2;
            }
            12 => {
                let n = i32::from(next?);
                dict.insert(1200 + n, std::mem::take(&mut args));
                i += 2;
            }
            0..=21 => {
                dict.insert(i32::from(b), std::mem::take(&mut args));
                i += 1;
            }
            _ => return None, // 22..27, 31, 255: reserved
        }
        if args.len() > 48 {
            return None;
        }
    }
    Some(dict)
}

/// The above-ASCII half of the CFF Standard Encoding (5176.CFF.pdf Appendix B): code -> SID.
/// Codes 32..126 are SIDs 1..95 in order and live in [`cff_standard_sid`]'s arithmetic instead.
const CFF_STD_ENC_HIGH: [(u8, u16); 54] = [
    (161, 96),
    (162, 97),
    (163, 98),
    (164, 99),
    (165, 100),
    (166, 101),
    (167, 102),
    (168, 103),
    (169, 104),
    (170, 105),
    (171, 106),
    (172, 107),
    (173, 108),
    (174, 109),
    (175, 110),
    (177, 111),
    (178, 112),
    (179, 113),
    (180, 114),
    (182, 115),
    (183, 116),
    (184, 117),
    (185, 118),
    (186, 119),
    (187, 120),
    (188, 121),
    (189, 122),
    (191, 123),
    (193, 124),
    (194, 125),
    (195, 126),
    (196, 127),
    (197, 128),
    (198, 129),
    (199, 130),
    (200, 131),
    (202, 132),
    (203, 133),
    (205, 134),
    (206, 135),
    (207, 136),
    (208, 137),
    (225, 138),
    (227, 139),
    (232, 140),
    (233, 141),
    (234, 142),
    (235, 143),
    (241, 144),
    (245, 145),
    (248, 146),
    (249, 147),
    (250, 148),
    (251, 149),
];

/// `cffStandardSID`: maps a byte code to its Standard Encoding SID (0 = .notdef).
pub fn cff_standard_sid(code: u8) -> u16 {
    if (32..=126).contains(&code) {
        return u16::from(code) - 31;
    }
    CFF_STD_ENC_HIGH
        .iter()
        .find(|&&(c, _)| c == code)
        .map_or(0, |&(_, sid)| sid)
}

/// `cffCharsetSIDs`: the SID of every glyph. Offset 0 is the predefined ISOAdobe charset, whose
/// SID is the glyph index; the Expert charsets (1, 2) and unknown formats refuse.
pub fn cff_charset_sids(p: &[u8], offset: usize, num_glyphs: usize) -> Option<Vec<u16>> {
    let mut sids: Vec<u16> = try_vec(num_glyphs)?;
    sids.resize(num_glyphs, 0);
    if offset == 0 {
        for (i, s) in sids.iter_mut().enumerate() {
            *s = i as u16;
        }
        return Some(sids);
    }
    if offset == 1 || offset == 2 || offset >= p.len() {
        return None;
    }
    let d = p.get(offset..)?;
    let mut gid = 1usize;
    match d[0] {
        0 => {
            let mut i = 1usize;
            while gid < num_glyphs {
                if i + 2 > d.len() {
                    return None;
                }
                sids[gid] = be16(d, i);
                i += 2;
                gid += 1;
            }
        }
        1 | 2 => {
            let range_len = if d[0] == 2 { 4 } else { 3 }; // nLeft is u8, or u16 in format 2
            let mut i = 1usize;
            while gid < num_glyphs {
                if i + range_len > d.len() {
                    return None;
                }
                let first = be16(d, i);
                let n_left = if range_len == 4 {
                    usize::from(be16(d, i + 2))
                } else {
                    usize::from(d[i + 2])
                };
                let mut k = 0usize;
                while k <= n_left && gid < num_glyphs {
                    sids[gid] = first.wrapping_add(k as u16);
                    gid += 1;
                    k += 1;
                }
                i += range_len;
            }
        }
        _ => return None,
    }
    Some(sids)
}

/// `cffCode2GID`: builds the code-to-glyph table from the font's builtin encoding. Offset 0 is the
/// Standard Encoding, resolved through the charset (code -> SID -> GID); offset 1 is Expert,
/// refused like the Expert charsets; otherwise a custom format 0 or 1 table maps codes onto glyph
/// order directly, with the optional supplement resolving through SIDs again.
pub fn cff_code2gid(p: &[u8], offset: usize, sids: &[u16]) -> Option<[u16; 256]> {
    let mut sid2gid: HashMap<u16, u16> = HashMap::new();
    for gid in (1..sids.len()).rev() {
        // first glyph wins
        sid2gid.insert(sids[gid], gid as u16);
    }
    let mut codes = [0u16; 256];
    if offset == 0 {
        for (c, slot) in codes.iter_mut().enumerate() {
            *slot = sid2gid
                .get(&cff_standard_sid(c as u8))
                .copied()
                .unwrap_or(0);
        }
        return Some(codes);
    }
    if offset == 1 || offset >= p.len() {
        return None;
    }
    let d = p.get(offset..)?;
    let mut i = 1usize;
    match d[0] & 0x7f {
        0 => {
            if d.len() < 2 {
                return None;
            }
            let n = usize::from(d[1]);
            i += 1;
            if i + n > d.len() {
                return None;
            }
            for gid in 1..=n {
                codes[usize::from(d[i + gid - 1])] = gid as u16;
            }
            i += n;
        }
        1 => {
            if d.len() < 2 {
                return None;
            }
            let n_ranges = usize::from(d[1]);
            i += 1;
            if i + 2 * n_ranges > d.len() {
                return None;
            }
            let mut gid = 1usize;
            for r in 0..n_ranges {
                let first = usize::from(d[i + 2 * r]);
                let n_left = usize::from(d[i + 2 * r + 1]);
                for k in 0..=n_left {
                    if first + k > 255 || gid > 0xffff {
                        return None;
                    }
                    if gid < sids.len() {
                        codes[first + k] = gid as u16;
                    }
                    gid += 1;
                }
            }
            i += 2 * n_ranges;
        }
        _ => return None,
    }
    if d[0] & 0x80 != 0 {
        // supplements: code u8, SID u16
        if i >= d.len() {
            return None;
        }
        let n = usize::from(d[i]);
        i += 1;
        if i + 3 * n > d.len() {
            return None;
        }
        for s in 0..n {
            codes[usize::from(d[i + 3 * s])] =
                sid2gid.get(&be16(d, i + 3 * s + 1)).copied().unwrap_or(0);
        }
    }
    Some(codes)
}

/// Whether `v` is an integer operand in `0..=len`, as the offset operators need.
fn dict_offset(v: &[f64], len: usize) -> Option<usize> {
    let [v0] = *v else {
        return None;
    };
    if v0 != v0.trunc() || v0 < 0.0 || v0 > len as f64 {
        return None;
    }
    Some(v0 as usize)
}

/// `parseCFF`: parses the container structure of a bare CFF, plain or CID-keyed. `None` means the
/// program cannot safely take the CFF path (malformed, or an Expert charset or encoding) and the
/// caller degrades to widths-only.
pub fn parse_cff(p: &[u8]) -> Option<CffProgram<'_>> {
    if p.len() < 4 || p[0] != 1 {
        return None;
    }
    let (_, pos) = cff_index(p, usize::from(p[2]))?; // Name INDEX
    let (tops, pos) = cff_index(p, pos)?;
    let (_, pos) = cff_index(p, pos)?; // String INDEX
    let (gsubrs, _) = cff_index(p, pos)?;
    let top = cff_dict(tops.first()?)?;
    let cid_keyed = top.contains_key(&1230); // ROS
    let mut c = CffProgram {
        charstrings: Vec::new(),
        gsubrs,
        lsubrs: Vec::new(),
        code2gid: [0; 256],
        upem: 1000,
        cid2gid: None,
        fd_subrs: Vec::new(),
        fd_sel: Vec::new(),
    };
    if let Some(m) = top.get(&1207).filter(|m| !m.is_empty()) {
        // FontMatrix: upem = 1/m[0]. Only the uniform axis-aligned [s 0 0 s 0 0] form maps onto
        // sfnt's single unitsPerEm; anything else would render at the wrong shape.
        let &[m0, m1, m2, m3, m4, m5, ..] = m.as_slice() else {
            return None;
        };
        if m0.is_nan() || m0 <= 0.0 || m1 != 0.0 || m2 != 0.0 || m3 != m0 || m4 != 0.0 || m5 != 0.0
        {
            return None;
        }
        let u = (1.0 / m0).round();
        if !(1.0..=65535.0).contains(&u) {
            return None;
        }
        c.upem = u as u16;
    }
    let int_op = |op: i32| -> Option<usize> {
        dict_offset(top.get(&op).map(Vec::as_slice).unwrap_or(&[]), p.len())
    };
    let cs_off = int_op(17)?;
    let (charstrings, _) = cff_index(p, cs_off)?;
    if charstrings.is_empty() {
        return None;
    }
    c.charstrings = charstrings;
    if cid_keyed {
        // CID-keyed: FDArray font DICTs carry the Private DICTs (partitioned by FDSelect) in
        // place of the single top-level one. Both operators are required.
        let (fds, _) = cff_index(p, int_op(1236)?)?; // FDArray
        // 256 is sfnt's maxNumFontDicts: Parse refuses a bigger FDArray, and the cap bounds this
        // loop's own allocations.
        if fds.is_empty() || fds.len() > 256 {
            return None;
        }
        for fd in fds {
            let d = cff_dict(fd)?;
            c.fd_subrs.push(cff_private_subrs(p, &d)?);
        }
        let fd_select_off = int_op(1237)?; // FDSelect
        c.fd_sel = cff_fd_select(p, fd_select_off, c.charstrings.len(), c.fd_subrs.len())?;
    } else {
        c.lsubrs = cff_private_subrs(p, &top)?;
    }
    let charset_off = if top.contains_key(&15) {
        int_op(15)?
    } else {
        0
    };
    let sids = cff_charset_sids(p, charset_off, c.charstrings.len())?;
    if cid_keyed {
        // The charset read as a CID map (its "SIDs" ARE the CIDs); offset 0 reads as the identity
        // map. The lowest GID wins a duplicated CID.
        let mut m = HashMap::with_capacity(sids.len());
        for gid in (1..sids.len()).rev() {
            m.insert(sids[gid], gid as u16);
        }
        c.cid2gid = Some(m);
        return Some(c); // CIDFonts carry no encoding table; codes arrive as CIDs
    }
    let encoding_off = if top.contains_key(&16) {
        int_op(16)?
    } else {
        0
    };
    c.code2gid = cff_code2gid(p, encoding_off, &sids)?;
    Some(c)
}

/// `cffPrivateSubrs`: reads `dict`'s Private operand pair (size then offset) and returns that
/// Private DICT's local subrs: an empty list for no Private at all or one without Subrs, `None`
/// for malformed operands wherever they hide.
pub fn cff_private_subrs<'a>(p: &'a [u8], dict: &HashMap<i32, Vec<f64>>) -> Option<Vec<&'a [u8]>> {
    let Some(v) = dict.get(&18) else {
        return Some(Vec::new());
    };
    let &[size, off] = v.as_slice() else {
        return None;
    };
    if size == 0.0 {
        // sfnt's parsePrivateDICT skips a zero-length DICT without touching the offset.
        return Some(Vec::new());
    }
    if size != size.trunc()
        || off != off.trunc()
        || size < 0.0
        || off <= 0.0
        || off + size > p.len() as f64
    {
        // off <= 0: sfnt refuses a non-empty Private at offset zero.
        return None;
    }
    let off_i = off as usize;
    let private = cff_dict(p.get(off_i..(off + size) as usize)?)?;
    let Some(s) = private.get(&19).filter(|s| !s.is_empty()) else {
        return Some(Vec::new()); // no Subrs
    };
    // Subrs is relative to the Private DICT. Bound BEFORE converting: the float sum cannot
    // overflow, where a 2^63-ish operand converted to an integer would.
    let &[s0] = s.as_slice() else {
        return None;
    };
    if s0 != s0.trunc() || s0 < 0.0 || off + s0 > p.len() as f64 {
        return None;
    }
    let (subrs, _) = cff_index(p, off_i + s0 as usize)?;
    if subrs.len() > 40000 {
        return None; // sfnt's maxNumSubroutines; Parse refuses more
    }
    Some(subrs)
}

/// `cffFDSelect`: resolves every glyph's font DICT index exactly as sfnt's `fdSelect.lookup` will
/// at that glyph's first callsubr: format 0 reads a byte per glyph, format 3 binary-searches
/// `[xlo, xhi)` ranges in table order WITHOUT assuming they are sorted. Mirroring the search
/// rather than validating ranges matters: the gate must agree with sfnt on which local subrs each
/// glyph runs. -1 marks a glyph whose lookup sfnt will refuse (a miss, or an FD past the FDArray).
/// `None` refuses the font: an out-of-bounds, truncated, or unknown-format table.
pub fn cff_fd_select(
    p: &[u8],
    offset: usize,
    num_glyphs: usize,
    num_fds: usize,
) -> Option<Vec<i16>> {
    if offset == 0 || offset >= p.len() {
        return None;
    }
    let d = p.get(offset..)?;
    let mut sel: Vec<i16> = try_vec(num_glyphs)?;
    sel.resize(num_glyphs, 0);
    let fd = |b: u8| -> i16 {
        if usize::from(b) < num_fds {
            i16::from(b)
        } else {
            -1
        }
    };
    match d[0] {
        0 => {
            if d.len() < 1 + num_glyphs {
                return None;
            }
            for (gid, s) in sel.iter_mut().enumerate() {
                *s = fd(d[1 + gid]);
            }
        }
        3 => {
            if d.len() < 3 {
                return None;
            }
            let n_ranges = usize::from(be16(d, 1));
            if d.len() < 3 + 3 * n_ranges + 2 {
                return None;
            }
            for (gid, s) in sel.iter_mut().enumerate() {
                *s = -1;
                let (mut lo, mut hi) = (0usize, n_ranges);
                while lo < hi {
                    let i = (lo + hi) / 2;
                    if gid < usize::from(be16(d, 3 + 3 * i)) {
                        hi = i;
                        continue;
                    }
                    if usize::from(be16(d, 3 + 3 * i + 3)) <= gid {
                        lo = i + 1;
                        continue;
                    }
                    *s = fd(d[3 + 3 * i + 2]);
                    break;
                }
            }
        }
        _ => return None,
    }
    Some(sel)
}

/// `cffSubrBias`: the Type 2 subr-number bias (5177.Type2.pdf section 4.7).
pub fn cff_subr_bias(count: usize) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// Adds `n` executed bytes to `work`; false once the budget is spent.
fn charge(work: &mut i64, n: usize, max_work: i64) -> bool {
    *work += n as i64;
    *work <= max_work
}

/// `stem`, shared by hstem/vstem/hstemhm/vstemhm and hintmask's implicit vstem (t2CStem): consume
/// the optional leading width, then count (never execute) the hint pairs so mask data bytes are
/// skipped correctly. false = sfnt errors here.
fn stem(args: &mut Vec<i32>, seen_width: &mut bool, hint_bits: &mut i32) -> bool {
    if !*seen_width {
        *seen_width = true;
        if args.len() % 2 == 1 {
            args.remove(0);
        }
    }
    if args.len() % 2 == 1 {
        return false;
    }
    *hint_bits += (args.len() / 2) as i32;
    args.clear();
    *hint_bits <= 256 // sfnt's maxHintBits
}

impl<'a> CffProgram<'a> {
    /// `cffWalkGlyph`: re-runs the control flow of one glyph's charstring exactly as sfnt's
    /// psInterpreter will, charging one unit per executed byte against `work`. false means the
    /// walk tripped `max_work`: the call tree is too big to hand sfnt. true means every byte sfnt
    /// will execute was charged and fits, including the malformed cases (reserved operator, bad
    /// subr index, stack over/underflow, truncated stream), where sfnt stops with an error at the
    /// same byte this walk stops at.
    pub fn walk_glyph(
        &self,
        cs: &'a [u8],
        lsubrs: &[&'a [u8]],
        work: &mut i64,
        max_work: i64,
    ) -> bool {
        // sfnt refuses any stream over its 64 KB maxGlyphDataLength before running a byte of it.
        if cs.len() > MAX_STREAM {
            return true;
        }
        let mut args: Vec<i32> = Vec::new();
        let mut call: Vec<&'a [u8]> = Vec::new();
        let mut hint_bits = 0i32;
        let mut seen_width = false;
        let mut ins = cs;
        while let Some(&b) = ins.first() {
            // Operand encodings (5177.Type2.pdf 3.2), each charged as consumed.
            if b == 28 || b >= 32 {
                let n = match b {
                    28 => 3,
                    247..=254 => 2,
                    255 => 5,
                    _ => 1,
                };
                if ins.len() < n || args.len() == 48 {
                    return true; // truncation / stack overflow
                }
                let b1 = i32::from(ins.get(1).copied().unwrap_or(0));
                let v = match b {
                    28 => i32::from(be16(ins, 1) as i16),
                    32..=246 => i32::from(b) - 139,
                    247..=250 => i32::from(b - 247) * 256 + b1 + 108,
                    251..=254 => -i32::from(b - 251) * 256 - b1 - 108,
                    _ => {
                        // 16.16 fixed. sfnt ROUNDS it to an integer before pushing
                        // (postscript.go's parseNumber); mirroring the raw value would let a
                        // bomb hide subr indices behind 255-encoded operands.
                        let v = be32(ins, 1) as i32;
                        (v >> 16) + (1 & (v >> 15))
                    }
                };
                args.push(v);
                ins = &ins[n..];
                if !charge(work, n, max_work) {
                    return false;
                }
                continue;
            }
            ins = &ins[1..];
            if !charge(work, 1, max_work) {
                return false;
            }
            match b {
                1 | 3 | 18 | 23 => {
                    // hstem vstem hstemhm vstemhm
                    if !stem(&mut args, &mut seen_width, &mut hint_bits) {
                        return true;
                    }
                }
                19 | 20 => {
                    // hintmask cntrmask
                    if !args.is_empty() {
                        if !stem(&mut args, &mut seen_width, &mut hint_bits) {
                            return true;
                        }
                    } else if !seen_width {
                        seen_width = true;
                    }
                    let n = ((hint_bits + 7) / 8) as usize;
                    if ins.len() < n {
                        return true;
                    }
                    ins = &ins[n..];
                    if !charge(work, n, max_work) {
                        return false;
                    }
                    args.clear();
                }
                4 | 21 | 22 => {
                    // vmoveto rmoveto hmoveto
                    seen_width = true;
                    args.clear();
                }
                5..=8 | 24..=27 | 30 | 31 => args.clear(), // line/curve families
                10 | 29 => {
                    // callsubr callgsubr
                    let subrs: &[&'a [u8]] = if b == 29 { &self.gsubrs } else { lsubrs };
                    if args.is_empty() || subrs.is_empty() || call.len() == 10 {
                        return true; // psCallStackSize is 10
                    }
                    let top = args.pop().unwrap_or(0);
                    let idx = top.wrapping_add(cff_subr_bias(subrs.len()));
                    let sub = if idx < 0 {
                        None
                    } else {
                        subrs.get(idx as usize)
                    };
                    let Some(&sub) = sub.filter(|s| s.len() <= MAX_STREAM) else {
                        return true;
                    };
                    call.push(ins);
                    ins = sub;
                }
                11 => {
                    // return
                    let Some(back) = call.pop() else {
                        return true;
                    };
                    ins = back;
                }
                14 => return true, // endchar: ends clean, or errors on leftovers
                12 => {
                    // escape: sfnt implements only hflex (34) and hflex1 (36)
                    let Some(&e) = ins.first() else {
                        return true;
                    };
                    ins = &ins[1..];
                    if !charge(work, 1, max_work) {
                        return false;
                    }
                    let pop = match e {
                        34 => 7,
                        36 => 9,
                        _ => return true,
                    };
                    if args.len() < pop {
                        return true;
                    }
                    args.truncate(args.len() - pop);
                }
                _ => return true, // reserved: 0 2 9 13 15 16 17
            }
        }
        true // stream ran out: sfnt's run loop ends here too
    }

    /// `cffGateWork`: walks every reachable glyph (the synthetic cmap's 256 codes, or for a
    /// CID-keyed font all of them) and returns the work charged per GID; `None` means a glyph
    /// tripped `max_work` or a CID font tripped the whole-font cap. GID 0 is never reached. A
    /// nonempty charstring always charges at least one unit, so `work[gid] != 0` doubles as the
    /// walked marker.
    pub fn gate_work(&self, max_work: i64) -> Option<Vec<i64>> {
        let n = self.charstrings.len();
        let mut work: Vec<i64> = try_vec(n)?;
        work.resize(n, 0);
        if self.cid2gid.is_some() {
            // CID-keyed: EVERY glyph is reachable (any two-byte code is a CID), so the whole
            // CharStrings INDEX is walked, each glyph with the local subrs of ITS font DICT,
            // and the font as a whole is capped at the 256-code gate's ceiling.
            let mut total = 0i64;
            for gid in 1..n {
                let lsubrs: &[&'a [u8]] = match self.fd_sel.get(gid) {
                    Some(&fd) if fd >= 0 => self
                        .fd_subrs
                        .get(fd as usize)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    _ => &[],
                };
                if !self.walk_glyph(self.charstrings[gid], lsubrs, &mut work[gid], max_work) {
                    return None;
                }
                total += work[gid];
                if total > max_work.saturating_mul(256) {
                    return None;
                }
            }
            return Some(work);
        }
        for &gid in &self.code2gid {
            let gid = usize::from(gid);
            if gid == 0 || gid >= n || work[gid] != 0 {
                continue;
            }
            if !self.walk_glyph(
                self.charstrings[gid],
                &self.lsubrs,
                &mut work[gid],
                max_work,
            ) {
                return None;
            }
        }
        Some(work)
    }
}

/// `cffWrapSFNT`: wraps the raw CFF blob as the `CFF ` table of a minimal OTTO container carrying
/// exactly the tables sfnt.Parse requires. head carries the upem; hhea/hmtx declare one zero
/// metric (PDF /Widths drive advances); the version-2 OS/2 stops Parse's metrics fallback from
/// loading glyphs; the format-6 cmap is the builtin encoding, one entry per byte code.
pub fn cff_wrap_sfnt(
    cff: &[u8],
    num_glyphs: usize,
    upem: u16,
    code2gid: &[u16; 256],
) -> Option<Vec<u8>> {
    let mut head = [0u8; 54];
    head[18..20].copy_from_slice(&upem.to_be_bytes());

    let mut maxp = Vec::with_capacity(6);
    maxp.extend_from_slice(&0x0000_5000u32.to_be_bytes());
    maxp.extend_from_slice(&(num_glyphs as u16).to_be_bytes());

    let mut hhea = [0u8; 36];
    hhea[35] = 1; // numberOfHMetrics = 1

    let hmtx = [0u8; 4];

    let mut sub: Vec<u8> = Vec::with_capacity(10 + 2 * code2gid.len());
    sub.extend_from_slice(&6u16.to_be_bytes()); // format 6
    sub.extend_from_slice(&((10 + 2 * code2gid.len()) as u16).to_be_bytes());
    sub.extend_from_slice(&0u16.to_be_bytes()); // language
    sub.extend_from_slice(&0u16.to_be_bytes()); // firstCode
    sub.extend_from_slice(&(code2gid.len() as u16).to_be_bytes());
    for g in code2gid {
        sub.extend_from_slice(&g.to_be_bytes());
    }
    let mut cmap: Vec<u8> = Vec::with_capacity(12 + sub.len());
    cmap.extend_from_slice(&0u16.to_be_bytes()); // version
    cmap.extend_from_slice(&1u16.to_be_bytes()); // one subtable: Windows Unicode BMP
    cmap.extend_from_slice(&3u16.to_be_bytes());
    cmap.extend_from_slice(&1u16.to_be_bytes());
    cmap.extend_from_slice(&12u32.to_be_bytes());
    cmap.extend_from_slice(&sub);

    let mut os2 = [0u8; 96];
    os2[1] = 2; // version 2: xHeight/capHeight read from the table, no fallback

    let mut post = [0u8; 32];
    post[1] = 3; // version 3.0: no glyph names

    // ascending tag order, as sfnt checks
    let tables: [(u32, &[u8]); 8] = [
        (0x4346_4620, cff),      // "CFF "
        (0x4f53_2f32, &os2[..]), // "OS/2"
        (0x636d_6170, &cmap[..]),
        (0x6865_6164, &head[..]),
        (0x6868_6561, &hhea[..]),
        (0x686d_7478, &hmtx[..]),
        (0x6d61_7870, &maxp[..]),
        (0x706f_7374, &post[..]),
    ];
    let mut total = 12 + 16 * tables.len();
    for (_, data) in &tables {
        total = total.checked_add(data.len().checked_add(3)? & !3)?;
    }
    let mut b: Vec<u8> = try_vec(total)?;
    b.extend_from_slice(&0x4f54_544fu32.to_be_bytes()); // OTTO
    b.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    b.extend_from_slice(&[0; 6]); // searchRange trio: unchecked
    let mut off = 12 + 16 * tables.len();
    for (tag, data) in &tables {
        b.extend_from_slice(&tag.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // checksum: unchecked
        b.extend_from_slice(&(off as u32).to_be_bytes());
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        off += (data.len() + 3) & !3;
    }
    for (_, data) in &tables {
        b.extend_from_slice(data);
        let pad = b.len().wrapping_neg() & 3; // tables must begin 4-aligned
        b.resize(b.len() + pad, 0);
    }
    Some(b)
}

/// `cffToSFNT`: the CFF entry point. A bare CFF in, an `sfnt.Parse`-able wrapper plus the
/// gate-measured per-GID charstring work out, or `None` when the program is not a CFF this path
/// can safely hand to sfnt (see [`parse_cff`] and [`CffProgram::gate_work`]).
pub fn cff_to_sfnt(p: &[u8]) -> Option<CffSfnt> {
    cff_to_sfnt_with_budget(p, MAX_CHARSTRING_WORK)
}

/// [`cff_to_sfnt`] with an explicit per-glyph work budget (Go's tests lower the package variable).
pub fn cff_to_sfnt_with_budget(p: &[u8], max_work: i64) -> Option<CffSfnt> {
    let c = parse_cff(p)?;
    let gate_work = c.gate_work(max_work)?;
    let otf = cff_wrap_sfnt(p, c.charstrings.len(), c.upem, &c.code2gid)?;
    Some(CffSfnt {
        otf,
        gate_work,
        cid2gid: c.cid2gid,
    })
}

#[cfg(test)]
mod tests {
    use super::super::glyf::parse_glyf_index;
    use super::*;

    // ---- minimal bare-CFF builder ----
    //
    // Hand-assembled like the Go fixtures: it can build the hostile variants (subr bombs, CID
    // keys) no legitimate tool will emit. Layout follows 5176.CFF.pdf; every offset is a fixed
    // 5-byte integer (operand 29) so section offsets are computable in one pass.

    fn be16v(mut b: Vec<u8>, v: u16) -> Vec<u8> {
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    /// Appends a Type 2 charstring integer operand (5177.Type2.pdf 3.2).
    fn t2i(b: &mut Vec<u8>, v: i32) {
        match v {
            -107..=107 => b.push((v + 139) as u8),
            108..=1131 => {
                let v = v - 108;
                b.extend_from_slice(&[(247 + v / 256) as u8, (v % 256) as u8]);
            }
            -1131..=-108 => {
                let v = -v - 108;
                b.extend_from_slice(&[(251 + v / 256) as u8, (v % 256) as u8]);
            }
            _ => b.extend_from_slice(&[28, (v >> 8) as u8, v as u8]),
        }
    }

    enum It<'a> {
        I(i32),
        Op(&'a str),
        B(&'a [u8]),
    }

    impl From<i32> for It<'_> {
        fn from(v: i32) -> Self {
            It::I(v)
        }
    }
    impl<'a> From<&'a str> for It<'a> {
        fn from(v: &'a str) -> Self {
            It::Op(v)
        }
    }
    impl<'a> From<&'a [u8]> for It<'a> {
        fn from(v: &'a [u8]) -> Self {
            It::B(v)
        }
    }
    impl<'a> From<&'a Vec<u8>> for It<'a> {
        fn from(v: &'a Vec<u8>) -> Self {
            It::B(v)
        }
    }

    fn t2_assemble(items: Vec<It>) -> Vec<u8> {
        let mut b = Vec::new();
        for it in items {
            match it {
                It::I(v) => t2i(&mut b, v),
                It::Op(name) => {
                    let op: &[u8] = match name {
                        "hstem" => &[1],
                        "vstem" => &[3],
                        "vmoveto" => &[4],
                        "rlineto" => &[5],
                        "hlineto" => &[6],
                        "vlineto" => &[7],
                        "rrcurveto" => &[8],
                        "callsubr" => &[10],
                        "return" => &[11],
                        "endchar" => &[14],
                        "hstemhm" => &[18],
                        "hintmask" => &[19],
                        "rmoveto" => &[21],
                        "hmoveto" => &[22],
                        "callgsubr" => &[29],
                        other => panic!("t2cs: unknown op {other}"),
                    };
                    b.extend_from_slice(op);
                }
                It::B(v) => b.extend_from_slice(v), // raw bytes, e.g. hintmask data
            }
        }
        b
    }

    /// `t2cs`: ints become operands, strings the named operator, byte slices raw bytes.
    macro_rules! t2cs {
        ($($x:expr),* $(,)?) => { t2_assemble(vec![$(It::from($x)),*]) };
    }

    /// A CFF INDEX with 2-byte offsets (an empty INDEX is the bare 2-byte zero count).
    fn cff_index_bytes(items: &[Vec<u8>]) -> Vec<u8> {
        let mut b = be16v(Vec::new(), items.len() as u16);
        if items.is_empty() {
            return b;
        }
        b.push(2);
        let mut off = 1u16;
        b = be16v(b, off);
        for it in items {
            off += it.len() as u16;
            b = be16v(b, off);
        }
        for it in items {
            b.extend_from_slice(it);
        }
        b
    }

    /// A DICT integer operand in fixed 5-byte form (operand 29).
    fn dict_int(mut b: Vec<u8>, v: i32) -> Vec<u8> {
        b.push(29);
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    #[derive(Default)]
    struct CffOpts {
        /// charstrings[0] is .notdef.
        charstrings: Vec<Vec<u8>>,
        /// Charset SIDs for glyphs 1..; `None` = ISOAdobe (SID = GID).
        sids: Option<Vec<u16>>,
        /// Raw Encoding table; `None` = predefined standard.
        encoding: Option<Vec<u8>>,
        gsubrs: Vec<Vec<u8>>,
        lsubrs: Option<Vec<Vec<u8>>>,
        /// String INDEX entries (SIDs 391..).
        strings: Vec<Vec<u8>>,
        /// Extra Top DICT bytes, e.g. a FontMatrix.
        top_extra: Vec<u8>,
        /// Raw Private DICT override (else derived from lsubrs).
        private: Option<Vec<u8>>,
        /// CID-keyed: a `Some` makes the font CID-keyed. One font DICT per entry, each with a
        /// Private whose Subrs are that entry when it is `Some`, no top-level Private, and the
        /// raw `fd_select` table; `sids` are then CIDs.
        fd_subrs: Option<Vec<Option<Vec<Vec<u8>>>>>,
        fd_select: Vec<u8>,
    }

    /// Assembles a bare CFF (the exact bytes a /FontFile3 /Subtype /Type1C stream carries).
    fn build_cff(o: &CffOpts) -> Vec<u8> {
        let header = [1u8, 0, 4, 2];
        let name = cff_index_bytes(&[b"BbOracle".to_vec()]);
        let strs = cff_index_bytes(&o.strings);
        let gsubrs = cff_index_bytes(&o.gsubrs);
        let char_strings = cff_index_bytes(&o.charstrings);
        let mut lsubrs = Vec::new();
        let mut private = Vec::new();
        if let Some(l) = &o.lsubrs {
            lsubrs = cff_index_bytes(l);
            private = dict_int(Vec::new(), 6); // Subrs at 6 = len(private)
            private.push(19);
        }
        if let Some(p) = &o.private {
            private = p.clone();
        }
        let mut charset = Vec::new();
        if let Some(sids) = &o.sids {
            charset.push(0); // format 0
            for &sid in sids {
                charset = be16v(charset, sid);
            }
        }
        let encoding = o.encoding.clone().unwrap_or_default();

        let mut dict_len = 6 + o.top_extra.len(); // CharStrings (+ extras)
        dict_len += if o.fd_subrs.is_none() { 11 } else { 17 + 7 + 7 }; // Private | ROS+FDArray+FDSelect
        if o.sids.is_some() {
            dict_len += 6;
        }
        if o.encoding.is_some() {
            dict_len += 6;
        }
        let top_index_len = 2 + 1 + 2 + 2 + dict_len;
        let charset_off = header.len() + name.len() + top_index_len + strs.len() + gsubrs.len();
        let encoding_off = charset_off + charset.len();
        let char_strings_off = encoding_off + encoding.len();
        let private_off = char_strings_off + char_strings.len();
        let fd_select_off = private_off + private.len() + lsubrs.len();
        let fd_array_off = fd_select_off + o.fd_select.len();

        // FDArray: every font DICT is the fixed 11 bytes of a Private operand pair, so the INDEX
        // length, which the private offsets inside it depend on, is known up front.
        let mut fd_array = Vec::new();
        let mut fd_tail = Vec::new();
        if let Some(fds) = &o.fd_subrs {
            let mut pos = fd_array_off + 2 + 1 + 2 * (fds.len() + 1) + 11 * fds.len();
            let mut font_dicts = Vec::new();
            for subs in fds {
                let (mut priv_, mut sub) = (Vec::new(), Vec::new());
                if let Some(subs) = subs {
                    priv_ = dict_int(Vec::new(), 6); // Subrs at 6 = len(priv)
                    priv_.push(19);
                    sub = cff_index_bytes(subs);
                }
                let mut fdict = dict_int(dict_int(Vec::new(), priv_.len() as i32), pos as i32);
                fdict.push(18);
                font_dicts.push(fdict);
                fd_tail.extend_from_slice(&priv_);
                fd_tail.extend_from_slice(&sub);
                pos += priv_.len() + sub.len();
            }
            fd_array = cff_index_bytes(&font_dicts);
        }

        let mut top = Vec::new();
        if o.sids.is_some() {
            top = dict_int(top, charset_off as i32);
            top.push(15);
        }
        if o.encoding.is_some() {
            top = dict_int(top, encoding_off as i32);
            top.push(16);
        }
        top = dict_int(top, char_strings_off as i32);
        top.push(17);
        if o.fd_subrs.is_none() {
            top = dict_int(top, private.len() as i32);
            top = dict_int(top, private_off as i32);
            top.push(18);
        } else {
            top = dict_int(dict_int(dict_int(top, 391), 392), 0);
            top.extend_from_slice(&[12, 30]); // ROS
            top = dict_int(top, fd_array_off as i32);
            top.extend_from_slice(&[12, 36]);
            top = dict_int(top, fd_select_off as i32);
            top.extend_from_slice(&[12, 37]);
        }
        top.extend_from_slice(&o.top_extra);
        let top_index = cff_index_bytes(&[top]);
        assert_eq!(top_index.len(), top_index_len, "buildCFF: top index layout");

        let mut b = Vec::new();
        b.extend_from_slice(&header);
        for part in [
            &name,
            &top_index,
            &strs,
            &gsubrs,
            &charset,
            &encoding,
            &char_strings,
            &private,
            &lsubrs,
            &o.fd_select,
            &fd_array,
            &fd_tail,
        ] {
            b.extend_from_slice(part);
        }
        b
    }

    /// A format-3 FDSelect from `(firstGID, fd)` ranges and the numGlyphs sentinel.
    fn fd_select3(num_glyphs: u16, ranges: &[(u16, u8)]) -> Vec<u8> {
        let mut b = be16v(vec![3], ranges.len() as u16);
        for &(first, fd) in ranges {
            b = be16v(b, first);
            b.push(fd);
        }
        be16v(b, num_glyphs)
    }

    /// The 500x500 axis-aligned square at the origin as a charstring.
    fn cff_square() -> Vec<u8> {
        t2cs!(
            0, 0, "rmoveto", 500, "hlineto", 500, "vlineto", -500, "hlineto", "endchar"
        )
    }

    /// 'A' (SID 34, standard encoding) is the square, as a bare CFF.
    fn cff_square_font() -> Vec<u8> {
        build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cff_square()],
            sids: Some(vec![34]),
            ..CffOpts::default()
        })
    }

    /// The CID fixture: CID 7 (GID 1 through the charset-as-CID-map) is the square, drawn entirely
    /// by FD 1's local subr, while FD 0 has no subrs at all.
    fn cid_square_font() -> Vec<u8> {
        build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![7]), // charset: GID 1 -> CID 7
            strings: vec![b"Adobe".to_vec(), b"Identity".to_vec()],
            fd_subrs: Some(vec![
                None,
                Some(vec![t2cs!(
                    0, 0, "rmoveto", 500, "hlineto", 500, "vlineto", -500, "hlineto", "return"
                )]),
            ]),
            fd_select: fd_select3(2, &[(0, 0), (1, 1)]),
            ..CffOpts::default()
        })
    }

    /// The 10-deep local-subr work bomb: subr i calls subr i+1 two hundred times and the last
    /// returns, so the full chain would execute ~200^9 charstring bytes.
    fn bomb_subrs() -> Vec<Vec<u8>> {
        const CHAIN: i32 = 10;
        (0..CHAIN)
            .map(|i| {
                let mut s = Vec::new();
                if i < CHAIN - 1 {
                    for _ in 0..200 {
                        s = t2cs!(&s, i + 1 - 107, "callsubr");
                    }
                }
                t2cs!(&s, "return")
            })
            .collect()
    }

    /// Nibble-encodes a DICT real (operand 30) from its decimal spelling.
    fn t30(s: &str) -> Vec<u8> {
        let mut nibs: Vec<u8> = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                c @ b'0'..=b'9' => nibs.push(c - b'0'),
                b'.' => nibs.push(0x0a),
                b'E' if b.get(i + 1) == Some(&b'-') => {
                    nibs.push(0x0c);
                    i += 1;
                }
                b'E' => nibs.push(0x0b),
                b'-' => nibs.push(0x0e),
                _ => panic!("t30: bad char"),
            }
            i += 1;
        }
        nibs.push(0x0f);
        if nibs.len() % 2 == 1 {
            nibs.push(0x0f);
        }
        let mut out = vec![30];
        out.extend(nibs.chunks(2).map(|c| (c[0] << 4) | c[1]));
        out
    }

    /// A one-glyph sled of `n` hstem bytes then a return, as a subr.
    fn hstem_sled(n: usize) -> Vec<u8> {
        let mut sled = vec![1u8; n];
        sled.extend_from_slice(&t2cs!("return"));
        sled
    }

    #[test]
    fn type1c_program_takes_the_cff_path() {
        let p = cff_square_font();
        assert!(
            parse_glyf_index(&p).is_none(),
            "bare CFF parsed as an indexable TrueType"
        );
        assert!(
            cff_to_sfnt(&p).is_some(),
            "cff_to_sfnt refused the minimal well-formed bare CFF"
        );
    }

    #[test]
    fn square_program_structure() {
        let p = cff_square_font();
        let c = parse_cff(&p).expect("parses");
        assert_eq!(c.upem, 1000);
        assert_eq!(c.charstrings.len(), 2);
        assert_eq!(c.code2gid[usize::from(b'A')], 1);
        assert_eq!(c.code2gid[usize::from(b'B')], 0);
        let w = cff_to_sfnt(&p).expect("wraps");
        assert_eq!(&w.otf[..4], b"OTTO");
        assert_eq!(w.otf.len() % 4, 0);
        assert!(w.cid2gid.is_none());
        // The square charstring is 13 bytes; glyph 0 is never walked.
        assert_eq!(w.gate_work, vec![0, 13]);
    }

    #[test]
    fn hostile_subr_bomb_refused_before_parse() {
        let bomb = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![34]),
            lsubrs: Some(bomb_subrs()),
            ..CffOpts::default()
        });
        assert!(
            cff_to_sfnt(&bomb).is_none(),
            "the subr work bomb was not refused before sfnt.Parse"
        );
    }

    #[test]
    fn hostile_fixed_encoded_call_still_gated() {
        // -107 (subr 0 under the bias) as 16.16 fixed: -107<<16 = 0xFF950000.
        let entry = t2cs!(&[255u8, 0xff, 0x95, 0x00, 0x00][..], "callsubr", "endchar");
        let bomb = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), entry],
            sids: Some(vec![34]),
            lsubrs: Some(bomb_subrs()),
            ..CffOpts::default()
        });
        assert!(
            cff_to_sfnt(&bomb).is_none(),
            "the fixed-encoded entry call hid the subr bomb from the work gate"
        );
    }

    #[test]
    fn hostile_self_recursion_cheap() {
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![34]),
            lsubrs: Some(vec![t2cs!(-107, "callsubr", "return")]),
            ..CffOpts::default()
        });
        assert!(
            cff_to_sfnt(&p).is_some(),
            "depth-limited self-recursion is cheap under sfnt's own wall; the gate must not refuse it"
        );
    }

    #[test]
    fn subrs_and_hints_are_admitted() {
        let lsub = t2cs!(0, 0, "rmoveto", 500, "hlineto", "return");
        let gsub = t2cs!(500, "vlineto", -500, "hlineto", "return");
        let cs = t2cs!(-107, "callsubr", -107, "callgsubr", "endchar");
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cs],
            sids: Some(vec![34]),
            lsubrs: Some(vec![lsub]),
            gsubrs: vec![gsub],
            ..CffOpts::default()
        });
        let w = cff_to_sfnt(&p).expect("subrs admitted");
        assert!(w.gate_work[1] > 0);
        // Stem hints and a hintmask before the outline: the mask's data byte is skipped as data.
        let cs = t2cs!(
            0,
            500,
            "hstemhm",
            0,
            500,
            "hintmask",
            &[0xf0u8][..],
            0,
            0,
            "rmoveto",
            500,
            "hlineto",
            "endchar"
        );
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cs],
            sids: Some(vec![34]),
            ..CffOpts::default()
        });
        assert!(cff_to_sfnt(&p).is_some());
    }

    #[test]
    fn encodings_map_codes_to_glyph_1() {
        let square = cff_square();
        let cases: [(&str, CffOpts, u8); 3] = [
            (
                "standard-high",
                CffOpts {
                    charstrings: vec![t2cs!("endchar"), square.clone()],
                    sids: Some(vec![114]),
                    ..CffOpts::default()
                },
                180,
            ),
            (
                "custom-format0",
                CffOpts {
                    charstrings: vec![t2cs!("endchar"), square.clone()],
                    sids: Some(vec![391]), // an arbitrary non-standard SID
                    encoding: Some(vec![0, 1, b'Q']),
                    ..CffOpts::default()
                },
                b'Q',
            ),
            (
                "format1-range-with-supplement",
                CffOpts {
                    charstrings: vec![t2cs!("endchar"), square],
                    sids: Some(vec![391]),
                    // One range (code 200, 0 left = glyph 1 at 200), plus a supplement mapping
                    // code 65 to SID 391 = glyph 1 as well.
                    encoding: Some(vec![0x81, 1, 200, 0, 1, 65, 0x01, 0x87]),
                    ..CffOpts::default()
                },
                b'A',
            ),
        ];
        for (name, o, code) in cases {
            let p = build_cff(&o);
            let c = parse_cff(&p).unwrap_or_else(|| panic!("{name}: refused"));
            assert_eq!(c.code2gid[usize::from(code)], 1, "{name}");
        }
        // The format-1 case also maps code 200 through the range.
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cff_square()],
            sids: Some(vec![391]),
            encoding: Some(vec![0x81, 1, 200, 0, 1, 65, 0x01, 0x87]),
            ..CffOpts::default()
        });
        assert_eq!(parse_cff(&p).expect("parses").code2gid[200], 1);
    }

    #[test]
    fn cid_keyed_takes_the_cff_path() {
        let p = cid_square_font();
        assert!(
            parse_glyf_index(&p).is_none(),
            "CID-keyed CFF parsed as an indexable TrueType"
        );
        let w = cff_to_sfnt(&p).expect("cff_to_sfnt refused the minimal well-formed CID-keyed CFF");
        let cid2gid = w.cid2gid.expect("CID-keyed");
        assert_eq!(cid2gid.get(&7), Some(&1), "charset-as-CID-map: want 7->1");
        // Both the empty FD 0 and FD 1's subr resolved: glyph 1 ran FD 1's 12-byte subr.
        assert!(w.gate_work[1] > 3);
    }

    /// `cffFDSelect` mirrors sfnt's `fdSelect.lookup`, which bisects the ranges in table order
    /// WITHOUT checking they are sorted: here the bisection lands on FD 2 for gids 20 and 39 where
    /// a first-match linear scan would report FD 0.
    #[test]
    fn fd_select_unsorted_format3_matches_sfnt() {
        let mut p = vec![0xffu8];
        p.extend(fd_select3(100, &[(0, 0), (40, 1), (10, 2), (60, 3)]));
        let sel = cff_fd_select(&p, 1, 100, 4).expect("a well-formed (if unsorted) table");
        for (gid, fd) in [(0, 0), (5, 0), (20, 2), (39, 2), (45, 2), (70, 3), (99, 3)] {
            assert_eq!(sel[gid], fd, "gid {gid} (sfnt's bisection order)");
        }
    }

    /// sfnt.Parse refuses an FDArray over 256 entries and a Subrs INDEX over 40000, so the gate
    /// must refuse them before its own per-FD parse. One at each cap exactly stays admitted.
    #[test]
    fn hostile_cid_counts_capped_like_sfnt() {
        let cid = |n_fd: usize, n_subrs: usize| {
            let mut fd_subrs: Vec<Option<Vec<Vec<u8>>>> = vec![None; n_fd];
            if n_subrs > 0 {
                fd_subrs[0] = Some(vec![Vec::new(); n_subrs]); // zero-length subrs: tiny file
            }
            build_cff(&CffOpts {
                charstrings: vec![t2cs!("endchar"), t2cs!("endchar")],
                sids: Some(vec![7]),
                fd_subrs: Some(fd_subrs),
                fd_select: fd_select3(2, &[(0, 0)]),
                ..CffOpts::default()
            })
        };
        assert!(cff_to_sfnt(&cid(257, 0)).is_none(), "257-entry FDArray");
        assert!(cff_to_sfnt(&cid(256, 0)).is_some(), "256-entry FDArray");
        assert!(cff_to_sfnt(&cid(1, 40001)).is_none(), "40001-entry Subrs");
        assert!(cff_to_sfnt(&cid(1, 40000)).is_some(), "40000-entry Subrs");
    }

    #[test]
    fn hostile_cid_subr_bomb_behind_fd_select() {
        let bomb = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![7]),
            fd_subrs: Some(vec![Some(vec![t2cs!("return")]), Some(bomb_subrs())]),
            fd_select: fd_select3(2, &[(0, 0), (1, 1)]),
            ..CffOpts::default()
        });
        assert!(
            cff_to_sfnt(&bomb).is_none(),
            "the subr work bomb behind FDSelect was not refused before sfnt.Parse"
        );
    }

    /// In a CID font EVERY glyph is reachable, so the whole-font walk stops at 256 x the
    /// per-glyph budget while a font under that total is admitted.
    #[test]
    fn hostile_cid_total_gate_budget() {
        let font = |n: u16| {
            let mut sled = vec![1u8; 99];
            sled.extend_from_slice(&t2cs!("endchar")); // 100 units, at the per-glyph cap
            let mut cs = vec![t2cs!("endchar")];
            let mut sids = Vec::new();
            for i in 1..=n {
                cs.push(sled.clone());
                sids.push(i);
            }
            build_cff(&CffOpts {
                charstrings: cs,
                sids: Some(sids),
                fd_subrs: Some(vec![None]),
                fd_select: fd_select3(n + 1, &[(0, 0)]),
                ..CffOpts::default()
            })
        };
        assert!(
            cff_to_sfnt_with_budget(&font(300), 100).is_none(),
            "30000 total walk units under a 25600 ceiling were not refused"
        );
        assert!(
            cff_to_sfnt_with_budget(&font(200), 100).is_some(),
            "a CID font within the total gate budget must be admitted"
        );
    }

    /// An FDSelect entry past the FDArray is what sfnt refuses at that glyph's first callsubr:
    /// the font is admitted.
    #[test]
    fn cid_fd_past_fd_array_is_admitted() {
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![7]),
            fd_subrs: Some(vec![
                None,
                Some(vec![t2cs!(
                    0, 0, "rmoveto", 500, "hlineto", 500, "vlineto", -500, "hlineto", "return"
                )]),
            ]),
            fd_select: fd_select3(2, &[(0, 0), (1, 9)]),
            ..CffOpts::default()
        });
        assert!(cff_to_sfnt(&p).is_some());
    }

    #[test]
    fn work_gate_budget_trips() {
        let p = cff_square_font();
        assert!(cff_to_sfnt_with_budget(&p, 3).is_none(), "the gate is dead");
        assert!(cff_to_sfnt_with_budget(&p, MAX_CHARSTRING_WORK).is_some());
    }

    /// Every truncation and a corruption sweep of a well-formed CFF, plain and CID-keyed: refuse
    /// or accept, but never panic.
    #[test]
    fn malformed_no_panic() {
        for good in [cff_square_font(), cid_square_font()] {
            for i in 0..=good.len() {
                let _ = cff_to_sfnt(&good[..i]);
            }
            for i in 0..good.len() {
                let mut bad = good.clone();
                bad[i] ^= 0xff;
                let _ = cff_to_sfnt(&bad);
            }
        }
    }

    #[test]
    fn width_prefix_and_huge_operands_are_admitted() {
        let cs = t2cs!(
            600, 0, 0, "rmoveto", 500, "hlineto", 500, "vlineto", -500, "hlineto", "endchar"
        );
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cs],
            sids: Some(vec![34]),
            ..CffOpts::default()
        });
        assert!(cff_to_sfnt(&p).is_some());
        let mut cs = t2cs!(0, 0, "rmoveto");
        for _ in 0..20 {
            cs.extend_from_slice(&[255, 0x7f, 0xff, 0xff, 0xff, 255, 0x7f, 0xff, 0xff, 0xff]);
            cs.extend_from_slice(&t2cs!("rlineto"));
        }
        cs.extend_from_slice(&t2cs!("endchar"));
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cs],
            sids: Some(vec![34]),
            ..CffOpts::default()
        });
        assert!(cff_to_sfnt(&p).is_some());
    }

    /// Real Type1C fonts spell FontMatrix with operand 30's nibble reals. A 1/2048 matrix must
    /// parse to upem 2048 through every nibble branch, and a non-uniform matrix must refuse.
    #[test]
    fn font_matrix_real_operands() {
        let font = |m3: &str| {
            let mut mtx = Vec::new();
            for s in ["0.00048828125", "0", "0", m3, "-0", "0E1"] {
                mtx.extend(t30(s));
            }
            mtx.extend_from_slice(&[12, 7]); // FontMatrix
            build_cff(&CffOpts {
                charstrings: vec![t2cs!("endchar"), cff_square()],
                sids: Some(vec![34]),
                top_extra: mtx,
                ..CffOpts::default()
            })
        };
        let p = font("4.8828125E-4"); // == 0.00048828125 = 1/2048 exactly
        let c = parse_cff(&p).expect("uniform real FontMatrix");
        assert_eq!(c.upem, 2048);
        let p = font("9.765625E-4");
        assert!(
            parse_cff(&p).is_none(),
            "non-uniform FontMatrix must refuse to widths-only"
        );
    }

    #[test]
    fn charset_formats() {
        const OFF: usize = 10;
        let pad = [0u8; OFF];
        let cases: [(&str, &[u8], usize, &[u16]); 3] = [
            ("isoadobe-predefined", &[], 0, &[0, 1, 2, 3, 4]),
            // format 1: first SID u16, nLeft u8 per range.
            (
                "format1",
                &[1, 0, 100, 2, 0, 200, 0],
                OFF,
                &[0, 100, 101, 102, 200],
            ),
            // format 2: first SID u16, nLeft u16 per range. 300 = 0x012C.
            ("format2", &[2, 1, 44, 0, 3], OFF, &[0, 300, 301, 302, 303]),
        ];
        for (name, data, off, want) in cases {
            let mut p = pad.to_vec();
            p.extend_from_slice(data);
            let got = cff_charset_sids(&p, off, want.len()).unwrap_or_default();
            assert_eq!(got, want, "{name}");
        }
        for expert in [1, 2] {
            assert!(
                cff_charset_sids(&pad, expert, 2).is_none(),
                "predefined Expert charset (offset {expert}) must refuse"
            );
        }
    }

    /// `cffIndex`'s overflow-safe bound: a position near the integer maximum must refuse, not wrap.
    #[test]
    fn index_huge_pos_refused() {
        let buf = [0u8; 100];
        for pos in [
            usize::MAX,
            usize::MAX - 1,
            usize::MAX - 2,
            i64::MAX as usize,
        ] {
            assert!(cff_index(&buf, pos).is_none(), "accepted pos={pos}");
        }
    }

    /// The Private DICT Subrs operand is the one offset that is relative; a real-encoded ~2^63
    /// operand must refuse to widths-only.
    #[test]
    fn subrs_offset_overflow_refused() {
        let mut private = t30("9223372036854774784"); // 2^63-1024, exactly representable
        private.push(19);
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), cff_square()],
            sids: Some(vec![34]),
            private: Some(private),
            ..CffOpts::default()
        });
        assert!(
            cff_to_sfnt(&p).is_none(),
            "a Subrs offset past the end of the program must refuse"
        );
    }

    /// A glyph can execute ~60 KB of charstring (admitted by the gate) yet emit zero segments; the
    /// per-GID gate work is what the renderer charges per show.
    #[test]
    fn zero_segment_glyph_work_is_measured() {
        let p = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![34]),
            lsubrs: Some(vec![hstem_sled(60_000)]),
            ..CffOpts::default()
        });
        let w = cff_to_sfnt(&p).expect("the hstem sled must be ADMITTED");
        assert!(w.gate_work[1] >= 60_000, "work {}", w.gate_work[1]);

        let cid = build_cff(&CffOpts {
            charstrings: vec![t2cs!("endchar"), t2cs!(-107, "callsubr", "endchar")],
            sids: Some(vec![7]),
            fd_subrs: Some(vec![Some(vec![hstem_sled(60_000)])]),
            fd_select: fd_select3(2, &[(0, 0)]),
            ..CffOpts::default()
        });
        let w = cff_to_sfnt(&cid).expect("the CID hstem sled must be ADMITTED");
        assert!(w.gate_work[1] >= 60_000, "work {}", w.gate_work[1]);
    }

    #[test]
    fn dict_reals_and_range_errors() {
        // A real whose value is out of float64 range is Go's ErrRange: refused.
        let mut d = t30("1E999");
        d.push(0);
        assert!(cff_dict(&d).is_none());
        let mut d = t30("-2.25");
        d.push(0);
        assert_eq!(cff_dict(&d).expect("dict").get(&0), Some(&vec![-2.25]));
        // A doubled marker errors.
        let mut d = t30("1.2.3");
        d.push(0);
        assert!(cff_dict(&d).is_none());
        // Reserved operators refuse; unknown ones are kept.
        assert!(cff_dict(&[22]).is_none());
        assert!(cff_dict(&[139, 12]).is_none(), "truncated escape");
        let m = cff_dict(&[139, 12, 7, 140, 5]).expect("dict");
        assert_eq!(m.get(&1207), Some(&vec![0.0]));
        assert_eq!(m.get(&5), Some(&vec![1.0]));
    }

    #[test]
    fn standard_encoding_sids() {
        assert_eq!(cff_standard_sid(b' '), 1);
        assert_eq!(cff_standard_sid(b'A'), 34);
        assert_eq!(cff_standard_sid(126), 95);
        assert_eq!(cff_standard_sid(161), 96);
        assert_eq!(cff_standard_sid(251), 149);
        assert_eq!(cff_standard_sid(176), 0);
        assert_eq!(cff_standard_sid(0), 0);
        assert_eq!(cff_standard_sid(255), 0);
    }
}
