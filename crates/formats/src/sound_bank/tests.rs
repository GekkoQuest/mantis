//! Sound bank tests: round trip, every rejection rule, and a corruption sweep.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn attenuation() -> Attenuation {
    Attenuation {
        model: AttenuationModel::Inverse,
        min_distance: 1.0,
        max_distance: 50.0,
        rolloff: 1.0,
    }
}

/// Two sounds over two clips: a mono `i16` clip with an odd frame count (so it carries
/// padding) and a stereo `f32` clip.
fn bank() -> SoundBank {
    SoundBank {
        sample_rate: 48_000,
        clips: vec![
            Clip {
                channels: 1,
                samples: ClipSamples::I16(vec![0, 16_384, -32_768]),
            },
            Clip {
                channels: 2,
                samples: ClipSamples::F32(vec![0.5, -0.5, 1.0, -1.0]),
            },
        ],
        sounds: vec![
            Sound {
                id: 10,
                clips: vec![0, 1],
                selection: ClipSelection::Random,
                volume: 0.8,
                pitch_min: 0.9,
                pitch_max: 1.1,
                looping: false,
                spatial: true,
                bus: 2,
                priority: 100,
                max_instances: 4,
                steal: StealPolicy::Quietest,
                attenuation: attenuation(),
            },
            Sound {
                id: 20,
                clips: vec![1],
                selection: ClipSelection::RoundRobin,
                volume: 1.0,
                pitch_min: 1.0,
                pitch_max: 1.0,
                looping: true,
                spatial: false,
                bus: 0,
                priority: 7,
                max_instances: 1,
                steal: StealPolicy::Refuse,
                attenuation: Attenuation {
                    model: AttenuationModel::Exponential,
                    ..attenuation()
                },
            },
        ],
    }
}

// Byte offsets in `bank().encode()`.
const S0: usize = 24;
const S1: usize = 132;
const C0: usize = 240;
const C1: usize = 256;
const LEN: usize = 280;

#[test]
fn round_trips() -> TestResult {
    let b = bank();
    let bytes = b.encode();
    assert_eq!(bytes.len(), LEN);
    let back = SoundBank::parse(&bytes)?;
    assert_eq!(back, b);
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.sound(20).map(|s| s.looping), Some(true));
    assert!(back.sound(30).is_none());
    let pcm = back.clips.first().ok_or("clip")?.samples.to_f32();
    assert_eq!(pcm, vec![0.0, 0.5, -1.0]);
    assert_eq!(back.clips.get(1).map(Clip::frames), Some(2));
    let empty = SoundBank {
        sample_rate: 8_000,
        clips: Vec::new(),
        sounds: Vec::new(),
    };
    assert_eq!(SoundBank::parse(&empty.encode())?, empty);
    Ok(())
}

#[test]
fn validate_checks_banks_built_in_memory() {
    assert_eq!(bank().validate(), Ok(()));
    let mut loud = bank();
    if let Some(s) = loud.sounds.first_mut() {
        s.volume = 9.0;
    }
    assert_eq!(loud.validate(), Err(FormatError::Dimensions));
    let mut many = bank();
    if let Some(s) = many.sounds.first_mut() {
        s.clips = vec![0; MAX_CLIPS_PER_SOUND + 1];
    }
    assert!(many.validate().is_err());
    let mut ragged = bank();
    if let Some(c) = ragged.clips.get_mut(1) {
        c.samples = ClipSamples::F32(vec![0.0; 3]);
    }
    assert!(ragged.validate().is_err(), "samples not a multiple of channels");
}

fn corrupt(at: usize, bytes: &[u8]) -> Result<SoundBank, FormatError> {
    let mut b = bank().encode();
    if let Some(s) = b.get_mut(at..at + bytes.len()) {
        s.copy_from_slice(bytes);
    }
    SoundBank::parse(&b)
}

fn f(v: f32) -> [u8; 4] {
    v.to_le_bytes()
}

