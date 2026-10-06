//! Animation clip v1: keyframed bone channels for one skeleton, produced by the cook and
//! sampled by the animation runtime.
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); bone channels are parent-local in that frame.
//!
//! Layout (little-endian):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MCLP"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 looping, bit 1 has root motion; other bits 0 |
//! | 8 | 4 | duration `f32` seconds, finite, positive |
//! | 12 | 4 | sample rate `f32` (authored keys per second, informational), finite, positive |
//! | 16 | 4 | bone count `u32` of the target skeleton, 1 to [`MAX_BONES`] |
//! | 20 | 4 | track count `u32`, 0 to `3 * bone_count` |
//! | 24 | 4 | reserved, 0 |
//! | 28 | | tracks |
//!
//! Each track:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 2 | bone `u16`, less than the bone count |
//! | 2 | 1 | channel `u8`: 0 translation, 1 rotation, 2 scale |
//! | 3 | 1 | interpolation `u8`: 0 step, 1 linear |
//! | 4 | 4 | key count `u32`, 1 to [`MAX_KEYS`] |
//! | 8 | `4 * keys` | key times `f32`, strictly increasing, within `[0, duration]` |
//! | | `4 * width * keys` | values: width 3 (`x, y, z`) for translation and scale, 4 (`x, y, z, w`) for rotation |
//!
//! Tracks are sorted by `(bone, channel)` strictly, so each channel of each bone has at
//! most one track. Rotation keys are unit quaternions within
//! [`crate::skeleton::UNIT_TOLERANCE`]. A channel without a track holds the skeleton's
//! bind pose. A clip with root motion has a translation or rotation track on bone 0 (the
//! root); extracting the motion is the runtime's job. Every `f32` must be finite, and the
//! length must match exactly.

use crate::bytes::{FormatError, Reader, Writer};
use crate::skeleton::{MAX_BONES, is_unit_quaternion};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MCLP";
/// Most keys per track.
pub const MAX_KEYS: u32 = 1 << 16;
/// Flag: the clip loops.
pub const FLAG_LOOPING: u16 = 1 << 0;
/// Flag: the clip carries root motion on bone 0.
pub const FLAG_ROOT_MOTION: u16 = 1 << 1;

/// Which part of a bone's local transform a track animates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// Translation, 3 floats per key.
    Translation,
    /// Rotation quaternion `x, y, z, w`, 4 floats per key.
    Rotation,
    /// Scale, 3 floats per key.
    Scale,
}

impl Channel {
    /// Floats per key.
    pub fn width(self) -> usize {
        match self {
            Channel::Rotation => 4,
            Channel::Translation | Channel::Scale => 3,
        }
    }

    /// The on-disk value.
    pub fn to_u8(self) -> u8 {
        match self {
            Channel::Translation => 0,
            Channel::Rotation => 1,
            Channel::Scale => 2,
        }
    }

    /// Decodes an on-disk value.
    ///
    /// # Errors
    /// [`FormatError::Encoding`] for an unknown value.
    pub fn from_u8(v: u8) -> Result<Self, FormatError> {
        match v {
            0 => Ok(Channel::Translation),
            1 => Ok(Channel::Rotation),
            2 => Ok(Channel::Scale),
            _ => Err(FormatError::Encoding(u32::from(v))),
        }
    }
}

/// How values between keys are computed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Interpolation {
    /// Hold the previous key.
    Step,
    /// Linear (spherical along the shortest path for rotations).
    Linear,
}

impl Interpolation {
    /// The on-disk value.
    pub fn to_u8(self) -> u8 {
        match self {
            Interpolation::Step => 0,
            Interpolation::Linear => 1,
        }
    }

    /// Decodes an on-disk value.
    ///
    /// # Errors
    /// [`FormatError::Encoding`] for an unknown value.
    pub fn from_u8(v: u8) -> Result<Self, FormatError> {
        match v {
            0 => Ok(Interpolation::Step),
            1 => Ok(Interpolation::Linear),
            _ => Err(FormatError::Encoding(u32::from(v))),
        }
    }
}

/// One keyframed channel.
#[derive(Clone, PartialEq, Debug)]
pub struct TrackDef {
    /// Target bone index.
    pub bone: u16,
    /// Animated channel.
    pub channel: Channel,
    /// Interpolation between keys.
    pub interpolation: Interpolation,
    /// Key times in seconds, strictly increasing within `[0, duration]`.
    pub times: Vec<f32>,
    /// `channel.width()` floats per key, key-major.
    pub values: Vec<f32>,
}

