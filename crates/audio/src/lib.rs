//! mantis-audio: the event-driven mixer the client's audio thread hosts (plan 8.5).
//!
//! Game threads post small `Copy` [`AudioEvent`]s; the audio thread applies them with
//! [`Mixer::handle`] and pulls interleaved stereo blocks with [`Mixer::render`]. Both
//! allocate nothing after [`Mixer::new`], which preallocates the voice pool, the per-bus
//! scratch buffers, and the emitter table.
//!
//! - **Data-defined banks.** Clips and sounds come from a
//!   [`mantis_formats::sound_bank::SoundBank`]: per sound, clip choice (round-robin or
//!   seeded random), volume, random pitch range, looping, spatial flag, bus, priority,
//!   instance limit with a steal policy, and distance attenuation.
//! - **Mixer graph in data.** Buses and their effects come from a
//!   [`mantis_formats::mixer_graph::MixerGraph`]. Voices mix into their bus; each bus
//!   applies its (fadeable) gain and effect chain and sums into its parent; the master is
//!   the output.
//! - **Voices.** A fixed pool of `max_voices` audible voices (plus as many slots again for
//!   fade-outs). Per-sound limits apply the sound's steal policy; the global limit steals
//!   the lowest-priority, then least audible, then oldest voice, and refuses a play that
//!   ranks below every voice. Playback resamples by linear interpolation for pitch and
//!   for a bank rate that differs from the output rate, and loops seamlessly.
//! - **No clicks.** Stops, steals, and bus gain changes fade over at least
//!   [`MIN_FADE_MS`]; pan and distance gains follow their targets over [`GAIN_RAMP_MS`].
//! - **Spatialization.** See [`spatial`]: equal-power panning from the listener frame and
//!   distance attenuation per the sound's model. Non-spatial sounds bypass it.
//! - **Handles.** The caller allocates [`VoiceHandle`]s with a [`HandleAllocator`], so a
//!   play is fire-and-forget; stale handles are ignored.
//! - **Determinism.** Random clip and pitch choices come from a generator seeded by
//!   [`MixerConfig::seed`], so a render is reproducible from its event sequence.
//!
//! Device output is out of scope here: the host's audio thread owns pacing and the
//! output, and calls [`Mixer::render`] for each block.

#![forbid(unsafe_code)]

mod bus;
mod config;
mod dsp;
mod event;
mod ids;
mod mixer;
pub mod spatial;
mod voice;

pub use config::{MAX_BLOCK_FRAMES, MAX_EMITTERS, MAX_VOICES, MixerConfig, MixerError};
pub use dsp::{GAIN_RAMP_MS, MAX_FADE_MS, MIN_FADE_MS, STEAL_FADE_MS};
pub use event::AudioEvent;
pub use ids::{BusId, EmitterId, HandleAllocator, SoundId, VoiceHandle};
pub use mixer::{Mixer, MixerStats, VoiceState};