fn u(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

#[test]
fn rejects_malformed_header() {
    assert_eq!(corrupt(0, b"XSBK").err(), Some(FormatError::Magic));
    assert_eq!(
        corrupt(4, &2u16.to_le_bytes()).err(),
        Some(FormatError::Version(2))
    );
    assert_eq!(corrupt(6, &1u16.to_le_bytes()).err(), Some(FormatError::Flags(1)));
    assert_eq!(corrupt(8, &u(7_999)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(8, &u(192_001)).err(), Some(FormatError::Dimensions));
    assert_eq!(
        corrupt(12, &u(MAX_CLIPS + 1)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(16, &u(MAX_SOUNDS + 1)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(corrupt(20, &u(1)).err(), Some(FormatError::Reserved));
    assert!(matches!(corrupt(16, &u(1000)), Err(FormatError::Length { .. })));
    let good = bank().encode();
    assert!(matches!(
        SoundBank::parse(good.get(..LEN - 1).unwrap_or(&[])),
        Err(FormatError::Length { .. })
    ));
    let mut longer = good.clone();
    longer.push(0);
    assert!(matches!(
        SoundBank::parse(&longer),
        Err(FormatError::Length { .. })
    ));
}

#[test]
fn rejects_malformed_sound_fields() {
    assert_eq!(corrupt(S0 + 8, &f(4.5)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 8, &f(-0.1)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 8, &f(f32::NAN)).err(), Some(FormatError::NonFinite));
    assert_eq!(corrupt(S0 + 12, &f(0.2)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 16, &f(4.5)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 12, &f(1.2)).err(), Some(FormatError::Inconsistent));
    assert_eq!(corrupt(S0 + 20, &f(0.0)).err(), Some(FormatError::Geometry));
    assert_eq!(corrupt(S0 + 24, &f(1.0)).err(), Some(FormatError::Geometry));
    assert_eq!(corrupt(S0 + 28, &f(0.0)).err(), Some(FormatError::Geometry));
    assert_eq!(
        corrupt(S0 + 28, &f(f32::INFINITY)).err(),
        Some(FormatError::NonFinite)
    );
    assert_eq!(
        corrupt(S0 + 32, &SOUND_STREAMED_RESERVED.to_le_bytes()).err(),
        Some(FormatError::Flags(4))
    );
    assert_eq!(
        corrupt(S0 + 32, &0x8002u16.to_le_bytes()).err(),
        Some(FormatError::Flags(0x8002))
    );
    assert_eq!(
        corrupt(S0 + 34, &0u16.to_le_bytes()).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(corrupt(S0 + 37, &[3]).err(), Some(FormatError::Encoding(3)));
    assert_eq!(corrupt(S0 + 38, &[2]).err(), Some(FormatError::Encoding(2)));
    assert_eq!(corrupt(S0 + 39, &[9]).err(), Some(FormatError::Encoding(9)));
    assert_eq!(corrupt(S0 + 40, &[0]).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 40, &[17]).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(S0 + 41, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(corrupt(S0 + 48, &u(2)).err(), Some(FormatError::Inconsistent));
    assert_eq!(corrupt(S0 + 52, &u(1)).err(), Some(FormatError::Reserved));
    assert_eq!(corrupt(S1, &u(10)).err(), Some(FormatError::DuplicateId(10)));
}

#[test]
fn rejects_malformed_clips() {
    assert_eq!(corrupt(C0, &[3]).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(C0, &[0]).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(C0 + 1, &[3]).err(), Some(FormatError::Encoding(3)));
    assert_eq!(corrupt(C0 + 2, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(corrupt(C0 + 4, &u(0)).err(), Some(FormatError::Dimensions));
    assert_eq!(
        corrupt(C0 + 4, &u(MAX_CLIP_FRAMES + 1)).err(),
        Some(FormatError::Dimensions)
    );
    assert!(matches!(
        corrupt(C0 + 4, &u(4000)),
        Err(FormatError::Length { .. })
    ));
    // Padding after the odd-length mono clip.
    assert_eq!(corrupt(C0 + 14, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(corrupt(C1 + 8, &f(1.5)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(C1 + 12, &f(f32::NAN)).err(), Some(FormatError::NonFinite));
    assert_eq!(
        corrupt(C1 + 12, &f(f32::NEG_INFINITY)).err(),
        Some(FormatError::NonFinite)
    );
}

#[test]
fn no_corruption_or_truncation_panics() {
    // Every single-byte corruption and every truncation either parses (to a bank that
    // re-encodes and re-parses) or is rejected; nothing panics or reads out of bounds.
    let bytes = bank().encode();
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(s) = SoundBank::parse(&b) {
                assert_eq!(SoundBank::parse(&s.encode()).as_ref(), Ok(&s));
            }
        }
    }
    for len in 0..bytes.len() {
        assert!(
            SoundBank::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
            "truncated to {len}"
        );
    }
}
