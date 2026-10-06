//! BC1 (DXT1): 8 bytes per 4x4 block of RGB with optional 1-bit alpha.
//!
//! # Block layout
//!
//! | bytes | field |
//! |---|---|
//! | 0..2 | `color0`, RGB 5:6:5 (`u16` little-endian, red in the high 5 bits) |
//! | 2..4 | `color1`, RGB 5:6:5 |
//! | 4..8 | 16 two-bit indices (`u32` little-endian), texel `i` (row-major) at bits `2i..2i+2` |
//!
//! Endpoints expand to 8 bits by bit replication (`r8 = r5 << 3 | r5 >> 2`,
//! `g8 = g6 << 2 | g6 >> 4`). When `color0 > color1` (as integers) the block is in
//! **four-color** mode: palette `c0, c1, (2·c0 + c1)/3, (c0 + 2·c1)/3`, all opaque. Otherwise
//! it is in **three-color** mode: `c0, c1, (c0 + c1)/2`, and index 3 is transparent black.
//! This decoder rounds the interpolants to nearest (`(2a + b + 1) / 3`, `(a + b + 1) / 2`);
//! hardware may differ by one step, which the spec allows.
//!
//! # Encoder
//!
//! Endpoints come from the principal axis of the block's colors (power iteration on the
//! covariance, fixed iteration count), refined by least squares on the chosen indices
//! (fixed pass count, keeping the best candidate measured with the decoder above). Opaque
//! blocks use four-color mode (`color0 > color1`; a block whose endpoints quantize to the
//! same color uses only indices 0 to 2, which decode to that color in either mode). With
//! an alpha cutoff, a block that has a texel below the cutoff uses three-color mode
//! (`color0 <= color1`): those texels take index 3 and the rest are fitted alone. Without a
//! cutoff alpha is ignored.

use super::fit::{V4, byte, from_u8, least_squares, principal_endpoints, round_clamp, texel_error};

/// Version of this encoder, recorded in every BC1 texture it produces. Bump it whenever
/// the same block would encode to different bytes.
pub const ENCODER_VERSION: u32 = 1;

/// Least-squares refinement passes.
const PASSES: usize = 3;

/// The 8-bit RGBA color of a 5:6:5 endpoint (alpha 255).
pub fn expand_565(c: u16) -> [u8; 4] {
    let c = u32::from(c);
    let r = (c >> 11) & 31;
    let g = (c >> 5) & 63;
    let b = c & 31;
    [
        byte((r << 3) | (r >> 2)),
        byte((g << 2) | (g >> 4)),
        byte((b << 3) | (b >> 2)),
        255,
    ]
}

/// The four palette entries of endpoints `c0` and `c1`, as the decoder sees them.
pub fn palette(c0: u16, c1: u16) -> [[u8; 4]; 4] {
    let a = expand_565(c0);
    let b = expand_565(c1);
    let mix = |f: fn(u32, u32) -> u32| -> [u8; 4] {
        [
            byte(f(u32::from(a[0]), u32::from(b[0]))),
            byte(f(u32::from(a[1]), u32::from(b[1]))),
            byte(f(u32::from(a[2]), u32::from(b[2]))),
            255,
        ]
    };
    if c0 > c1 {
        [
            a,
            b,
            mix(|x, y| (2 * x + y + 1) / 3),
            mix(|x, y| (x + 2 * y + 1) / 3),
        ]
    } else {
        [a, b, mix(|x, y| (x + y).div_ceil(2)), [0, 0, 0, 0]]
    }
}

/// Decodes one block to 16 RGBA texels, row-major.
pub fn decode_block(block: &[u8; 8]) -> [[u8; 4]; 16] {
    let [a0, a1, b0, b1, i0, i1, i2, i3] = *block;
    let pal = palette(u16::from_le_bytes([a0, a1]), u16::from_le_bytes([b0, b1]));
    let indices = u32::from_le_bytes([i0, i1, i2, i3]);
    let mut out = [[0u8; 4]; 16];
    for (shift, texel) in (0u32..).step_by(2).zip(out.iter_mut()) {
        let i = (indices >> shift) & 3;
        *texel = pal.get(i as usize).copied().unwrap_or_default();
    }
    out
}

