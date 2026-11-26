package main

// -bench: the Go side of the benchmark (docs/benchmarks.md). For each class it picks the first
// benchMaxFiles inputs (sorted by name) whose operation succeeds, runs the operation on all of
// them once per iteration, repeats until benchMinTime has passed and prints
// `<class> <ns per iteration> <files>`. The Rust side (examples/bench.rs) makes the same
// choices. Setup (decode, parse) is not timed.

import (
	"bytes"
	"context"
	"fmt"
	"image"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/dobbo-ca/byblos"
)

const (
	benchMinTime  = time.Second
	benchMaxFiles = 8
	benchMaxBytes = 2 << 20 // inputs over this are not used
)

// benchClass picks its inputs with match and prepares one, untimed, with prep; the returned
// function is the timed operation.
type benchClass struct {
	name  string
	match func(name, kind string) bool
	prep  func(in *input) (func() error, error)
}

func hasPrefix(name string, prefixes ...string) bool {
	for _, p := range prefixes {
		if strings.HasPrefix(name, p) {
			return true
		}
	}
	return false
}

func benchClasses() []benchClass {
	lim := baseLim()
	ctx := context.Background()
	isImage := func(name, kind string) bool {
		return (kind == "png" || kind == "jpg" || kind == "jp2") && hasPrefix(name, "fx-img2pdf-", "gen-")
	}
	pdf := func(name, kind string) bool { return kind == "pdf" && hasPrefix(name, "corpus-", "fx-real-") }
	// onImage prepares f on the decoded image.
	onImage := func(f func(img image.Image) error) func(in *input) (func() error, error) {
		return func(in *input) (func() error, error) {
			img, err := loadImage(in)
			if err != nil {
				return nil, err
			}
			return func() error { return f(img) }, nil
		}
	}
	// onPDF prepares f on the bytes of the document.
	onPDF := func(f func(in *input) error) func(in *input) (func() error, error) {
		return func(in *input) (func() error, error) { return func() error { return f(in) }, nil }
	}
	opt := func(o byblos.OptimizeOptions) func(in *input) error {
		return func(in *input) error {
			return byblos.OptimizeLimits(ctx, &bytes.Buffer{}, reader(in), o, lim)
		}
	}
	return []benchClass{
		{"sauvola", isImage, onImage(func(img image.Image) error { _, err := byblos.Sauvola(img); return err })},
		{"jbig2-encode", func(name, kind string) bool { return kind == "pbm" && hasPrefix(name, "sauvola-", "fx-jbig2-") },
			func(in *input) (func() error, error) {
				m, err := parsePBM(in.data)
				if err != nil {
					return nil, err
				}
				return func() error { _, err := byblos.EncodeJBIG2Generic(m); return err }, nil
			}},
		{"jbig2-decode", func(name, kind string) bool { return kind == "jb2" && hasPrefix(name, "fx-jbig2-") },
			func(in *input) (func() error, error) {
				return func() error { _, err := byblos.DecodeJBIG2Generic(in.data); return err }, nil
			}},
		{"downsample", isImage, onImage(func(img image.Image) error {
			for _, bpc := range []int{8, 1} {
				if _, err := byblos.DownsampleDeclaredBPCLimits(ctx, img, bpc, 300, 150, lim); err != nil {
					return err
				}
			}
			return nil
		})},
		{"quantize", isImage, onImage(func(img image.Image) error {
			for _, n := range []int{16, 256} {
				if _, err := byblos.QuantizePNG(img, n); err != nil {
					return err
				}
			}
			_, err := byblos.QuantizeIndexed(img, 16)
			return err
		})},
		{"embed-build", isImage, func(in *input) (func() error, error) {
			return func() error { _, err := buildFromImage(in, lim); return err }, nil
		}},
		{"inspect", pdf, onPDF(func(in *input) error { _, err := byblos.InspectDocument(ctx, reader(in), lim); return err })},
		{"extract", pdf, onPDF(func(in *input) error {
			_, err := byblos.ExtractPageRasterLimits(ctx, reader(in), 1, lim)
			return err
		})},
		{"render", pdf, onPDF(func(in *input) error {
			_, err := byblos.RenderPageLimits(ctx, reader(in), 1, 400, lim)
			return err
		})},
		{"optimize", pdf, onPDF(func(in *input) error {
			for _, o := range []byblos.OptimizeOptions{
				{},
				{Linearize: true},
				{RecompressJPEG: true, JPEGQuality: 75},
			} {
				if err := opt(o)(in); err != nil {
					return err
				}
			}
			return nil
		})},
		{"stamp", pdf, func(in *input) (func() error, error) {
			n, err := pageCount(in)
			if err != nil || n < 1 {
				return nil, errSkip
			}
			tl := byblos.TextLayer{Pages: make([][]byblos.PositionedWord, n)}
			for i := range tl.Pages {
				tl.Pages[i] = []byblos.PositionedWord{{Text: "Parity", Bounds: image.Rect(72, 72, 272, 102)}}
			}
			return func() error {
				return byblos.StampTextLayerLimits(ctx, &bytes.Buffer{}, reader(in), tl, lim)
			}, nil
		}},
	}
}

// bench runs every class over inDir; threads > 0 sets GOMAXPROCS.
func bench(inDir string, threads int) {
	byblos.SetMemoryLimit(0)
	if threads > 0 {
		runtime.GOMAXPROCS(threads)
	}
	ents, err := os.ReadDir(inDir) // sorted by name
	if err != nil {
		fatalf("%v", err)
	}
	classes := benchClasses()
	var inputs []*input
	for _, e := range ents {
		kind := kindOf(e.Name())
		if e.IsDir() || kind == "" {
			continue
		}
		if fi, err := e.Info(); err != nil || fi.Size() > benchMaxBytes {
			continue
		}
		used := false
		for _, c := range classes {
			used = used || c.match(e.Name(), kind)
		}
		if !used {
			continue
		}
		data, err := os.ReadFile(filepath.Join(inDir, e.Name()))
		if err != nil {
			fatalf("%v", err)
		}
		inputs = append(inputs, &input{name: e.Name(), kind: kind, data: data, dir: inDir})
	}
	for _, c := range classes {
		var jobs []func() error
		for _, in := range inputs {
			if len(jobs) == benchMaxFiles {
				break
			}
			if !c.match(in.name, in.kind) {
				continue
			}
			job, err := protect(func() (func() error, error) { return c.prep(in) })
			if err != nil {
				continue
			}
			if _, err := protect(func() (struct{}, error) { return struct{}{}, job() }); err != nil {
				continue
			}
			jobs = append(jobs, job)
		}
		fmt.Fprintf(os.Stderr, "bench %s: %d files\n", c.name, len(jobs))
		if len(jobs) == 0 {
			continue
		}
		var iters int64
		start := time.Now()
		for {
			for _, j := range jobs {
				_ = j()
			}
			iters++
			if time.Since(start) >= benchMinTime {
				break
			}
		}
		fmt.Printf("%s %d %d\n", c.name, time.Since(start).Nanoseconds()/iters, len(jobs))
	}
}
