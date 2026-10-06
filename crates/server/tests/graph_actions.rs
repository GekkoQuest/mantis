//! Gameplay graph actions registered by modules (decision 0021): a module
//! declares its actions in its manifest and registers their handlers; the
//! cell's catalog gives them ids in module order; a graph's `Action` node
//! runs the module's rule inside graph evaluation; a disabled module's
//! action fails closed; it all replays from the cell's log. Registration is
//! refused when the manifest and the registrations disagree.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::*;
use mantis_adapter_contract::native::NativeAdapter;
use mantis_adapter_contract::{AbilityId, AppearanceId, Cast, ConnectionId, MovementMode, WireAdapter};
use mantis_core::ecs::{Resource, World};
use mantis_core::graph::{
    ActionCall, ActionError, ActionParams, GameplayGraph, GraphCatalog, GraphId, Node, NodeKey, NodeKind,
    Target,
};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::kinematics::FlatGround;
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::module::{Discovered, parse_manifest, parse_package, resolve};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;
use mantis_server::cell::{BoxedSink, Cell, CellConfig, MemoryLog, NullSink};
use mantis_server::components::ReplicationId;
use mantis_server::intent::{CellIntent, CellLogSchema};
use mantis_server::modules::{ModuleSet, Registrar, RegistryError, ServerModule};

const HEAL: &str = "test.mend.heal";
const MEND: AbilityId = AbilityId(7);
const MEND_GRAPH: GraphId = GraphId::named("test.ability.mend");

#[derive(Debug, Default)]
struct Mended(i64);

impl StateHash for Mended {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0.cast_unsigned());
    }
}

impl Resource for Mended {
    const NAME: &'static str = "test.mend.total";
}

fn heal(world: &mut World, call: &ActionCall) -> Result<(), ActionError> {
    let m = world.resource_mut::<Mended>().ok_or(ActionError("mend state"))?;
    m.0 =
        m.0.checked_add(i64::from(call.params.0[0]))
            .ok_or(ActionError("overflow"))?;
    Ok(())
}

/// How the test module registers.
#[derive(Clone, Copy)]
enum Registers {
    /// Its declared action.
    Declared,
    /// An action it did not declare.
    Undeclared,
    /// Nothing.
    Nothing,
}

struct MendModule(Registers);

impl ServerModule for MendModule {
    fn key(&self) -> &'static str {
        "test.mend"
    }

    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.resource(Mended::default())?;
        match self.0 {
            Registers::Declared => r.graph_action(HEAL, heal),
            Registers::Undeclared => r.graph_action("test.mend.other", heal),
            Registers::Nothing => Ok(()),
        }
    }
}

const MANIFEST: &str =
    "[module]\nkey = \"test.mend\"\nversion = \"0.1.0\"\ngraph_actions = [\"test.mend.heal\"]\n";

fn module_set(registers: Registers) -> ModuleSet {
    let found = vec![Discovered {
        origin: "test".into(),
        manifest: parse_manifest(MANIFEST).unwrap(),
    }];
    let graph = resolve(
        &parse_package("[package]\nname = \"test\"\n").unwrap(),
        &found,
        &BTreeMap::new(),
    )
    .unwrap();
    ModuleSet::new(graph, &[Arc::new(MendModule(registers))]).unwrap()
}

/// The catalog as a cell loads it: the set's declared actions, then the
/// graphs (here built in code: heal 5, wait 2 ticks, heal 5).
fn catalog(set: &ModuleSet) -> GraphCatalog {
    let mut c = GraphCatalog::new();
    for a in set.graph().graph_actions() {
        c.register_action(a).unwrap();
    }
    let heal = c.action_id(HEAL).unwrap();
    let act = |key, next| Node {
        key: NodeKey(key),
        kind: NodeKind::Action {
            action: heal,
            target: Target::Source,
            params: ActionParams([5, 0, 0, 0]),
            next,
        },
    };
    c.insert(GameplayGraph {
        id: MEND_GRAPH,
        entry: NodeKey(1),
        nodes: vec![
            act(1, Some(NodeKey(2))),
            Node {
                key: NodeKey(2),
                kind: NodeKind::Delay {
                    ticks: 2,
                    next: Some(NodeKey(3)),
                },
            },
            act(3, None),
        ],
    })
    .unwrap();
    c
}

