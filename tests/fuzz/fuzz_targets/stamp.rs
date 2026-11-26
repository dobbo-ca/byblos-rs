#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::Rect;
use byblos::stamp::{PositionedWord, TextLayer, stamp_text_layer};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

// A repaired xref scan can register an object number near 10^10 (a number built from the digits
// of a garbage line), so the trailer `/Size` becomes ~10^10. pdf-go's `deleteRedundantObjects`
// walks `0..Size` (`write.go:626`); the pinned pdf-rs visits only the numbers in the xref table,
// so that input is no longer slow. The watchdog stays as a defensive bound: it cancels an
// iteration past BUDGET. Cancelled is the documented outcome; the target ignores it.
const BUDGET: Duration = Duration::from_secs(2);
static CANCEL: AtomicBool = AtomicBool::new(false);
static START_MS: AtomicU64 = AtomicU64::new(0); // 0 = idle, else 1 + ms since EPOCH
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn watchdog() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let s = START_MS.load(Relaxed);
                if s != 0 && now_ms().saturating_sub(s) > BUDGET.as_millis() as u64 {
                    CANCEL.store(true, Relaxed);
                }
            }
        });
    });
}
// A fuzzed PDF and a fixed one-word text layer on its first page, then on its first two (the second
// is refused on a 1-page file).
fuzz_target!(|data: &[u8]| {
    let word = || PositionedWord {
        text: "word".into(),
        bounds: Rect::new(10, 10, 60, 30),
    };
    watchdog();
    CANCEL.store(false, Relaxed);
    START_MS.store(now_ms(), Relaxed);
    for n in 1..=2 {
        let tl = TextLayer {
            pages: (0..n).map(|_| vec![word()]).collect(),
        };
        if let Err(e) = stamp_text_layer(data, &tl, None, &common::limits(), &CANCEL) {
            common::report(&e);
        }
    }
    START_MS.store(0, Relaxed);
});
