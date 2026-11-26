# byblos-rs scope

byblos-rs is a Rust port of byblos (`github.com/dobbo-ca/byblos` at `086b88b`). byblos is a
pure-Go PDF pipeline for scanned documents. This document covers what gets ported, what does
not, which dependencies supply what, how parity with Go is proven, and the order of the work.
It follows the rules shared by the dobbo-ca Rust ports; where it departs from them, it says so.

Paths: byblos paths are relative to the byblos repo root. pdf-rs paths are given at
`origin/main` = `9bdaeb5`. Line counts leave out test files (`wc -l`, non-`_test.go`).

Superseded at the pdf-rs pin `b2d3a4d`: Flate levels 1 and 9 (item 2.1) and a public MD5 (2.2)
are in pdf-rs; byblos reads JPEG headers with `gojpeg::decode_config` (item 2.11, pdf-rs
`19019a9`), and jpeg-rs and deflate-rs are pinned at pdf-rs's revs, so one copy of each links.
`docs/dependencies.md` and `docs/needs-from-pdf-rs.md` give the current state; sections 4.1, 4.2,
4.3 and the open-request lists below are kept as planned.

How this was built: area reports from six scouts, then a re-check against the source of every
fact a decision below rests on (the importers of each internal package, the pdf-go symbols in
non-comment code, the pdf-rs equivalents, the Go stdlib paths and the module versions).
Nothing was built or run.

## 1. Summary

byblos covers several jobs:

- It inspects PDFs page by page.
- It extracts the single page-covering raster of a scanned page. It refuses, and gives a
  reason, when a page is not one.
- It binarises (Sauvola), encodes and decodes JBIG2 (generic region; symbol and text regions
  on decode only), quantises (median cut plus Lloyd), and downsamples (Catmull-Rom).
- It builds PDFs from images, using img2pdf-compatible PNG/JPEG/JP2 embedding.
- It stamps an invisible OCR text layer, using the glyphless font.
- It rewrites documents:
  - optimise, with optional JPEG recompression;
  - its own Annex F linearizer;
  - image substitution;
  - page edit and straighten.
- It records capability-based provenance as JSON in the Info dictionary.
- It renders thumbnails, with its own content interpreter and TrueType/CFF/Type 1 outlines.

PDF parsing and writing go through pdf-go (a pdfcpu fork), and only from `internal/pdfdoc`. In
Rust that is pdf-rs.

| Package | Non-test lines | In scope | Reason |
|---|---|---|---|
| root (`byblos`) | 7,100 | 7,100 | the public API |
| `internal/pdfdoc` | 5,850 | 5,539 | the pdf-go seam; `fontcensus.go` (240, used only by `cmd/byblos-fonts`) and `danglingrefs.go` (71, used only by tests) are out |
| `internal/render` | 3,722 | 3,722 | used by `RenderPage` |
| `internal/jbig2` | 3,143 | 2,968 | codec; `profile.go` (175, `ProfileStream`, used only by tests and probes) is out |
| `internal/content` | 1,690 | 1,690 | content lexer and walker (inspect, extract, render, provenance) |
| `internal/pdfbuild` | 1,329 | 1,329 | the image-to-PDF writer and `TranscodePNG` |
| `internal/linearize` | 1,007 | 1,007 | Annex F layout, hints and writer |
| `internal/limits` | 329 | 329 | the caps; the global memory pool is replaced (section 6) |
| `internal/dct` | 127 | 127 | SOF parse and decode sizing |
| `internal/glyphless` | 409 | 81 | width table plus the embedded `.ttf`; `gen.go` (328, `//go:build ignore`) is out |
| `internal/skew` | 731 | 0 | imported only by `placementdeg_test.go` and `skew_probe_test.go` |
| `internal/sample` | 285 | 0 | imported only by `cmd/*` and tests |
| `internal/corpus` | 2,382 | 0 | test and oracle input generator (the Go oracle uses it, see section 5) |
| `internal/bench` | 993 | 0 | used only by `cmd/byblos-bench` |
| `cmd/*` (7 tools) | 2,488 | 0 | census and bench tools |
| `tools/fontmeasure`, `testdata/oracle/gen.go` | 808 | 0 | tooling |

**Go lines in scope: 23,892.** Not counted above are the Go standard library and `x/image`
code that has to be ported in-crate for byte parity (section 4.3). That is about 6,000 more Go
lines: `x/image/font/sfnt`, the `x/image/draw` kernel scaler, the `image/png` reader, the
`image/jpeg` writer, the `encoding/xml` tokenizer, `sort.Slice`, and the `math` trig functions.

Tests: root 26,154 lines across 74 files. Internal tests total 21,313 lines:

| Package | Test lines |
|---|---|
| `internal/pdfdoc` | 7,604 |
| `internal/jbig2` | 5,301 |
| `internal/render` | 4,866 |
| `internal/content` | 2,398 |
| `internal/glyphless` | 422 |
| `internal/pdfbuild` | 311 |
| `internal/linearize` | 237 |
| `internal/limits` | 174 |

Estimated Rust: about 28,000 non-test lines and 8,000 to 10,000 test lines (section 9).

## 2. Public API to port

The Go API comes in triples: `X`, `XContext(ctx, …)` and `XLimits(ctx, …, lim)`. Rust has
**one** function per operation. It takes `limits: &Limits` (no unlimited entry point,
project rules) and, where the work can run long, `cancel: &AtomicBool`. PDF inputs are `&[u8]`,
because pdf-rs reads bytes (`pdf::api::read(data, &Config, &AtomicBool)`, `src/api.rs:28`), not
an `io.ReadSeeker`. Outputs are `Vec<u8>`. The Go functions that take no `Limits` today still
get `&Limits` in Rust: `QuantizePNG`, `Sauvola`, `EncodeJBIG2*`, `DecodeJBIG2Generic`,
`EmbedJPEG`, `WriteProvenance`, `ReadProvenance` and `ValidatePages`.

Every operation that stamps `ProcessedAt` (Go uses `time.Now()` at `optimize.go:220`,
`provenance.go:478` and `editpages.go:247`) takes `now: Option<pdf::GoTime>`. `None` means the
system clock. Parity always passes `Some`.

### 2.1 Errors and limits (`src/error.rs`, `src/limits.rs`)

| Go | Rust |
|---|---|
| `Limits` (alias of `internal/limits.Limits`), `limits.go:11` | `pub struct Limits` (section 6) |
| `LimitError{Name,Got,Max}`, `ErrLimit` | `Error::Limit { name: &'static str, got: i64, max: i64 }` |
| `ErrMemoryLimit`, `SetMemoryLimit` (`limits.go:21,31`) | dropped; `Error::Alloc` from the per-call `max_alloc` ledger (section 6, open question 3) |
| `ErrPassword` (`limits.go:25`) | `Error::Password` |
| `ErrNotImplemented`, `NotImplemented{Capability,Why,Issue}` (`notimplemented.go:28,56`) | `Error::NotImplemented { capability, why, issue }` |
| `ErrNotSingleRaster`, `ErrUnsupportedImageCodec`, `*RasterRefusal` (`extract.go:37,46,73`) | `Error::NotSingleRaster`, `Error::UnsupportedImageCodec`, `Error::Refused { page, reason, class, source: Box<Error> }` |
| `ErrMirroredOrientation` (`embed.go:315`) | `Error::MirroredOrientation` |
| `ErrUnsupportedJBIG2Feature` (`jbig2.go:115`) | `Error::UnsupportedJbig2Feature(String)` |
| `ErrUnstampableRune` (`stamp.go:61`) | `Error::UnstampableRune(char)` |
| pdf-go errors | `Error::Pdf(pdf::Error)` |
| (new) | `Error::Cancelled`, `Error::Alloc`, `Error::Malformed(String)` |

Go's `errors.Is` sentinels become `Error::is_*` methods (for example `is_limit()` and
`is_not_implemented()`), as pdf-rs does with `is_limit_exceeded`.

### 2.2 Images and bitmaps (`src/image/`, `src/bitmap.rs`)

