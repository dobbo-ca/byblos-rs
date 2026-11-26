//! `encoding/xml` (`xml.go`) as byblos reaches it from `embed.go:723-747`: the `Decoder`
//! tokenizer (`NewDecoder`, `Token`, `RawToken`) with its token types, name space translation,
//! `Strict` mode, `AutoClose`, `Entity`, `DefaultSpace` and the syntax errors. Only the byte
//! slice input path is ported (`NewDecoder(bytes.NewReader(x))`); `CharsetReader`,
//! `NewTokenDecoder`, `Unmarshal` and the encoder are not. A non-UTF-8 `encoding` in the XML
//! declaration therefore always fails as Go does with a nil `CharsetReader`.
//!
//! The decoder holds at most one copy of the input per token, so its memory is bounded by the
//! input length times a small constant, plus the name space URLs copied into translated names
//! (Go shares the string); those copies are charged to a budget of `alloc_limit` bytes over the
//! decoder's life. [`Decoder::new`] checks the first bound against the allocation limit, and
//! callers (`embed`) cap the packet and the token count. The decoder has no recursion.
//!
//! Differences from Go that cannot be seen through byblos: error text for invalid UTF-8 inside
//! a message is lossy, `%q` is Rust's `{:?}`, and `strings.EqualFold` (used by `AutoClose`) is
//! the per-character lower/upper case comparison.

use crate::limits::Limits;
use std::collections::HashMap;
use std::fmt;

/// An error of the decoder: `io.EOF`, `*xml.SyntaxError` or a plain `fmt.Errorf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `io.EOF`: the input ended between tokens.
    Eof,
    /// `*SyntaxError`.
    Syntax { msg: String, line: usize },
    /// The `fmt.Errorf` errors (unsupported version, undeclared encoding).
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Eof => f.write_str("EOF"),
            Error::Syntax { msg, line } => write!(f, "XML syntax error on line {line}: {msg}"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

/// `xml.Name`: an XML name with its name space (the URL once translated).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Name {
    pub space: String,
    pub local: String,
}

/// `xml.Attr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: Name,
    pub value: String,
}

/// `xml.StartElement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartElement {
    pub name: Name,
    pub attr: Vec<Attr>,
}

/// `xml.EndElement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndElement {
    pub name: Name,
}

/// `xml.Token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    StartElement(StartElement),
    EndElement(EndElement),
    /// Text with its escape sequences replaced.
    CharData(Vec<u8>),
    /// The bytes between `<!--` and `-->`.
    Comment(Vec<u8>),
    ProcInst {
        target: String,
        inst: Vec<u8>,
    },
    /// The bytes between `<!` and `>`.
    Directive(Vec<u8>),
}

/// `xml.HTMLAutoClose`: the HTML elements that never have content.
pub const HTML_AUTO_CLOSE: &[&str] = &[
    "basefont", "br", "area", "link", "img", "param", "hr", "input", "col", "frame", "isindex",
    "base", "meta",
];

const XML_URL: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_PREFIX: &str = "xmlns";
const XML_PREFIX: &str = "xml";

/// One entry of the parse stack: an open element, or the name space binding that the element
/// above it replaced (to restore when that element closes).
#[derive(Debug)]
enum Stk {
    Start(Name),
    Ns {
        local: String,
        url: String,
        ok: bool,
    },
}

/// `xml.Decoder` over an in-memory input.
#[derive(Debug)]
pub struct Decoder<'a> {
    /// `Strict`, true by default.
    pub strict: bool,
    /// `AutoClose`, used when `strict` is false.
    pub auto_close: Vec<String>,
    /// `Entity`: replacements beyond `lt`, `gt`, `amp`, `apos` and `quot`.
    pub entity: Option<HashMap<String, String>>,
    /// `DefaultSpace`.
    pub default_space: String,
    data: &'a [u8],
    pos: usize,
    buf: Vec<u8>,
    stk: Vec<Stk>,
    need_close: bool,
    to_close: Name,
    next_token: Option<Token>,
    ns: HashMap<String, String>,
    /// Bytes of name space URL still allowed to be copied by `translate`.
    ns_budget: u64,
    err: Option<Error>,
    line: usize,
}

impl<'a> Decoder<'a> {
    /// `NewDecoder`. Refuses an input whose working buffers (about eight times its length)
    /// would not fit the allocation limit.
    ///
    /// # Errors
    ///
    /// [`Error::Other`] when eight times `data.len()` exceeds `Limits::alloc_limit()`.
    pub fn new(data: &'a [u8], limits: &Limits) -> Result<Self, Error> {
        let need = (data.len() as u64).saturating_mul(8);
        if need > limits.alloc_limit() {
            return Err(Error::Other(
                "XML input exceeds the allocation limit".to_string(),
            ));
        }
        Ok(Decoder {
            strict: true,
            auto_close: Vec::new(),
            entity: None,
            default_space: String::new(),
            data,
            pos: 0,
            buf: Vec::new(),
            stk: Vec::new(),
            need_close: false,
            to_close: Name::default(),
            next_token: None,
            ns: HashMap::new(),
            ns_budget: limits.alloc_limit(),
            err: None,
            line: 1,
        })
    }

    /// `InputOffset`: the byte offset of the end of the most recent token.
    pub fn input_offset(&self) -> usize {
        self.pos
    }

    /// `Token`: the next token with name spaces translated, start and end elements checked to
    /// nest. At the end of the input it returns `Error::Eof`.
    ///
    /// # Errors
    /// [`Error::Eof`] at the end of the input, [`Error::Syntax`] for malformed or mismatched markup,
    /// [`Error::Other`] for an unsupported version or encoding.
    pub fn token(&mut self) -> Result<Token, Error> {
        let mut t = match self.next_token.take() {
            Some(t) => t,
            None => match self.raw_token() {
                Ok(t) => t,
                Err(Error::Eof) if !self.stk.is_empty() => {
                    return Err(self.syntax_error("unexpected EOF"));
                }
                Err(e) => return Err(e),
            },
        };
        if !self.strict
            && let Some(t1) = self.auto_close_token(&t)
        {
            self.next_token = Some(t);
            t = t1;
        }
        match &mut t {
            Token::StartElement(se) => {
                // The translations listed in the attributes apply to the element name and to
                // the other attribute names, so process them first.
                for a in &se.attr {
                    if a.name.space == XMLNS_PREFIX {
                        self.push_ns(a.name.local.clone(), a.value.clone());
                    }
                    if a.name.space.is_empty() && a.name.local == XMLNS_PREFIX {
                        self.push_ns(String::new(), a.value.clone());
                    }
                }
                self.stk.push(Stk::Start(se.name.clone()));
                let mut ok = self.translate(&mut se.name, true);
                for a in &mut se.attr {
                    ok = ok && self.translate(&mut a.name, false);
                }
                if !ok {
                    return Err(self.fail());
                }
            }
            Token::EndElement(ee) => {
                if !self.pop_element(ee) {
                    return Err(self.fail());
                }
            }
            _ => {}
        }
        Ok(t)
    }

    /// Binds `ns[local] = url` and records the old binding on the stack.
    fn push_ns(&mut self, local: String, url: String) {
        let old = self.ns.insert(local.clone(), url);
        self.stk.push(Stk::Ns {
            local,
            ok: old.is_some(),
            url: old.unwrap_or_default(),
        });
    }

    /// Applies name space translation to `n`. The default name space applies only to element
    /// names, not to attribute names.
    /// Returns false (with the sticky error set) when the copied URLs exceed the budget.
    fn translate(&mut self, n: &mut Name, is_element: bool) -> bool {
        if n.space == XMLNS_PREFIX || (n.space.is_empty() && !is_element) {
            return true;
        }
        if n.space == XML_PREFIX {
            n.space = XML_URL.to_string();
        } else if n.space.is_empty() && n.local == XMLNS_PREFIX {
            return true;
        }
        if let Some(v) = self.ns.get(&n.space) {
            match self.ns_budget.checked_sub(v.len() as u64) {
                Some(b) => self.ns_budget = b,
                None => {
                    self.set_other("XML name space URLs exceed the allocation limit".to_string());
                    return false;
                }
            }
            n.space = v.clone();
        } else if n.space.is_empty() {
            n.space = self.default_space.clone();
        }
        true
    }

