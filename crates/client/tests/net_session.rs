//! The native session against a scripted server: handshake, delta snapshots decoded
//! against acknowledged baselines, acknowledgements for applied snapshots only, conversion
//! into the simulation's frames (markers and entered entities included), stale and corrupt
//! frames counted, and moves sent with a repeat of the previous one.

#![allow(clippy::too_many_lines)] // One scripted session, start to end.

use std::collections::VecDeque;
use std::sync::Arc;

use mantis_adapter_contract::native::{FRAME_SNAPSHOT, encode_outbound_frame, encode_snapshot};
use mantis_adapter_contract::{
    AppearanceId, Channel, ConnectionId, Inbound, LocalAvatar, MovementMode, Outbound, RemoteSample,
    SnapshotFrame, Transport, TransportError, TransportEvent, TransportKind, Welcome,
};
use mantis_client::net::{NativeSession, NetConfig, SessionState, move_channel};
use mantis_client::sim::IntentSink;
use mantis_client::snapshot::snapshot_channel;
use mantis_client::time::{HostClock, ManualClock};
use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_core::graph::{GraphId, GraphInstanceId, MarkerId, MarkerKind, NodeKey, TimelineMarker};
use mantis_core::kinematics::{AimAngles, Angle16, InputSeq, MotionState, MoveButtons, MoveInput};
use mantis_core::math::Vec3;
use mantis_core::time::Tick;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Scripted {
    inbound: VecDeque<Vec<u8>>,
    sent: Vec<(Channel, Vec<u8>)>,
}

impl Transport for Scripted {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        while let Some(bytes) = self.inbound.pop_front() {
            sink(TransportEvent::Frame {
                conn: ConnectionId(0),
                channel: Channel::Unreliable,
                bytes: &bytes,
            });
        }
    }
    fn send(&mut self, _conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        self.sent.push((channel, bytes.to_vec()));
        Ok(())
    }
    fn disconnect(&mut self, _conn: ConnectionId) {}
    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }
    fn max_unreliable_payload(&self) -> usize {
        1100
    }
}

const AVATAR: EntityId = EntityId::new(1, 0);
const OTHER: EntityId = EntityId::new(7, 0);

fn snapshot(tick: u64, x: f32) -> SnapshotFrame {
    let mut f = SnapshotFrame::with_capacity(4, 4, 4, 4);
    f.header.server_tick = Tick(tick);
    f.header.ack = Some(InputSeq(u32::try_from(tick).unwrap_or(0)));
    f.header.local = Some(LocalAvatar {
        id: AVATAR,
        state: MotionState::at_rest(Vec3::new(x, 0.0, 0.0), Angle16(0)),
    });
    let _ = f.remotes.push(RemoteSample {
        id: OTHER,
        tick: Tick(tick),
        position: Vec3::new(x, 0.0, 5.0),
        velocity: Vec3::new(1.0, 0.0, 0.0),
        yaw: Angle16(0),
    });
    f
}

fn wire(frame: &SnapshotFrame, baseline: Option<&SnapshotFrame>) -> Vec<u8> {
    let mut out = vec![FRAME_SNAPSHOT];
    encode_snapshot(frame, baseline, &mut out);
    out
}

fn sent_acks(t: &Scripted) -> Vec<u64> {
    t.sent
        .iter()
        .filter_map(|(_, b)| {
            let payload = b.get(3..)?;
            let id = u16::from_le_bytes([*b.get(1)?, *b.get(2)?]);
            match mantis_adapter_contract::parse_inbound(mantis_core::wire::MessageId(id), payload).ok()? {
                Inbound::SnapshotAck(a) => Some(a.tick.0),
                _ => None,
            }
        })
        .collect()
}

fn sent_moves(t: &Scripted) -> Vec<u32> {
    t.sent
        .iter()
        .filter_map(|(_, b)| {
            let payload = b.get(3..)?;
            let id = u16::from_le_bytes([*b.get(1)?, *b.get(2)?]);
            match mantis_adapter_contract::parse_inbound(mantis_core::wire::MessageId(id), payload).ok()? {
                Inbound::Move(m) => Some(m.input.seq.0),
                _ => None,
            }
        })
        .collect()
}

