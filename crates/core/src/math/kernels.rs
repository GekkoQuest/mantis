//! `f64` kernels for the `f32` public API.
//!
//! Every function here uses only IEEE-754 add, subtract, multiply, divide,
//! comparisons, `round`, and bit manipulation. Those operations are
//! correctly rounded and therefore bit-identical on every IEEE target. Rust
//! never contracts `a * b + c` into a fused multiply-add and never enables
//! fast-math, so the evaluation order written here is the order executed
//! (decision 0014).
//!
//! Accuracy target: each kernel's result is within a few `f64` ulps of the true
//! value over its documented domain, roughly 2^-50 relative. The public `f32`
//! functions round that once, so their results are the correctly rounded `f32`
//! except when the true value lies within about 2^-26 `f32` ulp of a rounding
//! boundary. The tests in `math/mod.rs` measure this.

/// pi/2 rounded to `f64`.
pub(super) const PIO2: f64 = f64::from_bits(0x3FF9_21FB_5444_2D18);
/// pi rounded to `f64`.
pub(super) const PI: f64 = f64::from_bits(0x4009_21FB_5444_2D18);
/// pi/4 rounded to `f64`.
pub(super) const PIO4: f64 = f64::from_bits(0x3FE9_21FB_5444_2D18);
/// 1/ln(2) rounded to `f64`.
const INV_LN2: f64 = f64::from_bits(0x3FF7_1547_652B_82FE);
/// ln(2) truncated to 32 significant bits: `k * LN2_HI` is exact for |k| < 2^21.
const LN2_HI: f64 = f64::from_bits(0x3FE6_2E42_FEE0_0000);
/// `ln(2) - LN2_HI` rounded to `f64`.
const LN2_LO: f64 = f64::from_bits(0x3DEA_39EF_3579_3C76);
/// sqrt(2) rounded to `f64`; the split point of the log reduction.
const SQRT2: f64 = f64::from_bits(0x3FF6_A09E_667F_3BCD);

/// atan(k/8) for k = 0..=8, each correctly rounded to `f64` (computed to
/// 150 digits and rounded once).
const ATAN_K: [f64; 9] = [
    0.0,
    f64::from_bits(0x3FBF_D5BA_9AAC_2F6E),
    f64::from_bits(0x3FCF_5B75_F92C_80DD),
    f64::from_bits(0x3FD6_F619_41E4_DEF1),
    f64::from_bits(0x3FDD_AC67_0561_BB4F),
    f64::from_bits(0x3FE1_E00B_ABDE_FEB4),
    f64::from_bits(0x3FE4_978F_A326_9EE1),
    f64::from_bits(0x3FE7_00A7_C578_4634),
    f64::from_bits(0x3FE9_21FB_5444_2D18),
];

/// 2^n for an integer n with the result a normal `f64` (-1022 <= n <= 1023),
/// built from bits, so it is exact.
#[allow(clippy::cast_sign_loss)]
pub(super) const fn pow2(n: i32) -> f64 {
    f64::from_bits(((n + 1023) as u64) << 52)
}

/// sin(r) for |r| <= pi/4 (and slightly beyond). Taylor series through r^15;
/// the truncation error is below 6.5e-17 relative at pi/4.
pub(super) fn sin_kernel(r: f64) -> f64 {
    const S1: f64 = -1.0 / 6.0;
    const S2: f64 = 1.0 / 120.0;
    const S3: f64 = -1.0 / 5040.0;
    const S4: f64 = 1.0 / 362_880.0;
    const S5: f64 = -1.0 / 39_916_800.0;
    const S6: f64 = 1.0 / 6_227_020_800.0;
    const S7: f64 = -1.0 / 1_307_674_368_000.0;
    let z = r * r;
    let p = S2 + z * (S3 + z * (S4 + z * (S5 + z * (S6 + z * S7))));
    r + r * z * (S1 + z * p)
}

/// cos(r) for |r| <= pi/4 (and slightly beyond). Taylor series through r^16;
/// the truncation error is below 3e-18 at pi/4.
pub(super) fn cos_kernel(r: f64) -> f64 {
    const C1: f64 = -0.5;
    const C2: f64 = 1.0 / 24.0;
    const C3: f64 = -1.0 / 720.0;
    const C4: f64 = 1.0 / 40_320.0;
    const C5: f64 = -1.0 / 3_628_800.0;
    const C6: f64 = 1.0 / 479_001_600.0;
    const C7: f64 = -1.0 / 87_178_291_200.0;
    const C8: f64 = 1.0 / 20_922_789_888_000.0;
    let z = r * r;
    1.0 + z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * (C6 + z * (C7 + z * C8)))))))
}

