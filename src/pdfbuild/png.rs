//! Port of byblos `internal/pdfbuild/png.go`: the PNG transcode (kleio in-process pipeline
//! spec, row 9). A PNG that `embed_png` cannot carry verbatim (alpha, tRNS, Adam7, 16-bit)
//! becomes Flate colour plus a Flate `/SMask`, a row at a time. The file is read once, front to
//! back; only a few rows are held, plus (for Adam7) the even rows the first six passes fill,
//! and the two compressed outputs.
//!
//! Go compresses both outputs with `zlib.BestSpeed` (level 1); `new_zlib_writer` makes pdf-rs's
//! Go-exact level 1 writer, with one sink call per Go `Write`.
//!
//! Go's `scratch` callback is dropped (docs/go-differences.md): every buffer is charged to the
//! call's `max_alloc` ledger.

use super::pngres::PngPhys;
use super::{
    deflate_limits, flate, io_err, is_unexpected_eof, malformed, read_err, read_full,
    unexpected_eof, unfilter_png_row, wrap, zeroed,
};
use crate::encoded::{ColorSpace, EncodedImage};
use crate::error::{Error, Result};
use crate::limits::{Limits, check, check_cancel, mul_sat};
use pdf::filter::goflate::ZlibWriter;
use pdf::{Held, Mem};
use std::cell::RefCell;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::atomic::AtomicBool;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// `adam7`: each pass's x0, y0, dx, dy. Passes 1 to 6 hold exactly the even rows of the image
/// and pass 7 the odd ones, whole rows.
const ADAM7: [[usize; 4]; 7] = [
    [0, 0, 8, 8],
    [4, 0, 8, 8],
    [0, 4, 4, 8],
    [2, 0, 4, 4],
    [0, 2, 2, 4],
    [1, 0, 2, 2],
    [0, 1, 1, 2],
];

/// The zlib writer for the two output streams: Go's `zlib.BestSpeed`.
fn new_zlib_writer(mem: &Mem) -> Result<ZlibWriter> {
    ZlibWriter::with_level(mem, 1).map_err(Error::from)
}

fn oob() -> Error {
    malformed("png row buffer out of range")
}

fn to_usize(v: i64) -> Result<usize> {
    usize::try_from(v).map_err(|_| Error::OutOfMemory)
}

// --- CRC-32 ---------------------------------------------------------------------------------

use crate::go::crc32::update as crc32_update;

// --- chunk reader ---------------------------------------------------------------------------

/// `chunkReader`: reads PNG chunks and checks each one's CRC as its last byte is read.
struct ChunkReader<R: Read> {
    br: BufReader<R>,
    crc: u32,
    /// Body bytes left in the current chunk.
    n: i64,
    typ: [u8; 4],
    /// A CRC mismatch seen while reading a body: the reader above may never read on to see it;
    /// only a chunk before the first IDAT is held to it.
    bad: Option<Error>,
}

impl<R: Read> ChunkReader<R> {
    fn new(r: R) -> Self {
        ChunkReader {
            br: BufReader::with_capacity(32 << 10, r),
            crc: 0,
            n: 0,
            typ: [0; 4],
            bad: None,
        }
    }

    fn typ_str(&self) -> String {
        String::from_utf8_lossy(&self.typ).into_owned()
    }

    fn next(&mut self) -> Result<()> {
        let mut h = [0u8; 8];
        self.br
            .read_exact(&mut h)
            .map_err(|e| wrap(io_err(&e), "chunk header"))?;
        let [l0, l1, l2, l3, t0, t1, t2, t3] = h;
        let n = u32::from_be_bytes([l0, l1, l2, l3]);
        if n > i32::MAX as u32 {
            return Err(malformed(format!("chunk length {n}")));
        }
        self.n = i64::from(n);
        self.typ = [t0, t1, t2, t3];
        self.crc = crc32_update(0, &self.typ);
        if self.n == 0 {
            return self.check_crc();
        }
        Ok(())
    }

