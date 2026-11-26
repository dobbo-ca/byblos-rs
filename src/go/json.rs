//! `encoding/json` as byblos uses it for `Provenance` (`provenance.go:370,551`,
//! `editpages.go:187`), plus the `time.Time` JSON form (`time/format_rfc3339.go`, the RFC 3339
//! `Parse` of `time/format.go`). Go 1.27 runs `encoding/json/v2` with `DefaultOptionsV1`
//! (`v2_encode.go`, `v2_decode.go`), not the v1 `encode.go`/`decode.go`. Ported behaviour:
//! `internal/jsonwire` (`AppendQuote`, `AppendFloat`, `AppendUnquote`), `jsontext/state.go`
//! (the 10 000 nesting cap), `v2/fold.go` with `MatchCaseSensitiveDelimiter` (field name
//! matching), `v2/arshal_time.go` (`time.Time`) and `v2/arshal_default.go` (arrays, slices).
//!
//! There is no reflection, so the module is a small JSON value tree and writer helpers that the
//! provenance code drives by hand:
//!
//! * [`Writer`] produces exactly `json.Marshal`'s bytes for strings (HTML escaping, U+2028 and
//!   U+2029, control characters), floats (`'f'` for `1e-6 <= abs < 1e21`, else `'e'` with the
//!   exponent trimmed), integers and times. The caller writes fields in struct order and skips
//!   `omitempty` ones.
//! * [`parse`] is `json.Unmarshal`'s syntax check and decode into a [`Value`], with Go's string
//!   unquoting (invalid UTF-8 and lone surrogates become U+FFFD, byte by byte). The typed
//!   accessors on [`Value`] (`as_f64`, `to_vec`, ...) have Go's rules for `null` (no effect),
//!   type mismatches (an error) and fixed-size arrays (short input zero-fills, long input is
//!   dropped). [`field_values`] gives the values of one struct field in document order, matched
//!   case-insensitively as Go does.
//!
//! The parser is iterative and [`Value`]'s `Drop` is too, so the nesting cap of 10 000 is the
//! only depth limit and no input overflows the stack. Memory is checked against
//! [`Limits::alloc_limit`] before the tree is built (a node costs at most one input byte).
//!
//! Known differences from `json.Unmarshal`: when a struct field appears twice, Go merges the
//! second struct or slice value into the first; callers here see both values through
//! [`field_values`] and decide (last wins is exact for scalars; for arrays it differs only when a later duplicate has `null` elements, which Go merges over the earlier value). All errors are
//! `Error`; their text is not Go's, only whether one occurs.

use super::xml::decode_rune;
use crate::limits::Limits;
use std::collections::TryReserveError;
use std::fmt;

/// `maxNestingDepth` of `scanner.go`.
pub const MAX_NESTING_DEPTH: usize = 10_000;

/// Everything this module reports. [`Error::Alloc`] and [`Error::OutOfMemory`] are the
/// resource failures; the caller maps them to the crate's `Error::Alloc` and
/// `Error::OutOfMemory`, and the rest to its own errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `*SyntaxError`: the input is not valid JSON (or nests deeper than 10 000).
    Syntax { msg: String, offset: usize },
    /// `*UnmarshalTypeError` and the other decode mismatches.
    Type(String),
    /// `*UnsupportedValueError`: NaN or an infinity.
    Unsupported(String),
    /// A `time.Time` that cannot be encoded or decoded as RFC 3339.
    Time(String),
    /// The byte budget (`max_alloc`) refused `got` bytes with `max` allowed.
    Alloc { got: u64, max: u64 },
    /// The allocator refused a size that is within the limits.
    OutOfMemory,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax { msg, offset } => write!(f, "{msg} at offset {offset}"),
            Error::Type(s) => write!(f, "json: cannot unmarshal {s}"),
            Error::Unsupported(s) => write!(f, "json: unsupported value: {s}"),
            Error::Time(s) => f.write_str(s),
            Error::Alloc { got, max } => {
                write!(f, "json: memory budget: {got} bytes needed, {max} allowed")
            }
            Error::OutOfMemory => f.write_str("json: out of memory"),
        }
    }
}

impl std::error::Error for Error {}

impl From<TryReserveError> for Error {
    fn from(_: TryReserveError) -> Self {
        Error::OutOfMemory
    }
}

// ---------------------------------------------------------------------------------------------
// time.Time

/// A `time.Time` as the JSON form sees it: the instant, its nanosecond part and the UTC offset
/// it is shown in. Go's zero `time.Time` is [`Time::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub unix: i64,
    /// 0 to 999 999 999.
    pub nanos: u32,
    /// The offset east of UTC in seconds.
    pub offset_secs: i32,
}

impl Time {
    /// `time.Time{}`: 0001-01-01T00:00:00Z.
    pub const ZERO: Time = Time {
        unix: -62_135_596_800,
        nanos: 0,
        offset_secs: 0,
    };

    /// The time of a pdf-rs `GoTime` (whole seconds, offset in minutes).
    pub fn from_go_time(t: pdf::GoTime) -> Time {
        Time {
            unix: t.unix,
            nanos: 0,
            offset_secs: t.utc_offset_min.saturating_mul(60),
        }
    }

