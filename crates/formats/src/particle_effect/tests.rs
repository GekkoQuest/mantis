//! Particle effect tests: round trip, every rejection rule, curves, and a corruption sweep.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn sparks() -> EmitterDef {
    EmitterDef {
        capacity: 256,
        duration: 1.0,
        looping: false,
        rate: 0.0,
        bursts: vec![Burst { time: 0.0, count: 64 }, Burst { time: 0.5, count: 32 }],
        lifetime_min: 0.4,
        lifetime_max: 0.8,
        shape: EmitterShape::Cone {
            angle: 0.6,
            radius: 0.1,
        },
        speed_min: 3.0,
        speed_max: 6.0,
        acceleration: [0.0, -9.81, 0.0],
        drag: 0.5,
        color: vec![
            ColorKey {
                t: 0.0,
                value: [4.0, 2.0, 0.5, 1.0],
            },
            ColorKey {
                t: 1.0,
                value: [1.0, 0.2, 0.0, 0.0],
            },
        ],
        size: vec![SizeKey {
            t: 0.0,
            value: [0.05],
        }],
        blend: BlendMode::Additive,
        space: SimulationSpace::World,
    }
}

fn smoke() -> EmitterDef {
    EmitterDef {
        capacity: 128,
        duration: 2.0,
        looping: true,
        rate: 20.0,
        bursts: Vec::new(),
        lifetime_min: 2.0,
        lifetime_max: 3.0,
        shape: EmitterShape::Sphere { radius: 0.3 },
        speed_min: 0.2,
        speed_max: 0.5,
        acceleration: [0.0, 0.4, 0.0],
        drag: 0.1,
        color: vec![
            ColorKey {
                t: 0.0,
                value: [0.3, 0.3, 0.3, 0.0],
            },
            ColorKey {
                t: 0.2,
                value: [0.3, 0.3, 0.3, 0.6],
            },
            ColorKey {
                t: 1.0,
                value: [0.2, 0.2, 0.2, 0.0],
            },
        ],
        size: vec![
            SizeKey { t: 0.0, value: [0.0] },
            SizeKey { t: 0.5, value: [1.0] },
            SizeKey { t: 1.0, value: [2.0] },
        ],
        blend: BlendMode::Alpha,
        space: SimulationSpace::Local,
    }
}

fn glow() -> EmitterDef {
    EmitterDef {
        capacity: 16,
        duration: 0.5,
        looping: true,
        rate: 8.0,
        bursts: vec![Burst { time: 0.25, count: 2 }],
        lifetime_min: 1.0,
        lifetime_max: 1.0,
        shape: EmitterShape::Point,
        speed_min: 0.0,
        speed_max: 0.0,
        acceleration: [0.0; 3],
        drag: 0.0,
        color: vec![ColorKey {
            t: 0.5,
            value: [1.0, 1.0, 2.0, 1.0],
        }],
        size: vec![SizeKey { t: 0.0, value: [0.5] }, SizeKey { t: 1.0, value: [0.0] }],
        blend: BlendMode::Additive,
        space: SimulationSpace::World,
    }
}

fn effect() -> ParticleEffect {
    ParticleEffect {
        emitters: vec![sparks(), smoke(), glow()],
    }
}

/// Parses `e` after encoding; the error, if any.
fn rejects(e: &ParticleEffect) -> Option<FormatError> {
    ParticleEffect::parse(&e.encode()).err()
}

/// An effect whose first emitter is `sparks()` changed by `f`.
fn with_sparks(f: impl FnOnce(&mut EmitterDef)) -> ParticleEffect {
    let mut s = sparks();
    f(&mut s);
    ParticleEffect { emitters: vec![s] }
}

