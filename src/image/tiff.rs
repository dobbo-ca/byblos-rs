//! The minimal TIFF reader behind `image.Decode`: a port of the part of `hhrutter/tiff` v1.0.7
//! (`reader.go`, `reader_config.go`, `buffer.go`) that reads the files pdf-rs `image/tiff.rs`
//! writes. `hhrutter/tiff` registers itself with `image.Decode` through pdf-go (docs/scope.md
//! section 4.4), which is how byblos decodes the TIFF that pdfcpu renders for a CMYK or RGBA
//! raster.
//!
//! Supported, with Go's result types and errors: either byte order, one or more strips,
//! uncompressed data, 8 bits per sample, and the colour types
//!
//! * grey (`WhiteIsZero`, `BlackIsZero`; a missing photometric tag is `WhiteIsZero`): [`Gray`];
//! * RGB with three samples: [`Rgba`] with alpha 255;
//! * RGB with four samples, `ExtraSamples` 0 or 1: [`Rgba`]; 2 (what pdf-rs writes): [`Nrgba`];
//! * CMYK: [`Cmyk`].
//!
//! Everything else that `hhrutter/tiff` decodes (tiles, LZW, Deflate, PackBits, CCITT and JPEG
//! compression, predictors, 1 and 16 bits per sample, palettes, YCbCr) is not read: the reader
//! returns an `unsupported feature` error where Go returns an image, so those inputs are a known
//! difference. `DecodeConfig` reports the colour model of the 1-bit and 16-bit grey and RGB
//! types too. The `ColorMap` and `JPEGTables` entries are not parsed.
//!
//! The error classes follow Go's `buffer`: a read past the end of the data is a raw `io.EOF`
//! when it is a bounded read of the IFD (class `error`, [`Error::Image`] with the text `EOF`),
//! and `io.EOF` or `io.ErrUnexpectedEOF` when the buffer fills up to a strip, depending on
//! whether the data ends where a 32 KiB fill starts.
//!
//! Go BSD licence: see NOTICE.

use super::decode::{ColorModel, Config};
use super::{Cmyk, Color, Gray, Image, Nrgba, Rect, Rgba};
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

const LE_HEADER: &[u8] = b"II\x2a\x00";
const BE_HEADER: &[u8] = b"MM\x00\x2a";

/// The length of an IFD entry.
const IFD_LEN: usize = 12;

/// The size of Go's `buffer` fill chunk.
const FILL_CHUNK: usize = 32 << 10;

/// `ifdValueCacheSize`.
const IFD_VALUE_CACHE: usize = 32 << 10;

/// Go's `lengths`, by data type.
const LENGTHS: [u32; 6] = [0, 1, 1, 2, 4, 8];

const DT_BYTE: u16 = 1;
const DT_SHORT: u16 = 3;
const DT_LONG: u16 = 4;

const T_IMAGE_WIDTH: u16 = 256;
const T_IMAGE_LENGTH: u16 = 257;
const T_BITS_PER_SAMPLE: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC: u16 = 262;
const T_FILL_ORDER: u16 = 266;
const T_STRIP_OFFSETS: u16 = 273;
const T_ROWS_PER_STRIP: u16 = 278;
const T_STRIP_BYTE_COUNTS: u16 = 279;
const T_T4_OPTIONS: u16 = 292;
const T_T6_OPTIONS: u16 = 293;
const T_TILE_WIDTH: u16 = 322;
const T_TILE_LENGTH: u16 = 323;
const T_PREDICTOR: u16 = 317;
const T_EXTRA_SAMPLES: u16 = 338;
const T_SAMPLE_FORMAT: u16 = 339;
const T_COLOR_MAP: u16 = 320;
const T_JPEG_TABLES: u16 = 347;

const P_WHITE_IS_ZERO: u32 = 0;
const P_BLACK_IS_ZERO: u32 = 1;
const P_RGB: u32 = 2;
const P_PALETTED: u32 = 3;
const P_CMYK: u32 = 5;
const P_YCBCR: u32 = 6;

const C_NONE: u32 = 1;

/// The tags `parseIFD` reads with `ifdUint`.
const SMALL_TAGS: [u16; 13] = [
    T_BITS_PER_SAMPLE,
    T_EXTRA_SAMPLES,
    T_PHOTOMETRIC,
    T_COMPRESSION,
    T_PREDICTOR,
    T_ROWS_PER_STRIP,
    T_TILE_WIDTH,
    T_TILE_LENGTH,
    T_IMAGE_LENGTH,
    T_IMAGE_WIDTH,
    T_FILL_ORDER,
    T_T4_OPTIONS,
    T_T6_OPTIONS,
];

/// `FormatError`.
fn format_err(s: &str) -> Error {
    Error::Image(format!("tiff: invalid format: {s}"))
}

/// `UnsupportedError`.
fn unsupported(s: &str) -> Error {
    Error::Image(format!("tiff: unsupported feature: {s}"))
}

/// A read past the end of the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Io {
    /// `io.EOF`.
    Eof,
    /// `io.ErrUnexpectedEOF`.
    Unexpected,
}

fn io_err(e: Io) -> Error {
    match e {
        Io::Eof => Error::Image("EOF".to_string()),
        Io::Unexpected => Error::Pdf(pdf::Error::UnexpectedEof),
    }
}

/// Go's `buffer`: `filled` is the length of its buffer, which only the reads that came before
/// decide, and which tells `io.EOF` from `io.ErrUnexpectedEOF`.
struct Buf<'a> {
    data: &'a [u8],
    filled: usize,
}

