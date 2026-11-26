//! The one error type of the crate. One variant per Go error that callers tell apart with
//! `errors.Is` or `errors.As`; the `is_*` methods mirror `errors.Is`. [`Error::class`] is the
//! short stable token the parity oracle writes to `.err` files (docs/parity.md has the mapping
//! to the Go classes).
//!
//! Go sources: `internal/limits/limits.go` (`LimitError`), `limits.go` (`ErrPassword`),
//! `notimplemented.go`, `extract.go` (`ErrNotSingleRaster`, `ErrUnsupportedImageCodec`,
//! `RasterRefusal`), `embed.go` (`ErrMirroredOrientation`), `jbig2.go`
//! (`ErrUnsupportedJBIG2Feature`), `stamp.go` (`ErrUnstampableRune`), `provenance.go`
//! (`errCorruptProvenance`) and `internal/pdfdoc` (`ErrUnsupportedCodec`, `ErrMalformed`,
//! `ErrUnbalancedContent`) and `image/png`, the tiff reader and `image.Decode` (`Image`).

use crate::notimplemented::NotImplemented;
use std::collections::TryReserveError;
use std::fmt;

/// `Result` with the crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Everything a byblos call can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `LimitError`: an input breached the cap `name` (the Go limit name, for example
    /// `"image-pixels"`). `got` is what the input needs; for a decode that stopped at its cap it
    /// is `max + 1`.
    Limit {
        name: &'static str,
        got: i64,
        max: i64,
    },
    /// The per-call `max_alloc` ledger refused `got` bytes with `max` left. Replaces Go's
    /// `ErrMemoryLimit` (docs/go-differences.md). Also what a nested pdf-rs allocation refusal
    /// becomes.
    Alloc { got: u64, max: u64 },
    /// The allocator refused a size that is within the limits.
    OutOfMemory,
    /// The cancel flag was set (Go's `ctx.Err()`).
    Cancelled,
    /// `ErrNotImplemented` with its `NotImplemented` detail.
    NotImplemented(NotImplemented),
    /// `RasterRefusal`: `extract_page_raster` refused page `page` (1-based) for `reason`, which
    /// is fine-grained (`"vector-paint"`, `"unsupported-codec-jbig2"`), while `class` is the
    /// coarse divert class recorded in the page provenance. `source` is the wrapped chain
    /// (`NotSingleRaster`, `UnsupportedImageCodec`, a `NotImplemented`, a limit); a hand-built
    /// refusal has none.
    Refused {
        page: usize,
        reason: String,
        class: String,
        source: Option<Box<Error>>,
    },
    /// `ErrNotSingleRaster`: the page is not a single raster.
    NotSingleRaster,
    /// `ErrUnsupportedImageCodec`: the page raster uses a codec byblos cannot decode.
    UnsupportedImageCodec,
    /// `ErrMirroredOrientation`: the EXIF orientation is mirrored.
    MirroredOrientation,
    /// `ErrUnsupportedJBIG2Feature`, with the detail Go wraps around it.
    UnsupportedJbig2Feature(String),
    /// `ErrUnstampableRune`: the rune is outside the glyphless font's coverage.
    UnstampableRune(char),
    /// `ErrPassword`: the document needs a non-empty user password.
    Password,
    /// `errCorruptProvenance`: the provenance value in the Info dictionary is corrupt.
    CorruptProvenance,
    /// `pdfdoc.ErrUnsupportedCodec`: an image codec that cannot be rendered.
    UnsupportedCodec,
    /// `pdfdoc.ErrMalformed`: a document pdf-go could not parse without panicking, or any other
    /// malformed input without its own variant. The text is a diagnostic.
    Malformed(String),
    /// `pdfdoc.ErrUnbalancedContent`: unbalanced `q`/`Q` in a content stream.
    UnbalancedContent,
    /// A decode error of `image::decode`: Go's `png.FormatError` and `png.UnsupportedError`, the
    /// tiff reader's `FormatError` and `UnsupportedError`, `image.ErrFormat` and a raw `io.EOF`,
    /// with Go's text. They are untyped as far as the Go oracle's class goes (`error`).
    Image(String),
    /// An error from pdf-rs (pdf-go's errors). Its allocation refusals and cancellation arrive
    /// as [`Error::Alloc`] and [`Error::Cancelled`] through `From`.
    Pdf(pdf::Error),
    /// A Go error built with `fmt.Errorf("...%w...", err)`: `msg` is the whole text it prints
    /// and `sources` are the errors its `%w` verbs wrap (several for `"%w ... (%w)"`). The `is_*`
    /// methods look through every source, [`Error::as_not_implemented`] finds a
    /// [`NotImplemented`] among them, and [`Error::class`] is that of the first.
    Wrapped { msg: String, sources: Vec<Error> },
}

