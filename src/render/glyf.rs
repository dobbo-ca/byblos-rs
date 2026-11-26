//! Pre-flight bounding of TrueType glyph expansion: port of byblos `internal/render/glyf.go`.
//!
//! sfnt materialises a compound glyph's whole expansion when it loads one, and `sfnt::Parse`
//! itself loads the glyphs for `x` and `H`, so a hostile font must be bounded before it is
//! parsed. [`parse_glyf_index`] indexes the raw `loca`/`glyf` tables of a font program and
//! [`GlyfIndex::bounded_by`] checks every glyph's expanded point count against a limit.
//! [`be16`] and [`be32`] stand in for Go's `binary.BigEndian` of the package.

/// Big-endian `u16` at `i`; 0 when the bytes are not there. Every caller checks its bounds first,
/// as the Go code does, so the fallback only keeps hostile input from panicking.
pub fn be16(p: &[u8], i: usize) -> u16 {
    match p.get(i..).and_then(|s| s.get(..2)) {
        Some(&[a, b]) => u16::from_be_bytes([a, b]),
        _ => 0,
    }
}

/// Big-endian `u32` at `i`; 0 when the bytes are not there.
pub fn be32(p: &[u8], i: usize) -> u32 {
    match p.get(i..).and_then(|s| s.get(..4)) {
        Some(&[a, b, c, d]) => u32::from_be_bytes([a, b, c, d]),
        _ => 0,
    }
}

/// `glyfIndex`: the raw `loca`/`glyf` tables of a font program `sfnt.Parse` accepted, so a
/// glyph's expanded point count can be bounded before the outline is materialised.
#[derive(Debug, Clone, Copy)]
pub struct GlyfIndex<'a> {
    glyf: &'a [u8],
    loca: &'a [u8],
    /// head `indexToLocFormat` 1: 4-byte loca entries.
    long: bool,
    num_glyphs: usize,
}

/// `parseGlyfIndex`: locates head/maxp/loca/glyf in a raw sfnt program. `None` means no glyph's
/// expansion can be bounded (a CFF flavour, a collection, or tables this parser cannot index), and
/// the caller must treat the program as unusable.
pub fn parse_glyf_index(p: &[u8]) -> Option<GlyfIndex<'_>> {
    if p.len() < 12 {
        return None;
    }
    match be32(p, 0) {
        0x0001_0000 | 0x7472_7565 => {} // the TrueType flavours ("true" is Apple's)
        _ => return None,
    }
    let (mut head, mut maxp, mut loca, mut glyf): (Option<&[u8]>, Option<&[u8]>, _, _) =
        (None, None, None, None);
    let n = usize::from(be16(p, 4));
    for i in 0..n {
        let e = 12 + i * 16;
        if e + 16 > p.len() {
            return None;
        }
        let off = u64::from(be32(p, e + 8));
        let ln = u64::from(be32(p, e + 12));
        if off + ln > p.len() as u64 {
            continue; // sfnt never read this table either, or it failed Parse
        }
        let Some(t) = p.get(off as usize..(off + ln) as usize) else {
            continue;
        };
        match p.get(e..e + 4) {
            Some(b"head") => head = Some(t),
            Some(b"maxp") => maxp = Some(t),
            Some(b"loca") => loca = Some(t),
            Some(b"glyf") => glyf = Some(t),
            _ => {}
        }
    }
    let head = head.filter(|h| h.len() >= 52)?;
    let maxp = maxp.filter(|m| m.len() >= 6)?;
    let (loca, glyf) = (loca?, glyf?);
    let long = be16(head, 50) == 1;
    let num_glyphs = usize::from(be16(maxp, 4));
    let mut need = (num_glyphs + 1) * 2;
    if long {
        need *= 2;
    }
    if loca.len() < need {
        return None;
    }
    Some(GlyfIndex {
        glyf,
        loca,
        long,
        num_glyphs,
    })
}

/// One compound glyph being walked by [`GlyfIndex::bounded_by`] (Go recurses; hostile input can
/// chain 65535 glyphs, so the recursion is an explicit stack here).
struct Frame<'a> {
    gi: usize,
    data: &'a [u8],
    off: usize,
    flags: u16,
    n: i64,
}

enum Enter<'a> {
    Done(i64),
    Frame(Frame<'a>),
}

