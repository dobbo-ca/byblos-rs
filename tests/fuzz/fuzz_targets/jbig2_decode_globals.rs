#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

// Go's caps (1<<26 MQ decisions per stream) admit a legal decode of several seconds in release
// and tens under ASan, and the library takes no tighter pixel cap, so the target bounds the
// call by the cancel flag (checked per symbol and per row): a watchdog cancels an iteration
// that runs past BUDGET. Cancelled is the documented outcome; the target ignores the result.
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

// Input: a big-endian u16 length G, then the globals (G bytes, clipped to the input), then the
// page stream.
fuzz_target!(|data: &[u8]| {
    let (Some(n), Some(rest)) = (data.get(..2), data.get(2..)) else {
        return;
    };
    let g = (usize::from(n[0]) << 8 | usize::from(n[1])).min(rest.len());
    let (globals, page) = rest.split_at(g);
    let lim = common::limits();
    watchdog();
    CANCEL.store(false, Relaxed);
    START_MS.store(now_ms(), Relaxed);
    let _ = byblos::jbig2::page_size_with_globals(globals, page, &lim);
    let _ = byblos::jbig2::decode_embedded_stream_with_globals(globals, page, &lim, &CANCEL);
    START_MS.store(0, Relaxed);
});
