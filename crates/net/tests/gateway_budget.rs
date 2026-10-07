//! The gateway's forwarding cost (a budget row): 64 sessions, each handed
//! off once (so every snapshot is stamped with its epoch, the worst case),
//! relaying one snapshot down and one input and one acknowledgement up per
//! session per poll.
//!
//! - **No per-message allocation**: every poll in steady state is counted
//!   with the allocation harness and must perform no heap operation.
//! - **Cost per relayed frame** (release builds; a `budget:` line).
//!
//! The transports are in memory and allocation-free themselves, so what is
//! counted is the gateway's own work.

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mantis_adapter_contract::core_types::{
    BoundedArray, ContentHash, EntityId, InputSeq, MotionState, MoveInput, Tick, Vec3, WireString,
};
use mantis_adapter_contract::native::{NativeAdapter, encode_inbound, encode_outbound_frame};
use mantis_adapter_contract::{
    Channel, ConnectionId, HandOff, Hello, Inbound, LocalAvatar, Move, MovementMode, Outbound, RemoteSample,
    SnapshotAck, SnapshotFrame, SnapshotHeader, SnapshotVisitor, Transport, TransportError, TransportEvent,
    TransportKind, Welcome, WireAdapter,
};
use mantis_net::gateway::{Dialer, Gateway, GatewayConfig, Route, Routes};

#[global_allocator]
static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;

const SESSIONS: usize = 64;

/// One queued transport event: 0 connected, 1 frame, 2 gone.
struct Ev {
    kind: u8,
    conn: ConnectionId,
    channel: Channel,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct NetState {
    events: VecDeque<Ev>,
    pool: Vec<Vec<u8>>,
    sent: u64,
    sent_bytes: u64,
    dialed: Vec<ConnectionId>,
    next: u64,
}

/// An in-memory transport (and dialer) that never allocates once warm:
/// frames travel in pooled buffers, sends are counted, not kept.
#[derive(Clone, Default)]
struct Mem(Arc<Mutex<NetState>>);

impl Mem {
    fn state(&self) -> std::sync::MutexGuard<'_, NetState> {
        self.0.lock().unwrap()
    }

    fn warm(&self, buffers: usize, events: usize) {
        let mut s = self.state();
        s.events.reserve(events);
        for _ in 0..buffers {
            s.pool.push(Vec::with_capacity(2048));
        }
        s.dialed.reserve(4 * SESSIONS);
    }

    fn push(&self, kind: u8, conn: ConnectionId, channel: Channel, bytes: &[u8]) {
        let mut s = self.state();
        let mut buf = s.pool.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(bytes);
        s.events.push_back(Ev {
            kind,
            conn,
            channel,
            bytes: buf,
        });
    }

    fn frame(&self, conn: ConnectionId, channel: Channel, bytes: &[u8]) {
        self.push(1, conn, channel, bytes);
    }

    fn connected(&self, conn: ConnectionId) {
        self.push(0, conn, Channel::Reliable, &[]);
    }
}

impl Transport for Mem {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        let mut s = self.state();
        while let Some(ev) = s.events.pop_front() {
            match ev.kind {
                0 => sink(TransportEvent::Connected(ev.conn)),
                1 => sink(TransportEvent::Frame {
                    conn: ev.conn,
                    channel: ev.channel,
                    bytes: &ev.bytes,
                }),
                _ => sink(TransportEvent::Disconnected {
                    conn: ev.conn,
                    reason: mantis_adapter_contract::DisconnectReason::Closed,
                }),
            }
            s.pool.push(ev.bytes);
        }
    }

    fn send(&mut self, _conn: ConnectionId, _channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        let mut s = self.state();
        s.sent += 1;
        s.sent_bytes += bytes.len() as u64;
        Ok(())
    }

    fn disconnect(&mut self, _conn: ConnectionId) {}

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn max_unreliable_payload(&self) -> usize {
        1200
    }
}

impl Dialer for Mem {
    fn dial(&mut self, _address: &str) -> Option<ConnectionId> {
        let mut s = self.state();
        s.next += 1;
        let id = ConnectionId(10_000 + s.next);
        s.dialed.push(id);
        drop(s);
        self.connected(id);
        Some(id)
    }
}

/// Every token routes to one host at once.
struct Direct(Vec<(u64, Route)>);

