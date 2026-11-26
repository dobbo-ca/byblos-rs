//! The arithmetic integer decoding procedure of T.88 Annex A and the symbol ID decoding
//! procedure of A.3 (`internal/jbig2/arith_int.go`).
//!
//! Everything a symbol dictionary or a text region reads that is not a bitmap pixel comes
//! through here. It is a second arithmetic alphabet over the same MQ decoder, which is why the
//! decoder is passed in: a symbol dictionary reads an integer, then a bitmap, then another
//! integer, all from one decoder whose state carries across.

use super::mq::Contexts;
use super::mq_decode::Decoder;

/// The prefix-to-length table of T.88 A.2 steps 2 and 3: after the sign bit, up to five more
/// decisions select how many value bits follow and what offset they carry.
///
/// The offsets are cumulative, not powers of two: 0, 0+4, 4+16, 20+64, 84+256, 340+4096.
static INT_LENGTHS: [(u32, i64); 6] = [(2, 0), (4, 4), (6, 20), (8, 84), (12, 340), (32, 4436)];

/// `newIntContexts`: one integer decoding context, 512 states, one per value of the PREV
/// register of T.88 A.2. Each use (IADH, IADW, IAEX, IADT, IAFS, IADS, IAIT, ...) gets its own.
pub fn new_int_contexts() -> Contexts {
    vec![0; 512]
}

/// `decodeInt`: the integer arithmetic decoding procedure of T.88 A.2. `None` is OOB.
///
/// OOB is not a number and not a sentinel integer: every value in the 32-bit range is a legal
/// decode, so it is returned beside the value. PREV is clamped at 256 (A.2 step 3, final
/// paragraph): once it reaches nine bits the top bit is pinned and the low eight rotate.
pub fn decode_int(d: &mut Decoder, cx: &mut [u8]) -> Option<i64> {
    let mut prev: usize = 1;
    let mut bit = || -> i64 {
        let b = usize::from(d.decode(cx, prev));
        prev = if prev < 256 {
            (prev << 1) | b
        } else {
            (((prev << 1) | b) & 511) | 256
        };
        b as i64
    };

    let sign = bit();
    // The last row needs no selector bit of its own: five 1 bits have already selected it.
    let mut sel = INT_LENGTHS[5];
    for l in &INT_LENGTHS[..5] {
        if bit() == 0 {
            sel = *l;
            break;
        }
    }

    let mut v: i64 = 0;
    for _ in 0..sel.0 {
        v = (v << 1) | bit();
    }
    v += sel.1;

    // A.2 step 4: a negative zero is OOB, not the number zero.
    if sign == 1 {
        if v == 0 {
            return None;
        }
        return Some(-v);
    }
    Some(v)
}

/// `decodeIAID`: the symbol ID decoding procedure of T.88 A.3: `code_len` decisions read down a
/// binary tree of contexts, with the running prefix as the index. The context array is
/// `1 << (code_len + 1)` entries and belongs to the region that owns it. `code_len == 0` reads
/// nothing and returns 0.
pub fn decode_iaid(d: &mut Decoder, cx: &mut [u8], code_len: u32) -> usize {
    let mut prev: usize = 1;
    for _ in 0..code_len {
        prev = (prev << 1) | usize::from(d.decode(cx, prev));
    }
    prev - (1usize << code_len)
}
