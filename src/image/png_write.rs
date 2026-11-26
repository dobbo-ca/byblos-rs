//! Wave 1c: Go's `image/png` encoder for `*image.Paletted` at `png.BestCompression`
//! (`writer.go`: `Encoder.Encode`, `writeIHDR`, `writePLTEAndTRNS`, `writeImage`, `writeIDATs`,
//! `encoder.Write`, `writeChunk`), for `quantize.go` and `quantize_indexed.go`.
//!
//! What the paletted path does, as in Go: the bit depth follows the palette length (1, 2, 4 or 8
//! bits for at most 2, 4, 16 or more colours); `PLTE` carries the colours converted with
//! `color.NRGBAModel`; `tRNS` is written up to the last entry whose alpha is not `0xff`; no filter
//! is applied (`writer.go:528`, so every scanline is the filter byte 0 and the packed row); the
//! zlib stream is fed one scanline per `Write`; and it goes through a 32 KiB `bufio.Writer`
//! whose flushes are the `IDAT` chunks. The chunk boundaries are therefore those of Go's
//! `bufio.Writer.Write`, which [`Idat::write`] reproduces call by call (including the direct
//! write of a call larger than the buffer), over the sink calls of the zlib writer.
//!
//! COMPRESSION LEVEL. `BestCompression` is zlib level 9. The compressor is behind the
//! [`ZlibStream`] trait and the [`NewZlib`] function; [`encode_paletted`] passes [`zlib_level`],
//! pdf-rs's `goflate::ZlibWriter` at the level asked for, which is Go's byte for byte and makes
//! one sink call per Go `Write`, so the IDAT chunk boundaries are Go's too.
//!
//! Gaps against Go, all for inputs Go does not handle without panicking: an empty palette (Go
//! takes a different path and panics), and a `pix` shorter than `rect` needs (Go indexes out of
//! range). Both are [`Error::Malformed`] here.
//!
//! Go BSD licence: see NOTICE.

use crate::error::{Error, Result};
use crate::image::{Color, Paletted};
use crate::limits::Limits;
use pdf::filter::goflate::{Sink, ZlibWriter};
use pdf::{Held, Mem};
use std::sync::atomic::AtomicBool;

/// `zlib.BestCompression`.
const BEST_COMPRESSION: i32 = 9;

/// The size of the `bufio.Writer` that `writeIDATs` makes: one `IDAT` chunk per flush.
const BUF_SIZE: usize = 1 << 15;

