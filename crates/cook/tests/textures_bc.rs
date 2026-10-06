//! The block encoders: byte-exact reference blocks derived from the block format specs, decoding
//! of hand-built spec blocks, round-trip error bounds, mode and anchor rules on random
//! blocks, and determinism.

#![allow(clippy::indexing_slicing, clippy::cast_possible_truncation)]

use mantis_cook::importers::textures::encode::{decode_image, encode_image};
use mantis_cook::importers::textures::{bc1, bc4, bc5, bc7};
use mantis_formats::texture::Encoding;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A small deterministic generator (xorshift32) for test data.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

fn block_of(f: impl FnMut(usize) -> [u8; 4]) -> [[u8; 4]; 16] {
    core::array::from_fn(f)
}

// ------------------------------------------------------------------ BC1

#[test]
fn bc1_reference_blocks() {
    // Solid red (255, 0, 0): 5:6:5 red is r5 = 31, g6 = 0, b5 = 0 -> 0xF800. Both endpoints
    // quantize to it; with color0 == color1 the block decodes in three-color mode where
    // indices 0 to 2 are all that color, so every index is 0.
    // Bytes: color0 = 00 F8, color1 = 00 F8, indices 00 00 00 00.
    let red = block_of(|_| [255, 0, 0, 255]);
    assert_eq!(
        bc1::encode_block(&red, None),
        [0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0]
    );

    // Columns of red 0, 85, 170, 255. Endpoints red 255 (0xF800) and black (0x0000);
    // 0xF800 > 0 is four-color mode with palette 255, 0, (2·255 + 0 + 1)/3 = 170,
    // (255 + 2·0 + 1)/3 = 85. Each row: column 0 -> index 1, column 1 -> 3, column 2 -> 2,
    // column 3 -> 0, packed as 1 | 3 << 2 | 2 << 4 | 0 << 6 = 0x2D.
    let ramp = block_of(|i| [[0, 85, 170, 255][i % 4], 0, 0, 255]);
    assert_eq!(
        bc1::encode_block(&ramp, None),
        [0x00, 0xF8, 0x00, 0x00, 0x2D, 0x2D, 0x2D, 0x2D]
    );

    // Black in columns 0 and 1, white in 2 and 3: endpoints white 0xFFFF and black 0x0000,
    // four-color; black -> index 1, white -> index 0: each row 1 | 1 << 2 = 0x05.
    let split = block_of(|i| if i % 4 < 2 { [0, 0, 0, 255] } else { [255; 4] });
    assert_eq!(
        bc1::encode_block(&split, None),
        [0xFF, 0xFF, 0x00, 0x00, 0x05, 0x05, 0x05, 0x05]
    );

    // Punch-through with cutoff 128: columns 0 and 1 transparent, 2 and 3 opaque red.
    // Three-color mode needs color0 <= color1: both 0xF800; transparent texels take
    // index 3, red index 0: each row 3 | 3 << 2 = 0x0F.
    let cut = block_of(|i| if i % 4 < 2 { [0, 0, 0, 0] } else { [255, 0, 0, 255] });
    assert_eq!(
        bc1::encode_block(&cut, Some(128)),
        [0x00, 0xF8, 0x00, 0xF8, 0x0F, 0x0F, 0x0F, 0x0F]
    );
    // Without a cutoff, alpha is ignored: black and red in four-color mode.
    assert_eq!(
        bc1::encode_block(&cut, None),
        [0x00, 0xF8, 0x00, 0x00, 0x05, 0x05, 0x05, 0x05]
    );

    // Fully transparent: endpoints 0, every index 3.
    let clear = block_of(|_| [10, 20, 30, 0]);
    assert_eq!(
        bc1::encode_block(&clear, Some(1)),
        [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]
    );
}