#[test]
fn round_trips() -> TestResult {
    let e = effect();
    e.validate()?;
    let bytes = e.encode();
    // Header, then each record: fixed part, bursts, color keys, size keys.
    let expected = 16 + (60 + 16 + 40 + 8) + (60 + 60 + 24) + (60 + 8 + 20 + 16);
    assert_eq!(bytes.len(), expected);
    assert_eq!(ParticleEffect::parse(&bytes)?, e);
    assert_eq!(e.capacity_total(), 256 + 128 + 16);
    assert_eq!(bytes.get(..4), Some(&b"MPFX"[..]));
    Ok(())
}

#[test]
fn header_rules_reject() {
    let bytes = effect().encode();
    let corrupt = |at: usize, v: &[u8]| {
        let mut b = bytes.clone();
        if let Some(s) = b.get_mut(at..at + v.len()) {
            s.copy_from_slice(v);
        }
        ParticleEffect::parse(&b).err()
    };
    assert_eq!(corrupt(0, b"MPFY"), Some(FormatError::Magic));
    assert_eq!(corrupt(4, &2u16.to_le_bytes()), Some(FormatError::Version(2)));
    assert_eq!(corrupt(6, &4u16.to_le_bytes()), Some(FormatError::Flags(4)));
    assert_eq!(corrupt(8, &0u32.to_le_bytes()), Some(FormatError::Dimensions));
    assert_eq!(corrupt(8, &9u32.to_le_bytes()), Some(FormatError::Dimensions));
    assert!(matches!(
        corrupt(8, &4u32.to_le_bytes()),
        Some(FormatError::Length { .. })
    ));
    assert_eq!(corrupt(12, &1u32.to_le_bytes()), Some(FormatError::Reserved));
    let mut long = bytes.clone();
    long.push(0);
    assert!(matches!(
        ParticleEffect::parse(&long),
        Err(FormatError::Length { .. })
    ));
    let nine = ParticleEffect {
        emitters: vec![glow(); 9],
    };
    assert_eq!(nine.validate(), Err(FormatError::Dimensions));
    assert_eq!(rejects(&nine), Some(FormatError::Dimensions));
    let empty = ParticleEffect { emitters: Vec::new() };
    assert_eq!(rejects(&empty), Some(FormatError::Dimensions));
}

#[test]
fn record_bytes_reject() {
    // A single-emitter effect: the record starts at 16.
    let bytes = with_sparks(|_| {}).encode();
    let corrupt = |at: usize, v: &[u8]| {
        let mut b = bytes.clone();
        if let Some(s) = b.get_mut(16 + at..16 + at + v.len()) {
            s.copy_from_slice(v);
        }
        ParticleEffect::parse(&b).err()
    };
    assert_eq!(ParticleEffect::parse(&bytes).err(), None);
    assert_eq!(corrupt(44, &[3]), Some(FormatError::Encoding(3)), "shape");
    assert_eq!(corrupt(45, &[2]), Some(FormatError::Encoding(2)), "blend");
    assert_eq!(corrupt(46, &[7]), Some(FormatError::Encoding(7)), "space");
    assert_eq!(corrupt(47, &[2]), Some(FormatError::Validity), "looping");
    assert_eq!(corrupt(59, &[1]), Some(FormatError::Reserved));
    assert_eq!(corrupt(56, &[9]), Some(FormatError::Dimensions), "bursts");
    assert_eq!(corrupt(57, &[0]), Some(FormatError::Keyframes), "no colors");
    assert_eq!(corrupt(58, &[5]), Some(FormatError::Keyframes), "sizes");
    assert_eq!(corrupt(4, &f32::NAN.to_le_bytes()), Some(FormatError::NonFinite));
    assert_eq!(
        corrupt(32, &f32::INFINITY.to_le_bytes()),
        Some(FormatError::NonFinite)
    );
    // Point and sphere shapes carry zeros in their unused parameters.
    let point = with_sparks(|s| s.shape = EmitterShape::Point).encode();
    let mut p = point.clone();
    if let Some(x) = p.get_mut(16 + 48) {
        *x = 1;
    }
    assert_eq!(ParticleEffect::parse(&p).err(), Some(FormatError::Reserved));
    let sphere = with_sparks(|s| s.shape = EmitterShape::Sphere { radius: 1.0 }).encode();
    let mut q = sphere.clone();
    if let Some(x) = q.get_mut(16 + 55) {
        *x = 0x80;
    }
    assert_eq!(ParticleEffect::parse(&q).err(), Some(FormatError::Reserved));
    assert!(ParticleEffect::parse(&point).is_ok() && ParticleEffect::parse(&sphere).is_ok());
}

