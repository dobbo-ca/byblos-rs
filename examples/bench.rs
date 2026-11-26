//! `bench [-threads N] <input dir>`: the Rust side of the benchmark (docs/benchmarks.md), the
//! same classes, inputs and repetitions as `oracle -bench` (tests/parity/go/oracle/bench.go). For
//! each class it takes the first `MAX_FILES` inputs (sorted by name) whose operation succeeds,
//! runs the operation on all of them once per iteration, repeats until `MIN_TIME` has passed and
//! prints `<class> <ns per iteration> <files>`. `-threads N` sets `Limits::max_threads`.
//! Setup (decode, parse) is not timed.

use byblos::build::{BuildPage, build_pdf};
use byblos::downsample::downsample;
use byblos::embed::{embed_jp2, embed_jpeg, embed_png, exif_rotation};
use byblos::extract::extract_page_raster;
use byblos::image::{Image, Rect, decode, decode_config};
use byblos::inspect::inspect;
use byblos::jbig2::{decode_generic, encode_generic};
use byblos::optimize::{OptimizeOptions, optimize};
use byblos::quantize::{quantize_indexed, quantize_png};
use byblos::render_page::render_page;
use byblos::sauvola::sauvola;
use byblos::stamp::{PositionedWord, TextLayer, stamp_text_layer};
use byblos::{Bitmap, Limits, Result};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const MIN_TIME: Duration = Duration::from_secs(1);
const MAX_FILES: usize = 8;
/// Inputs over this many bytes are not used.
const MAX_BYTES: usize = 2 << 20;
/// Images over this many declared pixels are skipped (the oracle's `maxDecodePixels`).
const MAX_DECODE_PIXELS: u64 = 1 << 22;

/// The caps and the cancel flag of every call.
struct Ctx {
    lim: Limits,
    cancel: AtomicBool,
}

struct Input {
    name: String,
    data: Vec<u8>,
}

/// One timed iteration of a class on one input.
type Job<'a> = Box<dyn Fn() -> Result<()> + 'a>;
type Prep = for<'a> fn(&'a Ctx, &'a Input) -> Option<Job<'a>>;

struct Class {
    name: &'static str,
    matches: fn(&str) -> bool,
    prep: Prep,
}

fn ext_of(name: &str) -> String {
    name.rfind('.')
        .map(|i| name[i + 1..].to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_image(name: &str) -> bool {
    matches!(ext_of(name).as_str(), "png" | "jpg" | "jp2")
        && (name.starts_with("fx-img2pdf-") || name.starts_with("gen-"))
}

fn is_pbm(name: &str) -> bool {
    ext_of(name) == "pbm" && (name.starts_with("sauvola-") || name.starts_with("fx-jbig2-"))
}

fn is_jb2(name: &str) -> bool {
    ext_of(name) == "jb2" && name.starts_with("fx-jbig2-")
}

fn is_pdf(name: &str) -> bool {
    ext_of(name) == "pdf" && (name.starts_with("corpus-") || name.starts_with("fx-real-"))
}

/// `loadImage`: the decoded image, or `None` when it is too big or does not decode.
fn load_image(c: &Ctx, data: &[u8]) -> Option<Image> {
    if let Ok(cfg) = decode_config(data, &c.lim)
        && (cfg.width as u64).saturating_mul(cfg.height as u64) > MAX_DECODE_PIXELS
    {
        return None;
    }
    decode(data, &c.lim, &c.cancel).ok()
}

fn sauvola_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let img = load_image(c, &i.data)?;
    Some(Box::new(move || sauvola(&img, &c.lim, &c.cancel).map(drop)))
}

/// The same as the oracle's `parsePBM`: a binary PBM (P4), a side 1 to 1<<14.
fn parse_pbm(data: &[u8]) -> Option<Bitmap> {
    fn token<'a>(data: &'a [u8], pos: &mut usize) -> &'a [u8] {
        while let Some(&c) = data.get(*pos) {
            if c == b'#' {
                while data.get(*pos).is_some_and(|&c| c != b'\n') {
                    *pos += 1;
                }
            } else if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                *pos += 1;
            } else {
                break;
            }
        }
        let start = *pos;
        while data.get(*pos).is_some_and(|c| !b" \t\n\r#".contains(c)) {
            *pos += 1;
        }
        data.get(start..*pos).unwrap_or_default()
    }
    fn number(t: &[u8]) -> Option<i64> {
        std::str::from_utf8(t).ok()?.parse().ok()
    }
    let mut pos = 0;
    if token(data, &mut pos) != b"P4" {
        return None;
    }
    let w = number(token(data, &mut pos))?;
    let h = number(token(data, &mut pos))?;
    let side = 1..=(1i64 << 14);
    if !side.contains(&w) || !side.contains(&h) || pos >= data.len() {
        return None;
    }
    pos += 1; // the single white-space byte
    let (width, height) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?);
    let stride = width.div_ceil(8);
    let rest = data.get(pos..)?;
    if rest.len() != stride.checked_mul(height)? {
        return None;
    }
    Some(Bitmap {
        width,
        height,
        stride,
        pix: rest.to_vec(),
    })
}

fn jbig2_encode_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let m = parse_pbm(&i.data)?;
    Some(Box::new(move || {
        encode_generic(&m, &c.lim, &c.cancel).map(drop)
    }))
}

fn jbig2_decode_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    Some(Box::new(move || {
        decode_generic(&i.data, &c.lim, &c.cancel).map(drop)
    }))
}

