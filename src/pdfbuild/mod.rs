//! Wave 1d: port of byblos `internal/pdfbuild/{pdfbuild,png,pngres}.go`: constructs a PDF from
//! already-encoded page images, with a hand-rolled streaming writer (no pdf-rs), and
//! transcodes PNG files that cannot be carried verbatim into Flate colour plus a Flate
//! `/SMask`.
//!
//! * `pdfbuild.go` is this file: [`Page`], [`write`], [`Builder`], `validatePage` and the
//!   minimal PDF writer.
//! * `png.go` is [`png`] ([`transcode_png`]) and `pngres.go` is [`pngres`] ([`PngPhys`]).
//!
//! Go's `Write(out, pages, lim)` takes no context; here every entry also takes the cancel flag,
//! because the inflate it reaches (the zlib header check and the `/Indexed` palette check) does.
//! Go's untyped `fmt.Errorf` errors are [`Error::Malformed`] (class `error`); a wrapped
//! standard-library error (zlib, unexpected EOF) stays an [`Error::Pdf`] so that its class
//! (`pdf/zlib/header`, `pdf/unexpected-eof`, ...) is the one Go's oracle reports.
//!
//! The Flate output of [`transcode_png`] is Go's `zlib.BestSpeed` (level 1), through pdf-rs's
//! `goflate` (`new_zlib_writer` in `png.rs`).

pub mod png;
pub mod pngres;

pub use png::transcode_png;
pub use pngres::PngPhys;

use crate::encoded::{DecodeParms, EncodedImage};
use crate::error::{Error, Result};
use crate::go::fmt::format_g;
use crate::limits::{Limits, check, check_cancel, mul_sat};
use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::sync::atomic::AtomicBool;

#[cfg(test)]
mod tests;

/// `JP2Signature`: the 12-byte JP2 signature box (ISO/IEC 15444-1 I.5.1) every JP2 file starts
/// with.
pub const JP2_SIGNATURE: [u8; 12] = [0, 0, 0, 12, b'j', b'P', b' ', b' ', 0x0D, 0x0A, 0x87, 0x0A];

/// `minCoord` and `maxCoord` bound every length pdfbuild emits into a MediaBox or a placement
/// matrix. `format_num` rounds to six decimal places, so a value below `MIN_COORD` is
/// indistinguishable from zero once rendered, and `MAX_COORD` keeps the token away from the
/// range where fixed notation degenerates and real readers crash.
const MIN_COORD: f64 = 1e-3;
const MAX_COORD: f64 = 1e6;

/// `Page`: one page to build: an encoded image and the page box, in points, to paint it on.
/// The box must already be resolved to a positive, finite size.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page<'a> {
    /// Borrowed by `BuildPDF` (Go copies the slice header only), owned by tests.
    pub image: Cow<'a, EncodedImage>,
    pub width_pt: f64,
    pub height_pt: f64,
    /// `/Rotate`: 0, 90, 180 or 270; 0 writes none.
    pub rotate: i64,
    /// Paints the image over the whole box instead of fit-centering it: the box was derived
    /// from the image's own resolution, so any aspect mismatch is the file's declared
    /// anisotropy (img2pdf draws it that way).
    pub fill: bool,
}

// --- error helpers -------------------------------------------------------------------------

/// An untyped `fmt.Errorf`.
pub(crate) fn malformed(msg: impl Into<String>) -> Error {
    Error::Malformed(msg.into())
}

/// `fmt.Errorf("<prefix>: %w", e)`: the message gains the prefix, the class stays. Errors that
/// carry no message (limits, cancellation) pass through unchanged.
pub(crate) fn wrap(e: Error, prefix: &str) -> Error {
    match e {
        Error::Malformed(s) => Error::Malformed(format!("{prefix}: {s}")),
        Error::Pdf(p) => Error::Pdf(p.context(prefix)),
        other => other,
    }
}

/// `io.ErrUnexpectedEOF`.
pub(crate) fn unexpected_eof() -> Error {
    Error::Pdf(pdf::Error::UnexpectedEof)
}

/// A decode error of deflate-rs as the error the Go standard library's flate/zlib gives.
pub(crate) fn flate(e: deflate::Error) -> Error {
    Error::from(pdf::Error::Flate(e))
}

/// Whether `e` is (or wraps) `io.ErrUnexpectedEOF`.
pub(crate) fn is_unexpected_eof(e: &Error) -> bool {
    match e {
        Error::Pdf(p) => matches!(
            p.root(),
            pdf::Error::UnexpectedEof | pdf::Error::Flate(deflate::Error::UnexpectedEof)
        ),
        _ => false,
    }
}

