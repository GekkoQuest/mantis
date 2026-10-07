//! A cell: one simulation thread owning one region of a world or one instance
//! (plan 7.1). A process hosts many.
//!
//! One tick, in order:
//! 1. **Inbound**: module event queues advance; economy commands are logged,
//!    executed on delivery, and their outcomes logged; then every inbox item
//!    is stamped with the tick, appended to the unified log, checked against
//!    the session's allowed-state list, and dispatched (extension intents to
//!    the module that registered their kind).
//! 2. **Timers through Scripts**: the schedule, strictly sequential.
//!    Movement applies one input per Predictive avatar and checks Validated
//!    claims against the envelope.
//! 3. Corrections for Validated clients go out reliably.
//! 4. **Interest and Outbound**: the rewind buffer records positions, the
//!    replication view is rebuilt, and every client's interest and snapshot
//!    run as per-client jobs on the worker set (or inline), then the
//!    snapshots are sent.
//! 5. **Persist**: the world state hash closes the tick in the log, and the
//!    segment is flushed (fsync only for economy records).
//!
//! Steps 2 to 4 allocate nothing after warm-up; tests enforce it on the cell
//! thread and inside every job.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use mantis_adapter_contract::core_types::{EntityId, InputSeq, MotionModifiers, MotionState, Tick, Vec3};
use mantis_adapter_contract::{
    AbilityId, AdapterError, AppearanceId, Channel, ConnectionId, ExtensionKind, ExtensionRefusal,
    ExtensionRefused, LocalAvatar, Outbound, SnapshotHeader, WireAdapter,
};
use mantis_core::ecs::{Access, EcsError, Query, Read, Resource, World};
use mantis_core::graph::{ActionCall, ActionError, ActionHandler, GraphCatalog, GraphId, GraphRuntime};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::kinematics::{GroundQuery, Motion, MotionParams};
use mantis_core::log::{CellId, LogError, LogSink, LogWriter, SessionId};
use mantis_core::mem::BoundedVec;
use mantis_core::rng::Seed;
use mantis_core::schedule::{Phase, RunError, Schedule, ScheduleError, SystemDesc, SystemError, TickContext};
use mantis_core::schedule::{Stopwatch, Timing};
use mantis_core::time::{Clock, TickClock, TickRate};

use crate::components::{Body, Controlled, Look, Mods, ReplicationId};
use crate::intent::{CellIntent, CellLogSchema, Transfer};
use crate::interest::{RepView, Replicated, TierConfig};
use crate::jobs::{BatchReport, Completion, JobBatch, WorkerSet};
use crate::lock;
use crate::modules::{ModuleCommand, ModuleOutcome, ModuleSet, ModuleStates, Registry, RegistryError, Route};
use crate::movement::{Claim, EnvelopeConfig, InputConfig, buffer_input, handle_claim, next_input};
use crate::replication::{ClientCaps, ClientRep};
use crate::rewind::RewindBuffer;
use crate::session::{CellSession, EnvelopeState, Sessions, allowed, tick_ms};

/// A cell's tunables.
#[derive(Clone, Debug)]
pub struct CellConfig {
    /// Identity.
    pub id: CellId,
    /// Tick rate.
    pub rate: TickRate,
    /// Root random seed (logged).
    pub seed: Seed,
    /// The tick before the first one simulated.
    pub start: Tick,
    /// Movement tuning (package content).
    pub motion: MotionParams,
    /// Validated-mode tolerances.
    pub envelope: EnvelopeConfig,
    /// Predictive input realignment after latency rises.
    pub inputs: InputConfig,
    /// Interest tiers.
    pub tiers: TierConfig,
    /// Per-client replication capacities.
    pub caps: ClientCaps,
    /// Entities the cell can replicate and rewind.
    pub max_entities: usize,
    /// Client slots.
    pub max_clients: usize,
    /// Inbox items per tick before refusal.
    pub inbox_capacity: usize,
    /// Rewind buffer length in ticks.
    pub rewind_ticks: usize,
    /// Per-client jobs this cell may have on the worker set at once.
    pub job_quota: usize,
    /// Largest unreliable payload of any adapter's transport.
    pub max_payload: usize,
    /// Client module keys the package permits (sent to every session with
    /// the cell's tier).
    pub client_mods: Vec<String>,
    /// The x range this cell owns in a seamless world (`None`: everything).
    pub region: Option<(f32, f32)>,
    /// Width of the strip near a border whose entities are ghosted to the
    /// neighbour.
    pub ghost_margin: f32,
}

impl CellConfig {
    /// Defaults for a cell with `id` and `seed`.
    #[must_use]
    pub fn new(id: CellId, seed: Seed) -> Self {
        Self {
            id,
            rate: TickRate::HZ_30,
            seed,
            start: Tick::ZERO,
            motion: MotionParams::DEFAULT,
            envelope: EnvelopeConfig::DEFAULT,
            inputs: InputConfig::DEFAULT,
            tiers: TierConfig::DEFAULT,
            caps: ClientCaps::DEFAULT,
            max_entities: 1024,
            max_clients: 128,
            inbox_capacity: 4096,
            rewind_ticks: 32,
            job_quota: 8,
            max_payload: 1100,
            region: None,
            client_mods: Vec::new(),
            ghost_margin: 100.0,
        }
    }
}

/// A cell failed to start or to tick.
#[derive(Clone, PartialEq, Debug)]
pub enum CellError {
    /// ECS setup failed.
    Ecs(EcsError),
    /// Schedule setup failed.
    Schedule(ScheduleError),
    /// A system failed.
    Run(RunError),
    /// The log failed.
    Log(LogError),
    /// Invalid configuration.
    Config(&'static str),
    /// No free client slot.
    ClientsFull,
}

impl From<EcsError> for CellError {
    fn from(e: EcsError) -> Self {
        Self::Ecs(e)
    }
}

impl From<ScheduleError> for CellError {
    fn from(e: ScheduleError) -> Self {
        Self::Schedule(e)
    }
}

impl From<LogError> for CellError {
    fn from(e: LogError) -> Self {
        Self::Log(e)
    }
}

/// Where economy commands arrive (from the host and from services). Each
/// is logged and executed at the start of the next tick's Inbound phase.
pub struct CommandInbox {
    items: Mutex<VecDeque<ModuleCommand>>,
    capacity: usize,
}

impl CommandInbox {
    fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Queues a command. False when full.
    pub fn push(&self, command: ModuleCommand) -> bool {
        let mut q = lock(&self.items);
        if q.len() >= self.capacity {
            return false;
        }
        q.push_back(command);
        true
    }
}

/// Where a cell's network threads deliver inbound work.
pub struct Inbox {
    items: Mutex<VecDeque<(SessionId, CellIntent)>>,
    acks: Mutex<VecDeque<(SessionId, Tick)>>,
    capacity: usize,
    dropped: AtomicU64,
}

impl Inbox {
    fn new(capacity: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            acks: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            dropped: AtomicU64::new(0),
        }
    }

    /// Queues an intent. False (and counted) when the inbox is full.
    pub fn push(&self, session: SessionId, intent: CellIntent) -> bool {
        let mut q = lock(&self.items);
        if q.len() >= self.capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        q.push_back((session, intent));
        true
    }

    /// Queues a snapshot acknowledgement (replication only; not logged).
    pub fn ack(&self, session: SessionId, tick: Tick) {
        let mut q = lock(&self.acks);
        if q.len() < self.capacity {
            q.push_back((session, tick));
        }
    }

    /// Items refused because the inbox was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Where a cell sends encoded frames: the host's transports.
pub trait OutboundSink {
    /// Sends `bytes` to `conn` on the transport of adapter `adapter`.
    fn send(&mut self, adapter: usize, conn: ConnectionId, channel: Channel, bytes: &[u8]);
}

/// Stable identities to cell-local entities (simulation state).
#[derive(Default, Debug)]
pub struct ReplMap(pub BTreeMap<ReplicationId, EntityId>);

impl StateHash for ReplMap {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.0.len() as u64);
        for (r, e) in &self.0 {
            r.state_hash(h);
            e.state_hash(h);
        }
    }
}

impl Resource for ReplMap {
    const NAME: &'static str = "server.repl_map";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.0.len()).unwrap_or(u32::MAX));
        for (r, ent) in &self.0 {
            e.u64(r.0.to_bits());
            e.u64(ent.to_bits());
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        self.0.clear();
        let n = d.u32()?;
        for _ in 0..n {
            let r = ReplicationId(EntityId::from_bits(d.u64()?));
            let ent = EntityId::from_bits(d.u64()?);
            self.0.insert(r, ent);
        }
        Ok(())
    }
}

/// The per-client job batch of a cell.
struct CellRep {
    view: RwLock<RepView>,
    clients: Vec<Mutex<Option<ClientRep>>>,
    adapters: Vec<Arc<dyn WireAdapter>>,
    tiers: TierConfig,
    max_payload: usize,
    /// Times each client's encode when set (the Ops inspector).
    stopwatch: std::sync::OnceLock<Arc<dyn Stopwatch>>,
}

