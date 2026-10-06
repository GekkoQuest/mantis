//! Root-motion extraction.
//!
//! Convention: +Y is up, and bone 0 is the root. A clip with root motion describes the
//! character's travel with the root bone's horizontal (XZ) translation and its yaw (twist
//! about +Y). Each update extracts the change of both since the previous update and
//! removes them from the pose, so the root bone stays at the model origin in XZ facing
//! yaw 0, keeping its vertical translation and its pitch and roll. The game applies the
//! reported [`RootMotion`] to the character's transform:
//! `position += heading * motion.translation; heading = heading * motion.rotation`.
//!
//! The delta is expressed in the character's frame at the start of the update. Each clip
//! node computes its own delta over its own playback interval (handling loop wraps), and
//! blends combine the deltas with the same weights as the poses. Blending deltas rather
//! than differencing the blended pose is what makes a wrap correct: two blended clips
//! wrap at different moments, and only per-clip deltas see each wrap.

use glam::{Quat, Vec3};

use crate::clip::Clip;
use crate::transform::Transform;

/// Most sub-steps one delta is integrated over (a long segment is subdivided at the
/// clip's sample rate so curved paths and turns over 180 degrees integrate correctly).
pub const MAX_SUBSTEPS: u16 = 256;

/// Root motion for one update, in the character's frame at the start of the update.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RootMotion {
    /// Horizontal travel (`y` is always 0).
    pub translation: Vec3,
    /// Turn about +Y.
    pub rotation: Quat,
}

impl RootMotion {
    /// No motion.
    pub const IDENTITY: RootMotion = RootMotion {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
    };
}

impl Default for RootMotion {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// A root-motion delta in blendable form: horizontal translation and a yaw angle.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct RootDelta {
    /// Horizontal travel in the start frame.
    pub translation: Vec3,
    /// Turn about +Y in radians.
    pub yaw: f32,
}

impl RootDelta {
    /// No motion.
    pub const ZERO: RootDelta = RootDelta {
        translation: Vec3::ZERO,
        yaw: 0.0,
    };

    /// This delta followed by `next` (whose translation is in the frame this one ends in).
    #[must_use]
    pub fn then(self, next: RootDelta) -> RootDelta {
        RootDelta {
            translation: self.translation + Quat::from_rotation_y(self.yaw) * next.translation,
            yaw: self.yaw + next.yaw,
        }
    }

    /// Linear blend toward `other`.
    #[must_use]
    pub fn lerp(self, other: RootDelta, t: f32) -> RootDelta {
        RootDelta {
            translation: self.translation.lerp(other.translation, t),
            yaw: self.yaw + (other.yaw - self.yaw) * t,
        }
    }

    /// The delta as a translation and rotation.
    pub fn to_motion(self) -> RootMotion {
        RootMotion {
            translation: self.translation,
            rotation: Quat::from_rotation_y(self.yaw),
        }
    }
}

/// The yaw (twist about +Y) of a rotation, in `(-pi, pi]`.
pub fn yaw_of(q: Quat) -> f32 {
    wrap_angle(2.0 * q.y.atan2(q.w))
}

/// An angle wrapped into `(-pi, pi]`.
pub fn wrap_angle(a: f32) -> f32 {
    use core::f32::consts::{PI, TAU};
    let w = (a + PI).rem_euclid(TAU) - PI;
    if w <= -PI { w + TAU } else { w }
}

/// Removes the horizontal translation and the yaw from a root bone's local transform.
pub fn strip_root_motion(root: &mut Transform) {
    let yaw = yaw_of(root.rotation);
    root.rotation = (Quat::from_rotation_y(-yaw) * root.rotation).normalize();
    root.translation.x = 0.0;
    root.translation.z = 0.0;
}