/// Encodes 16 RGBA texels (row-major). With `alpha_cutoff`, texels whose alpha is below
/// it become transparent (three-color mode); without, alpha is ignored.
pub fn encode_block(texels: &[[u8; 4]; 16], alpha_cutoff: Option<u8>) -> [u8; 8] {
    let transparent = texels.map(|t| alpha_cutoff.is_some_and(|c| t[3] < c));
    if transparent.iter().all(|&t| t) {
        // Three-color mode with both endpoints 0, every index 3.
        return [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    }
    let three = transparent.iter().any(|&t| t);
    // The colors of the texels that are not transparent (alpha plays no part in the fit).
    let mut opaque = [[0.0f32; 4]; 16];
    let mut count = 0;
    let kept = texels.iter().zip(&transparent).filter(|(_, t)| !**t);
    for ((t, _), slot) in kept.zip(opaque.iter_mut()) {
        let c = from_u8(*t);
        *slot = [c[0], c[1], c[2], 0.0];
        count += 1;
    }
    let opaque = opaque.get(..count).unwrap_or(&[]);
    let (mut a, mut b) = principal_endpoints(opaque);
    let mut best: Option<Candidate> = None;
    for _ in 0..PASSES {
        let cand = finalize(quantize(a), quantize(b), texels, &transparent, three);
        let weights: Vec<f32> = cand
            .indices
            .iter()
            .zip(&transparent)
            .filter(|(_, t)| !**t)
            .map(|(&i, _)| weight(i, cand.c0 > cand.c1))
            .collect();
        let next = least_squares(opaque, &weights);
        if best.as_ref().is_none_or(|b| cand.error < b.error) {
            best = Some(cand);
        }
        match next {
            Some((na, nb)) => (a, b) = (na, nb),
            None => break,
        }
    }
    best.map_or([0; 8], |c| c.bytes())
}

/// The interpolation weight of `color1` for index `i`.
fn weight(i: u8, four: bool) -> f32 {
    match (i, four) {
        (1, _) => 1.0,
        (2, true) => 1.0 / 3.0,
        (3, true) => 2.0 / 3.0,
        (2, false) => 0.5,
        _ => 0.0,
    }
}

fn quantize(v: V4) -> u16 {
    let r = u16::from(round_clamp(v[0] * 31.0 / 255.0, 31));
    let g = u16::from(round_clamp(v[1] * 63.0 / 255.0, 63));
    let b = u16::from(round_clamp(v[2] * 31.0 / 255.0, 31));
    (r << 11) | (g << 5) | b
}

struct Candidate {
    c0: u16,
    c1: u16,
    indices: [u8; 16],
    error: u32,
}

impl Candidate {
    fn bytes(&self) -> [u8; 8] {
        let mut bits = 0u32;
        for (shift, &i) in (0u32..).step_by(2).zip(&self.indices) {
            bits |= u32::from(i & 3) << shift;
        }
        let [a0, a1] = self.c0.to_le_bytes();
        let [b0, b1] = self.c1.to_le_bytes();
        let [i0, i1, i2, i3] = bits.to_le_bytes();
        [a0, a1, b0, b1, i0, i1, i2, i3]
    }
}

/// Orders the endpoints for the mode, picks each texel's best index with the decoder's
/// palette, and measures the RGB error of the texels that are not transparent.
fn finalize(e0: u16, e1: u16, texels: &[[u8; 4]; 16], transparent: &[bool; 16], three: bool) -> Candidate {
    let (c0, c1) = if three {
        (e0.min(e1), e0.max(e1))
    } else {
        (e0.max(e1), e0.min(e1))
    };
    let pal = palette(c0, c1);
    let usable = if c0 > c1 { 4 } else { 3 };
    let mut indices = [0u8; 16];
    let mut error = 0u32;
    for ((t, &skip), index) in texels.iter().zip(transparent).zip(indices.iter_mut()) {
        if skip {
            *index = 3;
            continue;
        }
        let mut best = (u32::MAX, 0u8);
        for (i, p) in (0u8..).zip(pal.iter().take(usable)) {
            let e = texel_error(*t, *p, 3);
            if e < best.0 {
                best = (e, i);
            }
        }
        *index = best.1;
        error += best.0;
    }
    Candidate {
        c0,
        c1,
        indices,
        error,
    }
}
