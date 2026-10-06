//! The native protocol codec: frames, delta snapshots, and the native adapter.

#![expect(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::{
    BASELINE_WINDOW_TICKS, BaselineStore, FRAME_MESSAGE, FRAME_SNAPSHOT, NativeAdapter, NoBaseline,
    ServerFrame, decode_server_frame, decode_snapshot, encode_inbound, encode_snapshot,
    encode_snapshot_based, peek_snapshot_ticks,
};
use mantis_adapter_contract::{
    AppearanceId, Inbound, LocalAvatar, MovementMode, Outbound, RemoteBases, RemoteSample, SetPosition,
    SnapshotAck, SnapshotFrame, SnapshotHeader, SnapshotVisitor, TransportKind, WireAdapter,
};
use mantis_core::rng::{Rng, Salt, Seed};

fn sample(i: u32, tick: u64, x: f32) -> RemoteSample {
    RemoteSample {
        id: EntityId::new(i, 0),
        tick: Tick(tick),
        position: Vec3::new(x, 1.0, -x),
        velocity: Vec3::new(2.5, 0.0, 0.0),
        yaw: Angle16((i * 100) as u16),
    }
}

fn frame(tick: u64, remotes: &[RemoteSample]) -> SnapshotFrame {
    let mut f = SnapshotFrame::with_capacity(8, 64, 8, 8);
    f.header(&SnapshotHeader {
        server_tick: Tick(tick),
        ack: Some(InputSeq(41)),
        local: Some(LocalAvatar {
            id: EntityId::new(0, 3),
            state: MotionState {
                position: Vec3::new(0.123_456_7, 2.0, -9.876_543),
                velocity: Vec3::new(1.0e-7, -3.0, 7.0),
                yaw: Angle16(12_345),
                grounded: false,
            },
        }),
        local_mods: MotionModifiers {
            speed_scale: 0.75,
            ..MotionModifiers::NONE
        },
    });
    for r in remotes {
        f.remote(r);
    }
    f
}

fn round_trip(
    f: &SnapshotFrame,
    baseline: Option<&SnapshotFrame>,
    store: &(impl BaselineStore + ?Sized),
) -> (SnapshotFrame, usize) {
    let mut bytes = Vec::new();
    encode_snapshot(f, baseline, &mut bytes);
    let mut out = SnapshotFrame::with_capacity(64, 64, 64, 64);
    decode_snapshot(&bytes, store, &mut out).unwrap();
    (out, bytes.len())
}

#[test]
fn full_snapshot_round_trip() {
    let mut f = frame(100, &[sample(1, 100, 10.0), sample(2, 98, -3.25)]);
    f.entered(EntityId::new(1, 0), AppearanceId(77));
    f.removed(EntityId::new(9, 2));
    f.marker(&TimelineMarker {
        id: MarkerId {
            graph: GraphId(5),
            node: NodeKey(2),
        },
        kind: MarkerKind::Impact {
            target: EntityId::new(2, 0),
        },
        at: Tick(105),
        offset: 5,
        source: EntityId::new(1, 0),
        target: Some(EntityId::new(2, 0)),
        instance: GraphInstanceId(31),
    });
    let (got, _) = round_trip(&f, None, &NoBaseline);
    // The local avatar is bit exact: never quantized.
    assert_eq!(
        got.header.local.unwrap().state.position.x.to_bits(),
        0.123_456_7f32.to_bits()
    );
    assert_eq!(
        got.header.local.unwrap().state.velocity.x.to_bits(),
        1.0e-7f32.to_bits()
    );
    assert_eq!(got.header, f.header);
    assert_eq!(
        got.entered.iter().copied().collect::<Vec<_>>(),
        vec![(EntityId::new(1, 0), AppearanceId(77))]
    );
    assert_eq!(
        got.removed.iter().copied().collect::<Vec<_>>(),
        vec![EntityId::new(9, 2)]
    );
    assert_eq!(
        got.markers.iter().copied().collect::<Vec<_>>(),
        f.markers.iter().copied().collect::<Vec<_>>()
    );
    let r = got.find_remote(EntityId::new(2, 0)).unwrap();
    assert_eq!(r.tick, Tick(98), "per-sample tick preserved");
    assert_eq!(
        r.position,
        Vec3::new(-3.25, 1.0, 3.25),
        "values on the 1/64 grid are exact"
    );
    assert_eq!(r.velocity, Vec3::new(2.5, 0.0, 0.0));
}

#[test]
fn quantization_error_is_bounded() {
    let mut rng = Rng::for_cell(Seed(3), Tick(0), Salt::named("test.native.quant"));
    for _ in 0..2000 {
        let x = (rng.next_f32() - 0.5) * 20_000.0;
        let v = (rng.next_f32() - 0.5) * 100.0;
        let mut r = sample(1, 10, 0.0);
        r.position = Vec3::new(x, -x, x * 0.5);
        r.velocity = Vec3::new(v, -v, 0.0);
        let (got, _) = round_trip(&frame(10, &[r]), None, &NoBaseline);
        let g = got.find_remote(r.id).unwrap();
        assert!(
            (g.position - r.position).length() <= 0.5 / 64.0 * 1.8,
            "{:?} vs {:?}",
            g.position,
            r.position
        );
        assert!((g.velocity - r.velocity).length() <= 0.5 / 256.0 * 1.8);
    }
}

