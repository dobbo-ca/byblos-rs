//! Wave 1d: port of byblos `build.go`: `BuildPDF`, `BuildWriter` and `TranscodePNG`, the
//! img2pdf replacement. [`EncodedImage`], [`ColorSpace`] and [`DecodeParms`], which Go
//! aliases here from `internal/pdfdoc`, are in [`crate::encoded`].
//!
//! Go's `BuildPDF` writes to an `io.Writer`; [`build_pdf`] returns the bytes. Go's
//! `Limits{}` and context variants collapse into one function taking `&Limits` and the cancel
//! flag. A page's image is copied into the writer's page type (Go copies the slice header);
//! that copy and the output are charged to `limits.max_alloc` before they are made.

use crate::embed::Resolution;
use crate::encoded::EncodedImage;
use crate::error::{Error, Result};
use crate::go::fmt::format_g;
use crate::limits::{Limits, check, check_cancel};
use crate::pdfbuild::{self, Page, malformed, wrap};
use std::borrow::Cow;
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;

/// `BuildPage`: one page of a document built from images: one encoded raster and the page box
/// to paint it on.
///
/// `width_pt` and `height_pt` are the MediaBox in points, PDF default user space (origin
/// lower-left, y increasing upward). Both zero means derive the box from the image's pixel
/// dimensions and DPI, which is what a scan wants.
///
/// `dpi` is the raster's resolution, used only when the box is not given. Zero DPI with no box
/// is an error, not a default: guessing 300 on a 600 DPI plate makes a page twice its true
/// size and nothing downstream can tell.
///
/// Placement within the box is fit-centered (contain): the image is scaled uniformly to fit
/// inside the box and centred there. A box derived from DPI (`width_pt` and `height_pt` zero)
/// is filled instead: with `dpi_y != dpi` the page has the pixels' own declared aspect, and
/// img2pdf paints it edge to edge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BuildPage {
    pub image: EncodedImage,
    pub width_pt: f64,
    pub height_pt: f64,
    pub dpi: f64,
    /// The vertical resolution; zero means the same as `dpi`.
    pub dpi_y: f64,
    /// The page's `/Rotate` in degrees: 0 (none written), 90, 180 or 270, anything else
    /// refused. It turns the page on display and leaves the MediaBox and the placement as they
    /// are, which is what img2pdf writes for an EXIF orientation (see
    /// [`crate::embed::exif_rotation`]).
    pub rotate: i64,
}

/// Bytes of `p`'s image that the output carries: the data, the palette, and the `/SMask`'s.
fn image_bytes(img: &EncodedImage) -> u64 {
    let own = (img.data.len() as u64).saturating_add(img.color_space.lookup.len() as u64);
    match &img.smask {
        Some(sm) => own.saturating_add(image_bytes(sm)),
        None => own,
    }
}

/// `pageBox`: resolves `p`'s MediaBox: the explicit `width_pt` and `height_pt` if given, else
/// derived from the image's pixel dimensions and DPI (points = pixels * 72 / DPI, not pixels *
/// 25.4, which would produce a page in millimetres).
fn page_box(p: &BuildPage) -> Result<(f64, f64)> {
    let (w, h) = (p.width_pt, p.height_pt);
    if w == 0.0 && h == 0.0 {
        let dy = if p.dpi_y == 0.0 { p.dpi } else { p.dpi_y };
        for d in [p.dpi, dy] {
            if d <= 0.0 || d.is_nan() || d.is_infinite() {
                return Err(malformed(format!(
                    "no page box given and DPI {} is not positive",
                    format_g(d)
                )));
            }
        }
        return Ok((
            p.image.width as f64 * 72.0 / p.dpi,
            p.image.height as f64 * 72.0 / dy,
        ));
    }
    if w.is_nan() || h.is_nan() || w.is_infinite() || h.is_infinite() || w <= 0.0 || h <= 0.0 {
        return Err(malformed(format!(
            "page box {}x{} is not a positive finite size",
            format_g(w),
            format_g(h)
        )));
    }
    Ok((w, h))
}

/// The writer's page for `p` at box `(pw, ph)`, borrowing the image.
fn to_page(p: &BuildPage, pw: f64, ph: f64) -> Page<'_> {
    Page {
        image: Cow::Borrowed(&p.image),
        width_pt: pw,
        height_pt: ph,
        rotate: p.rotate,
        fill: p.width_pt == 0.0 && p.height_pt == 0.0,
    }
}

