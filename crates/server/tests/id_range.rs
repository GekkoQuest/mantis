//! Adapter id ranges: a host allocates replicated ids only inside the range
//! every listening adapter declares it can carry. Past it, a join is refused
//! (fail closed, counted), and no client is ever handed an id its protocol
//! cannot carry.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::{ContentHash, EntityId, Vec3};
use mantis_adapter_contract::native::NativeAdapter;
use mantis_adapter_contract::{
    AdapterError, AppearanceId, EntityIdRange, Inbound, MovementMode, Outbound, RefuseReason, SnapshotFrame,
    TransportKind, WireAdapter,
};
use mantis_core::graph::GraphCatalog;
use mantis_core::kinematics::{FlatGround, MotionParams};
use mantis_core::log::CellId;
use mantis_core::rng::Seed;
use mantis_core::time::TickRate;
use mantis_net::handshake::ServerPolicy;
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_server::cell::{Cell, CellConfig};
use mantis_server::host::{Host, HostConfig, Listener};
use mantis_server::simnet::{LinkConfig, SimNet};
use mantis_server::zone::Zone;

const CONTENT: ContentHash = ContentHash::from_bytes([7; 32]);

/// The native protocol, declaring that it carries only ids 0 and 1.
struct Narrow(NativeAdapter);

impl WireAdapter for Narrow {
    fn name(&self) -> &'static str {
        "test.narrow"
    }
    fn movement_mode(&self) -> MovementMode {
        self.0.movement_mode()
    }
    fn transport(&self) -> TransportKind {
        self.0.transport()
    }
    fn entity_ids(&self) -> EntityIdRange {
        EntityIdRange { max_bits: 1 }
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

#[test]
fn a_join_past_the_narrowest_adapters_range_is_refused_and_counted() {
    let wide = SimNet::new(3);
    let narrow = SimNet::new(4);
    let mut host = Host::new(
        HostConfig {
            policy: ServerPolicy::new(CONTENT, 0),
            look: AppearanceId(1),
            tick_rate: 30,
            capacity: 64,
            token_ok: |t| !t.is_empty(),
            spawn: |_| Vec3::ZERO,
        },
        vec![
            Listener {
                adapter: Arc::new(NativeAdapter::new("test.native")),
                transport: Box::new(wide.server(TransportKind::Quic)),
            },
            Listener {
                adapter: Arc::new(Narrow(NativeAdapter::new("test.inner"))),
                transport: Box::new(narrow.server(TransportKind::Quic)),
            },
        ],
    );
    // Any listener's narrower range bounds every id: a wide client's avatar
    // may be seen by a narrow one.
    assert_eq!(host.entity_ids(), EntityIdRange { max_bits: 1 });
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
    let mut zone = Zone::new(vec![cell], vec![(-1.0e5, 1.0e5)]).unwrap();
    let mut bots: Vec<Bot> = (0..3u64)
        .map(|n| {
            let net = if n == 2 { &narrow } else { &wide };
            let mut bot = Bot::new(
                Box::new(NativeWire::new(MovementMode::Predictive)),
                Box::new(net.connect(LinkConfig::PERFECT)),
                Arc::new(FlatGround(0.0)),
                BotConfig {
                    profile: Profile::Idle,
                    motion: MotionParams::DEFAULT,
                    rate: TickRate::HZ_30,
                    seed: 7 + n,
                    content: CONTENT,
                    clock_offset_ms: 1000,
                },
                Vec3::ZERO,
            )
            .unwrap();
            bot.start();
            bot
        })
        .collect();
    for tick in 1..=60u64 {
        let ms = tick * 1000 / 30 - (tick - 1) * 1000 / 30;
        wide.advance(ms);
        narrow.advance(ms);
        host.poll(&mut zone);
        zone.step(&mut host, None).unwrap();
        for b in &mut bots {
            b.step();
        }
    }
    let welcomed = bots.iter().filter(|b| b.welcomed()).count();
    assert_eq!(welcomed, 2, "two ids fit the range");
    assert_eq!(host.stats.ids_exhausted, 1);
    assert_eq!(
        bots.iter().filter_map(|b| b.stats.refused).collect::<Vec<_>>(),
        vec![RefuseReason::Full]
    );
    // Every avatar handed out is inside the range.
    for b in bots.iter().filter(|b| b.welcomed()) {
        assert!(EntityIdRange { max_bits: 1 }.contains(b.avatar().unwrap_or(EntityId::new(9, 9))));
    }
}
