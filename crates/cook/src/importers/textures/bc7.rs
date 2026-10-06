//! BC7: 16 bytes per 4x4 block of RGBA.
//!
//! A block is a 128-bit little-endian value read from bit 0 up. It starts with the mode in
//! unary: mode `m` is `m` zero bits and then a one bit (a block whose low 8 bits are all
//! zero is reserved and decodes to transparent black). This encoder writes two modes:
//!
//! **Mode 6** (one subset, RGBA): mode bits (7), then `R0 R1 G0 G1 B0 B1 A0 A1` (7 bits
//! each), `P0 P1` (one bit per endpoint), then 16 four-bit indices, texel 0's being 3 bits
//! (its high bit is implicitly 0). An endpoint channel is `value7 << 1 | P`.
//!
//! **Mode 1** (two subsets, RGB, alpha 255): mode bits (2), a 6-bit partition number,
//! then `R` of subset 0's two endpoints and subset 1's two endpoints (6 bits each), the
//! same for `G` and `B`, two P bits (one shared by each subset's endpoints), then 16
//! three-bit indices, where texel 0 and the partition's anchor texel for subset 1 have 2
//! bits (high bit implicitly 0). An endpoint channel is the 7-bit `value6 << 1 | P`
//! expanded to 8 bits as `v7 << 1 | v7 >> 6`.
//!
//! A channel interpolates as `((64 - w)·e0 + w·e1 + 32) >> 6` with weights
//! `0 9 18 27 37 46 55 64` (3-bit indices) or `0 4 9 13 17 21 26 30 34 38 43 47 51 55 60 64`
//! (4-bit). Since the weights are symmetric (`w[n-1-i] = 64 - w[i]`), swapping a subset's
//! endpoints and inverting its indices decodes to the same texels; the encoder does that
//! whenever an anchor index would have its high bit set.
//!
//! # Encoder
//!
//! Every block is fitted in mode 6: principal axis of the RGBA texels (power iteration),
//! least-squares refinement on the chosen indices (fixed pass count), and every P-bit
//! pair, keeping the lowest squared RGBA error. A fully opaque block is also fitted in
//! mode 1: each of the 64 partitions is scored with a single principal-axis fit per
//! subset, the best partition is refined like mode 6, and mode 1 replaces mode 6 only
//! when its error is strictly lower.
//!
//! The decoder here ([`decode_block`]) implements modes 1 and 6 and the reserved mode;
//! other modes (never produced by this encoder) return `None`.

use super::fit::{V4, byte, from_u8, least_squares, principal_endpoints, round_clamp, texel_error};

/// Version of this encoder, recorded in every BC7 texture it produces. Bump it whenever
/// the same block would encode to different bytes.
pub const ENCODER_VERSION: u32 = 1;

/// Least-squares refinement passes.
const PASSES: usize = 3;

/// Interpolation weights of 3-bit indices.
pub const WEIGHTS3: [u8; 8] = [0, 9, 18, 27, 37, 46, 55, 64];