- `Bitmap{Width,Height,Stride,Pix}`, `NewBitmap`, `At`, `Set`, `Bounds`, `Clone`, `Equal`
  (`bitmap.go:23-75`).
  - Rust: `pub struct Bitmap { pub width: usize, pub height: usize, pub stride: usize, pub pix: Vec<u8> }`
    with `Bitmap::new(w, h, &Limits) -> Result<Bitmap>`, `get`, `set`, `PartialEq` and `Clone`.
  - `internal/jbig2.Bitmap` (`internal/jbig2/bitmap.go:11`) is the same packed MSB-first
    layout. Use one type if the stride and padding rules match. That is unverified; if they
    differ, convert at the boundary as `jbig2.go` does.
- Go's `image.Image` is an interface. Rust has one closed `pub enum Image`, with a variant for
  each concrete type that byblos produces or consumes:
  - `Gray`, `Gray16`, `Rgba`, `Rgba64`, `Nrgba`, `Nrgba64` and `Paletted` (from `image/png`);
  - `YCbCr` and `Cmyk` (from `image/jpeg` via pdf-rs `gojpeg`, and from TIFF);
  - `Gray` and `Rgba` (from jpeg-rs).

  Each variant keeps Go's `Pix`, `Stride` and `Rect`, because sampler fast paths and the draw
  scaler branch on the concrete type.
- `pub fn image::decode(data: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Image>`
  mirrors `image.Decode` as linked into byblos. It sniffs JPEG, PNG, TIFF and WebP, because
  pdf-go registers `hhrutter/tiff` and `x/image/webp`; see section 4.4.

### 2.3 Bitonal and JBIG2 (`src/sauvola.rs`, `src/jbig2/`)

| Go | Rust |
|---|---|
| `Sauvola(img) (*Bitmap, error)`, `sauvola.go:44` | `fn sauvola(img: &Image, limits: &Limits, cancel: &AtomicBool) -> Result<Bitmap>` |
| `EncodeJBIG2Generic(*Bitmap) ([]byte, error)`, `jbig2.go:49` | `fn jbig2::encode_generic(b: &Bitmap, limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>>` |
| `EncodeJBIG2Image(*Bitmap) (EncodedImage, error)`, `jbig2.go:91` | `fn jbig2::encode_image(b: &Bitmap, limits, cancel) -> Result<EncodedImage>` |
| `DecodeJBIG2Generic(data) (*Bitmap, error)`, `jbig2.go:181` | `fn jbig2::decode_generic(data: &[u8], limits, cancel) -> Result<Bitmap>` |
| `CapabilityJBIG2Generic`, `jbig2.go:17` | `pub const CAPABILITY_JBIG2_GENERIC: &str` |

`internal/jbig2`'s `EmbeddedStream`, `DecodeEmbeddedStream[WithGlobals]`,
`PageSize[WithGlobals]` and `MaxPagePixels` become `pub(crate)`. pdfdoc uses them for
`/JBIG2Globals`. `DecodedPixels()` is a test counter and becomes a `#[cfg(test)]` hook.

### 2.4 Image processing (`src/downsample.rs`, `src/quantize.rs`)

- `Downsample`, `DownsampleLimits`, `DownsampleDeclaredBPC` and `DownsampleDeclaredBPCLimits`
  (`downsample.go:32,38,88,100`) become one function:
  `fn downsample(img: &Image, declared_bpc: Option<u8>, src_dpi: f64, dst_dpi: f64, limits, cancel) -> Result<Image>`.
- `QuantizePNG(img, colors) ([]byte, error)` (`quantize.go:22`) becomes
  `fn quantize_png(img: &Image, colors: usize, limits, cancel) -> Result<Vec<u8>>`.
- `QuantizeIndexed(img, colors) (EncodedImage, error)` (`quantize_indexed.go:35`) becomes
  `fn quantize_indexed(img, colors, limits, cancel) -> Result<EncodedImage>`.

### 2.5 Embedding and building (`src/embed.rs`, `src/build.rs`)

- Types:
  - `EncodedImage`, `ColorSpace` and `DecodeParms` (aliases at `build.go:24-26` of
    `internal/pdfdoc/write.go:59,72,84`) become plain `pub struct`s in `src/encoded.rs`. They
    are pdf-free.
  - `Resolution{X,Y}` (`embed.go:43`) becomes `pub struct Resolution { pub x: f64, pub y: f64 }`.
  - `Orientation{Rotate,Ambiguous}` (`embed.go:351`) becomes `pub struct Orientation`.
- `EmbedPNG`/`EmbedPNGLimits` (`:74,:82`), `EmbedJPEG` (`:238`) and `EmbedJP2(data, lim)`
  (`:881`) become `fn embed_png|embed_jpeg|embed_jp2(data: &[u8], limits) -> Result<(EncodedImage, Resolution)>`.
- `EXIFRotation`/`EXIFRotationLimits` (`:334,:343`) and `EXIFOrientation` (`:363`) become
  `fn exif_rotation(data, limits) -> Result<i32>` and
  `fn exif_orientation(data, limits) -> Result<Orientation>`.
- `BuildPage{Image,WidthPt,HeightPt,DPI,DPIY,Rotate}` (`build.go:46`) becomes `pub struct BuildPage`.
- `BuildPDF`, `BuildPDFContext` and `BuildPDFLimits` (`:68,:81,:88`) become
  `fn build_pdf(pages: &[BuildPage], limits, cancel) -> Result<Vec<u8>>`.
- `NewBuildWriter(w, lim)`, `AddPage(ctx, p)` and `Close()` (`:176,:184,:202`) become
  `BuildWriter<W: Write>::new(w, &Limits)`, `.add_page(&BuildPage, cancel)` and
  `.close() -> Result<W>`.
- `TranscodePNG(ctx, r, lim, scratch)` (`:153`) becomes
  `fn transcode_png(r: impl Read, limits, cancel) -> Result<(EncodedImage, Resolution)>`.
  The `scratch` allocator callback is dropped: buffers are charged to `max_alloc` (open
  question 9).

### 2.6 Inspect and extract (`src/inspect.rs`, `src/extract.rs`, `src/stats.rs`)

- Types:
  - `ImageRef`, `PageInfo`, `Severity` (+`SeverityWarning`, `String`), `Diagnostic` and
    `DocumentInfo` (`inspect.go:79-216`) become plain structs. Go's `image.Rectangle` becomes
    `pub struct Rect { x0, y0, x1, y1: i64 }`.
  - `PageRaster{Image,Bounds,Page,RasterQuad}` and `CoversPage()` (`extract.go:235,342`), and
    `MaxSkewDeg` (`extract.go:140`), carry over.
- `Inspect`, `InspectContext` and `InspectDocument(ctx, r, lim)` (`inspect.go:223,238,252`)
  become `fn inspect(pdf: &[u8], limits, cancel) -> Result<DocumentInfo>`. Go's `Inspect`
  returns `DocumentInfo.pages`.
- `ExtractPageRaster`, `…Context` and `…Limits` (`extract.go:408,441,452`) become
  `fn extract_page_raster(pdf: &[u8], page: usize, limits, cancel) -> Result<PageRaster>`.
- `ExtractCounters`, `DivertRate`, `UnhandledRate`, `ExtractStats()` and
  `ResetExtractStats()` (`stats.go:16-85`) become `fn extract_stats() -> ExtractCounters` and
  `fn reset_extract_stats()`. This is process-global, as in Go (open question 4). The divert
  map is a `BTreeMap<String, u64>`.

### 2.7 Writers over an existing PDF

- `TextLayer{Pages}` and `PositionedWord{Text,Bounds}` (`stamp.go:44,52`) carry over.
  `StampTextLayer`, `…Context` and `…Limits` (`:170,:185,:191`) become
  `fn stamp_text_layer(pdf: &[u8], tl: &TextLayer, limits, cancel) -> Result<Vec<u8>>`.
- `OptimizeOptions{Linearize,RecompressJPEG,JPEGQuality}` (`optimize.go:18`) gains
  `now: Option<GoTime>`. `Optimize`, `…Context` and `…Limits` (`:109,:130,:138`) become
  `fn optimize(pdf: &[u8], opts: &OptimizeOptions, limits, cancel) -> Result<Vec<u8>>`.
