#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::go::xml::Decoder;
use libfuzzer_sys::fuzz_target;

// The XML tokenizer on its own, then the same bytes as the XMP packet of a JPEG (an APP1 segment),
// which reaches the orientation rule of embed.
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    if let Ok(mut d) = Decoder::new(data, &lim) {
        for _ in 0..data.len().saturating_mul(2) + 16 {
            if d.token().is_err() {
                break;
            }
        }
    }
    let pkt = &data[..data.len().min(60_000)];
    let id = b"http://ns.adobe.com/xap/1.0/\0";
    let len = (2 + id.len() + pkt.len()) as u16;
    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpg.extend_from_slice(&len.to_be_bytes());
    jpg.extend_from_slice(id);
    jpg.extend_from_slice(pkt);
    jpg.extend_from_slice(&[0xFF, 0xD9]);
    let _ = byblos::embed::exif_orientation(&jpg, &lim);
});
