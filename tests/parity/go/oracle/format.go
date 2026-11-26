package main

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"image"
	"image/color"
	"math"
	"strings"

	"github.com/dobbo-ca/byblos"
)

// dw writes the canonical text of a struct result: one `key=value` line each. Integers are
// decimal, booleans `true`/`false`, floats `%016x` of math.Float64bits, byte strings lower-case
// hex, and text is quote()d. A slice is `<key>.len=N` and then `<key>.<i>=...`; a nil pointer is
// `<key>=nil`.
type dw struct{ b bytes.Buffer }

func (d *dw) s(k, v string)       { fmt.Fprintf(&d.b, "%s=%s\n", k, quote(v)) }
func (d *dw) i(k string, v int64) { fmt.Fprintf(&d.b, "%s=%d\n", k, v) }
func (d *dw) b2(k string, v bool) { fmt.Fprintf(&d.b, "%s=%t\n", k, v) }
func (d *dw) f(k string, v float64) {
	fmt.Fprintf(&d.b, "%s=%016x\n", k, math.Float64bits(v))
}
func (d *dw) hex(k string, v []byte) { fmt.Fprintf(&d.b, "%s=%s\n", k, hex.EncodeToString(v)) }
func (d *dw) nilv(k string)          { fmt.Fprintf(&d.b, "%s=nil\n", k) }
func (d *dw) n(k string, n int)      { fmt.Fprintf(&d.b, "%s.len=%d\n", k, n) }
func (d *dw) fs(k string, v []float64) {
	d.n(k, len(v))
	for i, x := range v {
		d.f(fmt.Sprintf("%s.%d", k, i), x)
	}
}
func (d *dw) strs(k string, v []string) {
	d.n(k, len(v))
	for i, x := range v {
		d.s(fmt.Sprintf("%s.%d", k, i), x)
	}
}
func (d *dw) rect(k string, r image.Rectangle) {
	fmt.Fprintf(&d.b, "%s=%d %d %d %d\n", k, r.Min.X, r.Min.Y, r.Max.X, r.Max.Y)
}
func (d *dw) bytes() []byte { return d.b.Bytes() }

// quote is the text format: printable ASCII as is (except `"` and `\`, which are escaped), `\n`,
// `\r` and `\t`, and every other byte (including every byte of a UTF-8 sequence) as `\xNN`,
// all inside double quotes.
func quote(s string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c == '"':
			b.WriteString(`\"`)
		case c == '\\':
			b.WriteString(`\\`)
		case c == '\n':
			b.WriteString(`\n`)
		case c == '\r':
			b.WriteString(`\r`)
		case c == '\t':
			b.WriteString(`\t`)
		case c >= 0x20 && c < 0x7f:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, `\x%02x`, c)
		}
	}
	b.WriteByte('"')
	return b.String()
}

// plane writes `<name> <len>` and a newline, then the bytes themselves and no newline when there
// are at most bigOutput of them, or ` sha256 <hex>` and a newline in place of them.
func plane(b *bytes.Buffer, name string, pix []byte) {
	if len(pix) > bigOutput {
		fmt.Fprintf(b, "%s %d sha256 %s\n", name, len(pix), sha(pix))
		return
	}
	fmt.Fprintf(b, "%s %d\n", name, len(pix))
	b.Write(pix)
}