#[test]
fn bc1_decodes_spec_examples() {
    // Four-color: color0 = 0xF800 (red) > color1 = 0x001F (blue). Palette: red, blue,
    // (2·red + blue)/3 = (170, 0, 85), (red + 2·blue)/3 = (85, 0, 170). Index byte 0xE4 =
    // 0b11_10_01_00 puts indices 0, 1, 2, 3 in a row.
    let four = bc1::decode_block(&[0x00, 0xF8, 0x1F, 0x00, 0xE4, 0xE4, 0xE4, 0xE4]);
    for row in four.chunks(4) {
        assert_eq!(
            row,
            [
                [255, 0, 0, 255],
                [0, 0, 255, 255],
                [170, 0, 85, 255],
                [85, 0, 170, 255]
            ]
        );
    }
    // Three-color: color0 = 0x001F <= color1 = 0xF800. Palette: blue, red, the midpoint
    // ((0 + 255 + 1)/2, 0, (255 + 0 + 1)/2) = (128, 0, 128), and transparent black.
    let three = bc1::decode_block(&[0x1F, 0x00, 0x00, 0xF8, 0xE4, 0xE4, 0xE4, 0xE4]);
    assert_eq!(
        &three[..4],
        [
            [0, 0, 255, 255],
            [255, 0, 0, 255],
            [128, 0, 128, 255],
            [0, 0, 0, 0]
        ]
    );
    // 5:6:5 expansion replicates the high bits: 0x8410 is r5 = 16, g6 = 32, b5 = 16 ->
    // r8 = 16 << 3 | 16 >> 2 = 132, g8 = 32 << 2 | 32 >> 4 = 130, b8 = 132.
    assert_eq!(bc1::expand_565(0x8410), [132, 130, 132, 255]);
}

#[test]
fn bc1_mode_and_order_rules_hold_for_random_blocks() {
    let mut rng = Rng(0x1234_5678);
    for n in 0..400 {
        let cutoff = (n % 2 == 1).then_some(128u8);
        let block = block_of(|_| {
            let a = if rng.below(4) == 0 { rng.byte() / 2 } else { 255 };
            [rng.byte(), rng.byte(), rng.byte(), a]
        });
        let bytes = bc1::encode_block(&block, cutoff);
        let c0 = u16::from_le_bytes([bytes[0], bytes[1]]);
        let c1 = u16::from_le_bytes([bytes[2], bytes[3]]);
        let bits = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let indices: Vec<u32> = (0..16).map(|i| (bits >> (2 * i)) & 3).collect();
        let transparent: Vec<bool> = block.iter().map(|t| cutoff.is_some_and(|c| t[3] < c)).collect();
        if transparent.iter().any(|&t| t) {
            // Three-color mode: color0 <= color1, index 3 exactly on transparent texels.
            assert!(c0 <= c1, "block {n}: {c0:#06x} > {c1:#06x}");
            for (&i, &t) in indices.iter().zip(&transparent) {
                assert_eq!(i == 3, t, "block {n}");
            }
        } else {
            // Opaque: four-color mode, or equal endpoints never using index 3.
            assert!(
                c0 > c1 || (c0 == c1 && !indices.contains(&3)),
                "block {n}: {c0:#06x} {c1:#06x} {indices:?}"
            );
        }
        let decoded = bc1::decode_block(&bytes);
        for (d, &tr) in decoded.iter().zip(&transparent) {
            assert_eq!(d[3] == 0, tr, "block {n}: alpha decodes as the cutoff said");
        }
    }
}

// ------------------------------------------------------------------ BC4 / BC5

#[test]
fn bc4_reference_blocks() {
    // Solid 128: both endpoints 128 (six-value mode, where index 0 is 128), indices 0.
    assert_eq!(bc4::encode_block(&[128; 16]), [0x80, 0x80, 0, 0, 0, 0, 0, 0]);

    // Columns 0, 0, 255, 255: eight-value mode red0 = 255 > red1 = 0 is exact (the
    // six-value candidate is exact too; ties keep eight-value). 255 -> index 0, 0 ->
    // index 1. A row is 1 | 1 << 3 = 0x009 in 12 bits; four rows make the 48-bit value
    // 0x009_009_009_009, little-endian bytes 09 90 00 09 90 00.
    let split: [u8; 16] = core::array::from_fn(|i| if i % 4 < 2 { 0 } else { 255 });
    assert_eq!(
        bc4::encode_block(&split),
        [0xFF, 0x00, 0x09, 0x90, 0x00, 0x09, 0x90, 0x00]
    );

    // Columns 0, 100, 200, 255. Eight-value 255..0 misses 100 and 200 (nearest 109 and
    // 182/219). Six-value with red0 = 100 <= red1 = 200 is exact: 100 -> 0, 200 -> 1,
    // 0 -> 6, 255 -> 7. A row is 6 | 0 << 3 | 1 << 6 | 7 << 9 = 0xE46; the 48-bit value
    // 0xE46_E46_E46_E46 is bytes 46 6E E4 46 6E E4.
    let wide: [u8; 16] = core::array::from_fn(|i| [0, 100, 200, 255][i % 4]);
    assert_eq!(
        bc4::encode_block(&wide),
        [0x64, 0xC8, 0x46, 0x6E, 0xE4, 0x46, 0x6E, 0xE4]
    );
    assert_eq!(bc4::decode_block(&bc4::encode_block(&wide)), wide);
}

