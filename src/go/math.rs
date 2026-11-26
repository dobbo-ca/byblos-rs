//! Go's pure-Go `math.Sin`, `Cos`, `Atan`, `Atan2` (Go 1.27 `math/sin.go`, `trig_reduce.go`,
//! `atan.go`, `atan2.go`) and `Hypot` with the semantics of `hypot_amd64.s`.
//!
//! Rust's `f64::sin` calls the platform libm and can differ by an ulp. byblos writes these
//! results into content streams and uses them for geometry, so the Go algorithm is ported
//! operation by operation. Neither Go on amd64 (`GOAMD64=v1`) nor Rust fuses a multiply and an
//! add, so the same expression order gives the same bits. `sqrt`, `abs`, `floor` and
//! `copysign` are exact in both languages and use the std methods.
//!
//! `Hypot` on amd64 is `max * sqrt(1 + (min/max)^2)`: the same operations as Go's portable
//! `hypot`, and an infinity wins over a NaN. The oracle runs on amd64, so that is the
//! reference (`haveArchHypot` is true there).
//!
//! Go BSD licence and the Cephes notice of `sin.go`/`atan.go`: see NOTICE.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

// Go evaluates `Pi/2`, `4/Pi` and the like as exact constants and rounds once, so these are
// written as the correctly rounded values and not computed in f64.
const PI: f64 = 3.14159265358979323846264338327950288419716939937510582097494459;
const PI_2: f64 = 1.57079632679489661923132169163975144209858469968755291048747229;
const PI_4: f64 = 0.785398163397448309615660845819875721049292349843776455243736148;
const THREE_PI_4: f64 = 2.35619449019234492884698253745962716314787704953132936573120844;
const FOUR_OVER_PI: f64 = 1.27323954473516268615107010698011489627567716592365;

const SIN: [f64; 6] = [
    1.58962301576546568060e-10,
    -2.50507477628578072866e-8,
    2.75573136213857245213e-6,
    -1.98412698295895385996e-4,
    8.33333333332211858878e-3,
    -1.66666666666666307295e-1,
];

const COS: [f64; 6] = [
    -1.13585365213876817300e-11,
    2.08757008419747316778e-9,
    -2.75573141792967388112e-7,
    2.48015872888517045348e-5,
    -1.38888888888730564116e-3,
    4.16666666666665929218e-2,
];

// Pi/4 split into three parts.
const PI4A: f64 = 7.85398125648498535156e-1;
const PI4B: f64 = 3.77489470793079817668e-8;
const PI4C: f64 = 2.69515142907905952645e-15;

/// The maximum x where the reduction by Pi/4 in three float64 parts is still accurate.
const REDUCE_THRESHOLD: f64 = (1u64 << 29) as f64;

const SHIFT: u32 = 52;
const BIAS: u64 = 1023;
const MASK: u64 = 0x7FF;

/// Binary digits of 4/pi, `4/pi = sum mPi4[i] * 2^(-64 i)`.
const M_PI4: [u64; 20] = [
    0x0000000000000001,
    0x45f306dc9c882a53,
    0xf84eafa3ea69bb81,
    0xb6c52b3278872083,
    0xfca2c757bd778ac3,
    0x6e48dc74849ba5c0,
    0x0c925dd413a32439,
    0xfc3bd63962534e7d,
    0xd1046bea5d768909,
    0xd338e04d68befc82,
    0x7323ac7306a673e9,
    0x3908bf177bf25076,
    0x3ff12fffbc0b301f,
    0xde5e2316b414da3e,
    0xda6cfd9e4f96136e,
    0x9e8c7ecd3cbfd45a,
    0xea4f758fd7cbe2f6,
    0x7a0e73ef14a525d4,
    0xd7f6bf623f1aba10,
    0xac06608df8f6d757,
];

/// Go's `x << n` on a uint64: a shift of 64 or more gives 0.
fn shl(x: u64, n: u32) -> u64 {
    x.checked_shl(n).unwrap_or(0)
}

/// Go's `x >> n` on a uint64: a shift of 64 or more gives 0.
fn shr(x: u64, n: u32) -> u64 {
    x.checked_shr(n).unwrap_or(0)
}

