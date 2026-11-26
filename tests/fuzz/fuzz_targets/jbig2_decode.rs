#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    let _ = byblos::jbig2::page_size(data, &lim);
    let _ = byblos::jbig2::decode_generic(data, &lim, &common::NEVER);
});
