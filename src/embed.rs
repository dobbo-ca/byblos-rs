//! Wave 1d: port of byblos `embed.go`: [`embed_png`], [`embed_jpeg`] and [`embed_jp2`] lift an
//! already-encoded PNG, JPEG or JP2 file into an [`EncodedImage`] for `BuildPDF` without
//! re-encoding it, and report the resolution the file declares as img2pdf reads it.
//! [`exif_rotation`] and [`exif_orientation`] read the EXIF (or XMP) orientation of a JPEG or
//! PNG as Pillow does, for `BuildPage::rotate`.
//!
//! The Go helper `walkPNGChunks` of `quantize_indexed.go` is ported here as a private chunk
//! iterator, because `embed.go` is its only caller in this crate.
//!
//! Go's `Limits{}` default and the context-free `EmbedPNG`/`EXIFRotation` variants collapse to
//! one function each taking `&Limits`; none of them can run long (they read one in-memory
//! file's headers), so none takes a cancel flag. Go's `EmbedJPEG` takes no limits and applies
//! no pixel cap; [`embed_jpeg`]'s `limits` only bound the copy of the file and the XMP parse.
//!
//! Differences from Go that no input can show:
//!
//! * Every buffer that scales with the input (the IDAT concatenation, a copy of the file, a
//!   hex-decoded text) is checked against `limits.max_alloc` before it is allocated.
//! * The XML decoder refuses an XMP packet whose working buffers (eight times its size) do not
//!   fit `max_alloc`; such a packet is then "carries an orientation, not well formed", which is
//!   ambiguous. Go would parse it.
//! * A text chunk Go returns as a nil slice and one it returns as an empty slice are the same
//!   here; no result depends on the difference.

use crate::encoded::{ColorSpace, DecodeParms, EncodedImage};
use crate::error::{Error, Result};
use crate::go::bigrat::jp2_per_inch;
use crate::go::fmt::format_g;
use crate::go::xml;
use crate::limits::{Limits, check, mul_sat};
use crate::pdfbuild::{JP2_SIGNATURE, PngPhys, deflate_limits, malformed, wrap};
use pdf::filter::gojpeg;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;

/// Go's `pngSignature`.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// `Resolution`: a raster's pixels per inch along each axis, whole numbers as img2pdf reads
/// them. Zero means the file declares none (`BuildPage::dpi` is then the caller's to choose;
/// img2pdf uses 96). A PNG that declares only an aspect ratio (`pHYs` unit 0) gets img2pdf's
/// 96 dpi default scaled by it, as img2pdf does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Resolution {
    pub x: f64,
    pub y: f64,
}

impl Resolution {
    /// `check`: refuses a resolution with one zero axis, which img2pdf divides by (both zero is
    /// "none declared").
    ///
    /// # Errors
    /// [`Error::Malformed`] for exactly one zero axis.
    pub fn check(&self) -> Result<()> {
        if (self.x == 0.0) != (self.y == 0.0) {
            return Err(malformed(format!(
                "resolution {}x{} has a zero axis",
                format_g(self.x),
                format_g(self.y)
            )));
        }
        Ok(())
    }
}

/// `Orientation`: [`exif_orientation`]'s result. `ambiguous` means the file's orientation
/// metadata cannot be trusted (see [`exif_orientation`]) and `rotate` is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Orientation {
    pub rotate: i32,
    pub ambiguous: bool,
}

// --- byte helpers --------------------------------------------------------------------------

/// `N` bytes at `i`, or `None` past the end.
fn array_at<const N: usize>(b: &[u8], i: usize) -> Option<[u8; N]> {
    let end = i.checked_add(N)?;
    b.get(i..end)?.try_into().ok()
}

/// `binary.ByteOrder.Uint16` at `i`. Every caller has checked the bounds (Go would panic
/// otherwise), so the 0 for an out-of-range read cannot be seen.
fn u16_at(b: &[u8], i: usize, little: bool) -> u16 {
    array_at::<2>(b, i).map_or(0, |a| {
        if little {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    })
}

/// `binary.ByteOrder.Uint32` at `i`; see [`u16_at`].
fn u32_at(b: &[u8], i: usize, little: bool) -> u32 {
    array_at::<4>(b, i).map_or(0, |a| {
        if little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    })
}

/// `binary.ByteOrder.Uint64` at `i`; see [`u16_at`].
fn u64_at(b: &[u8], i: usize, little: bool) -> u64 {
    array_at::<8>(b, i).map_or(0, |a| {
        if little {
            u64::from_le_bytes(a)
        } else {
            u64::from_be_bytes(a)
        }
    })
}

/// `bytes.Index` for a non-empty `needle`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A copy of `src`, refused with [`Error::Alloc`] when it is over `max_alloc` and never
/// allocated infallibly.
fn copy_charged(src: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    let max = limits.alloc_limit();
    if src.len() as u64 > max {
        return Err(Error::Alloc {
            got: src.len() as u64,
            max,
        });
    }
    let mut v = Vec::new();
    v.try_reserve_exact(src.len())?;
    v.extend_from_slice(src);
    Ok(v)
}

use crate::go::crc32::checksum as crc32;

// --- PNG chunks ----------------------------------------------------------------------------

/// `pngChunk`: one chunk of a PNG's framing.
struct PngChunk<'a> {
    typ: [u8; 4],
    data: &'a [u8],
    crc_ok: bool,
}

impl PngChunk<'_> {
    fn typ_str(&self) -> String {
        String::from_utf8_lossy(&self.typ).into_owned()
    }
}

/// The checks `walkPNGChunks` makes before it returns any chunk: a file shorter than the
/// signature, and a chunk that runs past the end of the file. Go collects all chunks first, so
/// a framing error means no chunk is processed at all; this pass keeps that, so [`PngChunks`]
/// can be lazy.
fn check_png_framing(data: &[u8]) -> Result<()> {
    let mut p = data.get(8..).ok_or_else(|| malformed("not a PNG file"))?;
    while p.len() >= 8 {
        let length = u64::from(u32_at(p, 0, false));
        let typ = p.get(4..8).unwrap_or(&[]);
        if length + 12 > p.len() as u64 {
            return Err(malformed(format!(
                "truncated {} chunk",
                String::from_utf8_lossy(typ)
            )));
        }
        // length + 12 <= p.len(), so length fits a usize.
        let n = length as usize;
        let is_iend = typ == b"IEND";
        p = p.get(n + 12..).unwrap_or(&[]);
        if is_iend {
            break;
        }
    }
    Ok(())
}

/// The chunks of a PNG up to and including `IEND`, after [`check_png_framing`] passed.
struct PngChunks<'a> {
    p: &'a [u8],
    done: bool,
}

impl<'a> PngChunks<'a> {
    fn new(data: &'a [u8]) -> Self {
        PngChunks {
            p: data.get(8..).unwrap_or(&[]),
            done: false,
        }
    }
}