- `ReplaceImages(w, r, subs map[int]EncodedImage)` and its variants (`substitute.go:86,99,107`)
  become `fn replace_images(pdf: &[u8], subs: &BTreeMap<i64, EncodedImage>, limits, cancel) -> Result<Vec<u8>>`.
- `PageSource{Source,Page,Rotate,Straighten}` and `StraightenSpec{Deg,Crop}` (aliases at
  `editpages.go:34,40`):
  - Go identifies a source by `io.ReadSeeker` identity. Rust uses
    `PageSource { source: usize, page: usize, rotate: i32, straighten: Option<StraightenSpec> }`,
    where `source` indexes a `sources: &[&[u8]]`, so equal indices mean one shared source.
  - `ValidatePages` (`:91`) becomes `fn validate_pages(pages: &[PageSource], limits) -> Result<()>`.
  - `BuildFromPages`, `…Context` and `…Limits` (`:120,:143,:150`) become
    `fn build_from_pages(sources: &[&[u8]], pages: &[PageSource], now: Option<GoTime>, limits, cancel) -> Result<Vec<u8>>`.
- `RenderPage`, `…Context` and `…Limits` (`render.go:41,50,62`) become
  `fn render_page(pdf: &[u8], page: usize, long_edge_px: usize, limits, cancel) -> Result<Image>`.
  This is always `Image::Rgba`; Go returns `*image.RGBA`.

### 2.8 Provenance (`src/provenance.rs`, `src/upgrade.rs`)

- `Version = "0.2.0"` (`provenance.go:19`) becomes `pub const VERSION`.
- `Provenance`, `PageProvenance`, `PageGeometry` (+`CoversPage`) and `PageStraighten`
  (`:24,:149,:253,:270,:301`) become plain structs with hand-written Go-compatible JSON
  (section 4.3).
- `Capabilities() []string` (`:324`) becomes `fn capabilities() -> &'static [&'static str]`.
- `WriteProvenance` and its Context variant (`:352,:366`) become
  `fn write_provenance(pdf, p: &Provenance, limits, cancel) -> Result<Vec<u8>>`.
- `RecordExtraction` and its Context and Limits variants (`:415,:450,:457`) become
  `fn record_extraction(pdf, now: Option<GoTime>, limits, cancel) -> Result<Provenance>`.
- `ReadProvenance` and its Context variant (`:526,:538`) become
  `fn read_provenance(pdf, limits, cancel) -> Result<Option<Provenance>>`.
- `UpgradeCandidates(p, current)` (`upgrade.go:285`) becomes
  `fn upgrade_candidates(p: &Provenance, current: &[&str]) -> Vec<String>`.

## 3. Internal packages

| Package | In/out | Reason |
|---|---|---|
| `internal/limits` | in, as `src/limits.rs` | The caps, `MulSat`, `Check` and `LimitError` are used by every package. The FIFO memory pool (`Reserve`, `SetMemoryLimit`, the cgroup/`GOMEMLIMIT` default) is **replaced** by per-call `max_alloc`, charged through `pdf::Mem` (project rules: no global state for resources). |
| `internal/jbig2` | in, except `profile.go` | The codec behind `EncodeJBIG2*`, `DecodeJBIG2Generic` and pdfdoc's JBIG2 decode. `ProfileStream` has only test and probe callers. |
| `internal/dct` | in | `extract.go` and `pdfdoc.go` size every JPEG decode with it. |
| `internal/content` | in | The lexer and `Walk` are imported by `inspect.go`, `extract.go`, `provenance.go`, `render.go`, `internal/render` and `internal/pdfdoc`. |
| `internal/render` | in | `RenderPage`. It needs a port of `x/image/font/sfnt` (section 4.3). |
| `internal/glyphless` | in (81 lines + `glyphless.ttf` via `include_bytes!`) | `stamp.go:76-83`. `gen.go` is a build-ignored generator; the `.ttf` is a checked-in asset. |
| `internal/linearize` | in | `Optimize{Linearize}` and the editpages path, via `pdfdoc/linearize.go`. It is pure byte writing. |
| `internal/pdfbuild` | in | `BuildPDF`, `BuildWriter`, `TranscodePNG`, `PNGPhys` and `JP2Signature`. It is pure byte writing plus the `EncodedImage` types. |
| `internal/pdfdoc` | in, except `fontcensus.go` and `danglingrefs.go` | The only pdf-go user. `FontDicts` is called only by `cmd/byblos-fonts`; `DanglingRefs` only by tests. |
| `internal/skew` | **out** | Its non-test importers are none. Only `placementdeg_test.go` and `skew_probe_test.go` import it (grep of `byblos/internal/skew"` over all `.go` files). |
| `internal/sample` | out | Used by `cmd/*` and probe tests only. |
| `internal/corpus` | out of the crate | It generates the synthetic PDFs. The Go oracle runs it to make parity inputs (section 5); Rust never needs it. |
| `internal/bench` | out | Scoring for `cmd/byblos-bench`. byblos-rs benches follow project rules (`examples/bench.rs`). |

## 4. Dependencies

### 4.1 dobbo-ca ports (git dependencies pinned by rev)

| Crate | What byblos-rs takes from it | Why |
|---|---|---|
| **pdf-rs** (`pdf`) | <ul><li>All of `internal/pdfdoc`'s pdf-go use: `api::{read, read_and_validate, validate, read_validate_and_optimize, properties, write_context}`, `doc::create::*`, `doc::property::properties_add`, `image::extract::{extract_image, color_space_components}`, `model::{Context, XRefTable, Entry, EntryObject, InheritedPageAttrs}`, `object::*`, `Config`/`Limits`, `Error`/`LimitKind`.</li><li>`Mem`, the shared `max_alloc` ledger, so one call charges byblos and pdf-rs together.</li><li>`GoTime`/`WriteEnv`.</li><li>`filter::goflate::ZlibWriter`: Go's zlib at level 6, byte-exact; needed by `linearize/write.go:315` and `pdfdoc/deterministic.go:349`.</li><li>`filter::gojpeg::{decode, decode_config}`: Go's `image/jpeg` decoder, used by `extract.go:612,769`, `optimize.go:414` and `embed.go:239`.</li></ul> | byblos reaches pdf-go only through pdfdoc. pdf-rs `docs/scope.md` was scoped from exactly these byblos call sites. Section 2 of `needs-from-pdf-rs.md` lists what is still missing. |
| **jpeg-rs** (`jpeg`) | `jpeg::decode` with `Format::{Rgba, Gray}` | `extract.go:759-769` decodes through jpeg-go (`jpegcopy.Decode`, libjpeg-turbo-exact). The `ErrUnsupported` fallback goes to pdf-rs `gojpeg`. Pin the rev pdf-rs pins so that one jpeg crate links. |
| **deflate-rs** (`deflate`) | `zlib::decode*`, `decode*`, `Reader` | Every inflate that byblos does is decode-only and Go-exact: <ul><li>`embed.go:657` (zTXt/iTXt)</li><li>`pdfbuild.go:361,459`</li><li>`png.go:319` (IDAT)</li><li>`pdfdoc.go:917` (`flate.NewReader` + adler32)</li><li>the PNG reader</li></ul> Decode only, so it does not cover encode. |

No other third-party crate. pdf-rs and jpeg-rs already bring `rayon` (`=1.12.0`); byblos-rs uses
their pools through `max_threads` and adds no pool of its own (section 6).

### 4.2 The Go deflate encoder: reuse pdf-rs, extend it

byblos emits Go-`compress/flate` output at three levels. Each one is byte-visible:

| Level | Go site | Visible in | In pdf-rs? |
|---|---|---|---|
| 6 (`zlib.NewWriter`) | `linearize/write.go:315` (hint stream), `pdfdoc/deterministic.go:349` (xref stream body) | every linearized or self-written PDF | **yes**, `pdf::filter::goflate::ZlibWriter` (public; `src/filter/goflate/mod.rs:1-5`) |
| 1 (`BestSpeed`) | `pdfbuild/png.go:326-327` | every `BuildPDF`/`TranscodePNG` Flate image | **no**; Go 1.27 `compress/flate/level1.go` (195 lines) |
| 9 (`BestCompression`, via `png.Encoder`) | `quantize.go:28`, `quantize_indexed.go:42` | `QuantizePNG` bytes; `QuantizeIndexed` IDAT payload | **no**; Go 1.27 `deflate.go` `deflateLazy`/`fillDeflate`/`findMatch` (`deflate.go:129-610`, about 480 lines) |

