//! Events game threads post to the audio thread.

use crate::ids::{BusId, EmitterId, SoundId, VoiceHandle};

/// One request to the mixer. Small and `Copy`, so a bounded queue of them never
/// allocates per event.
///
/// Events with a non-finite or out-of-range value are ignored as a whole (and counted in
/// [`crate::MixerStats::ignored`]); they never poison the mix.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AudioEvent {
    /// Starts a voice of `sound`.
    Play {
        /// The sound.
        sound: SoundId,
        /// The emitter the voice follows ([`AudioEvent::SetEmitter`] moves it).
        emitter: Option<EmitterId>,
        /// World position. With an emitter, also records the emitter's position. A spatial
        /// sound with neither a position nor a known emitter plays at the listener.
        position: Option<[f32; 3]>,
        /// Linear volume multiplier, 0 to 4.
        volume: f32,
        /// Pitch multiplier, 0.25 to 4, applied on top of the sound's random pitch.
        pitch: f32,
        /// Caller-allocated handle, or [`VoiceHandle::NONE`].
        handle: VoiceHandle,
    },
    /// Fades out and stops the voice with `handle`. Unknown or stale handles are ignored.
    Stop {
        /// The voice.
        handle: VoiceHandle,
        /// Fade length; shorter than [`crate::MIN_FADE_MS`] is raised to it.
        fade_ms: f32,
    },
    /// Moves and orients the listener. `forward` and `up` need not be unit length but
    /// must be non-zero and not parallel.
    SetListener {
        /// World position.
        position: [f32; 3],
        /// Facing direction.
        forward: [f32; 3],
        /// Up direction.
        up: [f32; 3],
    },
    /// Moves an emitter and every voice following it.
    SetEmitter {
        /// The emitter.
        emitter: EmitterId,
        /// World position.
        position: [f32; 3],
    },
    /// Fades a bus to a new linear gain (0 to 4).
    SetBusGain {
        /// The bus.
        bus: BusId,
        /// Target linear gain.
        gain: f32,
        /// Fade length; shorter than [`crate::MIN_FADE_MS`] is raised to it.
        fade_ms: f32,
    },
    /// Fades out every voice over [`crate::MIN_FADE_MS`].
    StopAll,
}

impl AudioEvent {
    /// A non-positional play at unit volume and pitch.
    pub fn play(sound: SoundId, handle: VoiceHandle) -> AudioEvent {
        AudioEvent::Play {
            sound,
            emitter: None,
            position: None,
            volume: 1.0,
            pitch: 1.0,
            handle,
        }
    }

    /// A play at a world position, at unit volume and pitch.
    pub fn play_at(sound: SoundId, position: [f32; 3], handle: VoiceHandle) -> AudioEvent {
        AudioEvent::Play {
            sound,
            emitter: None,
            position: Some(position),
            volume: 1.0,
            pitch: 1.0,
            handle,
        }
    }
}