impl<'a> Buf<'a> {
    /// `buffer.fill`.
    fn fill(&mut self, end: usize) -> std::result::Result<(), Io> {
        while self.filled < end {
            let next = (end - self.filled).min(FILL_CHUNK);
            let avail = self.data.len() - self.filled;
            if avail < next {
                self.filled = self.data.len();
                return Err(if avail == 0 { Io::Eof } else { Io::Unexpected });
            }
            self.filled += next;
        }
        Ok(())
    }

    /// `buffer.ReadAt` for a whole read: a short read is `io.EOF`.
    fn read_at(&mut self, off: usize, n: usize) -> std::result::Result<&'a [u8], Io> {
        if n == 0 {
            return Ok(&[]);
        }
        let end = off.checked_add(n).ok_or(Io::Unexpected)?;
        // The error of `fill` is not the caller's: `ReadAt` reports a short read as `io.EOF`.
        let _ = self.fill(end);
        self.data.get(off..end).ok_or(Io::Eof)
    }

    /// `buffer.Slice`.
    fn slice(&mut self, off: usize, n: usize) -> std::result::Result<&'a [u8], Io> {
        let end = off.checked_add(n).ok_or(Io::Unexpected)?;
        self.fill(end)?;
        self.data.get(off..end).ok_or(Io::Unexpected)
    }
}

/// The byte order of the file.
#[derive(Debug, Clone, Copy)]
struct Order {
    little: bool,
}

impl Order {
    fn u16(self, b: &[u8], i: usize) -> u16 {
        let a = [
            b.get(i).copied().unwrap_or(0),
            b.get(i + 1).copied().unwrap_or(0),
        ];
        if self.little {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }

    fn u32(self, b: &[u8], i: usize) -> u32 {
        let a = [
            b.get(i).copied().unwrap_or(0),
            b.get(i + 1).copied().unwrap_or(0),
            b.get(i + 2).copied().unwrap_or(0),
            b.get(i + 3).copied().unwrap_or(0),
        ];
        if self.little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }
}

/// `checkedMul3`: the product, when it fits Go's `int`.
fn checked_mul3(a: u64, b: u64, c: u64) -> Option<u64> {
    let max = i64::MAX as u64;
    let ab = a.checked_mul(b).filter(|&v| v <= max)?;
    ab.checked_mul(c).filter(|&v| v <= max)
}

/// The value of the entry `p` at `i`, for a byte, short or long entry in `raw`.
fn raw_value(o: Order, datatype: u16, raw: &[u8], i: usize) -> u32 {
    match datatype {
        DT_BYTE => u32::from(raw.get(i).copied().unwrap_or(0)),
        DT_SHORT => u32::from(o.u16(raw, 2 * i)),
        _ => o.u32(raw, 4 * i),
    }
}

/// `decoder.ifdUint`: up to `max_count` values of a byte, short or long entry.
fn ifd_uint(o: Order, buf: &mut Buf<'_>, p: &[u8], max_count: usize) -> Result<Vec<u32>> {
    let datatype = o.u16(p, 2);
    let Some(&len) = LENGTHS.get(usize::from(datatype)).filter(|_| datatype != 0) else {
        return Err(unsupported("IFD entry datatype"));
    };
    let count = o.u32(p, 4);
    if count > i32::MAX as u32 / len {
        return Err(format_err("IFD data too large"));
    }
    let truncated = (count as usize).min(max_count);
    let raw = if len * count > 4 {
        // The IFD contains a pointer to the real value.
        let truncated_len = len as usize * truncated;
        buf.read_at(o.u32(p, 8) as usize, truncated_len)
            .map_err(io_err)?
    } else {
        p.get(8..8 + (len * count) as usize).unwrap_or(&[])
    };
    if !matches!(datatype, DT_BYTE | DT_SHORT | DT_LONG) {
        return Err(unsupported("data type"));
    }
    Ok((0..truncated)
        .map(|i| raw_value(o, datatype, raw, i))
        .collect())
}

/// `ifdValues`: bounded access to the offsets or byte counts of the strips.
#[derive(Default)]
struct IfdValues {
    datatype: u16,
    count: usize,
    offset: usize,
    inline: [u8; 4],
    is_inline: bool,
    cache: Vec<u8>,
    cache_at: usize,
}

impl IfdValues {
    /// `newIFDValues`.
    fn new(o: Order, buf: &mut Buf<'_>, p: &[u8], max_count: usize) -> Result<IfdValues> {
        let datatype = o.u16(p, 2);
        let Some(&len) = LENGTHS.get(usize::from(datatype)).filter(|_| datatype != 0) else {
            return Err(unsupported("IFD entry datatype"));
        };
        if !matches!(datatype, DT_BYTE | DT_SHORT | DT_LONG) {
            return Err(unsupported("data type"));
        }
        let count = o.u32(p, 4);
        if count > i32::MAX as u32 / len {
            return Err(format_err("IFD data too large"));
        }
        let mut v = IfdValues {
            datatype,
            count: (count as usize).min(max_count),
            ..IfdValues::default()
        };
        if len * count <= 4 {
            for (dst, src) in v.inline.iter_mut().zip(p.iter().skip(8)) {
                *dst = *src;
            }
            v.is_inline = true;
        } else {
            v.offset = o.u32(p, 8) as usize;
        }
        v.validate(buf)?;
        Ok(v)
    }

    fn value_size(&self) -> usize {
        LENGTHS
            .get(usize::from(self.datatype))
            .map_or(0, |&l| l as usize)
    }

    /// `ifdValues.validate`: the last value must be there.
    fn validate(&self, buf: &mut Buf<'_>) -> Result<()> {
        if self.is_inline || self.count == 0 {
            return Ok(());
        }
        let last = self
            .count
            .checked_mul(self.value_size())
            .and_then(|n| self.offset.checked_add(n))
            .map(|n| n - 1)
            .ok_or_else(|| io_err(Io::Unexpected))?;
        buf.read_at(last, 1).map_err(io_err)?;
        Ok(())
    }