/// `trigReduce`: Payne-Hanek reduction by Pi/4 for `x > 0`. Returns the octant `j` and the
/// reduced argument `z`.
fn trig_reduce(x: f64) -> (u64, f64) {
    if x < PI_4 {
        return (0, x);
    }
    // x = ix * 2**exp.
    let mut ix = x.to_bits();
    let exp = ((ix >> SHIFT) & MASK) as i64 - BIAS as i64 - SHIFT as i64;
    ix &= !(MASK << SHIFT);
    ix |= 1 << SHIFT;
    // exp >= -53 since x >= PI/4, and exp < 971 for the largest float64.
    let e = (exp + 61) as usize;
    let (digit, bitshift) = (e / 64, (e % 64) as u32);
    let m = &M_PI4;
    let z0 = shl(m[digit], bitshift) | shr(m[digit + 1], 64 - bitshift);
    let z1 = shl(m[digit + 1], bitshift) | shr(m[digit + 2], 64 - bitshift);
    let z2 = shl(m[digit + 2], bitshift) | shr(m[digit + 3], 64 - bitshift);
    // Multiply the mantissa by the digits and extract the upper two digits (hi, lo).
    let z2hi = ((z2 as u128 * ix as u128) >> 64) as u64;
    let z1p = z1 as u128 * ix as u128;
    let (z1hi, z1lo) = ((z1p >> 64) as u64, z1p as u64);
    let z0lo = z0.wrapping_mul(ix);
    let (lo, c) = z1lo.overflowing_add(z2hi);
    let hi = z0lo.wrapping_add(z1hi).wrapping_add(c as u64);
    // The top 3 bits are j.
    let mut j = hi >> 61;
    // Extract the fraction and find its magnitude.
    let hi = (hi << 3) | (lo >> 61);
    let lz = hi.leading_zeros();
    let e = BIAS.wrapping_sub(lz as u64 + 1);
    // Clear the implicit mantissa bit and shift into place.
    let hi = shl(hi, lz + 1) | shr(lo, 64u32.wrapping_sub(lz + 1));
    let hi = hi >> (64 - SHIFT);
    // Include the exponent and convert to a float.
    let mut z = f64::from_bits(hi | (e << SHIFT));
    // Map zeros to origin.
    if j & 1 == 1 {
        j += 1;
        j &= 7;
        z -= 1.0;
    }
    // Multiply the fractional part by pi/4.
    (j, z * PI_4)
}

/// `(j, z, y)` of the shared reduction of `sin` and `cos` for `x >= 0`.
fn reduce(x: f64) -> (u64, f64) {
    if x >= REDUCE_THRESHOLD {
        return trig_reduce(x);
    }
    // Integer part of x/(Pi/4), as integer for tests on the phase angle, and as float.
    let mut j = (x * FOUR_OVER_PI) as u64;
    let mut y = j as f64;
    // Map zeros to origin.
    if j & 1 == 1 {
        j += 1;
        y += 1.0;
    }
    j &= 7;
    // Extended precision modular arithmetic.
    let z = ((x - y * PI4A) - y * PI4B) - y * PI4C;
    (j, z)
}

fn sin_poly(z: f64, zz: f64) -> f64 {
    z + z
        * zz
        * ((((((SIN[0] * zz) + SIN[1]) * zz + SIN[2]) * zz + SIN[3]) * zz + SIN[4]) * zz + SIN[5])
}

fn cos_poly(zz: f64) -> f64 {
    1.0 - 0.5 * zz
        + zz * zz
            * ((((((COS[0] * zz) + COS[1]) * zz + COS[2]) * zz + COS[3]) * zz + COS[4]) * zz
                + COS[5])
}

/// `math.Cos`.
pub fn cos(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return f64::NAN;
    }
    let mut sign = false;
    let x = x.abs();
    let (mut j, z) = reduce(x);
    if j > 3 {
        j -= 4;
        sign = !sign;
    }
    if j > 1 {
        sign = !sign;
    }
    let zz = z * z;
    let y = if j == 1 || j == 2 {
        sin_poly(z, zz)
    } else {
        cos_poly(zz)
    };
    if sign { -y } else { y }
}

