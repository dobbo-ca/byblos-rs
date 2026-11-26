//! Classic Type 1 glyph outlines (/FontFile programs, PFB-wrapped or raw PFA/binary, with eexec
//! encryption): port of byblos `internal/render/type1.go`, minus `fillT1Glyph`, which belongs to
//! the renderer (it drives [`T1Sink`] through the path machinery of `render.go`).
//!
//! A minimal parser plus a Type 1 charstring interpreter for the outline subset: hsbw, the
//! move/line/curve families, closepath, callsubr/return, endchar, div, and the flex OtherSubrs
//! protocol (whose seven collected points are the two cubics). Hints are skipped. seac and sbw are
//! deferred: such a glyph is skipped.
//!
//! Budgets: decryption and every parse scan are linear in the input and no length field is
//! trusted past the remaining bytes. The interpreter charges one unit per executed charstring
//! byte against [`MAX_CHARSTRING_WORK`], with a depth-10 call stack, so a subr chain bomb
//! abandons the glyph in bounded time. The caller charges the executed total per show.

use super::cff::{MAX_CHARSTRING_WORK, cff_standard_sid, try_vec};
use super::glyf::be32;
use crate::error::Error;
use std::collections::HashMap;

/// `t1Font`: a parsed classic Type 1: decrypted charstrings by glyph name, the decrypted Subrs,
/// the builtin encoding, and the FontMatrix as a upem. A `None` charstring or subr is Go's nil
/// (a binary shorter than `lenIV`), which is not the same as an empty one.
#[derive(Debug, Clone)]
pub struct T1Font {
    pub glyphs: HashMap<Vec<u8>, Option<Vec<u8>>>,
    pub subrs: Vec<Option<Vec<u8>>>,
    /// Code -> glyph name; empty = unencoded.
    pub enc: [Vec<u8>; 256],
    pub upem: f64,
}

/// A code-to-name table with nothing encoded.
pub fn empty_encoding() -> [Vec<u8>; 256] {
    std::array::from_fn(|_| Vec::new())
}

/// `t1Decrypt`: runs the Type 1 decryption (Adobe Type 1 spec sections 7.2-7.3) and discards the
/// first `skip` plaintext bytes. Output is bounded by input. `None` is Go's nil: fewer than `skip`
/// bytes of input (or an allocation refusal).
pub fn t1_decrypt(data: &[u8], r: u16, skip: usize) -> Option<Vec<u8>> {
    if data.len() < skip {
        return None;
    }
    let mut r = r;
    let mut out = try_vec(data.len() - skip)?;
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = u16::from(c)
            .wrapping_add(r)
            .wrapping_mul(52845)
            .wrapping_add(22719);
        if i >= skip {
            out.push(p);
        }
    }
    Some(out)
}

fn t1_is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// `t1HexDecode`: decodes ASCII-hex until the first byte that is neither a hex digit nor
/// whitespace, the PFA convention, where the 512-zero trailer decodes into tail garbage the
/// section parsers never reach.
fn t1_hex_decode(d: &[u8]) -> Option<Vec<u8>> {
    let mut out = try_vec(d.len() / 2)?;
    let mut hi: Option<u8> = None;
    for &b in d {
        let v = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c => continue,
            _ => return Some(out),
        };
        match hi.take() {
            None => hi = Some(v),
            Some(h) => out.push((h << 4) | v),
        }
    }
    Some(out)
}

/// `bytes.Index`: the first occurrence of `needle` in `hay`.
fn index_of(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn t1_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c)
}

/// `t1Sections`: splits a Type 1 program into its cleartext header and its eexec-encrypted
/// section. PFB segments (0x80-framed, little-endian lengths) carry the split explicitly; a raw
/// program is cut at the `eexec` keyword, with the encrypted bytes binary or ASCII-hex. Every
/// length is checked against the remaining input before use. `None` = not a usable program.
pub fn t1_sections(p: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if p.len() >= 2 && p[0] == 0x80 {
        let mut clear: Vec<u8> = Vec::new();
        let mut enc: Vec<u8> = Vec::new();
        let mut pos = 0usize;
        while pos + 2 <= p.len() {
            if p[pos] != 0x80 {
                return None;
            }
            let t = p[pos + 1];
            if t == 3 {
                break;
            }
            if pos + 6 > p.len() {
                return None;
            }
            let n = usize::from(p[pos + 2])
                | (usize::from(p[pos + 3]) << 8)
                | (usize::from(p[pos + 4]) << 16)
                | (usize::from(p[pos + 5]) << 24);
            if n > p.len() - pos - 6 {
                return None;
            }
            let seg = p.get(pos + 6..pos + 6 + n)?;
            match t {
                1 => {
                    // ascii after binary is the trailer: ignored
                    if enc.is_empty() {
                        clear.try_reserve(seg.len()).ok()?;
                        clear.extend_from_slice(seg);
                    }
                }
                2 => {
                    enc.try_reserve(seg.len()).ok()?;
                    enc.extend_from_slice(seg);
                }
                _ => return None,
            }
            pos += 6 + n;
        }
        return (enc.len() >= 4).then_some((clear, enc));
    }
    if !p.starts_with(b"%!") {
        return None;
    }
    let i = index_of(p, b"eexec")?;
    let clear = p.get(..i)?;
    let mut rest = p.get(i + 5..)?;
    while let Some(&b) = rest.first() {
        if !t1_ws(b) {
            break;
        }
        rest = &rest[1..];
    }
    if rest.len() < 4 {
        return None;
    }
    let enc = if rest.iter().take(4).all(|&b| t1_is_hex(b)) {
        t1_hex_decode(rest)?
    } else {
        let mut v = try_vec(rest.len())?;
        v.extend_from_slice(rest);
        v
    };
    if enc.len() < 4 {
        return None;
    }
    let mut c = try_vec(clear.len())?;
    c.extend_from_slice(clear);
    Some((c, enc))
}

/// `t1Scan`: a whitespace-delimited token scanner over the decrypted private section. Every
/// method advances or returns `None`, so loops over it terminate.
struct T1Scan<'a> {
    d: &'a [u8],
    pos: usize,
}

fn t1_space(b: u8) -> bool {
    t1_ws(b) || b == 0
}

impl<'a> T1Scan<'a> {
    fn token(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.d.len() && t1_space(self.d[self.pos]) {
            self.pos += 1;
        }
        let start = self.pos;
        while self.pos < self.d.len() && !t1_space(self.d[self.pos]) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        self.d.get(start..self.pos)
    }