    fn syntax_error(&self, msg: &str) -> Error {
        Error::Syntax {
            msg: msg.to_string(),
            line: self.line,
        }
    }

    /// Records a syntax error as the sticky decoder error and returns it.
    fn set_err(&mut self, msg: &str) -> Error {
        let e = self.syntax_error(msg);
        self.err = Some(e.clone());
        e
    }

    /// Records a non-syntax error as the sticky decoder error and returns it.
    fn set_other(&mut self, msg: String) -> Error {
        let e = Error::Other(msg);
        self.err = Some(e.clone());
        e
    }

    /// The sticky error (`d.err`).
    fn fail(&self) -> Error {
        self.err.clone().unwrap_or(Error::Eof)
    }

    /// Fails with `msg` unless an error (an unexpected EOF, an invalid name) is already set:
    /// the callers of `nsname` and `name` let them provide better context.
    fn name_err(&mut self, msg: &str) -> Error {
        if self.err.is_none() {
            return self.set_err(msg);
        }
        self.fail()
    }

    /// Records that we are ending an element. After popping it, undoes the name space
    /// translations that came with it.
    fn pop_element(&mut self, t: &mut EndElement) -> bool {
        match self.stk.pop() {
            Some(Stk::Start(s)) => {
                if s.local != t.name.local {
                    if !self.strict {
                        self.need_close = true;
                        self.to_close = t.name.clone();
                        t.name = s;
                        return true;
                    }
                    let msg = format!("element <{}> closed by </{}>", s.local, t.name.local);
                    self.set_err(&msg);
                    return false;
                }
                if s.space != t.name.space {
                    let ns = if t.name.space.is_empty() {
                        "\"\""
                    } else {
                        t.name.space.as_str()
                    };
                    let msg = format!(
                        "element <{}> in space {} closed by </{}> in space {}",
                        s.local, s.space, t.name.local, ns
                    );
                    self.set_err(&msg);
                    return false;
                }
            }
            _ => {
                let msg = format!("unexpected end element </{}>", t.name.local);
                self.set_err(&msg);
                return false;
            }
        }
        if !self.translate(&mut t.name, true) {
            return false;
        }
        while let Some(Stk::Ns { .. }) = self.stk.last() {
            if let Some(Stk::Ns { local, url, ok }) = self.stk.pop() {
                if ok {
                    self.ns.insert(local, url);
                } else {
                    self.ns.remove(&local);
                }
            }
        }
        true
    }

    /// If the top element on the stack is autoclosing and `t` is not its end tag, the end
    /// tag to invent.
    fn auto_close_token(&self, t: &Token) -> Option<Token> {
        let Some(Stk::Start(top)) = self.stk.last() else {
            return None;
        };
        for s in &self.auto_close {
            if equal_fold(s, &top.local) {
                // This one should be auto closed if t does not close it.
                return match t {
                    Token::EndElement(et) if equal_fold(&et.name.local, &top.local) => None,
                    _ => Some(Token::EndElement(EndElement { name: top.clone() })),
                };
            }
        }
        None
    }

