//! Resource limits, the checks that use them, and cancellation.
//!
//! Ports `internal/limits/limits.go` without the process-global memory pool (`Reserve`,
//! `SetMemoryLimit`, `ErrMemoryLimit`, the cgroup default), the root `limits.go`, and
//! `context.go`. The pool is replaced by the per-call [`Limits::max_alloc`] (docs/limits.md and
//! docs/go-differences.md). `LimitError` is [`Error::Limit`].

use crate::error::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// Go's `DefaultFontProgramBytes`: 16 MiB.
pub const DEFAULT_FONT_PROGRAM_BYTES: i64 = 16 << 20;

/// `PerObjectBytes`: the object-table cost reserved per trailer `/Size` entry. It feeds the
/// default of [`Limits::max_parsed_bytes`] only.
pub const PER_OBJECT_BYTES: i64 = 1536;

/// Bounds one call. Every cap is checked from a header before the allocation it guards.
///
/// The fields Go has are `i64` and keep Go's rule: a value `<= 0` takes the default
/// ([`Limits::resolved`] applies it). `Limits::default()` holds Go's default for every field
/// whose default is a constant, except `max_gstate_depth`, `max_form_depth` and
/// `max_form_invocations`, which are 0 (inspect's `defaultWalkCap` tests `<= 0`); a field whose
/// default depends on another one (`max_images`,
/// `max_inline_images`, `max_unresolved`, `max_object_stream_entries`,
/// `max_total_decode_bytes`, `max_object_bytes`, `max_parsed_bytes`) is 0 there, which means
/// "derive it", exactly as Go's zero value does. `max_stream_total_bytes` is 0 = off.
///
/// `max_alloc` and `max_threads` are the two fields every port shares; see their docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Caps the trailer `/Size` (error name `objects`). Default 200 000.
    pub max_objects: i64,
    /// Caps the page count (`pages`). Default 10 000.
    pub max_pages: i64,
    /// Caps one decoded non-image stream (`stream-bytes`). Default 64 MiB.
    pub max_stream_bytes: i64,
    /// Caps the decoded non-image streams of one page together (`page-stream-bytes`). Default
    /// 128 MiB.
    pub max_page_stream_bytes: i64,
    /// Caps width x height of one image (`image-pixels`). Default 500 000 000.
    pub max_image_pixels: i64,
    /// Caps one decoded embedded font program. Default 16 MiB.
    pub max_font_program_bytes: i64,
    /// Caps the entries one page's content walk records in `Scan.Images` (`images`). Default
    /// `max_page_stream_bytes / 512` (262 144). Past it the call fails: dropping images would
    /// change classification.
    pub max_images: i64,
    /// As `max_images`, for inline images.
    pub max_inline_images: i64,
    /// As `max_images`, for unresolved `Do` operands.
    pub max_unresolved: i64,
    /// Caps the objects packed in object streams, summed over one parse. Default: `max_objects`,
    /// and never above it.
    pub max_object_stream_entries: i64,
    /// Caps the bytes pdf-rs decodes over one parse. Default: `max_stream_bytes`.
    pub max_total_decode_bytes: i64,
    /// Caps one indirect object read into memory, stream payload excluded. Default:
    /// `max_stream_bytes`.
    pub max_object_bytes: i64,
    /// Caps the estimated heap of the objects pdf-rs parses over one parse. Default 0, which
    /// means three quarters of the object table (`PER_OBJECT_BYTES` times the trailer `/Size`)
    /// plus `max_total_decode_bytes`; see [`Limits::parsed_bytes`].
    pub max_parsed_bytes: i64,
    /// Caps the encoded stream bytes pdf-rs reads over one parse. Default 0 = off.
    pub max_stream_total_bytes: i64,
    /// Caps what the content walk of one page keeps (`retained-bytes`). Default 128 MiB.
    pub max_retained_bytes: i64,
    /// Caps width x height of a PNG that `embed_png` lifts and of an `EncodedImage` that
    /// `build_pdf` writes (`encoded-image-pixels`). Default 1 << 26.
    pub max_encoded_image_pixels: i64,
    /// Caps the text decoded from one PNG's text chunks (`png-text-bytes`). Default 64 MiB.
    pub max_png_text_bytes: i64,
    /// Caps the `render_page` canvas in pixels a side (`raster-side`). Default 20 000.
    pub max_raster_side: i64,
    /// Caps the `render_page` canvas in total (`raster-pixels`). Default 1 << 26.
    pub max_raster_pixels: i64,
    /// Caps the font bytes one render retains. Past it the render skips further fonts and
    /// carries on. Default 16 MiB.
    pub max_render_font_bytes: i64,
    /// Caps the flattened device points a render holds for the current path (`path-points`).
    /// Default 1 << 21.
    pub max_path_points: i64,
    /// Caps a render's scanline filling, one unit per active edge per row (`fill-work`).
    /// Default 1 << 28.
    pub max_fill_work: i64,
    /// Caps `q` nesting in the content walk and the render (`q-depth`). Default (0): 1024.
    pub max_gstate_depth: i64,
    /// Caps Form XObject nesting in the content walk (`form-depth`). Default (0): 8.
    pub max_form_depth: i64,
    /// Caps the Form XObject invocations of one page walk (`form-invocations`). Default (0): 4096.
    pub max_form_invocations: i64,
    /// Bounds the pending-operand buffer of the walk and the render. Default 8192. Past it the
    /// walk stops recording and carries on.
    pub max_operands: i64,
    /// Bounds the walk's recorded `Scan.TextShows`. Default 1 << 16.
    pub max_text_shows: i64,
    /// Bounds the walk's recorded `Scan.Paints`. Default 1 << 18.
    pub max_paints: i64,
    /// The peak working memory of one call in bytes, all threads together, including nested
    /// calls into pdf-rs, jpeg-rs and deflate-rs (they are charged to the same
    /// [`pdf::Mem`], see [`Limits::mem`]). Over it: [`Error::Alloc`]. Replaces Go's
    /// process-global pool. 0 takes the default. Default 4 GiB, as pdf-rs.
    pub max_alloc: u64,
    /// The most threads one call uses, the calling thread included. 1 spawns no thread and
    /// touches no pool; 0 counts as 1. Default `std::thread::available_parallelism`, read once.
    /// The output is the same for every value. byblos itself is serial in phase 1; the value is
    /// passed to pdf-rs and jpeg-rs.
    pub max_threads: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_objects: 200_000,
            max_pages: 10_000,
            max_stream_bytes: 64 << 20,
            max_page_stream_bytes: 128 << 20,
            max_image_pixels: 500_000_000,
            max_font_program_bytes: DEFAULT_FONT_PROGRAM_BYTES,
            max_images: 0,
            max_inline_images: 0,
            max_unresolved: 0,
            max_object_stream_entries: 0,
            max_total_decode_bytes: 0,
            max_object_bytes: 0,
            max_parsed_bytes: 0,
            max_stream_total_bytes: 0,
            max_retained_bytes: 128 << 20,
            max_encoded_image_pixels: 1 << 26,
            max_png_text_bytes: 64 << 20,
            max_raster_side: 20_000,
            max_raster_pixels: 1 << 26,
            max_render_font_bytes: 16 << 20,
            max_path_points: 1 << 21,
            max_fill_work: 1 << 28,
            max_gstate_depth: 0,
            max_form_depth: 0,
            max_form_invocations: 0,
            max_operands: 8192,
            max_text_shows: 1 << 16,
            max_paints: 1 << 18,
            max_alloc: pdf::config::DEFAULT_MAX_ALLOC,
            max_threads: pdf::config::default_threads(),
        }
    }
}