    fn check_crc(&mut self) -> Result<()> {
        let mut b = [0u8; 4];
        self.br
            .read_exact(&mut b)
            .map_err(|e| wrap(io_err(&e), &format!("{} chunk", self.typ_str())))?;
        if u32::from_be_bytes(b) != self.crc {
            return Err(malformed(format!("{} chunk: bad CRC", self.typ_str())));
        }
        Ok(())
    }

    /// `Read`: `Ok(0)` is `io.EOF`, the end of the chunk body.
    fn read(&mut self, p: &mut [u8]) -> Result<usize> {
        if self.n == 0 {
            return Ok(0);
        }
        let max = usize::try_from(self.n).unwrap_or(usize::MAX).min(p.len());
        let Some(dst) = p.get_mut(..max) else {
            return Ok(0);
        };
        let m = self.br.read(dst).map_err(|e| io_err(&e))?;
        if m == 0 && max > 0 {
            return Err(unexpected_eof());
        }
        self.crc = crc32_update(self.crc, dst.get(..m).unwrap_or_default());
        self.n -= m as i64;
        if self.n == 0 && self.bad.is_none() {
            self.bad = self.check_crc().err();
        }
        Ok(m)
    }

    /// `io.ReadFull(c, buf)`.
    fn read_full(&mut self, buf: &mut [u8]) -> Result<()> {
        let mut filled = 0usize;
        while filled < buf.len() {
            let Some(dst) = buf.get_mut(filled..) else {
                break;
            };
            let k = self.read(dst)?;
            if k == 0 {
                return Err(if filled == 0 {
                    malformed("EOF")
                } else {
                    unexpected_eof()
                });
            }
            filled += k;
        }
        Ok(())
    }

    /// `io.Copy(io.Discard, c)`.
    fn discard(&mut self) -> Result<()> {
        let mut buf = [0u8; 8192];
        while self.read(&mut buf)? != 0 {}
        Ok(())
    }

    /// `body`: reads the rest of a chunk that must be at most `max` bytes.
    fn body(&mut self, max: i64) -> Result<Vec<u8>> {
        if self.n > max {
            return Err(malformed(format!(
                "{} chunk of {} bytes",
                self.typ_str(),
                self.n
            )));
        }
        let mut b = zeroed(to_usize(self.n)?)?;
        self.read_full(&mut b)?;
        Ok(b)
    }
}

/// `idat`: the concatenated body of the consecutive IDAT chunks, as the input of the zlib
/// reader. An error of the chunk reader is kept in `side` (the reader above only sees an
/// `io::Error` kind), so that its class survives.
struct Idat<'a, R: Read> {
    c: ChunkReader<R>,
    side: &'a RefCell<Option<Error>>,
}

impl<R: Read> Idat<'_, R> {
    fn fail(&self, e: Error) -> io::Error {
        let mut s = self.side.borrow_mut();
        if s.is_none() {
            *s = Some(e);
        }
        io::Error::other("png: IDAT")
    }
}

impl<R: Read> BufRead for Idat<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        // An error is final: a speculative caller must not get to see the next chunk's body.
        if self.side.borrow().is_some() {
            return Err(io::Error::other("png: IDAT"));
        }
        while self.c.n == 0 {
            if let Err(e) = self.c.next() {
                return Err(self.fail(e));
            }
            if self.c.typ != *b"IDAT" {
                return Err(self.fail(unexpected_eof()));
            }
        }
        let left = usize::try_from(self.c.n).unwrap_or(usize::MAX);
        let avail = match self.c.br.fill_buf() {
            Ok(b) => b.len().min(left),
            Err(e) => return Err(self.fail(io_err(&e))),
        };
        if avail == 0 {
            return Err(self.fail(unexpected_eof()));
        }
        Ok(self.c.br.buffer().get(..avail).unwrap_or_default())
    }

    fn consume(&mut self, amt: usize) {
        let amt = amt.min(usize::try_from(self.c.n).unwrap_or(usize::MAX));
        if let Some(b) = self.c.br.buffer().get(..amt) {
            self.c.crc = crc32_update(self.c.crc, b);
        }
        self.c.br.consume(amt);
        self.c.n -= amt as i64;
        if self.c.n == 0 && self.c.bad.is_none() {
            self.c.bad = self.c.check_crc().err();
        }
    }
}