    /// `RawToken`: like [`Decoder::token`] but does not verify that start and end elements
    /// match and does not translate name space prefixes.
    ///
    /// # Errors
    /// As [`Decoder::token`], less the nesting checks.
    pub fn raw_token(&mut self) -> Result<Token, Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        if self.need_close {
            // The last element we read was self-closing and we returned just the start half.
            self.need_close = false;
            return Ok(Token::EndElement(EndElement {
                name: self.to_close.clone(),
            }));
        }
        let Some(b) = self.getc() else {
            return Err(self.fail());
        };
        if b != b'<' {
            // Text section.
            self.ungetc(b);
            return Ok(Token::CharData(self.text(None, false)?));
        }
        match self.must()? {
            b'/' => self.end_element(),
            b'?' => self.proc_inst(),
            b'!' => self.bang(),
            b => {
                // Must be an open element like <a href="foo">.
                self.ungetc(b);
                self.start_element()
            }
        }
    }

    /// `</name>`, after the `</`.
    fn end_element(&mut self) -> Result<Token, Error> {
        let Some(name) = self.nsname() else {
            return Err(self.name_err("expected element name after </"));
        };
        self.space();
        if self.must()? != b'>' {
            let msg = format!("invalid characters between </{} and >", name.local);
            return Err(self.set_err(&msg));
        }
        Ok(Token::EndElement(EndElement { name }))
    }

    /// `<?target inst?>`, after the `<?`.
    fn proc_inst(&mut self) -> Result<Token, Error> {
        let Some(target) = self.name() else {
            return Err(self.name_err("expected target name after <?"));
        };
        self.space();
        self.buf.clear();
        let mut b0 = 0u8;
        loop {
            let b = self.must()?;
            self.buf.push(b);
            if b0 == b'?' && b == b'>' {
                break;
            }
            b0 = b;
        }
        let mut data = self.buf.clone();
        data.truncate(data.len().saturating_sub(2)); // chop ?>

        if target == "xml" {
            let ver = proc_inst_param("version", &data);
            if !ver.is_empty() && ver != "1.0" {
                return Err(self.set_other(format!(
                    "xml: unsupported version {ver:?}; only version 1.0 is supported"
                )));
            }
            let enc = proc_inst_param("encoding", &data);
            if !enc.is_empty() && !enc.eq_ignore_ascii_case("utf-8") {
                return Err(self.set_other(format!(
                    "xml: encoding {enc:?} declared but Decoder.CharsetReader is nil"
                )));
            }
        }
        Ok(Token::ProcInst { target, inst: data })
    }

    /// `<!--comment-->`, `<![CDATA[text]]>` or a directive, after the `<!`.
    fn bang(&mut self) -> Result<Token, Error> {
        let mut b = self.must()?;
        match b {
            b'-' => {
                // Probably <!-- for a comment.
                if self.must()? != b'-' {
                    return Err(self.set_err("invalid sequence <!- not part of <!--"));
                }
                // Look for terminator.
                self.buf.clear();
                let (mut b0, mut b1) = (0u8, 0u8);
                loop {
                    b = self.must()?;
                    self.buf.push(b);
                    if b0 == b'-' && b1 == b'-' {
                        if b != b'>' {
                            return Err(
                                self.set_err("invalid sequence \"--\" not allowed in comments")
                            );
                        }
                        break;
                    }
                    b0 = b1;
                    b1 = b;
                }
                let mut data = self.buf.clone();
                data.truncate(data.len().saturating_sub(3)); // chop -->
                return Ok(Token::Comment(data));
            }
            b'[' => {
                // Probably <![CDATA[.
                for want in b"CDATA[" {
                    if self.must()? != *want {
                        return Err(self.set_err("invalid <![ sequence"));
                    }
                }
                // Have <![CDATA[. Read text until ]]>.
                return Ok(Token::CharData(self.text(None, true)?));
            }
            _ => {}
        }

        // Probably a directive: <!DOCTYPE ...>, <!ENTITY ...>, etc. Quoted angle brackets do
        // not count for nesting.
        self.buf.clear();
        self.buf.push(b);
        let mut inquote = 0u8;
        let mut depth = 0i64;
        loop {
            b = self.must()?;
            if inquote == 0 && b == b'>' && depth == 0 {
                break;
            }
            // Go's `HandleB` label: a byte to process, possibly re-entered with a new byte.
            loop {
                self.buf.push(b);
                if b == inquote {
                    inquote = 0;
                } else if inquote != 0 {
                    // in quotes, no special action
                } else if b == b'\'' || b == b'"' {
                    inquote = b;
                } else if b == b'>' {
                    depth -= 1;
                } else if b == b'<' {
                    // Look for <!-- to begin a comment.
                    let mut reenter = false;
                    for (i, want) in b"!--".iter().enumerate() {
                        b = self.must()?;
                        if b != *want {
                            for w in b"!--".iter().take(i) {
                                self.buf.push(*w);
                            }
                            depth += 1;
                            reenter = true;
                            break;
                        }
                    }
                    if reenter {
                        continue;
                    }
                    // Remove the < that was written above.
                    self.buf.pop();
                    // Look for terminator.
                    let (mut b0, mut b1) = (0u8, 0u8);
                    loop {
                        b = self.must()?;
                        if b0 == b'-' && b1 == b'-' && b == b'>' {
                            break;
                        }
                        b0 = b1;
                        b1 = b;
                    }
                    // Replace the comment with a space, so that markup parts that it separated
                    // do not get joined.
                    self.buf.push(b' ');
                }
                break;
            }
        }
        Ok(Token::Directive(self.buf.clone()))
    }

    /// `<name attr="v" ...>` or `<name/>`, after un-reading the first byte of the name.
    fn start_element(&mut self) -> Result<Token, Error> {
        let Some(name) = self.nsname() else {
            return Err(self.name_err("expected element name after <"));
        };
        let mut attr = Vec::new();
        let mut empty = false;
        loop {
            self.space();
            let b = self.must()?;
            if b == b'/' {
                empty = true;
                if self.must()? != b'>' {
                    return Err(self.set_err("expected /> in element"));
                }
                break;
            }
            if b == b'>' {
                break;
            }
            self.ungetc(b);

            let Some(aname) = self.nsname() else {
                return Err(self.name_err("expected attribute name in element"));
            };
            self.space();
            let b = self.must()?;
            let value = if b != b'=' {
                if self.strict {
                    return Err(self.set_err("attribute name without = in element"));
                }
                self.ungetc(b);
                aname.local.clone()
            } else {
                self.space();
                let data = self.attrval()?;
                String::from_utf8_lossy(&data).into_owned()
            };
            attr.push(Attr { name: aname, value });
        }
        if empty {
            self.need_close = true;
            self.to_close = name.clone();
        }
        Ok(Token::StartElement(StartElement { name, attr }))
    }

    fn attrval(&mut self) -> Result<Vec<u8>, Error> {
        let b = self.must()?;
        // Quoted attribute values.
        if b == b'"' || b == b'\'' {
            return self.text(Some(b), false);
        }
        // Unquoted attribute values are for non-strict parsers only.
        if self.strict {
            return Err(self.set_err("unquoted or missing attribute value in element"));
        }
        self.ungetc(b);
        self.buf.clear();
        loop {
            let b = self.must()?;
            if b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'-' {
                self.buf.push(b);
            } else {
                self.ungetc(b);
                break;
            }
        }
        Ok(self.buf.clone())
    }

    /// Skips spaces.
    fn space(&mut self) {
        while let Some(b) = self.getc() {
            if !matches!(b, b' ' | b'\r' | b'\n' | b'\t') {
                self.ungetc(b);
                return;
            }
        }
    }

    /// Reads a single byte and maintains the line number. At the end of the input it returns
    /// `None` and leaves the error in `self.err`.
    fn getc(&mut self) -> Option<u8> {
        if self.err.is_some() {
            return None;
        }
        let Some(&b) = self.data.get(self.pos) else {
            self.err = Some(Error::Eof);
            return None;
        };
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
        }
        Some(b)
    }

    /// Reads a single byte that must exist: the end of the input is a syntax error.
    fn must(&mut self) -> Result<u8, Error> {
        match self.getc() {
            Some(b) => Ok(b),
            None => {
                if self.err == Some(Error::Eof) {
                    self.err = Some(self.syntax_error("unexpected EOF"));
                }
                Err(self.fail())
            }
        }
    }

    /// Unreads the single byte `b` just read.
    fn ungetc(&mut self, b: u8) {
        if b == b'\n' {
            self.line = self.line.saturating_sub(1);
        }
        self.pos = self.pos.saturating_sub(1);
    }

    /// Reads a plain text section. With `quote` set it is a quoted string and ends at the
    /// matching quote; with `cdata` it is a `<![CDATA[` section and ends at `]]>`.
    fn text(&mut self, quote: Option<u8>, cdata: bool) -> Result<Vec<u8>, Error> {
        let (mut b0, mut b1) = (0u8, 0u8);
        let mut trunc = 0;
        self.buf.clear();
        loop {
            let Some(b) = self.getc() else {
                if cdata {
                    if self.err == Some(Error::Eof) {
                        self.err = Some(self.syntax_error("unexpected EOF in CDATA section"));
                    }
                    return Err(self.fail());
                }
                break;
            };

            // <![CDATA[ section ends with ]]>. It is an error for ]]> to appear in ordinary
            // text, but it is allowed in quoted strings.
            if quote.is_none() && b0 == b']' && b1 == b']' && b == b'>' {
                if cdata {
                    trunc = 2;
                    break;
                }
                return Err(self.set_err("unescaped ]]> not in CDATA section"));
            }

            // Stop reading text if we see a <.
            if b == b'<' && !cdata {
                if quote.is_some() {
                    return Err(self.set_err("unescaped < inside quoted string"));
                }
                self.ungetc(b'<');
                break;
            }
            if quote == Some(b) {
                break;
            }
            if b == b'&' && !cdata {
                self.entity_ref()?;
                b0 = 0;
                b1 = 0;
                continue;
            }

            // Unescaped \r and \r\n are rewritten to \n.
            if b == b'\r' {
                self.buf.push(b'\n');
            } else if b1 == b'\r' && b == b'\n' {
                // Skip \r\n: the \n is already written.
            } else {
                self.buf.push(b);
            }
            b0 = b1;
            b1 = b;
        }
        let end = self.buf.len().saturating_sub(trunc);
        self.buf.truncate(end);

        // Inspect each rune for being a disallowed character.
        let mut at = 0;
        while let Some(rest) = self.buf.get(at..).filter(|r| !r.is_empty()) {
            let Some((r, size)) = decode_rune(rest) else {
                return Err(self.set_err("invalid UTF-8"));
            };
            at += size;
            if !is_in_character_range(r as u32) {
                let msg = format!("illegal character code U+{:04X}", r as u32);
                return Err(self.set_err(&msg));
            }
        }
        Ok(self.buf.clone())
    }

    /// An entity or character reference in text, after the `&` (already read): appends its
    /// replacement to the buffer. Unknown ones are left alone in non-strict mode.
    fn entity_ref(&mut self) -> Result<(), Error> {
        // XML allows a document to define its own entities with <!ENTITY ...>; parsers must
        // recognise lt, gt, amp, apos and quot even if they are not declared.
        let before = self.buf.len();
        self.buf.push(b'&');
        let mut text: Option<Vec<u8>> = None;
        let mut b = self.must()?;
        if b == b'#' {
            self.buf.push(b);
            b = self.must()?;
            let mut base = 10;
            if b == b'x' {
                base = 16;
                self.buf.push(b);
                b = self.must()?;
            }
            let start = self.buf.len();
            while b.is_ascii_digit() || (base == 16 && b.is_ascii_hexdigit()) {
                self.buf.push(b);
                b = self.must()?;
            }
            if b != b';' {
                self.ungetc(b);
            } else {
                let n = self
                    .buf
                    .get(start..)
                    .and_then(|d| std::str::from_utf8(d).ok())
                    .and_then(|s| u64::from_str_radix(s, base).ok());
                self.buf.push(b';');
                if let Some(n) = n
                    && n <= 0x10FFFF
                {
                    // string(rune(n)): a surrogate becomes U+FFFD.
                    let c = u32::try_from(n)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or('\u{FFFD}');
                    text = Some(c.to_string().into_bytes());
                }
            }
        } else {
            self.ungetc(b);
            if !self.read_name() && self.err.is_some() {
                return Err(self.fail());
            }
            b = self.must()?;
            if b != b';' {
                self.ungetc(b);
            } else {
                let name = self.buf.get(before + 1..).unwrap_or_default().to_vec();
                self.buf.push(b';');
                if is_name(&name) {
                    let s = String::from_utf8_lossy(&name);
                    text = match &*s {
                        "lt" => Some(b"<".to_vec()),
                        "gt" => Some(b">".to_vec()),
                        "amp" => Some(b"&".to_vec()),
                        "apos" => Some(b"'".to_vec()),
                        "quot" => Some(b"\"".to_vec()),
                        _ => self
                            .entity
                            .as_ref()
                            .and_then(|m| m.get(&*s))
                            .map(|v| v.as_bytes().to_vec()),
                    };
                }
            }
        }

        if let Some(t) = text {
            self.buf.truncate(before);
            self.buf.extend_from_slice(&t);
            return Ok(());
        }
        if !self.strict {
            return Ok(());
        }
        let raw = self.buf.get(before..).unwrap_or_default();
        let mut ent = String::from_utf8_lossy(raw).into_owned();
        if raw.last() != Some(&b';') {
            ent.push_str(" (no semicolon)");
        }
        Err(self.set_err(&format!("invalid character entity {ent}")))
    }

    /// A name with a `:` stuck in the middle; the part before it is the name space identifier.
    fn nsname(&mut self) -> Option<Name> {
        let s = self.name()?;
        if s.matches(':').count() > 1 {
            return None;
        }
        if let Some((space, local)) = s.split_once(':')
            && !space.is_empty()
            && !local.is_empty()
        {
            return Some(Name {
                space: space.to_string(),
                local: local.to_string(),
            });
        }
        Some(Name {
            space: String::new(),
            local: s,
        })
    }

    /// A name: it does not set the error if the name is missing (unless the input ends), so
    /// that the caller can provide better context.
    fn name(&mut self) -> Option<String> {
        self.buf.clear();
        if !self.read_name() {
            return None;
        }
        if !is_name(&self.buf) {
            let msg = format!("invalid XML name: {}", String::from_utf8_lossy(&self.buf));
            self.set_err(&msg);
            return None;
        }
        Some(String::from_utf8_lossy(&self.buf).into_owned())
    }

    /// Reads a name and appends its bytes to the buffer. The name is delimited by any
    /// single-byte character not valid in names. All multi-byte characters are accepted; the
    /// caller must check their validity.
    fn read_name(&mut self) -> bool {
        let Ok(b) = self.must() else {
            return false;
        };
        if b < 0x80 && !is_name_byte(b) {
            self.ungetc(b);
            return false;
        }
        self.buf.push(b);
        loop {
            let Ok(b) = self.must() else {
                return false;
            };
            if b < 0x80 && !is_name_byte(b) {
                self.ungetc(b);
                return true;
            }
            self.buf.push(b);
        }
    }
}