impl JobBatch for CellRep {
    fn run(&self, index: usize) {
        let Some(slot) = self.clients.get(index) else {
            return;
        };
        let mut guard = lock(slot);
        let Some(rep) = guard.as_mut() else { return };
        let Some(adapter) = self.adapters.get(rep.adapter) else {
            return;
        };
        let view = self
            .view
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stopwatch = self.stopwatch.get();
        let start = stopwatch.map(|s| s.now_nanos());
        let _ = rep.run(&view, adapter.as_ref(), &self.tiers, self.max_payload);
        if let (Some(s), Some(start)) = (stopwatch, start) {
            rep.encode_nanos = s.now_nanos().saturating_sub(start);
        }
    }
}

/// What the Ops inspector reads of a cell's run time (plan 13): each
/// system's recent durations, the graph evaluation's, the inbox depth of
/// the last tick, and per-client encode times. Diagnostics, not state.
#[derive(Debug)]
pub struct CellTimings<'a> {
    /// The cell's tick.
    pub tick: Tick,
    /// Systems and the engine's own steps: name, phase, recent durations.
    pub systems: Vec<(&'static str, Phase, &'a Timing)>,
    /// Intents drained in the last tick.
    pub inbox_depth: usize,
    /// Recent per-client encode durations.
    pub encode: &'a Timing,
}

/// What one tick did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TickReport {
    /// The tick simulated.
    pub tick: Tick,
    /// The world state hash after it.
    pub state_hash: u64,
    /// Inbound items refused by the allowed-state list or validation.
    pub refused: u32,
    /// How the per-client jobs ran.
    pub jobs: BatchReport,
    /// Snapshot bytes sent this tick.
    pub bytes_out: u64,
}

/// Graph actions, applied through the modules that registered them.
struct ModuleActions<'a> {
    world: &'a mut World,
    registry: &'a Registry,
}

impl ActionHandler for ModuleActions<'_> {
    fn apply(&mut self, call: &ActionCall) -> Result<(), ActionError> {
        self.registry.apply_action(self.world, call)
    }
}

/// The cell's gameplay graph catalog, readable by systems (scripts trigger
/// graphs through it). Content: covered by the content hash.
pub struct Catalog(pub Arc<GraphCatalog>);

/// Explicitly not hashed: content, covered by the content hash.
impl StateHash for Catalog {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for Catalog {
    const NAME: &'static str = "server.graph_catalog";

    /// Content, built with the cell.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

/// The tier client modules run at in this cell (plan 12): simulation
/// state, set by a logged intent, sent to every session with the package's
/// permitted client modules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModPolicy {
    /// The tier.
    pub tier: mantis_adapter_contract::ModTier,
}

impl Default for ModPolicy {
    fn default() -> Self {
        Self {
            tier: mantis_adapter_contract::ModTier::Automation,
        }
    }
}

impl StateHash for ModPolicy {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u8(self.tier as u8);
    }
}

impl Resource for ModPolicy {
    const NAME: &'static str = "server.mod_policy";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        use mantis_adapter_contract::core_types::Wire;
        self.tier.encode(e);
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        use mantis_adapter_contract::core_types::Wire;
        self.tier = mantis_adapter_contract::ModTier::decode(d)?;
        Ok(())
    }
}

/// The permitted-modules message for `mods` (keys longer than the wire
/// allows, or past the 32nd, are not sent).
fn permitted_message(mods: &[String]) -> Outbound {
    let mods: Vec<mantis_adapter_contract::core_types::WireString<32>> = mods
        .iter()
        .filter_map(|m| mantis_adapter_contract::core_types::WireString::new(m))
        .take(32)
        .collect();
    Outbound::PermittedModules(mantis_adapter_contract::PermittedModules {
        tier: mantis_adapter_contract::ModTier::Automation,
        modules: mantis_adapter_contract::core_types::BoundedArray::from_slice(&mods).unwrap_or_default(),
    })
}

/// True when `r` encoded a message to send. A refusal for a value the
/// protocol cannot carry is counted; a message the protocol has no form for
/// at all (`Unsupported`, such as permitted modules on a protocol without
/// client modules) is skipped silently.
fn sent(r: Result<(), AdapterError>, refused: &mut u64) -> bool {
    match r {
        Ok(()) => true,
        Err(AdapterError::Unsupported(_)) => false,
        Err(_) => {
            *refused += 1;
            false
        }
    }
}

/// A boxed log sink.
pub type BoxedSink = Box<dyn LogSink + Send>;

/// Session state that travels into a spawned avatar.
#[derive(Clone, Copy)]
struct Carried {
    character: u64,
    envelope: Option<(EnvelopeState, u32)>,
    input: Option<crate::movement::InputCarry>,
}

/// One cell.
pub struct Cell {
    cfg: CellConfig,
    world: World,
    schedule: Schedule,
    clock: TickClock,
    inbox: Arc<Inbox>,
    rep: Arc<CellRep>,
    batch: Arc<dyn JobBatch>,
    completion: Arc<Completion>,
    rewind: RewindBuffer,
    log: Option<LogWriter<CellLogSchema, BoxedSink>>,
    drain: VecDeque<(SessionId, CellIntent)>,
    ack_drain: VecDeque<(SessionId, Tick)>,
    slots: BTreeMap<SessionId, usize>,
    scratch: Vec<u8>,
    ground: Arc<dyn GroundQuery + Send + Sync>,
    catalog: Arc<GraphCatalog>,
    abilities: BTreeMap<AbilityId, GraphId>,
    q_repl: Query<(Read<ReplicationId>, Read<Body>, Read<Look>)>,
    ghosts: BoundedVec<Replicated>,
    ghost_exports: BoundedVec<Replicated>,
    transfers_out: BoundedVec<Transfer>,
    offered: BoundedVec<ReplicationId>,
    refused: u32,
    modules: Registry,
    commands: Arc<CommandInbox>,
    command_drain: VecDeque<ModuleCommand>,
    expected: VecDeque<ModuleOutcome>,
    /// Replay: recorded system outcomes, compared when systems emit them.
    expected_system: VecDeque<ModuleOutcome>,
    /// Replay: commands delivered whose outcome has not been read yet (an
    /// outcome with none pending was emitted by a system).
    commands_pending: usize,
    /// Outcomes not yet drained, with the tick each was made at.
    outcomes: BoundedVec<(Tick, ModuleOutcome)>,
    feature_news: BoundedVec<SessionId>,
    /// Sessions whose client came back this tick ([`CellIntent::Linked`]),
    /// to tell their gateway from which tick the snapshots are new. Output.
    relinked: BoundedVec<SessionId>,
    feature_broadcast: bool,
    /// Diagnostics for the Ops inspector: the clock (when timed), graph
    /// evaluation and per-client encode times, and the last inbox depth.
    stopwatch: Option<Arc<dyn Stopwatch>>,
    graph_timing: Timing,
    encode_timing: Timing,
    inbox_depth: usize,
    /// Messages an adapter refused to encode (a value its protocol cannot
    /// carry); the client was sent nothing for them.
    encode_refused: u64,
    /// The graph runtime while it evaluates (swapped with the world's, so
    /// module actions get the world): an empty runtime otherwise.
    graph_spare: GraphRuntime,
    /// The permitted-modules message, built once; its tier is set on send.
    permitted: Outbound,
    script_sources: BTreeMap<mantis_adapter_contract::core_types::ContentHash, String>,
}

