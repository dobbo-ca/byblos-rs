//! Wave 3: port of byblos substitute.go: [`replace_images`], the write seam by which bytes one of
//! byblos's encoders produced get back into an existing PDF.
//!
//! It is a general primitive rather than per-capability options: the caller chooses which
//! images to re-encode and with what. Nothing here resamples (pass `1` as the declared depth
//! to `downsample` for a bitonal source or an `/ImageMask`), compares sizes, or writes
//! provenance (the caller records what it applied through
//! [`record_extraction`](crate::provenance::record_extraction) and
//! [`write_provenance`](crate::provenance::write_provenance)); a record the document already
//! carries survives unchanged and can go stale.
//!
//! # Differences from Go
//!
//! * One function for `ReplaceImages`, `ReplaceImagesContext` and `ReplaceImagesLimits`.
//! * The map is a `BTreeMap`: Go sorts the keys so that two unsubstitutable images always name
//!   the same one first, and iterating a `BTreeMap` is that order.
//! * The write pins the clock and random source of pdf-rs's writer to `now` (see
//!   [`replace_images_at`]); [`replace_images`] leaves them real.

use crate::content::{self, Scan};
use crate::encoded::EncodedImage;
use crate::error::{Error, Result};
use crate::limits::{Limits, check_cancel};
use crate::linearize::wrap;
use crate::optimize::write_env;
use crate::pdfdoc::{Doc, DocEnv};
use pdf::GoTime;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// `fmt.Errorf("byblos: replace images: %w", err)`.
fn ctx(e: Error) -> Error {
    wrap(e, "byblos: replace images")
}

/// Walks page `n` (1-based) of `d` and returns what the walk established: the first half of
/// Go's `inspectPage`. An image is only substitutable once the document has resolved it, and
/// that happens as a page's content stream is walked; the walk records every image it reaches
/// in `d`.
///
/// # Errors
/// The errors of [`Doc::page`], and of the walk with the context `byblos: page N`.
pub(crate) fn scan_page(d: &mut Doc, n: usize, cancel: &AtomicBool) -> Result<Scan> {
    let p = d.page(n)?;
    let lim = *d.limits();
    let src: content::Stream = Arc::new(p.content);
    let env = DocEnv::new(d);
    let (scan, res) = content::walk(cancel, &src, p.scope, &env, &lim);
    res.map_err(|e| wrap(e, &format!("byblos: page {n}")))?;
    Ok(scan)
}

/// `ReplaceImages`: copies `pdf` with each named image XObject's stream and dictionary replaced
/// by the encoded image given for it. The keys are `ImageRef::obj_nr`, from `inspect`; an object
/// number this document has no image for is an error, not a silently skipped substitution.
///
/// It is per object: one XObject painted on several pages is substituted once and changes all of
/// them. Placement is untouched (a page positions an image with a CTM on the unit square), so a
/// raster with other pixel dimensions lands in the same rectangle at another resolution.
///
/// It refuses what the write seam refuses: an image carrying `/SMask` or `/Mask`, an
/// `/ImageMask` stencil, an image stream that is a direct object. A stale `/Decode` is dropped.
/// The first failure aborts the call and nothing is returned. It does not check that the data
/// decodes to `width x height` samples (the encoder's contract), and it does not compare sizes:
/// a substitution that grows the document is written.
///
/// Every page is walked, not only those being substituted on: nothing else says which page
/// paints a given object. `cancel` is checked per page and per substitution, but the open before
/// and the write after are single passes of pdf-rs.
///
/// # Errors
/// [`Error::Malformed`] for an empty `subs`, an object that is not an image of this document,
/// a masked or direct image, or an invalid [`EncodedImage`]; a page that cannot be read or
/// walked; the errors of [`Doc::open_limits`] (caps are `limits`, a breach is
/// [`Error::Limit`]) and of the write; [`Error::Alloc`], [`Error::Cancelled`]. Each carries the
/// context `byblos: replace images`.
pub fn replace_images(
    pdf: &[u8],
    subs: &BTreeMap<i64, EncodedImage>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    replace_images_at(pdf, subs, None, limits, cancel)
}

