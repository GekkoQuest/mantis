//! Server scripts inside a cell: script-driven outcomes (gameplay graphs,
//! variables, events) replay identically from the cell's log; an infinite
//! loop is stopped by the budget and the cell keeps ticking (its timing
//! budget is measured in `packages/toy/server/tests/budget_timing.rs`); the hot path
//! stays allocation-free outside the scripts' exempt scope; hot reload
//! happens at a tick boundary.

#![expect(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::NativeAdapter;
use mantis_adapter_contract::{AppearanceId, MovementMode, WireAdapter};
use mantis_core::graph::{
    GameplayGraph, GraphCatalog, GraphId, GraphRuntime, MarkerSpec, Node, NodeKey, NodeKind,
};
use mantis_core::kinematics::FlatGround;
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::module::{Discovered, parse_manifest, parse_package, resolve};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;
use mantis_script::Limits;
use mantis_server::cell::{BoxedSink, Cell, CellConfig, MemoryLog, NullSink};
use mantis_server::components::ReplicationId;
use mantis_server::intent::{CellIntent, CellLogSchema};
use mantis_server::modules::ModuleSet;
use mantis_server::scripting::{ScriptModule, ScriptSource, ScriptVars, Scripts};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, exempt};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

const PATROL: &str = r#"
state.pulses = state.pulses or 0
mantis.on("alarm", "on_alarm")
function on_tick(t)
    if t % 10 == 0 then
        local me = mantis.owner()
        local x, y, z = host.position(me)
        if x and math.random(4) <= 3 then
            host.trigger("test.pulse", me)
            state.pulses = state.pulses + 1
            host.set("pulses", state.pulses)
            host.set("last_x", math.floor(x * 100))
            host.emit("alarm", me)
        end
    end
end
function on_alarm(who)
    state.alarms = (state.alarms or 0) + 1
    host.set("neighbours", #({host.near(who, 50)}))
end
"#;

const SPIN: &str = "function on_tick() while true do end end";

fn catalog() -> GraphCatalog {
    let mut c = GraphCatalog::new();
    c.insert(GameplayGraph {
        id: GraphId::named("test.pulse"),
        entry: NodeKey(1),
        nodes: vec![
            Node {
                key: NodeKey(1),
                kind: NodeKind::Marker {
                    marker: MarkerSpec::CastStart,
                    offset: 0,
                    next: Some(NodeKey(2)),
                },
            },
            Node {
                key: NodeKey(2),
                kind: NodeKind::Delay {
                    ticks: 3,
                    next: Some(NodeKey(3)),
                },
            },
            Node {
                key: NodeKey(3),
                kind: NodeKind::Marker {
                    marker: MarkerSpec::Expire,
                    offset: 0,
                    next: None,
                },
            },
        ],
    })
    .unwrap();
    c
}

fn set(scripts: &[(&str, &str)], wrapper: mantis_script::ScriptWrapper) -> ModuleSet {
    let manifest = parse_manifest("[module]\nkey = \"test.scripts\"\nversion = \"0.1.0\"\n").unwrap();
    let graph = resolve(
        &parse_package("[package]\nname = \"test\"\n").unwrap(),
        &[Discovered {
            origin: "test".into(),
            manifest,
        }],
        &BTreeMap::new(),
    )
    .unwrap();
    let sources = scripts
        .iter()
        .map(|(name, source)| ScriptSource {
            name: (*name).to_owned(),
            source: (*source).to_owned(),
            owner: EntityId::new(1, 0),
        })
        .collect();
    let module = ScriptModule::new("test.scripts", sources, Limits::DEFAULT).with_wrapper(wrapper);
    ModuleSet::new(graph, &[Arc::new(module)]).unwrap()
}

fn identity(f: &mut dyn FnMut()) {
    f();
}

fn harness_exempt(f: &mut dyn FnMut()) {
    exempt(f);
}

fn cell(log: Option<LogWriter<CellLogSchema, BoxedSink>>, set: &ModuleSet, entities: u64) -> Cell {
    let mut cfg = CellConfig::new(CellId(1), Seed(3));
    cfg.max_entities = 256;
    cfg.max_clients = 64;
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("test.native"))];
    let mut c = Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        log,
        Arc::new(catalog()),
        BTreeMap::new(),
    )
    .unwrap();
    c.install_modules(set).unwrap();
    for s in 1..=entities {
        c.inbox().push(
            SessionId(s),
            CellIntent::Join {
                repl: ReplicationId(EntityId::new(s as u32, 0)),
                spawn: Vec3::new(s as f32 * 3.0, 0.0, 0.0),
                yaw: Angle16(0),
                look: AppearanceId(1),
                mode: MovementMode::Predictive,
                epoch: 1,
                character: s,
            },
        );
    }
    c
}

