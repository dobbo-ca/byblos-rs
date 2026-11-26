# What byblos-rs needs from pdf-rs

This compares byblos (`086b88b`) with pdf-rs `origin/main` (`b2d3a4d`); section 2 is updated to `19019a9`.

## How this was checked

1. Every pdf-go symbol in byblos non-comment, non-test code was grepped. The pattern was
   `\b(api|pdfcpu|model|types|filter)\.X`, plus the method calls on `Context`, `XRefTable`,
   `Dict` and `StreamDict`. All of them are in `internal/pdfdoc`.
2. Each symbol was then looked up with `git grep` on pdf-rs `origin/main`.
3. Symbols that appear only in comments were dropped. These are `api.Collect`,
   `api.InsertPages`, `api.ExtractImagesRaw`, `api.Optimize`, `api.OptimizeContext`,
   `api.ReadAndValidate`, `api.AddProperties`, `model.ErrNoContent`, `model.Image`,
   `model.InheritedPageAttrs` (as a name) and `pdfcpu.RenderImage`.
   - Examples: `pdfdoc.go:9,19,25,642,668,780,1317`; `buildpages.go:6,14,608`; `write.go:34`;
     `linearize.go:212`.
   - byblos never calls any of them.

## 1. pdf-go APIs that pdf-rs lacks

**None that the in-scope byblos code calls.** Every pdf-go symbol in non-comment code in
`internal/pdfdoc`, apart from `fontcensus.go`, has a public pdf-rs equivalent:

- `api::{read, validate, read_validate_and_optimize, properties, write_context}`
- `doc::create::{create_xref_table_with_root_dict, create_context}`
- `doc::property::properties_add`
- `image::extract::{extract_image, color_space_components}`
- `Error::WrongPassword`
- `XRefTable::{ensure_page_count, pages, catalog, page_dict → (Dict, Ref, InheritedPageAttrs), dereference, dereference_dict, dereference_array, dereference_stream_dict, insert_object, ind_ref_for_new_object, new_stream_dict_for_buf, find_table_entry, find_table_entry_for_ind_ref, version_string}`
- `StreamDict::{decode_with_limit, encode}`
- `Object::pdf_string`
- `Dict::{find, name_entry, int_entry, subtype, insert_int}`
- `Object::hex_from_bytes` (`NewHexLiteral`)
- `Rect::for_dim`
- `escaped_utf16_string`, `string_literal_to_string`, `hex_literal_to_string`
- `date_time`, `date_string`
- `ObjectStreamDict`, `XRefStreamDict`, `EntryObject::{ObjStm, XRefStm}`
- `Entry.{free, object}`, `Enc.{p, v, r}`, `ReadInfo.using_object_streams`
- `LimitKind::{ObjectBytes, StreamTotal, Parsed, …}`, `Error::XRefObjectRange`,
  `is_decode_limit_exceeded`
- `filter::{FLATE, DCT, JPX, JBIG2, CCITT_FAX}`
- `Config` / `Limits` with all seven fields byblos writes

One pdf-go API is missing, and byblos-rs does not need it:

| Go symbol | byblos file:line | pdf-go file:line | Missing in pdf-rs |
|---|---|---|---|
| `api.ReadContextFile` | `internal/pdfdoc/fontcensus.go:99` | `pkg/api/api.go:93` | There is no file-path reader; `git grep 'read_context_file'` finds nothing. **Not needed**: its only caller, `FontDicts`, is used only by `cmd/byblos-fonts`, which is out of scope. Out-of-scope code could use `api::read(&std::fs::read(path)?, …)` instead. |

## 2. Other things byblos needs from pdf-rs (not pdf-go APIs)

Done at pdf-rs `b2d3a4d` (byblos-rs uses each of them):

