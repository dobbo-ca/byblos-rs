#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::Limits;
use byblos::build::{BuildPage, build_pdf};
use byblos::encoded::{ColorSpace, DecodeParms, EncodedImage};
use libfuzzer_sys::fuzz_target;

// Input: filter, colour space, bpc, flags, width (u16), height (u16), rotate, dpi, then the data
// (for Indexed: hi_val and a palette first). Go's FuzzBuildPDF: whatever build_pdf accepts, inspect
// must read back.
fuzz_target!(|data: &[u8]| {
    let Some(h) = data.get(..10) else { return };
    let rest = &data[10..];
    if rest.len() > 1 << 16 {
        return;
    }
    let filter = [
        "FlateDecode",
        "DCTDecode",
        "JPXDecode",
        "JBIG2Decode",
        "CCITTFaxDecode",
    ][usize::from(h[0]) % 5];
    let cs = ["DeviceGray", "DeviceRGB", "DeviceCMYK", "Indexed"][usize::from(h[1]) % 4];
    let bpc = [1, 2, 4, 8, 16, 0, 32][usize::from(h[2]) % 7];
    let w = i64::from(u16::from_be_bytes([h[4], h[5]]));
    let ht = i64::from(u16::from_be_bytes([h[6], h[7]]));
    let rotate = [0, 90, 180, 270, 45, -90][usize::from(h[8]) % 6];
    let mut color_space = ColorSpace {
        name: cs.into(),
        ..ColorSpace::default()
    };
    let mut body = rest;
    if cs == "Indexed" {
        let hi = rest.first().copied().unwrap_or(0);
        let pal = rest.get(1..).unwrap_or(&[]);
        let pal = &pal[..pal.len().min((usize::from(hi) + 1) * 3)];
        color_space.base = "DeviceRGB".into();
        color_space.hi_val = i64::from(hi);
        color_space.lookup = pal.to_vec();
        body = &rest[(1 + pal.len()).min(rest.len())..];
    }
    let decode_parms = (h[3] & 1 != 0).then_some(DecodeParms {
        predictor: 15,
        colors: if cs == "DeviceRGB" { 3 } else { 1 },
        bits_per_component: bpc,
        columns: w,
    });
    let image = EncodedImage {
        width: w,
        height: ht,
        bpc,
        color_space,
        filter: filter.into(),
        decode_parms,
        data: body.to_vec(),
        ..EncodedImage::default()
    };
    let lim = Limits {
        max_encoded_image_pixels: 1 << 16,
        ..common::limits()
    };
    let page = BuildPage {
        image,
        dpi: 72.0 + f64::from(h[9]),
        rotate,
        ..BuildPage::default()
    };
    if let Ok(pdf) = build_pdf(&[page], &lim, &common::NEVER) {
        if let Err(e) = byblos::inspect::inspect(&pdf, &lim, &common::NEVER) {
            panic!("accepted {filter}/{cs}/{bpc} but inspect fails: {e:?}");
        }
    }
});