impl Error {
    /// `errors.Is(err, ErrLimit)`: a `LimitError`, or a pdf-rs limit error.
    pub fn is_limit(&self) -> bool {
        match self {
            Error::Wrapped { sources, .. } => sources.iter().any(Error::is_limit),
            Error::Limit { .. } => true,
            Error::Refused {
                source: Some(s), ..
            } => s.is_limit(),
            Error::Pdf(e) => e.is_limit_exceeded() || e.is_decode_limit_exceeded(),
            _ => false,
        }
    }

    /// `errors.Is(err, ErrNotImplemented)`.
    pub fn is_not_implemented(&self) -> bool {
        match self {
            Error::Wrapped { sources, .. } => sources.iter().any(Error::is_not_implemented),
            Error::NotImplemented(_) => true,
            Error::Refused {
                source: Some(s), ..
            } => s.is_not_implemented(),
            _ => false,
        }
    }

    /// `errors.Is(err, ErrNotSingleRaster)`.
    pub fn is_not_single_raster(&self) -> bool {
        match self {
            Error::Wrapped { sources, .. } => sources.iter().any(Error::is_not_single_raster),
            Error::NotSingleRaster => true,
            Error::Refused {
                source: Some(s), ..
            } => s.is_not_single_raster(),
            _ => false,
        }
    }

    /// `errors.Is(err, ErrUnsupportedImageCodec)`.
    pub fn is_unsupported_image_codec(&self) -> bool {
        match self {
            Error::Wrapped { sources, .. } => sources.iter().any(Error::is_unsupported_image_codec),
            Error::UnsupportedImageCodec => true,
            Error::Refused {
                source: Some(s), ..
            } => s.is_unsupported_image_codec(),
            _ => false,
        }
    }

    /// `errors.As(err, &*NotImplemented)`: the [`NotImplemented`] anywhere in the chain.
    pub fn as_not_implemented(&self) -> Option<&NotImplemented> {
        match self {
            Error::NotImplemented(n) => Some(n),
            Error::Refused {
                source: Some(s), ..
            } => s.as_not_implemented(),
            Error::Wrapped { sources, .. } => sources.iter().find_map(Error::as_not_implemented),
            _ => None,
        }
    }

    /// `errors.Is(err, ErrPassword)`.
    pub fn is_password(&self) -> bool {
        matches!(self, Error::Password)
    }

    /// `errors.Is(err, ErrMirroredOrientation)`.
    pub fn is_mirrored_orientation(&self) -> bool {
        matches!(self, Error::MirroredOrientation)
    }

    /// The short stable token the parity oracle writes to `.err` files, equal to `errClass` of
    /// the Go oracle (tests/parity/go/oracle/errclass.go, docs/parity.md "Error classes"):
    ///
    /// * `limit/<name>`, `limit/alloc`, `oom`, `cancelled`
    /// * `not-implemented/<capability>`
    /// * `refused/<reason>`
    /// * `not-single-raster`, `unsupported-codec`, `mirrored-orientation`, `jbig2/unsupported`,
    ///   `unstampable-rune`, `password`
    /// * `error`: the untyped Go errors (`Malformed`, `CorruptProvenance`, `UnsupportedCodec`,
    ///   `UnbalancedContent`)
    /// * `pdf/<pdf-rs class>`, with the untyped pdf-go errors as [`pdf_class`] sorts them
    pub fn class(&self) -> String {
        match self {
            Error::Limit { name, .. } => format!("limit/{name}"),
            Error::Alloc { .. } => "limit/alloc".to_string(),
            Error::OutOfMemory => "oom".to_string(),
            Error::Cancelled => "cancelled".to_string(),
            Error::NotImplemented(n) => format!("not-implemented/{}", n.capability),
            Error::Refused { reason, .. } => format!("refused/{reason}"),
            Error::NotSingleRaster => "not-single-raster".to_string(),
            Error::UnsupportedImageCodec => "unsupported-codec".to_string(),
            Error::MirroredOrientation => "mirrored-orientation".to_string(),
            Error::UnsupportedJbig2Feature(_) => "jbig2/unsupported".to_string(),
            Error::UnstampableRune(_) => "unstampable-rune".to_string(),
            Error::Password => "password".to_string(),
            Error::CorruptProvenance
            | Error::UnsupportedCodec
            | Error::Malformed(_)
            | Error::Image(_)
            | Error::UnbalancedContent => "error".to_string(),
            Error::Pdf(e) => pdf_class(e),
            Error::Wrapped { sources, .. } => sources
                .first()
                .map_or_else(|| "error".to_string(), Error::class),
        }
    }
}

