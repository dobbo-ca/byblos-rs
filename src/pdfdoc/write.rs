//! The write half of the pdfdoc seam: the non-type part of `internal/pdfdoc/write.go` (byb-0he).
//! The types (`EncodedImage`, `ColorSpace`, `DecodeParms`) are in `crate::encoded`; their
//! pdf-rs rendering ([`ColorSpace::object`], [`DecodeParms::dict`], [`EncodedImage::validate`]),
//! [`Doc::replace_image`], [`Doc::write`] and [`validate`] are here.
//!
//! The mechanism is deliberately narrow: replace an image XObject's stream payload and the
//! dictionary entries that describe it, then serialize the same context that was read, so
//! placement, the page tree, annotations and every untouched object survive without being
//! re-derived. Three pdf-go behaviours make it less obvious than it looks:
//!
//! * `DereferenceStreamDict` returns a COPY. The dictionary edits do not reach the table unless
//!   the whole stream is assigned back to its entry, which [`Doc::replace_image`] does. (Here
//!   `dereference_stream_dict` also clones, and `Doc::streams` holds clones; the table entry is
//!   replaced explicitly.)
//! * The writer checks the payload against the stream's length, and `/Length` in the dictionary
//!   is a separate field. Both are updated.
//! * The context must come from `Doc::open_limits`: its page-tree normalization is load-bearing
//!   for writing, not just for reading. Because `write` is a method on that `Doc`, it is
//!   structural here.
//!
//! # Assumptions about `mod.rs` and `doc.rs` (written concurrently)
//!
//! * `Doc` has `pub(super) ctx: pdf::model::Context`, and Go's `d.streams`, `d.refs` and
//!   `d.images` as `pub(super) streams: BTreeMap<i64, pdf::object::StreamDict>`,
//!   `pub(super) refs: BTreeMap<i64, pdf::object::Ref>` and
//!   `pub(super) images: BTreeMap<i64, ImageInfo>`.
//! * `ImageInfo` has `width: i64`, `height: i64`, `bpc: i64`, `image_mask: bool`, `smask: bool`
//!   and `mask: bool`.
//! * `Doc::open_limits(data, lim, cancel) -> Result<Doc>` is Go's `OpenLimits`.
//!
//! # Differences from Go
//!
//! * `Write` returns the document as a `Vec<u8>` and takes the [`WriteEnv`] of the fallback
//!   writer. `Validate` takes the caps of `lim` only for `max_alloc` and `max_threads`, as Go
//!   runs it under `defaultConfig()`.
//! * The new payload is charged to the document's ledger (`max_alloc`).
//! * Go's `%q` is Rust's `{:?}` in the error texts.

use super::Doc;
use super::deterministic::write_deterministic;
use super::linearize::pdf_err;
use super::optimize::default_pdf_config;
use crate::encoded::{ColorSpace, DecodeParms, EncodedImage};
use crate::error::Result;
use crate::limits::{Limits, check_cancel};
use crate::linearize::{malformed, wrap};
use pdf::model::EntryObject;
use pdf::object::{Dict, Object, PdfFilter};
use pdf::{Bytes, Mem, WriteEnv};
use std::sync::atomic::AtomicBool;

/// `componentsOf`: the number of components of a device colour space.
fn components_of(space: &str) -> Option<i64> {
    match space {
        "DeviceGray" => Some(1),
        "DeviceRGB" => Some(3),
        "DeviceCMYK" => Some(4),
        _ => None,
    }
}