impl<'a> Iterator for PngChunks<'a> {
    type Item = PngChunk<'a>;

    fn next(&mut self) -> Option<PngChunk<'a>> {
        if self.done || self.p.len() < 8 {
            return None;
        }
        let n = usize::try_from(u32_at(self.p, 0, false)).ok()?;
        let typ: [u8; 4] = array_at::<4>(self.p, 4)?;
        let end = n.checked_add(8)?;
        let data = self.p.get(8..end)?;
        let crc_ok = crc32(self.p.get(4..end)?) == u32_at(self.p, end, false);
        self.p = self.p.get(end + 4..).unwrap_or(&[]);
        if typ == *b"IEND" {
            self.done = true;
        }
        Some(PngChunk { typ, data, crc_ok })
    }
}

/// `pngInfo`: everything `embed_png` needs out of one walk over a PNG file's chunk framing
/// (ISO/IEC 15948).
#[derive(Default)]
struct PngInfo {
    width: i64,
    height: i64,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    plte: Vec<u8>,
    idat: Vec<u8>,
    has_trns: bool,
    dpi: Resolution,
}

/// `parsePNG`.
fn parse_png(data: &[u8], limits: &Limits) -> Result<PngInfo> {
    if !data.starts_with(&PNG_SIGNATURE) {
        return Err(malformed("not a PNG file"));
    }
    check_png_framing(data)?;
    let mut info = PngInfo::default();
    let mut saw_ihdr = false;
    let mut phys = PngPhys::default();
    let mut saw_idat = false;
    let max = limits.alloc_limit();
    for c in PngChunks::new(data) {
        // Pillow cannot open a file with a bad CRC on a chunk before the first IDAT
        // (TranscodePNG refuses it too); one after is never read.
        if c.typ == *b"IDAT" {
            saw_idat = true;
        } else if !saw_idat && !c.crc_ok {
            return Err(malformed(format!("{} chunk: bad CRC", c.typ_str())));
        }
        match &c.typ {
            b"IHDR" => {
                let Some(
                    &[
                        w0,
                        w1,
                        w2,
                        w3,
                        h0,
                        h1,
                        h2,
                        h3,
                        depth,
                        ctype,
                        _,
                        _,
                        interlace,
                    ],
                ) = c.data.first_chunk::<13>().filter(|_| c.data.len() == 13)
                else {
                    return Err(malformed(format!(
                        "IHDR is {} bytes; want 13",
                        c.data.len()
                    )));
                };
                info.width = i64::from(u32::from_be_bytes([w0, w1, w2, w3]));
                info.height = i64::from(u32::from_be_bytes([h0, h1, h2, h3]));
                info.bit_depth = depth;
                info.color_type = ctype;
                info.interlace = interlace;
                saw_ihdr = true;
            }
            b"PLTE" => info.plte = copy_charged(c.data, limits)?,
            b"IDAT" => {
                let total = (info.idat.len() as u64).saturating_add(c.data.len() as u64);
                if total > max {
                    return Err(Error::Alloc { got: total, max });
                }
                info.idat.try_reserve(c.data.len())?;
                info.idat.extend_from_slice(c.data);
            }
            b"tRNS" => info.has_trns = true,
            b"pHYs" => phys.add(c.data)?,
            _ => {}
        }
    }
    if !saw_ihdr {
        return Err(malformed("no IHDR chunk"));
    }
    let (x, y) = phys.resolution();
    info.dpi = Resolution { x, y };
    if info.width <= 0 || info.height <= 0 {
        return Err(malformed(format!(
            "IHDR declares {}x{}; dimensions must be positive",
            info.width, info.height
        )));
    }
    if info.idat.is_empty() {
        return Err(malformed("no IDAT chunk"));
    }
    Ok(info)
}

/// `EmbedPNG` and `EmbedPNGLimits`: lifts a PNG file's pixel data into a ready
/// [`EncodedImage`] under `/FlateDecode`, plus the resolution its `pHYs` chunk declares (zero
/// if the file declares none), rounded per axis as img2pdf does: pixels per metre (unit 1),
/// else an aspect ratio (unit 0) scaling img2pdf's 96 dpi default. A resolution with one zero
/// axis is refused, as img2pdf divides by it.
///
/// Refused, because none of these can be carried by this route: colour types 4 and 6 (alpha
/// needs an `/SMask` split), a `tRNS` chunk on any other colour type, an interlace method
/// other than 0 (Adam7 rows are not PDF scanlines) and 16-bit depth. See
/// `pdfbuild::transcode_png` for those.
///
/// An IHDR over `limits.max_encoded_image_pixels` is refused as `limit encoded-image-pixels`
/// from the header, before the image is built.
///
/// # Errors
/// [`Error::Malformed`] for every refusal above and for a file that is not a PNG,
/// [`Error::Limit`], [`Error::Alloc`] and [`Error::OutOfMemory`].
pub fn embed_png(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    embed_png_inner(data, limits).map_err(|e| wrap(e, "byblos: EmbedPNG"))
}

fn embed_png_inner(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    let info = parse_png(data, limits)?;
    if info.bit_depth == 16 {
        return Err(malformed("16-bit-per-component PNG is not supported"));
    }
    if info.interlace != 0 {
        return Err(malformed("interlaced PNG is not supported"));
    }
    if info.has_trns {
        return Err(malformed(format!(
            "colour type {} has a tRNS chunk, which needs an /SMask split this function does not do",
            info.color_type
        )));
    }
    // byb-37h: refuse an oversized IHDR before it becomes an EncodedImage that a caller other
    // than BuildPDF might use directly. mul_sat because width and height are each a raw uint32
    // out of IHDR, and their product can overflow before a comparison would catch it.
    check(
        "encoded-image-pixels",
        mul_sat(info.width, info.height),
        limits.encoded_image_pixels(),
    )?;

    let depth_ok = matches!(info.bit_depth, 1 | 2 | 4 | 8);
    let (cs, colors) = match info.color_type {
        0 => {
            if !depth_ok {
                return Err(malformed(format!(
                    "colour type 0 does not allow bit depth {}",
                    info.bit_depth
                )));
            }
            (
                ColorSpace {
                    name: "DeviceGray".to_string(),
                    ..ColorSpace::default()
                },
                1,
            )
        }
        2 => {
            if info.bit_depth != 8 {
                return Err(malformed(format!(
                    "colour type 2 does not allow bit depth {}",
                    info.bit_depth
                )));
            }
            (
                ColorSpace {
                    name: "DeviceRGB".to_string(),
                    ..ColorSpace::default()
                },
                3,
            )
        }
        3 => {
            if !depth_ok {
                return Err(malformed(format!(
                    "colour type 3 does not allow bit depth {}",
                    info.bit_depth
                )));
            }
            if info.plte.is_empty() || !info.plte.len().is_multiple_of(3) {
                return Err(malformed("indexed PNG has no usable PLTE chunk"));
            }
            (
                ColorSpace {
                    name: "Indexed".to_string(),
                    base: "DeviceRGB".to_string(),
                    hi_val: (info.plte.len() / 3) as i64 - 1,
                    lookup: info.plte,
                },
                1,
            )
        }
        4 | 6 => {
            return Err(malformed(format!(
                "colour type {} carries alpha, which needs an /SMask split this function does not do",
                info.color_type
            )));
        }
        other => return Err(malformed(format!("unsupported PNG colour type {other}"))),
    };

    info.dpi.check()?;
    let depth = i64::from(info.bit_depth);
    Ok((
        EncodedImage {
            width: info.width,
            height: info.height,
            bpc: depth,
            color_space: cs,
            filter: "FlateDecode".to_string(),
            decode_parms: Some(DecodeParms {
                predictor: 15,
                colors,
                bits_per_component: depth,
                columns: info.width,
            }),
            data: info.idat,
            ..EncodedImage::default()
        },
        info.dpi,
    ))
}

