//! Probe volume v1: a regular grid of L1 SH irradiance probes per time-of-day keyframe,
//! baked per sector by the cook (decision 0003).
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); the grid origin and spacing are world space.
//!
//! Layout (little-endian):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MPRB"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 = validity mask present; other bits 0 |
//! | 8 | 12 | `nx, ny, nz: u32`, each at least 2, product at most 2^20 |
//! | 20 | 12 | origin `[f32; 3]`: world position of probe (0, 0, 0), finite |
//! | 32 | 12 | spacing `[f32; 3]`: finite, positive |
//! | 44 | 4 | keyframe count `u32`, 1 to 8 |
//! | 48 | 4 | encoding `u32` = 1 (L1 SH RGB as IEEE f16) |
//! | 52 | 32 | keyframe times `[f32; 8]` (see [`crate::time_of_day`]) |
//! | 84 | 4 | reserved, 0 |
//! | 88 | | per keyframe, per probe (x fastest, then y, then z): 12 f16, R`[L00, L1-1, L10, L11]`, G, B |
//! | | | if flag bit 0: one byte per probe, 0 (invalid: inside geometry) or 1 (valid) |
//!
//! The length must match exactly and every coefficient must be finite. The SH convention
//! is [`crate::sh`]'s.

use crate::bytes::{FormatError, Reader, Writer};
use crate::half::{f16_to_f32, f32_to_f16};
use crate::sh::ShL1;
use crate::time_of_day::{read_keyframes, write_keyframes};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MPRB";
/// Most probes per volume.
pub const MAX_PROBES: u64 = 1 << 20;
const HEADER: u64 = 88;
const HALVES_PER_PROBE: u64 = 12;

/// A probe volume.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeVolume {
    /// Probes along x, y, z.
    pub dims: [u32; 3],
    /// World position of probe (0, 0, 0).
    pub origin: [f32; 3],
    /// Distance between probes along each axis.
    pub spacing: [f32; 3],
    /// Keyframe times (day fractions), strictly increasing.
    pub keyframes: Vec<f32>,
    /// Probes per keyframe, x fastest, then y, then z.
    pub probes: Vec<Vec<ShL1>>,
    /// Probe validity (false: inside geometry, excluded from interpolation).
    pub valid: Option<Vec<bool>>,
}

impl ProbeVolume {
    /// Dense index of probe `(x, y, z)`.
    pub fn index(&self, x: u32, y: u32, z: u32) -> usize {
        let [nx, ny, _] = self.dims;
        (x as usize) + (y as usize) * (nx as usize) + (z as usize) * (nx as usize) * (ny as usize)
    }