impl Cell {
    /// Builds a cell over `ground`, serving clients through `adapters`, with
    /// an optional log. `catalog` and `abilities` map `Cast` intents to
    /// gameplay graphs.
    ///
    /// # Errors
    /// [`CellError`] for invalid configuration or setup failures.
    pub fn new(
        cfg: CellConfig,
        ground: Arc<dyn GroundQuery + Send + Sync>,
        adapters: Vec<Arc<dyn WireAdapter>>,
        log: Option<LogWriter<CellLogSchema, BoxedSink>>,
        catalog: Arc<GraphCatalog>,
        abilities: BTreeMap<AbilityId, GraphId>,
    ) -> Result<Self, CellError> {
        let motion = Motion::new(cfg.motion).map_err(CellError::Config)?;
        let mut world = World::new();
        world.register::<ReplicationId>()?;
        world.register::<Body>()?;
        world.register::<Mods>()?;
        world.register::<Look>()?;
        world.register::<Controlled>()?;
        world
            .components
            .reserve::<(ReplicationId, Body, Mods, Look, Controlled)>(cfg.max_entities)?;
        world
            .components
            .reserve::<(ReplicationId, Body, Mods, Look)>(cfg.max_entities)?;
        world.insert_resource(Sessions::new(cfg.max_clients * 4))?;
        world.insert_resource(ReplMap::default())?;
        world.insert_resource(ModPolicy::default())?;
        world.insert_resource(GraphRuntime::with_capacity(cfg.max_entities, 256))?;
        world.insert_resource(Catalog(Arc::clone(&catalog)))?;

        let mut schedule = Schedule::new();
        let access = Access::builder(&mut world)
            .write::<Body>()
            .read::<Mods>()
            .write_resource::<Sessions>()
            .build()?;
        let g = Arc::clone(&ground);
        let envelope = cfg.envelope;
        let inputs = cfg.inputs;
        schedule.add(
            SystemDesc {
                name: "server.movement",
                phase: Phase::Movement,
                priority: 0,
                access,
            },
            move |w: &mut World, c: &TickContext| {
                movement_system(w, c, &motion, &envelope, inputs, g.as_ref())
            },
        )?;
        let q_repl = world.query()?;
        let rep = Arc::new(CellRep {
            view: RwLock::new(RepView::with_capacity(cfg.max_entities * 2, 256, cfg.tiers.near)),
            clients: (0..cfg.max_clients).map(|_| Mutex::new(None)).collect(),
            adapters,
            tiers: cfg.tiers,
            max_payload: cfg.max_payload,
            stopwatch: std::sync::OnceLock::new(),
        });
        let batch: Arc<dyn JobBatch> = rep.clone();
        Ok(Self {
            clock: TickClock::new(cfg.rate, cfg.start),
            inbox: Arc::new(Inbox::new(cfg.inbox_capacity)),
            rewind: RewindBuffer::with_capacity(cfg.rewind_ticks, cfg.max_entities * 2),
            drain: VecDeque::with_capacity(cfg.inbox_capacity),
            ack_drain: VecDeque::with_capacity(cfg.inbox_capacity),
            slots: BTreeMap::new(),
            scratch: Vec::with_capacity(256),
            ghosts: BoundedVec::with_capacity(cfg.max_entities),
            ghost_exports: BoundedVec::with_capacity(cfg.max_entities),
            transfers_out: BoundedVec::with_capacity(64),
            offered: BoundedVec::with_capacity(64),
            completion: Completion::new(),
            refused: 0,
            modules: Registry::default(),
            commands: Arc::new(CommandInbox::new(cfg.inbox_capacity)),
            command_drain: VecDeque::with_capacity(cfg.inbox_capacity),
            expected: VecDeque::new(),
            expected_system: VecDeque::new(),
            commands_pending: 0,
            outcomes: BoundedVec::with_capacity(cfg.inbox_capacity),
            feature_news: BoundedVec::with_capacity(cfg.max_clients),
            relinked: BoundedVec::with_capacity(cfg.max_clients),
            feature_broadcast: false,
            stopwatch: None,
            graph_timing: Timing::default(),
            encode_timing: Timing::default(),
            inbox_depth: 0,
            encode_refused: 0,
            graph_spare: GraphRuntime::with_capacity(0, 0),
            permitted: permitted_message(&cfg.client_mods),
            script_sources: BTreeMap::new(),
            ground,
            catalog,
            abilities,
            q_repl,
            rep,
            batch,
            schedule,
            world,
            log,
            cfg,
        })
    }

    /// Times every system, graph evaluation, and per-client encode from now
    /// on with `stopwatch` (the Ops inspector; diagnostics only).
    pub fn set_stopwatch(&mut self, stopwatch: Arc<dyn Stopwatch>) {
        self.schedule.set_stopwatch(Arc::clone(&stopwatch));
        let _ = self.rep.stopwatch.set(Arc::clone(&stopwatch));
        self.stopwatch = Some(stopwatch);
    }

    /// The cell's recent run times, for the Ops inspector. Allocates the
    /// list: an Ops call, not a tick step.
    #[must_use]
    pub fn timings(&self) -> CellTimings<'_> {
        let mut systems: Vec<(&'static str, Phase, &Timing)> =
            vec![("server.graphs", Phase::Effects, &self.graph_timing)];
        systems.extend(self.schedule.timings());
        CellTimings {
            tick: self.clock.tick(),
            systems,
            inbox_depth: self.inbox_depth,
            encode: &self.encode_timing,
        }
    }

    /// Messages an adapter refused to encode since the cell started: a value
    /// its protocol cannot carry. Nothing was sent for them (never a
    /// stand-in value).
    #[must_use]
    pub fn encode_refusals(&self) -> u64 {
        self.encode_refused
    }

