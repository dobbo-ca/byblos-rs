//! Wave 3: port of byblos `render.go` (`RenderPage`, which wires pdfdoc into render::page).
//!
//! [`render_page`] rasterises one page to an RGBA image whose long edge is `long_edge_px`
//! pixels. It is thumbnail fidelity: recognisable rather than faithful. There is no colour
//! management, no transparency group, no shading and no JBIG2 or JPX image XObject; a page
//! element this stage cannot draw is skipped and the rest of the page still renders. Do not
//! store the result as a rendition of the page; it is for looking at.
//!
//! This is NOT the `render` capability, and shipping it does not claim one (see Go's
//! `RenderPage` for why): `upgrade`'s capability rules are untouched.
//!
//! Go's `RenderPage`, `RenderPageContext` and `RenderPageLimits` are this one function: it takes
//! `&Limits` and a cancel flag. Go reads the document from an `io.ReadSeeker`; this takes the
//! bytes. The cancel flag is checked before the document is parsed and by the renderer as it
//! walks the content stream.
//!
//! # Differences from Go
//!
//! * `page` is a `usize`: Go's negative page numbers are refused by the document as out of
//!   range, and a `usize` cannot express them.
//! * Go wraps each error with `byblos: RenderPage: ` (and the page number). Here a typed error
//!   (limit, alloc, cancel, ...) is returned as it is, so its class survives; only the two
//!   errors that Go builds in this file carry the full Go text.
//! * Go's `reserveDCT` also reserved the decode's memory from the process pool. Here the real
//!   allocations are charged to `max_alloc` (docs/go-differences.md), so only its refusals stay:
//!   a JPEG whose frame is over the pixel cap or disagrees with the image dictionary is skipped.
//! * `%g` of the crop box sides is `format_g_exp`: shortest digits, exponent form below 1e-4 and from 1e6.

use crate::content::BBox;
use crate::dct;
use crate::error::{Error, Result};
use crate::go::fmt::format_g;
use crate::image::{self, Image, Rgba};
use crate::limits::{Limits, check_cancel, mul_sat};
use crate::pdfdoc::{self, Doc, DocEnv};
use crate::render::page::{self as render, PageParams, RenderImage};
use crate::render::text::Font;
use std::sync::atomic::AtomicBool;