    /// Parses and validates a probe volume.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<ProbeVolume, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags & !1 != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let dims = [r.u32()?, r.u32()?, r.u32()?];
        let count: u64 = dims.iter().map(|d| u64::from(*d)).product();
        if dims.iter().any(|d| *d < 2) || count > MAX_PROBES {
            return Err(FormatError::Dimensions);
        }
        let origin = [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?];
        let spacing = [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?];
        if origin.iter().chain(&spacing).any(|v| !v.is_finite()) || spacing.iter().any(|s| *s <= 0.0) {
            return Err(FormatError::Geometry);
        }
        let keyframe_count = r.u32()?;
        let encoding = r.u32()?;
        if encoding != 1 {
            return Err(FormatError::Encoding(encoding));
        }
        let keyframes = read_keyframes(&mut r, keyframe_count)?;
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let has_validity = flags & 1 != 0;
        let expected = HEADER
            + count * HALVES_PER_PROBE * 2 * u64::from(keyframe_count)
            + if has_validity { count } else { 0 };
        if bytes.len() as u64 != expected {
            return Err(FormatError::Length {
                expected,
                actual: bytes.len() as u64,
            });
        }
        #[expect(clippy::cast_possible_truncation)] // count <= 2^20.
        let count = count as usize;
        let mut probes = Vec::with_capacity(keyframes.len());
        for _ in 0..keyframes.len() {
            let mut frame = Vec::with_capacity(count);
            for _ in 0..count {
                let mut sh = ShL1::ZERO;
                for c in sh.rgb.iter_mut().flatten() {
                    *c = f16_to_f32(r.u16()?);
                }
                if !sh.is_finite() {
                    return Err(FormatError::NonFinite);
                }
                frame.push(sh);
            }
            probes.push(frame);
        }
        let valid = if has_validity {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                match r.u8()? {
                    0 => v.push(false),
                    1 => v.push(true),
                    _ => return Err(FormatError::Validity),
                }
            }
            Some(v)
        } else {
            None
        };
        r.finish()?;
        Ok(ProbeVolume {
            dims,
            origin,
            spacing,
            keyframes,
            probes,
            valid,
        })
    }

    /// Reference encoder (the inverse of [`ProbeVolume::parse`] up to f16 rounding).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(u16::from(self.valid.is_some()));
        for d in self.dims {
            w.u32(d);
        }
        w.vec3(self.origin);
        w.vec3(self.spacing);
        w.count(self.keyframes.len());
        w.u32(1);
        write_keyframes(&mut w, &self.keyframes);
        w.u32(0);
        for frame in &self.probes {
            for sh in frame {
                for c in sh.rgb.iter().flatten() {
                    w.u16(f32_to_f16(*c));
                }
            }
        }
        if let Some(valid) = &self.valid {
            for v in valid {
                w.u8(u8::from(*v));
            }
        }
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// 3 x 2 x 2 probes; keyframe 0 grows with x, keyframe 1 is uniform 2.
    pub(crate) fn volume() -> ProbeVolume {
        let levels = [0.0f32, 1.0, 2.0];
        let k0 = (0..12)
            .map(|i| ShL1::constant([levels.get(i % 3).copied().unwrap_or(0.0); 3]))
            .collect();
        ProbeVolume {
            dims: [3, 2, 2],
            origin: [-1.0, 0.0, 0.0],
            spacing: [1.0, 2.0, 2.0],
            keyframes: vec![0.25, 0.75],
            probes: vec![k0, vec![ShL1::constant([2.0; 3]); 12]],
            valid: None,
        }
    }

    #[test]
    fn round_trips() -> TestResult {
        let v = volume();
        let bytes = v.encode();
        assert_eq!(bytes.len(), 88 + 12 * 24 * 2);
        let back = ProbeVolume::parse(&bytes)?;
        assert_eq!(
            (back.dims, back.origin, back.spacing, &back.keyframes),
            (v.dims, v.origin, v.spacing, &v.keyframes)
        );
        assert_eq!(back.encode(), bytes, "f16 quantization is idempotent");
        for (a, b) in back.probes.iter().flatten().zip(v.probes.iter().flatten()) {
            for (x, y) in a.rgb.iter().flatten().zip(b.rgb.iter().flatten()) {
                assert!((x - y).abs() <= y.abs() * 1e-3, "{x} vs {y}");
            }
        }
        let mut masked = v.clone();
        masked.valid = Some((0..12).map(|i| i != 5).collect());
        assert_eq!(ProbeVolume::parse(&masked.encode())?.valid, masked.valid);
        Ok(())
    }

    #[test]
    fn rejects_every_malformed_field() {
        let good = volume().encode();
        let corrupt = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            if let Some(s) = b.get_mut(at..at + bytes.len()) {
                s.copy_from_slice(bytes);
            }
            ProbeVolume::parse(&b)
        };
        assert_eq!(corrupt(0, b"XPRB").err(), Some(FormatError::Magic));
        assert_eq!(
            corrupt(4, &2u16.to_le_bytes()).err(),
            Some(FormatError::Version(2))
        );
        assert_eq!(corrupt(6, &4u16.to_le_bytes()).err(), Some(FormatError::Flags(4)));
        assert_eq!(
            corrupt(8, &1u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(32, &0.0f32.to_le_bytes()).err(),
            Some(FormatError::Geometry)
        );
        assert_eq!(
            corrupt(20, &f32::NAN.to_le_bytes()).err(),
            Some(FormatError::Geometry)
        );
        assert_eq!(
            corrupt(44, &9u32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(48, &2u32.to_le_bytes()).err(),
            Some(FormatError::Encoding(2))
        );
        assert_eq!(
            corrupt(56, &0.1f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(60, &0.5f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(84, &1u32.to_le_bytes()).err(),
            Some(FormatError::Reserved)
        );
        assert_eq!(
            corrupt(88, &0x7c00u16.to_le_bytes()).err(),
            Some(FormatError::NonFinite)
        );
        assert!(matches!(
            ProbeVolume::parse(good.get(..good.len() - 1).unwrap_or(&[])),
            Err(FormatError::Length { .. })
        ));
        let mut longer = good.clone();
        longer.push(0);
        assert!(matches!(
            ProbeVolume::parse(&longer),
            Err(FormatError::Length { .. })
        ));
        let mut masked = volume();
        masked.valid = Some(vec![true; 12]);
        let mut b = masked.encode();
        if let Some(last) = b.last_mut() {
            *last = 7;
        }
        assert_eq!(ProbeVolume::parse(&b).err(), Some(FormatError::Validity));
    }
}
