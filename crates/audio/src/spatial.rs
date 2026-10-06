//! Spatialization: the listener frame, equal-power stereo panning, and distance
//! attenuation.
//!
//! Conventions: the left-handed world of decision 0019 (+X right, +Y up, +Z forward), so
//! `right = up x forward`. With the default listener (forward `+z`, up `+y`) the right ear
//! points along `+x`. Clients derive the listener from the camera basis.
//!
//! A source's pan is the sine of its azimuth (`dot(direction, right)`), so sources in
//! front and behind at the same lateral offset pan alike; there is no front/back cue yet.
//! Within the sound's `min_distance` the pan narrows linearly toward center, so a source
//! passing through the listener sweeps smoothly instead of flipping sides. A source at
//! the listener is centered with attenuation 1.
//!
//! Later (not in this version): HRTF, doppler, occlusion, and reverb sends.

use mantis_formats::sound_bank::{Attenuation, AttenuationModel};

/// Below this distance a source counts as at the listener.
const AT_LISTENER: f32 = 1e-6;

/// The listener's position and orthonormal frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Listener {
    position: [f32; 3],
    forward: [f32; 3],
    up: [f32; 3],
    right: [f32; 3],
}

impl Default for Listener {
    fn default() -> Self {
        Listener {
            position: [0.0; 3],
            forward: [0.0, 0.0, 1.0],
            up: [0.0, 1.0, 0.0],
            right: [1.0, 0.0, 0.0],
        }
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: [f32; 3]) -> Option<[f32; 3]> {
    let len = dot(a, a).sqrt();
    (len.is_finite() && len > 1e-12).then(|| [a[0] / len, a[1] / len, a[2] / len])
}

impl Listener {
    /// A listener at `position` facing `forward` with `up` roughly up. `None` when any
    /// value is not finite, a direction is zero, or the two are parallel.
    pub fn new(position: [f32; 3], forward: [f32; 3], up: [f32; 3]) -> Option<Listener> {
        if position.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let forward = normalize(forward)?;
        let right = normalize(cross(normalize(up)?, forward))?;
        Some(Listener {
            position,
            forward,
            up: cross(forward, right),
            right,
        })
    }

    /// World position.
    pub fn position(&self) -> [f32; 3] {
        self.position
    }

    /// Unit facing direction.
    pub fn forward(&self) -> [f32; 3] {
        self.forward
    }

    /// Unit up direction, orthogonal to forward.
    pub fn up(&self) -> [f32; 3] {
        self.up
    }

    /// Unit right direction (`up x forward`, decision 0019).
    pub fn right(&self) -> [f32; 3] {
        self.right
    }