    /// `Time.MarshalJSON` without the quotes: RFC 3339 with the sub-second part trimmed of
    /// trailing zeros, `Z` for a zero offset.
    ///
    /// # Errors
    ///
    /// [`Error::Time`] when the year is outside 0 to 9999 or the offset hour is 24 or more, as
    /// Go reports.
    pub fn format_rfc3339_nano(&self) -> Result<String, Error> {
        let local = self.unix.saturating_add(i64::from(self.offset_secs));
        let days = local.div_euclid(86_400);
        let sod = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        if !(0..=9999).contains(&year) {
            return Err(Error::Time(
                "Time.MarshalJSON: year outside of range [0,9999]".to_string(),
            ));
        }
        let mut s = format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            sod / 3600,
            sod % 3600 / 60,
            sod % 60
        );
        if self.nanos != 0 {
            let frac = format!("{:09}", self.nanos);
            s.push('.');
            s.push_str(frac.trim_end_matches('0'));
        }
        if self.offset_secs == 0 {
            s.push('Z');
            return Ok(s);
        }
        let zone = i64::from(self.offset_secs) / 60;
        if zone.abs() / 60 >= 24 {
            return Err(Error::Time(
                "Time.MarshalJSON: timezone hour outside of range [0,23]".to_string(),
            ));
        }
        let sign = if zone < 0 { '-' } else { '+' };
        let zone = zone.abs();
        s.push_str(&format!("{sign}{:02}:{:02}", zone / 60, zone % 60));
        Ok(s)
    }

    /// `Time.UnmarshalJSON` of the text inside the quotes: `time.Parse(time.RFC3339, s)` as Go
    /// runs it (the fast path and the lenient `Parse` fallback are the same function here: it
    /// accepts a one-digit hour, `,` as the sub-second separator, any number of sub-second
    /// digits (the first nine count), and a zone offset up to `+24:60`).
    ///
    /// # Errors
    ///
    /// [`Error::Time`] for anything `Parse` rejects.
    pub fn parse_rfc3339(s: &[u8]) -> Result<Time, Error> {
        fn digit(s: &[u8], i: usize) -> bool {
            s.get(i).is_some_and(u8::is_ascii_digit)
        }
        // getnum: one or two digits; `fixed` demands two.
        fn getnum(s: &[u8], fixed: bool) -> Option<(i64, usize)> {
            let a = i64::from(s.first()?.checked_sub(b'0').filter(|d| *d < 10)?);
            match s.get(1).filter(|c| c.is_ascii_digit()) {
                Some(c) => Some((a * 10 + i64::from(c - b'0'), 2)),
                None if fixed => None,
                None => Some((a, 1)),
            }
        }
        fn skip(s: &mut &[u8], c: u8) -> Option<()> {
            if s.first() == Some(&c) {
                *s = s.get(1..)?;
                Some(())
            } else {
                None
            }
        }
        let bad = || Error::Time("parsing time as RFC 3339 failed".to_string());
        let parse = |s: &[u8]| -> Option<Time> {
            let mut s = s;
            if s.len() < 4 || !s.get(..4)?.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let year = s
                .get(..4)?
                .iter()
                .fold(0i64, |a, d| a * 10 + i64::from(d - b'0'));
            s = s.get(4..)?;
            skip(&mut s, b'-')?;
            let (month, n) = getnum(s, true)?;
            s = s.get(n..)?;
            skip(&mut s, b'-')?;
            let (day, n) = getnum(s, true)?;
            s = s.get(n..)?;
            skip(&mut s, b'T')?;
            let (hour, n) = getnum(s, false)?;
            s = s.get(n..)?;
            skip(&mut s, b':')?;
            let (min, n) = getnum(s, true)?;
            s = s.get(n..)?;
            skip(&mut s, b':')?;
            let (sec, n) = getnum(s, true)?;
            s = s.get(n..)?;
            if !(1..=12).contains(&month) || hour >= 24 || min >= 60 || sec >= 60 {
                return None;
            }
            // A fractional second: the separator and at least one digit.
            let mut nanos = 0i64;
            if s.len() >= 2 && matches!(s.first(), Some(b'.' | b',')) && digit(s, 1) {
                let mut n = 2;
                while digit(s, n) {
                    n += 1;
                }
                // parseNanoseconds keeps the first nine digits.
                let nb = n.min(10);
                let ns = s
                    .get(1..nb)?
                    .iter()
                    .fold(0i64, |a, d| a * 10 + i64::from(d - b'0'));
                nanos = ns * 10i64.pow(u32::try_from(10 - nb).ok()?);
                s = s.get(n..)?;
            }
            let offset = if s.first() == Some(&b'Z') {
                s = s.get(1..)?;
                0
            } else {
                if s.len() < 6 || s.get(3) != Some(&b':') {
                    return None;
                }
                let (hr, _) = getnum(s.get(1..3)?, true)?;
                let (mm, _) = getnum(s.get(4..6)?, true)?;
                if hr > 24 || mm > 60 {
                    return None;
                }
                let off = (hr * 60 + mm) * 60;
                let off = match s.first()? {
                    b'+' => off,
                    b'-' => -off,
                    _ => return None,
                };
                s = s.get(6..)?;
                off
            };
            if !s.is_empty() || day < 1 || day > days_in_month(month, year) {
                return None;
            }
            let unix =
                days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec - offset;
            Some(Time {
                unix,
                nanos: u32::try_from(nanos).ok()?,
                offset_secs: i32::try_from(offset).ok()?,
            })
        };
        parse(s).ok_or_else(bad)
    }
}

fn days_in_month(month: i64, year: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of a day count since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era.saturating_mul(400) + i64::from(month <= 2);
    (year, month, day)
}

// ---------------------------------------------------------------------------------------------
// Encoder

/// A `json.Marshal` output buffer. The caller writes `{`, the fields in struct order (leaving
/// out `omitempty` ones that are empty) and `}` with the methods below; separators are placed
/// by [`Writer::key`] and [`Writer::item`]. Every write is checked against the byte budget
/// before it allocates.
#[derive(Debug)]
pub struct Writer {
    buf: Vec<u8>,
    max: u64,
}

impl Writer {
    /// An empty writer that holds at most `limits.alloc_limit()` bytes.
    pub fn new(limits: &Limits) -> Self {
        Writer {
            buf: Vec::new(),
            max: limits.alloc_limit(),
        }
    }

