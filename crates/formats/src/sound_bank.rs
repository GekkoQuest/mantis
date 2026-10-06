//! Sound bank v1: PCM clips and the data-defined sounds that play them (plan 8.5).
//!
//! A bank holds every clip at one sample rate and a table of sounds. A sound names one to
//! [`MAX_CLIPS_PER_SOUND`] clips (chosen round-robin or seeded-random per play), its
//! volume, pitch range, flags, output bus, voice-limiting rules, and distance attenuation.
//! The bus id must exist in the mixer graph ([`crate::mixer_graph`]); the runtime checks
//! that when it loads the pair, because a bank alone cannot.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MSBK"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | sample rate `u32`, [`MIN_SAMPLE_RATE`] to [`MAX_SAMPLE_RATE`] |
//! | 12 | 4 | clip count `u32`, at most [`MAX_CLIPS`] |
//! | 16 | 4 | sound count `u32`, at most [`MAX_SOUNDS`] |
//! | 20 | 4 | reserved, 0 |
//! | 24 | 108 each | sound records |
//! | | | clip records |
//!
//! Sound record (108 bytes):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | id `u32`, unique in the bank, stable across cooks |
//! | 4 | 4 | bus id `u32` (a bus of the mixer graph) |
//! | 8 | 4 | volume `f32`, 0 to [`MAX_VOLUME`] |
//! | 12 | 4 | pitch min `f32`, [`MIN_PITCH`] to [`MAX_PITCH`] |
//! | 16 | 4 | pitch max `f32`, [`MIN_PITCH`] to [`MAX_PITCH`], at least pitch min |
//! | 20 | 4 | min distance `f32`, > 0 |
//! | 24 | 4 | max distance `f32`, > min distance |
//! | 28 | 4 | rolloff `f32`, > 0 |
//! | 32 | 2 | flags `u16`: bit 0 looping, bit 1 spatial; bit 2 (streamed, reserved) and every other bit 0 |
//! | 34 | 2 | max instances `u16`, at least 1 |
//! | 36 | 1 | priority `u8` (higher is more important) |
//! | 37 | 1 | steal policy `u8`: 0 oldest, 1 quietest, 2 refuse |
//! | 38 | 1 | clip selection `u8`: 0 round-robin, 1 seeded random |
//! | 39 | 1 | attenuation model `u8`: 0 linear, 1 inverse, 2 exponential |
//! | 40 | 1 | clip count `u8`, 1 to [`MAX_CLIPS_PER_SOUND`] |
//! | 41 | 3 | reserved, 0 |
//! | 44 | 64 | clip indices `[u32; 16]`, each below the bank's clip count; unused slots 0 |
//!
//! Clip record (8-byte header, then data, then padding):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 1 | channels `u8`, 1 or 2 |
//! | 1 | 1 | encoding `u8`: 1 `i16` interleaved, 2 `f32` interleaved (finite, within -1 to 1) |
//! | 2 | 2 | reserved, 0 |
//! | 4 | 4 | frame count `u32`, 1 to [`MAX_CLIP_FRAMES`] |
//! | 8 | | samples, frame count x channels, interleaved |
//! | | | zero padding to the next 4-byte boundary |
//!
//! Attenuation fields are validated for every sound, spatial or not. Every `f32` must be
//! finite. The length must match exactly.
//!
//! # Errors by rule
//!
//! A value out of its range is [`FormatError::Dimensions`]; a distance or rolloff is
//! [`FormatError::Geometry`]; an unknown enumeration byte (encoding, steal policy, clip
//! selection, attenuation model) is [`FormatError::Encoding`]; pitch min above pitch max or
//! a clip index past the clip table is [`FormatError::Inconsistent`]; a repeated sound id
//! is [`FormatError::DuplicateId`].

use crate::bytes::{FormatError, Reader, Writer};

#[cfg(test)]
mod tests;

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MSBK";
/// Lowest bank sample rate.
pub const MIN_SAMPLE_RATE: u32 = 8_000;
/// Highest bank sample rate.
pub const MAX_SAMPLE_RATE: u32 = 192_000;
/// Most clips per bank.
pub const MAX_CLIPS: u32 = 65_536;
/// Most sounds per bank.
pub const MAX_SOUNDS: u32 = 65_536;
/// Most frames per clip.
pub const MAX_CLIP_FRAMES: u32 = 1 << 26;
/// Most clips one sound chooses between.
pub const MAX_CLIPS_PER_SOUND: usize = 16;
/// Highest sound volume.
pub const MAX_VOLUME: f32 = 4.0;
/// Lowest pitch multiplier.
pub const MIN_PITCH: f32 = 0.25;
/// Highest pitch multiplier.
pub const MAX_PITCH: f32 = 4.0;
/// Sound flag: the clip loops until stopped.
pub const SOUND_LOOPING: u16 = 1;
/// Sound flag: the sound is positioned in the world (panned and attenuated).
pub const SOUND_SPATIAL: u16 = 1 << 1;
/// Sound flag reserved for streamed clips; must be 0 in version 1.
pub const SOUND_STREAMED_RESERVED: u16 = 1 << 2;

