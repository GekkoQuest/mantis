//! Playback: gain staging, silence, resampling, looping, clip selection, handles.

use mantis_audio::{AudioEvent, HandleAllocator, Mixer, MixerConfig, SoundId, VoiceState};
use mantis_formats::mixer_graph::MixerGraph;
use mantis_formats::sound_bank::{ClipSelection, Sound, SoundBank};

use crate::support::{
    MASTER, RATE, SFX, TestResult, bank, bus, close, config, dc, graph, looping, max_step, mixer, mono, ramp,
    render, sound,
};

#[test]
fn dc_clip_reaches_volume_times_gain_exactly() -> TestResult {
    let b = bank(
        vec![dc(0.5, 32)],
        vec![Sound {
            volume: 0.8,
            ..looping(1, vec![0])
        }],
    );
    let g = MixerGraph {
        buses: vec![
            bus(MASTER, None, 1.5, Vec::new()),
            bus(SFX, Some(MASTER), 0.5, Vec::new()),
        ],
    };
    let mut m = Mixer::new(&b, &g, config())?;
    let handles = HandleAllocator::new();
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: None,
        position: None,
        volume: 0.5,
        pitch: 1.0,
        handle: handles.allocate(),
    });
    let expected = 0.5 * 0.8 * 0.5 * 0.5 * 1.5;
    let out = render(&mut m, 1000);
    for f in &out {
        assert!(
            close(f[0], expected, 1e-6) && close(f[1], expected, 1e-6),
            "{f:?}"
        );
    }
    assert_eq!(m.stats().plays, 1);
    Ok(())
}

#[test]
fn zero_voices_render_silence_over_any_buffer() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.5, 8)], vec![sound(1, vec![0])]))?;
    // Odd length, longer than one block, prefilled with garbage.
    let mut out = vec![7.0f32; 301];
    m.render(&mut out);
    assert!(out.iter().all(|s| *s == 0.0));
    let mut empty: [f32; 0] = [];
    m.render(&mut empty);
    Ok(())
}

#[test]
fn pitch_two_consumes_frames_twice_as_fast() -> TestResult {
    let mut m = mixer(&bank(vec![ramp(64)], vec![sound(1, vec![0])]))?;
    let h = HandleAllocator::new().allocate();
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: None,
        position: None,
        volume: 1.0,
        pitch: 2.0,
        handle: h,
    });
    let out = render(&mut m, 40);
    for (k, f) in out.iter().enumerate() {
        let expected = if k < 32 { (2 * k) as f32 / 64.0 } else { 0.0 };
        assert!(close(f[0], expected, 1e-6), "frame {k}: {} vs {expected}", f[0]);
    }
    assert_eq!(
        m.voice_state(h),
        None,
        "64 frames at pitch 2 end after 32 output frames"
    );
    Ok(())
}

#[test]
fn bank_rate_mismatch_is_resampled() -> TestResult {
    let b = SoundBank {
        sample_rate: RATE / 2,
        ..bank(vec![ramp(64)], vec![sound(1, vec![0])])
    };
    let mut m = mixer(&b)?;
    m.handle(AudioEvent::play(SoundId(1), HandleAllocator::new().allocate()));
    let out = render(&mut m, 126);
    for (k, f) in out.iter().enumerate() {
        // Half-rate clip: output frame k reads clip frame k / 2, linearly interpolated.
        let expected = k as f32 / 2.0 / 64.0;
        assert!(close(f[0], expected, 1e-6), "frame {k}");
    }
    Ok(())
}

#[test]
fn looping_repeats_the_clip_sample_exactly() -> TestResult {
    let mut m = mixer(&bank(vec![ramp(16)], vec![looping(1, vec![0])]))?;
    m.handle(AudioEvent::play(SoundId(1), HandleAllocator::new().allocate()));
    let out = render(&mut m, 200);
    for (k, f) in out.iter().enumerate() {
        assert_eq!(f[0], (k % 16) as f32 / 16.0, "frame {k}");
    }
    Ok(())
}

#[test]
fn looping_wraps_seamlessly_at_fractional_pitch() -> TestResult {
    // Four sine cycles in 64 frames: the loop point is continuous in the signal, so the
    // output must be a clean sine through every wrap, interpolation error aside.
    let omega = core::f32::consts::TAU * 4.0 / 64.0;
    let clip = mono((0..64).map(|i| 0.5 * (omega * i as f32).sin()).collect());
    let mut m = mixer(&bank(vec![clip], vec![looping(1, vec![0])]))?;
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: None,
        position: None,
        volume: 1.0,
        pitch: 1.5,
        handle: HandleAllocator::new().allocate(),
    });
    let out = render(&mut m, 2000);
    for (k, f) in out.iter().enumerate() {
        let phase = (1.5 * k as f64).rem_euclid(64.0) as f32;
        let expected = 0.5 * (omega * phase).sin();
        assert!(close(f[0], expected, 0.011), "frame {k}: {} vs {expected}", f[0]);
    }
    assert!(max_step(&out) <= 0.5 * omega * 1.5 + 1e-3);
    Ok(())
}

#[test]
fn stereo_clip_keeps_its_channels_when_not_spatial() -> TestResult {
    let clip = mantis_formats::sound_bank::Clip {
        channels: 2,
        samples: mantis_formats::sound_bank::ClipSamples::F32(vec![0.25, -0.75, 0.25, -0.75]),
    };
    let mut m = mixer(&bank(vec![clip], vec![looping(1, vec![0])]))?;
    m.handle(AudioEvent::play(SoundId(1), HandleAllocator::new().allocate()));
    for f in render(&mut m, 10) {
        assert_eq!(f, [0.25, -0.75]);
    }
    Ok(())
}

