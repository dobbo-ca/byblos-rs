//! Go 1.27's `image/png` decoder, as an adapter over png-rs.
//!
//! png-rs ([`Profile::Go`], [`Format::Native`]) does the chunk walk, the checksums, the zlib
//! stream, the unfiltering and the Adam7 merge with Go's flow and errors; this file turns its
//! rows into the concrete image type Go returns: `Gray`, `Gray16`, `Rgba`, `Rgba64`, `Nrgba`
//! (grey+alpha, and every colour type with a `tRNS` colour key), `Nrgba64` or `Paletted`, with
//! Go's pixel bytes, stride and palette (a palette grows to the largest index a pixel uses, and
//! the missing entries are opaque black; `tRNS` entries become `NRGBA`). It also maps png-rs's
//! errors to Go's classes: `png.FormatError` and `png.UnsupportedError` are [`Error::Image`]
//! with Go's text, `io.ErrUnexpectedEOF` is pdf-rs's `UnexpectedEof`, and the zlib and DEFLATE
//! errors of the pixel rows are pdf-rs's `Flate` errors (class `flate/corrupt`, `zlib/header`
//! and so on). As in Go, an error that shows up only after the last row, while the decoder looks
//! for trailing data, becomes a `FormatError` carrying the text.
//!
//! The IDAT byte source (the copy of the 4096-byte `bufio.Reader` that Go's `zlib.NewReader`
//! puts around the decoder, so the chunk checksums and the "too much pixel data" test see the
//! same bytes as Go) is png-rs's. Go's `image.Decode` puts a second `bufio.Reader` under the
//! decoder; its 4096-byte read granularity is not copied. Only an IDAT chunk with trailing bytes
//! after the zlib stream that straddles one of its block boundaries can tell, and so can a
//! stored block followed by trailing IDAT bytes (docs/go-differences.md).
//!
//! [`decode_config`] stays a reader of its own: Go's `DecodeConfig` stops after `IHDR` (after
//! `tRNS` for a paletted image), so a corrupt later chunk is no error there, while png-rs's
//! `read_info` reads and checks every chunk up to the first `IDAT`.
//!
//! Beyond Go: memory is charged to `limits.max_alloc` (png-rs gets what the ledger has left,
//! and the Go image is charged on top of png-rs's output), and `cancel` is checked per chunk
//! and per row.
//!
//! Go BSD licence: see NOTICE.

use super::decode::{ColorModel, Config};
use super::{Color, Gray, Gray16, Image, Nrgba, Nrgba64, Paletted, Rect, Rgba, Rgba64};
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use png::{Format, Options, Profile, Trns};
use std::sync::atomic::AtomicBool;

/// A colour type and bit depth (Go's `cb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cb {
    G1,
    G2,
    G4,
    G8,
    Ga8,
    Tc8,
    P1,
    P2,
    P4,
    P8,
    Tca8,
    G16,
    Ga16,
    Tc16,
    Tca16,
}

impl Cb {
    /// Go's `cb` of an IHDR bit depth and colour type, `None` for a pair Go does not support.
    fn of(depth: u8, ct: u8) -> Option<Cb> {
        Some(match (depth, ct) {
            (1, 0) => Cb::G1,
            (1, 3) => Cb::P1,
            (2, 0) => Cb::G2,
            (2, 3) => Cb::P2,
            (4, 0) => Cb::G4,
            (4, 3) => Cb::P4,
            (8, 0) => Cb::G8,
            (8, 2) => Cb::Tc8,
            (8, 3) => Cb::P8,
            (8, 4) => Cb::Ga8,
            (8, 6) => Cb::Tca8,
            (16, 0) => Cb::G16,
            (16, 2) => Cb::Tc16,
            (16, 4) => Cb::Ga16,
            (16, 6) => Cb::Tca16,
            _ => return None,
        })
    }

    fn paletted(self) -> bool {
        matches!(self, Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8)
    }
}

/// `FormatError`.
fn format_err(s: &str) -> Error {
    Error::Image(format!("png: invalid format: {s}"))
}

/// `UnsupportedError`.
fn unsupported(s: &str) -> Error {
    Error::Image(format!("png: unsupported feature: {s}"))
}