/// An error of the underlying `io::Read` or `io::Write`.
pub(crate) fn io_err(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        return unexpected_eof();
    }
    malformed(e.to_string())
}

/// The error behind an `io::Error` that deflate-rs's reader returned: `side` holds an error of
/// the reader's own source when it had one, else the deflate error is its source.
pub(crate) fn read_err(e: &io::Error, side: Option<&RefCell<Option<Error>>>) -> Error {
    if let Some(s) = side
        && let Some(own) = s.borrow_mut().take()
    {
        return own;
    }
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<deflate::Error>())
    {
        Some(de) => flate(de.clone()),
        None => io_err(e),
    }
}

/// `n` zero bytes without `vec![0; n]` on an unchecked size.
pub(crate) fn zeroed(n: usize) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)?;
    v.resize(n, 0);
    Ok(v)
}

/// `io.ReadFull`, with Go's errors: `EOF` (class `error`) when nothing was read,
/// `ErrUnexpectedEOF` when the buffer was part filled. `no_eof` is Go's `noEOF`: both are
/// `ErrUnexpectedEOF`.
pub(crate) fn read_full<R: Read>(
    r: &mut R,
    buf: &mut [u8],
    side: Option<&RefCell<Option<Error>>>,
    no_eof: bool,
) -> Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let Some(dst) = buf.get_mut(filled..) else {
            break;
        };
        match r.read(dst) {
            Ok(0) => {
                return Err(if filled == 0 && !no_eof {
                    malformed("EOF")
                } else {
                    unexpected_eof()
                });
            }
            Ok(k) => filled += k,
            Err(e) => return Err(read_err(&e, side)),
        }
    }
    Ok(())
}

// --- formatting ----------------------------------------------------------------------------

/// `formatNum`: v as a PDF real: fixed notation only (ISO 32000-1 section 7.3.3 forbids
/// exponential form), rounded to six decimal places so the output is stable and free of float
/// noise.
pub fn format_num(v: f64) -> String {
    format_g((v * 1e6).round() / 1e6)
}

/// `inRange`: whether `v` is a finite length pdfbuild can safely render.
fn in_range(v: f64) -> bool {
    (MIN_COORD..=MAX_COORD).contains(&v)
}

/// `hex.EncodeToString`.
fn hex_encode(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len().saturating_mul(2));
    for &x in b {
        s.push(char::from(DIGITS[usize::from(x >> 4)]));
        s.push(char::from(DIGITS[usize::from(x & 15)]));
    }
    s
}

/// `deviceComponents`: the sample count of a device colour space.
fn device_components(space: &str) -> Option<i64> {
    match space {
        "DeviceGray" => Some(1),
        "DeviceRGB" => Some(3),
        "DeviceCMYK" => Some(4),
        _ => None,
    }
}

/// `colorSpaceObject`: the object `/ColorSpace` takes: a name for a device space, or the
/// `[/Indexed base hival <lookup>]` array of ISO 32000-1 section 8.6.6.3. Any other space is
/// rejected rather than emitted as untested output.
fn color_space_object(cs: &crate::encoded::ColorSpace) -> Result<String> {
    if cs.name == "Indexed" {
        let Some(n) = device_components(&cs.base) else {
            return Err(malformed(format!(
                "indexed base {:?} is not a device colour space",
                cs.base
            )));
        };
        if !(0..=255).contains(&cs.hi_val) {
            return Err(malformed(format!(
                "indexed hival {} is outside 0..255",
                cs.hi_val
            )));
        }
        let want = (cs.hi_val + 1) * n;
        if cs.lookup.len() as i64 != want {
            return Err(malformed(format!(
                "indexed lookup is {} bytes, want {} for hival {} over {}",
                cs.lookup.len(),
                want,
                cs.hi_val,
                cs.base
            )));
        }
        return Ok(format!(
            "[/Indexed /{} {} <{}>]",
            cs.base,
            cs.hi_val,
            hex_encode(&cs.lookup)
        ));
    }
    if device_components(&cs.name).is_none() {
        return Err(malformed(format!(
            "colour space {:?} is not supported",
            cs.name
        )));
    }
    Ok(format!("/{}", cs.name))
}

// --- validation ----------------------------------------------------------------------------

