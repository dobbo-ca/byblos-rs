//! Measured peaks of `Limits::max_alloc` and `Limits::max_threads` on the main entry points
//! (tests assert both peaks for several values,
//! including 1). docs/limits.md, "Measured-peak tests", says which field each case covers.
//!
//! * Output: every case returns the same bytes (or the same error class) for every
//!   `max_threads` (1, 0, 2 and the default) at every `max_alloc`.
//! * Memory: a counting `#[global_allocator]` (std only) tracks the live and the peak heap bytes.
//!   Each measurement resets the peak to the live bytes at that moment, so the input and the
//!   test's own buffers do not count. The assert is `peak <= max_alloc + SLACK`, whether the call
//!   succeeds or fails: a refused charge must not follow an uncharged allocation of that size.
//!   `pdf::Mem::peak()` is not used: it cannot exceed its limit by construction, so it could not
//!   fail. The allocator sees what the ledger never charges.
//! * Threads: a sampler thread reads `Threads:` of `/proc/self/status` every 200 microseconds
//!   (Linux only; the assert is skipped elsewhere). The live threads during a call, over the
//!   count just before it, must not exceed `max_threads - 1` (the calling thread is already in
//!   the baseline). For `max_threads <= 1` the count must not exceed the count at the start of the
//!   test at all, so no thread and no pool is ever started. Those runs go first for that reason.
//!   A burst shorter than the sampling period can be missed; the assert is a lower bound on
//!   spawning, which is what a regression (a pool started by `max_threads = 1`) breaks for good.
//! * Tight budget: at `max_alloc = 64` bytes every case must fail with `Error::Alloc`,
//!   `Error::OutOfMemory` or a limit error, and the heap peak must stay within `TIGHT_SLACK`.
//!
//! Everything runs in one `#[test]`, because the allocator counters and the thread counts are
//! global and the harness would otherwise run several tests at once.

