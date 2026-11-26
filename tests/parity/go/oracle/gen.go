package main

// -gen: writes the inputs of the parity run (docs/parity.md, "Inputs"), flattened into one
// directory. File names are unique, stable and carry the extension that tells the kind (.pdf .png
// .jpg .jp2 .pbm .jb2 .content):
//
//	fx-img2pdf-<path>.{png,jpg,jp2}   tests/data/img2pdf, `/` of the path as `_`
//	fx-jbig2-<name>.{jb2,pbm}         tests/data/jbig2
//	fx-straighten-contract.pdf        tests/data/straighten
//	fx-census-<name>.pdf              tests/data/encrypt-census
//	fx-real-<name>.pdf                tests/data/real
//	corpus-<name>.pdf, corpus-x-<name>.pdf   internal/corpus (All() and the other constructors)
//	gen-<name>.{png,jpg}              the image generators of internal/corpus
//	build-<image file name>.pdf       EmbedX + BuildPDF of every image above that embeds
//	sauvola-<image file name>.pbm     Sauvola of every PNG and JPEG above that decodes
//	content-<seed>.content, content-<corpus name>-p<n>.content   content streams
//	mut-<file name>.<tag><ext>        deterministic mutations (see mutations below)
//
// Mutations are seeded by the name of the base file (FNV-1a) and use an own xorshift generator,
// so the set is the same on every run.

import (
	"bytes"
	"compress/zlib"
	"context"
	"encoding/binary"
	"fmt"
	"hash/fnv"
	"image"
	"image/color"
	"image/jpeg"
	"image/png"
	"math/rand"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"

	"github.com/dobbo-ca/byblos"
	"github.com/dobbo-ca/byblos/internal/corpus"
	"github.com/dobbo-ca/pdf-go/pkg/api"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
)

type generator struct {
	out   string
	names map[string]bool
	count int
}

// emit writes one input; a name that is used twice is a bug of the generator.
func (g *generator) emit(name string, data []byte) {
	if g.names[name] {
		fatalf("gen: duplicate input name %q", name)
	}
	g.names[name] = true
	write(filepath.Join(g.out, name), data)
	g.count++
}

// base is an input that is also a source of mutations.
type base struct {
	name string
	data []byte
}