#[test]
fn bc4_decodes_both_modes() {
    // Eight-value 255, 0: interpolants ((8 - i)·255 + 3) / 7 for i = 2..7.
    assert_eq!(bc4::palette(255, 0), [255, 0, 219, 182, 146, 109, 73, 36]);
    // Six-value 100, 200: ((6 - i)·100 + (i - 1)·200 + 2) / 5 for i = 2..5, then 0, 255.
    assert_eq!(bc4::palette(100, 200), [100, 200, 120, 140, 160, 180, 0, 255]);
    // Indices 0..7 then 7..0: the 48-bit value packs 3 bits per texel from bit 0.
    let mut bits = 0u64;
    for (i, index) in [0u64, 1, 2, 3, 4, 5, 6, 7, 7, 6, 5, 4, 3, 2, 1, 0]
        .iter()
        .enumerate()
    {
        bits |= index << (3 * i);
    }
    let b = bits.to_le_bytes();
    let decoded = bc4::decode_block(&[255, 0, b[0], b[1], b[2], b[3], b[4], b[5]]);
    assert_eq!(
        decoded,
        [
            255, 0, 219, 182, 146, 109, 73, 36, 36, 73, 109, 146, 182, 219, 0, 255
        ]
    );
}

#[test]
fn bc4_mode_rules_hold_for_random_blocks() {
    let mut rng = Rng(0x0BAD_F00D);
    for n in 0..400 {
        let extremes = n % 3 == 0;
        let values: [u8; 16] = core::array::from_fn(|_| {
            let v = rng.byte();
            if extremes { v } else { v.clamp(1, 254) }
        });
        let bytes = bc4::encode_block(&values);
        let decoded = bc4::decode_block(&bytes);
        let touches = values.iter().any(|&v| v == 0 || v == 255);
        if !touches && bytes[0] != bytes[1] {
            // Only a block with 0 or 255 may choose six-value mode.
            assert!(bytes[0] > bytes[1], "block {n}");
        }
        // Error bound: half of the largest gap of the eight-value palette of the range.
        let lo = *values.iter().min().unwrap_or(&0);
        let hi = *values.iter().max().unwrap_or(&0);
        let bound = u32::from(hi - lo) / 14 + 2;
        for (d, v) in decoded.iter().zip(&values) {
            assert!(u32::from(d.abs_diff(*v)) <= bound, "block {n}: {d} vs {v}");
        }
    }
}

#[test]
fn bc5_is_two_bc4_blocks() {
    let red: [u8; 16] = core::array::from_fn(|i| (i * 16) as u8);
    let green: [u8; 16] = core::array::from_fn(|i| 255 - (i * 9) as u8);
    let bytes = bc5::encode_block(&red, &green);
    assert_eq!(bytes[..8], bc4::encode_block(&red));
    assert_eq!(bytes[8..], bc4::encode_block(&green));
    let (r, g) = bc5::decode_block(&bytes);
    assert_eq!(r, bc4::decode_block(&bc4::encode_block(&red)));
    assert_eq!(g, bc4::decode_block(&bc4::encode_block(&green)));
    assert_eq!(bc5::ENCODER_VERSION, bc4::ENCODER_VERSION);
}

// ------------------------------------------------------------------ BC7

#[test]
fn bc7_reference_blocks() {
    // Solid (200, 100, 50, 254), not opaque, so mode 6 only. Every channel is even, so P
    // bits 0 and 7-bit values 100, 50, 25, 127 for both endpoints are exact; indices 0.
    // Bits from 0: mode 6 = 0b1000000 (7), R0 R1 = 100 100, G0 G1 = 50 50, B0 B1 = 25 25,
    // A0 A1 = 127 127 (7 each), P0 P1 = 0 0, then 3 + 15·4 index bits, all 0.
    let solid = block_of(|_| [200, 100, 50, 254]);
    assert_eq!(
        bc7::encode_block(&solid),
        [
            0x40, 0x32, 0x59, 0x26, 0xCB, 0x64, 0xFE, 0x7F, 0, 0, 0, 0, 0, 0, 0, 0
        ]
    );

    // Columns 0 and 1 transparent black, 2 and 3 (254, 128, 64, 254). The principal axis
    // puts the colored end first, which would give texel 0 (black) index 15, so the
    // encoder swaps: endpoint 0 = 0 (7-bit 0), endpoint 1 = 7-bit (127, 64, 32, 127), P 0 0;
    // black -> index 0, colored -> index 15 (texel 0's 3 stored bits are 0).
    let split = block_of(|i| if i % 4 < 2 { [0; 4] } else { [254, 128, 64, 254] });
    let bytes = bc7::encode_block(&split);
    assert_eq!(
        bytes,
        [
            0x40, 0xC0, 0x1F, 0x00, 0x04, 0x80, 0x00, 0x7F, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF
        ]
    );
    assert_eq!(bc7::decode_block(&bytes), Some(split));
}

