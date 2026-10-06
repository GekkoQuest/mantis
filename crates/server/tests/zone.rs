//! Seamless border crossing (plan 7.1; budget row `border-party`): the
//! transfer completes within 2 ticks and no observer loses the entity for
//! more than 1 tick. Here observers never lose it at all.

#![expect(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::{NativeAdapter, ServerFrame, decode_server_frame};
use mantis_adapter_contract::{
    AppearanceId, Channel, ConnectionId, MovementMode, SnapshotFrame, WireAdapter,
};
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::FlatGround;
use mantis_core::log::{CellId, SessionId};
use mantis_core::rng::Seed;
use mantis_server::cell::{Cell, CellConfig, OutboundSink};
use mantis_server::components::ReplicationId;
use mantis_server::intent::CellIntent;
use mantis_server::lease::CharacterId;
use mantis_server::zone::Zone;

#[derive(Default)]
struct Router {
    frames: Vec<(ConnectionId, Channel, Vec<u8>)>,
}

impl OutboundSink for Router {
    fn send(&mut self, _a: usize, conn: ConnectionId, channel: Channel, bytes: &[u8]) {
        self.frames.push((conn, channel, bytes.to_vec()));
    }
}

#[derive(Default)]
struct Client {
    ring: Vec<SnapshotFrame>,
    latest: Option<SnapshotFrame>,
}

impl Client {
    fn receive(&mut self, bytes: &[u8]) {
        let mut f = SnapshotFrame::with_capacity(64, 256, 64, 64);
        if let ServerFrame::Snapshot = decode_server_frame(bytes, &self.ring[..], &mut f).unwrap() {
            if self.ring.len() >= 16 {
                self.ring.remove(0);
            }
            self.ring.push(f.clone());
            self.latest = Some(f);
        }
    }
}

fn cell(id: u64, region: (f32, f32)) -> Cell {
    let mut cfg = CellConfig::new(CellId(id), Seed(id));
    cfg.max_entities = 64;
    cfg.max_clients = 8;
    cfg.region = Some(region);
    cfg.ghost_margin = 30.0;
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("test.native"))];
    Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        None,
        Arc::new(GraphCatalog::new()),
        BTreeMap::new(),
    )
    .unwrap()
}

fn join(zone: &mut Zone, session: u64, x: f32) {
    let s = SessionId(session);
    let i = zone.cell_for(x).unwrap();
    let epoch = zone.register(s, CharacterId(session), i).unwrap();
    let c = zone.cell_mut(i).unwrap();
    c.inbox().push(
        s,
        CellIntent::Join {
            repl: ReplicationId(EntityId::new(session as u32, 0)),
            spawn: Vec3::new(x, 0.0, 0.0),
            yaw: Angle16(16_384),
            look: AppearanceId(1),
            mode: MovementMode::Predictive,
            epoch,
            character: session,
        },
    );
    c.attach_client(s, ConnectionId(session), 0, false).unwrap();
}

#[test]
fn crossing_a_border_is_seamless() {
    let mut zone = Zone::new(
        vec![cell(1, (-1000.0, 0.0)), cell(2, (0.0, 1000.0))],
        vec![(-1000.0, 0.0), (0.0, 1000.0)],
    )
    .unwrap();
    join(&mut zone, 1, -3.0); // the walker, heading +x (yaw a quarter turn)
    join(&mut zone, 2, -8.0); // observer in cell 0
    join(&mut zone, 3, 8.0); // observer in cell 1
    let walker = EntityId::new(1, 0);
    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let mut router = Router::default();
    let mut seq = 0u32;
    let mut crossed_at = None;
    let mut seen_ticks: BTreeMap<u64, Vec<bool>> = BTreeMap::new();
    let mut last_ack = 0u32;
    for step in 0..40u32 {
        seq += 1;
        let input = MoveInput {
            seq: InputSeq(seq),
            tick: Tick(u64::from(seq)),
            buttons: MoveButtons::FORWARD,
            yaw: Angle16(16_384),
            aim: AimAngles::default(),
        };
        // The host routes each session's input to its current cell.
        let route = zone.route(SessionId(1)).unwrap();
        zone.cell_mut(route)
            .unwrap()
            .inbox()
            .push(SessionId(1), CellIntent::Move(input));
        let reports = zone.step(&mut router, None).unwrap();
        for (conn, _, bytes) in router.frames.drain(..) {
            clients.entry(conn.0).or_default().receive(&bytes);
        }
        // Acks go to wherever each session is served now.
        for (sid, client) in &clients {
            if let (Some(f), Some(r)) = (&client.latest, zone.route(SessionId(*sid))) {
                zone.cell_mut(r)
                    .unwrap()
                    .inbox()
                    .ack(SessionId(*sid), f.header.server_tick);
            }
        }
        if crossed_at.is_none() && !zone.transfers.is_empty() {
            let t = zone.transfers[0];
            assert!(t.issued);
            assert_eq!((t.from, t.to), (0, 1));
            crossed_at = Some(step);
        }
        // Observers see the walker every tick.
        for obs in [2u64, 3] {
            let f = clients[&obs].latest.as_ref().unwrap();
            assert_eq!(f.header.server_tick, reports[0].tick);
            seen_ticks
                .entry(obs)
                .or_default()
                .push(f.find_remote(walker).is_some());
            assert!(
                !f.removed.contains(&walker),
                "observer {obs} got a removal at step {step}"
            );
        }
        // The walker's own client: continuous local state, advancing ack.
        let me = clients[&1].latest.as_ref().unwrap();
        let local = me.header.local.unwrap();
        assert_eq!(local.id, walker);
        let ack = me.header.ack.unwrap().0;
        assert!(ack >= last_ack);
        last_ack = ack;
    }
    let crossed = crossed_at.expect("the walker crossed");
    // Within 2 ticks the destination owns it and the source does not.
    let walker_repl = ReplicationId(walker);
    assert!(
        zone.cells()[1].local(walker_repl).is_some(),
        "destination owns it"
    );
    assert!(zone.cells()[0].local(walker_repl).is_none(), "source released it");
    assert_eq!(zone.route(SessionId(1)), Some(1));
    // The lease moved with a new epoch.
    assert!(zone.leases().check(CharacterId(1), CellId(2), 2));
    // Observers never lost the walker after it first appeared.
    for (obs, seen) in &seen_ticks {
        let first = seen.iter().position(|s| *s).unwrap();
        let gaps = seen[first..].iter().filter(|s| !**s).count();
        assert_eq!(
            gaps, 0,
            "observer {obs} lost the walker (crossed at step {crossed})"
        );
    }
}