/// The level of the first frame of each of `plays` successive plays of sound 1.
fn levels(m: &mut Mixer, plays: usize) -> Vec<f32> {
    let handles = HandleAllocator::new();
    (0..plays)
        .map(|_| {
            m.handle(AudioEvent::play(SoundId(1), handles.allocate()));
            let first = render(m, 1)[0][0];
            m.handle(AudioEvent::StopAll);
            let _ = render(m, 512);
            first
        })
        .collect()
}

#[test]
fn round_robin_cycles_and_random_is_seeded() -> TestResult {
    let clips = vec![dc(0.1, 8), dc(0.2, 8), dc(0.3, 8)];
    let mut m = mixer(&bank(clips.clone(), vec![looping(1, vec![0, 1, 2])]))?;
    let got = levels(&mut m, 4);
    assert_eq!(got, vec![0.1, 0.2, 0.3, 0.1]);

    let random = bank(
        clips,
        vec![Sound {
            selection: ClipSelection::Random,
            ..looping(1, vec![0, 1, 2])
        }],
    );
    let run = |seed: u64| -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let mut m = Mixer::new(&random, &graph(), MixerConfig { seed, ..config() })?;
        Ok(levels(&mut m, 24))
    };
    let a = run(1)?;
    assert_eq!(a, run(1)?, "same seed, same choices");
    assert_ne!(a, run(2)?, "another seed, other choices");
    for level in [0.1f32, 0.2, 0.3] {
        assert!(a.contains(&level), "every clip is chosen eventually");
    }
    Ok(())
}

#[test]
fn random_pitch_stays_in_range_and_is_reproducible() -> TestResult {
    let b = bank(
        vec![ramp(4096)],
        vec![Sound {
            pitch_min: 0.5,
            pitch_max: 2.0,
            ..sound(1, vec![0])
        }],
    );
    let run = || -> Result<Vec<[f32; 2]>, Box<dyn std::error::Error>> {
        let mut m = mixer(&b)?;
        let handles = HandleAllocator::new();
        let mut all = Vec::new();
        for _ in 0..8 {
            m.handle(AudioEvent::play(SoundId(1), handles.allocate()));
            let out = render(&mut m, 101);
            // Frame 100 reads clip frame 100 * pitch.
            let pitch = out[100][0] * 4096.0 / 100.0;
            assert!((0.5 - 1e-3..=2.0 + 1e-3).contains(&pitch), "{pitch}");
            all.extend(out);
            m.handle(AudioEvent::StopAll);
            let _ = render(&mut m, 512);
        }
        Ok(all)
    };
    assert_eq!(run()?, run()?);
    Ok(())
}

#[test]
fn stale_and_unknown_handles_are_ignored() -> TestResult {
    let mut m = mixer(&bank(
        vec![dc(0.5, 32)],
        vec![sound(1, vec![0]), looping(2, vec![0])],
    ))?;
    let handles = HandleAllocator::new();
    let old = handles.allocate();
    m.handle(AudioEvent::play(SoundId(1), old));
    let _ = render(&mut m, 64);
    assert_eq!(m.voice_state(old), None, "the one-shot ended");
    let live = handles.allocate();
    m.handle(AudioEvent::play(SoundId(2), live));
    let ignored = m.stats().ignored;
    m.handle(AudioEvent::Stop {
        handle: old,
        fade_ms: 0.0,
    });
    m.handle(AudioEvent::Stop {
        handle: handles.allocate(),
        fade_ms: 0.0,
    });
    m.handle(AudioEvent::Stop {
        handle: mantis_audio::VoiceHandle::NONE,
        fade_ms: 0.0,
    });
    assert_eq!(m.stats().ignored, ignored + 3);
    assert_eq!(m.voice_state(live), Some(VoiceState::Playing));
    let out = render(&mut m, 64);
    assert!(out.iter().all(|f| f[0] == 0.5));
    // Reusing a live handle for a new play is ignored too.
    m.handle(AudioEvent::play(SoundId(2), live));
    assert_eq!(m.active_voices(), 1);
    Ok(())
}

#[test]
fn invalid_events_are_ignored() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.5, 32)], vec![looping(1, vec![0])]))?;
    let handles = HandleAllocator::new();
    let bad_plays = [
        (SoundId(9), 1.0, 1.0, None),
        (SoundId(1), f32::NAN, 1.0, None),
        (SoundId(1), 5.0, 1.0, None),
        (SoundId(1), 1.0, 0.1, None),
        (SoundId(1), 1.0, f32::INFINITY, None),
        (SoundId(1), 1.0, 1.0, Some([f32::NAN, 0.0, 0.0])),
    ];
    for (sound, volume, pitch, position) in bad_plays {
        m.handle(AudioEvent::Play {
            sound,
            emitter: None,
            position,
            volume,
            pitch,
            handle: handles.allocate(),
        });
    }
    m.handle(AudioEvent::SetListener {
        position: [0.0; 3],
        forward: [0.0, 1.0, 0.0],
        up: [0.0, 1.0, 0.0],
    });
    m.handle(AudioEvent::SetEmitter {
        emitter: mantis_audio::EmitterId(1),
        position: [f32::INFINITY, 0.0, 0.0],
    });
    m.handle(AudioEvent::SetBusGain {
        bus: mantis_audio::BusId(SFX),
        gain: -1.0,
        fade_ms: 1.0,
    });
    m.handle(AudioEvent::SetBusGain {
        bus: mantis_audio::BusId(77),
        gain: 1.0,
        fade_ms: 1.0,
    });
    assert_eq!(m.stats().ignored, 10);
    assert_eq!(m.voices_in_use(), 0);
    assert!(render(&mut m, 64).iter().all(|f| *f == [0.0, 0.0]));
    Ok(())
}