/// `validatePage`: rejects anything this writer cannot safely turn into a PDF a reader can
/// open: a non-finite or non-positive page box, or an image whose filter, bits per component
/// and colour space combination it does not know how to declare correctly.
///
/// A `FlateDecode` image has its zlib header checked (so a whole PNG file handed in as `data`
/// is caught), and an `/Indexed` one is inflated to prove that no sample is past the palette's
/// `hival` and that the stream holds the rows it declares.
///
/// # Errors
/// [`Error::Limit`] (`encoded-image-pixels`) for dimensions over the cap, [`Error::Malformed`]
/// for every other refusal, [`Error::Pdf`] for a zlib error of the data, [`Error::Cancelled`].
pub fn validate_page(p: &Page, lim: &Limits, cancel: &AtomicBool) -> Result<()> {
    if !matches!(p.rotate, 0 | 90 | 180 | 270) {
        return Err(malformed(format!(
            "/Rotate {} is not 0, 90, 180 or 270",
            p.rotate
        )));
    }
    if !in_range(p.width_pt) || !in_range(p.height_pt) {
        return Err(malformed(format!(
            "page box {}x{} is not in the representable range [{}, {}] points",
            format_g(p.width_pt),
            format_g(p.height_pt),
            format_g(MIN_COORD),
            format_g(MAX_COORD)
        )));
    }
    validate_image(&p.image, lim, cancel)
}

/// The image half of `validatePage`; the page checks of its recursive call for the `/SMask`
/// (rotation 0, the same box) already hold.
fn validate_image(img: &EncodedImage, lim: &Limits, cancel: &AtomicBool) -> Result<()> {
    if img.width <= 0 || img.height <= 0 {
        return Err(malformed(format!(
            "image dimensions {}x{} are not positive",
            img.width, img.height
        )));
    }
    // Each axis is bounded by the pixel cap BEFORE the product is taken (byb-37h), so nothing
    // below can wrap on a declared size.
    let cap = lim.encoded_image_pixels();
    check("encoded-image-pixels", img.width.max(img.height), cap)?;
    check("encoded-image-pixels", mul_sat(img.width, img.height), cap)?;
    if img.data.is_empty() {
        return Err(malformed("image has no data"));
    }
    match img.filter.as_str() {
        "FlateDecode" => validate_flate(img, lim, cancel)?,
        "DCTDecode" => {
            if img.bpc != 8 {
                return Err(malformed(format!(
                    "DCTDecode: bits per component must be 8, got {}",
                    img.bpc
                )));
            }
            if device_components(&img.color_space.name).is_none() {
                return Err(malformed(format!(
                    "DCTDecode: colour space {:?} is not supported (only DeviceGray/DeviceRGB/DeviceCMYK)",
                    img.color_space.name
                )));
            }
        }
        "JPXDecode" => {
            if !(1..=16).contains(&img.bpc) {
                return Err(malformed(format!(
                    "JPXDecode: bits per component {} is not 1 to 16",
                    img.bpc
                )));
            }
            if img.color_space.name != "DeviceGray" && img.color_space.name != "DeviceRGB" {
                return Err(malformed(format!(
                    "JPXDecode: colour space {:?} is not supported (only DeviceGray/DeviceRGB)",
                    img.color_space.name
                )));
            }
            if !img.data.starts_with(&JP2_SIGNATURE) {
                return Err(malformed(
                    "JPXDecode: data does not start with the JP2 signature box",
                ));
            }
        }
        "JBIG2Decode" => {
            if img.bpc != 1 {
                return Err(malformed(format!(
                    "JBIG2Decode: bits per component must be 1, got {}",
                    img.bpc
                )));
            }
            if img.color_space.name != "DeviceGray" {
                return Err(malformed(format!(
                    "JBIG2Decode: colour space must be DeviceGray, got {:?}",
                    img.color_space.name
                )));
            }
        }
        other => return Err(malformed(format!("filter {other:?} is not supported"))),
    }
    if let Some(sm) = &img.smask {
        if sm.smask.is_some() {
            return Err(malformed("an /SMask cannot carry an /SMask"));
        }
        if sm.width != img.width || sm.height != img.height {
            return Err(malformed(format!(
                "/SMask is {}x{}, the image {}x{}",
                sm.width, sm.height, img.width, img.height
            )));
        }
        if sm.filter != "FlateDecode"
            || sm.color_space.name != "DeviceGray"
            || !sm.decode.is_empty()
        {
            return Err(malformed(
                "/SMask must be FlateDecode DeviceGray without /Decode",
            ));
        }
        validate_image(sm, lim, cancel).map_err(|e| wrap(e, "/SMask"))?;
    }
    if !img.decode.is_empty() {
        let n = if img.color_space.name == "Indexed" {
            2
        } else {
            2 * device_components(&img.color_space.name).unwrap_or(0)
        };
        if img.decode.len() as i64 != n {
            return Err(malformed(format!(
                "/Decode has {} entries, want {} for {}",
                img.decode.len(),
                n,
                img.color_space.name
            )));
        }
        for &v in &img.decode {
            if !v.is_finite() {
                return Err(malformed(format!("/Decode holds {}", format_g(v))));
            }
        }
    }
    Ok(())
}