    /// Every registered component name (the inspector's component list).
    #[must_use]
    pub fn component_names(&self) -> Vec<&'static str> {
        self.world.components.component_names()
    }

    /// A page of the entities that have component `name`, every value as
    /// text (see [`mantis_core::ecs::Components::inspect`]).
    #[must_use]
    pub fn inspect_entities(
        &self,
        name: &str,
        offset: usize,
        limit: usize,
    ) -> Option<mantis_core::ecs::InspectPage> {
        self.world.components.inspect(name, offset, limit)
    }

    /// Installs the package's modules (before the first tick, identically on
    /// every replay).
    ///
    /// # Errors
    /// The first [`RegistryError`]; the host refuses to start.
    pub fn install_modules(&mut self, set: &ModuleSet) -> Result<(), RegistryError> {
        self.modules = set.install(
            &mut self.world,
            &mut self.schedule,
            self.cfg.rate.hz(),
            self.cfg.seed,
            &self.catalog,
        )?;
        Ok(())
    }

    /// Schedules a hot reload of server script `name` (development tooling
    /// only; production script changes arrive as new cooked content). The
    /// reload is a logged intent naming the source by hash; it takes effect
    /// in the tick that delivers it, at the tick boundary inside the VM,
    /// keeping the script's state. False when the cell runs no scripts or
    /// the name is too long.
    pub fn reload_script(&mut self, name: &str, source: &str) -> bool {
        if self.world.resource::<crate::scripting::Scripts>().is_none() {
            return false;
        }
        let Some(name) = mantis_adapter_contract::core_types::WireString::new(name) else {
            return false;
        };
        let hash = self.supply_script_source(source);
        self.inbox
            .push(SessionId(0), CellIntent::ScriptReload { name, source: hash })
    }

    /// Makes `source` available to `ScriptReload` intents by its hash (the
    /// replayer supplies every reloaded source this way before replaying a
    /// log that contains reloads). Returns the hash.
    pub fn supply_script_source(&mut self, source: &str) -> mantis_adapter_contract::core_types::ContentHash {
        let hash = mantis_adapter_contract::core_types::ContentHash::of(source.as_bytes());
        self.script_sources.insert(hash, source.to_owned());
        hash
    }

    /// How an extension kind is handled here, if any module registered it.
    #[must_use]
    pub fn extension_route(&self, kind: ExtensionKind) -> Option<Route> {
        self.modules.route(kind)
    }

    /// Where economy commands for this cell go.
    #[must_use]
    pub fn commands(&self) -> Arc<CommandInbox> {
        Arc::clone(&self.commands)
    }

    /// This tick's messages for service roles (drain after the tick; the
    /// cell forgets them when the next tick starts).
    pub fn service_messages(&self, mut each: impl FnMut(u16, &crate::modules::Payload)) {
        if let Some(o) = self.world.resources.get::<crate::service::ServiceOutbox>() {
            for (topic, payload) in o.pending() {
                each(*topic, payload);
            }
        }
    }

    /// Outcomes of executed commands since the last drain (for the
    /// persistence writer), each with the tick it was made at: an outcome
    /// a replay made (recovery) keeps its original tick, so the writer's
    /// per-tick batch numbering knows it as already durable.
    pub fn drain_outcomes(&mut self, mut each: impl FnMut(Tick, &ModuleOutcome)) {
        for (tick, o) in self.outcomes.iter() {
            each(*tick, o);
        }
        self.outcomes.clear();
    }

    /// Module metrics (Ops).
    #[must_use]
    pub fn metrics(&self) -> Option<&crate::modules::Metrics> {
        self.world.resource::<crate::modules::Metrics>()
    }

    /// The cell's id.
    #[must_use]
    pub fn id(&self) -> CellId {
        self.cfg.id
    }

    /// The cell's ground model.
    #[must_use]
    pub fn ground(&self) -> &(dyn GroundQuery + Send + Sync) {
        self.ground.as_ref()
    }

    /// The cell's tick rate.
    #[must_use]
    pub fn rate(&self) -> TickRate {
        self.cfg.rate
    }

    /// Where network threads deliver work for this cell.
    #[must_use]
    pub fn inbox(&self) -> Arc<Inbox> {
        Arc::clone(&self.inbox)
    }

    /// The current tick.
    #[must_use]
    pub fn tick_now(&self) -> Tick {
        self.clock.tick()
    }

    /// Writes a snapshot of this cell at the end of its last tick (decision
    /// 0007): call between ticks.
    ///
    /// # Errors
    /// The state that cannot be snapshotted, by name.
    pub fn snapshot(
        &self,
        build: mantis_core::log::BuildId,
        content: mantis_adapter_contract::core_types::ContentHash,
    ) -> Result<Vec<u8>, crate::snapshot::SnapshotError> {
        use crate::snapshot::{SnapshotError, SnapshotHeader, replicated};
        use mantis_adapter_contract::core_types::Wire;
        let mut out = Vec::with_capacity(64 * 1024);
        let mut e = mantis_adapter_contract::core_types::Encoder::new(&mut out);
        SnapshotHeader {
            build,
            content,
            cell: self.cfg.id,
            tick: self.clock.tick(),
            state_hash: self.world.state_hash(),
        }
        .write(&mut e);
        // Script sources first: a restore needs them before the world.
        e.u32(u32::try_from(self.script_sources.len()).unwrap_or(u32::MAX));
        for (hash, source) in &self.script_sources {
            hash.encode(&mut e);
            e.u32(u32::try_from(source.len()).unwrap_or(u32::MAX));
            e.bytes(source.as_bytes());
        }
        self.world
            .save(&mut e)
            .map_err(|name| SnapshotError(format!("`{name}` cannot be snapshotted")))?;
        e.u32(u32::try_from(self.offered.len()).unwrap_or(u32::MAX));
        for r in self.offered.iter() {
            e.u64(r.0.to_bits());
        }
        e.u32(u32::try_from(self.ghosts.len()).unwrap_or(u32::MAX));
        for g in self.ghosts.iter() {
            replicated(&mut e, g);
        }
        self.rewind.save(&mut e);
        Ok(out)
    }

    /// Restores a snapshot into this cell, which must be freshly built the
    /// same way (configuration, adapters, modules) and not yet ticked. The
    /// cell resumes after the snapshot's tick with the same state hash; its
    /// sessions have no clients ([`Cell::detached_sessions`]).
    ///
    /// # Errors
    /// Why the snapshot does not fit this cell.
    pub fn restore(
        &mut self,
        bytes: &[u8],
    ) -> Result<crate::snapshot::SnapshotHeader, crate::snapshot::SnapshotError> {
        use crate::snapshot::{SnapshotError, SnapshotHeader, replicated_of};
        use mantis_adapter_contract::core_types::Wire;
        let mut d = mantis_adapter_contract::core_types::Decoder::new(bytes);
        let header = SnapshotHeader::read(&mut d)?;
        if header.cell != self.cfg.id {
            return Err(SnapshotError(format!(
                "snapshot of cell {} restored into cell {}",
                header.cell.0, self.cfg.id.0
            )));
        }
        if !self.slots.is_empty() {
            return Err(SnapshotError("restore into a cell with clients".to_owned()));
        }
        self.script_sources.clear();
        for _ in 0..d.u32()? {
            let hash = mantis_adapter_contract::core_types::ContentHash::decode(&mut d)?;
            let n = d.u32()? as usize;
            let source = core::str::from_utf8(d.take(n)?)
                .map_err(|_| SnapshotError("script source is not UTF-8".to_owned()))?
                .to_owned();
            if let Some(scripts) = self.world.resource_mut::<crate::scripting::Scripts>() {
                scripts.know(&source);
            }
            self.script_sources.insert(hash, source);
        }
        self.world.load(&mut d).map_err(SnapshotError)?;
        self.offered.clear();
        for _ in 0..d.u32()? {
            let r = ReplicationId(mantis_core::ecs::EntityId::from_bits(d.u64()?));
            self.offered
                .push(r)
                .map_err(|_| SnapshotError("offered transfers over capacity".to_owned()))?;
        }
        self.ghosts.clear();
        for _ in 0..d.u32()? {
            let g = replicated_of(&mut d)?;
            self.ghosts
                .push(g)
                .map_err(|_| SnapshotError("ghosts over capacity".to_owned()))?;
        }
        self.rewind.load(&mut d)?;
        d.finish()?;
        self.clock = mantis_core::time::TickClock::new(self.cfg.rate, header.tick);
        self.world.components.set_change_tick(header.tick);
        let actual = self.world.state_hash();
        if actual != header.state_hash {
            return Err(SnapshotError(format!(
                "restored state hash {actual:#018x} differs from the snapshot's {:#018x}",
                header.state_hash
            )));
        }
        Ok(header)
    }

    /// Starts writing the log to `log` (after a recovery replayed the old
    /// one: its header names the next tick to simulate).
    pub fn set_log(&mut self, log: Option<LogWriter<CellLogSchema, BoxedSink>>) {
        self.log = log;
    }

    /// Sessions in the world without a client (after a restore): the host
    /// ends them with `Leave`, or a reconnecting client takes one over.
    #[must_use]
    pub fn detached_sessions(&self) -> Vec<SessionId> {
        self.world
            .resource::<Sessions>()
            .map(|s| {
                s.map
                    .keys()
                    .filter(|id| !self.slots.contains_key(id))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Characters in this cell with their lease epochs (a zone rebuilding
    /// its lease table after a restore).
    #[must_use]
    pub fn hosted(&self) -> Vec<(SessionId, u64, u64)> {
        self.world
            .resource::<Sessions>()
            .map(|s| s.map.values().map(|x| (x.id, x.character, x.epoch)).collect())
            .unwrap_or_default()
    }

    /// The world (read-only).
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The cell-side state of a session.
    #[must_use]
    pub fn session(&self, id: SessionId) -> Option<&CellSession> {
        self.world.resource::<Sessions>().and_then(|s| s.map.get(&id))
    }

    /// The local entity behind a replication id.
    #[must_use]
    pub fn local(&self, repl: ReplicationId) -> Option<EntityId> {
        self.world
            .resource::<ReplMap>()
            .and_then(|m| m.0.get(&repl).copied())
    }

    /// Attaches a client's replication state for `session`.
    ///
    /// # Errors
    /// [`CellError::ClientsFull`] when every slot is taken.
    pub fn attach_client(
        &mut self,
        session: SessionId,
        conn: ConnectionId,
        adapter: usize,
        implicit_ack: bool,
    ) -> Result<usize, CellError> {
        let slot = self
            .rep
            .clients
            .iter()
            .position(|c| lock(c).is_none())
            .ok_or(CellError::ClientsFull)?;
        if let Some(c) = self.rep.clients.get(slot) {
            let mut caps = self.cfg.caps;
            if self.cfg.tiers.snapshot_own_bases && caps.own_base_history == 0 {
                // Twice the known entities: few collisions.
                caps.own_base_history = caps.known.saturating_mul(2);
            }
            *lock(c) = Some(ClientRep::new(session, conn, adapter, implicit_ack, caps));
        }
        self.slots.insert(session, slot);
        Ok(slot)
    }

    /// Detaches a client's replication state.
    pub fn detach_client(&mut self, session: SessionId) {
        if let Some(slot) = self.slots.remove(&session)
            && let Some(c) = self.rep.clients.get(slot)
        {
            *lock(c) = None;
        }
    }

    /// Removes a client's replication state, to move it with its avatar to
    /// another cell (baselines and known entities stay valid: ticks are
    /// realm-wide).
    pub fn take_client(&mut self, session: SessionId) -> Option<ClientRep> {
        let slot = self.slots.remove(&session)?;
        self.rep.clients.get(slot).and_then(|c| lock(c).take())
    }

    /// Installs a client's replication state moved from another cell.
    ///
    /// # Errors
    /// [`CellError::ClientsFull`] when every slot is taken.
    pub fn put_client(&mut self, rep: ClientRep) -> Result<usize, CellError> {
        let slot = self
            .rep
            .clients
            .iter()
            .position(|c| lock(c).is_none())
            .ok_or(CellError::ClientsFull)?;
        let session = rep.session;
        if let Some(c) = self.rep.clients.get(slot) {
            *lock(c) = Some(rep);
        }
        self.slots.insert(session, slot);
        Ok(slot)
    }

    /// Runs `f` on a client's replication state.
    pub fn with_client<R>(&self, session: SessionId, f: impl FnOnce(&ClientRep) -> R) -> Option<R> {
        let slot = self.slots.get(&session)?;
        let c = self.rep.clients.get(*slot)?;
        lock(c).as_ref().map(f)
    }

    /// Replaces the ghosts imported from neighbours (replication only).
    pub fn set_ghosts(&mut self, ghosts: impl Iterator<Item = Replicated>) {
        self.ghosts.clear();
        for g in ghosts {
            if self.ghosts.push(g).is_err() {
                break;
            }
        }
    }

    /// Entities near this cell's borders, for neighbours to ghost.
    #[must_use]
    pub fn ghost_exports(&self) -> &[Replicated] {
        &self.ghost_exports
    }

    /// Entities that crossed out of this cell's region this tick.
    #[must_use]
    pub fn transfers_out(&self) -> &[Transfer] {
        &self.transfers_out
    }

    fn dispatch(&mut self, session: SessionId, intent: &CellIntent, tick: Tick) -> Result<(), &'static str> {
        let current = self.session(session);
        allowed(current, intent).map_err(|_| "not allowed in this session state")?;
        match *intent {
            CellIntent::Join {
                repl,
                spawn,
                yaw,
                look,
                mode,
                epoch,
                character,
            } => {
                let body = MotionState::at_rest(spawn, yaw);
                self.spawn_avatar(
                    session,
                    repl,
                    body,
                    MotionModifiers::NONE,
                    look,
                    mode,
                    epoch,
                    None,
                    Carried {
                        character,
                        envelope: None,
                        input: None,
                    },
                )
            }
            CellIntent::TransferIn(t) => {
                let sid = t.session.map(SessionId);
                if let Some(sid) = sid {
                    self.spawn_avatar(
                        sid,
                        t.repl,
                        t.body,
                        t.mods,
                        t.look,
                        t.mode,
                        t.epoch,
                        t.last_seq,
                        Carried {
                            character: t.character,
                            envelope: Some((t.envelope, t.cheats)),
                            input: Some(t.input),
                        },
                    )
                } else {
                    let e = self
                        .world
                        .spawn((t.repl, Body(t.body), Mods(t.mods), Look(t.look)))
                        .map_err(|_| "spawn")?;
                    self.map_mut()?.0.insert(t.repl, e);
                    Ok(())
                }
            }
            CellIntent::TransferAck(repl) => self.transfer_ack(repl),
            CellIntent::Leave => self.leave_session(session),
            CellIntent::Move(input) => {
                let cfg = self.cfg.inputs;
                let s = self.sessions_mut()?.map.get_mut(&session).ok_or("no session")?;
                let _ = buffer_input(s, input, cfg);
                Ok(())
            }
            CellIntent::MoveClaim {
                position,
                client_time_ms,
            } => self.claim(session, position, client_time_ms),
            CellIntent::Cast(c) => self.cast(session, c, tick),
            CellIntent::Interact(i) => self.interact(session, i, tick),
            CellIntent::Choose(_) => Ok(()),
            CellIntent::Extension {
                kind,
                request,
                payload,
            } => self.extension(session, (kind, request), payload.as_slice(), tick),
            CellIntent::SetModule { module, enabled } => self.set_module(module, enabled),
            CellIntent::SetLive { name, kind, value } => self.set_live(name.as_str(), kind, value),
            CellIntent::ServiceUpdate { topic, payload } => self.service_update(topic, &payload, tick),
            CellIntent::Relocate { character, position } => self.relocate(character, position, tick),
            CellIntent::SetModTier { tier } => {
                let policy = self.world.resource_mut::<ModPolicy>().ok_or("mod policy")?;
                // The same tier again (a second placement into the same
                // instance): nobody is told twice.
                if policy.tier != tier {
                    policy.tier = tier;
                    self.feature_broadcast = true;
                }
                Ok(())
            }
            CellIntent::ClockSlip { ticks } => self.clock_slip(ticks),
            CellIntent::Throttled { count } => {
                let s = self.sessions_mut()?.map.get_mut(&session).ok_or("no session")?;
                s.cheats = s.cheats.saturating_add(count);
                Ok(())
            }
            CellIntent::Linked { up } => self.linked(session, up),
            CellIntent::ScriptReload { name, source } => self.script_reload(name.as_str(), source),
        }
    }

    /// Queues a Validated claim for this tick's movement. More claims than
    /// the window in one tick (a host stall delivers them together): the
    /// oldest pending one makes way. The envelope judges the next from the
    /// last accepted position over the longer client time, so dropping one
    /// never lets a mover go further.
    fn claim(&mut self, session: SessionId, position: Vec3, client_time_ms: u32) -> Result<(), &'static str> {
        let s = self.sessions_mut()?.map.get_mut(&session).ok_or("no session")?;
        if s.claims.len() >= s.claims.capacity() {
            s.claims.remove(0);
        }
        s.claims
            .push((position, client_time_ms))
            .map_err(|_| "too many claims")
    }

    /// A gateway's word on `session`'s client ([`CellIntent::Linked`]).
    fn linked(&mut self, session: SessionId, up: bool) -> Result<(), &'static str> {
        let s = self.sessions_mut()?.map.get_mut(&session).ok_or("no session")?;
        if s.mode == mantis_adapter_contract::MovementMode::Predictive {
            s.inputs.clear();
            s.synth_mask = 0;
            if up {
                // The next input starts the stream, as for a new session.
                s.last_seq = None;
            } else {
                // Nothing held: the avatar comes to rest.
                s.last_input.buttons = mantis_core::kinematics::MoveButtons::default();
            }
        }
        // A client back over a new connection holds no baseline and knows no
        // entity: its next snapshot is whole, it is told the modules again,
        // and its gateway learns from which tick snapshots are new.
        if up
            && let Some(slot) = self.slots.get(&session)
            && let Some(c) = self.rep.clients.get(*slot)
            && let Some(rep) = lock(c).as_mut()
        {
            rep.forget_client_view();
            let _ = self.relinked.push(session);
            let _ = self.feature_news.push(session);
        }
        Ok(())
    }

    /// The cell's clock slipped `ticks` behind wall time: Validated clock
    /// baselines move with it ([`CellIntent::ClockSlip`]).
    fn clock_slip(&mut self, ticks: u32) -> Result<(), &'static str> {
        let slip = tick_ms(Tick(u64::from(ticks)), self.cfg.rate.hz());
        for s in self.sessions_mut()?.map.values_mut() {
            // Offsets are client time minus tick time: after a slip every
            // honest client reads `slip` further ahead.
            if let Some(m) = s.envelope.min_offset_ms.as_mut() {
                *m = m.saturating_add(slip);
            }
        }
        Ok(())
    }

    fn script_reload(
        &mut self,
        name: &str,
        source: mantis_adapter_contract::core_types::ContentHash,
    ) -> Result<(), &'static str> {
        let text = self
            .script_sources
            .get(&source)
            .cloned()
            .ok_or("script source not supplied")?;
        let scripts = self
            .world
            .resource_mut::<crate::scripting::Scripts>()
            .ok_or("no scripts")?;
        scripts.reload(name, &text);
        Ok(())
    }

    fn extension(
        &mut self,
        session: SessionId,
        (kind, request): (ExtensionKind, u32),
        payload: &[u8],
        tick: Tick,
    ) -> Result<(), &'static str> {
        let ctx = TickContext {
            tick,
            rate: self.cfg.rate,
            seed: self.cfg.seed,
        };
        match self.modules.handle(&mut self.world, &ctx, session, kind, payload) {
            Ok(()) => Ok(()),
            Err(reason) => {
                self.refuse_extension(session, kind, request, reason);
                // A disabled feature is not the client's fault.
                if reason == ExtensionRefusal::FeatureDisabled {
                    Ok(())
                } else {
                    Err("extension refused")
                }
            }
        }
    }

    fn transfer_ack(&mut self, repl: ReplicationId) -> Result<(), &'static str> {
        let e = self.map_mut()?.0.remove(&repl).ok_or("unknown entity")?;
        let owner = self.world.get::<Controlled>(e).map(|c| c.0);
        self.world.despawn(e).map_err(|_| "despawn")?;
        if let Some(owner) = owner {
            self.sessions_mut()?.map.remove(&owner);
            self.detach_client(owner);
        }
        self.offered.retain(|r| *r != repl);
        Ok(())
    }

    fn leave_session(&mut self, session: SessionId) -> Result<(), &'static str> {
        let s = self.sessions_mut()?.map.remove(&session).ok_or("no session")?;
        if let Some(e) = s.avatar {
            self.map_mut()?.0.remove(&s.repl);
            let _ = self.world.despawn(e);
        }
        Ok(())
    }

    /// Moves `character`'s avatar and corrects its client; the zone
    /// transfers it if `position` is another cell's.
    fn relocate(&mut self, character: u64, position: Vec3, tick: Tick) -> Result<(), &'static str> {
        let now = tick_ms(tick, self.cfg.rate.hz());
        let World {
            components,
            resources,
        } = &mut self.world;
        let sessions = resources.get_mut::<Sessions>().ok_or("sessions resource")?;
        let Sessions { map, corrections, .. } = sessions;
        let s = map
            .values_mut()
            .find(|s| s.character == character)
            .ok_or("no such character here")?;
        let avatar = s.avatar.ok_or("no avatar")?;
        let mut body = components.get_mut::<Body>(avatar).ok_or("no body")?;
        body.0 = MotionState::at_rest(position, body.0.yaw);
        s.envelope = EnvelopeState::new(position, now);
        s.inputs.clear();
        s.claims.clear();
        corrections
            .push((
                s.id,
                mantis_adapter_contract::SetPosition {
                    entity: s.repl.0,
                    position,
                    yaw: body.0.yaw,
                    tick,
                },
            ))
            .map_err(|_| "too many corrections")
    }

    fn service_update(
        &mut self,
        topic: u16,
        payload: &crate::modules::Payload,
        tick: Tick,
    ) -> Result<(), &'static str> {
        let ctx = TickContext {
            tick,
            rate: self.cfg.rate,
            seed: self.cfg.seed,
        };
        self.modules
            .service(&mut self.world, &ctx, topic, payload.as_slice())
    }

    fn set_live(&mut self, name: &str, kind: u8, value: f32) -> Result<(), &'static str> {
        let applied = crate::modules::set_live(&mut self.world, &mut self.schedule, name, kind, value)?;
        if applied == crate::modules::LiveApplied::Module {
            self.feature_broadcast = true;
        }
        Ok(())
    }

    fn set_module(&mut self, module: u16, enabled: bool) -> Result<(), &'static str> {
        let key = self
            .world
            .resource::<ModuleStates>()
            .and_then(|m| m.keys.get(usize::from(module)).cloned())
            .ok_or("unknown module")?;
        if crate::modules::set_enabled(&mut self.world, &mut self.schedule, &key, enabled) {
            self.feature_broadcast = true;
            Ok(())
        } else {
            Err("unknown module")
        }
    }

    /// Tells the gateway of each session whose client came back this tick
    /// that its snapshots are new from this tick on. Allocation-free.
    fn send_relinked(&mut self, sink: &mut dyn OutboundSink) {
        let msg = Outbound::Relinked(mantis_adapter_contract::Relinked {
            tick: self.clock.tick(),
        });
        for session in self.relinked.iter() {
            let Some(client) = self
                .slots
                .get(session)
                .and_then(|slot| self.rep.clients.get(*slot))
            else {
                continue;
            };
            let (adapter, conn) = match lock(client).as_ref() {
                Some(c) => (c.adapter, c.conn),
                None => continue,
            };
            let Some(a) = self.rep.adapters.get(adapter) else {
                continue;
            };
            self.scratch.clear();
            if sent(
                a.encode_outbound(&msg, &mut self.scratch),
                &mut self.encode_refused,
            ) {
                sink.send(adapter, conn, Channel::Reliable, &self.scratch);
            }
        }
        self.relinked.clear();
    }

    /// Tells newly joined sessions (or everyone, after a switch) which
    /// modules are enabled. Allocation-free.
    fn send_feature_states(&mut self, sink: &mut dyn OutboundSink) {
        let all = std::mem::take(&mut self.feature_broadcast);
        if !all && self.feature_news.is_empty() {
            return;
        }
        let Some(states) = self.world.resources.get::<ModuleStates>() else {
            self.feature_news.clear();
            return;
        };
        let tier = self
            .world
            .resources
            .get::<ModPolicy>()
            .map_or(mantis_adapter_contract::ModTier::Automation, |p| p.tier);
        if let Outbound::PermittedModules(p) = &mut self.permitted {
            p.tier = tier;
        }
        let permitted = &self.permitted;
        for (session, slot) in &self.slots {
            if !all && !self.feature_news.contains(session) {
                continue;
            }
            let Some(client) = self.rep.clients.get(*slot) else {
                continue;
            };
            let (adapter, conn) = match lock(client).as_ref() {
                Some(c) => (c.adapter, c.conn),
                None => continue,
            };
            let Some(a) = self.rep.adapters.get(adapter) else {
                continue;
            };
            self.scratch.clear();
            if sent(
                a.encode_outbound(permitted, &mut self.scratch),
                &mut self.encode_refused,
            ) {
                sink.send(adapter, conn, Channel::Reliable, &self.scratch);
            }
            for (key, enabled) in states.keys.iter().zip(&states.enabled) {
                let Some(module) = mantis_adapter_contract::core_types::WireString::new(key) else {
                    continue;
                };
                let msg = Outbound::FeatureState(mantis_adapter_contract::FeatureState {
                    module,
                    enabled: *enabled,
                });
                self.scratch.clear();
                if sent(
                    a.encode_outbound(&msg, &mut self.scratch),
                    &mut self.encode_refused,
                ) {
                    sink.send(adapter, conn, Channel::Reliable, &self.scratch);
                }
            }
        }
        self.feature_news.clear();
    }

    fn refuse_extension(
        &mut self,
        session: SessionId,
        kind: ExtensionKind,
        request: u32,
        reason: ExtensionRefusal,
    ) {
        if let Some(s) = self.world.resource_mut::<Sessions>() {
            let _ = s.refusals.push((
                session,
                ExtensionRefused {
                    kind,
                    request,
                    reason,
                },
            ));
        }
    }

    /// Logs, executes, and logs the outcome of every queued command.
    /// Logs this tick's system outcomes (lead ruling, M4) and, on replay,
    /// checks each against the recorded one.
    fn log_system_outcomes(&mut self, tick: Tick) -> Result<(), CellError> {
        let Some(sink) = self.world.resources.get_mut::<crate::modules::SystemOutcomes>() else {
            return Ok(());
        };
        for outcome in sink.pending() {
            if let Some(log) = self.log.as_mut() {
                log.append_outcome(tick, outcome)?;
            }
            if let Some(expected) = self.expected_system.pop_front()
                && expected != *outcome
            {
                return Err(CellError::Config("replayed system outcome differs"));
            }
            let _ = self.outcomes.push((tick, *outcome));
        }
        sink.clear();
        Ok(())
    }

    fn run_commands(&mut self, ctx: &TickContext) -> Result<(), CellError> {
        std::mem::swap(&mut *lock(&self.commands.items), &mut self.command_drain);
        while let Some(command) = self.command_drain.pop_front() {
            if let Some(log) = self.log.as_mut() {
                log.append_command(ctx.tick, &command)?;
            }
            let outcome = self.modules.execute(&mut self.world, ctx, &command);
            if let Some(log) = self.log.as_mut() {
                log.append_outcome(ctx.tick, &outcome)?;
            }
            // On replay, the recorded outcome must match.
            if let Some(expected) = self.expected.pop_front()
                && expected != outcome
            {
                return Err(CellError::Config("replayed outcome differs"));
            }
            if let (Err(reason), Some(session)) = (outcome.result, outcome.session) {
                self.refuse_extension(session, outcome.kind, command.request, reason);
            }
            let _ = self.outcomes.push((ctx.tick, outcome));
        }
        Ok(())
    }

    fn cast(
        &mut self,
        session: SessionId,
        c: mantis_adapter_contract::Cast,
        tick: Tick,
    ) -> Result<(), &'static str> {
        self.check_view(c.view_tick, c.view_frac, tick)?;
        let caster = self.session(session).and_then(|s| s.avatar).ok_or("no avatar")?;
        let target = match c.target {
            Some(t) => Some(self.local(ReplicationId(t)).ok_or("unknown target")?),
            None => None,
        };
        let graph = *self.abilities.get(&c.ability).ok_or("unknown ability")?;
        let catalog = Arc::clone(&self.catalog);
        let rt = self.world.resource_mut::<GraphRuntime>().ok_or("graph runtime")?;
        rt.start(&catalog, graph, caster, target, tick)
            .map(|_| ())
            .map_err(|_| "graph refused")
    }

    fn interact(
        &mut self,
        session: SessionId,
        i: mantis_adapter_contract::Interact,
        tick: Tick,
    ) -> Result<(), &'static str> {
        self.check_view(i.view_tick, i.view_frac, tick)?;
        let me = self.session(session).map(|s| s.repl).ok_or("no session")?;
        let here = self
            .rewind
            .position_at(me, i.view_tick, i.view_frac)
            .map_err(|_| "no rewind")?;
        let there = self
            .rewind
            .position_at(ReplicationId(i.entity), i.view_tick, i.view_frac)
            .map_err(|_| "unknown target")?;
        if (there - here).length() > 5.0 {
            return Err("out of reach");
        }
        Ok(())
    }

    fn check_view(&self, view: Tick, frac: u16, now: Tick) -> Result<(), &'static str> {
        if view > now || (view == now && frac > 0) {
            return Err("view time in the future");
        }
        match self.rewind.oldest() {
            Some(o) if view >= o => Ok(()),
            _ => Err("view time older than the rewind buffer"),
        }
    }

    #[expect(clippy::too_many_arguments)] // one spawn record
    fn spawn_avatar(
        &mut self,
        session: SessionId,
        repl: ReplicationId,
        body: MotionState,
        mods: MotionModifiers,
        look: AppearanceId,
        movement: mantis_adapter_contract::MovementMode,
        epoch: u64,
        last_seq: Option<InputSeq>,
        carried: Carried,
    ) -> Result<(), &'static str> {
        let e = self
            .world
            .spawn((repl, Body(body), Mods(mods), Look(look), Controlled(session)))
            .map_err(|_| "spawn")?;
        self.map_mut()?.0.insert(repl, e);
        let now_ms = tick_ms(self.clock.tick(), self.cfg.rate.hz());
        let mut s = CellSession::new(session, movement, repl, epoch, body.position, now_ms);
        s.avatar = Some(e);
        s.last_seq = last_seq;
        s.character = carried.character;
        let _ = self.feature_news.push(session);
        if let Some((envelope, cheats)) = carried.envelope {
            s.envelope = envelope;
            s.cheats = cheats;
        }
        if let Some(input) = carried.input {
            input.apply(&mut s);
        }
        self.sessions_mut()?.map.insert(session, s);
        Ok(())
    }

    fn sessions_mut(&mut self) -> Result<&mut Sessions, &'static str> {
        self.world.resource_mut::<Sessions>().ok_or("sessions resource")
    }

    fn map_mut(&mut self) -> Result<&mut ReplMap, &'static str> {
        self.world.resource_mut::<ReplMap>().ok_or("replication map")
    }

    /// Runs one tick. `workers` runs the per-client jobs (inline when `None`).
    ///
    /// # Errors
    /// [`CellError`] when a system or the log fails; the cell should stop.
    pub fn tick(
        &mut self,
        sink: &mut dyn OutboundSink,
        workers: Option<&WorkerSet>,
    ) -> Result<TickReport, CellError> {
        let tick = self.clock.advance();
        self.world.components.set_change_tick(tick);
        let ctx = TickContext {
            tick,
            rate: self.cfg.rate,
            seed: self.cfg.seed,
        };
        // 1. Inbound: event queues, commands, then intents. Last tick's
        // service messages were drained by the host; forget them.
        if let Some(o) = self.world.resources.get_mut::<crate::service::ServiceOutbox>() {
            o.clear();
        }
        self.modules.advance_events(&mut self.world);
        self.run_commands(&ctx)?;
        std::mem::swap(&mut *lock(&self.inbox.items), &mut self.drain);
        self.inbox_depth = self.drain.len();
        while let Some((session, intent)) = self.drain.pop_front() {
            if let Some(log) = self.log.as_mut() {
                log.append_intent(tick, session, &intent)?;
            }
            if self.dispatch(session, &intent, tick).is_err() {
                self.refused += 1;
                if let Some(s) = self
                    .world
                    .resource_mut::<Sessions>()
                    .and_then(|s| s.map.get_mut(&session))
                {
                    s.cheats += 1;
                }
            }
        }
        self.schedule
            .run_phase(Phase::Inbound, &mut self.world, &ctx)
            .map_err(CellError::Run)?;
        // 2. Timers through Scripts.
        for phase in [
            Phase::Timers,
            Phase::Movement,
            Phase::Combat,
            Phase::Effects,
            Phase::Ai,
            Phase::Scripts,
        ] {
            if phase == Phase::Effects {
                self.run_graphs(&ctx)?;
            }
            self.schedule
                .run_phase(phase, &mut self.world, &ctx)
                .map_err(CellError::Run)?;
        }
        // 3. The tick is durable before anything is sent (decision 0007):
        // system outcomes are logged, the tick ends, and the segment is
        // flushed, so every result a client is told is in the log. What
        // follows changes no hashed state.
        self.log_system_outcomes(tick)?;
        let state_hash = self.world.state_hash();
        if let Some(log) = self.log.as_mut() {
            log.end_tick(tick, state_hash)?;
            log.flush_segment()?;
        }
        // 4. Corrections.
        self.send_corrections(sink);
        // 5. Interest and Outbound.
        let jobs = self.replicate(tick, sink, workers);
        // Borders.
        self.collect_borders();
        // 6. Persist.
        self.schedule
            .run_phase(Phase::Persist, &mut self.world, &ctx)
            .map_err(CellError::Run)?;
        let refused = std::mem::take(&mut self.refused);
        Ok(TickReport {
            tick,
            state_hash,
            refused,
            jobs: jobs.0,
            bytes_out: jobs.1,
        })
    }

    /// Evaluates the gameplay graphs at the start of the effects phase.
    /// Actions run through the modules that registered them, with the
    /// world; the runtime is swapped out of it meanwhile (no allocation).
    fn run_graphs(&mut self, ctx: &TickContext) -> Result<(), CellError> {
        let rt = self
            .world
            .resources
            .get_mut::<GraphRuntime>()
            .ok_or(CellError::Config("graph runtime"))?;
        std::mem::swap(rt, &mut self.graph_spare);
        let mut actions = ModuleActions {
            world: &mut self.world,
            registry: &self.modules,
        };
        let start = self.stopwatch.as_ref().map(|s| s.now_nanos());
        let _ = self
            .graph_spare
            .evaluate(&self.catalog, ctx.tick, ctx.seed, &mut actions);
        if let (Some(s), Some(start)) = (self.stopwatch.as_ref(), start) {
            self.graph_timing.record(s.now_nanos().saturating_sub(start));
        }
        let rt = self
            .world
            .resources
            .get_mut::<GraphRuntime>()
            .ok_or(CellError::Config("graph runtime"))?;
        std::mem::swap(rt, &mut self.graph_spare);
        Ok(())
    }

    fn send_corrections(&mut self, sink: &mut dyn OutboundSink) {
        let Some(sessions) = self.world.resources.get_mut::<Sessions>() else {
            return;
        };
        for (session, correction) in sessions.corrections.iter() {
            let Some(slot) = self.slots.get(session) else {
                continue;
            };
            let Some(client) = self.rep.clients.get(*slot) else {
                continue;
            };
            let (adapter, conn) = match lock(client).as_ref() {
                Some(c) => (c.adapter, c.conn),
                None => continue,
            };
            let Some(a) = self.rep.adapters.get(adapter) else {
                continue;
            };
            self.scratch.clear();
            if sent(
                a.encode_outbound(&Outbound::SetPosition(*correction), &mut self.scratch),
                &mut self.encode_refused,
            ) {
                sink.send(adapter, conn, Channel::Reliable, &self.scratch);
            }
        }
        sessions.corrections.clear();
        for (session, refusal) in sessions.refusals.iter() {
            let Some(client) = self
                .slots
                .get(session)
                .and_then(|slot| self.rep.clients.get(*slot))
            else {
                continue;
            };
            let (adapter, conn) = match lock(client).as_ref() {
                Some(c) => (c.adapter, c.conn),
                None => continue,
            };
            let Some(a) = self.rep.adapters.get(adapter) else {
                continue;
            };
            self.scratch.clear();
            if sent(
                a.encode_outbound(&Outbound::ExtensionRefused(*refusal), &mut self.scratch),
                &mut self.encode_refused,
            ) {
                sink.send(adapter, conn, Channel::Reliable, &self.scratch);
            }
        }
        sessions.refusals.clear();
        self.send_relinked(sink);
        self.send_feature_states(sink);
        let Some(outbox) = self.world.resources.get_mut::<crate::modules::Outbox>() else {
            return;
        };
        for (session, kind, payload) in outbox.pending() {
            let Some(client) = self
                .slots
                .get(session)
                .and_then(|slot| self.rep.clients.get(*slot))
            else {
                continue;
            };
            let (adapter, conn) = match lock(client).as_ref() {
                Some(c) => (c.adapter, c.conn),
                None => continue,
            };
            let (Some(a), Some(bytes)) = (
                self.rep.adapters.get(adapter),
                mantis_adapter_contract::core_types::BoundedArray::from_slice(payload.as_slice()),
            ) else {
                continue;
            };
            let message = mantis_adapter_contract::ExtensionMessage {
                kind: *kind,
                payload: bytes,
            };
            self.scratch.clear();
            if sent(
                a.encode_outbound(&Outbound::ExtensionMessage(message), &mut self.scratch),
                &mut self.encode_refused,
            ) {
                sink.send(adapter, conn, Channel::Reliable, &self.scratch);
            }
        }
        outbox.clear();
    }

    fn replicate(
        &mut self,
        tick: Tick,
        sink: &mut dyn OutboundSink,
        workers: Option<&WorkerSet>,
    ) -> (BatchReport, u64) {
        // Acknowledgements first: they choose this tick's baselines.
        std::mem::swap(&mut *lock(&self.inbox.acks), &mut self.ack_drain);
        while let Some((session, t)) = self.ack_drain.pop_front() {
            if let Some(c) = self.slots.get(&session).and_then(|s| self.rep.clients.get(*s))
                && let Some(rep) = lock(c).as_mut()
            {
                rep.on_ack(t);
            }
        }
        // Rewind and the replication view.
        {
            let mut view = self
                .rep
                .view
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            view.begin(tick);
            let _ = self.q_repl.for_each(&mut self.world.components, |_, (r, b, l)| {
                let _ = view.push(*r, b.0.position, b.0.velocity, b.0.yaw, l.0);
            });
            for g in self.ghosts.iter() {
                let _ = view.push(g.repl, g.position, g.velocity, g.yaw, g.look);
            }
            view.finish();
            self.rewind
                .record(tick, view.entries.iter().map(|e| (e.repl, e.position)));
            // Markers, with source and target as replication ids.
            if let Some(rt) = self.world.resources.get::<GraphRuntime>() {
                for m in rt.markers() {
                    let repl = |e: EntityId| self.world.components.get::<ReplicationId>(e).map(|r| r.0);
                    let Some(source) = repl(m.source) else { continue };
                    let mut out = *m;
                    out.source = source;
                    out.target = m.target.and_then(repl);
                    if let mantis_core::graph::MarkerKind::Impact { target } = m.kind {
                        match repl(target) {
                            Some(t) => out.kind = mantis_core::graph::MarkerKind::Impact { target: t },
                            None => continue,
                        }
                    }
                    let _ = view.markers.push(out);
                }
            }
        }
        // Per-client headers, written on the cell thread.
        for (session, slot) in &self.slots {
            let Some(c) = self.rep.clients.get(*slot) else {
                continue;
            };
            let mut guard = lock(c);
            let Some(rep) = guard.as_mut() else { continue };
            let s = self
                .world
                .resources
                .get::<Sessions>()
                .and_then(|s| s.map.get(session));
            let avatar = s.and_then(|s| s.avatar);
            let local = avatar.and_then(|e| {
                let body = self.world.components.get::<Body>(e)?;
                let repl = self.world.components.get::<ReplicationId>(e)?;
                Some(LocalAvatar {
                    id: repl.0,
                    state: body.0,
                })
            });
            rep.avatar = local.map(|l| ReplicationId(l.id));
            rep.header = SnapshotHeader {
                server_tick: tick,
                ack: s.and_then(|s| s.last_seq),
                local,
                local_mods: avatar
                    .and_then(|e| self.world.components.get::<Mods>(e))
                    .map_or(MotionModifiers::NONE, |m| m.0),
            };
        }
        // Jobs.
        let report = if let Some(w) = workers {
            w.run_batch(
                &self.batch,
                &self.completion,
                self.slots.values().copied(),
                self.cfg.job_quota,
            )
        } else {
            let mut r = BatchReport::default();
            for slot in self.slots.values() {
                self.batch.run(*slot);
                r.inline += 1;
            }
            r
        };
        let bytes = self.send_snapshots(sink);
        (report, bytes)
    }

    /// Sends each client's encoded snapshot; returns the bytes sent.
    fn send_snapshots(&mut self, sink: &mut dyn OutboundSink) -> u64 {
        let mut bytes = 0u64;
        for slot in self.slots.values() {
            let Some(c) = self.rep.clients.get(*slot) else {
                continue;
            };
            let guard = lock(c);
            let Some(rep) = guard.as_ref() else { continue };
            if self.stopwatch.is_some() {
                self.encode_timing.record(rep.encode_nanos);
            }
            if !rep.out.is_empty() {
                sink.send(rep.adapter, rep.conn, Channel::Unreliable, &rep.out);
                bytes += rep.out.len() as u64;
            }
        }
        bytes
    }

    fn collect_borders(&mut self) {
        self.transfers_out.clear();
        self.ghost_exports.clear();
        let Some((min_x, max_x)) = self.cfg.region else {
            return;
        };
        let margin = self.cfg.ghost_margin;
        let view = self
            .rep
            .view
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for e in view.entries.iter() {
            // Ghosts are not ours to export.
            if self.ghosts.iter().any(|g| g.repl == e.repl) {
                continue;
            }
            let x = e.position.x;
            if x < min_x || x >= max_x {
                if self.offered.contains(&e.repl) {
                    continue;
                }
                let Some(local) = self
                    .world
                    .resource::<ReplMap>()
                    .and_then(|m| m.0.get(&e.repl).copied())
                else {
                    continue;
                };
                let session = self.world.components.get::<Controlled>(local).map(|c| c.0);
                let s =
                    session.and_then(|sid| self.world.resource::<Sessions>().and_then(|s| s.map.get(&sid)));
                let t = Transfer {
                    repl: e.repl,
                    body: self
                        .world
                        .components
                        .get::<Body>(local)
                        .map_or(MotionState::default(), |b| b.0),
                    mods: self
                        .world
                        .components
                        .get::<Mods>(local)
                        .map_or(MotionModifiers::NONE, |m| m.0),
                    look: e.look,
                    session: session.map(|s| s.0),
                    mode: s.map_or(mantis_adapter_contract::MovementMode::Predictive, |s| s.mode),
                    last_seq: s.and_then(|s| s.last_seq),
                    epoch: s.map_or(0, |s| s.epoch + 1),
                    envelope: s.map_or(EnvelopeState::new(e.position, 0), |s| s.envelope),
                    cheats: s.map_or(0, |s| s.cheats),
                    character: s.map_or(0, |s| s.character),
                    input: s.map(crate::movement::InputCarry::of).unwrap_or_default(),
                };
                if self.transfers_out.push(t).is_ok() {
                    let _ = self.offered.push(e.repl);
                }
            } else if x < min_x + margin || x >= max_x - margin {
                let _ = self.ghost_exports.push(*e);
            }
        }
    }
}