    /// `strconv.Atoi` of the next token.
    fn int(&mut self) -> Option<i64> {
        std::str::from_utf8(self.token()?).ok()?.parse().ok()
    }

    /// The `n` RD-delimited bytes that follow the current token: one separator byte, then
    /// exactly `n` bytes.
    fn binary(&mut self, n: i64) -> Option<&'a [u8]> {
        let rem = self.d.len() as i64 - self.pos as i64 - 1;
        if n < 0 || n > rem {
            return None;
        }
        let n = n as usize;
        let b = self.d.get(self.pos + 1..self.pos + 1 + n)?;
        self.pos += 1 + n;
        Some(b)
    }
}

/// `t1LenIV`: reads /lenIV from the decrypted private section; 4 when absent (the spec default),
/// -1 when present but unusable.
fn t1_len_iv(d: &[u8]) -> i64 {
    let Some(i) = index_of(d, b"/lenIV") else {
        return 4;
    };
    let mut s = T1Scan { d, pos: i + 6 };
    match s.int() {
        Some(v) if (0..=16).contains(&v) => v,
        _ => -1,
    }
}

/// `t1ParseSubrs`: reads the /Subrs array: `dup <index> <len> RD <bytes> NP` entries, each
/// charstring-decrypted. A malformed entry stops the scan; what parsed so far is kept, and a
/// missing subr abandons only the glyphs that call it. An empty result is Go's nil.
pub fn t1_parse_subrs(d: &[u8], len_iv: usize) -> Vec<Option<Vec<u8>>> {
    let Some(i) = index_of(d, b"/Subrs") else {
        return Vec::new();
    };
    let mut s = T1Scan { d, pos: i + 6 };
    // The count preallocates a slice header per declared subr, so it must be backed by input:
    // every entry costs at least 8 bytes ("dup 0 0 RD "), and a count the remaining bytes cannot
    // satisfy is hostile and refused before allocating.
    let count = match s.int() {
        Some(c) if c > 0 && c <= ((d.len() - s.pos) / 8) as i64 => c as usize,
        _ => return Vec::new(),
    };
    let mut subrs: Vec<Option<Vec<u8>>> = Vec::new();
    if subrs.try_reserve_exact(count).is_err() {
        return Vec::new();
    }
    subrs.resize(count, None);
    let mut seen = 0usize;
    while seen < count {
        let Some(tok) = s.token() else {
            break;
        };
        if tok == b"ND" || tok == b"|-" || tok == b"end" {
            break;
        }
        if tok != b"dup" {
            continue;
        }
        let idx = s.int();
        let n = s.int();
        let rd = s.token();
        let (Some(idx), Some(n), Some(_)) = (idx, n, rd) else {
            break;
        };
        let Some(bin) = s.binary(n) else {
            break;
        };
        if idx >= 0 && (idx as usize) < count {
            subrs[idx as usize] = t1_decrypt(bin, 4330, len_iv);
        }
        seen += 1;
    }
    subrs
}

/// `t1ParseCharstrings`: reads the /CharStrings dict: `/<name> <len> RD <bytes> ND` entries until
/// `end`, each charstring-decrypted.
pub fn t1_parse_charstrings(d: &[u8], len_iv: usize) -> HashMap<Vec<u8>, Option<Vec<u8>>> {
    let mut glyphs = HashMap::new();
    let Some(i) = index_of(d, b"/CharStrings") else {
        return glyphs;
    };
    let mut s = T1Scan { d, pos: i + 12 };
    while let Some(tok) = s.token() {
        if tok == b"end" {
            break;
        }
        if tok[0] != b'/' || tok.len() < 2 {
            continue;
        }
        let name = &tok[1..];
        let n = s.int();
        let rd = s.token();
        let (Some(n), Some(_)) = (n, rd) else {
            break;
        };
        let Some(bin) = s.binary(n) else {
            break;
        };
        glyphs.insert(name.to_vec(), t1_decrypt(bin, 4330, len_iv));
    }
    glyphs
}

/// `t1StdNames`: the Standard Encoding glyph name per SID, the same SID space `cff_standard_sid`
/// maps codes into.
const T1_STD_NAMES: &str = ".notdef
	space exclam quotedbl numbersign dollar percent ampersand quoteright
	parenleft parenright asterisk plus comma hyphen period slash
	zero one two three four five six seven eight nine
	colon semicolon less equal greater question at
	A B C D E F G H I J K L M N O P Q R S T U V W X Y Z
	bracketleft backslash bracketright asciicircum underscore quoteleft
	a b c d e f g h i j k l m n o p q r s t u v w x y z
	braceleft bar braceright asciitilde
	exclamdown cent sterling fraction yen florin section currency quotesingle
	quotedblleft guillemotleft guilsinglleft guilsinglright fi fl
	endash dagger daggerdbl periodcentered paragraph bullet
	quotesinglbase quotedblbase quotedblright guillemotright ellipsis
	perthousand questiondown grave acute circumflex tilde macron breve
	dotaccent dieresis ring cedilla hungarumlaut ogonek caron emdash
	AE ordfeminine Lslash Oslash OE ordmasculine ae dotlessi lslash oslash
	oe germandbls";

/// `t1StdEncoding`: the code -> glyph-name table of the Standard Encoding.
pub fn t1_std_encoding() -> [Vec<u8>; 256] {
    let names: Vec<&str> = T1_STD_NAMES.split_whitespace().collect();
    let mut enc = empty_encoding();
    for (c, slot) in enc.iter_mut().enumerate() {
        let sid = cff_standard_sid(c as u8);
        if sid != 0
            && let Some(n) = names.get(usize::from(sid))
        {
            *slot = n.as_bytes().to_vec();
        }
    }
    enc
}

/// `t1Encoding`: reads the builtin encoding from the cleartext header: the predefined
/// StandardEncoding, or `dup <code> /<name> put` entries.
pub fn t1_encoding(clear: &[u8]) -> [Vec<u8>; 256] {
    let Some(i) = index_of(clear, b"/Encoding") else {
        return t1_std_encoding();
    };
    let seg = clear.get(i + 9..).unwrap_or(&[]);
    if index_of(seg, b"StandardEncoding").is_some_and(|j| j < 8) {
        return t1_std_encoding();
    }
    let mut enc = empty_encoding();
    let mut s = T1Scan { d: seg, pos: 0 };
    while let Some(tok) = s.token() {
        if tok == b"readonly" || tok == b"def" {
            break;
        }
        if tok != b"dup" {
            continue;
        }
        let code = s.int();
        let name = s.token();
        let (Some(code), Some(name)) = (code, name) else {
            break;
        };
        if name.len() < 2 || name[0] != b'/' {
            break;
        }
        if s.token() != Some(&b"put"[..]) {
            break;
        }
        if (0..256).contains(&code) {
            enc[code as usize] = name[1..].to_vec();
        }
    }
    enc
}