/// [`replace_images`] with the write clock pinned to `now` (the parity random source goes with
/// it); `None` is the real clock and random source. Only the fallback writer reads either.
///
/// # Errors
/// As [`replace_images`].
pub fn replace_images_at(
    pdf: &[u8],
    subs: &BTreeMap<i64, EncodedImage>,
    now: Option<GoTime>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if subs.is_empty() {
        return Err(Error::Malformed(
            "byblos: replace images: no substitutions".to_string(),
        ));
    }
    let mut d = Doc::open_limits(pdf, limits, cancel).map_err(ctx)?;
    // Every page is walked: a page that cannot be read at all fails the call, since there is
    // no way to substitute into half a document and report that honestly.
    for n in 1..=d.page_count() {
        check_cancel(cancel)?;
        scan_page(&mut d, n, cancel).map_err(ctx)?;
    }
    for (&obj_nr, img) in subs {
        check_cancel(cancel)?;
        // The copy handed to the document is charged before it is made.
        let _held = d.ctx.xref.mem().charge(img.data.len() as u64)?;
        d.replace_image(obj_nr, img.clone()).map_err(ctx)?;
    }
    d.write(&write_env(now), cancel).map_err(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoded::ColorSpace;
    use crate::optimize::testpdf::{image_doc, image_doc_bodies, pdf_of};
    use crate::pdfdoc::open;

    /// The id of the image of `image_doc(1)`.
    const IMAGE: i64 = 5;

    fn never() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// `flateGrayImage`: a `w x h` DeviceGray image whose Flate stream is one stored block.
    fn flate_gray(w: usize, h: usize, seed: u8) -> EncodedImage {
        let pix: Vec<u8> = (0..w * h)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect();
        let n = u16::try_from(pix.len()).expect("small image");
        let mut z = vec![0x78, 0x01, 0x01];
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(&pix);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &pix {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        EncodedImage {
            width: w as i64,
            height: h as i64,
            bpc: 8,
            color_space: ColorSpace {
                name: "DeviceGray".to_string(),
                ..ColorSpace::default()
            },
            filter: "FlateDecode".to_string(),
            data: z,
            ..EncodedImage::default()
        }
    }

    fn subs(id: i64, img: EncodedImage) -> BTreeMap<i64, EncodedImage> {
        BTreeMap::from([(id, img)])
    }

    /// The `(width, height)` of the images of every page of `pdf`, in page order.
    fn image_sizes(pdf: &[u8]) -> Vec<(i64, i64)> {
        let mut d = open(pdf, &Limits::default(), &never()).expect("open");
        let mut out = Vec::new();
        for n in 1..=d.page_count() {
            let scan = scan_page(&mut d, n, &never()).expect("walk");
            for pl in &scan.images {
                let info = d.image_info(pl.id).expect("image info");
                out.push((info.width, info.height));
            }
        }
        out
    }

    // TestReplaceImagesSubstitutesAnImageStream, on a hand-built page.
    #[test]
    fn substitutes_an_image_stream() {
        let pdf = image_doc(1);
        assert_eq!(image_sizes(&pdf), vec![(2, 2)]);
        let img = flate_gray(8, 8, 3);
        let out =
            replace_images(&pdf, &subs(IMAGE, img), &Limits::default(), &never()).expect("replace");
        assert_eq!(image_sizes(&out), vec![(8, 8)]);
    }

    // TestReplaceImagesSubstitutesAnXObjectSharedByTwoPages: one object, one substitution, both
    // pages change.
    #[test]
    fn substitutes_an_xobject_shared_by_two_pages() {
        let pdf = image_doc(2);
        assert_eq!(image_sizes(&pdf), vec![(2, 2), (2, 2)]);
        let out = replace_images(
            &pdf,
            &subs(6, flate_gray(4, 4, 1)),
            &Limits::default(),
            &never(),
        )
        .expect("replace");
        assert_eq!(image_sizes(&out), vec![(4, 4), (4, 4)]);
    }

    // TestReplaceImagesRefusesWhatItCannotDoCorrectly.
    #[test]
    fn refuses_what_it_cannot_do_correctly() {
        let pdf = image_doc(1);
        let good = flate_gray(8, 8, 3);
        let lim = Limits::default();

        // no substitutions
        let e = replace_images(&pdf, &BTreeMap::new(), &lim, &never()).expect_err("empty map");
        assert!(e.to_string().contains("no substitutions"), "{e}");

        // an object number that is not an image of this document
        let e = replace_images(&pdf, &subs(9999, good.clone()), &lim, &never())
            .expect_err("unknown object");
        assert_eq!(e.class(), "error");

        // an image carrying /SMask: its transparency is keyed to the samples being replaced
        let mut bodies = image_doc_bodies(1);
        bodies[4] = "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray \
                     /BitsPerComponent 8 /SMask 6 0 R /Length 4 >>\nstream\nABCD\nendstream"
            .to_string();
        bodies.push(
            "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray \
             /BitsPerComponent 8 /Length 4 >>\nstream\nWXYZ\nendstream"
                .to_string(),
        );
        let masked = pdf_of(&bodies, "");
        let e = replace_images(&masked, &subs(IMAGE, good), &lim, &never()).expect_err("smask");
        assert!(e.to_string().contains("/SMask"), "{e}");
    }

    // The 8x8 DeviceGray/FlateDecode replacement of the parity op `replace`: its data is one
    // stored zlib block, and the document reads back with it.
    #[test]
    fn parity_replacement_image_round_trips() {
        let mut data = vec![0x78, 0x01, 0x01, 0x40, 0x00, 0xbf, 0xff];
        let pix: Vec<u8> = (0..64u32).map(|i| ((4 * i) & 255) as u8).collect();
        data.extend_from_slice(&pix);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &pix {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        data.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let img = EncodedImage {
            width: 8,
            height: 8,
            bpc: 8,
            color_space: ColorSpace {
                name: "DeviceGray".to_string(),
                ..ColorSpace::default()
            },
            filter: "FlateDecode".to_string(),
            data,
            ..EncodedImage::default()
        };
        let out = replace_images(
            &image_doc(1),
            &subs(IMAGE, img),
            &Limits::default(),
            &never(),
        )
        .expect("replace");
        assert_eq!(image_sizes(&out), vec![(8, 8)]);
    }

    #[test]
    fn a_cancelled_call_returns_no_bytes() {
        let cancel = AtomicBool::new(true);
        let r = replace_images(
            &image_doc(1),
            &subs(IMAGE, flate_gray(8, 8, 3)),
            &Limits::default(),
            &cancel,
        );
        assert_eq!(r, Err(Error::Cancelled));
    }

    fn corpus(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    // TestReplaceImagesRefusesWhatItCannotDoCorrectly, "no substitutions" and "object number is
    // not an image" on the corpus `scan` document.
    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn corpus_scan_refusals() {
        let pdf = corpus("scan");
        let lim = Limits::default();
        assert!(replace_images(&pdf, &BTreeMap::new(), &lim, &never()).is_err());
        let r = replace_images(&pdf, &subs(9999, flate_gray(8, 8, 3)), &lim, &never());
        assert!(r.is_err());
    }

    // TestReplaceImagesSubstitutesAnImageOnALaterPage, ...CarriesAQuantizedIndexedImage...: the
    // corpus `mixed` and `scan` documents with `inspect` to find the ObjNr (wave 3 `inspect`).
    #[test]
    #[ignore = "needs tests/data/corpus/{scan,mixed}.pdf from the Go oracle -gen, and inspect"]
    fn corpus_substitutions() {
        let pdf = corpus("mixed");
        let mut d = open(&pdf, &Limits::default(), &never()).expect("open");
        let mut first = None;
        for n in 1..=d.page_count() {
            let scan = scan_page(&mut d, n, &never()).expect("walk");
            if let Some(pl) = scan.images.first() {
                first = Some(pl.id);
                break;
            }
        }
        let id = first.expect("an image");
        let out = replace_images(
            &pdf,
            &subs(id, flate_gray(8, 8, 3)),
            &Limits::default(),
            &never(),
        )
        .expect("replace");
        assert!(image_sizes(&out).contains(&(8, 8)));
    }
}
