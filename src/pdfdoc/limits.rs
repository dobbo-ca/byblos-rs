//! Port of `internal/pdfdoc/limits.go`: the object count a document is held to before pdf-rs
//! parses anything (`trailerSize`, `objectBound`, `objectsIn`), the stream-decode caps
//! (`Doc::decode`), `isLimit`, `imageDecodeBytes`, and `Doc::Err` / `Doc::Limits`.
//!
//! The Go helpers read an `io.ReadSeeker` through fixed chunks; here the input is a byte slice,
//! so each is one search over it. The results are the same as Go's chunked reads (the chunking
//! only bounded Go's memory). `parsedBytes` is [`Limits::parsed_bytes`] in `crate::limits`.

use super::doc::{Doc, seam};
use crate::error::{Error, Result};
use crate::limits::Limits;
use pdf::object::StreamDict;
use std::sync::atomic::AtomicBool;

/// The first occurrence of `pat` in `hay` (`bytes.Index`); `None` for an empty `pat`.
pub(crate) fn find(hay: &[u8], pat: &[u8]) -> Option<usize> {
    let (&first, tail) = pat.split_first()?;
    let mut start = 0;
    while let Some(off) = hay.get(start..)?.iter().position(|&b| b == first) {
        let p = start + off;
        if hay.get(p + 1..)?.starts_with(tail) {
            return Some(p);
        }
        start = p + 1;
    }
    None
}

/// The last occurrence of the non-empty `pat` in `hay` (`bytes.LastIndex`), which is what
/// Go's `lastIndex` chunked search over the whole file finds.
fn rfind(hay: &[u8], pat: &[u8]) -> Option<usize> {
    if pat.is_empty() {
        return None;
    }
    hay.windows(pat.len()).rposition(|w| w == pat)
}

/// `indexFrom`: the offset of the first occurrence of `pat` that starts in `[from, limit)`, or
/// -1.
fn index_from(data: &[u8], pat: &[u8], from: i64, limit: i64) -> i64 {
    if from < 0 || from >= limit || pat.is_empty() {
        return -1;
    }
    let Ok(from_u) = usize::try_from(from) else {
        return -1;
    };
    let end = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_add(pat.len() - 1)
        .min(data.len());
    let Some(window) = data.get(from_u..end) else {
        return -1;
    };
    match find(window, pat) {
        Some(i) => {
            let at = from.saturating_add(i as i64);
            if at < limit { at } else { -1 }
        }
        None => -1,
    }
}

/// `readAt`: up to `n` bytes at `off`, empty when `off` is negative or past the end.
fn read_at(data: &[u8], off: i64, n: usize) -> &[u8] {
    let Ok(o) = usize::try_from(off) else {
        return &[];
    };
    let end = o.saturating_add(n).min(data.len());
    data.get(o..end).unwrap_or(&[])
}

/// `intAfter`: the decimal digits after leading white space. `(0, false)` for none; for a number
/// past `i64`, Go's `strconv.ParseInt` returns the maximum with an error, so `(i64::MAX, false)`.
fn int_after(b: &[u8]) -> (i64, bool) {
    let start = b
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(b.len());
    let mut acc: i64 = 0;
    let mut any = false;
    for &c in b.get(start..).unwrap_or(&[]) {
        if !c.is_ascii_digit() {
            break;
        }
        any = true;
        match acc
            .checked_mul(10)
            .and_then(|a| a.checked_add(i64::from(c - b'0')))
        {
            Some(a) => acc = a,
            None => return (i64::MAX, false),
        }
    }
    (acc, any)
}

/// `sizeIn`: the integer after the first `/Size` in `b`.
fn size_in(b: &[u8]) -> (i64, bool) {
    match find(b, b"/Size") {
        Some(i) => int_after(b.get(i + 5..).unwrap_or(&[])),
        None => (0, false),
    }
}

/// `trailerSize` reads the /Size of the last trailer (or cross-reference stream dictionary)
/// without parsing the file, so the object cap comes before pdf-rs allocates anything. Like a
/// reader, it finds the last `startxref` anywhere in the file (junk after `%%EOF` included) and
/// follows it. The bool is false when there is no `startxref` or no /Size there.
pub fn trailer_size(data: &[u8]) -> (i64, bool) {
    let Some(sx) = rfind(data, b"startxref") else {
        return (0, false);
    };
    let sx = sx as i64;
    let end = data.len() as i64;
    let (mut off, ok) = int_after(read_at(data, sx + b"startxref".len() as i64, 64));
    if !ok {
        return (0, false);
    }
    // Offsets count from the %PDF header, as a reader takes them. Go's `off += i` wraps.
    let i = index_from(data, b"%PDF-", 0, end);
    if i > 0 {
        off = off.wrapping_add(i);
    }
    // Cross-reference stream: /Size is in the dictionary at the startxref offset.
    let h = read_at(data, off, 4096);
    if h.first().is_some_and(|b| b.is_ascii_digit()) {
        return size_in(h);
    }
    // Classic table: the trailer that follows the xref at the startxref offset. In a linearized
    // file that is the first-page trailer, which holds the real total, not the last trailer in
    // the file.
    let t = index_from(data, b"trailer", off, sx);
    if t < 0 {
        return (0, false);
    }
    size_in(read_at(data, t, 4096))
}