const HEADER: u64 = 24;
const SOUND_RECORD: u64 = 108;
const CLIP_HEADER: u64 = 8;
const KNOWN_SOUND_FLAGS: u16 = SOUND_LOOPING | SOUND_SPATIAL;

/// PCM samples of one clip, interleaved by channel.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipSamples {
    /// Encoding 1: signed 16-bit; full scale is 32768.
    I16(Vec<i16>),
    /// Encoding 2: 32-bit float, finite, within -1 to 1.
    F32(Vec<f32>),
}

impl ClipSamples {
    /// The encoding byte.
    pub fn encoding(&self) -> u8 {
        match self {
            ClipSamples::I16(_) => 1,
            ClipSamples::F32(_) => 2,
        }
    }

    /// Interleaved sample count.
    pub fn len(&self) -> usize {
        match self {
            ClipSamples::I16(v) => v.len(),
            ClipSamples::F32(v) => v.len(),
        }
    }

    /// True when there are no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every sample as `f32` in -1 to 1 (`i16` divided by 32768).
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            ClipSamples::I16(v) => v.iter().map(|s| f32::from(*s) / 32768.0).collect(),
            ClipSamples::F32(v) => v.clone(),
        }
    }
}

/// One PCM clip.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// 1 (mono) or 2 (stereo, left first).
    pub channels: u8,
    /// Interleaved samples; the length is a multiple of `channels`.
    pub samples: ClipSamples,
}

impl Clip {
    /// Frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }
}

/// How a sound with several clips picks one per play.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipSelection {
    /// In list order, wrapping.
    RoundRobin,
    /// Uniformly at random from the mixer's seeded generator.
    Random,
}

/// What happens when a sound is already playing its maximum number of instances.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StealPolicy {
    /// Stop the instance that started first.
    Oldest,
    /// Stop the instance that is currently least audible.
    Quietest,
    /// Drop the new play.
    Refuse,
}

/// Distance attenuation curve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttenuationModel {
    /// `1 - rolloff * (d - min) / (max - min)`, floored at 0.
    Linear,
    /// `min / (min + rolloff * (d - min))`.
    Inverse,
    /// `(d / min) ^ -rolloff`.
    Exponential,
}

/// Distance attenuation of a spatial sound. The distance is clamped to
/// `min_distance..=max_distance` before the curve is applied, so the gain is 1 inside
/// `min_distance` and constant beyond `max_distance`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Attenuation {
    /// Curve.
    pub model: AttenuationModel,
    /// Distance at and inside which the gain is 1 (> 0).
    pub min_distance: f32,
    /// Distance beyond which the gain stops changing (> `min_distance`).
    pub max_distance: f32,
    /// Curve steepness (> 0).
    pub rolloff: f32,
}

/// One data-defined sound.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    /// Stable id the game posts.
    pub id: u32,
    /// Indices into [`SoundBank::clips`], 1 to [`MAX_CLIPS_PER_SOUND`].
    pub clips: Vec<u32>,
    /// How a clip is picked per play.
    pub selection: ClipSelection,
    /// Linear volume, 0 to [`MAX_VOLUME`].
    pub volume: f32,
    /// Lowest random pitch multiplier.
    pub pitch_min: f32,
    /// Highest random pitch multiplier.
    pub pitch_max: f32,
    /// Loops until stopped.
    pub looping: bool,
    /// Panned and attenuated by position; otherwise played as-is.
    pub spatial: bool,
    /// Output bus id in the mixer graph.
    pub bus: u32,
    /// Voice-stealing priority (higher is more important).
    pub priority: u8,
    /// Most simultaneous instances (at least 1).
    pub max_instances: u16,
    /// What happens when `max_instances` are already playing.
    pub steal: StealPolicy,
    /// Distance attenuation (used when `spatial`).
    pub attenuation: Attenuation,
}

/// A parsed sound bank.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundBank {
    /// Sample rate of every clip.
    pub sample_rate: u32,
    /// Clip table.
    pub clips: Vec<Clip>,
    /// Sound table, ids unique.
    pub sounds: Vec<Sound>,
}

impl SoundBank {
    /// The sound with `id`.
    pub fn sound(&self, id: u32) -> Option<&Sound> {
        self.sounds.iter().find(|s| s.id == id)
    }