/// Interpolation weights of 4-bit indices.
pub const WEIGHTS4: [u8; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// The two-subset partitions: bit `i` set means texel `i` (row-major) is in subset 1.
#[rustfmt::skip]
pub const PARTITIONS2: [u16; 64] = [
    0xCCCC, 0x8888, 0xEEEE, 0xECC8, 0xC880, 0xFEEC, 0xFEC8, 0xEC80,
    0xC800, 0xFFEC, 0xFE80, 0xE800, 0xFFE8, 0xFF00, 0xFFF0, 0xF000,
    0xF710, 0x008E, 0x7100, 0x08CE, 0x008C, 0x7310, 0x3100, 0x8CCE,
    0x088C, 0x3110, 0x6666, 0x366C, 0x17E8, 0x0FF0, 0x718E, 0x399C,
    0xAAAA, 0xF0F0, 0x5A5A, 0x33CC, 0x3C3C, 0x55AA, 0x9696, 0xA55A,
    0x73CE, 0x13C8, 0x324C, 0x3BDC, 0x6996, 0xC33C, 0x9966, 0x0660,
    0x0272, 0x04E4, 0x4E40, 0x2720, 0xC936, 0x936C, 0x39C6, 0x639C,
    0x9336, 0x9CC6, 0x817E, 0xE718, 0xCCF0, 0x0FCC, 0x7744, 0xEE22,
];

/// The anchor texel of subset 1 for each two-subset partition (subset 0's is texel 0).
#[rustfmt::skip]
pub const ANCHORS2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 15,
    15,  2,  8,  2,  2,  8,  8, 15,
     2,  8,  2,  2,  8,  8,  2,  2,
    15, 15,  6,  8,  2,  8, 15, 15,
     2,  8,  2,  2,  2, 15, 15,  6,
     6,  2,  6,  8, 15, 15,  2,  2,
    15, 15, 15, 15, 15,  2,  2, 15,
];

fn interpolate(e0: u8, e1: u8, w: u8) -> u8 {
    let w = u32::from(w);
    byte(((64 - w) * u32::from(e0) + w * u32::from(e1) + 32) >> 6)
}

fn weight3(i: u8) -> u8 {
    WEIGHTS3.get(usize::from(i)).copied().unwrap_or(0)
}

fn weight4(i: u8) -> u8 {
    WEIGHTS4.get(usize::from(i)).copied().unwrap_or(0)
}

fn mix(e0: [u8; 4], e1: [u8; 4], w: u8) -> [u8; 4] {
    [
        interpolate(e0[0], e1[0], w),
        interpolate(e0[1], e1[1], w),
        interpolate(e0[2], e1[2], w),
        interpolate(e0[3], e1[3], w),
    ]
}

/// Whether texel `i` is in subset 1 of `partition`.
fn in_subset1(partition: u8, i: usize) -> bool {
    PARTITIONS2
        .get(usize::from(partition))
        .is_some_and(|m| (m >> i) & 1 == 1)
}

fn anchor2(partition: u8) -> usize {
    ANCHORS2
        .get(usize::from(partition))
        .map_or(15, |&a| usize::from(a))
}

/// Little-endian bit writer over a 128-bit block.
struct BitWriter {
    value: u128,
    at: u32,
}

impl BitWriter {
    fn put(&mut self, v: u8, bits: u32) {
        let mask = (1u128 << bits) - 1;
        self.value |= (u128::from(v) & mask) << self.at;
        self.at += bits;
    }
}

/// Little-endian bit reader over a 128-bit block.
struct BitReader {
    value: u128,
    at: u32,
}

impl BitReader {
    fn take(&mut self, bits: u32) -> u8 {
        let mask = (1u128 << bits) - 1;
        let v = (self.value >> self.at) & mask;
        self.at += bits;
        u8::try_from(v).unwrap_or(0)
    }
}

/// The mode of a block (0 to 7), or `None` for the reserved mode.
pub fn mode(block: &[u8; 16]) -> Option<u32> {
    let first = block[0];
    (first != 0).then(|| first.trailing_zeros())
}