/// A clip asset.
#[derive(Clone, PartialEq, Debug)]
pub struct ClipAsset {
    /// Length in seconds.
    pub duration: f32,
    /// Authored keys per second (informational).
    pub sample_rate: f32,
    /// Whether playback wraps.
    pub looping: bool,
    /// Whether bone 0's motion is extracted as root motion.
    pub root_motion: bool,
    /// Bone count of the skeleton this clip targets.
    pub bone_count: u32,
    /// Tracks sorted by `(bone, channel)`.
    pub tracks: Vec<TrackDef>,
}

impl ClipAsset {
    /// Checks every rule of the format on an in-memory clip (the parser runs it too).
    ///
    /// # Errors
    /// [`FormatError::Keyframes`] for a bad duration, sample rate, key count, or key time;
    /// [`FormatError::Dimensions`] for counts out of range; [`FormatError::Inconsistent`]
    /// for a bone out of range, unsorted or duplicate tracks, a value count that does not
    /// match the key count, or root motion without a root track;
    /// [`FormatError::NonFinite`] and [`FormatError::Geometry`] (non-unit rotation key)
    /// for bad values.
    pub fn validate(&self) -> Result<(), FormatError> {
        if !positive(self.duration) || !positive(self.sample_rate) {
            return Err(FormatError::Keyframes);
        }
        if self.bone_count == 0 || self.bone_count > MAX_BONES {
            return Err(FormatError::Dimensions);
        }
        if self.tracks.len() > 3 * self.bone_count as usize {
            return Err(FormatError::Dimensions);
        }
        let mut previous: Option<(u16, Channel)> = None;
        for track in &self.tracks {
            if u32::from(track.bone) >= self.bone_count {
                return Err(FormatError::Inconsistent);
            }
            let key = (track.bone, track.channel);
            if previous.is_some_and(|p| p >= key) {
                return Err(FormatError::Inconsistent);
            }
            previous = Some(key);
            self.validate_track(track)?;
        }
        if self.root_motion
            && !self
                .tracks
                .iter()
                .any(|t| t.bone == 0 && t.channel != Channel::Scale)
        {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    fn validate_track(&self, track: &TrackDef) -> Result<(), FormatError> {
        if track.times.is_empty() || track.times.len() > MAX_KEYS as usize {
            return Err(FormatError::Keyframes);
        }
        if track.values.len() != track.times.len() * track.channel.width() {
            return Err(FormatError::Inconsistent);
        }
        if track.times.iter().chain(&track.values).any(|v| !v.is_finite()) {
            return Err(FormatError::NonFinite);
        }
        let in_range = track.times.iter().all(|t| (0.0..=self.duration).contains(t));
        let increasing = track.times.windows(2).all(|w| w.first() < w.get(1));
        if !in_range || !increasing {
            return Err(FormatError::Keyframes);
        }
        if track.channel == Channel::Rotation
            && !track
                .values
                .as_chunks::<4>()
                .0
                .iter()
                .all(|q| is_unit_quaternion(*q))
        {
            return Err(FormatError::Geometry);
        }
        Ok(())
    }

    /// Parses and validates a clip.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<ClipAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags & !(FLAG_LOOPING | FLAG_ROOT_MOTION) != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let duration = r.f32()?;
        let sample_rate = r.f32()?;
        if duration <= 0.0 || sample_rate <= 0.0 {
            return Err(FormatError::Keyframes);
        }
        let bone_count = r.u32()?;
        let track_count = r.u32()?;
        if bone_count == 0 || bone_count > MAX_BONES || track_count > 3 * bone_count {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let mut tracks = Vec::with_capacity(track_count as usize);
        for _ in 0..track_count {
            tracks.push(read_track(&mut r)?);
        }
        r.finish()?;
        let clip = ClipAsset {
            duration,
            sample_rate,
            looping: flags & FLAG_LOOPING != 0,
            root_motion: flags & FLAG_ROOT_MOTION != 0,
            bone_count,
            tracks,
        };
        clip.validate()?;
        Ok(clip)
    }

    /// Reference encoder (the exact inverse of [`ClipAsset::parse`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        let mut flags = 0;
        if self.looping {
            flags |= FLAG_LOOPING;
        }
        if self.root_motion {
            flags |= FLAG_ROOT_MOTION;
        }
        w.u16(flags);
        w.f32(self.duration);
        w.f32(self.sample_rate);
        w.u32(self.bone_count);
        w.count(self.tracks.len());
        w.u32(0);
        for track in &self.tracks {
            w.u16(track.bone);
            w.u8(track.channel.to_u8());
            w.u8(track.interpolation.to_u8());
            w.count(track.times.len());
            for t in track.times.iter().chain(&track.values) {
                w.f32(*t);
            }
        }
        w.into_bytes()
    }
}

fn positive(v: f32) -> bool {
    v.is_finite() && v > 0.0
}

fn read_track(r: &mut Reader<'_>) -> Result<TrackDef, FormatError> {
    let bone = r.u16()?;
    let channel = Channel::from_u8(r.u8()?)?;
    let interpolation = Interpolation::from_u8(r.u8()?)?;
    let keys = r.u32()?;
    if keys == 0 || keys > MAX_KEYS {
        return Err(FormatError::Keyframes);
    }
    let keys = keys as usize;
    let floats = keys * (1 + channel.width());
    if r.remaining() < floats * 4 {
        return Err(FormatError::Length {
            expected: (r.position() + floats * 4) as u64,
            actual: (r.position() + r.remaining()) as u64,
        });
    }
    let mut times = Vec::with_capacity(keys);
    for _ in 0..keys {
        times.push(r.f32()?);
    }
    let mut values = Vec::with_capacity(keys * channel.width());
    for _ in 0..keys * channel.width() {
        values.push(r.f32()?);
    }
    Ok(TrackDef {
        bone,
        channel,
        interpolation,
        times,
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Two bones; the root walks along +z and the child turns about y.
    fn walk() -> ClipAsset {
        let half = std::f32::consts::FRAC_1_SQRT_2;
        ClipAsset {
            duration: 1.0,
            sample_rate: 30.0,
            looping: true,
            root_motion: true,
            bone_count: 2,
            tracks: vec![
                TrackDef {
                    bone: 0,
                    channel: Channel::Translation,
                    interpolation: Interpolation::Linear,
                    times: vec![0.0, 1.0],
                    values: vec![0.0, 0.0, 0.0, 0.0, 0.0, 2.0],
                },
                TrackDef {
                    bone: 1,
                    channel: Channel::Rotation,
                    interpolation: Interpolation::Linear,
                    times: vec![0.0, 0.5],
                    values: vec![0.0, 0.0, 0.0, 1.0, 0.0, half, 0.0, half],
                },
                TrackDef {
                    bone: 1,
                    channel: Channel::Scale,
                    interpolation: Interpolation::Step,
                    times: vec![0.25],
                    values: vec![1.0, 2.0, 1.0],
                },
            ],
        }
    }

    // Byte offsets in `walk().encode()`.
    const T0: usize = 28; // track 0 header
    const T1: usize = T0 + 8 + 4 * 2 + 12 * 2; // track 1 header (rotation)
    const T2: usize = T1 + 8 + 4 * 2 + 16 * 2; // track 2 header (scale)

    #[test]
    fn round_trips() -> TestResult {
        let c = walk();
        let bytes = c.encode();
        assert_eq!(bytes.len(), T2 + 8 + 4 + 12);
        let back = ClipAsset::parse(&bytes)?;
        assert_eq!(back, c);
        assert_eq!(back.encode(), bytes);
        let mut plain = c.clone();
        plain.looping = false;
        plain.root_motion = false;
        plain.tracks.clear();
        assert_eq!(ClipAsset::parse(&plain.encode())?, plain);
        Ok(())
    }

    #[test]
    fn rejects_every_malformed_header_field() {
        let good = walk().encode();
        let corrupt = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            if let Some(s) = b.get_mut(at..at + bytes.len()) {
                s.copy_from_slice(bytes);
            }
            ClipAsset::parse(&b)
        };
        assert_eq!(corrupt(0, b"MCLX").err(), Some(FormatError::Magic));
        assert_eq!(
            corrupt(4, &0u16.to_le_bytes()).err(),
            Some(FormatError::Version(0))
        );
        assert_eq!(corrupt(6, &4u16.to_le_bytes()).err(), Some(FormatError::Flags(4)));
        assert_eq!(
            corrupt(8, &0.0f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(8, &f32::NAN.to_le_bytes()).err(),
            Some(FormatError::NonFinite)
        );
        assert_eq!(
            corrupt(12, &(-30.0f32).to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(16, &0u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(16, &1025u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(20, &7u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(24, &1u32.to_le_bytes()).err(),
            Some(FormatError::Reserved)
        );
        assert!(matches!(
            corrupt(20, &2u32.to_le_bytes()),
            Err(FormatError::Length { .. })
        ));
        assert!(matches!(
            corrupt(20, &4u32.to_le_bytes()),
            Err(FormatError::Length { .. })
        ));
        assert!(matches!(
            ClipAsset::parse(good.get(..good.len() - 1).unwrap_or(&[])),
            Err(FormatError::Length { .. })
        ));
        let mut longer = good.clone();
        longer.extend_from_slice(&[0; 4]);
        assert!(matches!(
            ClipAsset::parse(&longer),
            Err(FormatError::Length { .. })
        ));
    }

    #[test]
    fn rejects_every_malformed_track_field() {
        let good = walk().encode();
        let corrupt = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            if let Some(s) = b.get_mut(at..at + bytes.len()) {
                s.copy_from_slice(bytes);
            }
            ClipAsset::parse(&b)
        };
        assert_eq!(
            corrupt(T0, &2u16.to_le_bytes()).err(),
            Some(FormatError::Inconsistent)
        );
        assert_eq!(
            corrupt(T2, &0u16.to_le_bytes()).err(),
            Some(FormatError::Inconsistent),
            "unsorted"
        );
        assert_eq!(corrupt(T1 + 2, &[3]).err(), Some(FormatError::Encoding(3)));
        assert_eq!(
            corrupt(T2 + 2, &[0]).err(),
            Some(FormatError::Inconsistent),
            "after rotation"
        );
        assert_eq!(
            corrupt(T0 + 2, &[2]).err(),
            Some(FormatError::Inconsistent),
            "root motion with only a root scale track"
        );
        assert_eq!(corrupt(T1 + 3, &[2]).err(), Some(FormatError::Encoding(2)));
        assert_eq!(
            corrupt(T2 + 4, &0u32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(T2 + 4, &(MAX_KEYS + 1).to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert!(matches!(
            corrupt(T2 + 4, &2u32.to_le_bytes()),
            Err(FormatError::Length { .. })
        ));
        // Key times: decreasing, equal, negative, beyond the duration.
        assert_eq!(
            corrupt(T1 + 8, &0.5f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(T1 + 8, &0.6f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(T1 + 8, &(-0.1f32).to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(T1 + 12, &1.5f32.to_le_bytes()).err(),
            Some(FormatError::Keyframes)
        );
        assert_eq!(
            corrupt(T0 + 16, &f32::INFINITY.to_le_bytes()).err(),
            Some(FormatError::NonFinite)
        );
        assert_eq!(
            corrupt(T1 + 16 + 12, &0.5f32.to_le_bytes()).err(),
            Some(FormatError::Geometry),
            "rotation key 0 not unit"
        );
    }

    #[test]
    fn root_motion_needs_a_root_track() {
        let mut c = walk();
        c.tracks.remove(0);
        assert_eq!(c.validate(), Err(FormatError::Inconsistent));
        assert_eq!(
            ClipAsset::parse(&c.encode()).err(),
            Some(FormatError::Inconsistent)
        );
        c.root_motion = false;
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_mismatched_values() {
        let mut c = walk();
        if let Some(t) = c.tracks.get_mut(2) {
            t.values.push(1.0);
        }
        assert_eq!(c.validate(), Err(FormatError::Inconsistent));
    }

    #[test]
    fn channel_and_interpolation_codes_round_trip() -> TestResult {
        for c in [Channel::Translation, Channel::Rotation, Channel::Scale] {
            assert_eq!(Channel::from_u8(c.to_u8())?, c);
        }
        for i in [Interpolation::Step, Interpolation::Linear] {
            assert_eq!(Interpolation::from_u8(i.to_u8())?, i);
        }
        Ok(())
    }

    #[test]
    fn no_corruption_or_truncation_panics() {
        let bytes = walk().encode();
        for i in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                if let Some(x) = b.get_mut(i) {
                    *x ^= mask;
                }
                if let Ok(c) = ClipAsset::parse(&b) {
                    assert!(ClipAsset::parse(&c.encode()).is_ok());
                }
            }
        }
        for len in 0..bytes.len() {
            assert!(
                ClipAsset::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
                "truncated to {len}"
            );
        }
    }
}