/// The `FlateDecode` arm of `validatePage`.
fn validate_flate(img: &EncodedImage, lim: &Limits, cancel: &AtomicBool) -> Result<()> {
    match img.bpc {
        1 | 2 | 4 | 8 => {}
        16 => {
            if img.color_space.name == "Indexed" {
                return Err(malformed(
                    "FlateDecode: an /Indexed image cannot be 16 bits per component",
                ));
            }
        }
        _ => {
            return Err(malformed(format!(
                "FlateDecode: bits per component {} is not one of 1, 2, 4, 8, 16",
                img.bpc
            )));
        }
    }
    color_space_object(&img.color_space)?;
    // Parsing the zlib header is the cheapest check that catches a whole PNG file handed in as
    // the image data; it does not inflate.
    let dl = deflate_limits(&lim.mem());
    if let Err(e) = deflate::zlib::Reader::new(&img.data[..], &dl, cancel) {
        return Err(wrap(
            flate(e),
            "FlateDecode: data does not decode under the filter it declares",
        ));
    }
    if img.color_space.name == "Indexed" {
        let max = max_sample_value(img, lim, cancel).map_err(|e| {
            wrap(
                e,
                "FlateDecode: /Indexed data cannot be checked against its palette",
            )
        })?;
        if max > img.color_space.hi_val {
            return Err(malformed(format!(
                "FlateDecode: /Indexed data holds index {}, past the palette's hival {}",
                max, img.color_space.hi_val
            )));
        }
    }
    Ok(())
}

/// The inflate limits for a streaming read: the output is bounded by what the caller reads, so
/// only the working memory left in `mem` is capped.
pub(crate) fn deflate_limits(mem: &pdf::Mem) -> deflate::Limits {
    deflate::Limits {
        max_output: usize::MAX,
        max_alloc: usize::try_from(mem.left()).unwrap_or(usize::MAX),
    }
}

/// `maxSampleValue`: the largest sample in `img`'s FlateDecode data, decoded the way a reader
/// does: inflate, undo the `/DecodeParms` predictor, then unpack `width` samples of `bpc` bits
/// from each of `height` rows. It streams: one row is held plus the row before it.
///
/// A sample larger than `hival` indexes past the end of the lookup, and pdf-go, byblos's own
/// reader, does not bounds check it. Having inflated the data, the check also notices a stream
/// that runs out before the rows it declares.
fn max_sample_value(img: &EncodedImage, lim: &Limits, cancel: &AtomicBool) -> Result<i64> {
    let mem = lim.mem();
    let dl = deflate_limits(&mem);
    let mut zr = deflate::zlib::Reader::new(&img.data[..], &dl, cancel).map_err(flate)?;

    let predicted = png_predicted(img)?;
    // ISO 32000-1 section 8.9.5.1: each image row begins on a byte boundary.
    let bits = img
        .width
        .checked_mul(img.bpc)
        .and_then(|b| b.checked_add(7))
        .ok_or_else(|| malformed("row size overflows"))?;
    let row_len = usize::try_from(bits / 8).map_err(|_| Error::OutOfMemory)?;
    let width = usize::try_from(img.width).map_err(|_| Error::OutOfMemory)?;
    let bpc = u32::try_from(img.bpc).map_err(|_| malformed("bad bits per component"))?;

    let mut largest = 0i64;
    let mut cur: Vec<u8> = Vec::new();
    let mut prev: Vec<u8> = Vec::new();
    let mut held = None;
    let mut ft = [0u8; 1];
    for y in 0..img.height {
        check_cancel(cancel)?;
        if predicted {
            read_full(&mut zr, &mut ft, None, false).map_err(|e| {
                wrap(
                    e,
                    &format!("row {}: reading the predictor's filter type", y + 1),
                )
            })?;
        }
        if cur.is_empty() {
            // The first row is read into a buffer that grows as the bytes arrive, so a
            // width x height declaration far larger than the data behind it is reported as a
            // short stream rather than allocated for. Once one row has proven the stream
            // really is that wide, prev can be allocated outright.
            held = Some(mem.charge((row_len as u64).saturating_mul(2))?);
            cur = read_row(&mut zr, row_len).map_err(|e| wrap(e, &format!("row {}", y + 1)))?;
            if img.height == 1 {
                // Go's io.ReadAll over zlib returns the trailer's error (checksum, short
                // stream) together with the last bytes; read once more to see it.
                if let Err(e) = zr.read(&mut [0u8; 1]) {
                    return Err(wrap(read_err(&e, None), "row 1"));
                }
            }
            // PNG treats the row above row 1 as all zeroes.
            prev = zeroed(row_len)?;
        } else {
            read_full(&mut zr, &mut cur, None, false)
                .map_err(|e| wrap(e, &format!("row {}", y + 1)))?;
        }
        if predicted {
            let ft0 = ft.first().copied().unwrap_or(0);
            unfilter_png_row(ft0, &mut cur, &prev, 1)
                .map_err(|e| wrap(e, &format!("row {}", y + 1)))?;
        }
        let v = max_in_row(&cur, bpc, width);
        if v > largest {
            largest = v;
        }
        // The reconstructed row becomes the row above; its predecessor is the buffer the next
        // row is read into.
        std::mem::swap(&mut cur, &mut prev);
    }
    drop(held);
    Ok(largest)
}