    /// The bytes written so far.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    fn put(&mut self, b: &[u8]) -> Result<(), Error> {
        let total = (self.buf.len() as u64).saturating_add(b.len() as u64);
        if total > self.max {
            return Err(Error::Alloc {
                got: total,
                max: self.max,
            });
        }
        self.buf.try_reserve(b.len())?;
        self.buf.extend_from_slice(b);
        Ok(())
    }

    /// Appends `s` as it is: for `{`, `}`, `[`, `]`, `null` and other fixed tokens.
    ///
    /// # Errors
    /// [`Error::Alloc`] past the byte budget, [`Error::OutOfMemory`] if the allocator refuses.
    pub fn raw(&mut self, s: &str) -> Result<(), Error> {
        self.put(s.as_bytes())
    }

    /// `null`: a nil slice or pointer without `omitempty`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn null(&mut self) -> Result<(), Error> {
        self.raw("null")
    }

    /// `{`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn begin_object(&mut self) -> Result<(), Error> {
        self.raw("{")
    }

    /// `}`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn end_object(&mut self) -> Result<(), Error> {
        self.raw("}")
    }

    /// `[`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn begin_array(&mut self) -> Result<(), Error> {
        self.raw("[")
    }

    /// `]`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn end_array(&mut self) -> Result<(), Error> {
        self.raw("]")
    }

    /// The field name and colon, after a comma unless it is the first field. Write the value
    /// next.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn key(&mut self, name: &str) -> Result<(), Error> {
        if self.buf.last() != Some(&b'{') {
            self.raw(",")?;
        }
        self.string(name)?;
        self.raw(":")
    }

    /// The comma before an array element, unless it is the first one. Write the element next.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn item(&mut self) -> Result<(), Error> {
        if self.buf.last() != Some(&b'[') {
            self.raw(",")?;
        }
        Ok(())
    }

    /// A string with `json.Marshal`'s escaping: `"` and `\`, `\b \f \n \r \t`, other control
    /// characters as `\u00XX`, `<`, `>` and `&` as `\u003c`, `\u003e` and `\u0026`, and U+2028
    /// and U+2029 as `\u2028` and `\u2029`.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn string(&mut self, s: &str) -> Result<(), Error> {
        let mut out = String::new();
        out.try_reserve(s.len().saturating_add(2))?;
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '<' | '>' | '&' | '\u{0}'..='\u{1f}' | '\u{2028}' | '\u{2029}' => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
        self.put(out.as_bytes())
    }

    /// An integer.
    ///
    /// # Errors
    /// As [`Writer::raw`].
    pub fn i64(&mut self, v: i64) -> Result<(), Error> {
        self.raw(&v.to_string())
    }

    /// A float64 as `floatEncoder` writes it: the shortest digits that round-trip, in plain
    /// decimal for `1e-6 <= abs < 1e21` (and zero), else in exponent form with a signed,
    /// unpadded exponent (`1e-7`, `1.5e+21`).
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for NaN and the infinities.
    pub fn f64(&mut self, f: f64) -> Result<(), Error> {
        if !f.is_finite() {
            let what = if f.is_nan() {
                "NaN"
            } else if f > 0.0 {
                "+Inf"
            } else {
                "-Inf"
            };
            return Err(Error::Unsupported(what.to_string()));
        }
        let abs = f.abs();
        if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
            let s = format!("{f:e}");
            let Some((mant, exp)) = s.split_once('e') else {
                return self.raw(&s);
            };
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            self.raw(&format!("{mant}e{sign}{digits}"))
        } else {
            self.raw(&f.to_string())
        }
    }

    /// A `time.Time`: a quoted RFC 3339 string (`Time.MarshalJSON`).
    ///
    /// # Errors
    ///
    /// [`Error::Time`] when the time has no RFC 3339 form.
    pub fn time(&mut self, t: &Time) -> Result<(), Error> {
        let s = t.format_rfc3339_nano()?;
        self.raw("\"")?;
        self.raw(&s)?;
        self.raw("\"")
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder

/// A parsed JSON value. Numbers keep their literal (`json.Number`). The derived `Debug` and
/// `PartialEq` recurse, so they can overflow the stack on a value nested near
/// [`MAX_NESTING_DEPTH`]; do not print or compare untrusted trees.
#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The number literal as written.
    Number(String),
    /// The unquoted text. `escaped` says the source had a backslash escape.
    String {
        text: String,
        escaped: bool,
    },
    Array(Vec<Value>),
    /// Members in document order, duplicates kept.
    Object(Vec<(String, Value)>),
}

impl Drop for Value {
    /// Frees nested values iteratively: a 10 000 deep array must not recurse on drop.
    fn drop(&mut self) {
        let mut work: Vec<Value> = Vec::new();
        take_children(self, &mut work);
        while let Some(mut v) = work.pop() {
            take_children(&mut v, &mut work);
        }
    }
}

fn take_children(v: &mut Value, work: &mut Vec<Value>) {
    match v {
        Value::Array(items) => work.extend(std::mem::take(items)),
        Value::Object(items) => work.extend(std::mem::take(items).into_iter().map(|(_, v)| v)),
        _ => {}
    }
}

enum Frame {
    Array(Vec<Value>),
    Object {
        items: Vec<(String, Value)>,
        key: Option<String>,
    },
}

/// `json.Unmarshal`'s syntax check, building the tree. The whole input must be one value with
/// only JSON whitespace around it; nesting deeper than [`MAX_NESTING_DEPTH`] is an error.
///
/// # Errors
///
/// [`Error::Syntax`], or [`Error::Alloc`] when the tree would not fit the allocation limit.
pub fn parse(data: &[u8], limits: &Limits) -> Result<Value, Error> {
    // A node needs at least one input byte, and costs at most this much.
    let node = std::mem::size_of::<(String, Value)>() + std::mem::size_of::<usize>();
    let need = (data.len() as u64).saturating_mul(node as u64);
    if need > limits.alloc_limit() {
        return Err(Error::Alloc {
            got: need,
            max: limits.alloc_limit(),
        });
    }
    Parser { data, pos: 0 }.run()
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

fn push<T>(v: &mut Vec<T>, x: T) -> Result<(), Error> {
    v.try_reserve(1)?;
    v.push(x);
    Ok(())
}

impl Parser<'_> {
    fn err<T>(&self, msg: &str) -> Result<T, Error> {
        Err(Error::Syntax {
            msg: msg.to_string(),
            offset: self.pos,
        })
    }

    fn skip_ws(&mut self) {
        while matches!(self.data.get(self.pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    /// The next byte, consumed; the end of the input is an error.
    fn next(&mut self) -> Result<u8, Error> {
        match self.data.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                Ok(b)
            }
            None => self.err("unexpected end of JSON input"),
        }
    }

    fn run(&mut self) -> Result<Value, Error> {
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            self.skip_ws();
            let c = self.next()?;
            let mut v = match c {
                b'{' => {
                    if stack.len() >= MAX_NESTING_DEPTH {
                        return self.err("exceeded max depth");
                    }
                    self.skip_ws();
                    if self.data.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        Value::Object(Vec::new())
                    } else {
                        let key = self.read_key()?;
                        push(
                            &mut stack,
                            Frame::Object {
                                items: Vec::new(),
                                key: Some(key),
                            },
                        )?;
                        continue;
                    }
                }
                b'[' => {
                    if stack.len() >= MAX_NESTING_DEPTH {
                        return self.err("exceeded max depth");
                    }
                    self.skip_ws();
                    if self.data.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        Value::Array(Vec::new())
                    } else {
                        push(&mut stack, Frame::Array(Vec::new()))?;
                        continue;
                    }
                }
                b'"' => self.string()?,
                b't' => self.literal(b"rue", Value::Bool(true))?,
                b'f' => self.literal(b"alse", Value::Bool(false))?,
                b'n' => self.literal(b"ull", Value::Null)?,
                b'-' | b'0'..=b'9' => self.number(c)?,
                _ => return self.err("invalid character looking for beginning of value"),
            };
            // v is complete: attach it to its parent, closing containers as they end.
            loop {
                let closed = match stack.last_mut() {
                    None => {
                        self.skip_ws();
                        if self.pos < self.data.len() {
                            return self.err("invalid character after top-level value");
                        }
                        return Ok(v);
                    }
                    Some(Frame::Array(items)) => {
                        push(items, v)?;
                        self.skip_ws();
                        match self.next()? {
                            b',' => false,
                            b']' => true,
                            _ => return self.err("invalid character after array element"),
                        }
                    }
                    Some(Frame::Object { items, key }) => {
                        let k = key.take().unwrap_or_default();
                        push(items, (k, v))?;
                        self.skip_ws();
                        match self.next()? {
                            b',' => {
                                self.skip_ws();
                                *key = Some(self.read_key()?);
                                false
                            }
                            b'}' => true,
                            _ => return self.err("invalid character after object key:value pair"),
                        }
                    }
                };
                if !closed {
                    break;
                }
                v = match stack.pop() {
                    Some(Frame::Array(items)) => Value::Array(items),
                    Some(Frame::Object { items, .. }) => Value::Object(items),
                    None => return self.err("internal error: empty stack"),
                };
            }
        }
    }

    /// An object key and its colon.
    fn read_key(&mut self) -> Result<String, Error> {
        self.skip_ws();
        if self.next()? != b'"' {
            return self.err("invalid character looking for beginning of object key string");
        }
        let (text, _) = self.string_parts()?;
        self.skip_ws();
        if self.next()? != b':' {
            return self.err("invalid character after object key");
        }
        Ok(text)
    }

    fn literal(&mut self, rest: &[u8], v: Value) -> Result<Value, Error> {
        let end = self.pos.saturating_add(rest.len());
        if self.data.get(self.pos..end) != Some(rest) {
            return self.err("invalid character in literal");
        }
        self.pos = end;
        Ok(v)
    }

    /// A number whose first byte `first` is consumed. The grammar is JSON's.
    fn number(&mut self, first: u8) -> Result<Value, Error> {
        let start = self.pos - 1;
        let digits = |p: &mut Parser| {
            let from = p.pos;
            while p.data.get(p.pos).is_some_and(u8::is_ascii_digit) {
                p.pos += 1;
            }
            p.pos > from
        };
        let mut lead = first;
        if first == b'-' {
            lead = self.next()?;
        }
        match lead {
            b'0' => {}
            b'1'..=b'9' => {
                digits(self);
            }
            _ => return self.err("invalid character in numeric literal"),
        }
        if self.data.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !digits(self) {
                return self.err("invalid character after decimal point in numeric literal");
            }
        }
        if matches!(self.data.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.data.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return self.err("invalid character in exponent of numeric literal");
            }
        }
        let lit = self.data.get(start..self.pos).unwrap_or_default();
        let mut s = String::new();
        s.try_reserve_exact(lit.len())?;
        s.extend(lit.iter().map(|&b| char::from(b)));
        Ok(Value::Number(s))
    }

    /// A string value whose opening quote is consumed.
    fn string(&mut self) -> Result<Value, Error> {
        let (text, escaped) = self.string_parts()?;
        Ok(Value::String { text, escaped })
    }

    /// The text of a string whose opening quote is consumed, and whether it had escapes.
    fn string_parts(&mut self) -> Result<(String, bool), Error> {
        let start = self.pos;
        let mut escaped = false;
        loop {
            match self.next()? {
                b'"' => break,
                b'\\' => {
                    escaped = true;
                    match self.next()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            let hex = self.data.get(self.pos..self.pos + 4);
                            if hex.is_none_or(|h| !h.iter().all(u8::is_ascii_hexdigit)) {
                                return self.err("invalid character in \\u hexadecimal escape");
                            }
                            self.pos += 4;
                        }
                        _ => return self.err("invalid character in string escape code"),
                    }
                }
                c if c < b' ' => return self.err("invalid character in string literal"),
                _ => {}
            }
        }
        let raw = self.data.get(start..self.pos - 1).unwrap_or_default();
        Ok((unquote(raw)?, escaped))
    }
}