| Item | What pdf-rs `b2d3a4d` has | byblos-rs use |
|---|---|---|
| 2.1 Go `compress/flate` at levels 1 and 9 | `filter::goflate::ZlibWriter::with_level(mem, 1 \| 6 \| 9)`, one sink call per Go `Write` | `pdfbuild/png.rs` (`new_zlib_writer`, `zlib.BestSpeed`), `image/png_write.rs` (`zlib_level`, `png.BestCompression` for `quantize.go` and `quantize_indexed.go`) |
| 2.2 MD5 | `crypto::md5` (`md5.Sum` over several parts) | `pdfdoc/deterministic.rs` (the `/ID`); the local copy `src/go/md5.rs` is gone |
| 2.3 CMYK DCT status | `docs/go-differences.md` ("DCT status"): `filter/gojpeg.rs` ports Go 1.27 `image/jpeg` end to end; CMYK, Adobe YCCK and flex sampling decode as in Go, none is a known gap | nothing to change |
| 2.4-2.6 Go `image/jpeg` entropy decode, error classes and coefficient storage | `filter::gojpeg` is a Go-exact port of `image/jpeg` (Huffman tables, `FormatError`s, baseline and progressive scans); jpeg-rs is no longer called to decode | `image::decode`, `optimize` (`recompress_one_image`) |
| 2.7 pdf-go `err.Error()` text | `Error`'s `Display` is pdf-go's text: no `pdf: ` prefix on sentinels and `Corrupt`, `dereferenceDictTypeError`'s order, `wrong type <%v>` | `inspect` diagnostics (`e.to_string()`). All 50 outputs listed for it match now: 46 at the bump; `mut-corpus-dup-raster.pdf.f1.pdf` was a byblos-rs bug (the image `/Filter` was read through `from_utf8_lossy`; it is bytes now); `mut-corpus-x-corrupt-content-stream.pdf.f3.pdf` is 2.13 |
| 2.8 Page `/Thumb` `/Subtype` repair stored | validated through the `/Thumb` reference | `pdfdoc::optimize`, `properties` |
| 2.9 Skipped font of a direct `/Resources` repaired to `null` | `validate_font_resource_dict_repaired` on the page path | same |
| 2.10 Caller-supplied ledger | `api::{read_with_mem, read_and_validate_with_mem, read_validate_and_optimize_with_mem, create_xref_table_with_root_dict_with_mem}` | `pdfdoc::doc::open_with_mem`; `build_from_pages` (sources, output and the validation of the result on one `Mem`), `optimize` (gate, recompression, rewrite) |
| Inherited `/Resources` location | `InheritedPageAttrs::resources_owner` from `XRefTable::page_dict` | `pdfdoc/text.rs` `bind_font` binds in the ancestor as Go's `text.go:140-151` does |

Done at pdf-rs `19019a9`:

| Item | What pdf-rs `19019a9` has | byblos-rs use |
|---|---|---|
| 2.11 Public `jpeg.DecodeConfig` | `pub fn gojpeg::decode_config` | `src/embed.rs` (`embed_jpeg`), `src/image/decode.rs` (`image.DecodeConfig`); the local header pass `src/image/jpeg_config.rs` is gone |
| 2.12 Caller `Mem` for validate and properties | `api::validate_with_mem`, `api::properties_with_mem` | `pdfdoc::write::validate_with_mem` (no longer spelled out), `optimize`'s provenance read of the rewrite (`read_provenance_with_mem` on the call's ledger, with the rewritten bytes charged) |
| 2.13 Page boundary arrays shorter than four elements | `XRefTable::rect` dereferences `a[0..3]` in order and returns `Panic` only on a missing index, as Go's `rect` (`xreftable.go:2016`) | `/MediaBox [0 0!612 792]` is `wrong type <<nil>>` as in Go; `known-gaps.txt` is empty |

Nothing is missing now.

### Not asked of pdf-rs: byblos-rs ports these itself

- **PNG decoder.** pdf-rs has only a `pub(crate)` test decoder (`src/image/png.rs:409`).
  byblos-rs ports Go's `image/png` reader in full, because it also decodes caller images.
- **Paletted PNG encoder.** pdf-rs `image/png.rs` covers Gray and NRGBA at level 6 only.
  byblos-rs writes the paletted path on top of item 2.1.
- **TIFF reader for pdf-rs's own TIFF output.** byblos-rs ports it itself.
- **Go `image/jpeg` encoder.** Neither pdf-rs nor jpeg-rs has it; jpeg-rs's encoder is
  `cjpeg`-exact, not Go-exact.
