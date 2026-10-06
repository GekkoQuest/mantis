//! Exact argument reduction modulo pi/2 for every finite `f32`.
//!
//! An `f32` is `m * 2^e` with `m` a 24-bit integer. Its product with 2/pi is
//! computed in integer arithmetic against a 96-bit window of the binary
//! expansion of 2/pi (a Payne-Hanek reduction), which yields the quadrant and
//! the fractional part with an absolute error below 2^-70 quadrants for every
//! input. This is exact and deterministic by construction: integers only,
//! until the fraction is converted to `f64` once.

use super::kernels::{PIO2, PIO4, pow2};

/// The binary expansion of 2/pi, 32 bits per word, most significant first:
/// bit `i` (1-based) of the expansion has weight 2^-i. 320 bits, of which
/// at most 198 are used (the largest `f32` exponent needs bits up to 198).
/// Computed exactly with integer arithmetic from Machin's formula.
const TWO_OVER_PI: [u32; 10] = [
    0xA2F9_836E,
    0x4E44_1529,
    0xFC27_57D1,
    0xF534_DDC0,
    0xDB62_9599,
    0x3C43_9041,
    0xFE51_63AB,
    0xDEBB_C561,
    0xB724_6E3A,
    0x424D_D2E0,
];

fn word(i: usize) -> u128 {
    u128::from(TWO_OVER_PI.get(i).copied().unwrap_or(0))
}

/// Bits `s..s+96` (1-based) of 2/pi as a 96-bit integer, for `s >= 1`.
fn window96(s: u32) -> u128 {
    let first = (s - 1) as usize;
    let w = first / 32;
    let o = first % 32;
    let v = (word(w) << 96) | (word(w + 1) << 64) | (word(w + 2) << 32) | word(w + 3);
    (v << o) >> 32
}

/// Reduces `|x|` for a finite `f32` `x`: returns `(q, r)` with
/// `|x| = q * pi/2 + r` (q taken modulo 4) and `|r| <= pi/4 + tiny`.
///
/// `r` carries a relative error below 2^-40 for every `f32` input and below
/// 2^-50 for all but inputs pathologically close to a multiple of pi/2.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(super) fn rem_pio2(x: f32) -> (u32, f64) {
    let ax = x.abs();
    if f64::from(ax) <= PIO4 {
        return (0, f64::from(ax));
    }
    let bits = ax.to_bits();
    // Normal numbers only reach here (|x| > pi/4); exponent field 1..=254.
    let biased = (bits >> 23).cast_signed();
    let m = u128::from((bits & 0x007F_FFFF) | 0x0080_0000);
    let e = biased - 150; // |x| = m * 2^e, e in [-24, 104]
    let s = (e - 1).max(1);
    let shift = (s + 95 - e) as u32; // 94 ..= 120
    let p = m * window96(s as u32); // < 2^120
    let mask = (1u128 << shift) - 1;
    let mut q = ((p >> shift) & 3) as u32;
    let mut frac = (p & mask).cast_signed();
    if frac >= (1i128 << (shift - 1)) {
        frac -= 1i128 << shift;
        q = (q + 1) & 3;
    }
    let r = (frac as f64) * pow2(-shift.cast_signed()) * PIO2;
    (q, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_alignment() {
        assert_eq!(window96(1), 0xA2F9_836E_4E44_1529_FC27_57D1);
        assert_eq!(window96(33), 0x4E44_1529_FC27_57D1_F534_DDC0);
        // Offset by 4 bits: drop the leading nibble 0xA.
        assert_eq!(window96(5), 0x2F98_36E4_E441_529F_C275_7D1F);
    }

    #[test]
    fn small_and_quadrant_cases() {
        assert_eq!(rem_pio2(0.5), (0, 0.5));
        assert_eq!(rem_pio2(-0.5), (0, 0.5));
        let (q, r) = rem_pio2(core::f32::consts::FRAC_PI_2);
        assert_eq!(q, 1);
        assert!(r.abs() < 1e-7, "{r}");
        let (q, r) = rem_pio2(core::f32::consts::PI);
        assert_eq!(q, 2);
        assert!(r.abs() < 1e-6, "{r}");
        let (q, r) = rem_pio2(3.0 * core::f32::consts::FRAC_PI_2);
        assert_eq!(q, 3);
        assert!(r.abs() < 1e-6, "{r}");
        let (q, r) = rem_pio2(2.0);
        assert_eq!(q, 1);
        assert!((r - (2.0 - PIO2)).abs() < 1e-15, "{r}");
    }

    #[test]
    fn largest_input_reduces_into_range() {
        let (q, r) = rem_pio2(f32::MAX);
        assert!(q < 4);
        assert!(r.abs() <= PIO4 + 1e-12);
    }
}
