#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::Bitmap;
use byblos::jbig2::{bitmap::mask_padding, decode_generic, encode_generic};
use libfuzzer_sys::fuzz_target;

// Input: width-1 as a u16, height-1 as a u16 (each mod 512), then the packed pixels, repeated to
// fill the bitmap. Whatever encode_generic accepts must decode back to the same pixels.
fuzz_target!(|data: &[u8]| {
    let Some(head) = data.get(..4) else { return };
    let rest = &data[4..];
    let w = (usize::from(head[0]) << 8 | usize::from(head[1])) % 512 + 1;
    let h = (usize::from(head[2]) << 8 | usize::from(head[3])) % 512 + 1;
    let lim = common::limits();
    let Ok(mut bm) = Bitmap::new(w, h, &lim) else {
        return;
    };
    if !rest.is_empty() {
        for (d, s) in bm.pix.iter_mut().zip(rest.iter().cycle()) {
            *d = *s;
        }
    }
    mask_padding(&mut bm);
    let Ok(enc) = encode_generic(&bm, &lim, &common::NEVER) else {
        return;
    };
    let dec = decode_generic(&enc, &lim, &common::NEVER)
        .unwrap_or_else(|e| panic!("encoded {w}x{h} does not decode: {e:?}"));
    assert_eq!((dec.width, dec.height), (w, h));
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            assert_eq!(dec.get(x, y), bm.get(x, y), "pixel ({x}, {y}) of {w}x{h}");
        }
    }
});
