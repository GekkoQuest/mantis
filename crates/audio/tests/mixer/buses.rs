//! The bus graph: gain fades, stop fades, effects, the master limiter, and load-time
//! validation.

use mantis_audio::{AudioEvent, BusId, HandleAllocator, Mixer, MixerConfig, MixerError, SoundId, VoiceState};
use mantis_formats::FormatError;
use mantis_formats::mixer_graph::{Effect, MixerGraph};
use mantis_formats::sound_bank::Sound;

use crate::support::{
    MASTER, MIN_FADE_SAMPLES, SFX, TestResult, bank, bus, close, config, dc, graph, looping, mixer, mono,
    render, sound,
};

fn monotonic(frames: &[[f32; 2]], falling: bool) -> bool {
    frames.windows(2).all(|w| {
        if falling {
            w[1][0] <= w[0][0] + 1e-7
        } else {
            w[1][0] >= w[0][0] - 1e-7
        }
    })
}

fn max_step_left(frames: &[[f32; 2]]) -> f32 {
    frames
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn bus_gain_fades_are_monotonic_and_click_free() -> TestResult {
    let mut m = mixer(&bank(vec![dc(1.0, 16)], vec![looping(1, vec![0])]))?;
    m.handle(AudioEvent::play(SoundId(1), HandleAllocator::new().allocate()));
    let _ = render(&mut m, 64);
    m.handle(AudioEvent::SetBusGain {
        bus: BusId(SFX),
        gain: 0.0,
        fade_ms: 10.0,
    });
    let down = render(&mut m, 1000);
    assert!(monotonic(&down, true));
    assert!(max_step_left(&down) <= 1.0 / 480.0 + 1e-6);
    assert!(close(down[479][0], 0.0, 1e-6) && down[999][0] == 0.0);
    assert_eq!(m.bus_gain(BusId(SFX)), Some(0.0));
    // A zero-length fade is raised to the minimum fade.
    m.handle(AudioEvent::SetBusGain {
        bus: BusId(SFX),
        gain: 1.0,
        fade_ms: 0.0,
    });
    let up = render(&mut m, 1000);
    assert!(monotonic(&up, false));
    assert!(max_step_left(&up) <= 1.0 / MIN_FADE_SAMPLES + 1e-6);
    assert!(close(up[999][0], 1.0, 1e-6));
    assert_eq!(m.bus_gain(BusId(MASTER)), Some(1.0));
    assert_eq!(m.bus_gain(BusId(9)), None);
    Ok(())
}

#[test]
fn stop_fades_out_click_free_then_frees_the_voice() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.8, 16)], vec![looping(1, vec![0])]))?;
    let h = HandleAllocator::new().allocate();
    m.handle(AudioEvent::play(SoundId(1), h));
    let _ = render(&mut m, 64);
    m.handle(AudioEvent::Stop {
        handle: h,
        fade_ms: 0.0,
    });
    assert_eq!(m.voice_state(h), Some(VoiceState::Stopping));
    let out = render(&mut m, 512);
    assert!(monotonic(&out, true));
    assert!(max_step_left(&out) <= 0.8 / MIN_FADE_SAMPLES + 1e-6);
    assert!(out[300..].iter().all(|f| *f == [0.0, 0.0]));
    assert_eq!(m.voice_state(h), None);
    Ok(())
}

#[test]
fn master_limiter_never_exceeds_its_threshold() -> TestResult {
    let g = MixerGraph {
        buses: vec![
            bus(
                MASTER,
                None,
                1.0,
                vec![Effect::Limiter {
                    threshold: 0.25,
                    release_ms: 30.0,
                }],
            ),
            bus(SFX, Some(MASTER), 4.0, Vec::new()),
        ],
    };
    // A loud, jagged clip at full volume, several voices deep.
    let jagged = mono((0..97).map(|i| if i % 3 == 0 { 1.0 } else { -0.6 }).collect());
    let b = bank(
        vec![jagged, dc(1.0, 16)],
        vec![
            Sound {
                volume: 4.0,
                ..looping(1, vec![0])
            },
            Sound {
                volume: 4.0,
                ..looping(2, vec![1])
            },
        ],
    );
    let mut m = Mixer::new(&b, &g, config())?;
    let h = HandleAllocator::new();
    let mut peak = 0.0f32;
    for i in 0..6 {
        m.handle(AudioEvent::play(SoundId(1 + i % 2), h.allocate()));
        for f in render(&mut m, 700) {
            peak = peak.max(f[0].abs()).max(f[1].abs());
        }
    }
    assert!(peak <= 0.25, "{peak}");
    assert!(
        peak > 0.2,
        "the limiter passes material up to its ceiling: {peak}"
    );
    Ok(())
}

