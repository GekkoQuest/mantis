//! The module registry surface inside a cell: extension handlers with
//! required session state, economy commands through the log, systems with
//! phase and priority, cross-module events and queries, disabling a module
//! (`FeatureDisabled` to the client, systems skipped, queries refused), replay
//! of all of it from the cell's own log, and zero allocation on the hot path
//! with module traffic.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::{NativeAdapter, ServerFrame, decode_server_frame};
use mantis_adapter_contract::{
    AppearanceId, Channel, ConnectionId, ExtensionKind, ExtensionRefusal, MovementMode, Outbound,
    SnapshotFrame, WireAdapter,
};
use mantis_core::ecs::{Resource, World};
use mantis_core::graph::GraphCatalog;
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::kinematics::FlatGround;
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::module::{
    Discovered, Event, Events, Query, QueryError, ask, parse_manifest, parse_package, resolve,
};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_server::cell::{BoxedSink, Cell, CellConfig, MemoryLog, NullSink, OutboundSink};
use mantis_server::components::ReplicationId;
use mantis_server::intent::{CellIntent, CellLogSchema};
use mantis_server::modules::{
    ModuleCommand, ModuleOutcome, ModuleSet, Payload, Registrar, RegistryError, Require, Route, ServerModule,
};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

// ---- the "contract crate" of test.counter ----------------------------------

const BUMP: ExtensionKind = ExtensionKind(900);
const DEPOSIT: ExtensionKind = ExtensionKind(901);

#[derive(Clone, Copy, PartialEq, Debug)]
struct Bumped {
    by: u8,
}

impl StateHash for Bumped {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u8(self.by);
    }
}

impl Event for Bumped {
    const NAME: &'static str = "test.counter.bumped";
}

struct Hits;

impl Query for Hits {
    type Response = u32;
    const NAME: &'static str = "test.counter.hits";
}

// ---- test.counter, server half -------------------------------------------

#[derive(Debug, Default)]
struct Counter {
    hits: u32,
    ticks: u32,
    balance: u32,
}

impl StateHash for Counter {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.hits);
        h.write_u32(self.ticks);
        h.write_u32(self.balance);
    }
}

impl Resource for Counter {
    const NAME: &'static str = "test.counter.state";
}

struct CounterModule;

fn bump(world: &mut World, _: &TickContext, _: SessionId, payload: &[u8]) -> Result<(), ExtensionRefusal> {
    let by = *payload.first().ok_or(ExtensionRefusal::Invalid)?;
    world
        .resource_mut::<Counter>()
        .ok_or(ExtensionRefusal::Invalid)?
        .hits += u32::from(by);
    world
        .resource_mut::<Events<Bumped>>()
        .ok_or(ExtensionRefusal::Invalid)?
        .send(Bumped { by });
    Ok(())
}

fn deposit(world: &mut World, _: &TickContext, c: &ModuleCommand) -> ModuleOutcome {
    let amount = c
        .payload
        .as_slice()
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes);
    let Some(state) = world.resource_mut::<Counter>().filter(|_| amount > 0) else {
        return ModuleOutcome::refused(c, ExtensionRefusal::NotAllowed);
    };
    state.balance += amount;
    ModuleOutcome {
        kind: c.kind,
        session: c.session,
        result: Ok(()),
        payload: Payload::from_slice(&state.balance.to_le_bytes()).unwrap_or(Payload::EMPTY),
    }
}

impl ServerModule for CounterModule {
    fn key(&self) -> &'static str {
        "test.counter"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Counter::default())?;
        r.event::<Bumped>(16)?;
        r.handler(BUMP, Require::Avatar, bump)?;
        r.command(DEPOSIT, deposit)?;
        r.query::<Hits>(|w, _| w.resource::<Counter>().map_or(0, |c| c.hits))?;
        let access = r.access().write_resource::<Counter>().build()?;
        r.system(
            SystemDesc {
                name: "test.counter.tick",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
                w.resource_mut::<Counter>()
                    .ok_or(SystemError::Invariant("counter"))?
                    .ticks += 1;
                Ok(())
            },
        )?;
        let _ = r.metric("bumps");
        Ok(())
    }
}

// ---- test.listener: depends on the counter's contract only -----------------

#[derive(Debug, Default)]
struct Heard {
    events: u32,
    total: u32,
    last_answer: Option<u32>,
    disabled_answers: u32,
}

impl StateHash for Heard {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.events);
        h.write_u32(self.total);
        self.last_answer.state_hash(h);
        h.write_u32(self.disabled_answers);
    }
}

impl Resource for Heard {
    const NAME: &'static str = "test.listener.heard";
}

struct ListenerModule;