#[test]
fn handshake_snapshots_acks_and_moves() -> TestResult {
    let clock = Arc::new(ManualClock::new());
    let (snap_tx, mut inbox) = snapshot_channel::<MotionState>(4, 8);
    let (mut outbox, moves) = move_channel(16);
    let mut session = NativeSession::new(
        Scripted::default(),
        Arc::clone(&clock) as Arc<dyn HostClock>,
        NetConfig::new(ContentHash::ZERO),
        snap_tx,
        moves,
    );
    session.start(b"token");
    assert_eq!(session.state(), SessionState::Connecting);
    let hello = session.transport_mut().sent.first().map(|(c, _)| *c);
    assert_eq!(hello, Some(Channel::Reliable));

    // Welcome, then a full snapshot carrying an entered entity and a marker.
    let mut welcome = Vec::new();
    encode_outbound_frame(
        &Outbound::Welcome(Welcome {
            protocol: 1,
            capabilities: 0,
            session: 9,
            tick: Tick(10),
            tick_rate: 30,
            mode: MovementMode::Predictive,
            avatar: Some(AVATAR),
            character: 77,
        }),
        &mut welcome,
    );
    let mut first = snapshot(10, 1.0);
    let _ = first.entered.push((OTHER, AppearanceId(3)));
    let marker = TimelineMarker {
        id: MarkerId {
            graph: GraphId(2),
            node: NodeKey(4),
        },
        kind: MarkerKind::Impact { target: OTHER },
        at: Tick(12),
        offset: 2,
        source: AVATAR,
        target: Some(OTHER),
        instance: GraphInstanceId(5),
    };
    let _ = first.markers.push(marker);
    let t = session.transport_mut();
    t.inbound.push_back(welcome);
    t.inbound.push_back(wire(&first, None));
    session.step();
    assert_eq!(
        session.state(),
        SessionState::Welcomed {
            avatar: Some(AVATAR),
            character: 77
        }
    );
    // A delta against the acknowledged frame, a stale repeat, and garbage.
    let second = snapshot(11, 2.0);
    let t = session.transport_mut();
    t.inbound.push_back(wire(&second, Some(&first)));
    t.inbound.push_back(wire(&first, None));
    t.inbound.push_back(vec![FRAME_SNAPSHOT, 0xff, 0xff]);
    // A delta against a frame the client never had.
    t.inbound
        .push_back(wire(&snapshot(13, 4.0), Some(&snapshot(12, 3.0))));
    session.step();
    let stats = session.stats();
    assert_eq!(
        (stats.snapshots, stats.stale, stats.undecodable),
        (2, 1, 2),
        "{stats:?}"
    );
    assert_eq!(
        sent_acks(session.transport_mut()),
        [10, 11],
        "only applied snapshots are acknowledged"
    );

    let mut frames = Vec::new();
    let _ = inbox.drain(|f| {
        frames.push((
            f.server_tick,
            f.ack,
            f.local.map(|l| l.1.position.x),
            f.remotes.len(),
            f.entered.clone(),
            f.markers.clone(),
        ));
    });
    assert_eq!(frames.len(), 2);
    let (tick, ack, x, remotes, entered, markers) = frames.first().cloned().ok_or("first")?;
    assert_eq!(
        (tick, ack, x, remotes),
        (Tick(10), Some(InputSeq(10)), Some(1.0), 1)
    );
    assert_eq!(entered, [(OTHER, 3)]);
    assert_eq!(markers, [marker]);
    assert_eq!(frames.get(1).and_then(|f| f.2), Some(2.0), "the delta decoded");

    // Moves go out unreliable, each followed by a repeat of the previous one.
    for seq in 1..=3u32 {
        outbox.send_move(&MoveInput {
            seq: InputSeq(seq),
            tick: Tick(u64::from(seq)),
            buttons: MoveButtons::FORWARD,
            yaw: Angle16(0),
            aim: AimAngles::default(),
        });
    }
    session.step();
    assert_eq!(sent_moves(session.transport_mut()), [1, 2, 1, 3, 2]);
    assert_eq!(session.stats().moves_sent, 5);
    Ok(())
}
