//! Color grading v1: the data a renderer bakes into its grading lookup table, applied in
//! display space after tone mapping. Authored per package (and per area, blended by the
//! renderer).
//!
//! Layout (little-endian, 64 bytes):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MGRD"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | contrast `f32`, 0 to 4 (1 neutral) |
//! | 12 | 4 | saturation `f32`, 0 to 4 (1 neutral) |
//! | 16 | 4 | temperature `f32`, -1 to 1 (0 neutral; positive warms) |
//! | 20 | 4 | tint `f32`, -1 to 1 (0 neutral; positive toward magenta) |
//! | 24 | 12 | lift `[f32; 3]`, each -1 to 1 (0 neutral) |
//! | 36 | 12 | gamma `[f32; 3]`, each 0.1 to 10 (1 neutral) |
//! | 48 | 12 | gain `[f32; 3]`, each 0 to 4 (1 neutral) |
//! | 60 | 4 | reserved, 0 |

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MGRD";

/// Grading parameters.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ColorGrading {
    /// Contrast around mid gray.
    pub contrast: f32,
    /// Saturation.
    pub saturation: f32,
    /// White balance: blue (negative) to orange (positive).
    pub temperature: f32,
    /// White balance: green (negative) to magenta (positive).
    pub tint: f32,
    /// Added to shadows.
    pub lift: [f32; 3],
    /// Midtone power.
    pub gamma: [f32; 3],
    /// Multiplier on highlights.
    pub gain: [f32; 3],
}

impl ColorGrading {
    /// No change.
    pub const NEUTRAL: ColorGrading = ColorGrading {
        contrast: 1.0,
        saturation: 1.0,
        temperature: 0.0,
        tint: 0.0,
        lift: [0.0; 3],
        gamma: [1.0; 3],
        gain: [1.0; 3],
    };

    /// Checks every range.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for any value out of range (non-finite values are out
    /// of range).
    pub fn validate(&self) -> Result<(), FormatError> {
        let within = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        let ok = within(self.contrast, 0.0, 4.0)
            && within(self.saturation, 0.0, 4.0)
            && within(self.temperature, -1.0, 1.0)
            && within(self.tint, -1.0, 1.0)
            && self.lift.iter().all(|v| within(*v, -1.0, 1.0))
            && self.gamma.iter().all(|v| within(*v, 0.1, 10.0))
            && self.gain.iter().all(|v| within(*v, 0.0, 4.0));
        if ok { Ok(()) } else { Err(FormatError::Dimensions) }
    }

    /// Grades one display-space color in [0, 1]. The shared definition the renderer bakes
    /// into its lookup table.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        // White balance as channel gains (a small, monotonic approximation).
        let wb = [
            1.0 + 0.1 * self.temperature - 0.05 * self.tint,
            1.0 + 0.1 * self.tint,
            1.0 - 0.1 * self.temperature - 0.05 * self.tint,
        ];
        let mut c = [0.0f32; 3];
        for (((o, v), w), ((lift, gamma), gain)) in c
            .iter_mut()
            .zip(rgb)
            .zip(wb)
            .zip(self.lift.iter().zip(&self.gamma).zip(&self.gain))
        {
            // Lift, gamma, gain: gain * (x + lift * (1 - x)) ^ (1 / gamma).
            let x = (v * w).clamp(0.0, 1.0);
            let lifted = (x + lift * (1.0 - x)).clamp(0.0, 1.0);
            *o = gain * lifted.powf(1.0 / gamma);
        }
        let [red, green, blue] = c;
        let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
        c.map(|v| {
            let saturated = luma + (v - luma) * self.saturation;
            ((saturated - 0.5) * self.contrast + 0.5).clamp(0.0, 1.0)
        })
    }

    /// Parses and validates.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<ColorGrading, FormatError> {
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
        let g = ColorGrading {
            contrast: r.f32_raw()?,
            saturation: r.f32_raw()?,
            temperature: r.f32_raw()?,
            tint: r.f32_raw()?,
            lift: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
            gamma: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
            gain: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
        };
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        r.finish()?;
        g.validate()?;
        Ok(g)
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        for v in [self.contrast, self.saturation, self.temperature, self.tint] {
            w.f32(v);
        }
        w.vec3(self.lift);
        w.vec3(self.gamma);
        w.vec3(self.gain);
        w.u32(0);
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn neutral_is_identity() {
        for v in [0.0f32, 0.1, 0.5, 0.9, 1.0] {
            let out = ColorGrading::NEUTRAL.apply([v, v * 0.5, 1.0 - v]);
            for (o, e) in out.iter().zip([v, v * 0.5, 1.0 - v]) {
                assert!((o - e).abs() < 1e-6, "{out:?}");
            }
        }
    }

    #[test]
    fn saturation_zero_is_gray_and_controls_respond() {
        let gray = ColorGrading {
            saturation: 0.0,
            ..ColorGrading::NEUTRAL
        }
        .apply([0.8, 0.2, 0.1]);
        assert!((gray[0] - gray[1]).abs() < 1e-6 && (gray[1] - gray[2]).abs() < 1e-6);
        let warm = ColorGrading {
            temperature: 1.0,
            ..ColorGrading::NEUTRAL
        }
        .apply([0.5; 3]);
        assert!(warm[0] > warm[2]);
        let contrast = ColorGrading {
            contrast: 2.0,
            ..ColorGrading::NEUTRAL
        }
        .apply([0.25, 0.5, 0.75]);
        assert!(
            (contrast[0] - 0.0).abs() < 1e-6
                && (contrast[1] - 0.5).abs() < 1e-6
                && (contrast[2] - 1.0).abs() < 1e-6
        );
        let lifted = ColorGrading {
            lift: [0.2; 3],
            ..ColorGrading::NEUTRAL
        }
        .apply([0.0; 3]);
        assert!((lifted[0] - 0.2).abs() < 1e-6, "black lifts");
    }

    #[test]
    fn round_trips_and_rejects() -> TestResult {
        let g = ColorGrading {
            contrast: 1.2,
            saturation: 0.8,
            temperature: 0.3,
            tint: -0.1,
            lift: [0.01, 0.0, 0.02],
            gamma: [1.1, 1.0, 0.9],
            gain: [1.0, 1.05, 1.1],
        };
        let bytes = g.encode();
        assert_eq!(bytes.len(), 64);
        assert_eq!(ColorGrading::parse(&bytes)?, g);
        let corrupt = |at: usize, v: &[u8]| {
            let mut b = bytes.clone();
            if let Some(s) = b.get_mut(at..at + v.len()) {
                s.copy_from_slice(v);
            }
            ColorGrading::parse(&b).err()
        };
        assert_eq!(corrupt(0, b"MGRX"), Some(FormatError::Magic));
        assert_eq!(corrupt(6, &1u16.to_le_bytes()), Some(FormatError::Flags(1)));
        assert_eq!(corrupt(8, &9.0f32.to_le_bytes()), Some(FormatError::Dimensions));
        assert_eq!(
            corrupt(36, &0.0f32.to_le_bytes()),
            Some(FormatError::Dimensions),
            "gamma 0"
        );
        assert_eq!(
            corrupt(12, &f32::NAN.to_le_bytes()),
            Some(FormatError::Dimensions)
        );
        assert_eq!(corrupt(60, &1u32.to_le_bytes()), Some(FormatError::Reserved));
        assert!(matches!(
            ColorGrading::parse(bytes.get(..63).unwrap_or(&[])),
            Err(FormatError::Length { .. })
        ));
        Ok(())
    }
}