/// `readRow`: reads exactly `n` bytes, growing the buffer as they arrive rather than
/// allocating `n` up front.
fn read_row<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut row: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while row.len() < n {
        let want = (n - row.len()).min(chunk.len());
        let Some(dst) = chunk.get_mut(..want) else {
            break;
        };
        match r.read(dst) {
            Ok(0) => break,
            Ok(k) => {
                row.try_reserve(k)?;
                if let Some(got) = dst.get(..k) {
                    row.extend_from_slice(got);
                }
            }
            Err(e) => return Err(read_err(&e, None)),
        }
    }
    if row.len() != n {
        return Err(malformed(format!(
            "stream holds {} of the {} bytes the row needs",
            row.len(),
            n
        )));
    }
    Ok(row)
}

/// `pngPredicted`: whether `img`'s data carries the PNG predictors' per-row filter type byte
/// (ISO 32000-1 table 10 gives `/Predictor >= 10` to them, and 1 or absent to "no prediction"),
/// and refuses parameters whose layout `max_sample_value` cannot reconstruct.
fn png_predicted(img: &EncodedImage) -> Result<bool> {
    let Some(p) = &img.decode_parms else {
        return Ok(false);
    };
    if p.predictor <= 1 {
        return Ok(false);
    }
    if p.predictor < 10 {
        return Err(malformed(format!(
            "/DecodeParms /Predictor {} (TIFF prediction) has no byblos producer",
            p.predictor
        )));
    }
    // ISO 32000-1 table 10 defaults: 1 colour, 8 bits, 1 column.
    let colors = if p.colors == 0 { 1 } else { p.colors };
    let bpc = if p.bits_per_component == 0 {
        8
    } else {
        p.bits_per_component
    };
    let columns = if p.columns == 0 { 1 } else { p.columns };
    if colors != 1 || bpc != img.bpc || columns != img.width {
        return Err(malformed(format!(
            "/DecodeParms ({} colours, {} bits, {} columns) describes different rows than the image (1 colour, {} bits, {} columns)",
            colors, bpc, columns, img.bpc, img.width
        )));
    }
    Ok(true)
}