/// e^t for -160 <= t <= 130 (callers clamp). Cody-Waite reduction by ln 2,
/// then a Taylor series through r^13 on |r| <= 0.347 (truncation below
/// 4.2e-18 relative), then an exact scaling by 2^k.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn exp_kernel(t: f64) -> f64 {
    const E: [f64; 14] = [
        1.0,
        1.0,
        1.0 / 2.0,
        1.0 / 6.0,
        1.0 / 24.0,
        1.0 / 120.0,
        1.0 / 720.0,
        1.0 / 5040.0,
        1.0 / 40_320.0,
        1.0 / 362_880.0,
        1.0 / 3_628_800.0,
        1.0 / 39_916_800.0,
        1.0 / 479_001_600.0,
        1.0 / 6_227_020_800.0,
    ];
    let k = (t * INV_LN2).round();
    let r = (t - k * LN2_HI) - k * LN2_LO;
    let mut p = 0.0;
    for c in E.iter().rev() {
        p = c + r * p;
    }
    p * pow2(k as i32)
}

/// ln(v) for finite positive normal `v`. Reduction to m in [sqrt(2)/2, sqrt(2))
/// by the exponent bits, then ln(m) = 2 atanh(s) with s = (m-1)/(m+1),
/// |s| <= 0.1716, series through s^19 (truncation below 2e-17 relative).
#[allow(clippy::many_single_char_names)]
pub(super) fn ln_kernel(v: f64) -> f64 {
    const L: [f64; 10] = [
        1.0,
        1.0 / 3.0,
        1.0 / 5.0,
        1.0 / 7.0,
        1.0 / 9.0,
        1.0 / 11.0,
        1.0 / 13.0,
        1.0 / 15.0,
        1.0 / 17.0,
        1.0 / 19.0,
    ];
    let bits = v.to_bits();
    let mut e = ((bits >> 52) & 0x7FF) as i32 - 1023;
    let mut m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    if m > SQRT2 {
        m *= 0.5;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let z = s * s;
    let mut p = 0.0;
    for c in L.iter().rev() {
        p = c + z * p;
    }
    let ln_m = 2.0 * s * p;
    let ef = f64::from(e);
    (ef * LN2_HI + ln_m) + ef * LN2_LO
}

/// atan(t) for 0 <= t <= 1. Table point c = k/8 nearest t, then
/// atan(t) = atan(c) + atan(u) with u = (t - c)/(1 + t c), |u| <= 1/16,
/// and a Taylor series for atan(u) through u^15 (truncation below 1e-20).
#[allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(super) fn atan_unit(t: f64) -> f64 {
    const A: [f64; 8] = [
        1.0,
        -1.0 / 3.0,
        1.0 / 5.0,
        -1.0 / 7.0,
        1.0 / 9.0,
        -1.0 / 11.0,
        1.0 / 13.0,
        -1.0 / 15.0,
    ];
    let k = (t * 8.0).round();
    let c = k * 0.125;
    let u = (t - c) / (1.0 + t * c);
    let z = u * u;
    let mut p = 0.0;
    for a in A.iter().rev() {
        p = a + z * p;
    }
    let base = ATAN_K.get(k as usize).copied().unwrap_or(PIO4);
    base + u * p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_their_definitions() {
        // Split constants reassemble to the rounded value.
        assert_eq!(LN2_HI + LN2_LO, f64::from_bits(0x3FE6_2E42_FEFA_39EF));
        assert_eq!(LN2_HI.to_bits() & 0x1F_FFFF, 0, "low 21 bits clear");
        assert_eq!(PIO2 * 2.0, PI);
        assert_eq!(PIO4 * 4.0, PI);
        assert_eq!(ATAN_K[8], PIO4);
        assert_eq!(SQRT2 * SQRT2, 2.000_000_000_000_000_4);
        assert_eq!(pow2(0), 1.0);
        assert_eq!(pow2(-3), 0.125);
        assert_eq!(pow2(10), 1024.0);
    }

    #[test]
    fn kernels_exact_points() {
        assert_eq!(sin_kernel(0.0), 0.0);
        assert_eq!(cos_kernel(0.0), 1.0);
        assert_eq!(exp_kernel(0.0), 1.0);
        assert_eq!(ln_kernel(1.0), 0.0);
        assert_eq!(atan_unit(0.0), 0.0);
        assert_eq!(atan_unit(1.0), PIO4);
    }
}