#[test]
fn bc7_decodes_hand_built_blocks() -> TestResult {
    // Mode 1, partition 13 (0xFF00: rows 0-1 subset 0, rows 2-3 subset 1; subset 1's
    // anchor is texel 15). Subset 0: both endpoints RGB6 (63, 0, 0), P 0 -> 7-bit 126 ->
    // 8-bit 126 << 1 | 126 >> 6 = 253; green and blue 0. Subset 1: endpoint 0 RGB6 (0, 0, 0)
    // and endpoint 1 (0, 0, 63) with P 1 -> 7-bit 1 and 127 -> 8-bit 2 and 255. Indices:
    // texels 0-7 all 0; texels 8-15: 0 1 2 3 4 5 6 3 (texels 0 and 15 have 2 bits).
    // Blue = ((64 - w)·2 + w·255 + 32) >> 6 with w = 0 9 18 27 37 46 55 27.
    let block = [
        0x36, 0xFF, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFC, 0x02, 0x00, 0x00, 0x10, 0x8D, 0xF5,
    ];
    assert_eq!(bc7::mode(&block), Some(1));
    let decoded = bc7::decode_block(&block).ok_or("mode 1 decodes")?;
    for t in &decoded[..8] {
        assert_eq!(*t, [253, 0, 0, 255]);
    }
    let blue: Vec<u8> = decoded[8..].iter().map(|t| t[2]).collect();
    assert_eq!(blue, [2, 38, 73, 109, 148, 184, 219, 109]);
    assert!(decoded[8..].iter().all(|t| t[0] == 2 && t[1] == 2 && t[3] == 255));

    // The reserved mode (low byte 0) decodes to transparent black.
    assert_eq!(bc7::decode_block(&[0; 16]), Some([[0; 4]; 16]));
    Ok(())
}

#[test]
fn bc7_partition_tables_are_consistent() {
    for (p, (&mask, &anchor)) in bc7::PARTITIONS2.iter().zip(&bc7::ANCHORS2).enumerate() {
        assert_eq!(mask & 1, 0, "partition {p}: texel 0 anchors subset 0");
        assert_eq!(
            (mask >> anchor) & 1,
            1,
            "partition {p}: the anchor is in subset 1"
        );
        assert!(mask.count_ones() >= 1 && mask.count_ones() <= 15, "partition {p}");
    }
    let w3 = bc7::WEIGHTS3;
    let w4 = bc7::WEIGHTS4;
    assert!(
        (0..8).all(|i| w3[i] + w3[7 - i] == 64),
        "3-bit weights are symmetric"
    );
    assert!(
        (0..16).all(|i| w4[i] + w4[15 - i] == 64),
        "4-bit weights are symmetric"
    );
}

/// The 8-bit value of a mode 1 channel (6-bit `c`, P bit `p`).
fn expand6(c: u8, p: u8) -> u8 {
    let v7 = (c << 1) | p;
    (v7 << 1) | (v7 >> 6)
}

