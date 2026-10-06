//! Time-of-day keyframes shared by every baked asset.
//!
//! Time of day is a day fraction in [0, 1), 0 = midnight. An asset carries 1 to
//! [`MAX_KEYFRAMES`] keyframe times, strictly increasing, stored in a fixed
//! `[f32; MAX_KEYFRAMES]` field with unused slots exactly `0.0`. At runtime the two
//! keyframes around the current time are blended, wrapping across midnight.

use crate::bytes::{FormatError, Reader, Writer};

/// Most keyframes per asset.
pub const MAX_KEYFRAMES: usize = 8;

/// Reads the fixed keyframe-times field for `count` keyframes.
///
/// # Errors
/// [`FormatError::Keyframes`] for a bad count, a time outside [0, 1), times not strictly
/// increasing, or a nonzero unused slot; [`FormatError::Length`] on short data.
pub fn read_keyframes(r: &mut Reader<'_>, count: u32) -> Result<Vec<f32>, FormatError> {
    if count == 0 || count as usize > MAX_KEYFRAMES {
        return Err(FormatError::Keyframes);
    }
    let mut times = Vec::with_capacity(count as usize);
    for i in 0..MAX_KEYFRAMES {
        let t = r.f32_raw()?;
        if i < count as usize {
            if !(0.0..1.0).contains(&t) || times.last().is_some_and(|p| t <= *p) {
                return Err(FormatError::Keyframes);
            }
            times.push(t);
        } else if t.to_bits() != 0 {
            return Err(FormatError::Keyframes);
        }
    }
    Ok(times)
}

/// Writes the fixed keyframe-times field.
pub fn write_keyframes(w: &mut Writer, times: &[f32]) {
    for i in 0..MAX_KEYFRAMES {
        w.f32(times.get(i).copied().unwrap_or(0.0));
    }
}

/// The blend for `time`: `(a, b, t)` meaning `keyframe[a] * (1 - t) + keyframe[b] * t`.
pub fn keyframe_blend(times: &[f32], time: f32) -> (usize, usize, f32) {
    let n = times.len();
    if n <= 1 {
        return (0, 0, 0.0);
    }
    let time = if time.is_finite() {
        time.rem_euclid(1.0)
    } else {
        0.0
    };
    // The keyframe at or before `time`, wrapping to the last when before the first.
    let a = times.iter().rposition(|t| *t <= time).unwrap_or(n - 1);
    let b = (a + 1) % n;
    let (ta, tb) = (
        times.get(a).copied().unwrap_or(0.0),
        times.get(b).copied().unwrap_or(0.0),
    );
    let span = if tb > ta { tb - ta } else { tb + 1.0 - ta };
    let since = if time >= ta { time - ta } else { time + 1.0 - ta };
    let t = if span > 0.0 {
        (since / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a, b, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blends_and_wraps_across_midnight() {
        let times = [0.25, 0.75];
        assert_eq!(keyframe_blend(&times, 0.25), (0, 1, 0.0));
        assert_eq!(keyframe_blend(&times, 0.5), (0, 1, 0.5));
        assert_eq!(keyframe_blend(&times, 0.75), (1, 0, 0.0));
        let (a, b, t) = keyframe_blend(&times, 0.0);
        assert_eq!((a, b), (1, 0));
        assert!((t - 0.5).abs() < 1e-6);
        assert_eq!(keyframe_blend(&[0.4], 0.9), (0, 0, 0.0));
        let (_, _, t) = keyframe_blend(&times, 1.5);
        assert!((t - 0.5).abs() < 1e-6, "times wrap");
    }

    #[test]
    fn keyframe_field_rules() {
        let encode = |times: &[f32]| {
            let mut w = Writer::new();
            write_keyframes(&mut w, times);
            w.into_bytes()
        };
        let ok = encode(&[0.0, 0.5]);
        assert_eq!(read_keyframes(&mut Reader::new(&ok), 2), Ok(vec![0.0, 0.5]));
        assert_eq!(
            read_keyframes(&mut Reader::new(&ok), 0),
            Err(FormatError::Keyframes)
        );
        assert_eq!(
            read_keyframes(&mut Reader::new(&ok), 9),
            Err(FormatError::Keyframes)
        );
        assert_eq!(
            read_keyframes(&mut Reader::new(&ok), 1),
            Err(FormatError::Keyframes),
            "slot 1 not zero"
        );
        for bad in [[0.5, 0.5], [0.5, 0.25], [0.0, 1.0], [-0.1, 0.5], [0.0, f32::NAN]] {
            assert_eq!(
                read_keyframes(&mut Reader::new(&encode(&bad)), 2),
                Err(FormatError::Keyframes),
                "{bad:?}"
            );
        }
    }
}