Recommendation: add levels 1 and 9 to pdf-rs's `goflate`. They share its Huffman bit writer,
token and table code, so porting them a second time in byblos-rs would duplicate about 1,500
lines. The API would be `ZlibWriter::with_level(mem, level)`. This is item 2.1 in
`needs-from-pdf-rs.md`. Until it lands, byblos-rs builds and tests the quantize and transcode
logic, and their byte-parity operations are marked pending in `docs/parity.md`.

### 4.3 Go stdlib and `x/image` replacements

| Go package (what byblos uses) | Where | Rust plan |
|---|---|---|
| `image/png` encoder, `png.Encoder{BestCompression}.Encode(*image.Paletted)` | `quantize.go`, `quantize_indexed.go` | In-crate `src/image/png_write.rs`, about 150 lines. Paletted only: bit depth from palette size, PLTE/tRNS, filter None (Go skips filtering for `cbP1..cbP8`, `image/png/writer.go:528`), IDAT cut on each Go `Write`, over goflate level 9. pdf-rs `image/png.rs` covers only Gray and NRGBA at level 6. |
| `image/png` decoder (`image.Decode`, `image.DecodeConfig`) | `extract.go:612,737,769`, `render.go:122`; decodes the PNG that pdf-rs `extract_image` renders | Adapter in `src/image/png_read.rs` over png-rs (`Profile::Go`), which follows Go 1.27 `image/png/reader.go`; `decode_config` keeps its own Go-style reader (it stops after IHDR, which png-rs does not). The Go image types are built here, because the oracle feeds arbitrary PNG fixtures to Sauvola, quantize and downsample, and the returned type (Gray, Gray16, RGBA, NRGBA, NRGBA64, Paletted, …) steers those code paths. pdf-rs only has a `pub(crate)` test decoder (`src/image/png.rs:409`). |
| `image/jpeg` decoder | as above, plus `optimize.go:414` | pdf-rs `gojpeg` (reuse). |
| `image/jpeg` encoder, `jpeg.Encode(&buf, src, &Options{Quality})` | `optimize.go:419` | Port of Go 1.27 `image/jpeg/writer.go` (641 lines) and the FDCT half of `dct.go`, about 600 Rust lines, in `src/image/jpeg_write.rs`. jpeg-rs's encoder matches libjpeg-turbo `cjpeg`, not Go, so it cannot be used. |
| TIFF decoder (`hhrutter/tiff` registers itself; see 4.4) | `extract.go` default arm, `render.go:122` | In-crate reader for the one layout pdf-rs `image/tiff.rs` writes: little-endian, uncompressed, one strip, 8-bit CMYK or NRGBA. It returns `Image::Cmyk` or `Image::Nrgba`, as `hhrutter/tiff` does. About 200 lines. Anything else gets Go's error class. |
| `x/image/draw`: `draw.CatmullRom.Scale`, `draw.NearestNeighbor.Scale`, `draw.Src` | `downsample.go:152-162` | Port the kernel scaler (`scale.go`, and the `scaleX_*`/`scaleY_*` arms of the generated `impl.go` for the source types in `Image`) with Go's float64 weight tables and rounding. About 700 Rust lines in `src/image/draw.rs`. raster-rs's Catmull-Rom uses Q14 taps (raster-rs `docs/testing.md:13`), not x/image's arithmetic, so it does not fit. |
| `x/image/font/sfnt`: `Parse`, `UnitsPerEm`, `GlyphIndex`, `GlyphAdvance(…, fixed.Int26_6(upem), font.HintingNone)`, `LoadGlyph` → `Segments`, `SegmentOp*`, `Buffer` | `internal/render/text.go:169-418` | Port of `sfnt.go` (2,021), `truetype.go` (578), `postscript.go` (1,426, because `cffWrapSFNT` feeds CFF through it), `cmap.go` (312) and `data.go` (68). `gpos.go` and `gen.go` are not reached. About 2,500 Rust lines in `src/render/sfnt/`. Parse errors decide whether a font is skipped, so they are ported too. |
| `x/image/math/fixed`, `x/image/font` | same | `Int26_6` is an `i32` newtype plus `I()`; `HintingNone` is a constant. |
| `encoding/xml` (`NewDecoder`, `Token`, `StartElement`/`CharData`/`EndElement`, namespace resolution, strict mode) | `embed.go:723-747`, the XMP orientation rule | Port of the reached `Decoder` tokenizer from `xml.go` (about 900 of 2,083 lines), about 600 Rust in `src/go/xml.rs`. Whether a packet "parses completely" changes `Orientation.Ambiguous`, so `quick-xml` would not match. |
| `encoding/json` (`Marshal`/`Unmarshal` of `Provenance`) | `provenance.go:370,551`, `editpages.go:187` | Hand-written in `src/go/json.rs`, about 400 lines. Encoder: field order, `omitempty`, HTML escaping (`<` and friends), U+2028/2029, invalid UTF-8 to U+FFFD, Go float formatting (`'f'` for 1e-6 ≤ abs < 1e21, else `'e'` with the exponent trimmed), `time.Time` as RFC 3339 nano. Decoder: case-insensitive field match, unknown keys ignored, last duplicate wins. serde_json differs on escaping and floats. |
| `math/big` (`Rat.SetFrac64`, `Int.Exp`, `Rat.Float64`) | `embed.go:985` `jp2PerInch` | A tiny bignum in `src/go/bigrat.rs`, about 120 lines: `254*num*10^exp / (10000*den)` rounded to nearest-even f64, with `exp` from 0 to 255. |
| `math.Sin`, `Cos`, `Atan2`, `Hypot` | `extract.go:151,152,1035`, `inspect.go:447`, `content/walk.go:119`, `pdfdoc/straighten.go:124` (written into the content stream), `render/render.go:523-526,1010-1027` | Port Go's pure-Go `sin.go`, `trig_reduce.go`, `atan.go` and `atan2.go` (about 530 Go lines). For `Hypot`, port **amd64's** `hypot_amd64.s` semantics, which is what the oracle runs (`haveArchHypot` is true on amd64). `src/go/math.rs`, about 300 lines. Rust's `f64::sin` calls the platform libm and can differ by an ulp. |
| `sort.Slice` (pdqsort, unstable) | `quantize.go:72,122,297-301` (ties are real: the median-cut axis sort compares one channel), `render/render.go:910,948`, `inspect.go:365`, `pdfdoc/danglingrefs.go` (out) | Port `sort/zsortfunc.go` (479 lines) as `src/go/sort.rs::slice_by`, about 300 lines, and use it at every Go `sort.Slice` site. `slices.Sort`, `sort.Ints` and `sort.Float64s` on totally ordered keys use std `sort_unstable`. |
| `fmt` `%.4f` (`stamp.go:142`), `%.10f` (`pdfdoc/text.go:222`, `straighten.go:69`); `strconv.FormatFloat(v,'f',-1,64)` (`pdfbuild.go:178`) | content streams and PDF numbers | Rust `{:.N}` and `{}` are expected to agree with Go: both use exact decimal conversion, round half to even, and print negative zero as `-0`. This is **unverified**. `src/go/fmt.rs` wraps them, with a unit test of exact ties (0.125, 2.5e-5, -0.0, 1e21). If they disagree, port Go's `strconv/ftoa`. |
| `strconv.ParseFloat` | `content/lexer.go:172,184`, `render/cff.go:180`, `render/type1.go:380` | `str::parse::<f64>`, which is correctly rounded like Go. The longest-valid-prefix fallback (`lexer.go:182`) is a hand-written scanner. |
| `crypto/md5.Sum` | `pdfdoc/deterministic.go:283` (the `/ID`) | pdf-rs has MD5, but it is private (`src/crypto/mod.rs:24 mod md5;`). This is item 2.2 in `needs-from-pdf-rs.md`. |
| `crypto/sha256.Sum256` | `render/text.go:148,155`, the font cache key only | Not ported. Key the cache by `(tag, program bytes)`, which gives the same hits. |
| `hash/crc32`, `hash/adler32`, `encoding/binary`, `encoding/hex`, `unicode/utf8`, `bytes`, `strings`, `slices`, `maps` | many | std, plus a CRC-32 table (about 20 lines) and adler32 (about 10). |
| `unsafe.Sizeof(TextShow/Paint/Placement/Box/string)` | `content/walk.go:392-396` (the retained-bytes cap) | Use **Go's 64-bit sizes as constants**, not `size_of`, so `max_retained_bytes` trips at the same element count as Go. |
| `time.Now`, RFC 3339 | provenance | `pdf::GoTime` plus the `now` parameter (section 2). |
| `context` | everywhere | `&AtomicBool` (section 6). |
| `runtime/debug.SetMemoryLimit(-1)`, the cgroup read | `limits.go` default budget | Dropped (section 6). |