    /// `ifdValues.value`.
    fn value(&mut self, o: Order, buf: &mut Buf<'_>, index: usize) -> Result<u32> {
        if index >= self.count {
            return Err(format_err("IFD value index out of range"));
        }
        let size = self.value_size();
        let at = index * size;
        if self.is_inline {
            return Ok(raw_value(o, self.datatype, &self.inline, index));
        }
        if at < self.cache_at || at + size > self.cache_at + self.cache.len() {
            let n = IFD_VALUE_CACHE.min(self.count * size - at);
            self.cache = buf.read_at(self.offset + at, n).map_err(io_err)?.to_vec();
            self.cache_at = at;
        }
        let start = at - self.cache_at;
        let bytes = self.cache.get(start..start + size).unwrap_or(&[]);
        Ok(raw_value(o, self.datatype, bytes, 0))
    }
}

/// `imageMode`, for the modes the reader decodes or reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Gray,
    GrayInvert,
    Rgb,
    Rgba,
    Nrgba,
    Cmyk,
}

/// The parsed header of the first image.
struct Decoder {
    order: Order,
    width: usize,
    height: usize,
    model: ColorModel,
    mode: Mode,
    bpp: u32,
    palette: Vec<Color>,
    features: HashMap<u16, Vec<u32>>,
    ifd: HashMap<u16, [u8; IFD_LEN]>,
}

impl Decoder {
    /// `firstVal`.
    fn first_val(&self, tag: u16) -> u32 {
        self.features
            .get(&tag)
            .and_then(|f| f.first())
            .copied()
            .unwrap_or(0)
    }

    /// The number of `BitsPerSample` values.
    fn samples(&self) -> usize {
        self.features.get(&T_BITS_PER_SAMPLE).map_or(0, Vec::len)
    }

    /// `setRGBAMode`.
    fn set_rgba_mode(&mut self, associated: bool) {
        let sixteen = self.bpp == 16;
        (self.mode, self.model) = match (associated, sixteen) {
            (true, false) => (Mode::Rgba, ColorModel::Rgba),
            (true, true) => (Mode::Rgba, ColorModel::Rgba64),
            (false, false) => (Mode::Nrgba, ColorModel::Nrgba),
            (false, true) => (Mode::Nrgba, ColorModel::Nrgba64),
        };
    }

    /// `initRGB`.
    fn init_rgb(&mut self) -> Result<()> {
        let bits = if self.bpp == 16 { 16 } else { 8 };
        if self
            .features
            .get(&T_BITS_PER_SAMPLE)
            .is_some_and(|f| f.iter().any(|&b| b != bits))
        {
            return Err(format_err(&format!(
                "wrong number of samples for {bits}bit RGB"
            )));
        }
        // RGB images normally have 3 samples per pixel. If there are more, ExtraSamples gives
        // their meaning (usually an alpha channel).
        match self.samples() {
            3 => {
                self.mode = Mode::Rgb;
                self.model = if self.bpp == 16 {
                    ColorModel::Rgba64
                } else {
                    ColorModel::Rgba
                };
            }
            4 => match self.first_val(T_EXTRA_SAMPLES) {
                0 | 1 => self.set_rgba_mode(true),
                2 => self.set_rgba_mode(false),
                _ => return Err(format_err("wrong number of samples for RGB")),
            },
            _ => return Err(format_err("wrong number of samples for RGB")),
        }
        Ok(())
    }

    /// `initBitsPerSample`.
    fn init_bits_per_sample(&mut self) -> Result<()> {
        // The default is 1 per specification.
        let bps = self
            .features
            .entry(T_BITS_PER_SAMPLE)
            .or_insert_with(|| vec![1]);
        self.bpp = bps.first().copied().unwrap_or(0);
        match self.bpp {
            0 => Err(format_err("BitsPerSample must not be 0")),
            1 | 8 | 16 => Ok(()),
            n => Err(unsupported(&format!("BitsPerSample of {n}"))),
        }
    }

    /// `initColorModel`.
    fn init_color_model(&mut self) -> Result<()> {
        match self.first_val(T_PHOTOMETRIC) {
            P_RGB => return self.init_rgb(),
            // Only decode_config returns this model; decode does not read palette data.
            P_PALETTED => self.model = ColorModel::Paletted(std::mem::take(&mut self.palette)),
            p @ (P_WHITE_IS_ZERO | P_BLACK_IS_ZERO) => {
                self.mode = if p == P_WHITE_IS_ZERO {
                    Mode::GrayInvert
                } else {
                    Mode::Gray
                };
                self.model = if self.bpp == 16 {
                    ColorModel::Gray16
                } else {
                    ColorModel::Gray
                };
            }
            P_CMYK => {
                self.mode = Mode::Cmyk;
                if self.bpp == 16 {
                    return Err(unsupported(&format!("CMYK BitsPerSample of {}", self.bpp)));
                }
                self.model = ColorModel::Cmyk;
            }
            _ => return Err(unsupported("color model")),
        }
        Ok(())
    }

    /// `initConfig`.
    fn init_config(&mut self) -> Result<()> {
        self.width = self.first_val(T_IMAGE_WIDTH) as usize;
        self.height = self.first_val(T_IMAGE_LENGTH) as usize;
        if checked_mul3(self.width as u64, self.height as u64, 8).is_none() {
            return Err(format_err("image too large"));
        }
        if self.first_val(T_PHOTOMETRIC) == P_YCBCR {
            return Err(unsupported("YCbCr"));
        }
        self.init_bits_per_sample()?;
        self.init_color_model()
    }

