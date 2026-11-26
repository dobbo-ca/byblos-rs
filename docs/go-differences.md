# Differences from Go

Where the Rust port differs from byblos 086b88b on purpose. The decisions are in docs/scope.md,
"Decisions taken for the port (2026-11-17)"; the owner can reverse any of them. Output
(PDF bytes, pixels, struct dumps, error classes) is meant to be identical except where a
difference below says otherwise.

## Decided

- **The global memory pool becomes a per-call `max_alloc`.** Go's `SetMemoryLimit`, `Reserve`,
  `ErrMemoryLimit` and the `GOMEMLIMIT`/cgroup default are gone (no global state for
  resources). Each call has its own ledger (`Limits::mem()`), charged by byblos and by the nested
  pdf-rs, jpeg-rs and deflate-rs together, and fails fast with `Error::Alloc` instead of waiting
  for other calls to release bytes. A caller that shared one pool across concurrent calls (kleio)
  must split its budget per call. Go's up-front estimates (`reserveDCT`'s `4*w*h + len(data)`,
  `dct.Need()`, `TranscodePNG`'s row reservations) are no longer reservations: the real
  allocations are charged, and the estimates stay only where Go refuses with a `LimitError`.
- **WebP is not decoded.** Go's `image.Decode` in byblos also sniffs `RIFF....WEBPVP8`, because
  pdf-go registers `x/image/webp`. pdfcpu never produces WebP, so only a non-JPEG DCT payload
  that happens to start with that magic could reach it. `image::decode` returns an unknown-format
  error for it.
- **The TIFF reader is minimal.** `image::tiff` decodes only 8-bit, uncompressed, strip-based
  gray, RGB, RGBA, NRGBA and CMYK, which is all pdf-rs writes. Tiles, LZW, Deflate, PackBits,
  CCITT and JPEG compression, predictors, 1- and 16-bit samples, palettes (only `decode_config`
  returns `Paletted`) and YCbCr return `Error::Image("tiff: unsupported feature: ...")` where
  Go returns an image, or an error with its own class (for example `pdf/lzw/invalid-code`).
- **PNG: trailing bytes after the zlib stream of an IDAT chunk.** Go reads the IDAT through two
  `bufio.Reader`s and `io.ReadFull`, which reads 4096 bytes or more straight from the decoder;
  the port copies only the inner 4096-byte reader. Where the IDAT ends, and so the "too much
  pixel data" decision, can differ when trailing IDAT bytes follow a zlib stream that ends in a
  stored block, or straddle a 4096-byte boundary of the outer reader.
- **No `scratch` callback in `transcode_png`.** The buffers are charged to `max_alloc`.
- **`Limits` fields are plain public fields on one struct**, with `max_alloc` and `max_threads`
  added; Go's `X`, `XContext` and `XLimits` are one function taking `&Limits` and a cancel flag.
  Functions that took no limits in Go (`Sauvola`, `QuantizePNG`, `EncodeJBIG2*`, ...) take them
  too.
- **Cancellation is an `&AtomicBool`**, checked at every point where Go checks `ctx`, and also per
  row or block in Sauvola, downsample, quantize, JBIG2 and the image codecs, which Go does not
  do; that does not change output.
- **`ExtractStats` stays process-global**, behind one mutex. `fn extract_stats()` returns a
  snapshot whose `reasons` is a `BTreeMap`.
- **`ProcessedAt` takes a clock argument.** The operations that stamp it (`optimize`,
  `record_extraction`, `build_from_pages`) take `now: Option<pdf::GoTime>`; `None` is the system
  clock. The Go oracle applies a build-time `timeNow` patch to a copy of the upstream snapshot
  and pins it to 2000-01-01T00:00:00Z; the snapshot in the repository stays verbatim.
- **PDF inputs are `&[u8]`**, outputs `Vec<u8>`, instead of `io.ReadSeeker` and `io.Writer`.
  `build_from_pages` identifies a source by an index into `sources: &[&[u8]]`, where Go uses the
  reader's identity.
- **`image.Image` is a closed enum** (`byblos::Image`) of the concrete types byblos produces or
  consumes (Gray, Gray16, RGBA, RGBA64, NRGBA, NRGBA64, CMYK, Paletted, YCbCr). `At` outside the
  bounds returns the zero colour of the type, as Go does; a `Paletted` index past its palette
  reads as transparent black where Go panics.
- **Errors are one enum.** `errors.Is` sentinels are `Error::is_*` methods; `errors.As` on
  `*RasterRefusal` is `Error::Refused { page, reason, class, source }`.
- **Go's `sort.Slice` is ported** (`src/go/sort.rs`), as are `math.Sin`, `Cos`, `Atan2` and the
  amd64 `Hypot` (`src/go/math.rs`), because ties and last-bit differences are visible in the
  output. The oracle runs on amd64 with `GOAMD64=v1`; Go on arm64 fuses multiply-adds and would
  differ.
- **Flate levels 1 and 9** (`quantize_png`, `quantize_indexed`, `transcode_png`) are pdf-rs's
  Go-exact writer since pdf-rs `b2d3a4d`; their parity ops are enabled and match.

- **Object numbers of `BuildFromPages`.** Go copies page dictionaries in map iteration order
  (`internal/pdfdoc/buildpages.go:559,796`), so its object numbering is random per run. Rust
  copies in sorted key order, which is what the parity oracle's patched Go does.

## Not checked yet

- Whether Rust's `{:.N}` matches Go's `%.Nf` on every exact tie. `src/go/fmt.rs` pins the cases
  that were worked out; if the parity run finds one more, Go's `strconv/ftoa` gets ported.

## pdfdoc `add_font_resource` on a page with inherited `/Resources`

Go writes the new `BbGl<i>` font into the ancestor's stored `/Resources /Font` (shared map), so
the ancestor and every inheriting page see it and later pages get `BbGl1`, `BbGl2`. The Rust port
binds in the ancestor too (`InheritedPageAttrs::resources_owner`) and gives each such page a copy
of the ancestor's final `/Resources` when the document is written, which is what Go serialises.
Go holds one map; the copies are N maps, each charged to `max_alloc` at its deep size for as long
as the document holds them, so N inheriting pages that each add a name (N^2 entries) can fail
with `limit/alloc` where Go succeeds.