### 4.4 Third-party Go modules reached through pdf-go

byblos imports only `jpeg-go`, `pdf-go` and `x/image` directly (`go.mod`; the import grep of
all non-test files). The other modules are `// indirect`:

| Module | Does byblos reach it? | Owner in Rust |
|---|---|---|
| `github.com/hhrutter/tiff` v1.0.7 | **Yes, in two ways.** <ul><li>Its writer, through `pdfcpu.ExtractImage`. This is the non-DCT CMYK output, `writeImage.go:428,780`.</li><li>Its `init`, which calls `image.RegisterFormat("tiff", …)` (`reader.go:1252-1253`). That makes byblos's own `image.Decode` decode the TIFF that pdfcpu returns. byblos documents and relies on this at `extract.go:626-639`.</li></ul> | Writer: pdf-rs (`src/image/tiff.rs`). Reader: byblos-rs, the minimal reader in 4.3. |
| `golang.org/x/image/webp` | Registered by a blank import in pdf-go `model/image.go:39`. byblos's `image.Decode` therefore also sniffs `RIFF????WEBPVP8`. pdfcpu never produces WebP, so only a non-JPEG DCT payload that happens to start with that magic could reach it. | Not ported. It is listed as a known difference in `docs/go-differences.md` (open question 7). |
| `github.com/xdg-go/stringprep`, `golang.org/x/text` | Only through pdf-go `crypto_password.go`, for the SASLprep of an empty password. | pdf-rs (empty-password path, hard-coded). |
| `github.com/clipperhouse/uax29/v2`, `github.com/mattn/go-runewidth` | No. pdf-go's CLI and config-schema code only. | pdf-rs: not ported. |
| `go.yaml.in/yaml/v3`, `golang.org/x/crypto` | No (pdf-go config file, signing). | pdf-rs: not ported. |

The Go oracle still has to compile all of them. It vendors them under `tests/parity/go/deps/`,
copied from pdf-rs's `tests/parity/go/deps/`. It adds `x/image` `draw`, `font`, `font/sfnt`
and `math/fixed`, and also `jpeg-go`.

## 5. Determinism and parity plan

### 5.1 What is byte-exact against Go

The target is **byte-exact for every operation**. byblos is deterministic by design
(`deterministic_test.go`; `pdfdoc/deterministic.go` pins dates and `/ID` and sorts objects), and
its only wall-clock input is `ProcessedAt`.

| Output | Exactness rests on |
|---|---|
| JBIG2 encode and decode | Integer MQ coder (`mq.go`), segment headers; goldens `testdata/jbig2/*.jb2`. |
| Sauvola bitmap | f64 with explicit casts (`sauvola.go:101-108`). Rust does not fuse multiply-add. |
| Downsample pixels | The x/image/draw port (4.3). |
| `QuantizePNG` / `QuantizeIndexed` | `sort.Slice` port, Lloyd f64, PNG writer, goflate level 9 (4.2). |
| `Embed*`, EXIF/XMP orientation, `TranscodePNG`, `BuildPDF` | Header parsing, the XML port, bigrat, goflate level 1, `formatNum`. |
| Inspect, extract and record-extraction structs | `math` port, content walk, pdf-rs parity. |
| `Optimize`, `StampTextLayer`, `ReplaceImages`, `BuildFromPages`, `WriteProvenance` PDFs | pdf-rs writer parity, goflate level 6, MD5 `/ID`, `%.4f`/`%.10f`, the JPEG encoder port, the JSON encoder, and `now` pinned. |
| `RenderPage` RGBA | `sort.Slice` and `math` ports, the sfnt port, premultiplied-alpha integer maths (`render.go:1160`). This is the riskiest; see open question 5. |

Go-side facts this relies on:

- The oracle runs on amd64 with the default `GOAMD64=v1`. Go does not fuse FMA there, and
  Rust never fuses without fast-math, so `parity-arm64` compares Rust-on-arm64 against
  Go-on-amd64 outputs. Go-on-arm64 would differ: it fuses, and it uses pure-Go `Hypot`
  (`testdata/fused_external.txt` and `nofma_test.go` exist for this).
- `layout.go` iterates maps but sorts each part afterwards (`internal/linearize/layout.go:184-194`).
  `outlineGroup`'s order was not checked.
- `quantize.go:72` sorts the map-derived buckets before use.

### 5.2 Oracle layout (project rules plus the pdf-rs precedent)

```
tests/parity/go/upstream/   byblos non-test sources at 086b88b (root + the internal packages
                            the oracle imports, incl. internal/corpus), go.mod, LICENSE, NOTICE
tests/parity/go/deps/       vendored: pdf-go v0.17.0 (= 113d693: the module zip matches the
                            113d693 tree except README.md and testdata, checked against the
                            module cache), jpeg-go v0.7.0 (trimmed), x/image v0.47.0 subset,
                            and the indirect modules of 4.4 (copied from pdf-rs)
tests/parity/go/patches/    byblos-determinism.patch (adds `var timeNow = time.Now` and uses
                            it at optimize.go:220, provenance.go:478, editpages.go:247);
                            pdf-go determinism.patch copied from pdf-rs (TimeNow, RandReader,
                            map-order fixes; pdf-rs docs/parity.md:222-266)
tests/parity/go/oracle/     main.go: `-gen` writes inputs; default mode writes one output
                            per (input, operation)
tests/parity/go/build.sh    copy, apply both patches, build offline
examples/oracle.rs          the Rust side of every operation
```

Settings for the oracle:

- `timeNow` and `pdfcpu.TimeNow` are pinned to 2000-01-01T00:00:00Z UTC. Rust passes
  `now = Some(that)` and `WriteEnv::parity()`.
- `byblos.SetMemoryLimit(0)` (no global pool), and `GOMEMLIMIT=3GiB` as a backstop.
- It runs twice, and `diff -r` of the two runs must be empty (the pdf-rs run-twice check).

Outputs:

- Each output is `<input>.<op>.out` (bytes) or `<input>.<op>.err` (a short stable class).
- Struct results (inspect, extract metadata, embed, orientation, provenance) are written as
  canonical text, one field per line, with every float as `%016x` of `math.Float64bits`. That
  way the harness cannot hide a last-bit difference.
- Image results are a `type w h stride` header followed by `Pix`.

Error classes are listed one-to-one with Rust variants in `docs/parity.md`. Examples:

- `limit/<name>`, `refused/<reason>`, `not-single-raster`, `unsupported-codec`
- `not-implemented/<capability>`, `password`, `mirrored-orientation`, `jbig2/unsupported`
- `unstampable-rune`, `pdf/<pdf-rs class>`

### 5.3 Inputs

- **Copied fixtures (`tests/data/`)**:
  - `testdata/img2pdf/` (83 files, 604 KB, plus `oracle.json`)
  - `testdata/jbig2/` (11 `.jb2` and `symbol-page.pbm`)
  - `testdata/straighten/` (`contract.json`, `contract.pdf`)
  - `testdata/oracle/poppler.json`
  - `cmd/byblos-encrypt-census/testdata/*.pdf` (2 encrypted PDFs)
  - the Liberation TTFs and `glyphless.ttf`, which ship in `src/` via `include_bytes!` anyway