/// pdf-rs sentinels that Go's `isSentinel` (errclass.go) lists: `pdf/syntax`. The other
/// sentinels are internal to pdf-go's read path and the oracle does not know them, so they are
/// untyped there.
const SYNTAX_SENTINELS: &[pdf::Sentinel] = &[
    pdf::Sentinel::ActionCycle,
    pdf::Sentinel::AnnotationIrtCycle,
    pdf::Sentinel::BeadCycle,
    pdf::Sentinel::CMapCycle,
    pdf::Sentinel::ColorSpaceCycle,
    pdf::Sentinel::FormFieldCycle,
    pdf::Sentinel::FormFieldDuplicate,
    pdf::Sentinel::FunctionCycle,
    pdf::Sentinel::HalftoneCycle,
    pdf::Sentinel::MaxRecursionDepth,
    pdf::Sentinel::MediaClipCycle,
    pdf::Sentinel::NameTreeCycle,
    pdf::Sentinel::NameTreeDuplicate,
    pdf::Sentinel::NumberTreeCycle,
    pdf::Sentinel::NumberTreeDuplicate,
    pdf::Sentinel::PageTreeCycle,
    pdf::Sentinel::PageTreeDuplicate,
    pdf::Sentinel::PatternCycle,
    pdf::Sentinel::RenditionCycle,
    pdf::Sentinel::StructureTreeCycle,
    pdf::Sentinel::StructureTreeDuplicate,
    pdf::Sentinel::TargetCycle,
    pdf::Sentinel::PageNotFound,
    pdf::Sentinel::ExpectedDict,
    pdf::Sentinel::MissingEncryptDictObject,
    pdf::Sentinel::WrongTypeEncryptDictObject,
    pdf::Sentinel::CorruptObjectOffset,
    pdf::Sentinel::XRefStreamIndexSizeMismatch,
    pdf::Sentinel::ReferenceDoesNotExist,
    pdf::Sentinel::MissingFont,
    pdf::Sentinel::BookmarksRepair,
    pdf::Sentinel::PageContentCorrupt,
    pdf::Sentinel::TJExpressionCorrupt,
    pdf::Sentinel::BIExpressionCorrupt,
];