/// Go's `def`: `v` if positive, else the default `d`.
fn def(v: i64, d: i64) -> i64 {
    if v > 0 { v } else { d }
}

impl Limits {
    /// `Objects()`.
    pub fn objects(&self) -> i64 {
        def(self.max_objects, 200_000)
    }
    /// `Pages()`.
    pub fn pages(&self) -> i64 {
        def(self.max_pages, 10_000)
    }
    /// `StreamBytes()`.
    pub fn stream_bytes(&self) -> i64 {
        def(self.max_stream_bytes, 64 << 20)
    }
    /// `PageStreamBytes()`.
    pub fn page_stream_bytes(&self) -> i64 {
        def(self.max_page_stream_bytes, 128 << 20)
    }
    /// `ImagePixels()`.
    pub fn image_pixels(&self) -> i64 {
        def(self.max_image_pixels, 500_000_000)
    }
    /// `Images()`.
    pub fn images(&self) -> i64 {
        def(self.max_images, self.page_stream_bytes() / 512)
    }
    /// `InlineImages()`.
    pub fn inline_images(&self) -> i64 {
        def(self.max_inline_images, self.page_stream_bytes() / 512)
    }
    /// `Unresolved()`.
    pub fn unresolved(&self) -> i64 {
        def(self.max_unresolved, self.page_stream_bytes() / 512)
    }
    /// `RetainedBytes()`.
    pub fn retained_bytes(&self) -> i64 {
        def(self.max_retained_bytes, 128 << 20)
    }
    /// `EncodedImagePixels()`.
    pub fn encoded_image_pixels(&self) -> i64 {
        def(self.max_encoded_image_pixels, 1 << 26)
    }
    /// `PNGTextBytes()`.
    pub fn png_text_bytes(&self) -> i64 {
        def(self.max_png_text_bytes, 64 << 20)
    }
    /// `RasterSide()`.
    pub fn raster_side(&self) -> i64 {
        def(self.max_raster_side, 20_000)
    }
    /// `RasterPixels()`.
    pub fn raster_pixels(&self) -> i64 {
        def(self.max_raster_pixels, 1 << 26)
    }
    /// `RenderFontBytes()`.
    pub fn render_font_bytes(&self) -> i64 {
        def(self.max_render_font_bytes, 16 << 20)
    }
    /// `PathPoints()`.
    pub fn path_points(&self) -> i64 {
        def(self.max_path_points, 1 << 21)
    }
    /// `FillWork()`.
    pub fn fill_work(&self) -> i64 {
        def(self.max_fill_work, 1 << 28)
    }
    /// `GStateDepth()`.
    pub fn gstate_depth(&self) -> i64 {
        def(self.max_gstate_depth, 1024)
    }
    /// `FormDepth()`.
    pub fn form_depth(&self) -> i64 {
        def(self.max_form_depth, 8)
    }
    /// `FormInvocations()`.
    pub fn form_invocations(&self) -> i64 {
        def(self.max_form_invocations, 4096)
    }
    /// `Operands()`.
    pub fn operands(&self) -> i64 {
        def(self.max_operands, 8192)
    }
    /// `TextShows()`.
    pub fn text_shows(&self) -> i64 {
        def(self.max_text_shows, 1 << 16)
    }
    /// `Paints()`.
    pub fn paints(&self) -> i64 {
        def(self.max_paints, 1 << 18)
    }
    /// `ObjectStreamEntries()`: the own value or `objects()`, never above `objects()`.
    pub fn object_stream_entries(&self) -> i64 {
        def(self.max_object_stream_entries, self.objects()).min(self.objects())
    }
    /// `TotalDecodeBytes()`.
    pub fn total_decode_bytes(&self) -> i64 {
        def(self.max_total_decode_bytes, self.stream_bytes())
    }
    /// `ObjectBytes()`.
    pub fn object_bytes(&self) -> i64 {
        def(self.max_object_bytes, self.stream_bytes())
    }
    /// `FontProgramBytes()`.
    pub fn font_program_bytes(&self) -> i64 {
        def(self.max_font_program_bytes, DEFAULT_FONT_PROGRAM_BYTES)
    }
    /// `max_alloc` with 0 taken as the default.
    pub fn alloc_limit(&self) -> u64 {
        if self.max_alloc == 0 {
            pdf::config::DEFAULT_MAX_ALLOC
        } else {
            self.max_alloc
        }
    }
    /// `max_threads` with 0 counted as 1.
    pub fn threads(&self) -> usize {
        self.max_threads.max(1)
    }