- **Generated by `oracle -gen` (not committed)**:
  - the `internal/corpus` PDFs, which is what `make corpus` writes;
  - one `BuildPDF` per img2pdf fixture;
  - Sauvola bitmaps (PBM) of the grey fixtures, as JBIG2-encode inputs;
  - content streams pulled from the corpus pages, for the lexer and walk.
- **Mutations by `-gen`**: deterministic truncations, bit flips, and length-field edits:
  - PNG chunk lengths
  - JBIG2 segment data lengths
  - JP2 box lengths
  - EXIF IFD counts
  - PDF `/Length` and xref offsets
- **Real PDFs**: a subset of pdf-rs `tests/data/*.pdf` (born-digital and scanned, for
  inspect, extract and render).
- **Limit presets**: every operation also runs under one tight `Limits` preset, for example
  `max_images: 2`, `max_encoded_image_pixels: 1 << 16`, `max_raster_pixels: 1 << 14` and
  `max_form_depth: 1`. That puts the limit errors in the `.err` outputs.

### 5.4 Operations the oracle runs

| Input kind | Operations (`op` token) |
|---|---|
| PDF | <ul><li>`inspect`</li><li>`extract` (every page; raster pixels or error)</li><li>`record` (`RecordExtraction`)</li><li>`render` (page 1, `longEdgePx` 400)</li><li>`optimize`, `optimize-lin`, `optimize-jpeg75`, `optimize-lin-jpeg75`</li><li>`stamp` (a fixed synthetic `TextLayer`, one word per page)</li><li>`replace` (first substitutable image swapped for a fixed 8x8 Flate gray image)</li><li>`edit` (pages reversed, page 1 rotated 90, page 2 straightened 1.5°)</li><li>`provenance-write` then `provenance-read`</li></ul> |
| PNG/JPEG/JP2 | <ul><li>`embed`</li><li>`orientation`</li><li>`transcode` (PNG)</li><li>`build` (embed then `BuildPDF`)</li><li>`decode` (`image.Decode` → pixels)</li><li>`sauvola`</li><li>`downsample-150`, `downsample-bpc1`</li><li>`quantize-png-2`, `-16`, `-256`</li><li>`quantize-indexed-16`</li></ul> |
| Bitmap (PBM) | `jbig2-encode`, `jbig2-image` |
| JBIG2 stream | `jbig2-decode` (with `symbol-globals` paired as globals) |
| Content stream | `lex` (token dump), `walk` (`Scan` dump under a fixed mock `Env`) |

`docs/parity.md` counts the Go test cases against the ported and generated cases, as the
project rules require.

### 5.5 Go tests: what ports

Ported to `cargo test`:

- Every in-memory unit, golden and limit test, including the jbig2 goldens, `hints_test.go`'s
  hex vectors, `pdfbuild_test.go`, the content lexer/walk/caps tests, `limits_test.go` (minus
  the pool), the render and caps tests, and `glyphless_test.go` minus its poppler test.
- `img2pdf/oracle.json` and `poppler.json`, as committed fixtures.

Dropped:

- `nofma_test.go`, `arch_test.go`, `*_arch_test.go` and `capability_register_test.go`, which
  inspect Go source.
- The `-race` test.
- The probe tests (`*_probe_test.go`).

External-tool tests (`gs`, `pngquant`, `jbig2`, `pdftoppm`, `pdfimages`, `qpdf`) become
`#[ignore]` integration tests that skip when the tool is missing. The CI image does not install
them, because the parity job covers the same ground against Go.

## 6. Limits, threading, cancellation

### 6.1 Go today

- **Caps**: `internal/limits.Limits`. A value of ≤ 0 means the default, and `def()` applies it.
- **Memory**: a process-global FIFO byte pool.
  - `Reserve(ctx, n)` blocks until the bytes are free or `ctx` ends
    (`internal/limits/limits.go`, `SetMemoryLimit` and `Reserve`).
  - The default budget is half of `GOMEMLIMIT`, else half of the cgroup limit, else none.
  - `n` above the budget is `LimitError{"memory-budget"}`.
- **Threads**: byblos runs no goroutines in the in-scope packages. The only worker pool is
  `internal/sample` (out of scope).
- **Cancellation**: ctx checks at these points:
  - per page and per image in inspect (`inspect.go:253,274,291`);
  - at extract entry (`extract.go:453-465`);
  - once in downsample (`downsample.go:101`);
  - per token in the content walk (`walk.go:644`);
  - per operator in render (`render.go:546`);
  - per row in `TranscodePNG` (`png.go:334`);
  - inside `Reserve`.

### 6.2 Rust `Limits` (`src/limits.rs`)

Public fields with explicit defaults. A field set to 0 takes the default, keeping Go's rule (as
pdf-rs does).

| Field | Default | Go name, error name |
|---|---|---|
| `max_objects` | 200,000 | `MaxObjects` → pdf `max_object_count`; `objects` |
| `max_pages` | 10,000 | `MaxPages`; `pages` |
| `max_stream_bytes` | 64 MiB | `MaxStreamBytes`; `stream-bytes` |
| `max_page_stream_bytes` | 128 MiB | `MaxPageStreamBytes` |
| `max_image_pixels` | 500,000,000 | `MaxImagePixels`; `image-pixels` |
| `max_font_program_bytes` | 16 MiB | `MaxFontProgramBytes` |
| `max_images`, `max_inline_images`, `max_unresolved` | `max_page_stream_bytes / 512` (262,144) | `images` |
| `max_object_stream_entries` | `min(own or max_objects, max_objects)` | → pdf `max_object_stream_total_count` |
| `max_total_decode_bytes`, `max_object_bytes` | `max_stream_bytes` | → pdf `max_total_decode_bytes`, `max_object_bytes` |
| `max_parsed_bytes`, `max_stream_total_bytes` | 0 = off | → pdf, same names |
| `max_retained_bytes` | 128 MiB | content `Scan` retention |
| `max_encoded_image_pixels` | 1<<26 | `encoded-image-pixels` |
| `max_png_text_bytes` | 64 MiB | `png-text-bytes` |
| `max_raster_side`, `max_raster_pixels` | 20,000; 1<<26 | `raster-side`, `raster-pixels` |
| `max_render_font_bytes` | 16 MiB | render font budget |
| `max_path_points`, `max_fill_work` | 1<<21; 1<<28 | `path-points`, `fill-work` |
| `max_gstate_depth`, `max_form_depth`, `max_form_invocations` | 1024; 8; 4096 | `q-depth`, `form-depth`, `form-invocations` |
| `max_operands`, `max_text_shows`, `max_paints` | 8192; 65,536; 262,144 | |
| `max_alloc` | 4 GiB, as pdf-rs | Peak bytes of the call, byblos plus pdf-rs plus jpeg-rs plus deflate-rs, through one `pdf::Mem`. Over it: `Error::Alloc`. This replaces Go's pool and `ErrMemoryLimit`. |
| `max_threads` | `available_parallelism()` | Passed to pdf-rs and jpeg-rs. 1 means no spawn. |

Fixed constants stay constants, as in Go:

- jbig2: `MaxPagePixels` 1<<26, `maxStreamBitmapBytes` 16 MiB, overdraw 4×page + 65,536,
  1<<16 segments, symbols 1<<16, instances 1<<21 (`segment_decode.go:397-402`,
  `budget_symbol.go:87,101`).
- render: `maxCanvasPixels` 1<<32.

The pdf-go configuration that `pdfdoc.defaultConfig`/`openSized` writes
(`pdfdoc.go:81-93, 447-456, 1417-1420`) maps one-to-one onto `pdf::Limits`.

Where Go's `Reserve` charged an estimate up front (`reserveDCT`'s `4*w*h + len(data)`,
`dct.Need()`, `TranscodePNG`'s row reservations), Rust charges the real allocations to
`max_alloc`, as the project rules require. These estimates stay as **refusal checks** only where
Go refuses with a `LimitError`. Go's blocking wait has no Rust equivalent. It is documented in
`docs/go-differences.md`.

