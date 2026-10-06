//! Mixer configuration and construction errors.

use mantis_formats::FormatError;
use mantis_formats::sound_bank::{MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};

use crate::ids::{BusId, SoundId};

/// Most voices a mixer may be configured for.
pub const MAX_VOICES: usize = 4096;
/// Longest block a mixer may be configured for, in frames.
pub const MAX_BLOCK_FRAMES: usize = 16_384;
/// Most emitters a mixer may track.
pub const MAX_EMITTERS: usize = 65_536;

/// How a mixer renders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MixerConfig {
    /// Output sample rate, 8000 to 192000. Clips at another rate are resampled.
    pub sample_rate: u32,
    /// Output channels; 2 (interleaved stereo) is the only supported layout for now.
    pub channels: u16,
    /// Audible voices at once, 1 to [`MAX_VOICES`]. The pool holds twice as many slots so
    /// stopped and stolen voices can fade out while new ones start.
    pub max_voices: usize,
    /// Frames mixed per internal block, 1 to [`MAX_BLOCK_FRAMES`]; per-bus scratch buffers
    /// are this long. `render` accepts any length and splits it into blocks.
    pub block_frames: usize,
    /// Emitter positions remembered for plays that name an emitter without a position,
    /// 0 to [`MAX_EMITTERS`]; the least recently moved one is forgotten when full.
    pub max_emitters: usize,
    /// Seed of the generator that picks random clips and pitches, so renders are
    /// reproducible from the event sequence.
    pub seed: u64,
}

impl Default for MixerConfig {
    fn default() -> Self {
        MixerConfig {
            sample_rate: 48_000,
            channels: 2,
            max_voices: 64,
            block_frames: 256,
            max_emitters: 256,
            seed: 0x6d61_6e74_6973_0001,
        }
    }
}

impl MixerConfig {
    /// Checks every field's range.
    ///
    /// # Errors
    /// The first [`MixerError`] configuration variant that applies.
    pub fn validate(&self) -> Result<(), MixerError> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(MixerError::SampleRate(self.sample_rate));
        }
        if self.channels != 2 {
            return Err(MixerError::Channels(self.channels));
        }
        if !(1..=MAX_VOICES).contains(&self.max_voices) {
            return Err(MixerError::MaxVoices(self.max_voices));
        }
        if !(1..=MAX_BLOCK_FRAMES).contains(&self.block_frames) {
            return Err(MixerError::BlockFrames(self.block_frames));
        }
        if self.max_emitters > MAX_EMITTERS {
            return Err(MixerError::MaxEmitters(self.max_emitters));
        }
        Ok(())
    }
}

/// Why a mixer could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MixerError {
    /// Output sample rate out of range.
    SampleRate(u32),
    /// Unsupported channel count.
    Channels(u16),
    /// Voice count out of range.
    MaxVoices(usize),
    /// Block length out of range.
    BlockFrames(usize),
    /// Emitter count out of range.
    MaxEmitters(usize),
    /// The sound bank breaks a format rule.
    Bank(FormatError),
    /// The mixer graph breaks a format rule.
    Graph(FormatError),
    /// A sound names a bus the graph does not have.
    UnknownBus {
        /// The sound.
        sound: SoundId,
        /// The missing bus.
        bus: BusId,
    },
}

impl core::fmt::Display for MixerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MixerError::Bank(e) => write!(f, "sound bank: {e}"),
            MixerError::Graph(e) => write!(f, "mixer graph: {e}"),
            MixerError::UnknownBus { sound, bus } => {
                write!(
                    f,
                    "sound {} names bus {}, which the mixer graph lacks",
                    sound.0, bus.0
                )
            }
            other => write!(f, "invalid mixer config: {other:?}"),
        }
    }
}

impl std::error::Error for MixerError {}
