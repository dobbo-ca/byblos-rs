Fixtures of the sfnt port (src/render/sfnt/tests.rs).

- `CFFTest.otf`, `cmapTest.ttf`, `glyfTest.ttf`: golang.org/x/image v0.47.0 `font/testdata`
  (BSD-3-Clause, Go Authors), the fonts of its sfnt tests.
- `Go-Regular.ttf`, `Go-Bold.ttf`, `Go-Mono.ttf`: golang.org/x/image v0.47.0
  `font/gofont/ttfs` (the Go fonts, Bigelow & Holmes, BSD-3-Clause).
- `glyphless.ttf`: byblos `internal/glyphless/glyphless.ttf`, the font of the OCR text layer.

The Liberation fonts byblos renders with are read from `src/render/fonts/`.
