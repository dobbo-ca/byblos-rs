//! Shared pieces of the Rust parity oracle: the Limits presets, the output writer and the
//! canonical formats of docs/parity.md ("Output files", "Formats"), as in the Go oracle's
//! `main.go`, `format.go` and `ops.go`.

use crate::sha256::sha256_hex;
use byblos::embed::{Resolution, embed_jp2, embed_jpeg, embed_png};
use byblos::encoded::EncodedImage;
use byblos::image::{Image, Subsample};
use byblos::{Bitmap, Error, Limits};
use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Outputs over this many bytes are written as `big <len> <sha256>` (Go's `bigOutput`).
const BIG_OUTPUT: usize = 1 << 20;

/// The result of an operation: `Ok(None)` is a failed precondition (no file).
pub type Op = Result<Option<Vec<u8>>, Error>;

static IN_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Remembers the input directory, for the operations that read a sibling file.
pub fn set_in_dir(dir: &Path) {
    let _ = IN_DIR.set(dir.to_path_buf());
}

/// The contents of `name` in the input directory, if it can be read.
pub fn read_in_dir(name: &str) -> Option<Vec<u8>> {
    std::fs::read(IN_DIR.get()?.join(name)).ok()
}

/// The lower-cased extension of `name` without the dot (Go's `strings.ToLower(filepath.Ext)`).
pub fn ext_of(name: &str) -> String {
    name.rfind('.')
        .map(|i| name[i + 1..].to_ascii_lowercase())
        .unwrap_or_default()
}

/// `baseLim`: byblos's defaults, except `MaxImagePixels = 1<<25`. `threads` is `-threads`.
pub fn base_lim(threads: usize) -> Limits {
    let mut l = Limits {
        max_image_pixels: 1 << 25,
        ..Limits::default()
    };
    if threads > 0 {
        l.max_threads = threads;
    }
    l
}

/// `tightLim`: the preset of the `-tight` operations.
pub fn tight_lim(threads: usize) -> Limits {
    let mut l = Limits {
        max_objects: 20_000,
        max_pages: 8,
        max_stream_bytes: 1 << 20,
        max_page_stream_bytes: 1 << 21,
        max_image_pixels: 1 << 20,
        max_font_program_bytes: 1 << 18,
        max_images: 2,
        max_inline_images: 2,
        max_unresolved: 2,
        max_retained_bytes: 1 << 16,
        max_encoded_image_pixels: 1 << 16,
        max_png_text_bytes: 1 << 10,
        max_raster_side: 256,
        max_raster_pixels: 1 << 14,
        max_render_font_bytes: 1 << 18,
        max_path_points: 1 << 12,
        max_fill_work: 1 << 16,
        max_gstate_depth: 4,
        max_form_depth: 1,
        max_form_invocations: 8,
        max_operands: 64,
        max_text_shows: 64,
        max_paints: 64,
        ..Limits::default()
    };
    if threads > 0 {
        l.max_threads = threads;
    }
    l
}

/// Runs one operation and writes `<name>.<op>.out` or `<name>.<op>.err` to `out`: no file for
/// `Ok(None)`, the class token and a newline for an error, `go-panic` for a panic (Go's
/// `recover()`).
pub fn emit(out: &Path, name: &str, op: &str, f: impl FnOnce() -> Op) -> std::io::Result<()> {
    let base = format!("{name}.{op}");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(None)) => Ok(()),
        Ok(Ok(Some(b))) => std::fs::write(out.join(format!("{base}.out")), b),
        Ok(Err(e)) => std::fs::write(out.join(format!("{base}.err")), format!("{}\n", e.class())),
        Err(_) => std::fs::write(out.join(format!("{base}.err")), "go-panic\n"),
    }
}

/// `raw`: the bytes, or `big <len> <sha256 hex>` and a newline over 1 MiB.
pub fn raw(b: &[u8]) -> Vec<u8> {
    if b.len() > BIG_OUTPUT {
        return format!("big {} {}\n", b.len(), sha256_hex(b)).into_bytes();
    }
    b.to_vec()
}