    /// `pdfdoc.parsedBytes`: `max_parsed_bytes` if positive, else three quarters of the object
    /// table the trailer `/Size` of `size` objects implies, plus `total_decode_bytes()`.
    pub fn parsed_bytes(&self, size: i64) -> i64 {
        if self.max_parsed_bytes > 0 {
            return self.max_parsed_bytes;
        }
        object_table_bytes(size) / 4 * 3 + self.total_decode_bytes()
    }

    /// The same limits with every `<= 0` field replaced by its default, so each field holds the
    /// value in force. `max_parsed_bytes` stays as given, because its default depends on the
    /// document (see [`Limits::parsed_bytes`]); `max_stream_total_bytes` is clamped at 0 (off).
    pub fn resolved(&self) -> Limits {
        Limits {
            max_objects: self.objects(),
            max_pages: self.pages(),
            max_stream_bytes: self.stream_bytes(),
            max_page_stream_bytes: self.page_stream_bytes(),
            max_image_pixels: self.image_pixels(),
            max_font_program_bytes: self.font_program_bytes(),
            max_images: self.images(),
            max_inline_images: self.inline_images(),
            max_unresolved: self.unresolved(),
            max_object_stream_entries: self.object_stream_entries(),
            max_total_decode_bytes: self.total_decode_bytes(),
            max_object_bytes: self.object_bytes(),
            max_parsed_bytes: self.max_parsed_bytes.max(0),
            max_stream_total_bytes: self.max_stream_total_bytes.max(0),
            max_retained_bytes: self.retained_bytes(),
            max_encoded_image_pixels: self.encoded_image_pixels(),
            max_png_text_bytes: self.png_text_bytes(),
            max_raster_side: self.raster_side(),
            max_raster_pixels: self.raster_pixels(),
            max_render_font_bytes: self.render_font_bytes(),
            max_path_points: self.path_points(),
            max_fill_work: self.fill_work(),
            max_gstate_depth: self.gstate_depth(),
            max_form_depth: self.form_depth(),
            max_form_invocations: self.form_invocations(),
            max_operands: self.operands(),
            max_text_shows: self.text_shows(),
            max_paints: self.paints(),
            max_alloc: self.alloc_limit(),
            max_threads: self.threads(),
        }
    }

