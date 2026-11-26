//! The MQ arithmetic decoder of T.88 Annex E.3 (`internal/jbig2/mq_decode.go`), the exact
//! inverse of [`Encoder`](super::mq::Encoder).
//!
//! It shares [`QE_TABLE`] and the context packing (`index << 1 | mps`) with the encoder, so the
//! two directions cannot drift apart on the probability estimates.

use super::mq::{QE_TABLE, set_cx};

/// `decoder`. `c` is the 32-bit code register with CHIGH in bits 16-31, which is why every
/// comparison against Qe is written against `c >> 16` and every subtraction against `Qe << 16`.
/// Bits shifted out of bit 31 are consumed and discarded, as T.88's own 32-bit register does.
pub struct Decoder<'a> {
    c: u32,
    /// Interval register, 16-bit.
    a: u32,
    /// Bits remaining in the current byte.
    ct: i32,
    input: &'a [u8],
    bp: usize,
}

impl<'a> Decoder<'a> {
    /// `newDecoder`: INITDEC (T.88 E.3.5, Figure E.20).
    pub fn new(input: &'a [u8]) -> Decoder<'a> {
        let mut d = Decoder {
            c: 0,
            a: 0,
            ct: 0,
            input,
            bp: 0,
        };
        d.c = u32::from(d.at(0)) << 16;
        d.bytein();
        d.c <<= 7;
        d.ct -= 7;
        d.a = 0x8000;
        d
    }

    /// The coded byte at `i`, or 0xFF for any index outside the input.
    ///
    /// T.88 E.3.4 ends the coded data at a marker (0xFF followed by a byte above 0x8F) and from
    /// there feeds 1 bits forever. A stream truncated before its 0xFF 0xAC terminator therefore
    /// reads as if the marker were there, which keeps a damaged embedded stream a decode error
    /// instead of a crash.
    fn at(&self, i: usize) -> u8 {
        self.input.get(i).copied().unwrap_or(0xFF)
    }

    /// BYTEIN (T.88 E.3.4, Figure E.19). The 0xFF branch is the mirror of the encoder's
    /// `stuff`: seven bits after a 0xFF.
    fn bytein(&mut self) {
        if self.at(self.bp) == 0xFF {
            if self.at(self.bp + 1) > 0x8F {
                // A marker. bp does not advance again, so every later call takes this branch
                // and shifts in 1 bits.
                self.c = self.c.wrapping_add(0xFF00);
                self.ct = 8;
                return;
            }
            self.bp += 1;
            self.c = self.c.wrapping_add(u32::from(self.at(self.bp)) << 9);
            self.ct = 7;
            return;
        }
        self.bp += 1;
        self.c = self.c.wrapping_add(u32::from(self.at(self.bp)) << 8);
        self.ct = 8;
    }

    /// RENORMD (T.88 E.3.3, Figure E.18). The decoder tests CT before shifting, the encoder
    /// after.
    fn renorm(&mut self) {
        loop {
            if self.ct == 0 {
                self.bytein();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                return;
            }
        }
    }

    /// The decision coded in context `i` (0 or 1). This is DECODE (T.88 E.3.2, Figure E.17)
    /// with MPS_EXCHANGE (Figure E.16) and LPS_EXCHANGE (Figure E.15) inlined into their two
    /// call sites.
    pub fn decode(&mut self, cx: &mut [u8], i: usize) -> u8 {
        let st = cx.get(i).copied().unwrap_or(0);
        let q = QE_TABLE
            .get(usize::from(st >> 1))
            .copied()
            .unwrap_or(QE_TABLE[0]);
        let mut mps = st & 1;

        self.a = self.a.wrapping_sub(q.qe);
        if self.c >> 16 < q.qe {
            // The code register landed in the LPS sub-interval: LPS_EXCHANGE. The conditional
            // exchange means this is not necessarily an LPS decision.
            let bit;
            if self.a < q.qe {
                bit = mps;
                set_cx(cx, i, (q.nmps << 1) | mps);
            } else {
                bit = 1 - mps;
                if q.swch == 1 {
                    mps = 1 - mps;
                }
                set_cx(cx, i, (q.nlps << 1) | mps);
            }
            self.a = q.qe;
            self.renorm();
            return bit;
        }

        self.c = self.c.wrapping_sub(q.qe << 16);
        if self.a & 0x8000 != 0 {
            return mps;
        }

        // MPS_EXCHANGE.
        let bit;
        if self.a < q.qe {
            bit = 1 - mps;
            if q.swch == 1 {
                mps = 1 - mps;
            }
            set_cx(cx, i, (q.nlps << 1) | mps);
        } else {
            bit = mps;
            set_cx(cx, i, (q.nmps << 1) | mps);
        }
        self.renorm();
        bit
    }
}
