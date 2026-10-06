//! The host end to end over a simulated network at the budget condition
//! (100 ms RTT, 2% loss): headless bots connect through two adapters, one
//! Predictive over a QUIC-shaped link and one Validated over a TCP-shaped
//! link, and play. Honest bots are never rejected; cheating bots are caught.

#![expect(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::NativeAdapter;
use mantis_adapter_contract::{
    AdapterError, AppearanceId, Inbound, MovementMode, Outbound, SnapshotFrame, TransportKind, WireAdapter,
};
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::{FlatGround, MotionParams};
use mantis_core::log::{CellId, SessionId};
use mantis_core::rng::Seed;
use mantis_core::time::TickRate;
use mantis_net::handshake::ServerPolicy;
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_server::cell::{Cell, CellConfig};
use mantis_server::host::{Host, HostConfig, Listener};
use mantis_server::simnet::{LinkConfig, SimNet};
use mantis_server::zone::Zone;

/// The native protocol in Validated mode over TCP, standing in for a
/// package's legacy adapter.
struct ValidatedNative(NativeAdapter);

impl WireAdapter for ValidatedNative {
    fn name(&self) -> &'static str {
        "test.validated"
    }
    fn movement_mode(&self) -> MovementMode {
        MovementMode::Validated
    }
    fn transport(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn entity_ids(&self) -> mantis_adapter_contract::EntityIdRange {
        mantis_adapter_contract::EntityIdRange::ALL
    }
    fn decode(&self, frame: &[u8], out: &mut dyn FnMut(Inbound)) -> Result<(), AdapterError> {
        self.0.decode(frame, out)
    }
    fn encode_outbound(&self, msg: &Outbound, out: &mut Vec<u8>) -> Result<(), AdapterError> {
        self.0.encode_outbound(msg, out)
    }
    fn encode_snapshot(
        &self,
        frame: &SnapshotFrame,
        baseline: Option<&SnapshotFrame>,
        out: &mut Vec<u8>,
    ) -> Result<(), AdapterError> {
        self.0.encode_snapshot(frame, baseline, out)
    }
    fn encode_snapshot_based(
        &self,
        frame: &SnapshotFrame,
        baseline: Option<&SnapshotFrame>,
        older: &dyn mantis_adapter_contract::RemoteBases,
        out: &mut Vec<u8>,
    ) -> Result<(), AdapterError> {
        self.0.encode_snapshot_based(frame, baseline, older, out)
    }
}

const CONTENT: ContentHash = ContentHash::from_bytes([7; 32]);

fn spawn(s: SessionId) -> Vec3 {
    // A ring of spawn points, all inside interest range of each other.
    let i = (s.0 % 16) as f32;
    Vec3::new(i * 2.0 - 16.0, 0.0, (s.0 / 16) as f32 * 2.0)
}

struct Rig {
    predictive_net: SimNet,
    validated_net: SimNet,
    host: Host,
    zone: Zone,
    bots: Vec<(Profile, MovementMode, Bot)>,
    ms: u64,
    ticks: u64,
}

impl Rig {
    fn new() -> Self {
        let predictive_net = SimNet::new(11);
        let validated_net = SimNet::new(12);
        let host = Host::new(
            HostConfig {
                policy: ServerPolicy::new(CONTENT, 0),
                look: AppearanceId(1),
                tick_rate: 30,
                capacity: 64,
                token_ok: |t| !t.is_empty(),
                spawn,
            },
            vec![
                Listener {
                    adapter: Arc::new(NativeAdapter::new("test.native")),
                    transport: Box::new(predictive_net.server(TransportKind::Quic)),
                },
                Listener {
                    adapter: Arc::new(ValidatedNative(NativeAdapter::new("test.validated"))),
                    transport: Box::new(validated_net.server(TransportKind::Tcp)),
                },
            ],
        );
        let mut cfg = CellConfig::new(CellId(1), Seed(5));
        cfg.max_entities = 128;
        cfg.max_clients = 64;
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
            predictive_net,
            validated_net,
            host,
            zone,
            bots: Vec::new(),
            ms: 0,
            ticks: 0,
        }
    }

    fn add(&mut self, mode: MovementMode, profile: Profile, link: LinkConfig) {
        let n = self.bots.len() as u64;
        let transport = match mode {
            MovementMode::Predictive => self.predictive_net.connect(link),
            MovementMode::Validated => self.validated_net.connect(link),
        };
        let mut bot = Bot::new(
            Box::new(NativeWire::new(mode)),
            Box::new(transport),
            Arc::new(FlatGround(0.0)),
            BotConfig {
                profile,
                motion: MotionParams::DEFAULT,
                rate: TickRate::HZ_30,
                seed: 100 + n,
                content: CONTENT,
                clock_offset_ms: 5_000 + 37 * n as u32,
            },
            Vec3::ZERO,
        )
        .unwrap();
        bot.start();
        self.bots.push((profile, mode, bot));
    }

    /// One server tick and one client tick, with simulated time advancing
    /// a tick's worth (33 or 34 ms).
    fn step(&mut self) {
        self.ticks += 1;
        let target = self.ticks * 1000 / 30;
        let dt = target - self.ms;
        self.ms = target;
        self.predictive_net.advance(dt);
        self.validated_net.advance(dt);
        self.host.poll(&mut self.zone);
        self.zone.step(&mut self.host, None).unwrap();
        for (_, _, b) in &mut self.bots {
            b.step();
        }
    }
}

