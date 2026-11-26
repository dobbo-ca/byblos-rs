// Command oracle is the Go side of the byblos-rs parity harness (docs/parity.md).
//
//	oracle -gen <data dir> <input dir>       write the inputs (gen.go)
//	oracle [-threads N] <input dir> <out dir>  run every operation on every input
//	oracle -bench <input dir> <threads>      time the bench classes (bench.go); 0 = GOMAXPROCS as is
//
// -threads is accepted for symmetry with the Rust oracle and ignored. For input `<n>` and
// operation `<op>` the output is `<n>.<op>.out` (the result) or `<n>.<op>.err` (the class token of
// the error and a newline, see errclass.go). An operation whose precondition fails writes no file.
// The oracle builds only against the patched upstream (tests/parity/go/build.sh).
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/dobbo-ca/byblos"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu"
)

// fixedTime is the clock of every run: 2000-01-01T00:00:00Z.
func fixedTime() time.Time { return time.Unix(946_684_800, 0).UTC() }

// parityRand is the patched pdfcpu.RandReader: byte i of every request is i mod 256.
type parityRand struct{}

func (parityRand) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = byte(i % 256)
	}
	return len(p), nil
}

// Bounds of the oracle's own work, so that it fits a BuildKit pod.
const (
	maxDecodePixels = 1 << 22 // image ops skip an image whose header declares more
	maxPages        = 8       // extract runs the first maxPages pages
	maxEditPages    = 32      // edit takes the first maxEditPages pages
	bigOutput       = 1 << 20 // outputs over this are written as `big <len> <sha256>`
)

// errSkip is returned by an operation whose precondition fails: no file is written.
var errSkip = errors.New("skip")

// input is one file of the input directory.
type input struct {
	name string // file name, with the extension
	kind string // pdf, png, jpg, jp2, pbm, jb2, content
	data []byte
	dir  string
}

// op is one named operation. Its result is the content of the `.out` file.
type op struct {
	name string
	fn   func(in *input) ([]byte, error)
}

func main() {
	args := os.Args[1:]
	if len(args) > 0 && args[0] == "-gen" {
		if len(args) != 3 {
			fatalf("usage: oracle -gen <data dir> <input dir>")
		}
		gen(args[1], args[2])
		return
	}
	if len(args) > 0 && args[0] == "-bench" {
		n := 0
		if len(args) != 3 {
			fatalf("usage: oracle -bench <input dir> <threads>")
		}
		if _, err := fmt.Sscan(args[2], &n); err != nil {
			fatalf("bad thread count %q", args[2])
		}
		bench(args[1], n)
		return
	}
	if len(args) > 1 && args[0] == "-threads" {
		args = args[2:]
	}
	if len(args) != 2 {
		fatalf("usage: oracle [-threads N] <input dir> <output dir>")
	}
	run(args[0], args[1])
}

func fatalf(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "oracle: "+format+"\n", a...)
	os.Exit(1)
}

// kindOf maps a file name to its kind by extension; "" means the file is not an input.
func kindOf(name string) string {
	switch strings.ToLower(filepath.Ext(name)) {
	case ".pdf":
		return "pdf"
	case ".png":
		return "png"
	case ".jpg":
		return "jpg"
	case ".jp2":
		return "jp2"
	case ".pbm":
		return "pbm"
	case ".jb2":
		return "jb2"
	case ".content":
		return "content"
	}
	return ""
}

func run(inDir, outDir string) {
	// The clocks and the random source (docs/parity.md, Determinism), and no global memory pool.
	byblos.SetTimeNow(fixedTime)
	pdfcpu.TimeNow = fixedTime
	pdfcpu.RandReader = parityRand{}
	byblos.SetMemoryLimit(0)

	if err := os.MkdirAll(outDir, 0o755); err != nil {
		fatalf("%v", err)
	}
	ents, err := os.ReadDir(inDir)
	if err != nil {
		fatalf("%v", err)
	}
	var names []string
	for _, e := range ents {
		if !e.IsDir() && kindOf(e.Name()) != "" {
			names = append(names, e.Name())
		}
	}
	sort.Strings(names)
	ops := map[string][]op{}
	for _, k := range []string{"pdf", "png", "jpg", "jp2", "pbm", "jb2", "content"} {
		ops[k] = opsFor(k)
	}
	for _, name := range names {
		data, err := os.ReadFile(filepath.Join(inDir, name))
		if err != nil {
			fatalf("%v", err)
		}
		in := &input{name: name, kind: kindOf(name), data: data, dir: inDir}
		for _, o := range ops[in.kind] {
			runOp(outDir, in, o)
		}
	}
}

// runOp runs one operation under recover() and writes its file.
func runOp(outDir string, in *input, o op) {
	res, err := protect(func() ([]byte, error) { return o.fn(in) })
	base := filepath.Join(outDir, in.name+"."+o.name)
	switch {
	case errors.Is(err, errSkip):
		return
	case err != nil:
		write(base+".err", []byte(errClass(err)+"\n"))
	default:
		write(base+".out", res)
	}
}

func write(path string, data []byte) {
	if err := os.WriteFile(path, data, 0o644); err != nil {
		fatalf("%v", err)
	}
}

// errGoPanic stands for a recovered Go panic.
var errGoPanic = errors.New("go panic")

// protect runs f under recover(): a Go panic is the class `go-panic`.
func protect[T any](f func() (T, error)) (v T, err error) {
	defer func() {
		if r := recover(); r != nil {
			var zero T
			v, err = zero, errGoPanic
		}
	}()
	return f()
}

func sha(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

// raw is the result of an operation whose output is a byte string (a PDF, a PNG, a JBIG2
// stream): the bytes, or `big <len> <sha256 hex>` and a newline when there are more than
// bigOutput of them.
func raw(b []byte) []byte {
	if len(b) > bigOutput {
		return []byte(fmt.Sprintf("big %d %s\n", len(b), sha(b)))
	}
	return b
}

// bufRaw is raw(buf.Bytes()).
func bufRaw(buf *bytes.Buffer) []byte { return raw(buf.Bytes()) }