// --- JPEG ----------------------------------------------------------------------------------

/// `EmbedJPEG`: lifts a JPEG file's compressed data into a ready [`EncodedImage`] under
/// `/DCTDecode`, carried verbatim and not re-encoded, plus its resolution as img2pdf (through
/// Pillow) reads it, per axis and rounded: the density of the last JFIF segment whose units are
/// inches or centimetres, else the Exif `XResolution` and `ResolutionUnit` (72 if the file has
/// an Exif block that does not give them), else zero. A resolution with one zero axis is
/// refused, as img2pdf divides by it.
///
/// Dimensions and colour components come from Go's `jpeg.DecodeConfig` (pdf-rs `gojpeg`),
/// which accepts only a single-scan baseline, extended-sequential or progressive frame.
///
/// A 4-component file (CMYK, or Adobe YCCK) is `DeviceCMYK`. Adobe writes such samples
/// inverted and marks the file with an APP14 `Adobe` segment, so with that segment present the
/// image carries `/Decode [1 0 1 0 1 0 1 0]`, as img2pdf writes it; without it the samples are
/// taken as plain CMYK.
///
/// # Errors
/// [`Error::Pdf`] (`jpeg/format`, `jpeg/unsupported`) for a file `jpeg.DecodeConfig` refuses,
/// [`Error::Malformed`] for a short JFIF segment or a resolution with one zero axis,
/// [`Error::Alloc`] and [`Error::OutOfMemory`] for the copy of the file.
pub fn embed_jpeg(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    embed_jpeg_inner(data, limits).map_err(|e| wrap(e, "byblos: EmbedJPEG"))
}

fn embed_jpeg_inner(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    let cfg = gojpeg::decode_config(data)?;
    let mut decode: Vec<f64> = Vec::new();
    let name = match cfg.model {
        gojpeg::ColorModel::Gray => "DeviceGray",
        gojpeg::ColorModel::YCbCr | gojpeg::ColorModel::Rgba => "DeviceRGB",
        gojpeg::ColorModel::Cmyk => {
            if has_adobe_segment(data) {
                decode = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
            }
            "DeviceCMYK"
        }
    };
    // Pillow reads the version out of a JFIF segment and cannot open the file if it is short.
    if jpeg_segment(data, |m, p| {
        m == 0xE0 && p.starts_with(b"JFIF") && p.len() < 7
    })
    .is_some()
    {
        return Err(malformed("JFIF segment is too short"));
    }
    let dpi = jpeg_dpi(data, limits)?;
    dpi.check()?;
    Ok((
        EncodedImage {
            width: i64::try_from(cfg.width).unwrap_or(i64::MAX),
            height: i64::try_from(cfg.height).unwrap_or(i64::MAX),
            bpc: 8,
            color_space: ColorSpace {
                name: name.to_string(),
                ..ColorSpace::default()
            },
            filter: "DCTDecode".to_string(),
            decode,
            data: copy_charged(data, limits)?,
            ..EncodedImage::default()
        },
        dpi,
    ))
}

/// `jpegSegment`: the payload (after the two length bytes) of the first marker segment before
/// the scan for which `pick` returns true, or `None`. It walks only the headers, so it is
/// bounded by the file and allocates nothing.
fn jpeg_segment<'a>(
    data: &'a [u8],
    mut pick: impl FnMut(u8, &'a [u8]) -> bool,
) -> Option<&'a [u8]> {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if *data.get(i)? != 0xFF {
            return None;
        }
        let m = *data.get(i + 1)?;
        if m == 0xFF {
            // fill byte
            i += 1;
            continue;
        }
        if m == 0x01 || (0xD0..=0xD8).contains(&m) {
            // no length
            i += 2;
            continue;
        }
        if m == 0xD9 || m == 0xDA {
            // end of image, start of scan
            return None;
        }
        let n = usize::from(u16_at(data, i + 2, false));
        if n < 2 || i + 2 + n > data.len() {
            return None;
        }
        let p = data.get(i + 4..i + 2 + n)?;
        if pick(m, p) {
            return Some(p);
        }
        i += 2 + n;
    }
    None
}

/// `hasAdobeSegment`.
fn has_adobe_segment(data: &[u8]) -> bool {
    jpeg_segment(data, |m, p| m == 0xEE && p.starts_with(b"Adobe")).is_some()
}

/// `jfifDensity`: the last JFIF segment whose units are inches or centimetres, the one Pillow
/// takes a density from.
fn jfif_density(data: &[u8]) -> Option<&[u8]> {
    let mut jfif = None;
    jpeg_segment(data, |m, p| {
        if m == 0xE0 && p.starts_with(b"JFIF") && p.len() >= 12 && matches!(p.get(7), Some(1 | 2)) {
            jfif = Some(p);
        }
        false
    });
    jfif
}

/// `jpegDPI`: the resolution Pillow reads from a JPEG, rounded as img2pdf rounds it: the
/// density of the last JFIF segment in inches or centimetres, else (with an Exif block
/// present) the Exif `XResolution` in `ResolutionUnit`, 72 when the block lacks them or they do
/// not parse, else zero. Pillow takes the vertical resolution from `XResolution` too.
fn jpeg_dpi(data: &[u8], limits: &Limits) -> Result<Resolution> {
    if let Some(jfif) = jfif_density(data) {
        let (mut x, mut y) = (
            f64::from(u16_at(jfif, 8, false)),
            f64::from(u16_at(jfif, 10, false)),
        );
        if jfif.get(7) == Some(&2) {
            x = (x * 2.54).round_ties_even();
            y = (y * 2.54).round_ties_even();
        }
        return Ok(Resolution { x, y });
    }
    let d = exif_block(data, limits).unwrap_or_default();
    if !d.exif {
        return Ok(Resolution::default());
    }
    let res = tiff_field(&d.tiff, 0x011A, false).fields.pop();
    let unit = tiff_field(&d.tiff, 0x0128, false).fields.pop();
    let (Some(res), Some(unit)) = (res, unit) else {
        return Ok(Resolution { x: 72.0, y: 72.0 });
    };
    let mut dpi = res.num();
    if res.typ == 1 || res.typ == 7 {
        // Pillow hands over bytes, and divides the first by the second.
        dpi = match (res.val.first(), res.val.get(1)) {
            (Some(&a), Some(&b)) if b != 0 => Some(f64::from(a) / f64::from(b)),
            _ => None,
        };
    }
    let Some(mut dpi) = dpi.filter(|v| !v.is_nan()) else {
        return Ok(Resolution { x: 72.0, y: 72.0 });
    };
    if unit.num().unwrap_or(0.0) == 3.0 {
        dpi *= 2.54;
    }
    let dpi = dpi.round_ties_even();
    Ok(Resolution { x: dpi, y: dpi })
}

