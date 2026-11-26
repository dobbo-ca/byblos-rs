//! Regression for the `stamp` fuzz timeout `8cbc219c`: the xref repair reads a garbage line as
//! object number 9995598601, so the trailer `/Size` becomes ~10^10. pdf-go's writer walks every
//! number below it (`write.go:626`), a worst case of minutes; the pinned pdf-rs visits only the
//! numbers in the xref table. Either way the calls must return promptly once the cancel flag is
//! set.

use byblos::stamp::{PositionedWord, TextLayer, stamp_text_layer};
use byblos::{Limits, Rect};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[test]
fn size_walk_input_stops_on_cancel() {
    let data = include_bytes!("fuzz/regressions/stamp-timeout-8cbc219c");
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            cancel.store(true, Relaxed);
        });
        for n in 1..=2 {
            let tl = TextLayer {
                pages: (0..n)
                    .map(|_| {
                        vec![PositionedWord {
                            text: "word".into(),
                            bounds: Rect::new(10, 10, 60, 30),
                        }]
                    })
                    .collect(),
            };
            // No panic; any outcome is allowed, it must just end soon after the flag.
            let _ = stamp_text_layer(data, &tl, None, &Limits::default(), &cancel);
        }
    });
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "took {:?}",
        t.elapsed()
    );
}
