# byblos-rs

A Rust port of [dobbo-ca/byblos](https://github.com/dobbo-ca/byblos) at 086b88b: a PDF pipeline
for scanned documents. It inspects PDFs, extracts the single page-covering raster of a scanned
page, binarises (Sauvola), encodes and decodes JBIG2, quantises, downsamples, builds PDFs from
images, stamps an invisible OCR text layer, optimises and linearises, records provenance and
renders thumbnails, output-compatible with the Go library. Pure Rust, no `unsafe`
(`#![forbid(unsafe_code)]`). Apache-2.0; see `NOTICE`. It builds on the pdf-rs, jpeg-rs,
deflate-rs and png-rs ports ([docs/dependencies.md](docs/dependencies.md)). The port is in progress:
[docs/scope.md](docs/scope.md) is the plan and the order of the waves.

```toml
byblos = { git = "https://github.com/dobbo-ca/byblos-rs", rev = "<sha>" }
```

## Quick start

```rust
use byblos::{Bitmap, Limits};

// At most one thread and 64 MiB for the call.
let limits = Limits { max_threads: 1, max_alloc: 64 << 20, ..Limits::default() };
let mut bitmap = Bitmap::new(100, 50, &limits).unwrap(); // 1 bit per pixel, a set bit is black
bitmap.set(3, 4, 1);
assert_eq!(bitmap.get(3, 4), 1);
```

Every operation is one function taking `&Limits` and, where it can run long, a cancel flag
(`&AtomicBool`).

## Resource limits

There is no unlimited call: every entry point takes `&Limits`. A field `<= 0` takes its default,
as in Go; `max_alloc` and `max_threads` are the two fields the ports share. Over a Go cap the call
returns `Error::Limit` with the name in the last column.

| Field | Default | Unit | Error name |
|---|---|---|---|
| `max_objects`; `max_pages` | 200 000; 10 000 | trailer `/Size`; pages | `objects`; `pages` |
| `max_stream_bytes`; `max_page_stream_bytes` | 64 MiB; 128 MiB | decoded non-image stream; all of one page | `stream-bytes`; `page-stream-bytes` |
| `max_image_pixels`; `max_encoded_image_pixels` | 500 000 000; 1 << 26 | pixels; pixels for `embed_png`, `build_pdf` | `image-pixels`; `encoded-image-pixels` |
| `max_images`, `max_inline_images`, `max_unresolved` | 262 144 | entries one page walk records (inspect also checks the document total under `images`) | `images`; `inline-images`; `unresolved` |
| `max_font_program_bytes`; `max_render_font_bytes` | 16 MiB | one embedded font; fonts one render keeps | none (font skipped) |
| `max_retained_bytes`; `max_png_text_bytes` | 128 MiB; 64 MiB | page walk keeps; PNG text chunks | `retained-bytes`; `png-text-bytes` |
| `max_object_stream_entries`, `max_total_decode_bytes`, `max_object_bytes`, `max_parsed_bytes` | derived | pdf-rs parse caps | from pdf-rs |
| `max_stream_total_bytes` | 0 (off) | encoded streams per parse | from pdf-rs |
| `max_raster_side`; `max_raster_pixels` | 20 000; 1 << 26 | canvas side; canvas pixels | `raster-side`; `raster-pixels` |
| `max_path_points`; `max_fill_work` | 1 << 21; 1 << 28 | path points; active edges per row, summed | `path-points`; `fill-work` |
| `max_gstate_depth`; `max_form_depth`; `max_form_invocations` | 1024; 8; 4096 | `q` nesting; Form nesting; Form invocations per walk | `q-depth`; `form-depth`; `form-invocations` |
| `max_operands`, `max_text_shows`, `max_paints` | 8192; 1 << 16; 1 << 18 | entries | none (walk stops recording) |
| `max_alloc`; `max_threads` | 4 GiB; `available_parallelism` | bytes, peak of one call; threads, caller included | `Error::Alloc`; none |

[docs/limits.md](docs/limits.md) has the derivation rules and the pdf-rs conversion.

- `max_alloc` is the peak of one call, all threads and the nested pdf-rs, jpeg-rs and
  deflate-rs together, charged before each input-sized allocation. It replaces Go's global
  memory pool. A few paths are not yet charged to the shared ledger: see
  [Known gaps](docs/limits.md#known-gaps).
- `max_threads`: 1 spawns no thread and touches no pool. The output is the same for every value.
  A caller's pool wins over the crate's lazy shared pool (none is used yet).
- A cancel flag that is set makes the next check return `Error::Cancelled`.

## Docs

[limits](docs/limits.md), [safety](docs/safety.md), [threading](docs/threading.md), [go-differences](docs/go-differences.md),
[dependencies](docs/dependencies.md), [parity](docs/parity.md), [fuzzing](docs/fuzzing.md),
[benchmarks](docs/benchmarks.md), [scope](docs/scope.md).
