//! Voice limits: per-sound steal policies and global stealing by priority then
//! audibility.

use mantis_audio::{AudioEvent, HandleAllocator, Mixer, MixerConfig, SoundId, VoiceHandle, VoiceState};
use mantis_formats::sound_bank::{AttenuationModel, Sound, StealPolicy};

use crate::support::{TestResult, attenuation, bank, config, dc, graph, looping, mixer, render};

fn limited(steal: StealPolicy, max_instances: u16) -> Sound {
    Sound {
        steal,
        max_instances,
        spatial: true,
        attenuation: attenuation(AttenuationModel::Inverse, 1.0, 100.0),
        ..looping(1, vec![0])
    }
}

fn play_at(m: &mut Mixer, sound: u32, handle: VoiceHandle, distance: f32) {
    m.handle(AudioEvent::play_at(SoundId(sound), [0.0, 0.0, -distance], handle));
}

#[test]
fn refuse_policy_drops_the_new_play() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.5, 16)], vec![limited(StealPolicy::Refuse, 1)]))?;
    let h = HandleAllocator::new();
    let (a, b) = (h.allocate(), h.allocate());
    play_at(&mut m, 1, a, 1.0);
    play_at(&mut m, 1, b, 1.0);
    assert_eq!(m.voice_state(a), Some(VoiceState::Playing));
    assert_eq!(m.voice_state(b), None);
    assert_eq!((m.stats().refused, m.stats().stolen), (1, 0));
    Ok(())
}

#[test]
fn oldest_policy_fades_out_the_first_instance() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.5, 16)], vec![limited(StealPolicy::Oldest, 2)]))?;
    let handles = HandleAllocator::new();
    let (first, second, third) = (handles.allocate(), handles.allocate(), handles.allocate());
    for handle in [first, second, third] {
        play_at(&mut m, 1, handle, 1.0);
    }
    assert_eq!(m.voice_state(first), Some(VoiceState::Stopping));
    assert_eq!(m.voice_state(second), Some(VoiceState::Playing));
    assert_eq!(m.voice_state(third), Some(VoiceState::Playing));
    assert_eq!(m.stats().stolen, 1);
    let _ = render(&mut m, 512);
    assert_eq!(m.voice_state(first), None, "the stolen voice finished its fade");
    assert_eq!(m.active_voices(), 2);
    Ok(())
}

#[test]
fn quietest_policy_fades_out_the_least_audible_instance() -> TestResult {
    let mut m = mixer(&bank(vec![dc(0.5, 16)], vec![limited(StealPolicy::Quietest, 2)]))?;
    let h = HandleAllocator::new();
    let (near, far, newer) = (h.allocate(), h.allocate(), h.allocate());
    play_at(&mut m, 1, near, 1.0);
    play_at(&mut m, 1, far, 40.0);
    let _ = render(&mut m, 64);
    play_at(&mut m, 1, newer, 1.0);
    assert_eq!(m.voice_state(far), Some(VoiceState::Stopping));
    assert_eq!(m.voice_state(near), Some(VoiceState::Playing));
    assert_eq!(m.voice_state(newer), Some(VoiceState::Playing));
    Ok(())
}

#[test]
fn global_limit_steals_by_priority_then_audibility() -> TestResult {
    let high = Sound {
        priority: 200,
        ..looping(1, vec![0])
    };
    let low = Sound {
        priority: 10,
        ..looping(2, vec![0])
    };
    let b = bank(vec![dc(0.5, 16)], vec![high, low]);
    let mut m = Mixer::new(
        &b,
        &graph(),
        MixerConfig {
            max_voices: 2,
            ..config()
        },
    )?;
    let h = HandleAllocator::new();
    let [hi_a, lo_b, hi_c, lo_d, hi_e] = [
        h.allocate(),
        h.allocate(),
        h.allocate(),
        h.allocate(),
        h.allocate(),
    ];
    m.handle(AudioEvent::play(SoundId(1), hi_a));
    m.handle(AudioEvent::play(SoundId(2), lo_b));
    m.handle(AudioEvent::play(SoundId(1), hi_c));
    assert_eq!(
        m.voice_state(lo_b),
        Some(VoiceState::Stopping),
        "the low-priority voice goes first"
    );
    assert_eq!(m.active_voices(), 2);
    let _ = render(&mut m, 50);
    m.handle(AudioEvent::play(SoundId(2), lo_d));
    assert_eq!(m.voice_state(lo_d), None, "a lower-priority play is refused");
    assert_eq!(m.stats().refused, 1);
    // Equal priority and equal audibility: the oldest is stolen.
    m.handle(AudioEvent::play(SoundId(1), hi_e));
    assert_eq!(m.voice_state(hi_a), Some(VoiceState::Stopping));
    assert_eq!(m.voice_state(hi_c), Some(VoiceState::Playing));
    assert_eq!(m.voice_state(hi_e), Some(VoiceState::Playing));
    // Every slot is now busy (two playing, two fading): the next steal cuts the quietest
    // fading voice short instead of failing.
    let _ = render(&mut m, 100);
    let f = h.allocate();
    m.handle(AudioEvent::play(SoundId(1), f));
    assert_eq!(m.voice_state(f), Some(VoiceState::Playing));
    assert_eq!(m.stats().cut, 1);
    assert_eq!(m.voice_state(lo_b), None, "the most faded voice was cut");
    assert_eq!(m.voices_in_use(), 4);
    Ok(())
}

#[test]
fn stolen_voices_fade_without_clicks() -> TestResult {
    let b = bank(
        vec![dc(1.0, 16)],
        vec![Sound {
            max_instances: 1,
            ..looping(1, vec![0])
        }],
    );
    let mut m = mixer(&b)?;
    let h = HandleAllocator::new();
    m.handle(AudioEvent::play(SoundId(1), h.allocate()));
    let _ = render(&mut m, 64);
    // The new instance starts at full level while the old one fades: the sum rises from
    // 1 to 2 and ramps back to 1, never jumping by more than the fade slope.
    m.handle(AudioEvent::play(SoundId(1), h.allocate()));
    let out = render(&mut m, 512);
    assert!((out[0][0] - 2.0).abs() < 0.01);
    assert!((out[511][0] - 1.0).abs() < 1e-6);
    let steps = out
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).abs())
        .fold(0.0f32, f32::max);
    assert!(steps <= 1.0 / 240.0 + 1e-6, "{steps}");
    Ok(())
}
