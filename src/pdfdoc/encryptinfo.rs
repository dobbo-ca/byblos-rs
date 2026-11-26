//! Port of `internal/pdfdoc/encryptinfo.go`: the encrypt-census reads. They exist so a caller
//! can classify a document's `/Encrypt` shape without importing the PDF library itself.

use super::doc::Doc;
use crate::error::Error;

/// `EncryptInfo` is what a document [`super::open`] already accepted says about its own
/// `/Encrypt` dictionary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncryptInfo {
    pub encrypted: bool,
    pub p: i64,
    pub v: i64,
    pub r: i64,
}

impl Doc {
    /// `EncryptInfo` reports the document's `/Encrypt` shape. It reads the SAME context `open`
    /// already parsed: no second read of the source.
    pub fn encrypt_info(&self) -> EncryptInfo {
        let xt = &self.ctx.xref;
        if xt.encrypt.is_none() {
            return EncryptInfo::default();
        }
        let mut info = EncryptInfo {
            encrypted: true,
            ..EncryptInfo::default()
        };
        if let Some(e) = &xt.e {
            info.p = e.p;
            info.v = e.v;
            info.r = e.r;
        }
        info
    }
}

/// `IsWrongPassword` reports whether `err` is pdf-rs's own refusal to open a document because
/// neither an empty user nor an empty owner password authenticated, as opposed to any other
/// reason [`super::open`] can fail (a malformed file, an unsupported `/V`, ...), which is not
/// evidence of a password.
pub fn is_wrong_password(err: &Error) -> bool {
    matches!(err, Error::Pdf(e) if e.is_wrong_password())
}
