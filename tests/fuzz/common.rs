// Shared by the targets (`#[path = "../common.rs"] mod common;`); not a target itself.
#![allow(dead_code)]
use byblos::build::{BuildPage, build_pdf};
use byblos::embed::Resolution;
use byblos::encoded::EncodedImage;
use byblos::{Limits, inspect};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// Counts the bytes the process allocates, so a target can check Go's allocation invariants
/// (`TotalAlloc` in FuzzEXIFRotation, the scratch cap in FuzzPNGTranscode) and that a call stays
/// under `max_alloc` when it gets there in pieces, which `-malloc_limit_mb` cannot see.
struct Counting;
static TOTAL: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

fn grow(n: usize) {
    TOTAL.fetch_add(n as u64, Relaxed);
    let live = LIVE.fetch_add(n as u64, Relaxed) + n as u64;
    PEAK.fetch_max(live, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size() as u64, Relaxed);
            grow(n);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Runs `f` and returns what it allocated in all and the most it held at once.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, u64, u64) {
    let (t0, l0) = (TOTAL.load(Relaxed), LIVE.load(Relaxed));
    PEAK.store(l0, Relaxed);
    let r = f();
    (
        r,
        TOTAL.load(Relaxed) - t0,
        PEAK.load(Relaxed).saturating_sub(l0),
    )
}

/// Never set: a target is bounded by its limits and libFuzzer's -timeout, not by cancellation.
pub static NEVER: AtomicBool = AtomicBool::new(false);

/// The limits every target runs under, small enough that `-malloc_limit_mb=512` is meaningful:
/// 256 MiB for the call (`max_alloc`), no spawned thread, tight pixel and stream caps.
pub fn limits() -> Limits {
    Limits {
        max_alloc: 256 << 20,
        max_threads: 1,
        max_objects: 20_000,
        max_pages: 500,
        max_stream_bytes: 16 << 20,
        max_page_stream_bytes: 16 << 20,
        max_image_pixels: 1 << 22,
        max_encoded_image_pixels: 1 << 22,
        max_png_text_bytes: 1 << 20,
        max_font_program_bytes: 4 << 20,
        max_render_font_bytes: 4 << 20,
        max_retained_bytes: 16 << 20,
        max_raster_side: 2048,
        max_raster_pixels: 1 << 20,
        max_path_points: 1 << 16,
        max_fill_work: 1 << 24,
        ..Limits::default()
    }
}

/// Formats an entry point's error, so `class()` and `Display` (with any wrapped chain) run too.
pub fn report(e: &byblos::Error) {
    let _ = e.class();
    let _ = e.to_string();
}

/// Builds a one-page PDF from an image an `embed_*` or `transcode_png` accepted, and reads it
/// back. Both results are ignored: the point is that neither panics or overruns the limits.
pub fn build_and_inspect(image: EncodedImage, res: Resolution, lim: &Limits) {
    let dpi = if res.x > 0.0 { res.x } else { 72.0 };
    let page = BuildPage {
        image,
        dpi,
        dpi_y: if res.y > 0.0 { res.y } else { dpi },
        ..BuildPage::default()
    };
    let (_, _, peak) = measure(|| {
        if let Ok(pdf) = build_pdf(&[page], lim, &NEVER) {
            let _ = inspect::inspect(&pdf, lim, &NEVER);
        }
    });
    // The 64 MiB is slack for the page and the PDF the call holds beside its ledger.
    assert!(
        peak <= lim.max_alloc + (64 << 20),
        "peak {peak} bytes over max_alloc"
    );
}