fn downsample_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let img = load_image(c, &i.data)?;
    Some(Box::new(move || -> Result<()> {
        for bpc in [8u8, 1] {
            downsample(&img, Some(bpc), 300.0, 150.0, &c.lim, &c.cancel)?;
        }
        Ok(())
    }))
}

fn quantize_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let img = load_image(c, &i.data)?;
    Some(Box::new(move || {
        for colors in [16usize, 256] {
            quantize_png(&img, colors, &c.lim, &c.cancel)?;
        }
        quantize_indexed(&img, 16, &c.lim, &c.cancel).map(drop)
    }))
}

fn embed_build_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let ext = ext_of(&i.name);
    Some(Box::new(move || {
        let (image, res) = match ext.as_str() {
            "png" => embed_png(&i.data, &c.lim)?,
            "jpg" => embed_jpeg(&i.data, &c.lim)?,
            _ => embed_jp2(&i.data, &c.lim)?,
        };
        // Rotate is exif_rotation, 0 when that fails.
        let rotate = exif_rotation(&i.data, &c.lim).map_or(0, i64::from);
        let page = BuildPage {
            image,
            dpi: res.x,
            dpi_y: res.y,
            rotate,
            ..BuildPage::default()
        };
        build_pdf(&[page], &c.lim, &c.cancel).map(drop)
    }))
}

fn inspect_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    Some(Box::new(move || {
        inspect(&i.data, &c.lim, &c.cancel).map(drop)
    }))
}

fn extract_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    Some(Box::new(move || {
        extract_page_raster(&i.data, 1, &c.lim, &c.cancel).map(drop)
    }))
}

fn render_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    Some(Box::new(move || {
        render_page(&i.data, 1, 400, &c.lim, &c.cancel).map(drop)
    }))
}

fn optimize_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    Some(Box::new(move || -> Result<()> {
        let variants = [
            OptimizeOptions::default(),
            OptimizeOptions {
                linearize: true,
                ..OptimizeOptions::default()
            },
            OptimizeOptions {
                recompress_jpeg: true,
                jpeg_quality: 75,
                ..OptimizeOptions::default()
            },
        ];
        for o in &variants {
            optimize(&i.data, o, &c.lim, &c.cancel)?;
        }
        Ok(())
    }))
}

fn stamp_job<'a>(c: &'a Ctx, i: &'a Input) -> Option<Job<'a>> {
    let n = inspect(&i.data, &c.lim, &c.cancel)
        .ok()
        .map(|di| di.pages.len())
        .filter(|&n| n >= 1)?;
    let word = PositionedWord {
        text: "Parity".to_string(),
        bounds: Rect::new(72, 72, 272, 102),
    };
    let tl = TextLayer {
        pages: vec![vec![word]; n],
    };
    Some(Box::new(move || {
        stamp_text_layer(&i.data, &tl, None, &c.lim, &c.cancel).map(drop)
    }))
}

fn class(name: &'static str, matches: fn(&str) -> bool, prep: Prep) -> Class {
    Class {
        name,
        matches,
        prep,
    }
}

fn classes() -> Vec<Class> {
    vec![
        class("sauvola", is_image, sauvola_job),
        class("jbig2-encode", is_pbm, jbig2_encode_job),
        class("jbig2-decode", is_jb2, jbig2_decode_job),
        class("downsample", is_image, downsample_job),
        class("quantize", is_image, quantize_job),
        class("embed-build", is_image, embed_build_job),
        class("inspect", is_pdf, inspect_job),
        class("extract", is_pdf, extract_job),
        class("render", is_pdf, render_job),
        class("optimize", is_pdf, optimize_job),
        class("stamp", is_pdf, stamp_job),
    ]
}

fn run() -> std::io::Result<()> {
    let usage = || std::io::Error::other("usage: bench [-threads N] <input dir>");
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = 0usize;
    if args.first().map(String::as_str) == Some("-threads") {
        if args.len() < 2 {
            return Err(usage());
        }
        threads = args[1].parse().map_err(|_| usage())?;
        args.drain(..2);
    }
    let [dir] = args.as_slice() else {
        return Err(usage());
    };
    let mut lim = Limits {
        max_image_pixels: 1 << 25,
        ..Limits::default()
    };
    if threads > 0 {
        lim.max_threads = threads;
    }
    let ctx = Ctx {
        lim,
        cancel: AtomicBool::new(false),
    };
    let classes = classes();

    let mut names = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_file() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    let mut inputs = Vec::new();
    for name in names {
        if !classes.iter().any(|c| (c.matches)(&name)) {
            continue;
        }
        let data = std::fs::read(std::path::Path::new(dir).join(&name))?;
        if data.len() <= MAX_BYTES {
            inputs.push(Input { name, data });
        }
    }

    for c in &classes {
        let mut jobs: Vec<Job<'_>> = Vec::new();
        for i in &inputs {
            if jobs.len() == MAX_FILES {
                break;
            }
            if !(c.matches)(&i.name) {
                continue;
            }
            if let Some(job) = (c.prep)(&ctx, i)
                && job().is_ok()
            {
                jobs.push(job);
            }
        }
        eprintln!("bench {}: {} files", c.name, jobs.len());
        if jobs.is_empty() {
            continue;
        }
        let mut iters = 0u128;
        let start = Instant::now();
        loop {
            for j in &jobs {
                let _ = j();
            }
            iters += 1;
            if start.elapsed() >= MIN_TIME {
                break;
            }
        }
        println!(
            "{} {} {}",
            c.name,
            start.elapsed().as_nanos() / iters,
            jobs.len()
        );
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bench: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