/// `RenderPage`, `RenderPageContext` and `RenderPageLimits`: rasterises page `page` (1-based) of
/// the PDF `data` to an RGBA image whose long edge is `long_edge_px` pixels.
///
/// The page box is the CROP box, not the media box: it is what a viewer shows. `/Rotate` turns
/// the raster but does not change which edge is longer, so the scale is taken from the unrotated
/// box. A canvas over `limits.max_raster_side` or `max_raster_pixels` is refused from the page
/// box before it is allocated (`raster-side`, `raster-pixels`); a path, a fill or a `q` stack
/// past `max_path_points`, `max_fill_work` or `max_gstate_depth` is `path-points`, `fill-work`
/// or `q-depth`. Fonts past `max_render_font_bytes` are skipped, not refused.
///
/// # Errors
/// [`Error::Image`] for a non-positive `long_edge_px` and for a degenerate crop box;
/// [`Error::Cancelled`]; the errors of [`pdfdoc::open`] and [`Doc::page`]; and those of
/// [`render::page`]. An image or font that cannot be decoded is skipped, not an error.
pub fn render_page(
    data: &[u8],
    page: usize,
    long_edge_px: i64,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Rgba> {
    if long_edge_px <= 0 {
        return Err(Error::Image(format!(
            "byblos: RenderPage: long edge {long_edge_px} is not a positive number of pixels"
        )));
    }
    check_cancel(cancel)?;
    let mut d = pdfdoc::open(data, limits, cancel)?;
    let p = d.page(page)?;
    // The CROP box, not the media box: it is what a viewer shows, and what the pdftoppm
    // -cropbox oracle the fidelity was measured against renders.
    let bbox = BBox {
        llx: p.crop_box.llx,
        lly: p.crop_box.lly,
        urx: p.crop_box.urx,
        ury: p.crop_box.ury,
    };
    let mut long = bbox.urx - bbox.llx;
    let h = bbox.ury - bbox.lly;
    if h > long {
        long = h;
    }
    if long.is_nan() || long <= 0.0 {
        return Err(Error::Image(format!(
            "byblos: RenderPage: page {page} has a degenerate crop box {}x{}",
            format_g_exp(bbox.urx - bbox.llx),
            format_g_exp(bbox.ury - bbox.lly)
        )));
    }
    // /Rotate turns the raster but does not change which edge is longer, so the scale is taken
    // from the unrotated box.
    let params = PageParams {
        bbox,
        rotate: p.rotate,
        scale: long_edge_px as f64 / long,
    };
    let scope = p.scope;
    // The canvas lives for the whole render, so it is charged to the document's ledger: an image
    // decode is then capped at what the parse and the canvas leave.
    let (cw, ch, _, _) = render::raster_size(&params.bbox, params.scale, limits)?;
    let _canvas = d
        .ctx
        .xref
        .mem()
        .charge((cw as u64).saturating_mul(ch as u64).saturating_mul(4))?;
    // Both resolvers read the one document, so it sits in the `RefCell` of a `DocEnv`.
    let env = DocEnv::new(&mut d);
    let mut images = |name: &[u8]| env.with(|d| render_image(d, scope, name, limits));
    let mut fonts = |name: &[u8]| env.with(|d| render_font(d, scope, name));
    render::page(
        &p.content,
        &params,
        Some(&mut images),
        Some(&mut fonts),
        limits,
        cancel,
    )
}

/// `renderImages`: resolves a `Do` operand to a decoded image XObject through the same seam the
/// extract path uses, [`Doc::raw_image`] then [`image::decode`].
///
/// `None` skips the draw and leaves the rest of the page renderable, which is what makes a codec
/// byblos does not decode a missing element rather than a failed page. JBIG2 and JPX are refused
/// here by name: `raw_image` hands both back as opaque bytes with a file type and no error, so
/// `image::decode` would be asked to guess at them.
fn render_image(d: &mut Doc, scope: i64, name: &[u8], limits: &Limits) -> Option<RenderImage> {
    // Go passes context.Background() to RawImage, reserveDCT and image.Decode.
    let never = AtomicBool::new(false);
    let xo = d.xobject(scope, name)?;
    if !xo.image {
        return None;
    }
    let (data, file_type) = d.raw_image(&never, xo.id).ok()?;
    if file_type == "jbig2" || file_type == "jpx" {
        return None;
    }
    reserve_dct(d, &data, xo.id, limits)?;
    // image::decode opens its own ledger; cap it at what the document's ledger has left so the
    // two together stay within max_alloc.
    let left = limits
        .alloc_limit()
        .saturating_sub(d.ctx.xref.mem().used())
        .saturating_sub(data.len() as u64)
        .max(1);
    let capped = Limits {
        max_alloc: left,
        ..*limits
    };
    let im: Image = image::decode(&data, &capped, &never).ok()?;
    Some(RenderImage {
        data: im,
        stencil: false,
        invert: false,
    })
}

/// Go's `%g` for a float64: shortest digits, `e+06` form when the decimal exponent is below -4
/// or at least 6, plain decimal otherwise.
fn format_g_exp(v: f64) -> String {
    if !v.is_finite() || v == 0.0 {
        return format_g(v);
    }
    let e = format!("{v:e}");
    let Some((mant, exp)) = e.split_once('e') else {
        return format_g(v);
    };
    match exp.parse::<i32>() {
        Ok(x) if !(-4..6).contains(&x) => {
            let sign = if x < 0 { '-' } else { '+' };
            format!("{mant}e{sign}{:02}", x.unsigned_abs())
        }
        _ => format_g(v),
    }
}

/// `reserveDCT`, without the budget: a JPEG's SOF against the image dictionary and the pixel
/// cap. `None` for a refusal. Any other raster passes: `image::decode` charges its own buffers.
fn reserve_dct(d: &Doc, data: &[u8], id: i64, limits: &Limits) -> Option<()> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Some(());
    }
    let (mut dict_pixels, mut n) = (-1i64, 0usize);
    if let Some(info) = d.image_info(id) {
        dict_pixels = mul_sat(info.width, info.height);
        n = match info.color_space.as_str() {
            "DeviceGray" => 1,
            "DeviceRGB" => 3,
            "DeviceCMYK" => 4,
            _ => 0,
        };
    }
    dct::check_frame(data, limits.image_pixels(), dict_pixels, n)
        .ok()
        .map(|_| ())
}

