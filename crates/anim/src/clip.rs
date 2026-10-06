//! Runtime clips: keyframe sampling (step or linear, shortest-path slerp for rotations),
//! looping wrap versus clamping, and playback stepping.

use glam::{Quat, Vec3};
use mantis_formats::anim_clip::{Channel, ClipAsset, Interpolation, TrackDef};

use crate::error::AnimError;
use crate::transform::{Transform, normalize_or_identity, slerp_shortest};

/// Loop wraps counted per step at most (a larger step is not meaningful playback).
pub const MAX_WRAPS: i32 = 64;
/// [`MAX_WRAPS`] as a float.
const MAX_WRAPS_F32: f32 = 64.0;

#[derive(Clone, Debug)]
struct Track {
    bone: usize,
    channel: Channel,
    interpolation: Interpolation,
    times: Vec<f32>,
    values: Vec<f32>,
}

impl Track {
    fn new(def: &TrackDef) -> Self {
        Self {
            bone: usize::from(def.bone),
            channel: def.channel,
            interpolation: def.interpolation,
            times: def.times.clone(),
            values: def.values.clone(),
        }
    }

    fn vec3(&self, key: usize) -> Vec3 {
        match self.values.get(key * 3..key * 3 + 3) {
            Some([x, y, z]) => Vec3::new(*x, *y, *z),
            _ => Vec3::ZERO,
        }
    }

    fn quat(&self, key: usize) -> Quat {
        match self.values.get(key * 4..key * 4 + 4) {
            Some([x, y, z, w]) => normalize_or_identity(Quat::from_xyzw(*x, *y, *z, *w)),
            _ => Quat::IDENTITY,
        }
    }

    /// `(key a, key b, fraction)`: the value at `t` is key `a` blended toward key `b`.
    fn locate(&self, t: f32) -> (usize, usize, f32) {
        let after = self.times.partition_point(|k| *k <= t);
        let a = after.saturating_sub(1);
        let b = after.min(self.times.len().saturating_sub(1));
        if a == b || self.interpolation == Interpolation::Step || after == 0 {
            return (a, a, 0.0);
        }
        match (self.times.get(a), self.times.get(b)) {
            (Some(ta), Some(tb)) if tb > ta => (a, b, ((t - ta) / (tb - ta)).clamp(0.0, 1.0)),
            _ => (a, a, 0.0),
        }
    }

    fn apply(&self, t: f32, out: &mut Transform) {
        let (a, b, f) = self.locate(t);
        match self.channel {
            Channel::Translation => out.translation = self.vec3(a).lerp(self.vec3(b), f),
            Channel::Scale => out.scale = self.vec3(a).lerp(self.vec3(b), f),
            Channel::Rotation => {
                out.rotation = if a == b {
                    self.quat(a)
                } else {
                    slerp_shortest(self.quat(a), self.quat(b), f)
                };
            }
        }
    }
}

/// A validated clip ready for sampling.
#[derive(Clone, Debug)]
pub struct Clip {
    duration: f32,
    sample_rate: f32,
    looping: bool,
    root_motion: bool,
    bone_count: usize,
    tracks: Vec<Track>,
}

impl Clip {
    /// Builds a clip from a parsed (or in-memory) asset.
    ///
    /// # Errors
    /// [`AnimError::Format`] when the asset breaks a format rule.
    pub fn new(asset: &ClipAsset) -> Result<Self, AnimError> {
        asset.validate()?;
        Ok(Self {
            duration: asset.duration,
            sample_rate: asset.sample_rate,
            looping: asset.looping,
            root_motion: asset.root_motion,
            bone_count: asset.bone_count as usize,
            tracks: asset.tracks.iter().map(Track::new).collect(),
        })
    }

    /// Length in seconds.
    pub fn duration(&self) -> f32 {
        self.duration
    }

    /// Authored keys per second.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Whether playback wraps.
    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// Whether bone 0 carries root motion.
    pub fn has_root_motion(&self) -> bool {
        self.root_motion
    }

    /// Bone count of the target skeleton.
    pub fn bone_count(&self) -> usize {
        self.bone_count
    }

