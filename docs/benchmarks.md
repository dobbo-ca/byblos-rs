# Benchmarks

The target is at most 1.10 times Go's time on every class, with Rust at the default `max_threads`
against Go at its default `GOMAXPROCS`, and both single-threaded (`max_threads = 1` against
`GOMAXPROCS=1`). byblos itself is serial in phase 1, so the default-thread rows differ from the
single-thread rows only where pdf-rs or jpeg-rs spawn.

There is no benchmark framework. `examples/bench.rs` (Rust, `std::time::Instant`) and
`oracle -bench` (Go, `tests/parity/go/oracle/bench.go`, `time.Now`) run the same classes on the
same files, and `tests/parity/bench.sh` joins the two.

## Running

`.github/workflows/bench.yml` (`workflow_dispatch` and push to `main`, not a required check) builds
the `bench` stage of `.github/ci/Dockerfile` and runs `sh tests/parity/bench.sh` in it with
`docker run`, so Go and Rust run on the same machine and a BuildKit cache hit cannot skip the
timing. The tables go to the job summary. The `bench` stage holds the Go oracle
(`/usr/local/bin/oracle`), the Rust example (`/usr/local/bin/bench`) and `/bench-in`: the files
`oracle -gen` writes, without the `mut-` mutations and without files over 2 MiB.

The job runs on `ubuntu-24.04` (a GitHub-hosted shared VM); the summary prints the vCPU count.
Timings are noisy, and they are a new baseline, not a regression against earlier runs on other
hardware.

Locally, from the repo root, with `oracle` and `bench` built:

```
oracle -gen tests/data /gen-in                 # the inputs; copy the ones to use to /bench-in
oracle -bench /bench-in 0                      # Go, GOMAXPROCS as is; `1` pins it to 1
bench [-threads N] /bench-in                   # Rust; -threads sets Limits.max_threads
sh tests/parity/bench.sh                       # both, best of three, two tables
```

Both print one line per class, `class nanoseconds-per-iteration files`, and a
`bench <class>: <n> files` line on stderr. `bench.sh` takes the lowest of three runs per class and
side (the machine is shared), prints `class | Go ms | Rust ms | ratio` for each of the two settings
and marks a ratio (Rust over Go) above 1.10 with `SLOW`. It exits 0: it is a report, not a gate.

## Method

- A class takes the first 8 inputs, sorted by file name, whose input is at most 2 MiB and whose
  operation succeeds once. Both sides use the same name rules, so they time the same files (the
  stderr file counts should match; a difference is a Go/Rust behaviour difference, not a speed
  difference).
- One iteration runs the operation on every picked file. The class repeats until at least 1 s has
  passed and reports the mean time of an iteration.
- Setup is not timed: the image decode of `sauvola`, `downsample` and `quantize`, the PBM parse of
  `jbig2-encode`, the page count of `stamp`.
- Go and Rust use byblos's default limits except `MaxImagePixels = 1<<25`, as the parity oracle
  does. Go calls `SetMemoryLimit(0)`.
- The clock hooks of the parity run are not set: `optimize` and `stamp` read the system clock on
  both sides.

## Classes

| Class | Inputs (file name) | Timed operation |
|---|---|---|
| `sauvola` | PNG, JPEG: `fx-img2pdf-*`, `gen-*` | `Sauvola` |
| `jbig2-encode` | PBM: `sauvola-*`, `fx-jbig2-*` | `EncodeJBIG2Generic` |
| `jbig2-decode` | JBIG2: `fx-jbig2-*.jb2` (streams that decode without globals) | `DecodeJBIG2Generic` |
| `downsample` | as `sauvola` | 300 to 150 dpi at declared depth 8, and at depth 1 |
| `quantize` | as `sauvola` | PNG at 16 and 256 colours, indexed at 16 |
| `embed-build` | PNG, JPEG, JP2: `fx-img2pdf-*`, `gen-*` | `Embed*`, `EXIFRotation`, `BuildPDF` |
| `inspect` | PDF: `corpus-*`, `fx-real-*` | `Inspect` |
| `extract` | as `inspect` | `ExtractPageRaster`, page 1 (only the files that are single-raster scans) |
| `render` | as `inspect` | page 1 at 400 px |
| `optimize` | as `inspect` | rewrite, linearize, JPEG recompress at q75 (three calls) |
| `stamp` | as `inspect` | `StampTextLayer`, one word on every page |

The scope table (scope.md section 8) names larger inputs: 300 dpi Letter scans for `sauvola`, a
2550 by 3300 text page for `jbig2-encode`, a 10-page document for `stamp`. The generated inputs are
what exists today; adding such files to `tests/data` and to `-gen` is not enough for the image classes:
the bench skips images over 1<<22 pixels (`MAX_DECODE_PIXELS`, `maxDecodePixels`), which a
2550 by 3300 page exceeds, so both caps must be raised too. A `FILES n vs m` mark in the table means
Go and Rust timed different file sets.

**Not yet evidence for the 1.10x ceiling.** The default inputs are tiny (first 8 names in byte order,
so images of at most 48 by 36 pixels and synthetic `corpus-*` PDFs), so each ratio mostly measures
per-call setup. `quantize` and the transcode arm of `embed-build` (16-bit, interlaced and alpha
PNGs) use pdf-rs's Go-exact Flate levels 9 and 1, as Go does, so they count toward the criterion.
Input selection by
explicit list or `gen-` prefix and the scan-sized inputs need `bench.go` changes as well, and the
Go side is read only here: open.

## Results

From `bench.yml` run 45737358148 on a GitHub-hosted `ubuntu-24.04` runner (2 vCPU), the first
hosted baseline. Every class is below 1.10 in both tables.

### Go default GOMAXPROCS against Rust default max_threads

| class | Go ms | Rust ms | ratio | |
|---|---|---|---|---|
| stamp | 4.731 | 1.972 | 0.42 |  |
| embed-build | 0.057 | 0.027 | 0.47 |  |
| render | 51.991 | 39.308 | 0.76 |  |
| optimize | 123.198 | 62.305 | 0.51 |  |
| jbig2-decode | 0.542 | 0.394 | 0.73 |  |
| inspect | 0.966 | 0.461 | 0.48 |  |
| sauvola | 0.173 | 0.157 | 0.90 |  |
| jbig2-encode | 0.906 | 0.754 | 0.83 |  |
| quantize | 4.174 | 1.401 | 0.34 |  |
| downsample | 0.165 | 0.135 | 0.82 |  |
| extract | 47.524 | 30.757 | 0.65 |  |

### Go GOMAXPROCS=1 against Rust max_threads 1

| class | Go ms | Rust ms | ratio | |
|---|---|---|---|---|
| stamp | 4.443 | 1.999 | 0.45 |  |
| embed-build | 0.062 | 0.027 | 0.44 |  |
| render | 54.567 | 39.125 | 0.72 |  |
| optimize | 125.607 | 62.247 | 0.50 |  |
| jbig2-decode | 0.543 | 0.395 | 0.73 |  |
| inspect | 0.997 | 0.458 | 0.46 |  |
| sauvola | 0.183 | 0.157 | 0.86 |  |
| jbig2-encode | 0.905 | 0.753 | 0.83 |  |
| quantize | 3.794 | 1.398 | 0.37 |  |
| downsample | 0.168 | 0.136 | 0.81 |  |
| extract | 49.341 | 30.470 | 0.62 |  |
