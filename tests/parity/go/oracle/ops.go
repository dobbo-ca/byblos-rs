package main

import (
	"bytes"
	"context"
	"fmt"
	"image"
	"strconv"
	"strings"

	"github.com/dobbo-ca/byblos"
)

// baseLim is the Limits of every operation without the `-tight` suffix: byblos's defaults, except
// that an image is at most 1<<25 pixels (the defaults allow 500 000 000, which a BuildKit pod
// cannot hold). The Rust oracle uses the same values.
func baseLim() byblos.Limits { return byblos.Limits{MaxImagePixels: 1 << 25} }

// tightLim is the preset of the `-tight` operations: small caps, so that the limit errors reach
// the `.err` outputs.
func tightLim() byblos.Limits {
	return byblos.Limits{
		MaxObjects:            20_000,
		MaxPages:              8,
		MaxStreamBytes:        1 << 20,
		MaxPageStreamBytes:    1 << 21,
		MaxImagePixels:        1 << 20,
		MaxFontProgramBytes:   1 << 18,
		MaxImages:             2,
		MaxInlineImages:       2,
		MaxUnresolved:         2,
		MaxRetainedBytes:      1 << 16,
		MaxEncodedImagePixels: 1 << 16,
		MaxPNGTextBytes:       1 << 10,
		MaxRasterSide:         256,
		MaxRasterPixels:       1 << 14,
		MaxRenderFontBytes:    1 << 18,
		MaxPathPoints:         1 << 12,
		MaxFillWork:           1 << 16,
		MaxGStateDepth:        4,
		MaxFormDepth:          1,
		MaxFormInvocations:    8,
		MaxOperands:           64,
		MaxTextShows:          64,
		MaxPaints:             64,
	}
}

// opsFor lists the operations of an input kind, in the order they run.
func opsFor(kind string) []op {
	var ops []op
	// add registers name with the base limits and, when tight is set, name-tight with tightLim.
	add := func(name string, tight bool, f func(in *input, lim byblos.Limits) ([]byte, error)) {
		ops = append(ops, op{name, func(in *input) ([]byte, error) { return f(in, baseLim()) }})
		if tight {
			ops = append(ops, op{name + "-tight", func(in *input) ([]byte, error) { return f(in, tightLim()) }})
		}
	}
	switch kind {
	case "pdf":
		add("inspect", true, opInspect)
		add("extract", true, opExtract)
		add("record", true, opRecord)
		add("render", true, opRender)
		add("optimize", true, optimizeOp(byblos.OptimizeOptions{}))
		add("optimize-lin", false, optimizeOp(byblos.OptimizeOptions{Linearize: true}))
		add("optimize-jpeg75", false, optimizeOp(byblos.OptimizeOptions{RecompressJPEG: true, JPEGQuality: 75}))
		add("optimize-lin-jpeg75", false, optimizeOp(byblos.OptimizeOptions{Linearize: true, RecompressJPEG: true, JPEGQuality: 75}))
		add("stamp", true, opStamp)
		add("replace", true, opReplace)
		add("edit", true, opEdit)
		add("provenance-write", false, opProvenanceWrite)
		add("provenance-read", false, opProvenanceRead)
	case "png", "jpg", "jp2":
		add("embed", true, opEmbed)
		add("rotation", true, opRotation)
		add("orientation", true, opOrientation)
		add("transcode", true, opTranscode)
		add("build", true, opBuild)
		add("decode", false, opDecode)
		add("sauvola", false, opSauvola)
		add("downsample-150", true, opDownsample(8))
		add("downsample-bpc1", false, opDownsample(1))
		add("quantize-png-2", false, opQuantizePNG(2))
		add("quantize-png-16", false, opQuantizePNG(16))
		add("quantize-png-256", false, opQuantizePNG(256))
		add("quantize-indexed-16", false, opQuantizeIndexed(16))
	case "pbm":
		add("jbig2-encode", false, opJBIG2Encode)
		add("jbig2-image", false, opJBIG2Image)
	case "jb2":
		add("jbig2-decode", false, opJBIG2Decode)
		add("jbig2-decode-globals", false, opJBIG2DecodeGlobals)
	case "content":
		add("lex", false, opLex)
		add("walk", true, opWalk)
	}
	return ops
}

