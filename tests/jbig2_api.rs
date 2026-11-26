//! Ports of the root-package JBIG2 tests of byblos: `jbig2_test.go`, `jbig2_image_test.go` and
//! `TestDecodeJBIG2GenericRoundTripsTheEncoder` of `jbig2_decode_test.go`.
//!
//! Not ported here: the tests that need the PDF layer (`ExtractPageRaster`, `ReplaceImages`,
//! `decodeJBIG2Placement`, `Inspect`) and `Capabilities()`. The decode refusals and budgets of
//! `jbig2_decode_test.go` that call `jbig2.DecodeEmbeddedStream` directly are ported in
//! `src/jbig2/tests.rs`. A nil bitmap and negative dimensions cannot be built in Rust, so
//! `RejectsANilBitmap...` has no counterpart and `AttributesItsOwnRefusals` keeps the
//! zero-size cases.

use byblos::jbig2::{CAPABILITY_JBIG2_GENERIC, decode_generic, encode_generic, encode_image};
use byblos::{Bitmap, Error, Limits};
use std::sync::atomic::AtomicBool;

fn lim() -> Limits {
    Limits::default()
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn enc(b: &Bitmap) -> Result<Vec<u8>, Error> {
    encode_generic(b, &lim(), &no_cancel())
}

fn raw(width: usize, height: usize, stride: usize, pix: Vec<u8>) -> Bitmap {
    Bitmap {
        width,
        height,
        stride,
        pix,
    }
}

/// The 54x44 bitmap with a 2-pixel border of ITU-T T.88 Annex H.1.
fn border_bitmap() -> Bitmap {
    let (w, h) = (54usize, 44usize);
    let stride = w.div_ceil(8);
    let mut b = raw(w, h, stride, vec![0; stride * h]);
    for y in 0..h {
        for x in 0..w {
            if !(2..h - 2).contains(&y) || !(2..w - 2).contains(&x) {
                b.pix[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    b
}

/// `jbig2TestBitmap`: 101x73 with a solid top row and left column and a regular pattern.
fn test_bitmap() -> Bitmap {
    let mut b = Bitmap::new(101, 73, &lim()).unwrap();
    for y in 0..73 {
        for x in 0..101 {
            if y == 0 || x == 0 || (x * 7 + y * 13) % 11 < 4 {
                b.set(x, y, 1);
            }
        }
    }
    b
}

fn message(e: &Error) -> String {
    e.to_string()
}

#[test]
fn encode_reaches_spec_vector() {
    let got = enc(&border_bitmap()).unwrap();
    let want = [0x04, 0xEE, 0xED, 0x87, 0xFB, 0xCB, 0x2B, 0xFF, 0xAC];
    assert!(
        got.ends_with(&want),
        "stream does not end with the T.88 Annex H.1 region data: {:02X?}",
        &got[got.len().saturating_sub(want.len())..]
    );
}

#[test]
fn encode_rejects_empty_bitmap() {
    assert!(enc(&raw(0, 0, 0, Vec::new())).is_err());
}

#[test]
fn encode_rejects_short_pix_slice() {
    assert!(enc(&raw(16, 4, 2, vec![0; 3])).is_err());
}

#[test]
fn capability_string_is_stable() {
    assert_eq!(CAPABILITY_JBIG2_GENERIC, "jbig2-generic");
}

#[test]
fn encode_rejects_a_stride_too_small_for_the_width() {
    for b in [raw(16, 4, 1, vec![0; 4]), raw(17, 4, 2, vec![0; 8])] {
        let e = enc(&b).unwrap_err();
        assert!(message(&e).contains("stride"), "{e}");
    }
}

#[test]
fn encode_rejects_a_pix_slice_one_byte_short() {
    let e = enc(&raw(16, 4, 2, vec![0; 2 * 4 - 1])).unwrap_err();
    assert!(message(&e).contains("want at least"), "{e}");
}

#[test]
fn encode_attributes_its_own_refusals() {
    for b in [raw(0, 4, 2, vec![0; 8]), raw(16, 0, 2, Vec::new())] {
        let e = enc(&b).unwrap_err();
        assert!(matches!(e, Error::Malformed(_)), "{e:?}");
    }
}

#[test]
fn encode_rejects_a_stride_height_overflow() {
    // The product is over i64::MAX, as in Go.
    let b = raw(16, (1 << 32) - 1, 1 << 32, vec![0; 8]);
    assert!(message(&enc(&b).unwrap_err()).contains("overflow"));
}

#[test]
fn encode_image_dictionary() {
    let src = test_bitmap();
    let img = encode_image(&src, &lim(), &no_cancel()).unwrap();
    assert_eq!(img.width, src.width as i64);
    assert_eq!(img.height, src.height as i64);
    assert_eq!(img.bpc, 1);
    assert_eq!(img.color_space.name, "DeviceGray");
    assert_eq!(img.filter, "JBIG2Decode");
    assert!(img.decode.is_empty() && img.decode_parms.is_none() && img.smask.is_none());
}

#[test]
fn encode_does_not_modify_a_well_formed_bitmap() {
    let src = test_bitmap();
    let before = src.clone();
    enc(&src).unwrap();
    assert_eq!(src, before);
}

#[test]
fn decode_round_trips_the_encoder() {
    let mut all_ink = Bitmap::new(13, 11, &lim()).unwrap();
    for y in 0..11 {
        for x in 0..13 {
            all_ink.set(x, y, 1);
        }
    }
    for want in [
        test_bitmap(),
        Bitmap::new(1, 1, &lim()).unwrap(),
        Bitmap::new(64, 8, &lim()).unwrap(),
        all_ink,
    ] {
        let data = enc(&want).unwrap();
        let got = decode_generic(&data, &lim(), &no_cancel()).unwrap();
        assert_eq!(got, want, "{}x{}", want.width, want.height);
    }
}

#[test]
fn cancelled_flag_stops_encode_and_decode() {
    let cancelled = AtomicBool::new(true);
    let src = test_bitmap();
    assert_eq!(
        encode_generic(&src, &lim(), &cancelled),
        Err(Error::Cancelled)
    );
    let data = enc(&src).unwrap();
    assert_eq!(
        decode_generic(&data, &lim(), &cancelled),
        Err(Error::Cancelled)
    );
}