/// `io.ErrUnexpectedEOF`.
fn unexpected_eof() -> Error {
    Error::Pdf(pdf::Error::UnexpectedEof)
}

fn chunk_order() -> Error {
    format_err("chunk out of order")
}

use crate::go::crc32::update as crc_update;

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([at(b, i), at(b, i + 1)])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([at(b, 0), at(b, 1), at(b, 2), at(b, 3)])
}

/// `copy(dst, src)`.
fn copy_prefix(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    if let (Some(d), Some(s)) = (dst.get_mut(..n), src.get(..n)) {
        d.copy_from_slice(s);
    }
}

/// The 256-entry palette of Go's decoder and the number of entries that belong to it: the
/// `PLTE` colours, then opaque black; the `tRNS` alphas turn the first entries into `NRGBA`
/// (and the count grows to the number of alphas). The spec says that any out-of-range pixel
/// value is an error, but some real-world files have them; Go falls back to opaque black, as
/// libpng 1.5.13 does.
fn palette_of(plte: impl Iterator<Item = [u8; 3]>, alpha: &[u8]) -> (Vec<Color>, usize) {
    let black = Color::Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0xff,
    };
    let mut palette = vec![black; 256];
    let mut np = 0;
    for (slot, [r, g, b]) in palette.iter_mut().zip(plte) {
        *slot = Color::Rgba { r, g, b, a: 0xff };
        np += 1;
    }
    for (slot, &a) in palette.iter_mut().zip(alpha) {
        if let Color::Rgba { r, g, b, .. } = *slot {
            *slot = Color::Nrgba { r, g, b, a };
        }
    }
    (palette, np.max(alpha.len()))
}

/// The chunk reader of `DecodeConfig`: Go's `decoder` up to `IHDR`, or up to `tRNS` (or the
/// first `IDAT`) for a paletted image.
struct ConfigReader<'a> {
    data: &'a [u8],
    pos: usize,
    crc: u32,
    seen_plte: bool,
    cb: Option<Cb>,
    width: usize,
    height: usize,
    plte: &'a [u8],
    trns: &'a [u8],
}

