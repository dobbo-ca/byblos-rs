# Limits

`byblos::Limits` is `internal/limits.Limits` of the Go library, plus the two fields every port
shares (`max_alloc`, `max_threads`). Every entry point takes `&Limits`; there is no unlimited
call. Source: `src/limits.rs`.

## The zero rule

The Go fields are `i64` and keep Go's rule: a value `<= 0` takes the default (`def(v, d)`).
`Limits::resolved()` returns the same limits with every such field replaced by the value in
force. Accessors with Go's names (`limits.objects()`, `limits.images()`, ...) return the value in
force for one field.

`Limits::default()` holds Go's default for every field that has a constant default, except
`max_gstate_depth`, `max_form_depth` and `max_form_invocations`, which are 0 there (inspect's
`defaultWalkCap` tests `<= 0`; the value in force is 1024, 8 and 4096). A field whose default
depends on another is also 0, which means "derive it", as Go's zero value does:

| Field | 0 means |
|---|---|
| `max_images`, `max_inline_images`, `max_unresolved` | `max_page_stream_bytes / 512` |
| `max_object_stream_entries` | `max_objects`; the value in force never exceeds `max_objects` |
| `max_total_decode_bytes`, `max_object_bytes` | `max_stream_bytes` |
| `max_parsed_bytes` | three quarters of the object table (1536 bytes times the trailer `/Size`) plus `max_total_decode_bytes`; see `Limits::parsed_bytes(size)` |
| `max_stream_total_bytes` | off |

## Fields

| Field | Default | Unit; over it |
|---|---|---|
| `max_objects` | 200 000 | trailer `/Size`: `Limit("objects")` |
| `max_pages` | 10 000 | pages: `Limit("pages")` |
| `max_stream_bytes` | 64 MiB | one decoded non-image stream: `Limit("stream-bytes")` |
| `max_page_stream_bytes` | 128 MiB | decoded non-image streams of one page together: `Limit("page-stream-bytes")` |
| `max_image_pixels` | 500 000 000 | width x height of one image: `Limit("image-pixels")` |
| `max_font_program_bytes` | 16 MiB | one decoded embedded font program |
| `max_images` | 262 144 | images one page walk records: `Limit("images")`; `inspect` also checks the document-wide total under `images` (`src/inspect.rs:208`) |
| `max_inline_images` | 262 144 | inline images one page walk records: `Limit("inline-images")` |
| `max_unresolved` | 262 144 | unresolved references one page walk records: `Limit("unresolved")` |
| `max_object_stream_entries` | `max_objects` | objects packed in object streams over one parse (to pdf-rs) |
| `max_total_decode_bytes` | `max_stream_bytes` | bytes pdf-rs decodes over one parse (to pdf-rs) |
| `max_object_bytes` | `max_stream_bytes` | one indirect object, stream payload excluded (to pdf-rs) |
| `max_parsed_bytes` | derived | estimated heap of the parsed objects (to pdf-rs) |
| `max_stream_total_bytes` | 0 (off) | encoded stream bytes pdf-rs reads over one parse |
| `max_retained_bytes` | 128 MiB | what one page's content walk keeps: `Limit("retained-bytes")` |
| `max_encoded_image_pixels` | 1 << 26 | `embed_png` and `build_pdf`: `Limit("encoded-image-pixels")` |
| `max_png_text_bytes` | 64 MiB | text from one PNG's text chunks: `Limit("png-text-bytes")` |
| `max_raster_side`, `max_raster_pixels` | 20 000; 1 << 26 | the `render_page` canvas: `Limit("raster-side")`, `Limit("raster-pixels")` |
| `max_render_font_bytes` | 16 MiB | font bytes one render retains; past it the render skips fonts and carries on |
| `max_path_points`, `max_fill_work` | 1 << 21; 1 << 28 | render path points and scanline work: `Limit("path-points")`, `Limit("fill-work")` |
| `max_gstate_depth`, `max_form_depth`, `max_form_invocations` | 0 (in force 1024; 8; 4096) | `q` nesting, Form XObject nesting and invocations: `Limit("q-depth")`, `Limit("form-depth")`, `Limit("form-invocations")` |
| `max_operands`, `max_text_shows`, `max_paints` | 8192; 1 << 16; 1 << 18 | pending operands, recorded text shows and paints; past them the walk stops recording and carries on |
| `max_alloc` | 4 GiB | bytes, the enforced peak of one call, all threads and nested crates: `Error::Alloc` |
| `max_threads` | `available_parallelism` | threads, the caller included; 0 counts as 1 |

