//! Port of golang.org/x/image/font/sfnt `truetype.go`: the `loca` table and the `glyf` outlines,
//! simple and compound.

use super::fixed::{Int26_6, Point26_6};
use super::{
    Budget, Buffer, Error, Font, GlyphIndex, INVALID_GLYPH_DATA, INVALID_LOCA_TABLE,
    MAX_COMPOUND_RECURSION_DEPTH, MAX_COMPOUND_STACK_SIZE, Result, Segment, SegmentOp, Table,
    UNSUPPORTED_COMPOUND_GLYPH, u16, u32, view,
};

// Flags for simple (non-compound) glyphs.
const FLAG_ON_CURVE: u8 = 1 << 0;
const FLAG_X_SHORT_VECTOR: u8 = 1 << 1;
const FLAG_Y_SHORT_VECTOR: u8 = 1 << 2;
const FLAG_REPEAT: u8 = 1 << 3;
// The same bits are overloaded, dependent on the short vector bits.
const FLAG_POSITIVE_X_SHORT_VECTOR: u8 = 1 << 4;
const FLAG_THIS_X_IS_SAME: u8 = 1 << 4;
const FLAG_POSITIVE_Y_SHORT_VECTOR: u8 = 1 << 5;
const FLAG_THIS_Y_IS_SAME: u8 = 1 << 5;

// Flags for compound glyphs.
const FLAG_ARG1_AND2_ARE_WORDS: u16 = 1 << 0;
const FLAG_ARGS_ARE_XY_VALUES: u16 = 1 << 1;
const FLAG_WE_HAVE_A_SCALE: u16 = 1 << 3;
const FLAG_MORE_COMPONENTS: u16 = 1 << 5;
const FLAG_WE_HAVE_AN_X_AND_Y_SCALE: u16 = 1 << 6;
const FLAG_WE_HAVE_A_TWO_BY_TWO: u16 = 1 << 7;

fn mid_point(p: Point26_6, q: Point26_6) -> Point26_6 {
    Point26_6 {
        x: Int26_6(p.x.0.wrapping_add(q.x.0) / 2),
        y: Int26_6(p.y.0.wrapping_add(q.y.0) / 2),
    }
}

/// `parseLoca`: the glyph data locations, `numGlyphs + 1` of them.
pub(crate) fn parse_loca(
    src: &[u8],
    loca: &Table,
    glyf_offset: u32,
    index_to_loc_format: bool,
    num_glyphs: i32,
    budget: &mut Budget,
) -> Result<Vec<u32>> {
    let n = (num_glyphs + 1) as u32;
    let entry = if index_to_loc_format { 4 } else { 2 };
    if loca.length != entry * n {
        return Err(INVALID_LOCA_TABLE);
    }

    let mut locations = budget.vec::<u32>(n as usize)?;
    let buf = view(src, i64::from(loca.offset), i64::from(loca.length))?;
    for i in 0..n as usize {
        let at = buf.get(entry as usize * i..).unwrap_or_default();
        locations.push(if index_to_loc_format {
            u32(at).wrapping_add(glyf_offset)
        } else {
            (2 * u32::from(u16(at))).wrapping_add(glyf_offset)
        });
    }
    Ok(locations)
}

/// The glyf header length: "Each glyph begins with the following [10 byte] header".
const GLYF_HEADER_LEN: usize = 10;

