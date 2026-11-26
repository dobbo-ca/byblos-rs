#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if let Err(e) =
        byblos::render_page::render_page(data, 1, 128, &common::limits(), &common::NEVER)
    {
        common::report(&e);
    }
});
