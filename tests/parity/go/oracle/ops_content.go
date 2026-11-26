package main

import (
	"context"
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/dobbo-ca/byblos"
	"github.com/dobbo-ca/byblos/internal/content"
	"github.com/dobbo-ca/byblos/internal/jbig2"
)

func sortStrings(s []string) { sort.Strings(s) }

func readFileIn(dir, name string) ([]byte, bool) {
	b, err := os.ReadFile(filepath.Join(dir, name))
	return b, err == nil
}

// globalsName is the input file that jbig2-decode-globals pairs with every other JBIG2 stream: the
// page stream of the symbol fixtures names it as its globals.
const globalsName = "fx-jbig2-symbol-globals.jb2"

// opJBIG2DecodeGlobals is jbig2.DecodeEmbeddedStreamWithGlobals(globals, data) with the globals of
// globalsName, as byblos decodes a /JBIG2Globals stream. No file for the globals stream itself or
// when the input directory has no globals file. A Go jbig2.ErrUnsupportedFeature is wrapped as
// byblos.ErrUnsupportedJBIG2Feature, as byblos does (jbig2.go, markUnsupportedJBIG2).
func opJBIG2DecodeGlobals(in *input, _ byblos.Limits) ([]byte, error) {
	if in.name == globalsName {
		return nil, errSkip
	}
	globals, ok := readGlobals(in)
	if !ok {
		return nil, errSkip
	}
	m, err := jbig2.DecodeEmbeddedStreamWithGlobals(globals, in.data)
	if err != nil {
		if errors.Is(err, jbig2.ErrUnsupportedFeature) {
			return nil, fmt.Errorf("%w: %v", byblos.ErrUnsupportedJBIG2Feature, err)
		}
		return nil, err
	}
	return bitmapDump(&byblos.Bitmap{Width: m.W, Height: m.H, Stride: m.Stride, Pix: m.Pix}), nil
}

func readGlobals(in *input) ([]byte, bool) { return readFileIn(in.dir, globalsName) }

// maxTokens bounds the token dump of lex.
const maxTokens = 100_000

// opLex dumps the tokens of internal/content's Lexer, one per line:
// `<kind> <num as %016x> <text hex> <alias 0|1>`, then `eof`, or `error <quoted message>` at the
// first error, or `truncated` after maxTokens tokens.
func opLex(in *input, _ byblos.Limits) ([]byte, error) {
	var d dw
	lx := content.NewLexer(in.data)
	for i := 0; ; i++ {
		if i == maxTokens {
			d.b.WriteString("truncated\n")
			break
		}
		t, err := lx.Next()
		if errors.Is(err, io.EOF) {
			d.b.WriteString("eof\n")
			break
		}
		if err != nil {
			fmt.Fprintf(&d.b, "error %s\n", quote(err.Error()))
			break
		}
		alias := 0
		if t.Alias {
			alias = 1
		}
		fmt.Fprintf(&d.b, "%d %016x %x %d\n", t.Kind, math.Float64bits(t.Num), t.Text, alias)
	}
	return d.bytes(), nil
}

// mockEnv is the fixed Env of walk. XObject names: `Im*` is an image (ID from the name),
// `Fm0` a form with no nested form, `Fm1` a form that paints Fm0, `Fm2` a form that paints
// itself, any other `Fm*` like Fm0 (all forms have /Matrix [1 0 0 1 10 20], and /BBox
// [0 0 300 400] unless the name ends in `x`); every other name is unresolved. A graphics state
// name is opaque unless it ends in `1`. A font resolves to its ID unless it is `F9`.
type mockEnv struct{ lim byblos.Limits }

func (e mockEnv) Limits() byblos.Limits { return e.lim }

func nameID(name string) int {
	h := fnv.New32a()
	h.Write([]byte(name))
	return int(h.Sum32()%1000) + 1
}

