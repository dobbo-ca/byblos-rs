//! The content-stream lexer: port of `internal/content/lexer.go`.
//!
//! pdfcpu decodes content streams but does not tokenize them, so byblos has its own
//! operator-level parser. Syntax follows ISO 32000-1:2008 section 7.2 (lexical conventions) and
//! section 8.2 (content streams). `inline_image` consumes a whole `BI ... ID ... EI` sequence as
//! one token and finds the end of the sample data from the dictionary where it can.

use crate::error::{Error, Result};
use std::borrow::Cow;

/// `Kind`: the class of a content-stream token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Number,
    String,
    Name,
    ArrayOpen,
    ArrayClose,
    DictOpen,
    DictClose,
    /// An operator, or `true`/`false`/`null`.
    Keyword,
    /// An entire `BI ... ID ... EI` sequence.
    InlineImage,
}

/// One lexed item. `num` is meaningful for [`Kind::Number`]; `text` holds the name text
/// (without the leading slash), the decoded string bytes, or the keyword text. `text` borrows
/// the source for keywords and for strings without escapes (`alias`), and is freshly allocated
/// for names and other strings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Token<'a> {
    pub kind: Kind,
    pub num: f64,
    pub text: Cow<'a, [u8]>,
    /// A string whose `text` is a slice of the source buffer, not a copy: it holds no bytes of
    /// its own.
    pub alias: bool,
    /// For an `alias` string, the offset of `text` in the source buffer (0 otherwise). It lets
    /// the walker share the stream's buffer instead of copying the string.
    pub at: usize,
}

impl<'a> Token<'a> {
    fn of(kind: Kind) -> Self {
        Token {
            kind,
            ..Token::default()
        }
    }

    fn with_text(kind: Kind, text: Cow<'a, [u8]>) -> Self {
        Token {
            kind,
            text,
            ..Token::default()
        }
    }
}

/// Tokenizes a decoded content stream.
#[derive(Debug, Clone)]
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

