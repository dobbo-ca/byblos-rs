//! Go's float formatting as byblos uses it: `fmt` `%.Nf` (`stamp.go:142`, `pdfdoc/text.go:222`,
//! `straighten.go:69`) and `strconv.FormatFloat(v, 'f', -1, 64)` (`pdfbuild.go:178`).
//!
//! Rust's `{:.N}` and `{}` use exact decimal conversion and round half to even, as Go does, and
//! print a negative zero (and a negative value that rounds to zero) with its sign. The only
//! differences are the spelling of the non-finite values, which these wrappers fix. The unit
//! tests pin the exact-tie cases.

use std::fmt::Write;

/// Go's spelling of a non-finite value (`NaN`, `+Inf`, `-Inf`), or `None` for a finite one.
fn non_finite(v: f64) -> Option<&'static str> {
    if v.is_nan() {
        Some("NaN")
    } else if v == f64::INFINITY {
        Some("+Inf")
    } else if v == f64::NEG_INFINITY {
        Some("-Inf")
    } else {
        None
    }
}

/// `fmt.Sprintf("%.*f", prec, v)`.
pub fn format_f(v: f64, prec: usize) -> String {
    if let Some(s) = non_finite(v) {
        return s.to_string();
    }
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = write!(out, "{v:.prec$}");
    out
}

/// `strconv.FormatFloat(v, 'f', -1, 64)`: the shortest decimal that parses back to `v`, never in
/// exponent form.
pub fn format_g(v: f64) -> String {
    if let Some(s) = non_finite(v) {
        return s.to_string();
    }
    let mut out = String::new();
    let _ = write!(out, "{v}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings computed with exact decimal arithmetic (Python's correctly rounded
    // `%.*f`), which is what Go's strconv does.
    #[test]
    fn format_f_ties_round_to_even() {
        assert_eq!(format_f(0.125, 2), "0.12");
        assert_eq!(format_f(0.375, 2), "0.38");
        assert_eq!(format_f(0.5, 0), "0");
        assert_eq!(format_f(1.5, 0), "2");
        assert_eq!(format_f(2.5, 0), "2");
        assert_eq!(format_f(3.5, 0), "4");
        // 2.5e-5 is not exactly representable; it is a hair above the tie at 5 digits.
        assert_eq!(format_f(2.5e-5, 5), "0.00003");
        assert_eq!(format_f(2.5e-5, 4), "0.0000");
        assert_eq!(format_f(5e-5, 4), "0.0001");
        assert_eq!(format_f(-1.00005, 4), "-1.0001");
    }

    #[test]
    fn format_f_zero_sign_and_range() {
        assert_eq!(format_f(0.0, 4), "0.0000");
        assert_eq!(format_f(-0.0, 4), "-0.0000");
        assert_eq!(format_f(-0.00001, 4), "-0.0000");
        assert_eq!(format_f(1e21, 2), "1000000000000000000000.00");
        assert_eq!(format_f(0.1, 10), "0.1000000000");
        assert_eq!(format_f(1.0 / 3.0, 4), "0.3333");
        assert_eq!(format_f(123456.789, 10), "123456.7890000000");
    }

    #[test]
    fn non_finite_values() {
        assert_eq!(format_f(f64::NAN, 4), "NaN");
        assert_eq!(format_f(f64::INFINITY, 4), "+Inf");
        assert_eq!(format_f(f64::NEG_INFINITY, 4), "-Inf");
        assert_eq!(format_g(f64::NAN), "NaN");
        assert_eq!(format_g(f64::INFINITY), "+Inf");
        assert_eq!(format_g(f64::NEG_INFINITY), "-Inf");
    }

    #[test]
    fn format_g_shortest() {
        assert_eq!(format_g(3.0), "3");
        assert_eq!(format_g(0.1), "0.1");
        assert_eq!(format_g(-0.0), "-0");
        assert_eq!(format_g(1e21), "1000000000000000000000");
        assert_eq!(format_g(1e-7), "0.0000001");
        assert_eq!(format_g(2.5e-5), "0.000025");
        assert_eq!(format_g(123456789.125), "123456789.125");
    }
}
