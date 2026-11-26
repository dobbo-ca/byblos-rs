//! Wave 1d: port of byblos `internal/linearize/{doc,hints,layout,write}.go`: ISO 32000-1:2008
//! Annex F linearization: the object partition, the two cross-reference sections, the
//! linearization parameter dictionary and the primary hint stream.
//!
//! It knows nothing about parsing PDF. Everything it needs arrives as a neutral representation
//! (object numbers, outgoing references, and already-serialized object bodies) built by
//! `pdfdoc`. The split is not cosmetic: the hint tables are bit-packed, column-major, MSB-first
//! structures where a wrong bit width produces a file every viewer accepts and qpdf rejects
//! with "overflow reading bit stream". Apart from the error and limit types it uses only the
//! standard library and `pdf::filter::goflate::ZlibWriter` (Go's zlib at level 6, for the hint
//! stream).
//!
//! * [`layout`] (`layout.go`): [`Graph`], [`Plan`] and [`plan_layout`].
//! * [`hints`] (`hints.go`, `hints_test.go`): [`Hints`] and [`encode_hints`].
//! * [`write`] (`write.go`): [`Meta`] and [`write()`].
//!
//! Object numbers, offsets and counts are `i64` (Go's `int`). The Go package has no context;
//! here [`plan_layout`] and [`write()`] take `&Limits` (their buffers are charged to
//! `max_alloc`) and the cancel flag.

pub mod hints;
pub mod layout;
pub mod write;

pub use hints::{Hint, Hints, OutlineHint, PageHint, SharedHint, encode_hints, nbits};
pub use layout::{Graph, Plan, plan_layout};
pub use write::{Meta, write};

use crate::error::Error;

#[cfg(test)]
mod tests;

/// An untyped `fmt.Errorf`.
pub(crate) fn malformed(msg: impl Into<String>) -> Error {
    Error::Malformed(msg.into())
}

/// `fmt.Errorf("<prefix>: %w", e)`: the message gains the prefix, the class stays.
pub(crate) fn wrap(e: Error, prefix: &str) -> Error {
    match e {
        Error::Malformed(s) => Error::Malformed(format!("{prefix}: {s}")),
        Error::Pdf(p) => Error::Pdf(p.context(prefix)),
        other => other,
    }
}
