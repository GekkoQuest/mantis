//! Builders for banks, graphs, and mixers, and render helpers.

use mantis_audio::{Mixer, MixerConfig};
use mantis_formats::mixer_graph::{Bus, Effect, MixerGraph};
use mantis_formats::sound_bank::{
    Attenuation, AttenuationModel, Clip, ClipSamples, ClipSelection, Sound, SoundBank, StealPolicy,
};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Output and (unless a test says otherwise) bank sample rate.
pub const RATE: u32 = 48_000;
/// The master bus id.
pub const MASTER: u32 = 0;
/// The effects bus id (child of the master).
pub const SFX: u32 = 1;

/// Samples of the minimum fade at [`RATE`].
pub const MIN_FADE_SAMPLES: f32 = 240.0;

pub fn attenuation(model: AttenuationModel, min_distance: f32, max_distance: f32) -> Attenuation {
    Attenuation {
        model,
        min_distance,
        max_distance,
        rolloff: 1.0,
    }
}

/// A one-shot, non-spatial sound at unit volume and pitch on the effects bus.
pub fn sound(id: u32, clips: Vec<u32>) -> Sound {
    Sound {
        id,
        clips,
        selection: ClipSelection::RoundRobin,
        volume: 1.0,
        pitch_min: 1.0,
        pitch_max: 1.0,
        looping: false,
        spatial: false,
        bus: SFX,
        priority: 100,
        max_instances: 16,
        steal: StealPolicy::Oldest,
        attenuation: attenuation(AttenuationModel::Linear, 1.0, 11.0),
    }
}

/// `sound` but looping.
pub fn looping(id: u32, clips: Vec<u32>) -> Sound {
    Sound {
        looping: true,
        ..sound(id, clips)
    }
}

pub fn mono(samples: Vec<f32>) -> Clip {
    Clip {
        channels: 1,
        samples: ClipSamples::F32(samples),
    }
}

/// A constant mono clip.
pub fn dc(value: f32, frames: usize) -> Clip {
    mono(vec![value; frames])
}

/// A mono ramp: frame `i` is `i / frames`.
pub fn ramp(frames: usize) -> Clip {
    mono((0..frames).map(|i| i as f32 / frames as f32).collect())
}

pub fn bank(clips: Vec<Clip>, sounds: Vec<Sound>) -> SoundBank {
    SoundBank {
        sample_rate: RATE,
        clips,
        sounds,
    }
}

pub fn bus(id: u32, parent: Option<u32>, gain: f32, effects: Vec<Effect>) -> Bus {
    Bus {
        id,
        parent,
        gain,
        effects,
    }
}

/// Master (unit gain, no effects) and the effects bus (unit gain) under it.
pub fn graph() -> MixerGraph {
    MixerGraph {
        buses: vec![
            bus(MASTER, None, 1.0, Vec::new()),
            bus(SFX, Some(MASTER), 1.0, Vec::new()),
        ],
    }
}

pub fn config() -> MixerConfig {
    MixerConfig {
        sample_rate: RATE,
        block_frames: 64,
        max_voices: 8,
        ..MixerConfig::default()
    }
}

pub fn mixer(bank: &SoundBank) -> Result<Mixer, Box<dyn std::error::Error>> {
    Ok(Mixer::new(bank, &graph(), config())?)
}

/// Renders `frames` stereo frames.
pub fn render(m: &mut Mixer, frames: usize) -> Vec<[f32; 2]> {
    let mut out = vec![0.0f32; frames * 2];
    m.render(&mut out);
    out.as_chunks::<2>().0.to_vec()
}

/// The largest absolute change between consecutive samples of either channel.
pub fn max_step(frames: &[[f32; 2]]) -> f32 {
    frames
        .windows(2)
        .flat_map(|w| [(w[1][0] - w[0][0]).abs(), (w[1][1] - w[0][1]).abs()])
        .fold(0.0, f32::max)
}

pub fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}