impl ServerModule for ListenerModule {
    fn key(&self) -> &'static str {
        "test.listener"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Heard::default())?;
        r.event::<Bumped>(16)?;
        let access = r
            .access()
            .read_resource::<Events<Bumped>>()
            .write_resource::<Heard>()
            .build()?;
        r.system(
            SystemDesc {
                name: "test.listener.listen",
                phase: Phase::Effects,
                priority: 0,
                access,
            },
            |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
                let (n, sum) = w.resource::<Events<Bumped>>().map_or((0, 0), |q| {
                    (
                        q.read().len() as u32,
                        q.read().iter().map(|b| u32::from(b.by)).sum(),
                    )
                });
                let answer = ask(w, &Hits);
                let heard = w.resource_mut::<Heard>().ok_or(SystemError::Invariant("heard"))?;
                heard.events += n;
                heard.total += sum;
                match answer {
                    Ok(v) => heard.last_answer = Some(v),
                    Err(QueryError::FeatureDisabled(_)) => heard.disabled_answers += 1,
                    Err(QueryError::NoProvider(_)) => return Err(SystemError::Invariant("no provider")),
                }
                Ok(())
            },
        )?;
        Ok(())
    }
}

fn module_set() -> ModuleSet {
    let found = vec![
        Discovered {
            origin: "test".into(),
            manifest: parse_manifest("[module]\nkey = \"test.counter\"\nversion = \"0.1.0\"\n").unwrap(),
        },
        Discovered {
            origin: "test".into(),
            manifest: parse_manifest(
                "[module]\nkey = \"test.listener\"\nversion = \"0.1.0\"\n[dependencies]\n\"test.counter\" = \"0.1\"\n",
            )
            .unwrap(),
        },
    ];
    let graph = resolve(
        &parse_package("[package]\nname = \"test\"\n").unwrap(),
        &found,
        &BTreeMap::new(),
    )
    .unwrap();
    // Registration follows the graph, not the order modules are linked in.
    ModuleSet::new(graph, &[Arc::new(ListenerModule), Arc::new(CounterModule)]).unwrap()
}

#[derive(Default)]
struct Capture(Vec<(ConnectionId, Vec<u8>)>);

impl OutboundSink for Capture {
    fn send(&mut self, _a: usize, conn: ConnectionId, _ch: Channel, bytes: &[u8]) {
        self.0.push((conn, bytes.to_vec()));
    }
}

fn refusals(cap: &mut Capture) -> Vec<(u64, ExtensionKind, ExtensionRefusal)> {
    let mut out = Vec::new();
    let mut scratch = SnapshotFrame::with_capacity(64, 64, 64, 64);
    let none: [SnapshotFrame; 0] = [];
    for (conn, bytes) in cap.0.drain(..) {
        if let Ok(ServerFrame::Message(Outbound::ExtensionRefused(r))) =
            decode_server_frame(&bytes, &none[..], &mut scratch)
        {
            out.push((conn.0, r.kind, r.reason));
        }
    }
    out
}

fn cell(log: Option<LogWriter<CellLogSchema, BoxedSink>>) -> Cell {
    let mut cfg = CellConfig::new(CellId(1), Seed(5));
    cfg.max_entities = 64;
    cfg.max_clients = 16;
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("test.native"))];
    let mut c = Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        log,
        Arc::new(GraphCatalog::new()),
        BTreeMap::new(),
    )
    .unwrap();
    c.install_modules(&module_set()).unwrap();
    c
}

fn join(c: &mut Cell, s: u64) {
    c.inbox().push(
        SessionId(s),
        CellIntent::Join {
            repl: ReplicationId(EntityId::new(s as u32, 0)),
            spawn: Vec3::new(s as f32, 0.0, 0.0),
            yaw: Angle16(0),
            look: AppearanceId(1),
            mode: MovementMode::Predictive,
            epoch: 1,
            character: s,
        },
    );
    c.attach_client(SessionId(s), ConnectionId(s), 0, false).unwrap();
}

fn ext(kind: ExtensionKind, bytes: &[u8]) -> CellIntent {
    CellIntent::Extension {
        kind,
        request: 0,
        payload: Payload::from_slice(bytes).unwrap(),
    }
}

fn command(kind: ExtensionKind, session: u64, amount: u32) -> ModuleCommand {
    ModuleCommand {
        kind,
        session: Some(SessionId(session)),
        request: 0,
        payload: Payload::from_slice(&amount.to_le_bytes()).unwrap(),
    }
}