/// `math.Sin`.
pub fn sin(x: f64) -> f64 {
    if x == 0.0 || x.is_nan() {
        return x;
    }
    if x.is_infinite() {
        return f64::NAN;
    }
    let mut sign = false;
    let mut x = x;
    if x < 0.0 {
        x = -x;
        sign = true;
    }
    let (mut j, z) = reduce(x);
    // Reflect in the x axis.
    if j > 3 {
        sign = !sign;
        j -= 4;
    }
    let zz = z * z;
    let y = if j == 1 || j == 2 {
        cos_poly(zz)
    } else {
        sin_poly(z, zz)
    };
    if sign { -y } else { y }
}

/// `xatan`: a series valid in [0, 0.66].
fn xatan(x: f64) -> f64 {
    const P0: f64 = -8.750608600031904122785e-01;
    const P1: f64 = -1.615753718733365076637e+01;
    const P2: f64 = -7.500855792314704667340e+01;
    const P3: f64 = -1.228866684490136173410e+02;
    const P4: f64 = -6.485021904942025371773e+01;
    const Q0: f64 = 2.485846490142306297962e+01;
    const Q1: f64 = 1.650270098316988542046e+02;
    const Q2: f64 = 4.328810604912902668951e+02;
    const Q3: f64 = 4.853903996359136964868e+02;
    const Q4: f64 = 1.945506571482613964425e+02;
    let mut z = x * x;
    z = z * ((((P0 * z + P1) * z + P2) * z + P3) * z + P4)
        / (((((z + Q0) * z + Q1) * z + Q2) * z + Q3) * z + Q4);
    x * z + x
}

/// `satan`: reduces its (positive) argument to [0, 0.66] and calls `xatan`.
fn satan(x: f64) -> f64 {
    const MOREBITS: f64 = 6.123233995736765886130e-17; // pi/2 = PIO2 + Morebits
    const TAN3PIO8: f64 = 2.41421356237309504880; // tan(3*pi/8)
    if x <= 0.66 {
        return xatan(x);
    }
    if x > TAN3PIO8 {
        return PI_2 - xatan(1.0 / x) + MOREBITS;
    }
    PI_4 + xatan((x - 1.0) / (x + 1.0)) + 0.5 * MOREBITS
}

/// `math.Atan`.
pub fn atan(x: f64) -> f64 {
    if x == 0.0 {
        return x;
    }
    if x > 0.0 {
        return satan(x);
    }
    -satan(-x)
}

/// `math.Atan2`.
pub fn atan2(y: f64, x: f64) -> f64 {
    if y.is_nan() || x.is_nan() {
        return f64::NAN;
    }
    if y == 0.0 {
        if x >= 0.0 && !x.is_sign_negative() {
            return 0.0f64.copysign(y);
        }
        return PI.copysign(y);
    }
    if x == 0.0 {
        return PI_2.copysign(y);
    }
    if x.is_infinite() {
        if x > 0.0 {
            if y.is_infinite() {
                return PI_4.copysign(y);
            }
            return 0.0f64.copysign(y);
        }
        if y.is_infinite() {
            return THREE_PI_4.copysign(y);
        }
        return PI.copysign(y);
    }
    if y.is_infinite() {
        return PI_2.copysign(y);
    }

    // Call atan and determine the quadrant.
    let q = atan(y / x);
    if x < 0.0 {
        if q <= 0.0 {
            return q + PI;
        }
        return q - PI;
    }
    q
}