    /// The pdf-rs limits that `pdfdoc.openSized` and `defaultConfig` write (`pdfdoc.go:81-93`,
    /// `:447-456`), for a document whose trailer `/Size` is `size`. Use `self.objects()` for
    /// `size` where Go's `defaultConfig` runs without a document. The other pdf-rs fields keep
    /// their defaults, as in Go. `max_alloc` and `max_threads` pass through.
    pub fn pdf_limits(&self, size: i64) -> pdf::Limits {
        pdf::Limits {
            max_decode_bytes: self.stream_bytes() as u64,
            max_object_count: self.objects() as u64,
            max_object_stream_total_count: self.object_stream_entries() as u64,
            max_total_decode_bytes: self.total_decode_bytes() as u64,
            max_object_bytes: self.object_bytes() as u64,
            max_parsed_bytes: self.parsed_bytes(size) as u64,
            max_stream_total_bytes: self.max_stream_total_bytes.max(0) as u64,
            max_alloc: self.alloc_limit(),
            max_threads: self.threads(),
            ..pdf::Limits::default()
        }
    }

    /// [`Limits::pdf_limits`] in a `pdf::Config`.
    pub fn pdf_config(&self, size: i64) -> pdf::Config {
        pdf::Config {
            limits: self.pdf_limits(size),
            ..pdf::Config::default()
        }
    }

    /// A fresh ledger of `max_alloc` bytes. Pass it (or its clones) to pdf-rs, jpeg-rs and
    /// deflate-rs so one call charges byblos and the nested crates together.
    pub fn mem(&self) -> pdf::Mem {
        pdf::Mem::with_max(self.alloc_limit())
    }

    /// `n` zero bytes, refused with [`Error::Alloc`] when `n` is over `max_alloc` and never
    /// allocated by `vec![0; n]` on an unchecked size. This checks one buffer against the whole
    /// budget; code that holds several buffers charges a shared [`Limits::mem`] ledger instead.
    ///
    /// # Errors
    /// [`Error::Alloc`] if `n` is over `max_alloc`, [`Error::OutOfMemory`] if the allocator
    /// refuses.
    pub fn alloc_zeroed(&self, n: usize) -> Result<Vec<u8>> {
        let max = self.alloc_limit();
        if n as u64 > max {
            return Err(Error::Alloc { got: n as u64, max });
        }
        let mut v = Vec::new();
        v.try_reserve_exact(n)?;
        v.resize(n, 0);
        Ok(v)
    }
}

