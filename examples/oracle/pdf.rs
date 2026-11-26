//! The `pdf` operations of the parity oracle (docs/parity.md, "Operations"): `inspect`,
//! `extract`, `record`, `render`, `optimize*`, `stamp`, `replace`, `edit`, `provenance-write` and
//! `provenance-read` on `.pdf` inputs. Mirrors the PDF operations of the Go oracle's `ops.go`.

use crate::common::{Dw, Op, base_lim, emit, ext_of, image_dump, raw, tight_lim};
use ::pdf::GoTime;
use byblos::editpages::{PageSource, StraightenSpec, build_from_pages};
use byblos::encoded::{ColorSpace, EncodedImage};
use byblos::extract::extract_page_raster;
use byblos::go::json::Time;
use byblos::image::{Image, Rect};
use byblos::inspect::{DocumentInfo, inspect};
use byblos::optimize::{OptimizeOptions, optimize};
use byblos::provenance::{
    PageGeometry, PageProvenance, PageStraighten, Provenance, VERSION, capabilities,
    read_provenance, record_extraction, write_provenance_at,
};
use byblos::render_page::render_page;
use byblos::stamp::{PositionedWord, TextLayer, stamp_text_layer};
use byblos::stats::{extract_stats, reset_extract_stats};
use byblos::substitute::replace_images_at;
use byblos::{Error, Limits};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// `extract` runs the first `MAX_PAGES` pages (Go's `maxPages`).
const MAX_PAGES: usize = 8;
/// `edit` takes the first `MAX_EDIT_PAGES` pages (Go's `maxEditPages`).
const MAX_EDIT_PAGES: usize = 32;

/// `fixedTime`: 2000-01-01T00:00:00Z, the clock of every run.
const PINNED: GoTime = GoTime {
    unix: 946_684_800,
    utc_offset_min: 0,
};

/// `dw.rect`: `<key>=x0 y0 x1 y1`.
fn rect(d: &mut Dw, k: &str, r: &Rect) {
    d.b.extend_from_slice(format!("{k}={} {} {} {}\n", r.x0, r.y0, r.x1, r.y1).as_bytes());
}

/// `dw.strs`.
fn strs(d: &mut Dw, k: &str, v: &[String]) {
    d.n(k, v.len());
    for (i, x) in v.iter().enumerate() {
        d.s(&format!("{k}.{i}"), x);
    }
}

/// `dumpDocument`.
fn dump_document(di: &DocumentInfo) -> Vec<u8> {
    let mut d = Dw::default();
    d.b2("encrypted", di.encrypted);
    d.b2("has_signature", di.has_signature);
    d.b2("needs_rendering", di.needs_rendering);
    d.n("pages", di.pages.len());
    for (i, p) in di.pages.iter().enumerate() {
        let k = format!("pages.{i}.");
        d.i(&format!("{k}index"), p.index as i64);
        rect(&mut d, &format!("{k}bounds"), &p.bounds);
        d.i(&format!("{k}rotate"), p.rotate);
        d.i(&format!("{k}text_chars"), p.text_chars as i64);
        d.n(&format!("{k}images"), p.images.len());
        for (j, im) in p.images.iter().enumerate() {
            let ik = format!("{k}images.{j}.");
            rect(&mut d, &format!("{ik}bounds"), &im.bounds);
            d.fs(&format!("{ik}placement"), &im.placement);
            d.f(&format!("{ik}placement_deg"), im.placement_deg);
            d.i(&format!("{ik}width"), im.width);
            d.i(&format!("{ik}height"), im.height);
            d.b2(&format!("{ik}bitonal"), im.bitonal);
            d.s(&format!("{ik}colorspace"), &im.color_space);
            d.i(&format!("{ik}bpc"), im.bpc);
            d.s(&format!("{ik}filter"), &im.filter);
            d.i(&format!("{ik}obj_nr"), im.obj_nr);
            d.b2(&format!("{ik}substitutable"), im.substitutable);
            d.b2(&format!("{ik}inline"), im.inline);
        }
        d.n(&format!("{k}diagnostics"), p.diagnostics.len());
        for (j, dg) in p.diagnostics.iter().enumerate() {
            d.s(
                &format!("{k}diagnostics.{j}.severity"),
                dg.severity.to_string(),
            );
            d.s(&format!("{k}diagnostics.{j}.message"), &dg.message);
        }
    }
    d.b
}