    /// `collectImageIFD` and `parseImageIFD`: reads the tags of the IFD `entries`.
    fn parse_image_ifd(&mut self, buf: &mut Buf<'_>, entries: &[u8]) -> Result<()> {
        let o = self.order;
        let mut prev: Option<u16> = None;
        for p in entries.chunks_exact(IFD_LEN) {
            let tag = o.u16(p, 0);
            if prev.is_some_and(|t| tag <= t) {
                return Err(format_err("tags are not sorted in ascending order"));
            }
            let mut entry = [0u8; IFD_LEN];
            entry.copy_from_slice(p);
            self.ifd.insert(tag, entry);
            prev = Some(tag);
        }
        let ycbcr = self.ifd.get(&T_PHOTOMETRIC).is_some_and(|p| {
            o.u16(p, 2) == DT_SHORT && o.u32(p, 4) == 1 && u32::from(o.u16(p, 8)) == P_YCBCR
        });
        if ycbcr {
            return Err(unsupported("YCbCr"));
        }
        for p in entries.chunks_exact(IFD_LEN) {
            let tag = o.u16(p, 0);
            if SMALL_TAGS.contains(&tag) {
                let val = ifd_uint(o, buf, p, 32)?;
                self.features.insert(tag, val);
            } else if tag == T_COLOR_MAP {
                // parseIFDColorMap.
                let val = ifd_uint(o, buf, p, 3 * 256 + 1)?;
                let n = val.len() / 3;
                if !val.len().is_multiple_of(3) || n == 0 || n > 256 {
                    return Err(format_err("bad ColorMap length"));
                }
                let (r, rest) = val.split_at(n);
                let (g, b) = rest.split_at(n);
                self.palette = r
                    .iter()
                    .zip(g)
                    .zip(b)
                    .map(|((&r, &g), &b)| Color::Rgba64 {
                        r: r as u16,
                        g: g as u16,
                        b: b as u16,
                        a: 0xffff,
                    })
                    .collect();
            } else if tag == T_JPEG_TABLES {
                // validateJPEGTables.
                if o.u16(p, 2) != 7 {
                    return Err(unsupported("IFD entry datatype"));
                }
                if o.u32(p, 4) < 4 {
                    return Err(format_err("bad JPEG tables"));
                }
            } else if tag == T_SAMPLE_FORMAT {
                // A SampleFormat other than 1 (unsigned integer data) ends the import.
                if ifd_uint(o, buf, p, 32)?.iter().any(|&v| v != 1) {
                    return Err(unsupported("sample format"));
                }
            }
        }
        Ok(())
    }

    /// `parseIFDOffsets`: the values of a strip entry, or none when the tag is missing.
    fn ifd_offsets(&self, buf: &mut Buf<'_>, tag: u16, max_count: usize) -> Result<IfdValues> {
        match self.ifd.get(&tag) {
            None => Ok(IfdValues::default()),
            Some(p) => IfdValues::new(self.order, buf, p, max_count),
        }
    }
}

/// `newDecoderAt(r, 0)`: reads the header and the first IFD.
fn new_decoder(buf: &mut Buf<'_>, limits: &Limits) -> Result<Decoder> {
    // readImageIFD.
    let p = buf.read_at(0, 8).map_err(|e| match e {
        Io::Eof => io_err(Io::Unexpected),
        e => io_err(e),
    })?;
    let order = if p.starts_with(LE_HEADER) {
        Order { little: true }
    } else if p.starts_with(BE_HEADER) {
        Order { little: false }
    } else {
        return Err(format_err("malformed header"));
    };
    let offset = order.u32(p, 4) as usize;
    let count = buf.read_at(offset, 2).map_err(io_err)?;
    // The first two bytes contain the number of entries (12 bytes each). All IFD entries are
    // read in one chunk.
    let num_items = usize::from(order.u16(count, 0));
    // The IFD map is charged before it is built.
    let _ifd_held = limits.mem().charge((IFD_LEN * num_items) as u64)?;
    let entries = buf
        .read_at(offset + 2, IFD_LEN * num_items)
        .map_err(io_err)?;
    let mut d = Decoder {
        order,
        width: 0,
        height: 0,
        model: ColorModel::Gray,
        mode: Mode::Gray,
        bpp: 0,
        palette: Vec::new(),
        features: HashMap::new(),
        ifd: HashMap::new(),
    };
    d.parse_image_ifd(buf, entries)?;
    d.init_config()?;
    Ok(d)
}

/// `tiff.DecodeConfig`: the colour model and size of the first image.
///
/// # Errors
/// [`Error::Image`] for `FormatError` and `UnsupportedError` (with Go's text) and for a raw
/// `io.EOF`, pdf-rs's `UnexpectedEof` for `io.ErrUnexpectedEOF`.
pub fn decode_config(data: &[u8], limits: &Limits) -> Result<Config> {
    let mut buf = Buf { data, filled: 0 };
    let d = new_decoder(&mut buf, limits)?;
    Ok(Config {
        model: d.model,
        width: d.width,
        height: d.height,
    })
}

/// `blockDataSize`: the bytes `rows` rows of a strip occupy, or `limit`.
fn block_data_size(d: &Decoder, width: usize, rows: usize, limit: u64) -> u64 {
    let row_bits = checked_mul3(width as u64, d.samples() as u64, u64::from(d.bpp));
    let Some(row_bits) = row_bits.filter(|&b| b > 0 && b <= i64::MAX as u64 - 7) else {
        return limit;
    };
    let row_bytes = row_bits.div_ceil(8);
    if rows == 0 || rows as u64 > limit / row_bytes {
        return limit;
    }
    row_bytes * rows as u64
}