impl<'a> ConfigReader<'a> {
    /// `io.ReadFull(d.r, buf[:n])`.
    fn read_full(&mut self, n: usize) -> Result<&'a [u8]> {
        let rest = self.data.get(self.pos..).unwrap_or(&[]);
        if rest.len() < n {
            self.pos = self.data.len();
            return Err(unexpected_eof());
        }
        let (head, _) = rest.split_at(n);
        self.pos += n;
        Ok(head)
    }

    /// Reads `n` bytes of chunk data and adds them to the chunk checksum.
    fn read_chunk_data(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.read_full(n)?;
        self.crc = crc_update(self.crc, b);
        Ok(b)
    }

    /// `verifyChecksum`.
    fn verify_checksum(&mut self) -> Result<()> {
        let b = self.read_full(4)?;
        if be32(b) != self.crc {
            return Err(format_err("invalid checksum"));
        }
        Ok(())
    }

    fn parse_ihdr(&mut self, length: u32) -> Result<()> {
        if length != 13 {
            return Err(format_err("bad IHDR length"));
        }
        let t = self.read_chunk_data(13)?;
        if at(t, 10) != 0 {
            return Err(unsupported("compression method"));
        }
        if at(t, 11) != 0 {
            return Err(unsupported("filter method"));
        }
        if at(t, 12) > 1 {
            return Err(format_err("invalid interlace method"));
        }

        let w = be32(t) as i32;
        let h = be32(t.get(4..8).unwrap_or(&[])) as i32;
        if w <= 0 || h <= 0 {
            return Err(format_err("non-positive dimension"));
        }
        let n_pixels = i64::from(w) * i64::from(h);
        // There can be up to 8 bytes per pixel, for 16 bits per channel RGBA. Go's int
        // multiplication wraps.
        if n_pixels != n_pixels.wrapping_mul(8) / 8 {
            return Err(unsupported("dimension overflow"));
        }

        let (depth, ct) = (at(t, 8), at(t, 9));
        let Some(cb) = Cb::of(depth, ct) else {
            return Err(unsupported(&format!("bit depth {depth}, color type {ct}")));
        };
        self.cb = Some(cb);
        self.width = w as usize;
        self.height = h as usize;
        self.verify_checksum()
    }

    fn parse_plte(&mut self, length: u32) -> Result<()> {
        let np = (length / 3) as usize; // The number of palette entries.
        let depth = match self.cb {
            Some(Cb::P1) => 1,
            Some(Cb::P2) => 2,
            Some(Cb::P4) => 4,
            _ => 8,
        };
        if !length.is_multiple_of(3) || np == 0 || np > 256 || np > 1usize << depth {
            return Err(format_err("bad PLTE length"));
        }
        self.plte = self.read_chunk_data(3 * np)?;
        self.verify_checksum()
    }

    fn parse_trns(&mut self, length: u32) -> Result<()> {
        if length > 256 {
            return Err(format_err("bad tRNS length"));
        }
        self.trns = self.read_chunk_data(length as usize)?;
        self.verify_checksum()
    }

    /// One chunk; `true` once the reader has what `DecodeConfig` needs. A colour type other
    /// than paletted is done after `IHDR`, so the chunks Go's `parseChunk` handles for those
    /// (`PLTE` of a true colour image, `tRNS` with a colour key) are not here.
    fn parse_chunk(&mut self) -> Result<bool> {
        // Read the length and chunk type.
        let hd = self.read_full(8)?;
        let length = be32(hd);
        let typ = hd.get(4..8).unwrap_or(&[]);
        self.crc = crc_update(0, typ);
        let paletted = self.cb.is_some_and(Cb::paletted);

        match typ {
            b"IHDR" => {
                if self.cb.is_some() {
                    return Err(chunk_order());
                }
                self.parse_ihdr(length)?;
                return Ok(!self.cb.is_some_and(Cb::paletted));
            }
            b"PLTE" => {
                if self.cb.is_none() || self.seen_plte {
                    return Err(chunk_order());
                }
                self.seen_plte = true;
                self.parse_plte(length)?;
                return Ok(false);
            }
            b"tRNS" => {
                // A paletted image needs its PLTE first.
                if !paletted || !self.seen_plte {
                    return Err(chunk_order());
                }
                self.parse_trns(length)?;
                return Ok(true);
            }
            b"IDAT" => {
                if self.cb.is_none() || (paletted && !self.seen_plte) {
                    return Err(chunk_order());
                }
                return Ok(true);
            }
            // The reader only gets here before the first IDAT, so IEND is out of order.
            b"IEND" => return Err(chunk_order()),
            _ => {}
        }
        if length > 0x7fff_ffff {
            return Err(format_err(&format!("Bad chunk length: {length}")));
        }
        // Ignore this chunk (of a known length).
        self.read_chunk_data(length as usize)?;
        self.verify_checksum()?;
        Ok(false)
    }
}

/// The colour type of the file and its colour key (the `tRNS` of a grey or true colour image),
/// which [`convert_row`] needs.
struct Hdr {
    depth: u8,
    cb: Cb,
    use_transparent: bool,
    transparent: [u8; 6],
}

/// The pixel storage of a decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gray,
    Gray16,
    Rgba,
    Rgba64,
    Nrgba,
    Nrgba64,
    Paletted,
}

impl Kind {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Kind::Gray | Kind::Paletted => 1,
            Kind::Gray16 => 2,
            Kind::Rgba | Kind::Nrgba => 4,
            Kind::Rgba64 | Kind::Nrgba64 => 8,
        }
    }
}

/// The pixel storage of a colour type.
fn layout(h: &Hdr) -> Kind {
    let t = h.use_transparent;
    match h.cb {
        Cb::G1 | Cb::G2 | Cb::G4 | Cb::G8 | Cb::Tc8 => {
            if t {
                Kind::Nrgba
            } else if h.cb == Cb::Tc8 {
                Kind::Rgba
            } else {
                Kind::Gray
            }
        }
        Cb::Ga8 | Cb::Tca8 => Kind::Nrgba,
        Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => Kind::Paletted,
        Cb::G16 | Cb::Tc16 => {
            if t {
                Kind::Nrgba64
            } else if h.cb == Cb::Tc16 {
                Kind::Rgba64
            } else {
                Kind::Gray16
            }
        }
        Cb::Ga16 | Cb::Tca16 => Kind::Nrgba64,
    }
}