// ---- PDF operations ----

func reader(in *input) *bytes.Reader { return bytes.NewReader(in.data) }

func opInspect(in *input, lim byblos.Limits) ([]byte, error) {
	di, err := byblos.InspectDocument(context.Background(), reader(in), lim)
	if err != nil {
		return nil, err
	}
	return dumpDocument(di), nil
}

func dumpDocument(di *byblos.DocumentInfo) []byte {
	var d dw
	d.b2("encrypted", di.Encrypted)
	d.b2("has_signature", di.HasSignature)
	d.b2("needs_rendering", di.NeedsRendering)
	d.n("pages", len(di.Pages))
	for i, p := range di.Pages {
		k := fmt.Sprintf("pages.%d.", i)
		d.i(k+"index", int64(p.Index))
		d.rect(k+"bounds", p.Bounds)
		d.i(k+"rotate", int64(p.Rotate))
		d.i(k+"text_chars", int64(p.TextChars))
		d.n(k+"images", len(p.Images))
		for j, im := range p.Images {
			ik := fmt.Sprintf("%simages.%d.", k, j)
			d.rect(ik+"bounds", im.Bounds)
			d.fs(ik+"placement", im.Placement[:])
			d.f(ik+"placement_deg", im.PlacementDeg)
			d.i(ik+"width", int64(im.Width))
			d.i(ik+"height", int64(im.Height))
			d.b2(ik+"bitonal", im.Bitonal)
			d.s(ik+"colorspace", im.ColorSpace)
			d.i(ik+"bpc", int64(im.BPC))
			d.s(ik+"filter", im.Filter)
			d.i(ik+"obj_nr", int64(im.ObjNr))
			d.b2(ik+"substitutable", im.Substitutable)
			d.b2(ik+"inline", im.Inline)
		}
		d.n(k+"diagnostics", len(p.Diagnostics))
		for j, dg := range p.Diagnostics {
			d.s(fmt.Sprintf("%sdiagnostics.%d.severity", k, j), dg.Severity.String())
			d.s(fmt.Sprintf("%sdiagnostics.%d.message", k, j), dg.Message)
		}
	}
	return d.bytes()
}

// pageCount is the page count by Inspect under the base limits; errSkip when Inspect fails.
func pageCount(in *input) (int, error) {
	di, err := byblos.InspectDocument(context.Background(), reader(in), baseLim())
	if err != nil {
		return 0, errSkip
	}
	return len(di.Pages), nil
}

// opExtract extracts the first maxPages pages (page 1 when Inspect fails), one section each:
// `== page <n>` and then `err <class>` or the raster, then `== stats` and the process-global
// counters (reset before the first page).
func opExtract(in *input, lim byblos.Limits) ([]byte, error) {
	n, err := pageCount(in)
	if err != nil || n < 1 {
		n = 1
	}
	n = min(n, maxPages)
	byblos.ResetExtractStats()
	var b bytes.Buffer
	for p := 1; p <= n; p++ {
		fmt.Fprintf(&b, "== page %d\n", p)
		pr, err := protect(func() (*byblos.PageRaster, error) {
			return byblos.ExtractPageRasterLimits(context.Background(), reader(in), p, lim)
		})
		if err != nil {
			fmt.Fprintf(&b, "err %s\n", errClass(err))
			continue
		}
		var d dw
		d.rect("bounds", pr.Bounds)
		d.rect("page", pr.Page)
		if pr.RasterQuad == nil {
			d.nilv("raster_quad")
		} else {
			d.fs("raster_quad", pr.RasterQuad[:])
		}
		d.i("obj_nr", int64(pr.ObjNr))
		d.i("dropped_annots", int64(pr.DroppedAnnots))
		d.b2("bitonal", pr.Bitonal)
		d.b2("covers_page", pr.CoversPage())
		b.Write(d.bytes())
		b.WriteString("image\n")
		b.Write(imageDump(pr.Image))
		b.WriteString("\n")
	}
	st := byblos.ExtractStats()
	b.WriteString("== stats\n")
	var d dw
	d.i("attempted", int64(st.Attempted))
	d.i("extracted", int64(st.Extracted))
	d.i("partial", int64(st.Partial))
	d.i("diverted", int64(st.Diverted))
	d.i("failed", int64(st.Failed))
	keys := make([]string, 0, len(st.Reasons))
	for k := range st.Reasons {
		keys = append(keys, k)
	}
	sortStrings(keys)
	d.n("reasons", len(keys))
	for i, k := range keys {
		d.s(fmt.Sprintf("reasons.%d.name", i), k)
		d.i(fmt.Sprintf("reasons.%d.count", i), int64(st.Reasons[k]))
	}
	b.Write(d.bytes())
	return b.Bytes(), nil
}