func gen(dataDir, outDir string) {
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		fatalf("%v", err)
	}
	g := &generator{out: outDir, names: map[string]bool{}}
	var mut []base    // inputs that get mutations
	var images []base // PNG, JPEG and JP2 inputs, for build- and sauvola-

	// 1. The fixtures.
	for _, f := range listFiles(filepath.Join(dataDir, "img2pdf")) {
		switch kindOf(f.rel) {
		case "png", "jpg", "jp2":
			b := base{"fx-img2pdf-" + strings.ReplaceAll(f.rel, "/", "_"), f.data}
			g.emit(b.name, b.data)
			mut = append(mut, b)
			images = append(images, b)
		}
	}
	for _, f := range listFiles(filepath.Join(dataDir, "jbig2")) {
		switch kindOf(f.rel) {
		case "jb2":
			b := base{"fx-jbig2-" + f.rel, f.data}
			g.emit(b.name, b.data)
			mut = append(mut, b)
		case "pbm":
			g.emit("fx-jbig2-"+f.rel, f.data)
		}
	}
	for _, f := range listFiles(filepath.Join(dataDir, "straighten")) {
		if kindOf(f.rel) == "pdf" {
			b := base{"fx-straighten-" + f.rel, f.data}
			g.emit(b.name, b.data)
			mut = append(mut, b)
		}
	}
	for _, f := range listFiles(filepath.Join(dataDir, "encrypt-census")) {
		if kindOf(f.rel) == "pdf" {
			b := base{"fx-census-" + f.rel, f.data}
			g.emit(b.name, b.data)
			mut = append(mut, b)
		}
	}
	for _, f := range listFiles(filepath.Join(dataDir, "real")) {
		if kindOf(f.rel) == "pdf" {
			g.emit("fx-real-"+f.rel, f.data)
		}
	}

	// 2. The corpus PDFs.
	var corpusPDFs []base
	for _, d := range corpus.All() {
		corpusPDFs = append(corpusPDFs, base{"corpus-" + d.Name + ".pdf", d.Data})
	}
	extras := []struct {
		name string
		data []byte
	}{
		{"no-media-box", corpus.NoMediaBox()},
		{"inline-image-scan", corpus.InlineImageScan()},
		{"scan-placed-rot90", corpus.ScanPlacedAt([6]float64{0, 612, -792, 0, 792, 0})},
		{"scan-fractional-crop-half", corpus.ScanFractionalCropBox(0.5)},
		{"dup-raster-info", corpus.DupRasterWithInfo("Title", "Parity <&>")},
		{"rotate-inheritance", corpus.RotateInheritance()},
		{"corrupt-content-stream", corpus.CorruptContentStream()},
		{"corrupt-content-stream-in-array", corpus.CorruptContentStreamInArray()},
		{"bad-adler32-content-stream", corpus.BadAdler32ContentStream()},
		{"clean-contents-array", corpus.CleanContentsArray()},
		{"bad-adler32-in-array", corpus.BadAdler32InArray()},
		{"mid-block-truncation", corpus.MidBlockTruncation()},
		{"predictor12-bad-adler", corpus.Predictor12BadAdler()},
		{"filter-chain-bad-adler", corpus.FilterChainBadAdler()},
		{"bad-adler-trailing-byte", corpus.BadAdlerWithTrailingByte()},
		{"bad-adler-trailing-byte-in-array", corpus.BadAdlerWithTrailingByteInArray()},
		{"double-flate-bad-inner", corpus.DoubleFlateBadInner()},
		{"double-flate-bad-inner-in-array", corpus.DoubleFlateBadInnerInArray()},
		{"double-flate-both-bad", corpus.DoubleFlateBothBad()},
		{"double-flate-both-bad-in-array", corpus.DoubleFlateBothBadInArray()},
		{"partial-predictor-row-cut-trailer", corpus.PartialPredictorRowCutTrailer()},
		{"truncated-adler-trailer", corpus.TruncatedAdlerTrailer()},
		{"corrupt-element-beside-healthy-sibling", corpus.CorruptElementBesideHealthySibling()},
		{"null-contents-ref", corpus.NullContentsRef()},
		{"mixed-page-two-unreadable", corpus.MixedPageTwoUnreadable()},
		{"page-two-stops-mid-stream", corpus.PageTwoStopsMidStream()},
		{"scan-jpeg", corpus.ScanJPEG()},
		{"scan-image-mask", corpus.ScanImageMask()},
		{"scan-smask-jpeg", corpus.ScanSMaskJPEG()},
	}
	for _, e := range extras {
		corpusPDFs = append(corpusPDFs, base{"corpus-x-" + e.name + ".pdf", e.data})
	}
	for _, b := range corpusPDFs {
		g.emit(b.name, b.data)
		mut = append(mut, b)
	}

	// 3. The image generators of the corpus, as PNG and JPEG.
	for _, im := range []struct {
		name string
		img  image.Image
	}{
		{"gradient", corpus.Gradient()},
		{"photo", corpus.Photo()},
		{"scanpage", corpus.Scanpage()},
		{"scanjpeg", corpus.Scanjpeg()},
	} {
		var pb, jb bytes.Buffer
		if err := png.Encode(&pb, im.img); err != nil {
			fatalf("gen: png %s: %v", im.name, err)
		}
		if err := jpeg.Encode(&jb, im.img, &jpeg.Options{Quality: 90}); err != nil {
			fatalf("gen: jpeg %s: %v", im.name, err)
		}
		bp, bj := base{"gen-" + im.name + ".png", pb.Bytes()}, base{"gen-" + im.name + ".jpg", jb.Bytes()}
		for _, b := range []base{bp, bj} {
			g.emit(b.name, b.data)
			mut = append(mut, b)
			images = append(images, b)
		}
	}

	// 3b. The random inputs of the Go tests (math/rand, seed 1, same draw order as the tests):
	// downsample_test.go (bilevel 64x64), quantize_indexed_test.go randomIndexedFixture, and
	// build_test.go TestBuildPDFIndexedHonoursEveryPNGRowFilter (64 row sets, all five PNG filters).
	{
		rng := rand.New(rand.NewSource(1))
		bi := image.NewRGBA(image.Rect(0, 0, 64, 64))
		for y := 0; y < 64; y++ {
			for x := 0; x < 64; x++ {
				v := uint8(0)
				if rng.Intn(2) == 1 {
					v = 255
				}
				bi.SetRGBA(x, y, color.RGBA{R: v, G: v, B: v, A: 255})
			}
		}
		rng = rand.New(rand.NewSource(1))
		pal := make([]color.RGBA, 256)
		for i := range pal {
			pal[i] = color.RGBA{uint8(rng.Intn(256)), uint8(rng.Intn(256)), uint8(rng.Intn(256)), 255}
		}
		ri := image.NewRGBA(image.Rect(0, 0, 200, 200))
		for y := 0; y < 200; y++ {
			for x := 0; x < 200; x++ {
				ri.SetRGBA(x, y, pal[rng.Intn(256)])
			}
		}
		for _, im := range []struct {
			name string
			img  image.Image
		}{{"bilevel-random", bi}, {"indexed-random", ri}} {
			var pb bytes.Buffer
			if err := png.Encode(&pb, im.img); err != nil {
				fatalf("gen: png %s: %v", im.name, err)
			}
			b := base{"gen-" + im.name + ".png", pb.Bytes()}
			g.emit(b.name, b.data)
			mut = append(mut, b)
			images = append(images, b)
		}
		const w, h = 16, 5
		rng = rand.New(rand.NewSource(1))
		for iter := 0; iter < 64; iter++ {
			rows := make([][]byte, h)
			filters := make([]byte, h)
			maxSample := 0
			for i := range rows {
				filters[i] = byte((i + iter) % 5)
				rows[i] = make([]byte, w)
				for j := range rows[i] {
					v := rng.Intn(90)
					rows[i][j] = byte(v)
					if v > maxSample {
						maxSample = v
					}
				}
			}
			var out bytes.Buffer
			err := byblos.BuildPDF(&out, []byblos.BuildPage{{Image: byblos.EncodedImage{
				Width: w, Height: h, BPC: 8,
				ColorSpace: byblos.ColorSpace{Name: "Indexed", Base: "DeviceRGB", HiVal: maxSample, Lookup: make([]byte, 3*(maxSample+1))},
				Filter:     "FlateDecode",
				DecodeParms: &byblos.DecodeParms{
					Predictor: 15, Colors: 1, BitsPerComponent: 8, Columns: w,
				},
				Data: pngFilterRows(rows, filters),
			}, DPI: 300}})
			if err != nil {
				fatalf("gen: indexed row filters %d: %v", iter, err)
			}
			g.emit(fmt.Sprintf("build-indexed-rowfilter-%02d.pdf", iter), out.Bytes())
		}
	}

	// 4. A PDF per image (EmbedX + BuildPDF) and the Sauvola bitmap of every PNG and JPEG.
	for _, b := range images {
		in := &input{name: b.name, kind: kindOf(b.name), data: b.data}
		if pdf, err := protect(func() ([]byte, error) { return buildFromImage(in, baseLim()) }); err == nil {
			g.emit("build-"+b.name+".pdf", pdf)
		}
		if in.kind == "jp2" {
			continue
		}
		pbm, err := protect(func() ([]byte, error) {
			img, err := loadImage(in)
			if err != nil {
				return nil, err
			}
			m, err := byblos.Sauvola(img)
			if err != nil {
				return nil, err
			}
			return append(fmt.Appendf(nil, "P4\n%d %d\n", m.Width, m.Height), m.Pix...), nil
		})
		if err == nil {
			g.emit("sauvola-"+b.name+".pbm", pbm)
		}
	}

	// 5. Content streams: hand-written seeds, and the page contents of the corpus documents.
	for _, s := range contentSeeds {
		b := base{"content-" + s.name + ".content", []byte(s.body)}
		g.emit(b.name, b.data)
		mut = append(mut, b)
	}
	for _, c := range corpusPDFs {
		for _, cs := range corpusContents(c) {
			g.emit(cs.name, cs.data)
			mut = append(mut, cs)
		}
	}

	// 6. Mutations.
	for _, b := range mut {
		for _, v := range mutations(b.name, kindOf(b.name), b.data) {
			g.emit("mut-"+b.name+"."+v.tag+filepath.Ext(b.name), v.data)
		}
	}
	fmt.Printf("oracle: wrote %d inputs to %s\n", g.count, g.out)
}

