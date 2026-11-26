#![no_main]
#[path = "../common.rs"]
mod common;
use libfuzzer_sys::fuzz_target;
// Go's FuzzPNGTranscode: under a 4096 pixel cap, an accepted image is within the cap and
// BuildPDF takes it.
fuzz_target!(|data: &[u8]| {
    let lim = byblos::Limits {
        max_encoded_image_pixels: 1 << 12,
        ..common::limits()
    };
    let (r, _, peak) = common::measure(|| byblos::build::transcode_png(data, &lim, &common::NEVER));
    // Go caps one scratch request at 64 KiB here; this is the peak of the call, with room for
    // the chunk and row buffers the input itself sizes.
    assert!(
        peak <= (8 << 20) + 16 * data.len() as u64,
        "transcode_png held {peak} bytes for {} under a {} px cap",
        data.len(),
        lim.max_encoded_image_pixels
    );
    let Ok((img, _res)) = r else {
        return;
    };
    assert!(
        img.width.saturating_mul(img.height) <= lim.max_encoded_image_pixels,
        "accepted {}x{}",
        img.width,
        img.height
    );
    // Go builds at DPI 72, not at the PNG's pHYs: a huge pHYs makes a page box build_pdf refuses.
    let page = byblos::build::BuildPage {
        image: img,
        dpi: 72.0,
        ..Default::default()
    };
    if let Err(e) = byblos::build::build_pdf(&[page], &lim, &common::NEVER) {
        // Go's TranscodePNG does not check pixels against the PLTE either (a 255-entry PLTE with
        // index 255), and Go's BuildPDF refuses that exactly so; Go's FuzzPNGTranscode would trip
        // on it too. Not a library divergence.
        if format!("{e:?}").contains("past the palette's hival") {
            return;
        }
        panic!("accepted but build_pdf refuses: {e:?}");
    }
});
