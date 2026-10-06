//! Deterministic math for the simulation (decision 0013, principle 9).
//!
//! IEEE add, subtract, multiply, divide, and square root are correctly rounded
//! and bit-identical across `x86_64` and aarch64. Library transcendentals are
//! not. This module provides the transcendentals the simulation uses,
//! implemented in-crate from those exact operations only, so a client and a
//! server on different architectures compute the same bits by construction.
//! The `clippy.toml` of every simulation crate bans the `std` versions.
//!
//! # Design
//!
//! The simulation scalar is `f32`. Each function widens its argument to `f64`
//! (exact), evaluates an `f64` kernel accurate to a few `f64` ulps, and rounds
//! to `f32` once. Range reduction for `sin`/`cos`/`tan` is an exact integer
//! Payne-Hanek reduction valid for every finite `f32`.
//!
//! # Accuracy (documented bound, enforced by tests)
//!
//! For `sin`, `cos`, `tan`, `atan`, `atan2`, `exp`, `ln`, and `pow`, the result
//! differs from the exact mathematical value by at most **0.5001 `f32` ulp**
//! over the whole `f32` domain. In other words it is the correctly rounded
//! result except when the exact value lies within 1e-4 ulp of a rounding
//! midpoint. `sqrt` is IEEE-exact (0.5 ulp). `mantis-testkit`'s
//! `tests/math_accuracy.rs` checks the bound on dense deterministic samples
//! against an `f64` reference (the `std` oracle cannot live in a simulation
//! crate), and its ignored `exhaustive_unary` test sweeps all 2^32 inputs.
//!
//! # Special values
//!
//! Special cases follow C99 Annex F (IEEE 754): signed zeros are preserved
//! where Annex F says so, and infinities map as specified. **Every NaN result is
//! the canonical quiet NaN [`CANONICAL_NAN`] (`0x7FC0_0000`)**, because NaN sign
//! and payload differ between `x86_64` (`0xFFC0_0000`) and aarch64 (`0x7FC0_0000`)
//! for the same operation, and state hashes must agree.
//!
//! # What is not here
//!
//! Plain arithmetic, `abs`, `floor`, `ceil`, `round`, `trunc`, `min`, `max`,
//! and `copysign` are exact in IEEE and need no wrapper. Note that arithmetic
//! producing NaN (for example `0.0 / 0.0`) still yields a target-dependent NaN;
//! [`canonicalize`] exists for code that must store such a value.

mod kernels;
mod reduce;
mod vec3;

pub use vec3::Vec3;

#[expect(clippy::cast_possible_truncation)]
fn to_f32(v: f64) -> f32 {
    v as f32
}

/// The canonical quiet NaN returned by every function in this module.
pub const CANONICAL_NAN: f32 = f32::from_bits(0x7FC0_0000);

/// Replaces any NaN by [`CANONICAL_NAN`]; every other value passes unchanged.
#[must_use]
pub fn canonicalize(x: f32) -> f32 {
    if x.is_nan() { CANONICAL_NAN } else { x }
}

/// Square root, IEEE-exact. NaN for negative inputs (canonical); `sqrt(-0) = -0`.
///
/// `f32::sqrt` itself is permitted in simulation crates: IEEE 754 requires a
/// correctly rounded square root, so it is bit-identical on every target.
/// This wrapper exists for the one thing `f32::sqrt` does not guarantee, the
/// bit pattern of the NaN for a negative input, which differs between
/// `x86_64` and aarch64. Use it wherever the result may be stored or hashed.
#[must_use]
pub fn sqrt(x: f32) -> f32 {
    canonicalize(x.sqrt())
}

#[expect(clippy::many_single_char_names)] // mathematical notation
fn sin_cos_reduced(x: f32) -> (f32, f32) {
    let (q, r) = reduce::rem_pio2(x);
    let s = kernels::sin_kernel(r);
    let c = kernels::cos_kernel(r);
    let (sin_abs, cos) = match q {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    };
    let sin = if x.is_sign_negative() { -sin_abs } else { sin_abs };
    (to_f32(sin), to_f32(cos))
}

/// Sine. `sin(±0) = ±0`; `sin(±inf)` and `sin(NaN)` are NaN.
#[must_use]
pub fn sin(x: f32) -> f32 {
    if !x.is_finite() {
        return CANONICAL_NAN;
    }
    if x == 0.0 {
        return x;
    }
    sin_cos_reduced(x).0
}