func opRecord(in *input, lim byblos.Limits) ([]byte, error) {
	p, err := byblos.RecordExtractionLimits(context.Background(), reader(in), lim)
	if err != nil {
		return nil, err
	}
	return dumpProvenance(&p), nil
}

func opRender(in *input, lim byblos.Limits) ([]byte, error) {
	img, err := byblos.RenderPageLimits(context.Background(), reader(in), 1, 400, lim)
	if err != nil {
		return nil, err
	}
	return imageDump(img), nil
}

func optimizeOp(opts byblos.OptimizeOptions) func(*input, byblos.Limits) ([]byte, error) {
	return func(in *input, lim byblos.Limits) ([]byte, error) {
		var out bytes.Buffer
		if err := byblos.OptimizeLimits(context.Background(), &out, reader(in), opts, lim); err != nil {
			return nil, err
		}
		return bufRaw(&out), nil
	}
}

// opStamp stamps one word, "Parity" in the rectangle (72,72)-(272,102), on every page.
func opStamp(in *input, lim byblos.Limits) ([]byte, error) {
	n, err := pageCount(in)
	if err != nil || n < 1 {
		return nil, errSkip
	}
	tl := byblos.TextLayer{Pages: make([][]byblos.PositionedWord, n)}
	for i := range tl.Pages {
		tl.Pages[i] = []byblos.PositionedWord{{Text: "Parity", Bounds: image.Rect(72, 72, 272, 102)}}
	}
	var out bytes.Buffer
	if err := byblos.StampTextLayerLimits(context.Background(), &out, reader(in), tl, lim); err != nil {
		return nil, err
	}
	return bufRaw(&out), nil
}

// replacementImage is the 8x8 DeviceGray image that replace swaps in: a FlateDecode stream made
// of one stored block, pixel i = (4*i) & 255, so the bytes do not depend on a deflate encoder.
func replacementImage() byblos.EncodedImage {
	pix := make([]byte, 64)
	for i := range pix {
		pix[i] = byte(4 * i)
	}
	a, b := uint32(1), uint32(0)
	for _, c := range pix {
		a = (a + uint32(c)) % 65521
		b = (b + a) % 65521
	}
	z := []byte{0x78, 0x01, 0x01, 0x40, 0x00, 0xbf, 0xff}
	z = append(z, pix...)
	z = append(z, byte(b>>8), byte(b), byte(a>>8), byte(a))
	return byblos.EncodedImage{
		Width: 8, Height: 8, BPC: 8,
		ColorSpace: byblos.ColorSpace{Name: "DeviceGray"},
		Filter:     "FlateDecode",
		Data:       z,
	}
}

// opReplace swaps the first Substitutable image of the document (in page order) for
// replacementImage().
func opReplace(in *input, lim byblos.Limits) ([]byte, error) {
	di, err := byblos.InspectDocument(context.Background(), reader(in), baseLim())
	if err != nil {
		return nil, errSkip
	}
	obj := 0
	for _, p := range di.Pages {
		for _, im := range p.Images {
			if obj == 0 && im.Substitutable {
				obj = im.ObjNr
			}
		}
	}
	if obj == 0 {
		return nil, errSkip
	}
	var out bytes.Buffer
	subs := map[int]byblos.EncodedImage{obj: replacementImage()}
	if err := byblos.ReplaceImagesLimits(context.Background(), &out, reader(in), subs, lim); err != nil {
		return nil, err
	}
	return bufRaw(&out), nil
}

