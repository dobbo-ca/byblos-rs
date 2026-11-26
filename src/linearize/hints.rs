//! Port of byblos `internal/linearize/hints.go` and `hints_test.go`: the primary hint stream
//! (ISO 32000-1:2008 Annex F.4).
//!
//! Two tables are emitted: the page offset hint table and the shared object hint table, plus
//! the generic outline table when the document has outlines. The thumbnail, thread,
//! named-destination, logical-structure, page-label, renumbering and embedded-file tables are
//! all optional and are not emitted; nothing reads them (qpdf's checker skips every table it is
//! not given).
//!
//! LAYOUT RULES. Every table is a fixed-width big-endian header followed by bit-packed COLUMNS,
//! not rows. Table F.3 lists the per-page items in an order that reads like a row per page, and
//! the natural misreading of it (write page 0's items, then page 1's) produces a payload of
//! exactly the right length with entirely wrong bytes. The accumulator is MSB-first and is
//! flushed to a byte boundary after each column, never within one and never only at the end of
//! the table.
//!
//! Bit widths come from [`nbits`], which is 0 for a zero value: a column whose values are all
//! equal to the header's minimum occupies no bytes at all.
//!
//! The tests pin both facts against two byte vectors.
//!
//! Go's `int` is `i64` here and Go's wrapping arithmetic is kept (`wrapping_sub`), so a hostile
//! table cannot panic the encoder.

use super::malformed;
use crate::error::Result;

/// `PageHint`: one page's row of the page offset hint table.
///
/// `length` is the number of bytes the page's objects occupy, i.e. the sum over the page's
/// `n_objects` consecutive object numbers of each object's serialized extent. `shared_ids` are
/// indices into `Hints::shared`, and must be empty for page 0: Table F.4 item 3 defines the
/// first page's shared objects implicitly, and qpdf warns "page 0 has shared identifier
/// entries" for a file that lists them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageHint {
    pub n_objects: i64,
    pub length: i64,
    pub shared_ids: Vec<i64>,
}

/// `SharedHint`: one group of the shared object hint table. Every group here is a single
/// object, so the table's "number of objects in the group" column is zero-width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SharedHint {
    pub group_length: i64,
}

/// `OutlineHint`: the generic hint table Annex F.4.6 defines for the outline tree. It is
/// emitted only when the document has one; the hint stream dictionary's `/O` names its offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutlineHint {
    pub first_object: i64,
    pub first_object_offset: i64,
    pub n_objects: i64,
    pub group_length: i64,
}

/// `Hints`: everything the primary hint stream is computed from.
///
/// `first_page_offset`, like every offset stored in a hint table, is measured as if the primary
/// hint stream were not present (Annex F.4): it is the offset the first page's first object had
/// before the hint stream was spliced in, not its offset in the finished file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hints {
    pub pages: Vec<PageHint>,
    pub first_page_offset: i64,
    pub shared: Vec<SharedHint>,
    pub n_shared_first_page: i64,
    pub first_shared_obj: i64,
    pub first_shared_offset: i64,
    pub outline: Option<OutlineHint>,
}

/// `Hint`: an encoded primary hint stream.
///
/// `s` and `o` are the `/S` and `/O` entries of the hint stream dictionary: the offsets, within
/// `payload`, of the shared object table and of the outline table. `o` is 0 when there is no
/// outline table, and `/O` must then be omitted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hint {
    pub payload: Vec<u8>,
    pub s: i64,
    pub o: i64,
}

/// `sharedDenominator` is item 13 of Table F.3. The numerator column is zero-width, so the
/// fraction is always 0/4 and the denominator is inert; 4 is what qpdf writes and what its
/// checker was measured against.
const SHARED_DENOMINATOR: i64 = 4;

