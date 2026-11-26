package main

import (
	"compress/flate"
	"compress/zlib"
	"context"
	"encoding/ascii85"
	"encoding/hex"
	"errors"
	"image/jpeg"
	"io"
	"strings"

	"github.com/dobbo-ca/byblos"
	"github.com/dobbo-ca/pdf-go/pkg/filter"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/validate"
)

// errClass is the stable class token of an error (docs/parity.md, "Error classes"). The tests are
// errors.Is and errors.As on the real types, from the most specific down. An error that is none
// of them (the untyped fmt.Errorf errors of byblos itself) is `error`.
func errClass(err error) string {
	var (
		le *byblos.LimitError
		ne *byblos.NotImplemented
		rr *byblos.RasterRefusal
	)
	switch {
	case errors.Is(err, errGoPanic):
		return "go-panic"
	case errors.Is(err, byblos.ErrPassword):
		return "password"
	case errors.As(err, &rr):
		return "refused/" + rr.Reason
	case errors.As(err, &le):
		return "limit/" + le.Name
	case errors.Is(err, byblos.ErrMemoryLimit):
		return "memory-limit"
	case errors.As(err, &ne):
		return "not-implemented/" + ne.Capability
	case errors.Is(err, byblos.ErrMirroredOrientation):
		return "mirrored-orientation"
	case errors.Is(err, byblos.ErrUnsupportedJBIG2Feature):
		return "jbig2/unsupported"
	case errors.Is(err, byblos.ErrUnstampableRune):
		return "unstampable-rune"
	case errors.Is(err, byblos.ErrNotSingleRaster):
		return "not-single-raster"
	case errors.Is(err, byblos.ErrUnsupportedImageCodec):
		return "unsupported-codec"
	case errors.Is(err, context.Canceled):
		return "cancelled"
	case errors.Is(err, byblos.ErrLimit):
		return "limit/unnamed"
	}
	if c := pdfClass(err); c != "" {
		return "pdf/" + c
	}
	return "error"
}

