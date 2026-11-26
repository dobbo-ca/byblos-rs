//! The MQ arithmetic encoder of T.88 Annex E.2 and the probability table it shares with the
//! decoder (`internal/jbig2/mq.go`).

use crate::error::{Error, Result};

/// One row of T.88 Table E.1: the LPS probability estimate and the index transitions taken
/// after an MPS or an LPS renormalisation.
#[derive(Debug, Clone, Copy)]
pub struct QeEntry {
    pub qe: u32,
    pub nmps: u8,
    pub nlps: u8,
    pub swch: u8,
}

const fn q(qe: u32, nmps: u8, nlps: u8, swch: u8) -> QeEntry {
    QeEntry {
        qe,
        nmps,
        nlps,
        swch,
    }
}

/// T.88 Table E.1 in full. Index 46 is the fixed 0.5 estimate.
pub static QE_TABLE: [QeEntry; 47] = [
    q(0x5601, 1, 1, 1),
    q(0x3401, 2, 6, 0),
    q(0x1801, 3, 9, 0),
    q(0x0AC1, 4, 12, 0),
    q(0x0521, 5, 29, 0),
    q(0x0221, 38, 33, 0),
    q(0x5601, 7, 6, 1),
    q(0x5401, 8, 14, 0),
    q(0x4801, 9, 14, 0),
    q(0x3801, 10, 14, 0),
    q(0x3001, 11, 17, 0),
    q(0x2401, 12, 18, 0),
    q(0x1C01, 13, 20, 0),
    q(0x1601, 29, 21, 0),
    q(0x5601, 15, 14, 1),
    q(0x5401, 16, 14, 0),
    q(0x5101, 17, 15, 0),
    q(0x4801, 18, 16, 0),
    q(0x3801, 19, 17, 0),
    q(0x3401, 20, 18, 0),
    q(0x3001, 21, 19, 0),
    q(0x2801, 22, 19, 0),
    q(0x2401, 23, 20, 0),
    q(0x2201, 24, 21, 0),
    q(0x1C01, 25, 22, 0),
    q(0x1801, 26, 23, 0),
    q(0x1601, 27, 24, 0),
    q(0x1401, 28, 25, 0),
    q(0x1201, 29, 26, 0),
    q(0x1101, 30, 27, 0),
    q(0x0AC1, 31, 28, 0),
    q(0x09C1, 32, 29, 0),
    q(0x08A1, 33, 30, 0),
    q(0x0521, 34, 31, 0),
    q(0x0441, 35, 32, 0),
    q(0x02A1, 36, 33, 0),
    q(0x0221, 37, 34, 0),
    q(0x0141, 38, 35, 0),
    q(0x0111, 39, 36, 0),
    q(0x0085, 40, 37, 0),
    q(0x0049, 41, 38, 0),
    q(0x0025, 42, 39, 0),
    q(0x0015, 43, 40, 0),
    q(0x0009, 44, 41, 0),
    q(0x0005, 45, 42, 0),
    q(0x0001, 45, 43, 0),
    q(0x5601, 46, 46, 0),
];

/// Writes context `i`, ignoring an index outside `cx` (the callers keep `i` inside).
pub(crate) fn set_cx(cx: &mut [u8], i: usize, v: u8) {
    if let Some(c) = cx.get_mut(i) {
        *c = v;
    }
}

/// The adaptive state of a set of coding contexts is one byte each, packed as `index << 1 | mps`.
/// T.88 7.4.6.4 step 2 requires them to be zeroed at the start of every generic region segment,
/// so callers allocate a fresh array per segment rather than reusing one.
pub type Contexts = Vec<u8>;

/// `encoder`: the MQ arithmetic encoder of T.88 Annex E.2.
///
/// `out` carries a one-byte leading sentinel so that `bp` can start at the E.2.8 "BPST - 1"
/// position without a negative index; [`Encoder::flush`] slices it off. The carry path in
/// `byteout` increments `out[bp]` in place, which is the "propagate the carry into the
/// previously written byte" behaviour of Figure E.9.
pub struct Encoder {
    /// Code register.
    c: u32,
    /// Interval register, 16-bit.
    a: u32,
    /// Bits remaining before the next byte is emitted.
    ct: i32,
    out: Vec<u8>,
    bp: usize,
    /// The most bytes `out` may hold (`Limits::max_alloc`).
    max: usize,
}

impl Encoder {
    /// `newEncoder`: INITENC (T.88 E.2.8, Figure E.10). `max` caps the coded bytes.
    ///
    /// # Errors
    /// [`Error::OutOfMemory`] if the allocator refuses the first byte.
    pub fn new(max: u64) -> Result<Encoder> {
        // The sentinel byte is 0, never 0xFF, so CT starts at 12 rather than 13.
        let mut out = Vec::new();
        out.try_reserve(64)?;
        out.push(0);
        Ok(Encoder {
            c: 0,
            a: 0x8000,
            ct: 12,
            out,
            bp: 0,
            max: usize::try_from(max).unwrap_or(usize::MAX),
        })
    }