fn movement_system(
    w: &mut World,
    c: &TickContext,
    motion: &Motion,
    envelope: &EnvelopeConfig,
    inputs: InputConfig,
    ground: &(dyn GroundQuery + Send + Sync),
) -> Result<(), SystemError> {
    let World {
        components,
        resources,
    } = w;
    let sessions = resources
        .get_mut::<Sessions>()
        .ok_or(SystemError::Invariant("sessions"))?;
    let dt = c.rate.dt_seconds();
    let server_ms = tick_ms(c.tick, c.rate.hz());
    let Sessions { map, corrections, .. } = sessions;
    for s in map.values_mut() {
        let Some(avatar) = s.avatar else { continue };
        let mods = components
            .get::<Mods>(avatar)
            .map_or(MotionModifiers::NONE, |m| m.0);
        let Some(mut body) = components.get_mut::<Body>(avatar) else {
            continue;
        };
        if s.mode == mantis_adapter_contract::MovementMode::Predictive {
            if let Some(input) = next_input(s, inputs) {
                body.0 = motion.step(ground, &body.0, &input, &mods, dt);
            }
        } else {
            // Validated, and any mode a newer contract adds: claims checked
            // against the envelope (the stricter path).

            let start = body.0.position;
            let mut moved = false;
            for (claim, client_ms) in s.claims.iter().copied() {
                match handle_claim(
                    &mut s.envelope,
                    envelope,
                    motion,
                    &mods,
                    ground,
                    Claim {
                        position: claim,
                        client_ms,
                        server_ms,
                    },
                ) {
                    Ok(Some(p)) => {
                        body.0.position = p;
                        moved = true;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        s.cheats += 1;
                        let _ = corrections.push((
                            s.id,
                            mantis_adapter_contract::SetPosition {
                                entity: s.repl.0,
                                position: s.envelope.last_pos,
                                yaw: body.0.yaw,
                                tick: c.tick,
                            },
                        ));
                        body.0.position = s.envelope.last_pos;
                        break;
                    }
                }
            }
            s.claims.clear();
            body.0.velocity = if moved && dt > 0.0 {
                (body.0.position - start) / dt
            } else {
                Vec3::ZERO
            };
        }
    }
    Ok(())
}