Fixed constants stay constants, as in Go: the JBIG2 caps (`MaxPagePixels` 1 << 26, 16 MiB per
stream bitmap, 65 536 segments and symbols, 1 << 21 instances) and the render canvas cap
(1 << 32 pixels).

## Errors

`Error::Limit { name, got, max }` carries the Go limit name, so a caller can match on it, and
`Error::is_limit()` is `errors.Is(err, ErrLimit)`. `Error::Alloc { got, max }` is the `max_alloc`
ledger refusing `got` bytes with `max` left. `Error::Cancelled` is a set cancel flag. `class()`
gives the short stable token the parity oracle writes: `limit/<name>`, `limit/alloc`,
`cancelled`.

## `max_alloc`

The peak working memory of one call. `Limits::mem()` returns a `pdf::Mem` of that size; passing
it (or a clone) to pdf-rs, jpeg-rs and deflate-rs makes one call charge byblos and the nested
crates together, so the peak is bounded as a whole. `Limits::alloc_zeroed(n)` checks one buffer
against the whole budget and allocates with `try_reserve_exact`; code that holds several buffers
charges the shared ledger instead. Where Go's `Reserve` charged an estimate up front, Rust
charges the real allocations; the estimates stay only as refusal checks where Go refuses with a
`LimitError`.

### Known gaps

`max_alloc` is not yet one ledger for every path. A call that holds several documents passes one
`pdf::Mem` to each of them through pdf-rs's `*_with_mem` entry points: `build_from_pages` (the
sources, the output, and the validation of the built bytes, which stay charged) and `optimize`
(the parse gate, the JPEG recompression and the rewrite, with the recompressed PDF charged while
it is rewritten). What is left:

- L1: `optimize`'s provenance read and write and its linearization open their own ledgers,
  capped at what the held output leaves; `api::properties` takes no `Mem`
  ([needs-from-pdf-rs.md](needs-from-pdf-rs.md) 2.12).
- L2: the content walk's retained bytes (`Scan`) are not on the document ledger.
- L3: render path points and edge lists are uncharged.
- L7: JBIG2 and `gray_image` buffers are checked one at a time against the budget.

## Conversion to pdf-rs

`Limits::pdf_limits(size)` and `Limits::pdf_config(size)` write the seven pdf-go limits that
`pdfdoc.openSized` and `defaultConfig` write (`pdfdoc.go:81-93, 447-456`): `max_decode_bytes` =
`stream_bytes()`, `max_object_count` = `objects()`, `max_object_stream_total_count` =
`object_stream_entries()`, `max_total_decode_bytes` = `total_decode_bytes()`, `max_object_bytes`
= `object_bytes()`, `max_parsed_bytes` = `parsed_bytes(size)`, `max_stream_total_bytes`. `size`
is the trailer `/Size`; pass `objects()` where Go's `defaultConfig` runs without a document.
`max_alloc` and `max_threads` pass through; every other pdf-rs field keeps its default.

## Measured-peak tests

`tests/resources.rs` is one `#[test]` (the counters are global) that runs real inputs from
`tests/data` through the main entry points at `max_threads` 1, 0, 2 and the default, each at
`max_alloc` 16 MiB and 1 GiB, and asserts:

1. **Output.** The bytes (or the error class) are identical for every `max_threads`.
2. **Heap peak.** A counting `#[global_allocator]` (std only; the one `unsafe` in the test
   binary, with SAFETY comments; the crate keeps `forbid(unsafe_code)`) measures the live heap
   above the call's start. Assert: `peak <= max_alloc + 4 MiB`, whether the call succeeds or
   fails. The 4 MiB is the uncharged fixed overhead: nested-crate scratch (the flate window, JPEG
   tables), the document shell, `Vec` growth where the old and new buffer coexist during a
   `realloc`, and the harness. It is far below what the test hunts (an input of a few hundred KB
   that costs hundreds of MiB). `pdf::Mem::peak()` is not used: the ledger refuses a charge over
   its limit before it records it, so its peak cannot exceed `max_alloc` and the check could
   not fail.