/// `BuildPDF`, `BuildPDFContext` and `BuildPDFLimits`: a PDF whose page `i` paints
/// `pages[i].image` and nothing else.
///
/// It supports `FlateDecode` (up to 16 bits per component), `DCTDecode` (`DeviceGray`,
/// `DeviceRGB`, `DeviceCMYK`), `JPXDecode` (a JP2 file, see [`crate::embed::embed_jp2`]) and
/// `JBIG2Decode` (carried verbatim); any other filter, or an unsupported BPC/colour-space
/// combination for one of those, is rejected rather than written as a file no reader can open.
/// An image over `limits.max_encoded_image_pixels` (width x height, and each axis) is refused
/// as `limit encoded-image-pixels`, before anything is written.
///
/// `cancel` is checked on entry and at each page, but the write is one pass over the pages'
/// bytes and is not interrupted: the same cancellation latency as Go (whole call). A cancelled
/// call returns no bytes.
///
/// # Errors
/// [`Error::Malformed`] for no pages, a page box that is not positive and finite, and any
/// image the writer refuses; [`Error::Limit`], [`Error::Alloc`], [`Error::Cancelled`] and
/// the [`Error::Pdf`] of a zlib error in an image's data.
pub fn build_pdf(pages: &[BuildPage], limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if pages.is_empty() {
        return Err(malformed("byblos: BuildPDF: no pages"));
    }
    let mut built: Vec<Page<'_>> = Vec::new();
    built.try_reserve_exact(pages.len())?;
    for (i, p) in pages.iter().enumerate() {
        check_cancel(cancel)?;
        let (pw, ph) =
            page_box(p).map_err(|e| wrap(e, &format!("byblos: BuildPDF: page {}", i + 1)))?;
        built.push(to_page(p, pw, ph));
    }

    // The output (about the image bytes plus the page objects) is the only copy. It is
    // charged after the page boxes are checked, so a bad box is reported first, as in Go.
    let mem = limits.mem();
    let mut total = 0u64;
    for p in pages {
        total = total.saturating_add(image_bytes(&p.image));
    }
    let overhead = (pages.len() as u64)
        .saturating_mul(4096)
        .saturating_add(4096);
    let out_estimate = total.saturating_add(overhead);
    let _out = mem.charge(out_estimate)?;

    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(usize::try_from(out_estimate).map_err(|_| Error::OutOfMemory)?)?;
    pdfbuild::write(&mut out, &built, limits, cancel).map_err(|e| wrap(e, "byblos: BuildPDF"))?;
    Ok(out)
}

/// `TranscodePNG`: turns a PNG that `embed_png` cannot carry (alpha, tRNS, Adam7, 16-bit)
/// into an [`EncodedImage`] for [`build_pdf`]: Flate colour at the PNG's depth with a Flate
/// `/SMask` for alpha or tRNS, read from `r` once and a row at a time. It also returns the
/// resolution the `pHYs` chunk declares, rounded per axis as img2pdf rounds it (zero if
/// none). See [`pdfbuild::transcode_png`] for the rules.
///
/// `limits.max_encoded_image_pixels` is checked against the IHDR before anything is allocated,
/// and the row buffers and the compressed output are charged to `limits.max_alloc`. Go's
/// `scratch` callback is dropped: the half image an interlaced file needs is charged to the
/// same ledger. The file is read on to `IEND` with every chunk's CRC checked, and inflated
/// data past the last row is refused, as `image/png` does. `cancel` is checked between rows.
///
/// # Errors
/// As [`pdfbuild::transcode_png`], and [`Error::Malformed`] for a `pHYs` resolution with one
/// zero axis.
pub fn transcode_png(
    r: impl Read,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(EncodedImage, Resolution)> {
    let (img, x, y) =
        pdfbuild::transcode_png(r, limits, cancel).map_err(|e| wrap(e, "byblos: TranscodePNG"))?;
    let res = Resolution { x, y };
    res.check().map_err(|e| wrap(e, "byblos: TranscodePNG"))?;
    Ok((img, res))
}

/// `BuildWriter`: writes a PDF one page at a time ([`build_pdf`] holds every page's encoded
/// image at once). [`BuildWriter::add_page`] puts a page on the writer as it is added and
/// keeps no more than its object numbers; [`BuildWriter::close`] writes the page tree and the
/// xref. The page being added is the caller's, in memory for the call: `transcode_png`'s
/// output, say.
pub struct BuildWriter<W: Write> {
    b: pdfbuild::Builder<W>,
    lim: Limits,
}

impl<W: Write> BuildWriter<W> {
    /// `NewBuildWriter`: starts a PDF on `w`. `limits.max_pages` (default 10 000) caps the
    /// pages, and `limits.max_encoded_image_pixels` each image, as for [`build_pdf`].
    pub fn new(w: W, limits: &Limits) -> Self {
        BuildWriter {
            b: pdfbuild::Builder::new(w, limits),
            lim: *limits,
        }
    }

    /// `AddPage`: writes one page, with the same page-box, image and rotation rules as
    /// [`build_pdf`]. A page it refuses (including on a set `cancel` or past the page cap)
    /// writes nothing and leaves the writer usable. `cancel` is checked on entry; the write
    /// itself is one pass over the page's bytes and cannot be cancelled.
    ///
    /// # Errors
    /// As [`build_pdf`]; [`Error::Limit`] named `pages` past `limits.max_pages`.
    pub fn add_page(&mut self, p: &BuildPage, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        let next = self.b.pages() as i64 + 1;
        check("pages", next, self.lim.pages()).map_err(|e| wrap(e, "byblos: BuildWriter"))?;
        let (pw, ph) =
            page_box(p).map_err(|e| wrap(e, &format!("byblos: BuildWriter: page {next}")))?;
        self.b
            .add_page(&to_page(p, pw, ph), cancel)
            .map_err(|e| wrap(e, "byblos: BuildWriter"))
    }

    /// `Close`: writes the page tree and the xref, and returns the writer. A document with no
    /// page is an error.
    ///
    /// # Errors
    /// [`Error::Malformed`] for no pages, or the writer's first write error.
    pub fn close(mut self) -> Result<W> {
        self.b.close().map_err(|e| wrap(e, "byblos: BuildWriter"))?;
        Ok(self.b.into_inner())
    }
}

#[cfg(test)]
mod tests;
