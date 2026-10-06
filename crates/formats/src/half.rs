//! IEEE 754 half precision conversion.

/// Half to single precision (exact).
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x3ff);
    let bits = match (exp, mant) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalize.
            let shift = m.leading_zeros() - 21;
            let m = (m << shift) & 0x3ff;
            sign | ((127 - 15 - shift + 1) << 23) | (m << 13)
        }
        (31, 0) => sign | 0x7f80_0000,
        (31, m) => sign | 0x7f80_0000 | (m << 13),
        (e, m) => sign | ((e + 127 - 15) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// Single to half precision, rounding to nearest even; overflow saturates to infinity, NaN
/// stays NaN.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)] // Bit-field arithmetic on masked, range-checked values.
pub fn f32_to_f16(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 31 {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let rounded = m + half - 1 + ((m >> shift) & 1);
        return sign | (rounded >> shift) as u16;
    }
    let rounded = mant + 0xfff + ((mant >> 13) & 1);
    let (e, m) = if rounded & 0x80_0000 != 0 {
        (e + 1, 0)
    } else {
        (e, rounded >> 13)
    };
    if e >= 31 {
        return sign | 0x7c00;
    }
    sign | ((e as u16) << 10) | (m as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rounds_to_nearest_even() {
        for v in [0.0f32, -0.0, 1.0, -2.5, 65504.0, 6.1e-5, 5.96e-8, 0.333_333_34] {
            let back = f16_to_f32(f32_to_f16(v));
            assert!((back - v).abs() <= v.abs() * 1e-3 + 6e-8, "{v} -> {back}");
        }
        assert_eq!(f16_to_f32(f32_to_f16(1.0e6)), f32::INFINITY);
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3c00, "ties round to even");
        assert_eq!(f32_to_f16(1.0 + 3.0 / 2048.0), 0x3c02);
        // Every finite half survives a round trip exactly.
        for h in 0..=u16::MAX {
            let f = f16_to_f32(h);
            if f.is_finite() {
                assert_eq!(f16_to_f32(f32_to_f16(f)).to_bits(), f.to_bits(), "{h:#06x}");
            }
        }
    }
}