impl<R: Read> Read for Idat<'_, R> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if p.is_empty() {
            return Ok(0);
        }
        let n = {
            let b = self.fill_buf()?;
            let n = b.len().min(p.len());
            if let (Some(d), Some(s)) = (p.get_mut(..n), b.get(..n)) {
                d.copy_from_slice(s);
            }
            n
        };
        self.consume(n);
        Ok(n)
    }
}

// --- output ---------------------------------------------------------------------------------

/// `sink`: collects a compressed stream, reserving its memory from the budget as it grows
/// (twice what is held, since `Vec` doubles).
struct OutBuf {
    mem: Mem,
    buf: Vec<u8>,
    held: u64,
    rels: Vec<Held>,
}

impl OutBuf {
    fn new(mem: &Mem) -> Self {
        OutBuf {
            mem: mem.clone(),
            buf: Vec::new(),
            held: 0,
            rels: Vec::new(),
        }
    }

    fn write(&mut self, p: &[u8]) -> std::result::Result<(), pdf::Error> {
        let need = 2u64.saturating_mul((self.buf.len() + p.len()) as u64);
        if need > self.held {
            let extra = (need - self.held).max(1 << 20);
            self.rels.push(self.mem.charge(extra)?);
            self.held += extra;
        }
        self.buf.try_reserve(p.len())?;
        self.buf.extend_from_slice(p);
        Ok(())
    }
}

/// `sampleAt`: the i-th sample of depth 1, 2, 4, 8 or 16 bits in a packed row.
fn sample_at(row: &[u8], i: usize, depth: usize) -> u16 {
    match depth {
        16 => match row.get(2 * i..2 * i + 2) {
            Some(&[a, b]) => u16::from_be_bytes([a, b]),
            _ => 0,
        },
        8 => row.get(i).copied().map_or(0, u16::from),
        _ => {
            let bit = i * depth;
            let Some(&b) = row.get(bit / 8) else {
                return 0;
            };
            let shift = 8 - depth - bit % 8;
            u16::from(b >> shift) & ((1 << depth) - 1)
        }
    }
}

/// `putSample`: stores `v` as the i-th sample of depth 1, 2 or 4 bits.
fn put_sample(row: &mut [u8], i: usize, depth: usize, v: u16) {
    let bit = i * depth;
    let shift = 8 - depth - bit % 8;
    let mask = ((1u16 << depth) - 1) as u8;
    if let Some(b) = row.get_mut(bit / 8) {
        *b = (*b & !(mask << shift)) | ((v as u8) << shift);
    }
}

/// Splits finished rows into colour and mask and compresses both (Go's `emit`).
struct Emitter<'a> {
    ctype: i64,
    depth: usize,
    channels: usize,
    w: usize,
    /// Palette files: the alpha of each palette entry (tRNS).
    palette_alpha: Option<Vec<u8>>,
    /// Grey and RGB files with a tRNS colour key: pixels equal to `key` are transparent.
    use_key: bool,
    key: [u16; 3],
    has_mask: bool,
    crow: Vec<u8>,
    mrow: Vec<u8>,
    cz: ZlibWriter,
    mz: Option<ZlibWriter>,
    color_out: OutBuf,
    mask_out: OutBuf,
    cancel: &'a AtomicBool,
}

