//! Camera orientation: render-thread state (decision 0009).
//!
//! Mouse look is applied every frame on the render thread, so turning is as smooth as
//! the frame rate allows regardless of tick rate. The simulation never owns the camera;
//! it receives the quantized yaw and aim inside the move intent via [`LookSample`].
//!
//! Angles are in turns (1.0 = full circle). Building view matrices (which needs
//! trigonometry) is the renderer's job, outside this crate.

use crate::core_api::{Angle16, angle_from_turns};

/// Look tuning. Data, normally loaded from user settings.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LookConfig {
    /// Turns per mouse count.
    pub mouse_turns_per_count: f32,
    /// Invert vertical look.
    pub invert_y: bool,
    /// Lowest pitch, in turns (negative looks down).
    pub pitch_min: f32,
    /// Highest pitch, in turns.
    pub pitch_max: f32,
    /// Turns per second at full stick deflection.
    pub stick_turns_per_second: f32,
}

impl Default for LookConfig {
    fn default() -> Self {
        Self {
            mouse_turns_per_count: 0.000_25,
            invert_y: false,
            pitch_min: -0.24,
            pitch_max: 0.24,
            stick_turns_per_second: 0.5,
        }
    }
}

/// The quantized look state handed to the simulation for the move intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LookSample {
    /// Facing.
    pub yaw: Angle16,
    /// Aim pitch.
    pub pitch: Angle16,
}

/// Camera orientation owned by the render thread.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CameraRig {
    yaw: f32,
    pitch: f32,
    config: LookConfig,
}

impl CameraRig {
    /// A camera facing yaw 0, level.
    pub fn new(config: LookConfig) -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            config,
        }
    }

    /// Look tuning.
    pub fn config(&self) -> &LookConfig {
        &self.config
    }

    /// Replaces look tuning (settings change).
    pub fn set_config(&mut self, config: LookConfig) {
        self.config = config;
        self.pitch = self.clamp_pitch(self.pitch);
    }

    /// Yaw in turns, in [0, 1).
    pub fn yaw_turns(&self) -> f32 {
        self.yaw
    }

    /// Pitch in turns, within the configured limits.
    pub fn pitch_turns(&self) -> f32 {
        self.pitch
    }

    /// Sets orientation directly (for example, to face the avatar after a teleport).
    pub fn set(&mut self, yaw_turns: f32, pitch_turns: f32) {
        if yaw_turns.is_finite() {
            self.yaw = yaw_turns.rem_euclid(1.0);
        }
        if pitch_turns.is_finite() {
            self.pitch = self.clamp_pitch(pitch_turns);
        }
    }

    /// Applies raw mouse motion in counts. Positive `dx` turns right, positive `dy`
    /// looks down (screen convention) unless inverted. Non-finite input is ignored.
    pub fn apply_mouse(&mut self, dx: f32, dy: f32) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        let k = self.config.mouse_turns_per_count;
        let dy = if self.config.invert_y { dy } else { -dy };
        self.turn(dx * k, dy * k);
    }

    /// Applies analog stick look for one frame of `dt_seconds`. Axes are in [-1, 1].
    pub fn apply_stick(&mut self, x: f32, y: f32, dt_seconds: f32) {
        if !x.is_finite() || !y.is_finite() || !dt_seconds.is_finite() || dt_seconds <= 0.0 {
            return;
        }
        let k = self.config.stick_turns_per_second * dt_seconds;
        let y = if self.config.invert_y { -y } else { y };
        self.turn(x.clamp(-1.0, 1.0) * k, y.clamp(-1.0, 1.0) * k);
    }

    fn turn(&mut self, dyaw: f32, dpitch: f32) {
        self.yaw = (self.yaw + dyaw).rem_euclid(1.0);
        self.pitch = self.clamp_pitch(self.pitch + dpitch);
    }

    fn clamp_pitch(&self, p: f32) -> f32 {
        let (lo, hi) = (
            self.config.pitch_min.min(self.config.pitch_max),
            self.config.pitch_max.max(self.config.pitch_min),
        );
        p.clamp(lo, hi)
    }

    /// The quantized look the simulation receives.
    pub fn sample(&self) -> LookSample {
        LookSample {
            yaw: angle_from_turns(self.yaw),
            pitch: angle_from_turns(self.pitch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_turns_and_wraps() {
        let mut c = CameraRig::new(LookConfig {
            mouse_turns_per_count: 0.001,
            ..LookConfig::default()
        });
        c.apply_mouse(250.0, 0.0);
        assert!((c.yaw_turns() - 0.25).abs() < 1e-6);
        c.apply_mouse(-500.0, 0.0);
        assert!((c.yaw_turns() - 0.75).abs() < 1e-6);
        assert_eq!(c.sample().yaw, Angle16(49_152));
    }

    #[test]
    fn pitch_is_clamped_and_invertible() {
        let mut c = CameraRig::new(LookConfig {
            mouse_turns_per_count: 0.001,
            ..LookConfig::default()
        });
        c.apply_mouse(0.0, -100.0); // mouse up looks up
        assert!((c.pitch_turns() - 0.1).abs() < 1e-6);
        c.apply_mouse(0.0, -10_000.0);
        assert_eq!(c.pitch_turns(), 0.24);
        c.set_config(LookConfig {
            mouse_turns_per_count: 0.001,
            invert_y: true,
            ..LookConfig::default()
        });
        c.apply_mouse(0.0, -100.0); // inverted: mouse up looks down
        assert!((c.pitch_turns() - 0.14).abs() < 1e-6);
    }

    #[test]
    fn stick_look_scales_with_frame_time() {
        let mut a = CameraRig::new(LookConfig::default());
        let mut b = CameraRig::new(LookConfig::default());
        for _ in 0..4 {
            a.apply_stick(1.0, 0.0, 0.25);
        }
        for _ in 0..100 {
            b.apply_stick(1.0, 0.0, 0.01);
        }
        assert!((a.yaw_turns() - 0.5).abs() < 1e-5);
        assert!((b.yaw_turns() - 0.5).abs() < 1e-4);
    }

    #[test]
    fn non_finite_input_is_ignored() {
        let mut c = CameraRig::new(LookConfig::default());
        c.apply_mouse(f32::NAN, 1.0);
        c.apply_stick(f32::INFINITY, 0.0, 0.1);
        c.set(f32::NAN, f32::NAN);
        assert_eq!((c.yaw_turns(), c.pitch_turns()), (0.0, 0.0));
    }
}