/// `strings.EqualFold` as far as element names need it.
fn equal_fold(a: &str, b: &str) -> bool {
    let mut x = a.chars();
    let mut y = b.chars();
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(c), Some(d)) => {
                if c != d
                    && !c.to_lowercase().eq(d.to_lowercase())
                    && !c.to_uppercase().eq(d.to_uppercase())
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// `utf8.DecodeRune`: the rune and its size, or `None` for `(RuneError, 1)`.
pub(super) fn decode_rune(s: &[u8]) -> Option<(char, usize)> {
    let n = match *s.first()? {
        0..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let c = std::str::from_utf8(s.get(..n)?).ok()?.chars().next()?;
    Some((c, n))
}

/// Whether `r` is in the XML Character Range (the `Char` production, section 2.2 of the spec).
pub fn is_in_character_range(r: u32) -> bool {
    r == 0x09
        || r == 0x0A
        || r == 0x0D
        || (0x20..=0xD7FF).contains(&r)
        || (0xE000..=0xFFFD).contains(&r)
        || (0x10000..=0x10FFFF).contains(&r)
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'.' | b'-')
}

/// Whether `s` is a valid XML name.
fn is_name(s: &[u8]) -> bool {
    let Some((c, mut n)) = decode_rune(s) else {
        return false;
    };
    if !in_table(FIRST, c as u32) {
        return false;
    }
    while let Some(rest) = s.get(n..).filter(|r| !r.is_empty()) {
        let Some((c, size)) = decode_rune(rest) else {
            return false;
        };
        n += size;
        if !in_table(FIRST, c as u32) && !in_table(SECOND, c as u32) {
            return false;
        }
    }
    true
}

/// `unicode.Is` for a table of `Range16`: (lo, hi, stride).
fn in_table(table: &[(u16, u16, u16)], c: u32) -> bool {
    let Ok(c) = u16::try_from(c) else {
        return false;
    };
    table
        .iter()
        .any(|&(lo, hi, stride)| (lo..=hi).contains(&c) && (c - lo) % stride == 0)
}

/// `procInst`: the `param="..."` or `param='...'` value in `s`, or "" if not found.
fn proc_inst_param(param: &str, s: &[u8]) -> String {
    let param = format!("{param}=").into_bytes();
    let lenp = param.len();
    let mut i = 0;
    let mut sep = 0u8;
    while i < s.len() {
        let Some(sub) = s.get(i..) else {
            return String::new();
        };
        let Some(k) = sub.windows(lenp).position(|w| w == param.as_slice()) else {
            return String::new();
        };
        if lenp + k >= sub.len() {
            return String::new();
        }
        i += lenp + k + 1;
        let c = sub.get(lenp + k).copied().unwrap_or(0);
        if c == b'\'' || c == b'"' {
            sep = c;
            break;
        }
    }
    if sep == 0 {
        return String::new();
    }
    let rest = s.get(i..).unwrap_or_default();
    match rest.iter().position(|&b| b == sep) {
        Some(j) => String::from_utf8_lossy(rest.get(..j).unwrap_or_default()).into_owned(),
        None => String::new(),
    }
}

// These tables were generated from Appendix B of the XML spec (as in Go's xml.go). FIRST
// corresponds to (Letter | '_' | ':') and SECOND to NameChar.

const FIRST: &[(u16, u16, u16)] = &[
    (0x003A, 0x003A, 1),
    (0x0041, 0x005A, 1),
    (0x005F, 0x005F, 1),
    (0x0061, 0x007A, 1),
    (0x00C0, 0x00D6, 1),
    (0x00D8, 0x00F6, 1),
    (0x00F8, 0x00FF, 1),
    (0x0100, 0x0131, 1),
    (0x0134, 0x013E, 1),
    (0x0141, 0x0148, 1),
    (0x014A, 0x017E, 1),
    (0x0180, 0x01C3, 1),
    (0x01CD, 0x01F0, 1),
    (0x01F4, 0x01F5, 1),
    (0x01FA, 0x0217, 1),
    (0x0250, 0x02A8, 1),
    (0x02BB, 0x02C1, 1),
    (0x0386, 0x0386, 1),
    (0x0388, 0x038A, 1),
    (0x038C, 0x038C, 1),
    (0x038E, 0x03A1, 1),
    (0x03A3, 0x03CE, 1),
    (0x03D0, 0x03D6, 1),
    (0x03DA, 0x03E0, 2),
    (0x03E2, 0x03F3, 1),
    (0x0401, 0x040C, 1),
    (0x040E, 0x044F, 1),
    (0x0451, 0x045C, 1),
    (0x045E, 0x0481, 1),
    (0x0490, 0x04C4, 1),
    (0x04C7, 0x04C8, 1),
    (0x04CB, 0x04CC, 1),
    (0x04D0, 0x04EB, 1),
    (0x04EE, 0x04F5, 1),
    (0x04F8, 0x04F9, 1),
    (0x0531, 0x0556, 1),
    (0x0559, 0x0559, 1),
    (0x0561, 0x0586, 1),
    (0x05D0, 0x05EA, 1),
    (0x05F0, 0x05F2, 1),
    (0x0621, 0x063A, 1),
    (0x0641, 0x064A, 1),
    (0x0671, 0x06B7, 1),
    (0x06BA, 0x06BE, 1),
    (0x06C0, 0x06CE, 1),
    (0x06D0, 0x06D3, 1),
    (0x06D5, 0x06D5, 1),
    (0x06E5, 0x06E6, 1),
    (0x0905, 0x0939, 1),
    (0x093D, 0x093D, 1),
    (0x0958, 0x0961, 1),
    (0x0985, 0x098C, 1),
    (0x098F, 0x0990, 1),
    (0x0993, 0x09A8, 1),
    (0x09AA, 0x09B0, 1),
    (0x09B2, 0x09B2, 1),
    (0x09B6, 0x09B9, 1),
    (0x09DC, 0x09DD, 1),
    (0x09DF, 0x09E1, 1),
    (0x09F0, 0x09F1, 1),
    (0x0A05, 0x0A0A, 1),
    (0x0A0F, 0x0A10, 1),
    (0x0A13, 0x0A28, 1),
    (0x0A2A, 0x0A30, 1),
    (0x0A32, 0x0A33, 1),
    (0x0A35, 0x0A36, 1),
    (0x0A38, 0x0A39, 1),
    (0x0A59, 0x0A5C, 1),
    (0x0A5E, 0x0A5E, 1),
    (0x0A72, 0x0A74, 1),
    (0x0A85, 0x0A8B, 1),
    (0x0A8D, 0x0A8D, 1),
    (0x0A8F, 0x0A91, 1),
    (0x0A93, 0x0AA8, 1),
    (0x0AAA, 0x0AB0, 1),
    (0x0AB2, 0x0AB3, 1),
    (0x0AB5, 0x0AB9, 1),
    (0x0ABD, 0x0AE0, 0x23),
    (0x0B05, 0x0B0C, 1),
    (0x0B0F, 0x0B10, 1),
    (0x0B13, 0x0B28, 1),
    (0x0B2A, 0x0B30, 1),
    (0x0B32, 0x0B33, 1),
    (0x0B36, 0x0B39, 1),
    (0x0B3D, 0x0B3D, 1),
    (0x0B5C, 0x0B5D, 1),
    (0x0B5F, 0x0B61, 1),
    (0x0B85, 0x0B8A, 1),
    (0x0B8E, 0x0B90, 1),
    (0x0B92, 0x0B95, 1),
    (0x0B99, 0x0B9A, 1),
    (0x0B9C, 0x0B9C, 1),
    (0x0B9E, 0x0B9F, 1),
    (0x0BA3, 0x0BA4, 1),
    (0x0BA8, 0x0BAA, 1),
    (0x0BAE, 0x0BB5, 1),
    (0x0BB7, 0x0BB9, 1),
    (0x0C05, 0x0C0C, 1),
    (0x0C0E, 0x0C10, 1),
    (0x0C12, 0x0C28, 1),
    (0x0C2A, 0x0C33, 1),
    (0x0C35, 0x0C39, 1),
    (0x0C60, 0x0C61, 1),
    (0x0C85, 0x0C8C, 1),
    (0x0C8E, 0x0C90, 1),
    (0x0C92, 0x0CA8, 1),
    (0x0CAA, 0x0CB3, 1),
    (0x0CB5, 0x0CB9, 1),
    (0x0CDE, 0x0CDE, 1),
    (0x0CE0, 0x0CE1, 1),
    (0x0D05, 0x0D0C, 1),
    (0x0D0E, 0x0D10, 1),
    (0x0D12, 0x0D28, 1),
    (0x0D2A, 0x0D39, 1),
    (0x0D60, 0x0D61, 1),
    (0x0E01, 0x0E2E, 1),
    (0x0E30, 0x0E30, 1),
    (0x0E32, 0x0E33, 1),
    (0x0E40, 0x0E45, 1),
    (0x0E81, 0x0E82, 1),
    (0x0E84, 0x0E84, 1),
    (0x0E87, 0x0E88, 1),
    (0x0E8A, 0x0E8D, 3),
    (0x0E94, 0x0E97, 1),
    (0x0E99, 0x0E9F, 1),
    (0x0EA1, 0x0EA3, 1),
    (0x0EA5, 0x0EA7, 2),
    (0x0EAA, 0x0EAB, 1),
    (0x0EAD, 0x0EAE, 1),
    (0x0EB0, 0x0EB0, 1),
    (0x0EB2, 0x0EB3, 1),
    (0x0EBD, 0x0EBD, 1),
    (0x0EC0, 0x0EC4, 1),
    (0x0F40, 0x0F47, 1),
    (0x0F49, 0x0F69, 1),
    (0x10A0, 0x10C5, 1),
    (0x10D0, 0x10F6, 1),
    (0x1100, 0x1100, 1),
    (0x1102, 0x1103, 1),
    (0x1105, 0x1107, 1),
    (0x1109, 0x1109, 1),
    (0x110B, 0x110C, 1),
    (0x110E, 0x1112, 1),
    (0x113C, 0x1140, 2),
    (0x114C, 0x1150, 2),
    (0x1154, 0x1155, 1),
    (0x1159, 0x1159, 1),
    (0x115F, 0x1161, 1),
    (0x1163, 0x1169, 2),
    (0x116D, 0x116E, 1),
    (0x1172, 0x1173, 1),
    (0x1175, 0x119E, 41),
    (0x11A8, 0x11AB, 3),
    (0x11AE, 0x11AF, 1),
    (0x11B7, 0x11B8, 1),
    (0x11BA, 0x11BA, 1),
    (0x11BC, 0x11C2, 1),
    (0x11EB, 0x11F0, 5),
    (0x11F9, 0x11F9, 1),
    (0x1E00, 0x1E9B, 1),
    (0x1EA0, 0x1EF9, 1),
    (0x1F00, 0x1F15, 1),
    (0x1F18, 0x1F1D, 1),
    (0x1F20, 0x1F45, 1),
    (0x1F48, 0x1F4D, 1),
    (0x1F50, 0x1F57, 1),
    (0x1F59, 0x1F5B, 2),
    (0x1F5D, 0x1F5D, 1),
    (0x1F5F, 0x1F7D, 1),
    (0x1F80, 0x1FB4, 1),
    (0x1FB6, 0x1FBC, 1),
    (0x1FBE, 0x1FBE, 1),
    (0x1FC2, 0x1FC4, 1),
    (0x1FC6, 0x1FCC, 1),
    (0x1FD0, 0x1FD3, 1),
    (0x1FD6, 0x1FDB, 1),
    (0x1FE0, 0x1FEC, 1),
    (0x1FF2, 0x1FF4, 1),
    (0x1FF6, 0x1FFC, 1),
    (0x2126, 0x2126, 1),
    (0x212A, 0x212B, 1),
    (0x212E, 0x212E, 1),
    (0x2180, 0x2182, 1),
    (0x3007, 0x3007, 1),
    (0x3021, 0x3029, 1),
    (0x3041, 0x3094, 1),
    (0x30A1, 0x30FA, 1),
    (0x3105, 0x312C, 1),
    (0x4E00, 0x9FA5, 1),
    (0xAC00, 0xD7A3, 1),
];

const SECOND: &[(u16, u16, u16)] = &[
    (0x002D, 0x002E, 1),
    (0x0030, 0x0039, 1),
    (0x00B7, 0x00B7, 1),
    (0x02D0, 0x02D1, 1),
    (0x0300, 0x0345, 1),
    (0x0360, 0x0361, 1),
    (0x0387, 0x0387, 1),
    (0x0483, 0x0486, 1),
    (0x0591, 0x05A1, 1),
    (0x05A3, 0x05B9, 1),
    (0x05BB, 0x05BD, 1),
    (0x05BF, 0x05BF, 1),
    (0x05C1, 0x05C2, 1),
    (0x05C4, 0x0640, 124),
    (0x064B, 0x0652, 1),
    (0x0660, 0x0669, 1),
    (0x0670, 0x0670, 1),
    (0x06D6, 0x06DC, 1),
    (0x06DD, 0x06DF, 1),
    (0x06E0, 0x06E4, 1),
    (0x06E7, 0x06E8, 1),
    (0x06EA, 0x06ED, 1),
    (0x06F0, 0x06F9, 1),
    (0x0901, 0x0903, 1),
    (0x093C, 0x093C, 1),
    (0x093E, 0x094C, 1),
    (0x094D, 0x094D, 1),
    (0x0951, 0x0954, 1),
    (0x0962, 0x0963, 1),
    (0x0966, 0x096F, 1),
    (0x0981, 0x0983, 1),
    (0x09BC, 0x09BC, 1),
    (0x09BE, 0x09BF, 1),
    (0x09C0, 0x09C4, 1),
    (0x09C7, 0x09C8, 1),
    (0x09CB, 0x09CD, 1),
    (0x09D7, 0x09D7, 1),
    (0x09E2, 0x09E3, 1),
    (0x09E6, 0x09EF, 1),
    (0x0A02, 0x0A3C, 58),
    (0x0A3E, 0x0A3F, 1),
    (0x0A40, 0x0A42, 1),
    (0x0A47, 0x0A48, 1),
    (0x0A4B, 0x0A4D, 1),
    (0x0A66, 0x0A6F, 1),
    (0x0A70, 0x0A71, 1),
    (0x0A81, 0x0A83, 1),
    (0x0ABC, 0x0ABC, 1),
    (0x0ABE, 0x0AC5, 1),
    (0x0AC7, 0x0AC9, 1),
    (0x0ACB, 0x0ACD, 1),
    (0x0AE6, 0x0AEF, 1),
    (0x0B01, 0x0B03, 1),
    (0x0B3C, 0x0B3C, 1),
    (0x0B3E, 0x0B43, 1),
    (0x0B47, 0x0B48, 1),
    (0x0B4B, 0x0B4D, 1),
    (0x0B56, 0x0B57, 1),
    (0x0B66, 0x0B6F, 1),
    (0x0B82, 0x0B83, 1),
    (0x0BBE, 0x0BC2, 1),
    (0x0BC6, 0x0BC8, 1),
    (0x0BCA, 0x0BCD, 1),
    (0x0BD7, 0x0BD7, 1),
    (0x0BE7, 0x0BEF, 1),
    (0x0C01, 0x0C03, 1),
    (0x0C3E, 0x0C44, 1),
    (0x0C46, 0x0C48, 1),
    (0x0C4A, 0x0C4D, 1),
    (0x0C55, 0x0C56, 1),
    (0x0C66, 0x0C6F, 1),
    (0x0C82, 0x0C83, 1),
    (0x0CBE, 0x0CC4, 1),
    (0x0CC6, 0x0CC8, 1),
    (0x0CCA, 0x0CCD, 1),
    (0x0CD5, 0x0CD6, 1),
    (0x0CE6, 0x0CEF, 1),
    (0x0D02, 0x0D03, 1),
    (0x0D3E, 0x0D43, 1),
    (0x0D46, 0x0D48, 1),
    (0x0D4A, 0x0D4D, 1),
    (0x0D57, 0x0D57, 1),
    (0x0D66, 0x0D6F, 1),
    (0x0E31, 0x0E31, 1),
    (0x0E34, 0x0E3A, 1),
    (0x0E46, 0x0E46, 1),
    (0x0E47, 0x0E4E, 1),
    (0x0E50, 0x0E59, 1),
    (0x0EB1, 0x0EB1, 1),
    (0x0EB4, 0x0EB9, 1),
    (0x0EBB, 0x0EBC, 1),
    (0x0EC6, 0x0EC6, 1),
    (0x0EC8, 0x0ECD, 1),
    (0x0ED0, 0x0ED9, 1),
    (0x0F18, 0x0F19, 1),
    (0x0F20, 0x0F29, 1),
    (0x0F35, 0x0F39, 2),
    (0x0F3E, 0x0F3F, 1),
    (0x0F71, 0x0F84, 1),
    (0x0F86, 0x0F8B, 1),
    (0x0F90, 0x0F95, 1),
    (0x0F97, 0x0F97, 1),
    (0x0F99, 0x0FAD, 1),
    (0x0FB1, 0x0FB7, 1),
    (0x0FB9, 0x0FB9, 1),
    (0x20D0, 0x20DC, 1),
    (0x20E1, 0x3005, 3876),
    (0x302A, 0x302F, 1),
    (0x3031, 0x3035, 1),
    (0x3099, 0x309A, 1),
    (0x309D, 0x309E, 1),
    (0x30FC, 0x30FE, 1),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(data: &[u8]) -> Decoder<'_> {
        Decoder::new(data, &Limits::default()).expect("decoder")
    }

    fn n(space: &str, local: &str) -> Name {
        Name {
            space: space.to_string(),
            local: local.to_string(),
        }
    }
    fn start(space: &str, local: &str, attrs: &[(&str, &str, &str)]) -> Token {
        Token::StartElement(StartElement {
            name: n(space, local),
            attr: attrs
                .iter()
                .map(|(s, l, v)| Attr {
                    name: n(s, l),
                    value: v.to_string(),
                })
                .collect(),
        })
    }
    fn end(space: &str, local: &str) -> Token {
        Token::EndElement(EndElement {
            name: n(space, local),
        })
    }
    fn cd(s: &str) -> Token {
        Token::CharData(s.as_bytes().to_vec())
    }
    fn pi(target: &str, inst: &str) -> Token {
        Token::ProcInst {
            target: target.to_string(),
            inst: inst.as_bytes().to_vec(),
        }
    }
    fn dir(s: &str) -> Token {
        Token::Directive(s.as_bytes().to_vec())
    }
    fn com(s: &str) -> Token {
        Token::Comment(s.as_bytes().to_vec())
    }

    /// Reads tokens until the first error.
    fn drain(d: &mut Decoder) -> Error {
        loop {
            if let Err(e) = d.token() {
                return e;
            }
        }
    }

    const TEST_INPUT: &str = "\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\"\n  \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd\">\n<body xmlns:foo=\"ns1\" xmlns=\"ns2\" xmlns:tag=\"ns3\" \r\n\t  >\n  <hello lang=\"en\">World &lt;&gt;&apos;&quot; &#x767d;&#40300;翔</hello>\n  <query>&何; &is-it;</query>\n  <goodbye />\n  <goodbye />\n  <outer foo:attr=\"value\" xmlns:tag=\"ns4\">\n    <inner/>\n  </outer>\n  <tag:name>\n    <![CDATA[Some text here.]]>\n  </tag:name>\n</body><!-- missing final newline -->";

    fn test_entity() -> HashMap<String, String> {
        HashMap::from([
            ("何".to_string(), "What".to_string()),
            ("is-it".to_string(), "is it?".to_string()),
        ])
    }

    const DOCTYPE: &str = "DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\"\n  \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd\"";

    #[test]
    fn raw_token() {
        let mut d = dec(TEST_INPUT.as_bytes());
        d.entity = Some(test_entity());
        let want = vec![
            cd("\n"),
            pi("xml", "version=\"1.0\" encoding=\"UTF-8\""),
            cd("\n"),
            dir(DOCTYPE),
            cd("\n"),
            start(
                "",
                "body",
                &[
                    ("xmlns", "foo", "ns1"),
                    ("", "xmlns", "ns2"),
                    ("xmlns", "tag", "ns3"),
                ],
            ),
            cd("\n  "),
            start("", "hello", &[("", "lang", "en")]),
            cd("World <>'\" 白鵬翔"),
            end("", "hello"),
            cd("\n  "),
            start("", "query", &[]),
            cd("What is it?"),
            end("", "query"),
            cd("\n  "),
            start("", "goodbye", &[]),
            end("", "goodbye"),
            cd("\n  "),
            start("", "goodbye", &[]),
            end("", "goodbye"),
            cd("\n  "),
            start(
                "",
                "outer",
                &[("foo", "attr", "value"), ("xmlns", "tag", "ns4")],
            ),
            cd("\n    "),
            start("", "inner", &[]),
            end("", "inner"),
            cd("\n  "),
            end("", "outer"),
            cd("\n  "),
            start("tag", "name", &[]),
            cd("\n    "),
            cd("Some text here."),
            cd("\n  "),
            end("tag", "name"),
            cd("\n"),
            end("", "body"),
            com(" missing final newline "),
        ];
        let mut last_end = 0;
        for (i, want) in want.iter().enumerate() {
            let start = d.input_offset();
            let have = d.raw_token().expect("token");
            assert_eq!(&have, want, "token {i}");
            // The offsets advance, and a raw tag token spans <...>.
            let end = d.input_offset();
            assert!(start >= last_end && end <= TEST_INPUT.len(), "token {i}");
            assert!(
                start < end || (start == end && end == last_end),
                "token {i}"
            );
            if let Some(text) = TEST_INPUT.get(start..end)
                && text.contains(['<', '>'])
            {
                assert!(text.starts_with('<') && text.ends_with('>'), "token {i}");
            }
            last_end = end;
        }
    }

    #[test]
    fn token_cooked() {
        let mut d = dec(TEST_INPUT.as_bytes());
        d.entity = Some(test_entity());
        let want = vec![
            cd("\n"),
            pi("xml", "version=\"1.0\" encoding=\"UTF-8\""),
            cd("\n"),
            dir(DOCTYPE),
            cd("\n"),
            start(
                "ns2",
                "body",
                &[
                    ("xmlns", "foo", "ns1"),
                    ("", "xmlns", "ns2"),
                    ("xmlns", "tag", "ns3"),
                ],
            ),
            cd("\n  "),
            start("ns2", "hello", &[("", "lang", "en")]),
            cd("World <>'\" 白鵬翔"),
            end("ns2", "hello"),
            cd("\n  "),
            start("ns2", "query", &[]),
            cd("What is it?"),
            end("ns2", "query"),
            cd("\n  "),
            start("ns2", "goodbye", &[]),
            end("ns2", "goodbye"),
            cd("\n  "),
            start("ns2", "goodbye", &[]),
            end("ns2", "goodbye"),
            cd("\n  "),
            start(
                "ns2",
                "outer",
                &[("ns1", "attr", "value"), ("xmlns", "tag", "ns4")],
            ),
            cd("\n    "),
            start("ns2", "inner", &[]),
            end("ns2", "inner"),
            cd("\n  "),
            end("ns2", "outer"),
            cd("\n  "),
            start("ns3", "name", &[]),
            cd("\n    "),
            cd("Some text here."),
            cd("\n  "),
            end("ns3", "name"),
            cd("\n"),
            end("ns2", "body"),
            com(" missing final newline "),
        ];
        for (i, want) in want.iter().enumerate() {
            assert_eq!(&d.token().expect("token"), want, "token {i}");
        }
        assert_eq!(d.token(), Err(Error::Eof));
    }

    #[test]
    fn many_prefixed_attrs_on_long_ns_url_hit_budget() {
        let mut x = String::from("<x xmlns:a=\"");
        x.push_str(&"A".repeat(1000));
        x.push('"');
        for i in 0..100 {
            x.push_str(&format!(" a:b{i}=\"\""));
        }
        x.push_str("/>");
        let lim = Limits {
            max_alloc: 50_000,
            ..Limits::default()
        };
        let mut d = Decoder::new(x.as_bytes(), &lim).expect("decoder");
        assert!(matches!(d.token(), Err(Error::Other(_))));
        let mut d = dec(x.as_bytes());
        assert!(matches!(d.token(), Ok(Token::StartElement(_))));
    }

    #[test]
    fn non_strict_raw_token() {
        let input = "\n<tag>non&entity</tag>\n<tag>&unknown;entity</tag>\n<tag>&#123</tag>\n<tag>&#zzz;</tag>\n<tag>&なまえ3;</tag>\n<tag>&lt-gt;</tag>\n<tag>&;</tag>\n<tag>&0a;</tag>\n";
        let texts = [
            "non&entity",
            "&unknown;entity",
            "&#123",
            "&#zzz;",
            "&なまえ3;",
            "&lt-gt;",
            "&;",
            "&0a;",
        ];
        let mut want = vec![cd("\n")];
        for t in texts {
            want.extend([start("", "tag", &[]), cd(t), end("", "tag"), cd("\n")]);
        }
        let mut d = dec(input.as_bytes());
        d.strict = false;
        for (i, want) in want.iter().enumerate() {
            assert_eq!(&d.raw_token().expect("token"), want, "token {i}");
        }
    }

    #[test]
    fn alt_encoding_without_converter() {
        let input = "\n<?xml version=\"1.0\" encoding=\"x-testing-uppercase\"?>\n<TAG>VALUE</TAG>";
        let mut d = dec(input.as_bytes());
        assert_eq!(d.raw_token(), Ok(cd("\n")));
        let err = d.raw_token().expect_err("encoding");
        assert!(err.to_string().contains("x-testing-uppercase"), "{err}");
    }

    #[test]
    fn nested_directives() {
        let input = "\n<!DOCTYPE [<!ENTITY rdf \"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">]>\n<!DOCTYPE [<!ENTITY xlt \">\">]>\n<!DOCTYPE [<!ENTITY xlt \"<\">]>\n<!DOCTYPE [<!ENTITY xlt '>'>]>\n<!DOCTYPE [<!ENTITY xlt '<'>]>\n<!DOCTYPE [<!ENTITY xlt '\">'>]>\n<!DOCTYPE [<!ENTITY xlt \"'<\">]>\n";
        let dirs = [
            "DOCTYPE [<!ENTITY rdf \"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">]",
            "DOCTYPE [<!ENTITY xlt \">\">]",
            "DOCTYPE [<!ENTITY xlt \"<\">]",
            "DOCTYPE [<!ENTITY xlt '>'>]",
            "DOCTYPE [<!ENTITY xlt '<'>]",
            "DOCTYPE [<!ENTITY xlt '\">'>]",
            "DOCTYPE [<!ENTITY xlt \"'<\">]",
        ];
        let mut d = dec(input.as_bytes());
        assert_eq!(d.token(), Ok(cd("\n")));
        for want in dirs {
            assert_eq!(d.token(), Ok(dir(want)));
            assert_eq!(d.token(), Ok(cd("\n")));
        }
    }

    #[test]
    fn syntax_errors() {
        let inputs = [
            // unexpected EOF cases
            "<",
            "<t",
            "<t ",
            "<t/",
            "<!",
            "<!-",
            "<!--",
            "<!--c-",
            "<!--c--",
            "<!d",
            "<t></",
            "<t></t",
            "<?",
            "<?p",
            "<t a",
            "<t a=",
            "<t a='",
            "<t a=''",
            "<t/><![",
            "<t/><![C",
            "<t/><![CDATA[d",
            "<t/><![CDATA[d]",
            "<t/><![CDATA[d]]",
            // other syntax errors
            "<>",
            "<t/a",
            "<0 />",
            "<?0 >",
            "</0>",
            "<t 0=''>",
            "<t a='&'>",
            "<t a='<'>",
            "<t>&nbspc;</t>",
            "<t a>",
            "<t a=>",
            "<t a=v>",
            "<t></e>",
            "<t></>",
            "<t></t!",
            "<t>cdata]]></t>",
        ];
        for input in inputs {
            let mut d = dec(input.as_bytes());
            let err = drain(&mut d);
            assert!(matches!(err, Error::Syntax { .. }), "{input:?}: {err:?}");
        }
    }

    #[test]
    fn syntax_error_line_number() {
        let mut d = dec(b"<P>Foo<P>\n\n<P>Bar</>\n");
        match drain(&mut d) {
            Error::Syntax { line, .. } => assert_eq!(line, 3),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn issue_68387() {
        let mut d = dec(b"<item b=']]>'/>");
        assert_eq!(d.raw_token(), Ok(start("", "item", &[("", "b", "]]>")])));
        assert_eq!(d.raw_token(), Ok(end("", "item")));
        assert_eq!(d.raw_token(), Err(Error::Eof));
    }

    #[test]
    fn unquoted_attrs() {
        let mut d = dec(b"<tag attr=azAZ09:-_\t>");
        d.strict = false;
        assert_eq!(
            d.token(),
            Ok(start("", "tag", &[("", "attr", "azAZ09:-_")]))
        );
    }

    #[test]
    fn valueless_attrs() {
        for (input, tag, attr) in [
            ("<p nowrap>", "p", "nowrap"),
            ("<p nowrap >", "p", "nowrap"),
            ("<input checked/>", "input", "checked"),
            ("<input checked />", "input", "checked"),
        ] {
            let mut d = dec(input.as_bytes());
            d.strict = false;
            assert_eq!(
                d.token(),
                Ok(start("", tag, &[("", attr, attr)])),
                "{input}"
            );
        }
    }

    #[test]
    fn trailing_tokens() {
        let mut d = dec(b"<FOO></FOO>  ");
        assert_eq!(drain(&mut d), Error::Eof);
        let mut d = dec(b"<FOO></FOO>  ");
        let err = loop {
            if let Err(e) = d.raw_token() {
                break e;
            }
        };
        assert_eq!(err, Error::Eof);
    }

    #[test]
    fn entity_inside_cdata() {
        let mut d = dec(b"<test><![CDATA[ &val=foo ]]></test>");
        assert_eq!(drain(&mut d), Error::Eof);
    }

    #[test]
    fn disallowed_characters() {
        let cases: [(&[u8], &str); 9] = [
            (b"\x12<doc/>", "illegal character code U+0012"),
            (
                b"<?xml version=\"1.0\"?>\x0b<doc/>",
                "illegal character code U+000B",
            ),
            (b"\xef\xbf\xbe<doc/>", "illegal character code U+FFFE"),
            (
                b"<?xml version=\"1.0\"?><doc>\r\n<hiya/>\x07<toots/></doc>",
                "illegal character code U+0007",
            ),
            (
                b"<?xml version=\"1.0\"?><doc \x12='value'>what's up</doc>",
                "expected attribute name in element",
            ),
            (
                b"<doc>&abc\x01;</doc>",
                "invalid character entity &abc (no semicolon)",
            ),
            (
                b"<doc>&\x01;</doc>",
                "invalid character entity & (no semicolon)",
            ),
            (
                b"<doc>&\xef\xbf\xbe;</doc>",
                "invalid character entity &\u{FFFE};",
            ),
            (b"<doc>&hello;</doc>", "invalid character entity &hello;"),
        ];
        for (input, want) in cases {
            let mut d = dec(input);
            match drain(&mut d) {
                Error::Syntax { msg, .. } => assert_eq!(msg, want),
                e => panic!("{input:?}: {e:?}"),
            }
        }
    }

    #[test]
    fn character_range() {
        for r in [0x110000, 0xD800, 0xDFFF, u32::MAX] {
            assert!(!is_in_character_range(r), "{r:#x}");
        }
        for r in [
            0x9, 0xA, 0xD, 0x20, 0xD7FF, 0xE000, 0xFFFD, 0x10000, 0x10FFFF,
        ] {
            assert!(is_in_character_range(r), "{r:#x}");
        }
    }

    #[test]
    fn proc_inst_params() {
        let tests = [
            (r#"version="1.0" encoding="utf-8""#, "1.0", "utf-8"),
            (r#"version="1.0" encoding='utf-8'"#, "1.0", "utf-8"),
            (r#"version="1.0" encoding='utf-8' "#, "1.0", "utf-8"),
            (r#"version="1.0" encoding=utf-8"#, "1.0", ""),
            (r#"encoding="FOO" "#, "", "FOO"),
            (
                r#"version=2.0 version="1.0" encoding=utf-7 encoding='utf-8'"#,
                "1.0",
                "utf-8",
            ),
            ("version= encoding=", "", ""),
            (r#"encoding="version=1.0""#, "", "version=1.0"),
            ("", "", ""),
            (r#"encoding="version='1.0'""#, "1.0", "version='1.0'"),
            (r#"version="encoding='utf-8'""#, "encoding='utf-8'", "utf-8"),
        ];
        for (input, ver, enc) in tests {
            assert_eq!(proc_inst_param("version", input.as_bytes()), ver, "{input}");
            assert_eq!(
                proc_inst_param("encoding", input.as_bytes()),
                enc,
                "{input}"
            );
        }
    }

    #[test]
    fn directives_with_comments() {
        let input = "\n<!DOCTYPE [<!-- a comment --><!ENTITY rdf \"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">]>\n<!DOCTYPE [<!ENTITY go \"Golang\"><!-- a comment-->]>\n<!DOCTYPE <!-> <!> <!----> <!-->--> <!--->--> [<!ENTITY go \"Golang\"><!-- a comment-->]>\n";
        let want = [
            cd("\n"),
            dir("DOCTYPE [ <!ENTITY rdf \"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">]"),
            cd("\n"),
            dir("DOCTYPE [<!ENTITY go \"Golang\"> ]"),
            cd("\n"),
            dir("DOCTYPE <!-> <!>       [<!ENTITY go \"Golang\"> ]"),
            cd("\n"),
        ];
        let mut d = dec(input.as_bytes());
        for (i, want) in want.iter().enumerate() {
            assert_eq!(&d.token().expect("token"), want, "token {i}");
        }
    }

    #[test]
    fn parse_errors() {
        let header = |s: &str| format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{s}");
        let tests: Vec<(String, &str)> = vec![
            (header("</foo>"), "unexpected end element </foo>"),
            (
                header("<x:foo></y:foo>"),
                "element <foo> in space x closed by </foo> in space y",
            ),
            (header("<? not ok ?>"), "expected target name after <?"),
            (
                header("<!- not ok -->"),
                "invalid sequence <!- not part of <!--",
            ),
            (
                header("<!-? not ok -->"),
                "invalid sequence <!- not part of <!--",
            ),
            (header("<![not ok]>"), "invalid <![ sequence"),
            (
                header("<zzz:foo xmlns:zzz=\"http://example.com\"><bar>baz</bar></foo>"),
                "element <foo> in space zzz closed by </foo> in space \"\"",
            ),
            (
                "<?xml version=\"1.1\" encoding=\"UTF-8\"?>".to_string(),
                "unsupported version \"1.1\"; only version 1.0 is supported",
            ),
            (header("<?ok?>"), ""),
            (header("<?ok version=\"ok\"?>"), ""),
        ];
        for (src, want) in tests {
            let mut d = dec(src.as_bytes());
            let err = drain(&mut d);
            if want.is_empty() {
                assert_eq!(err, Error::Eof, "{src}");
            } else {
                assert!(err.to_string().contains(want), "{src}: {err}");
            }
        }
        // A lone 0xF1 byte is invalid UTF-8.
        let mut d = dec(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\xf1");
        match drain(&mut d) {
            Error::Syntax { msg, .. } => assert_eq!(msg, "invalid UTF-8"),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn html_auto_close() {
        let input = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<br>\n<br/><br/>\n<br><br>\n<br></br>\n<BR>\n<BR/><BR/>\n<Br></Br>\n<BR><span id=\"test\">abc</span><br/><br/>";
        let mut want = vec![pi("xml", "version=\"1.0\" encoding=\"UTF-8\""), cd("\n")];
        let br = |name: &str| [start("", name, &[]), end("", name)];
        want.extend(br("br"));
        want.push(cd("\n"));
        want.extend(br("br"));
        want.extend(br("br"));
        want.push(cd("\n"));
        want.extend(br("br"));
        want.extend(br("br"));
        want.push(cd("\n"));
        want.extend(br("br"));
        want.push(cd("\n"));
        want.extend(br("BR"));
        want.push(cd("\n"));
        want.extend(br("BR"));
        want.extend(br("BR"));
        want.push(cd("\n"));
        want.extend(br("Br"));
        want.push(cd("\n"));
        want.extend(br("BR"));
        want.extend([
            start("", "span", &[("", "id", "test")]),
            cd("abc"),
            end("", "span"),
        ]);
        want.extend(br("br"));
        want.extend(br("br"));

        let mut d = dec(input.as_bytes());
        d.strict = false;
        d.auto_close = HTML_AUTO_CLOSE.iter().map(|s| s.to_string()).collect();
        let mut have = Vec::new();
        loop {
            match d.token() {
                Ok(t) => have.push(t),
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(have, want);
    }

    #[test]
    fn default_space_and_xml_prefix() {
        let mut d = dec(b"<a xml:lang=\"en\" b=\"1\"/>");
        d.default_space = "urn:d".to_string();
        assert_eq!(
            d.token(),
            Ok(start(
                "urn:d",
                "a",
                &[(XML_URL, "lang", "en"), ("", "b", "1")]
            ))
        );
    }

    #[test]
    fn undeclared_prefix_is_kept() {
        let mut d = dec(b"<p:a/>");
        assert_eq!(d.token(), Ok(start("p", "a", &[])));
        assert_eq!(d.token(), Ok(end("p", "a")));
    }

    #[test]
    fn character_references() {
        let mut d = dec(b"<a>&#65;&#x42;&#xD800;&#1114112;</a>");
        assert_eq!(d.token(), Ok(start("", "a", &[])));
        // U+D800 becomes U+FFFD; 0x110000 is not a rune, so the reference is invalid.
        let err = d.token().expect_err("invalid reference");
        assert!(
            err.to_string()
                .contains("invalid character entity &#1114112;"),
            "{err}"
        );
    }

    #[test]
    fn crlf_is_normalised() {
        let mut d = dec(b"a\r\nb\rc");
        assert_eq!(d.token(), Ok(cd("a\nb\nc")));
    }
}