#[test]
fn bc7_anchor_rules_hold_for_random_blocks() -> TestResult {
    let mut rng = Rng(0x00C0_FFEE);
    // Mode 6: two exactly representable colors (even channels, so P = 0), placed at
    // random. Texel 0 is either color, so both orientations need the anchor rule; any
    // mistake decodes texel 0 (or every texel) wrong.
    for n in 0..200 {
        let a = [0u8; 4].map(|_| rng.byte() & 0xFE);
        let mut b = [0u8; 4].map(|_| rng.byte() & 0xFE);
        b[3] = b[3].min(252); // not opaque: mode 6 only
        let pick: Vec<bool> = (0..16).map(|_| rng.below(2) == 0).collect();
        let block = block_of(|i| if pick[i] { a } else { b });
        let bytes = bc7::encode_block(&block);
        assert_eq!(bc7::mode(&bytes), Some(6), "block {n}");
        assert_eq!(bc7::decode_block(&bytes), Some(block), "block {n}");
    }
    // Mode 1: four representable colors, two per subset of a random partition. One subset
    // of mode 6 cannot hold four colors off a line, so mode 1 wins with an exact fit, and
    // both anchors (texel 0 and the partition's subset-1 anchor) must hold.
    let mut mode1 = 0;
    for n in 0..120 {
        let partition = rng.below(64) as usize;
        let mask = bc7::PARTITIONS2[partition];
        let colors: Vec<[u8; 4]> = (0..2)
            .flat_map(|_| {
                let p = (rng.below(2)) as u8;
                let mut c = || {
                    [
                        expand6(rng.below(64) as u8, p),
                        expand6(rng.below(64) as u8, p),
                        expand6(rng.below(64) as u8, p),
                        255,
                    ]
                };
                [c(), c()]
            })
            .collect();
        let block = block_of(|i| {
            let subset = ((mask >> i) & 1) as usize;
            colors[subset * 2 + usize::from(i % 3 == 0)]
        });
        let bytes = bc7::encode_block(&block);
        let decoded = bc7::decode_block(&bytes).ok_or("decodes")?;
        if bc7::mode(&bytes) == Some(1) {
            mode1 += 1;
            // Anchor index high bits are implicit zeros: check the stored 2-bit fields
            // decode exactly, which is the observable consequence.
            assert_eq!(decoded, block, "block {n} (partition {partition})");
        }
        let err: u32 = decoded
            .iter()
            .zip(&block)
            .flat_map(|(d, t)| d.iter().zip(t).map(|(x, y)| u32::from(x.abs_diff(*y)).pow(2)))
            .sum();
        assert_eq!(
            err, 0,
            "block {n}: four representable colors in two subsets are exact"
        );
    }
    assert!(
        mode1 > 60,
        "mode 1 is used for two-subset blocks ({mode1} of 120)"
    );
    Ok(())
}

// ------------------------------------------------------------------ images

fn gradient(width: u32, height: u32, alpha: bool) -> Vec<[u8; 4]> {
    (0..width * height)
        .map(|i| {
            let (x, y) = (i % width, i / width);
            let a = if alpha {
                255 - (x * 255 / (width - 1)) as u8
            } else {
                255
            };
            [
                (x * 255 / (width - 1)) as u8,
                (y * 255 / (height - 1)) as u8,
                ((x + y) * 255 / (width + height - 2)) as u8,
                a,
            ]
        })
        .collect()
}

/// A one-dimensional ramp: gray rising left to right (alpha falling, if any).
fn ramp(w: u32, h: u32, alpha: bool) -> Vec<[u8; 4]> {
    (0..w * h)
        .map(|i| {
            let v = ((i % w) * 255 / (w - 1)) as u8;
            [v, v, v, if alpha { 255 - v } else { 255 }]
        })
        .collect()
}

fn noise(w: u32, h: u32, seed: u32) -> Vec<[u8; 4]> {
    let mut rng = Rng(seed);
    (0..w * h)
        .map(|_| [rng.byte(), rng.byte(), rng.byte(), rng.byte()])
        .collect()
}

/// PSNR in dB over the first `channels` channels.
fn psnr(a: &[[u8; 4]], b: &[[u8; 4]], channels: usize) -> f64 {
    let mut sum = 0.0f64;
    let mut n = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        for c in 0..channels {
            let d = f64::from(x[c]) - f64::from(y[c]);
            sum += d * d;
            n += 1.0;
        }
    }
    if sum == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / (sum / n)).log10()
}

fn round_trip(
    enc: Encoding,
    w: u32,
    h: u32,
    texels: &[[u8; 4]],
    cutoff: Option<u8>,
) -> Result<Vec<[u8; 4]>, String> {
    let bytes = encode_image(enc, w, h, texels, cutoff);
    decode_image(enc, w, h, &bytes).ok_or_else(|| format!("{enc:?} decodes"))
}

