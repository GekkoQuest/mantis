//! A single cell end to end, without sockets: sessions join, Predictive and
//! Validated movement, snapshots decoded exactly as a client would, delta
//! baselines, the allowed-state list, corrections, zero allocation on the
//! hot path (cell thread and every job), and replay from the cell's own log.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::{BaselineStore, NativeAdapter, ServerFrame, decode_server_frame};
use mantis_adapter_contract::{
    AppearanceId, Channel, ConnectionId, MovementMode, Outbound, SnapshotFrame, WireAdapter,
};
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::{FlatGround, Motion, MotionParams};
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;
use mantis_core::time::TickRate;
use mantis_server::cell::{BoxedSink, Cell, CellConfig, MemoryLog, OutboundSink};
use mantis_server::components::ReplicationId;
use mantis_server::intent::{CellIntent, CellLogSchema};
use mantis_server::jobs::WorkerSet;
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// Captures sent frames per (adapter, conn, channel).
#[derive(Default)]
struct Capture {
    frames: Vec<(usize, ConnectionId, Channel, Vec<u8>)>,
}

impl OutboundSink for Capture {
    fn send(&mut self, adapter: usize, conn: ConnectionId, channel: Channel, bytes: &[u8]) {
        self.frames.push((adapter, conn, channel, bytes.to_vec()));
    }
}

/// Counts bytes only: allocation-free.
#[derive(Default)]
struct Counting {
    bytes: u64,
    frames: u64,
}

impl OutboundSink for Counting {
    fn send(&mut self, _a: usize, _c: ConnectionId, _ch: Channel, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        self.frames += 1;
    }
}

/// A minimal client: keeps received frames as baselines, like the native client.
struct Client {
    ring: Vec<SnapshotFrame>,
    latest: Option<SnapshotFrame>,
    corrections: Vec<Outbound>,
}

impl Client {
    fn new() -> Self {
        Self {
            ring: Vec::new(),
            latest: None,
            corrections: Vec::new(),
        }
    }

    fn receive(&mut self, bytes: &[u8]) {
        let mut f = SnapshotFrame::with_capacity(64, 256, 64, 64);
        match decode_server_frame(bytes, &self.ring[..], &mut f).unwrap() {
            ServerFrame::Snapshot => {
                if self.ring.len() >= 16 {
                    self.ring.remove(0);
                }
                self.ring.push(f.clone());
                self.latest = Some(f);
            }
            ServerFrame::Message(m) => self.corrections.push(m),
        }
    }
}

impl BaselineStore for Client {
    fn baseline(&self, tick: Tick) -> Option<&SnapshotFrame> {
        self.ring[..].baseline(tick)
    }
}

fn cell(log: Option<LogWriter<CellLogSchema, BoxedSink>>) -> Cell {
    let mut cfg = CellConfig::new(CellId(1), Seed(77));
    cfg.max_entities = 256;
    cfg.max_clients = 64;
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("test.native"))];
    Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        log,
        Arc::new(GraphCatalog::new()),
        BTreeMap::new(),
    )
    .unwrap()
}

fn join(c: &mut Cell, session: u64, x: f32, mode: MovementMode) {
    let s = SessionId(session);
    c.inbox().push(
        s,
        CellIntent::Join {
            repl: ReplicationId(EntityId::new(session as u32, 0)),
            spawn: Vec3::new(x, 0.0, 0.0),
            yaw: Angle16(0),
            look: AppearanceId(session as u32 * 10),
            mode,
            epoch: 1,
            character: session,
        },
    );
    c.attach_client(s, ConnectionId(session), 0, false).unwrap();
}

fn mv(seq: u32, buttons: MoveButtons) -> CellIntent {
    CellIntent::Move(MoveInput {
        seq: InputSeq(seq),
        tick: Tick(u64::from(seq)),
        buttons,
        yaw: Angle16(16_384),
        aim: AimAngles::default(),
    })
}