/// The class of a pdf-rs error, equal to Go's `pdfClass` (errclass.go). A typed error is
/// `pdf/<its pdf-rs class>`. An untyped one (`Corrupt`, `Validation`, a sentinel Go does not
/// list) is `pdf/syntax` for a listed sentinel, else `pdf/validate` when the whole message has
/// `validation error` (the context Go's `api` puts on every error of the validation phase, so
/// also on untyped ones), else `error`.
fn pdf_class(e: &pdf::Error) -> String {
    match e.root() {
        pdf::Error::Sentinel(s) if SYNTAX_SENTINELS.contains(s) => "pdf/syntax".to_string(),
        pdf::Error::Sentinel(_) | pdf::Error::Corrupt(_) | pdf::Error::Validation(_) => {
            if e.to_string().contains("validation error") {
                "pdf/validate".to_string()
            } else {
                "error".to_string()
            }
        }
        _ => format!("pdf/{}", e.class()),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Limit { name, got, max } => {
                write!(f, "byblos: limit {name} exceeded: {got} > {max}")
            }
            Error::Alloc { got, max } => {
                write!(f, "byblos: memory budget: {got} bytes needed, {max} left")
            }
            Error::OutOfMemory => f.write_str("byblos: out of memory"),
            Error::Cancelled => f.write_str("byblos: cancelled"),
            Error::NotImplemented(n) => write!(f, "{n}"),
            Error::Refused { reason, source, .. } => match source {
                Some(s) => write!(f, "{s}"),
                None => write!(f, "byblos: page raster refused: {reason}"),
            },
            Error::NotSingleRaster => f.write_str("byblos: page is not a single raster"),
            Error::UnsupportedImageCodec => {
                f.write_str("byblos: page raster uses an image codec byblos cannot decode")
            }
            Error::MirroredOrientation => f.write_str("byblos: EXIF orientation is mirrored"),
            Error::UnsupportedJbig2Feature(s) => {
                write!(
                    f,
                    "byblos: JBIG2 stream uses a feature byblos does not decode: {s}"
                )
            }
            Error::UnstampableRune(c) => write!(
                f,
                "byblos: rune {c:?} is outside the glyphless font's coverage"
            ),
            Error::Password => f.write_str("byblos: document needs a password"),
            Error::CorruptProvenance => f.write_str("byblos: corrupt provenance value"),
            Error::UnsupportedCodec => f.write_str("byblos/pdfdoc: image codec cannot be rendered"),
            // Go's untyped `fmt.Errorf` text, verbatim: only catchPanic (no Rust analogue) adds
            // the "byblos/pdfdoc: malformed document" prefix.
            Error::Malformed(s) => f.write_str(s),
            Error::UnbalancedContent => {
                f.write_str("byblos/pdfdoc: unbalanced q/Q in content stream")
            }
            Error::Image(s) => f.write_str(s),
            Error::Pdf(e) => write!(f, "{e}"),
            Error::Wrapped { msg, .. } => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::NotImplemented(n) => Some(n),
            Error::Refused {
                source: Some(s), ..
            } => Some(&**s),
            Error::Pdf(e) => Some(e),
            Error::Wrapped { sources, .. } => sources
                .first()
                .map(|e| e as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

impl From<NotImplemented> for Error {
    fn from(n: NotImplemented) -> Self {
        Error::NotImplemented(n)
    }
}

impl From<TryReserveError> for Error {
    fn from(_: TryReserveError) -> Self {
        Error::OutOfMemory
    }
}

impl From<pdf::Error> for Error {
    /// A pdf-rs allocation refusal or cancellation keeps its meaning, also when it is the cause
    /// under a context wrap or comes from the nested deflate or jpeg; everything else is wrapped.
    fn from(e: pdf::Error) -> Self {
        match e.root() {
            pdf::Error::Alloc { got, max }
            | pdf::Error::Jpeg(jpeg::Error::AllocLimit { got, max }) => Error::Alloc {
                got: *got,
                max: *max,
            },
            // deflate's AllocLimit carries no sizes.
            pdf::Error::Flate(deflate::Error::AllocLimit) => Error::Alloc { got: 0, max: 0 },
            pdf::Error::OutOfMemory
            | pdf::Error::Flate(deflate::Error::OutOfMemory)
            | pdf::Error::Jpeg(jpeg::Error::OutOfMemory) => Error::OutOfMemory,
            pdf::Error::Cancelled
            | pdf::Error::Flate(deflate::Error::Cancelled)
            | pdf::Error::Jpeg(jpeg::Error::Cancelled) => Error::Cancelled,
            // Go's `catchPanic` turns a pdf-go panic into `ErrMalformed` on every pdfdoc path
            // (validate, optimize, linearize, ...); `?` and `pdf_err` reach here without `seam`.
            pdf::Error::Panic(m) => {
                Error::Malformed(format!("byblos/pdfdoc: malformed document: {m}"))
            }
            _ => Error::Pdf(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_message_and_class() {
        let e = Error::Limit {
            name: "image-pixels",
            got: 11,
            max: 10,
        };
        assert_eq!(
            e.to_string(),
            "byblos: limit image-pixels exceeded: 11 > 10"
        );
        assert_eq!(e.class(), "limit/image-pixels");
        assert!(e.is_limit());
        assert!(!e.is_not_implemented());
    }

    #[test]
    fn plain_classes() {
        assert_eq!(Error::Cancelled.class(), "cancelled");
        assert_eq!(Error::Alloc { got: 2, max: 1 }.class(), "limit/alloc");
        assert_eq!(Error::OutOfMemory.class(), "oom");
        assert_eq!(Error::Password.class(), "password");
        assert_eq!(Error::NotSingleRaster.class(), "not-single-raster");
        assert_eq!(Error::UnsupportedImageCodec.class(), "unsupported-codec");
        assert_eq!(Error::MirroredOrientation.class(), "mirrored-orientation");
        assert_eq!(
            Error::UnsupportedJbig2Feature("x".into()).class(),
            "jbig2/unsupported"
        );
        assert_eq!(Error::UnstampableRune('x').class(), "unstampable-rune");
        assert_eq!(Error::Malformed("x".into()).class(), "error");
        assert_eq!(Error::Image("x".into()).class(), "error");
        assert_eq!(Error::CorruptProvenance.class(), "error");
        assert_eq!(Error::UnsupportedCodec.class(), "error");
        assert_eq!(Error::UnbalancedContent.class(), "error");
    }

    #[test]
    fn refusal_keeps_the_chain() {
        let e = Error::Refused {
            page: 3,
            reason: "multiple-images".into(),
            class: "not-single-raster".into(),
            source: Some(Box::new(Error::NotSingleRaster)),
        };
        assert!(e.is_not_single_raster());
        assert!(!e.is_unsupported_image_codec());
        assert_eq!(e.class(), "refused/multiple-images");
        assert_eq!(e.to_string(), "byblos: page is not a single raster");

        let c = Error::Refused {
            page: 1,
            reason: "unsupported-codec-jbig2".into(),
            class: "unsupported-codec".into(),
            source: Some(Box::new(Error::UnsupportedImageCodec)),
        };
        assert!(c.is_unsupported_image_codec());
        assert_eq!(c.class(), "refused/unsupported-codec-jbig2");

        let hand = Error::Refused {
            page: 1,
            reason: "vector-paint".into(),
            class: "not-single-raster".into(),
            source: None,
        };
        assert!(!hand.is_not_single_raster());
        assert_eq!(
            hand.to_string(),
            "byblos: page raster refused: vector-paint"
        );
    }

    #[test]
    fn wrapped_chain_with_two_causes() {
        let n = NotImplemented {
            capability: "decode-jbig2".into(),
            why: "w".into(),
            issue: "i".into(),
        };
        let e = Error::Wrapped {
            msg: "outer: inner".into(),
            sources: vec![
                Error::UnsupportedImageCodec,
                Error::NotImplemented(n.clone()),
            ],
        };
        assert!(e.is_unsupported_image_codec());
        assert!(e.is_not_implemented());
        assert!(!e.is_not_single_raster());
        assert_eq!(e.as_not_implemented(), Some(&n));
        assert_eq!(e.class(), "unsupported-codec");
        assert_eq!(e.to_string(), "outer: inner");
        let r = Error::Refused {
            page: 1,
            reason: "unsupported-codec-jbig2".into(),
            class: "unsupported-codec-jbig2".into(),
            source: Some(Box::new(e)),
        };
        assert!(r.is_not_implemented());
        assert_eq!(r.as_not_implemented(), Some(&n));
        assert_eq!(r.class(), "refused/unsupported-codec-jbig2");
    }

    #[test]
    fn pdf_errors() {
        assert_eq!(
            Error::from(pdf::Error::Alloc { got: 5, max: 4 }),
            Error::Alloc { got: 5, max: 4 }
        );
        assert_eq!(Error::from(pdf::Error::Cancelled), Error::Cancelled);
        let e = Error::from(pdf::Error::WrongPassword);
        assert_eq!(e.class(), "pdf/wrong-password");
        let l = Error::from(pdf::Error::DecodeLimit);
        assert!(l.is_limit());
        assert_eq!(
            Error::from(pdf::Error::Flate(deflate::Error::Cancelled)),
            Error::Cancelled
        );
        assert_eq!(
            Error::from(pdf::Error::Jpeg(jpeg::Error::AllocLimit { got: 3, max: 2 })),
            Error::Alloc { got: 3, max: 2 }
        );
        assert_eq!(
            Error::from(pdf::Error::Flate(deflate::Error::AllocLimit)).class(),
            "limit/alloc"
        );
        assert_eq!(
            Error::from(pdf::Error::Jpeg(jpeg::Error::AfterScan {
                cause: Box::new(jpeg::Error::Format("bad")),
                truncated: false,
            }))
            .class(),
            "pdf/jpeg/format"
        );
    }

    #[test]
    fn untyped_pdf_errors() {
        let c = |e: pdf::Error| Error::from(e).class();
        let s = pdf::Error::Sentinel;
        assert_eq!(c(s(pdf::Sentinel::PageNotFound)), "pdf/syntax");
        assert_eq!(c(s(pdf::Sentinel::Eof)), "error");
        assert_eq!(c(pdf::Error::Corrupt("x".into())), "error");
        let v = pdf::Error::Corrupt("x".into()).context("validation error");
        assert_eq!(c(v.context("byblos/pdfdoc: optimize")), "pdf/validate");
        let v = s(pdf::Sentinel::PageNotFound).context("validation error");
        assert_eq!(c(v), "pdf/syntax");
    }
}
