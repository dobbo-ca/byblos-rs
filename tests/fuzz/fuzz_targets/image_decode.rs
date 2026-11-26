#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    let _ = byblos::image::decode_config(data, &lim);
    let _ = byblos::image::decode(data, &lim, &common::NEVER);
});