/// `unfilterPNGRow`: reconstructs one PNG-predicted row in place, given the reconstructed row
/// above it (ISO/IEC 15948 section 9.2). `bpp` is PNG's: the byte distance to the pixel on the
/// left, 1 for any pixel under 8 bits.
///
/// # Errors
/// [`Error::Malformed`] for a filter type other than 0 to 4.
pub(crate) fn unfilter_png_row(ft: u8, row: &mut [u8], prev: &[u8], bpp: usize) -> Result<()> {
    if prev.len() < row.len() {
        return Err(malformed("the row above is shorter than the row"));
    }
    let n = row.len();
    match ft {
        0 => {} // None
        1 => {
            // Sub
            for i in bpp..n {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        2 => {
            // Up
            for (r, &u) in row.iter_mut().zip(prev) {
                *r = r.wrapping_add(u);
            }
        }
        3 => {
            // Average
            for i in 0..n {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let sum = usize::from(left) + usize::from(prev[i]);
                row[i] = row[i].wrapping_add((sum / 2) as u8);
            }
        }
        4 => {
            // Paeth
            for i in 0..n {
                let (left, up_left) = if i >= bpp {
                    (row[i - bpp], prev[i - bpp])
                } else {
                    (0, 0)
                };
                row[i] = row[i].wrapping_add(paeth_predictor(left, prev[i], up_left));
            }
        }
        other => {
            return Err(malformed(format!(
                "predictor filter type {other} is not one of PNG's 0..4"
            )));
        }
    }
    Ok(())
}

/// `paethPredictor`: ISO/IEC 15948 section 9.4's PaethPredictor: of the byte to the left `a`,
/// the byte above `b` and the byte above-left `c`, the one closest to `a + b - c`.
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (i32::from(a), i32::from(b), i32::from(c));
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// `maxInRow`: the largest of the first `width` samples of `bpc` bits packed into `row`,
/// high-order bits first (ISO 32000-1 section 8.9.5.1). Bits past the last sample are row
/// padding. It walks bytes, not samples.
fn max_in_row(row: &[u8], bpc: u32, width: usize) -> i64 {
    if bpc == 8 {
        return row
            .get(..width)
            .unwrap_or_default()
            .iter()
            .map(|&b| i64::from(b))
            .max()
            .unwrap_or(0);
    }
    if bpc == 0 || bpc > 8 {
        return 0;
    }
    let per_byte = (8 / bpc) as usize;
    let full = width / per_byte;
    let mut largest = 0;
    for &b in row.get(..full).unwrap_or_default() {
        largest = largest.max(max_in_byte(b, bpc, per_byte));
    }
    // The last byte of a row whose width is not a whole number of bytes is only partly
    // samples; the rest is padding.
    let rem = width % per_byte;
    if rem > 0
        && let Some(&b) = row.get(full)
    {
        largest = largest.max(max_in_byte(b, bpc, rem));
    }
    largest
}

/// `maxInByte`: the largest of the first `n` samples of `bpc` bits in `b`, high-order bits
/// first.
fn max_in_byte(b: u8, bpc: u32, n: usize) -> i64 {
    let mask = (1u32 << bpc) - 1;
    let mut largest = 0;
    for i in 0..n as u32 {
        let Some(shift) = 8u32.checked_sub(bpc * (i + 1)) else {
            break;
        };
        let v = i64::from((u32::from(b) >> shift) & mask);
        largest = largest.max(v);
    }
    largest
}

// --- rendering of the dictionaries ----------------------------------------------------------

/// `decodeParmsDict`: `/DecodeParms`, or "" when there is nothing to say. Only for
/// `FlateDecode`: its keys are the PNG predictor parameters (ISO 32000-1 Table 12), which
/// `DCTDecode` and `JBIG2Decode` do not accept.
fn decode_parms_dict(p: Option<&DecodeParms>) -> String {
    let Some(p) = p else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if p.predictor != 0 {
        parts.push(format!("/Predictor {}", p.predictor));
    }
    if p.colors != 0 {
        parts.push(format!("/Colors {}", p.colors));
    }
    if p.bits_per_component != 0 {
        parts.push(format!("/BitsPerComponent {}", p.bits_per_component));
    }
    if p.columns != 0 {
        parts.push(format!("/Columns {}", p.columns));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("<< {} >>", parts.join(" "))
}

/// `imageDict`: the image XObject dictionary contents (without the enclosing `<< >>`) for
/// `img`; `smask` is the object number of its soft mask, or 0.
fn image_dict(img: &EncodedImage, smask: usize) -> Result<String> {
    let cs = color_space_object(&img.color_space)?;
    let mut d = format!(
        "/Type /XObject /Subtype /Image /Width {} /Height {} /BitsPerComponent {} /ColorSpace {} /Filter /{}",
        img.width, img.height, img.bpc, cs, img.filter
    );
    // /DecodeParms only has a defined meaning for FlateDecode here.
    if img.filter == "FlateDecode" {
        let parms = decode_parms_dict(img.decode_parms.as_ref());
        if !parms.is_empty() {
            d.push_str(" /DecodeParms ");
            d.push_str(&parms);
        }
    }
    if !img.decode.is_empty() {
        let v: Vec<String> = img.decode.iter().map(|&f| format_num(f)).collect();
        d.push_str(" /Decode [");
        d.push_str(&v.join(" "));
        d.push(']');
    }
    if smask > 0 {
        d.push_str(&format!(" /SMask {smask} 0 R"));
    }
    Ok(d)
}

// --- the minimal streaming PDF writer -------------------------------------------------------

/// `writer`: emits PDF objects directly to an `io::Write`, tracking each object's byte offset
/// so the xref table can be written last without rewinding. The first write error is sticky,
/// as in Go.
struct PdfWriter<W: Write> {
    w: W,
    /// Bytes written so far.
    n: usize,
    /// `offsets[i - 1]` is the byte offset of object `i`; `None` until filled.
    offsets: Vec<Option<usize>>,
    err: Option<Error>,
}

impl<W: Write> PdfWriter<W> {
    fn new(out: W) -> Self {
        let mut w = PdfWriter {
            w: out,
            n: 0,
            offsets: Vec::new(),
            err: None,
        };
        // The binary comment line marks the file as containing binary data, per ISO 32000-1
        // section 7.5.2.
        w.write(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
        w
    }

    fn write(&mut self, b: &[u8]) {
        if self.err.is_some() {
            return;
        }
        match self.w.write_all(b) {
            Ok(()) => self.n += b.len(),
            Err(e) => self.err = Some(io_err(&e)),
        }
    }

    fn write_str(&mut self, s: &str) {
        self.write(s.as_bytes());
    }

    /// The sticky error, if any.
    fn result(&self) -> Result<()> {
        match &self.err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// `reserve`: allocates an object number to be filled later, so parents can refer to
    /// children that have not been written yet.
    fn reserve(&mut self) -> usize {
        self.offsets.push(None);
        self.offsets.len()
    }

    fn set_offset(&mut self, n: usize) {
        let at = self.n;
        if let Some(slot) = n.checked_sub(1).and_then(|i| self.offsets.get_mut(i)) {
            *slot = Some(at);
        }
    }

    fn fill(&mut self, n: usize, body: &str) {
        self.set_offset(n);
        self.write_str(&format!("{n} 0 obj\n{body}\nendobj\n"));
    }

    /// `fillStream`: a stream object whose payload is stored verbatim: by the time a page
    /// reaches this writer its image bytes are already in whatever `/Filter` names.
    fn fill_stream(&mut self, n: usize, dict: &str, payload: &[u8]) {
        self.set_offset(n);
        let sep = if dict.is_empty() { "" } else { " " };
        self.write_str(&format!(
            "{n} 0 obj\n<< {dict}{sep}/Length {} >>\nstream\n",
            payload.len()
        ));
        self.write(payload);
        self.write_str("\nendstream\nendobj\n");
    }

    /// `finish`: the cross-reference table and trailer. Each xref entry is exactly 20 bytes, as
    /// ISO 32000-1 section 7.5.4 requires.
    fn finish(&mut self, root: usize) -> Result<()> {
        self.result()?;
        let start = self.n;
        self.write_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            self.offsets.len() + 1
        ));
        let mut entries = String::new();
        for (i, off) in self.offsets.iter().enumerate() {
            let Some(off) = off else {
                return Err(malformed(format!(
                    "byblos/pdfbuild: object {} was reserved but never filled",
                    i + 1
                )));
            };
            entries.push_str(&format!("{off:010} 00000 n \n"));
        }
        self.write_str(&entries);
        self.write_str(&format!(
            "trailer\n<< /Size {} /Root {} 0 R >>\nstartxref\n{}\n%%EOF\n",
            self.offsets.len() + 1,
            root,
            start
        ));
        self.result()
    }
}

// --- Builder and Write ----------------------------------------------------------------------

/// `Builder`: writes a PDF one page at a time: [`Builder::add_page`] puts the page's objects on
/// the writer at once and keeps only their numbers, so a caller holds one page's encoded image,
/// not the document. The page tree and the xref come at [`Builder::close`]. [`write`] is a
/// `Builder` run over a whole slice.
pub struct Builder<W: Write> {
    w: PdfWriter<W>,
    lim: Limits,
    catalog: usize,
    pages_root: usize,
    kids: Vec<usize>,
    closed: bool,
}

impl<W: Write> Builder<W> {
    /// `NewBuilder`: starts a PDF on `out`.
    pub fn new(out: W, lim: &Limits) -> Self {
        let mut w = PdfWriter::new(out);
        let catalog = w.reserve();
        let pages_root = w.reserve();
        Builder {
            w,
            lim: *lim,
            catalog,
            pages_root,
            kids: Vec::new(),
            closed: false,
        }
    }

    /// `Pages`: the number of pages added so far.
    pub fn pages(&self) -> usize {
        self.kids.len()
    }

    /// `AddPage`: validates `p` and writes it. A page it refuses writes nothing.
    ///
    /// # Errors
    /// The errors of [`validate_page`], a scaled image size that is not representable, and the
    /// first write error of the underlying writer.
    pub fn add_page(&mut self, p: &Page, cancel: &AtomicBool) -> Result<()> {
        if self.closed {
            return Err(malformed("byblos/pdfbuild: AddPage after Close"));
        }
        validate_page(p, &self.lim, cancel)
            .map_err(|e| wrap(e, &format!("byblos/pdfbuild: page {}", self.kids.len() + 1)))?;
        self.emit(p)
    }

    /// `emit`: writes the objects of a validated page.
    fn emit(&mut self, p: &Page) -> Result<()> {
        let img = &p.image;
        let scale = (p.width_pt / img.width as f64).min(p.height_pt / img.height as f64);
        let mut dw = img.width as f64 * scale;
        let mut dh = img.height as f64 * scale;
        if p.fill {
            dw = p.width_pt;
            dh = p.height_pt;
        }
        if !in_range(dw) || !in_range(dh) {
            return Err(malformed(format!(
                "byblos/pdfbuild: page {}: scaled image size {}x{} is not representable",
                self.kids.len() + 1,
                format_g(dw),
                format_g(dh)
            )));
        }
        let tx = (p.width_pt - dw) / 2.0;
        let ty = (p.height_pt - dh) / 2.0;

        let page = self.w.reserve();
        let content = self.w.reserve();
        let image = self.w.reserve();
        let smask = if img.smask.is_some() {
            self.w.reserve()
        } else {
            0
        };
        self.kids.push(page);

        let draw = format!(
            "q {} 0 0 {} {} {} cm /Im0 Do Q\n",
            format_num(dw),
            format_num(dh),
            format_num(tx),
            format_num(ty)
        );
        self.w.fill_stream(content, "", draw.as_bytes());

        let page_no = self.kids.len();
        let ctx = |e: Error| wrap(e, &format!("byblos/pdfbuild: page {page_no}"));
        let dict = image_dict(img, smask).map_err(ctx)?;
        if let Some(sm) = &img.smask {
            let sdict = image_dict(sm, 0).map_err(ctx)?;
            self.w.fill_stream(smask, &sdict, &sm.data);
        }
        self.w.fill_stream(image, &dict, &img.data);

        let rotate = if p.rotate != 0 {
            format!(" /Rotate {}", p.rotate)
        } else {
            String::new()
        };
        self.w.fill(
            page,
            &format!(
                "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 {} {}]{} /Resources << /XObject << /Im0 {} 0 R >> >> /Contents {} 0 R >>",
                self.pages_root,
                format_num(p.width_pt),
                format_num(p.height_pt),
                rotate,
                image,
                content
            ),
        );
        self.w.result()
    }

    /// `Close`: writes the page tree, the catalog and the xref. A document with no page is an
    /// error.
    ///
    /// # Errors
    /// A second call, no pages, or a write error.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Err(malformed("byblos/pdfbuild: Close twice"));
        }
        self.closed = true;
        if self.kids.is_empty() {
            return Err(malformed("byblos/pdfbuild: no pages"));
        }
        let kids: Vec<String> = self.kids.iter().map(|n| format!("{n} 0 R")).collect();
        self.w.fill(
            self.pages_root,
            &format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                self.kids.len()
            ),
        );
        self.w.fill(
            self.catalog,
            &format!("<< /Type /Catalog /Pages {} 0 R >>", self.pages_root),
        );
        self.w.finish(self.catalog)
    }

    /// The underlying writer, after [`Builder::close`].
    pub fn into_inner(self) -> W {
        self.w.w
    }
}