    /// Parses and validates a sound bank.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found (see the module docs).
    pub fn parse(bytes: &[u8]) -> Result<SoundBank, FormatError> {
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
        let sample_rate = r.u32()?;
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(FormatError::Dimensions);
        }
        let clip_count = r.u32()?;
        let sound_count = r.u32()?;
        if clip_count > MAX_CLIPS || sound_count > MAX_SOUNDS {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        // Every record has a fixed minimum size, so a lying count fails here, before any
        // table is allocated.
        let least = HEADER + u64::from(sound_count) * SOUND_RECORD + u64::from(clip_count) * CLIP_HEADER;
        if (bytes.len() as u64) < least {
            return Err(FormatError::Length {
                expected: least,
                actual: bytes.len() as u64,
            });
        }
        let mut sounds = Vec::with_capacity(sound_count as usize);
        for _ in 0..sound_count {
            sounds.push(read_sound(&mut r, clip_count)?);
        }
        let mut ids: Vec<u32> = sounds.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        if let Some(w) = ids.windows(2).find(|w| w.first() == w.get(1)) {
            return Err(FormatError::DuplicateId(w.first().copied().unwrap_or(0)));
        }
        let mut clips = Vec::with_capacity(clip_count as usize);
        for _ in 0..clip_count {
            clips.push(read_clip(&mut r)?);
        }
        r.finish()?;
        Ok(SoundBank {
            sample_rate,
            clips,
            sounds,
        })
    }

    /// Checks every rule [`SoundBank::parse`] enforces, for a bank built in memory rather
    /// than parsed (it round-trips through the encoder, so it costs a copy of the bank).
    ///
    /// # Errors
    /// The [`FormatError`] parsing the encoded bank would report.
    pub fn validate(&self) -> Result<(), FormatError> {
        SoundBank::parse(&self.encode()).map(|_| ())
    }

    /// Reference encoder (the exact inverse of [`SoundBank::parse`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.u32(self.sample_rate);
        w.count(self.clips.len());
        w.count(self.sounds.len());
        w.u32(0);
        for s in &self.sounds {
            write_sound(&mut w, s);
        }
        for c in &self.clips {
            write_clip(&mut w, c);
        }
        w.into_bytes()
    }
}

fn steal_from(v: u8) -> Result<StealPolicy, FormatError> {
    match v {
        0 => Ok(StealPolicy::Oldest),
        1 => Ok(StealPolicy::Quietest),
        2 => Ok(StealPolicy::Refuse),
        other => Err(FormatError::Encoding(u32::from(other))),
    }
}

fn steal_to(v: StealPolicy) -> u8 {
    match v {
        StealPolicy::Oldest => 0,
        StealPolicy::Quietest => 1,
        StealPolicy::Refuse => 2,
    }
}

fn selection_from(v: u8) -> Result<ClipSelection, FormatError> {
    match v {
        0 => Ok(ClipSelection::RoundRobin),
        1 => Ok(ClipSelection::Random),
        other => Err(FormatError::Encoding(u32::from(other))),
    }
}

fn selection_to(v: ClipSelection) -> u8 {
    match v {
        ClipSelection::RoundRobin => 0,
        ClipSelection::Random => 1,
    }
}

fn model_from(v: u8) -> Result<AttenuationModel, FormatError> {
    match v {
        0 => Ok(AttenuationModel::Linear),
        1 => Ok(AttenuationModel::Inverse),
        2 => Ok(AttenuationModel::Exponential),
        other => Err(FormatError::Encoding(u32::from(other))),
    }
}

fn model_to(v: AttenuationModel) -> u8 {
    match v {
        AttenuationModel::Linear => 0,
        AttenuationModel::Inverse => 1,
        AttenuationModel::Exponential => 2,
    }
}