impl Emitter<'_> {
    /// `emit`: splits one finished row into colour and mask and compresses both.
    fn emit(&mut self, row: &[u8]) -> Result<()> {
        let mut from_crow = false;
        match self.ctype {
            4 | 6 => {
                let sz = self.depth / 8;
                let cs = self.channels - 1;
                for ((px, c), m) in row
                    .chunks_exact(self.channels * sz)
                    .zip(self.crow.chunks_exact_mut(cs * sz))
                    .zip(self.mrow.chunks_exact_mut(sz))
                {
                    if let Some((a, b)) = px.split_at_checked(cs * sz) {
                        c.copy_from_slice(a);
                        m.copy_from_slice(b);
                    }
                }
                from_crow = true;
            }
            3 => {
                if let Some(trns) = &self.palette_alpha {
                    for (x, m) in self.mrow.iter_mut().enumerate().take(self.w) {
                        let i = usize::from(sample_at(row, x, self.depth));
                        *m = trns.get(i).copied().unwrap_or(255);
                    }
                }
            }
            _ => {
                if self.use_key {
                    for (x, m) in self.mrow.iter_mut().enumerate().take(self.w) {
                        let mut same = true;
                        for (k, &key) in self.key.iter().enumerate().take(self.channels) {
                            same = same && sample_at(row, x * self.channels + k, self.depth) == key;
                        }
                        *m = if same { 0 } else { 255 };
                    }
                }
            }
        }
        let out: &[u8] = if from_crow { &self.crow } else { row };
        self.cz
            .write(out, &mut |b: &[u8]| self.color_out.write(b), self.cancel)
            .map_err(Error::from)?;
        if self.has_mask
            && let Some(mz) = self.mz.as_mut()
        {
            mz.write(
                &self.mrow,
                &mut |b: &[u8]| self.mask_out.write(b),
                self.cancel,
            )
            .map_err(Error::from)?;
        }
        Ok(())
    }
}

/// The inflated IDAT stream, read one unfiltered row at a time (Go's `readRow` closure).
struct Rows<'a, R: Read> {
    zr: deflate::zlib::Reader<'a, Idat<'a, R>>,
    side: &'a RefCell<Option<Error>>,
    cur: Vec<u8>,
    prev: Vec<u8>,
    bpp: usize,
    cancel: &'a AtomicBool,
}

impl<R: Read> Rows<'_, R> {
    /// Reads and unfilters one row of a pass whose rows are `n` bytes and returns it; `prev` is
    /// the row above in the same pass.
    fn read_row(&mut self, n: usize) -> Result<&[u8]> {
        check_cancel(self.cancel)?;
        let dst = self.cur.get_mut(..=n).ok_or_else(oob)?;
        read_full(&mut self.zr, dst, Some(self.side), true).map_err(|e| wrap(e, "IDAT"))?;
        let (head, body) = self.cur.split_at_mut(1);
        let ft = head.first().copied().unwrap_or(0);
        let row = body.get_mut(..n).ok_or_else(oob)?;
        let above = self.prev.get(1..=n).ok_or_else(oob)?;
        unfilter_png_row(ft, row, above, self.bpp)?;
        std::mem::swap(&mut self.cur, &mut self.prev);
        self.prev.get(1..=n).ok_or_else(oob)
    }

    /// `clear(prev[:n+1])`: the row above the first row of a pass is all zeroes.
    fn clear_prev(&mut self, n: usize) {
        if let Some(p) = self.prev.get_mut(..=n) {
            p.fill(0);
        }
    }
}

