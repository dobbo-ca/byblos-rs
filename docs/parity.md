# Parity with byblos (Go)

The parity job runs byblos at 086b88b (a vendored snapshot plus two determinism patches) and
byblos-rs on the same inputs, and compares the output files of the operations that Rust has
ported. This page is the spec of the Go side: `examples/oracle` (Rust) writes the same files.

```
tests/parity/go/upstream/   byblos non-test sources at 086b88b: the root package and
                            internal/{content,corpus,dct,glyphless,jbig2,limits,linearize,
                            pdfbuild,pdfdoc,render}, go.mod, go.sum, LICENSE, NOTICE. Verbatim
tests/parity/go/deps/       vendored modules, each with its licence (see Vendoring)
tests/parity/go/patches/    byblos-determinism.patch, pdf-go-determinism.patch
tests/parity/go/oracle/     main.go errclass.go format.go ops.go ops_content.go gen.go, go.mod
tests/parity/go/build.sh    copies, patches, builds offline
tests/parity/ops.txt        the operations that are compared (one op token per line, # comments)
tests/parity/diff.sh        compares the listed operations only
tests/data/                 fixtures (README.md there)
examples/oracle/            the Rust side: main.rs and image.rs jbig2.rs content.rs render.rs
                            build.rs pdf.rs, one `run(input_name, data, threads, out)` per area
```

## Running it

```
sh tests/parity/go/build.sh /out/oracle
/out/oracle -gen tests/data /inputs
/out/oracle /inputs /go-out
/out/oracle /inputs /go-out-2          # run-twice check: diff -r /go-out /go-out-2 must be empty
cargo run --locked --release --example oracle -- -threads 1 /inputs /rust-out
sh tests/parity/diff.sh /go-out /rust-out
```

Run the Go oracle with `GOMEMLIMIT=3GiB` as a backstop. `-threads N` is accepted and ignored by
the Go oracle. The Go oracle runs the inputs one after the other and bounds its own work (below).

## Vendoring

Nothing is fetched at build time and no token is needed. `oracle/go.mod` requires every module
and `replace`s it with a directory:

| Module | Directory | Source |
|---|---|---|
| `github.com/dobbo-ca/byblos` | `upstream/` | the snapshot (patched at build time) |
| `github.com/dobbo-ca/pdf-go` v0.17.0 | `deps/pdf-go/` | pdf-rs `tests/parity/go/upstream` at `9bdaeb5`: the pdf-go 113d693 non-test sources with a trimmed `go.mod` (identical to the v0.17.0 module cache except `go.mod`) |
| `github.com/dobbo-ca/jpeg-go` v0.7.0 | `deps/jpeg-go/` | module cache: `jpeg.go`, `internal/**` (including the amd64 and arm64 assembly), LICENSE, `go.mod`; no tests, `cmd`, bench, docs. Trimmed to the libjpeg-turbo-exact codec: the alternative IDCT and colour paths byblos never calls are removed (byblos calls only `Decode`) |
| `golang.org/x/image` v0.47.0 | `deps/x-image/` | pdf-rs's (ccitt, riff, vp8, vp8l, webp) plus `draw`, `font` (font.go only), `font/sfnt`, `math/f64`, `math/fixed` from the module cache |
| `github.com/hhrutter/tiff`, `x/text`, `x/crypto`, `uax29`, `go-runewidth`, `stringprep`, `yaml-v3` | `deps/<name>/` | pdf-rs's copies (the indirect modules of byblos's `go.mod`) |

The oracle module is `github.com/dobbo-ca/byblos/oracle`, under the byblos path, because Go's
`internal` rule is on the import path and the oracle imports `internal/{content,corpus,jbig2}`.
`go.mod` of each dep is trimmed to `module` and `go`, as in pdf-rs (no pruning graph to load).
`build.sh` copies `upstream/`, `deps/` and `oracle/` to a temporary directory, applies the two
patches there with `git apply -p1`, and runs
`GOFLAGS=-mod=mod GOPROXY=off GOTOOLCHAIN=local CGO_ENABLED=0 go build`. Not copied: tests,
testdata, `cmd`, `tools`, `internal/{bench,sample,skew}` (no non-test importer in the build).

## Determinism