/// An in-memory log sink whose bytes stay readable through a shared handle
/// (tests, tools, short recordings).
#[derive(Clone, Default, Debug)]
pub struct MemoryLog(Arc<Mutex<Vec<u8>>>);

impl MemoryLog {
    /// A copy of everything written so far.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        lock(&self.0).clone()
    }
}

impl LogSink for MemoryLog {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A sink that drops everything (replay, headless tests).
pub struct NullSink;

impl OutboundSink for NullSink {
    fn send(&mut self, _adapter: usize, _conn: ConnectionId, _channel: Channel, _bytes: &[u8]) {}
}

/// A cell replays from its own log: intents go through the inbox and the same
/// dispatch, and each `TickEnd` runs one tick (decision 0007). Build the
/// replaying cell without a log and with the recording's seed and start tick.
impl Cell {
    /// Drops what a replay queued for a tick whose log records end before
    /// its `TickEnd` (a crash tore the log): that tick never completed, so
    /// nothing of it runs. Recovery calls this after a replay that reports
    /// trailing records.
    pub fn discard_incomplete_tick(&mut self) {
        lock(&self.inbox.items).clear();
        lock(&self.commands.items).clear();
        self.commands_pending = 0;
        self.expected.clear();
        self.expected_system.clear();
    }
}

impl mantis_core::replay::Replayable for Cell {
    type Schema = CellLogSchema;
    type Error = CellError;