fn deliver(cap: &mut Capture, clients: &mut BTreeMap<u64, Client>) {
    for (_, conn, _, bytes) in cap.frames.drain(..) {
        clients.entry(conn.0).or_insert_with(Client::new).receive(&bytes);
    }
}

#[test]
fn predictive_snapshots_match_client_prediction_bit_for_bit() {
    let mut c = cell(None);
    join(&mut c, 1, 0.0, MovementMode::Predictive);
    join(&mut c, 2, 10.0, MovementMode::Predictive);
    let mut cap = Capture::default();
    let mut clients = BTreeMap::new();
    let motion = Motion::new(MotionParams::DEFAULT).unwrap();
    let dt = TickRate::HZ_30.dt_seconds();
    let mut predicted = MotionState::at_rest(Vec3::ZERO, Angle16(0));
    let mut seq = 0u32;
    for t in 1..=60u32 {
        // Client 1 runs forward; input for seq 31 is lost.
        seq += 1;
        let input = MoveInput {
            seq: InputSeq(seq),
            tick: Tick(u64::from(seq)),
            buttons: MoveButtons::FORWARD,
            yaw: Angle16(16_384),
            aim: AimAngles::default(),
        };
        if seq != 31 {
            c.inbox().push(SessionId(1), CellIntent::Move(input));
        }
        predicted = motion.step(&FlatGround(0.0), &predicted, &input, &MotionModifiers::NONE, dt);
        c.tick(&mut cap, None).unwrap();
        deliver(&mut cap, &mut clients);
        let snap = clients.get(&1).and_then(|c| c.latest.clone()).unwrap();
        assert_eq!(snap.header.server_tick, Tick(u64::from(t)));
        assert_eq!(snap.header.ack, Some(InputSeq(seq)), "one consumed seq per step");
        let local = snap.header.local.unwrap();
        // The lost input was synthesized by repeating the last one, which is
        // identical here, so prediction and server agree exactly.
        assert!(local.state.bits_eq(&predicted), "tick {t}");
        // Client 1 sees client 2.
        let other = ReplicationId(EntityId::new(2, 0)).0;
        if t == 1 {
            assert!(
                snap.entered
                    .iter()
                    .any(|(id, look)| *id == other && *look == AppearanceId(20))
            );
        }
        let r = snap.find_remote(other).unwrap();
        assert_eq!(r.position, Vec3::new(10.0, 0.0, 0.0));
        // Acknowledge to enable deltas.
        c.inbox().ack(SessionId(1), snap.header.server_tick);
        c.inbox().ack(SessionId(2), snap.header.server_tick);
    }
    assert_eq!(c.session(SessionId(1)).unwrap().synthesized, 1);
}

#[test]
fn deltas_shrink_snapshots_and_entered_stops_after_ack() {
    let mut c = cell(None);
    join(&mut c, 1, 0.0, MovementMode::Predictive);
    for i in 0..30u32 {
        let s = 100 + u64::from(i);
        c.inbox().push(
            SessionId(s),
            CellIntent::Join {
                repl: ReplicationId(EntityId::new(s as u32, 0)),
                spawn: Vec3::new(f32::from(i as u16) * 2.0, 0.0, 5.0),
                yaw: Angle16(0),
                look: AppearanceId(1),
                mode: MovementMode::Predictive,
                epoch: 1,
                character: s,
            },
        );
    }
    let mut cap = Capture::default();
    let mut client = Client::new();
    let mut sizes = Vec::new();
    for _ in 0..10 {
        c.tick(&mut cap, None).unwrap();
        for (_, conn, _, bytes) in cap.frames.drain(..) {
            if conn == ConnectionId(1) {
                sizes.push(bytes.len());
                client.receive(&bytes);
            }
        }
        let tick = client.latest.as_ref().unwrap().header.server_tick;
        c.inbox().ack(SessionId(1), tick);
    }
    let first = sizes[0];
    let last = *sizes.last().unwrap();
    assert!(last * 3 < first, "first {first} bytes, steady {last} bytes");
    let latest = client.latest.unwrap();
    assert!(latest.entered.is_empty(), "entered stops once acknowledged");
    assert_eq!(
        c.with_client(SessionId(1), mantis_server::replication::ClientRep::known_len),
        Some(30)
    );
}

