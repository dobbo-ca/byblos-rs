# Fuzzing

`tests/fuzz` is a cargo-fuzz crate (`byblos-fuzz`, its own `Cargo.toml` and `Cargo.lock`, and a
`[workspace]` of its own so only it depends on `libfuzzer-sys`). It has one target per parser or
input entry point (scope.md section 7). Do not build or run it on a laptop: push a branch and use CI.

```
gh workflow run fuzz.yml --ref <branch> -f max_total_time=60
```

## Targets

Every target must return normally on any input: it never unwraps a result, and an `Err` is
dropped. The only panics are the invariants marked "asserts", ported from Go's fuzz functions.
`tests/fuzz/common.rs` (included by each target with `#[path]`) holds the limits and the
build-and-inspect helper.

| Target | Entry point | Also checks |
|---|---|---|
| `go_sort` | `go::sort::slice_by` on the input bytes, and on (key, index) pairs | the result equals std's sort; keys non-decreasing and the indices a permutation |
| `content_lexer` | `content::Lexer::next_token` loop | asserts it ends within len + 1 tokens (Go `FuzzLexer`) |
| `content_walk` | `content::walk` with a mock `Env` whose forms hold the input itself | form depth and invocation caps (16 invocations); input over 64 KiB is skipped |
| `jbig2_decode` | `jbig2::decode_generic`, `page_size` | |
| `jbig2_decode_globals` | `decode_embedded_stream_with_globals`, `page_size_with_globals` | input is a u16 length, that many globals bytes, then the page stream |
| `jbig2_roundtrip` | bytes to `Bitmap` (u16 w-1, u16 h-1, repeated pixels; at most 512x512), `encode_generic`, `decode_generic` | asserts the decoded pixels equal the source |
| `embed_png` | `embed_png`, `exif_orientation`, then `build_pdf` and `inspect` of an accepted image | |
| `embed_jpeg` | `embed_jpeg`, `exif_rotation`, `exif_orientation`, then `build_pdf` and `inspect` | asserts a rotation is 0, 90, 180 or 270 (Go `FuzzEXIFRotation`) |
| `embed_jp2` | `embed_jp2`, then `build_pdf` and `inspect` | asserts an accepted image is positive and within the pixel cap (Go `FuzzEmbedJP2`) |
| `transcode_png` | `build::transcode_png` under a 4096 pixel cap | asserts the cap holds and `build_pdf` takes what is accepted (Go `FuzzPNGTranscode`) |
| `build_pdf` | `build_pdf` of one page from a fuzzed `(filter, colour space, bpc, flags, w, h, rotate, dpi, data)` | asserts `inspect` reads back what `build_pdf` accepts (Go `FuzzBuildPDF`) |
| `image_decode` | `image::decode_config`, `image::decode` (PNG reader, TIFF reader, JPEG) | |
| `xmp` | the `go::xml` tokenizer, then the bytes as a JPEG's XMP APP1 segment through `exif_orientation` | |
| `provenance_json` | `Provenance::from_json`, then `to_json` of a parsed record | |
| `pixels` | fuzzed dims, kind (all seven pixel types) and pixels to `sauvola`, `downsample`, `quantize_png`, `quantize_indexed` | |
| `dct_parse` | `dct::parse`, `Frame::need`, `check_frame`, and `need` of a hand-built frame | |
| `sfnt` | `render::sfnt::Font::parse`, `glyph_index`, `glyph_advance` and `load_glyph` over the first 512 glyphs | |
| `cff` | `parse_cff`, `cff_to_sfnt`, then the wrapped font through `Font::parse` and `load_glyph` | |
| `type1` | `parse_type1`, then `interpret` and `advance` for every code | |
| `inspect` | `inspect::inspect` | |
| `extract` | `extract::extract_page_raster`, pages 1 and 2 | |
| `render` | `render_page::render_page`, page 1 at 128 px | |
| `optimize` | `optimize::optimize` with linearize and JPEG recompress (q75) on | |
| `stamp` | `stamp::stamp_text_layer` with a fixed one-word layer on pages 1 and 2 | a 2 s watchdog sets the cancel flag, as a defensive bound: a repaired xref can claim object number ~10^10, and pdf-go's writer walks `0..Size`; the pinned pdf-rs visits only the xref table's numbers |
| `replace_images` | `inspect`, then `substitute::replace_images` for every substitutable image, as a 1x1 grey pixel | |
| `build_from_pages` | `editpages::build_from_pages` over one fuzzed source, pages 2 and 1; rotate and straighten from the last two bytes | |
| `provenance` | `read_provenance` and `record_extraction` | |

Left out of scope.md section 7: nothing is dropped; `stamp`, `replace_images`, `build_from_pages` and `provenance` cover the untrusted-PDF entry points beyond it. `xmp` fuzzes the public `go::xml::Decoder` and
reaches the XMP rule (`xmp_normal`, private to `embed`) through `exif_orientation`. The `render`
target takes a PDF, so `render::page` with a content stream and font program is reached through
that; add a target that calls it directly if the PDF route starves it.