/// Decodes one block to 16 RGBA texels, row-major. The reserved mode decodes to
/// transparent black; `None` for modes 0, 2, 3, 4, 5, and 7, which this encoder never
/// writes and this decoder does not implement.
pub fn decode_block(block: &[u8; 16]) -> Option<[[u8; 4]; 16]> {
    let mut r = BitReader {
        value: u128::from_le_bytes(*block),
        at: 0,
    };
    match mode(block) {
        None => Some([[0; 4]; 16]),
        Some(6) => {
            r.take(7);
            let mut e = [[0u8; 4]; 2];
            for ch in 0..4 {
                for end in &mut e {
                    if let Some(c) = end.get_mut(ch) {
                        *c = r.take(7) << 1;
                    }
                }
            }
            for end in &mut e {
                let p = r.take(1);
                *end = end.map(|c| c | p);
            }
            let mut out = [[0u8; 4]; 16];
            for (i, texel) in out.iter_mut().enumerate() {
                let index = r.take(if i == 0 { 3 } else { 4 });
                *texel = mix(e[0], e[1], weight4(index));
            }
            Some(out)
        }
        Some(1) => {
            r.take(2);
            let partition = r.take(6);
            // e[subset][end] channel values, 6 bits for now.
            let mut e = [[[0u8, 0, 0, 255]; 2]; 2];
            for ch in 0..3 {
                for subset in &mut e {
                    for end in subset.iter_mut() {
                        if let Some(c) = end.get_mut(ch) {
                            *c = r.take(6);
                        }
                    }
                }
            }
            for subset in &mut e {
                let p = r.take(1);
                for end in subset.iter_mut() {
                    for c in end.iter_mut().take(3) {
                        let v7 = (*c << 1) | p;
                        *c = (v7 << 1) | (v7 >> 6);
                    }
                }
            }
            let anchor = anchor2(partition);
            let mut out = [[0u8; 4]; 16];
            for (i, texel) in out.iter_mut().enumerate() {
                let index = r.take(if i == 0 || i == anchor { 2 } else { 3 });
                let [e0, e1] = if in_subset1(partition, i) { e[1] } else { e[0] };
                *texel = mix(e0, e1, weight3(index));
            }
            Some(out)
        }
        Some(_) => None,
    }
}

/// Encodes 16 RGBA texels (row-major).
pub fn encode_block(texels: &[[u8; 4]; 16]) -> [u8; 16] {
    let six = mode6(texels);
    if texels.iter().all(|t| t[3] == 255) {
        let one = mode1(texels);
        if one.error < six.error {
            return one.bytes();
        }
    }
    six.bytes()
}

// ---------------------------------------------------------------- mode 6

struct Mode6 {
    /// 7-bit endpoint channels.
    e: [[u8; 4]; 2],
    p: [u8; 2],
    indices: [u8; 16],
    error: u32,
}

impl Mode6 {
    fn bytes(&self) -> [u8; 16] {
        let mut e = self.e;
        let mut p = self.p;
        let mut indices = self.indices;
        if indices[0] >= 8 {
            e.swap(0, 1);
            p.swap(0, 1);
            indices = indices.map(|i| 15 - i);
        }
        let mut w = BitWriter { value: 0, at: 0 };
        w.put(1 << 6, 7);
        for ch in 0..4 {
            for end in &e {
                w.put(end.get(ch).copied().unwrap_or(0), 7);
            }
        }
        w.put(p[0], 1);
        w.put(p[1], 1);
        for (i, &index) in indices.iter().enumerate() {
            w.put(index, if i == 0 { 3 } else { 4 });
        }
        w.value.to_le_bytes()
    }
}

fn mode6(texels: &[[u8; 4]; 16]) -> Mode6 {
    let points = texels.map(from_u8);
    let (mut a, mut b) = principal_endpoints(&points);
    let mut best: Option<Mode6> = None;
    for _ in 0..PASSES {
        let cand = quantize6(a, b, texels);
        let weights = cand.indices.map(|i| f32::from(weight4(i)) / 64.0);
        let next = least_squares(&points, &weights);
        if best.as_ref().is_none_or(|b| cand.error < b.error) {
            best = Some(cand);
        }
        match next {
            Some((na, nb)) => (a, b) = (na, nb),
            None => break,
        }
    }
    best.unwrap_or(Mode6 {
        e: [[0; 4]; 2],
        p: [0; 2],
        indices: [0; 16],
        error: u32::MAX,
    })
}