fn push(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
}

/// `quote`: the text format. Printable ASCII as is except `"` and `\`, `\n`, `\r`, `\t`, and every
/// other byte as `\xNN`, inside double quotes.
pub fn quote(s: &[u8]) -> String {
    let mut o = String::from("\"");
    for &c in s {
        match c {
            b'"' => o.push_str("\\\""),
            b'\\' => o.push_str("\\\\"),
            b'\n' => o.push_str("\\n"),
            b'\r' => o.push_str("\\r"),
            b'\t' => o.push_str("\\t"),
            0x20..=0x7e => o.push(char::from(c)),
            _ => o.push_str(&format!("\\x{c:02x}")),
        }
    }
    o.push('"');
    o
}

pub fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// `dw`: the canonical text of a struct result, one `key=value` line each.
#[derive(Default)]
pub struct Dw {
    pub b: Vec<u8>,
}

impl Dw {
    fn line(&mut self, k: &str, v: &str) {
        push(&mut self.b, &format!("{k}={v}\n"));
    }
    pub fn s(&mut self, k: &str, v: impl AsRef<[u8]>) {
        self.line(k, &quote(v.as_ref()));
    }
    pub fn i(&mut self, k: &str, v: i64) {
        self.line(k, &v.to_string());
    }
    pub fn b2(&mut self, k: &str, v: bool) {
        self.line(k, if v { "true" } else { "false" });
    }
    pub fn f(&mut self, k: &str, v: f64) {
        self.line(k, &format!("{:016x}", v.to_bits()));
    }
    pub fn hex(&mut self, k: &str, v: &[u8]) {
        self.line(k, &hex(v));
    }
    pub fn nil(&mut self, k: &str) {
        self.line(k, "nil");
    }
    pub fn n(&mut self, k: &str, n: usize) {
        self.line(&format!("{k}.len"), &n.to_string());
    }
    pub fn fs(&mut self, k: &str, v: &[f64]) {
        self.n(k, v.len());
        for (i, x) in v.iter().enumerate() {
            self.f(&format!("{k}.{i}"), *x);
        }
    }
    pub fn strs(&mut self, k: &str, v: &[Vec<u8>]) {
        self.n(k, v.len());
        for (i, x) in v.iter().enumerate() {
            self.s(&format!("{k}.{i}"), x);
        }
    }
}

/// `plane`: `<name> <len>` and a newline, then the bytes (no trailing newline), or
/// `<name> <len> sha256 <hex>` and a newline in place of them over 1 MiB.
fn plane(b: &mut Vec<u8>, name: &str, pix: &[u8]) {
    if pix.len() > BIG_OUTPUT {
        push(
            b,
            &format!("{name} {} sha256 {}\n", pix.len(), sha256_hex(pix)),
        );
        return;
    }
    push(b, &format!("{name} {}\n", pix.len()));
    b.extend_from_slice(pix);
}

/// Go's `YCbCrSubsampleRatio` name.
fn subsample_name(s: Subsample) -> &'static str {
    match s {
        Subsample::S444 => "YCbCrSubsampleRatio444",
        Subsample::S422 => "YCbCrSubsampleRatio422",
        Subsample::S420 => "YCbCrSubsampleRatio420",
        Subsample::S440 => "YCbCrSubsampleRatio440",
        Subsample::S411 => "YCbCrSubsampleRatio411",
        Subsample::S410 => "YCbCrSubsampleRatio410",
    }
}