use byblos::build::{BuildPage, build_pdf};
use byblos::downsample::downsample;
use byblos::editpages::{PageSource, build_from_pages};
use byblos::embed::{embed_jpeg, embed_png};
use byblos::extract::extract_page_raster;
use byblos::image::{Gray, Rgba};
use byblos::inspect::inspect;
use byblos::jbig2::{decode_generic, encode_image};
use byblos::optimize::{OptimizeOptions, optimize};
use byblos::quantize::{quantize_indexed, quantize_png};
use byblos::render_page::render_page;
use byblos::sauvola::sauvola;
use byblos::stamp::{PositionedWord, TextLayer, stamp_text_layer};
use byblos::{Bitmap, Error, Image, Limits, Rect};
use pdf::GoTime;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Heap that the ledger does not charge by design: nested-crate scratch (the flate window, the
/// JPEG decoder's tables), the document shell, `Vec` growth where the old and the new buffer
/// coexist during a `realloc`, the returned output built from a charged buffer, and the harness's
/// own small allocations. 4 MiB is far below the amplification this test hunts (a small input
/// that costs hundreds of MiB), so it does not hide a finding. A call that holds the input, a
/// decoded copy and an output at once is charged for all three by the ledger, so a larger
/// uncharged copy is a finding, not slack.
const SLACK: u64 = 4 << 20;

/// The slack at `max_alloc = 64`: a refused call must not have started the work.
const TIGHT_SLACK: u64 = 2 << 20;

const TIGHT: u64 = 64;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn add(n: usize) {
    let now = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments and only adds counting.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract, which `System` shares.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` come from this allocator, which forwards to `System`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as in `dealloc`; `new_size` is the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                add(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Runs `f` and returns its result with the most heap bytes that were live at once above the
/// live bytes at the start. The result is still alive when the peak is read.
fn measure<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let r = f();
    (r, PEAK.load(Ordering::Relaxed).saturating_sub(base) as u64)
}

/// The number of OS threads of this process, or `None` off Linux.
fn os_threads() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let n = fs::File::open("/proc/self/status")
            .ok()?
            .read(&mut buf)
            .ok()?;
        let text = std::str::from_utf8(buf.get(..n)?).ok()?;
        let line = text.lines().find(|l| l.starts_with("Threads:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// A thread that records the most live OS threads it saw until it is stopped.
struct Sampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicUsize>,
    handle: std::thread::JoinHandle<()>,
}

impl Sampler {
    fn start() -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicUsize::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if let Some(n) = os_threads() {
                    p.fetch_max(n, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        });
        Sampler { stop, peak, handle }
    }

    /// The peak seen, 0 if the platform has no count.
    fn finish(self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
        self.peak.load(Ordering::Relaxed)
    }
}

/// What a case returns: the bytes it produced, or the class of its error.
type Outcome = Result<Vec<u8>, Error>;

struct Case {
    name: &'static str,
    /// The input is so small (a few hundred bytes) that the call may fit the tight budget; the
    /// tight run then asserts the peak only.
    tiny: bool,
    run: Box<dyn Fn(&Limits) -> Outcome>,
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The same bytes for the same input: a fixed time, so nothing depends on the clock.
const PINNED: GoTime = GoTime {
    unix: 946_684_800,
    utc_offset_min: 0,
};

/// A page-sized RGBA image of smooth bands, a little text-like structure and deterministic
/// noise, opaque, so Sauvola, downsample and quantize all have work.
fn page_image(w: i64, h: i64) -> Image {
    let mut img = Rgba::new(Rect::new(0, 0, w, h), &Limits::default()).expect("image fits");
    let mut s: u32 = 0x1234_5678;
    for y in 0..h as usize {
        for x in 0..w as usize {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (s >> 28) as usize;
            let ink = (x / 7 + y / 11) % 5 == 0 && (x % 7) < 3;
            let v = if ink {
                30 + noise
            } else {
                200 + (x + y) % 40 + noise
            };
            let o = y * img.stride + x * 4;
            img.pix[o] = v as u8;
            img.pix[o + 1] = (v + x % 16) as u8;
            img.pix[o + 2] = (v + y % 16) as u8;
            img.pix[o + 3] = 255;
        }
    }
    Image::Rgba(img)
}

fn gray_page(w: i64, h: i64) -> Image {
    let mut g = Gray::new(Rect::new(0, 0, w, h), &Limits::default()).expect("image fits");
    let mut s: u32 = 0x9e37_79b9;
    for p in g.pix.iter_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *p = if s >> 31 == 1 { 255 } else { (s >> 24) as u8 };
    }
    Image::Gray(g)
}

/// The bytes of an image, for the comparison across thread counts.
fn image_bytes(img: &Image) -> Vec<u8> {
    match img {
        Image::Rgba(i) => i.pix.clone(),
        Image::Gray(i) => i.pix.clone(),
        other => format!("{:?}", other.bounds()).into_bytes(),
    }
}

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

const PDFS: [&str; 5] = [
    "real/blank-scan.pdf",
    "real/testImage.pdf",
    "real/5116.DCT_Filter.pdf",
    "real/testWithText.pdf",
    "real/Walden.pdf",
];

fn cases() -> Vec<Case> {
    let mut v: Vec<Case> = Vec::new();

    for p in PDFS {
        let data = Arc::new(fixture(p));
        let d = data.clone();
        v.push(Case {
            name: "inspect",
            tiny: false,
            run: Box::new(move |l| inspect(&d, l, &never()).map(|i| format!("{i:?}").into_bytes())),
        });
        let d = data.clone();
        v.push(Case {
            name: "extract",
            tiny: false,
            run: Box::new(move |l| {
                extract_page_raster(&d, 1, l, &never()).map(|r| {
                    let mut b = format!("{:?} {:?} {}", r.bounds, r.page, r.obj_nr).into_bytes();
                    b.extend(image_bytes(&r.image));
                    b
                })
            }),
        });
        let d = data.clone();
        v.push(Case {
            name: "render_page",
            tiny: false,
            run: Box::new(move |l| render_page(&d, 1, 400, l, &never()).map(|r| r.pix)),
        });
        let d = data.clone();
        v.push(Case {
            name: "optimize",
            tiny: false,
            run: Box::new(move |l| {
                let o = OptimizeOptions {
                    now: Some(PINNED),
                    ..OptimizeOptions::default()
                };
                optimize(&d, &o, l, &never())
            }),
        });
        let d = data.clone();
        v.push(Case {
            name: "optimize-linearize-recompress",
            tiny: false,
            run: Box::new(move |l| {
                let o = OptimizeOptions {
                    linearize: true,
                    recompress_jpeg: true,
                    jpeg_quality: 75,
                    now: Some(PINNED),
                };
                optimize(&d, &o, l, &never())
            }),
        });
        let d = data;
        v.push(Case {
            name: "stamp",
            tiny: false,
            run: Box::new(move |l| {
                let tl = TextLayer {
                    pages: vec![vec![
                        PositionedWord {
                            text: "alpha".to_string(),
                            bounds: Rect::new(72, 700, 120, 712),
                        },
                        PositionedWord {
                            text: "beta".to_string(),
                            bounds: Rect::new(130, 700, 170, 712),
                        },
                    ]],
                };
                stamp_text_layer(&d, &tl, Some(PINNED), l, &never())
            }),
        });
    }

    // Image processing on a page-sized raster: the grey copy, two integral images and the
    // output (sauvola), the scaler buffers (downsample), the histogram and the PNG encoder
    // (quantize) are all charged to the ledger.
    let rgb = Arc::new(page_image(800, 1000));
    let grey = Arc::new(gray_page(800, 1000));
    let i = rgb.clone();
    v.push(Case {
        name: "sauvola",
        tiny: false,
        run: Box::new(move |l| sauvola(&i, l, &never()).map(|b| b.pix)),
    });
    let i = rgb.clone();
    v.push(Case {
        name: "downsample-rgb",
        tiny: false,
        run: Box::new(move |l| {
            downsample(&i, Some(8), 300.0, 150.0, l, &never()).map(|o| image_bytes(&o))
        }),
    });
    let i = grey.clone();
    v.push(Case {
        name: "downsample-1bit",
        tiny: false,
        run: Box::new(move |l| {
            downsample(&i, Some(1), 300.0, 150.0, l, &never()).map(|o| image_bytes(&o))
        }),
    });
    for colors in [16usize, 256] {
        let i = rgb.clone();
        v.push(Case {
            name: "quantize-png",
            tiny: false,
            run: Box::new(move |l| quantize_png(&i, colors, l, &never())),
        });
        let i = rgb.clone();
        v.push(Case {
            name: "quantize-indexed",
            tiny: false,
            run: Box::new(move |l| quantize_indexed(&i, colors, l, &never()).map(|e| e.data)),
        });
    }

    // JBIG2: encode a Sauvola page, decode what it wrote, and decode the golden streams.
    let bitmap: Arc<Bitmap> =
        Arc::new(sauvola(&rgb, &Limits::default(), &never()).expect("sauvola"));
    let b = bitmap.clone();
    v.push(Case {
        name: "jbig2-encode",
        tiny: false,
        run: Box::new(move |l| encode_image(&b, l, &never()).map(|e| e.data)),
    });
    let enc = Arc::new(
        encode_image(&bitmap, &Limits::default(), &never())
            .expect("encode")
            .data,
    );
    v.push(Case {
        name: "jbig2-decode-roundtrip",
        tiny: false,
        run: Box::new(move |l| decode_generic(&enc, l, &never()).map(|b| b.pix)),
    });
    for f in ["text", "noise", "full"] {
        let data = fixture(&format!("jbig2/{f}.jb2"));
        v.push(Case {
            name: "jbig2-decode",
            tiny: true,
            run: Box::new(move |l| decode_generic(&data, l, &never()).map(|b| b.pix)),
        });
    }

    // Embedding and building: the fixtures through `embed_*` and `build_pdf`, and a three-page
    // JBIG2 document, the biggest input `build_pdf` is given here.
    for (f, png) in [("dpi-300x150.png", true), ("dpi-300x150.jpg", false)] {
        let data = fixture(&format!("img2pdf/{f}"));
        v.push(Case {
            name: "embed-build",
            tiny: true,
            run: Box::new(move |l| {
                let (image, res) = if png {
                    embed_png(&data, l)?
                } else {
                    embed_jpeg(&data, l)?
                };
                let page = BuildPage {
                    image,
                    width_pt: 0.0,
                    height_pt: 0.0,
                    dpi: if res.x > 0.0 { res.x } else { 300.0 },
                    dpi_y: if res.y > 0.0 { res.y } else { 300.0 },
                    rotate: 0,
                };
                build_pdf(&[page], l, &never())
            }),
        });
    }
    let pages: Arc<Vec<BuildPage>> = Arc::new(
        (0..3)
            .map(|_| BuildPage {
                image: encode_image(&bitmap, &Limits::default(), &never()).expect("encode"),
                width_pt: 0.0,
                height_pt: 0.0,
                dpi: 300.0,
                dpi_y: 0.0,
                rotate: 0,
            })
            .collect(),
    );
    v.push(Case {
        name: "build-jbig2-3-pages",
        tiny: false,
        run: Box::new(move |l| build_pdf(&pages, l, &never())),
    });
    // L1: four distinct sources of the same document are open together; their ledgers share
    // the one budget, so the peak stays near max_alloc and not four times it.
    let multi = Arc::new(fixture("real/Walden.pdf"));
    v.push(Case {
        name: "build-from-pages-4-sources",
        tiny: false,
        run: Box::new(move |l| {
            let srcs: [&[u8]; 4] = [&multi, &multi, &multi, &multi];
            let pages: Vec<PageSource> = (0..4)
                .map(|source| PageSource {
                    source,
                    page: 1,
                    rotate: 0,
                    straighten: None,
                })
                .collect();
            build_from_pages(&srcs, &pages, Some(PINNED), l, &never())
        }),
    });
    // L4: 8 MiB of verbatim JBIG2 data. No copy of the image is made: build_pdf borrows it, and
    // the output buffer is charged before it is reserved.
    let big: Arc<Vec<BuildPage>> = Arc::new(vec![BuildPage {
        image: byblos::encoded::EncodedImage {
            width: 8,
            height: 1,
            bpc: 1,
            color_space: byblos::encoded::ColorSpace {
                name: "DeviceGray".to_string(),
                ..byblos::encoded::ColorSpace::default()
            },
            filter: "JBIG2Decode".to_string(),
            data: vec![0u8; 8 << 20],
            ..byblos::encoded::EncodedImage::default()
        },
        width_pt: 0.0,
        height_pt: 0.0,
        dpi: 300.0,
        dpi_y: 0.0,
        rotate: 0,
    }]);
    v.push(Case {
        name: "build-large-encoded-image",
        tiny: false,
        run: Box::new(move |l| build_pdf(&big, l, &never())),
    });
    v
}

/// An allocation or limit refusal, however it is wrapped.
fn is_refusal(e: &Error) -> bool {
    match e {
        Error::Alloc { .. } | Error::OutOfMemory => true,
        Error::Wrapped { sources, .. } => sources.iter().any(is_refusal),
        e => e.is_limit() || e.class().starts_with("limit/"),
    }
}

/// The comparable form of an outcome: the bytes, or the error's class.
fn fingerprint(o: &Outcome) -> Result<&[u8], String> {
    o.as_ref().map(Vec::as_slice).map_err(|e| e.class())
}

/// Runs one case; returns the outcome, the heap peak and the live-thread numbers.
struct Run {
    outcome: Outcome,
    peak: u64,
    /// The most OS threads sampled during the call; 0 if the platform has no count.
    threads_peak: usize,
    /// The OS threads just before the call.
    threads_base: usize,
}

fn run(case: &Case, limits: &Limits) -> Run {
    // Before the spawn, plus one for the sampler: a thread just spawned (or one just joined)
    // can be missing from (or still in) the OS count for a moment.
    let threads_base = os_threads().map_or(0, |n| n + 1);
    let sampler = Sampler::start();
    let (outcome, peak) = measure(|| (case.run)(limits));
    let threads_peak = sampler.finish();
    Run {
        outcome,
        peak,
        threads_peak,
        threads_base,
    }
}

#[test]
fn measured_peaks() {
    let cases = cases();
    let default_threads = Limits::default().max_threads;
    // The sampler is a thread too: it is in the baseline of every run, so a run is judged
    // against the count with a sampler alive.
    let probe = Sampler::start();
    std::thread::sleep(Duration::from_millis(5));
    let process_threads = probe.finish().max(os_threads().unwrap_or(0));
    let have_threads = os_threads().is_some();

    // 1 and 0 (which counts as 1) first, so that no pool exists yet.
    let mut reference: HashMap<(usize, u64), Outcome> = HashMap::new();
    let allocs = [16u64 << 20, 1 << 30];
    for threads in [1usize, 0, 2, default_threads] {
        if threads == 2 {
            // jpeg-rs and pdf-rs build a lazy shared pool of `available_parallelism` threads on
            // the first call that may use one. It must not exist after the 1 and 0 runs. Then
            // every case runs once at the default so the pools exist, and the baseline of the
            // runs below holds them.
            assert!(
                !jpeg::shared_pool_started() && !pdf::par::shared_pool_started(),
                "a shared pool was started by a max_threads 1 or 0 call"
            );
            let warm = Limits {
                max_alloc: 1 << 30,
                max_threads: default_threads,
                ..Limits::default()
            };
            for case in &cases {
                let _ = (case.run)(&warm);
            }
        }
        for &max_alloc in &allocs {
            let limits = Limits {
                max_alloc,
                max_threads: threads,
                ..Limits::default()
            };
            for (idx, case) in cases.iter().enumerate() {
                let r = run(case, &limits);
                let what = format!(
                    "{} (#{idx}) at max_alloc {max_alloc}, max_threads {threads}",
                    case.name
                );
                let shown = match &r.outcome {
                    Ok(b) => format!("ok, {} bytes", b.len()),
                    Err(e) => format!("{e:?}"),
                };

                // (b) The heap peak, success or refusal.
                assert!(
                    r.peak <= max_alloc + SLACK,
                    "{what}: heap peak {} is over max_alloc + {SLACK} (result: {shown})",
                    r.peak
                );

                // (c) The live threads. A baseline that already holds a sampler counts it, so
                // the call may add `max_threads - 1` threads and no more.
                if have_threads {
                    let allowed = threads.max(1) - 1;
                    let added = r.threads_peak.saturating_sub(r.threads_base);
                    assert!(
                        added <= allowed,
                        "{what}: {added} extra threads were live, the call may add {allowed}"
                    );
                    if threads <= 1 {
                        assert!(
                            r.threads_peak <= process_threads.max(r.threads_base),
                            "{what}: {} threads were live with no spawn allowed, \
                             {process_threads} at the start",
                            r.threads_peak
                        );
                    }
                }

                // (a) The same output for every thread count.
                match reference.entry((idx, max_alloc)) {
                    Entry::Vacant(v) => {
                        v.insert(r.outcome);
                    }
                    Entry::Occupied(first) => assert_eq!(
                        fingerprint(first.get()),
                        fingerprint(&r.outcome),
                        "{what}: output differs from the max_threads 1 run"
                    ),
                }
            }
        }
    }

    // At 1 GiB at least the small cases succeed, so the comparison above is not all errors.
    let ok = (0..cases.len())
        .filter(|i| matches!(reference.get(&(*i, 1 << 30)), Some(Ok(b)) if !b.is_empty()))
        .count();
    assert!(
        ok * 2 >= cases.len(),
        "only {ok} of {} cases succeed at 1 GiB; the fixtures or the cases are wrong",
        cases.len()
    );

    // (d) A tight budget is refused, and the work is not started.
    for threads in [1usize, 2, default_threads] {
        let limits = Limits {
            max_alloc: TIGHT,
            max_threads: threads,
            ..Limits::default()
        };
        for (idx, case) in cases.iter().enumerate() {
            let r = run(case, &limits);
            let what = format!(
                "{} (#{idx}) at max_alloc {TIGHT}, max_threads {threads}",
                case.name
            );
            match &r.outcome {
                Err(e) => assert!(is_refusal(e), "{what}: not an Alloc or limit error: {e:?}"),
                Ok(b) => assert!(case.tiny, "{what}: succeeded with {} bytes", b.len()),
            }
            assert!(
                r.peak <= TIGHT + TIGHT_SLACK,
                "{what}: heap peak {} is over max_alloc + {TIGHT_SLACK}, the work was started",
                r.peak
            );
        }
    }
}