func (e mockEnv) XObject(scope int, name string) (content.XObject, bool) {
	switch {
	case strings.HasPrefix(name, "Im"):
		return content.XObject{Image: true, ID: nameID(name)}, true
	case strings.HasPrefix(name, "Fm"):
		x := content.XObject{
			ID:     nameID(name),
			Matrix: content.Matrix{1, 0, 0, 1, 10, 20},
			Scope:  scope + 1,
		}
		if !strings.HasSuffix(name, "x") {
			x.BBox = &content.Box{LLX: 0, LLY: 0, URX: 300, URY: 400}
		}
		body := "q 2 0 0 2 5 5 cm /Im1 Do Q BT /F1 10 Tf (nested) Tj ET 0 0 50 50 re f\n"
		switch name {
		case "Fm1":
			body += "/Fm0 Do\n"
		case "Fm2":
			body += "/Fm2 Do\n"
		}
		x.Content = []byte(body)
		return x, true
	}
	return content.XObject{}, false
}

func (e mockEnv) ExtGStateOpaque(scope int, name string) bool { return !strings.HasSuffix(name, "1") }

func (e mockEnv) Font(scope int, name string) (int, bool) { return nameID(name), name != "F9" }

// opWalk dumps the Scan of content.Walk(ctx, data, 0, mockEnv{lim}), or the class of its error.
func opWalk(in *input, lim byblos.Limits) ([]byte, error) {
	s, err := content.Walk(context.Background(), in.data, 0, mockEnv{lim: lim})
	if err != nil {
		return nil, err
	}
	var d dw
	placements := func(k string, ps []content.Placement) {
		d.n(k, len(ps))
		for i, p := range ps {
			pk := fmt.Sprintf("%s.%d.", k, i)
			d.s(pk+"name", p.Name)
			d.i(pk+"id", int64(p.ID))
			d.fs(pk+"ctm", p.CTM[:])
			dumpBox(&d, pk+"box", &p.Box)
			d.b2(pk+"inline", p.Inline)
			dumpBox(&d, pk+"clip", p.Clip)
			d.b2(pk+"opaque", p.Opaque)
			d.i(pk+"index", int64(p.Index))
		}
	}
	placements("images", s.Images)
	placements("inline_images", s.InlineImages)
	d.i("text_chars", int64(s.TextChars))
	d.i("text_ops", int64(s.TextOps))
	d.i("inked_text_ops", int64(s.InkedTextOps))
	d.n("text_shows", len(s.TextShows))
	for i, t := range s.TextShows {
		k := fmt.Sprintf("text_shows.%d.", i)
		d.hex(k+"raw", t.Raw)
		d.s(k+"font", t.Font)
		d.i(k+"font_id", int64(t.FontID))
		d.f(k+"size", t.Size)
		d.f(k+"char_spacing", t.CharSpacing)
		d.f(k+"word_spacing", t.WordSpacing)
		d.f(k+"hscale", t.Hscale)
		d.f(k+"rise", t.Rise)
		d.i(k+"mode", int64(t.Mode))
		d.fs(k+"trm", t.Trm[:])
		d.i(k+"index", int64(t.Index))
	}
	d.n("paints", len(s.Paints))
	for i, p := range s.Paints {
		k := fmt.Sprintf("paints.%d.", i)
		d.s(k+"op", p.Op)
		dumpBox(&d, k+"box", &p.Box)
		dumpColor(&d, k+"fill", p.Fill)
		dumpColor(&d, k+"stroke", p.Stroke)
		d.i(k+"index", int64(p.Index))
		dumpBox(&d, k+"clip", p.Clip)
		d.fs(k+"ctm", p.CTM[:])
	}
	d.i("shading_ops", int64(s.ShadingOps))
	d.i("inline_imgs", int64(s.InlineImgs))
	d.strs("unresolved", s.Unresolved)
	return d.bytes(), nil
}

func dumpBox(d *dw, k string, b *content.Box) {
	if b == nil {
		d.nilv(k)
		return
	}
	d.fs(k, []float64{b.LLX, b.LLY, b.URX, b.URY})
}

func dumpColor(d *dw, k string, c content.Color) {
	d.s(k+".space", c.Space)
	d.fs(k+".comps", c.Comps)
}
