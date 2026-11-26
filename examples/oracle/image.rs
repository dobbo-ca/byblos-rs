//! The `image` operations of the parity oracle (docs/parity.md, "Operations"): `embed`,
//! `rotation`, `orientation`, `transcode`, `decode`, `sauvola`, `downsample-*` and `quantize-*`,
//! for `.png`, `.jpg` and `.jp2` inputs. Mirrors `opsFor` and the image operations of the Go
//! oracle's `ops.go`.

use crate::common::{
    Dw, Op, base_lim, bitmap_dump, dump_encoded, dump_resolution, embed_any, emit, ext_of,
    image_dump, raw, tight_lim,
};
use byblos::build::transcode_png;
use byblos::downsample::downsample;
use byblos::embed::{exif_orientation, exif_rotation};
use byblos::image::{Image, decode, decode_config};
use byblos::quantize::{quantize_indexed, quantize_png};
use byblos::sauvola::sauvola;
use byblos::{Error, Limits};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Images over this many declared pixels are skipped by the image ops (Go's `maxDecodePixels`).
const MAX_DECODE_PIXELS: u64 = 1 << 22;

/// `loadImage`: `image.Decode`, except that an image whose header declares more than
/// `MAX_DECODE_PIXELS` is skipped (`Ok(None)`). If the header does not parse, the decode runs
/// and its error is the result.
fn load_image(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Option<Image>, Error> {
    if let Ok(cfg) = decode_config(data, lim)
        && (cfg.width as u64).saturating_mul(cfg.height as u64) > MAX_DECODE_PIXELS
    {
        return Ok(None);
    }
    decode(data, lim, cancel).map(Some)
}

/// Runs `f` on the loaded image (Go loads it again for each operation).
fn on_image(
    data: &[u8],
    base: &Limits,
    cancel: &AtomicBool,
    f: impl FnOnce(&Image) -> Result<Vec<u8>, Error>,
) -> Op {
    match load_image(data, base, cancel)? {
        None => Ok(None),
        Some(img) => f(&img).map(Some),
    }
}

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(name: &str, data: &[u8], threads: usize, out: &Path) -> std::io::Result<()> {
    let ext = ext_of(name);
    if !matches!(ext.as_str(), "png" | "jpg" | "jp2") {
        return Ok(());
    }
    let base = base_lim(threads);
    let tight = tight_lim(threads);
    let cancel = AtomicBool::new(false);

    for (sfx, lim) in [("", &base), ("-tight", &tight)] {
        emit(out, name, &format!("embed{sfx}"), || {
            let (img, res) = embed_any(&ext, data, lim)?;
            let mut d = Dw::default();
            dump_encoded(&mut d, "", &img);
            dump_resolution(&mut d, &res);
            Ok(Some(d.b))
        })?;
        emit(out, name, &format!("rotation{sfx}"), || {
            let r = exif_rotation(data, lim)?;
            Ok(Some(format!("rotation={r}\n").into_bytes()))
        })?;
        emit(out, name, &format!("orientation{sfx}"), || {
            let o = exif_orientation(data, lim)?;
            let mut d = Dw::default();
            d.i("rotate", i64::from(o.rotate));
            d.b2("ambiguous", o.ambiguous);
            Ok(Some(d.b))
        })?;
        // PNG only; the scratch callback of Go is dropped (docs/go-differences.md).
        if ext == "png" {
            emit(out, name, &format!("transcode{sfx}"), || {
                let (img, res) = transcode_png(data, lim, &cancel)?;
                let mut d = Dw::default();
                dump_encoded(&mut d, "", &img);
                dump_resolution(&mut d, &res);
                Ok(Some(d.b))
            })?;
        }
        emit(out, name, &format!("downsample-150{sfx}"), || {
            on_image(data, &base, &cancel, |img| {
                let o = downsample(img, Some(8), 300.0, 150.0, lim, &cancel)?;
                Ok(image_dump(&o))
            })
        })?;
    }

    emit(out, name, "decode", || {
        on_image(data, &base, &cancel, |img| Ok(image_dump(img)))
    })?;
    emit(out, name, "sauvola", || {
        on_image(data, &base, &cancel, |img| {
            Ok(bitmap_dump(&sauvola(img, &base, &cancel)?))
        })
    })?;
    emit(out, name, "downsample-bpc1", || {
        on_image(data, &base, &cancel, |img| {
            let o = downsample(img, Some(1), 300.0, 150.0, &base, &cancel)?;
            Ok(image_dump(&o))
        })
    })?;
    for colors in [2usize, 16, 256] {
        emit(out, name, &format!("quantize-png-{colors}"), || {
            on_image(data, &base, &cancel, |img| {
                Ok(raw(&quantize_png(img, colors, &base, &cancel)?))
            })
        })?;
    }
    emit(out, name, "quantize-indexed-16", || {
        on_image(data, &base, &cancel, |img| {
            let e = quantize_indexed(img, 16, &base, &cancel)?;
            let mut d = Dw::default();
            dump_encoded(&mut d, "", &e);
            Ok(d.b)
        })
    })
}