    /// Maps a playback time into the clip: wrapped into `[0, duration)` when looping,
    /// clamped to `[0, duration]` otherwise. Non-finite times map to 0.
    pub fn local_time(&self, time: f32) -> f32 {
        if !time.is_finite() {
            return 0.0;
        }
        if self.looping {
            let t = time.rem_euclid(self.duration);
            if t < self.duration { t } else { 0.0 }
        } else {
            time.clamp(0.0, self.duration)
        }
    }

    /// Advances playback from `time` (already local) by `delta` seconds: the new local
    /// time and the number of loop wraps crossed (negative when playing backwards; always
    /// 0 for a clamped clip), at most [`MAX_WRAPS`] either way.
    pub fn step(&self, time: f32, delta: f32) -> (f32, i32) {
        let raw = time + if delta.is_finite() { delta } else { 0.0 };
        let new = self.local_time(raw);
        if !self.looping {
            return (new, 0);
        }
        (new, wraps_between(raw - new, self.duration))
    }

    /// Samples every bone at `time` (mapped by [`Clip::local_time`]) into `out`. Channels
    /// without a track take the bind pose. Allocates nothing.
    pub fn sample(&self, bind: &[Transform], time: f32, out: &mut [Transform]) {
        for (o, b) in out.iter_mut().zip(bind) {
            *o = *b;
        }
        let t = self.local_time(time);
        for track in &self.tracks {
            if let Some(o) = out.get_mut(track.bone) {
                track.apply(t, o);
            }
        }
    }

    /// Samples one bone at a time clamped to `[0, duration]` without wrapping (so the
    /// clip's end can be sampled exactly), starting from that bone's bind transform.
    pub fn sample_bone(&self, bind: Transform, bone: usize, time: f32) -> Transform {
        let t = if time.is_finite() {
            time.clamp(0.0, self.duration)
        } else {
            0.0
        };
        let mut out = bind;
        for track in self.tracks.iter().filter(|tr| tr.bone == bone) {
            track.apply(t, &mut out);
        }
        out
    }
}