/// `t1UPEM`: reads /FontMatrix from the cleartext. Like `parse_cff`, only the uniform
/// axis-aligned `[s 0 0 s 0 0]` form maps onto a single upem; anything else returns 0 and the font
/// degrades to widths-only. Absent, the Type 1 default 0.001 gives upem 1000.
pub fn t1_upem(clear: &[u8]) -> f64 {
    let Some(i) = index_of(clear, b"/FontMatrix") else {
        return 1000.0;
    };
    let seg = clear.get(i + 11..).unwrap_or(&[]);
    let j = seg.iter().position(|&b| b == b'[');
    let k = seg.iter().position(|&b| b == b']');
    let (Some(j), Some(k)) = (j, k) else {
        return 0.0;
    };
    if k <= j || k > j + 128 {
        return 0.0;
    }
    let text = String::from_utf8_lossy(seg.get(j + 1..k).unwrap_or(&[]));
    let fields: Vec<&str> = text.split_whitespace().collect();
    let mut m = [0f64; 6];
    if fields.len() != 6 {
        return 0.0;
    }
    for (slot, f) in m.iter_mut().zip(&fields) {
        match parse_go_float(f) {
            Some(v) => *slot = v,
            None => return 0.0,
        }
    }
    if m[0].is_nan()
        || m[0] <= 0.0
        || m[1] != 0.0
        || m[2] != 0.0
        || m[3] != m[0]
        || m[4] != 0.0
        || m[5] != 0.0
    {
        return 0.0;
    }
    let u = (1.0 / m[0]).round();
    if !(1.0..=65535.0).contains(&u) {
        return 0.0;
    }
    u
}

/// `strconv.ParseFloat` for the spellings Rust's `parse` lacks: hex floats (`0x1p-10`, with `_`).
fn parse_go_float(f: &str) -> Option<f64> {
    if let Ok(v) = f.parse::<f64>() {
        return Some(v);
    }
    let (neg, rest) = match f.as_bytes().first()? {
        b'-' => (true, f.get(1..)?),
        b'+' => (false, f.get(1..)?),
        _ => (false, f),
    };
    let rest = rest
        .strip_prefix("0x")
        .or_else(|| rest.strip_prefix("0X"))?;
    let (mant, exp) = rest.split_once(['p', 'P'])?;
    let exp: i32 = exp.replace('_', "").parse().ok()?;
    let mut m: u64 = 0;
    let mut e = i64::from(exp.clamp(-100_000, 100_000));
    let mut seen = false;
    let mut frac = false;
    for c in mant.chars() {
        if c == '_' {
            continue;
        }
        if c == '.' {
            if frac {
                return None;
            }
            frac = true;
            continue;
        }
        let d = u64::from(c.to_digit(16)?);
        seen = true;
        if m >> 56 == 0 {
            m = (m << 4) | d;
            if frac {
                e -= 4;
            }
        } else if !frac {
            e += 4;
        }
    }
    if !seen {
        return None;
    }
    let e = e.clamp(-2200, 2200) as i32;
    // Scale in two steps so a large mantissa shift cannot overflow the intermediate power.
    let v = m as f64 * 2f64.powi(e / 2) * 2f64.powi(e - e / 2);
    Some(if neg { -v } else { v })
}

/// `parseType1`: parses a classic Type 1 program. `None` means the program is not a Type 1 this
/// stage can safely interpret and the caller degrades to widths-only.
pub fn parse_type1(p: &[u8]) -> Option<T1Font> {
    let (clear, enc) = t1_sections(p)?;
    if !clear.starts_with(b"%!") {
        return None;
    }
    let upem = t1_upem(&clear);
    if upem == 0.0 {
        return None;
    }
    let private = t1_decrypt(&enc, 55665, 4)?;
    let len_iv = usize::try_from(t1_len_iv(&private)).ok()?;
    let glyphs = t1_parse_charstrings(&private, len_iv);
    if glyphs.is_empty() {
        return None;
    }
    Some(T1Font {
        glyphs,
        subrs: t1_parse_subrs(&private, len_iv),
        enc: t1_encoding(&clear),
        upem,
    })
}

/// `errT1Skip` and the sink's own failures. `Skip` abandons ONE glyph (a malformed charstring,
/// deferred seac, a tripped per-glyph budget, or a non-finite device coordinate) without failing
/// the page; `Sink` carries an error from the sink (a path or fill budget) that does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum T1Error {
    Skip,
    Sink(Error),
}

/// `t1Sink`: receives outline commands in font units, y UP (Type 1 charstring space).
pub trait T1Sink {
    fn move_to(&mut self, x: f64, y: f64) -> Result<(), T1Error>;
    fn line_to(&mut self, x: f64, y: f64) -> Result<(), T1Error>;
    fn curve_to(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
    ) -> Result<(), T1Error>;
}

/// Go's `int(f)` on amd64: an out-of-range or NaN value converts to the minimum integer.
fn go_int(v: f64) -> i64 {
    let lim = -(i64::MIN as f64);
    if !(-lim..lim).contains(&v) {
        i64::MIN
    } else {
        v as i64
    }
}

/// The current point and the flex state of one charstring run.
struct Pen {
    x: f64,
    y: f64,
    flexing: bool,
    flex_pts: Vec<f64>,
}

impl Pen {
    fn move_to<S: T1Sink>(&mut self, sink: &mut S, dx: f64, dy: f64) -> Result<(), T1Error> {
        self.x += dx;
        self.y += dy;
        if self.flexing {
            // Flex points are COLLECTED, not moved to: the seven rmoveto'd points become the two
            // cubics at othersubr 0.
            if self.flex_pts.len() >= 14 {
                return Err(T1Error::Skip);
            }
            self.flex_pts.push(self.x);
            self.flex_pts.push(self.y);
            return Ok(());
        }
        sink.move_to(self.x, self.y)
    }

    fn curve_to<S: T1Sink>(&mut self, sink: &mut S, c: [f64; 6]) -> Result<(), T1Error> {
        self.x = c[4];
        self.y = c[5];
        sink.curve_to(c[0], c[1], c[2], c[3], c[4], c[5])
    }
}