// --- EXIF ----------------------------------------------------------------------------------

/// `exifField`: one IFD0 entry of an Exif block: `typ` is its TIFF type and `val` the bytes of
/// its values.
struct ExifField<'a> {
    typ: u16,
    val: &'a [u8],
    little: bool,
}

impl ExifField<'_> {
    /// `num`: the first value as the number Pillow hands img2pdf, which compares it with the
    /// orientations (a rational with a zero denominator is NaN); `None` for a type that is not
    /// a number (BYTE, ASCII, UNDEFINED).
    fn num(&self) -> Option<f64> {
        let (v, l) = (self.val, self.little);
        match self.typ {
            3 => Some(f64::from(u16_at(v, 0, l))),
            4 | 13 => Some(f64::from(u32_at(v, 0, l))),
            6 => Some(f64::from(v.first().map_or(0, |&b| b as i8))),
            8 => Some(f64::from(u16_at(v, 0, l) as i16)),
            9 => Some(f64::from(u32_at(v, 0, l) as i32)),
            16 => Some(u64_at(v, 0, l) as f64),
            11 => Some(f64::from(f32::from_bits(u32_at(v, 0, l)))),
            12 => Some(f64::from_bits(u64_at(v, 0, l))),
            5 | 10 => {
                let (n, d) = if self.typ == 10 {
                    (
                        f64::from(u32_at(v, 0, l) as i32),
                        f64::from(u32_at(v, 4, l) as i32),
                    )
                } else {
                    (f64::from(u32_at(v, 0, l)), f64::from(u32_at(v, 4, l)))
                };
                if d == 0.0 {
                    return Some(f64::NAN);
                }
                Some(n / d)
            }
            _ => None,
        }
    }
}

/// `exifSize`: the size of one value of each TIFF type Pillow reads; the other types are
/// skipped.
const EXIF_SIZE: [u8; 17] = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8, 4, 0, 0, 8];

/// `exifData`: what Pillow's `getexif` works from: the TIFF data of the Exif block (minus any
/// `Exif\0\0` it starts with; `exif` reports whether there is a block at all) and the XMP
/// bookkeeping.
#[derive(Default)]
struct ExifData<'a> {
    tiff: Cow<'a, [u8]>,
    exif: bool,
    /// See [`exif_orientation`].
    ambiguous: bool,
    /// Normal XMP packets that carry an orientation.
    xmp_n: i64,
    /// A packet that carries an orientation is not normal.
    xmp_bad: bool,
    /// The orientation of the last normal one.
    xmp_v: i64,
}

impl ExifData<'_> {
    /// `addXMP`: records one XMP packet; a packet without `Orientation` is ignored (see
    /// [`xmp_normal`]).
    fn add_xmp(&mut self, v: &[u8], limits: &Limits) {
        let (x, carries, normal) = xmp_normal(v, limits);
        if normal {
            self.xmp_n += 1;
            self.xmp_v = x;
        } else if carries {
            self.xmp_bad = true;
        }
    }

    /// `xmpAmbiguous`: a packet that carries is not normal, or two are.
    fn xmp_ambiguous(&self) -> bool {
        self.xmp_bad || self.xmp_n > 1
    }
}

const EXIF_PREFIX: &[u8] = b"Exif\x00\x00";
const XAP_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\x00";

/// `exifBlock`: finds `data`'s Exif block as Pillow takes it. A JPEG's is the first Exif APP1
/// segment, its XMP the last xap one. A PNG's is the last of its `eXIf` chunks and `tEXt`
/// chunks named exif, else its "Raw profile type exif" text (ImageMagick's), a hex dump, which
/// is an error if it does not parse.
fn exif_block<'a>(data: &'a [u8], lim: &Limits) -> Result<ExifData<'a>> {
    let mut d = ExifData::default();
    if data.starts_with(&PNG_SIGNATURE) {
        // `chunks, _ := walkPNGChunks(data)`: a framing error leaves no chunk at all.
        let framed = check_png_framing(data).is_ok();
        let mut raw: Option<Cow<'a, [u8]>> = None;
        let mut text = TextBudget {
            used: 0,
            max: lim.png_text_bytes(),
        };
        let (mut saw_idat, mut n_exif) = (false, 0usize);
        if framed {
            for c in PngChunks::new(data) {
                // Pillow cannot open a file with a bad CRC before the first IDAT.
                if c.typ == *b"IDAT" {
                    saw_idat = true;
                } else if !saw_idat && !c.crc_ok {
                    return Err(malformed(format!("{} chunk: bad CRC", c.typ_str())));
                }
                if c.typ == *b"eXIf" {
                    d.tiff = Cow::Borrowed(c.data);
                    d.exif = true;
                    n_exif += 1;
                    d.ambiguous = d.ambiguous || saw_idat;
                    continue;
                }
                let t = match png_text(&c, &mut text, lim) {
                    Ok(t) => t,
                    Err(TextErr::BadUtf8) => {
                        d.ambiguous = true;
                        continue;
                    }
                    Err(TextErr::Err(e)) => return Err(e),
                };
                if !t.ok {
                    continue;
                }
                if t.key == b"exif" && c.typ == *b"tEXt" {
                    d.tiff = t.v;
                    d.exif = true;
                    n_exif += 1;
                } else if t.key == b"exif" {
                    // Pillow hands img2pdf a str where it wants bytes.
                    return Err(malformed(format!("{} chunk named exif", c.typ_str())));
                } else if t.key == b"Raw profile type exif" {
                    raw = Some(t.v);
                    n_exif += 1;
                } else if t.key == b"XML:com.adobe.xmp" {
                    d.add_xmp(&t.v, lim);
                } else {
                    continue;
                }
                d.ambiguous = d.ambiguous || saw_idat;
            }
        }
        d.ambiguous = d.ambiguous || n_exif > 1 || d.xmp_ambiguous();
        if !d.exif
            && let Some(raw) = raw
        {
            d.exif = true;
            let mut lines = raw.split(|&b| b == b'\n');
            if lines.nth(3).is_some() {
                // The text after the first three lines, joined without the newlines.
                let rest = raw
                    .split(|&b| b == b'\n')
                    .skip(3)
                    .flat_map(|l| l.iter().copied());
                d.tiff = Cow::Owned(from_hex(rest, lim)?);
            }
        }
    } else {
        // Pillow joins every Exif segment into one block, a later one after the first's end.
        // ponytail: capped at 1 MiB so a file of nothing but APP1 segments cannot make a copy
        // of itself; a real block is under 64 KiB.
        let mut n_exif = 0usize;
        let mut oom = false;
        jpeg_segment(data, |m, p| {
            if m == 0xE1 && p.starts_with(EXIF_PREFIX) {
                n_exif += 1;
                let body = p.get(6..).unwrap_or(&[]);
                if !d.exif {
                    d.tiff = Cow::Borrowed(body);
                    d.exif = true;
                } else if d.tiff.len() < 1 << 20 {
                    let t = d.tiff.to_mut();
                    if t.try_reserve(body.len()).is_err() {
                        oom = true;
                    } else {
                        t.extend_from_slice(body);
                    }
                }
            } else if m == 0xE1 && p.starts_with(XAP_PREFIX) {
                d.add_xmp(p.get(XAP_PREFIX.len()..).unwrap_or(&[]), lim);
            }
            false
        });
        if oom {
            return Err(Error::OutOfMemory);
        }
        d.ambiguous = n_exif > 1 || d.xmp_ambiguous();
    }
    let mut off = 0usize;
    while d
        .tiff
        .get(off..)
        .is_some_and(|t| t.starts_with(EXIF_PREFIX))
    {
        off += 6;
    }
    if off > 0 {
        d.tiff = match d.tiff {
            Cow::Borrowed(b) => Cow::Borrowed(b.get(off..).unwrap_or(&[])),
            Cow::Owned(mut v) => {
                v.drain(..off.min(v.len()));
                Cow::Owned(v)
            }
        };
    }
    Ok(d)
}