/// `Write`: emits a PDF whose page `i` paints `pages[i].image`, fit-centered on its
/// `width_pt x height_pt` box, and nothing else.
///
/// Placement is "contain": the image is scaled uniformly so it fits entirely inside the box and
/// centred there, never stretched and never cropped, unless the page has `fill` set. ISO
/// 32000-1 section 8.9.5.2 maps image sample row 0 to the TOP of the unit square, so no y-flip
/// belongs here.
///
/// # Errors
/// No pages, any page [`validate_page`] refuses (the message names the page), a page whose
/// scaled size is not representable, and a write error.
pub fn write<W: Write>(out: W, pages: &[Page], lim: &Limits, cancel: &AtomicBool) -> Result<()> {
    if pages.is_empty() {
        return Err(malformed("byblos/pdfbuild: no pages"));
    }
    for (i, p) in pages.iter().enumerate() {
        validate_page(p, lim, cancel)
            .map_err(|e| wrap(e, &format!("byblos/pdfbuild: page {}", i + 1)))?;
    }
    let mut b = Builder::new(out, lim);
    for (i, p) in pages.iter().enumerate() {
        check_cancel(cancel)?;
        b.emit(p)
            .map_err(|e| wrap(e, &format!("byblos/pdfbuild: page {}", i + 1)))?;
    }
    b.close()
}