/// The delta between two root samples, in the frame of the first.
fn segment(a: &Transform, b: &Transform) -> RootDelta {
    let yaw_a = yaw_of(a.rotation);
    let mut travel = b.translation - a.translation;
    travel.y = 0.0;
    RootDelta {
        translation: Quat::from_rotation_y(-yaw_a) * travel,
        yaw: wrap_angle(yaw_of(b.rotation) - yaw_a),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to 1..=MAX_SUBSTEPS first.
fn substeps(seconds: f32, rate: f32) -> u16 {
    let n = (seconds.abs() * rate).ceil();
    if n.is_finite() {
        n.clamp(1.0, f32::from(MAX_SUBSTEPS)) as u16
    } else {
        1
    }
}

impl Clip {
    /// Root motion from clip time `from` to `to` (both within `[0, duration]`, no wrap;
    /// `to < from` plays backwards). `bind_root` is bone 0's bind transform. Zero for a
    /// clip without root motion.
    pub fn root_delta(&self, bind_root: Transform, from: f32, to: f32) -> RootDelta {
        if !self.has_root_motion() || from == to {
            return RootDelta::ZERO;
        }
        let n = substeps(to - from, self.sample_rate());
        let mut acc = RootDelta::ZERO;
        let mut previous = self.sample_bone(bind_root, 0, from);
        for k in 1..=n {
            let t = if k == n {
                to
            } else {
                from + (to - from) * (f32::from(k) / f32::from(n))
            };
            let next = self.sample_bone(bind_root, 0, t);
            acc = acc.then(segment(&previous, &next));
            previous = next;
        }
        acc
    }

    /// Root motion over one playback step from local time `from` to local time `to`
    /// crossing `wraps` loop boundaries (see [`Clip::step`]). Across a forward wrap it is
    /// the motion from `from` to the end, then whole loops, then from the start to `to`;
    /// backwards is the mirror image.
    pub fn root_delta_wrapped(&self, bind_root: Transform, from: f32, wraps: i32, to: f32) -> RootDelta {
        if !self.has_root_motion() {
            return RootDelta::ZERO;
        }
        let end = self.duration();
        let (exit, enter) = match wraps.signum() {
            0 => return self.root_delta(bind_root, from, to),
            1 => (end, 0.0),
            _ => (0.0, end),
        };
        let mut acc = self.root_delta(bind_root, from, exit);
        if wraps.unsigned_abs() > 1 {
            let full = self.root_delta(bind_root, enter, exit);
            for _ in 1..wraps.unsigned_abs() {
                acc = acc.then(full);
            }
        }
        acc.then(self.root_delta(bind_root, enter, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::tests::{clip_asset, track};
    use crate::skeleton::tests::TestResult;
    use core::f32::consts::{FRAC_PI_2, PI};
    use mantis_formats::anim_clip::{Channel, ClipAsset};

    fn walking(looping: bool) -> ClipAsset {
        // The root walks 2 units along +z over 1 second.
        let mut a = clip_asset(
            1.0,
            looping,
            vec![track(
                0,
                Channel::Translation,
                &[0.0, 1.0],
                &[0.0, 0.5, 0.0, 0.0, 0.5, 2.0],
            )],
        );
        a.root_motion = true;
        a
    }

    fn turning() -> ClipAsset {
        // The root turns 90 degrees left while walking 1 unit forward along its heading.
        let q = Quat::from_rotation_y(FRAC_PI_2);
        let mut a = clip_asset(
            1.0,
            true,
            vec![
                track(
                    0,
                    Channel::Translation,
                    &[0.0, 1.0],
                    &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                ),
                track(
                    0,
                    Channel::Rotation,
                    &[0.0, 1.0],
                    &[0.0, 0.0, 0.0, 1.0, q.x, q.y, q.z, q.w],
                ),
            ],
        );
        a.root_motion = true;
        a
    }

    #[test]
    fn wrap_angle_range() {
        assert!((wrap_angle(3.0 * PI) - PI).abs() < 1e-5);
        assert!((wrap_angle(-PI) - PI).abs() < 1e-5);
        assert!((wrap_angle(0.5) - 0.5).abs() < 1e-6);
        assert!((yaw_of(Quat::from_rotation_y(1.0)) - 1.0).abs() < 1e-5);
        assert!((yaw_of(Quat::from_rotation_y(1.0) * Quat::from_rotation_x(0.7)) - 1.0).abs() < 1e-5);
        assert!(yaw_of(Quat::from_rotation_x(-FRAC_PI_2)).abs() < 1e-6);
    }

    #[test]
    fn delta_within_a_loop() -> TestResult {
        let c = Clip::new(&walking(true))?;
        let d = c.root_delta(Transform::IDENTITY, 0.25, 0.5);
        assert!(d.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 0.5), 1e-5), "{d:?}");
        assert!(d.yaw.abs() < 1e-6);
        Ok(())
    }

    #[test]
    fn delta_across_the_wrap_is_end_minus_prev_plus_start_to_current() -> TestResult {
        let c = Clip::new(&walking(true))?;
        let (to, wraps) = c.step(0.9, 0.2);
        assert_eq!(wraps, 1);
        let d = c.root_delta_wrapped(Transform::IDENTITY, 0.9, wraps, to);
        // (2.0 - 1.8) + (0.2 - 0.0) = 0.4 along z.
        assert!(d.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 0.4), 1e-5), "{d:?}");
        // Three wraps (two whole loops between): 0.2 + 4 + 0.2.
        let (to, wraps) = c.step(0.9, 2.2);
        assert_eq!(wraps, 3);
        let d = c.root_delta_wrapped(Transform::IDENTITY, 0.9, wraps, to);
        assert!(d.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 4.4), 1e-4), "{d:?}");
        // Backwards across the start: -(0.1 * 2) - (0.1 * 2).
        let (to, wraps) = c.step(0.1, -0.2);
        assert_eq!(wraps, -1);
        let d = c.root_delta_wrapped(Transform::IDENTITY, 0.1, wraps, to);
        assert!(
            d.translation.abs_diff_eq(Vec3::new(0.0, 0.0, -0.4), 1e-5),
            "{d:?}"
        );
        Ok(())
    }

    #[test]
    fn turning_integrates_in_the_rotating_frame() -> TestResult {
        let c = Clip::new(&turning())?;
        let d = c.root_delta(Transform::IDENTITY, 0.0, 1.0);
        assert!((d.yaw - FRAC_PI_2).abs() < 1e-4, "{d:?}");
        // Walking along +z in clip space while turning: the start frame equals clip space.
        assert!(d.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 1.0), 1e-4), "{d:?}");
        // Composition of halves equals the whole.
        let a = c.root_delta(Transform::IDENTITY, 0.0, 0.5);
        let b = c.root_delta(Transform::IDENTITY, 0.5, 1.0);
        let ab = a.then(b);
        assert!(ab.translation.abs_diff_eq(d.translation, 1e-4) && (ab.yaw - d.yaw).abs() < 1e-5);
        // Four loops turn a full circle: the loop delta is not lost to angle wrapping.
        let (to, wraps) = c.step(0.0, 4.0);
        let full = c.root_delta_wrapped(Transform::IDENTITY, 0.0, wraps, to);
        assert!((full.yaw - 2.0 * PI).abs() < 1e-3, "{full:?}");
        Ok(())
    }

    #[test]
    fn strip_keeps_height_pitch_and_roll() {
        let mut root = Transform::new(
            Vec3::new(3.0, 1.0, -2.0),
            Quat::from_rotation_y(0.8) * Quat::from_rotation_x(0.3),
            Vec3::ONE,
        );
        strip_root_motion(&mut root);
        assert_eq!(root.translation, Vec3::new(0.0, 1.0, 0.0));
        assert!(root.rotation.angle_between(Quat::from_rotation_x(0.3)) < 1e-5);
    }

    #[test]
    fn no_root_motion_means_zero() -> TestResult {
        let mut a = walking(true);
        a.root_motion = false;
        let c = Clip::new(&a)?;
        assert_eq!(
            c.root_delta_wrapped(Transform::IDENTITY, 0.5, 1, 0.2),
            RootDelta::ZERO
        );
        assert_eq!(RootDelta::ZERO.to_motion(), RootMotion::IDENTITY);
        assert_eq!(RootMotion::default(), RootMotion::IDENTITY);
        let half = RootDelta::ZERO.lerp(
            RootDelta {
                translation: Vec3::X,
                yaw: 1.0,
            },
            0.5,
        );
        assert_eq!((half.translation.x, half.yaw), (0.5, 0.5));
        Ok(())
    }
}