/// `objectsIn` is the object count the cap rests on: the larger of the trailer /Size and
/// [`object_bound`]. The trailer is only a claim; pdf-rs rebuilds the table by scanning when it
/// cannot use it (a corrupt xref stream, a startxref that lands on an object), and holds every
/// object it finds.
pub fn objects_in(data: &[u8]) -> i64 {
    let (size, _) = trailer_size(data);
    size.max(object_bound(data))
}

/// `objectBound` is an upper bound on the objects with a header that pdf-rs can create from the
/// file: every `obj` token (not `endobj`), with no regard to the number before it. A token count
/// cannot undercount the table inserts. Objects packed in object streams have no header and are
/// not counted: pdf-rs holds them to `max_object_stream_total_count`.
pub fn object_bound(data: &[u8]) -> i64 {
    let mut n: i64 = 0;
    let mut i = 0;
    while let Some(j) = data.get(i..).and_then(|rest| find(rest, b"obj")) {
        let p = i + j;
        i = p + 3;
        if p < 3 || data.get(p - 3..p) != Some(b"end".as_slice()) {
            n += 1;
        }
    }
    n
}

/// `isLimit` reports whether err is a cap or budget refusal, which no repair or per-element drop
/// may swallow. A refused allocation (Go's `ErrMemoryLimit`), an allocator failure and a set
/// cancel flag count too: none of them is damage in one element.
pub fn is_limit(e: &Error) -> bool {
    e.is_limit()
        || matches!(
            e,
            Error::Alloc { .. } | Error::OutOfMemory | Error::Cancelled
        )
}

/// `imageDecodeBytes` is the decoded size an image dictionary allows: `n` components at `bpc`
/// bits, rounded up per row, plus a predictor tag byte per row. A chain of stages filters also
/// gets twice that and 64 bytes, for the encoded bytes between its stages. The arithmetic wraps
/// as Go's `int64` does; a caller treats a result `<= 0` as an overflow.
pub fn image_decode_bytes(w: i64, h: i64, bpc: i64, n: i64, stages: usize) -> i64 {
    let bpc = if bpc < 1 { 8 } else { bpc };
    let row = (w.wrapping_mul(n).wrapping_mul(bpc).wrapping_add(7)) / 8 + 1;
    if stages > 1 {
        return row.wrapping_mul(h).wrapping_mul(2).wrapping_add(64);
    }
    row.wrapping_mul(h)
}

impl Doc {
    /// `decode` decodes `sd` under the stream caps. `page` counts the bytes toward the current
    /// page's running total.
    ///
    /// # Errors
    /// [`Error::Limit`] `stream-bytes` or `page-stream-bytes` over the cap, the errors of the
    /// stream decode (a pdf-rs error is [`Error::Pdf`]; a refused allocation is
    /// [`Error::Alloc`]), [`Error::Cancelled`].
    pub(crate) fn decode(
        &mut self,
        sd: &mut StreamDict,
        page: bool,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut max = self.lim.stream_bytes();
        let mut name = "stream-bytes";
        let mut cap_max = max;
        if page {
            let rem = self.lim.page_stream_bytes() - self.page_bytes;
            if rem < max {
                max = rem;
                name = "page-stream-bytes";
                cap_max = self.lim.page_stream_bytes();
            }
        }
        if max <= 0 {
            return Err(Error::Limit {
                name,
                got: self.page_bytes.saturating_add(1),
                max: cap_max,
            });
        }
        let over = Error::Limit {
            name,
            got: self.page_bytes.saturating_add(max).saturating_add(1),
            max: cap_max,
        };
        let mem = self.ctx.xref.mem().clone();
        match sd.decode_with_limit(max, &mem, cancel) {
            Err(e) if e.is_decode_limit_exceeded() => Err(over),
            Err(e) => Err(match seam("decode", e) {
                Ok(p) => Error::Pdf(p),
                Err(o) => o,
            }),
            Ok(()) => {
                let len = sd.content.as_ref().map_or(0, |c| c.len()) as i64;
                // An unfiltered stream is returned as is, whatever its size, so the length is
                // checked as well as the filter's own refusal.
                if len > max {
                    return Err(over);
                }
                if page {
                    self.page_bytes += len;
                }
                Ok(())
            }
        }
    }

    /// `Err` returns the first limit breach a lookup that can only answer "no" (a form XObject,
    /// say) had to swallow, or `None`.
    pub fn err(&self) -> Option<&Error> {
        self.limit_err.as_ref()
    }

    /// `Limits` is the caps the document was opened under (after [`super::open_budgeted`]
    /// lowered `max_objects`); `content::walk` reads its image-list caps from here.
    pub fn limits(&self) -> &Limits {
        &self.lim
    }
}
