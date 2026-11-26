# Dependencies

Dependency reasons go here, not in the README. `Cargo.lock` is committed. There
is no `cc`, `bindgen` or `-sys` crate, and no FFI. All four dependencies are dobbo-ca ports,
pinned by rev; jpeg-rs and deflate-rs are each linked twice (see below). The only crates.io
crates are transitive, through pdf-rs and jpeg-rs: `rayon` 1.12.0, `rayon-core` 1.13.0, `crossbeam-deque`, `crossbeam-epoch`, `crossbeam-utils` and `either`
(see `Cargo.lock`). They are pdf-rs's and jpeg-rs's thread pool; the reason is in pdf-rs's
`docs/threading.md`. byblos-rs itself names none of them.

| Crate | Source | Why |
|---|---|---|
| `pdf` | git, `dobbo-ca/pdf-rs`, rev `3854d8e` | everything `internal/pdfdoc` takes from pdf-go: reading, validating, optimising and writing PDFs and extracting images. Also `Mem` (the shared `max_alloc` ledger, which several documents of one call share through the `*_with_mem` entry points), `GoTime` and `WriteEnv`, `filter::goflate::ZlibWriter` (Go's zlib at levels 1, 6 and 9, byte-exact), `filter::gojpeg` (Go's `image/jpeg` decoder) and `crypto::md5`. Wave 1 uses only the library parts, wave 2 the PDF API |
| `jpeg` | git, `dobbo-ca/jpeg-rs`, rev `c0cf0fc` | the libjpeg-turbo-exact decode that `extract.go` runs through jpeg-go, with `Format::{Rgba, Gray}` |
| `deflate` | git, `dobbo-ca/deflate-rs`, rev `211232f` | every inflate byblos does is decode-only and Go-exact: png-rs (the PNG decoder), PNG text chunks, `TranscodePNG`, the adler32 check of a Flate stream |
| `png` | git, `dobbo-ca/png-rs`, rev `1a4cd76` | the PNG decoder (`Profile::Go`, `Format::Native`): Go `image/png`'s chunk walk, checksums, zlib stream, unfiltering and errors. `image/png_read.rs` turns its rows into Go's image types and maps its errors. It pins its own deflate-rs rev, bumped separately from byblos's |

**One copy of jpeg-rs and deflate-rs.** byblos-rs pins the same revs as pdf-rs `3854d8e`
(jpeg-rs `c0cf0fc`, deflate-rs `211232f`), so each crate links once, and the types that cross
pdf-rs's API (`pdf::Error::Flate`, `pdf::Error::Jpeg`, the image types of `gojpeg::GoImage`) are
the ones byblos uses. Bump the three revs together.

Transitive crates.io crates (`Cargo.lock`), all MIT OR Apache-2.0: `rayon` 1.12.0 and `rayon-core`
1.13.0 (the thread pool pdf-rs and jpeg-rs use; byblos-rs adds no pool of its own and reaches it
through `max_threads`), and what they need: `crossbeam-deque`, `crossbeam-epoch`,
`crossbeam-utils` and `either`. They are not copied or modified, so NOTICE does not list them.

Go's `compress/flate` encoder at levels 1 and 9 is pdf-rs's (`ZlibWriter::with_level`), so the
code is not duplicated. Anything else the Go library takes
from its standard library or `x/image` (`sort.Slice`, `math`, `encoding/xml`, `encoding/json`,
`math/big`, the PNG reader and writer, the JPEG writer, the draw scaler, `font/sfnt`, a TIFF
reader) is ported in-crate, because output parity needs Go's exact behaviour. NOTICE lists each
port with its licence.

All four git dependencies (pdf-rs, jpeg-rs, deflate-rs, png-rs) are public; CI fetches them once in
the `deps` stage of `.github/ci/Dockerfile`, and every later step runs offline. The separate fuzz crate (`tests/fuzz`)
adds `libfuzzer-sys`, which only that crate uses.

The Go side of the parity harness fetches nothing: the byblos non-test sources, pdf-go, a
trimmed jpeg-go and the public modules they build with are vendored under `tests/parity/go`
([parity.md](parity.md)).