#[test]
fn bus_effects_shape_the_signal() -> TestResult {
    let g = MixerGraph {
        buses: vec![
            bus(MASTER, None, 1.0, vec![Effect::Gain { db: -6.020_6 }]),
            bus(SFX, Some(MASTER), 1.0, vec![Effect::LowPass { cutoff_hz: 200.0 }]),
            bus(2, Some(MASTER), 1.0, vec![Effect::HighPass { cutoff_hz: 200.0 }]),
        ],
    };
    // Alternating samples: all energy at the Nyquist frequency.
    let nyquist = mono((0..64).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect());
    let b = bank(
        vec![nyquist, dc(1.0, 16)],
        vec![
            looping(1, vec![0]),
            Sound {
                bus: 2,
                ..looping(2, vec![1])
            },
        ],
    );
    let mut lp = Mixer::new(&b, &g, config())?;
    lp.handle(AudioEvent::play(SoundId(1), HandleAllocator::new().allocate()));
    let out = render(&mut lp, 4800);
    let tail = out[4000..].iter().fold(0.0f32, |m, f| m.max(f[0].abs()));
    assert!(tail < 0.05, "low-pass removes the Nyquist tone: {tail}");
    let mut hp = Mixer::new(&b, &g, config())?;
    hp.handle(AudioEvent::play(SoundId(2), HandleAllocator::new().allocate()));
    let out = render(&mut hp, 4800);
    assert!(out[4799][0].abs() < 1e-3, "high-pass removes DC");
    assert!(
        close(out[0][0], 0.5, 0.02),
        "the master's -6 dB gain halves the onset"
    );
    Ok(())
}

#[test]
fn construction_validates_config_bank_graph_and_buses() {
    let ok_bank = bank(vec![dc(0.5, 4)], vec![sound(1, vec![0])]);
    let unknown_bus = bank(
        vec![dc(0.5, 4)],
        vec![Sound {
            bus: 99,
            ..sound(7, vec![0])
        }],
    );
    assert_eq!(
        Mixer::new(&unknown_bus, &graph(), config()).err(),
        Some(MixerError::UnknownBus {
            sound: SoundId(7),
            bus: BusId(99),
        })
    );
    let loud = bank(
        vec![dc(0.5, 4)],
        vec![Sound {
            volume: 9.0,
            ..sound(1, vec![0])
        }],
    );
    assert_eq!(
        Mixer::new(&loud, &graph(), config()).err(),
        Some(MixerError::Bank(FormatError::Dimensions))
    );
    let orphan = MixerGraph {
        buses: vec![
            bus(MASTER, None, 1.0, Vec::new()),
            bus(SFX, Some(5), 1.0, Vec::new()),
        ],
    };
    assert_eq!(
        Mixer::new(&ok_bank, &orphan, config()).err(),
        Some(MixerError::Graph(FormatError::Inconsistent))
    );
    let bad_configs = [
        (
            MixerConfig {
                channels: 1,
                ..config()
            },
            MixerError::Channels(1),
        ),
        (
            MixerConfig {
                sample_rate: 100,
                ..config()
            },
            MixerError::SampleRate(100),
        ),
        (
            MixerConfig {
                max_voices: 0,
                ..config()
            },
            MixerError::MaxVoices(0),
        ),
        (
            MixerConfig {
                block_frames: 0,
                ..config()
            },
            MixerError::BlockFrames(0),
        ),
        (
            MixerConfig {
                max_emitters: usize::MAX,
                ..config()
            },
            MixerError::MaxEmitters(usize::MAX),
        ),
    ];
    for (c, e) in bad_configs {
        assert_eq!(Mixer::new(&ok_bank, &graph(), c).err(), Some(e));
    }
    assert!(Mixer::new(&ok_bank, &graph(), config()).is_ok());
}