#[test]
#[expect(clippy::too_many_lines)] // One declarative table of rule cases.
fn value_rules_reject() {
    let d = FormatError::Dimensions;
    let k = FormatError::Keyframes;
    let cases: Vec<(&str, ParticleEffect, FormatError)> = vec![
        ("capacity 0", with_sparks(|s| s.capacity = 0), d),
        ("capacity max", with_sparks(|s| s.capacity = MAX_CAPACITY + 1), d),
        ("duration 0", with_sparks(|s| s.duration = 0.0), d),
        ("rate negative", with_sparks(|s| s.rate = -1.0), d),
        ("lifetime 0", with_sparks(|s| s.lifetime_min = 0.0), d),
        ("lifetime order", with_sparks(|s| s.lifetime_max = 0.1), d),
        ("speed order", with_sparks(|s| s.speed_min = 7.0), d),
        ("drag negative", with_sparks(|s| s.drag = -0.1), d),
        (
            "sphere radius",
            with_sparks(|s| s.shape = EmitterShape::Sphere { radius: -1.0 }),
            d,
        ),
        (
            "cone angle 0",
            with_sparks(|s| {
                s.shape = EmitterShape::Cone {
                    angle: 0.0,
                    radius: 0.0,
                };
            }),
            d,
        ),
        (
            "cone angle past pi",
            with_sparks(|s| {
                s.shape = EmitterShape::Cone {
                    angle: 3.2,
                    radius: 0.0,
                };
            }),
            d,
        ),
        (
            "cone radius",
            with_sparks(|s| {
                s.shape = EmitterShape::Cone {
                    angle: 1.0,
                    radius: -0.5,
                };
            }),
            d,
        ),
        (
            "too many bursts",
            with_sparks(|s| s.bursts = vec![Burst { time: 0.0, count: 1 }; 9]),
            d,
        ),
        ("burst unsorted", with_sparks(|s| s.bursts.reverse()), k),
        (
            "burst negative time",
            with_sparks(|s| s.bursts = vec![Burst { time: -0.1, count: 1 }]),
            k,
        ),
        (
            "burst at duration",
            with_sparks(|s| s.bursts = vec![Burst { time: 1.0, count: 1 }]),
            k,
        ),
        (
            "burst count 0",
            with_sparks(|s| s.bursts = vec![Burst { time: 0.0, count: 0 }]),
            d,
        ),
        (
            "burst past capacity",
            with_sparks(|s| {
                s.bursts = vec![Burst {
                    time: 0.0,
                    count: 257,
                }];
            }),
            d,
        ),
        (
            "never emits",
            with_sparks(|s| s.bursts.clear()),
            FormatError::Inconsistent,
        ),
        ("no color keys", with_sparks(|s| s.color.clear()), k),
        (
            "too many color keys",
            with_sparks(|s| {
                s.color = (0..5u8)
                    .map(|i| ColorKey {
                        t: f32::from(i) * 0.2,
                        value: [1.0; 4],
                    })
                    .collect();
            }),
            k,
        ),
        (
            "color t repeats",
            with_sparks(|s| {
                if let Some(c) = s.color.get_mut(1) {
                    c.t = 0.0;
                }
            }),
            k,
        ),
        (
            "color t past 1",
            with_sparks(|s| {
                if let Some(c) = s.color.get_mut(1) {
                    c.t = 1.5;
                }
            }),
            k,
        ),
        (
            "color negative",
            with_sparks(|s| {
                if let Some(c) = s.color.get_mut(0) {
                    c.value[1] = -0.1;
                }
            }),
            d,
        ),
        (
            "alpha past 1",
            with_sparks(|s| {
                if let Some(c) = s.color.get_mut(0) {
                    c.value[3] = 1.5;
                }
            }),
            d,
        ),
        ("no size keys", with_sparks(|s| s.size.clear()), k),
        (
            "size t order",
            with_sparks(|s| {
                s.size = vec![SizeKey { t: 0.5, value: [1.0] }, SizeKey { t: 0.2, value: [1.0] }];
            }),
            k,
        ),
        (
            "size negative",
            with_sparks(|s| {
                s.size = vec![SizeKey {
                    t: 0.0,
                    value: [-1.0],
                }];
            }),
            d,
        ),
        (
            "interior size 0",
            with_sparks(|s| {
                s.size = vec![
                    SizeKey { t: 0.0, value: [1.0] },
                    SizeKey { t: 0.5, value: [0.0] },
                    SizeKey { t: 1.0, value: [1.0] },
                ];
            }),
            d,
        ),
        (
            "every size 0",
            with_sparks(|s| s.size = vec![SizeKey { t: 0.0, value: [0.0] }]),
            d,
        ),
    ];
    for (name, e, expected) in cases {
        assert_eq!(e.validate(), Err(expected), "{name}: validate");
        assert_eq!(rejects(&e), Some(expected), "{name}: parse");
    }
    // Constructed values are checked for finiteness too.
    let nan = with_sparks(|s| s.acceleration[1] = f32::NAN);
    assert_eq!(nan.validate(), Err(FormatError::NonFinite));
    assert_eq!(rejects(&nan), Some(FormatError::NonFinite));
    // Boundary values are accepted.
    let edge = with_sparks(|s| {
        s.capacity = MAX_CAPACITY;
        s.shape = EmitterShape::Cone {
            angle: core::f32::consts::PI,
            radius: 0.0,
        };
        s.lifetime_max = s.lifetime_min;
        s.speed_min = -1.0;
        s.speed_max = -1.0;
    });
    assert_eq!(edge.validate(), Ok(()));
    assert_eq!(rejects(&edge), None);
}