    fn b(&self) -> u8 {
        self.out.get(self.bp).copied().unwrap_or(0)
    }

    fn set_b(&mut self, v: u8) {
        if let Some(x) = self.out.get_mut(self.bp) {
            *x = v;
        }
    }

    fn inc_bp(&mut self) -> Result<()> {
        self.bp += 1;
        if self.bp == self.out.len() {
            if self.out.len() >= self.max {
                return Err(Error::Alloc {
                    got: self.out.len() as u64 + 1,
                    max: self.max as u64,
                });
            }
            self.out.try_reserve(1)?;
            self.out.push(0);
        }
        Ok(())
    }

    /// T.88 E.2.7, Figure E.9.
    fn byteout(&mut self) -> Result<()> {
        if self.b() == 0xFF {
            return self.stuff();
        }
        if self.c < 0x800_0000 {
            return self.normal();
        }
        // Carry out of the code register: propagate into the previous byte.
        self.set_b(self.b().wrapping_add(1));
        if self.b() == 0xFF {
            self.c &= 0x7FF_FFFF;
            return self.stuff();
        }
        self.normal()
    }

    /// Emits 7 bits after a 0xFF byte, which is what prevents a 0xFF 0x90..0xFF marker sequence
    /// from ever appearing in the coded data.
    fn stuff(&mut self) -> Result<()> {
        self.inc_bp()?;
        self.set_b((self.c >> 20) as u8);
        self.c &= 0xF_FFFF;
        self.ct = 7;
        Ok(())
    }

    fn normal(&mut self) -> Result<()> {
        self.inc_bp()?;
        self.set_b((self.c >> 19) as u8);
        self.c &= 0x7_FFFF;
        self.ct = 8;
        Ok(())
    }

    /// RENORME (T.88 E.2.6, Figure E.8).
    fn renorm(&mut self) -> Result<()> {
        loop {
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.ct == 0 {
                self.byteout()?;
            }
            if self.a & 0x8000 != 0 {
                return Ok(());
            }
        }
    }

    /// Codes decision `d` (0 or 1) in context `i`. This is CODE0/CODE1 (T.88 E.2.3)
    /// dispatching to CODEMPS (Figure E.6) or CODELPS (Figure E.7).
    ///
    /// # Errors
    /// [`Error::Alloc`] or [`Error::OutOfMemory`] when the coded bytes outgrow their cap.
    pub fn encode(&mut self, cx: &mut [u8], i: usize, d: u8) -> Result<()> {
        let st = cx.get(i).copied().unwrap_or(0);
        let q = QE_TABLE
            .get(usize::from(st >> 1))
            .copied()
            .unwrap_or(QE_TABLE[0]);
        let mut mps = st & 1;

        if d == mps {
            // CODEMPS. The conditional exchange can only matter when the interval needs
            // renormalising, which is why it sits inside this branch.
            self.a = self.a.wrapping_sub(q.qe);
            if self.a & 0x8000 == 0 {
                if self.a < q.qe {
                    self.a = q.qe;
                } else {
                    self.c = self.c.wrapping_add(q.qe);
                }
                set_cx(cx, i, (q.nmps << 1) | mps);
                return self.renorm();
            }
            self.c = self.c.wrapping_add(q.qe);
            return Ok(());
        }

        // CODELPS.
        self.a = self.a.wrapping_sub(q.qe);
        if self.a < q.qe {
            self.c = self.c.wrapping_add(q.qe);
        } else {
            self.a = q.qe;
        }
        if q.swch == 1 {
            mps = 1 - mps;
        }
        set_cx(cx, i, (q.nlps << 1) | mps);
        self.renorm()
    }

    /// FLUSH (T.88 E.2.9, Figure E.11) including SETBITS (Figure E.12). It returns the complete
    /// coded segment, always terminated by 0xFF 0xAC.
    ///
    /// The optional trailing-0x7FFF removal of E.2.10 is deliberately NOT applied. It saves at
    /// most two bytes per region and is the only legitimate source of byte-level variation
    /// between two correct encoders; omitting it is what makes this encoder reproduce the
    /// Annex H.1 and H.2 vectors byte for byte.
    ///
    /// # Errors
    /// As [`Encoder::encode`].
    pub fn flush(mut self) -> Result<Vec<u8>> {
        let tempc = self.c.wrapping_add(self.a);
        self.c |= 0xFFFF;
        if self.c >= tempc {
            self.c = self.c.wrapping_sub(0x8000);
        }
        self.c <<= self.ct as u32;
        self.byteout()?;
        self.c <<= self.ct as u32;
        self.byteout()?;
        if self.b() != 0xFF {
            self.inc_bp()?;
            self.set_b(0xFF);
        }
        self.inc_bp()?;
        self.set_b(0xAC);
        self.out.truncate(self.bp + 1);
        self.out.drain(..1);
        Ok(self.out)
    }
}