type file struct {
	rel  string // slash-separated path below the directory
	data []byte
}

// listFiles returns the files below dir in lexical order; a missing directory is no files.
func listFiles(dir string) []file {
	var out []file
	err := filepath.WalkDir(dir, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		rel, err := filepath.Rel(dir, p)
		if err != nil {
			return err
		}
		data, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		out = append(out, file{filepath.ToSlash(rel), data})
		return nil
	})
	if err != nil && !os.IsNotExist(err) {
		fatalf("gen: %v", err)
	}
	return out
}

var contentSeeds = []struct{ name, body string }{
	{"text", "BT /F1 12 Tf 1 0 0 1 72 720 Tm (Hello \\(world\\)) Tj [ (A) -250 (B) ] TJ ET\n"},
	{"paths", "q 1 0 0 RG 0.5 g 10 10 100 50 re B 0 0 m 100 100 l 200 0 300 100 400 0 c S Q\n"},
	{"images", "q 612 0 0 792 0 0 cm /Im0 Do Q q 100 0 0 100 10 10 cm /Im1 Do Q /Missing Do\n"},
	{"forms", "q 1 0 0 1 5 5 cm /Fm0 Do /Fm1 Do /Fmx Do Q /Fm2 Do\n"},
	{"inline", "q 50 0 0 50 0 0 cm BI /W 2 /H 2 /CS /G /BPC 8 ID \x00\x40\x80\xff EI Q\n"},
	{"gstate", "q q q /GS0 gs /GS1 gs 0.5 0 0 0.5 0 0 cm Q Q Q Q Q\n"},
	{"text-modes", "BT /F1 10 Tf 3 Tr (invisible) Tj 0 Tr 2 Tc 3 Tw 150 Tz 5 Ts (shown) Tj T* (x) ' 1 2 (y) \" ET /F9 8 Tf\n"},
	{"numbers", "1.5e3 -.5 +7 4. . - 0.0000001 123456789012345678901234567890 re\n"},
	{"strings", "(unbalanced ( paren) <48656C6C6F> <4> (\\101\\x\\n) /Na#20me /#zz\n"},
	{"clip", "0 0 100 100 re W n q 0 0 50 50 re W* n 612 0 0 792 0 0 cm /Im0 Do Q\n"},
	{"dict-array", "/Span << /MCID 0 /A [ 1 2 [ 3 ] ] >> BDC EMC [ [ [ ( x ) ] ] ] >> << ] )\n"},
}