/// `textBudget`: Pillow's `text_memory`: what a file's text chunks may hold.
struct TextBudget {
    used: i64,
    max: i64,
}

impl TextBudget {
    fn add(&mut self, n: usize) -> Result<()> {
        let total = self
            .used
            .saturating_add(i64::try_from(n).unwrap_or(i64::MAX));
        check("png-text-bytes", total, self.max)?;
        self.used = total;
        Ok(())
    }
}

/// `pngText`'s error: `errBadUTF8`, the note that an iTXt it would read is not UTF-8, or a
/// real error.
enum TextErr {
    BadUtf8,
    Err(Error),
}

impl From<Error> for TextErr {
    fn from(e: Error) -> Self {
        TextErr::Err(e)
    }
}

/// The key and value Pillow's `PngStream` gives a tEXt, zTXt or iTXt chunk (`ok` is false for
/// a chunk it ignores).
struct PngText<'a> {
    key: &'a [u8],
    v: Cow<'a, [u8]>,
    ok: bool,
}

/// `pngText`.
fn png_text<'a>(
    c: &PngChunk<'a>,
    text: &mut TextBudget,
    lim: &Limits,
) -> std::result::Result<PngText<'a>, TextErr> {
    let ignored = || PngText {
        key: &[],
        v: Cow::Borrowed(&[]),
        ok: false,
    };
    // bytes.Cut(c.data, {0})
    let (key, v, found) = match c.data.iter().position(|&b| b == 0) {
        Some(i) => {
            let (k, rest) = c.data.split_at(i);
            (k, rest.get(1..).unwrap_or(&[]), true)
        }
        None => (c.data, &[][..], false),
    };
    // Only these texts are read (exif_block); the others are inflated to be counted against
    // the budget, and dropped.
    let keep = key == b"Raw profile type exif" || key == b"XML:com.adobe.xmp" || key == b"exif";
    let out: Cow<'a, [u8]> = match &c.typ {
        b"zTXt" => {
            let mut v = v;
            if let Some(&m) = v.first() {
                if m != 0 {
                    return Err(
                        malformed(format!("unknown compression method {m} in zTXt chunk")).into(),
                    );
                }
                v = v.get(1..).unwrap_or(&[]);
            }
            Cow::Owned(inflate_text(v, text, lim)?)
        }
        b"iTXt" => {
            // compression flag and method, language, translated key, text
            let (Some(&flag), Some(&method)) = (v.first(), v.get(1)) else {
                return Ok(ignored());
            };
            let mut parts = v.get(2..).unwrap_or(&[]).splitn(3, |&b| b == 0);
            let (Some(p0), Some(p1), Some(p2)) = (parts.next(), parts.next(), parts.next()) else {
                return Ok(ignored());
            };
            if !found {
                return Ok(ignored());
            }
            let out: Cow<'a, [u8]> = if flag != 0 {
                if method != 0 {
                    return Ok(ignored());
                }
                Cow::Owned(inflate_text(p2, text, lim)?)
            } else {
                text.add(p2.len())?;
                Cow::Borrowed(p2)
            };
            // Pillow ignores an iTXt that is not UTF-8.
            if keep
                && !(std::str::from_utf8(p0).is_ok()
                    && std::str::from_utf8(p1).is_ok()
                    && std::str::from_utf8(&out).is_ok())
            {
                return Err(TextErr::BadUtf8);
            }
            out
        }
        b"tEXt" => {
            text.add(v.len())?;
            Cow::Borrowed(v)
        }
        _ => return Ok(ignored()),
    };
    Ok(PngText {
        key,
        v: out,
        ok: !key.is_empty() && keep,
    })
}

/// `inflateText`: Pillow's `_safe_zlib_decompress` for text: nothing if the data is corrupt,
/// what there is if it is cut short, an error past a MiB or past what is left of the file's
/// text budget. It reads no more than that, so a bomb is never inflated.
fn inflate_text(b: &[u8], text: &mut TextBudget, lim: &Limits) -> Result<Vec<u8>> {
    // io.LimitReader(zr, min(1<<20, max-used)+1)
    let room = (1i64 << 20).min(text.max.saturating_sub(text.used)).max(0);
    let n = usize::try_from(room + 1).unwrap_or(1);
    let cancel = AtomicBool::new(false);
    let dl = deflate_limits(&lim.mem());
    let mut out = Vec::new();
    let res = deflate::zlib::decode_limit_into(b, n, &mut out, &dl, &cancel);
    if let Err(
        e @ (deflate::Error::OutputLimit
        | deflate::Error::AllocLimit
        | deflate::Error::OutOfMemory
        | deflate::Error::Cancelled),
    ) = &res
    {
        return Err(Error::from(pdf::Error::Flate(e.clone())));
    }
    if out.len() > 1 << 20 {
        return Err(malformed("decompressed text is too large"));
    }
    text.add(out.len())?;
    match res {
        Ok(()) | Err(deflate::Error::UnexpectedEof) => Ok(out),
        // Corrupt data, a bad header or checksum: nothing.
        Err(_) => Ok(Vec::new()),
    }
}

