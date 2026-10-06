use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::{
    AbilityId, AdapterError, AppearanceId, Cast, Inbound, LocalAvatar, MoveClaim, Outbound, PROTOCOL_VERSION,
    RefuseReason, RemoteSample, SetPosition, SnapshotFrame, WireAdapter,
};

use super::client::{ServerPacket, decode_server, encode_inbound};
use super::{LegacyAdapter, LegacyConfig, op, packet};

const BUILD: u32 = 0x0203;

fn adapter() -> LegacyAdapter {
    LegacyAdapter::new(LegacyConfig {
        build: BUILD,
        content: ContentHash::from_bytes([9; 32]),
    })
}

fn decode_all(a: &LegacyAdapter, bytes: &[u8]) -> Result<Vec<Inbound>, AdapterError> {
    let mut v = Vec::new();
    a.decode(bytes, &mut |m| v.push(m))?;
    Ok(v)
}

#[test]
fn client_messages_round_trip_through_the_adapter() {
    let a = adapter();
    let mut frame = Vec::new();
    let hello = Inbound::Hello(mantis_adapter_contract::Hello {
        protocol: PROTOCOL_VERSION,
        capabilities: 0,
        content: ContentHash::from_bytes([0; 32]),
        modules: BoundedArray::new(),
        token: BoundedArray::from_slice(b"secret").unwrap(),
    });
    let claim = Inbound::MoveClaim(MoveClaim {
        position: Vec3::new(1.5, 0.25, -3.0),
        client_time_ms: 123_456,
    });
    let cast = Inbound::Cast(Cast {
        ability: AbilityId(7),
        target: Some(EntityId::new(4, 2)),
        view_tick: Tick(99),
        view_frac: 0,
    });
    // Several packets in one frame, as legacy clients batch them.
    for m in [&hello, &claim, &cast] {
        assert!(encode_inbound(m, BUILD, &mut frame));
    }
    let got = decode_all(&a, &frame).unwrap();
    assert_eq!(got.len(), 3);
    match got[0] {
        Inbound::Hello(h) => {
            assert_eq!(
                h.protocol, PROTOCOL_VERSION,
                "the configured build maps to this protocol"
            );
            assert_eq!(
                h.content,
                ContentHash::from_bytes([9; 32]),
                "the build maps to the served content"
            );
            assert_eq!(h.token.iter().copied().collect::<Vec<u8>>(), b"secret");
        }
        _ => panic!("expected a hello"),
    }
    assert_eq!(got[1], claim);
    assert_eq!(got[2], cast);
    // Native-only messages have no legacy form.
    let mut out = Vec::new();
    assert!(!encode_inbound(
        &Inbound::SnapshotAck(mantis_adapter_contract::SnapshotAck { tick: Tick(1) }),
        BUILD,
        &mut out
    ));
    assert!(out.is_empty());
}

#[test]
fn an_unknown_build_is_announced_as_an_unknown_protocol() {
    let mut frame = Vec::new();
    packet(&mut frame, op::LOGIN, |e| {
        e.u32(BUILD + 1);
        e.u8(1);
        e.u8(b'x');
    });
    match decode_all(&adapter(), &frame).unwrap()[0] {
        Inbound::Hello(h) => assert_ne!(h.protocol, PROTOCOL_VERSION),
        _ => panic!("expected a hello"),
    }
}