### 6.3 Threading and cancellation

- byblos-rs is serial in its own code in phase 1. Parallel work happens inside pdf-rs and
  jpeg-rs, under `max_threads`.
- Candidates once the bench shows a need, each byte-identical to serial:
  - Sauvola threshold pass, by row bands. The integral image stays serial.
  - Downsample, by output rows. x/image scales rows independently.
  - Render, by scanline bands. The edge list is shared and read-only.
  - JBIG2 encode can only split by stripe; one MQ stream is serial.
  - Lloyd assignment, only if the sums stay in Go's order (integer counts, f64 sums
    accumulated in index order).
- Cancellation (`&AtomicBool`) is checked at every Go ctx point above. It is also checked per
  row in Sauvola, downsample, quantize, JBIG2 encode/decode and the PNG/JPEG codecs, which Go
  does not do and which does not change output.

## 7. Fuzz targets (`tests/fuzz`, one per parser or input entry)

Each runs with `-rss_limit_mb=2048 -malloc_limit_mb=512`.

| Target | Entry | Seeds |
|---|---|---|
| `content_lexer` | `content::Lexer::next` loop | Go `FuzzLexer` (`internal/content/lexer_test.go:400`) seeds, corpus content streams |
| `content_walk` | `content::walk` with a fixed mock `Env` | corpus content streams |
| `jbig2_decode` | `jbig2::decode_generic` | `testdata/jbig2/*.jb2` |
| `jbig2_decode_globals` | `decode_embedded_stream_with_globals(split input)` | `symbol-globals.jb2` + `symbol-page.jb2` |
| `jbig2_roundtrip` | bytes → `Bitmap` → `encode_generic` → `decode_generic`, which must be equal | random bitmaps |
| `embed_png` | `embed_png`, `exif_orientation` | `testdata/img2pdf/*.png` |
| `embed_jpeg` | `embed_jpeg`, `exif_orientation` (EXIF, JFIF, XMP) | `testdata/img2pdf/*.jpg`; Go `FuzzEXIFRotation` |
| `embed_jp2` | `embed_jp2` | `testdata/img2pdf/*.jp2`; Go `FuzzEmbedJP2` |
| `transcode_png` | `transcode_png` | Go `FuzzPNGTranscode` seeds, PNG fixtures |
| `build_pdf` | `build_pdf` from fuzzed `(filter, cs, bpc, w, h, rot, data)` | Go `FuzzBuildPDF` |
| `image_decode` | `image::decode` (PNG reader, TIFF reader, gojpeg) | all image fixtures |
| `xmp` | `go::xml` tokenizer through the XMP rule | XMP packets from fixtures |
| `provenance_json` | the JSON decoder behind `read_provenance` | recorded provenance JSON |
| `pixels` | fuzzed dims and pixels → `sauvola`, `downsample`, `quantize_png` | — |
| `dct_parse` | `dct::parse`, `Frame::need` | JPEG fixtures |
| `sfnt` | `sfnt::parse` + `load_glyph` over all glyphs | Liberation TTFs, `glyphless.ttf` |
| `cff`, `type1` | render's CFF and Type 1 parsers and charstring interpreters | programs extracted from corpus PDFs |
| `inspect`, `extract`, `render`, `optimize` | the PDF entry points | corpus PDFs, pdf-rs PDFs |

There are no SIMD kernels in the plan, so there are no differential tier targets. Add them with
any kernel the bench forces.

## 8. Bench classes

`examples/bench.rs`, with Go's matching benchmark in the same `bench.yml` job (project rules).
Go runs with its default parallelism; byblos itself is serial. Rust runs with the default
`max_threads`, and both also run single-threaded.

| Class | Input |
|---|---|
| `sauvola` | 300 dpi Letter grey and RGB scans |
| `jbig2-encode` | Sauvola bitmap of a text page, 2550×3300 |
| `jbig2-decode` | generic and symbol-mode streams |
| `downsample` | 300 → 150 dpi grey and RGB; 1-bit nearest neighbour |
| `quantize` | RGB page to 16 and 256 colours, PNG and indexed |
| `embed-build` | JPEG and PNG fixtures to `BuildPDF`; `TranscodePNG` (16-bit, interlaced) |
| `inspect` | multi-page corpus and real PDFs |
| `extract` | DCT, Flate and JBIG2 single-raster pages |
| `render` | page 1 thumbnail at 400 px of born-digital PDFs (TrueType, CFF and Type 1 fonts) |
| `optimize` | rewrite, linearize, JPEG recompress at q75 |
| `stamp` | 10-page document, about 300 words per page |

## 9. Port order

Each wave lands as PRs that keep `test` green and adds its parity operations as soon as both
sides exist. Rust line estimates are for non-test code; ported tests come with each wave.

### Wave 1: everything that does not need the pdf-rs PDF API (scaffold, CI, oracle)

Wave 1 uses pdf-rs only as a library of Go-stdlib ports: `Mem`, `GoTime`, `goflate`, `gojpeg`.
It touches no `Context`, `XRefTable` or object model.

- **1a. Scaffold and foundations (~2,000 lines, plus ~600 Go in the oracle).**
  - Repo layout, CI, Dockerfile, fuzz and bench workflows, copied from pdf-rs.
  - LICENSE and NOTICE, with the Go BSD, x/image BSD, Liberation OFL and `hhrutter/tiff`
    notices.
  - `tests/parity/go` (upstream snapshot, deps, both patches, oracle skeleton) and
    `examples/oracle.rs`.
  - `src/error.rs`.
  - `src/limits.rs`, from `internal/limits/limits.go` without the pool, plus root `limits.go`,
    `context.go` and `notimplemented.go`.
  - `src/stats.rs` (`stats.go`) and `src/bitmap.rs` (`bitmap.go`).
  - `src/go/{sort, math, fmt}.rs` and `src/image/mod.rs` (the `Image` enum).
- **1b. Bitonal and JBIG2 (~3,600).**
  - `internal/jbig2/*` except `profile.go`, that is:
    - `bitmap.go`, `mq.go`, `mq_decode.go`, `arith_int.go`
    - `generic.go`, `generic_decode.go`, `generic_general.go`
    - `segment.go`, `segment_decode.go`
    - `symbol_header.go`, `symbol_decode.go`, `text_decode.go`, `budget_symbol.go`
  - `jbig2.go`, `sauvola.go`, `internal/dct/dct.go`.
- **1c. Image codecs and processing (~3,500).**
  - `downsample.go` plus the `x/image/draw` port.
  - `quantize.go`, `quantize_indexed.go`, and the paletted PNG writer. Byte parity waits for
    goflate level 9.
  - The `image/png` reader, the minimal TIFF reader, and `image::decode`.
  - The `image/jpeg` writer port, used in wave 3 but pdf-free.
- **1d. Byte-level PDF writers and image embedding (~4,000).**
  - The types from `internal/pdfdoc/write.go`: `EncodedImage`, `ColorSpace`, `DecodeParms`.
  - `internal/pdfbuild/{pdfbuild,png,pngres}.go`. `TranscodePNG` byte parity waits for
    goflate level 1.
  - `internal/linearize/{hints,layout,write}.go`.
  - `build.go`.
  - `embed.go`, with `src/go/xml.rs` and `src/go/bigrat.rs`.
  - `upgrade.go`.
  - The types and JSON half of `provenance.go`, with `src/go/json.rs`.
  - `internal/glyphless/glyphless.go`, with `glyphless.ttf`.
- **1e. Content and render engine (~7,000).**
  - `internal/content/{lexer,walk}.go`, with the `Env` trait.
  - `internal/render/{render,text,cff,type1,glyf,substitute}.go`, with the Liberation fonts
    and the `x/image/font/sfnt` port. `render::page` takes the content bytes plus
    `ImageFor`/`FontFor` closures, so it needs no PDF parser.

### Wave 2: `internal/pdfdoc` on pdf-rs (~5,000)

