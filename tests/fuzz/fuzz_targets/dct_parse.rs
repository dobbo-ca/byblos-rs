#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::dct::{self, Frame};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(f) = dct::parse(data) {
        let _ = f.need();
    }
    let _ = dct::check_frame(data, 1 << 22, -1, 0);
    let _ = dct::check_frame(data, 1 << 22, 1 << 20, 3);
    // A hand-built frame: need() must not overflow whatever the sizes.
    if let Some(b) = data.get(..16) {
        let u = |i: usize| usize::from(u16::from_be_bytes([b[i], b[i + 1]]));
        let f = Frame {
            w: u(0) << 8 | u(2),
            h: u(4) << 8 | u(6),
            n: usize::from(b[8]),
            samp: [[u(9) & 0xF, u(10) & 0xF]; 4],
            coefs: b[11] & 1 != 0,
        };
        let _ = f.need();
    }
});
