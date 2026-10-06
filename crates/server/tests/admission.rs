//! Async admission (lead ruling, M7): a session whose handshake passed waits
//! for its token verdict holding no cell resources; verdicts apply at the
//! next host poll (a tick boundary); the number waiting is bounded; and a
//! verdict that never arrives refuses the session at the timeout, while the
//! cell keeps ticking unaffected.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mantis_adapter_contract::core_types::{ContentHash, Vec3};
use mantis_adapter_contract::native::NativeAdapter;
use mantis_adapter_contract::{AppearanceId, MovementMode, RefuseReason, TransportKind};
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::{FlatGround, MotionParams};
use mantis_core::log::CellId;
use mantis_core::rng::Seed;
use mantis_core::time::TickRate;
use mantis_net::handshake::ServerPolicy;
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_server::cell::{Cell, CellConfig};
use mantis_server::host::{Admission, AdmissionLimits, Host, HostConfig, Listener, Verdict};
use mantis_server::simnet::{LinkConfig, SimNet};
use mantis_server::zone::Zone;

const CONTENT: ContentHash = ContentHash::from_bytes([7; 32]);

/// An admission service the test answers by hand (or never).
#[derive(Clone, Default)]
struct Manual {
    asked: Arc<Mutex<Vec<u64>>>,
    answers: Arc<Mutex<Vec<(u64, Verdict)>>>,
}

impl Admission for Manual {
    fn begin(&mut self, ticket: u64, _token: &[u8]) {
        self.asked.lock().unwrap().push(ticket);
    }
    fn ready(&mut self, out: &mut Vec<(u64, Verdict)>) {
        out.append(&mut self.answers.lock().unwrap());
    }
}

struct Rig {
    net: SimNet,
    host: Host,
    zone: Zone,
    bots: Vec<Bot>,
    ms: u64,
    ticks: u64,
}

impl Rig {
    fn new(admission: Manual, limits: AdmissionLimits) -> Self {
        let net = SimNet::new(3);
        let host = Host::new(
            HostConfig {
                policy: ServerPolicy::new(CONTENT, 0),
                look: AppearanceId(1),
                tick_rate: 30,
                capacity: 64,
                token_ok: |t| !t.is_empty(),
                spawn: |_| Vec3::ZERO,
            },
            vec![Listener {
                adapter: Arc::new(NativeAdapter::new("test.native")),
                transport: Box::new(net.server(TransportKind::Quic)),
            }],
        )
        .with_admission(Box::new(admission), limits);
        let mut cfg = CellConfig::new(CellId(1), Seed(5));
        cfg.max_entities = 64;
        cfg.max_clients = 16;
        let cell = Cell::new(
            cfg,
            Arc::new(FlatGround(0.0)),
            host.adapters(),
            None,
            Arc::new(GraphCatalog::new()),
            BTreeMap::new(),
        )
        .unwrap();
        let zone = Zone::new(vec![cell], vec![(-1.0e5, 1.0e5)]).unwrap();
        Self {
            net,
            host,
            zone,
            bots: Vec::new(),
            ms: 0,
            ticks: 0,
        }
    }

    fn add(&mut self) {
        let mut bot = Bot::new(
            Box::new(NativeWire::new(MovementMode::Predictive)),
            Box::new(self.net.connect(LinkConfig::PERFECT)),
            Arc::new(FlatGround(0.0)),
            BotConfig {
                profile: Profile::Idle,
                motion: MotionParams::DEFAULT,
                rate: TickRate::HZ_30,
                seed: 7 + self.bots.len() as u64,
                content: CONTENT,
                clock_offset_ms: 1000,
            },
            Vec3::ZERO,
        )
        .unwrap();
        bot.start();
        self.bots.push(bot);
    }

    /// One tick; returns the cell's tick number.
    fn step(&mut self) -> u64 {
        self.ticks += 1;
        let target = self.ticks * 1000 / 30;
        self.net.advance(target - self.ms);
        self.ms = target;
        self.host.poll(&mut self.zone);
        let reports = self.zone.step(&mut self.host, None).unwrap();
        for b in &mut self.bots {
            b.step();
        }
        reports[0].tick.0
    }

    fn sessions_in_cell(&self) -> usize {
        self.zone.cells()[0]
            .world()
            .resource::<mantis_server::session::Sessions>()
            .map_or(0, |s| s.map.len())
    }
}

#[test]
fn a_verdict_that_never_arrives_refuses_at_the_timeout_and_the_cell_ticks_on() {
    let silent = Manual::default();
    let limits = AdmissionLimits {
        max_verifying: 8,
        timeout_ticks: 20,
    };
    let mut rig = Rig::new(silent.clone(), limits);
    rig.add();
    let mut last = 0;
    let mut waited = 0;
    while rig.bots[0].stats.refused.is_none() {
        let tick = rig.step();
        // Every poll is followed by exactly one cell tick: nothing waits on
        // the admission service.
        assert_eq!(tick, last + 1);
        last = tick;
        waited += 1;
        assert!(waited < 100, "never refused");
        if rig.host.sessions_verifying() == 1 {
            assert_eq!(
                rig.sessions_in_cell(),
                0,
                "a verifying session holds cell resources"
            );
        }
    }
    assert_eq!(rig.bots[0].stats.refused, Some(RefuseReason::BadToken));
    assert_eq!(silent.asked.lock().unwrap().len(), 1);
    assert_eq!(rig.host.stats.admission_timeouts, 1);
    assert_eq!(rig.host.stats.joined, 0, "never admitted on timeout");
    assert_eq!(rig.host.sessions_verifying(), 0);
    assert_eq!(rig.sessions_in_cell(), 0);
    // A verdict arriving after the timeout is ignored.
    let ticket = silent.asked.lock().unwrap()[0];
    silent
        .answers
        .lock()
        .unwrap()
        .push((ticket, Verdict::Admit { character: 9 }));
    for _ in 0..5 {
        rig.step();
    }
    assert_eq!(rig.host.stats.joined, 0);
}

#[test]
fn verdicts_admit_as_the_issued_character_or_refuse_and_waiting_is_bounded() {
    let service = Manual::default();
    let limits = AdmissionLimits {
        max_verifying: 2,
        timeout_ticks: 300,
    };
    let mut rig = Rig::new(service.clone(), limits);
    for _ in 0..3 {
        rig.add();
    }
    for _ in 0..10 {
        rig.step();
    }
    assert_eq!(rig.host.sessions_verifying(), 2);
    let refused: Vec<_> = rig.bots.iter().filter_map(|b| b.stats.refused).collect();
    assert_eq!(refused, vec![RefuseReason::Full], "the third waits for no slot");
    let asked = service.asked.lock().unwrap().clone();
    service.answers.lock().unwrap().extend([
        (asked[0], Verdict::Admit { character: 4242 }),
        (asked[1], Verdict::Refuse),
    ]);
    for _ in 0..10 {
        rig.step();
    }
    assert_eq!(rig.host.stats.joined, 1);
    assert_eq!(rig.sessions_in_cell(), 1);
    let s = rig.zone.cells()[0]
        .world()
        .resource::<mantis_server::session::Sessions>()
        .unwrap();
    let character = s.map.values().next().unwrap().character;
    assert_eq!(character, 4242);
    let refused: Vec<_> = rig.bots.iter().filter_map(|b| b.stats.refused).collect();
    assert_eq!(refused.len(), 2);
    assert!(refused.contains(&RefuseReason::BadToken));
    assert_eq!(rig.host.sessions_verifying(), 0);
}
