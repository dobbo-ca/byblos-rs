# Safety

byblos-rs contains no `unsafe`: `src/lib.rs` has `#![forbid(unsafe_code)]`, so a use of it
anywhere in the crate is a compile error. There is no FFI, no `cc`/`bindgen`/`-sys` crate and no
SIMD intrinsics ([dependencies.md](dependencies.md)). Memory safety is the compiler's job; what
is left for the code is hostile input.

## Hostile-input rules

Every input (PDF, JBIG2, JPEG, PNG, XML, JSON, font program, content stream) is untrusted.

- No panics on input. Outside `cfg(test)` the crate denies `clippy::unwrap_used`,
  `expect_used`, `panic`, `todo` and `unimplemented`; input-derived sizes and counts are
  checked against the limits before use. `cargo test` runs with overflow checks on, and fuzzing
  hunts the remaining panics.
- Memory is charged to `Limits::max_alloc` before it is allocated, so a small file cannot ask
  for a huge buffer. Counts and sizes read from the file are checked against the limits first.
- Loops over input are bounded by the Go caps (`Error::Limit`) and check the cancel flag
  (`Error::Cancelled`).
- Recursion over input structure (page trees, form XObjects, nested JSON) has a depth cap; the XML decoder does not recurse.
- A malformed input is an `Error`, never a partial silent success, except where Go returns a
  partial result on purpose (documented on the function, for example `content::walk`).
- Fuzzing covers the parsers ([fuzzing.md](fuzzing.md)); a crash found there is fixed.

See [limits.md](limits.md) for every cap and the error it returns.
