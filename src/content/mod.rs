//! Wave 1e: port of `internal/content/{lexer,walk}.go`, with the `Env` trait.
//!
//! `lexer.rs` is `lexer.go` (the content-stream tokenizer) and `walk.rs` is `walk.go` (the
//! operator walker, its geometry types and the caps). The Go package has no other non-test file.

mod lexer;
mod walk;

pub use lexer::{Kind, Lexer, Token};
pub use walk::{
    BBox, Color, Env, IDENTITY, MAX_NATIVE_DEPTH, Matrix, NullEnv, Paint, Placement, Quad, Raw,
    Scan, Stream, TextShow, XObject, intersect_box, walk,
};