/// Decodes the operand starting at `ins[0]` (a byte >= 32) of `n` bytes, as Type 1 spec 6.2 (opcode
/// 255 is a full 32-bit integer, unlike Type 2's 16.16).
fn operand(ins: &[u8], b: u8) -> f64 {
    let b1 = f64::from(ins.get(1).copied().unwrap_or(0));
    match b {
        0..=246 => f64::from(i32::from(b) - 139),
        247..=250 => f64::from(b - 247) * 256.0 + b1 + 108.0,
        251..=254 => -f64::from(b - 251) * 256.0 - b1 - 108.0,
        255 => f64::from(be32(ins, 1) as i32),
    }
}

fn operand_len(b: u8) -> usize {
    match b {
        247..=254 => 2,
        255 => 5,
        _ => 1,
    }
}

impl T1Font {
    /// The charstring of the glyph `code` is encoded to, `None` for no glyph (Go's nil).
    pub fn glyph(&self, code: u8) -> Option<&[u8]> {
        self.glyphs
            .get(self.enc[usize::from(code)].as_slice())?
            .as_deref()
    }

    /// `interpret`: executes one decrypted charstring, charging one unit per executed byte against
    /// `work` (the caller charges the total against its fill budget per show).
    /// [`T1Error::Skip`] means the glyph is abandoned; [`T1Error::Sink`] comes from the sink and
    /// fails the page.
    ///
    /// # Errors
    /// [`T1Error::Skip`] when the glyph is abandoned, [`T1Error::Sink`] with the sink's error.
    pub fn interpret<S: T1Sink>(
        &self,
        cs: &[u8],
        sink: &mut S,
        work: &mut i64,
    ) -> Result<(), T1Error> {
        let mut st: Vec<f64> = Vec::new();
        let mut call: Vec<&[u8]> = Vec::new();
        let mut psq: Vec<f64> = Vec::new(); // PostScript-stack stand-in feeding `pop`
        let mut pen = Pen {
            x: 0.0,
            y: 0.0,
            flexing: false,
            flex_pts: Vec::new(),
        };
        let mut ins = cs;
        let mut charge = |n: usize| -> bool {
            *work += n as i64;
            *work <= MAX_CHARSTRING_WORK
        };
        while let Some(&b) = ins.first() {
            if b >= 32 || b == 28 {
                // 28 is not a Type 1 encoding; treating it as malformed keeps the byte from
                // aliasing an operator.
                if b == 28 {
                    return Err(T1Error::Skip);
                }
                let n = operand_len(b);
                if ins.len() < n || st.len() >= 48 {
                    return Err(T1Error::Skip);
                }
                st.push(operand(ins, b));
                ins = &ins[n..];
                if !charge(n) {
                    return Err(T1Error::Skip);
                }
                continue;
            }
            ins = &ins[1..];
            if !charge(1) {
                return Err(T1Error::Skip);
            }
            match b {
                1 | 3 => st.clear(), // hstem vstem: hints shape grid-fitting, never outlines
                4 | 21 | 22 => {
                    // vmoveto rmoveto hmoveto
                    let (dx, dy) = match (b, st.as_slice()) {
                        (21, &[a, c, ..]) => (a, c),
                        (22, &[a, ..]) => (a, 0.0),
                        (4, &[a, ..]) => (0.0, a),
                        _ => return Err(T1Error::Skip),
                    };
                    pen.move_to(sink, dx, dy)?;
                    st.clear();
                }
                5..=7 => {
                    // rlineto hlineto vlineto
                    match (b, st.as_slice()) {
                        (5, &[a, c, ..]) => {
                            pen.x += a;
                            pen.y += c;
                        }
                        (6, &[a, ..]) => pen.x += a,
                        (7, &[a, ..]) => pen.y += a,
                        _ => return Err(T1Error::Skip),
                    }
                    sink.line_to(pen.x, pen.y)?;
                    st.clear();
                }
                8 => {
                    // rrcurveto
                    let &[a, b2, c, d, e, f, ..] = st.as_slice() else {
                        return Err(T1Error::Skip);
                    };
                    let (c1x, c1y) = (pen.x + a, pen.y + b2);
                    let (c2x, c2y) = (c1x + c, c1y + d);
                    pen.curve_to(sink, [c1x, c1y, c2x, c2y, c2x + e, c2y + f])?;
                    st.clear();
                }
                30 | 31 => {
                    // vhcurveto hvcurveto
                    let &[a, b2, c, d, ..] = st.as_slice() else {
                        return Err(T1Error::Skip);
                    };
                    let curve = if b == 30 {
                        // dy1 dx2 dy2 dx3
                        let (c1x, c1y) = (pen.x, pen.y + a);
                        let (c2x, c2y) = (c1x + b2, c1y + c);
                        [c1x, c1y, c2x, c2y, c2x + d, c2y]
                    } else {
                        // dx1 dx2 dy2 dy3
                        let (c1x, c1y) = (pen.x + a, pen.y);
                        let (c2x, c2y) = (c1x + b2, c1y + c);
                        [c1x, c1y, c2x, c2y, c2x, c2y + d]
                    };
                    pen.curve_to(sink, curve)?;
                    st.clear();
                }
                // closepath: the filler closes subpaths implicitly, and the current point is
                // unchanged by spec.
                9 => st.clear(),
                10 => {
                    // callsubr (no bias in Type 1)
                    let Some(top) = st.pop() else {
                        return Err(T1Error::Skip);
                    };
                    if call.len() >= 10 {
                        return Err(T1Error::Skip);
                    }
                    let idx = go_int(top);
                    let sub = usize::try_from(idx)
                        .ok()
                        .and_then(|i| self.subrs.get(i))
                        .and_then(|s| s.as_deref());
                    let Some(sub) = sub else {
                        return Err(T1Error::Skip);
                    };
                    call.push(ins);
                    ins = sub;
                }
                11 => {
                    // return
                    let Some(back) = call.pop() else {
                        return Err(T1Error::Skip);
                    };
                    ins = back;
                }
                13 => {
                    // hsbw: the sidebearing IS the initial current point; the width is ignored
                    // here (PDF /Widths drive the advance; see `advance` for no /Widths).
                    let &[sbx, _, ..] = st.as_slice() else {
                        return Err(T1Error::Skip);
                    };
                    pen.x = sbx;
                    pen.y = 0.0;
                    st.clear();
                }
                14 => return Ok(()), // endchar
                12 => {
                    let Some(&e) = ins.first() else {
                        return Err(T1Error::Skip);
                    };
                    ins = &ins[1..];
                    if !charge(1) {
                        return Err(T1Error::Skip);
                    }
                    match e {
                        0..=2 => st.clear(), // dotsection vstem3 hstem3: hints, skipped
                        // 6 (seac) is deferred; 7 (sbw) is deliberately absent: horizontal Latin
                        // fonts use hsbw, and a glyph using sbw skips like seac.
                        12 => {
                            // div
                            let &[.., num, den] = st.as_slice() else {
                                return Err(T1Error::Skip);
                            };
                            if den == 0.0 {
                                return Err(T1Error::Skip);
                            }
                            st.truncate(st.len() - 2);
                            st.push(num / den);
                        }
                        16 => {
                            // callothersubr: flex and hint replacement
                            let &[.., n_raw, oth_raw] = st.as_slice() else {
                                return Err(T1Error::Skip);
                            };
                            let oth = go_int(oth_raw);
                            let n = go_int(n_raw);
                            st.truncate(st.len() - 2);
                            if n < 0 || n > st.len() as i64 {
                                return Err(T1Error::Skip);
                            }
                            let n = n as usize;
                            let args_at = st.len() - n;
                            match oth {
                                0 => {
                                    // flex end: the collected points are the two cubics
                                    if !pen.flexing || pen.flex_pts.len() != 14 {
                                        return Err(T1Error::Skip);
                                    }
                                    let p = std::mem::take(&mut pen.flex_pts);
                                    pen.curve_to(sink, [p[2], p[3], p[4], p[5], p[6], p[7]])?;
                                    pen.curve_to(sink, [p[8], p[9], p[10], p[11], p[12], p[13]])?;
                                    pen.flexing = false;
                                    // The following pop/pop/setcurrentpoint re-states the end
                                    // point the curves already reached.
                                    psq.clear();
                                    psq.push(pen.x);
                                    psq.push(pen.y);
                                }
                                1 => {
                                    // flex start
                                    pen.flexing = true;
                                    pen.flex_pts.clear();
                                }
                                2 => {} // one flex point, already collected by its rmoveto
                                _ => {
                                    // hint replacement (3) and unknowns: args feed pop
                                    psq.clear();
                                    psq.extend_from_slice(&st[args_at..]);
                                }
                            }
                            st.truncate(args_at);
                        }
                        17 => {
                            // pop
                            if st.len() >= 48 {
                                return Err(T1Error::Skip);
                            }
                            st.push(psq.pop().unwrap_or(0.0));
                        }
                        // setcurrentpoint: only ever follows an othersubr that already left the
                        // current point correct; clearing is enough.
                        33 => st.clear(),
                        _ => return Err(T1Error::Skip),
                    }
                }
                _ => return Err(T1Error::Skip), // reserved
            }
        }
        // Stream ran out mid-glyph (no endchar): keep what was outlined, exactly as sfnt's run
        // loop ends a Type 2 stream.
        Ok(())
    }