/// `loadGlyf`: appends the segments of glyph `x` (font units, Y up) to the buffer.
pub(crate) fn load_glyf(
    f: &Font,
    b: &mut Buffer,
    x: GlyphIndex,
    stack_bottom: u32,
    recursion_depth: u32,
) -> Result<()> {
    let (data, _, _) = f.view_glyph_data(x)?;
    if data.is_empty() {
        return Ok(());
    }
    if data.len() < GLYF_HEADER_LEN {
        return Err(INVALID_GLYPH_DATA);
    }
    let mut index = GLYF_HEADER_LEN;

    let num_contours = u16(data) as i16;
    let mut num_points = 0usize;
    match num_contours {
        // A compound glyph.
        -1 => {}
        0 => return Ok(()),
        1.. => {
            // A simple glyph.
            index += 2 * num_contours as usize;
            if index > data.len() {
                return Err(INVALID_GLYPH_DATA);
            }
            // The +1: the value in the file is inclusive.
            num_points = 1 + usize::from(u16(data.get(index - 2..).unwrap_or_default()));
        }
        _ => return Err(INVALID_GLYPH_DATA),
    }

    if num_contours < 0 {
        return load_compound_glyf(
            f,
            b,
            data.get(GLYF_HEADER_LEN..).unwrap_or_default(),
            stack_bottom,
            recursion_depth,
        );
    }

    // Skip the hinting instructions.
    index += 2;
    if index > data.len() {
        return Err(INVALID_GLYPH_DATA);
    }
    let hints_length = usize::from(u16(data.get(index - 2..).unwrap_or_default()));
    index += hints_length;
    if index > data.len() {
        return Err(INVALID_GLYPH_DATA);
    }

    // The points are stored in columns (all the flags, then all the x coordinates, then all the
    // y coordinates). The first pass finds where each column starts.
    let flag_index = index as i32;
    let Some((x_index, y_index)) = find_xy_indexes(data, index, num_points) else {
        return Err(INVALID_GLYPH_DATA);
    };

    // The second pass decodes each (flags, x, y) tuple in row order.
    let mut g = GlyfIter {
        data,
        err: None,
        flag_index,
        x_index,
        y_index,
        end_index: GLYF_HEADER_LEN as i32,
        // The -1 on prev_end and final_end: the contour end index in the file is inclusive.
        prev_end: -1,
        final_end: num_points as i32 - 1,
        c: 0,
        num_contours: i32::from(num_contours),
        p: 0,
        n_points: 0,
        x: 0,
        y: 0,
        on: false,
        flag: 0,
        repeats: 0,
        closing: false,
        closed: false,
        first_on_curve_valid: false,
        first_off_curve_valid: false,
        last_off_curve_valid: false,
        first_on_curve: Point26_6::default(),
        first_off_curve: Point26_6::default(),
        last_off_curve: Point26_6::default(),
        seg: Segment::default(),
    };
    while g.next_contour() {
        while g.next_segment() {
            b.segs.push(g.seg)?;
        }
    }
    match g.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `findXYIndexes`: the indexes of the x and y coordinate columns, from the flags that start at
/// `index`.
fn find_xy_indexes(data: &[u8], mut index: usize, num_points: usize) -> Option<(i32, i32)> {
    let mut x_data_len = 0usize;
    let mut y_data_len = 0usize;
    let mut i = 0usize;
    loop {
        if i > num_points {
            return None;
        }
        if i == num_points {
            break;
        }

        let mut repeat_count = 1usize;
        let flag = *data.get(index)?;
        index += 1;
        if flag & FLAG_REPEAT != 0 {
            repeat_count += usize::from(*data.get(index)?);
            index += 1;
        }

        let x_size = if flag & FLAG_X_SHORT_VECTOR != 0 {
            1
        } else if flag & FLAG_THIS_X_IS_SAME == 0 {
            2
        } else {
            0
        };
        x_data_len += x_size * repeat_count;

        let y_size = if flag & FLAG_Y_SHORT_VECTOR != 0 {
            1
        } else if flag & FLAG_THIS_Y_IS_SAME == 0 {
            2
        } else {
            0
        };
        y_data_len += y_size * repeat_count;

        i += repeat_count;
    }
    if index + x_data_len + y_data_len > data.len() {
        return None;
    }
    Some((index as i32, (index + x_data_len) as i32))
}

pub(crate) fn translate_args(args: &mut [Point26_6; 3], dx: Int26_6, dy: Int26_6) {
    for a in args {
        a.x = a.x.wrapping_add(dx);
        a.y = a.y.wrapping_add(dy);
    }
}

/// `transformArgs`: an affine transformation; the `t??` values are 2.14 fixed point.
pub(crate) fn transform_args(args: &mut [Point26_6; 3], t: [i16; 4], dx: Int26_6, dy: Int26_6) {
    for a in args {
        *a = tform(t, dx, dy, *a);
    }
}

fn tform(t: [i16; 4], dx: Int26_6, dy: Int26_6, p: Point26_6) -> Point26_6 {
    const HALF: i64 = 1 << 13;
    let [txx, txy, tyx, tyy] = t.map(i64::from);
    let (px, py) = (i64::from(p.x.0), i64::from(p.y.0));
    Point26_6 {
        x: Int26_6(
            dx.0.wrapping_add(((px * txx + HALF) >> 14) as i32)
                .wrapping_add(((py * tyx + HALF) >> 14) as i32),
        ),
        y: Int26_6(
            dy.0.wrapping_add(((px * txy + HALF) >> 14) as i32)
                .wrapping_add(((py * tyy + HALF) >> 14) as i32),
        ),
    }
}

/// `loadCompoundGlyf`.
fn load_compound_glyf(
    f: &Font,
    b: &mut Buffer,
    mut data: &[u8],
    stack_bottom: u32,
    mut recursion_depth: u32,
) -> Result<()> {
    recursion_depth += 1;
    if recursion_depth == MAX_COMPOUND_RECURSION_DEPTH {
        return Err(UNSUPPORTED_COMPOUND_GLYPH);
    }

    // Read the components first: processing them can reuse the buffer.
    let mut stack_top = stack_bottom;
    loop {
        if stack_top >= MAX_COMPOUND_STACK_SIZE {
            return Err(UNSUPPORTED_COMPOUND_GLYPH);
        }
        let slot = stack_top as usize;
        stack_top += 1;
        let mut elem = super::CompoundElem::default();

        if data.len() < 4 {
            return Err(INVALID_GLYPH_DATA);
        }
        let flags = u16(data);
        elem.glyph_index = GlyphIndex(u16(data.get(2..).unwrap_or_default()));
        if flags & FLAG_ARG1_AND2_ARE_WORDS == 0 {
            if data.len() < 6 {
                return Err(INVALID_GLYPH_DATA);
            }
            elem.dx = i16::from(data.get(4).copied().unwrap_or(0) as i8);
            elem.dy = i16::from(data.get(5).copied().unwrap_or(0) as i8);
            data = data.get(6..).unwrap_or_default();
        } else {
            if data.len() < 8 {
                return Err(INVALID_GLYPH_DATA);
            }
            elem.dx = u16(data.get(4..).unwrap_or_default()) as i16;
            elem.dy = u16(data.get(6..).unwrap_or_default()) as i16;
            data = data.get(8..).unwrap_or_default();
        }

        if flags & FLAG_ARGS_ARE_XY_VALUES == 0 {
            return Err(UNSUPPORTED_COMPOUND_GLYPH);
        }
        elem.has_transform = flags
            & (FLAG_WE_HAVE_A_SCALE | FLAG_WE_HAVE_AN_X_AND_Y_SCALE | FLAG_WE_HAVE_A_TWO_BY_TWO)
            != 0;
        if elem.has_transform {
            if flags & FLAG_WE_HAVE_A_SCALE != 0 {
                if data.len() < 2 {
                    return Err(INVALID_GLYPH_DATA);
                }
                elem.xx = u16(data) as i16;
                elem.xy = 0;
                elem.yx = 0;
                elem.yy = elem.xx;
                data = data.get(2..).unwrap_or_default();
            } else if flags & FLAG_WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                if data.len() < 4 {
                    return Err(INVALID_GLYPH_DATA);
                }
                elem.xx = u16(data) as i16;
                elem.xy = 0;
                elem.yx = 0;
                elem.yy = u16(data.get(2..).unwrap_or_default()) as i16;
                data = data.get(4..).unwrap_or_default();
            } else if flags & FLAG_WE_HAVE_A_TWO_BY_TWO != 0 {
                if data.len() < 8 {
                    return Err(INVALID_GLYPH_DATA);
                }
                elem.xx = u16(data) as i16;
                elem.xy = u16(data.get(2..).unwrap_or_default()) as i16;
                elem.yx = u16(data.get(4..).unwrap_or_default()) as i16;
                elem.yy = u16(data.get(6..).unwrap_or_default()) as i16;
                data = data.get(8..).unwrap_or_default();
            }
        }
        if let Some(s) = b.compound_stack.get_mut(slot) {
            *s = elem;
        }

        if flags & FLAG_MORE_COMPONENTS == 0 {
            break;
        }
    }

    for i in stack_bottom..stack_top {
        let elem = b
            .compound_stack
            .get(i as usize)
            .copied()
            .ok_or(Error::Invalid("sfnt: compound stack out of range"))?;
        let base = b.segs.v.len();
        load_glyf(f, b, elem.glyph_index, stack_top, recursion_depth)?;
        let dx = Int26_6(i32::from(elem.dx));
        let dy = Int26_6(i32::from(elem.dy));
        let segments = b.segs.v.get_mut(base..).unwrap_or_default();
        for s in segments {
            if elem.has_transform {
                transform_args(&mut s.args, [elem.xx, elem.xy, elem.yx, elem.yy], dx, dy);
            } else {
                translate_args(&mut s.args, dx, dy);
            }
        }
    }

    Ok(())
}