/// Go's `cb` and colour key of what png-rs read from the header and the `tRNS` chunk.
fn hdr_of(info: &png::Info) -> Hdr {
    let header = &info.header;
    // png-rs only returns the colour types and depths Go supports.
    let cb = Cb::of(header.bit_depth, header.color_type as u8).unwrap_or(Cb::G8);
    let mut h = Hdr {
        depth: header.bit_depth,
        cb,
        use_transparent: false,
        transparent: [0; 6],
    };
    match &info.trns {
        Some(Trns::Gray(v)) => {
            copy_prefix(&mut h.transparent, &v.to_be_bytes());
            let mult = match cb {
                Cb::G1 => 0xff,
                Cb::G2 => 0x55,
                Cb::G4 => 0x11,
                _ => 1,
            };
            if let Some(v) = h.transparent.get_mut(1) {
                *v = v.wrapping_mul(mult);
            }
            h.use_transparent = true;
        }
        Some(Trns::Rgb(rgb)) => {
            for (dst, v) in h.transparent.chunks_exact_mut(2).zip(rgb) {
                dst.copy_from_slice(&v.to_be_bytes());
            }
            h.use_transparent = true;
        }
        _ => {}
    }
    h
}

/// The sample `x` of a row of `depth` (1, 2 or 4) bits per sample.
fn sample(cdat: &[u8], depth: usize, x: usize) -> u8 {
    let bit = x * depth;
    let b = at(cdat, bit / 8);
    (b >> (8 - depth - bit % 8)) & ((1u8 << depth) - 1)
}

/// The eight bytes of a 16-bit-per-channel pixel.
fn px64(r: u16, g: u16, b: u16, a: u16) -> [u8; 8] {
    let [rh, rl] = r.to_be_bytes();
    let [gh, gl] = g.to_be_bytes();
    let [bh, bl] = b.to_be_bytes();
    let [ah, al] = a.to_be_bytes();
    [rh, rl, gh, gl, bh, bl, ah, al]
}