#[test]
fn script_driven_outcomes_replay_from_the_log() {
    let header = LogHeader {
        build: BuildId([4; 32]),
        content: ContentHash::of(b"scripts"),
        cell: CellId(1),
        seed: Seed(3),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let modules = set(&[("patrol", PATROL)], identity);
    let mut c = cell(
        Some(LogWriter::create(sink, &header, 1 << 20).unwrap()),
        &modules,
        3,
    );
    let mut graphs_seen = 0;
    let mut hashes = Vec::new();
    for _ in 0..120 {
        let r = c.tick(&mut NullSink, None).unwrap();
        hashes.push(r.state_hash);
        graphs_seen = graphs_seen.max(c.world().resource::<GraphRuntime>().unwrap().len());
    }
    let vars = c.world().resource::<ScriptVars>().unwrap().clone();
    let pulses = vars.0["pulses"];
    assert!(pulses >= 3.0, "the script drove graphs: {vars:?}");
    assert!(graphs_seen >= 1, "graphs ran");
    assert_eq!(
        vars.0["neighbours"], 2.0,
        "events reached the subscriber next tick"
    );
    let stats = c.world().resource::<Scripts>().unwrap().stats;
    assert_eq!(
        stats.errors,
        0,
        "{:?}",
        c.world().resource::<Scripts>().unwrap().last_errors
    );
    drop(c);
    let bytes = memory.bytes();
    let mut reader =
        LogReader::<CellLogSchema>::open(&bytes, BuildId([4; 32]), ContentHash::of(b"scripts")).unwrap();
    let mut fresh = cell(None, &modules, 0);
    let report = replay(&mut fresh, &mut reader).unwrap();
    assert_eq!(report.ticks, 120);
    assert_eq!(
        fresh.world().state_hash(),
        *hashes.last().unwrap(),
        "script state and outcomes replay exactly"
    );
    assert_eq!(fresh.world().resource::<ScriptVars>().unwrap(), &vars);
}

#[test]
fn an_infinite_loop_is_stopped_and_the_cell_keeps_ticking() {
    let modules = set(&[("spin", SPIN), ("patrol", PATROL)], identity);
    let mut c = cell(None, &modules, 3);
    for _ in 0..40 {
        c.tick(&mut NullSink, None).unwrap();
    }
    let s = c.world().resource::<Scripts>().unwrap().stats;
    assert_eq!(s.budget_exhausted, 40, "the spinner hits the budget every tick");
    assert!(c.world().resource::<ScriptVars>().unwrap().0.is_empty() || s.calls > 40);
}

#[test]
fn scripts_allocate_only_inside_their_exempt_scope() {
    let emitter = "mantis.on('x', 'f') function f() end function on_tick() host.emit('x', 1) end";
    let modules = set(&[("patrol", PATROL), ("emitter", emitter)], harness_exempt);
    let mut c = cell(None, &modules, 3);
    for _ in 0..40 {
        c.tick(&mut NullSink, None).unwrap();
    }
    for _ in 0..60 {
        assert_no_alloc("cell tick with scripts", || c.tick(&mut NullSink, None)).unwrap();
    }
}

#[test]
fn a_reload_happens_at_the_tick_boundary_with_state_kept() {
    let modules = set(&[("patrol", PATROL)], identity);
    let mut c = cell(None, &modules, 3);
    for _ in 0..60 {
        c.tick(&mut NullSink, None).unwrap();
    }
    let pulses = |c: &Cell| {
        c.world()
            .resource::<ScriptVars>()
            .unwrap()
            .0
            .get("pulses")
            .copied()
            .unwrap_or(0.0)
    };
    let before = pulses(&c);
    // A new version counts by ten, starting from the kept state.
    let v2 = PATROL.replace(
        "state.pulses = state.pulses + 1",
        "state.pulses = state.pulses + 10",
    );
    assert!(c.reload_script("patrol", &v2));
    for _ in 0..60 {
        c.tick(&mut NullSink, None).unwrap();
    }
    let after = pulses(&c);
    let gained = after - before;
    assert!(
        gained > 0.0 && gained % 10.0 == 0.0,
        "the new code counted on the old state: {before} -> {after}"
    );
}

#[test]
fn a_logged_reload_replays_only_with_the_same_source() {
    let header = LogHeader {
        build: BuildId([5; 32]),
        content: ContentHash::of(b"reload"),
        cell: CellId(1),
        seed: Seed(3),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let modules = set(&[("patrol", PATROL)], identity);
    let mut c = cell(
        Some(LogWriter::create(sink, &header, 1 << 20).unwrap()),
        &modules,
        3,
    );
    for _ in 0..30 {
        c.tick(&mut NullSink, None).unwrap();
    }
    let v2 = PATROL.replace(
        "state.pulses = state.pulses + 1",
        "state.pulses = state.pulses + 10",
    );
    assert!(c.reload_script("patrol", &v2));
    let mut last = 0;
    for _ in 0..60 {
        last = c.tick(&mut NullSink, None).unwrap().state_hash;
    }
    drop(c);
    let bytes = memory.bytes();
    let open =
        || LogReader::<CellLogSchema>::open(&bytes, BuildId([5; 32]), ContentHash::of(b"reload")).unwrap();
    // Without the reloaded source, replay refuses at the reload.
    let mut blind = cell(None, &modules, 0);
    assert!(replay(&mut blind, &mut open()).is_err());
    // With it, replay is exact.
    let mut informed = cell(None, &modules, 0);
    informed.supply_script_source(&v2);
    let report = replay(&mut informed, &mut open()).unwrap();
    assert_eq!(report.ticks, 90);
    assert_eq!(informed.world().state_hash(), last);
}

const KEEPER: &str = r#"
state.n = state.n or 0
state.log = state.log or {}
mantis.on("alarm", "on_alarm")
function on_tick(t)
    state.n = state.n + 1
    if t % 7 == 0 then
        state.log[#state.log + 1] = { at = t, roll = math.random(100) }
        mantis.after(3, "later", t)
    end
end
function later(t)
    state.last_later = t
    host.emit("alarm", t)
end
function on_alarm(v)
    state.alarms = (state.alarms or 0) + 1
end
"#;

const KEEPER_V2: &str = r#"
state.n = state.n or 0
function on_tick(t)
    state.n = state.n + 2
    if t % 5 == 0 then
        mantis.after(2, "later", t)
    end
end
function later(t)
    state.last_later = t
end
function on_alarm(v)
    state.alarms = (state.alarms or 0) + 10
end
"#;

/// Lead ruling (M8): script state is complete by construction (frozen
/// environments, a value lint on `state`), so a snapshot holds it: state,
/// timers, subscriptions, pending events, variables, and sources by hash.
#[test]
fn a_snapshot_of_a_scripted_cell_restores_bit_identically() {
    let modules = set(&[("patrol", PATROL), ("keeper", KEEPER)], identity);
    let mut original = cell(None, &modules, 3);
    for _ in 0..40 {
        original.tick(&mut NullSink, None).unwrap();
    }
    // A hot reload: the restored cell must load this source by its hash.
    original.reload_script("keeper", KEEPER_V2);
    for _ in 0..23 {
        original.tick(&mut NullSink, None).unwrap();
    }
    let build = BuildId([4; 32]);
    let content = ContentHash::of(b"scripts");
    let bytes = original.snapshot(build, content).unwrap();

    let mut restored = cell(None, &modules, 0);
    let header = restored.restore(&bytes).unwrap();
    assert_eq!(header.tick, original.tick_now());
    assert_eq!(restored.world().state_hash(), original.world().state_hash());
    for _ in 0..60 {
        let a = original.tick(&mut NullSink, None).unwrap();
        let b = restored.tick(&mut NullSink, None).unwrap();
        assert_eq!(
            (a.tick, a.state_hash),
            (b.tick, b.state_hash),
            "diverged after the restore"
        );
    }
    let vars = |c: &Cell| c.world().resource::<ScriptVars>().unwrap().0.clone();
    assert_eq!(vars(&original), vars(&restored));
}