#[test]
fn delta_against_baseline_is_smaller_and_exact() {
    let base_remotes: Vec<_> = (0..40).map(|i| sample(i, 100, i as f32)).collect();
    let base = frame(100, &base_remotes);
    // The client keeps what it decoded, which is what the server diffs against.
    let (client_base, full_len) = round_trip(&base, None, &NoBaseline);
    // Tick 101: half the entities moved, the rest are unchanged.
    let next_remotes: Vec<_> = (0..40)
        .map(|i| {
            if i % 2 == 0 {
                sample(i, 101, i as f32 + 0.25)
            } else {
                sample(i, 101, i as f32)
            }
        })
        .collect();
    let next = frame(101, &next_remotes);
    let (decoded_delta, delta_len) = round_trip(&next, Some(&base), &client_base);
    let (decoded_full, _) = round_trip(&next, None, &NoBaseline);
    assert!(delta_len * 2 < full_len, "delta {delta_len} vs full {full_len}");
    for r in &next_remotes {
        assert_eq!(
            decoded_delta.find_remote(r.id),
            decoded_full.find_remote(r.id),
            "delta and full decode agree"
        );
    }
    // A store holding several frames finds the right one.
    let frames = [client_base.clone()];
    let (from_slice, _) = round_trip(&next, Some(&base), &frames[..]);
    assert_eq!(from_slice.remotes.len(), 40);
}

#[test]
fn missing_baseline_and_malformed_frames_are_refused_before_visiting() {
    struct Count(u32);
    impl SnapshotVisitor for Count {
        fn header(&mut self, _: &SnapshotHeader) {
            self.0 += 1;
        }
        fn entered(&mut self, _: EntityId, _: AppearanceId) {
            self.0 += 1;
        }
        fn remote(&mut self, _: &RemoteSample) {
            self.0 += 1;
        }
        fn removed(&mut self, _: EntityId) {
            self.0 += 1;
        }
        fn marker(&mut self, _: &TimelineMarker) {
            self.0 += 1;
        }
    }
    let base = frame(100, &[sample(1, 100, 1.0)]);
    let next = frame(103, &[sample(1, 103, 2.0)]);
    let mut bytes = Vec::new();
    encode_snapshot(&next, Some(&base), &mut bytes);
    assert_eq!(peek_snapshot_ticks(&bytes), Ok((Tick(103), Some(Tick(100)))));
    let mut v = Count(0);
    assert_eq!(
        decode_snapshot(&bytes, &NoBaseline, &mut v),
        Err(DecodeError::Invalid("missing baseline"))
    );
    assert_eq!(v.0, 0, "visitor saw nothing");

    // Duplicate remote ids.
    let dup = frame(5, &[sample(1, 5, 1.0), sample(1, 5, 2.0)]);
    let mut bytes = Vec::new();
    encode_snapshot(&dup, None, &mut bytes);
    assert_eq!(
        decode_snapshot(&bytes, &NoBaseline, &mut v),
        Err(DecodeError::Invalid("duplicate remote id"))
    );
    assert_eq!(v.0, 0);

    // Fuzzed mutations never panic and never half-visit.
    let mut rng = Rng::for_cell(Seed(9), Tick(0), Salt::named("test.native.fuzz"));
    let good = {
        let mut f = frame(50, &(0..10).map(|i| sample(i, 50, i as f32)).collect::<Vec<_>>());
        f.entered(EntityId::new(3, 0), AppearanceId(1));
        let mut b = Vec::new();
        encode_snapshot(&f, None, &mut b);
        b
    };
    for _ in 0..5000 {
        let mut bad = good.clone();
        match rng.below(3) {
            0 => {
                let i = rng.below(bad.len() as u32) as usize;
                bad[i] ^= 1 << rng.below(8);
            }
            1 => bad.truncate(rng.below(bad.len() as u32) as usize),
            _ => bad.push(rng.next_u32() as u8),
        }
        let mut probe = Count(0);
        if decode_snapshot(&bad, &NoBaseline, &mut probe).is_err() {
            assert_eq!(probe.0, 0, "no partial visit");
        }
    }
}