3. **Threads.** A sampler reads `Threads:` of `/proc/self/status` every 200 microseconds (Linux
   only; skipped elsewhere). The call may add at most `max_threads - 1` threads over the count
   just before it (the calling thread is in the baseline). For `max_threads` 1 and 0 the count
   may not exceed the count at the start of the test at all: no thread, no pool. Those runs go
   first, so no lazy pool exists yet. byblos itself spawns none in phase 1 (docs/threading.md),
   so today the assert catches a spawn by byblos; it also covers the threaded kernels when
   they land. The lazy shared pools of pdf-rs and jpeg-rs (`available_parallelism` threads,
   started together) are checked separately: after the 1 and 0 runs the test asserts that
   neither `shared_pool_started()` is true, then runs every case once at the default so the
   pools exist, and the later baselines include them.
4. **Tight budget.** At `max_alloc = 64` bytes every case fails with `Error::Alloc`,
   `Error::OutOfMemory` or a limit error, and its heap peak stays within `max_alloc + 2 MiB`:
   the call is refused before it allocates. Cases whose input is a few hundred bytes (the
   `embed-build` fixtures, the JBIG2 goldens) may fit 64 bytes; for them only the peak is asserted.

| Case | Entry point | Input |
|---|---|---|
| `inspect` | `inspect` | five PDFs of `tests/data/real` (a blank scan, an image page, a DCT page, text, Walden) |
| `extract` | `extract_page_raster`, page 1 | the same five |
| `render_page` | `render_page`, page 1, 400 px | the same five |
| `optimize`, `optimize-linearize-recompress` | `optimize` | the same five |
| `stamp` | `stamp_text_layer` | the same five |
| `sauvola` | `sauvola` | 800 x 1000 RGBA page |
| `downsample-rgb`, `downsample-1bit` | `downsample` 300 to 150 dpi | 800 x 1000 RGBA; 800 x 1000 grey, declared 1 bit |
| `quantize-png`, `quantize-indexed` | `quantize_png`, `quantize_indexed`, 16 and 256 colours | 800 x 1000 RGBA |
| `jbig2-encode`, `jbig2-decode-roundtrip`, `jbig2-decode` | `jbig2::encode_image`, `decode_generic` | the Sauvola page; `jbig2/{text,noise,full}.jb2` |
| `embed-build`, `build-jbig2-3-pages` | `embed_png`, `embed_jpeg`, `build_pdf` | `img2pdf/dpi-300x150.{png,jpg}`; three JBIG2 pages |

### Which field each test covers

| Field | Covered by |
|---|---|
| `max_alloc` | heap peak (2) and tight budget (4), every case |
| `max_threads` | threads (3) and output (1), every case |
| `max_image_pixels` | `downsample-*`, `sauvola`, `quantize-*` (refused from the bounds before allocation), `src/*/tests.rs` |
| `max_encoded_image_pixels`, `max_png_text_bytes` | `embed-build`, `build-jbig2-3-pages`; the refusals are in `src/embed/tests.rs`, `src/build/tests.rs` |
| `max_raster_side`, `max_raster_pixels`, `max_path_points`, `max_fill_work`, `max_gstate_depth`, `max_render_font_bytes` | `render_page` (peak and threads); the refusals are in `src/render/page_tests.rs` |
| `max_retained_bytes`, `max_images`, `max_inline_images`, `max_unresolved`, `max_form_depth`, `max_form_invocations`, `max_operands`, `max_text_shows`, `max_paints` | `inspect`, `extract` (peak and threads); the refusals are in `src/content/walk_tests.rs` and `src/inspect.rs` |
| `max_objects`, `max_pages`, `max_stream_bytes`, `max_page_stream_bytes`, `max_font_program_bytes`, `max_object_stream_entries`, `max_total_decode_bytes`, `max_object_bytes`, `max_parsed_bytes`, `max_stream_total_bytes` | every PDF case through `pdfdoc::open`; the refusals are in `src/pdfdoc/tests.rs` |

The refusals of the caps other than `max_alloc` and `max_threads` are unit tests of the owning
module; `tests/resources.rs` asserts only that no cap lets a call exceed `max_alloc`.

### Where the peak exceeds the ledger by design

The `+ 4 MiB` of assert 2 is the whole allowance; no entry point is asserted against a larger
bound. The uncharged parts are the ones named there. A case that needs more than 4 MiB uncharged
is a finding in the owning module (a buffer that scales with the input and is not charged), not a
bound to raise.