/// Converts the filtered row `cdat` to the pixels of the row `out`. `pal_len` grows to the
/// palette length that the indices imply.
fn convert_row(h: &Hdr, cdat: &[u8], width: usize, out: &mut [u8], pal_len: &mut usize) {
    let depth = usize::from(h.depth);
    let t = &h.transparent;
    match h.cb {
        Cb::G1 | Cb::G2 | Cb::G4 => {
            let mult = match h.cb {
                Cb::G1 => 0xff,
                Cb::G2 => 0x55,
                _ => 0x11,
            };
            if h.use_transparent {
                let ty = at(t, 1);
                for (x, px) in out.chunks_exact_mut(4).enumerate().take(width) {
                    let y = sample(cdat, depth, x) * mult;
                    let a = if y == ty { 0x00 } else { 0xff };
                    px.copy_from_slice(&[y, y, y, a]);
                }
            } else {
                for (x, px) in out.iter_mut().enumerate().take(width) {
                    *px = sample(cdat, depth, x) * mult;
                }
            }
        }
        Cb::G8 => {
            if h.use_transparent {
                let ty = at(t, 1);
                for (x, px) in out.chunks_exact_mut(4).enumerate().take(width) {
                    let y = at(cdat, x);
                    let a = if y == ty { 0x00 } else { 0xff };
                    px.copy_from_slice(&[y, y, y, a]);
                }
            } else {
                copy_prefix(out, cdat);
            }
        }
        Cb::Ga8 => {
            for (x, px) in out.chunks_exact_mut(4).enumerate().take(width) {
                let y = at(cdat, 2 * x);
                px.copy_from_slice(&[y, y, y, at(cdat, 2 * x + 1)]);
            }
        }
        Cb::Tc8 => {
            let (tr, tg, tb) = (at(t, 1), at(t, 3), at(t, 5));
            for (x, px) in out.chunks_exact_mut(4).enumerate().take(width) {
                let (r, g, b) = (at(cdat, 3 * x), at(cdat, 3 * x + 1), at(cdat, 3 * x + 2));
                let a = if h.use_transparent && r == tr && g == tg && b == tb {
                    0x00
                } else {
                    0xff
                };
                px.copy_from_slice(&[r, g, b, a]);
            }
        }
        Cb::P1 | Cb::P2 | Cb::P4 => {
            for (x, px) in out.iter_mut().enumerate().take(width) {
                let idx = sample(cdat, depth, x);
                *pal_len = (*pal_len).max(usize::from(idx) + 1);
                *px = idx;
            }
        }
        Cb::P8 => {
            for &idx in cdat.iter().take(width) {
                *pal_len = (*pal_len).max(usize::from(idx) + 1);
            }
            copy_prefix(out, cdat);
        }
        Cb::Tca8 | Cb::Tca16 => copy_prefix(out, cdat),
        Cb::G16 => {
            if h.use_transparent {
                let ty = be16(t, 0);
                for (x, px) in out.chunks_exact_mut(8).enumerate().take(width) {
                    let y = be16(cdat, 2 * x);
                    let a = if y == ty { 0x0000 } else { 0xffff };
                    px.copy_from_slice(&px64(y, y, y, a));
                }
            } else {
                copy_prefix(out, cdat);
            }
        }
        Cb::Ga16 => {
            for (x, px) in out.chunks_exact_mut(8).enumerate().take(width) {
                let y = be16(cdat, 4 * x);
                let a = be16(cdat, 4 * x + 2);
                px.copy_from_slice(&px64(y, y, y, a));
            }
        }
        Cb::Tc16 => {
            let (tr, tg, tb) = (be16(t, 0), be16(t, 2), be16(t, 4));
            for (x, px) in out.chunks_exact_mut(8).enumerate().take(width) {
                let r = be16(cdat, 6 * x);
                let g = be16(cdat, 6 * x + 2);
                let b = be16(cdat, 6 * x + 4);
                let a = if h.use_transparent && r == tr && g == tg && b == tb {
                    0x0000
                } else {
                    0xffff
                };
                px.copy_from_slice(&px64(r, g, b, a));
            }
        }
    }
}

/// A png-rs error as Go's: `FormatError` and `UnsupportedError` keep their text, and the stream
/// errors of the pixel rows become pdf-rs's `Flate` errors. `budget` is what png-rs was given.
fn map_error(e: png::Error, budget: u64) -> Error {
    use png::Error as P;
    match e {
        P::Zlib(deflate::Error::UnexpectedEof) | P::UnexpectedEof => unexpected_eof(),
        P::Zlib(deflate::Error::Internal(s)) => Error::Image(format!("flate: internal error: {s}")),
        P::Zlib(e) => Error::from(pdf::Error::Flate(e)),
        P::AllocLimit => Error::Alloc {
            got: u64::MAX,
            max: budget,
        },
        P::OutOfMemory => Error::OutOfMemory,
        P::Cancelled => Error::Cancelled,
        // NotPng, Checksum, ChunkOrder, Format, Unsupported and Trailing carry Go's text.
        e => Error::Image(e.to_string()),
    }
}