fn is_white(c: u8) -> bool {
    matches!(c, 0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_delim(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `strconv.ParseFloat` for the text of a number token. The token holds only `+ - . 0-9`, so
/// Go's syntax (a sign, digits, one optional point, at least one digit) is Rust's. Go reports a
/// range error for a value that overflows; Rust returns infinity, which is mapped to `None`.
fn parse_float(s: &[u8]) -> Option<f64> {
    let v = std::str::from_utf8(s).ok()?.parse::<f64>().ok()?;
    v.is_finite().then_some(v)
}

/// The value of the longest parseable prefix of `s`, or 0. Real content streams carry malformed
/// reals such as `1.-2`; viewers keep rendering, and a page is not worth abandoning over one bad
/// operand. Linear in `s`: the longest prefix in Go's grammar is found in one scan and parsed
/// once; only an out-of-range value (Go retries shorter prefixes) needs more, and magnitude is
/// monotone in the integer-digit prefix, so that is a binary search.
fn parse_prefix_float(s: &[u8]) -> f64 {
    let sign = usize::from(matches!(s.first(), Some(b'+' | b'-')));
    let digits = |from: usize| {
        s.get(from..)
            .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count())
    };
    let int_end = sign + digits(sign);
    let mut end = int_end;
    if s.get(int_end) == Some(&b'.') {
        let frac = digits(int_end + 1);
        if int_end > sign || frac > 0 {
            end = int_end + 1 + frac;
        }
    }
    if int_end == sign && end == int_end {
        return 0.0;
    }
    if let Some(v) = s.get(..end).and_then(parse_float) {
        return v;
    }
    // Out of range: the longest finite prefix lies within the integer digits.
    let (mut lo, mut hi) = (sign, int_end); // prefix of length lo is not known finite; hi is not finite
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if s.get(..mid).and_then(parse_float).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    s.get(..lo).and_then(parse_float).unwrap_or(0.0)
}

impl<'a> Lexer<'a> {
    /// A lexer over `src`, positioned at its start.
    pub fn new(src: &'a [u8]) -> Self {
        Lexer { src, pos: 0 }
    }

    fn byte(&self, i: usize) -> Option<u8> {
        self.src.get(i).copied()
    }

    fn skip_space(&mut self) {
        while let Some(c) = self.byte(self.pos) {
            if is_white(c) {
                self.pos += 1;
                continue;
            }
            if c == b'%' {
                while let Some(c) = self.byte(self.pos) {
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            return;
        }
    }

    /// Returns the next token, or `None` when the stream is exhausted (Go's `io.EOF`). Every
    /// successful call advances the read position, so a caller cannot loop forever.
    ///
    /// # Errors
    /// [`Error::Malformed`] for a stray `>` or `)`, an unterminated string, an inline image
    /// without `ID` or `EI`.
    pub fn next_token(&mut self) -> Result<Option<Token<'a>>> {
        self.skip_space();
        let Some(c) = self.byte(self.pos) else {
            return Ok(None);
        };
        match c {
            b'[' => {
                self.pos += 1;
                Ok(Some(Token::of(Kind::ArrayOpen)))
            }
            b']' => {
                self.pos += 1;
                Ok(Some(Token::of(Kind::ArrayClose)))
            }
            b'<' => {
                if self.byte(self.pos + 1) == Some(b'<') {
                    self.pos += 2;
                    Ok(Some(Token::of(Kind::DictOpen)))
                } else {
                    self.hex_string().map(Some)
                }
            }
            b'>' => {
                if self.byte(self.pos + 1) == Some(b'>') {
                    self.pos += 2;
                    Ok(Some(Token::of(Kind::DictClose)))
                } else {
                    Err(Error::Malformed(format!(
                        "content: stray '>' at offset {}",
                        self.pos
                    )))
                }
            }
            b'(' => self.literal_string().map(Some),
            b')' => Err(Error::Malformed(format!(
                "content: stray ')' at offset {}",
                self.pos
            ))),
            b'/' => Ok(Some(self.name())),
            b'+' | b'-' | b'.' | b'0'..=b'9' => Ok(Some(self.number())),
            b'{' | b'}' => {
                // Type 4 function braces never occur in a page content stream; report them as
                // keywords so the walker can treat the page as unclassifiable.
                let src = self.src;
                self.pos += 1;
                let text = src.get(self.pos - 1..self.pos).unwrap_or_default();
                Ok(Some(Token::with_text(Kind::Keyword, Cow::Borrowed(text))))
            }
            _ => self.keyword().map(Some),
        }
    }

    fn keyword(&mut self) -> Result<Token<'a>> {
        let src = self.src;
        let start = self.pos;
        while let Some(c) = self.byte(self.pos) {
            if is_white(c) || is_delim(c) {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            self.pos += 1; // guarantee progress
            return Err(Error::Malformed(format!(
                "content: unexpected byte {:#04x} at offset {}",
                self.byte(start).unwrap_or(0),
                start
            )));
        }
        let kw = src.get(start..self.pos).unwrap_or_default();
        if kw == b"BI" {
            return self.inline_image(start);
        }
        Ok(Token::with_text(Kind::Keyword, Cow::Borrowed(kw)))
    }

    fn number(&mut self) -> Token<'a> {
        let start = self.pos;
        if matches!(self.byte(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        while let Some(c) = self.byte(self.pos) {
            if c.is_ascii_digit() || c == b'.' || c == b'-' || c == b'+' {
                self.pos += 1;
                continue;
            }
            break;
        }
        let text = self.src.get(start..self.pos).unwrap_or_default();
        let num = parse_float(text).unwrap_or_else(|| parse_prefix_float(text));
        Token {
            kind: Kind::Number,
            num,
            ..Token::default()
        }
    }

    fn name(&mut self) -> Token<'a> {
        self.pos += 1; // consume '/'
        let mut out = Vec::new();
        while let Some(c) = self.byte(self.pos) {
            if is_white(c) || is_delim(c) {
                break;
            }
            if c == b'#'
                && self.pos + 2 < self.src.len()
                && let (Some(hi), Some(lo)) = (
                    self.byte(self.pos + 1).and_then(hex_val),
                    self.byte(self.pos + 2).and_then(hex_val),
                )
            {
                out.push((hi << 4) | lo);
                self.pos += 3;
                continue;
            }
            out.push(c);
            self.pos += 1;
        }
        Token::with_text(Kind::Name, Cow::Owned(out))
    }

    fn literal_string(&mut self) -> Result<Token<'a>> {
        let src = self.src;
        let start = self.pos;
        // No escape and no nesting: the string is the bytes between the parens.
        let rest = src.get(self.pos + 1..).unwrap_or_default();
        if let Some(i) = rest
            .iter()
            .position(|&b| b == b'\\' || b == b'(' || b == b')')
            && rest.get(i) == Some(&b')')
        {
            let (a, b) = (self.pos + 1, self.pos + 1 + i);
            self.pos = b + 1;
            return Ok(Token {
                kind: Kind::String,
                text: Cow::Borrowed(src.get(a..b).unwrap_or_default()),
                alias: true,
                at: a,
                ..Token::default()
            });
        }
        self.pos += 1; // consume '('
        let mut depth = 1;
        let mut out = Vec::new();
        while let Some(c) = self.byte(self.pos) {
            self.pos += 1;
            match c {
                b'\\' => {
                    let Some(e) = self.byte(self.pos) else {
                        return Err(Error::Malformed(format!(
                            "content: string at offset {start} ends in a backslash"
                        )));
                    };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'(' | b')' | b'\\' => out.push(e),
                        b'\r' => {
                            if self.byte(self.pos) == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        // line continuation: contributes nothing
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                let Some(d) = self.byte(self.pos) else { break };
                                if !(b'0'..=b'7').contains(&d) {
                                    break;
                                }
                                v = v * 8 + u32::from(d - b'0');
                                self.pos += 1;
                            }
                            // Go's byte(v): three octal digits can reach 511, which wraps.
                            out.push(v as u8);
                        }
                        _ => out.push(e),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Token::with_text(Kind::String, Cow::Owned(out)));
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        Err(Error::Malformed(format!(
            "content: unterminated literal string at offset {start}"
        )))
    }

    fn hex_string(&mut self) -> Result<Token<'a>> {
        let start = self.pos;
        self.pos += 1; // consume '<'
        let mut out = Vec::new();
        let mut cur = 0u8;
        let mut half = false;
        while let Some(c) = self.byte(self.pos) {
            self.pos += 1;
            if c == b'>' {
                if half {
                    out.push(cur << 4); // an odd digit count pads with zero
                }
                return Ok(Token::with_text(Kind::String, Cow::Owned(out)));
            }
            let Some(v) = hex_val(c) else {
                continue; // whitespace and stray bytes are ignored
            };
            if half {
                out.push((cur << 4) | v);
                half = false;
            } else {
                cur = v;
                half = true;
            }
        }
        Err(Error::Malformed(format!(
            "content: unterminated hex string at offset {start}"
        )))
    }

    /// Consumes `BI ... ID <binary> EI` as a single token. The sample data is not decoded: the
    /// walker only needs to know that an inline image is present. The payload is skipped
    /// verbatim because it may contain any byte sequence, including text that looks like
    /// operators.
    ///
    /// The end of the payload is a computed offset whenever the dictionary determines one, and a
    /// delimiter search only when it does not. ISO 32000-1 8.9.7 puts EI after the sample data
    /// but requires no whitespace in between, so searching for a whitespace-preceded EI both
    /// misses real terminators and stops at false ones inside the samples.
    fn inline_image(&mut self, start: usize) -> Result<Token<'a>> {
        let dict = self.pos;
        if !self.seek_keyword(b"ID") {
            return Err(Error::Malformed(format!(
                "content: inline image at offset {start} has no ID"
            )));
        }
        let id = self.pos - 2;
        if self.byte(self.pos).is_some_and(is_white) {
            self.pos += 1; // exactly one whitespace byte separates ID from the samples
        }
        let hdr = parse_inline_dict(self.src.get(dict..id).unwrap_or_default());
        if let Some(n) = hdr.data_len() {
            // EI must sit exactly at the computed end. Anything else means the dictionary and
            // the payload disagree, and the search is the better guess.
            let end = self.pos as i64 + n;
            if let Ok(e) = usize::try_from(end)
                && self.byte(e) == Some(b'E')
                && self.byte(e + 1) == Some(b'I')
            {
                self.pos = e + 2;
                return Ok(Token::of(Kind::InlineImage));
            }
        }
        if !hdr.eod.is_empty() {
            // Same contract as the computed length: the marker fixes the end, and EI has to be
            // there, or the search takes over.
            let rest = self.src.get(self.pos..).unwrap_or_default();
            if let Some(i) = find(rest, hdr.eod) {
                let mut end = self.pos + i + hdr.eod.len();
                while self.byte(end).is_some_and(is_white) {
                    end += 1;
                }
                if self.byte(end) == Some(b'E') && self.byte(end + 1) == Some(b'I') {
                    self.pos = end + 2;
                    return Ok(Token::of(Kind::InlineImage));
                }
            }
        }
        while self.pos + 1 < self.src.len() {
            if self.byte(self.pos) == Some(b'E')
                && self.byte(self.pos + 1) == Some(b'I')
                && self.pos > 0
                && self.byte(self.pos - 1).is_some_and(is_white)
                && (self.pos + 2 == self.src.len()
                    || self
                        .byte(self.pos + 2)
                        .is_some_and(|c| is_white(c) || is_delim(c)))
            {
                self.pos += 2;
                return Ok(Token::of(Kind::InlineImage));
            }
            self.pos += 1;
        }
        self.pos = self.src.len();
        Err(Error::Malformed(format!(
            "content: inline image at offset {start} has no EI"
        )))
    }

    /// Advances past the next standalone occurrence of `kw`.
    fn seek_keyword(&mut self, kw: &[u8]) -> bool {
        while self.pos + kw.len() <= self.src.len() {
            let after = self.pos + kw.len();
            if self.src.get(self.pos..after) == Some(kw)
                && (self.pos == 0
                    || self
                        .byte(self.pos - 1)
                        .is_some_and(|c| is_white(c) || is_delim(c)))
                && (after == self.src.len()
                    || self.byte(after).is_some_and(|c| is_white(c) || is_delim(c)))
            {
                self.pos = after;
                return true;
            }
            self.pos += 1;
        }
        false
    }
}

/// `bytes.Index`: the offset of the first occurrence of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What a `BI ... ID` dictionary states about the sample data that follows it. Keys use the
/// abbreviations of ISO 32000-1 Table 93 and the full names alike, because both appear in real
/// files.
#[derive(Debug, Default)]
struct InlineHeader {
    w: i64,
    h: i64,
    bpc: i64,
    comps: i64,
    mask: bool,
    filtered: bool,
    /// The end-of-data marker of the outermost filter, empty unless that filter has one.
    eod: &'static [u8],
}

/// Reads the bytes between `BI` and `ID`. It reports what it could read and never fails: a
/// dictionary this lexer cannot finish is simply one that determines less, and every caller has
/// a fallback.
fn parse_inline_dict(dict: &[u8]) -> InlineHeader {
    let mut h = InlineHeader::default();
    let mut lx = Lexer::new(dict);
    while let Ok(Some(tok)) = lx.next_token() {
        if tok.kind != Kind::Name {
            continue;
        }
        let Ok(Some(val)) = lx.next_token() else {
            break;
        };
        match tok.text.as_ref() {
            b"W" | b"Width" => h.w = inline_int(&val).unwrap_or(0),
            b"H" | b"Height" => h.h = inline_int(&val).unwrap_or(0),
            b"BPC" | b"BitsPerComponent" => h.bpc = inline_int(&val).unwrap_or(0),
            b"IM" | b"ImageMask" => {
                h.mask = val.kind == Kind::Keyword && val.text.as_ref() == b"true"
            }
            b"F" | b"Filter" => {
                h.filtered = true;
                // The outermost filter is the one whose bytes are on the wire, and in an array
                // that is the first entry.
                h.eod = inline_filter_eod(&first_of_array_or_self(&mut lx, val));
            }
            b"CS" | b"ColorSpace" => {
                // The family is the first element; the rest of the array holds a base space and
                // a lookup table this does not need.
                h.comps = inline_components(&first_of_array_or_self(&mut lx, val)).unwrap_or(0);
            }
            _ => {}
        }
    }
    h
}

/// Returns `val`, or the first element of the array `val` opens, consuming the rest of that
/// array either way.
fn first_of_array_or_self<'a>(lx: &mut Lexer<'a>, val: Token<'a>) -> Token<'a> {
    if val.kind != Kind::ArrayOpen {
        return val;
    }
    let Ok(Some(first)) = lx.next_token() else {
        return Token::default();
    };
    skip_array(lx);
    first
}

/// The end-of-data marker of a filter that has one. ASCII85 ends at `~>` and ASCIIHex at `>`,
/// and neither marker's bytes belong to its own alphabet, so the first occurrence after ID is
/// the real end. Every other filter ends only where its own decoder says so, which this lexer
/// does not run; those return an empty marker and the caller searches for EI.
fn inline_filter_eod(t: &Token<'_>) -> &'static [u8] {
    if t.kind != Kind::Name {
        return b"";
    }
    match t.text.as_ref() {
        b"A85" | b"ASCII85Decode" => b"~>",
        b"AHx" | b"ASCIIHexDecode" => b">",
        _ => b"",
    }
}

impl InlineHeader {
    /// The number of sample bytes an unfiltered inline image holds. `None` whenever the length
    /// is not determined: a filtered image, a colour space whose component count the
    /// dictionary does not state, or any missing dimension. `None` is not an error.
    fn data_len(&self) -> Option<i64> {
        let (w, mut bpc, mut comps) = (self.w, self.bpc, self.comps);
        if self.filtered || w <= 0 || self.h <= 0 {
            return None;
        }
        if self.mask {
            // An image mask is one bit per sample and declares no colour space.
            comps = 1;
            bpc = 1;
        }
        if comps <= 0 || bpc <= 0 {
            return None;
        }
        // Bound each product before taking it. /W, /H and /BPC come out of the file and nothing
        // obliges them to be sensible, so a per-field limit is not enough: a wrapped length is
        // worse than no length, because it reads as an offset. `MAX_INLINE_SAMPLES` is far
        // above any content stream that fits in memory and far below where i64 stops counting.
        const MAX_INLINE_SAMPLES: i64 = 1 << 40;
        if w > MAX_INLINE_SAMPLES / (bpc * comps) {
            return None;
        }
        let row = (w * bpc * comps + 7) / 8;
        if row > MAX_INLINE_SAMPLES / self.h {
            return None;
        }
        Some(row * self.h)
    }
}

/// The number of colour components a colour space name carries. Only the spaces whose component
/// count is fixed by the name appear here: /ICCBased takes its count from a stream, and a name
/// from the page's resources is not resolvable from the dictionary alone.
fn inline_components(t: &Token<'_>) -> Option<i64> {
    if t.kind != Kind::Name {
        return None;
    }
    match t.text.as_ref() {
        b"G" | b"DeviceGray" | b"CalGray" => Some(1),
        b"RGB" | b"DeviceRGB" | b"CalRGB" | b"Lab" => Some(3),
        b"CMYK" | b"DeviceCMYK" => Some(4),
        // One index per sample, whatever the base space is.
        b"I" | b"Indexed" => Some(1),
        _ => None,
    }
}

/// Reads a dimension. Anything negative or absurd is rejected rather than converted, so that a
/// hostile /W cannot produce a nonsense offset.
fn inline_int(t: &Token<'_>) -> Option<i64> {
    if t.kind != Kind::Number || t.num < 0.0 || t.num > f64::from(1u32 << 31) {
        return None;
    }
    Some(t.num as i64)
}

/// Consumes tokens through the close of an array already opened.
fn skip_array(lx: &mut Lexer<'_>) {
    let mut depth = 1;
    while depth > 0 {
        let Ok(Some(tok)) = lx.next_token() else {
            return;
        };
        match tok.kind {
            Kind::ArrayOpen => depth += 1,
            Kind::ArrayClose => depth -= 1,
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "lexer_tests.rs"]
mod tests;
