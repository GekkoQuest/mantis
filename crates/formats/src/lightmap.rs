//! Lightmap v1: an atlas of `Rgb9e5Ufloat` texels with a layer per time-of-day keyframe,
//! baked per sector by the cook (decision 0003). Texels hold irradiance divided by pi,
//! directly comparable with probe-volume irradiance.
//!
//! Layout (little-endian):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MLMP"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 8 | `width, height: u32`, each 1 to 8192 |
//! | 16 | 4 | keyframe count `u32`, 1 to 8 |
//! | 20 | 4 | encoding `u32` = 1 (`Rgb9e5Ufloat`, one `u32` per texel) |
//! | 24 | 32 | keyframe times `[f32; 8]` (see [`crate::time_of_day`]) |
//! | 56 | 8 | reserved, 0 |
//! | 64 | | per keyframe, rows top to bottom, texels left to right: `u32` |

use crate::bytes::{FormatError, Reader, Writer};
use crate::time_of_day::{read_keyframes, write_keyframes};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MLMP";
/// Largest edge.
pub const MAX_EDGE: u32 = 8192;
const HEADER: u64 = 64;

/// A lightmap atlas.
#[derive(Clone, Debug, PartialEq)]
pub struct Lightmap {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Keyframe times.
    pub keyframes: Vec<f32>,
    /// `Rgb9e5Ufloat` texels per keyframe, row-major.
    pub layers: Vec<Vec<u32>>,
}

/// Decodes one `Rgb9e5Ufloat` texel to linear RGB.
#[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)] // 5- and 9-bit fields.
pub fn rgb9e5_to_rgb(v: u32) -> [f32; 3] {
    let exponent = (v >> 27) as i32 - 15 - 9;
    let scale = 2.0f32.powi(exponent);
    let field = |shift: u32| ((v >> shift) & 0x1ff) as f32 * scale;
    [field(0), field(9), field(18)]
}

/// Encodes linear RGB as `Rgb9e5Ufloat` (negative and non-finite components become 0;
/// values beyond the format's range saturate).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Ranges clamped.
pub fn rgb_to_rgb9e5(color: [f32; 3]) -> u32 {
    const MAX: f32 = 65_408.0; // (511 / 512) * 2^16
    let clean = |x: f32| if x.is_finite() { x.clamp(0.0, MAX) } else { 0.0 };
    let [red, green, blue] = color.map(clean);
    let largest = red.max(green).max(blue);
    if largest <= 0.0 {
        return 0;
    }
    let mut exponent = (largest.log2().floor() as i32).max(-16) + 1 + 15;
    let mut scale = 2.0f32.powi(exponent - 15 - 9);
    if (largest / scale + 0.5).floor() as u32 == 512 {
        exponent += 1;
        scale *= 2.0;
    }
    let quantize = |x: f32| ((x / scale + 0.5).floor() as u32).min(511);
    ((exponent.clamp(0, 31) as u32) << 27) | (quantize(blue) << 18) | (quantize(green) << 9) | quantize(red)
}

impl Lightmap {
    /// Parses and validates a lightmap.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<Lightmap, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let (width, height) = (r.u32()?, r.u32()?);
        if !(1..=MAX_EDGE).contains(&width) || !(1..=MAX_EDGE).contains(&height) {
            return Err(FormatError::Dimensions);
        }
        let keyframe_count = r.u32()?;
        let encoding = r.u32()?;
        if encoding != 1 {
            return Err(FormatError::Encoding(encoding));
        }
        let keyframes = read_keyframes(&mut r, keyframe_count)?;
        if r.u32()? != 0 || r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let texels = u64::from(width) * u64::from(height);
        let expected = HEADER + texels * 4 * u64::from(keyframe_count);
        if bytes.len() as u64 != expected {
            return Err(FormatError::Length {
                expected,
                actual: bytes.len() as u64,
            });
        }
        #[allow(clippy::cast_possible_truncation)] // At most 2^26 texels.
        let texels = texels as usize;
        let mut layers = Vec::with_capacity(keyframes.len());
        for _ in 0..keyframes.len() {
            let mut layer = Vec::with_capacity(texels);
            for _ in 0..texels {
                layer.push(r.u32()?);
            }
            layers.push(layer);
        }
        r.finish()?;
        Ok(Lightmap {
            width,
            height,
            keyframes,
            layers,
        })
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.u32(self.width);
        w.u32(self.height);
        w.count(self.layers.len());
        w.u32(1);
        write_keyframes(&mut w, &self.keyframes);
        w.u32(0);
        w.u32(0);
        for layer in &self.layers {
            for t in layer {
                w.u32(*t);
            }
        }
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn rgb9e5_round_trips_within_precision() {
        for c in [[0.0; 3], [1.0; 3], [0.1, 2.0, 300.0], [65_000.0, 1e-3, 0.5]] {
            let back = rgb9e5_to_rgb(rgb_to_rgb9e5(c));
            let m = c.iter().fold(0.0f32, |a, b| a.max(*b));
            for (x, y) in back.iter().zip(&c) {
                assert!((x - y).abs() <= m / 256.0 + 1e-6, "{c:?} -> {back:?}");
            }
        }
        assert_eq!(rgb_to_rgb9e5([-1.0, f32::NAN, 0.0]), 0);
    }

    #[test]
    fn round_trips_and_validates() -> TestResult {
        let lm = Lightmap {
            width: 4,
            height: 2,
            keyframes: vec![0.0, 0.5],
            layers: vec![vec![rgb_to_rgb9e5([1.0; 3]); 8], vec![rgb_to_rgb9e5([4.0; 3]); 8]],
        };
        let bytes = lm.encode();
        assert_eq!(bytes.len(), 64 + 8 * 4 * 2);
        assert_eq!(Lightmap::parse(&bytes)?, lm);
        let corrupt = |at: usize, v: &[u8]| {
            let mut b = bytes.clone();
            if let Some(s) = b.get_mut(at..at + v.len()) {
                s.copy_from_slice(v);
            }
            Lightmap::parse(&b)
        };
        assert_eq!(corrupt(0, b"MLMX").err(), Some(FormatError::Magic));
        assert_eq!(corrupt(6, &1u16.to_le_bytes()).err(), Some(FormatError::Flags(1)));
        assert_eq!(
            corrupt(8, &0u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(12, &8193u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(20, &2u32.to_le_bytes()).err(),
            Some(FormatError::Encoding(2))
        );
        assert_eq!(
            corrupt(56, &1u32.to_le_bytes()).err(),
            Some(FormatError::Reserved)
        );
        assert!(matches!(
            Lightmap::parse(bytes.get(..70).unwrap_or(&[])),
            Err(FormatError::Length { .. })
        ));
        Ok(())
    }
}
