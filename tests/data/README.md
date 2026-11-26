# Test data

Inputs for the ported tests and for the Go parity oracle (`oracle -gen tests/data <inputs>`,
docs/parity.md). Nothing here is generated; the oracle makes the rest (corpus PDFs, mutations).
No file is in Git LFS: byblos has no `.gitattributes`, so every fixture is a plain file in both
repositories.

| Path | Origin |
|---|---|
| `img2pdf/` | `testdata/img2pdf/` of byblos at 086b88b, copied unchanged (144 files: PNG, JPEG and JP2 fixtures, the `r4/` and `r5/` generations, `gen.py`, and `oracle.json`, the img2pdf results the tests compare with). |
| `jbig2/` | `testdata/jbig2/` of byblos: the 11 `.jb2` goldens and `symbol-page.pbm`. |
| `straighten/` | `testdata/straighten/` of byblos: `contract.json`, `contract.pdf`. |
| `png/` | `src/image/png/testdata/` of Go 1.27.4: the `gray-gradient` and `invalid-*` files and the 35 `pngsuite/` PNGs with their `.sng` goldens that Go's `reader_test.go` uses (the pngsuite files are from libpng.org; see `pngsuite/README.original`). |
| `oracle/poppler.json` | `testdata/oracle/poppler.json` of byblos: poppler results for the corpus. |
| `encrypt-census/` | `cmd/byblos-encrypt-census/testdata/*.pdf` of byblos: two encrypted PDFs (owner password only; user and owner password). |
| `real/` | 24 PDFs (3.8 MB) from pdf-rs `tests/data/*.pdf` at `9bdaeb5`. |

`real/` is a subset of pdf-rs's `tests/data`. pdf-rs's NOTICE records that source: `tests/data/*.pdf`
is `pkg/testdata` of pdfcpu (github.com/pdfcpu/pdfcpu), copied unchanged, and pdfcpu is
licensed under the Apache License, Version 2.0 (`LICENSE` at the repo root). pdf-rs has no
other note on where each file came from. The subset keeps the born-digital documents, the
small scans and the image fixtures that exercise inspect, extract and render:

`5116.DCT_Filter`, `Acroforms2`, `Hybrid-PDF`, `OptimizeTest`, `Paclitaxel`, `RA_CI`, `T4`, `T6`,
`Walden`, `Wonderwall`, `adobeImplOfPDFSpec`, `annotTest`, `blank-scan`, `bookletTestA6`,
`empty`, `go`, `jphysiol01396-0132`, `pike-stanford`, `test`, `testImage`, `testRot`,
`testWithText`, `text_annotations`, `zineTest`.

The ones that are third-party documents (for example `Walden`, `Paclitaxel`, `adobeImplOfPDFSpec`,
`jphysiol01396-0132`) are in pdfcpu's repository under its Apache-2.0 licence, and pdf-rs
records no separate licence for them. If one of them has to go, drop it here and from the
parity input list.

`testdata/fused_external.txt` of byblos is not copied: only `nofma_test.go` reads it, and that
test is not ported (scope.md section 5.5).