/// Whole loops in `distance` seconds of a loop `duration` long, clamped to
/// [`MAX_WRAPS`].
#[expect(clippy::cast_possible_truncation)] // Rounded and clamped to +-MAX_WRAPS first.
fn wraps_between(distance: f32, duration: f32) -> i32 {
    let w = (distance / duration).round();
    if w.is_finite() {
        w.clamp(-MAX_WRAPS_F32, MAX_WRAPS_F32) as i32
    } else {
        0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::skeleton::tests::{TestResult, leg};
    use core::f32::consts::FRAC_PI_2;
    use mantis_formats::anim_clip::TrackDef;

    pub(crate) fn track(bone: u16, channel: Channel, times: &[f32], values: &[f32]) -> TrackDef {
        TrackDef {
            bone,
            channel,
            interpolation: Interpolation::Linear,
            times: times.to_vec(),
            values: values.to_vec(),
        }
    }

    pub(crate) fn clip_asset(duration: f32, looping: bool, tracks: Vec<TrackDef>) -> ClipAsset {
        ClipAsset {
            duration,
            sample_rate: 30.0,
            looping,
            root_motion: false,
            bone_count: 4,
            tracks,
        }
    }

    /// The knee bends from 0 to 90 degrees about x over one second.
    fn bend(looping: bool) -> Result<Clip, AnimError> {
        let q = Quat::from_rotation_x(FRAC_PI_2);
        Clip::new(&clip_asset(
            1.0,
            looping,
            vec![track(
                2,
                Channel::Rotation,
                &[0.0, 1.0],
                &[0.0, 0.0, 0.0, 1.0, q.x, q.y, q.z, q.w],
            )],
        ))
    }

    #[test]
    fn linear_rotation_is_slerp() -> TestResult {
        let s = leg()?;
        let c = bend(false)?;
        let mut out = vec![Transform::IDENTITY; 4];
        c.sample(s.bind_pose(), 0.5, &mut out);
        let knee = out.get(2).ok_or("knee")?;
        assert!(
            knee.rotation
                .angle_between(Quat::from_rotation_x(FRAC_PI_2 / 2.0))
                < 1e-5
        );
        assert_eq!(
            knee.translation,
            Vec3::new(0.0, -1.0, 0.0),
            "untracked channel keeps bind"
        );
        assert_eq!(out.get(1), s.bind_pose().get(1), "untracked bone keeps bind");
        Ok(())
    }

    #[test]
    fn rotation_keys_in_opposite_hemispheres_take_the_short_way() -> TestResult {
        let q = -Quat::from_rotation_y(0.2);
        let c = Clip::new(&clip_asset(
            1.0,
            false,
            vec![track(
                0,
                Channel::Rotation,
                &[0.0, 1.0],
                &[0.0, 0.0, 0.0, 1.0, q.x, q.y, q.z, q.w],
            )],
        ))?;
        let mid = c.sample_bone(Transform::IDENTITY, 0, 0.5);
        assert!(mid.rotation.angle_between(Quat::from_rotation_y(0.1)) < 1e-5);
        Ok(())
    }

    #[test]
    fn step_interpolation_holds_keys() -> TestResult {
        let mut t = track(
            1,
            Channel::Translation,
            &[0.0, 0.5, 1.0],
            &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0],
        );
        t.interpolation = Interpolation::Step;
        let c = Clip::new(&clip_asset(1.0, false, vec![t]))?;
        let x = |time: f32| c.sample_bone(Transform::IDENTITY, 1, time).translation.x;
        assert_eq!(
            [x(0.0), x(0.49), x(0.5), x(0.99), x(1.0)],
            [0.0, 0.0, 1.0, 1.0, 2.0]
        );
        Ok(())
    }

    #[test]
    fn keys_hold_outside_their_range() -> TestResult {
        let c = Clip::new(&clip_asset(
            2.0,
            false,
            vec![track(
                1,
                Channel::Scale,
                &[0.5, 1.5],
                &[1.0, 1.0, 1.0, 3.0, 3.0, 3.0],
            )],
        ))?;
        let s = |time: f32| c.sample_bone(Transform::IDENTITY, 1, time).scale.x;
        assert_eq!(
            [s(0.0), s(0.5), s(1.0), s(1.5), s(2.0)],
            [1.0, 1.0, 2.0, 3.0, 3.0]
        );
        Ok(())
    }

    #[test]
    fn looping_wraps_and_clamped_clamps() -> TestResult {
        let looping = bend(true)?;
        let clamped = bend(false)?;
        assert!((looping.local_time(1.25) - 0.25).abs() < 1e-6);
        assert!((looping.local_time(-0.25) - 0.75).abs() < 1e-6);
        assert_eq!(looping.local_time(1.0), 0.0);
        assert_eq!(clamped.local_time(1.25), 1.0);
        assert_eq!(clamped.local_time(-1.0), 0.0);
        assert_eq!(clamped.local_time(f32::NAN), 0.0);
        assert_eq!(looping.step(0.75, 0.5), (0.25, 1));
        assert_eq!(looping.step(0.25, -0.5), (0.75, -1));
        assert_eq!(looping.step(0.5, 2.25).1, 2);
        assert_eq!(clamped.step(0.75, 0.5), (1.0, 0));
        assert_eq!(looping.step(0.0, 1e9).1, MAX_WRAPS);
        assert_eq!(f64::from(MAX_WRAPS), f64::from(MAX_WRAPS_F32));
        let s = leg()?;
        let mut a = vec![Transform::IDENTITY; 4];
        let mut b = vec![Transform::IDENTITY; 4];
        looping.sample(s.bind_pose(), 0.25, &mut a);
        looping.sample(s.bind_pose(), 3.25, &mut b);
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn accessors() -> TestResult {
        let c = bend(true)?;
        assert_eq!((c.duration(), c.sample_rate(), c.is_looping()), (1.0, 30.0, true));
        assert!(!c.has_root_motion());
        assert_eq!(c.bone_count(), 4);
        let mut bad = clip_asset(1.0, true, vec![]);
        bad.duration = -1.0;
        assert!(matches!(Clip::new(&bad), Err(AnimError::Format(_))));
        Ok(())
    }
}