// pdfClass is the class of an error of pdf-go: `classify` of pdf-rs's oracle (docs/parity.md "Error
// classes"), so that Rust writes "pdf/" + pdf::Error::class(). It returns "" where pdf-rs's
// `classify` falls back to `syntax` (an untyped error): byblos's own untyped errors look the same,
// so they stay `error`, and the Rust side maps pdf::Error::Corrupt to `error` too. `panic` and
// `cancelled` are answered by errClass before this.
func pdfClass(err error) string {
	var (
		fe flate.CorruptInputError
		ae ascii85.CorruptInputError
		he hex.InvalidByteError
		je jpeg.FormatError
		ju jpeg.UnsupportedError
	)
	msg := err.Error()
	switch {
	case errors.Is(err, pdfcpu.ErrWrongPassword):
		return "wrong-password"
	case errors.Is(err, model.ErrObjectBytesExceeded):
		return "limit/object-bytes"
	case errors.Is(err, model.ErrParsedLimitExceeded):
		return "limit/parsed"
	case errors.Is(err, model.ErrStreamTotalLimitExceeded):
		return "limit/stream-total"
	case errors.Is(err, model.ErrLimitExceeded) && errors.Is(err, filter.ErrDecodeLimitExceeded):
		return "limit/total-decode"
	case errors.Is(err, model.ErrLimitExceeded) && strings.Contains(msg, "object stream entries"):
		return "limit/objstm-entries"
	case errors.Is(err, model.ErrLimitExceeded):
		return "limit/objects"
	case errors.Is(err, filter.ErrDecodeLimitExceeded):
		return "limit/decode"
	case strings.Contains(msg, "xref subsection object range exceeds limit"):
		return "limit/xref-range"
	case strings.Contains(msg, "pixel count") && strings.Contains(msg, "exceeds limit"):
		return "limit/image-pixels"
	case strings.Contains(msg, "byte size exceeds limit"):
		return "limit/image-bytes"
	case errors.Is(err, model.ErrNoContent):
		return "no-content"
	case errors.Is(err, filter.ErrUnsupportedFilter):
		return "unsupported-filter"
	case strings.Contains(msg, "invalid filter: <"):
		return "invalid-filter"
	case errors.Is(err, pdfcpu.ErrUnsupportedResource):
		return "unsupported-resource"
	case errors.Is(err, pdfcpu.ErrUnsupportedEncryptionFeature):
		return "encryption/unsupported"
	case errors.Is(err, pdfcpu.ErrMalformedEncryption):
		return "encryption/malformed"
	case errors.As(err, &fe):
		return "flate/corrupt"
	case errors.Is(err, zlib.ErrHeader):
		return "zlib/header"
	case errors.Is(err, zlib.ErrChecksum):
		return "flate/checksum"
	case errors.Is(err, zlib.ErrDictionary):
		return "zlib/dictionary"
	case errors.Is(err, io.ErrUnexpectedEOF):
		return "unexpected-eof"
	case strings.Contains(msg, "lzw: invalid code"):
		return "lzw/invalid-code"
	case errors.As(err, &ae):
		return "ascii85/corrupt"
	case strings.Contains(msg, "ASCII85 decode: missing eod marker"):
		return "ascii85/eod"
	case errors.As(err, &he):
		return "hex/invalid-byte"
	case errors.As(err, &ju):
		return "jpeg/unsupported"
	case errors.As(err, &je):
		return "jpeg/format"
	case strings.Contains(msg, "CCITT decode") || strings.Contains(msg, "ccitt:"):
		return "ccitt"
	case strings.Contains(msg, "gob:"):
		return "gob"
	case containsAny(msg, "flate decode: unexpected PNG predictor", "flate decode: postprocessing failed", "flate decode: read error, expected"):
		return "predictor/rows"
	case containsAny(msg, "flate decode: undefined \"Predictor\"", "flate decode: \"Colors\" must be", "flate decode: \"Columns\" must be",
		"flate decode: unexpected \"BitsPerComponent\"", "flate encode: TIFF predictor", "LZW decode: unsupported predictor"):
		return "predictor/params"
	case isSentinel(err):
		return "syntax"
	case strings.Contains(msg, "validation error"):
		return "validate"
	}
	return ""
}

func containsAny(msg string, subs ...string) bool {
	for _, s := range subs {
		if strings.Contains(msg, s) {
			return true
		}
	}
	return false
}

// sentinels are the pdf-go errors that stay `syntax` when validation wraps them.
var sentinels = []error{
	model.ErrActionCycle, model.ErrAnnotationIRTCycle, model.ErrBeadCycle, model.ErrCMapCycle,
	model.ErrColorSpaceCycle, model.ErrFormFieldCycle, model.ErrFormFieldDuplicate,
	model.ErrFunctionCycle, model.ErrHalftoneCycle, model.ErrMaxRecursionDepthExceeded,
	model.ErrMediaClipCycle, model.ErrNameTreeCycle, model.ErrNameTreeDuplicate,
	model.ErrNumberTreeCycle, model.ErrNumberTreeDuplicate, model.ErrPageTreeCycle,
	model.ErrPageTreeDuplicate, model.ErrPatternCycle, model.ErrRenditionCycle,
	model.ErrStructureTreeCycle, model.ErrStructureTreeDuplicate, model.ErrTargetCycle,
	model.ErrPageNotFound, model.ErrExpectedDict, model.ErrMissingEncryptDictObject,
	model.ErrWrongTypeEncryptDictObject, model.ErrCorruptObjectOffset,
	model.ErrXRefStreamIndexSizeMismatch, pdfcpu.ErrReferenceDoesNotExist,
	validate.ErrMissingFont, validate.ErrBookmarksRepair,
}

func isSentinel(err error) bool {
	for _, s := range sentinels {
		if errors.Is(err, s) {
			return true
		}
	}
	return containsAny(err.Error(), "corrupt page content", "corrupt TJ expression", "corrupt BI expression")
}
