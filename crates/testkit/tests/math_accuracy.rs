//! Accuracy of `mantis_core::math` against an `f64` reference (decision 0013).
//!
//! The reference is the platform's `std` `f64` functions, accurate to within
//! about one `f64` ulp. That is far tighter than the 0.5001 `f32` ulp bound
//! being checked, but it is not bit-reproducible across platforms. It
//! therefore lives here, in test tooling, and never in a simulation crate,
//! where the `std` functions are banned. The pinned bit patterns that guard
//! cross-architecture determinism live in `mantis-core` itself.
//!
//! The exhaustive sweep over all 2^32 inputs is ignored by default:
//! `cargo test -p mantis-testkit --release --test math_accuracy -- --ignored`.

#![allow(clippy::cast_possible_truncation)]

use mantis_core::math::{CANONICAL_NAN, atan, atan2, cos, exp, ln, pow, sin, tan};

/// The documented bound, in `f32` ulps.
const BOUND: f64 = 0.5001;

/// Error of `got` against the `f64` reference, in units of the `f32` ulp at
/// the reference value (spacing of the `f32` binade containing it, with the
/// subnormal spacing as the floor).
fn ulp_error(got: f32, reference: f64) -> f64 {
    if reference.is_nan() {
        return if got.is_nan() { 0.0 } else { f64::INFINITY };
    }
    if got.is_infinite() || reference.abs() > f64::from(f32::MAX) {
        return if (reference as f32) == got {
            0.0
        } else {
            f64::INFINITY
        };
    }
    let e = ((reference.abs().to_bits() >> 52) & 0x7FF) as i32 - 1023;
    let ulp = 2f64.powi(e - 23).max(2f64.powi(-149));
    (f64::from(got) - reference).abs() / ulp
}

/// Deterministic sample of `f32` bit patterns: a stride through all of them.
fn sample(stride: u32) -> impl Iterator<Item = f32> {
    (0..=u32::MAX / stride).map(move |i| f32::from_bits(i.wrapping_mul(stride)))
}

fn check_unary(name: &str, f: fn(f32) -> f32, reference: fn(f64) -> f64, stride: u32) -> Result<(), String> {
    let mut worst = (0.0f64, 0.0f32);
    for x in sample(stride).chain(core::iter::once(f32::MAX)) {
        let r = reference(f64::from(x));
        let got = f(x);
        if r.is_nan() {
            if got.to_bits() != CANONICAL_NAN.to_bits() {
                return Err(format!("{name}({x:e}) = {got:e}, expected canonical NaN"));
            }
            continue;
        }
        let e = ulp_error(got, r);
        if e > worst.0 {
            worst = (e, x);
        }
    }
    if worst.0 <= BOUND {
        Ok(())
    } else {
        Err(format!(
            "{name}: worst error {} ulp at x = {:e} ({:#010x})",
            worst.0,
            worst.1,
            worst.1.to_bits()
        ))
    }
}

#[test]
fn sin_accuracy() -> Result<(), String> {
    check_unary("sin", sin, f64::sin, 9_973)
}

#[test]
fn cos_accuracy() -> Result<(), String> {
    check_unary("cos", cos, f64::cos, 9_973)
}

#[test]
fn tan_accuracy() -> Result<(), String> {
    check_unary("tan", tan, f64::tan, 9_973)
}

#[test]
fn atan_accuracy() -> Result<(), String> {
    check_unary("atan", atan, f64::atan, 9_973)
}

#[test]
fn exp_accuracy() -> Result<(), String> {
    check_unary("exp", exp, f64::exp, 9_973)
}

#[test]
fn ln_accuracy() -> Result<(), String> {
    check_unary("ln", ln, f64::ln, 9_973)
}

#[test]
fn atan2_accuracy() -> Result<(), String> {
    let mut worst = (0.0f64, 0.0f32, 0.0f32);
    let ys: Vec<f32> = sample(4_000_037).filter(|v| v.is_finite()).collect();
    let xs: Vec<f32> = sample(3_999_971).filter(|v| v.is_finite()).collect();
    for &y in &ys {
        for &x in &xs {
            let e = ulp_error(atan2(y, x), f64::from(y).atan2(f64::from(x)));
            if e > worst.0 {
                worst = (e, y, x);
            }
        }
    }
    if worst.0 <= BOUND {
        Ok(())
    } else {
        Err(format!(
            "atan2 worst {} ulp at ({:e}, {:e})",
            worst.0, worst.1, worst.2
        ))
    }
}

#[test]
fn pow_accuracy() -> Result<(), String> {
    let mut worst = (0.0f64, 0.0f32, 0.0f32);
    let xs: Vec<f32> = sample(3_000_017).filter(|v| v.is_finite() && *v > 0.0).collect();
    let ys: Vec<f32> = [
        -3.0f32, -2.5, -1.0, -0.5, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 7.0, 10.0, 0.1, -0.3, 33.3,
    ]
    .into_iter()
    .chain(sample(50_000_017).filter(|v| v.is_finite()))
    .collect();
    for &x in &xs {
        for &y in &ys {
            let e = ulp_error(pow(x, y), f64::from(x).powf(f64::from(y)));
            if e > worst.0 {
                worst = (e, x, y);
            }
        }
    }
    if worst.0 <= BOUND {
        Ok(())
    } else {
        Err(format!(
            "pow worst {} ulp at ({:e}, {:e})",
            worst.0, worst.1, worst.2
        ))
    }
}

/// The measurement itself: a value one ulp off is reported as one ulp.
#[test]
fn ulp_error_measures_correctly() {
    let one = 1.0f32;
    let next = f32::from_bits(one.to_bits() + 1);
    assert!((ulp_error(next, 1.0) - 1.0).abs() < 1e-12);
    assert_eq!(ulp_error(one, 1.0), 0.0);
    assert_eq!(ulp_error(f32::NAN, f64::NAN), 0.0);
    assert_eq!(ulp_error(1.0, f64::NAN), f64::INFINITY);
}

/// Every `f32` input for the unary functions. Release mode recommended.
#[test]
#[ignore = "exhaustive: run in release mode on demand"]
fn exhaustive_unary() -> Result<(), String> {
    check_unary("sin", sin, f64::sin, 1)?;
    check_unary("cos", cos, f64::cos, 1)?;
    check_unary("tan", tan, f64::tan, 1)?;
    check_unary("atan", atan, f64::atan, 1)?;
    check_unary("exp", exp, f64::exp, 1)?;
    check_unary("ln", ln, f64::ln, 1)
}