/// Plays the scripted scenario against `c`; returns the per-tick hashes.
fn script(c: &mut Cell, cap: &mut Capture) -> Vec<u64> {
    let mut hashes = Vec::new();
    join(c, 1);
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    // Tick 2: two bumps, a deposit, a refused deposit, an unknown kind.
    c.inbox().push(SessionId(1), ext(BUMP, &[3]));
    c.inbox().push(SessionId(1), ext(BUMP, &[4]));
    c.inbox().push(SessionId(1), ext(ExtensionKind(4242), &[1]));
    c.commands().push(command(DEPOSIT, 1, 50));
    c.commands().push(command(DEPOSIT, 1, 0));
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    // Tick 3: events from tick 2 are delivered now.
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    // Tick 4: the counter is switched off (a logged intent with no session).
    c.inbox().push(
        SessionId(0),
        CellIntent::SetModule {
            module: 0,
            enabled: false,
        },
    );
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    // Tick 5: everything the counter offers answers FeatureDisabled.
    c.inbox().push(SessionId(1), ext(BUMP, &[9]));
    c.commands().push(command(DEPOSIT, 1, 7));
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    // Tick 6: back on.
    c.inbox().push(
        SessionId(0),
        CellIntent::SetModule {
            module: 0,
            enabled: true,
        },
    );
    c.inbox().push(SessionId(1), ext(BUMP, &[1]));
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    hashes.push(c.tick(cap, None).unwrap().state_hash);
    hashes
}

#[test]
fn modules_register_dispatch_disable_and_replay() {
    let header = LogHeader {
        build: BuildId([2; 32]),
        content: ContentHash::of(b"test.modules"),
        cell: CellId(1),
        seed: Seed(5),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let mut c = cell(Some(LogWriter::create(sink, &header, 1 << 16).unwrap()));
    assert_eq!(c.extension_route(BUMP), Some(Route::Intent));
    assert_eq!(c.extension_route(DEPOSIT), Some(Route::Command));
    assert_eq!(c.extension_route(ExtensionKind(4242)), None);
    let mut cap = Capture::default();
    // Before joining, a module handler refuses (Require::Avatar).
    c.inbox().push(SessionId(9), ext(BUMP, &[1]));
    let hashes = script(&mut c, &mut cap);

    let counter = c.world().resource::<Counter>().unwrap();
    assert_eq!(counter.hits, 3 + 4 + 1, "bumps applied; the disabled one was not");
    assert_eq!(counter.balance, 50, "one deposit succeeded");
    assert_eq!(
        counter.ticks,
        7 - 2,
        "the counter's system was skipped while disabled (ticks 4 and 5)"
    );
    let heard = c.world().resource::<Heard>().unwrap();
    assert_eq!(
        (heard.events, heard.total),
        (3, 8),
        "events delivered the tick after they were sent"
    );
    assert_eq!(
        heard.disabled_answers, 2,
        "queries answered FeatureDisabled while off"
    );
    assert_eq!(heard.last_answer, Some(8));
    let mut outcomes = Vec::new();
    c.drain_outcomes(|o| outcomes.push(*o));
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].result, Ok(()));
    assert_eq!(outcomes[0].payload.as_slice(), 50u32.to_le_bytes());
    assert_eq!(outcomes[1].result, Err(ExtensionRefusal::NotAllowed));
    assert_eq!(outcomes[2].result, Err(ExtensionRefusal::FeatureDisabled));
    assert_eq!(c.metrics().unwrap().get("test.counter.bumps"), Some(0));
    // The client heard every refusal, with the reason. Commands run at the
    // start of a tick, before its intents.
    let got = refusals(&mut cap);
    assert_eq!(
        got,
        vec![
            (1, DEPOSIT, ExtensionRefusal::NotAllowed),
            (1, ExtensionKind(4242), ExtensionRefusal::Invalid),
            (1, DEPOSIT, ExtensionRefusal::FeatureDisabled),
            (1, BUMP, ExtensionRefusal::FeatureDisabled),
        ]
    );

    // Replay: commands are inputs, outcomes are checked, flags are intents.
    drop(c);
    let bytes = memory.bytes();
    let mut reader =
        LogReader::<CellLogSchema>::open(&bytes, BuildId([2; 32]), ContentHash::of(b"test.modules")).unwrap();
    let mut fresh = cell(None);
    let report = replay(&mut fresh, &mut reader).unwrap();
    assert_eq!(report.ticks, hashes.len() as u64);
    assert_eq!(fresh.world().state_hash(), *hashes.last().unwrap());
    assert_eq!(fresh.world().resource::<Counter>().unwrap().balance, 50);
}