impl Routes for Direct {
    fn begin(&mut self, ticket: u64, _token: &[u8]) {
        self.0.push((
            ticket,
            Route::Host {
                cell: 1,
                address: "host-a".to_owned(),
            },
        ));
    }

    fn ready(&mut self, out: &mut Vec<(u64, Route)>) {
        out.append(&mut self.0);
    }
}

fn message(msg: &Outbound) -> Vec<u8> {
    let mut out = Vec::new();
    encode_outbound_frame(msg, &mut out);
    out
}

fn inbound(msg: &Inbound) -> Vec<u8> {
    let mut out = Vec::new();
    encode_inbound(msg, &mut out);
    out
}

/// A host's snapshot for tick `tick`: an avatar and 24 remotes, encoded
/// whole (about the size of a busy delta frame).
fn snapshot(tick: u64) -> Vec<u8> {
    let mut f = SnapshotFrame::with_capacity(8, 64, 8, 8);
    f.header(&SnapshotHeader {
        server_tick: Tick(tick),
        ack: Some(InputSeq(7)),
        local: Some(LocalAvatar {
            id: EntityId::new(1, 0),
            state: MotionState::at_rest(
                Vec3::new(1.0, 0.0, 2.0),
                mantis_adapter_contract::core_types::Angle16(0),
            ),
        }),
        ..SnapshotHeader::default()
    });
    for i in 0..24u32 {
        f.remote(&RemoteSample {
            id: EntityId::new(i + 2, 0),
            tick: Tick(tick),
            position: Vec3::new(f32::from(u16::try_from(i).unwrap()), 0.0, 3.0),
            velocity: Vec3::new(1.0, 0.0, 0.0),
            yaw: mantis_adapter_contract::core_types::Angle16(0),
        });
    }
    let mut out = Vec::new();
    NativeAdapter::new("budget")
        .encode_snapshot(&f, None, &mut out)
        .unwrap();
    out
}

struct Bench {
    gateway: Gateway,
    clients: Mem,
    hosts: Mem,
    host_conns: Vec<ConnectionId>,
    snap: Vec<u8>,
    tick: u64,
}

/// 64 sessions joined and each handed off once.
fn bench() -> Bench {
    let (clients, hosts) = (Mem::default(), Mem::default());
    clients.warm(4 * SESSIONS, 4 * SESSIONS);
    hosts.warm(4 * SESSIONS, 4 * SESSIONS);
    let mut n = 0u8;
    let mut gateway = Gateway::new(
        Box::new(clients.clone()),
        Box::new(hosts.clone()),
        Box::new(Direct(Vec::with_capacity(SESSIONS))),
        Box::new(move |t: &mut [u8; 32]| {
            n = n.wrapping_add(1);
            *t = [n; 32];
        }),
        GatewayConfig::DEFAULT,
    );
    let hello = inbound(&Inbound::Hello(Hello {
        protocol: 1,
        capabilities: 0,
        content: ContentHash::of(b"budget"),
        modules: BoundedArray::new(),
        token: BoundedArray::from_slice(&[3; 32]).unwrap(),
    }));
    for i in 0..SESSIONS as u64 {
        clients.connected(ConnectionId(i));
        clients.frame(ConnectionId(i), Channel::Reliable, &hello);
    }
    for _ in 0..3 {
        gateway.poll(0);
    }
    let welcome = |tick: u64| {
        message(&Outbound::Welcome(Welcome {
            protocol: 1,
            capabilities: 0,
            session: 1,
            tick: Tick(tick),
            tick_rate: 30,
            mode: MovementMode::Predictive,
            avatar: Some(EntityId::new(1, 0)),
            character: 1,
        }))
    };
    let first: Vec<ConnectionId> = hosts.state().dialed.clone();
    assert_eq!(first.len(), SESSIONS);
    for c in &first {
        hosts.frame(*c, Channel::Reliable, &welcome(100));
    }
    gateway.poll(0);
    assert_eq!(gateway.stats.joined, SESSIONS as u64);
    // Every session is handed off: from now on its snapshots are stamped.
    let hand_off = message(&Outbound::HandOff(HandOff {
        cell: 2,
        address: WireString::new("host-b").unwrap(),
        token: BoundedArray::from_slice(&[4; 32]).unwrap(),
    }));
    for c in &first {
        hosts.frame(*c, Channel::Reliable, &hand_off);
    }
    gateway.poll(0);
    gateway.poll(0);
    let second: Vec<ConnectionId> = hosts.state().dialed[SESSIONS..].to_vec();
    for c in &second {
        hosts.frame(*c, Channel::Reliable, &welcome(10));
    }
    gateway.poll(0);
    assert_eq!(gateway.stats.handed_off, SESSIONS as u64);
    Bench {
        gateway,
        clients,
        hosts,
        host_conns: second,
        snap: snapshot(10),
        tick: 10,
    }
}