/// `glyfIter`: decodes the points of a simple glyph into segments.
struct GlyfIter<'a> {
    data: &'a [u8],
    err: Option<Error>,

    // Indexes into data. See the "Decoding those points in row order" comment in `load_glyf`.
    flag_index: i32,
    x_index: i32,
    y_index: i32,

    // end_index points to the u16 that is the inclusive point index of the current contour's
    // end. prev_end is the previous contour's end. final_end should match the final contour's
    // end.
    end_index: i32,
    prev_end: i32,
    final_end: i32,

    // c and p count the current contour and point, up to num_contours and n_points.
    c: i32,
    num_contours: i32,
    p: i32,
    n_points: i32,

    // Points are what the file provides; segments are what the rasterizer consumes. Two
    // consecutive on-curve points are a line; N off-curve points between on-curve points are N
    // quadratic curves, with implicit on-curve midpoints.
    x: i16,
    y: i16,
    on: bool,
    flag: u8,
    repeats: u8,

    closing: bool,
    closed: bool,
    first_on_curve_valid: bool,
    first_off_curve_valid: bool,
    last_off_curve_valid: bool,
    first_on_curve: Point26_6,
    first_off_curve: Point26_6,
    last_off_curve: Point26_6,
    seg: Segment,
}

impl GlyfIter<'_> {
    /// The byte at `i`. `find_xy_indexes` has checked every index the iterator reaches, so the
    /// 0 for an out of range one is never read.
    fn byte(&self, i: i32) -> u8 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.data.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// The big-endian `u16` at `i`, with the same contract as [`GlyfIter::byte`].
    fn word(&self, i: i32) -> u16 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.data.get(i..))
            .map_or(0, u16)
    }

    fn next_contour(&mut self) -> bool {
        if self.c == self.num_contours {
            if self.prev_end != self.final_end {
                self.err = Some(INVALID_GLYPH_DATA);
            }
            return false;
        }
        self.c += 1;

        let end = i32::from(self.word(self.end_index));
        self.end_index += 2;
        if end <= self.prev_end || self.final_end < end {
            self.err = Some(INVALID_GLYPH_DATA);
            return false;
        }
        self.n_points = end - self.prev_end;
        self.p = 0;
        self.prev_end = end;

        self.closing = false;
        self.closed = false;
        self.first_on_curve_valid = false;
        self.first_off_curve_valid = false;
        self.last_off_curve_valid = false;

        true
    }

    fn close(&mut self) {
        match (self.first_off_curve_valid, self.last_off_curve_valid) {
            (false, false) => {
                self.closed = true;
                self.seg = Segment {
                    op: SegmentOp::LineTo,
                    args: [
                        self.first_on_curve,
                        Point26_6::default(),
                        Point26_6::default(),
                    ],
                };
            }
            (false, true) => {
                self.closed = true;
                self.seg = Segment {
                    op: SegmentOp::QuadTo,
                    args: [
                        self.last_off_curve,
                        self.first_on_curve,
                        Point26_6::default(),
                    ],
                };
            }
            (true, false) => {
                self.closed = true;
                self.seg = Segment {
                    op: SegmentOp::QuadTo,
                    args: [
                        self.first_off_curve,
                        self.first_on_curve,
                        Point26_6::default(),
                    ],
                };
            }
            (true, true) => {
                self.last_off_curve_valid = false;
                self.seg = Segment {
                    op: SegmentOp::QuadTo,
                    args: [
                        self.last_off_curve,
                        mid_point(self.last_off_curve, self.first_off_curve),
                        Point26_6::default(),
                    ],
                };
            }
        }
    }

    fn next_segment(&mut self) -> bool {
        while !self.closed {
            if self.closing || !self.next_point() {
                self.closing = true;
                self.close();
                return true;
            }

            // A straight numerical conversion, with no scaling factor: a later step scales the
            // coordinates from font units to pixels.
            let p = Point26_6 {
                x: Int26_6(i32::from(self.x)),
                y: Int26_6(i32::from(self.y)),
            };
            let z = Point26_6::default();

            if !self.first_on_curve_valid {
                if self.on {
                    self.first_on_curve = p;
                    self.first_on_curve_valid = true;
                    self.seg = Segment {
                        op: SegmentOp::MoveTo,
                        args: [p, z, z],
                    };
                    return true;
                } else if !self.first_off_curve_valid {
                    self.first_off_curve = p;
                    self.first_off_curve_valid = true;
                    continue;
                } else {
                    self.first_on_curve = mid_point(self.first_off_curve, p);
                    self.first_on_curve_valid = true;
                    self.last_off_curve = p;
                    self.last_off_curve_valid = true;
                    self.seg = Segment {
                        op: SegmentOp::MoveTo,
                        args: [self.first_on_curve, z, z],
                    };
                    return true;
                }
            } else if !self.last_off_curve_valid {
                if !self.on {
                    self.last_off_curve = p;
                    self.last_off_curve_valid = true;
                    continue;
                } else {
                    self.seg = Segment {
                        op: SegmentOp::LineTo,
                        args: [p, z, z],
                    };
                    return true;
                }
            } else if !self.on {
                self.seg = Segment {
                    op: SegmentOp::QuadTo,
                    args: [self.last_off_curve, mid_point(self.last_off_curve, p), z],
                };
                self.last_off_curve = p;
                self.last_off_curve_valid = true;
                return true;
            } else {
                self.seg = Segment {
                    op: SegmentOp::QuadTo,
                    args: [self.last_off_curve, p, z],
                };
                self.last_off_curve_valid = false;
                return true;
            }
        }
        false
    }

    fn next_point(&mut self) -> bool {
        if self.p == self.n_points {
            return false;
        }
        self.p += 1;

        if self.repeats > 0 {
            self.repeats -= 1;
        } else {
            self.flag = self.byte(self.flag_index);
            self.flag_index += 1;
            if self.flag & FLAG_REPEAT != 0 {
                self.repeats = self.byte(self.flag_index);
                self.flag_index += 1;
            }
        }

        if self.flag & FLAG_X_SHORT_VECTOR != 0 {
            let d = i16::from(self.byte(self.x_index));
            if self.flag & FLAG_POSITIVE_X_SHORT_VECTOR != 0 {
                self.x = self.x.wrapping_add(d);
            } else {
                self.x = self.x.wrapping_sub(d);
            }
            self.x_index += 1;
        } else if self.flag & FLAG_THIS_X_IS_SAME == 0 {
            self.x = self.x.wrapping_add(self.word(self.x_index) as i16);
            self.x_index += 2;
        }

        if self.flag & FLAG_Y_SHORT_VECTOR != 0 {
            let d = i16::from(self.byte(self.y_index));
            if self.flag & FLAG_POSITIVE_Y_SHORT_VECTOR != 0 {
                self.y = self.y.wrapping_add(d);
            } else {
                self.y = self.y.wrapping_sub(d);
            }
            self.y_index += 1;
        } else if self.flag & FLAG_THIS_Y_IS_SAME == 0 {
            self.y = self.y.wrapping_add(self.word(self.y_index) as i16);
            self.y_index += 2;
        }

        self.on = self.flag & FLAG_ON_CURVE != 0;
        true
    }
}