    /// Refuses to cross a script reload whose source the replayer did not
    /// supply: a log that silently diverges is worse than one that stops.
    fn apply_intent(
        &mut self,
        _tick: Tick,
        session: SessionId,
        intent: &CellIntent,
    ) -> Result<(), CellError> {
        if let CellIntent::ScriptReload { source, .. } = intent
            && !self.script_sources.contains_key(source)
        {
            return Err(CellError::Config(
                "the log reloads a script whose source was not supplied",
            ));
        }
        if self.inbox.push(session, *intent) {
            Ok(())
        } else {
            Err(CellError::Config("inbox full"))
        }
    }

    fn apply_seed(&mut self, _tick: Tick, _seed: u64) -> Result<(), CellError> {
        Ok(())
    }

    /// Queues the command; it executes at the start of the tick, exactly
    /// where it executed live (before that tick's intents).
    fn apply_command(&mut self, _tick: Tick, command: &ModuleCommand) -> Result<(), CellError> {
        if self.commands.push(*command) {
            self.commands_pending += 1;
            Ok(())
        } else {
            Err(CellError::Config("command inbox full"))
        }
    }

    /// Records the expected outcome; the tick compares it with the outcome
    /// the replayed command (or system) produces and fails on any
    /// difference. A command's outcome directly follows the command in the
    /// log; an outcome with no command pending was emitted by a system.
    fn check_outcome(&mut self, _tick: Tick, outcome: &ModuleOutcome) -> Result<(), CellError> {
        if self.commands_pending > 0 {
            self.commands_pending -= 1;
            self.expected.push_back(*outcome);
        } else {
            self.expected_system.push_back(*outcome);
        }
        Ok(())
    }

    fn step(&mut self, tick: Tick) -> Result<(), CellError> {
        let report = self.tick(&mut NullSink, None)?;
        if report.tick == tick {
            Ok(())
        } else {
            Err(CellError::Config("replay tick mismatch"))
        }
    }

    fn state_hash(&self) -> u64 {
        self.world.state_hash()
    }
}