/// `utf16.DecodeRune` for a validated pair; `None` for `unicode.ReplacementChar`.
fn utf16_pair(hi: u32, lo: u32) -> Option<char> {
    if (0xD800..0xDC00).contains(&hi) && (0xDC00..0xE000).contains(&lo) {
        char::from_u32(0x10000 + (((hi - 0xD800) << 10) | (lo - 0xDC00)))
    } else {
        None
    }
}

/// Four hex digits at the start of `s`.
fn hex4(s: &[u8]) -> Option<u32> {
    let h = s.get(..4)?;
    h.iter()
        .try_fold(0u32, |a, &c| char::from(c).to_digit(16).map(|d| a * 16 + d))
}

/// `unquoteBytes` for the body of a string already validated by the scanner: escapes are
/// resolved, a lone or invalid surrogate becomes U+FFFD and so does every byte of invalid UTF-8.
fn unquote(raw: &[u8]) -> Result<String, Error> {
    let mut out = String::new();
    out.try_reserve(raw.len())?;
    let mut r = 0;
    while let Some(&c) = raw.get(r) {
        if c == b'\\' {
            r += 1;
            match raw.get(r) {
                Some(b'b') => out.push('\u{8}'),
                Some(b'f') => out.push('\u{c}'),
                Some(b'n') => out.push('\n'),
                Some(b'r') => out.push('\r'),
                Some(b't') => out.push('\t'),
                Some(b'u') => {
                    let rr = hex4(raw.get(r + 1..).unwrap_or_default()).unwrap_or(0xFFFD);
                    r += 4;
                    let ch = if (0xD800..0xE000).contains(&rr) {
                        // A high surrogate takes the escape after it when that is a low one.
                        let next = raw
                            .get(r + 1..r + 3)
                            .filter(|p| *p == b"\\u")
                            .and_then(|_| hex4(raw.get(r + 3..).unwrap_or_default()));
                        match next.and_then(|lo| utf16_pair(rr, lo)) {
                            Some(pair) => {
                                r += 6;
                                pair
                            }
                            None => '\u{FFFD}',
                        }
                    } else {
                        char::from_u32(rr).unwrap_or('\u{FFFD}')
                    };
                    out.push(ch);
                }
                // '"', '\\' and '/'.
                Some(&other) => out.push(char::from(other)),
                None => {}
            }
            r += 1;
        } else if c < 0x80 {
            out.push(char::from(c));
            r += 1;
        } else {
            match decode_rune(raw.get(r..).unwrap_or_default()) {
                Some((ch, n)) => {
                    out.push(ch);
                    r += n;
                }
                None => {
                    out.push('\u{FFFD}');
                    r += 1;
                }
            }
        }
    }
    Ok(out)
}

/// `foldName`: the key with ASCII lower case raised and the two non-ASCII letters that fold to
/// ASCII (U+017F to `S`, U+212A to `K`) mapped, so that two names fold equal exactly when
/// `bytes.EqualFold` says they match, for names made of ASCII letters.
pub fn fold_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{17F}' => 'S',
            '\u{212A}' => 'K',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