    /// Distance to `source` and its pan in -1 (left) to 1 (right), narrowed toward center
    /// inside `min_distance`.
    pub fn locate(&self, source: [f32; 3], min_distance: f32) -> (f32, f32) {
        let d = sub(source, self.position);
        let distance = dot(d, d).sqrt();
        if !distance.is_finite() || distance < AT_LISTENER {
            return (0.0, 0.0);
        }
        let lateral = (dot(d, self.right) / distance).clamp(-1.0, 1.0);
        let narrow = if min_distance > 0.0 {
            (distance / min_distance).min(1.0)
        } else {
            1.0
        };
        (distance, lateral * narrow)
    }
}

/// Equal-power gains `[left, right]` for a pan in -1 to 1: `left^2 + right^2 = 1`, center
/// is `1/sqrt(2)` per side, hard right is `[0, 1]`.
pub fn equal_power(pan: f32) -> [f32; 2] {
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * core::f32::consts::FRAC_PI_4;
    [theta.cos().max(0.0), theta.sin().max(0.0)]
}

/// Distance gain per the sound's model, with the distance clamped to
/// `min_distance..=max_distance` first (so 1 inside `min_distance`).
pub fn attenuation(a: &Attenuation, distance: f32) -> f32 {
    let d = distance.clamp(a.min_distance, a.max_distance);
    let g = match a.model {
        AttenuationModel::Linear => {
            1.0 - a.rolloff * (d - a.min_distance) / (a.max_distance - a.min_distance)
        }
        AttenuationModel::Inverse => a.min_distance / (a.min_distance + a.rolloff * (d - a.min_distance)),
        AttenuationModel::Exponential => (d / a.min_distance).powf(-a.rolloff),
    };
    g.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn att(model: AttenuationModel, rolloff: f32) -> Attenuation {
        Attenuation {
            model,
            min_distance: 1.0,
            max_distance: 11.0,
            rolloff,
        }
    }

    #[test]
    fn attenuation_curves_at_min_mid_max() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        let lin = att(AttenuationModel::Linear, 1.0);
        assert!(close(attenuation(&lin, 0.2), 1.0));
        assert!(close(attenuation(&lin, 1.0), 1.0));
        assert!(close(attenuation(&lin, 6.0), 0.5));
        assert!(close(attenuation(&lin, 11.0), 0.0));
        assert!(close(attenuation(&lin, 100.0), 0.0));
        let half = att(AttenuationModel::Linear, 0.5);
        assert!(close(attenuation(&half, 11.0), 0.5));
        let inv = att(AttenuationModel::Inverse, 1.0);
        assert!(close(attenuation(&inv, 1.0), 1.0));
        assert!(close(attenuation(&inv, 6.0), 1.0 / 6.0));
        assert!(close(attenuation(&inv, 11.0), 1.0 / 11.0));
        assert!(close(attenuation(&inv, 50.0), 1.0 / 11.0));
        let exp = att(AttenuationModel::Exponential, 2.0);
        assert!(close(attenuation(&exp, 1.0), 1.0));
        assert!(close(attenuation(&exp, 6.0), 1.0 / 36.0));
        assert!(close(attenuation(&exp, 11.0), 1.0 / 121.0));
        assert!(close(attenuation(&exp, 1e9), 1.0 / 121.0));
    }

    #[test]
    fn equal_power_is_constant_power() {
        for i in 0..=20u8 {
            let pan = f32::from(i) / 10.0 - 1.0;
            let [l, r] = equal_power(pan);
            assert!((l * l + r * r - 1.0).abs() < 1e-6, "pan {pan}");
        }
        let [l, r] = equal_power(1.0);
        assert!(l < 1e-6 && (r - 1.0).abs() < 1e-6);
        let [l, r] = equal_power(0.0);
        assert!((l - r).abs() < 1e-7 && (l - core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn listener_frame_and_locate() -> TestResult {
        let l = Listener::default();
        assert_eq!(l.locate([5.0, 0.0, 0.0], 1.0), (5.0, 1.0));
        assert_eq!(l.locate([-5.0, 0.0, 0.0], 1.0), (5.0, -1.0));
        assert_eq!(l.locate([0.0, 0.0, 5.0], 1.0), (5.0, 0.0));
        assert_eq!(l.locate([0.0; 3], 1.0), (0.0, 0.0));
        // Inside min distance the pan narrows.
        let (d, pan) = l.locate([0.5, 0.0, 0.0], 1.0);
        assert!((d - 0.5).abs() < 1e-6 && (pan - 0.5).abs() < 1e-6);
        // Turned to face +x, a source at +x is straight ahead and -z is on the right.
        let t = Listener::new([0.0; 3], [2.0, 0.0, 0.0], [1.0, 3.0, 0.0]).ok_or("listener")?;
        let (_, ahead) = t.locate([5.0, 0.0, 0.0], 1.0);
        assert!(ahead.abs() < 1e-6);
        let (_, right) = t.locate([0.0, 0.0, -5.0], 1.0);
        assert!((right - 1.0).abs() < 1e-6);
        assert!((dot(t.up(), t.forward())).abs() < 1e-6);
        assert!(Listener::new([0.0; 3], [0.0, 1.0, 0.0], [0.0, 2.0, 0.0]).is_none());
        assert!(Listener::new([0.0; 3], [0.0; 3], [0.0, 1.0, 0.0]).is_none());
        assert!(Listener::new([f32::NAN, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]).is_none());
        Ok(())
    }
}