#[test]
fn curves_interpolate_and_clamp() {
    let keys = smoke().size;
    let at = |t: f32| evaluate_curve(&keys, t)[0];
    assert!((at(-1.0) - 0.0).abs() < 1e-6, "clamped before the first key");
    assert!((at(0.25) - 0.5).abs() < 1e-6);
    assert!((at(0.5) - 1.0).abs() < 1e-6);
    assert!((at(0.75) - 1.5).abs() < 1e-6);
    assert!((at(2.0) - 2.0).abs() < 1e-6, "clamped after the last key");
    assert!((at(f32::NAN) - 0.0).abs() < 1e-6);
    let one = glow().color;
    assert_eq!(evaluate_curve(&one, 0.0), [1.0, 1.0, 2.0, 1.0]);
    assert_eq!(evaluate_curve(&one, 0.9), [1.0, 1.0, 2.0, 1.0]);
    let colors = smoke().color;
    let mid = evaluate_curve(&colors, 0.1);
    assert!((mid[3] - 0.3).abs() < 1e-6, "{mid:?}");
    assert_eq!(evaluate_curve::<2>(&[], 0.5), [0.0, 0.0]);
}

#[test]
fn no_corruption_or_truncation_panics() {
    let bytes = effect().encode();
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(e) = ParticleEffect::parse(&b) {
                assert_eq!(e.validate(), Ok(()));
                assert_eq!(ParticleEffect::parse(&e.encode()).as_ref(), Ok(&e));
            }
        }
    }
    for len in 0..bytes.len() {
        assert!(
            ParticleEffect::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
            "truncated to {len}"
        );
    }
}