/// The best P-bit pair for float endpoints `a`, `b`.
fn quantize6(a: V4, b: V4, texels: &[[u8; 4]; 16]) -> Mode6 {
    let mut best: Option<Mode6> = None;
    for (pa, pb) in [(0u8, 0u8), (0, 1), (1, 0), (1, 1)] {
        let q = |v: V4, p: u8| v.map(|c| round_clamp((c - f32::from(p)) / 2.0, 127));
        let e = [q(a, pa), q(b, pb)];
        let full = [e[0].map(|c| (c << 1) | pa), e[1].map(|c| (c << 1) | pb)];
        let pal: [[u8; 4]; 16] = WEIGHTS4.map(|w| mix(full[0], full[1], w));
        let (indices, error) = nearest(texels.iter().copied(), &pal);
        if best.as_ref().is_none_or(|b| error < b.error) {
            best = Some(Mode6 {
                e,
                p: [pa, pb],
                indices,
                error,
            });
        }
    }
    best.unwrap_or(Mode6 {
        e: [[0; 4]; 2],
        p: [0; 2],
        indices: [0; 16],
        error: u32::MAX,
    })
}

/// Each texel's nearest palette index (lowest on a tie) and the total squared RGBA error.
/// Indices fill from the front, one per texel.
fn nearest(texels: impl Iterator<Item = [u8; 4]>, pal: &[[u8; 4]]) -> ([u8; 16], u32) {
    let mut indices = [0u8; 16];
    let mut error = 0u32;
    for (t, slot) in texels.zip(indices.iter_mut()) {
        let mut best = (u32::MAX, 0u8);
        for (i, p) in (0u8..).zip(pal) {
            let e = texel_error(t, *p, 4);
            if e < best.0 {
                best = (e, i);
            }
        }
        *slot = best.1;
        error += best.0;
    }
    (indices, error)
}

// ---------------------------------------------------------------- mode 1

/// One subset's fit in mode 1.
#[derive(Clone, Copy)]
struct Subset {
    /// 6-bit endpoint channels (RGB).
    e: [[u8; 3]; 2],
    p: u8,
    /// Indices of the subset's texels, in texel order.
    indices: [u8; 16],
    error: u32,
}

struct Mode1 {
    partition: u8,
    subsets: [Subset; 2],
    error: u32,
}

impl Mode1 {
    fn bytes(&self) -> [u8; 16] {
        let anchor = anchor2(self.partition);
        // Per-texel indices from the per-subset lists, and each subset's anchor fix.
        let mut subsets = self.subsets;
        let mut indices = [0u8; 16];
        let mut next = [0usize; 2];
        for (i, slot) in indices.iter_mut().enumerate() {
            let s = usize::from(in_subset1(self.partition, i));
            if let (Some(n), Some(sub)) = (next.get_mut(s), subsets.get(s)) {
                *slot = sub.indices.get(*n).copied().unwrap_or(0);
                *n += 1;
            }
        }
        for (s, &anchor_texel) in [0usize, anchor].iter().enumerate() {
            if indices.get(anchor_texel).is_some_and(|&i| i >= 4) {
                if let Some(sub) = subsets.get_mut(s) {
                    sub.e.swap(0, 1);
                }
                for (i, index) in indices.iter_mut().enumerate() {
                    if usize::from(in_subset1(self.partition, i)) == s {
                        *index = 7 - *index;
                    }
                }
            }
        }
        let mut w = BitWriter { value: 0, at: 0 };
        w.put(0b10, 2);
        w.put(self.partition, 6);
        for ch in 0..3 {
            for sub in &subsets {
                for end in &sub.e {
                    w.put(end.get(ch).copied().unwrap_or(0), 6);
                }
            }
        }
        for sub in &subsets {
            w.put(sub.p, 1);
        }
        for (i, &index) in indices.iter().enumerate() {
            w.put(index, if i == 0 || i == anchor { 2 } else { 3 });
        }
        w.value.to_le_bytes()
    }
}

