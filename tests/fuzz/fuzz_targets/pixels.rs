#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::Limits;
use byblos::image::{Cmyk, Gray, Gray16, Image, Nrgba, Nrgba64, Rect, Rgba, Rgba64};
use libfuzzer_sys::fuzz_target;

fn fill(pix: &mut [u8], src: &[u8]) {
    for (d, s) in pix.iter_mut().zip(src.iter().cycle()) {
        *d = *s;
    }
}

fn make(kind: u8, rect: Rect, src: &[u8], lim: &Limits) -> Option<Image> {
    macro_rules! mk {
        ($t:ident) => {{
            let mut i = $t::new(rect, lim).ok()?;
            fill(&mut i.pix, src);
            Image::$t(i)
        }};
    }
    Some(match kind % 7 {
        0 => mk!(Gray),
        1 => mk!(Gray16),
        2 => mk!(Rgba),
        3 => mk!(Nrgba),
        4 => mk!(Cmyk),
        5 => mk!(Rgba64),
        _ => mk!(Nrgba64),
    })
}

// Input: width, height (u16 each, mod 160 + 1), image kind, colours, source dpi, target dpi,
// declared bpc, a spare, then the pixel bytes (repeated to fill the image).
fuzz_target!(|data: &[u8]| {
    let Some(h) = data.get(..10) else { return };
    let src = &data[10..];
    let w = i64::from(u16::from_be_bytes([h[0], h[1]]) % 160 + 1);
    let ht = i64::from(u16::from_be_bytes([h[2], h[3]]) % 160 + 1);
    let rect = Rect::new(0, 0, w, ht);
    let lim = common::limits();
    let Some(img) = make(h[4], rect, src, &lim) else {
        return;
    };
    let _ = byblos::sauvola::sauvola(&img, &lim, &common::NEVER);
    let bpc = [None, Some(1), Some(8), Some(16), Some(0)][usize::from(h[7]) % 5];
    let _ = byblos::downsample::downsample(
        &img,
        bpc,
        100.0 + f64::from(h[6]) * 2.0,
        10.0 + f64::from(h[8]),
        &lim,
        &common::NEVER,
    );
    // quantize needs an opaque image.
    if let Ok(mut q) = Nrgba::new(rect, &lim) {
        fill(&mut q.pix, src);
        for px in q.pix.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let q = Image::Nrgba(q);
        let colors = 2 + usize::from(h[5]) % 255;
        let _ = byblos::quantize::quantize_png(&q, colors, &lim, &common::NEVER);
        let _ = byblos::quantize::quantize_indexed(&q, colors, &lim, &common::NEVER);
    }
});