/// `renderFonts`: resolves a `Tf` operand through [`Doc::render_font`]. `None` leaves the glyphs
/// undrawn without failing the page, the same degradation [`render_image`] gives an undecodable
/// raster.
fn render_font(d: &mut Doc, scope: i64, name: &[u8]) -> Option<Font> {
    let rf = d.render_font(scope, name)?;
    Some(Font {
        program: rf.program,
        base_font: rf.base_font,
        flags: rf.flags,
        first_char: rf.first_char,
        widths: rf.widths,
        type0: rf.type0,
        w: rf.w,
        dw: rf.dw,
    })
}

#[cfg(test)]
mod tests {
    //! Ports of `render_test.go` (package `byblos_test`). Left out: `TestRenderPageAgreesWith
    //! ExtractPageRaster` (needs `extract_page_raster`, wave 3 `extract`), and the byte-reader
    //! half of `TestRenderPageContextIsCancellable` (Go counts the reads of an `io.ReadSeeker`;
    //! the input here is a slice).
    use super::*;
    use crate::build::{BuildPage, build_pdf};
    use crate::encoded::{ColorSpace, EncodedImage};
    use std::fmt::Write as _;

    /// A zlib stream of `raw` made of stored blocks.
    fn zlib(raw: &[u8]) -> Vec<u8> {
        let mut z = vec![0x78, 0x01];
        let mut chunks = raw.chunks(65535).peekable();
        if chunks.peek().is_none() {
            z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
        }
        while let Some(c) = chunks.next() {
            z.push(u8::from(chunks.peek().is_none()));
            let n = c.len() as u16;
            z.extend_from_slice(&n.to_le_bytes());
            z.extend_from_slice(&(!n).to_le_bytes());
            z.extend_from_slice(c);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &x in raw {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        z
    }

    /// `bornDigitalPDF`: a one-page document with a 4x4 truecolour image covering a 4x4 point
    /// page. All 16 pixels are distinct.
    fn born_digital_pdf() -> Vec<u8> {
        let mut raw = Vec::new();
        for i in 0..16u8 {
            raw.extend_from_slice(&[i * 16, 255 - i * 16, i * 8 + 40]);
        }
        let img = EncodedImage {
            width: 4,
            height: 4,
            bpc: 8,
            color_space: ColorSpace {
                name: "DeviceRGB".into(),
                ..ColorSpace::default()
            },
            filter: "FlateDecode".into(),
            data: zlib(&raw),
            ..EncodedImage::default()
        };
        let page = BuildPage {
            image: img,
            width_pt: 4.0,
            height_pt: 4.0,
            ..BuildPage::default()
        };
        build_pdf(&[page], &Limits::default(), &AtomicBool::new(false)).unwrap()
    }

    fn render_px(data: &[u8], page: usize, px: i64) -> Result<Rgba> {
        render_page(data, page, px, &Limits::default(), &AtomicBool::new(false))
    }

    fn is_white(img: &Rgba, x: i64, y: i64) -> bool {
        img.pixel(x, y) == Some(&[255, 255, 255, 255][..])
    }

    /// A document of `objs` (object 1 first) with a correct xref.
    fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = b"%PDF-1.7\n".to_vec();
        let mut offs = vec![0usize];
        for (i, o) in objs.iter().enumerate() {
            offs.push(b.len());
            b.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            b.extend_from_slice(o);
            b.extend_from_slice(b"\nendobj\n");
        }
        let x = b.len();
        let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
        for o in &offs[1..] {
            let _ = writeln!(tail, "{o:010} 00000 n ");
        }
        let _ = write!(
            tail,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n",
            objs.len() + 1
        );
        b.extend_from_slice(tail.as_bytes());
        b
    }

    fn stream(dict: &str, payload: &[u8]) -> Vec<u8> {
        let mut o = format!("<< {dict} /Length {} >>\nstream\n", payload.len()).into_bytes();
        o.extend_from_slice(payload);
        o.extend_from_slice(b"\nendstream");
        o
    }

    /// `rawPDF`: a one-page document with the given `/MediaBox`.
    fn raw_pdf(media_box: &str) -> Vec<u8> {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox {media_box} /Contents 4 0 R >>")
                .into_bytes(),
            stream("", b"0 0 0 rg"),
        ])
    }

    /// `brokenImagePDF`: a 200x200 page with an image XObject that has NO `/BitsPerComponent`,
    /// drawn over the lower-left quadrant, and a black rectangle in the upper-left.
    fn broken_image_pdf() -> Vec<u8> {
        let raster = zlib(&[0u8; 16]);
        let content = b"q 100 0 0 100 10 10 cm /Im0 Do Q\n0 0 0 rg 10 150 50 20 re f";
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            stream(
                "/Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceGray \
                 /Filter /FlateDecode",
                &raster,
            ),
        ])
    }

    #[test]
    fn reachable_and_paints_the_image() {
        let img = render_px(&born_digital_pdf(), 1, 400).unwrap();
        assert_eq!((img.rect.dx(), img.rect.dy()), (400, 400));
        let mut non_white = 0;
        for y in 0..400 {
            for x in 0..400 {
                if !is_white(&img, x, y) {
                    non_white += 1;
                }
            }
        }
        assert!(
            non_white > 0,
            "every pixel is white: the image was not drawn"
        );
    }

    #[test]
    fn refuses_a_non_positive_long_edge() {
        let pdf = born_digital_pdf();
        for px in [0, -1] {
            let e = render_px(&pdf, 1, px).unwrap_err();
            assert!(e.to_string().contains("long edge"), "{e}");
        }
    }

    #[test]
    fn a_cancelled_call_returns_cancelled() {
        let cancel = AtomicBool::new(true);
        let e = render_page(&born_digital_pdf(), 1, 400, &Limits::default(), &cancel).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    #[test]
    fn names_the_crop_box_not_a_scale() {
        for b in ["[0 0 0 0]", "[0 0 -5 -5]", "[10 10 10 10]"] {
            let e = render_px(&raw_pdf(b), 1, 400).unwrap_err();
            assert!(e.to_string().contains("degenerate crop box"), "{b}: {e}");
        }
    }

    #[test]
    fn rejects_a_page_out_of_range() {
        assert!(render_px(&born_digital_pdf(), 2, 400).is_err());
    }

    /// byb-11r: a corrupt image skips; the rest of the page still renders.
    #[test]
    fn survives_an_image_without_bits_per_component() {
        let img = render_px(&broken_image_pdf(), 1, 400).unwrap();
        // Scale 2, device y flipped: the rectangle is device (20,60)-(120,100), the image
        // (20,180)-(220,380).
        assert!(!is_white(&img, 50, 80), "the whole page was abandoned");
        assert!(is_white(&img, 100, 300), "the corrupt image drew something");
    }
}
