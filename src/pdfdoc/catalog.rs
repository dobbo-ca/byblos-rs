//! Port of `internal/pdfdoc/catalog.go`: what the document catalog says about signatures and
//! dynamic XFA.

use super::doc::Doc;
use pdf::object::Object;

/// `CatalogInfo` is what the document catalog says about signatures and dynamic XFA, the two
/// inputs OCRmyPDF refuses (`_pipeline.py` `validate_pdfinfo_options`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogInfo {
    /// `/AcroForm /SigFlags` has bit 1 (SignaturesExist) set.
    pub has_signature: bool,
    /// The catalog's `/NeedsRendering` is true (dynamic XFA).
    pub needs_rendering: bool,
}

impl Doc {
    /// `Catalog` reads the catalog flags from the context [`super::open`] already parsed. A
    /// catalog that does not resolve answers the default: no flags.
    pub fn catalog(&mut self) -> CatalogInfo {
        let cat = match self.ctx.xref.catalog() {
            Ok(c) => c.clone(),
            Err(_) => return CatalogInfo::default(),
        };
        let mut info = CatalogInfo::default();
        if let Object::Bool(b) = self.get_deref(&cat, b"NeedsRendering") {
            info.needs_rendering = b;
        }
        if let Some(af) = self.dict_entry(&cat, b"AcroForm")
            && let Object::Integer(v) = self.get_deref(&af, b"SigFlags")
        {
            info.has_signature = v & 1 != 0;
        }
        info
    }
}