#[test]
fn round_trip_error_is_bounded() -> TestResult {
    // (encoding, image, alpha in the gradient, channels compared, minimum PSNR in dB)
    // Floors sit 1 to 2 dB under the measured values. The gradient varies R, G, and B
    // independently across each block (a plane, not a line, in color space), which a
    // one-line BC1 or mode 6 fit cannot follow exactly; the ramp is one-dimensional.
    let cases: [(Encoding, &str, bool, usize, f64); 14] = [
        (Encoding::Bc1, "gradient", false, 3, 30.0),
        (Encoding::Bc1, "ramp", false, 3, 40.5),
        (Encoding::Bc1, "noise", false, 3, 12.5),
        (Encoding::Bc4, "gradient", false, 1, 46.0),
        (Encoding::Bc4, "ramp", false, 1, 46.0),
        (Encoding::Bc4, "noise", false, 1, 28.0),
        (Encoding::Bc5, "gradient", false, 2, 46.0),
        (Encoding::Bc5, "noise", false, 2, 28.0),
        (Encoding::Bc7, "gradient", false, 4, 38.5),
        (Encoding::Bc7, "gradient", true, 4, 30.5),
        (Encoding::Bc7, "ramp", false, 4, 54.0),
        (Encoding::Bc7, "ramp", true, 4, 53.0),
        (Encoding::Bc7, "noise", false, 4, 12.0),
        (Encoding::Rgba8, "noise", false, 4, f64::INFINITY),
    ];
    let (w, h) = (30, 22); // partial blocks on both edges
    for (enc, image, alpha, channels, floor) in cases {
        let src = match image {
            "gradient" => gradient(w, h, alpha),
            "ramp" => ramp(w, h, alpha),
            _ => noise(w, h, 7),
        };
        let src: Vec<[u8; 4]> = if enc == Encoding::Bc7 || enc == Encoding::Rgba8 || alpha {
            src
        } else {
            src.iter().map(|t| [t[0], t[1], t[2], 255]).collect()
        };
        let back = round_trip(enc, w, h, &src, None)?;
        let p = psnr(&src, &back, channels);
        println!("{enc:?} {image} alpha={alpha}: {p:.2} dB");
        assert!(p >= floor, "{enc:?} {image}: {p:.2} dB < {floor} dB");
    }
    // Two-subset mode 1 helps blocks with two color clusters: BC7 on a hard-edged
    // two-tone pattern beats a single-subset bound.
    let edges: Vec<[u8; 4]> = (0..32 * 32)
        .map(|i| {
            if (i % 32 + i / 32) % 7 < 3 {
                [200, 30, 40, 255]
            } else {
                [20, 90, 220, 255]
            }
        })
        .collect();
    let back = round_trip(Encoding::Bc7, 32, 32, &edges, None)?;
    let p = psnr(&edges, &back, 4);
    println!("Bc7 two-tone edges: {p:.2} dB");
    assert!(p >= 50.0, "two-tone edges: {p:.2} dB");
    Ok(())
}

#[test]
fn encoding_is_deterministic() -> TestResult {
    let src = noise(37, 19, 99);
    for enc in [
        Encoding::Bc1,
        Encoding::Bc4,
        Encoding::Bc5,
        Encoding::Bc7,
        Encoding::Rgba8,
    ] {
        let a = encode_image(enc, 37, 19, &src, None);
        let b = encode_image(enc, 37, 19, &src, None);
        assert_eq!(a, b, "{enc:?}");
        assert_eq!(a.len() as u64, enc.image_bytes(37, 19));
        decode_image(enc, 37, 19, &a).ok_or("the reference decoder reads it")?;
    }
    let a = encode_image(Encoding::Bc1, 37, 19, &src, Some(100));
    assert_eq!(a, encode_image(Encoding::Bc1, 37, 19, &src, Some(100)));
    // A fixed hash pins the exact bytes, so an unannounced encoder change fails here
    // until its ENCODER_VERSION is bumped and this value updated with it.
    let all: Vec<u8> = [Encoding::Bc1, Encoding::Bc4, Encoding::Bc5, Encoding::Bc7]
        .iter()
        .flat_map(|&e| encode_image(e, 37, 19, &src, None))
        .collect();
    let hash = blake3_hex(&all);
    println!("encoder output hash: {hash}");
    assert_eq!(
        (
            bc1::ENCODER_VERSION,
            bc4::ENCODER_VERSION,
            bc7::ENCODER_VERSION,
            hash.as_str()
        ),
        (1, 1, 1, PINNED_HASH)
    );
    Ok(())
}

const PINNED_HASH: &str = "46d57aa6174063d92709c5e219f24bea79dc38505ad5f6508f43998ee6918941";

fn blake3_hex(bytes: &[u8]) -> String {
    mantis_core::content::ContentHash::of(bytes).to_string()
}