/// `ObjectTableBytes`: the reservation for a document declaring `size` objects. Saturates where
/// Go's `int64` product would wrap.
pub fn object_table_bytes(size: i64) -> i64 {
    size.saturating_mul(PER_OBJECT_BYTES)
}

/// `MulSat`: `a * b` for non-negative `a` and `b`, saturating at `i64::MAX`.
pub fn mul_sat(a: i64, b: i64) -> i64 {
    if a != 0 && b > i64::MAX / a {
        return i64::MAX;
    }
    a.wrapping_mul(b)
}

/// `Check`: an [`Error::Limit`] named `name` when `got > max`.
///
/// # Errors
/// [`Error::Limit`] if `got > max`.
pub fn check(name: &'static str, got: i64, max: i64) -> Result<()> {
    if got > max {
        return Err(Error::Limit { name, got, max });
    }
    Ok(())
}

/// `checkContext`: the one place a loop boundary consults the cancel flag. Every Go
/// `ctx.Err()` point, and the rows and blocks Go does not check, call this.
///
/// # Errors
/// [`Error::Cancelled`] if the flag is set.
pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_go() {
        let l = Limits::default();
        assert_eq!(l.objects(), 200_000);
        assert_eq!(l.pages(), 10_000);
        assert_eq!(l.stream_bytes(), 64 << 20);
        assert_eq!(l.page_stream_bytes(), 128 << 20);
        assert_eq!(l.image_pixels(), 500_000_000);
        assert_eq!(l.font_program_bytes(), 16 << 20);
        assert_eq!(l.images(), 262_144);
        assert_eq!(l.inline_images(), 262_144);
        assert_eq!(l.unresolved(), 262_144);
        assert_eq!(l.object_stream_entries(), 200_000);
        assert_eq!(l.total_decode_bytes(), 64 << 20);
        assert_eq!(l.object_bytes(), 64 << 20);
        assert_eq!(l.retained_bytes(), 128 << 20);
        assert_eq!(l.encoded_image_pixels(), 1 << 26);
        assert_eq!(l.png_text_bytes(), 64 << 20);
        assert_eq!(l.raster_side(), 20_000);
        assert_eq!(l.raster_pixels(), 1 << 26);
        assert_eq!(l.render_font_bytes(), 16 << 20);
        assert_eq!(l.path_points(), 1 << 21);
        assert_eq!(l.fill_work(), 1 << 28);
        assert_eq!(l.gstate_depth(), 1024);
        assert_eq!(l.form_depth(), 8);
        assert_eq!(l.form_invocations(), 4096);
        assert_eq!(l.operands(), 8192);
        assert_eq!(l.text_shows(), 1 << 16);
        assert_eq!(l.paints(), 1 << 18);
        assert_eq!(l.max_stream_total_bytes, 0);
        assert_eq!(l.alloc_limit(), 4 << 30);
        assert!(l.threads() >= 1);
    }

    // Go: "The zero value, and any field <= 0, takes the safe default."
    #[test]
    fn non_positive_fields_take_the_default() {
        let zero = Limits {
            max_objects: 0,
            max_pages: -1,
            max_stream_bytes: i64::MIN,
            max_image_pixels: 0,
            max_alloc: 0,
            max_threads: 0,
            ..Limits::default()
        };
        assert_eq!(zero.objects(), 200_000);
        assert_eq!(zero.pages(), 10_000);
        assert_eq!(zero.stream_bytes(), 64 << 20);
        assert_eq!(zero.image_pixels(), 500_000_000);
        assert_eq!(zero.alloc_limit(), 4 << 30);
        assert_eq!(zero.threads(), 1);
        let r = zero.resolved();
        assert_eq!(r.max_objects, 200_000);
        assert_eq!(r.max_alloc, 4 << 30);
        assert_eq!(r.max_threads, 1);
        assert_eq!(r.resolved(), r, "resolved is idempotent");
    }

    #[test]
    fn derived_defaults_follow_their_base() {
        let l = Limits {
            max_page_stream_bytes: 5120,
            max_objects: 100,
            max_object_stream_entries: 500,
            max_stream_bytes: 1000,
            ..Limits::default()
        };
        assert_eq!(l.images(), 10);
        assert_eq!(l.inline_images(), 10);
        assert_eq!(l.unresolved(), 10);
        // Never above the object count.
        assert_eq!(l.object_stream_entries(), 100);
        assert_eq!(l.total_decode_bytes(), 1000);
        assert_eq!(l.object_bytes(), 1000);
        let own = Limits {
            max_images: 3,
            max_total_decode_bytes: 7,
            max_object_bytes: 9,
            max_object_stream_entries: 4,
            ..l
        };
        assert_eq!(own.images(), 3);
        assert_eq!(own.total_decode_bytes(), 7);
        assert_eq!(own.object_bytes(), 9);
        assert_eq!(own.object_stream_entries(), 4);
    }

    #[test]
    fn mul_sat_and_check() {
        assert_eq!(mul_sat(0, i64::MAX), 0);
        assert_eq!(mul_sat(3, 4), 12);
        assert_eq!(mul_sat(i64::MAX, 2), i64::MAX);
        assert_eq!(mul_sat(1 << 40, 1 << 40), i64::MAX);
        assert_eq!(mul_sat(i64::MAX, 1), i64::MAX);
        assert!(check("x", 10, 10).is_ok());
        assert_eq!(
            check("image-pixels", 11, 10),
            Err(Error::Limit {
                name: "image-pixels",
                got: 11,
                max: 10
            })
        );
    }

    #[test]
    fn cancel_flag() {
        let f = AtomicBool::new(false);
        assert_eq!(check_cancel(&f), Ok(()));
        f.store(true, Ordering::Relaxed);
        assert_eq!(check_cancel(&f), Err(Error::Cancelled));
    }

    // pdfdoc.go:81-93 and :447-456 with the default limits.
    #[test]
    fn pdf_limits_as_pdfdoc_writes_them() {
        let l = Limits::default();
        let size = l.objects();
        let p = l.pdf_limits(size);
        assert_eq!(p.max_decode_bytes, 64 << 20);
        assert_eq!(p.max_object_count, 200_000);
        assert_eq!(p.max_object_stream_total_count, 200_000);
        assert_eq!(p.max_total_decode_bytes, 64 << 20);
        assert_eq!(p.max_object_bytes, 64 << 20);
        // parsedBytes: 200000*1536/4*3 + 64 MiB.
        assert_eq!(p.max_parsed_bytes, 200_000 * 1536 / 4 * 3 + (64 << 20));
        assert_eq!(p.max_stream_total_bytes, 0);
        assert_eq!(p.max_alloc, 4 << 30);
        // Everything else is pdf-rs's own default.
        let d = pdf::Limits::default();
        assert_eq!(p.max_image_pixels, d.max_image_pixels);
        assert_eq!(p.max_stream_bytes, d.max_stream_bytes);

        let tight = Limits {
            max_parsed_bytes: 1234,
            max_stream_total_bytes: 99,
            max_alloc: 1 << 20,
            max_threads: 1,
            ..l
        };
        let p = tight.pdf_limits(10);
        assert_eq!(p.max_parsed_bytes, 1234);
        assert_eq!(p.max_stream_total_bytes, 99);
        assert_eq!(p.max_alloc, 1 << 20);
        assert_eq!(p.max_threads, 1);
        assert_eq!(tight.pdf_config(10).limits, p);
    }

    #[test]
    fn parsed_bytes_scales_with_the_table() {
        let l = Limits::default();
        assert_eq!(l.parsed_bytes(0), 64 << 20);
        assert_eq!(l.parsed_bytes(4), 4 * 1536 / 4 * 3 + (64 << 20));
        assert_eq!(object_table_bytes(i64::MAX), i64::MAX);
    }

    #[test]
    fn alloc_is_bounded_by_max_alloc() {
        let l = Limits {
            max_alloc: 16,
            ..Limits::default()
        };
        assert_eq!(l.alloc_zeroed(16).map(|v| v.len()), Ok(16));
        assert_eq!(l.alloc_zeroed(17), Err(Error::Alloc { got: 17, max: 16 }));
        let m = l.mem();
        assert_eq!(m.left(), 16);
    }
}