#[test]
fn malformed_frames_fail_closed() {
    let a = adapter();
    // Size smaller than the header.
    assert!(decode_all(&a, &[2, 0, 1, 0]).is_err());
    // Truncated body.
    let mut f = Vec::new();
    packet(&mut f, op::POS_REPORT, |e| e.f32(1.0));
    assert!(decode_all(&a, &f).is_err());
    // Trailing bytes inside a packet.
    let mut f = Vec::new();
    packet(&mut f, op::LOGOUT, |e| e.u8(0));
    assert!(decode_all(&a, &f).is_err());
    // Unknown opcode, and a server opcode sent by a client.
    for code in [0x0077, op::SET_POS] {
        let mut f = Vec::new();
        packet(&mut f, code, |_| {});
        assert!(matches!(decode_all(&a, &f), Err(AdapterError::Protocol(_))));
    }
    // A non-finite position.
    let mut f = Vec::new();
    packet(&mut f, op::POS_REPORT, |e| {
        e.f32(f32::NAN);
        e.f32(0.0);
        e.f32(0.0);
        e.u16(0);
        e.u32(0);
    });
    assert!(decode_all(&a, &f).is_err());
    // An oversized token.
    let mut f = Vec::new();
    packet(&mut f, op::LOGIN, |e| {
        e.u32(BUILD);
        e.u8(65);
        e.bytes(&[0; 65]);
    });
    assert!(decode_all(&a, &f).is_err());
}

#[test]
fn snapshots_and_corrections_decode_on_the_client() {
    let a = adapter();
    let mut frame = SnapshotFrame::with_capacity(4, 4, 4, 4);
    frame.header.server_tick = Tick(42);
    frame.header.local = Some(LocalAvatar {
        id: EntityId::new(1, 0),
        state: MotionState::at_rest(Vec3::new(1.0, 0.0, 2.0), Angle16(100)),
    });
    frame
        .entered
        .push((EntityId::new(2, 0), AppearanceId(5)))
        .unwrap();
    // Entity 0 is an ordinary entity, distinct from "none".
    frame
        .entered
        .push((EntityId::new(0, 0), AppearanceId(6)))
        .unwrap();
    frame
        .remotes
        .push(RemoteSample {
            id: EntityId::new(2, 0),
            tick: Tick(42),
            position: Vec3::new(3.0, 0.0, 4.0),
            velocity: Vec3::ZERO,
            yaw: Angle16(200),
        })
        .unwrap();
    frame.removed.push(EntityId::new(3, 1)).unwrap();
    let mut bytes = Vec::new();
    a.encode_snapshot(&frame, None, &mut bytes).unwrap();
    a.encode_outbound(
        &Outbound::SetPosition(SetPosition {
            entity: EntityId::new(1, 0),
            position: Vec3::new(9.0, 0.0, 9.0),
            yaw: Angle16(0),
            tick: Tick(42),
        }),
        &mut bytes,
    )
    .unwrap();
    a.encode_outbound(
        &Outbound::Refuse(mantis_adapter_contract::Refuse {
            reason: RefuseReason::Full,
        }),
        &mut bytes,
    )
    .unwrap();
    let mut got = Vec::new();
    decode_server(&bytes, |p| got.push(p)).unwrap();
    assert_eq!(
        got,
        vec![
            ServerPacket::WorldTick(42),
            ServerPacket::SelfState {
                id: EntityId::new(1, 0),
                position: Vec3::new(1.0, 0.0, 2.0),
                heading: 100
            },
            ServerPacket::Leave {
                id: EntityId::new(3, 1)
            },
            ServerPacket::Enter {
                id: EntityId::new(2, 0),
                look: AppearanceId(5)
            },
            ServerPacket::Enter {
                id: EntityId::new(0, 0),
                look: AppearanceId(6)
            },
            ServerPacket::Move {
                id: EntityId::new(2, 0),
                position: Vec3::new(3.0, 0.0, 4.0),
                heading: 200
            },
            ServerPacket::SetPos {
                entity: EntityId::new(1, 0),
                position: Vec3::new(9.0, 0.0, 9.0),
                heading: 0,
                tick: 42
            },
            ServerPacket::LoginFail(RefuseReason::Full),
        ]
    );
}