## Limits

Every target takes its limits from `common::limits()`. `Limits::mem()` makes a new ledger on each
call, so the fuzzer cannot see the crate's total: `-malloc_limit_mb=512` catches one allocation over
512 MiB and `-rss_limit_mb=2048` catches growth over time, not a call that overruns `max_alloc` in
pieces; `common::measure` (a counting global allocator) covers that for `embed_jpeg` (Go's 64 MiB + 16*len bound on `exif_rotation`), `transcode_png` (peak under 8 MiB + 16*len) and `build_and_inspect` (peak under `max_alloc` + 64 MiB). The values: `max_alloc` 256 MiB, `max_threads` 1, `max_image_pixels` and
`max_encoded_image_pixels` 4 Mpx, `max_raster_side` 2048 and `max_raster_pixels` 1 Mpx, streams and
retained bytes 16 MiB, fonts 4 MiB, 20,000 objects and 500 pages, `max_png_text_bytes` 1 MiB. A
target that needs a tighter cap sets it itself (`transcode_png` 4096 px, `build_pdf` 65,536 px,
`content_walk` 16 form invocations).

## Seeds

Seeds are `tests/fuzz/seeds/<target>/` (committed). Where they come from:

| Target | Seeds |
|---|---|
| `content_lexer`, `content_walk` | Go's `FuzzLexer` seeds, and content streams from the `tests/data/real` PDFs |
| `jbig2_decode`, `jbig2_decode_globals` | `tests/data/jbig2/*.jb2`; the globals target prefixes each with its length (`symbol-globals.jb2` plus `symbol-page.jb2` is the symbol case) |
| `jbig2_roundtrip`, `pixels`, `build_pdf` | written by hand in each target's input layout (`build_pdf` from Go's `FuzzBuildPDF` cases) |
| `embed_png`, `embed_jpeg`, `embed_jp2`, `dct_parse` | the `tests/data/img2pdf` files of that type; `embed_jp2` and `embed_jpeg` also the `go-*` files, Go's `f.Add` seeds of `FuzzEmbedJP2` and `FuzzEXIFRotation` (the PNG text-chunk ones are in `embed_jpeg`, which runs `exif_rotation`) |
| `transcode_png`, `image_decode` | the img2pdf PNGs and the `tests/data/png` files (and for `image_decode` JPEG and TIFF); `transcode_png` also `go-*`, rebuilt from Go's `FuzzPNGTranscode` specs (same dimensions, depth, interlace, PLTE/tRNS; filter 0 throughout) |
| `xmp` | XMP packets from the corpus PDFs, and three by hand |
| `provenance_json` | a record by hand, and edge cases |
| `sfnt` | the small fonts in `tests/data/sfnt` |
| `cff`, `type1` | CFF programs from the corpus PDFs; a Type 1 program built by hand (raw and PFB) |
| `inspect`, `extract`, `render`, `optimize`, `stamp`, `replace_images`, `provenance` | PDFs of at most 120 KB from `tests/data/real`, `straighten` and `encrypt-census` (`stamp` etc. start from `test.pdf`; `stamp` and `build_from_pages` also from the multi-page `bookletTestA6.pdf` and `zineTest.pdf`, because both targets ask for page 2) |

The image also builds `tests/fuzz/seeds/data` from the small inputs in `tests/data` and a sample
of the oracle's `-gen` output. `fuzz.yml` adds it as a seed dir to every target that reads a file
(all but `go_sort`, `content_lexer`, `content_walk`, `jbig2_roundtrip`, `build_pdf`, `pixels`,
`provenance_json` and `xmp`).

## Running in CI

`.github/workflows/fuzz.yml` runs weekly and on `workflow_dispatch` only. The matrix has one job
per architecture and target (x86_64 on `ubuntu-24.04`, aarch64 natively on `ubuntu-24.04-arm`;
`max-parallel: 2`), with `-max_total_time` from the `max_total_time` input (default 600),
`-timeout=20 -rss_limit_mb=2048 -malloc_limit_mb=512`, with AddressSanitizer on both. The corpus is
kept between runs in the Actions cache, one per architecture and target; crashers are uploaded as
artifacts. The crate has no SIMD, so there are no tier-differential targets.

## Adding a target

A `[[bin]]` in `tests/fuzz/Cargo.toml` (`test = false`, `doc = false`, `bench = false`), a file in
`fuzz_targets/` that takes `common::limits()`, a seed dir, and a line in the `fuzz.yml` matrix
(and in its no-data `case` if it takes no file). Regenerate the lockfile with
`cargo generate-lockfile --manifest-path tests/fuzz/Cargo.toml` if a dependency changes.
