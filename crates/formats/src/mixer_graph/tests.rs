//! Mixer graph tests: round trip, every rejection rule, and a corruption sweep.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Master (limiter) <- music (low-pass), master <- effects (gain, high-pass) <- interface.
/// The interface bus is listed before its parent to show that order is free.
fn graph() -> MixerGraph {
    MixerGraph {
        buses: vec![
            Bus {
                id: 0,
                parent: None,
                gain: 1.0,
                effects: vec![Effect::Limiter {
                    threshold: 0.9,
                    release_ms: 50.0,
                }],
            },
            Bus {
                id: 1,
                parent: Some(0),
                gain: 0.5,
                effects: vec![Effect::LowPass { cutoff_hz: 2_000.0 }],
            },
            Bus {
                id: 3,
                parent: Some(2),
                gain: 1.0,
                effects: Vec::new(),
            },
            Bus {
                id: 2,
                parent: Some(0),
                gain: 2.0,
                effects: vec![Effect::Gain { db: -6.0 }, Effect::HighPass { cutoff_hz: 80.0 }],
            },
        ],
    }
}

/// Offset of bus record `i`.
fn bus(i: usize) -> usize {
    16 + 64 * i
}

/// Offset of effect slot `s` of bus `i`.
fn slot(i: usize, s: usize) -> usize {
    bus(i) + 16 + 12 * s
}

fn corrupt(at: usize, bytes: &[u8]) -> Result<MixerGraph, FormatError> {
    let mut b = graph().encode();
    if let Some(s) = b.get_mut(at..at + bytes.len()) {
        s.copy_from_slice(bytes);
    }
    MixerGraph::parse(&b)
}

fn f(v: f32) -> [u8; 4] {
    v.to_le_bytes()
}

fn u(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

#[test]
fn round_trips() -> TestResult {
    let g = graph();
    let bytes = g.encode();
    assert_eq!(bytes.len(), 16 + 4 * 64);
    let back = MixerGraph::parse(&bytes)?;
    assert_eq!(back, g);
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.master().map(|b| b.id), Some(0));
    assert_eq!(back.index_of(2), Some(3));
    assert_eq!(back.bus(3).and_then(|b| b.parent), Some(2));
    assert!(back.bus(9).is_none());
    Ok(())
}

#[test]
fn validate_checks_graphs_built_in_memory() {
    assert_eq!(graph().validate(), Ok(()));
    let mut cyclic = graph();
    if let Some(b) = cyclic.buses.get_mut(1) {
        b.parent = Some(1);
    }
    assert_eq!(cyclic.validate(), Err(FormatError::Inconsistent));
    let mut crowded = graph();
    if let Some(b) = crowded.buses.first_mut() {
        b.effects = vec![Effect::Gain { db: 0.0 }; MAX_EFFECTS + 1];
    }
    assert!(crowded.validate().is_err());
}

#[test]
fn rejects_malformed_header_and_buses() {
    assert_eq!(corrupt(0, b"XMIX").err(), Some(FormatError::Magic));
    assert_eq!(
        corrupt(4, &3u16.to_le_bytes()).err(),
        Some(FormatError::Version(3))
    );
    assert_eq!(corrupt(6, &2u16.to_le_bytes()).err(), Some(FormatError::Flags(2)));
    assert_eq!(corrupt(8, &u(0)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(8, &u(MAX_BUSES + 1)).err(), Some(FormatError::Dimensions));
    assert!(matches!(corrupt(8, &u(5)), Err(FormatError::Length { .. })));
    assert_eq!(corrupt(12, &u(1)).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(bus(1), &u(NO_PARENT)).err(),
        Some(FormatError::Inconsistent)
    );
    assert_eq!(corrupt(bus(1) + 8, &f(4.5)).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(bus(1) + 8, &f(-1.0)).err(), Some(FormatError::Dimensions));
    assert_eq!(
        corrupt(bus(1) + 8, &f(f32::NAN)).err(),
        Some(FormatError::NonFinite)
    );
    assert_eq!(corrupt(bus(1) + 12, &[5]).err(), Some(FormatError::Dimensions));
    assert_eq!(corrupt(bus(1) + 13, &[1]).err(), Some(FormatError::Reserved));
    let good = graph().encode();
    assert!(matches!(
        MixerGraph::parse(good.get(..good.len() - 1).unwrap_or(&[])),
        Err(FormatError::Length { .. })
    ));
    let mut longer = good.clone();
    longer.extend_from_slice(&[0; 4]);
    assert!(matches!(
        MixerGraph::parse(&longer),
        Err(FormatError::Length { .. })
    ));
}

#[test]
fn rejects_malformed_effects() {
    assert_eq!(corrupt(slot(1, 0), &[4]).err(), Some(FormatError::Encoding(4)));
    assert_eq!(corrupt(slot(1, 0) + 1, &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(
        corrupt(slot(1, 0) + 4, &f(0.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(1, 0) + 4, &f(96_001.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(1, 0) + 8, &f(1.0)).err(),
        Some(FormatError::Reserved)
    );
    assert_eq!(
        corrupt(slot(1, 0) + 8, &f(-0.0)).err(),
        Some(FormatError::Reserved)
    );
    assert_eq!(
        corrupt(slot(3, 0) + 4, &f(-97.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(3, 0) + 4, &f(25.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(3, 1) + 4, &f(-1.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(0, 0) + 4, &f(0.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(0, 0) + 4, &f(1.01)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(0, 0) + 8, &f(0.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(0, 0) + 8, &f(10_001.0)).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        corrupt(slot(0, 0) + 8, &f(f32::INFINITY)).err(),
        Some(FormatError::NonFinite)
    );
    // An unused slot must be entirely zero.
    assert_eq!(corrupt(slot(0, 1), &[1]).err(), Some(FormatError::Reserved));
    assert_eq!(corrupt(slot(2, 3) + 11, &[1]).err(), Some(FormatError::Reserved));
}

#[test]
fn rejects_bad_trees() {
    // Duplicate id.
    assert_eq!(corrupt(bus(2), &u(1)).err(), Some(FormatError::DuplicateId(1)));
    // Id equal to the master sentinel.
    assert_eq!(
        corrupt(bus(2), &u(NO_PARENT)).err(),
        Some(FormatError::Inconsistent)
    );
    // A second master.
    assert_eq!(
        corrupt(bus(1) + 4, &u(NO_PARENT)).err(),
        Some(FormatError::Inconsistent)
    );
    // No master.
    assert_eq!(corrupt(bus(0) + 4, &u(1)).err(), Some(FormatError::Inconsistent));
    // Missing parent.
    assert_eq!(corrupt(bus(2) + 4, &u(7)).err(), Some(FormatError::Inconsistent));
    // Self parent and a two-bus cycle (effects <- interface <- effects).
    assert_eq!(corrupt(bus(1) + 4, &u(1)).err(), Some(FormatError::Inconsistent));
    assert_eq!(corrupt(bus(3) + 4, &u(3)).err(), Some(FormatError::Inconsistent));
}

#[test]
fn no_corruption_or_truncation_panics() {
    let bytes = graph().encode();
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(g) = MixerGraph::parse(&b) {
                assert_eq!(MixerGraph::parse(&g.encode()).as_ref(), Ok(&g));
            }
        }
    }
    for len in 0..bytes.len() {
        assert!(
            MixerGraph::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
            "truncated to {len}"
        );
    }
}