// opEdit builds a document of the first maxEditPages pages in reverse order. The first output page
// has /Rotate 90, the second is straightened by 1.5 degrees, the rest have /Rotate 0.
func opEdit(in *input, lim byblos.Limits) ([]byte, error) {
	n, err := pageCount(in)
	if err != nil || n < 1 {
		return nil, errSkip
	}
	n = min(n, maxEditPages)
	src := reader(in)
	pages := make([]byblos.PageSource, 0, n)
	for p := n; p >= 1; p-- {
		ps := byblos.PageSource{Source: src, Page: p}
		switch len(pages) {
		case 0:
			ps.Rotate = 90
		case 1:
			ps.Straighten = &byblos.StraightenSpec{Deg: 1.5}
		}
		pages = append(pages, ps)
	}
	var out bytes.Buffer
	if err := byblos.BuildFromPagesLimits(context.Background(), &out, pages, lim); err != nil {
		return nil, err
	}
	return bufRaw(&out), nil
}

// syntheticProvenance is the record provenance-write stores. The strings carry the characters
// that encoding/json escapes (`<`, `>`, `&`, U+2028) and a multi-byte rune.
func syntheticProvenance() byblos.Provenance {
	clip := [4]float64{10, 20.5, 300, 400.25}
	quad := [8]float64{0, 0, 612, 1.5, 612.25, 792, -0.5, 790.75}
	return byblos.Provenance{
		Version:      byblos.Version,
		Capabilities: byblos.Capabilities(),
		ProcessedAt:  fixedTime(),
		Optimized:    "linearized",
		Pages: []byblos.PageProvenance{
			{Applied: []string{"extract-raster", "text-layer"}},
			{
				Diverted:      "a<b>&c é",
				Placement:     []float64{1, 0, 0, 1, 0.5, 2.25},
				DroppedAnnots: 2,
				Geometry: &byblos.PageGeometry{
					RasterBox:  [4]float64{0, 0, 612, 792},
					PageBox:    [4]float64{0, 0, 612, 792},
					ClipBox:    &clip,
					RasterQuad: &quad,
				},
				Straightened: &byblos.PageStraighten{Deg: -1.25},
			},
		},
	}
}