fn mode1(texels: &[[u8; 4]; 16]) -> Mode1 {
    let mut best: Option<(u8, u32)> = None;
    for partition in 0u8..64 {
        let error = fit_partition(partition, texels, 1).error;
        if best.is_none_or(|(_, e)| error < e) {
            best = Some((partition, error));
        }
    }
    fit_partition(best.map_or(0, |(p, _)| p), texels, PASSES)
}

fn fit_partition(partition: u8, texels: &[[u8; 4]; 16], passes: usize) -> Mode1 {
    let mut subsets = [empty_subset(); 2];
    for (s, slot) in subsets.iter_mut().enumerate() {
        let mut members = [[0u8; 4]; 16];
        let mut count = 0;
        let chosen = texels
            .iter()
            .enumerate()
            .filter(|(i, _)| usize::from(in_subset1(partition, *i)) == s);
        for ((_, t), m) in chosen.zip(members.iter_mut()) {
            *m = *t;
            count += 1;
        }
        *slot = fit_subset(members.get(..count).unwrap_or(&[]), passes);
    }
    Mode1 {
        partition,
        error: subsets.iter().map(|s| s.error).sum(),
        subsets,
    }
}

fn empty_subset() -> Subset {
    Subset {
        e: [[0; 3]; 2],
        p: 0,
        indices: [0; 16],
        error: 0,
    }
}

fn fit_subset(members: &[[u8; 4]], passes: usize) -> Subset {
    let mut all = [[0.0f32; 4]; 16];
    for (slot, t) in all.iter_mut().zip(members) {
        let c = from_u8(*t);
        *slot = [c[0], c[1], c[2], 0.0];
    }
    let points = all.get(..members.len()).unwrap_or(&[]);
    let (mut a, mut b) = principal_endpoints(points);
    let mut best: Option<Subset> = None;
    for _ in 0..passes {
        let cand = quantize1(a, b, members);
        let all_weights = cand.indices.map(|i| f32::from(weight3(i)) / 64.0);
        let weights = all_weights.get(..members.len()).unwrap_or(&[]);
        let next = least_squares(points, weights);
        if best.as_ref().is_none_or(|b| cand.error < b.error) {
            best = Some(cand);
        }
        match next {
            Some((na, nb)) => (a, b) = (na, nb),
            None => break,
        }
    }
    best.unwrap_or_else(empty_subset)
}

/// The 8-bit value of a 6-bit mode 1 endpoint channel with P bit `p`.
fn expand6(c: u8, p: u8) -> u8 {
    let v7 = (c << 1) | p;
    (v7 << 1) | (v7 >> 6)
}

/// The 6-bit value whose expansion with `p` is nearest `v`.
fn nearest6(v: f32, p: u8) -> u8 {
    let guess = round_clamp((v * 127.0 / 255.0 - f32::from(p)) / 2.0, 63);
    let mut best = (f32::INFINITY, guess);
    for c in [guess.saturating_sub(1), guess, guess.saturating_add(1).min(63)] {
        let d = (f32::from(expand6(c, p)) - v).abs();
        if d < best.0 {
            best = (d, c);
        }
    }
    best.1
}

fn quantize1(a: V4, b: V4, members: &[[u8; 4]]) -> Subset {
    let mut best: Option<Subset> = None;
    for p in [0u8, 1] {
        let q = |v: V4| [nearest6(v[0], p), nearest6(v[1], p), nearest6(v[2], p)];
        let e = [q(a), q(b)];
        let full = |c: [u8; 3]| [expand6(c[0], p), expand6(c[1], p), expand6(c[2], p), 255];
        let pal: [[u8; 4]; 8] = WEIGHTS3.map(|w| mix(full(e[0]), full(e[1]), w));
        let (indices, error) = nearest(members.iter().copied(), &pal);
        if best.as_ref().is_none_or(|b| error < b.error) {
            best = Some(Subset { e, p, indices, error });
        }
    }
    best.unwrap_or_else(empty_subset)
}