// imageDump is the canonical form of an image: the header line `<type> <w> <h> <stride>`, the
// line `min <x> <y>`, the lines the type adds, and the pixel planes (plane()). Types and their
// Pix layouts are Go's: gray, gray16, alpha, alpha16, rgba, rgba64, nrgba, nrgba64, cmyk,
// paletted (line `palette <n> <hex of RGBA bytes>`), ycbcr (line `ycbcr <cstride> <ratio>` and the
// planes y, cb, cr). Any other type is `other` with the pixels as nrgba64.
func imageDump(img image.Image) []byte {
	var b bytes.Buffer
	hdr := func(t string, r image.Rectangle, stride int) {
		fmt.Fprintf(&b, "%s %d %d %d\nmin %d %d\n", t, r.Dx(), r.Dy(), stride, r.Min.X, r.Min.Y)
	}
	switch m := img.(type) {
	case *image.Gray:
		hdr("gray", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.Gray16:
		hdr("gray16", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.Alpha:
		hdr("alpha", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.Alpha16:
		hdr("alpha16", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.RGBA:
		hdr("rgba", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.RGBA64:
		hdr("rgba64", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.NRGBA:
		hdr("nrgba", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.NRGBA64:
		hdr("nrgba64", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.CMYK:
		hdr("cmyk", m.Rect, m.Stride)
		plane(&b, "pix", m.Pix)
	case *image.Paletted:
		hdr("paletted", m.Rect, m.Stride)
		var p bytes.Buffer
		for _, c := range m.Palette {
			r, g, bl, a := c.RGBA()
			p.Write([]byte{byte(r >> 8), byte(g >> 8), byte(bl >> 8), byte(a >> 8)})
		}
		fmt.Fprintf(&b, "palette %d %s\n", len(m.Palette), hex.EncodeToString(p.Bytes()))
		plane(&b, "pix", m.Pix)
	case *image.YCbCr:
		hdr("ycbcr", m.Rect, m.YStride)
		fmt.Fprintf(&b, "ycbcr %d %s\n", m.CStride, m.SubsampleRatio)
		plane(&b, "y", m.Y)
		plane(&b, "cb", m.Cb)
		plane(&b, "cr", m.Cr)
	default:
		r := img.Bounds()
		n := image.NewNRGBA64(image.Rect(0, 0, r.Dx(), r.Dy()))
		for y := 0; y < r.Dy(); y++ {
			for x := 0; x < r.Dx(); x++ {
				n.Set(x, y, color.NRGBA64Model.Convert(img.At(r.Min.X+x, r.Min.Y+y)))
			}
		}
		hdr("other", r, n.Stride)
		plane(&b, "pix", n.Pix)
	}
	return b.Bytes()
}

// bitmapDump is the canonical form of a bitmap: `bitmap <w> <h> <stride>` and the plane pix.
func bitmapDump(m *byblos.Bitmap) []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, "bitmap %d %d %d\n", m.Width, m.Height, m.Stride)
	plane(&b, "pix", m.Pix)
	return b.Bytes()
}

// dumpEncoded writes an EncodedImage under the key prefix p (data is its length and SHA-256).
func dumpEncoded(d *dw, p string, e byblos.EncodedImage) {
	d.i(p+"width", int64(e.Width))
	d.i(p+"height", int64(e.Height))
	d.i(p+"bpc", int64(e.BPC))
	d.s(p+"colorspace.name", e.ColorSpace.Name)
	d.s(p+"colorspace.base", e.ColorSpace.Base)
	d.i(p+"colorspace.hival", int64(e.ColorSpace.HiVal))
	d.hex(p+"colorspace.lookup", e.ColorSpace.Lookup)
	d.s(p+"filter", e.Filter)
	if e.DecodeParms == nil {
		d.nilv(p + "decodeparms")
	} else {
		d.i(p+"decodeparms.predictor", int64(e.DecodeParms.Predictor))
		d.i(p+"decodeparms.colors", int64(e.DecodeParms.Colors))
		d.i(p+"decodeparms.bpc", int64(e.DecodeParms.BitsPerComponent))
		d.i(p+"decodeparms.columns", int64(e.DecodeParms.Columns))
	}
	d.fs(p+"decode", e.Decode)
	d.i(p+"data_len", int64(len(e.Data)))
	d.s(p+"data_sha256", sha(e.Data))
	if e.SMask == nil {
		d.nilv(p + "smask")
	} else {
		dumpEncoded(d, p+"smask.", *e.SMask)
	}
}

func dumpResolution(d *dw, r byblos.Resolution) {
	d.f("resolution.x", r.X)
	d.f("resolution.y", r.Y)
}
