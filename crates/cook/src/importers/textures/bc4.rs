//! BC4 (unsigned): 8 bytes per 4x4 block of one channel.
//!
//! # Block layout
//!
//! | bytes | field |
//! |---|---|
//! | 0 | `red0` |
//! | 1 | `red1` |
//! | 2..8 | 16 three-bit indices (48-bit little-endian), texel `i` (row-major) at bits `3i..3i+3` |
//!
//! When `red0 > red1` the block is in **eight-value** mode: palette `red0, red1`, then
//! `((8 - i)·red0 + (i - 1)·red1) / 7` for `i` in 2 to 7. Otherwise it is in **six-value**
//! mode: `red0, red1`, `((6 - i)·red0 + (i - 1)·red1) / 5` for `i` in 2 to 5, then 0 and 255.
//! This decoder rounds the interpolants to nearest (adding half the divisor, rounded
//! down); hardware may differ by one step, which the spec allows.
//!
//! # Encoder
//!
//! Eight-value mode with the block's maximum and minimum as endpoints; when the block
//! contains 0 or 255, six-value mode with the minimum and maximum of the other values (0
//! and 255 then come from indices 6 and 7) is tried too, and the lower squared error wins
//! (eight-value on a tie). Each value takes its nearest palette entry (lowest index on a
//! tie). A solid block stores its value in both endpoints with every index 0.

/// Version of this encoder, recorded in every BC4 texture it produces (and, through
/// [`super::bc5::ENCODER_VERSION`], every BC5 texture). Bump it whenever the same block
/// would encode to different bytes.
pub const ENCODER_VERSION: u32 = 1;

/// The eight palette values of endpoints `red0` and `red1`.
pub fn palette(red0: u8, red1: u8) -> [u8; 8] {
    let (a, b) = (u32::from(red0), u32::from(red1));
    let mut out = [0u8; 8];
    for (i, slot) in (0u32..).zip(out.iter_mut()) {
        *slot = match i {
            0 => red0,
            1 => red1,
            _ if red0 > red1 => super::fit::byte(((8 - i) * a + (i - 1) * b + 3) / 7),
            6 => 0,
            7 => 255,
            _ => super::fit::byte(((6 - i) * a + (i - 1) * b + 2) / 5),
        };
    }
    out
}

/// Decodes one block to 16 values, row-major.
pub fn decode_block(block: &[u8; 8]) -> [u8; 16] {
    let [red0, red1, i0, i1, i2, i3, i4, i5] = *block;
    let pal = palette(red0, red1);
    let indices = u64::from_le_bytes([i0, i1, i2, i3, i4, i5, 0, 0]);
    let mut out = [0u8; 16];
    for (shift, value) in (0u32..).step_by(3).zip(out.iter_mut()) {
        let i = (indices >> shift) & 7;
        *value = usize::try_from(i)
            .ok()
            .and_then(|i| pal.get(i))
            .copied()
            .unwrap_or(0);
    }
    out
}

/// Encodes 16 values (row-major).
pub fn encode_block(values: &[u8; 16]) -> [u8; 8] {
    let max = values.iter().copied().max().unwrap_or(0);
    let min = values.iter().copied().min().unwrap_or(0);
    let eight = fit(max, min, values);
    if values.iter().any(|&v| v == 0 || v == 255) {
        let inner = values.iter().copied().filter(|&v| v != 0 && v != 255);
        let lo = inner.clone().min().unwrap_or(0);
        let hi = inner.max().unwrap_or(0);
        let six = fit(lo, hi, values);
        if six.1 < eight.1 {
            return six.0;
        }
    }
    eight.0
}

/// The block with endpoints `red0`, `red1` (the order selects the mode) and each value's
/// nearest index, with its squared error.
fn fit(red0: u8, red1: u8, values: &[u8; 16]) -> ([u8; 8], u32) {
    let pal = palette(red0, red1);
    let mut bits = 0u64;
    let mut error = 0u32;
    for (shift, &v) in (0u32..).step_by(3).zip(values) {
        let mut best = (u32::MAX, 0u64);
        for (i, &p) in (0u64..).zip(&pal) {
            let d = u32::from(v.abs_diff(p));
            if d * d < best.0 {
                best = (d * d, i);
            }
        }
        error += best.0;
        bits |= best.1 << shift;
    }
    let [i0, i1, i2, i3, i4, i5, _, _] = bits.to_le_bytes();
    ([red0, red1, i0, i1, i2, i3, i4, i5], error)
}
