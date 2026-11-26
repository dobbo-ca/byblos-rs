//! A Rust port of [dobbo-ca/byblos](https://github.com/dobbo-ca/byblos): a PDF pipeline for
//! scanned documents. It inspects PDFs page by page, extracts the single page-covering raster
//! of a scanned page (and refuses, with a reason, when a page is not one), binarises (Sauvola),
//! encodes and decodes JBIG2, quantises, downsamples, builds PDFs from images, stamps an
//! invisible OCR text layer, rewrites documents (optimise, linearise, image substitution, page
//! edit), records provenance, and renders thumbnails. PDF parsing and writing go through
//! pdf-rs.
//!
//! The port is in progress: docs/scope.md is the plan and lists what is in and out, and
//! docs/go-differences.md the decided differences from Go. The modules that have not landed yet
//! are empty and name the Go files they will port. The API is one function per Go operation
//! (Go's `X`, `XContext` and `XLimits` become one), taking `&Limits` and, where the work can
//! run long, `cancel: &AtomicBool`.
//!
//! # Quick start
//!
//! ```
//! use byblos::{Bitmap, Error, Limits};
//!
//! // At most one thread and 64 MiB for the call.
//! let limits = Limits {
//!     max_threads: 1,
//!     max_alloc: 64 << 20,
//!     ..Limits::default()
//! };
//!
//! // A 100 x 50 bilevel bitmap, every pixel clear. A set bit is black ink.
//! let mut bitmap = Bitmap::new(100, 50, &limits)?;
//! bitmap.set(3, 4, 1);
//! assert_eq!(bitmap.get(3, 4), 1);
//! assert_eq!(bitmap.get(-1, 0), 0); // outside reads as clear
//! # Ok::<(), Error>(())
//! ```
//!
//! # Resource limits
//!
//! Every entry point takes [`Limits`], and every call that can run long takes a cancel flag.
//! Set them first:
//!
//! ```
//! use byblos::{Bitmap, Error, Limits};
//! use std::sync::atomic::{AtomicBool, Ordering};
//!
//! // No thread is spawned, and the call never holds more than 1 MiB.
//! let limits = Limits {
//!     max_threads: 1,
//!     max_alloc: 1 << 20,
//!     ..Limits::default()
//! };
//!
//! // Buffers that scale with the input are checked against `max_alloc` before they are
//! // allocated: a 32 768 x 32 768 bitmap needs 128 MiB.
//! let err = Bitmap::new(32_768, 32_768, &limits).unwrap_err();
//! assert!(matches!(err, Error::Alloc { .. }));
//! assert_eq!(err.class(), "limit/alloc");
//!
//! // Cancellation: a set flag makes the next check return `Error::Cancelled`.
//! let cancel = AtomicBool::new(false);
//! cancel.store(true, Ordering::Relaxed);
//! assert_eq!(byblos::limits::check_cancel(&cancel), Err(Error::Cancelled));
//! ```
//!
//! The fields of [`Limits`] mirror byblos's `limits.Limits`. A field `<= 0` takes its default,
//! as in Go; docs/limits.md has the table with every default and error name. The two fields
//! every port shares:
//!
//! * `max_alloc` (default 4 GiB, bytes): the enforced peak of one call, all threads together,
//!   including pdf-rs, jpeg-rs and deflate-rs, which are charged to the same ledger
//!   ([`Limits::mem`]). Over it: [`Error::Alloc`]. It replaces Go's process-global memory pool.
//!   A few paths are not yet charged to the shared ledger (docs/limits.md, "Known gaps").
//! * `max_threads` (default [`std::thread::available_parallelism`]): the most threads of one
//!   call, the calling thread included. 1 spawns no thread and touches no pool; 0 counts as 1.
//!   The output is the same for every value. byblos itself is serial in phase 1; the value is
//!   passed to pdf-rs and jpeg-rs.
//!
//! The limit errors are [`Error::Limit`] (with the Go limit name, for example `image-pixels`),
//! [`Error::Alloc`] and [`Error::Cancelled`]; [`Error::is_limit`] mirrors Go's
//! `errors.Is(err, ErrLimit)`.
//!
//! A Rust port of dobbo-ca/byblos at 086b88b.
#![forbid(unsafe_code)]
// The ported code keeps Go's index loops and long argument lists.
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::chunks_exact_to_as_chunks,
    clippy::type_complexity
)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]

// Wave 1a (foundations).
pub mod bitmap;
pub mod error;
pub mod go;
pub mod image;
pub mod limits;
pub mod notimplemented;
pub mod stats;

// Wave 1b.
pub mod dct;
pub mod jbig2;
pub mod sauvola;

// Wave 1c.
pub mod downsample;
pub mod quantize;

// Wave 1d.
pub mod build;
pub mod embed;
pub mod encoded;
pub mod glyphless;
pub mod linearize;
pub mod pdfbuild;
pub mod provenance;
pub mod upgrade;

// Wave 1e.
pub mod content;
pub mod render;

// Wave 2.
pub mod pdfdoc;

// Wave 3.
pub mod editpages;
pub mod extract;
pub mod inspect;
pub mod optimize;
pub mod render_page;
pub mod stamp;
pub mod substitute;

pub use bitmap::Bitmap;
pub use error::{Error, Result};
pub use image::{Image, Rect};
pub use limits::Limits;
pub use notimplemented::NotImplemented;

/// Compiles the README quick start as a doc-test so it cannot go stale.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
