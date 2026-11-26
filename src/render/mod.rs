//! Wave 1e: port of byblos internal/render/{render,text,cff,type1,glyf,substitute}.go and the
//! Liberation fonts in render/fonts. `page.rs` is `render.go` and `text.rs` is `text.go`.
pub mod cff;
pub mod glyf;
pub mod page;
pub mod sfnt;
pub mod substitute;
pub mod text;
pub mod type1;

pub use page::{ImageFor, PageParams, RenderImage, page};
pub use text::{Font, FontFor};