| Hook | Where | Effect |
|---|---|---|
| `timeNow` and `SetTimeNow(func() time.Time)` | `byblos-determinism.patch`, `provenance.go` | the clock behind `ProcessedAt` at `optimize.go:220`, `editpages.go:247` (`.UTC()` kept) and `provenance.go:478`. The `time` import of `optimize.go` and `editpages.go` goes. Three hunks of three files, plus the new `SetTimeNow` |
| sorted copy order | `byblos-determinism.patch`, `internal/pdfdoc/buildpages.go` | `migration` copies a page dictionary (`:559`) and every nested dictionary (`copyDict`, `:796`) in Go map order, and the copy order assigns the output object numbers, so `BuildFromPages` (the `edit` op) wrote different bytes on each run. The patch copies in sorted key order; the Rust port does the same (`docs/go-differences.md`) |
| `pdfcpu.TimeNow`, `pdfcpu.RandReader` and the map-order fixes (31 hunks of 8 files) | `pdf-go-determinism.patch` (pdf-rs's `determinism.patch`, copied) | sorted map loops, the clock and the random source of pdf-go; see pdf-rs `docs/parity.md` "Determinism" |

The oracle sets `SetTimeNow` and `pdfcpu.TimeNow` to 2000-01-01T00:00:00Z (UTC, Unix 946684800),
`pdfcpu.RandReader` to a reader that fills each request with byte `i mod 256` at position `i`
(`WriteEnv::parity()` in pdf-rs), and calls `byblos.SetMemoryLimit(0)` (no global pool). Rust
passes `now = Some(that instant)` and `WriteEnv::parity()`. Go's own map order is not pinned for
byblos code: the run-twice check finds any loop that matters.

## Inputs

`oracle -gen <data dir> <input dir>` writes, in one flat directory, with unique names and an
extension that tells the kind:

| Name | Content |
|---|---|
| `fx-img2pdf-<path>.{png,jpg,jp2}` | `tests/data/img2pdf/**` (`/` of the path becomes `_`) |
| `fx-jbig2-<name>.jb2`, `fx-jbig2-symbol-page.pbm` | `tests/data/jbig2` |
| `fx-straighten-contract.pdf`, `fx-census-<name>.pdf`, `fx-real-<name>.pdf` | `tests/data/straighten`, `encrypt-census`, `real` |
| `corpus-<name>.pdf` | `corpus.All()` (37 documents) |
| `corpus-x-<name>.pdf` | the other constructors of `internal/corpus` (the content-stream and filter variants, `ScanPlacedAt`, `ScanFractionalCropBox(0.5)`, `DupRasterWithInfo`, `ScanJPEG`, `ScanImageMask`, `ScanSMaskJPEG`, ...) |
| `gen-{gradient,photo,scanpage,scanjpeg}.{png,jpg}` | the image generators of `internal/corpus`, Go's PNG encoder and JPEG quality 90 |
| `build-<image file name>.pdf` | `EmbedX` and `BuildPDF` of every image above that embeds |
| `sauvola-<image file name>.pbm` | `Sauvola` of every PNG and JPEG above that decodes (P4) |
| `content-<seed>.content` | 11 hand-written content streams |
| `content-<corpus name>-p<n>.content` | the decoded contents of pages 1 to 3 of the corpus documents (read with pdf-go) |
| `mut-<file name>.<tag><ext>` | mutations (below) |

Mutations are seeded with FNV-1a of the base file name and use an own xorshift64\* generator, so
the set is the same on every run. Every base except `fx-real-*`, `build-*` and `sauvola-*` gets:
`t2` (cut to half) and `t1` (cut by one byte); `f0`.. (one bit flip each, 4 for PDF and JPEG,
else 3); and the edits of its kind (a variant whose pattern is absent is not written):

| Kind | Edits |
|---|---|
| PDF | the first `/Length` as `0`, +1, 4294967295 (`ln0`, `ln1`, `lnh`); `startxref` +7 (`sx`); the first xref table entry +3 (`xo`); `/Width 1073741824` (`iw`); `/Height 0` (`ih`) |
| PNG | for the first three distinct chunk types after IHDR, at chunk index `i`: length +1 (`l<i>p`) and 0xFFFFFFFF (`l<i>x`) |
| JPEG | the IFD0 entry count of the Exif block as 0, 1, 0xFFFF (`xc0`, `xc1`, `xcf`) and the IFD0 offset as 0xFFFFFFF0 (`xof`) |
| JP2 | for the first three top-level boxes: length 0 (`b<i>z`) and 0xFFFFFFFF (`b<i>h`) |
| JBIG2 | for the first three segments (short-form headers): data length +1 (`s<i>p`) and 0xFFFFFFFF (`s<i>h`) |
| content | 3000 `q ` in front (`qdeep`), 5000 `[ ` in front (`brk`) |

The count of generated inputs is printed by `-gen` (`oracle: wrote N inputs`); CI prints it with
the number of Go output files. The Go test cases of byblos at 086b88b: 1098 `func Test` in 134
test files (root and `internal/`), plus 26 in `cmd/`. The Rust side has 1000 `#[test]` functions: 971 in `src/` and 29 in `tests/`
(`png_read` 15, `jbig2_api` 12, `resources` 2). Go tests with no counterpart by design: `cmd/`,
`internal/skew` and the fontcensus and danglingrefs tests of `internal/pdfdoc`. There is no
Go-to-Rust test map yet.

Go tests that build random inputs have generators in `-gen` (section 3b of
`tests/parity/go/oracle/gen.go`, math/rand seed 1 like the tests): `gen-bilevel-random.png`
(`downsample_test.go`), `gen-indexed-random.png` (`quantize_indexed_test.go`) and
`build-indexed-rowfilter-00..63.pdf` (`TestBuildPDFIndexedHonoursEveryPNGRowFilter`).
The inputs counted in Status include these 66 base inputs and their derived `mut-`, `build-` and
`sauvola-` files.

## Status

Parity is not complete. At the last run: 47 ops enabled over 3323 inputs, 48513 compared outputs,
and the diff is empty except the known-gap outputs (2 of the 2 listed differ).

**Known gaps.** `tests/parity/known-gaps.txt` lists output file names (one per line) that may
differ from Go until a dependency lands a fix. Each group is headed by its root cause and the
[needs-from-pdf-rs.md](needs-from-pdf-rs.md) item it waits for. The pdf-rs `b2d3a4d` bump fixed
200 of the 254; of the 54 left, 50 (JPEG decode) no longer had an output on either side, 2 were a
byblos-rs bug (a non-UTF-8 image `/Filter`), and the last 2 closed with 2.13 at pdf-rs `19019a9`
(page boundary arrays shorter than four elements). The list is empty now.
`diff.sh` counts a listed file that differs ("known gaps: N of M listed files differ") and does not
fail on it, but it fails with "gap fixed: <name>" when a listed file exists on either side and now
matches, so the list stays exact: remove the line in the PR that bumps the dependency.

Every op the oracles write is enabled. `transcode`, `transcode-tight` (Flate level 1) and
`quantize-png-2`, `quantize-png-16`, `quantize-png-256`, `quantize-indexed-16` (level 9, with the
IDAT chunks cut by Go's 32 KiB `bufio.Writer`) match byte for byte with pdf-rs's
`goflate::ZlibWriter` ([needs-from-pdf-rs.md](needs-from-pdf-rs.md) 2.1).

## Limits

`base` is the Limits of every operation without a suffix: byblos's defaults, except
`MaxImagePixels = 1<<25` (the default 500 000 000 does not fit a pod). `tight` is the Limits of
the `-tight` operations:

| Field | Tight value | Field | Tight value |
|---|---|---|---|
| `MaxObjects` | 20 000 | `MaxRasterSide` | 256 |
| `MaxPages` | 8 | `MaxRasterPixels` | 1<<14 |
| `MaxStreamBytes` | 1<<20 | `MaxRenderFontBytes` | 1<<18 |
| `MaxPageStreamBytes` | 1<<21 | `MaxPathPoints` | 1<<12 |
| `MaxImagePixels` | 1<<20 | `MaxFillWork` | 1<<16 |
| `MaxFontProgramBytes` | 1<<18 | `MaxGStateDepth` | 4 |
| `MaxImages`, `MaxInlineImages`, `MaxUnresolved` | 2 | `MaxFormDepth` | 1 |
| `MaxRetainedBytes` | 1<<16 | `MaxFormInvocations` | 8 |
| `MaxEncodedImagePixels` | 1<<16 | `MaxOperands`, `MaxTextShows`, `MaxPaints` | 64 |
| `MaxPNGTextBytes` | 1<<10 | | |

Fields not listed keep the default of byblos.

## Output files

For input `<n>` and operation `<op>` the file is `<n>.<op>.out` (the result) or `<n>.<op>.err`
(the error class and a newline, see Error classes). An operation whose precondition fails writes
no file. Every Go call runs under `recover()`: a panic is the class `go-panic`.

### Formats

- **Bytes** (`raw`): the bytes themselves, or, when there are more than 1 MiB (1 048 576), the
  one line `big <len> <sha256 hex>` and a newline.
- **Plane**: `<name> <len>` and a newline, then the bytes with no trailing newline; or, over 1 MiB,
  `<name> <len> sha256 <hex>` and a newline in place of the bytes.
- **Struct** (`dw`): one `key=value` line each. Integers are decimal, booleans `true` or `false`,
  floats are `%016x` of `math.Float64bits`, byte strings lower-case hex, text is `quote`d (double
  quotes; printable ASCII as it is except `"` and `\`, which are `\"` and `\\`; `\n`, `\r`, `\t`;
  every other byte as `\xNN`, so each byte of a UTF-8 sequence is escaped). A slice is
  `<key>.len=N` and `<key>.<i>=...`; for a slice of structs the fields are `<key>.<i>.<field>`. A
  nil pointer is `<key>=nil`. A rectangle is `x0 y0 x1 y1` in decimal.
- **Image**: the line `<type> <w> <h> <stride>`, the line `min <x> <y>`, the lines of the type,
  and the planes. `type` is one of `gray`, `gray16`, `alpha`, `alpha16`, `rgba`, `rgba64`, `nrgba`,
  `nrgba64`, `cmyk` (plane `pix`, Go's `Pix`), `paletted` (line `palette <n> <hex of the n RGBA
  bytes>`, plane `pix`), `ycbcr` (stride is `YStride`; line `ycbcr <cstride> <ratio>` with Go's
  `YCbCrSubsampleRatio` name, for example `YCbCrSubsampleRatio420`; planes `y`, `cb`, `cr`), or
  `other` (any other Go type: the pixels as `nrgba64`).
- **Bitmap**: `bitmap <w> <h> <stride>` and the plane `pix` (the packed MSB-first bits).
- **EncodedImage** (key prefix `p`): `width`, `height`, `bpc`, `colorspace.name`, `colorspace.base`,
  `colorspace.hival`, `colorspace.lookup` (hex), `filter`, `decodeparms` (`nil`, or
  `decodeparms.predictor|colors|bpc|columns`), `decode` (float slice), `data_len`, `data_sha256`,
  and `smask` (`nil`, or the same keys with the prefix `smask.`).
- **Provenance**: `version`, `capabilities` (string slice), `processed_at` (RFC 3339 with
  nanoseconds as Go formats the parsed time: `2000-01-01T00:00:00Z`), `optimized`, `pages.len`
  and per page `applied` (string slice), `diverted`, `placement` (float slice), `dropped_annots`,
  `geometry` (`nil`, or `geometry.raster_box`, `page_box` as float slices, `clip_box` and
  `raster_quad` as `nil` or float slices, `geometry.covers_page`), `straightened` (`nil`, or
  `straightened.deg`). Keys are `pages.<i>.<name>`.

### Operations

`-tight` is the same operation under the tight Limits. `lim` below is base unless the op token ends
in `-tight`.

| Input | Op | What it does and its `.out` |
|---|---|---|
| `.pdf` | `inspect`, `inspect-tight` | `InspectDocument(lim)`: struct `encrypted`, `has_signature`, `needs_rendering`, and `pages.<i>.{index, bounds, rotate, text_chars, images.<j>.{bounds, placement (6 floats), placement_deg, width, height, bitonal, colorspace, bpc, filter, obj_nr, substitutable, inline}, diagnostics.<j>.{severity ("warning"/"error"), message}}` |
| | `extract`, `extract-tight` | pages 1 to min(N, 8) (N from `InspectDocument` with base; page 1 when that fails). Sections `== page <n>`, then `err <class>`, or the struct `bounds`, `page`, `raster_quad` (`nil` or 8 floats), `obj_nr`, `dropped_annots`, `bitonal`, `covers_page`, then the line `image`, an Image, and an empty line. Last section `== stats`: `ExtractStats()` after `ResetExtractStats()` before page 1: `attempted`, `extracted`, `partial`, `diverted`, `failed`, `reasons` (sorted by name: `reasons.<i>.name`, `.count`) |
| | `record`, `record-tight` | `RecordExtractionLimits`: a Provenance |
| | `render`, `render-tight` | `RenderPageLimits(page 1, longEdgePx 400)`: an Image (`rgba`) |
| | `optimize`, `optimize-tight` | `OptimizeLimits` with the zero options: bytes |
| | `optimize-lin` | `Linearize`: bytes |
| | `optimize-jpeg75` | `RecompressJPEG`, `JPEGQuality 75`: bytes |
| | `optimize-lin-jpeg75` | both: bytes |
| | `stamp`, `stamp-tight` | `StampTextLayerLimits`, one word `Parity` in `Rect(72, 72, 272, 102)` on every page (page count from `InspectDocument`; no file when it fails or is 0): bytes |
| | `replace`, `replace-tight` | `ReplaceImagesLimits`: the first `Substitutable` image (page order) is replaced by the 8x8 `DeviceGray`, `FlateDecode` image of the next paragraph; no file when there is none: bytes |
| | `edit`, `edit-tight` | `BuildFromPagesLimits` over one shared source: the first min(N, 32) pages in reverse order; output page 1 has `Rotate 90`, output page 2 has `Straighten{Deg: 1.5}`, the others `Rotate 0` (no file when `InspectDocument` fails): bytes |
| | `provenance-write` | `WriteProvenanceContext` of the fixed record below: bytes |
| | `provenance-read` | `ReadProvenanceContext` of the output of `provenance-write` (no file when the write fails): a Provenance |
| `.png` `.jpg` `.jp2` | `embed`, `embed-tight` | `EmbedPNGLimits` (PNG), `EmbedJPEG` (JPEG, no Limits), `EmbedJP2` (JP2): an EncodedImage and `resolution.x`, `resolution.y` |
| | `rotation`, `rotation-tight` | `EXIFRotationLimits`: `rotation=<n>` |
| | `orientation`, `orientation-tight` | `EXIFOrientation`: `rotate`, `ambiguous` |
| | `transcode`, `transcode-tight` | PNG only (no file for the others): `TranscodePNG` with a nil scratch callback, as the Rust port has none: an EncodedImage and the resolution |
| | `build`, `build-tight` | embed (as above), `Rotate` = `EXIFRotationLimits` (0 on error), then `BuildPDFLimits` of one `BuildPage{Image, DPI: x, DPIY: y, Rotate}`: bytes |
| | `decode` | `image.Decode` (Go 1.27 `image/png`, `image/jpeg`; the tiff and webp decoders that pdf-go registers): an Image. Every image op below skips the input (no file) when `image.DecodeConfig` succeeds and declares more than 1<<22 pixels; when it fails, the op runs and returns the decode error |
| | `sauvola` | `Sauvola`: a Bitmap |
| | `downsample-150`, `downsample-150-tight` | `DownsampleDeclaredBPCLimits(img, 8, 300, 150, lim)`: an Image |
| | `downsample-bpc1` | the same with declared depth 1 (nearest neighbour): an Image |
| | `quantize-png-2`, `-16`, `-256` | `QuantizePNG(img, n)`: bytes |
| | `quantize-indexed-16` | `QuantizeIndexed(img, 16)`: an EncodedImage |
| `.pbm` | `jbig2-encode` | P4 reader (the magic, width and height separated by white space and `#` comments, one white-space byte, exactly `stride*height` bytes, each side 1 to 16384; otherwise no file), `EncodeJBIG2Generic`: bytes |
| | `jbig2-image` | `EncodeJBIG2Image`: an EncodedImage |
| `.jb2` | `jbig2-decode` | `DecodeJBIG2Generic`: a Bitmap |
| | `jbig2-decode-globals` | `jbig2.DecodeEmbeddedStreamWithGlobals(globals, data)` with the globals stream `fx-jbig2-symbol-globals.jb2` of the input directory; no file for that file itself or when it is absent. A `jbig2.ErrUnsupportedFeature` is class `jbig2/unsupported`. A Bitmap |
| `.content` | `lex` | the tokens of `content.NewLexer(data)`, one line `<kind> <num as %016x> <text hex> <alias 0\|1>` each (`kind` is the number of `content.Kind`), then `eof`, or `error <quoted message>` at the first error, or `truncated` after 100 000 tokens |
| | `walk`, `walk-tight` | `content.Walk(ctx, data, 0, env)` with the mock env below and `lim`: the Scan as a struct: `images` and `inline_images` (`<i>.{name, id, ctm (6), box (4: llx lly urx ury), inline, clip (nil or 4), opaque, index}`), `text_chars`, `text_ops`, `inked_text_ops`, `text_shows` (`<i>.{raw (hex), font, font_id, size, char_spacing, word_spacing, hscale, rise, mode, trm (6), index}`), `paints` (`<i>.{op, box, fill.{space, comps}, stroke.{space, comps}, index, clip, ctm}`), `shading_ops`, `inline_imgs`, `unresolved` |

**The replacement image of `replace`**: 8x8, `BPC 8`, `DeviceGray`, no `Decode`, no `SMask`, filter
`FlateDecode`, no parms; data = `78 01 01 40 00 bf ff`, then 64 bytes with pixel `i` = `(4*i) & 255`,
then the Adler-32 of those 64 bytes big endian (a zlib stream of one stored block).

**The record of `provenance-write`**: `Version` = `VERSION`, `Capabilities` = `Capabilities()`,
`ProcessedAt` 2000-01-01T00:00:00Z, `Optimized` `linearized`, two pages. Page 1: `Applied`
`["extract-raster", "text-layer"]`. Page 2: `Diverted` `"a<b>&c é"` (U+2028 and U+00E9
as UTF-8), `Placement` `[1 0 0 1 0.5 2.25]`, `DroppedAnnots` 2, `Geometry` {`RasterBox` and `PageBox`
`[0 0 612 792]`, `ClipBox` `[10 20.5 300 400.25]`, `RasterQuad` `[0 0 612 1.5 612.25 792 -0.5 790.75]`},
`Straightened` `{Deg: -1.25}`.

**The mock env of `walk`** (`content.Env`, plus `Limits()` returning `lim`): `XObject(scope, name)`:
a name starting `Im` is an image with `ID = FNV-1a-32(name) mod 1000 + 1`; a name starting `Fm` is a
form with the same ID, `Matrix [1 0 0 1 10 20]`, `Scope = scope + 1`, `BBox [0 0 300 400]` unless
the name ends in `x`, and the content `q 2 0 0 2 5 5 cm /Im1 Do Q BT /F1 10 Tf (nested) Tj ET 0 0 50
50 re f` and a newline, plus `/Fm0 Do` and a newline for `Fm1` and `/Fm2 Do` and a newline for `Fm2`;
any other name is unresolved. `ExtGStateOpaque` is true unless the name ends in `1`. `Font` returns
`(FNV-1a-32(name) mod 1000 + 1, true)` unless the name is `F9` (then `false`).

### Bounds of the oracle's own work

Images over 1<<22 declared pixels are skipped by the image ops; `extract` runs 8 pages; `edit`
takes 32; `-gen` mutates only small inputs (`fx-real-*` is not mutated). Inputs are read whole.

## Error classes

`errClass(err)` tests with `errors.As`/`errors.Is` on the real types, in this order; the first match
wins. The Rust `Error` variant has to map to the same token.

| Class | Go test |
|---|---|
| `go-panic` | a recovered panic |
| `password` | `ErrPassword` |
| `refused/<reason>` | `*RasterRefusal`, `<reason>` is its fine-grained `Reason` (not `Class`) |
| `limit/<name>` | `*LimitError`, `<name>` is its `Name` (for example `images`, `encoded-image-pixels`, `raster-side`, `form-depth`, `q-depth`) |
| `memory-limit` | `ErrMemoryLimit` (never expected with `SetMemoryLimit(0)`) |
| `not-implemented/<capability>` | `*NotImplemented` |
| `mirrored-orientation` | `ErrMirroredOrientation` |
| `jbig2/unsupported` | `ErrUnsupportedJBIG2Feature` |
| `unstampable-rune` | `ErrUnstampableRune` |
| `not-single-raster` | `ErrNotSingleRaster` (without a `RasterRefusal`) |
| `unsupported-codec` | `ErrUnsupportedImageCodec` (without a `RasterRefusal`) |
| `cancelled` | `context.Canceled` |
| `limit/unnamed` | `ErrLimit` without a `*LimitError` (not expected) |
| `pdf/<class>` | an error of pdf-go (or of the Go standard library's codecs), with `<class>` from the table of pdf-rs `docs/parity.md` "Error classes": `wrong-password`, `limit/object-bytes`, `limit/parsed`, `limit/stream-total`, `limit/total-decode`, `limit/objstm-entries`, `limit/objects`, `limit/decode`, `limit/xref-range`, `limit/image-pixels`, `limit/image-bytes`, `no-content`, `unsupported-filter`, `invalid-filter`, `unsupported-resource`, `encryption/unsupported`, `encryption/malformed`, `flate/corrupt`, `zlib/header`, `flate/checksum`, `zlib/dictionary`, `unexpected-eof`, `lzw/invalid-code`, `ascii85/corrupt`, `ascii85/eod`, `hex/invalid-byte`, `jpeg/unsupported`, `jpeg/format`, `ccitt`, `gob`, `predictor/rows`, `predictor/params`, `syntax` (a pdf-go sentinel, or a message pdf-rs lists), `validate` (a message with `validation error`) |
| `error` | any other error: the untyped `fmt.Errorf` errors of byblos itself (`Malformed`, `CorruptProvenance`, `UnsupportedCodec` and `UnbalancedContent` in Rust: `pdfdoc.ErrMalformed`, `ErrUnsupportedCodec`, `ErrUnbalancedContent` and `errCorruptProvenance` get no class of their own), and the untyped errors of pdf-go, for which pdf-rs's `classify` says `syntax` (`pdf::Error::Corrupt`, which Rust maps to `error`) |

`pdf/` is `pdf::Error::class()` of pdf-rs, as `classify` of pdf-rs's oracle assigns it, except for the untyped errors, which Rust sorts in `error::pdf_class` as Go's `pdfClass` does (the wrap `%w` of byblos is `Error::context`, transparent to the root): a pdf-rs `Sentinel` that Go's `sentinels` list names (`SYNTAX_SENTINELS`; also the `corrupt page content`, `corrupt TJ expression` and `corrupt BI expression` texts) is `pdf/syntax`; any other untyped root (`Corrupt`, `Validation`, an unlisted `Sentinel` such as `Eof`, `MissingXRefSection`, `CircularRef`) is `pdf/validate` when the message has `validation error` (pdf-go's `api` prefixes every error of the validation phase with it, pdf-rs's `read_and_validate` does the same with its context) and `error` otherwise. Rust turns the allocation refusals, out-of-memory and cancellation of pdf-rs, deflate and jpeg into `limit/alloc`, `oom` and `cancelled`. `pdf/` is a name space: an `io.ErrUnexpectedEOF` or `flate.CorruptInputError` from `image/png`
is `pdf/unexpected-eof` or `pdf/flate/corrupt` too.

## Entry points

Every public operation of docs/scope.md section 2 is reached by an operation: `Inspect` and
`InspectContext` by `inspect`; `ExtractPageRaster*` by `extract`; `RecordExtraction*`,
`ReadProvenance*` and `WriteProvenance*` by `record`, `provenance-read`, `provenance-write`;
`RenderPage*` by `render`; `Optimize*` by the four `optimize` ops; `StampTextLayer*` by `stamp`;
`ReplaceImages*` by `replace`; `BuildFromPages*` by `edit`; `BuildPDF*` and `TranscodePNG` by
`build` and `transcode`; `Embed*`, `EXIF*` by `embed`, `rotation`, `orientation`;
`Downsample*`, `Sauvola`, `Quantize*`, `EncodeJBIG2*`, `DecodeJBIG2Generic` by their ops. Not
covered by an operation: `ValidatePages`, `NewBuildWriter`, `UpgradeCandidates`, `Capabilities`,
and the in-memory types, which `cargo test` ports cover.