/// The values of the struct field `name` in document order: every member whose key equals
/// `name` or folds equal to it (an exact match is preferred by Go, which makes no difference
/// when no two fields fold equal). Apply them in order; unknown keys are simply not returned.
pub fn field_values<'a>(
    fields: &'a [(String, Value)],
    name: &str,
) -> impl Iterator<Item = &'a Value> + use<'a> {
    let name = name.to_owned();
    let folded = fold_name(&name);
    fields
        .iter()
        .filter(move |(k, _)| *k == name || fold_name(k) == folded)
        .map(|(_, v)| v)
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String { .. } => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    fn mismatch<T>(&self, into: &str) -> Result<T, Error> {
        Err(Error::Type(format!(
            "Go value of type {into}: JSON {}",
            self.kind()
        )))
    }

    /// The members of an object; `None` for `null` (no effect).
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not an object or `null`.
    pub fn as_object(&self) -> Result<Option<&[(String, Value)]>, Error> {
        match self {
            Value::Object(m) => Ok(Some(m)),
            Value::Null => Ok(None),
            v => v.mismatch("struct"),
        }
    }

    /// The text of a string; `None` for `null`.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not a string or `null`.
    pub fn as_str(&self) -> Result<Option<&str>, Error> {
        match self {
            Value::String { text, .. } => Ok(Some(text)),
            Value::Null => Ok(None),
            v => v.mismatch("string"),
        }
    }

    /// A float64 as `strconv.ParseFloat(lit, 64)` gives it; a literal that overflows is an
    /// error. `None` for `null`.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not a finite number or `null`.
    pub fn as_f64(&self) -> Result<Option<f64>, Error> {
        match self {
            Value::Number(lit) => match lit.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(Some(f)),
                _ => Err(Error::Type(format!(
                    "number {lit} into Go value of type float64"
                ))),
            },
            Value::Null => Ok(None),
            v => v.mismatch("float64"),
        }
    }

    /// An int as `strconv.ParseInt(lit, 10, 64)` gives it: `1.0` and `1e2` are errors. `None`
    /// for `null`.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not an integer literal or `null`.
    pub fn as_i64(&self) -> Result<Option<i64>, Error> {
        match self {
            Value::Number(lit) => lit
                .parse::<i64>()
                .map(Some)
                .map_err(|_| Error::Type(format!("number {lit} into Go value of type int"))),
            Value::Null => Ok(None),
            v => v.mismatch("int"),
        }
    }

    /// A `time.Time` as `Time.UnmarshalJSON` reads it: an unescaped RFC 3339 string. `None` for
    /// `null`.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not a string or `null`, [`Error::Time`] if the string is not RFC 3339.
    pub fn as_time(&self) -> Result<Option<Time>, Error> {
        match self {
            Value::String { text, .. } => Time::parse_rfc3339(text.as_bytes()).map(Some),
            Value::Null => Ok(None),
            v => v.mismatch("time.Time"),
        }
    }

    /// A slice: each element through `f`, where `None` (a `null` element) leaves the zero
    /// value. `None` for a `null` slice.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not an array or `null`, [`Error::OutOfMemory`] if the
    /// allocator refuses, or the first error from `f`.
    pub fn to_vec<T: Default>(
        &self,
        mut f: impl FnMut(&Value) -> Result<Option<T>, Error>,
    ) -> Result<Option<Vec<T>>, Error> {
        let items = match self {
            Value::Array(items) => items,
            Value::Null => return Ok(None),
            v => return v.mismatch("slice"),
        };
        let mut out = Vec::new();
        out.try_reserve_exact(items.len())?;
        for item in items {
            out.push(f(item)?.unwrap_or_default());
        }
        Ok(Some(out))
    }

    /// A `[N]T` array: a short input leaves the rest zero, a long input is dropped past `N`.
    /// `None` for `null`.
    ///
    /// # Errors
    /// [`Error::Type`] if the value is not an array or `null`, or the first error from `f`.
    pub fn to_array<T: Default + Copy, const N: usize>(
        &self,
        mut f: impl FnMut(&Value) -> Result<Option<T>, Error>,
    ) -> Result<Option<[T; N]>, Error> {
        let items = match self {
            Value::Array(items) => items,
            Value::Null => return Ok(None),
            v => return v.mismatch("array"),
        };
        let mut out = [T::default(); N];
        // Go skips the elements past the array without decoding them.
        for (slot, item) in out.iter_mut().zip(items) {
            *slot = f(item)?.unwrap_or_default();
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits::default()
    }

    fn marshal_string(s: &str) -> String {
        let mut w = Writer::new(&lim());
        w.string(s).expect("string");
        String::from_utf8(w.finish()).expect("utf8")
    }

    fn marshal_f64(f: f64) -> String {
        let mut w = Writer::new(&lim());
        w.f64(f).expect("float");
        String::from_utf8(w.finish()).expect("utf8")
    }

    // TestEncodeString: the control characters.
    #[test]
    fn encode_string_controls() {
        for c in 0u8..0x20 {
            let want = match c {
                8 => "\"\\b\"".to_string(),
                9 => "\"\\t\"".to_string(),
                10 => "\"\\n\"".to_string(),
                12 => "\"\\f\"".to_string(),
                13 => "\"\\r\"".to_string(),
                c => format!("\"\\u{c:04x}\""),
            };
            assert_eq!(marshal_string(&char::from(c).to_string()), want, "{c:#x}");
        }
    }

    // The escapes of appendString with escapeHTML on.
    #[test]
    fn encode_string_escapes() {
        assert_eq!(marshal_string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(
            marshal_string("<a href=\"x\">&</a>"),
            r#""\u003ca href=\"x\"\u003e\u0026\u003c/a\u003e""#
        );
        assert_eq!(marshal_string("\u{2028}\u{2029}"), r#""\u2028\u2029""#);
        assert_eq!(
            marshal_string("\u{7f}é日\u{1F600}"),
            "\"\u{7f}é日\u{1F600}\""
        );
        assert_eq!(marshal_string(""), "\"\"");
    }

    #[test]
    fn encode_float_forms() {
        for (f, want) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (100.0, "100"),
            (0.1, "0.1"),
            (-2.5, "-2.5"),
            (1e-6, "0.000001"),
            (9.999999e-7, "9.999999e-7"),
            (1e-7, "1e-7"),
            (1.5e-10, "1.5e-10"),
            (-1.5e-10, "-1.5e-10"),
            (123456789.125, "123456789.125"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (-1e300, "-1e+300"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (0.30000000000000004, "0.30000000000000004"),
        ] {
            assert_eq!(marshal_f64(f), want, "{f:e}");
        }
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut w = Writer::new(&lim());
            assert!(matches!(w.f64(f), Err(Error::Unsupported(_))));
        }
    }

    // TestMarshalFloat, 64-bit half: the text parses back to the same float and has Go's form.
    #[test]
    fn marshal_float_round_trip_and_form() {
        let digits = "1.2345678901234567890123";
        for i in (2..=digits.len()).rev() {
            for exp in -30..=30 {
                for sign in ['+', '-'] {
                    let s = format!("{sign}{}e{exp}", &digits[..i]);
                    let f: f64 = s.parse().expect("float");
                    for g in [
                        f,
                        f64::from_bits(f.to_bits() + 1),
                        f64::from_bits(f.to_bits() - 1),
                    ] {
                        let out = marshal_f64(g);
                        let back: f64 = out.parse().expect("float");
                        assert_eq!(back.to_bits(), g.to_bits(), "{out}");
                        check_form(&out);
                    }
                }
            }
        }
    }

    /// The badFloatREs of encode_test.go, by hand.
    fn check_form(out: &str) {
        assert!(!out.contains('p'), "{out}");
        assert!(!out.starts_with('+'), "{out}");
        let unsigned = out.strip_prefix('-').unwrap_or(out);
        assert!(!unsigned.starts_with('.'), "{out}");
        if unsigned.starts_with('0') {
            assert!(
                unsigned.len() == 1 || unsigned.as_bytes().get(1) == Some(&b'.'),
                "{out}"
            );
        }
        let (mant, exp) = match unsigned.split_once('e') {
            Some((m, e)) => (m, Some(e)),
            None => (unsigned, None),
        };
        if let Some((int, frac)) = mant.split_once('.') {
            assert!(!frac.is_empty() && !frac.ends_with('0'), "{out}");
            if exp.is_some() {
                assert!(int.len() == 1 && int != "0", "{out}");
            }
        }
        if let Some(e) = exp {
            let (sign, digits) = e.split_at(1);
            assert!(sign == "+" || sign == "-", "{out}");
            assert!(!digits.starts_with('0'), "{out}");
            let n: i32 = digits.parse().expect("exp");
            if sign == "-" {
                assert!(n >= 7, "{out}");
            } else {
                assert!(n >= 21, "{out}");
            }
        } else {
            assert!(!unsigned.starts_with("0.0000000"), "{out}");
        }
    }

    #[test]
    fn writer_structure_and_budget() {
        let mut w = Writer::new(&lim());
        w.begin_object().expect("w");
        w.key("version").expect("w");
        w.string("0.2.0").expect("w");
        w.key("pages").expect("w");
        w.begin_array().expect("w");
        for i in 0..3 {
            w.item().expect("w");
            w.i64(i).expect("w");
        }
        w.end_array().expect("w");
        w.key("n").expect("w");
        w.null().expect("w");
        w.end_object().expect("w");
        assert_eq!(
            String::from_utf8(w.finish()).expect("utf8"),
            r#"{"version":"0.2.0","pages":[0,1,2],"n":null}"#
        );

        let tiny = Limits {
            max_alloc: 8,
            ..Limits::default()
        };
        let mut w = Writer::new(&tiny);
        assert!(w.string("short").is_ok());
        assert!(matches!(
            w.string("this does not fit"),
            Err(Error::Alloc { .. })
        ));
    }

    // ---- time ----

    fn t(s: &str) -> Result<Time, Error> {
        Time::parse_rfc3339(s.as_bytes())
    }

    #[test]
    fn time_zero_and_format() {
        assert_eq!(
            Time::ZERO.format_rfc3339_nano().expect("fmt"),
            "0001-01-01T00:00:00Z"
        );
        let x = Time {
            unix: 1_136_214_245,
            nanos: 123_456_789,
            offset_secs: -7 * 3600,
        };
        assert_eq!(
            x.format_rfc3339_nano().expect("fmt"),
            "2006-01-02T08:04:05.123456789-07:00"
        );
        let y = Time {
            nanos: 120_000_000,
            offset_secs: 5 * 3600 + 30 * 60,
            ..x
        };
        assert_eq!(
            y.format_rfc3339_nano().expect("fmt"),
            "2006-01-02T20:34:05.12+05:30"
        );
        let from_go = Time::from_go_time(pdf::GoTime {
            unix: 1_136_214_245,
            utc_offset_min: 0,
        });
        assert_eq!(
            from_go.format_rfc3339_nano().expect("fmt"),
            "2006-01-02T15:04:05Z"
        );
    }

    #[test]
    fn time_format_errors() {
        // Year 10000 and year -1 have no RFC 3339 form (Time.MarshalJSON errors).
        let far = Time {
            unix: 253_402_300_800,
            nanos: 0,
            offset_secs: 0,
        };
        assert!(far.format_rfc3339_nano().is_err());
        assert!(
            Time {
                unix: -62_167_219_201,
                ..far
            }
            .format_rfc3339_nano()
            .is_err()
        );
        assert!(
            Time {
                unix: -62_167_219_200,
                ..far
            }
            .format_rfc3339_nano()
            .is_ok()
        );
        assert!(
            Time {
                unix: 253_402_300_799,
                ..far
            }
            .format_rfc3339_nano()
            .is_ok()
        );
        // An offset of 24 hours or more.
        assert!(
            Time {
                unix: 0,
                nanos: 0,
                offset_secs: 24 * 3600
            }
            .format_rfc3339_nano()
            .is_err()
        );
    }

    #[test]
    fn time_parse_round_trips() {
        for s in [
            "2006-01-02T15:04:05Z",
            "2006-01-02T15:04:05.999999999Z",
            "2006-01-02T15:04:05.5-07:00",
            "2024-02-29T23:59:59.000000001+14:00",
            "0000-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
            "1969-12-31T23:59:59-00:30",
        ] {
            let x = t(s).expect(s);
            assert_eq!(x.format_rfc3339_nano().expect("fmt"), s);
        }
        let x = t("2006-01-02T15:04:05+00:00").expect("parse");
        assert_eq!(
            x.format_rfc3339_nano().expect("fmt"),
            "2006-01-02T15:04:05Z"
        );
        assert_eq!(x.unix, 1_136_214_245);
        let y = t("2006-01-02T15:04:05.123456789123Z").expect("parse");
        assert_eq!(y.nanos, 123_456_789);
    }

    #[test]
    fn time_parse_lenient_forms_of_go_parse() {
        // time.Parse(RFC3339, ...) accepts these although RFC 3339 does not.
        assert_eq!(
            t("2006-01-02T1:04:05Z").expect("hour").unix,
            1_136_214_245 - 14 * 3600
        );
        assert_eq!(
            t("2006-01-02T15:04:05,5Z").expect("comma").nanos,
            500_000_000
        );
        assert_eq!(
            t("2006-01-02T15:04:05+24:00").expect("tz24").offset_secs,
            24 * 3600
        );
    }

    #[test]
    fn time_parse_rejects() {
        for s in [
            "",
            "2006-01-02",
            "2006-01-02 15:04:05Z",
            "2006-01-02T15:04:05",
            "2006-01-02T15:04:05z",
            "2006-01-02T15:04:05+0700",
            "2006-01-02T15:04:05+07:0",
            "2006-01-02T15:04:05+25:00",
            "2006-01-02T15:04:05+07:61",
            "2006-01-02T15:04:05Zx",
            "2006-13-02T15:04:05Z",
            "2006-00-02T15:04:05Z",
            "2006-01-32T15:04:05Z",
            "2006-02-29T15:04:05Z",
            "2006-01-02T24:04:05Z",
            "2006-01-02T15:60:05Z",
            "2006-01-02T15:04:60Z",
            "2006-1-02T15:04:05Z",
            "2006-01-2T15:04:05Z",
            "06-01-02T15:04:05Z",
            "2006-01-02T15:04:05.Z",
            "+006-01-02T15:04:05Z",
        ] {
            assert!(t(s).is_err(), "{s}");
        }
        assert!(t("2008-02-29T15:04:05Z").is_ok());
        assert!(t("2000-02-29T15:04:05Z").is_ok());
        assert!(t("1900-02-29T15:04:05Z").is_err());
    }

    // ---- parse ----

    fn p(s: &str) -> Result<Value, Error> {
        parse(s.as_bytes(), &lim())
    }

    // TestValid and TestUnmarshalSyntax.
    #[test]
    fn syntax_valid_and_invalid() {
        for (s, ok) in [
            ("foo", false),
            ("}{", false),
            ("{]", false),
            ("{}", true),
            (r#"{"foo":"bar"}"#, true),
            (r#"{"foo":"bar","bar":{"baz":["qux"]}}"#, true),
            ("tru", false),
            ("fals", false),
            ("nul", false),
            ("123e", false),
            (r#""hello"#, false),
            ("[1,2,3", false),
            (r#"{"key":1"#, false),
            (r#"{"key":1,"#, false),
            ("", false),
            ("  ", false),
            (" \t\r\n null \n", true),
            ("[1,]", false),
            ("[,1]", false),
            (r#"{"a":1,}"#, false),
            (r#"{a:1}"#, false),
            (r#"{"a" 1}"#, false),
            ("01", false),
            ("-", false),
            ("1.", false),
            (".5", false),
            ("1e", false),
            ("1e+", false),
            ("+1", false),
            ("-0", true),
            ("0.5e-10", true),
            ("1E5", true),
            ("[1 2]", false),
            ("true false", false),
            ("truex", false),
            (r#""\x""#, false),
            (r#""\u12""#, false),
            (r#""\u12G4""#, false),
            (r#""\'""#, false),
            ("\"a\u{1}b\"", false),
            ("\"a\tb\"", false),
            ("\u{feff}1", false),
            ("[\"\u{ff}\"]", true),
        ] {
            assert_eq!(p(s).is_ok(), ok, "{s:?}");
        }
        // Invalid UTF-8 inside a string is not a syntax error.
        assert!(parse(b"\"a\xffb\"", &lim()).is_ok());
    }

    // TestUnmarshalMaxDepth.
    #[test]
    fn max_nesting_depth() {
        let arr = |n: usize| format!("{{\"a\":{}{}}}", "[".repeat(n), "]".repeat(n));
        assert!(p(&arr(MAX_NESTING_DEPTH - 1)).is_ok());
        assert!(matches!(
            p(&arr(MAX_NESTING_DEPTH)),
            Err(Error::Syntax { .. })
        ));
        assert!(matches!(p(&arr(3_000_000)), Err(Error::Syntax { .. })));
        let obj = |n: usize| format!("{{\"a\":{}0{}}}", "{\"a\":".repeat(n), "}".repeat(n));
        assert!(p(&obj(MAX_NESTING_DEPTH - 1)).is_ok());
        assert!(matches!(
            p(&obj(MAX_NESTING_DEPTH)),
            Err(Error::Syntax { .. })
        ));
        assert!(matches!(p(&obj(3_000_000)), Err(Error::Syntax { .. })));
        // A 9 999 deep tree drops without recursing.
        let deep = p(&format!("{}{}", "[".repeat(9_999), "]".repeat(9_999))).expect("deep");
        drop(deep);
    }

    #[test]
    fn parse_tree() {
        let v =
            p(r#" {"a": [1, "x", null, true, false, {}], "b": -0.5e3, "a": 2} "#).expect("parse");
        let Value::Object(m) = &v else {
            panic!("not an object");
        };
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].0, "a");
        assert_eq!(m[1].1, Value::Number("-0.5e3".to_string()));
        let Value::Array(a) = &m[0].1 else {
            panic!("not an array");
        };
        assert_eq!(
            *a,
            vec![
                Value::Number("1".to_string()),
                Value::String {
                    text: "x".to_string(),
                    escaped: false
                },
                Value::Null,
                Value::Bool(true),
                Value::Bool(false),
                Value::Object(vec![]),
            ]
        );
    }

    // The string cases of decode_test.go.
    #[test]
    fn unquote_cases() {
        let s = |input: &[u8]| {
            let v = parse(input, &lim()).expect("parse");
            match &v {
                Value::String { text, .. } => text.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(s(br#""a\u1234""#), "a\u{1234}");
        assert_eq!(s(br#""http:\/\/""#), "http://");
        assert_eq!(s(br#""g-clef: \uD834\uDD1E""#), "g-clef: \u{1D11E}");
        assert_eq!(
            s(br#""invalid: \uD834x\uDD1E""#),
            "invalid: \u{FFFD}x\u{FFFD}"
        );
        assert_eq!(s(br#""hello\ud800world""#), "hello\u{FFFD}world");
        assert_eq!(
            s(br#""hello\ud800\ud800world""#),
            "hello\u{FFFD}\u{FFFD}world"
        );
        assert_eq!(s(b"\"hello\xc2\xffworld\""), "hello\u{FFFD}\u{FFFD}world");
        assert_eq!(
            s(b"\"hello\xed\xa0\x80\xed\xb0\x80world\""),
            "hello\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}world"
        );
        assert_eq!(s(br#""\b\f\n\r\t\"\\""#), "\u{8}\u{c}\n\r\t\"\\");
        assert_eq!(s(br#""\udc00""#), "\u{FFFD}");
        assert_eq!(s(br#""\ud800\u0041""#), "\u{FFFD}A");
        assert_eq!(s("\"日本\"".as_bytes()), "日本");
        let v = p(r#""a\nb""#).expect("parse");
        let Value::String { escaped, .. } = &v else {
            panic!("not a string");
        };
        assert!(*escaped);
    }

    #[test]
    fn parse_budget() {
        let tiny = Limits {
            max_alloc: 64,
            ..Limits::default()
        };
        assert!(matches!(
            parse(b"[1,2,3,4,5,6,7,8,9,10]", &tiny),
            Err(Error::Alloc { .. })
        ));
    }

    // ---- typed access ----

    #[test]
    fn scalars_and_null() {
        assert_eq!(p("1.5").expect("p").as_f64(), Ok(Some(1.5)));
        assert_eq!(
            p("-0").expect("p").as_f64().expect("f").map(f64::to_bits),
            Some((-0.0f64).to_bits())
        );
        assert_eq!(p("1e5").expect("p").as_f64(), Ok(Some(100_000.0)));
        assert_eq!(p("1e-400").expect("p").as_f64(), Ok(Some(0.0)));
        assert!(p("1e400").expect("p").as_f64().is_err());
        assert!(p("\"1\"").expect("p").as_f64().is_err());
        assert_eq!(p("null").expect("p").as_f64(), Ok(None));
        assert_eq!(p("7").expect("p").as_i64(), Ok(Some(7)));
        assert_eq!(p("-0").expect("p").as_i64(), Ok(Some(0)));
        assert!(p("1.0").expect("p").as_i64().is_err());
        assert!(p("1e2").expect("p").as_i64().is_err());
        assert!(p("9223372036854775808").expect("p").as_i64().is_err());
        assert_eq!(
            p("-9223372036854775808").expect("p").as_i64(),
            Ok(Some(i64::MIN))
        );
        assert_eq!(p("\"x\"").expect("p").as_str(), Ok(Some("x")));
        assert!(p("1").expect("p").as_str().is_err());
        assert_eq!(p("null").expect("p").as_str(), Ok(None));
        assert!(p("[]").expect("p").as_object().is_err());
        assert_eq!(p("null").expect("p").as_object(), Ok(None));
    }

    #[test]
    fn arrays_and_slices() {
        let f = |v: &Value| v.as_f64();
        let short: Option<[f64; 4]> = p("[1,2]").expect("p").to_array(f).expect("a");
        assert_eq!(short, Some([1.0, 2.0, 0.0, 0.0]));
        let long: Option<[f64; 2]> = p("[1,2,3]").expect("p").to_array(f).expect("a");
        assert_eq!(long, Some([1.0, 2.0]));
        let nulls: Option<[f64; 3]> = p("[null,5]").expect("p").to_array(f).expect("a");
        assert_eq!(nulls, Some([0.0, 5.0, 0.0]));
        assert_eq!(p("null").expect("p").to_array::<f64, 2>(f), Ok(None));
        assert!(p("[\"a\"]").expect("p").to_array::<f64, 2>(f).is_err());
        // An element past the array is skipped without decoding.
        assert!(p("[1,2,\"a\"]").expect("p").to_array::<f64, 2>(f).is_ok());
        assert!(p("{}").expect("p").to_array::<f64, 2>(f).is_err());

        let names = p(r#"["a",null,"c"]"#)
            .expect("p")
            .to_vec(|v| Ok(v.as_str()?.map(str::to_string)))
            .expect("v");
        assert_eq!(
            names,
            Some(vec!["a".to_string(), String::new(), "c".to_string()])
        );
        assert_eq!(p("[]").expect("p").to_vec(f), Ok(Some(vec![])));
        assert_eq!(p("null").expect("p").to_vec(f), Ok(None));
    }

    #[test]
    fn field_matching_folds_case() {
        let v = p(r#"{"Version":"1","version":"2","VERSION":"3","versions":"x","ſcore":"s"}"#)
            .expect("p");
        let m = v.as_object().expect("o").expect("some");
        let vals: Vec<_> = field_values(m, "version")
            .map(|v| v.as_str().expect("s").expect("some"))
            .collect();
        assert_eq!(vals, ["1", "2", "3"]);
        assert_eq!(field_values(m, "score").count(), 1);
        assert_eq!(field_values(m, "absent").count(), 0);
        assert_eq!(fold_name("raster_box"), "RASTER_BOX");
        assert_eq!(fold_name("raster_\u{17F}ox"), fold_name("RASTER_Sox"));
        assert_eq!(fold_name("\u{212A}"), "K");
    }

    #[test]
    fn time_values() {
        let v = p(r#"{"a":"2006-01-02T15:04:05Z","b":null,"c":5,"d":"2006-01-02T15:04:05\u005a"}"#)
            .expect("p");
        let m = v.as_object().expect("o").expect("some");
        let get = |k| field_values(m, k).next().expect("field");
        assert_eq!(
            get("a").as_time().expect("t").map(|t| t.unix),
            Some(1_136_214_245)
        );
        assert_eq!(get("b").as_time(), Ok(None));
        assert!(get("c").as_time().is_err());
        // v2 unquotes the string before parsing the time.
        assert_eq!(
            get("d").as_time().expect("t").map(|t| t.unix),
            Some(1_136_214_245)
        );
    }

    // The whole shape of a Provenance record, written and read back.
    #[test]
    fn provenance_shaped_round_trip() {
        let when = Time {
            unix: 1_700_000_000,
            nanos: 5,
            offset_secs: 3600,
        };
        let mut w = Writer::new(&lim());
        w.begin_object().expect("w");
        w.key("version").expect("w");
        w.string("0.2.0").expect("w");
        w.key("capabilities").expect("w");
        w.begin_array().expect("w");
        for c in ["build-pdf", "inspect"] {
            w.item().expect("w");
            w.string(c).expect("w");
        }
        w.end_array().expect("w");
        w.key("processed_at").expect("w");
        w.time(&when).expect("w");
        w.key("pages").expect("w");
        w.begin_array().expect("w");
        w.item().expect("w");
        w.begin_object().expect("w");
        w.key("placement").expect("w");
        w.begin_array().expect("w");
        for x in [1.0, 0.0, -0.5, 1e-7] {
            w.item().expect("w");
            w.f64(x).expect("w");
        }
        w.end_array().expect("w");
        w.end_object().expect("w");
        w.end_array().expect("w");
        w.end_object().expect("w");
        let bytes = w.finish();
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert_eq!(
            text,
            r#"{"version":"0.2.0","capabilities":["build-pdf","inspect"],"processed_at":"2023-11-14T23:13:20.000000005+01:00","pages":[{"placement":[1,0,-0.5,1e-7]}]}"#
        );
        let v = parse(&bytes, &lim()).expect("parse");
        let m = v.as_object().expect("o").expect("some");
        let at = field_values(m, "processed_at").next().expect("f");
        assert_eq!(at.as_time(), Ok(Some(when)));
        let pages = field_values(m, "pages").next().expect("f");
        let placement = pages
            .to_vec(|page| {
                let o = page.as_object()?.unwrap_or_default();
                match field_values(o, "placement").last() {
                    Some(v) => v.to_vec(Value::as_f64),
                    None => Ok(None),
                }
            })
            .expect("pages");
        assert_eq!(placement, Some(vec![vec![1.0, 0.0, -0.5, 1e-7]]));
    }
}