/// `pageCount`: the page count by `inspect` under the base limits; `None` when it fails.
fn page_count(data: &[u8], base: &Limits, cancel: &AtomicBool) -> Option<usize> {
    inspect(data, base, cancel).ok().map(|di| di.pages.len())
}

/// `opExtract`: the first `MAX_PAGES` pages (page 1 when `inspect` fails), one section each, then
/// the process-global counters (reset before the first page).
fn op_extract(data: &[u8], lim: &Limits, base: &Limits, cancel: &AtomicBool) -> Op {
    let n = page_count(data, base, cancel)
        .filter(|&n| n >= 1)
        .unwrap_or(1)
        .min(MAX_PAGES);
    reset_extract_stats();
    let mut b: Vec<u8> = Vec::new();
    for p in 1..=n {
        b.extend_from_slice(format!("== page {p}\n").as_bytes());
        // Every Go call runs under recover(): a panic is the class go-panic.
        let res = catch_unwind(AssertUnwindSafe(|| {
            extract_page_raster(data, p, lim, cancel)
        }));
        let pr = match res {
            Err(_) => {
                b.extend_from_slice(b"err go-panic\n");
                continue;
            }
            Ok(Err(e)) => {
                b.extend_from_slice(format!("err {}\n", e.class()).as_bytes());
                continue;
            }
            Ok(Ok(pr)) => pr,
        };
        let mut d = Dw::default();
        rect(&mut d, "bounds", &pr.bounds);
        rect(&mut d, "page", &pr.page);
        match &pr.raster_quad {
            None => d.nil("raster_quad"),
            Some(q) => d.fs("raster_quad", q),
        }
        d.i("obj_nr", pr.obj_nr);
        d.i("dropped_annots", pr.dropped_annots);
        d.b2("bitonal", pr.bitonal);
        d.b2("covers_page", pr.covers_page());
        b.extend_from_slice(&d.b);
        b.extend_from_slice(b"image\n");
        b.extend_from_slice(&image_dump(&pr.image));
        b.push(b'\n');
    }
    let st = extract_stats();
    b.extend_from_slice(b"== stats\n");
    let mut d = Dw::default();
    d.i("attempted", st.attempted as i64);
    d.i("extracted", st.extracted as i64);
    d.i("partial", st.partial as i64);
    d.i("diverted", st.diverted as i64);
    d.i("failed", st.failed as i64);
    // The map is sorted by name.
    d.n("reasons", st.reasons.len());
    for (i, (k, v)) in st.reasons.iter().enumerate() {
        d.s(&format!("reasons.{i}.name"), k);
        d.i(&format!("reasons.{i}.count"), *v as i64);
    }
    b.extend_from_slice(&d.b);
    Ok(Some(b))
}

/// `optimizeOp`.
fn op_optimize(data: &[u8], opts: OptimizeOptions, lim: &Limits, cancel: &AtomicBool) -> Op {
    let o = OptimizeOptions {
        now: Some(PINNED),
        ..opts
    };
    Ok(Some(raw(&optimize(data, &o, lim, cancel)?)))
}

/// `opStamp`: one word, "Parity" in the rectangle (72,72)-(272,102), on every page.
fn op_stamp(data: &[u8], lim: &Limits, base: &Limits, cancel: &AtomicBool) -> Op {
    let Some(n) = page_count(data, base, cancel).filter(|&n| n >= 1) else {
        return Ok(None);
    };
    let word = PositionedWord {
        text: "Parity".to_string(),
        bounds: Rect::new(72, 72, 272, 102),
    };
    let tl = TextLayer {
        pages: vec![vec![word]; n],
    };
    Ok(Some(raw(&stamp_text_layer(
        data,
        &tl,
        Some(PINNED),
        lim,
        cancel,
    )?)))
}

