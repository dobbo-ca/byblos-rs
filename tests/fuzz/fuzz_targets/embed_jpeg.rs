#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
// A rotation is 0, 90, 180 or 270 (Go's FuzzEXIFRotation).
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    // Go's FuzzEXIFRotation: no more than 64 MiB + 16 bytes per input byte.
    let (r, total, _) = common::measure(|| byblos::embed::exif_rotation(data, &lim));
    assert!(
        total <= (64 << 20) + 16 * data.len() as u64,
        "exif_rotation allocated {total} bytes for {}",
        data.len()
    );
    if let Ok(r) = r {
        assert!(matches!(r, 0 | 90 | 180 | 270), "rotation {r}");
    }
    let _ = byblos::embed::exif_orientation(data, &lim);
    if let Ok((img, res)) = byblos::embed::embed_jpeg(data, &lim) {
        common::build_and_inspect(img, res, &lim);
    }
});