#[test]
fn allowed_state_list_and_mode_checks_refuse() {
    let mut c = cell(None);
    // Moving before joining is refused.
    c.inbox().push(SessionId(5), mv(1, MoveButtons::FORWARD));
    let r = c.tick(&mut Capture::default(), None).unwrap();
    assert_eq!(r.refused, 1);
    join(&mut c, 5, 0.0, MovementMode::Predictive);
    c.tick(&mut Capture::default(), None).unwrap();
    // A position claim from a Predictive session is refused and counted.
    c.inbox().push(
        SessionId(5),
        CellIntent::MoveClaim {
            position: Vec3::X,
            client_time_ms: 1,
        },
    );
    let r = c.tick(&mut Capture::default(), None).unwrap();
    assert_eq!(r.refused, 1);
    assert_eq!(c.session(SessionId(5)).unwrap().cheats, 1);
    // A second join is refused.
    c.inbox().push(
        SessionId(5),
        CellIntent::Join {
            repl: ReplicationId(EntityId::new(9, 0)),
            spawn: Vec3::ZERO,
            yaw: Angle16(0),
            look: AppearanceId(0),
            mode: MovementMode::Predictive,
            epoch: 1,
            character: 5,
        },
    );
    assert_eq!(c.tick(&mut Capture::default(), None).unwrap().refused, 1);
}

#[test]
fn validated_claims_accept_honest_and_correct_cheats() {
    let mut c = cell(None);
    join(&mut c, 7, 0.0, MovementMode::Validated);
    let mut cap = Capture::default();
    c.tick(&mut cap, None).unwrap();
    let mut x = 0.0f32;
    let mut ms = 0u32;
    for t in 2..=20u64 {
        ms = (t * 1000 / 30) as u32;
        x += 7.0 / 30.0;
        c.inbox().push(
            SessionId(7),
            CellIntent::MoveClaim {
                position: Vec3::new(x, 0.0, 0.0),
                client_time_ms: ms,
            },
        );
        c.tick(&mut cap, None).unwrap();
    }
    assert_eq!(
        c.session(SessionId(7)).unwrap().cheats,
        0,
        "honest claims accepted"
    );
    let pos = c
        .world()
        .get::<mantis_server::components::Body>(c.local(ReplicationId(EntityId::new(7, 0))).unwrap())
        .unwrap()
        .0
        .position;
    assert_eq!(pos.x, x);
    // A 20% speed hack is caught on its first claim, and a correction goes out
    // on the reliable channel.
    cap.frames.clear();
    c.inbox().push(
        SessionId(7),
        CellIntent::MoveClaim {
            position: Vec3::new(x + 7.0 * 1.2 / 30.0, 0.0, 0.0),
            client_time_ms: ms + 33,
        },
    );
    c.tick(&mut cap, None).unwrap();
    assert_eq!(c.session(SessionId(7)).unwrap().cheats, 1);
    let corrections: Vec<_> = cap.frames.iter().filter(|f| f.2 == Channel::Reliable).collect();
    assert_eq!(corrections.len(), 1);
    let mut client = Client::new();
    client.receive(&corrections[0].3);
    match client.corrections.first() {
        Some(Outbound::SetPosition(p)) => assert_eq!(p.position, Vec3::new(x, 0.0, 0.0)),
        other => panic!("{other:?}"),
    }
}

static JOB_ALLOCS: AtomicU64 = AtomicU64::new(0);

