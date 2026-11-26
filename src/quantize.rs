//! Port of byblos `quantize.go` and `quantize_indexed.go`.
//!
//! [`quantize_png`] reduces an opaque image to at most `colors` distinct colours by median cut
//! with Lloyd (k-means) refinement and returns a palette-indexed PNG file. [`quantize_indexed`]
//! runs the same core ([`quantize_core`]) and packages the result as a PDF `/Indexed`,
//! `/FlateDecode` image: the PLTE payload and the concatenated IDAT payloads of the PNG, which
//! together are the PNG-predicted scanline stream a `/Predictor 15` filter expects.
//!
//! Determinism. The histogram is sorted before use (its map order is not stable), the sorts are
//! Go's pdqsort ([`crate::go::sort::slice_by`], so ties land as in Go), and every float sum keeps
//! Go's operation order. The palette is then reordered by descending pixel population, ties by
//! ascending RGB, as pngquant does: it puts the paper colour of a scan at index 0, which lets
//! deflate match a run of zero bytes across scanlines.
//!
//! Compression level. Go compresses at `png.BestCompression` (zlib level 9), and so does the PNG
//! writer ([`crate::image::png_write`], pdf-rs's Go-exact level 9), so the compressed bytes and
//! the IDAT split of [`quantize_indexed`] are Go's.
//!
//! Limits. Go has no image-size limit here. The histogram, the work arrays and the output are
//! charged to `limits.max_alloc` before they are allocated, and `cancel` is checked per row and
//! per split or refinement step.
//!
//! Not reachable in Go either, so not ported: the `total == 0` and `len(out) == 0` fallbacks of
//! `medianCutPalette` (every box holds at least one bucket and every bucket holds a pixel).

use crate::encoded::{ColorSpace, DecodeParms, EncodedImage};
use crate::error::{Error, Result};
use crate::go::sort::slice_by;
use crate::image::png_write::encode_paletted;
use crate::image::{Color, Image, Paletted, Rect};
use crate::limits::{Limits, check_cancel};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
pub(crate) mod test_corpus;
#[cfg(test)]
mod tests;

/// One palette entry: a true 8-bit colour, the weighted mean of the pixels assigned to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb24 {
    r: u8,
    g: u8,
    bl: u8,
}

/// One distinct colour of the source histogram and its pixel count.
#[derive(Debug, Clone, Copy)]
struct HistBucket {
    r: u8,
    g: u8,
    bl: u8,
    n: usize,
}

/// A bound on the bytes one distinct colour costs across the histogram map, the bucket list, the
/// member index array and the memo map. The number of distinct colours is at most
/// `min(pixels, 2^24)`.
const BYTES_PER_COLOUR: u64 = 96;

/// `QuantizePNG`: reduces `img` to at most `colors` distinct colours and returns a complete,
/// palette-indexed PNG file.
///
/// `img` must be opaque and `colors` must be 2..=256 (pngquant's own range; clamping would
/// silently substitute a different request).
///
/// # Errors
/// [`Error::Malformed`] for empty bounds, `colors` outside 2..=256 or a pixel that is not
/// opaque; [`Error::Alloc`] / [`Error::OutOfMemory`] when a buffer is over `limits.max_alloc` or
/// the allocator refuses; [`Error::Cancelled`].
pub fn quantize_png(
    img: &Image,
    colors: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let out = quantize_core(img, colors, limits, cancel)?;
    encode_paletted(&out, &without(limits, out.pix.len()), cancel)
}