/// `png.Decode`: the image of the PNG file `data`.
///
/// png-rs decodes the rows (Go profile, rows as stored) with what is left of the `max_alloc`
/// ledger, and its output and the Go image are then charged on top of each other. `cancel` is
/// checked per chunk and per row.
///
/// # Errors
/// [`Error::Image`] for Go's `FormatError` and `UnsupportedError` (with Go's text),
/// pdf-rs's `UnexpectedEof` for a truncated file, `Flate` errors for a bad zlib stream (as
/// Go reports them before the last row), [`Error::Alloc`], [`Error::OutOfMemory`] and
/// [`Error::Cancelled`].
pub fn decode(data: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Image> {
    check_cancel(cancel)?;
    let mem = limits.mem();
    let budget = mem.left();
    let png_limits = png::Limits {
        max_alloc: usize::try_from(budget).unwrap_or(usize::MAX),
        max_threads: limits.threads(),
        max_width: u32::MAX,
        max_height: u32::MAX,
        max_pixels: u64::MAX,
        ..png::Limits::default()
    };
    let opts = Options {
        profile: Profile::Go,
    };
    let (info, native) = png::decode(data, Format::Native, &opts, &png_limits, cancel)
        .map_err(|e| map_error(e, budget))?;
    let _native_held = mem.charge(native.len() as u64)?;

    let h = hdr_of(&info);
    let (width, height) = (info.header.width as usize, info.header.height as usize);
    let kind = layout(&h);
    let stride = width * kind.bytes_per_pixel();
    let (mut pix, _held) = mem.alloc_zeroed(stride * height)?;
    let (palette, palette_len) = if h.cb.paletted() {
        let alpha = match &info.trns {
            Some(Trns::Alpha(a)) => a.as_slice(),
            _ => &[],
        };
        palette_of(info.palette.iter().copied(), alpha)
    } else {
        (Vec::new(), 0)
    };
    let mut pal_len = palette_len;
    let row_bytes = info.row_bytes(Format::Native).unwrap_or(0);
    if row_bytes > 0 && stride > 0 {
        for (cdat, out) in native
            .chunks_exact(row_bytes)
            .zip(pix.chunks_exact_mut(stride))
        {
            convert_row(&h, cdat, width, out, &mut pal_len);
        }
    }

    let rect = Rect::new(0, 0, width as i64, height as i64);
    Ok(match kind {
        Kind::Gray => Image::Gray(Gray { pix, stride, rect }),
        Kind::Gray16 => Image::Gray16(Gray16 { pix, stride, rect }),
        Kind::Rgba => Image::Rgba(Rgba { pix, stride, rect }),
        Kind::Rgba64 => Image::Rgba64(Rgba64 { pix, stride, rect }),
        Kind::Nrgba => Image::Nrgba(Nrgba { pix, stride, rect }),
        Kind::Nrgba64 => Image::Nrgba64(Nrgba64 { pix, stride, rect }),
        Kind::Paletted => Image::Paletted(Paletted {
            pix,
            stride,
            rect,
            palette: palette.into_iter().take(pal_len).collect(),
        }),
    })
}

/// `png.DecodeConfig`: the colour model and size, without decoding the pixels. For a paletted
/// image it reads up to the `tRNS` chunk (or the first `IDAT`), as Go does.
///
/// # Errors
/// As [`decode`], for the chunks it reads.
pub fn decode_config(data: &[u8], limits: &Limits) -> Result<Config> {
    let mut d = ConfigReader {
        data,
        pos: 0,
        crc: 0,
        seen_plte: false,
        cb: None,
        width: 0,
        height: 0,
        plte: &[],
        trns: &[],
    };
    if d.read_full(8)? != b"\x89PNG\r\n\x1a\n" {
        return Err(format_err("not a PNG file"));
    }
    while !d.parse_chunk()? {}
    let Some(cb) = d.cb else {
        return Err(chunk_order());
    };
    let (palette, palette_len) = if cb.paletted() {
        palette_of(
            d.plte
                .chunks_exact(3)
                .map(|c| [at(c, 0), at(c, 1), at(c, 2)]),
            d.trns,
        )
    } else {
        (Vec::new(), 0)
    };
    let _palette_held = limits
        .mem()
        .charge((palette_len * std::mem::size_of::<Color>()) as u64)?;
    let model = match cb {
        Cb::G1 | Cb::G2 | Cb::G4 | Cb::G8 => ColorModel::Gray,
        Cb::Ga8 | Cb::Tca8 => ColorModel::Nrgba,
        Cb::Tc8 => ColorModel::Rgba,
        Cb::P1 | Cb::P2 | Cb::P4 | Cb::P8 => {
            ColorModel::Paletted(palette.into_iter().take(palette_len).collect())
        }
        Cb::G16 => ColorModel::Gray16,
        Cb::Ga16 | Cb::Tca16 => ColorModel::Nrgba64,
        Cb::Tc16 => ColorModel::Rgba64,
    };
    Ok(Config {
        model,
        width: d.width,
        height: d.height,
    })
}
