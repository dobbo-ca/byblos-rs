#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::editpages::{PageSource, build_from_pages};
use byblos::pdfdoc::straighten::StraightenSpec;
use libfuzzer_sys::fuzz_target;
// A fuzzed PDF as the only source, whole (so a seed is a plain PDF). The rotation and the straighten
// angle come from its last two bytes. Pages 1 and 1, then 2 and 1 (the second fails on a 1-page file).
fuzz_target!(|data: &[u8]| {
    let Some(h) = data.last_chunk::<2>() else {
        return;
    };
    let page = |n: usize, b: u8| PageSource {
        source: 0,
        page: n,
        rotate: [0, 90, 180, 270][usize::from(b & 3)],
        straighten: (b & 4 != 0).then(|| StraightenSpec {
            deg: f64::from(i16::from(b >> 3) - 16) / 4.0,
            crop: None,
        }),
    };
    for first in [1, 2] {
        let pages = [page(first, h[0]), page(1, h[1])];
        if let Err(e) = build_from_pages(&[data], &pages, None, &common::limits(), &common::NEVER) {
            common::report(&e);
        }
    }
});
