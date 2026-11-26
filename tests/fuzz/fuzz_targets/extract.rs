#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    for page in 1..=2 {
        if let Err(e) = byblos::extract::extract_page_raster(data, page, &lim, &common::NEVER) {
            common::report(&e);
        }
    }
});
