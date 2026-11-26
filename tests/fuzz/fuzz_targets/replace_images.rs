#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::encoded::{ColorSpace, EncodedImage};
use byblos::substitute::replace_images;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;
// A fuzzed PDF; every substitutable image it reports is replaced by a 1x1 grey pixel.
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    let Ok(info) = byblos::inspect::inspect(data, &lim, &common::NEVER) else {
        return;
    };
    let px = EncodedImage {
        width: 1,
        height: 1,
        bpc: 8,
        color_space: ColorSpace {
            name: "DeviceGray".into(),
            ..ColorSpace::default()
        },
        filter: "FlateDecode".into(),
        data: vec![0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01],
        ..EncodedImage::default()
    };
    let subs: BTreeMap<i64, EncodedImage> = info
        .pages
        .iter()
        .flat_map(|p| &p.images)
        .filter(|i| i.substitutable)
        .map(|i| (i.obj_nr, px.clone()))
        .collect();
    if let Err(e) = replace_images(data, &subs, &lim, &common::NEVER) {
        common::report(&e);
    }
});