fn p99(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f32 * 0.99) as usize]
}

#[test]
fn honest_bots_play_through_both_adapters_at_100ms_rtt_and_2pct_loss() {
    let mut rig = Rig::new();
    for _ in 0..8 {
        rig.add(
            MovementMode::Predictive,
            Profile::Honest,
            LinkConfig::RTT100_LOSS2,
        );
    }
    for _ in 0..8 {
        rig.add(MovementMode::Validated, Profile::Honest, LinkConfig::RTT100_LOSS2);
    }
    for _ in 0..300 {
        rig.step();
    }
    let midway: Vec<Vec3> = rig.bots.iter().map(|(_, _, b)| b.state().position).collect();
    for _ in 0..300 {
        rig.step();
    }
    let moved = rig
        .bots
        .iter()
        .zip(&midway)
        .filter(|((_, _, b), m)| (b.state().position - **m).length() > 1.0)
        .count();
    assert!(moved >= 12, "bots actually walk ({moved} of 16 moved)");
    assert_eq!(rig.host.stats.joined, 16);
    assert_eq!(rig.host.stats.refused_handshakes, 0);
    assert_eq!(rig.host.stats.adapter_errors, 0);
    assert_eq!(rig.host.stats.invalid, 0);

    let mut reconcile = Vec::new();
    let (mut claims, mut corrections) = (0u64, 0u64);
    for (_, mode, b) in &rig.bots {
        assert!(b.welcomed() && b.synced(), "every bot entered the world");
        assert!(b.stats.snapshots > 400, "snapshots flow ({})", b.stats.snapshots);
        assert!(b.stats.last_remotes > 0, "bots see each other");
        match mode {
            MovementMode::Predictive => reconcile.extend_from_slice(&b.stats.reconcile),
            MovementMode::Validated => {
                claims += b.stats.claims;
                corrections += b.stats.corrections;
            }
        }
    }
    // Predictive: reconciliation error stays under 10 cm at p99.
    let p = p99(reconcile);
    eprintln!("reconciliation p99: {p} m");
    assert!(p < 0.10, "reconciliation p99 {p} m");
    // Validated: under 0.1% false rejections, both as the server counts them
    // and as the clients saw corrections.
    let cheats: u64 = (1..=16u64)
        .filter_map(|s| rig.zone.cells()[0].session(SessionId(s)))
        .map(|s| u64::from(s.cheats))
        .sum();
    assert!(claims > 8 * 500);
    let rate = cheats.max(corrections) as f64 / claims as f64;
    eprintln!("false rejections: {cheats} server, {corrections} client, of {claims} claims");
    assert!(
        rate < 0.001,
        "false rejection rate {rate} ({cheats} server, {corrections} client, {claims} claims)"
    );
}

#[test]
fn cheating_bots_are_corrected_and_honest_neighbours_are_not() {
    let mut rig = Rig::new();
    rig.add(MovementMode::Validated, Profile::Honest, LinkConfig::RTT100_LOSS2);
    rig.add(
        MovementMode::Validated,
        Profile::SpeedHack(1.2),
        LinkConfig::RTT100_LOSS2,
    );
    rig.add(
        MovementMode::Validated,
        Profile::Teleport {
            every: 60,
            distance: 8.0,
        },
        LinkConfig::RTT100_LOSS2,
    );
    for _ in 0..400 {
        rig.step();
    }
    let honest = &rig.bots[0].2;
    let speed = &rig.bots[1].2;
    let teleport = &rig.bots[2].2;
    assert_eq!(honest.stats.corrections, 0);
    assert!(speed.stats.corrections > 0, "speed hack corrected");
    assert!(teleport.stats.corrections > 0, "teleport corrected");
    // Seen from the client, the first correction arrives within the round
    // trip plus a few ticks of the first cheating claim.
    let d = speed.stats.detection_ticks.unwrap();
    eprintln!("speed hack corrected {d} client ticks after its first claim");
    assert!(d <= 12, "speed hack corrected after {d} client ticks");
    let cheats: Vec<u32> = (1..=3u64)
        .map(|s| rig.zone.cells()[0].session(SessionId(s)).unwrap().cheats)
        .collect();
    assert_eq!(cheats[0], 0);
    assert!(cheats[1] > 0 && cheats[2] > 0);
}