/// Reads one 108-byte sound record, validating each field as it goes.
fn read_sound(r: &mut Reader<'_>, clip_count: u32) -> Result<Sound, FormatError> {
    let id = r.u32()?;
    let bus = r.u32()?;
    let volume = r.f32()?;
    if !(0.0..=MAX_VOLUME).contains(&volume) {
        return Err(FormatError::Dimensions);
    }
    let pitch_min = r.f32()?;
    let pitch_max = r.f32()?;
    let pitch_range = MIN_PITCH..=MAX_PITCH;
    if !pitch_range.contains(&pitch_min) || !pitch_range.contains(&pitch_max) {
        return Err(FormatError::Dimensions);
    }
    if pitch_min > pitch_max {
        return Err(FormatError::Inconsistent);
    }
    let min_distance = r.f32()?;
    let max_distance = r.f32()?;
    let rolloff = r.f32()?;
    if min_distance <= 0.0 || max_distance <= min_distance || rolloff <= 0.0 {
        return Err(FormatError::Geometry);
    }
    let flags = r.u16()?;
    if flags & !KNOWN_SOUND_FLAGS != 0 {
        return Err(FormatError::Flags(u32::from(flags)));
    }
    let max_instances = r.u16()?;
    if max_instances == 0 {
        return Err(FormatError::Dimensions);
    }
    let priority = r.u8()?;
    let steal = steal_from(r.u8()?)?;
    let selection = selection_from(r.u8()?)?;
    let model = model_from(r.u8()?)?;
    let count = usize::from(r.u8()?);
    if count == 0 || count > MAX_CLIPS_PER_SOUND {
        return Err(FormatError::Dimensions);
    }
    if r.array::<3>()? != [0; 3] {
        return Err(FormatError::Reserved);
    }
    let mut clips = Vec::with_capacity(count);
    for slot in 0..MAX_CLIPS_PER_SOUND {
        let clip = r.u32()?;
        if slot < count {
            if clip >= clip_count {
                return Err(FormatError::Inconsistent);
            }
            clips.push(clip);
        } else if clip != 0 {
            return Err(FormatError::Reserved);
        }
    }
    Ok(Sound {
        id,
        clips,
        selection,
        volume,
        pitch_min,
        pitch_max,
        looping: flags & SOUND_LOOPING != 0,
        spatial: flags & SOUND_SPATIAL != 0,
        bus,
        priority,
        max_instances,
        steal,
        attenuation: Attenuation {
            model,
            min_distance,
            max_distance,
            rolloff,
        },
    })
}

fn write_sound(w: &mut Writer, s: &Sound) {
    w.u32(s.id);
    w.u32(s.bus);
    w.f32(s.volume);
    w.f32(s.pitch_min);
    w.f32(s.pitch_max);
    w.f32(s.attenuation.min_distance);
    w.f32(s.attenuation.max_distance);
    w.f32(s.attenuation.rolloff);
    let mut flags = 0;
    if s.looping {
        flags |= SOUND_LOOPING;
    }
    if s.spatial {
        flags |= SOUND_SPATIAL;
    }
    w.u16(flags);
    w.u16(s.max_instances);
    w.u8(s.priority);
    w.u8(steal_to(s.steal));
    w.u8(selection_to(s.selection));
    w.u8(model_to(s.attenuation.model));
    w.u8(u8::try_from(s.clips.len()).unwrap_or(u8::MAX));
    w.bytes(&[0; 3]);
    for slot in 0..MAX_CLIPS_PER_SOUND {
        w.u32(s.clips.get(slot).copied().unwrap_or(0));
    }
}

/// Zero bytes after `data_len` bytes of sample data, to the next 4-byte boundary.
fn padding(data_len: usize) -> usize {
    (4 - data_len % 4) % 4
}

fn read_clip(r: &mut Reader<'_>) -> Result<Clip, FormatError> {
    let channels = r.u8()?;
    if !(1..=2).contains(&channels) {
        return Err(FormatError::Dimensions);
    }
    let encoding = r.u8()?;
    if r.u16()? != 0 {
        return Err(FormatError::Reserved);
    }
    let frames = r.u32()?;
    if frames == 0 || frames > MAX_CLIP_FRAMES {
        return Err(FormatError::Dimensions);
    }
    let count = frames as usize * usize::from(channels);
    let (samples, data_len) = match encoding {
        1 => {
            let data = r.slice(count * 2)?;
            let v = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect();
            (ClipSamples::I16(v), data.len())
        }
        2 => {
            let data = r.slice(count * 4)?;
            let mut v = Vec::with_capacity(count);
            for b in data.as_chunks::<4>().0 {
                let s = f32::from_le_bytes(*b);
                if !s.is_finite() {
                    return Err(FormatError::NonFinite);
                }
                if !(-1.0..=1.0).contains(&s) {
                    return Err(FormatError::Dimensions);
                }
                v.push(s);
            }
            (ClipSamples::F32(v), data.len())
        }
        other => return Err(FormatError::Encoding(u32::from(other))),
    };
    if r.slice(padding(data_len))?.iter().any(|b| *b != 0) {
        return Err(FormatError::Reserved);
    }
    Ok(Clip { channels, samples })
}

fn write_clip(w: &mut Writer, c: &Clip) {
    w.u8(c.channels);
    w.u8(c.samples.encoding());
    w.u16(0);
    w.count(c.frames());
    let data_len = match &c.samples {
        ClipSamples::I16(v) => {
            for s in v {
                w.bytes(&s.to_le_bytes());
            }
            v.len() * 2
        }
        ClipSamples::F32(v) => {
            for s in v {
                w.f32(*s);
            }
            v.len() * 4
        }
    };
    for _ in 0..padding(data_len) {
        w.u8(0);
    }
}