/// `EncodeHints`: builds the primary hint stream payload.
///
/// # Errors
/// [`crate::Error::Malformed`] for no pages, for shared identifiers on page 0, and for more
/// first-page shared groups than groups.
pub fn encode_hints(h: &Hints) -> Result<Hint> {
    let Some(first) = h.pages.first() else {
        return Err(malformed("linearize: hint tables need at least one page"));
    };
    if !first.shared_ids.is_empty() {
        // Not defensive: an encoder that treats page 0 like the others changes both the
        // nshared_objects column and the identifier column's length, and qpdf rejects the
        // result.
        return Err(malformed(format!(
            "linearize: page 0 must carry no shared identifiers, has {}",
            first.shared_ids.len()
        )));
    }
    if h.n_shared_first_page > h.shared.len() as i64 {
        return Err(malformed(format!(
            "linearize: {} first-page shared groups out of {}",
            h.n_shared_first_page,
            h.shared.len()
        )));
    }

    let mut w = BitWriter::default();
    write_page_offset_table(&mut w, h);
    let s = w.out.len() as i64;
    write_shared_object_table(&mut w, h);
    let mut o = 0;
    if let Some(ol) = &h.outline {
        o = w.out.len() as i64;
        w.uint32(ol.first_object);
        w.uint32(ol.first_object_offset);
        w.uint32(ol.n_objects);
        w.uint32(ol.group_length);
    }
    Ok(Hint {
        payload: w.out,
        s,
        o,
    })
}

fn write_page_offset_table(w: &mut BitWriter, h: &Hints) {
    let (min_objects, max_objects) = extent(&h.pages, |p| p.n_objects);
    let (min_length, max_length) = extent(&h.pages, |p| p.length);
    let (_, max_shared) = extent(&h.pages, |p| p.shared_ids.len() as i64);

    let nbits_objects = nbits(max_objects.wrapping_sub(min_objects));
    let nbits_length = nbits(max_length.wrapping_sub(min_length));
    let nbits_shared = nbits(max_shared);
    let nbits_ident = nbits(h.shared.len() as i64);

    // Items 1-13 of Table F.3. Items 6-9 describe the first content stream of each page; a
    // conforming reader may treat the whole page as one unit, so the content offset column is
    // zero-width from a minimum of 0 and the content length column mirrors the page length
    // column exactly. That is what qpdf writes (calculateHPageOffset) and what its checker
    // accepts.
    w.uint32(min_objects); // 1  least number of objects in a page
    w.uint32(h.first_page_offset); // 2  location of the first page's first object
    w.uint16(nbits_objects); // 3  bits for the object-count delta
    w.uint32(min_length); // 4  least page length
    w.uint16(nbits_length); // 5  bits for the page-length delta
    w.uint32(0); // 6  least content stream offset
    w.uint16(0); // 7  bits for the content-offset delta
    w.uint32(min_length); // 8  least content stream length
    w.uint16(nbits_length); // 9  bits for the content-length delta
    w.uint16(nbits_shared); // 10 bits for a page's shared-object count
    w.uint16(nbits_ident); // 11 bits for a shared-object identifier
    w.uint16(0); // 12 bits for a shared-object numerator
    w.uint16(SHARED_DENOMINATOR); // 13 shared-object denominator

    for p in &h.pages {
        w.bits(p.n_objects.wrapping_sub(min_objects), nbits_objects);
    }
    w.flush();
    for p in &h.pages {
        w.bits(p.length.wrapping_sub(min_length), nbits_length);
    }
    w.flush();
    for p in &h.pages {
        w.bits(p.shared_ids.len() as i64, nbits_shared);
    }
    w.flush();
    for p in &h.pages {
        for &id in &p.shared_ids {
            w.bits(id, nbits_ident);
        }
    }
    w.flush();
    // Shared numerators: zero-width, so nothing is written, but the flush is kept so the
    // column structure reads as it is defined.
    w.flush();
    // Content offsets: zero-width for the same reason as item 7.
    w.flush();
    for p in &h.pages {
        w.bits(p.length.wrapping_sub(min_length), nbits_length);
    }
    w.flush();
}

