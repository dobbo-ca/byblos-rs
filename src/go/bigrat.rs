//! The `math/big` parts of `embed.go` `jp2PerInch` (`embed.go:984-989`): `Rat.SetFrac64`,
//! `Int.Exp` and `Rat.Float64`, as one small bignum routine. `num / den * 10^exp` is divided
//! exactly (the numerator is a few dozen 32-bit limbs), then rounded to the nearest f64 with ties
//! to even, which is what `Rat.Float64` returns. There is no general `Rat` or `Int`.

/// `num / den * 10^exp10` rounded to the nearest f64 (ties to even), as `big.Rat.Float64` does.
///
/// `None` where Go would panic or not return a normal number: `den == 0` (`SetFrac64` panics),
/// or a result outside the normal f64 range (`num / den * 10^exp10` below 2^-1022 cannot happen
/// for non-zero `num` and `exp10 >= 0` with a 32-bit `den`; above the range it would be
/// infinite in Go).
pub fn rat_float64(num: u64, den: u32, exp10: u8) -> Option<f64> {
    if den == 0 {
        return None;
    }
    if num == 0 {
        return Some(0.0);
    }
    // n = num * 10^exp10 * 2^96 as little-endian 32-bit limbs. The 2^96 is three zero limbs:
    // it keeps the quotient at 65 bits or more for any 32-bit `den`, so 53 bits plus the round
    // and sticky bits are always there.
    let mut n: Vec<u32> = vec![0, 0, 0, num as u32, (num >> 32) as u32];
    let mut left = exp10;
    while left > 0 {
        let k = left.min(9);
        mul_small(&mut n, 10u32.pow(u32::from(k)));
        left -= k;
    }
    // n = n / den, rem = n % den.
    let d = u64::from(den);
    let mut rem = 0u64;
    for limb in n.iter_mut().rev() {
        let cur = (rem << 32) | u64::from(*limb);
        *limb = (cur / d) as u32;
        rem = cur % d;
    }
    while n.last() == Some(&0) {
        n.pop();
    }
    let top = *n.last()?;
    let bits = 32 * (n.len() - 1) + (32 - top.leading_zeros() as usize);
    let mut shift = bits.checked_sub(53)?;

    let bit = |i: usize| n.get(i / 32).is_some_and(|l| ((l >> (i % 32)) & 1) == 1);
    let mut mant = 0u64;
    for i in 0..53 {
        mant |= u64::from(bit(shift + i)) << i;
    }
    let round_at = shift.checked_sub(1)?;
    let round = bit(round_at);
    let sticky = rem != 0
        || n.iter().take(round_at / 32).any(|&l| l != 0)
        || n.get(round_at / 32)
            .is_some_and(|&l| l & ((1u32 << (round_at % 32)) - 1) != 0);
    if round && (sticky || mant & 1 == 1) {
        mant += 1;
    }
    if mant == 1 << 53 {
        mant >>= 1;
        shift += 1;
    }
    let e = i64::try_from(shift).ok()? - 96;
    if !(-1022..=1023).contains(&e) {
        return None;
    }
    // mant is at most 2^53, so mant * 2^e is exact.
    let pow2 = f64::from_bits(u64::try_from(e + 1023).ok()? << 52);
    Some(mant as f64 * pow2)
}

/// `limbs *= m`.
fn mul_small(limbs: &mut Vec<u32>, m: u32) {
    let mut carry = 0u64;
    for l in limbs.iter_mut() {
        let cur = u64::from(*l) * u64::from(m) + carry;
        *l = cur as u32;
        carry = cur >> 32;
    }
    if carry != 0 {
        limbs.push(carry as u32);
    }
}

/// `jp2PerInch`: a JP2 `resc` capture resolution, `num / den * 10^exp` per metre, in pixels per
/// inch: the exact `254 * num * 10^exp / (10000 * den)`, rounded to even as img2pdf rounds it.
/// `None` for `den == 0`, which the caller refuses before it gets here.
pub fn jp2_per_inch(num: u16, den: u16, exp: u8) -> Option<f64> {
    let f = rat_float64(254 * u64::from(num), 10000 * u32::from(den), exp)?;
    Some(f.round_ties_even())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ties_round_to_even() {
        // Floating-point arithmetic would round 3.5 to 3 (the Go comment on jp2PerInch).
        // 254 * 35000 / (10000 * 254) = 3.5 and 254 * 25000 / (10000 * 254) = 2.5.
        assert_eq!(jp2_per_inch(35000, 254, 0), Some(4.0));
        assert_eq!(jp2_per_inch(25000, 254, 0), Some(2.0));
        assert_eq!(jp2_per_inch(30000, 254, 0), Some(3.0));
    }

    #[test]
    fn common_resolutions() {
        // 11811 px/m is 300.0 dpi (254 * 11811 / 10000 = 299.9994).
        assert_eq!(jp2_per_inch(11811, 1, 0), Some(300.0));
        // 3937 px/m: 99.9998.
        assert_eq!(jp2_per_inch(3937, 1, 0), Some(100.0));
        // num/den with an exponent: 3937 * 10^2 / 100 = 3937 per metre.
        assert_eq!(jp2_per_inch(3937, 100, 2), Some(100.0));
        assert_eq!(jp2_per_inch(0, 1, 7), Some(0.0));
        assert_eq!(jp2_per_inch(1, 1, 0), Some(0.0));
        assert_eq!(jp2_per_inch(1, 0, 0), None);
    }

    #[test]
    fn exp_up_to_255() {
        // 254 * 10^255 / 10000 = 2.54e253, and the correctly rounded decimal parse is the
        // nearest f64 of the same exact value.
        let want: f64 = "2.54e253".parse().expect("float");
        assert_eq!(rat_float64(254, 10000, 255), Some(want));
        assert_eq!(jp2_per_inch(65535, 1, 255).map(f64::to_bits), {
            let v: f64 = "1.664589e258".parse().expect("float");
            Some(v.round_ties_even().to_bits())
        });
    }

    #[test]
    fn matches_ieee_division() {
        // For operands below 2^53 the IEEE quotient is the correctly rounded value.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let a = (next() >> 11) | 1;
            let b = (next() >> 33) as u32 | 1;
            let want = a as f64 / f64::from(b);
            assert_eq!(rat_float64(a, b, 0), Some(want), "{a}/{b}");
        }
    }

    #[test]
    fn matches_decimal_parse() {
        // den = 10^k makes the value an exact decimal, whose parse is correctly rounded.
        let mut x = 0xD1B5_4A32_D192_ED03u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let a = next() >> 4;
            let exp = (next() % 256) as u8;
            let want: f64 = format!("{a}e{}", i32::from(exp) - 4)
                .parse()
                .expect("float");
            assert_eq!(rat_float64(a, 10000, exp), Some(want), "{a}e{exp}");
        }
    }
}