/// `QuantizeIndexed`: [`quantize_png`]'s palette and pixels as a PDF `/Indexed`, `/FlateDecode`
/// image. `data` is the raw PNG-predicted, deflate-compressed scanline stream (not a PNG file),
/// `/DecodeParms` has `/Predictor 15`, and the bit depth follows the palette length.
///
/// # Errors
/// As [`quantize_png`].
pub fn quantize_indexed(
    img: &Image,
    colors: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<EncodedImage> {
    let out = quantize_core(img, colors, limits, cancel)?;
    let png = encode_paletted(&out, &without(limits, out.pix.len()), cancel)?;
    let (b, colours) = (out.rect, out.palette.len());
    drop(out);
    let (plte, idat) = extract_plte_and_idat(&png, limits)?;

    let depth = palette_bit_depth(colours);
    Ok(EncodedImage {
        width: b.dx(),
        height: b.dy(),
        bpc: depth,
        color_space: ColorSpace {
            name: "Indexed".into(),
            base: "DeviceRGB".into(),
            hi_val: colours as i64 - 1,
            lookup: plte,
        },
        filter: "FlateDecode".into(),
        decode_parms: Some(DecodeParms {
            predictor: 15,
            colors: 1,
            bits_per_component: depth,
            columns: b.dx(),
        }),
        data: idat,
        ..Default::default()
    })
}

/// `limits` with `held` bytes already alive taken off `max_alloc`, so a stage with its own ledger
/// still answers to one overall budget.
fn without(limits: &Limits, held: usize) -> Limits {
    Limits {
        max_alloc: limits.alloc_limit().saturating_sub(held as u64).max(1),
        ..*limits
    }
}

/// `paletteBitDepth`: image/png's own palette bit-depth choice, 1, 2, 4 or 8 bits per sample for
/// palette lengths of at most 2, 4, 16 or more.
pub fn palette_bit_depth(n: usize) -> i64 {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// `quantizeCore`: the median-cut, Lloyd and population-order core shared by [`quantize_png`] and
/// [`quantize_indexed`]. Returns the paletted image at origin (0, 0).
///
/// # Errors
/// As [`quantize_png`].
pub fn quantize_core(
    img: &Image,
    colors: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Paletted> {
    check_cancel(cancel)?;
    let b = img.bounds();
    if b.is_empty() {
        return Err(Error::Malformed(format!(
            "byblos: quantize: image bounds ({},{})-({},{}) are empty",
            b.x0, b.y0, b.x1, b.y1
        )));
    }
    if !(2..=256).contains(&colors) {
        return Err(Error::Malformed(format!(
            "byblos: quantize: colors {colors} is outside 2..256"
        )));
    }

    let over = || Error::Alloc {
        got: u64::MAX,
        max: limits.alloc_limit(),
    };
    let w = usize::try_from(b.dx()).map_err(|_| over())?;
    let h = usize::try_from(b.dy()).map_err(|_| over())?;
    let npix = w.checked_mul(h).ok_or_else(over)?;
    let mem = limits.mem();
    let _out_held = mem.charge(npix as u64)?;
    // The per-colour working set (histogram, buckets, index arrays, memo) is charged as the
    // histogram grows, in doubling steps, so it follows the distinct colours actually present.
    let mut work_held = Vec::new();
    let mut covered = 0u64;

    // Histogram at full 24-bit precision: packed 0xRRGGBB -> pixel count.
    let mut hist: HashMap<u32, usize> = HashMap::new();
    for y in b.y0..b.y1 {
        check_cancel(cancel)?;
        for x in b.x0..b.x1 {
            let (r, g, bl, a) = img.rgba_at(x, y);
            if a != 0xffff {
                return Err(Error::Malformed(format!(
                    "byblos: quantize: image is not opaque at ({x},{y})"
                )));
            }
            let packed = ((r >> 8) << 16) | ((g >> 8) << 8) | (bl >> 8);
            if let Some(n) = hist.get_mut(&packed) {
                *n += 1;
            } else {
                if hist.len() as u64 >= covered {
                    let step = covered.max(1024);
                    work_held.push(mem.charge(step.saturating_mul(BYTES_PER_COLOUR))?);
                    covered += step;
                }
                hist.try_reserve(1)?;
                hist.insert(packed, 1);
            }
        }
    }

    let mut buckets: Vec<HistBucket> = Vec::new();
    buckets.try_reserve_exact(hist.len())?;
    for (&packed, &n) in &hist {
        buckets.push(HistBucket {
            r: (packed >> 16) as u8,
            g: (packed >> 8) as u8,
            bl: packed as u8,
            n,
        });
    }
    drop(hist);
    // Sort for determinism: map iteration order is random, and every following step (box
    // splitting, palette assignment) must not depend on it.
    slice_by(&mut buckets, |a, b| (a.r, a.g, a.bl) < (b.r, b.g, b.bl));

    let mut palette = median_cut_palette(&buckets, colors, cancel)?;

    // Lloyd (k-means) refinement, work-budgeted.
    let iters = 20_000_000usize
        .checked_div(buckets.len().saturating_mul(palette.len()))
        .unwrap_or(1)
        .clamp(1, 10);
    palette = lloyd_refine(&buckets, &palette, iters, cancel)?;

    // Reorder the palette by descending pixel population (ties by ascending RGB), matching
    // pngquant. The tally is computed explicitly because lloyd_refine can return early once
    // nothing moves, when its last accumulators belong to the previous iteration's palette.
    let mut pixel_count = vec![0usize; palette.len()];
    for bkt in &buckets {
        if let Some(c) = pixel_count.get_mut(nearest_palette_index(&palette, bkt.r, bkt.g, bkt.bl))
        {
            *c += bkt.n;
        }
    }
    let mut order: Vec<usize> = (0..palette.len()).collect();
    slice_by(&mut order, |&oi, &oj| {
        if pixel_count[oi] != pixel_count[oj] {
            return pixel_count[oi] > pixel_count[oj];
        }
        let (ci, cj) = (palette[oi], palette[oj]);
        (ci.r, ci.g, ci.bl) < (cj.r, cj.g, cj.bl)
    });
    let unordered = palette;
    let mut reordered = Vec::with_capacity(unordered.len());
    let mut rank = vec![0u8; unordered.len()]; // old (unordered) index -> new index
    for (new_idx, &old_idx) in order.iter().enumerate() {
        reordered.push(unordered[old_idx]);
        rank[old_idx] = new_idx as u8;
    }

    // Build the output image, mapping each pixel to its nearest palette entry, memoized on the
    // exact 24-bit colour. The search runs against `unordered` and is remapped through `rank`:
    // nearest_palette_index breaks exact ties by earliest array position, so searching the
    // reordered palette would let the reorder change which entry a tied pixel lands on.
    let pal: Vec<Color> = reordered
        .iter()
        .map(|c| Color::Rgba {
            r: c.r,
            g: c.g,
            b: c.bl,
            a: 255,
        })
        .collect();
    let mut out = Paletted::new(Rect::new(0, 0, b.dx(), b.dy()), pal, limits)?;

    let mut nearest: HashMap<u32, u8> = HashMap::new();
    for (yy, y) in (b.y0..b.y1).enumerate() {
        check_cancel(cancel)?;
        for (xx, x) in (b.x0..b.x1).enumerate() {
            let (r, g, bl, _) = img.rgba_at(x, y);
            let packed = ((r >> 8) << 16) | ((g >> 8) << 8) | (bl >> 8);
            let idx = if let Some(&i) = nearest.get(&packed) {
                i
            } else {
                let i = rank[nearest_palette_index(
                    &unordered,
                    (r >> 8) as u8,
                    (g >> 8) as u8,
                    (bl >> 8) as u8,
                )];
                nearest.try_reserve(1)?;
                nearest.insert(packed, i);
                i
            };
            let off = yy * out.stride + xx;
            if let Some(p) = out.pix.get_mut(off) {
                *p = idx;
            }
        }
    }
    Ok(out)
}

/// The summed pixel count of the buckets `members` name.
fn pop_of(buckets: &[HistBucket], members: &[usize]) -> usize {
    members.iter().map(|&i| buckets[i].n).sum()
}

/// The population-weighted mean colour of `members` and their pixel count.
fn mean_of(buckets: &[HistBucket], members: &[usize]) -> (f64, f64, f64, usize) {
    let (mut sr, mut sg, mut sb) = (0f64, 0f64, 0f64);
    let mut total = 0usize;
    for &i in members {
        let bkt = buckets[i];
        let w = bkt.n as f64;
        sr += w * f64::from(bkt.r);
        sg += w * f64::from(bkt.g);
        sb += w * f64::from(bkt.bl);
        total += bkt.n;
    }
    if total == 0 {
        return (0.0, 0.0, 0.0, 0);
    }
    let t = total as f64;
    (sr / t, sg / t, sb / t, total)
}

/// The population-weighted variance of `members` around their mean.
fn variance_of(buckets: &[HistBucket], members: &[usize]) -> f64 {
    let (mr, mg, mb, total) = mean_of(buckets, members);
    if total == 0 {
        return 0.0;
    }
    let mut v = 0f64;
    for &i in members {
        let bkt = buckets[i];
        let dr = f64::from(bkt.r) - mr;
        let dg = f64::from(bkt.g) - mg;
        let db = f64::from(bkt.bl) - mb;
        v += bkt.n as f64 * (dr * dr + dg * dg + db * db);
    }
    v
}

/// `medianCutPalette`: median cut over `buckets` (a histogram sorted for determinism) down to at
/// most `n` colours.
///
/// Go gives each box its own slice of bucket indices. Here every box is a range of one index
/// array, sorted in place: Go sorts a copy of the box's members and splits that copy, so the
/// member order of both halves is the same, and the float sums that follow see the same order.
fn median_cut_palette(buckets: &[HistBucket], n: usize, cancel: &AtomicBool) -> Result<Vec<Rgb24>> {
    let mut idx: Vec<usize> = Vec::new();
    idx.try_reserve_exact(buckets.len())?;
    idx.extend(0..buckets.len());
    let mut boxes: Vec<(usize, usize)> = vec![(0, idx.len())];

    while boxes.len() < n {
        check_cancel(cancel)?;
        // The box with the greatest population-weighted variance that can still be split (more
        // than one distinct bucket).
        let mut best: Option<usize> = None;
        let mut best_var = -1.0f64;
        for (i, &(s, e)) in boxes.iter().enumerate() {
            let members = &idx[s..e];
            if members.len() < 2 {
                continue;
            }
            let v = variance_of(buckets, members);
            if v > best_var {
                best_var = v;
                best = Some(i);
            }
        }
        let Some(best) = best else {
            break; // nothing left worth splitting
        };
        let (s, e) = boxes[best];
        let members = &mut idx[s..e];

        // Split axis: greatest range in the box.
        let (mut min_r, mut max_r) = (255u8, 0u8);
        let (mut min_g, mut max_g) = (255u8, 0u8);
        let (mut min_b, mut max_b) = (255u8, 0u8);
        for &i in members.iter() {
            let bkt = buckets[i];
            min_r = min_r.min(bkt.r);
            max_r = max_r.max(bkt.r);
            min_g = min_g.min(bkt.g);
            max_g = max_g.max(bkt.g);
            min_b = min_b.min(bkt.bl);
            max_b = max_b.max(bkt.bl);
        }
        let range_r = i32::from(max_r) - i32::from(min_r);
        let range_g = i32::from(max_g) - i32::from(min_g);
        let range_b = i32::from(max_b) - i32::from(min_b);

        if range_r >= range_g && range_r >= range_b {
            slice_by(members, |&a, &b| buckets[a].r < buckets[b].r);
        } else if range_g >= range_r && range_g >= range_b {
            slice_by(members, |&a, &b| buckets[a].g < buckets[b].g);
        } else {
            slice_by(members, |&a, &b| buckets[a].bl < buckets[b].bl);
        }

        // Split at the population-weighted median. A box has two or more members here.
        let total = pop_of(buckets, members);
        let half = total / 2;
        let mut cum = 0usize;
        let mut split = members.len() - 1;
        for (i, &mi) in members.iter().enumerate() {
            cum += buckets[mi].n;
            if cum >= half {
                split = i;
                break;
            }
        }
        if split >= members.len() - 1 {
            split = members.len() - 2;
        }
        boxes[best] = (s, s + split + 1);
        boxes.push((s + split + 1, e));
    }

    let mut out = Vec::with_capacity(boxes.len());
    for &(s, e) in &boxes {
        let (mr, mg, mb, _) = mean_of(buckets, &idx[s..e]);
        out.push(Rgb24 {
            r: (mr + 0.5) as u8,
            g: (mg + 0.5) as u8,
            bl: (mb + 0.5) as u8,
        });
    }
    Ok(out)
}

/// `lloydRefine`: assigns each histogram bucket to its nearest palette entry and moves each entry
/// to the weighted mean of what it won, `iters` times, stopping early when nothing moves.
fn lloyd_refine(
    buckets: &[HistBucket],
    palette: &[Rgb24],
    iters: usize,
    cancel: &AtomicBool,
) -> Result<Vec<Rgb24>> {
    #[derive(Clone, Copy, Default)]
    struct Accum {
        sr: f64,
        sg: f64,
        sb: f64,
        n: usize,
    }
    let mut pal = palette.to_vec();
    for _ in 0..iters {
        let mut accums = vec![Accum::default(); pal.len()];
        let mut moved = false;
        for (i, bkt) in buckets.iter().enumerate() {
            if i & 0xfff == 0 {
                check_cancel(cancel)?;
            }
            let best = nearest_palette_index(&pal, bkt.r, bkt.g, bkt.bl);
            let w = bkt.n as f64;
            let a = &mut accums[best];
            a.sr += w * f64::from(bkt.r);
            a.sg += w * f64::from(bkt.g);
            a.sb += w * f64::from(bkt.bl);
            a.n += bkt.n;
        }
        let mut next = Vec::with_capacity(pal.len());
        for (a, old) in accums.iter().zip(pal.iter()) {
            if a.n == 0 {
                next.push(*old); // no pixels assigned: keep it where it was
                continue;
            }
            let n = a.n as f64;
            let nr = (a.sr / n + 0.5) as u8;
            let ng = (a.sg / n + 0.5) as u8;
            let nb = (a.sb / n + 0.5) as u8;
            if nr != old.r || ng != old.g || nb != old.bl {
                moved = true;
            }
            next.push(Rgb24 {
                r: nr,
                g: ng,
                bl: nb,
            });
        }
        pal = next;
        if !moved {
            break;
        }
    }
    Ok(pal)
}

/// `nearestPaletteIndex`: the index of the palette entry nearest to (r, g, b) by unweighted
/// Euclidean RGB distance; an exact tie goes to the earliest entry.
fn nearest_palette_index(pal: &[Rgb24], r: u8, g: u8, b: u8) -> usize {
    let mut best = 0;
    let mut best_d = -1i32;
    for (i, c) in pal.iter().enumerate() {
        let dr = i32::from(c.r) - i32::from(r);
        let dg = i32::from(c.g) - i32::from(g);
        let db = i32::from(c.bl) - i32::from(b);
        let d = dr * dr + dg * dg + db * db;
        if best_d < 0 || d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// `pngChunk`: one chunk out of `walk_png_chunks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngChunk<'a> {
    pub typ: [u8; 4],
    pub data: &'a [u8],
    pub crc_ok: bool,
}

/// `walkPNGChunks`: walks the chunk framing of `png_data` (ISO/IEC 15948), skipping the 8-byte
/// signature without checking it, and returns every chunk up to and including `IEND`. Chunks
/// after `IEND` are not part of the image and are ignored. It is shared with the PNG embedder
/// so the framing walk and its bounds checks exist once.
///
/// # Errors
/// [`Error::Malformed`] for fewer than 8 bytes ("not a PNG file") or a truncated chunk,
/// [`Error::OutOfMemory`].
pub(crate) fn walk_png_chunks(png_data: &[u8]) -> Result<Vec<PngChunk<'_>>> {
    const SIG_LEN: usize = 8;
    let Some(mut p) = png_data.get(SIG_LEN..) else {
        return Err(Error::Malformed("not a PNG file".into()));
    };
    let mut chunks = Vec::new();
    while p.len() >= 8 {
        let length = u64::from(u32::from_be_bytes([p[0], p[1], p[2], p[3]]));
        let typ = [p[4], p[5], p[6], p[7]];
        if length + 12 > p.len() as u64 {
            return Err(Error::Malformed(format!(
                "truncated {} chunk",
                String::from_utf8_lossy(&typ)
            )));
        }
        let n = length as usize; // length + 12 <= p.len(), so it fits
        let want = u32::from_be_bytes([p[8 + n], p[9 + n], p[10 + n], p[11 + n]]);
        chunks.try_reserve(1)?;
        chunks.push(PngChunk {
            typ,
            data: &p[8..8 + n],
            crc_ok: crc32(&p[4..8 + n]) == want,
        });
        p = &p[8 + n + 4..];
        if typ == *b"IEND" {
            break;
        }
    }
    Ok(chunks)
}

use crate::go::crc32::checksum as crc32;

/// `extractPLTEAndIDAT`: the `PLTE` payload and the concatenation of every `IDAT` payload of
/// `png_data`. Every `IDAT` is needed, not just the first: image/png splits its zlib stream
/// across chunks once it outgrows an internal buffer, and the chunks are one continuous stream.
/// The concatenation is charged to `limits.max_alloc`.
///
/// # Errors
/// [`Error::Malformed`] when the framing is bad or there is no `PLTE` or no `IDAT`,
/// [`Error::Alloc`], [`Error::OutOfMemory`].
fn extract_plte_and_idat(png_data: &[u8], limits: &Limits) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut plte: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    let mem = limits.mem();
    let _held = mem.charge(png_data.len() as u64)?;
    for c in walk_png_chunks(png_data)? {
        match &c.typ {
            b"PLTE" => {
                plte.clear();
                plte.try_reserve_exact(c.data.len())?;
                plte.extend_from_slice(c.data);
            }
            b"IDAT" => {
                idat.try_reserve(c.data.len())?;
                idat.extend_from_slice(c.data);
            }
            _ => {}
        }
    }
    if plte.is_empty() {
        return Err(Error::Malformed(
            "byblos: quantizeindexed: no PLTE chunk in encoded PNG".into(),
        ));
    }
    if idat.is_empty() {
        return Err(Error::Malformed(
            "byblos: quantizeindexed: no IDAT chunk in encoded PNG".into(),
        ));
    }
    Ok((plte, idat))
}