- **Files**: `pdfdoc.go`, `limits.go`, `buildpages.go`, `linearize.go`, `text.go`,
  `deterministic.go`, `selfwrite.go`, the rest of `write.go`, `annots.go`, `straighten.go`,
  `properties.go`, `encryptinfo.go`, `optimize.go` and `catalog.go`.
- **Page tree**: keep `normalizePageTree` (`pdfdoc.go:490-540`). pdf-rs keeps pdf-go's
  `PageDict` quirk (pdf-rs `docs/scope.md` §4.3).
- **Dropped audit**: drop `auditSerializable`, because pdf-rs's exhaustive `Object` enum
  cannot lose a type.
- **Untyped xref error**: byblos matches the untyped xref-range error as text in Go
  (`pdfdoc.go:463`). Rust matches `pdf::Error::XRefObjectRange` (`src/error.rs:136`).
- **Parity operations**: inspect-level open/page/image reads through a thin test op, until
  wave 3 lands.

### Wave 3: root PDF operations (~3,300)

- `inspect.go`
- `extract.go`, with the jpeg-rs decode and the gojpeg fallback
- `render.go`, which wires pdfdoc into `render::page`
- `stamp.go`
- `optimize.go`
- `substitute.go`
- `editpages.go`
- the PDF half of `provenance.go`: `WriteProvenance`, `RecordExtraction`, `ReadProvenance`

All PDF parity operations of section 5.4 go live here.

### Wave 4: done criteria (round C)

- The full ported test suite.
- Parity non-empty, with an empty diff on amd64 and arm64.
- All fuzz targets, with one 900 s run on `main`.
- The bench at or below 1.10× Go on every class, with threading added where needed.
- Docs: README "Resource limits"; `limits.md`, `parity.md`, `go-differences.md`,
  `fuzzing.md`, `benchmarks.md` and `dependencies.md`.
- The adversarial verify.

Total: about 28,400 Rust non-test lines. Separately, pdf-rs needs about 700 lines for goflate
levels 1 and 9.

## 10. Out of scope

| Item | Reason |
|---|---|
| `cmd/byblos-annots`, `-bench`, `-corpus`, `-ctm-census`, `-divert`, `-encrypt-census`, `-fonts` | Research and census tools, not library API. Only `-encrypt-census`'s two test PDFs are reused, as fixtures. |
| `internal/skew` | Test-only importers (section 3). `StraightenSpec` takes an angle the caller measured. |
| `internal/sample`, `internal/bench`, `tools/*`, `testdata/oracle/gen.go` | Corpus walking, bench scoring and poppler golden generation. The committed `poppler.json` is reused. |
| `internal/corpus` | Runs only in the Go oracle (`-gen`). |
| `pdfdoc/fontcensus.go` (`FontDicts`, the only `api.ReadContextFile` user) | Its only caller is `cmd/byblos-fonts/main.go:159`. |
| `pdfdoc/danglingrefs.go`, `jbig2/profile.go`, `jbig2.DecodedPixels` | Test and probe helpers. They are ported with their tests only if a ported test needs them. |
| `glyphless/gen.go` | The `.ttf` is embedded as-is. |
| `SetMemoryLimit`, the blocking pool, `ErrMemoryLimit` | Replaced by `max_alloc` (section 6). |
| WebP decode via `image.Decode` | pdfcpu never produces it (section 4.4, open question 7). |
| Go-source architecture tests, `nofma_test.go`, the race test, the probe tests | Go-specific (section 5.5). |
| Everything in FUTURE.md (JBIG2 symbol encoder, CCITT encoder, JPX/TIFF decode, archival renderer, PDF/A) | Not built in Go either. |

## 11. Open questions

1. **Go flate levels 1 and 9.** Recommended: add them to pdf-rs `goflate` (`needs-from-pdf-rs.md` 2.1).
   The alternative is to port them into byblos-rs and duplicate the shared bit writer. Until
   this is settled, the byte parity of quantize and transcode is blocked.
2. **Determinism patch to the Go upstream.** `ProcessedAt` comes from `time.Now()` and lands
   in output PDFs. Recommended: a three-line `timeNow` patch applied at build time, as pdf-rs
   does for pdf-go, with `now: Option<GoTime>` on the Rust API. This needs owner sign-off,
   because the upstream snapshot is kept verbatim.
3. **Global memory pool replaced by per-call `max_alloc`.** Go callers (kleio) can share one
   budget across concurrent calls and block until bytes free up. Rust fails fast per call
   instead. Does any caller depend on the cross-call pool?
4. **`ExtractStats` counters.** These are process-global in Go. Recommended: keep them as a
   process-global `Mutex<ExtractCounters>`. It is telemetry, not a resource. The alternative is
   a per-call return.
5. **Render pixel-exactness.** The target is bit-exact. The risks are the sfnt port, tie order
   in the sorts (handled by the `sort.Slice` port), Go amd64's assembly `Hypot`, and long float
   chains. If exactness is not reached, the fallback is a stated per-pixel tolerance, and that
   needs owner approval.
6. **jpeg-go pin.** byblos pins jpeg-go v0.7.0. jpeg-rs ports the next commit, which adds
   `RowReader` and refactors `internal/codec/progressive.go`. The oracle vendors v0.7.0, and
   parity will show any decode difference.
7. **WebP sniffing.** `image.Decode` in Go byblos would decode a WebP payload, through x/image's
   registration via pdf-go. Recommended: a documented difference rather than porting a WebP
   decoder. webp-rs exists if the owner wants it.
8. **Corpus fixtures for `cargo test`.** Ported Go tests that call `internal/corpus` need its
   PDFs. Either commit the `-gen` output under `tests/data/corpus/` or port the generator
   (2,382 lines). The size of the generated corpus was not measured.
9. **`TranscodePNG`'s `scratch` callback.** Recommended: drop it, because the ledger covers
   it. Keep it only if a caller supplies pooled buffers.
10. **pdf-rs JPEG gaps.** pdf-rs `docs/go-differences.md:63` says CMYK DCT ends in
    `jpeg/unsupported`. But `src/filter/gojpeg.rs:945` calls
    `decode_coefficients_any_components`, which exists at the pinned jpeg-rs rev
    (`src/coef.rs:73`), so the doc looks stale. Go's "flex" sampling is still marked as a gap
    (`gojpeg.rs:13`). Both affect byblos extract and optimize on such JPEGs; confirm with
    pdf-rs.

### Not checked

- Test bodies, beyond the headers named above.
- Which Go tests assert exact bytes and which assert structure.
- The full method set of pdfdoc's `Doc` interface (`pdfdoc.go:264`).
- `outlineGroup`'s order in `layout.go`.
- Whether Rust's `{:.N}` rounding equals Go's `%.Nf` on exact ties.
- Whether the root `Bitmap` and `jbig2.Bitmap` layouts are identical.
- The exact `x/image/draw` `impl.go` arms reached by each source type.
- The size of the generated corpus.
- Whether the pdf-go `Float` printing in pdf-rs `pdf_string` covers every number byblos
  writes through pdf-go.

### Decisions taken for the port (2026-11-17)

These take the recommended default for each open question above, so the port is not blocked.
The owner can reverse any of them.

1. Flate levels 1 and 9: requested from pdf-rs (`needs-from-pdf-rs.md` 2.1). byblos-rs does not
   port a second copy. The parity ops that need them stay out of `tests/parity/ops.txt` until
   the pdf-rs pin has them.
2. The oracle applies a build-time `timeNow` patch to a copy of the upstream snapshot, as pdf-rs
   does for pdf-go. The snapshot in the repository stays verbatim.
3. Per-call `max_alloc` replaces the global pool, as the project rules require. The difference is
   in `docs/go-differences.md`.
4. `ExtractStats` stays process-global (atomic counters, no growth).
5. Render aims for bit-exact output. A tolerance is used only if the owner approves it.
6. Parity shows any jpeg-go pin difference.
7. WebP sniffing in `image::decode` is a documented difference.
8. The Go oracle generates the corpus (`-gen`). The `cargo test` ports that need corpus PDFs
   read a small committed subset in `tests/data/corpus/`.
9. The `TranscodePNG` scratch callback is dropped.
10. Asked of pdf-rs in `needs-from-pdf-rs.md` 2.3.
