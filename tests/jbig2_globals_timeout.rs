//! Regression for the `jbig2_decode_globals` fuzz timeout `a02ac06b`: a 123-byte globals
//! dictionary declaring 39680 symbols decodes noise up to Go's 1<<26 MQ-decision cap, which is a
//! legal, Go-equal decode that runs for tens of seconds under ASan. The library has no tighter
//! pixel cap, so the bound is the cancel flag; the call must return promptly once it is set.

use byblos::{Error, Limits};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[test]
fn globals_timeout_input_stops_on_cancel() {
    let data = include_bytes!("fuzz/regressions/jbig2_decode_globals-timeout-a02ac06b");
    let g = (usize::from(data[0]) << 8 | usize::from(data[1])).min(data.len() - 2);
    let (globals, page) = data[2..].split_at(g);
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            cancel.store(true, Relaxed);
        });
        byblos::jbig2::decode_embedded_stream_with_globals(
            globals,
            page,
            &Limits::default(),
            &cancel,
        )
    });
    // No panic; it ends in the documented refusal or cancellation, and soon after the flag.
    assert!(matches!(r, Err(Error::Malformed(_) | Error::Cancelled)) || r.is_ok());
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "took {:?}",
        t.elapsed()
    );
}