// corpusContents returns the decoded contents of pages 1..3 of a corpus document (pdf-go reads
// it), as `content-<corpus file name without .pdf>-p<n>.content`; none when it does not read.
func corpusContents(c base) []base {
	var out []base
	ctx, err := protect(func() (*model.Context, error) {
		return api.ReadContext(context.Background(), bytes.NewReader(c.data), model.NewStatelessConfiguration())
	})
	if err != nil {
		return nil
	}
	t := ctx.XRefTable
	if _, err := protect(func() (struct{}, error) { return struct{}{}, t.EnsurePageCount() }); err != nil {
		return nil
	}
	for p := 1; p <= t.PageCount && p <= 3; p++ {
		b, err := protect(func() ([]byte, error) {
			d, _, _, err := t.PageDict(context.Background(), p, false)
			if err != nil {
				return nil, err
			}
			return t.PageContent(d, p)
		})
		if err != nil || len(b) == 0 || len(b) > 1<<20 {
			continue
		}
		stem := strings.TrimSuffix(c.name, ".pdf")
		out = append(out, base{fmt.Sprintf("content-%s-p%d.content", stem, p), b})
	}
	return out
}

// ---- mutations ----

// rng is xorshift64*.
type rng struct{ s uint64 }

func newRng(seed uint64) *rng {
	if seed == 0 {
		seed = 0x9E3779B97F4A7C15
	}
	return &rng{seed}
}

func (r *rng) next() uint64 {
	x := r.s
	x ^= x >> 12
	x ^= x << 25
	x ^= x >> 27
	r.s = x
	return x * 0x2545F4914F6CDD1D
}

func (r *rng) n(m int) int { return int(r.next() % uint64(m)) }

type variant struct {
	tag  string
	data []byte
}

func clone(b []byte) []byte { return append([]byte(nil), b...) }

// mutations returns the variants of one input: truncations (`t2` to half, `t1` by one byte), bit
// flips (`f0`..), and the edits of the kind's length fields (tags in the helper of each kind).
func mutations(name, kind string, data []byte) []variant {
	h := fnv.New64a()
	h.Write([]byte(name))
	r := newRng(h.Sum64())
	n := len(data)
	if n < 2 {
		return nil
	}
	flips := 3
	if kind == "pdf" || kind == "jpg" {
		flips = 4
	}
	vs := []variant{{"t2", clone(data[:n/2])}, {"t1", clone(data[:n-1])}}
	for i := 0; i < flips; i++ {
		d := clone(data)
		d[r.n(n)] ^= 1 << uint(r.n(8))
		vs = append(vs, variant{"f" + strconv.Itoa(i), d})
	}
	switch kind {
	case "pdf":
		vs = append(vs, pdfEdits(data)...)
	case "png":
		vs = append(vs, pngEdits(data)...)
	case "jpg":
		vs = append(vs, jpegEdits(data)...)
	case "jp2":
		vs = append(vs, jp2Edits(data)...)
	case "jb2":
		vs = append(vs, jb2Edits(data)...)
	case "content":
		vs = append(vs,
			variant{"qdeep", append([]byte(strings.Repeat("q ", 3000)), data...)},
			variant{"brk", append([]byte(strings.Repeat("[ ", 5000)), data...)})
	}
	return vs
}

