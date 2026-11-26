#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
// An accepted image is positive and within the pixel cap (Go's FuzzEmbedJP2).
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    if let Ok((img, res)) = byblos::embed::embed_jp2(data, &lim) {
        assert!(
            img.width > 0
                && img.height > 0
                && img.width.saturating_mul(img.height) <= lim.max_encoded_image_pixels,
            "accepted {}x{}",
            img.width,
            img.height
        );
        common::build_and_inspect(img, res, &lim);
    }
});