/// `fromHex`: Python's `bytes.fromhex` over `it`: pairs of hex digits, ASCII white space
/// allowed between pairs.
fn from_hex(mut it: impl Iterator<Item = u8>, lim: &Limits) -> Result<Vec<u8>> {
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let not_hex = || malformed("Raw profile type exif is not hex");
    let max = lim.alloc_limit();
    let mut out: Vec<u8> = Vec::new();
    while let Some(a) = it.next() {
        if matches!(a, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C) {
            continue;
        }
        let b = it.next().ok_or_else(not_hex)?;
        let (Some(hi), Some(lo)) = (digit(a), digit(b)) else {
            return Err(not_hex());
        };
        if out.len() as u64 >= max {
            return Err(Error::Alloc {
                got: out.len() as u64 + 1,
                max,
            });
        }
        out.try_reserve(1)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

const XMP_TIFF_NS: &str = "http://ns.adobe.com/tiff/1.0/";
/// The packet is already capped; this bounds the work per packet.
const XMP_MAX_TOKENS: usize = 1 << 17;
const XMP_MAX_NESTING: usize = 1024;

/// `xmlSpace`: XML whitespace; `strings.TrimSpace` also strips NBSP and the like.
fn trim_xml_space(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

/// `xmpNormal`: applies the closed XMP rule to one packet. A packet whose bytes do not contain
/// `Orientation` is ignored (`carries` false). Otherwise it is normal only if it parses
/// completely as XML within the bounds, holds exactly one distinct trimmed `tiff:Orientation`
/// value (any prefix, attribute or element, references decoded) that is 1..8, and Pillow's
/// regex digit is absent or equal to it; `v` is that value. Any other packet that carries is
/// ambiguous. Returns `(v, carries, normal)`.
fn xmp_normal(x: &[u8], lim: &Limits) -> (i64, bool, bool) {
    if find(x, b"Orientation").is_none() {
        return (0, false, false);
    }
    let mut vals: HashSet<String> = HashSet::new();
    let Ok(mut d) = xml::Decoder::new(x, lim) else {
        return (0, true, false);
    };
    let mut buf: Vec<u8> = Vec::new();
    // `in`: depth of the open Orientation element.
    let (mut depth, mut inside): (usize, Option<usize>) = (0, None);
    let mut err: Option<xml::Error> = None;
    let mut n = 0usize;
    while err.is_none() && n < XMP_MAX_TOKENS && depth < XMP_MAX_NESTING {
        n += 1;
        let t = match d.token() {
            Ok(t) => t,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        match t {
            xml::Token::StartElement(se) => {
                depth += 1;
                for a in &se.attr {
                    if a.name.space == XMP_TIFF_NS && a.name.local == "Orientation" {
                        vals.insert(trim_xml_space(&a.value).to_string());
                    }
                }
                if inside.is_none()
                    && se.name.space == XMP_TIFF_NS
                    && se.name.local == "Orientation"
                {
                    inside = Some(depth);
                    buf.clear();
                }
            }
            xml::Token::CharData(cd) => {
                if inside.is_some() {
                    buf.extend_from_slice(&cd);
                }
            }
            xml::Token::EndElement(_) => {
                if Some(depth) == inside {
                    vals.insert(trim_xml_space(&String::from_utf8_lossy(&buf)).to_string());
                    inside = None;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    if !matches!(err, Some(xml::Error::Eof)) || vals.len() != 1 {
        return (0, true, false);
    }
    let mut v = 0i64;
    for s in &vals {
        if let [b] = s.as_bytes() {
            v = i64::from(*b) - i64::from(b'0');
        }
    }
    if !(1..=8).contains(&v) || xmp_orientation(x).is_some_and(|p| p != v) {
        return (0, true, false);
    }
    (v, true, true)
}

/// `xmpOrientation`: the digit Pillow takes from an XMP packet: the first
/// `tiff:Orientation="` or `tiff:Orientation>` that a digit follows.
fn xmp_orientation(mut x: &[u8]) -> Option<i64> {
    const P: &[u8] = b"tiff:Orientation";
    loop {
        let i = find(x, P)?;
        x = x.get(i + P.len()..)?;
        if let Some(r) = x.strip_prefix(b"=\"").or_else(|| x.strip_prefix(b">"))
            && let Some(&c) = r.first()
            && c.is_ascii_digit()
        {
            return Some(i64::from(c - b'0'));
        }
    }
}

/// `loadFails`: whether Pillow raises when it loads `t` as an Exif block: it is not empty and
/// has no TIFF header (or BigTIFF's) with room for its offset.
fn load_fails(t: &[u8]) -> bool {
    if t.is_empty() {
        return false;
    }
    let (Some(&t0), Some(&t1), Some(&t2), Some(&t3)) = (t.first(), t.get(1), t.get(2), t.get(3))
    else {
        return true;
    };
    if t.len() < 8
        || t0 != t1
        || (t0 != b'I' && t0 != b'M')
        || t2 & t3 != 0
        || ((t2 | t3) != 0x2a && (t2 | t3) != 0x2b)
    {
        return true;
    }
    (t2 | t3) == 0x2b && t.len() < 16
}

/// What [`tiff_field`] found.
struct TiffFields<'a> {
    /// Every entry of the tag that was read, in file order; the last is the one that wins.
    /// There are at most 65 535 of them, so this is bounded by the format.
    fields: Vec<ExifField<'a>>,
    dup: bool,
    bad: bool,
}

/// `tiffField`: finds `tag` in IFD0 of the Exif block `t`, loaded as Pillow loads it (img2pdf
/// and the JPEG resolution both go through it): a block without a TIFF header or with IFD0 out
/// of range has no tags; entries are read in order and the last of a repeated tag wins (`dup`
/// reports any repeated entry of `tag`, skipped or not); an entry of a type Pillow does not
/// know or with no values is skipped; an entry whose values lie outside the block ends the
/// reading of values, keeping the tags before it, but not the scan for repeats of `tag`, which
/// runs over every entry the block holds (an entry cut short by the block end that starts with
/// `tag` counts as a repeat). `bad` reports an entry of `tag` whose values lie outside the
/// block or that follows such an entry, so Pillow never sees it. With `lost`, an entry after
/// such an entry is still read if its own values lie in range (for a reader of the whole file,
/// which Pillow is not).
fn tiff_field(t: &[u8], tag: u16, lost: bool) -> TiffFields<'_> {
    let mut out = TiffFields {
        fields: Vec::new(),
        dup: false,
        bad: false,
    };
    let (Some(&t0), Some(&t1), Some(&t2), Some(&t3)) = (t.first(), t.get(1), t.get(2), t.get(3))
    else {
        return out;
    };
    // The second pair is 0x002a or 0x2a00 (Pillow accepts either order); BigTIFF is not read.
    if t.len() < 8 || t0 != t1 || (t0 != b'I' && t0 != b'M') || (t2 | t3) != 0x2a || t2 & t3 != 0 {
        return out;
    }
    let little = t0 == b'I';
    let len = t.len() as u64;
    let ifd = u64::from(u32_at(t, 4, little));
    if ifd + 2 > len {
        return out;
    }
    let mut n = u16_at(t, ifd as usize, little);
    let (mut seen, mut stop) = (false, false);
    let mut e = ifd + 2;
    while n > 0 && e + 2 <= len {
        let cur = e as usize;
        e += 12;
        n -= 1;
        let is_tag = u16_at(t, cur, little) == tag;
        if is_tag {
            // A repeat counts even if Pillow would skip the entry.
            out.dup = out.dup || seen;
            seen = true;
        }
        if cur as u64 + 12 > len {
            // cut short by the block end
            out.dup = out.dup || is_tag;
            break;
        }
        if stop {
            out.bad = out.bad || is_tag;
            if !lost {
                continue;
            }
        }
        let typ = u16_at(t, cur + 2, little);
        let count = u32_at(t, cur + 4, little);
        let size_of_type = EXIF_SIZE.get(usize::from(typ)).copied().unwrap_or(0);
        if size_of_type == 0 || count == 0 {
            continue;
        }
        // Inline if every value fits, else an offset to them.
        let mut val = t.get(cur + 8..cur + 12).unwrap_or(&[]);
        let size = u64::from(size_of_type) * u64::from(count);
        if size > 4 {
            let off = u64::from(u32_at(val, 0, little));
            if off + size > len {
                stop = true;
                out.bad = out.bad || is_tag;
                continue;
            }
            val = t.get(off as usize..).unwrap_or(&[]);
        }
        if is_tag {
            out.fields.push(ExifField {
                typ,
                val: val.get(..size as usize).unwrap_or(&[]),
                little,
            });
        }
    }
    out
}

/// `EXIFRotation` and `EXIFRotationLimits`: the `/Rotate` (0, 90, 180 or 270) that a JPEG's or
/// PNG's EXIF orientation asks for, for `BuildPage::rotate`: orientation 1 or no Exif at all is
/// 0, 3 is 180, 6 is 90, 8 is 270. See [`exif_orientation`] for the rules.
///
/// # Errors
/// [`Error::MirroredOrientation`] for 2, 4, 5 and 7, which flip the image and which `/Rotate`
/// cannot express (img2pdf refuses them too); [`Error::Malformed`] for any other value, or one
/// that is not a number, and for the defects img2pdf refuses; [`Error::Limit`]
/// (`png-text-bytes`) for a PNG whose Exif and XMP text chunks inflate to more than
/// `limits.max_png_text_bytes` together (with its tEXt chunks), as img2pdf refuses it ("Too
/// much memory used in text chunks"). Other text chunks are not inflated.
pub fn exif_rotation(data: &[u8], limits: &Limits) -> Result<i32> {
    Ok(exif_orientation(data, limits)?.rotate)
}

/// `EXIFOrientation`: [`exif_rotation`] with the ambiguity reported. This is img2pdf's
/// behaviour (checked on `testdata/img2pdf`), reading the Exif block as Pillow does: the Exif
/// APP1 segments of a JPEG, joined, or a PNG's last `eXIf` chunk or `tEXt` chunk named exif,
/// else its "Raw profile type exif" text (found anywhere in `data`: a caller streaming a PNG
/// may pass the file up to its first IDAT); with no orientation in it, the `tiff:Orientation`
/// of the XMP packet (a JPEG's last xap APP1 segment, a PNG's `XML:com.adobe.xmp` text); a
/// block that is not a TIFF is an error where Pillow raises on it (always for a PNG, for a JPEG
/// only with a JFIF density) and has no tags where it does not; a block that is malformed, or
/// whose IFD0 is cut short, is read as far as it goes, and a tag Pillow skips (a type it does
/// not know, an empty value) is absent; the last of a repeated tag wins; the value may be any
/// numeric type, its first element if there are several. It reads the headers only.
///
/// A well-formed file gets img2pdf's rotation. Where its orientation metadata is duplicated
/// (several `eXIf` chunks, Exif texts or XMP packets in a PNG), conflicting (the Exif and the
/// XMP orientation differ), an XMP packet naming `Orientation` that is not well-formed (see
/// [`xmp_normal`]), placed after a PNG's IDAT, or a PNG iTXt that is not valid UTF-8, the
/// rotation is 0 and `ambiguous` is set, whatever img2pdf makes of it. A defect img2pdf
/// refuses stays an error (a bad CRC before the IDAT, a zTXt or iTXt named exif, text past the
/// budget).
///
/// # Errors
/// As [`exif_rotation`].
pub fn exif_orientation(data: &[u8], limits: &Limits) -> Result<Orientation> {
    exif_orientation_inner(data, limits).map_err(|e| wrap(e, "byblos: EXIFRotation"))
}

fn exif_orientation_inner(data: &[u8], limits: &Limits) -> Result<Orientation> {
    let ambiguous = Orientation {
        rotate: 0,
        ambiguous: true,
    };
    let b = exif_block(data, limits)?;
    if b.ambiguous {
        return Ok(ambiguous);
    }
    if load_fails(&b.tiff) {
        // Pillow raises when it loads the block, and a JPEG's read at open is the one that is
        // swallowed (and cached, XMP and all) unless the JFIF density spared it.
        if data.starts_with(&PNG_SIGNATURE) || jfif_density(data).is_some() {
            return Err(malformed("Exif block is not a TIFF file"));
        }
        return Ok(Orientation::default());
    }
    let orient = tiff_field(&b.tiff, 0x0112, false);
    let x7 = tiff_field(&b.tiff, 700, true);
    // The packets of the file and of IFD0 tag 700 (which Pillow never reads, but a reader of
    // the whole file does) follow one rule.
    let (mut n, mut v_xmp, mut bad_x) = (b.xmp_n, b.xmp_v, b.xmp_bad);
    for p in x7.fields.iter().rev() {
        let (v, carries, ok) = xmp_normal(p.val, limits);
        if carries && ok {
            n += 1;
            v_xmp = v;
        } else if carries {
            bad_x = true;
        }
    }
    if orient.dup || orient.bad || n > 1 || bad_x {
        return Ok(ambiguous);
    }
    let o = if let Some(f) = orient.fields.last() {
        let Some(o) = f.num() else {
            return Err(malformed("orientation entry is not a number"));
        };
        if n == 1 && v_xmp as f64 != o {
            return Ok(ambiguous);
        }
        o
    } else if n == 1 {
        v_xmp as f64
    } else {
        return Ok(Orientation::default());
    };
    let rotate = |r: i32| {
        Ok(Orientation {
            rotate: r,
            ambiguous: false,
        })
    };
    if o == 1.0 {
        rotate(0)
    } else if o == 3.0 {
        rotate(180)
    } else if o == 6.0 {
        rotate(90)
    } else if o == 8.0 {
        rotate(270)
    } else if o == 2.0 || o == 4.0 || o == 5.0 || o == 7.0 {
        Err(Error::MirroredOrientation)
    } else {
        Err(malformed(format!("invalid orientation {}", format_g(o))))
    }
}

// --- JP2 -----------------------------------------------------------------------------------

/// The state of one walk over a JP2 file's boxes.
#[derive(Default)]
struct Jp2 {
    w: u32,
    h: u32,
    depth: i64,
    enum_cs: i64,
    dpi: Resolution,
    have_ihdr: bool,
    have_colr: bool,
    have_codestream: bool,
    have_res: bool,
}

impl Jp2 {
    /// The box walk of `EmbedJP2`: `b` is a run of boxes, `nest` the depth of the container.
    fn walk(&mut self, mut b: &[u8], nest: u32) -> Result<()> {
        while !b.is_empty() {
            if b.len() < 8 {
                return Err(malformed("truncated box header"));
            }
            let mut size = u64::from(u32_at(b, 0, false));
            let typ = b.get(4..8).unwrap_or(&[]);
            let mut hdr = 8u64;
            if size == 1 {
                if b.len() < 16 {
                    return Err(malformed("truncated box header"));
                }
                size = u64_at(b, 8, false);
                hdr = 16;
            } else if size == 0 {
                size = b.len() as u64;
            }
            if size < hdr || size > b.len() as u64 {
                return Err(malformed(format!(
                    "{:?} box overruns the file",
                    String::from_utf8_lossy(typ)
                )));
            }
            // size <= b.len(), so both fit a usize.
            let (size, hdr) = (size as usize, hdr as usize);
            let body = b.get(hdr..size).unwrap_or(&[]);
            match typ {
                b"jp2h" | b"res " => {
                    if nest > 2 {
                        return Err(malformed("boxes nest too deep"));
                    }
                    self.walk(body, nest + 1)?;
                }
                b"ihdr" => {
                    if body.len() != 14 || self.have_ihdr {
                        return Err(malformed("bad ihdr box"));
                    }
                    self.h = u32_at(body, 0, false);
                    self.w = u32_at(body, 4, false);
                    let bpc = body.get(10).copied().unwrap_or(0);
                    if bpc == 0xFF || bpc & 0x80 != 0 {
                        return Err(malformed("components differ in depth or are signed"));
                    }
                    self.depth = i64::from(bpc) + 1;
                    self.have_ihdr = true;
                }
                b"colr" => {
                    // Only the first applies.
                    if !self.have_colr {
                        if body.len() < 3 || body.first() != Some(&1) || body.len() < 7 {
                            return Err(malformed("colour is not an enumerated space"));
                        }
                        self.enum_cs = i64::from(u32_at(body, 3, false));
                        self.have_colr = true;
                    }
                }
                b"resc" => {
                    // VRcN VRcD HRcN HRcD VRcE HRcE; img2pdf reads the first, and refuses any
                    // but 10 bytes or a zero denominator.
                    if !self.have_res {
                        self.have_res = true;
                        let u = |i: usize| u16_at(body, i, false);
                        if body.len() != 10 || u(2) == 0 || u(6) == 0 {
                            return Err(malformed(format!(
                                "resc box of {} bytes, or with a zero denominator",
                                body.len()
                            )));
                        }
                        let per_inch = |num: u16, den: u16, exp: u8| {
                            jp2_per_inch(num, den, exp)
                                .ok_or_else(|| malformed("resc resolution is out of range"))
                        };
                        self.dpi = Resolution {
                            x: per_inch(u(4), u(6), body.get(9).copied().unwrap_or(0))?,
                            y: per_inch(u(0), u(2), body.get(8).copied().unwrap_or(0))?,
                        };
                    }
                }
                b"jp2c" => self.have_codestream = true,
                _ => {}
            }
            b = b.get(size..).unwrap_or(&[]);
        }
        Ok(())
    }
}

/// `EmbedJP2`: lifts a JP2 file under `/JPXDecode`, verbatim, after reading its boxes as
/// img2pdf does: size and bit depth from `ihdr`, colour from the first `colr` box, which must
/// be enumerated sRGB (`DeviceRGB`) or greyscale (`DeviceGray`); CMYK, sYCC and ICC-profile
/// files are refused, as img2pdf refuses them. The resolution is the `res` box's first capture
/// resolution box, rounded per axis as img2pdf rounds it (zero if none); a `resc` box that is
/// not 10 bytes, has a zero denominator or gives one zero axis is refused, as img2pdf refuses
/// it. The pixel cap in `limits` is checked against the `ihdr` before the image is built, so a
/// forged header is refused as `limit encoded-image-pixels`. byblos does not decode JPX; the
/// returned image is for `BuildPDF` only.
///
/// # Errors
/// [`Error::Malformed`] for a file img2pdf refuses, [`Error::Limit`], [`Error::Alloc`] and
/// [`Error::OutOfMemory`].
pub fn embed_jp2(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    embed_jp2_inner(data, limits).map_err(|e| wrap(e, "byblos: EmbedJP2"))
}

fn embed_jp2_inner(data: &[u8], limits: &Limits) -> Result<(EncodedImage, Resolution)> {
    if !data.starts_with(&JP2_SIGNATURE) {
        return Err(malformed("no JP2 signature box"));
    }
    let mut s = Jp2::default();
    s.walk(data, 0)?;
    if !s.have_ihdr || !s.have_colr || !s.have_codestream {
        return Err(malformed("no ihdr, colr or codestream box"));
    }
    if s.w == 0 || s.h == 0 {
        return Err(malformed(format!("ihdr declares {}x{}", s.w, s.h)));
    }
    let cap = limits.encoded_image_pixels();
    check(
        "encoded-image-pixels",
        mul_sat(i64::from(s.w), i64::from(s.h)),
        cap,
    )?;
    check("encoded-image-pixels", i64::from(s.w.max(s.h)), cap)?;
    let name = match s.enum_cs {
        16 => "DeviceRGB",
        17 => "DeviceGray",
        other => {
            return Err(malformed(format!(
                "only sRGB and greyscale are supported, got enumerated colour space {other}"
            )));
        }
    };
    s.dpi.check()?;
    Ok((
        EncodedImage {
            width: i64::from(s.w),
            height: i64::from(s.h),
            bpc: s.depth,
            color_space: ColorSpace {
                name: name.to_string(),
                ..ColorSpace::default()
            },
            filter: "JPXDecode".to_string(),
            data: copy_charged(data, limits)?,
            ..EncodedImage::default()
        },
        s.dpi,
    ))
}

#[cfg(test)]
mod tests;
