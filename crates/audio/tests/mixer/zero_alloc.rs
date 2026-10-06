//! Zero allocation on the audio hot path (CLAUDE.md rule 4): after construction,
//! `Mixer::handle` and `Mixer::render` perform no heap operation, across every event
//! kind, voice stealing, refusals, cuts, emitter-table eviction, and odd buffer lengths.

use mantis_audio::{AudioEvent, BusId, EmitterId, HandleAllocator, Mixer, MixerConfig, SoundId};
use mantis_formats::mixer_graph::{Effect, MixerGraph};
use mantis_formats::sound_bank::{AttenuationModel, ClipSelection, Sound, StealPolicy};
use mantis_testkit::alloc::assert_no_alloc;

use crate::support::{MASTER, SFX, TestResult, attenuation, bank, bus, config, dc, looping, ramp, sound};

fn busy_mixer() -> Result<Mixer, Box<dyn std::error::Error>> {
    let sounds = vec![
        Sound {
            spatial: true,
            selection: ClipSelection::Random,
            pitch_min: 0.5,
            pitch_max: 2.0,
            max_instances: 3,
            steal: StealPolicy::Quietest,
            attenuation: attenuation(AttenuationModel::Exponential, 1.0, 50.0),
            ..looping(1, vec![0, 1, 2])
        },
        Sound {
            max_instances: 2,
            steal: StealPolicy::Refuse,
            priority: 10,
            ..sound(2, vec![1])
        },
        Sound {
            priority: 250,
            bus: 2,
            steal: StealPolicy::Oldest,
            max_instances: 4,
            ..looping(3, vec![2])
        },
    ];
    let b = bank(vec![dc(0.3, 40), ramp(333), dc(-0.2, 7)], sounds);
    let g = MixerGraph {
        buses: vec![
            bus(
                MASTER,
                None,
                1.0,
                vec![Effect::Limiter {
                    threshold: 0.9,
                    release_ms: 40.0,
                }],
            ),
            bus(
                SFX,
                Some(MASTER),
                0.8,
                vec![Effect::LowPass { cutoff_hz: 4000.0 }],
            ),
            bus(
                2,
                Some(SFX),
                1.2,
                vec![Effect::HighPass { cutoff_hz: 60.0 }, Effect::Gain { db: -3.0 }],
            ),
        ],
    };
    Ok(Mixer::new(
        &b,
        &g,
        MixerConfig {
            max_voices: 6,
            max_emitters: 4,
            ..config()
        },
    )?)
}

fn events(i: u32, handles: &HandleAllocator, last: mantis_audio::VoiceHandle) -> [AudioEvent; 4] {
    let x = (i % 17) as f32 - 8.0;
    let sound = SoundId(1 + i % 3);
    let emitter = EmitterId(i % 9);
    let play = AudioEvent::Play {
        sound,
        emitter: Some(emitter),
        position: (!i.is_multiple_of(4)).then_some([x, 0.0, -3.0]),
        volume: 0.5 + (i % 3) as f32 * 0.5,
        pitch: 0.5 + (i % 5) as f32 * 0.25,
        handle: handles.allocate(),
    };
    let control = match i % 6 {
        0 => AudioEvent::Stop {
            handle: last,
            fade_ms: (i % 20) as f32,
        },
        1 => AudioEvent::SetListener {
            position: [x * 0.1, 0.0, 0.0],
            forward: [x.sin(), 0.0, -x.cos()],
            up: [0.0, 1.0, 0.0],
        },
        2 => AudioEvent::SetBusGain {
            bus: BusId(i % 3),
            gain: (i % 4) as f32 * 0.5,
            fade_ms: 3.0,
        },
        3 => AudioEvent::Stop {
            handle: mantis_audio::VoiceHandle::from_bits(u64::from(i) / 2 + 1),
            fade_ms: 1.0,
        },
        4 if i % 60 == 4 => AudioEvent::StopAll,
        _ => AudioEvent::play(SoundId(2), handles.allocate()),
    };
    let moved = AudioEvent::SetEmitter {
        emitter,
        position: [-x, 1.0, 2.0],
    };
    [
        play,
        control,
        moved,
        AudioEvent::play(SoundId(3), handles.allocate()),
    ]
}

#[test]
fn handle_and_render_allocate_nothing() -> TestResult {
    let mut m = busy_mixer()?;
    let handles = HandleAllocator::new();
    // Buffers the host owns; one is odd and spans several blocks.
    let mut block = vec![0.0f32; 512];
    let mut odd = vec![0.0f32; 301];
    m.render(&mut block);
    let peak = assert_no_alloc("mixer handle and render", || {
        let mut peak = 0.0f32;
        let mut last = mantis_audio::VoiceHandle::NONE;
        for i in 0..2000u32 {
            for e in events(i, &handles, last) {
                if let AudioEvent::Play { handle, .. } = e {
                    last = handle;
                }
                m.handle(e);
            }
            let out = if i % 7 == 0 { &mut odd } else { &mut block };
            m.render(out);
            peak = out.iter().fold(peak, |p, s| p.max(s.abs()));
        }
        peak
    });
    let stats = m.stats();
    assert!(
        stats.plays > 1000 && stats.stolen > 100 && stats.refused > 100,
        "{stats:?}"
    );
    assert!(stats.ignored > 100, "stale stops were exercised: {stats:?}");
    assert!(peak > 0.0 && peak <= 0.9, "{peak}");
    Ok(())
}