/// `TranscodePNG`: reads the PNG on `r` once and returns Flate colour with a Flate `/SMask`
/// for alpha or tRNS (an 8-bit mask, or the PNG's 16 bits for a 16-bit alpha channel), at the
/// PNG's own depth; palette files stay `/Indexed`. The resolution, x then y, is the `pHYs`
/// chunks', per axis, each rounded to a whole number as img2pdf rounds it (0 if none; see
/// [`PngPhys`]).
///
/// Limits are applied from the header before anything is allocated: the pixel cap, then the
/// budget for the row buffers and compressors, which are charged to `limits.max_alloc`. An
/// Adam7 file needs half the image at native sample size for the even rows. The compressed
/// output grows under the same budget while it is built and is released when the call returns:
/// the returned bytes are the caller's to account for.
///
/// The file is read as far as Pillow reads it for img2pdf, and no further: the CRC of every
/// chunk before the first IDAT is checked, but not an IDAT's, and once the last row is out
/// nothing after it is looked at. Inflated data past the last row is ignored (as is a zlib
/// trailer cut short, so a missing IEND, a bad CRC after the image and extra pixel data are
/// all accepted, as img2pdf accepts them); a bad zlib checksum on a stream that ends with the
/// last row is an error. Cancellation is checked between rows.
///
/// # Errors
/// [`Error::Limit`] (`encoded-image-pixels`), [`Error::Alloc`], [`Error::Cancelled`],
/// [`Error::Malformed`] for a file that is not a PNG this transcode accepts, and an
/// [`Error::Pdf`] carrying the zlib error or `unexpected EOF` of a truncated or corrupt file.
pub fn transcode_png<R: Read>(
    r: R,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(EncodedImage, f64, f64)> {
    let mut c = ChunkReader::new(r);
    let mut sig = [0u8; 8];
    if c.br.read_exact(&mut sig).is_err() || sig != PNG_SIGNATURE {
        return Err(malformed("not a PNG file"));
    }
    let (mut w, mut h, mut depth, mut ctype, mut interlace) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut plte: Option<Vec<u8>> = None;
    let mut trns: Option<Vec<u8>> = None;
    let mut phys = PngPhys::default();
    let mut have_ihdr = false;
    while c.typ != *b"IDAT" || c.n == 0 {
        check_cancel(cancel)?;
        c.next()?;
        if !have_ihdr && c.typ != *b"IHDR" {
            return Err(malformed(format!(
                "first chunk is {}, not IHDR",
                c.typ_str()
            )));
        }
        let typ = c.typ;
        match &typ {
            b"IHDR" => {
                if have_ihdr || c.n != 13 {
                    return Err(malformed("bad IHDR chunk"));
                }
                let mut b = [0u8; 13];
                c.read_full(&mut b)?;
                let uw = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                let uh = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
                if uw == 0 || uh == 0 || uw > i32::MAX as u32 || uh > i32::MAX as u32 {
                    return Err(malformed(format!("IHDR declares {uw}x{uh}")));
                }
                w = i64::from(uw);
                h = i64::from(uh);
                depth = i64::from(b[8]);
                ctype = i64::from(b[9]);
                interlace = i64::from(b[12]);
                have_ihdr = true;
            }
            b"PLTE" => {
                let p = c.body(768)?;
                if p.is_empty() || !p.len().is_multiple_of(3) {
                    return Err(malformed(format!("PLTE of {} bytes", p.len())));
                }
                plte = Some(p);
            }
            b"tRNS" => trns = Some(c.body(256)?),
            b"pHYs" => {
                let k = to_usize(c.n.min(9))?;
                let mut b = [0u8; 9];
                if let Some(dst) = b.get_mut(..k) {
                    c.read_full(dst)?;
                }
                phys.add(b.get(..k).unwrap_or_default())?;
                c.discard()?;
            }
            b"IEND" => return Err(malformed("no IDAT chunk")),
            b"IDAT" => {}
            _ => c.discard()?,
        }
        if let Some(e) = &c.bad {
            return Err(e.clone());
        }
    }

    let (dpi_x, dpi_y) = phys.resolution();

    // Header checks, before any allocation.
    let channels: i64 = match ctype {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => 0,
    };
    let ok_depth = match ctype {
        0 | 3 => matches!(depth, 1 | 2 | 4 | 8) || (ctype == 0 && depth == 16),
        _ => depth == 8 || depth == 16,
    };
    if channels == 0 || !ok_depth || interlace > 1 {
        return Err(malformed(format!(
            "colour type {ctype} at {depth} bits, interlace {interlace}, is not a PNG"
        )));
    }
    check(
        "encoded-image-pixels",
        w.max(h),
        limits.encoded_image_pixels(),
    )?;
    check(
        "encoded-image-pixels",
        mul_sat(w, h),
        limits.encoded_image_pixels(),
    )?;
    let has_alpha_channel = ctype == 4 || ctype == 6;
    let trns_len = trns.as_ref().map_or(0, Vec::len);
    if has_alpha_channel && trns.is_some() {
        return Err(malformed("tRNS on a colour type with alpha"));
    }
    if ctype == 3
        && match &plte {
            None => true,
            Some(p) => p.len() / 3 > 1 << depth || trns_len > p.len() / 3,
        }
    {
        return Err(malformed(format!(
            "palette missing or too long for {depth} bits"
        )));
    }
    if (ctype == 0 && trns.is_some() && trns_len != 2)
        || (ctype == 2 && trns.is_some() && trns_len != 6)
    {
        return Err(malformed(format!(
            "tRNS of {trns_len} bytes for colour type {ctype}"
        )));
    }

    let bits_pp = channels * depth;
    let row_bytes = (w * bits_pp + 7) >> 3; // an unpacked row of the image
    let bpp = (bits_pp / 8).max(1);
    let has_mask = has_alpha_channel || trns.is_some();
    // Per pixel.
    let (color_bits, mask_bits) = if has_alpha_channel {
        (bits_pp - depth, depth)
    } else if has_mask {
        (bits_pp, 8)
    } else {
        (bits_pp, 0)
    };
    let color_row = (w * color_bits + 7) >> 3;
    let mask_row = (w * mask_bits + 7) >> 3;
    let scratch_bytes = if interlace == 1 {
        mul_sat((h + 1) / 2, row_bytes)
    } else {
        0
    };
    let held = (2 * (row_bytes + 1) + color_row + mask_row).saturating_add(scratch_bytes);
    let mem = limits.mem();
    let _held = mem.charge(u64::try_from(held).unwrap_or(u64::MAX))?;
    let mut even: Vec<u8> = if scratch_bytes > 0 {
        zeroed(to_usize(scratch_bytes)?)?
    } else {
        Vec::new()
    };

    // Go reserves the writers' memory before it opens the zlib reader.
    let cz = new_zlib_writer(&mem)?;
    let mz = if has_mask {
        Some(new_zlib_writer(&mem)?)
    } else {
        None
    };
    let side: RefCell<Option<Error>> = RefCell::new(None);
    let dl = deflate_limits(&mem);
    let zr = match deflate::zlib::Reader::new(Idat { c, side: &side }, &dl, cancel) {
        Ok(z) => z,
        Err(e) => {
            let own = side.borrow_mut().take();
            return Err(wrap(own.unwrap_or_else(|| flate(e)), "IDAT"));
        }
    };

    let (w_u, h_u) = (to_usize(w)?, to_usize(h)?);
    let row_bytes_u = to_usize(row_bytes)?;
    let depth_u = to_usize(depth)?;
    let channels_u = to_usize(channels)?;
    let bpp_u = to_usize(bpp)?;
    let bits_pp_u = to_usize(bits_pp)?;

    let mut rows = Rows {
        zr,
        side: &side,
        cur: zeroed(row_bytes_u + 1)?,
        prev: zeroed(row_bytes_u + 1)?,
        bpp: bpp_u,
        cancel,
    };
    let mut key = [0u16; 3];
    if let Some(t) = &trns
        && ctype != 3
    {
        for (k, pair) in key.iter_mut().zip(t.chunks_exact(2)) {
            *k = u16::from_be_bytes([pair[0], pair[1]]);
            if depth < 16 {
                *k &= 0xFF;
            }
        }
    }
    let use_key = ctype != 3 && trns.is_some();
    let mut em = Emitter {
        ctype,
        depth: depth_u,
        channels: channels_u,
        w: w_u,
        palette_alpha: if ctype == 3 { trns } else { None },
        use_key,
        key,
        has_mask,
        crow: zeroed(to_usize(color_row)?)?,
        mrow: zeroed(to_usize(mask_row)?)?,
        cz,
        mz,
        color_out: OutBuf::new(&mem),
        mask_out: OutBuf::new(&mem),
        cancel,
    };

    if interlace == 0 {
        for _ in 0..h_u {
            let row = rows.read_row(row_bytes_u)?;
            em.emit(row)?;
        }
    } else {
        for p in ADAM7.iter().take(6) {
            let [p0, p1, p2, p3] = *p;
            let pw = (w_u + p2 - 1).saturating_sub(p0) / p2;
            let ph = (h_u + p3 - 1).saturating_sub(p1) / p3;
            if pw == 0 || ph == 0 {
                continue;
            }
            let n = (pw * bits_pp_u).div_ceil(8);
            rows.clear_prev(n);
            for j in 0..ph {
                let row = rows.read_row(n)?;
                let start = (p1 + j * p3) / 2 * row_bytes_u;
                let dst = even.get_mut(start..start + row_bytes_u).ok_or_else(oob)?;
                for i in 0..pw {
                    let x = p0 + i * p2;
                    if bits_pp_u >= 8 {
                        let d = dst.get_mut(x * bpp_u..(x + 1) * bpp_u);
                        let s = row.get(i * bpp_u..(i + 1) * bpp_u);
                        let (Some(d), Some(s)) = (d, s) else {
                            return Err(oob());
                        };
                        d.copy_from_slice(s);
                    } else {
                        put_sample(dst, x, depth_u, sample_at(row, i, depth_u));
                    }
                }
            }
        }
        rows.clear_prev(row_bytes_u);
        for y in 0..h_u {
            if y.is_multiple_of(2) {
                let start = y / 2 * row_bytes_u;
                let row = even.get(start..start + row_bytes_u).ok_or_else(oob)?;
                em.emit(row)?;
            } else {
                let row = rows.read_row(row_bytes_u)?;
                em.emit(row)?;
            }
        }
    }

    // Past the last row: more inflated data, or a zlib trailer cut short, is ignored, as Pillow
    // ignores it; a stream that ends here must check out.
    let mut one = [0u8; 1];
    if let Err(e) = rows.zr.read(&mut one) {
        let e = read_err(&e, Some(&side));
        if !is_unexpected_eof(&e) {
            return Err(wrap(e, "IDAT"));
        }
    }
    drop(rows);

    em.cz
        .close(&mut |b: &[u8]| em.color_out.write(b))
        .map_err(Error::from)?;
    let color_space = match ctype {
        0 | 4 => ColorSpace {
            name: "DeviceGray".to_string(),
            ..ColorSpace::default()
        },
        2 | 6 => ColorSpace {
            name: "DeviceRGB".to_string(),
            ..ColorSpace::default()
        },
        _ => {
            let lookup = plte.unwrap_or_default();
            ColorSpace {
                name: "Indexed".to_string(),
                base: "DeviceRGB".to_string(),
                hi_val: (lookup.len() / 3) as i64 - 1,
                lookup,
            }
        }
    };
    let mut img = EncodedImage {
        width: w,
        height: h,
        bpc: depth,
        color_space,
        filter: "FlateDecode".to_string(),
        data: std::mem::take(&mut em.color_out.buf),
        ..EncodedImage::default()
    };
    if let Some(mz) = em.mz.as_mut() {
        mz.close(&mut |b: &[u8]| em.mask_out.write(b))
            .map_err(Error::from)?;
        let bpc = if has_alpha_channel { depth } else { 8 };
        img.smask = Some(Box::new(EncodedImage {
            width: w,
            height: h,
            bpc,
            color_space: ColorSpace {
                name: "DeviceGray".to_string(),
                ..ColorSpace::default()
            },
            filter: "FlateDecode".to_string(),
            data: std::mem::take(&mut em.mask_out.buf),
            ..EncodedImage::default()
        }));
    }
    Ok((img, dpi_x, dpi_y))
}