var (
	reLength = regexp.MustCompile(`/Length (\d+)`)
	reStartX = regexp.MustCompile(`startxref\s+(\d+)`)
	reXref   = regexp.MustCompile(`(?m)^(\d{10}) \d{5} n`)
	reWidth  = regexp.MustCompile(`/Width (\d+)`)
	reHeight = regexp.MustCompile(`/Height (\d+)`)
)

// replaceFirst replaces group 1 of the first match of re with repl; nil when there is no match.
func replaceFirst(data []byte, re *regexp.Regexp, repl string) []byte {
	loc := re.FindSubmatchIndex(data)
	if loc == nil {
		return nil
	}
	out := append([]byte(nil), data[:loc[2]]...)
	out = append(out, repl...)
	return append(out, data[loc[3]:]...)
}

func firstNumber(data []byte, re *regexp.Regexp) (int, bool) {
	m := re.FindSubmatch(data)
	if m == nil {
		return 0, false
	}
	v, err := strconv.Atoi(string(m[1]))
	return v, err == nil
}

// pdfEdits: `ln0`, `ln1`, `lnh` (the first /Length: 0, +1, 4294967295), `sx` (startxref +7), `xo`
// (the first in-use xref table entry +3), `iw` (/Width 1<<30), `ih` (/Height 0).
func pdfEdits(data []byte) []variant {
	var vs []variant
	add := func(tag string, d []byte) {
		if d != nil {
			vs = append(vs, variant{tag, d})
		}
	}
	add("ln0", replaceFirst(data, reLength, "0"))
	if v, ok := firstNumber(data, reLength); ok {
		add("ln1", replaceFirst(data, reLength, strconv.Itoa(v+1)))
	}
	add("lnh", replaceFirst(data, reLength, "4294967295"))
	if v, ok := firstNumber(data, reStartX); ok {
		add("sx", replaceFirst(data, reStartX, strconv.Itoa(v+7)))
	}
	if v, ok := firstNumber(data, reXref); ok {
		add("xo", replaceFirst(data, reXref, fmt.Sprintf("%010d", v+3)))
	}
	add("iw", replaceFirst(data, reWidth, "1073741824"))
	add("ih", replaceFirst(data, reHeight, "0"))
	return vs
}

// putU32 returns a copy of data with the 4 bytes at off set to v (big endian).
func putU32(data []byte, off int, v uint32) []byte {
	d := clone(data)
	binary.BigEndian.PutUint32(d[off:], v)
	return d
}

// pngEdits: for the first three distinct chunk types after IHDR (not IEND), at chunk index i, `l<i>p`
// (length +1) and `l<i>x` (length 0xFFFFFFFF).
func pngEdits(data []byte) []variant {
	if len(data) < 8 || string(data[:8]) != "\x89PNG\r\n\x1a\n" {
		return nil
	}
	var vs []variant
	seen := map[string]bool{}
	off := 8
	for i := 0; off+12 <= len(data) && len(seen) < 3; i++ {
		l := int(binary.BigEndian.Uint32(data[off:]))
		typ := string(data[off+4 : off+8])
		if typ == "IEND" {
			break
		}
		if i > 0 && !seen[typ] {
			seen[typ] = true
			vs = append(vs,
				variant{fmt.Sprintf("l%dp", i), putU32(data, off, uint32(l)+1)},
				variant{fmt.Sprintf("l%dx", i), putU32(data, off, 0xFFFFFFFF)})
		}
		if l < 0 || l > len(data) {
			break
		}
		off += 12 + l
	}
	return vs
}