fn write_shared_object_table(w: &mut BitWriter, h: &Hints) {
    let (mut min_group, mut max_group) = (0i64, 0i64);
    for (i, s) in h.shared.iter().enumerate() {
        if i == 0 || s.group_length < min_group {
            min_group = s.group_length;
        }
        if s.group_length > max_group {
            max_group = s.group_length;
        }
    }
    let nbits_group = nbits(max_group.wrapping_sub(min_group));

    // Table F.5 items 1-7. Item 5, the bits needed for a group's object count minus one, is 0
    // because every group here holds exactly one object.
    w.uint32(h.first_shared_obj);
    w.uint32(h.first_shared_offset);
    w.uint32(h.n_shared_first_page);
    w.uint32(h.shared.len() as i64);
    w.uint16(0);
    w.uint32(min_group);
    w.uint16(nbits_group);

    for s in &h.shared {
        w.bits(s.group_length.wrapping_sub(min_group), nbits_group);
    }
    w.flush();
    // Signature present: one bit per group, always 0. Annex F.4.5 makes the signature optional
    // and nothing consumes it.
    for _ in &h.shared {
        w.bits(0, 1);
    }
    w.flush();
    // Object count minus one: zero-width, see item 5 above.
    w.flush();
}

/// `extent`: the least and the greatest of `f(x)`. As in Go the greatest starts at 0, so it is
/// never below 0.
fn extent<T>(xs: &[T], f: impl Fn(&T) -> i64) -> (i64, i64) {
    let (mut lo, mut hi) = (0i64, 0i64);
    for (i, x) in xs.iter().enumerate() {
        let v = f(x);
        if i == 0 || v < lo {
            lo = v;
        }
        if v > hi {
            hi = v;
        }
    }
    (lo, hi)
}

/// `nbits`: the number of bits needed to represent `v`, with `nbits(0) == 0`.
///
/// The zero case is not a rounding choice: a column whose values are all equal to the header
/// minimum has width 0 and occupies no bytes, and an encoder that returned 1 there would emit
/// an extra flush byte per column and three wrong header fields on a single-page document.
pub fn nbits(v: i64) -> i64 {
    let mut n = 0;
    let mut v = v;
    while v > 0 {
        n += 1;
        v >>= 1;
    }
    n
}

/// `bitWriter`: appends fixed-width values MSB-first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    /// Bits currently held in `acc`, always < 8 outside `bits`.
    n_acc: u32,
}