/// Cosine. `cos(±0) = 1`; `cos(±inf)` and `cos(NaN)` are NaN.
#[must_use]
pub fn cos(x: f32) -> f32 {
    if !x.is_finite() {
        return CANONICAL_NAN;
    }
    sin_cos_reduced(x).1
}

/// Sine and cosine with one shared reduction. Identical bits to calling
/// [`sin`] and [`cos`] separately.
#[must_use]
pub fn sin_cos(x: f32) -> (f32, f32) {
    if !x.is_finite() {
        return (CANONICAL_NAN, CANONICAL_NAN);
    }
    if x == 0.0 {
        return (x, 1.0);
    }
    sin_cos_reduced(x)
}

/// Tangent. `tan(±0) = ±0`; `tan(±inf)` and `tan(NaN)` are NaN.
#[must_use]
#[expect(clippy::many_single_char_names)] // mathematical notation
pub fn tan(x: f32) -> f32 {
    if !x.is_finite() {
        return CANONICAL_NAN;
    }
    if x == 0.0 {
        return x;
    }
    let (q, r) = reduce::rem_pio2(x);
    let s = kernels::sin_kernel(r);
    let c = kernels::cos_kernel(r);
    let t = if q & 1 == 0 { s / c } else { -c / s };
    let t = if x.is_sign_negative() { -t } else { t };
    to_f32(t)
}

/// atan(a) for finite non-negative `f64` a (from an `f32`).
fn atan_pos(a: f64) -> f64 {
    if a <= 1.0 {
        kernels::atan_unit(a)
    } else {
        kernels::PIO2 - kernels::atan_unit(1.0 / a)
    }
}

/// Arctangent, in `[-pi/2, pi/2]`. `atan(±0) = ±0`, `atan(±inf) = ±pi/2`.
#[must_use]
pub fn atan(x: f32) -> f32 {
    if x.is_nan() {
        return CANONICAL_NAN;
    }
    if x == 0.0 {
        return x;
    }
    let a = f64::from(x.abs());
    let r = if x.is_infinite() {
        kernels::PIO2
    } else {
        atan_pos(a)
    };
    to_f32(if x.is_sign_negative() { -r } else { r })
}

/// Four-quadrant arctangent of `y / x`, in `[-pi, pi]`, with every special
/// case of C99 Annex F: signed zeros select the half-plane, and infinities give
/// multiples of pi/4.
#[must_use]
pub fn atan2(y: f32, x: f32) -> f32 {
    if x.is_nan() || y.is_nan() {
        return CANONICAL_NAN;
    }
    let neg_y = y.is_sign_negative();
    let neg_x = x.is_sign_negative();
    let ay = f64::from(y.abs());
    let ax = f64::from(x.abs());
    let theta = if y == 0.0 {
        // atan2(±0, +0 or x>0) = ±0; atan2(±0, -0 or x<0) = ±pi.
        if neg_x { kernels::PI } else { 0.0 }
    } else if x == 0.0 {
        kernels::PIO2
    } else if y.is_infinite() {
        if x.is_infinite() {
            if neg_x { 3.0 * kernels::PIO4 } else { kernels::PIO4 }
        } else {
            kernels::PIO2
        }
    } else if x.is_infinite() {
        if neg_x { kernels::PI } else { 0.0 }
    } else {
        let first = if ay <= ax {
            kernels::atan_unit(ay / ax)
        } else {
            kernels::PIO2 - kernels::atan_unit(ax / ay)
        };
        if neg_x { kernels::PI - first } else { first }
    };
    let theta = to_f32(theta);
    if neg_y { -theta } else { theta }
}

/// e^x. Overflows to `+inf` above about 88.72, underflows through the
/// subnormals to `+0` below about -103.97. `exp(-inf) = 0`, `exp(+inf) = inf`.
#[must_use]
pub fn exp(x: f32) -> f32 {
    if x.is_nan() {
        return CANONICAL_NAN;
    }
    if x > 89.0 {
        return f32::INFINITY;
    }
    if x < -110.0 {
        return 0.0;
    }
    to_f32(kernels::exp_kernel(f64::from(x)))
}

