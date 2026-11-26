//! The `jbig2` operations of the parity oracle (docs/parity.md, "Operations"): `jbig2-encode` and
//! `jbig2-image` on `.pbm` inputs, `jbig2-decode` and `jbig2-decode-globals` on `.jb2` inputs.
//! Mirrors `parsePBM` and the bilevel operations of the Go oracle's `ops.go` and
//! `opJBIG2DecodeGlobals` of `ops_content.go`.

use crate::common::{Dw, base_lim, bitmap_dump, dump_encoded, emit, ext_of, raw, read_in_dir};
use byblos::Bitmap;
use byblos::jbig2::{
    decode_embedded_stream_with_globals, decode_generic, encode_generic, encode_image,
};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// The globals stream that `jbig2-decode-globals` pairs with every other JBIG2 stream.
const GLOBALS_NAME: &str = "fx-jbig2-symbol-globals.jb2";

/// One token of a PBM header: skips white space and `#` comments, then reads up to the next
/// white space or `#`.
fn token<'a>(data: &'a [u8], pos: &mut usize) -> &'a [u8] {
    while let Some(&c) = data.get(*pos) {
        if c == b'#' {
            while data.get(*pos).is_some_and(|&c| c != b'\n') {
                *pos += 1;
            }
        } else if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            *pos += 1;
        } else {
            break;
        }
    }
    let start = *pos;
    while data.get(*pos).is_some_and(|c| !b" \t\n\r#".contains(c)) {
        *pos += 1;
    }
    data.get(start..*pos).unwrap_or_default()
}

fn number(t: &[u8]) -> Option<i64> {
    std::str::from_utf8(t).ok()?.parse().ok()
}

/// `parsePBM`: a binary PBM (P4): the magic, width and height separated by white space (and `#`
/// comments), one white-space byte, and exactly `stride * height` bytes. A side is 1 to 1<<14.
/// `None` is "no file".
fn parse_pbm(data: &[u8]) -> Option<Bitmap> {
    let mut pos = 0;
    if token(data, &mut pos) != b"P4" {
        return None;
    }
    let w = number(token(data, &mut pos))?;
    let h = number(token(data, &mut pos))?;
    let side = 1..=(1i64 << 14);
    if !side.contains(&w) || !side.contains(&h) || pos >= data.len() {
        return None;
    }
    pos += 1; // the single white-space byte
    let (width, height) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?);
    let stride = width.div_ceil(8);
    let rest = data.get(pos..)?;
    if rest.len() != stride.checked_mul(height)? {
        return None;
    }
    Some(Bitmap {
        width,
        height,
        stride,
        pix: rest.to_vec(),
    })
}

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(name: &str, data: &[u8], threads: usize, out: &Path) -> std::io::Result<()> {
    let lim = base_lim(threads);
    let cancel = AtomicBool::new(false);
    match ext_of(name).as_str() {
        "pbm" => {
            let Some(m) = parse_pbm(data) else {
                return Ok(());
            };
            emit(out, name, "jbig2-encode", || {
                Ok(Some(raw(&encode_generic(&m, &lim, &cancel)?)))
            })?;
            emit(out, name, "jbig2-image", || {
                let e = encode_image(&m, &lim, &cancel)?;
                let mut d = Dw::default();
                dump_encoded(&mut d, "", &e);
                Ok(Some(d.b))
            })
        }
        "jb2" => {
            emit(out, name, "jbig2-decode", || {
                Ok(Some(bitmap_dump(&decode_generic(data, &lim, &cancel)?)))
            })?;
            // No file for the globals stream itself or when the input directory has none.
            if name == GLOBALS_NAME {
                return Ok(());
            }
            let Some(globals) = read_in_dir(GLOBALS_NAME) else {
                return Ok(());
            };
            emit(out, name, "jbig2-decode-globals", || {
                let m = decode_embedded_stream_with_globals(&globals, data, &lim, &cancel)?;
                Ok(Some(bitmap_dump(&m)))
            })
        }
        _ => Ok(()),
    }
}