fn cell(
    log: Option<LogWriter<CellLogSchema, BoxedSink>>,
    registers: Registers,
    with_actions: bool,
) -> Result<Cell, RegistryError> {
    let mut cfg = CellConfig::new(CellId(1), Seed(9));
    cfg.max_entities = 16;
    cfg.max_clients = 4;
    let set = module_set(registers);
    let catalog = if with_actions {
        catalog(&set)
    } else {
        GraphCatalog::new()
    };
    let adapters: Vec<Arc<dyn WireAdapter>> = vec![Arc::new(NativeAdapter::new("test.native"))];
    let mut c = Cell::new(
        cfg,
        Arc::new(FlatGround(0.0)),
        adapters,
        log,
        Arc::new(catalog),
        BTreeMap::from([(MEND, MEND_GRAPH)]),
    )
    .unwrap();
    c.install_modules(&set)?;
    Ok(c)
}

fn mended(c: &Cell) -> i64 {
    c.world().resource::<Mended>().unwrap().0
}

fn cast(c: &mut Cell) {
    let view_tick = c.tick_now();
    c.inbox().push(
        SessionId(1),
        CellIntent::Cast(Cast {
            ability: MEND,
            target: None,
            view_tick,
            view_frac: 0,
        }),
    );
}

#[test]
fn a_graph_action_runs_the_module_rule_fails_closed_when_disabled_and_replays() {
    let header = LogHeader {
        build: BuildId([6; 32]),
        content: ContentHash::of(b"test.graph_actions"),
        cell: CellId(1),
        seed: Seed(9),
        start_tick: Tick(1),
    };
    let memory = MemoryLog::default();
    let sink: BoxedSink = Box::new(memory.clone());
    let mut c = cell(
        Some(LogWriter::create(sink, &header, 1 << 16).unwrap()),
        Registers::Declared,
        true,
    )
    .unwrap();
    let mut hashes = Vec::new();
    let mut tick = |c: &mut Cell| hashes.push(c.tick(&mut NullSink, None).unwrap().state_hash);
    c.inbox().push(
        SessionId(1),
        CellIntent::Join {
            repl: ReplicationId(EntityId::new(1, 0)),
            spawn: Vec3::new(0.0, 0.0, 0.0),
            yaw: Angle16(0),
            look: AppearanceId(1),
            mode: MovementMode::Predictive,
            epoch: 1,
            character: 1,
        },
    );
    c.attach_client(SessionId(1), ConnectionId(1), 0, false).unwrap();
    tick(&mut c);
    cast(&mut c);
    tick(&mut c);
    assert_eq!(mended(&c), 5, "the first action ran in the cast's tick");
    for _ in 0..4 {
        tick(&mut c);
    }
    assert_eq!(mended(&c), 10, "the second ran after the delay");

    // Disabled, the module's action fails closed: the instance stops and
    // nothing changes.
    c.inbox().push(
        SessionId(0),
        CellIntent::SetModule {
            module: 0,
            enabled: false,
        },
    );
    tick(&mut c);
    cast(&mut c);
    for _ in 0..4 {
        tick(&mut c);
    }
    assert_eq!(mended(&c), 10);

    drop(c);
    let bytes = memory.bytes();
    let mut reader =
        LogReader::<CellLogSchema>::open(&bytes, BuildId([6; 32]), ContentHash::of(b"test.graph_actions"))
            .unwrap();
    let mut fresh = cell(None, Registers::Declared, true).unwrap();
    let report = replay(&mut fresh, &mut reader).unwrap();
    assert_eq!(report.ticks, hashes.len() as u64);
    assert_eq!(fresh.world().state_hash(), *hashes.last().unwrap());
    assert_eq!(mended(&fresh), 10);
}

#[test]
fn registration_must_match_the_manifest_and_the_catalog() {
    assert_eq!(
        cell(None, Registers::Undeclared, true).err(),
        Some(RegistryError::UndeclaredAction("test.mend.other".to_owned()))
    );
    assert_eq!(
        cell(None, Registers::Nothing, true).err(),
        Some(RegistryError::UnregisteredAction(HEAL.to_owned()))
    );
    assert_eq!(
        cell(None, Registers::Declared, false).err(),
        Some(RegistryError::ActionNotInCatalog(HEAL.to_owned()))
    );
    assert_eq!(module_set(Registers::Declared).graph().graph_actions(), [HEAL]);
}
