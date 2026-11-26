#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::optimize::{OptimizeOptions, optimize};
use libfuzzer_sys::fuzz_target;

// Every option on: the rewrite, JPEG recompression, then the linearizing writer.
fuzz_target!(|data: &[u8]| {
    let opts = OptimizeOptions {
        linearize: true,
        recompress_jpeg: true,
        jpeg_quality: 75,
        now: None,
    };
    if let Err(e) = optimize(data, &opts, &common::limits(), &common::NEVER) {
        common::report(&e);
    }
});