impl ColorSpace {
    /// `object`: the colour space as the PDF object `/ColorSpace` takes.
    ///
    /// # Errors
    /// [`crate::Error::Malformed`] for an indexed space whose base is not a device space, whose
    /// `hi_val` is outside 0..255 or whose lookup is not `(hi_val + 1) * components` bytes, and
    /// for a name that is not a device space.
    pub fn object(&self) -> Result<Object> {
        if self.name == "Indexed" {
            let Some(n) = components_of(&self.base) else {
                return Err(malformed(format!(
                    "indexed base {:?} is not a device colour space",
                    self.base
                )));
            };
            if !(0..=255).contains(&self.hi_val) {
                return Err(malformed(format!(
                    "indexed hival {} is outside 0..255",
                    self.hi_val
                )));
            }
            let want = (self.hi_val + 1) * n;
            if self.lookup.len() as i64 != want {
                return Err(malformed(format!(
                    "indexed lookup is {} bytes, want {want} for hival {} over {}",
                    self.lookup.len(),
                    self.hi_val,
                    self.base
                )));
            }
            return Ok(Object::Array(vec![
                Object::name(b"Indexed"),
                Object::name(self.base.as_bytes()),
                Object::Integer(self.hi_val),
                Object::hex_from_bytes(&self.lookup),
            ]));
        }
        if components_of(&self.name).is_none() {
            return Err(malformed(format!(
                "colour space {:?} is not supported",
                self.name
            )));
        }
        Ok(Object::name(self.name.as_bytes()))
    }
}

impl DecodeParms {
    /// `dict`: the `/DecodeParms` dictionary, or `None` when there is nothing to say.
    pub fn dict(&self) -> Option<Dict> {
        let mut d = Dict::default();
        for (key, v) in [
            (&b"Predictor"[..], self.predictor),
            (&b"Colors"[..], self.colors),
            (&b"BitsPerComponent"[..], self.bits_per_component),
            (&b"Columns"[..], self.columns),
        ] {
            if v != 0 {
                d.set(key, Object::Integer(v));
            }
        }
        if d.is_empty() { None } else { Some(d) }
    }
}

impl EncodedImage {
    /// `validate`: what `ReplaceImage` needs of an image before it stores it.
    ///
    /// # Errors
    /// [`crate::Error::Malformed`] for non-positive dimensions, a bit depth outside
    /// 1, 2, 4, 8 and 16, no filter, a `/Decode` array or `/SMask`, no data, or a colour space
    /// that [`ColorSpace::object`] refuses.
    pub fn validate(&self) -> Result<()> {
        if self.width <= 0 || self.height <= 0 {
            return Err(malformed(format!(
                "dimensions {}x{} are not positive",
                self.width, self.height
            )));
        }
        if !matches!(self.bpc, 1 | 2 | 4 | 8 | 16) {
            return Err(malformed(format!(
                "bits per component {} is not one of 1, 2, 4, 8, 16",
                self.bpc
            )));
        }
        if self.filter.is_empty() {
            return Err(malformed("no filter named"));
        }
        if !self.decode.is_empty() || self.smask.is_some() {
            return Err(malformed(
                "a /Decode array or /SMask is not carried by ReplaceImage",
            ));
        }
        if self.data.is_empty() {
            return Err(malformed("no data"));
        }
        self.color_space.object()?;
        Ok(())
    }
}