/// `math.Hypot` as `hypot_amd64.s` computes it.
pub fn hypot(p: f64, q: f64) -> f64 {
    let (p, q) = (p.abs(), q.abs());
    if p.is_infinite() || q.is_infinite() {
        return f64::INFINITY;
    }
    if p.is_nan() || q.is_nan() {
        return f64::from_bits(0x7FF8_0000_0000_0001);
    }
    if p == 0.0 && q == 0.0 {
        return 0.0;
    }
    let (max, min) = if p < q { (q, p) } else { (p, q) };
    let r = min / max;
    max * (1.0 + r * r).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go's math/all_test.go: vf and the expected results computed with high precision.
    const VF: [f64; 10] = [
        4.9790119248836735e+00,
        7.7388724745781045e+00,
        -2.7688005719200159e-01,
        -5.0106036182710749e+00,
        9.6362937071984173e+00,
        2.9263772392439646e+00,
        5.2290834314593066e+00,
        2.7279399104360102e+00,
        1.8253080916808550e+00,
        -8.6859247685756013e+00,
    ];
    const SIN_WANT: [f64; 10] = [
        -9.6466616586009283766724726e-01,
        9.9338225271646545763467022e-01,
        -2.7335587039794393342449301e-01,
        9.5586257685042792878173752e-01,
        -2.099421066779969164496634e-01,
        2.135578780799860532750616e-01,
        -8.694568971167362743327708e-01,
        4.019566681155577786649878e-01,
        9.6778633541687993721617774e-01,
        -6.734405869050344734943028e-01,
    ];
    const COS_WANT: [f64; 10] = [
        2.634752140995199110787593e-01,
        1.148551260848219865642039e-01,
        9.6191297325640768154550453e-01,
        2.938141150061714816890637e-01,
        -9.777138189897924126294461e-01,
        -9.7693041344303219127199518e-01,
        4.940088096948647263961162e-01,
        -9.1565869021018925545016502e-01,
        -2.517729313893103197176091e-01,
        -7.39241351595676573201918e-01,
    ];
    const ATAN_WANT: [f64; 10] = [
        1.372590262129621651920085e+00,
        1.442290609645298083020664e+00,
        -2.7011324359471758245192595e-01,
        -1.3738077684543379452781531e+00,
        1.4673921193587666049154681e+00,
        1.2415173565870168649117764e+00,
        1.3818396865615168979966498e+00,
        1.2194305844639670701091426e+00,
        1.0696031952318783760193244e+00,
        -1.4561721938838084990898679e+00,
    ];
    const ATAN2_WANT: [f64; 10] = [
        1.1088291730037004444527075e+00,
        9.1218183188715804018797795e-01,
        1.5984772603216203736068915e+00,
        2.0352918654092086637227327e+00,
        8.0391819139044720267356014e-01,
        1.2861075249894661588866752e+00,
        1.0889904479131695712182587e+00,
        1.3044821793397925293797357e+00,
        1.3902530903455392306872261e+00,
        2.2859857424479142655411058e+00,
    ];
    const SIN_LARGE: [f64; 10] = [
        -9.646661658548936063912e-01,
        9.933822527198506903752e-01,
        -2.7335587036246899796e-01,
        9.55862576853689321268e-01,
        -2.099421066862688873691e-01,
        2.13557878070308981163e-01,
        -8.694568970959221300497e-01,
        4.01956668098863248917e-01,
        9.67786335404528727927e-01,
        -6.7344058693131973066e-01,
    ];
    const COS_LARGE: [f64; 10] = [
        2.634752141185559426744e-01,
        1.14855126055543100712e-01,
        9.61912973266488928113e-01,
        2.9381411499556122552e-01,
        -9.777138189880161924641e-01,
        -9.76930413445147608049e-01,
        4.940088097314976789841e-01,
        -9.15658690217517835002e-01,
        -2.51772931436786954751e-01,
        -7.3924135157173099849e-01,
    ];

    // Go's `tolerance(a, b, e)`: b is the expected value.
    fn tol(a: f64, b: f64, e: f64) -> bool {
        if a == b {
            return true;
        }
        let d = (a - b).abs();
        let e = if b != 0.0 { (e * b).abs() } else { e };
        d < e
    }
    fn veryclose(a: f64, b: f64) -> bool {
        tol(a, b, 4e-16)
    }
    fn close(a: f64, b: f64) -> bool {
        tol(a, b, 1e-14)
    }
    // Go's `alike`: equal, or both NaN, and the same sign of zero.
    fn alike(a: f64, b: f64) -> bool {
        if a.is_nan() && b.is_nan() {
            return true;
        }
        a == b && a.is_sign_negative() == b.is_sign_negative()
    }

    #[test]
    fn sin_cos_values() {
        for i in 0..VF.len() {
            assert!(veryclose(sin(VF[i]), SIN_WANT[i]), "sin({})", VF[i]);
            assert!(veryclose(cos(VF[i]), COS_WANT[i]), "cos({})", VF[i]);
        }
    }

    #[test]
    fn sin_cos_large_args() {
        let large = 314159.26535897932384626;
        for i in 0..VF.len() {
            assert!(close(sin(VF[i] + large), SIN_LARGE[i]), "sin large {i}");
            assert!(close(cos(VF[i] + large), COS_LARGE[i]), "cos large {i}");
        }
        // Above the 1<<29 threshold the Payne-Hanek path runs on its own.
        for x in [(1u64 << 30) as f64, 1e10, 1e100, 1e300] {
            let (s, c) = (sin(x), cos(x));
            assert!((s * s + c * c - 1.0).abs() < 1e-14, "x = {x}");
        }
    }

    #[test]
    fn sin_cos_special() {
        assert!(alike(sin(0.0), 0.0));
        assert!(alike(sin(-0.0), -0.0));
        assert!(sin(f64::INFINITY).is_nan());
        assert!(sin(f64::NEG_INFINITY).is_nan());
        assert!(sin(f64::NAN).is_nan());
        assert!(cos(f64::INFINITY).is_nan());
        assert!(cos(f64::NAN).is_nan());
        assert_eq!(cos(0.0), 1.0);
    }

    #[test]
    fn atan_values() {
        for i in 0..VF.len() {
            assert!(veryclose(atan(VF[i]), ATAN_WANT[i]), "atan({})", VF[i]);
            assert!(
                veryclose(atan2(10.0, VF[i]), ATAN2_WANT[i]),
                "atan2(10,{})",
                VF[i]
            );
        }
        let sc: [(f64, f64); 5] = [
            (f64::NEG_INFINITY, -PI_2),
            (-0.0, -0.0),
            (0.0, 0.0),
            (f64::INFINITY, PI_2),
            (f64::NAN, f64::NAN),
        ];
        for (x, want) in sc {
            assert!(alike(atan(x), want), "atan({x})");
        }
    }

    #[test]
    fn atan2_special_cases() {
        let inf = f64::INFINITY;
        let nan = f64::NAN;
        let cases: [(f64, f64, f64); 24] = [
            (-inf, -inf, -THREE_PI_4),
            (-inf, -PI, -PI_2),
            (-inf, 0.0, -PI_2),
            (-inf, PI, -PI_2),
            (-inf, inf, -PI_4),
            (-inf, nan, nan),
            (-PI, -inf, -PI),
            (-PI, 0.0, -PI_2),
            (-PI, inf, -0.0),
            (-0.0, -inf, -PI),
            (-0.0, -PI, -PI),
            (-0.0, -0.0, -PI),
            (-0.0, 0.0, -0.0),
            (-0.0, PI, -0.0),
            (-0.0, inf, -0.0),
            (0.0, -inf, PI),
            (0.0, -PI, PI),
            (0.0, -0.0, PI),
            (0.0, 0.0, 0.0),
            (0.0, inf, 0.0),
            (PI, -inf, PI),
            (PI, 0.0, PI_2),
            (PI, inf, 0.0),
            (-1.0, inf, -0.0),
        ];
        for (y, x, want) in cases {
            assert!(
                alike(atan2(y, x), want),
                "atan2({y}, {x}) = {}",
                atan2(y, x)
            );
        }
    }

    #[test]
    fn hypot_values() {
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(hypot(-3.0, 4.0), 5.0);
        let inf = f64::INFINITY;
        assert_eq!(hypot(f64::NEG_INFINITY, f64::NAN), inf);
        assert_eq!(hypot(f64::NAN, inf), inf);
        assert!(hypot(f64::NAN, 0.0).is_nan());
        assert!(alike(hypot(-0.0, -0.0), 0.0));
        assert_eq!(hypot(0.0, 7.0), 7.0);
        // No overflow where p*p would overflow.
        assert!(close(hypot(1e200, 1e200), 1.4142135623730951e200));
    }
}