impl<'a> GlyfIndex<'a> {
    /// `glyphData`: glyph `gi`'s slice of the glyf table, or `None` when the glyph is empty, out
    /// of range, or its loca entries are malformed.
    pub fn glyph_data(&self, gi: usize) -> Option<&'a [u8]> {
        if gi >= self.num_glyphs {
            return None;
        }
        let (o1, o2) = if self.long {
            (
                i64::from(be32(self.loca, gi * 4)),
                i64::from(be32(self.loca, gi * 4 + 4)),
            )
        } else {
            (
                2 * i64::from(be16(self.loca, gi * 2)),
                2 * i64::from(be16(self.loca, gi * 2 + 2)),
            )
        };
        if o2 <= o1 || o2 > self.glyf.len() as i64 {
            return None;
        }
        self.glyf.get(o1 as usize..o2 as usize)
    }

    /// `boundedBy`: whether every glyph's expanded outline stays within `limit` points. The count
    /// charges every on-disk point of every component instance plus one per component record.
    /// Counts are memoised across the component DAG and capped at `limit + 1`, so the check is
    /// O(len(glyf)). A component cycle refuses the font.
    pub fn bounded_by(&self, limit: i64) -> bool {
        let mut memo: Vec<i64> = Vec::new();
        if memo.try_reserve_exact(self.num_glyphs).is_err() {
            return false;
        }
        memo.resize(self.num_glyphs, 0); // 0 unvisited, -1 in progress, else count+1
        let mut stack: Vec<Frame<'a>> = Vec::new();
        for gi in 0..self.num_glyphs {
            if self.count(gi, limit, &mut memo, &mut stack) > limit {
                return false;
            }
        }
        true
    }

    /// Starts glyph `gi`: a finished count, or a compound glyph to walk.
    fn enter(&self, gi: usize, limit: i64, memo: &mut [i64]) -> Enter<'a> {
        let Some(slot) = memo.get_mut(gi) else {
            return Enter::Done(0);
        };
        match *slot {
            -1 => return Enter::Done(limit.wrapping_add(1)), // cycle
            m if m > 0 => return Enter::Done(m - 1),
            _ => {}
        }
        *slot = -1;
        let d = self.glyph_data(gi).unwrap_or(&[]);
        let mut n = 0i64;
        if d.len() >= 10 {
            let nc = be16(d, 0) as i16;
            if nc < 0 {
                return Enter::Frame(Frame {
                    gi,
                    data: d,
                    off: 10,
                    flags: 0,
                    n: 0,
                });
            }
            // Simple glyph: point count = last end-of-contour index + 1.
            let end = 10 + (nc as usize) * 2;
            if nc > 0 && d.len() >= end {
                n = i64::from(be16(d, end - 2)) + 1;
            }
        }
        *slot = n.wrapping_add(1);
        Enter::Done(n)
    }

    fn count(&self, root: usize, limit: i64, memo: &mut [i64], stack: &mut Vec<Frame<'a>>) -> i64 {
        match self.enter(root, limit, memo) {
            Enter::Done(v) => return v,
            Enter::Frame(f) => stack.push(f),
        }
        let mut ret: Option<i64> = None;
        loop {
            let Some(top) = stack.last_mut() else {
                return ret.unwrap_or(0);
            };
            let mut finished = false;
            if let Some(r) = ret.take() {
                // A child's count came back.
                top.n = top.n.wrapping_add(1).wrapping_add(r);
                if top.n > limit {
                    top.n = limit.wrapping_add(1);
                    finished = true;
                } else {
                    top.off += 8;
                    if top.flags & 0x0001 == 0 {
                        top.off -= 2; // args are bytes, not words
                    }
                    if top.flags & 0x0008 != 0 {
                        top.off += 2; // WE_HAVE_A_SCALE
                    } else if top.flags & 0x0040 != 0 {
                        top.off += 4; // WE_HAVE_AN_X_AND_Y_SCALE
                    } else if top.flags & 0x0080 != 0 {
                        top.off += 8; // WE_HAVE_A_TWO_BY_TWO
                    }
                    finished = top.flags & 0x0020 == 0; // no MORE_COMPONENTS
                }
            }
            if !finished {
                if top.off + 4 > top.data.len() {
                    finished = true;
                } else {
                    top.flags = be16(top.data, top.off);
                    let child = usize::from(be16(top.data, top.off + 2));
                    match self.enter(child, limit, memo) {
                        Enter::Done(v) => ret = Some(v),
                        Enter::Frame(f) => stack.push(f),
                    }
                    continue;
                }
            }
            if finished {
                let (gi, n) = (top.gi, top.n);
                if let Some(slot) = memo.get_mut(gi) {
                    *slot = n.wrapping_add(1);
                }
                stack.pop();
                ret = Some(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;

    fn b16(b: &mut Vec<u8>, v: u16) {
        b.extend_from_slice(&v.to_be_bytes());
    }

    /// A simple glyph of one contour of `n` on-curve points, flags and coordinates left zero
    /// (the index only reads the end-of-contour entry).
    fn simple_glyph(n: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b16(&mut b, 1);
        for _ in 0..4 {
            b16(&mut b, 0);
        }
        b16(&mut b, n - 1);
        b16(&mut b, 0);
        b.resize(b.len().next_multiple_of(2), 0);
        b
    }

    /// `buildCompound`: n copies of glyph `child`, ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES,
    /// MORE_COMPONENTS on all but the last.
    fn compound(child: u16, n: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b16(&mut b, 0xFFFF);
        for _ in 0..4 {
            b16(&mut b, 0);
        }
        for i in 0..n {
            let mut flags = 0x0001 | 0x0002;
            if i + 1 < n {
                flags |= 0x0020;
            }
            b16(&mut b, flags);
            b16(&mut b, child);
            b16(&mut b, 0);
            b16(&mut b, 0);
        }
        b
    }

    /// A font with only the four tables the index reads; glyph 0 is empty, glyph i+1 is
    /// `glyphs[i]`. `long` selects the 4-byte loca format.
    fn ttf_with(glyphs: &[Vec<u8>], long: bool) -> Vec<u8> {
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        let entry = |loca: &mut Vec<u8>, off: usize| {
            if long {
                loca.extend_from_slice(&(off as u32).to_be_bytes());
            } else {
                b16(loca, (off / 2) as u16);
            }
        };
        entry(&mut loca, 0);
        entry(&mut loca, 0);
        for g in glyphs {
            glyf.extend_from_slice(g);
            entry(&mut loca, glyf.len());
        }
        let mut head = vec![0u8; 54];
        head[51] = u8::from(long); // indexToLocFormat at [50..52]
        let mut maxp = Vec::new();
        maxp.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b16(&mut maxp, (glyphs.len() + 1) as u16);
        let tables: [(&[u8; 4], &[u8]); 4] = [
            (b"glyf", &glyf),
            (b"head", &head),
            (b"loca", &loca),
            (b"maxp", &maxp),
        ];
        let mut out = Vec::new();
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        b16(&mut out, tables.len() as u16);
        out.extend_from_slice(&[0; 6]);
        let mut off = 12 + 16 * tables.len();
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(off as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            off += data.len().next_multiple_of(4);
        }
        for (_, data) in &tables {
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(4), 0);
        }
        out
    }

    fn ttf(glyphs: &[Vec<u8>]) -> Vec<u8> {
        ttf_with(glyphs, false)
    }

    #[test]
    fn leaf_font_is_indexed_and_bounded() {
        let p = ttf(&[simple_glyph(4)]);
        let g = parse_glyf_index(&p).expect("indexable");
        assert!(g.bounded_by(Limits::default().path_points()));
        assert!(g.glyph_data(0).is_none());
        assert_eq!(g.glyph_data(1).map(<[u8]>::len), Some(14));
        assert!(!g.bounded_by(3), "4 points exceed a limit of 3");
    }

    #[test]
    fn non_truetype_flavours_are_not_indexable() {
        let mut p = ttf(&[simple_glyph(4)]);
        p[..4].copy_from_slice(b"OTTO");
        assert!(parse_glyf_index(&p).is_none());
        assert!(parse_glyf_index(&p[..8]).is_none());
        assert!(parse_glyf_index(&[]).is_none());
    }

    /// `TestHostileCompoundGlyphRefusedBeforeParse`, index half: 7 levels of 9 components each
    /// are ~9^7 instances from ~1 KB of font.
    #[test]
    fn compound_bomb_is_refused() {
        let mut glyphs = vec![simple_glyph(4)];
        for i in 0..7u16 {
            glyphs.push(compound(i + 1, 9));
        }
        let p = ttf(&glyphs);
        let g = parse_glyf_index(&p).expect("indexable");
        assert!(!g.bounded_by(Limits::default().path_points()));
    }

    #[test]
    fn empty_compound_bomb_is_refused() {
        // An all-empty compound still costs one unit per component record per instance.
        let mut glyphs = vec![Vec::new()];
        for i in 0..7u16 {
            glyphs.push(compound(i + 1, 9));
        }
        let p = ttf(&glyphs);
        let g = parse_glyf_index(&p).expect("indexable");
        assert!(!g.bounded_by(Limits::default().path_points()));
    }

    #[test]
    fn component_cycle_refuses_the_font() {
        let p = ttf(&[compound(1, 1)]); // glyph 1 contains itself
        let g = parse_glyf_index(&p).expect("indexable");
        assert!(!g.bounded_by(1000));
    }

    #[test]
    fn deep_component_chain_does_not_recurse() {
        // 40000 glyphs each containing the next: the Go recursion is an explicit stack here.
        let n = 40_000u16;
        let mut glyphs: Vec<Vec<u8>> = (1..n).map(|i| compound(i + 1, 1)).collect();
        glyphs.push(simple_glyph(1));
        let p = ttf_with(&glyphs, true);
        let g = parse_glyf_index(&p).expect("indexable");
        assert!(!g.bounded_by(1000));
        assert!(g.bounded_by(1 << 30));
    }
}