/// `imageDump`: the canonical form of an image.
pub fn image_dump(img: &Image) -> Vec<u8> {
    let mut b = Vec::new();
    let r = img.bounds();
    let hdr = |b: &mut Vec<u8>, t: &str, stride: usize| {
        push(
            b,
            &format!(
                "{t} {} {} {stride}\nmin {} {}\n",
                r.dx(),
                r.dy(),
                r.x0,
                r.y0
            ),
        );
    };
    match img {
        Image::Gray(m) => {
            hdr(&mut b, "gray", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Gray16(m) => {
            hdr(&mut b, "gray16", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Rgba(m) => {
            hdr(&mut b, "rgba", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Rgba64(m) => {
            hdr(&mut b, "rgba64", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Nrgba(m) => {
            hdr(&mut b, "nrgba", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Nrgba64(m) => {
            hdr(&mut b, "nrgba64", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Cmyk(m) => {
            hdr(&mut b, "cmyk", m.stride);
            plane(&mut b, "pix", &m.pix);
        }
        Image::Paletted(m) => {
            hdr(&mut b, "paletted", m.stride);
            let mut p = Vec::with_capacity(m.palette.len().saturating_mul(4));
            for c in &m.palette {
                let (r, g, bl, a) = c.rgba();
                p.extend_from_slice(&[
                    (r >> 8) as u8,
                    (g >> 8) as u8,
                    (bl >> 8) as u8,
                    (a >> 8) as u8,
                ]);
            }
            push(
                &mut b,
                &format!("palette {} {}\n", m.palette.len(), hex(&p)),
            );
            plane(&mut b, "pix", &m.pix);
        }
        Image::YCbCr(m) => {
            hdr(&mut b, "ycbcr", m.y_stride);
            push(
                &mut b,
                &format!("ycbcr {} {}\n", m.c_stride, subsample_name(m.ratio)),
            );
            plane(&mut b, "y", &m.y);
            plane(&mut b, "cb", &m.cb);
            plane(&mut b, "cr", &m.cr);
        }
    }
    b
}

/// `bitmapDump`: `bitmap <w> <h> <stride>` and the plane `pix`.
pub fn bitmap_dump(m: &Bitmap) -> Vec<u8> {
    let mut b = Vec::new();
    push(
        &mut b,
        &format!("bitmap {} {} {}\n", m.width, m.height, m.stride),
    );
    plane(&mut b, "pix", &m.pix);
    b
}

/// `dumpEncoded`: an EncodedImage under the key prefix `p`.
pub fn dump_encoded(d: &mut Dw, p: &str, e: &EncodedImage) {
    d.i(&format!("{p}width"), e.width);
    d.i(&format!("{p}height"), e.height);
    d.i(&format!("{p}bpc"), e.bpc);
    d.s(&format!("{p}colorspace.name"), &e.color_space.name);
    d.s(&format!("{p}colorspace.base"), &e.color_space.base);
    d.i(&format!("{p}colorspace.hival"), e.color_space.hi_val);
    d.hex(&format!("{p}colorspace.lookup"), &e.color_space.lookup);
    d.s(&format!("{p}filter"), &e.filter);
    match &e.decode_parms {
        None => d.nil(&format!("{p}decodeparms")),
        Some(dp) => {
            d.i(&format!("{p}decodeparms.predictor"), dp.predictor);
            d.i(&format!("{p}decodeparms.colors"), dp.colors);
            d.i(&format!("{p}decodeparms.bpc"), dp.bits_per_component);
            d.i(&format!("{p}decodeparms.columns"), dp.columns);
        }
    }
    d.fs(&format!("{p}decode"), &e.decode);
    d.i(&format!("{p}data_len"), e.data.len() as i64);
    d.s(&format!("{p}data_sha256"), sha256_hex(&e.data));
    match &e.smask {
        None => d.nil(&format!("{p}smask")),
        Some(sm) => dump_encoded(d, &format!("{p}smask."), sm),
    }
}

/// `dumpResolution`.
pub fn dump_resolution(d: &mut Dw, r: &Resolution) {
    d.f("resolution.x", r.x);
    d.f("resolution.y", r.y);
}

/// `embedAny`: `EmbedPNGLimits`, `EmbedJPEG` or `EmbedJP2` by the input's extension.
pub fn embed_any(
    ext: &str,
    data: &[u8],
    lim: &Limits,
) -> Result<(EncodedImage, Resolution), Error> {
    match ext {
        "png" => embed_png(data, lim),
        "jpg" => embed_jpeg(data, lim),
        _ => embed_jp2(data, lim),
    }
}