func provenanceWritten(in *input) ([]byte, error) {
	var out bytes.Buffer
	if err := byblos.WriteProvenanceContext(context.Background(), reader(in), &out, syntheticProvenance()); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

func opProvenanceWrite(in *input, _ byblos.Limits) ([]byte, error) {
	b, err := provenanceWritten(in)
	if err != nil {
		return nil, err
	}
	return raw(b), nil
}

// opProvenanceRead reads the provenance back from the output of provenance-write; no file when the
// write fails.
func opProvenanceRead(in *input, _ byblos.Limits) ([]byte, error) {
	b, err := provenanceWritten(in)
	if err != nil {
		return nil, errSkip
	}
	p, err := byblos.ReadProvenanceContext(context.Background(), bytes.NewReader(b))
	if err != nil {
		return nil, err
	}
	if p == nil {
		return []byte("nil\n"), nil
	}
	return dumpProvenance(p), nil
}

func dumpProvenance(p *byblos.Provenance) []byte {
	var d dw
	d.s("version", p.Version)
	d.strs("capabilities", p.Capabilities)
	d.s("processed_at", p.ProcessedAt.Format("2006-01-02T15:04:05.999999999Z07:00"))
	d.s("optimized", p.Optimized)
	d.n("pages", len(p.Pages))
	for i, pg := range p.Pages {
		k := fmt.Sprintf("pages.%d.", i)
		d.strs(k+"applied", pg.Applied)
		d.s(k+"diverted", pg.Diverted)
		d.fs(k+"placement", pg.Placement)
		d.i(k+"dropped_annots", int64(pg.DroppedAnnots))
		if pg.Geometry == nil {
			d.nilv(k + "geometry")
		} else {
			g := pg.Geometry
			d.fs(k+"geometry.raster_box", g.RasterBox[:])
			d.fs(k+"geometry.page_box", g.PageBox[:])
			if g.ClipBox == nil {
				d.nilv(k + "geometry.clip_box")
			} else {
				d.fs(k+"geometry.clip_box", g.ClipBox[:])
			}
			if g.RasterQuad == nil {
				d.nilv(k + "geometry.raster_quad")
			} else {
				d.fs(k+"geometry.raster_quad", g.RasterQuad[:])
			}
			d.b2(k+"geometry.covers_page", g.CoversPage())
		}
		if pg.Straightened == nil {
			d.nilv(k + "straightened")
		} else {
			d.f(k+"straightened.deg", pg.Straightened.Deg)
		}
	}
	return d.bytes()
}

// ---- image operations (PNG, JPEG, JP2) ----

func embedAny(in *input, lim byblos.Limits) (byblos.EncodedImage, byblos.Resolution, error) {
	switch in.kind {
	case "png":
		return byblos.EmbedPNGLimits(in.data, lim)
	case "jpg":
		return byblos.EmbedJPEG(in.data)
	}
	return byblos.EmbedJP2(in.data, lim)
}

func opEmbed(in *input, lim byblos.Limits) ([]byte, error) {
	img, res, err := embedAny(in, lim)
	if err != nil {
		return nil, err
	}
	var d dw
	dumpEncoded(&d, "", img)
	dumpResolution(&d, res)
	return d.bytes(), nil
}

func opRotation(in *input, lim byblos.Limits) ([]byte, error) {
	r, err := byblos.EXIFRotationLimits(in.data, lim)
	if err != nil {
		return nil, err
	}
	return []byte("rotation=" + strconv.Itoa(r) + "\n"), nil
}

func opOrientation(in *input, lim byblos.Limits) ([]byte, error) {
	o, err := byblos.EXIFOrientation(in.data, lim)
	if err != nil {
		return nil, err
	}
	var d dw
	d.i("rotate", int64(o.Rotate))
	d.b2("ambiguous", o.Ambiguous)
	return d.bytes(), nil
}

// opTranscode runs TranscodePNG on a PNG with no scratch callback: the Adam7 buffer comes from
// the memory budget, as Rust charges it to max_alloc (the Rust port has no callback).
func opTranscode(in *input, lim byblos.Limits) ([]byte, error) {
	if in.kind != "png" {
		return nil, errSkip
	}
	img, res, err := byblos.TranscodePNG(context.Background(), bytes.NewReader(in.data), lim, nil)
	if err != nil {
		return nil, err
	}
	var d dw
	dumpEncoded(&d, "", img)
	dumpResolution(&d, res)
	return d.bytes(), nil
}

// opBuild embeds the image and writes a one-page PDF: DPI and DPIY from the resolution, Rotate from
// EXIFRotationLimits (0 when that fails).
func opBuild(in *input, lim byblos.Limits) ([]byte, error) {
	pdf, err := buildFromImage(in, lim)
	if err != nil {
		return nil, err
	}
	return raw(pdf), nil
}

func buildFromImage(in *input, lim byblos.Limits) ([]byte, error) {
	img, res, err := embedAny(in, lim)
	if err != nil {
		return nil, err
	}
	rot, err := byblos.EXIFRotationLimits(in.data, lim)
	if err != nil {
		rot = 0
	}
	var out bytes.Buffer
	page := byblos.BuildPage{Image: img, DPI: res.X, DPIY: res.Y, Rotate: rot}
	if err := byblos.BuildPDFLimits(context.Background(), &out, []byblos.BuildPage{page}, lim); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

// loadImage is image.Decode, except that an image whose header declares more than maxDecodePixels
// is errSkip. If the header does not parse, Decode runs and its error is the result.
func loadImage(in *input) (image.Image, error) {
	if cfg, _, err := image.DecodeConfig(bytes.NewReader(in.data)); err == nil && int64(cfg.Width)*int64(cfg.Height) > maxDecodePixels {
		return nil, errSkip
	}
	img, _, err := image.Decode(bytes.NewReader(in.data))
	return img, err
}

func opDecode(in *input, _ byblos.Limits) ([]byte, error) {
	img, err := loadImage(in)
	if err != nil {
		return nil, err
	}
	return imageDump(img), nil
}

func opSauvola(in *input, _ byblos.Limits) ([]byte, error) {
	img, err := loadImage(in)
	if err != nil {
		return nil, err
	}
	m, err := byblos.Sauvola(img)
	if err != nil {
		return nil, err
	}
	return bitmapDump(m), nil
}

// opDownsample is DownsampleDeclaredBPCLimits from 300 to 150 DPI with the declared depth bpc.
func opDownsample(bpc int) func(*input, byblos.Limits) ([]byte, error) {
	return func(in *input, lim byblos.Limits) ([]byte, error) {
		img, err := loadImage(in)
		if err != nil {
			return nil, err
		}
		out, err := byblos.DownsampleDeclaredBPCLimits(context.Background(), img, bpc, 300, 150, lim)
		if err != nil {
			return nil, err
		}
		return imageDump(out), nil
	}
}

func opQuantizePNG(colors int) func(*input, byblos.Limits) ([]byte, error) {
	return func(in *input, _ byblos.Limits) ([]byte, error) {
		img, err := loadImage(in)
		if err != nil {
			return nil, err
		}
		b, err := byblos.QuantizePNG(img, colors)
		if err != nil {
			return nil, err
		}
		return raw(b), nil
	}
}

func opQuantizeIndexed(colors int) func(*input, byblos.Limits) ([]byte, error) {
	return func(in *input, _ byblos.Limits) ([]byte, error) {
		img, err := loadImage(in)
		if err != nil {
			return nil, err
		}
		e, err := byblos.QuantizeIndexed(img, colors)
		if err != nil {
			return nil, err
		}
		var d dw
		dumpEncoded(&d, "", e)
		return d.bytes(), nil
	}
}

// ---- bilevel operations (PBM, JBIG2) ----

// parsePBM reads a binary PBM (P4): the magic, width and height separated by white space (and
// `#` comments), one white-space byte, and exactly stride*height bytes. A side is at most 1<<14.
func parsePBM(data []byte) (*byblos.Bitmap, error) {
	pos := 0
	token := func() (string, bool) {
		for pos < len(data) {
			c := data[pos]
			if c == '#' {
				for pos < len(data) && data[pos] != '\n' {
					pos++
				}
			} else if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
				pos++
			} else {
				break
			}
		}
		start := pos
		for pos < len(data) && !strings.ContainsRune(" \t\n\r#", rune(data[pos])) {
			pos++
		}
		return string(data[start:pos]), pos > start
	}
	magic, ok := token()
	if !ok || magic != "P4" {
		return nil, errBadInput
	}
	ws, ok1 := token()
	hs, ok2 := token()
	w, err1 := strconv.Atoi(ws)
	h, err2 := strconv.Atoi(hs)
	if !ok1 || !ok2 || err1 != nil || err2 != nil || w < 1 || h < 1 || w > 1<<14 || h > 1<<14 || pos >= len(data) {
		return nil, errBadInput
	}
	pos++ // the single white-space byte
	m := byblos.NewBitmap(w, h)
	if len(data)-pos != len(m.Pix) {
		return nil, errBadInput
	}
	copy(m.Pix, data[pos:])
	return m, nil
}

var errBadInput = fmt.Errorf("oracle: bad pbm")

func opJBIG2Encode(in *input, _ byblos.Limits) ([]byte, error) {
	m, err := parsePBM(in.data)
	if err != nil {
		return nil, errSkip
	}
	b, err := byblos.EncodeJBIG2Generic(m)
	if err != nil {
		return nil, err
	}
	return raw(b), nil
}

func opJBIG2Image(in *input, _ byblos.Limits) ([]byte, error) {
	m, err := parsePBM(in.data)
	if err != nil {
		return nil, errSkip
	}
	e, err := byblos.EncodeJBIG2Image(m)
	if err != nil {
		return nil, err
	}
	var d dw
	dumpEncoded(&d, "", e)
	return d.bytes(), nil
}

func opJBIG2Decode(in *input, _ byblos.Limits) ([]byte, error) {
	m, err := byblos.DecodeJBIG2Generic(in.data)
	if err != nil {
		return nil, err
	}
	return bitmapDump(m), nil
}