/// The PNG file signature (`pngHeader`).
const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// A zlib writer as `compress/zlib.Writer` behaves: every call it makes to its `io.Writer` is
/// one call of the sink, with the same bytes. pdf-rs's `ZlibWriter` is one.
pub trait ZlibStream {
    /// `Write`.
    ///
    /// # Errors
    /// The error of the sink, an allocation refusal, or cancellation.
    fn write(
        &mut self,
        p: &[u8],
        sink: &mut Sink<'_>,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), pdf::Error>;

    /// `Close`: the end of the stream and the Adler-32 trailer.
    ///
    /// # Errors
    /// The error of the sink.
    fn close(&mut self, sink: &mut Sink<'_>) -> std::result::Result<(), pdf::Error>;
}

impl ZlibStream for ZlibWriter {
    fn write(
        &mut self,
        p: &[u8],
        sink: &mut Sink<'_>,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), pdf::Error> {
        ZlibWriter::write(self, p, sink, cancel)
    }

    fn close(&mut self, sink: &mut Sink<'_>) -> std::result::Result<(), pdf::Error> {
        ZlibWriter::close(self, sink)
    }
}

/// `zlib.NewWriterLevel(w, level)`, charged to `mem`.
pub type NewZlib =
    fn(level: i32, mem: &pdf::Mem) -> std::result::Result<Box<dyn ZlibStream>, pdf::Error>;

/// `goflate::ZlibWriter` at `level`.
fn zlib_level(level: i32, mem: &pdf::Mem) -> std::result::Result<Box<dyn ZlibStream>, pdf::Error> {
    Ok(Box::new(ZlibWriter::with_level(mem, level)?))
}

/// `png.Encoder{CompressionLevel: png.BestCompression}.Encode(&buf, img)` for a paletted image:
/// the complete PNG file.
///
/// `limits.max_alloc` caps the output, the scanline buffer and the compressor together. `cancel` is
/// checked by the compressor, once per 64 KiB window.
///
/// # Errors
/// [`Error::Malformed`] for a size of zero or of 2^32 or more, a palette of zero or more than 256
/// colours (Go's `FormatError`), or a `pix` too short for `rect`; [`Error::Alloc`] and
/// [`Error::OutOfMemory`] when a buffer is over `max_alloc` or the allocator refuses;
/// [`Error::Cancelled`].
pub fn encode_paletted(img: &Paletted, limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    encode_paletted_with(img, zlib_level, limits, cancel)
}

/// [`encode_paletted`] over the zlib writer `new_zlib` makes (asked for level 9).
///
/// # Errors
/// As [`encode_paletted`].
pub fn encode_paletted_with(
    img: &Paletted,
    new_zlib: NewZlib,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let mem = limits.mem();
    let (w, h) = (img.rect.dx(), img.rect.dy());
    if w <= 0 || h <= 0 || w >= 1 << 32 || h >= 1 << 32 {
        return Err(format_error(&format!("invalid image size: {w}x{h}")));
    }
    let (wu, hu) = (
        usize::try_from(w).map_err(|_| Error::OutOfMemory)?,
        usize::try_from(h).map_err(|_| Error::OutOfMemory)?,
    );
    let n = img.palette.len();
    if !(1..=256).contains(&n) {
        return Err(format_error(&format!("bad palette length: {n}")));
    }
    let depth: u8 = match n {
        1..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    };

    // The row buffer: the filter byte and the packed row.
    let sz = wu
        .checked_mul(usize::from(depth))
        .and_then(|bits| bits.checked_add(7))
        .and_then(|bits| (bits / 8).checked_add(1))
        .ok_or(Error::OutOfMemory)?;
    let _row_held = mem.charge(sz as u64)?;
    let mut row: Vec<u8> = Vec::new();
    row.try_reserve_exact(sz)?;

    // Go indexes `Pix` row by row and would panic on a short buffer.
    let last_end = (hu - 1)
        .checked_mul(img.stride)
        .and_then(|o| o.checked_add(wu))
        .ok_or(Error::OutOfMemory)?;
    if last_end > img.pix.len() {
        return Err(Error::Malformed(
            "png: paletted pix is shorter than its rectangle".to_string(),
        ));
    }

    let mut idat = Idat::new(&mem)?;
    idat.out.try_reserve(SIGNATURE.len())?;
    idat.out.extend_from_slice(SIGNATURE);

    // writeIHDR.
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(
        &u32::try_from(w)
            .map_err(|_| Error::OutOfMemory)?
            .to_be_bytes(),
    );
    ihdr[4..8].copy_from_slice(
        &u32::try_from(h)
            .map_err(|_| Error::OutOfMemory)?
            .to_be_bytes(),
    );
    ihdr[8] = depth;
    ihdr[9] = 3; // ctPaletted
    idat.chunk(b"IHDR", &ihdr)?;

    // writePLTEAndTRNS.
    let mut plte = Vec::new();
    plte.try_reserve_exact(3 * n)?;
    let mut alpha = Vec::new();
    alpha.try_reserve_exact(n)?;
    let mut last = None;
    for (i, &c) in img.palette.iter().enumerate() {
        let [r, g, b, a] = to_nrgba(c);
        plte.extend_from_slice(&[r, g, b]);
        alpha.push(a);
        if a != 0xff {
            last = Some(i);
        }
    }
    idat.chunk(b"PLTE", &plte)?;
    if let Some(last) = last {
        idat.chunk(b"tRNS", alpha.get(..=last).unwrap_or_default())?;
    }

    // writeImage: one zlib Write per scanline, no filter (the first byte of a row is 0).
    let mut zw = new_zlib(BEST_COMPRESSION, &mem)?;
    for y in 0..hu {
        let off = y.saturating_mul(img.stride);
        let src = img
            .pix
            .get(off..off.saturating_add(wu))
            .ok_or_else(|| Error::Malformed("png: paletted pix is too short".to_string()))?;
        row.clear();
        row.push(0);
        if depth == 8 {
            row.extend_from_slice(src);
        } else {
            pack_row(&mut row, src, usize::from(depth));
        }
        zw.write(&row, &mut |b: &[u8]| idat.write(b), cancel)?;
    }
    zw.close(&mut |b: &[u8]| idat.write(b))?;
    idat.flush()?;

    idat.chunk(b"IEND", &[])?;
    Ok(idat.out)
}

/// Go's `png.FormatError`: an untyped error, class `error` for the oracle.
fn format_error(msg: &str) -> Error {
    Error::Malformed(format!("png: invalid format: {msg}"))
}

/// The `cbP4, cbP2, cbP1` arm of `writeImage`: indices packed high bits first, the last byte
/// padded with zero bits. An index wider than the depth spills into its neighbours, as in Go.
fn pack_row(row: &mut Vec<u8>, src: &[u8], bpp: usize) {
    let per_byte = 8 / bpp;
    let mut a: u8 = 0;
    let mut c = 0;
    for &idx in src {
        a = (a << bpp) | idx;
        c += 1;
        if c == per_byte {
            row.push(a);
            a = 0;
            c = 0;
        }
    }
    if c != 0 {
        while c != per_byte {
            a <<= bpp;
            c += 1;
        }
        row.push(a);
    }
}

/// `color.NRGBAModel.Convert(c).(color.NRGBA)`.
fn to_nrgba(c: Color) -> [u8; 4] {
    if let Color::Nrgba { r, g, b, a } = c {
        return [r, g, b, a];
    }
    let (r, g, b, a) = c.rgba();
    if a == 0xffff {
        return [(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8, 0xff];
    }
    if a == 0 {
        return [0; 4];
    }
    // Color.RGBA is alpha-premultiplied, so undo that.
    let un = |v: u32| (v.wrapping_mul(0xffff) / a) >> 8;
    [un(r) as u8, un(g) as u8, un(b) as u8, (a >> 8) as u8]
}

/// `crc32.NewIEEE` over the chunk name and then the chunk data.
fn crc32(name: &[u8], data: &[u8]) -> u32 {
    crate::go::crc32::update(crate::go::crc32::checksum(name), data)
}

/// `encoder.writeChunk`: length, name, data, CRC, appended to `out`, whose size `held` charges.
fn push_chunk(
    out: &mut Vec<u8>,
    name: &[u8; 4],
    held: &mut Held,
    data: &[u8],
) -> std::result::Result<(), pdf::Error> {
    // Go: `uint32(len(b))` must round-trip, else UnsupportedError.
    let n = u32::try_from(data.len()).map_err(|_| pdf::Error::Alloc {
        got: data.len() as u64,
        max: u64::from(u32::MAX),
    })?;
    held.grow_by((data.len() as u64).saturating_add(12))?;
    out.try_reserve(data.len().saturating_add(12))
        .map_err(|_| pdf::Error::OutOfMemory)?;
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(name, data).to_be_bytes());
    Ok(())
}

/// Go's `bufio.NewWriterSize(e, 1<<15)` over the encoder: the output file so far (`out`), and
/// the pending bytes that become the next `IDAT` chunk.
struct Idat {
    out: Vec<u8>,
    buf: Vec<u8>,
    /// The charges of `out` (grown per chunk) and of `buf`.
    out_held: Held,
    _buf_held: Held,
}

impl Idat {
    fn new(mem: &Mem) -> std::result::Result<Idat, pdf::Error> {
        let buf_held = mem.charge(BUF_SIZE as u64)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(BUF_SIZE)
            .map_err(|_| pdf::Error::OutOfMemory)?;
        Ok(Idat {
            out: Vec::new(),
            buf,
            out_held: mem.charge(0)?,
            _buf_held: buf_held,
        })
    }

    /// Appends a chunk to the output.
    fn chunk(&mut self, name: &[u8; 4], data: &[u8]) -> std::result::Result<(), pdf::Error> {
        push_chunk(&mut self.out, name, &mut self.out_held, data)
    }

    /// `bufio.Writer.Write`: a call that does not fit fills the buffer and flushes it; when the
    /// buffer is empty the call is passed on whole.
    fn write(&mut self, mut p: &[u8]) -> std::result::Result<(), pdf::Error> {
        while p.len() > BUF_SIZE - self.buf.len() {
            if self.buf.is_empty() {
                self.chunk(b"IDAT", p)?;
                p = &[];
            } else {
                let (head, tail) = p.split_at(BUF_SIZE - self.buf.len());
                self.buf.extend_from_slice(head);
                p = tail;
                self.flush()?;
            }
        }
        self.buf.extend_from_slice(p);
        Ok(())
    }

    /// `bufio.Writer.Flush`: nothing is written for an empty buffer.
    fn flush(&mut self) -> std::result::Result<(), pdf::Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        push_chunk(&mut self.out, b"IDAT", &mut self.out_held, &self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Rect;

    /// A zlib stand-in that passes each write through, so the container can be checked exactly.
    struct Raw;

    impl ZlibStream for Raw {
        fn write(
            &mut self,
            p: &[u8],
            sink: &mut Sink<'_>,
            _cancel: &AtomicBool,
        ) -> std::result::Result<(), pdf::Error> {
            sink(p)
        }

        fn close(&mut self, sink: &mut Sink<'_>) -> std::result::Result<(), pdf::Error> {
            sink(&[])
        }
    }

    fn raw(_level: i32, _mem: &pdf::Mem) -> std::result::Result<Box<dyn ZlibStream>, pdf::Error> {
        Ok(Box::new(Raw))
    }

    /// The chunks after the signature, with their CRCs checked.
    fn chunks(mut p: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut v = Vec::new();
        while !p.is_empty() {
            let n = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) as usize;
            let name = p[4..8].to_vec();
            let data = p[8..8 + n].to_vec();
            let crc = u32::from_be_bytes([p[8 + n], p[9 + n], p[10 + n], p[11 + n]]);
            assert_eq!(crc, crc32(&name, &data));
            v.push((String::from_utf8(name).unwrap(), data));
            p = &p[12 + n..];
        }
        v
    }

    fn opaque(r: u8, g: u8, b: u8) -> Color {
        Color::Nrgba { r, g, b, a: 255 }
    }

    fn encode(img: &Paletted) -> Result<Vec<u8>> {
        encode_paletted_with(img, raw, &Limits::default(), &AtomicBool::new(false))
    }

    #[test]
    fn crc32_known_values() {
        assert_eq!(crc32(b"IEND", &[]), 0xAE42_6082);
        let ihdr = [0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
        assert_eq!(crc32(b"IHDR", &ihdr), 0x1F15_C489);
    }

    #[test]
    fn bilevel_rows_are_packed_high_bit_first() {
        let pal = vec![opaque(0, 0, 0), opaque(255, 255, 255)];
        let mut img = Paletted::new(Rect::new(0, 0, 3, 2), pal, &Limits::default()).unwrap();
        img.pix.copy_from_slice(&[0, 1, 1, 1, 0, 1]);
        let png = encode(&img).unwrap();
        assert_eq!(&png[..8], SIGNATURE);
        let c = chunks(&png[8..]);
        let names: Vec<&str> = c.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["IHDR", "PLTE", "IDAT", "IEND"]);
        assert_eq!(c[0].1, [0, 0, 0, 3, 0, 0, 0, 2, 1, 3, 0, 0, 0]);
        assert_eq!(c[1].1, [0, 0, 0, 255, 255, 255]);
        // Each row: filter byte 0, then 011 and 101 padded to a byte.
        assert_eq!(c[2].1, [0, 0x60, 0, 0xA0]);
        assert!(c[3].1.is_empty());
    }

    #[test]
    fn trns_stops_at_the_last_translucent_entry() {
        let pal = vec![
            Color::Nrgba {
                r: 1,
                g: 2,
                b: 3,
                a: 0,
            },
            opaque(4, 5, 6),
            Color::Nrgba {
                r: 7,
                g: 8,
                b: 9,
                a: 128,
            },
            opaque(10, 11, 12),
        ];
        let img = Paletted::new(Rect::new(0, 0, 1, 1), pal, &Limits::default()).unwrap();
        let c = chunks(&encode(&img).unwrap()[8..]);
        let names: Vec<&str> = c.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["IHDR", "PLTE", "tRNS", "IDAT", "IEND"]);
        assert_eq!(c[0].1[8], 2); // four colours: two bits
        assert_eq!(c[2].1, [0, 255, 128]);
    }

    #[test]
    fn premultiplied_palette_entries_are_unpremultiplied() {
        assert_eq!(
            to_nrgba(Color::Rgba {
                r: 64,
                g: 0,
                b: 0,
                a: 128
            }),
            [127, 0, 0, 128]
        );
        assert_eq!(to_nrgba(Color::Gray(0x80)), [0x80, 0x80, 0x80, 0xff]);
    }

    #[test]
    fn idat_is_cut_at_the_bufio_size() {
        let pal = vec![opaque(0, 0, 0); 17];
        let img = Paletted::new(Rect::new(0, 0, 200, 300), pal, &Limits::default()).unwrap();
        let c = chunks(&encode(&img).unwrap()[8..]);
        let sizes: Vec<usize> = c
            .iter()
            .filter(|(n, _)| n == "IDAT")
            .map(|(_, d)| d.len())
            .collect();
        // 300 rows of 201 bytes: one full buffer, then the rest.
        assert_eq!(sizes, [32768, 300 * 201 - 32768]);
        assert_eq!(c[0].1[8], 8);
    }

    #[test]
    fn bufio_passes_a_large_write_through() {
        let mut idat = Idat::new(&Mem::with_max(u64::MAX)).unwrap();
        idat.write(&[1; 10]).unwrap();
        idat.write(&[2; 40000]).unwrap();
        idat.write(&[3; 70000]).unwrap();
        idat.flush().unwrap();
        let sizes: Vec<usize> = chunks(&idat.out).iter().map(|(_, d)| d.len()).collect();
        // 10 + 40000 fills one buffer and leaves 7242; 70000 tops that up (25526), leaving
        // 44474, which is over the buffer and goes out whole.
        assert_eq!(sizes, [32768, 32768, 44474]);
        assert!(idat.buf.is_empty());
    }

    #[test]
    fn bad_images_are_format_errors() {
        let pal = vec![opaque(0, 0, 0)];
        let empty = Paletted::new(Rect::new(0, 0, 0, 5), pal, &Limits::default()).unwrap();
        assert!(matches!(encode(&empty), Err(Error::Malformed(_))));
        let no_palette =
            Paletted::new(Rect::new(0, 0, 2, 2), Vec::new(), &Limits::default()).unwrap();
        assert!(matches!(encode(&no_palette), Err(Error::Malformed(_))));
        let big_palette = Paletted::new(
            Rect::new(0, 0, 2, 2),
            vec![opaque(0, 0, 0); 257],
            &Limits::default(),
        )
        .unwrap();
        assert!(matches!(encode(&big_palette), Err(Error::Malformed(_))));
    }

    #[test]
    fn real_zlib_writer_produces_a_stream() {
        // Level 9: the header's FLEVEL is 3.
        let pal = vec![opaque(0, 0, 0), opaque(255, 255, 255)];
        let mut img = Paletted::new(Rect::new(0, 0, 8, 8), pal, &Limits::default()).unwrap();
        img.pix[9] = 1;
        let png = encode_paletted(&img, &Limits::default(), &AtomicBool::new(false)).unwrap();
        let c = chunks(&png[8..]);
        let idat: Vec<u8> = c
            .iter()
            .filter(|(n, _)| n == "IDAT")
            .flat_map(|(_, d)| d.clone())
            .collect();
        assert_eq!(&idat[..2], [0x78, 0xda]);
    }
}
