//! `transcode_png-crash-fe0441b0`: an 8-bit palette PNG with a 255-entry PLTE whose pixels use
//! index 255. Go's TranscodePNG accepts it (hival = len(PLTE)/3 - 1 = 254, no index check) and
//! Go's BuildPDF refuses it as an /Indexed sample past hival; Rust matches, so the fuzz target's
//! "an accepted image always builds" invariant was wrong for this case.

use byblos::Limits;
use byblos::build::{BuildPage, build_pdf, transcode_png};
use std::sync::atomic::AtomicBool;

#[test]
fn transcode_png_index_past_hival_is_refused_by_build_pdf() {
    let data = include_bytes!("fuzz/regressions/transcode_png-crash-fe0441b0");
    let cancel = AtomicBool::new(false);
    let lim = Limits {
        max_encoded_image_pixels: 1 << 12,
        ..Limits::default()
    };
    let (img, _) = transcode_png(&data[..], &lim, &cancel).expect("Go's TranscodePNG accepts it");
    let page = BuildPage {
        image: img,
        dpi: 72.0,
        ..Default::default()
    };
    let e = build_pdf(&[page], &lim, &cancel).expect_err("Go's BuildPDF refuses it");
    assert!(
        format!("{e:?}").contains("past the palette's hival 254"),
        "{e:?}"
    );
}