/// `tiff.Decode`: the first image of the TIFF file `data`.
///
/// The image buffer is checked against `limits.max_alloc` before it is allocated, and `cancel`
/// is checked per strip and per row.
///
/// # Errors
/// As [`decode_config`], and [`Error::Image`] with Go's `not enough pixel data` for a strip
/// that is too short, [`Error::Alloc`], [`Error::OutOfMemory`] and [`Error::Cancelled`].
pub fn decode(data: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Image> {
    let mut buf = Buf { data, filled: 0 };
    let d = new_decoder(&mut buf, limits)?;
    let o = d.order;

    if matches!(d.model, ColorModel::Paletted(_)) {
        return Err(unsupported("palette"));
    }
    // readBlockLayout. Tiles are not read.
    if d.first_val(T_TILE_WIDTH) != 0 {
        return Err(unsupported("tiles"));
    }
    let (width, height) = (d.width, d.height);
    let (mut across, mut down) = (1usize, 1usize);
    if width == 0 {
        across = 0;
    }
    if height == 0 {
        down = 0;
    }
    let mut strip_height = height;
    let rows_per_strip = d.first_val(T_ROWS_PER_STRIP) as usize;
    if rows_per_strip > 0 && rows_per_strip < strip_height {
        strip_height = rows_per_strip;
    }
    if strip_height != 0 {
        down = height.div_ceil(strip_height);
    }
    let mut offsets = d.ifd_offsets(&mut buf, T_STRIP_OFFSETS, down)?;
    let mut counts = d.ifd_offsets(&mut buf, T_STRIP_BYTE_COUNTS, down)?;
    // Check that there are the right number of strips, offsets and counts.
    let n = across * down;
    if offsets.count < n || counts.count < n {
        return Err(format_err("inconsistent header"));
    }

    // newImage: what Go reads that this reader does not is refused here.
    if d.bpp != 8 {
        return Err(unsupported(&format!("BitsPerSample of {}", d.bpp)));
    }
    let out_bpp = match d.mode {
        Mode::Gray | Mode::GrayInvert => 1,
        Mode::Rgb | Mode::Rgba | Mode::Nrgba | Mode::Cmyk => 4,
    };
    // Bytes per pixel in the file.
    let in_bpp = if d.mode == Mode::Rgb { 3 } else { out_bpp };
    let over = Error::Alloc {
        got: u64::MAX,
        max: limits.alloc_limit(),
    };
    let stride = width.checked_mul(out_bpp).ok_or_else(|| over.clone())?;
    let mut pix = limits.alloc_zeroed(stride.checked_mul(height).ok_or(over)?)?;
    let rect = Rect::new(0, 0, width as i64, height as i64);
    if across == 0 || down == 0 {
        return Ok(finish(d.mode, pix, stride, rect));
    }
    let Some(limit) = checked_mul3(width as u64, strip_height as u64, 8) else {
        return Err(format_err("block data size too large"));
    };

    // decodeBlocks, with a single strip across.
    for j in 0..down {
        check_cancel(cancel)?;
        let rows = if j == down - 1 && !height.is_multiple_of(strip_height) {
            height % strip_height
        } else {
            strip_height
        };
        let offset = offsets.value(o, &mut buf, j)?;
        let count = counts.value(o, &mut buf, j)?;

        // decodeBlock and loadBlockData.
        let compression = d.first_val(T_COMPRESSION);
        if compression != 0 && compression != C_NONE {
            return Err(unsupported(&format!("compression value {compression}")));
        }
        let want = block_data_size(&d, width, rows, limit);
        let block = buf
            .slice(offset as usize, u64::from(count).min(want) as usize)
            .map_err(io_err)?;

        // decode. A horizontal predictor is not applied.
        if d.first_val(T_PREDICTOR) == 2 {
            return Err(unsupported("horizontal predictor"));
        }
        let y0 = j * strip_height;
        for y in y0..(y0 + rows).min(height) {
            check_cancel(cancel)?;
            let row = y - y0;
            let i0 = row * width * in_bpp;
            let Some(src) = block.get(i0..i0 + width * in_bpp) else {
                return Err(format_err("not enough pixel data"));
            };
            let Some(dst) = pix.get_mut(y * stride..(y + 1) * stride) else {
                break;
            };
            convert_row(d.mode, src, dst);
        }
    }
    Ok(finish(d.mode, pix, stride, rect))
}

/// One row of 8-bit pixels of `mode`, as the image type of the mode holds it.
fn convert_row(mode: Mode, src: &[u8], dst: &mut [u8]) {
    match mode {
        Mode::Gray => dst.copy_from_slice(src),
        Mode::GrayInvert => {
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = 0xff - s;
            }
        }
        Mode::Rgb => {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                d.copy_from_slice(&[
                    s.first().copied().unwrap_or(0),
                    s.get(1).copied().unwrap_or(0),
                    s.get(2).copied().unwrap_or(0),
                    0xff,
                ]);
            }
        }
        Mode::Rgba | Mode::Nrgba | Mode::Cmyk => dst.copy_from_slice(src),
    }
}