impl Doc {
    /// `ReplaceImage`: substitutes the encoded bytes and dictionary of the image XObject that
    /// `XObject` previously resolved as `id`.
    ///
    /// The substitution is in memory; [`Doc::write`] serializes it. The image info of `id`
    /// reflects the new image afterwards. `/Width` and `/Height` are rewritten to `img`'s
    /// dimensions, which need not match the original raster's: placement is a CTM on the unit
    /// square, so it is unaffected by the raster shrinking or growing underneath it.
    ///
    /// It refuses an image carrying `/SMask` or `/Mask`: those describe transparency keyed to
    /// the samples being replaced, and neither dropping them nor keeping them against unrelated
    /// pixels is defensible. It also refuses `/ImageMask`, whose dictionary is a different
    /// shape (no `/ColorSpace`, the samples are a stencil).
    ///
    /// # Errors
    /// [`crate::Error::Malformed`] for an id that was not resolved, a direct stream, a masked
    /// image, an invalid `img`, or an image with no cross-reference entry; [`crate::Error::Alloc`]
    /// and [`crate::Error::OutOfMemory`] for the payload.
    pub fn replace_image(&mut self, id: i64, mut img: EncodedImage) -> Result<()> {
        let Some(mut sd) = self.streams.get(&id).cloned() else {
            return Err(malformed(format!(
                "byblos/pdfdoc: image {id} has not been resolved on this document"
            )));
        };
        let Some(r) = self.refs.get(&id).copied() else {
            // ISO 32000-1 section 7.3.8.1: every stream shall be an indirect object. A direct
            // image stream has no xref entry to write back to, and is malformed anyway.
            return Err(malformed(format!(
                "byblos/pdfdoc: image {id} is a direct object and cannot be replaced"
            )));
        };
        if let Some(info) = self.images.get(&id) {
            if info.smask {
                return Err(malformed(format!(
                    "byblos/pdfdoc: image {id} carries an /SMask"
                )));
            }
            if info.mask {
                return Err(malformed(format!(
                    "byblos/pdfdoc: image {id} carries a /Mask"
                )));
            }
            if info.image_mask {
                return Err(malformed(format!(
                    "byblos/pdfdoc: image {id} is an /ImageMask stencil"
                )));
            }
        }
        let replacing = format!("byblos/pdfdoc: replacing image {id}");
        img.validate().map_err(|e| wrap(e, &replacing))?;
        let cs = img.color_space.object().map_err(|e| wrap(e, &replacing))?;

        let xt = &mut self.ctx.xref;
        if xt.find_table_entry(r.num, r.generation).is_none() {
            return Err(malformed(format!(
                "byblos/pdfdoc: image {id} has no cross-reference entry"
            )));
        }

        let len = img.data.len();
        let held = xt.mem().charge(len as u64)?;
        sd.dict.set(b"Width", Object::Integer(img.width));
        sd.dict.set(b"Height", Object::Integer(img.height));
        sd.dict.set(b"BitsPerComponent", Object::Integer(img.bpc));
        sd.dict.set(b"ColorSpace", cs);
        sd.dict.set(b"Filter", Object::name(img.filter.as_bytes()));
        sd.dict.set(b"Length", Object::Integer(len as i64));

        // Entries whose meaning is tied to the samples being replaced. /Decode in particular is
        // not inert: a leftover [1 0] silently inverts the new image.
        let parms = img.decode_parms.as_ref().and_then(DecodeParms::dict);
        match &parms {
            Some(p) => sd.dict.set(b"DecodeParms", Object::Dict(p.clone())),
            None => {
                sd.dict.0.remove(&b"DecodeParms"[..]);
            }
        }
        sd.dict.0.remove(&b"Decode"[..]);

        sd.raw = Some(Bytes::new(std::mem::take(&mut img.data), len, Some(held)));
        sd.stream_length = Some(len as i64);
        sd.stream_length_obj_nr = None;
        sd.content = None; // the decoded cache now describes the old samples
        sd.filter_pipeline = vec![PdfFilter {
            name: img.filter.as_bytes().to_vec(),
            decode_parms: parms,
        }];

        // The copy trap: this, not the mutations above, is what reaches the writer.
        if let Some(entry) = xt.entry_mut(r.num) {
            entry.object = Some(EntryObject::Obj(Object::Stream(Box::new(sd.clone()))));
        }
        self.streams.insert(id, sd);
        if let Some(info) = self.images.get_mut(&id) {
            info.width = img.width;
            info.height = img.height;
            info.bpc = img.bpc;
        }
        Ok(())
    }

    /// `Write`: serializes the document, including any substitution [`Doc::replace_image`]
    /// made.
    ///
    /// It does not optimize and does not validate. Optimize is byb-b5's, and `open_limits`
    /// deliberately skips validation: a file byblos accepted on the way in must not be rejected
    /// on the way out. The output is byte-deterministic: a pure function of the context's
    /// content (see `deterministic.rs`), except for encrypted documents, which fail open to
    /// pdf-rs's writer.
    ///
    /// # Errors
    /// The errors of [`write_deterministic`] and of giving the pages that inherit `/Resources` a
    /// copy of them ([`crate::Error::Alloc`] for the copies), with the context
    /// `byblos/pdfdoc: write`.
    pub fn write(&mut self, env: &WriteEnv, cancel: &AtomicBool) -> Result<Vec<u8>> {
        let held = self
            .store_adopted_resources(cancel)
            .map_err(|e| wrap(e, "byblos/pdfdoc: write"))?;
        self.adopted_held = Some(held);
        write_deterministic(&mut self.ctx, env, cancel).map_err(|e| wrap(e, "byblos/pdfdoc: write"))
    }
}