impl Bench {
    /// One tick's traffic: a snapshot down, an input and an acknowledgement
    /// of the last snapshot up, per session.
    fn load(&mut self) {
        self.tick += 1;
        self.snap[1..9].copy_from_slice(&self.tick.to_le_bytes());
        let mut mv = [0u8; 64];
        let mut ack = [0u8; 16];
        let (mv_len, ack_len) = {
            let a = inbound(&Inbound::Move(Move {
                input: MoveInput {
                    seq: InputSeq(u32::try_from(self.tick).unwrap()),
                    ..MoveInput::default()
                },
            }));
            let b = inbound(&Inbound::SnapshotAck(SnapshotAck {
                tick: Tick(self.tick - 1),
            }));
            mv[..a.len()].copy_from_slice(&a);
            ack[..b.len()].copy_from_slice(&b);
            (a.len(), b.len())
        };
        for (i, host) in self.host_conns.iter().enumerate() {
            self.hosts.frame(*host, Channel::Unreliable, &self.snap);
            self.clients
                .frame(ConnectionId(i as u64), Channel::Unreliable, &mv[..mv_len]);
            self.clients
                .frame(ConnectionId(i as u64), Channel::Unreliable, &ack[..ack_len]);
        }
    }
}

#[test]
fn the_gateway_relays_with_no_heap_operation_per_message() {
    let mut b = bench();
    for _ in 0..20 {
        b.load();
        b.gateway.poll(1);
    }
    let before = b.gateway.stats;
    for _ in 0..200 {
        // Building the traffic allocates (test side); the poll must not.
        b.load();
        let gateway = &mut b.gateway;
        mantis_testkit::alloc::assert_no_alloc("gateway poll", || gateway.poll(2));
    }
    let after = b.gateway.stats;
    let down = after.down - before.down;
    let up = after.up - before.up;
    println!(
        "gateway: {down} frames down ({} stamped), {up} up, {} acknowledgements of unsent snapshots dropped, 0 heap operations",
        after.stamped - before.stamped,
        after.dropped - before.dropped
    );
    assert_eq!(down, 200 * SESSIONS as u64, "every snapshot relayed");
    assert_eq!(after.stamped - before.stamped, down, "every one stamped");
    assert_eq!(
        up,
        2 * 200 * SESSIONS as u64,
        "every input and acknowledgement relayed"
    );
    assert_eq!(after.send_errors, 0);
    assert!(b.clients.state().sent > 0 && b.hosts.state().sent_bytes > 0);
}

/// Nanoseconds per relayed frame, release builds: the budget. The gateway's
/// own work only (the transports here are in memory); about 21 ns measured
/// on the development desktop, so ten times that and some.
const NANOS_PER_FRAME_LIMIT: u128 = 250;

#[test]
#[cfg_attr(debug_assertions, ignore = "timing budgets run in release builds")]
fn the_gateway_relays_a_frame_within_budget() {
    let mut b = bench();
    for _ in 0..50 {
        b.load();
        b.gateway.poll(1);
    }
    let rounds = 2_000u64;
    let mut spent = 0u128;
    let before = b.gateway.stats;
    for _ in 0..rounds {
        b.load();
        let t = Instant::now();
        b.gateway.poll(2);
        spent += t.elapsed().as_nanos();
    }
    let after = b.gateway.stats;
    let frames = u128::from((after.down - before.down) + (after.up - before.up));
    let per = spent / frames.max(1);
    println!(
        "budget: the gateway relays a frame in {per} ns (limit {NANOS_PER_FRAME_LIMIT} ns; {SESSIONS} sessions, every snapshot stamped, {frames} frames)"
    );
    assert!(per <= NANOS_PER_FRAME_LIMIT);
}