// jpegEdits: in the Exif block, the entry count of IFD0 set to 0, 1 and 0xFFFF (`xc0`, `xc1`, `xcf`)
// and the offset of IFD0 set to 0xFFFFFFF0 (`xof`).
func jpegEdits(data []byte) []variant {
	i := bytes.Index(data, []byte("Exif\x00\x00"))
	if i < 0 || i+6+8 > len(data) {
		return nil
	}
	tiff := i + 6
	var order binary.ByteOrder
	switch string(data[tiff : tiff+2]) {
	case "II":
		order = binary.LittleEndian
	case "MM":
		order = binary.BigEndian
	default:
		return nil
	}
	ifd := tiff + int(order.Uint32(data[tiff+4:]))
	var vs []variant
	if ifd >= 0 && ifd+2 <= len(data) {
		for _, c := range []struct {
			tag string
			v   uint16
		}{{"xc0", 0}, {"xc1", 1}, {"xcf", 0xFFFF}} {
			d := clone(data)
			order.PutUint16(d[ifd:], c.v)
			vs = append(vs, variant{c.tag, d})
		}
	}
	d := clone(data)
	order.PutUint32(d[tiff+4:], 0xFFFFFFF0)
	return append(vs, variant{"xof", d})
}

// jp2Edits: for each of the first three top-level boxes, `b<i>z` (length 0) and `b<i>h` (0xFFFFFFFF).
func jp2Edits(data []byte) []variant {
	var vs []variant
	off := 0
	for i := 0; i < 3 && off+8 <= len(data); i++ {
		l := int(binary.BigEndian.Uint32(data[off:]))
		vs = append(vs,
			variant{fmt.Sprintf("b%dz", i), putU32(data, off, 0)},
			variant{fmt.Sprintf("b%dh", i), putU32(data, off, 0xFFFFFFFF)})
		if l < 8 || l > len(data) {
			break
		}
		off += l
	}
	return vs
}

// jb2Edits: for each of the first three segments of an embedded JBIG2 stream (short-form headers
// only), `s<i>p` (data length +1) and `s<i>h` (0xFFFFFFFF).
func jb2Edits(data []byte) []variant {
	var vs []variant
	off := 0
	for i := 0; i < 3 && off+11 <= len(data); i++ {
		segnum := binary.BigEndian.Uint32(data[off:])
		flags := data[off+4]
		cnt := int(data[off+5] >> 5)
		if cnt > 4 {
			break
		}
		size := 1
		if segnum > 65536 {
			size = 4
		} else if segnum > 256 {
			size = 2
		}
		pos := off + 6 + cnt*size
		if flags&0x40 != 0 {
			pos += 4
		} else {
			pos++
		}
		if pos+4 > len(data) {
			break
		}
		dl := binary.BigEndian.Uint32(data[pos:])
		vs = append(vs,
			variant{fmt.Sprintf("s%dp", i), putU32(data, pos, dl+1)},
			variant{fmt.Sprintf("s%dh", i), putU32(data, pos, 0xFFFFFFFF)})
		if dl == 0xFFFFFFFF || int64(dl) > int64(len(data)) {
			break
		}
		off = pos + 4 + int(dl)
	}
	return vs
}

// pngFilterRows is pngFilterRows of build_test.go: 8-bit one-component rows, one PNG filter per
// row, zlib-compressed.
func pngFilterRows(rows [][]byte, filters []byte) []byte {
	var raw bytes.Buffer
	prev := make([]byte, len(rows[0]))
	for i, row := range rows {
		out := make([]byte, len(row))
		for j := range row {
			var a, c byte
			if j > 0 {
				a, c = row[j-1], prev[j-1]
			}
			b := prev[j]
			switch filters[i] {
			case 0:
				out[j] = row[j]
			case 1:
				out[j] = row[j] - a
			case 2:
				out[j] = row[j] - b
			case 3:
				out[j] = row[j] - byte((int(a)+int(b))/2)
			case 4:
				out[j] = row[j] - paeth(a, b, c)
			}
		}
		raw.WriteByte(filters[i])
		raw.Write(out)
		prev = row
	}
	var buf bytes.Buffer
	zw := zlib.NewWriter(&buf)
	zw.Write(raw.Bytes())
	zw.Close()
	return buf.Bytes()
}

func paeth(a, b, c byte) byte {
	p := int(a) + int(b) - int(c)
	abs := func(v int) int {
		if v < 0 {
			return -v
		}
		return v
	}
	pa, pb, pc := abs(p-int(a)), abs(p-int(b)), abs(p-int(c))
	switch {
	case pa <= pb && pa <= pc:
		return a
	case pb <= pc:
		return b
	}
	return c
}