fn counting_wrapper(f: &mut dyn FnMut()) {
    let ((), stats) = count_allocs(f);
    JOB_ALLOCS.fetch_add(stats.total_ops(), Ordering::Relaxed);
}

#[test]
fn hot_path_allocates_nothing_including_jobs() {
    let mut c = cell(None);
    for s in 1..=40u64 {
        join(&mut c, s, (s as f32) * 1.5, MovementMode::Predictive);
    }
    let workers = WorkerSet::new(3, 64, Some(counting_wrapper));
    let mut sink = Counting::default();
    let mut seq = 0u32;
    // Warm-up.
    for _ in 0..40 {
        seq += 1;
        for s in 1..=40u64 {
            c.inbox().push(
                SessionId(s),
                mv(
                    seq,
                    if s % 3 == 0 {
                        MoveButtons::NONE
                    } else {
                        MoveButtons::FORWARD
                    },
                ),
            );
        }
        let r = c.tick(&mut sink, Some(&workers)).unwrap();
        for s in 1..=40u64 {
            c.inbox().ack(SessionId(s), r.tick);
        }
    }
    JOB_ALLOCS.store(0, Ordering::Relaxed);
    let (mut offloaded, mut inline) = (0, 0);
    for _ in 0..60 {
        seq += 1;
        for s in 1..=40u64 {
            c.inbox().push(
                SessionId(s),
                mv(
                    seq,
                    if s % 3 == 0 {
                        MoveButtons::NONE
                    } else {
                        MoveButtons::FORWARD
                    },
                ),
            );
        }
        let r = assert_no_alloc("cell tick", || c.tick(&mut sink, Some(&workers))).unwrap();
        offloaded += r.jobs.offloaded;
        inline += r.jobs.inline;
        for s in 1..=40u64 {
            c.inbox().ack(SessionId(s), r.tick);
        }
    }
    // Which jobs a worker takes in one tick depends on scheduling (under
    // load the cell thread may run them all); across the run both paths
    // must have been exercised.
    assert!(offloaded > 0, "jobs ran on workers");
    assert!(inline > 0, "quota forced some inline");
    assert_eq!(JOB_ALLOCS.load(Ordering::Relaxed), 0, "a job allocated");
    assert!(sink.bytes > 0);
}

#[test]
fn a_cell_replays_from_its_own_log() {
    let header = LogHeader {
        build: BuildId([1; 32]),
        content: ContentHash::of(b"test.cell"),
        cell: CellId(1),
        seed: Seed(77),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let writer = LogWriter::create(sink, &header, 1 << 16).unwrap();
    let mut c = cell(Some(writer));
    join(&mut c, 1, 0.0, MovementMode::Predictive);
    join(&mut c, 2, 3.0, MovementMode::Validated);
    let mut hashes = Vec::new();
    for t in 1..=90u32 {
        c.inbox().push(
            SessionId(1),
            mv(
                t,
                if t % 4 == 0 {
                    MoveButtons::JUMP
                } else {
                    MoveButtons::FORWARD
                },
            ),
        );
        c.inbox().push(
            SessionId(2),
            CellIntent::MoveClaim {
                position: Vec3::new(3.0 + (t as f32) * 0.2, 0.0, 0.0),
                client_time_ms: t * 33,
            },
        );
        hashes.push(c.tick(&mut Capture::default(), None).unwrap().state_hash);
    }
    drop(c);
    let bytes = memory.bytes();
    let mut reader =
        LogReader::<CellLogSchema>::open(&bytes, BuildId([1; 32]), ContentHash::of(b"test.cell")).unwrap();
    let mut fresh = cell(None);
    let report = replay(&mut fresh, &mut reader).unwrap();
    assert_eq!(report.ticks, 90);
    assert_eq!(fresh.world().state_hash(), *hashes.last().unwrap());
}

#[test]
fn ground_is_exposed() {
    let c = cell(None);
    assert_eq!(c.ground().height_at(5.0, 5.0), Some(0.0));
}