#[test]
fn a_tampered_outcome_fails_replay() {
    let header = LogHeader {
        build: BuildId([3; 32]),
        content: ContentHash::of(b"t"),
        cell: CellId(1),
        seed: Seed(5),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let mut c = cell(Some(LogWriter::create(sink, &header, 1 << 16).unwrap()));
    script(&mut c, &mut Capture::default());
    drop(c);
    let mut bytes = memory.bytes();
    // The deposit's outcome payload is the new balance, 50 = 0x32: flip it.
    let at = bytes.windows(4).rposition(|w| w == 50u32.to_le_bytes()).unwrap();
    bytes[at] = 51;
    let reader = LogReader::<CellLogSchema>::open(&bytes, BuildId([3; 32]), ContentHash::of(b"t"));
    // Either the checksum catches the edit or the outcome check does.
    if let Ok(mut reader) = reader {
        assert!(replay(&mut cell(None), &mut reader).is_err());
    }
}

#[test]
fn module_traffic_allocates_nothing_on_the_hot_path() {
    let mut c = cell(None);
    for s in 1..=8u64 {
        join(&mut c, s);
    }
    let mut sink = NullSink;
    for _ in 0..30 {
        for s in 1..=8u64 {
            c.inbox().push(SessionId(s), ext(BUMP, &[1]));
            c.inbox().ack(SessionId(s), c.tick_now());
        }
        c.commands().push(command(DEPOSIT, 1, 1));
        c.tick(&mut sink, None).unwrap();
        c.drain_outcomes(|_| {});
    }
    for _ in 0..30 {
        for s in 1..=8u64 {
            c.inbox().push(SessionId(s), ext(BUMP, &[1]));
            c.inbox().ack(SessionId(s), c.tick_now());
        }
        c.commands().push(command(DEPOSIT, 1, 1));
        assert_no_alloc("cell tick with module traffic", || c.tick(&mut sink, None)).unwrap();
        c.drain_outcomes(|_| {});
    }
    assert_eq!(c.world().resource::<Counter>().unwrap().balance, 60);
}

#[test]
fn registration_is_checked() {
    struct BadName;
    impl ServerModule for BadName {
        fn key(&self) -> &'static str {
            "test.counter"
        }
        fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
            r.system(
                SystemDesc {
                    name: "other.thing",
                    phase: Phase::Timers,
                    priority: 0,
                    access: mantis_core::ecs::Access::NONE,
                },
                |_: &mut World, _: &TickContext| -> Result<(), SystemError> { Ok(()) },
            )
        }
    }
    let found = vec![Discovered {
        origin: "test".into(),
        manifest: parse_manifest("[module]\nkey = \"test.counter\"\nversion = \"0.1.0\"\n").unwrap(),
    }];
    let graph = resolve(
        &parse_package("[package]\nname = \"test\"\n").unwrap(),
        &found,
        &BTreeMap::new(),
    )
    .unwrap();
    let mut cfg = CellConfig::new(CellId(1), Seed(5));
    cfg.max_entities = 8;
    let mut c = Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        vec![],
        None,
        Arc::new(GraphCatalog::new()),
        BTreeMap::new(),
    )
    .unwrap();
    let set = ModuleSet::new(graph.clone(), &[Arc::new(BadName)]).unwrap();
    assert_eq!(
        c.install_modules(&set),
        Err(RegistryError::SystemName("other.thing"))
    );
    assert!(matches!(
        ModuleSet::new(graph, &[Arc::new(ListenerModule)]),
        Err(RegistryError::NotLinked(k)) if k == "test.counter"
    ));
}

fn feature_states(cap: &mut Capture) -> Vec<(u64, String, bool)> {
    let mut out = Vec::new();
    let mut scratch = SnapshotFrame::with_capacity(64, 64, 64, 64);
    let none: [SnapshotFrame; 0] = [];
    for (conn, bytes) in cap.0.drain(..) {
        if let Ok(ServerFrame::Message(Outbound::FeatureState(f))) =
            decode_server_frame(&bytes, &none[..], &mut scratch)
        {
            out.push((conn.0, f.module.as_str().to_owned(), f.enabled));
        }
    }
    out
}

#[test]
fn clients_learn_feature_states_on_join_and_on_every_switch() {
    let mut c = cell(None);
    let mut cap = Capture::default();
    join(&mut c, 1);
    c.tick(&mut cap, None).unwrap();
    assert_eq!(
        feature_states(&mut cap),
        [
            (1, "test.counter".to_owned(), true),
            (1, "test.listener".to_owned(), true)
        ]
    );
    join(&mut c, 2);
    c.tick(&mut cap, None).unwrap();
    assert_eq!(feature_states(&mut cap).len(), 2, "only the newcomer is told");
    c.inbox().push(
        SessionId(0),
        CellIntent::SetModule {
            module: 1,
            enabled: false,
        },
    );
    c.tick(&mut cap, None).unwrap();
    let got = feature_states(&mut cap);
    assert_eq!(got.len(), 4, "everyone is told after a switch");
    assert!(got.contains(&(2, "test.listener".to_owned(), false)));
    c.tick(&mut cap, None).unwrap();
    assert!(feature_states(&mut cap).is_empty(), "and only then");
}