#[test]
fn feature_traffic_round_trips() {
    use mantis_adapter_contract::{
        Extension, ExtensionKind, ExtensionMessage, ExtensionRefusal, ExtensionRefused, FeatureState,
    };
    let a = adapter();
    let request = Inbound::Extension(Extension {
        kind: ExtensionKind(1010),
        request: 0,
        payload: BoundedArray::from_slice(&[1, 2, 3]).unwrap(),
    });
    let mut frame = Vec::new();
    assert!(encode_inbound(&request, BUILD, &mut frame));
    assert_eq!(decode_all(&a, &frame).unwrap(), [request]);
    let mut out = Vec::new();
    a.encode_outbound(
        &Outbound::ExtensionMessage(ExtensionMessage {
            kind: ExtensionKind(1011),
            payload: BoundedArray::from_slice(&[9, 8]).unwrap(),
        }),
        &mut out,
    )
    .unwrap();
    a.encode_outbound(
        &Outbound::FeatureState(FeatureState {
            module: WireString::new("std.chat").unwrap(),
            enabled: false,
        }),
        &mut out,
    )
    .unwrap();
    a.encode_outbound(
        &Outbound::ExtensionRefused(ExtensionRefused {
            kind: ExtensionKind(1010),
            request: 5,
            reason: ExtensionRefusal::FeatureDisabled,
        }),
        &mut out,
    )
    .unwrap();
    let mut got = Vec::new();
    decode_server(&out, |p| got.push(p)).unwrap();
    match got[0] {
        ServerPacket::FeatureData { kind, len, bytes } => {
            assert_eq!((kind, &bytes[..usize::from(len)]), (1011, &[9u8, 8][..]));
        }
        _ => panic!("expected feature data"),
    }
    match got[1] {
        ServerPacket::FeatureState { len, key, enabled } => {
            assert_eq!((&key[..usize::from(len)], enabled), (&b"std.chat"[..], false));
        }
        _ => panic!("expected a feature state"),
    }
    assert_eq!(
        got[2],
        ServerPacket::ExtensionRefused {
            kind: 1010,
            reason: 0
        }
    );
    assert_eq!(super::refusal_from(0), ExtensionRefusal::FeatureDisabled);
}

#[test]
fn an_entity_with_no_object_id_is_refused_never_sent_as_none() {
    let a = adapter();
    let unrepresentable = EntityId::from_bits(u64::MAX);
    assert!(matches!(
        super::object_id(unrepresentable),
        Err(AdapterError::Unrepresentable(_))
    ));
    assert_eq!(super::object_id(EntityId::new(0, 0)).ok(), Some(1));
    let mut out = Vec::new();
    let set = Outbound::SetPosition(SetPosition {
        entity: unrepresentable,
        position: Vec3::ZERO,
        yaw: Angle16(0),
        tick: Tick(1),
    });
    assert!(matches!(
        a.encode_outbound(&set, &mut out),
        Err(AdapterError::Unrepresentable(_))
    ));
    assert!(out.is_empty(), "nothing is written for a refused message");
    // A snapshot naming it anywhere is refused before any byte is written.
    let mut frame = SnapshotFrame::with_capacity(4, 4, 4, 4);
    frame.header.server_tick = Tick(3);
    frame.removed.push(EntityId::new(1, 0)).unwrap();
    frame.removed.push(unrepresentable).unwrap();
    assert!(matches!(
        a.encode_snapshot(&frame, None, &mut out),
        Err(AdapterError::Unrepresentable(_))
    ));
    assert!(out.is_empty());
    // The client half holds the same line.
    let cast = Inbound::Cast(Cast {
        ability: AbilityId(1),
        target: Some(unrepresentable),
        view_tick: Tick(1),
        view_frac: 0,
    });
    assert!(!encode_inbound(&cast, BUILD, &mut out));
    assert!(out.is_empty());
}

#[test]
fn the_legacy_adapter_declares_every_id_but_the_one_it_cannot_carry() {
    let range = adapter().entity_ids();
    assert_eq!(range, super::ENTITY_IDS);
    assert!(range.contains(EntityId::from_bits(u64::MAX - 1)));
    assert!(!range.contains(EntityId::from_bits(u64::MAX)));
    assert!(super::object_id(EntityId::from_bits(u64::MAX - 1)).is_ok());
}
