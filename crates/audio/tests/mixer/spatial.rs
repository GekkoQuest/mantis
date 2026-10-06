//! Spatialization through the mixer: panning, constant power, attenuation, listener and
//! emitter movement.

use core::f32::consts::FRAC_1_SQRT_2;

use mantis_audio::{AudioEvent, EmitterId, HandleAllocator, Mixer, SoundId, spatial};
use mantis_formats::sound_bank::{AttenuationModel, Sound};

use crate::support::{TestResult, attenuation, bank, close, dc, looping, max_step, mixer, render};

/// A looping spatial DC 1.0 sound with the given attenuation distances.
fn spatial_mixer(model: AttenuationModel, min: f32, max: f32) -> Result<Mixer, Box<dyn std::error::Error>> {
    mixer(&bank(
        vec![dc(1.0, 16)],
        vec![Sound {
            spatial: true,
            attenuation: attenuation(model, min, max),
            ..looping(1, vec![0])
        }],
    ))
}

/// The steady output of one voice of sound 1 at `position`.
fn level_at(m: &mut Mixer, position: Option<[f32; 3]>) -> [f32; 2] {
    let handle = HandleAllocator::new().allocate();
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: None,
        position,
        volume: 1.0,
        pitch: 1.0,
        handle,
    });
    let out = render(m, 8);
    m.handle(AudioEvent::StopAll);
    let _ = render(m, 512);
    out[7]
}

#[test]
fn hard_right_is_right_only() -> TestResult {
    let mut m = spatial_mixer(AttenuationModel::Linear, 10.0, 100.0)?;
    let [l, r] = level_at(&mut m, Some([10.0, 0.0, 0.0]));
    assert!(l.abs() < 1e-6 && close(r, 1.0, 1e-6), "{l} {r}");
    let [l, r] = level_at(&mut m, Some([-30.0, 0.0, 0.0]));
    assert!(r.abs() < 1e-6 && l > 0.0, "{l} {r}");
    Ok(())
}

#[test]
fn power_is_constant_across_azimuths() -> TestResult {
    let mut m = spatial_mixer(AttenuationModel::Inverse, 10.0, 100.0)?;
    for step in 0..24 {
        let angle = step as f32 * core::f32::consts::TAU / 24.0;
        let [l, r] = level_at(&mut m, Some([10.0 * angle.cos(), 0.0, 10.0 * angle.sin()]));
        assert!(close(l * l + r * r, 1.0, 1e-5), "angle {angle}: {l} {r}");
    }
    Ok(())
}

#[test]
fn source_at_listener_is_centered_at_full_volume() -> TestResult {
    let mut m = spatial_mixer(AttenuationModel::Exponential, 1.0, 10.0)?;
    for position in [Some([0.0; 3]), None] {
        let [l, r] = level_at(&mut m, position);
        assert!(close(l, FRAC_1_SQRT_2, 1e-6) && close(r, FRAC_1_SQRT_2, 1e-6));
    }
    Ok(())
}

#[test]
fn attenuation_curves_at_min_mid_max_distance() -> TestResult {
    let cases = [
        (AttenuationModel::Linear, [1.0, 0.5, 0.0]),
        (AttenuationModel::Inverse, [1.0, 1.0 / 6.0, 1.0 / 11.0]),
        (AttenuationModel::Exponential, [1.0, 1.0 / 6.0, 1.0 / 11.0]),
    ];
    for (model, expected) in cases {
        let mut m = spatial_mixer(model, 1.0, 11.0)?;
        let a = attenuation(model, 1.0, 11.0);
        for (distance, want) in [1.0f32, 6.0, 11.0].into_iter().zip(expected) {
            // Straight ahead (-z): centered, so each side is the gain over sqrt(2).
            let [l, r] = level_at(&mut m, Some([0.0, 0.0, -distance]));
            assert!(close(l, r, 1e-7));
            assert!(
                close(l, want * FRAC_1_SQRT_2, 1e-6),
                "{model:?} at {distance}: {l}"
            );
            assert!(close(spatial::attenuation(&a, distance), want, 1e-6));
        }
        // Beyond max distance the gain holds at its max-distance value.
        let [far, _] = level_at(&mut m, Some([0.0, 0.0, -500.0]));
        assert!(close(far, expected[2] * FRAC_1_SQRT_2, 1e-6));
    }
    Ok(())
}

#[test]
fn listener_orientation_moves_the_image() -> TestResult {
    let mut m = spatial_mixer(AttenuationModel::Linear, 10.0, 100.0)?;
    // Facing +x with +y up, the right ear points along -z (decision 0019).
    m.handle(AudioEvent::SetListener {
        position: [0.0; 3],
        forward: [1.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
    });
    let [l, r] = level_at(&mut m, Some([0.0, 0.0, 10.0]));
    assert!(r.abs() < 1e-6 && close(l, 1.0, 1e-6));
    // Moving the listener moves the reference point.
    m.handle(AudioEvent::SetListener {
        position: [0.0, 0.0, 20.0],
        forward: [1.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
    });
    let [l, r] = level_at(&mut m, Some([0.0, 0.0, 10.0]));
    assert!(l.abs() < 1e-6 && close(r, 1.0, 1e-6));
    Ok(())
}

#[test]
fn voices_follow_their_emitter_without_clicks() -> TestResult {
    let mut m = spatial_mixer(AttenuationModel::Linear, 10.0, 100.0)?;
    let e = EmitterId(4);
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: Some(e),
        position: Some([10.0, 0.0, 0.0]),
        volume: 1.0,
        pitch: 1.0,
        handle: HandleAllocator::new().allocate(),
    });
    let before = render(&mut m, 64);
    assert!(close(before[63][1], 1.0, 1e-6));
    m.handle(AudioEvent::SetEmitter {
        emitter: e,
        position: [-10.0, 0.0, 0.0],
    });
    let after = render(&mut m, 512);
    let last = after[511];
    assert!(close(last[0], 1.0, 1e-6) && last[1].abs() < 1e-6, "{last:?}");
    // The pan moved over the gain ramp, not in one step.
    assert!(max_step(&after) <= 1.0 / 240.0 + 1e-5, "{}", max_step(&after));
    // A later play naming only the emitter starts at its remembered position.
    m.handle(AudioEvent::StopAll);
    let _ = render(&mut m, 512);
    m.handle(AudioEvent::Play {
        sound: SoundId(1),
        emitter: Some(e),
        position: None,
        volume: 1.0,
        pitch: 1.0,
        handle: HandleAllocator::new().allocate(),
    });
    let out = render(&mut m, 4);
    assert!(close(out[3][0], 1.0, 1e-6) && out[3][1].abs() < 1e-6);
    Ok(())
}
