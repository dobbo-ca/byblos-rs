//! Go's `hash/crc32` with the IEEE polynomial: `Update`, `ChecksumIEEE`. Slicing-by-8, safe code;
//! Go uses CLMUL / the ARM CRC instructions, so a one-byte table loop would be several times
//! slower on a multi-MB PNG.

const TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut j = 1;
    while j < 8 {
        let mut i = 0;
        while i < 256 {
            let p = t[j - 1][i];
            t[j][i] = t[0][(p & 0xFF) as usize] ^ (p >> 8);
            i += 1;
        }
        j += 1;
    }
    t
};

/// `crc32.Update(crc, IEEETable, p)`.
#[must_use]
pub fn update(crc: u32, p: &[u8]) -> u32 {
    let mut c = !crc;
    let mut it = p.chunks_exact(8);
    for b in &mut it {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][usize::from(b[4])]
            ^ TABLES[2][usize::from(b[5])]
            ^ TABLES[1][usize::from(b[6])]
            ^ TABLES[0][usize::from(b[7])];
    }
    for &b in it.remainder() {
        c = TABLES[0][usize::from((c as u8) ^ b)] ^ (c >> 8);
    }
    !c
}

/// `crc32.ChecksumIEEE(p)`.
#[must_use]
pub fn checksum(p: &[u8]) -> u32 {
    update(0, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytewise(p: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in p {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    #[test]
    fn check_value_and_split() {
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(update(checksum(b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn matches_bitwise_at_every_length_and_split() {
        let d: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(131) >> 2) as u8)
            .collect();
        for n in 0..d.len() {
            assert_eq!(checksum(&d[..n]), bytewise(&d[..n]), "len {n}");
        }
        for s in 0..40 {
            assert_eq!(
                update(checksum(&d[..s]), &d[s..]),
                bytewise(&d),
                "split {s}"
            );
        }
    }
}