/// `replacementImage`: the 8x8 DeviceGray image that `replace` swaps in, a FlateDecode stream of
/// one stored block (the bytes do not depend on a deflate encoder).
fn replacement_image() -> EncodedImage {
    let pix: Vec<u8> = (0..64u32).map(|i| (4 * i) as u8).collect();
    let (mut a, mut b) = (1u32, 0u32);
    for &c in &pix {
        a = (a + u32::from(c)) % 65521;
        b = (b + a) % 65521;
    }
    let mut z = vec![0x78, 0x01, 0x01, 0x40, 0x00, 0xbf, 0xff];
    z.extend_from_slice(&pix);
    z.extend_from_slice(&[(b >> 8) as u8, b as u8, (a >> 8) as u8, a as u8]);
    EncodedImage {
        width: 8,
        height: 8,
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

/// `opReplace`: the first `substitutable` image (in page order) is replaced.
fn op_replace(data: &[u8], lim: &Limits, base: &Limits, cancel: &AtomicBool) -> Op {
    let Ok(di) = inspect(data, base, cancel) else {
        return Ok(None);
    };
    let mut obj = 0;
    for p in &di.pages {
        for im in &p.images {
            if obj == 0 && im.substitutable {
                obj = im.obj_nr;
            }
        }
    }
    if obj == 0 {
        return Ok(None);
    }
    let subs = BTreeMap::from([(obj, replacement_image())]);
    Ok(Some(raw(&replace_images_at(
        data,
        &subs,
        Some(PINNED),
        lim,
        cancel,
    )?)))
}

/// `opEdit`: the first `MAX_EDIT_PAGES` pages in reverse order. The first output page has
/// `/Rotate 90`, the second is straightened by 1.5 degrees, the rest have `/Rotate 0`.
fn op_edit(data: &[u8], lim: &Limits, base: &Limits, cancel: &AtomicBool) -> Op {
    let Some(n) = page_count(data, base, cancel).filter(|&n| n >= 1) else {
        return Ok(None);
    };
    let n = n.min(MAX_EDIT_PAGES);
    let pages: Vec<PageSource> = (1..=n)
        .rev()
        .enumerate()
        .map(|(i, page)| PageSource {
            source: 0,
            page,
            rotate: if i == 0 { 90 } else { 0 },
            straighten: (i == 1).then_some(StraightenSpec {
                deg: 1.5,
                crop: None,
            }),
        })
        .collect();
    let out = build_from_pages(&[data], &pages, Some(PINNED), lim, cancel)?;
    Ok(Some(raw(&out)))
}

/// `syntheticProvenance`: the record `provenance-write` stores.
fn synthetic_provenance() -> Provenance {
    Provenance {
        version: VERSION.to_string(),
        capabilities: Some(capabilities().iter().map(|s| (*s).to_string()).collect()),
        processed_at: Time::from_go_time(PINNED),
        optimized: "linearized".to_string(),
        pages: Some(vec![
            PageProvenance {
                applied: vec!["extract-raster".to_string(), "text-layer".to_string()],
                ..PageProvenance::default()
            },
            PageProvenance {
                diverted: "a<b>&c\u{2028}\u{e9}".to_string(),
                placement: vec![1.0, 0.0, 0.0, 1.0, 0.5, 2.25],
                dropped_annots: 2,
                geometry: Some(PageGeometry {
                    raster_box: [0.0, 0.0, 612.0, 792.0],
                    page_box: [0.0, 0.0, 612.0, 792.0],
                    clip_box: Some([10.0, 20.5, 300.0, 400.25]),
                    raster_quad: Some([0.0, 0.0, 612.0, 1.5, 612.25, 792.0, -0.5, 790.75]),
                }),
                straightened: Some(PageStraighten { deg: -1.25 }),
                ..PageProvenance::default()
            },
        ]),
    }
}

/// `provenanceWritten`. Go's `WriteProvenanceContext` takes no Limits: the package defaults.
fn provenance_written(data: &[u8], threads: usize, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    write_provenance_at(
        data,
        &synthetic_provenance(),
        Some(PINNED),
        &default_lim(threads),
        cancel,
    )
}

/// The caps of a Go call that takes no Limits: the defaults.
fn default_lim(threads: usize) -> Limits {
    let mut l = Limits::default();
    if threads > 0 {
        l.max_threads = threads;
    }
    l
}

/// `dumpProvenance`.
fn dump_provenance(p: &Provenance) -> Vec<u8> {
    let mut d = Dw::default();
    d.s("version", &p.version);
    strs(
        &mut d,
        "capabilities",
        p.capabilities.as_deref().unwrap_or_default(),
    );
    d.s(
        "processed_at",
        p.processed_at.format_rfc3339_nano().unwrap_or_default(),
    );
    d.s("optimized", &p.optimized);
    let pages = p.pages.as_deref().unwrap_or_default();
    d.n("pages", pages.len());
    for (i, pg) in pages.iter().enumerate() {
        let k = format!("pages.{i}.");
        strs(&mut d, &format!("{k}applied"), &pg.applied);
        d.s(&format!("{k}diverted"), &pg.diverted);
        d.fs(&format!("{k}placement"), &pg.placement);
        d.i(&format!("{k}dropped_annots"), pg.dropped_annots);
        match &pg.geometry {
            None => d.nil(&format!("{k}geometry")),
            Some(g) => {
                d.fs(&format!("{k}geometry.raster_box"), &g.raster_box);
                d.fs(&format!("{k}geometry.page_box"), &g.page_box);
                match &g.clip_box {
                    None => d.nil(&format!("{k}geometry.clip_box")),
                    Some(c) => d.fs(&format!("{k}geometry.clip_box"), c),
                }
                match &g.raster_quad {
                    None => d.nil(&format!("{k}geometry.raster_quad")),
                    Some(q) => d.fs(&format!("{k}geometry.raster_quad"), q),
                }
                d.b2(&format!("{k}geometry.covers_page"), g.covers_page());
            }
        }
        match &pg.straightened {
            None => d.nil(&format!("{k}straightened")),
            Some(s) => d.f(&format!("{k}straightened.deg"), s.deg),
        }
    }
    d.b
}

/// The `optimize-lin`, `optimize-jpeg75` and `optimize-lin-jpeg75` options (base limits only).
fn optimize_variants() -> [(&'static str, OptimizeOptions); 3] {
    let jpeg75 = OptimizeOptions {
        recompress_jpeg: true,
        jpeg_quality: 75,
        ..OptimizeOptions::default()
    };
    let lin = OptimizeOptions {
        linearize: true,
        ..OptimizeOptions::default()
    };
    [
        ("optimize-lin", lin),
        ("optimize-jpeg75", jpeg75),
        (
            "optimize-lin-jpeg75",
            OptimizeOptions {
                linearize: true,
                ..jpeg75
            },
        ),
    ]
}

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(name: &str, data: &[u8], threads: usize, out: &Path) -> std::io::Result<()> {
    if ext_of(name) != "pdf" {
        return Ok(());
    }
    let base = base_lim(threads);
    let cancel = AtomicBool::new(false);

    for (sfx, lim) in [("", base_lim(threads)), ("-tight", tight_lim(threads))] {
        emit(out, name, &format!("inspect{sfx}"), || {
            Ok(Some(dump_document(&inspect(data, &lim, &cancel)?)))
        })?;
        emit(out, name, &format!("extract{sfx}"), || {
            op_extract(data, &lim, &base, &cancel)
        })?;
        emit(out, name, &format!("record{sfx}"), || {
            let p = record_extraction(data, Some(PINNED), &lim, &cancel)?;
            Ok(Some(dump_provenance(&p)))
        })?;
        emit(out, name, &format!("render{sfx}"), || {
            let img = render_page(data, 1, 400, &lim, &cancel)?;
            Ok(Some(image_dump(&Image::Rgba(img))))
        })?;
        emit(out, name, &format!("optimize{sfx}"), || {
            op_optimize(data, OptimizeOptions::default(), &lim, &cancel)
        })?;
        if sfx.is_empty() {
            for (op, opts) in optimize_variants() {
                emit(out, name, op, || op_optimize(data, opts, &base, &cancel))?;
            }
        }
        emit(out, name, &format!("stamp{sfx}"), || {
            op_stamp(data, &lim, &base, &cancel)
        })?;
        emit(out, name, &format!("replace{sfx}"), || {
            op_replace(data, &lim, &base, &cancel)
        })?;
        emit(out, name, &format!("edit{sfx}"), || {
            op_edit(data, &lim, &base, &cancel)
        })?;
    }

    emit(out, name, "provenance-write", || {
        Ok(Some(raw(&provenance_written(data, threads, &cancel)?)))
    })?;
    emit(out, name, "provenance-read", || {
        // No file when the write fails.
        let Ok(written) = provenance_written(data, threads, &cancel) else {
            return Ok(None);
        };
        match read_provenance(&written, &default_lim(threads), &cancel)? {
            None => Ok(Some(b"nil\n".to_vec())),
            Some(p) => Ok(Some(dump_provenance(&p))),
        }
    })
}
