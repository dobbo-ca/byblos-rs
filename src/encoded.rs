//! Wave 1d: port of the `EncodedImage`, `ColorSpace` and `DecodeParms` types of byblos
//! `internal/pdfdoc/write.go` (the types only, not their pdf-go rendering). They are pdf-free.
//!
//! Go's `int` fields are `i64` here, so a hostile or hand-built value (negative, or past
//! `i32`) reaches the same validation Go applies and cannot wrap.

/// `ColorSpace`: the colour space of an encoded image.
///
/// `name` is a device space (`"DeviceGray"`, `"DeviceRGB"`, `"DeviceCMYK"`) or `"Indexed"`, in
/// which case `base`, `hi_val` and `lookup` describe the palette.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColorSpace {
    pub name: String,
    /// Indexed only: the base space the palette entries are expressed in.
    pub base: String,
    /// Indexed only: the highest palette index; the palette has `hi_val + 1` entries.
    pub hi_val: i64,
    /// Indexed only: `(hi_val + 1) * components(base)` bytes.
    pub lookup: Vec<u8>,
}

/// `DecodeParms`: the subset of `/DecodeParms` byblos's encoders emit. A zero `predictor` means
/// no predictor, which is also how PDF reads an absent `/Predictor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeParms {
    pub predictor: i64,
    pub colors: i64,
    pub bits_per_component: i64,
    pub columns: i64,
}

/// `EncodedImage`: a fully encoded image ready to be stored verbatim.
///
/// `data` is the ENCODED stream payload: whatever `filter` names has already been applied to
/// it. Nothing compresses it, and nothing checks that it decodes to `width x height` samples;
/// that is the encoder's contract.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EncodedImage {
    pub width: i64,
    pub height: i64,
    /// `/BitsPerComponent`.
    pub bpc: i64,
    pub color_space: ColorSpace,
    /// For example `"JBIG2Decode"`, `"FlateDecode"`, `"DCTDecode"`.
    pub filter: String,
    pub decode_parms: Option<DecodeParms>,
    /// `/Decode`; `BuildPDF` writes it, `ReplaceImage` refuses it.
    pub decode: Vec<f64>,
    /// `BuildPDF` writes it, `ReplaceImage` refuses it.
    pub smask: Option<Box<EncodedImage>>,
    pub data: Vec<u8>,
}