    /// `advance`: `code`'s hsbw width in font units, for the no-/Widths dict shape. It decodes only
    /// the leading operands of the charstring (hsbw must be the first operator), so it is bounded
    /// by the 48-entry stack.
    pub fn advance(&self, code: u8) -> f64 {
        let mut cs = self.glyph(code).unwrap_or(&[]);
        let mut st: Vec<f64> = Vec::new();
        while let Some(&b) = cs.first() {
            if st.len() >= 48 {
                break;
            }
            if b < 32 {
                if let (13, &[_, wx, ..]) = (b, st.as_slice()) {
                    return wx;
                }
                return 0.0;
            }
            let n = operand_len(b);
            if cs.len() < n {
                return 0.0;
            }
            st.push(operand(cs, b));
            cs = &cs[n..];
        }
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::super::cff::cff_to_sfnt;
    use super::super::glyf::parse_glyf_index;
    use super::*;

    // ---- minimal Type 1 builder ----
    //
    // Hand-assembled like the Go fixtures: a cleartext PostScript header, an eexec-encrypted
    // private section carrying charstring-encrypted Subrs and CharStrings, and a 512-zero
    // trailer. It can also emit the hostile variants.

    /// Appends a Type 1 charstring integer operand (opcode 255 is a full 32-bit integer).
    fn t1i(b: &mut Vec<u8>, v: i32) {
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
            _ => {
                b.push(255);
                b.extend_from_slice(&v.to_be_bytes());
            }
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
    impl<'a> From<&'a Vec<u8>> for It<'a> {
        fn from(v: &'a Vec<u8>) -> Self {
            It::B(v)
        }
    }

    fn t1_assemble(items: Vec<It>) -> Vec<u8> {
        let mut b = Vec::new();
        for it in items {
            match it {
                It::I(v) => t1i(&mut b, v),
                It::Op(name) => {
                    let op: &[u8] = match name {
                        "hstem" => &[1],
                        "vstem" => &[3],
                        "vmoveto" => &[4],
                        "rlineto" => &[5],
                        "hlineto" => &[6],
                        "vlineto" => &[7],
                        "rrcurveto" => &[8],
                        "closepath" => &[9],
                        "callsubr" => &[10],
                        "return" => &[11],
                        "hsbw" => &[13],
                        "endchar" => &[14],
                        "rmoveto" => &[21],
                        "hmoveto" => &[22],
                        "vhcurveto" => &[30],
                        "hvcurveto" => &[31],
                        "dotsection" => &[12, 0],
                        "seac" => &[12, 6],
                        "div" => &[12, 12],
                        "callothersubr" => &[12, 16],
                        "pop" => &[12, 17],
                        "setcurrentpoint" => &[12, 33],
                        other => panic!("t1cs: unknown op {other}"),
                    };
                    b.extend_from_slice(op);
                }
                It::B(v) => b.extend_from_slice(v),
            }
        }
        b
    }

    macro_rules! t1cs {
        ($($x:expr),* $(,)?) => { t1_assemble(vec![$(It::from($x)),*]) };
    }

    /// Runs the Type 1 encryption (the inverse of [`t1_decrypt`]) over `prefix` then `plain`.
    fn t1encrypt(key: u16, prefix: &[u8], plain: &[u8]) -> Vec<u8> {
        let mut r = key;
        prefix
            .iter()
            .chain(plain)
            .map(|&p| {
                let c = p ^ (r >> 8) as u8;
                r = u16::from(c)
                    .wrapping_add(r)
                    .wrapping_mul(52845)
                    .wrapping_add(22719);
                c
            })
            .collect()
    }

    /// Assembles a raw (PFA-structure, binary eexec section) Type 1 font: the three returned
    /// sections are `/Length1/2/3`. `encoding` is the whole /Encoding clause.
    fn build_type1(
        glyphs: &[(&str, Vec<u8>)],
        subrs: &[Vec<u8>],
        encoding: &str,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let clear = format!(
            "%!PS-AdobeFont-1.0: BbT1 001.001\n/FontName /BbT1 def\n/FontType 1 def\n\
             /FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/PaintType 0 def\n\
             /FontBBox {{0 -200 1000 800}} readonly def\n{encoding}currentdict end\ncurrentfile eexec\n"
        )
        .into_bytes();

        let mut pr: Vec<u8> = Vec::new();
        pr.extend_from_slice(b"dup /Private 15 dict dup begin\n");
        pr.extend_from_slice(b"/RD {string currentfile exch readstring pop} executeonly def\n");
        pr.extend_from_slice(b"/ND {noaccess def} executeonly def\n");
        pr.extend_from_slice(b"/NP {noaccess put} executeonly def\n");
        pr.extend_from_slice(b"/BlueValues [] noaccess def\n");
        pr.extend_from_slice(b"/MinFeature {16 16} noaccess def\n");
        pr.extend_from_slice(b"/password 5839 def\n");
        pr.extend_from_slice(b"/lenIV 4 def\n");
        if !subrs.is_empty() {
            pr.extend_from_slice(format!("/Subrs {} array\n", subrs.len()).as_bytes());
            for (i, s) in subrs.iter().enumerate() {
                let e = t1encrypt(4330, b"~~~~", s);
                pr.extend_from_slice(format!("dup {i} {} RD ", e.len()).as_bytes());
                pr.extend_from_slice(&e);
                pr.extend_from_slice(b" NP\n");
            }
            pr.extend_from_slice(b"ND\n");
        }
        pr.extend_from_slice(
            format!("2 index /CharStrings {} dict dup begin\n", glyphs.len()).as_bytes(),
        );
        for (name, cs) in glyphs {
            let e = t1encrypt(4330, b"~~~~", cs);
            pr.extend_from_slice(format!("/{name} {} RD ", e.len()).as_bytes());
            pr.extend_from_slice(&e);
            pr.extend_from_slice(b" ND\n");
        }
        pr.extend_from_slice(
            b"end\nend\nreadonly put\nnoaccess put\ndup /FontName get exch definefont pop\nmark currentfile closefile\n",
        );

        // The first four CIPHERTEXT bytes must not all be hex digits, or every consumer reads the
        // section as ASCII-hex. Vary the discarded prefix until they are not.
        let mut enc = Vec::new();
        for p in 0..=255u8 {
            enc = t1encrypt(55665, &[b'B', b'Y', b'B', p], &pr);
            if !enc[..4].iter().all(|&c| c.is_ascii_hexdigit()) {
                break;
            }
        }

        let mut trailer = Vec::new();
        for _ in 0..8 {
            trailer.extend_from_slice(&[b'0'; 64]);
            trailer.push(b'\n');
        }
        trailer.extend_from_slice(b"cleartomark\n");
        (clear, enc, trailer)
    }

    /// Wraps the three sections as PFB segments (0x80, type, LE length).
    fn pfb_wrap(clear: &[u8], enc: &[u8], trailer: &[u8]) -> Vec<u8> {
        let seg = |t: u8, d: &[u8]| {
            let n = d.len() as u32;
            let mut h = vec![0x80, t];
            h.extend_from_slice(&n.to_le_bytes());
            h.extend_from_slice(d);
            h
        };
        let mut out = seg(1, clear);
        out.extend(seg(2, enc));
        out.extend(seg(1, trailer));
        out.extend_from_slice(&[0x80, 3]);
        out
    }

    const T1_STD_ENC: &str = "/Encoding StandardEncoding def\n";

    fn raw_font(glyphs: &[(&str, Vec<u8>)], subrs: &[Vec<u8>], encoding: &str) -> Vec<u8> {
        let (clear, enc, trailer) = build_type1(glyphs, subrs, encoding);
        [clear, enc, trailer].concat()
    }

    /// 'A' (standard encoding) is the 500x500 square at the origin, as a raw Type 1.
    fn square() -> Vec<u8> {
        t1cs!(
            0,
            600,
            "hsbw",
            0,
            0,
            "rmoveto",
            500,
            0,
            "rlineto",
            0,
            500,
            "rlineto",
            -500,
            0,
            "rlineto",
            "closepath",
            "endchar"
        )
    }

    fn square_font() -> Vec<u8> {
        raw_font(
            &[(".notdef", t1cs!(0, 0, "hsbw", "endchar")), ("A", square())],
            &[],
            T1_STD_ENC,
        )
    }

    fn bare(subrs: Vec<Vec<u8>>) -> T1Font {
        T1Font {
            glyphs: HashMap::new(),
            subrs: subrs.into_iter().map(Some).collect(),
            enc: empty_encoding(),
            upem: 1000.0,
        }
    }

    /// Records every sink call for coordinate-convention tests.
    #[derive(Default)]
    struct Capture {
        calls: Vec<String>,
    }

    impl T1Sink for Capture {
        fn move_to(&mut self, x: f64, y: f64) -> Result<(), T1Error> {
            self.calls.push(format!("move {x} {y}"));
            Ok(())
        }
        fn line_to(&mut self, x: f64, y: f64) -> Result<(), T1Error> {
            self.calls.push(format!("line {x} {y}"));
            Ok(())
        }
        fn curve_to(
            &mut self,
            x1: f64,
            y1: f64,
            x2: f64,
            y2: f64,
            x3: f64,
            y3: f64,
        ) -> Result<(), T1Error> {
            self.calls
                .push(format!("curve {x1} {y1} {x2} {y2} {x3} {y3}"));
            Ok(())
        }
    }

    fn run(cs: &[u8]) -> (Result<(), T1Error>, Vec<String>, i64) {
        let f = bare(Vec::new());
        let mut c = Capture::default();
        let mut work = 0;
        let r = f.interpret(cs, &mut c, &mut work);
        (r, c.calls, work)
    }

    #[test]
    fn program_takes_the_type1_path() {
        let raw = square_font();
        assert!(
            parse_glyf_index(&raw).is_none(),
            "Type 1 parsed as TrueType"
        );
        assert!(cff_to_sfnt(&raw).is_none(), "Type 1 parsed as a bare CFF");
        let f = parse_type1(&raw).expect("refused the minimal well-formed raw Type 1");
        assert!(matches!(f.glyphs.get(b"A".as_slice()), Some(Some(_))));
        assert_eq!(f.upem, 1000.0);
        assert_eq!(f.glyph(b'A'), Some(square().as_slice()));
        assert!(f.glyph(b'B').is_none());
        let (clear, enc, trailer) = build_type1(
            &[
                (".notdef", t1cs!(0, 0, "hsbw", "endchar")),
                ("A", t1cs!(0, 600, "hsbw", "endchar")),
            ],
            &[],
            T1_STD_ENC,
        );
        let pf =
            parse_type1(&pfb_wrap(&clear, &enc, &trailer)).expect("refused the PFB-wrapped form");
        assert!(matches!(pf.glyphs.get(b"A".as_slice()), Some(Some(_))));
    }

    /// Every edge a degenerate rrcurveto, the right edge through a Subr, the top edge's dx
    /// computed with div: pins curves, callsubr/return and div together.
    #[test]
    fn curve_subr_and_div() {
        let edge =
            |dx: i32, dy: i32| t1cs!(dx / 2, dy / 2, dx / 4, dy / 4, dx / 4, dy / 4, "rrcurveto");
        let mut sub = edge(0, 500);
        sub.extend(t1cs!("return"));
        let mut cs = t1cs!(0, 600, "hsbw", 0, 0, "rmoveto");
        cs.extend(edge(500, 0));
        cs.extend(t1cs!(
            0,
            "callsubr",
            -1000,
            2,
            "div",
            0,
            "rlineto",
            "closepath",
            "endchar"
        ));
        let f = bare(vec![sub]);
        let mut c = Capture::default();
        let mut work = 0;
        assert_eq!(f.interpret(&cs, &mut c, &mut work), Ok(()));
        assert_eq!(
            c.calls,
            [
                "move 0 0",
                "curve 250 0 375 0 500 0",
                "curve 500 250 500 375 500 500",
                "line 0 500",
            ]
        );
    }

    /// The flex othersubr protocol (1, 2 x7, 0): the seven collected points are the two cubics,
    /// and the trailing pop/pop/setcurrentpoint leaves the current point alone.
    #[test]
    fn flex_collapses_to_curves() {
        let cs = t1cs!(
            0,
            600,
            "hsbw",
            0,
            0,
            "rmoveto",
            500,
            0,
            "rlineto",
            0,
            500,
            "rlineto",
            0,
            1,
            "callothersubr",
            -50,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -50,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -100,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -50,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -50,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -100,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            -100,
            0,
            "rmoveto",
            0,
            2,
            "callothersubr",
            50,
            0,
            500,
            3,
            0,
            "callothersubr",
            "pop",
            "pop",
            "setcurrentpoint",
            "closepath",
            "endchar"
        );
        let (r, calls, _) = run(&cs);
        assert_eq!(r, Ok(()));
        assert_eq!(
            calls,
            [
                "move 0 0",
                "line 500 0",
                "line 500 500",
                "curve 400 500 300 500 250 500",
                "curve 200 500 100 500 0 500",
            ]
        );
    }

    #[test]
    fn custom_encoding_maps_code_to_named_glyph() {
        let encoding = "/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\n\
                        dup 65 /box put\nreadonly def\n";
        let raw = raw_font(
            &[
                (".notdef", t1cs!(0, 0, "hsbw", "endchar")),
                ("box", square()),
            ],
            &[],
            encoding,
        );
        let f = parse_type1(&raw).expect("parses");
        assert_eq!(f.glyph(b'A'), Some(square().as_slice()));
        assert_eq!(f.enc[65], b"box");
        assert!(f.enc[66].is_empty());
    }

    /// A 3-level chain of subrs each calling the next 200 times executes ~200^3 charstring bytes
    /// from a sub-KB font: the per-glyph work budget must abandon the glyph.
    #[test]
    fn hostile_subr_bomb_budgeted() {
        const CHAIN: i32 = 3;
        let subrs: Vec<Vec<u8>> = (0..CHAIN)
            .map(|i| {
                if i == CHAIN - 1 {
                    return t1cs!("return");
                }
                let mut s = Vec::new();
                for _ in 0..200 {
                    s = t1cs!(&s, i + 1, "callsubr");
                }
                t1cs!(&s, "return")
            })
            .collect();
        let f = bare(subrs);
        let mut work = 0i64;
        let r = f.interpret(
            &t1cs!(0, 600, "hsbw", 0, "callsubr", "endchar"),
            &mut Capture::default(),
            &mut work,
        );
        assert_eq!(
            r,
            Err(T1Error::Skip),
            "the per-glyph budget is not enforced"
        );
        assert!(
            work <= MAX_CHARSTRING_WORK + 8,
            "interpreter charged {work} work; the budget must stop it at ~{MAX_CHARSTRING_WORK}"
        );
    }

    /// A subr that calls itself forever hits the depth wall almost immediately.
    #[test]
    fn hostile_self_recursion_bounded() {
        let f = bare(vec![t1cs!(0, "callsubr", "return")]);
        let mut work = 0i64;
        let r = f.interpret(
            &t1cs!(0, 600, "hsbw", 0, "callsubr", "endchar"),
            &mut Capture::default(),
            &mut work,
        );
        assert_eq!(r, Err(T1Error::Skip));
        assert!(
            work <= 100,
            "self-recursion charged {work} work before the depth wall"
        );
    }

    /// The executed total of a zero-segment hint sled is what the caller charges per show.
    #[test]
    fn work_is_measured_per_run() {
        let mut sled = t1cs!(0, 600, "hsbw");
        sled.extend(std::iter::repeat_n(1u8, 60_000)); // hstem sled
        sled.extend(t1cs!("endchar"));
        let (r, calls, work) = run(&sled);
        assert_eq!(r, Ok(()));
        assert!(calls.is_empty());
        assert!(
            (60_000..=MAX_CHARSTRING_WORK).contains(&work),
            "work {work}"
        );
    }

    /// Truncations and mutations of a valid font must degrade to None or skipped glyphs.
    #[test]
    fn malformed_no_panic() {
        let good = square_font();
        let from: &[u8] = b"/FontMatrix [0.001 0 0 0.001 0 0]";
        let at = index_of(&good, from).expect("present");
        let skewed = [
            &good[..at],
            b"/FontMatrix [0.001 0.5 0 0.001 0 0]",
            &good[at + from.len()..],
        ]
        .concat();
        let (c0, e0, t0) = build_type1(&[], &[], T1_STD_ENC);
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"%!".to_vec(),
            b"%!PS-AdobeFont eexec".to_vec(),
            good[..40].to_vec(),
            skewed,
            pfb_wrap(&[], &[], &[]),
            pfb_wrap(&c0, &e0, &t0),
            vec![0x80, 1, 255, 255, 255, 255],
        ];
        for p in &cases {
            assert!(parse_type1(p).is_none());
        }
        assert!(parse_type1(&good).is_some());
        for i in 0..=good.len() {
            let _ = parse_type1(&good[..i]);
        }
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0xff;
            let _ = parse_type1(&bad);
        }
    }

    /// seac is deferred: the glyph skips, abandoning even the outline drawn before it.
    #[test]
    fn seac_deferred() {
        let cs = t1cs!(
            0,
            600,
            "hsbw",
            0,
            0,
            "rmoveto",
            500,
            0,
            "rlineto",
            0,
            500,
            "rlineto",
            -500,
            0,
            "rlineto",
            "closepath",
            0,
            65,
            65,
            193,
            "seac"
        );
        let (r, calls, _) = run(&cs);
        assert_eq!(r, Err(T1Error::Skip));
        assert_eq!(
            calls.len(),
            4,
            "outline before the seac is emitted, then the glyph skips"
        );
    }

    /// vhcurveto (dy1 dx2 dy2 dx3: leaves vertically, arrives horizontally) and hvcurveto (dx1
    /// dx2 dy2 dy3: the mirror).
    #[test]
    fn curve_operator_conventions() {
        let cs = t1cs!(
            0,
            600,
            "hsbw",
            100,
            100,
            "rmoveto",
            10,
            20,
            30,
            40,
            "vhcurveto",
            1,
            2,
            3,
            4,
            "hvcurveto",
            "endchar"
        );
        let (r, calls, _) = run(&cs);
        assert_eq!(r, Ok(()));
        assert_eq!(
            calls,
            [
                "move 100 100",
                "curve 100 110 120 140 160 140", // vh: up then across
                "curve 161 140 163 143 163 147", // hv: across then up
            ]
        );
    }

    /// Hint replacement (othersubr 3): the argument must feed the PostScript stack so the next
    /// pop retrieves it.
    #[test]
    fn hint_replacement_feeds_pop() {
        let cs = t1cs!(
            0,
            600,
            "hsbw",
            0,
            0,
            "rmoveto",
            42,
            1,
            3,
            "callothersubr",
            "pop",
            0,
            "rlineto",
            "endchar"
        );
        let (r, calls, _) = run(&cs);
        assert_eq!(r, Ok(()));
        assert_eq!(calls, ["move 0 0", "line 42 0"]);
    }

    /// The ASCII-hex eexec form (the PFA convention) parses to the same glyphs as the binary form.
    #[test]
    fn hex_eexec() {
        let (clear, enc, trailer) = build_type1(
            &[(".notdef", t1cs!(0, 0, "hsbw", "endchar")), ("A", square())],
            &[],
            T1_STD_ENC,
        );
        let mut hexed = Vec::new();
        for (i, b) in enc.iter().enumerate() {
            hexed.extend_from_slice(format!("{b:02x}").as_bytes());
            if i % 32 == 31 {
                hexed.push(b'\n');
            }
        }
        let raw = [clear, hexed, trailer].concat();
        let f = parse_type1(&raw).expect("refused the hex-eexec form");
        assert_eq!(f.glyph(b'A'), Some(square().as_slice()));
    }

    /// A /Subrs count no remaining input could satisfy is refused before its preallocation.
    #[test]
    fn hostile_subr_count_bounded() {
        let mut hostile = b"/Subrs 500 ".to_vec();
        hostile.extend(std::iter::repeat_n(b' ', 3000));
        assert!(
            t1_parse_subrs(&hostile, 4).is_empty(),
            "unbacked /Subrs 500"
        );
        let s = t1_parse_subrs(b"/Subrs 1 dup 0 5 RD aaaaa NP end more padding", 4);
        assert_eq!(s.len(), 1);
        assert!(
            s[0].is_some(),
            "the well-formed 1-entry /Subrs failed to parse"
        );
    }

    /// A Type 1 dict with no /Widths advances by each charstring's own hsbw width.
    #[test]
    fn no_widths_uses_hsbw_advance() {
        let f = parse_type1(&square_font()).expect("parses");
        assert_eq!(f.advance(b'A'), 600.0);
        assert_eq!(f.advance(b'B'), 0.0);
    }

    #[test]
    fn decrypt_round_trips_and_skips() {
        let plain = b"hello charstring";
        let e = t1encrypt(4330, b"abcd", plain);
        assert_eq!(t1_decrypt(&e, 4330, 4).as_deref(), Some(&plain[..]));
        assert_eq!(t1_decrypt(&e[..3], 4330, 4), None);
        assert_eq!(t1_decrypt(&e[..4], 4330, 4).as_deref(), Some(&[][..]));
    }

    #[test]
    fn std_encoding_names() {
        let e = t1_std_encoding();
        assert_eq!(e[usize::from(b'A')], b"A");
        assert_eq!(e[usize::from(b' ')], b"space");
        assert_eq!(e[251], b"germandbls");
        assert!(e[0].is_empty());
        assert!(e[176].is_empty());
    }

    #[test]
    fn font_matrix_forms() {
        assert_eq!(t1_upem(b"/FontMatrix [0.001 0 0 0.001 0 0] def"), 1000.0);
        assert_eq!(t1_upem(b"/FontMatrix [0.0005 0 0 0.0005 0 0]"), 2000.0);
        assert_eq!(t1_upem(b"no matrix here"), 1000.0);
        assert_eq!(t1_upem(b"/FontMatrix [0.001 0 0 0.002 0 0]"), 0.0);
        assert_eq!(t1_upem(b"/FontMatrix [0.001 0 0 0.001 0]"), 0.0);
        assert_eq!(t1_upem(b"/FontMatrix 0.001 0 0 0.001 0 0"), 0.0);
        assert_eq!(t1_upem(b"/FontMatrix [-0.001 0 0 -0.001 0 0]"), 0.0);
    }
}