fn finish(mode: Mode, pix: Vec<u8>, stride: usize, rect: Rect) -> Image {
    match mode {
        Mode::Gray | Mode::GrayInvert => Image::Gray(Gray { pix, stride, rect }),
        Mode::Rgb | Mode::Rgba => Image::Rgba(Rgba { pix, stride, rect }),
        Mode::Nrgba => Image::Nrgba(Nrgba { pix, stride, rect }),
        Mode::Cmyk => Image::Cmyk(Cmyk { pix, stride, rect }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TIFF in the layout pdf-rs writes (the header, one strip, the IFD, its pointer area),
    /// with the given tags: (tag, type, values).
    fn build(le: bool, tags: &[(u16, u16, &[u32])], pix: &[u8]) -> Vec<u8> {
        let p16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let p32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut out = Vec::new();
        out.extend_from_slice(if le { LE_HEADER } else { BE_HEADER });
        let ifd_off = 8 + pix.len() as u32;
        out.extend_from_slice(&p32(ifd_off));
        out.extend_from_slice(pix);
        out.extend_from_slice(&p16(tags.len() as u16));
        let parea_start = ifd_off + 2 + 12 * tags.len() as u32 + 4;
        let mut parea = Vec::new();
        for &(tag, dt, vals) in tags {
            out.extend_from_slice(&p16(tag));
            out.extend_from_slice(&p16(dt));
            out.extend_from_slice(&p32(vals.len() as u32));
            let size = if dt == DT_SHORT { 2 } else { 4 };
            let mut bytes = Vec::new();
            for &v in vals {
                if dt == DT_SHORT {
                    bytes.extend_from_slice(&p16(v as u16));
                } else {
                    bytes.extend_from_slice(&p32(v));
                }
            }
            if vals.len() * size <= 4 {
                bytes.resize(4, 0);
                out.extend_from_slice(&bytes);
            } else {
                out.extend_from_slice(&p32(parea_start + parea.len() as u32));
                parea.extend_from_slice(&bytes);
            }
        }
        out.extend_from_slice(&p32(0));
        out.extend_from_slice(&parea);
        out
    }

    /// The tags of `build` for a `w` by `h` image of `bps`, `photometric` and `extra` samples.
    fn tags(
        w: u32,
        h: u32,
        bps: &'static [u32],
        photometric: u32,
        extra: Option<u32>,
        size: u32,
    ) -> Vec<(u16, u16, Vec<u32>)> {
        let mut t = vec![
            (T_IMAGE_WIDTH, DT_LONG, vec![w]),
            (T_IMAGE_LENGTH, DT_LONG, vec![h]),
            (T_BITS_PER_SAMPLE, DT_SHORT, bps.to_vec()),
            (T_COMPRESSION, DT_SHORT, vec![C_NONE]),
            (T_PHOTOMETRIC, DT_SHORT, vec![photometric]),
            (T_STRIP_OFFSETS, DT_LONG, vec![8]),
            (T_ROWS_PER_STRIP, DT_LONG, vec![h]),
            (T_STRIP_BYTE_COUNTS, DT_LONG, vec![size]),
        ];
        if let Some(e) = extra {
            t.push((T_EXTRA_SAMPLES, DT_SHORT, vec![e]));
        }
        t
    }

    fn file(le: bool, t: &[(u16, u16, Vec<u32>)], pix: &[u8]) -> Vec<u8> {
        let refs: Vec<(u16, u16, &[u32])> = t.iter().map(|(a, b, c)| (*a, *b, &c[..])).collect();
        build(le, &refs, pix)
    }

    fn config(data: &[u8]) -> Result<Config> {
        decode_config(data, &Limits::default())
    }

    fn run(data: &[u8]) -> Result<Image> {
        decode(data, &Limits::default(), &AtomicBool::new(false))
    }

    #[test]
    fn cmyk_and_nrgba_as_pdf_rs_writes_them() {
        let pix: Vec<u8> = (0..16).collect();
        for le in [true, false] {
            let t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
            let data = file(le, &t, &pix);
            let cfg = config(&data).unwrap();
            assert_eq!(
                cfg,
                Config {
                    model: ColorModel::Cmyk,
                    width: 2,
                    height: 2
                }
            );
            let Image::Cmyk(img) = run(&data).unwrap() else {
                panic!("not CMYK")
            };
            assert_eq!(img.pix, pix);
            assert_eq!((img.stride, img.rect), (8, Rect::new(0, 0, 2, 2)));

            let t = tags(2, 2, &[8, 8, 8, 8], P_RGB, Some(2), 16);
            let data = file(le, &t, &pix);
            assert_eq!(config(&data).unwrap().model, ColorModel::Nrgba);
            let Image::Nrgba(img) = run(&data).unwrap() else {
                panic!("not NRGBA")
            };
            assert_eq!(img.pix, pix);

            // Associated alpha is RGBA.
            let t = tags(2, 2, &[8, 8, 8, 8], P_RGB, Some(1), 16);
            assert!(matches!(run(&file(le, &t, &pix)), Ok(Image::Rgba(_))));
        }
    }

    #[test]
    fn rgb_and_gray() {
        let pix: Vec<u8> = (1..=6).collect();
        let t = tags(2, 1, &[8, 8, 8], P_RGB, None, 6);
        let Image::Rgba(img) = run(&file(true, &t, &pix)).unwrap() else {
            panic!("not RGBA")
        };
        assert_eq!(img.pix, vec![1, 2, 3, 255, 4, 5, 6, 255]);

        let t = tags(3, 1, &[8], P_BLACK_IS_ZERO, None, 3);
        let Image::Gray(img) = run(&file(true, &t, &[1, 2, 3])).unwrap() else {
            panic!("not Gray")
        };
        assert_eq!(img.pix, vec![1, 2, 3]);
        let t = tags(3, 1, &[8], P_WHITE_IS_ZERO, None, 3);
        let Image::Gray(img) = run(&file(true, &t, &[1, 2, 3])).unwrap() else {
            panic!("not Gray")
        };
        assert_eq!(img.pix, vec![254, 253, 252]);
    }

    #[test]
    fn errors_and_classes() {
        let pix = [0u8; 16];
        let t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        let data = file(true, &t, &pix);

        // Short pixel data: the strip is cut at the end of the file.
        let mut short = file(true, &t, &pix);
        short.truncate(8 + 10);
        // The IFD is gone too: a bounded read of it is a raw EOF.
        assert_eq!(run(&short).unwrap_err().to_string(), "EOF");
        assert_eq!(run(&short).unwrap_err().class(), "error");

        // A header shorter than 8 bytes is io.ErrUnexpectedEOF.
        let e = run(&data[..6]).unwrap_err();
        assert_eq!(e.class(), "pdf/unexpected-eof");

        // A count of 16 bytes in a strip that holds 8 is "not enough pixel data".
        let t2 = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 8);
        let e = run(&file(true, &t2, &pix)).unwrap_err();
        assert_eq!(e.to_string(), "tiff: invalid format: not enough pixel data");

        // A bad header.
        let mut bad = data.clone();
        bad[0] = b'X';
        assert_eq!(
            config(&bad).unwrap_err().to_string(),
            "tiff: invalid format: malformed header"
        );

        // Tags out of order.
        let mut t3 = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        t3.swap(0, 1);
        assert_eq!(
            config(&file(true, &t3, &pix)).unwrap_err().to_string(),
            "tiff: invalid format: tags are not sorted in ascending order"
        );

        // Compression other than none.
        let mut t4 = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        t4[3].2 = vec![5];
        assert_eq!(
            run(&file(true, &t4, &pix)).unwrap_err().to_string(),
            "tiff: unsupported feature: compression value 5"
        );
        // The config does not look at the compression.
        assert!(config(&file(true, &t4, &pix)).is_ok());

        // 16-bit CMYK.
        let t5 = tags(2, 2, &[16, 16, 16, 16], P_CMYK, None, 16);
        assert_eq!(
            config(&file(true, &t5, &pix)).unwrap_err().to_string(),
            "tiff: unsupported feature: CMYK BitsPerSample of 16"
        );

        // A sample count that does not fit RGB.
        let t6 = tags(2, 2, &[8, 8], P_RGB, None, 16);
        assert_eq!(
            config(&file(true, &t6, &pix)).unwrap_err().to_string(),
            "tiff: invalid format: wrong number of samples for RGB"
        );
    }

    #[test]
    fn empty_image_and_limits() {
        let t = tags(0, 0, &[8, 8, 8, 8], P_CMYK, None, 0);
        let Image::Cmyk(img) = run(&file(true, &t, &[])).unwrap() else {
            panic!("not CMYK")
        };
        assert!(img.pix.is_empty());

        // A huge image is refused by the budget before it is allocated.
        let t = tags(40_000, 40_000, &[8, 8, 8, 8], P_CMYK, None, 16);
        let limits = Limits {
            max_alloc: 1 << 20,
            ..Limits::default()
        };
        let e = decode(&file(true, &t, &[0; 16]), &limits, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, Error::Alloc { .. }));
        // A cancelled call stops at the first strip.
        let t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        let e = decode(
            &file(true, &t, &[0; 16]),
            &Limits::default(),
            &AtomicBool::new(true),
        );
        assert_eq!(e.unwrap_err(), Error::Cancelled);
    }

    /// A little-endian TIFF with one IFD at offset 8: (tag, type, count, inline value).
    fn raw_le(entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
        let mut b = LE_HEADER.to_vec();
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(tag, dt, count, value) in entries {
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&dt.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
            b.extend_from_slice(&value.to_le_bytes());
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    /// The first four entries of a 1x1 8-bit gray image.
    fn gray_entries() -> Vec<(u16, u16, u32, u32)> {
        vec![
            (T_IMAGE_WIDTH, DT_SHORT, 1, 1),
            (T_IMAGE_LENGTH, DT_SHORT, 1, 1),
            (T_BITS_PER_SAMPLE, DT_SHORT, 1, 8),
            (T_PHOTOMETRIC, DT_SHORT, 1, P_BLACK_IS_ZERO),
        ]
    }

    /// `TestNoRPS` and `TestNoCompression`.
    #[test]
    fn missing_rows_per_strip_and_compression() {
        for data in [
            &include_bytes!("../../tests/data/tiff/no_rps.tiff")[..],
            &include_bytes!("../../tests/data/tiff/no_compress.tiff")[..],
        ] {
            assert_eq!(config(data).unwrap().model, ColorModel::Nrgba);
            let Image::Nrgba(img) = run(data).unwrap() else {
                panic!("not NRGBA")
            };
            assert_eq!(img.rect, Rect::new(0, 0, 16, 15));
        }
    }

    /// `TestShortBlockData`: the byte count of the strip is less than the pixel data.
    #[test]
    fn short_block_data() {
        let t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 1);
        assert!(run(&file(true, &t, &[0u8; 16])).is_err());
    }

    /// `TestDecodeInvalidDataType`.
    #[test]
    fn decode_invalid_data_type() {
        let mut t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        t[0].1 = LENGTHS.len() as u16;
        let e = run(&file(true, &t, &[0u8; 16])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "tiff: unsupported feature: IFD entry datatype"
        );
    }

    /// `TestDecodeTagOrder`: the first two IFD entries are swapped.
    #[test]
    fn decode_tag_order() {
        let mut t = tags(2, 2, &[8, 8, 8, 8], P_CMYK, None, 16);
        t.swap(0, 1);
        assert!(run(&file(true, &t, &[0u8; 16])).is_err());
    }

    /// `TestZeroBitsPerSample`.
    #[test]
    fn zero_bits_per_sample() {
        let t = tags(2, 2, &[0], P_BLACK_IS_ZERO, None, 4);
        let e = decode(
            &file(true, &t, &[0u8; 4]),
            &Limits::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(
            e.unwrap_err().to_string(),
            "tiff: invalid format: BitsPerSample must not be 0"
        );
    }

    /// `TestZeroSizedImages`, with NRGBA images in the layout pdf-rs writes.
    #[test]
    fn zero_sized_images() {
        for (w, h) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
            let size = w * h * 4;
            let t = tags(w, h, &[8, 8, 8, 8], P_RGB, Some(2), size);
            let pix = vec![0u8; size as usize];
            let img = run(&file(true, &t, &pix)).unwrap();
            let Image::Nrgba(img) = img else {
                panic!("not NRGBA")
            };
            assert_eq!(img.rect, Rect::new(0, 0, i64::from(w), i64::from(h)));
        }
    }

    /// `TestLargeIFDEntry`: a large IFD entry must not panic.
    #[test]
    fn large_ifd_entry() {
        let data = [
            &b"II*\x00\x08\x00\x00\x00\x0c\x00"[..],
            b"0000000000",
            b"00000000000000000000",
            b"00000000000000000000",
            b"00000000000000000000",
            b"00000000000000\x17\x01\x04\x00\x01\x00",
            b"\x00\xc0",
            b"000000000000000000",
            b"00000000000000000000",
            b"00000000000000000000",
            b"000000",
        ]
        .concat();
        assert!(run(&data).is_err());
    }

    /// `TestRejectSmallJPEGTables`.
    #[test]
    fn reject_small_jpeg_tables() {
        let mut e = gray_entries();
        e.push((T_JPEG_TABLES, 7, 2, 0));
        assert_eq!(
            config(&raw_le(&e)).unwrap_err().to_string(),
            "tiff: invalid format: bad JPEG tables"
        );
        e[4].1 = DT_SHORT;
        assert_eq!(
            config(&raw_le(&e)).unwrap_err().to_string(),
            "tiff: unsupported feature: IFD entry datatype"
        );
    }

    /// `TestDecodeConfigDefersJPEGTables`: the table payload is not read by `decode_config`.
    #[test]
    fn config_defers_jpeg_tables() {
        let mut e = gray_entries();
        e.insert(3, (T_COMPRESSION, DT_SHORT, 1, 7));
        e.push((T_JPEG_TABLES, 7, 1 << 30, 1 << 30));
        let data = raw_le(&e);
        assert!(config(&data).is_ok());
        assert!(run(&data).is_err());
    }

    /// `TestRejectHugeDimensions`.
    #[test]
    fn reject_huge_dimensions() {
        let mut e = gray_entries();
        e[0] = (T_IMAGE_WIDTH, DT_LONG, 1, 1 << 30);
        e[1] = (T_IMAGE_LENGTH, DT_LONG, 1, 1 << 30);
        assert_eq!(
            config(&raw_le(&e)).unwrap_err().to_string(),
            "tiff: invalid format: image too large"
        );
    }

    /// A ColorMap entry gives `Paletted` in the config, as in Go, and is refused by `decode`.
    #[test]
    fn color_map() {
        let mut t = tags(2, 1, &[8], P_PALETTED, None, 2);
        t.push((T_COLOR_MAP, DT_SHORT, vec![1, 2, 3, 4, 5, 6]));
        let data = file(true, &t, &[0, 1]);
        let cfg = config(&data).unwrap();
        let ColorModel::Paletted(pal) = cfg.model else {
            panic!("not paletted")
        };
        assert_eq!(
            pal,
            vec![
                Color::Rgba64 {
                    r: 1,
                    g: 3,
                    b: 5,
                    a: 0xffff
                },
                Color::Rgba64 {
                    r: 2,
                    g: 4,
                    b: 6,
                    a: 0xffff
                },
            ]
        );
        assert_eq!(
            run(&data).unwrap_err().to_string(),
            "tiff: unsupported feature: palette"
        );
        // A length that is not a multiple of three.
        let last = t.len() - 1;
        t[last].2 = vec![1, 2, 3, 4];
        assert_eq!(
            config(&file(true, &t, &[0, 1])).unwrap_err().to_string(),
            "tiff: invalid format: bad ColorMap length"
        );
    }

    /// `TestReadAt` and `TestBufferInvalidOffsets`, for the slice-backed buffer.
    #[test]
    fn buffer_reads() {
        let mut b = Buf {
            data: b"abcdef",
            filled: 0,
        };
        assert_eq!(b.read_at(0, 2), Ok(&b"ab"[..]));
        assert_eq!(b.read_at(3, 3), Ok(&b"def"[..]));
        assert_eq!(b.read_at(5, 3), Err(Io::Eof));
        assert_eq!(b.read_at(6, 3), Err(Io::Eof));
        assert_eq!(b.read_at(1, 3), Ok(&b"bcd"[..]));

        let mut b = Buf {
            data: b"abc",
            filled: 0,
        };
        assert!(b.read_at(usize::MAX, 3).is_err());
        assert!(b.slice(usize::MAX, 1).is_err());
        assert_eq!(b.slice(1, 2), Ok(&b"bc"[..]));
        assert!(b.slice(2, 2).is_err());
        assert!(b.slice(1 << 30, 1).is_err());
        assert_eq!(b.filled, 3);
    }

    /// `TestIFDValuesUseBoundedCache`, without the read tracking.
    #[test]
    fn ifd_values_use_bounded_cache() {
        let count = 20_000usize;
        let data: Vec<u8> = (1..=count as u32).flat_map(u32::to_le_bytes).collect();
        let mut buf = Buf {
            data: &data,
            filled: 0,
        };
        let o = Order { little: true };
        let mut entry = [0u8; IFD_LEN];
        entry[2..4].copy_from_slice(&DT_LONG.to_le_bytes());
        entry[4..8].copy_from_slice(&(count as u32).to_le_bytes());
        let mut v = IfdValues::new(o, &mut buf, &entry, count).unwrap();
        for index in [0, IFD_VALUE_CACHE / 4 - 1, IFD_VALUE_CACHE / 4, count - 1] {
            assert_eq!(v.value(o, &mut buf, index).unwrap(), index as u32 + 1);
        }
        assert!(v.cache.len() <= IFD_VALUE_CACHE);
        assert!(v.value(o, &mut buf, count).is_err());
    }
}
