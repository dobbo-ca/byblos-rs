#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    let _ = byblos::embed::exif_orientation(data, &lim);
    if let Ok((img, res)) = byblos::embed::embed_png(data, &lim) {
        common::build_and_inspect(img, res, &lim);
    }
});