/// Natural logarithm. `ln(±0) = -inf`, `ln(x < 0) = NaN`, `ln(+inf) = +inf`,
/// `ln(1) = +0`.
#[must_use]
pub fn ln(x: f32) -> f32 {
    if x.is_nan() || x < 0.0 {
        return CANONICAL_NAN;
    }
    if x == 0.0 {
        return f32::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f32::INFINITY;
    }
    to_f32(kernels::ln_kernel(f64::from(x)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parity {
    NotInteger,
    Even,
    Odd,
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parity(y: f32) -> Parity {
    let a = y.abs();
    if a >= 16_777_216.0 {
        return Parity::Even; // every f32 at or above 2^24 is an even integer
    }
    if a.trunc() != a {
        return Parity::NotInteger;
    }
    if (a as u32) & 1 == 1 {
        Parity::Odd
    } else {
        Parity::Even
    }
}

/// `x` raised to `y`, with every special case of C99 Annex F. In particular
/// `pow(x, ±0) = 1` for any `x` (even NaN), `pow(1, y) = 1` for any `y` (even
/// NaN), a negative base with a non-integer exponent is NaN, and a negative
/// base with an integer exponent takes the sign of the parity. Integer powers
/// whose result is representable come out exact.
#[must_use]
pub fn pow(x: f32, y: f32) -> f32 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return CANONICAL_NAN;
    }
    if y.is_infinite() {
        let ax = x.abs();
        return if ax == 1.0 {
            1.0
        } else if (ax < 1.0) == (y > 0.0) {
            0.0
        } else {
            f32::INFINITY
        };
    }
    let par = parity(y);
    if x == 0.0 {
        let odd = par == Parity::Odd;
        return match (y < 0.0, odd) {
            (true, true) => {
                if x.is_sign_negative() {
                    f32::NEG_INFINITY
                } else {
                    f32::INFINITY
                }
            }
            (true, false) => f32::INFINITY,
            (false, true) => x,
            (false, false) => 0.0,
        };
    }
    if x.is_infinite() {
        let odd = par == Parity::Odd;
        return match (x > 0.0, y < 0.0, odd) {
            (true, true, _) | (false, true, false) => 0.0,
            (true, false, _) | (false, false, false) => f32::INFINITY,
            (false, true, true) => -0.0,
            (false, false, true) => f32::NEG_INFINITY,
        };
    }
    let negate = if x < 0.0 {
        match par {
            Parity::NotInteger => return CANONICAL_NAN,
            Parity::Odd => true,
            Parity::Even => false,
        }
    } else {
        false
    };
    let t = f64::from(y) * kernels::ln_kernel(f64::from(x.abs()));
    let r = if t > 130.0 {
        f32::INFINITY
    } else if t < -160.0 {
        0.0
    } else {
        to_f32(kernels::exp_kernel(t))
    };
    if negate { -r } else { r }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic sample of f32 bit patterns: a stride through all of them.
    fn sample(stride: u32) -> impl Iterator<Item = f32> {
        (0..=u32::MAX / stride).map(move |i| f32::from_bits(i.wrapping_mul(stride)))
    }

    #[test]
    fn sqrt_is_ieee() {
        for x in sample(65_537) {
            let got = sqrt(x);
            if x < 0.0 {
                assert!(got.is_nan() && got.to_bits() == CANONICAL_NAN.to_bits());
            } else {
                assert_eq!(got.to_bits(), canonicalize(x.sqrt()).to_bits());
            }
        }
        assert_eq!(sqrt(-0.0).to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn sin_cos_matches_separate_calls() {
        for x in sample(1_000_003) {
            let (s, c) = sin_cos(x);
            assert_eq!(s.to_bits(), sin(x).to_bits(), "{x:e}");
            assert_eq!(c.to_bits(), cos(x).to_bits(), "{x:e}");
        }
    }

    #[test]
    fn special_values() {
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let canon = CANONICAL_NAN.to_bits();
        for v in [
            sin(inf),
            cos(-inf),
            tan(inf),
            sin(nan),
            exp(nan),
            ln(-1.0),
            ln(nan),
            pow(-2.0, 0.5),
            atan2(nan, 1.0),
            sqrt(-1.0),
        ] {
            assert_eq!(v.to_bits(), canon);
        }
        assert_eq!(sin(-0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(tan(-0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(atan(-0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(cos(-0.0), 1.0);
        assert_eq!(sin_cos(-0.0).0.to_bits(), (-0.0f32).to_bits());
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(-inf), 0.0);
        assert_eq!(exp(inf), inf);
        assert_eq!(exp(89.0), inf);
        // e^-103 is 1.32 smallest subnormals and e^-104 is 0.49 of one.
        assert_eq!(exp(-103.0), f32::from_bits(1));
        assert_eq!(exp(-104.0), 0.0);
        assert_eq!(ln(0.0), -inf);
        assert_eq!(ln(-0.0), -inf);
        assert_eq!(ln(inf), inf);
        assert_eq!(ln(1.0).to_bits(), 0);
        assert_eq!(atan(inf), core::f32::consts::FRAC_PI_2);
        assert_eq!(atan(-inf), -core::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn atan2_annex_f() {
        use core::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        let inf = f32::INFINITY;
        let b = |v: f32| v.to_bits();
        assert_eq!(b(atan2(0.0, 0.0)), b(0.0));
        assert_eq!(b(atan2(-0.0, 0.0)), b(-0.0));
        assert_eq!(atan2(0.0, -0.0), PI);
        assert_eq!(atan2(-0.0, -0.0), -PI);
        assert_eq!(atan2(0.0, -1.0), PI);
        assert_eq!(atan2(-0.0, -1.0), -PI);
        assert_eq!(b(atan2(-0.0, 1.0)), b(-0.0));
        assert_eq!(atan2(1.0, 0.0), FRAC_PI_2);
        assert_eq!(atan2(-1.0, -0.0), -FRAC_PI_2);
        assert_eq!(atan2(inf, inf), FRAC_PI_4);
        assert_eq!(atan2(-inf, inf), -FRAC_PI_4);
        assert_eq!(atan2(inf, -inf), 3.0 * FRAC_PI_4);
        assert_eq!(atan2(inf, 5.0), FRAC_PI_2);
        assert_eq!(atan2(1.0, inf), 0.0);
        assert_eq!(b(atan2(-1.0, inf)), b(-0.0));
        assert_eq!(atan2(1.0, -inf), PI);
        assert_eq!(atan2(1.0, 1.0), FRAC_PI_4);
        assert_eq!(atan2(1.0, -1.0), 3.0 * FRAC_PI_4);
    }

    #[test]
    fn pow_annex_f() {
        let inf = f32::INFINITY;
        let nan = f32::NAN;
        let b = |v: f32| v.to_bits();
        assert_eq!(pow(nan, 0.0), 1.0);
        assert_eq!(pow(nan, -0.0), 1.0);
        assert_eq!(pow(1.0, nan), 1.0);
        assert_eq!(pow(1.0, inf), 1.0);
        assert_eq!(pow(-1.0, inf), 1.0);
        assert_eq!(pow(-1.0, -inf), 1.0);
        assert_eq!(pow(0.5, inf), 0.0);
        assert_eq!(pow(0.5, -inf), inf);
        assert_eq!(pow(2.0, inf), inf);
        assert_eq!(pow(2.0, -inf), 0.0);
        assert_eq!(pow(0.0, -3.0), inf);
        assert_eq!(pow(-0.0, -3.0), -inf);
        assert_eq!(pow(-0.0, -2.0), inf);
        assert_eq!(pow(-0.0, -0.5), inf);
        assert_eq!(b(pow(-0.0, 3.0)), b(-0.0));
        assert_eq!(b(pow(-0.0, 2.0)), b(0.0));
        assert_eq!(b(pow(-0.0, 0.5)), b(0.0));
        assert_eq!(b(pow(-inf, -3.0)), b(-0.0));
        assert_eq!(b(pow(-inf, -2.0)), b(0.0));
        assert_eq!(pow(-inf, 3.0), -inf);
        assert_eq!(pow(-inf, 2.0), inf);
        assert_eq!(pow(inf, -1.0), 0.0);
        assert_eq!(pow(inf, 0.5), inf);
        assert_eq!(b(pow(-2.0, 0.5)), CANONICAL_NAN.to_bits());
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert_eq!(pow(-2.0, 2.0), 4.0);
        assert_eq!(pow(-2.0, 3.0e10), inf, "huge exponents are even");
        // Exact integer powers.
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(pow(3.0, 5.0), 243.0);
        assert_eq!(pow(10.0, 7.0), 10_000_000.0);
        assert_eq!(pow(2.0, -2.0), 0.25);
        assert_eq!(pow(4.0, 0.5), 2.0);
        assert_eq!(pow(10.0, 39.0), inf);
        assert_eq!(pow(10.0, -50.0), 0.0);
    }

    /// Exact bit patterns for a fixed input vector. These are the values the
    /// cross-architecture soak relies on: any change on any target is a
    /// determinism break and fails here first.
    #[test]
    fn pinned_bit_patterns() {
        let inputs: [f32; 12] = [
            0.1, 0.5, 1.0, 1.5, 2.0, 3.0, 10.0, 100.0, -0.7, 1.0e-3, 12_345.678, 1.0e30,
        ];
        let mut got = Vec::new();
        for &x in &inputs {
            got.push((
                sin(x).to_bits(),
                cos(x).to_bits(),
                tan(x).to_bits(),
                atan(x).to_bits(),
            ));
        }
        assert_eq!(got, PINNED_TRIG.to_vec(), "trig bits changed");
        let mut got2 = Vec::new();
        for &x in &inputs {
            got2.push((
                exp(x).to_bits(),
                ln(x).to_bits(),
                pow(x, 1.7).to_bits(),
                atan2(x, -0.3).to_bits(),
            ));
        }
        assert_eq!(got2, PINNED_EXP.to_vec(), "exp/ln/pow/atan2 bits changed");
    }

    // Cross-checked against the correctly rounded f32 of an independent f64
    // reference for every entry (sin, cos, tan, atan).
    const PINNED_TRIG: [(u32, u32, u32, u32); 12] = [
        (0x3DCC_7577, 0x3F7E_B898, 0x3DCD_7C44, 0x3DCC_1F14),
        (0x3EF5_7744, 0x3F60_A940, 0x3F0B_DA7B, 0x3EED_6338),
        (0x3F57_6AA4, 0x3F0A_5140, 0x3FC7_5923, 0x3F49_0FDB),
        (0x3F7F_5BD5, 0x3D90_DEAA, 0x4161_9F6B, 0x3F7B_985F),
        (0x3F68_C7B7, 0xBED5_1133, 0xC00B_D7B1, 0x3F8D_B70D),
        (0x3E10_81C3, 0xBF7D_7026, 0xBE11_F7B9, 0x3F9F_E0BB),
        (0xBF0B_44F8, 0xBF56_CD64, 0x3F25_FAFA, 0x3FBC_4DE9),
        (0xBF01_A12E, 0x3F5C_C0EE, 0xBF16_53A7, 0x3FC7_C82F),
        (0xBF24_EB73, 0x3F43_CCB3, 0xBF57_A036, 0xBF1C_5889),
        (0x3A83_126E, 0x3F7F_FFF8, 0x3A83_1272, 0x3A83_126C),
        (0xBF34_4B08, 0x3F35_BE20, 0xBF7D_F549, 0x3FC9_0D33),
        (0xBF4A_89B0, 0xBF1C_9222, 0x3FA5_943B, 0x3FC9_0FDB),
    ];
    // Same cross-check (exp, ln, pow(x, 1.7), atan2(x, -0.3)).
    const PINNED_EXP: [(u32, u32, u32, u32); 12] = [
        (0x3F8D_763E, 0xC013_5D8E, 0x3CA3_73AE, 0x4034_784B),
        (0x3FD3_094C, 0xBF31_7218, 0x3E9D_9624, 0x4007_1E29),
        (0x402D_F854, 0x0000_0000, 0x3F80_0000, 0x3FEE_5E50),
        (0x408F_69FF, 0x3ECF_991F, 0x3FFF_03C0, 0x3FE2_541D),
        (0x40EC_7326, 0x3F31_7218, 0x404F_EFC6, 0x3FDC_1EAE),
        (0x41A0_AF2E, 0x3F8C_9F54, 0x40CF_22E2, 0x3FD5_D1CC),
        (0x46AC_14EE, 0x4013_5D8E, 0x4248_7994, 0x3FCC_E699),
        (0x7F80_0000, 0x4093_5D8E, 0x451C_FE31, 0x3FC9_7228),
        (0x3EFE_406E, 0x7FC0_0000, 0x7FC0_0000, 0xBFFC_E359),
        (0x3F80_20C9, 0xC0DD_0C55, 0x3705_4421, 0x4048_D93E),
        (0x7F80_0000, 0x4116_BCAB, 0x4B09_C054, 0x3FC9_10A6),
        (0x7F80_0000, 0x428A_27B5, 0x7F80_0000, 0x3FC9_0FDB),
    ];
}