/// `Validate`: whether `data` parses as a structurally valid PDF: xref offsets resolve and the
/// page tree's `/Count` agrees with its `/Kids`. It checks structure only, not pixel content.
///
/// This exists for `BuildPDF` (byb-c3o): a hand-rolled writer has no reader of its own to
/// round-trip through, and pdf-rs's validator is the independent check that the bytes it emits
/// are a PDF at all.
///
/// # Errors
/// The read and validation errors of pdf-rs, with the context `byblos/pdfdoc: validate`;
/// `Error::Cancelled`.
pub fn validate(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<()> {
    validate_with_mem(data, lim, &lim.mem(), cancel)
}

/// [`validate`] on the caller's ledger `mem` (pdf-rs `api::validate_with_mem`).
///
/// # Errors
/// As [`validate`].
pub(crate) fn validate_with_mem(
    data: &[u8],
    lim: &Limits,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    pdf::api::validate_with_mem(data, &default_pdf_config(lim), mem, cancel)
        .map_err(|e| pdf_err(e, "byblos/pdfdoc: validate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn gray() -> ColorSpace {
        ColorSpace {
            name: "DeviceGray".into(),
            ..ColorSpace::default()
        }
    }

    fn good() -> EncodedImage {
        EncodedImage {
            width: 4,
            height: 4,
            bpc: 8,
            color_space: gray(),
            filter: "FlateDecode".into(),
            data: vec![1, 2, 3],
            ..EncodedImage::default()
        }
    }

    /// `TestEncodedImageValidateRefusesDecode`: `ReplaceImage` does not write `/Decode`, so an
    /// image that carries one is refused rather than stored with the array silently dropped.
    #[test]
    fn validate_refuses_decode_and_smask() {
        let mut img = EncodedImage {
            width: 1,
            height: 1,
            data: vec![0],
            ..good()
        };
        assert!(img.validate().is_ok());
        img.decode = vec![1.0, 0.0];
        assert!(img.validate().is_err());
        img.decode.clear();
        img.smask = Some(Box::new(good()));
        assert!(img.validate().is_err());
    }

    /// The `bad` table of `TestReplaceImageRefusesWhatItCannotDoCorrectly`: what is refused
    /// before the document is touched.
    #[test]
    fn validate_refuses_what_it_cannot_do_correctly() {
        let indexed = |base: &str, hi_val: i64, lookup: &[u8]| ColorSpace {
            name: "Indexed".into(),
            base: base.into(),
            hi_val,
            lookup: lookup.to_vec(),
        };
        let cases: Vec<(&str, EncodedImage)> = vec![
            ("zero width", EncodedImage { width: 0, ..good() }),
            (
                "zero height",
                EncodedImage {
                    height: 0,
                    ..good()
                },
            ),
            ("bad bpc", EncodedImage { bpc: 3, ..good() }),
            (
                "no filter",
                EncodedImage {
                    filter: String::new(),
                    ..good()
                },
            ),
            (
                "no data",
                EncodedImage {
                    data: Vec::new(),
                    ..good()
                },
            ),
            (
                "unknown space",
                EncodedImage {
                    color_space: ColorSpace {
                        name: "DeviceLab".into(),
                        ..ColorSpace::default()
                    },
                    ..good()
                },
            ),
            (
                "short palette",
                EncodedImage {
                    color_space: indexed("DeviceRGB", 3, &[1, 2, 3]),
                    ..good()
                },
            ),
            (
                "bad palette base",
                EncodedImage {
                    color_space: indexed("DeviceLab", 0, &[1]),
                    ..good()
                },
            ),
            (
                "hival over 255",
                EncodedImage {
                    color_space: indexed("DeviceGray", 256, &[0; 257]),
                    ..good()
                },
            ),
        ];
        assert!(good().validate().is_ok());
        for (name, img) in cases {
            let e = img.validate().unwrap_err();
            assert!(matches!(e, Error::Malformed(_)), "{name}: {e:?}");
            assert_eq!(e.class(), "error", "{name}");
        }
    }

    #[test]
    fn color_space_objects() {
        assert!(matches!(
            gray().object().unwrap(),
            Object::Name(n) if n == b"DeviceGray"
        ));
        let cs = ColorSpace {
            name: "Indexed".into(),
            base: "DeviceRGB".into(),
            hi_val: 1,
            lookup: vec![0, 0xab, 0xff, 1, 2, 3],
        };
        let Object::Array(a) = cs.object().unwrap() else {
            panic!("not an array");
        };
        assert_eq!(a.len(), 4);
        assert!(matches!(&a[0], Object::Name(n) if n == b"Indexed"));
        assert!(matches!(&a[1], Object::Name(n) if n == b"DeviceRGB"));
        assert!(matches!(&a[2], Object::Integer(1)));
        // NewHexLiteral writes lower case.
        assert!(matches!(&a[3], Object::Hex(h) if h == b"00abff010203"));
    }

    /// `TestReplaceImageDecodeParmsDict`'s dictionary half: only the non-zero fields are
    /// written, and nothing at all for a zero value.
    #[test]
    fn decode_parms_dict() {
        assert!(DecodeParms::default().dict().is_none());
        let d = DecodeParms {
            predictor: 15,
            colors: 3,
            bits_per_component: 0,
            columns: 7,
        }
        .dict()
        .unwrap();
        assert_eq!(
            d.pdf_string(),
            b"<</Colors 3/Columns 7/Predictor 15>>".to_vec()
        );
    }

    #[test]
    fn validate_is_an_error_for_garbage_and_honours_cancel() {
        let lim = Limits::default();
        assert!(validate(b"not a pdf", &lim, &AtomicBool::new(false)).is_err());
        assert_eq!(
            validate(b"%PDF-1.4", &lim, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        );
    }

    // The tests below need a corpus PDF, which the Go internal/corpus generator makes and this
    // crate does not port. They read it from tests/data/corpus/<name>.pdf.

    fn corpus(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/data/corpus/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn open(data: &[u8]) -> Doc {
        Doc::open_limits(data, &Limits::default(), &AtomicBool::new(false)).unwrap()
    }

    /// `TestIndirectKidsSurvivesAWriteRoundTrip`, reduced to the page count: through a bare
    /// read this document writes back as 594 bytes with zero pages, and nothing about the
    /// failure is observable except by counting what came out. (The Go test also compares each
    /// page's MediaBox, content length and annotation count.)
    #[test]
    #[ignore = "needs tests/data/corpus/indirect-kids.pdf from the Go oracle -gen"]
    fn indirect_kids_survives_a_write_round_trip() {
        let mut before = open(&corpus("indirect-kids"));
        assert_eq!(before.ctx.xref.page_count, 2);
        let out = before
            .write(&WriteEnv::parity(), &AtomicBool::new(false))
            .unwrap();
        let after = open(&out);
        assert_eq!(after.ctx.xref.page_count, 2, "{} bytes out", out.len());
    }

    /// `TestEveryCorpusDocumentSurvivesAWriteRoundTrip`: every document in
    /// tests/data/corpus that opens keeps its page count through a write.
    #[test]
    #[ignore = "needs tests/data/corpus/*.pdf from the Go oracle -gen"]
    fn every_corpus_document_survives_a_write_round_trip() {
        let dir = format!("{}/tests/data/corpus", env!("CARGO_MANIFEST_DIR"));
        let mut seen = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if path.extension().is_none_or(|x| x != "pdf") {
                continue;
            }
            let data = std::fs::read(&path).unwrap();
            // 'malformed' is truncated on purpose and must not open.
            let Ok(mut d) = Doc::open_limits(&data, &Limits::default(), &AtomicBool::new(false))
            else {
                continue;
            };
            let want = d.ctx.xref.page_count;
            let out = d
                .write(&WriteEnv::parity(), &AtomicBool::new(false))
                .unwrap();
            let after = open(&out);
            assert_eq!(after.ctx.xref.page_count, want, "{}", path.display());
            seen += 1;
        }
        assert!(seen > 0, "no corpus document in {dir}");
    }

    /// The "unresolved id" case of `TestReplaceImageRefusesWhatItCannotDoCorrectly`.
    #[test]
    #[ignore = "needs tests/data/corpus/scan.pdf from the Go oracle -gen"]
    fn replace_image_refuses_an_unresolved_id() {
        let mut d = open(&corpus("scan"));
        assert!(d.replace_image(9999, good()).is_err());
    }
}