#[test]
fn native_adapter_frames() {
    let adapter = NativeAdapter::new("test.native");
    assert_eq!(adapter.movement_mode(), MovementMode::Predictive);
    assert_eq!(adapter.transport(), TransportKind::Quic);
    let ack = Inbound::SnapshotAck(SnapshotAck { tick: Tick(7) });
    let mut bytes = Vec::new();
    encode_inbound(&ack, &mut bytes);
    assert_eq!(bytes[0], FRAME_MESSAGE);
    let mut got = Vec::new();
    adapter.decode(&bytes, &mut |m| got.push(m)).unwrap();
    assert_eq!(got, vec![ack]);
    assert!(
        adapter.decode(&[FRAME_SNAPSHOT], &mut |_| {}).is_err(),
        "clients never send snapshots"
    );
    assert!(adapter.decode(&[], &mut |_| {}).is_err());

    let correction = Outbound::SetPosition(SetPosition {
        entity: EntityId::new(1, 0),
        position: Vec3::Y,
        yaw: Angle16(9),
        tick: Tick(3),
    });
    let mut out = Vec::new();
    adapter.encode_outbound(&correction, &mut out).unwrap();
    let mut sink = SnapshotFrame::with_capacity(1, 1, 1, 1);
    assert!(matches!(
        decode_server_frame(&out, &NoBaseline, &mut sink),
        Ok(ServerFrame::Message(m)) if m == correction
    ));
    let mut snap = Vec::new();
    adapter
        .encode_snapshot(&frame(4, &[sample(1, 4, 0.0)]), None, &mut snap)
        .unwrap();
    assert!(matches!(
        decode_server_frame(&snap, &NoBaseline, &mut sink),
        Ok(ServerFrame::Snapshot)
    ));
    assert_eq!(sink.header.server_tick, Tick(4));
}

/// Per-remote baselines for the encoder: one acknowledged frame.
struct OneBase<'a>(&'a SnapshotFrame);

impl RemoteBases for OneBase<'_> {
    fn base_for(&self, id: EntityId) -> Option<(Tick, &RemoteSample)> {
        self.0.find_remote(id).map(|r| (self.0.header.server_tick, r))
    }
}

#[test]
fn a_remote_missing_from_the_baseline_deltas_against_its_own_acknowledged_frame() {
    // Tick 100: all 40 remotes. Tick 110 (the frame-level baseline): the
    // budget dropped the odd ones. Tick 112: all 40 again.
    let all = |tick: u64, dx: f32| {
        (0..40)
            .map(|i| sample(i, tick, i as f32 + dx))
            .collect::<Vec<_>>()
    };
    let old = frame(100, &all(100, 0.0));
    let even: Vec<_> = all(110, 0.5)
        .into_iter()
        .filter(|r| r.id.index() % 2 == 0)
        .collect();
    let base = frame(110, &even);
    let next = frame(112, &all(112, 0.75));
    // The client holds what it decoded.
    let (client_old, _) = round_trip(&old, None, &NoBaseline);
    let (client_base, _) = round_trip(&base, None, &NoBaseline);
    let held = [client_old.clone(), client_base.clone()];

    let mut plain = Vec::new();
    encode_snapshot(&next, Some(&base), &mut plain);
    let mut based = Vec::new();
    encode_snapshot_based(&next, Some(&base), &OneBase(&old), &mut based);
    assert!(
        based.len() < plain.len(),
        "own bases {} vs frame baseline only {}",
        based.len(),
        plain.len()
    );

    let mut from_based = SnapshotFrame::with_capacity(64, 64, 64, 64);
    decode_snapshot(&based, &held[..], &mut from_based).unwrap();
    let (from_full, _) = round_trip(&next, None, &NoBaseline);
    assert_eq!(from_based.remotes.len(), 40);
    for i in 0..40 {
        let id = EntityId::new(i, 0);
        assert_eq!(
            from_based.find_remote(id),
            from_full.find_remote(id),
            "remote {i} decodes exactly"
        );
    }

    // The client lacks the own base frame: refused before visiting.
    let mut probe = SnapshotFrame::with_capacity(64, 64, 64, 64);
    assert_eq!(
        decode_snapshot(&based, &[client_base][..], &mut probe),
        Err(DecodeError::Invalid("missing own base"))
    );
    assert!(probe.remotes.is_empty(), "nothing visited");

    // Beyond the window the remote is sent in full, as without own bases.
    let late = frame(
        100 + BASELINE_WINDOW_TICKS + 1,
        &all(100 + BASELINE_WINDOW_TICKS + 1, 0.75),
    );
    let mut far = Vec::new();
    encode_snapshot_based(&late, None, &OneBase(&old), &mut far);
    let mut full = Vec::new();
    encode_snapshot(&late, None, &mut full);
    assert_eq!(far, full, "no own base past {BASELINE_WINDOW_TICKS} ticks");
    // At the window's edge it is still used.
    let edge = frame(
        100 + BASELINE_WINDOW_TICKS,
        &all(100 + BASELINE_WINDOW_TICKS, 0.75),
    );
    let mut near = Vec::new();
    encode_snapshot_based(&edge, None, &OneBase(&old), &mut near);
    let mut out = SnapshotFrame::with_capacity(64, 64, 64, 64);
    decode_snapshot(&near, &[client_old][..], &mut out).unwrap();
    assert_eq!(out.remotes.len(), 40);
}
