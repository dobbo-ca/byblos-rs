//! Wave 2: port of byblos
//! internal/pdfdoc/{pdfdoc,limits,buildpages,linearize,text,deterministic,selfwrite,write,annots,straighten,properties,encryptinfo,optimize,catalog}.go
//! over pdf-rs (not fontcensus.go, danglingrefs.go).
//!
//! `pdfdoc` is the only module of byblos that reaches pdf-rs. Everything above it speaks in the
//! types declared here, so replacing the PDF library is a change to this module alone.
//!
//! The module tree has one file per Go file; each file holds the methods of [`Doc`] that its Go
//! file declared:
//!
//! | File | Go | Holds |
//! |---|---|---|
//! | `doc` | `pdfdoc.go` | [`Doc`], [`open`], pages, resources, images, fonts, [`DocEnv`] |
//! | `limits` | `limits.go` | the object count before the parse, the stream caps |
//! | `encryptinfo` | `encryptinfo.go` | [`EncryptInfo`], [`is_wrong_password`] |
//! | `catalog` | `catalog.go` | [`CatalogInfo`] |
//! | `properties` | `properties.go` | [`read_properties`], [`write_properties`] |
//! | `annots`, `buildpages`, `deterministic`, `linearize`, `optimize`, `selfwrite`, `straighten`, `text`, `write` | the Go files of the same names | filled by their own ports |
//!
//! The `Doc` interface of Go (`pdfdoc.go:264`) is the [`Doc`] struct: its write half
//! (`replace_image`, `write`, `add_font_resource`, `append_content`, `wrap_content`) and
//! `annots` are inherent methods that `write`, `text` and `annots` add. The fields they need
//! are `pub(crate)`: `ctx` (the pdf-rs `Context`), `lim`, `images`, `streams`, `refs`,
//! `font_refs`. The helpers they share are `pub(crate)` in `doc`: `seam`, `pdf_err`, `wrapped`,
//! `conv`, `default_config` and `NEVER` (the error mapping of Go's `catchPanic` and
//! `pdfcpuLimit`, the pdf-rs configuration of `defaultConfig`, and a cancel flag that is never
//! set), and `Doc::page_contents`, `Doc::decode`, `Doc::deref`, `Doc::int_entry`,
//! `Doc::dict_entry` and `Doc::array_entry`.

pub mod annots;
pub mod buildpages;
pub mod catalog;
pub mod deterministic;
pub mod doc;
pub mod encryptinfo;
pub mod limits;
pub mod linearize;
pub mod optimize;
pub mod properties;
pub mod selfwrite;
pub mod straighten;
pub mod text;
pub mod write;

#[cfg(test)]
mod tests;

pub use catalog::CatalogInfo;
pub use deterministic::{PINNED_DATE, pin_date, write_deterministic};
pub use doc::{Doc, DocEnv, ImageInfo, Page, Rect, RenderFont, open, open_budgeted};
pub use encryptinfo::{EncryptInfo, is_wrong_password};
pub use limits::{image_decode_bytes, is_limit, object_bound, objects_in, trailer_size};
pub use linearize::{linearize, linearize_context};
pub use optimize::optimize;
pub use properties::{read_properties, write_properties, write_properties_at};
pub use selfwrite::write_document;
pub use write::validate;