impl BitWriter {
    fn bits(&mut self, v: i64, n: i64) {
        if n <= 0 {
            return;
        }
        for i in (0..n.min(63)).rev() {
            self.acc = (self.acc << 1) | u64::from(((v >> i) & 1) as u8);
            self.n_acc += 1;
            if self.n_acc == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n_acc = 0;
            }
        }
    }

    /// Pads the accumulator out to a byte boundary with zeroes. It is a no-op on an
    /// already-aligned writer, which is what makes it safe to call after a zero-width column.
    fn flush(&mut self) {
        if self.n_acc == 0 {
            return;
        }
        self.out.push((self.acc << (8 - self.n_acc)) as u8);
        self.acc = 0;
        self.n_acc = 0;
    }

    fn uint32(&mut self, v: i64) {
        self.flush();
        self.out.extend_from_slice(&(v as u32).to_be_bytes());
    }

    fn uint16(&mut self, v: i64) {
        self.flush();
        self.out.extend_from_slice(&(v as u16).to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of hints_test.go: the primary hint stream's bit packing, pinned by two fixed byte
    // vectors.
    //
    // WHY BYTE VECTORS AND NOT "COMPARE TO QPDF". The hint tables are written column-major,
    // MSB-first, with the bit accumulator flushed to a byte boundary after EVERY column (qpdf
    // QPDF_linearization.cc write_vector_int). Table F.3 lists the per-page items in an order
    // that reads like rows, so the natural misreading is a row-major encoder, which produces a
    // payload of exactly the right LENGTH and entirely wrong bytes. A length check would pass.
    // Only fixed expected bytes catch it, and only a fixed vector runs when qpdf is not
    // installed.
    //
    // Specifically, the vectors catch (a) row-major instead of column-major layout, (b)
    // LSB-first bit order within a value, (c) flushing per table or per row instead of per
    // column, (d) nbits(0) == 1 instead of 0, (e) deriving the header's minima or bit widths
    // from the wrong extremum.
    //
    // PROVENANCE. Both were encoded by hand from the field layout in ISO 32000-1 Tables F.3 and
    // F.5, from the stated inputs and nothing else, and re-derived by a second bit-packer.
    // They are not extracted from a qpdf-linearized file.

    /// The /S value is the byte offset of the shared-object hint table within the payload, i.e.
    /// the length of the page-offset hint table.
    const WANT_S_SINGLE_PAGE: i64 = 36;
    const WANT_S_THREE_PAGE: i64 = 57;

    /// A malformed literal in this file is a bug in the test, not a result.
    fn must_hex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2), "odd hex literal");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Reports the first differing byte rather than dumping two blobs, because the failure that
    /// matters (column-major vs row-major) diverges at a known offset and the offset is the
    /// diagnosis.
    fn assert_payload(got: &[u8], want: &[u8]) {
        if got == want {
            return;
        }
        for i in 0..got.len().min(want.len()) {
            assert!(
                got[i] == want[i],
                "payload differs at byte {i}: got {:#04x}, want {:#04x} (byte {i} is in the {})",
                got[i],
                want[i],
                which_region(i)
            );
        }
        panic!(
            "payload lengths differ: got {}, want {}",
            got.len(),
            want.len()
        );
    }

    /// Names the structure a differing byte offset falls in. The header is a fixed 36 bytes for
    /// every document (Table F.3 items 1-13, 288 bits).
    fn which_region(i: usize) -> &'static str {
        if i < 36 {
            "page-offset hint table header (ISO 32000-1 Table F.3 items 1-13)"
        } else {
            "per-page columns, or the shared-object table that follows them at /S"
        }
    }

    /// The degenerate shape that 24 of the 28 corpus documents have: one page, so every
    /// per-page column is zero-width and the whole page-offset table is its 36-byte header.
    ///
    /// It is the (d) vector. nbits_delta_nobjects, nbits_delta_page_length and
    /// nbits_nshared_objects are all 0 here because their maxima equal their minima; an
    /// encoder using nbits(0) == 1 would emit three 1-bit columns of one row each, three extra
    /// flush bytes, a payload of 71 rather than 68 bytes, and three wrong 16-bit header fields.
    #[test]
    fn encode_hints_single_page_vector() {
        let want = must_hex(concat!(
            "000000030000022500000001dac30000",
            "0000000000000001dac3000000000002",
            "00000004000000000000000000000003",
            "000000030000000000500011001a0000",
            "3b33e000"
        ));

        let got = encode_hints(&Hints {
            // One page holding 3 objects and 121539 bytes.
            pages: vec![PageHint {
                n_objects: 3,
                length: 121539,
                shared_ids: vec![],
            }],
            first_page_offset: 549,
            // Those same 3 objects are the first page's shared groups. Their lengths sum to
            // the page length: 132 + 80 + 121327 == 121539, which makes min_group_length 80
            // and nbits_delta_group_length 17 (121327 - 80 == 121247, which needs 17 bits).
            // 17 is not a multiple of 8, so the three 17-bit values straddle byte boundaries
            // and the last one is padded.
            shared: vec![
                SharedHint { group_length: 132 },
                SharedHint { group_length: 80 },
                SharedHint {
                    group_length: 121327,
                },
            ],
            n_shared_first_page: 3,
            // Left at zero exactly as qpdf leaves them when nshared_total is not greater than
            // nshared_first_page (calculateHSharedObject).
            first_shared_obj: 0,
            first_shared_offset: 0,
            outline: None,
        })
        .unwrap();
        assert_eq!(
            got.s, WANT_S_SINGLE_PAGE,
            "the shared table must start right after the 36-byte page-offset header when every per-page column is zero-width"
        );
        assert_eq!(got.o, 0);
        assert_payload(&got.payload, &want);
    }

    /// The vector that actually exercises the bit packing: three pages, four columns of
    /// non-trivial width, and a vector-of-vectors column.
    ///
    /// Widths in play: 3-bit delta_nobjects, 13-bit delta_page_length, 3-bit nshared_objects,
    /// 4-bit shared_identifiers (14 entries), zero-width shared_numerators and
    /// delta_content_offset, 13-bit delta_content_length, then 13-bit delta_group_length over 9
    /// groups and a 1-bit signature column. A per-table (rather than per-column) flush loses
    /// the pad bits between columns and shortens the payload to 96.
    ///
    /// Page 0 carries no shared identifiers: Table F.4 item 3 defines the first page's shared
    /// objects implicitly.
    #[test]
    fn encode_hints_three_page_vector() {
        let want = must_hex(concat!(
            "000000020000029f000300000241000d",
            "00000000000000000241000d00030004",
            "00000004e000a1b000000a1f80234567",
            "82345678a1b000000a00000000000000",
            "00000000090000000900000000002800",
            "0d061049c00000003503242260a181f8",
            "0000"
        ));

        let shared = vec![2i64, 3, 4, 5, 6, 7, 8];
        let group = |g| SharedHint { group_length: g };
        let got = encode_hints(&Hints {
            pages: vec![
                PageHint {
                    n_objects: 9,
                    length: 5751,
                    shared_ids: vec![],
                },
                PageHint {
                    n_objects: 2,
                    length: 577,
                    shared_ids: shared.clone(),
                },
                PageHint {
                    n_objects: 2,
                    length: 582,
                    shared_ids: shared,
                },
            ],
            first_page_offset: 671,
            // Page 0's nine objects, in order. Their lengths sum to 5751, page 0's length.
            // min_group_length is 40 and the widest delta is 4199 - 40, hence 13 bits over
            // nine values: 117 bits, so the column is padded to 15 bytes and the 1-bit
            // signature column that follows starts on a fresh byte.
            shared: vec![
                group(234),
                group(335),
                group(40),
                group(40),
                group(146),
                group(241),
                group(315),
                group(201),
                group(4199),
            ],
            n_shared_first_page: 9,
            first_shared_obj: 0,
            first_shared_offset: 0,
            outline: None,
        })
        .unwrap();
        assert_eq!(got.s, WANT_S_THREE_PAGE);
        assert_payload(&got.payload, &want);
    }

    #[test]
    fn encode_hints_refuses_bad_input() {
        assert!(encode_hints(&Hints::default()).is_err());
        let page0_shared = Hints {
            pages: vec![PageHint {
                n_objects: 1,
                length: 1,
                shared_ids: vec![0],
            }],
            ..Hints::default()
        };
        assert!(encode_hints(&page0_shared).is_err());
        let too_many = Hints {
            pages: vec![PageHint::default()],
            n_shared_first_page: 1,
            ..Hints::default()
        };
        assert!(encode_hints(&too_many).is_err());
    }

    #[test]
    fn nbits_is_zero_for_zero() {
        assert_eq!(nbits(0), 0);
        assert_eq!(nbits(1), 1);
        assert_eq!(nbits(2), 2);
        assert_eq!(nbits(255), 8);
        assert_eq!(nbits(256), 9);
        assert_eq!(nbits(-5), 0);
    }
}
