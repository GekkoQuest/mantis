//! The server's module registry surface (plan 13, decision 0010).
//!
//! A module's server crate implements [`ServerModule`]; at cell start-up the
//! host installs the package's resolved [`ModuleSet`] into each cell, and
//! each module registers through one [`Registrar`]:
//!
//! - **handlers** for package-defined intent extensions, each with the
//!   session state it requires ([`Require`]);
//! - **commands**: economy-touching extensions and service requests, which
//!   travel the log as commands, execute on delivery, and log their outcome
//!   (decision 0007), so no module ever writes a database;
//! - **systems** with phase and priority, inside the cell's schedule and its
//!   conflict lint, named `<module key>.<name>`;
//! - **components**, **resources**, **events** ([`mantis_core::module::Events`])
//!   and **queries** ([`mantis_core::module::Queries`]);
//! - **tables** (content the module reads) and **metrics** (Ops counters).
//!
//! Disabling a module (a feature flag, logged as an intent so replay sees it)
//! makes the dispatcher answer its extensions with `FeatureDisabled`, skips
//! its systems, and makes its queries answer `FeatureDisabled`.

use core::fmt;
use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::{DecodeError, Decoder, Encoder, Wire};
pub use mantis_adapter_contract::{ExtensionKind, ExtensionRefusal};
use mantis_core::ecs::{Access, AccessBuilder, Component, EcsError, Resource, World};
use mantis_core::graph::{ActionCall, ActionError, GraphCatalog};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::module::bus::{DuplicateQuery, QueryFn, advance_events};
use mantis_core::module::{Event, Events, ModuleGraph, Queries, Query};
use mantis_core::schedule::{Schedule, ScheduleError, System, SystemDesc, TickContext};

/// Largest extension payload, as the contract's `Extension` message allows.
pub const PAYLOAD: usize = 512;

/// An extension payload carried by value (intents and commands are `Copy`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Payload {
    len: u16,
    bytes: [u8; PAYLOAD],
}

impl Payload {
    /// An empty payload.
    pub const EMPTY: Self = Self {
        len: 0,
        bytes: [0; PAYLOAD],
    };

    /// A copy of `bytes`, or `None` when longer than [`PAYLOAD`].
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let mut p = Self::EMPTY;
        p.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        p.len = u16::try_from(bytes.len()).ok()?;
        Some(p)
    }

    /// A payload from up to [`PAYLOAD`] bytes, or `None` when there are more.
    #[must_use]
    pub fn from_bytes(bytes: impl Iterator<Item = u8>) -> Option<Self> {
        let mut p = Self::EMPTY;
        for b in bytes {
            *p.bytes.get_mut(usize::from(p.len))? = b;
            p.len += 1;
        }
        Some(p)
    }

    /// The bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Payload({} bytes)", self.len)
    }
}

impl Wire for Payload {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u16(self.len);
        e.bytes(self.as_slice());
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = usize::from(d.u16()?);
        Self::from_slice(d.take(len)?).ok_or(DecodeError::Invalid("payload length"))
    }
}

/// An economy command: an input to the cell, logged before it executes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModuleCommand {
    /// The command kind (an extension kind registered with
    /// [`Registrar::command`]).
    pub kind: ExtensionKind,
    /// The session it acts for, if any.
    pub session: Option<SessionId>,
    /// The client's request id, echoed in a refusal (0: untracked).
    pub request: u32,
    /// Module-encoded arguments.
    pub payload: Payload,
}

/// What a command did: logged after it, checked on replay, drained by the
/// persistence writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModuleOutcome {
    /// The command kind.
    pub kind: ExtensionKind,
    /// The session it acted for.
    pub session: Option<SessionId>,
    /// Success, or why it was refused.
    pub result: Result<(), ExtensionRefusal>,
    /// Module-encoded result (ledger rows, for example).
    pub payload: Payload,
}

impl ModuleOutcome {
    /// A success of `kind` for `session`, carrying `payload` encoded (an
    /// economy outcome's payload starts with its `Ledger`). A payload past
    /// [`PAYLOAD`] bytes is dropped.
    #[must_use]
    pub fn ok(kind: ExtensionKind, session: Option<SessionId>, payload: &impl Wire) -> Self {
        let mut bytes = Vec::new();
        mantis_core::wire::encode_into(payload, &mut bytes);
        Self {
            kind,
            session,
            result: Ok(()),
            payload: Payload::from_slice(&bytes).unwrap_or(Payload::EMPTY),
        }
    }

    /// [`ModuleOutcome::ok`] for `command`.
    #[must_use]
    pub fn done(command: &ModuleCommand, payload: &impl Wire) -> Self {
        Self::ok(command.kind, command.session, payload)
    }

    /// A refusal with no payload.
    #[must_use]
    pub fn refused(command: &ModuleCommand, reason: ExtensionRefusal) -> Self {
        Self {
            kind: command.kind,
            session: command.session,
            result: Err(reason),
            payload: Payload::EMPTY,
        }
    }
}

fn put_session(e: &mut Encoder<'_>, s: Option<SessionId>) {
    e.bool(s.is_some());
    e.u64(s.map_or(0, |s| s.0));
}

fn get_session(d: &mut Decoder<'_>) -> Result<Option<SessionId>, DecodeError> {
    let some = d.bool()?;
    let id = d.u64()?;
    Ok(some.then_some(SessionId(id)))
}

impl Wire for ModuleCommand {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u16(self.kind.0);
        put_session(e, self.session);
        e.u32(self.request);
        self.payload.encode(e);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            kind: ExtensionKind(d.u16()?),
            session: get_session(d)?,
            request: d.u32()?,
            payload: Payload::decode(d)?,
        })
    }
}

impl Wire for ModuleOutcome {
    fn encode(&self, e: &mut Encoder<'_>) {
        e.u16(self.kind.0);
        put_session(e, self.session);
        match self.result {
            Ok(()) => e.u8(0xFF),
            Err(r) => e.u8(r as u8),
        }
        self.payload.encode(e);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let kind = ExtensionKind(d.u16()?);
        let session = get_session(d)?;
        let result = match d.u8()? {
            0xFF => Ok(()),
            0 => Err(ExtensionRefusal::FeatureDisabled),
            1 => Err(ExtensionRefusal::NotAllowed),
            2 => Err(ExtensionRefusal::Invalid),
            _ => return Err(DecodeError::Invalid("outcome result")),
        };
        Ok(Self {
            kind,
            session,
            result,
            payload: Payload::decode(d)?,
        })
    }
}

/// The session state an extension handler requires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Require {
    /// The session is in this cell.
    Joined,
    /// The session is in this cell and has an avatar.
    Avatar,
}

/// Index of a module in its set, in registration order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ModuleId(pub u16);

/// Handles one extension intent from `session`. Refusals are answered to
/// the client.
pub type HandlerFn = fn(&mut World, &TickContext, SessionId, &[u8]) -> Result<(), ExtensionRefusal>;

/// Handles one inbound service update (a logged intent).
pub type ServiceFn = fn(&mut World, &TickContext, &[u8]) -> Result<(), &'static str>;

/// Executes one economy command on delivery.
pub type CommandFn = fn(&mut World, &TickContext, &ModuleCommand) -> ModuleOutcome;

/// A module's server half. Implemented by `packages/<p>/modules/<f>/server`.
pub trait ServerModule: Send + Sync {
    /// The module key, as in its manifest.
    fn key(&self) -> &'static str;

    /// Registers everything the module provides.
    ///
    /// # Errors
    /// [`RegistryError`]; the host refuses to start.
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError>;
}

/// Registration was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegistryError {
    /// An extension or command kind is already taken.
    DuplicateKind(u16),
    /// A system's name is not `<module key>.<name>`.
    SystemName(&'static str),
    /// The schedule refused a system.
    Schedule(ScheduleError),
    /// The world refused a component or resource.
    Ecs(EcsError),
    /// Two modules answer the same query.
    DuplicateQuery(&'static str),
    /// A table the module needs is absent from content.
    MissingTable(String),
    /// The resolved graph names a module no linked crate implements.
    NotLinked(String),
    /// A module failed its own setup.
    Module(&'static str),
    /// A script failed to load (lint, compile, or run).
    Script(String),
    /// A module registered a graph action its manifest does not declare.
    UndeclaredAction(String),
    /// A module declared a graph action it did not register.
    UnregisteredAction(String),
    /// A declared graph action is absent from the cell's catalog (the
    /// catalog was not built from this module set's actions).
    ActionNotInCatalog(String),
    /// A graph action registered twice.
    DuplicateAction(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKind(k) => write!(f, "extension kind {k} is registered twice"),
            Self::SystemName(n) => write!(f, "system {n} is not named after its module"),
            Self::Schedule(e) => write!(f, "schedule: {e:?}"),
            Self::Ecs(e) => write!(f, "world: {e}"),
            Self::DuplicateQuery(q) => write!(f, "{q} has two providers"),
            Self::MissingTable(t) => write!(f, "content table {t} is missing"),
            Self::NotLinked(m) => write!(f, "module {m} is in the graph but not linked into this server"),
            Self::Module(why) => write!(f, "module setup: {why}"),
            Self::Script(why) => write!(f, "script: {why}"),
            Self::UndeclaredAction(a) => write!(
                f,
                "graph action {a} is registered but not declared in the manifest"
            ),
            Self::UnregisteredAction(a) => write!(f, "graph action {a} is declared but not registered"),
            Self::ActionNotInCatalog(a) => write!(f, "graph action {a} is not in the cell's catalog"),
            Self::DuplicateAction(a) => write!(f, "graph action {a} is registered twice"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<EcsError> for RegistryError {
    fn from(e: EcsError) -> Self {
        Self::Ecs(e)
    }
}

impl From<ScheduleError> for RegistryError {
    fn from(e: ScheduleError) -> Self {
        Self::Schedule(e)
    }
}

impl From<DuplicateQuery> for RegistryError {
    fn from(e: DuplicateQuery) -> Self {
        Self::DuplicateQuery(e.0)
    }
}

/// Which modules are enabled. Simulation state: hashed and changed only
/// through a logged intent.
#[derive(Clone, Debug, Default)]
pub struct ModuleStates {
    /// Module keys in registration order.
    pub keys: Vec<String>,
    /// Enabled, per module.
    pub enabled: Vec<bool>,
    /// Every module flag by full name (`<module key>.<flag>`), after
    /// package overrides and live (Ops) changes. Modules read them when
    /// they act, through [`flag`], so a live change takes effect at the
    /// tick boundary it was applied on.
    pub flags: BTreeMap<String, bool>,
    /// Every live tunable by full name (`<module key>.<name>`), read
    /// through [`tunable`].
    pub tunables: BTreeMap<String, f32>,
}

impl ModuleStates {
    /// True when module `id` is enabled.
    #[must_use]
    pub fn is_enabled(&self, id: ModuleId) -> bool {
        self.enabled.get(usize::from(id.0)).copied().unwrap_or(false)
    }

    /// The id of `key`.
    #[must_use]
    pub fn id(&self, key: &str) -> Option<ModuleId> {
        self.keys
            .iter()
            .position(|k| k == key)
            .and_then(|i| u16::try_from(i).ok())
            .map(ModuleId)
    }
}

impl StateHash for ModuleStates {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.keys.len() as u64);
        for (k, on) in self.keys.iter().zip(&self.enabled) {
            h.write(k.as_bytes());
            h.write_u8(0);
            h.write_u8(u8::from(*on));
        }
        h.write_u64(self.flags.len() as u64);
        for (k, on) in &self.flags {
            h.write(k.as_bytes());
            h.write_u8(0);
            h.write_u8(u8::from(*on));
        }
        h.write_u64(self.tunables.len() as u64);
        for (k, v) in &self.tunables {
            h.write(k.as_bytes());
            h.write_u8(0);
            h.write_u32(v.to_bits());
        }
    }
}

impl Resource for ModuleStates {
    const NAME: &'static str = "server.module_states";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.keys.len()).unwrap_or(u32::MAX));
        for (k, on) in self.keys.iter().zip(&self.enabled) {
            e.u16(u16::try_from(k.len()).unwrap_or(u16::MAX));
            e.bytes(k.as_bytes());
            e.bool(*on);
        }
        e.u32(u32::try_from(self.flags.len()).unwrap_or(u32::MAX));
        for (k, on) in &self.flags {
            e.u16(u16::try_from(k.len()).unwrap_or(u16::MAX));
            e.bytes(k.as_bytes());
            e.bool(*on);
        }
        e.u32(u32::try_from(self.tunables.len()).unwrap_or(u32::MAX));
        for (k, v) in &self.tunables {
            e.u16(u16::try_from(k.len()).unwrap_or(u16::MAX));
            e.bytes(k.as_bytes());
            e.u32(v.to_bits());
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        fn name(
            d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
        ) -> Result<String, mantis_adapter_contract::core_types::DecodeError> {
            let n = usize::from(d.u16()?);
            core::str::from_utf8(d.take(n)?)
                .map(str::to_owned)
                .map_err(|_| mantis_adapter_contract::core_types::DecodeError::Invalid("utf-8"))
        }
        let n = d.u32()?;
        let mut keys = Vec::new();
        let mut enabled = Vec::new();
        for _ in 0..n {
            keys.push(name(d)?);
            enabled.push(d.bool()?);
        }
        // The module set must be the one this cell installed.
        if keys != self.keys {
            return Err(mantis_adapter_contract::core_types::DecodeError::Invalid(
                "snapshot module set differs",
            ));
        }
        self.enabled = enabled;
        self.flags.clear();
        for _ in 0..d.u32()? {
            let k = name(d)?;
            self.flags.insert(k, d.bool()?);
        }
        self.tunables.clear();
        for _ in 0..d.u32()? {
            let k = name(d)?;
            self.tunables.insert(k, f32::from_bits(d.u32()?));
        }
        Ok(())
    }
}

/// Ops counters registered by modules. Not simulation state.
#[derive(Debug, Default)]
pub struct Metrics {
    entries: Vec<(String, u64)>,
}

/// A registered metric.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MetricId(pub usize);

impl Metrics {
    /// Adds `n` to a metric.
    pub fn add(&mut self, id: MetricId, n: u64) {
        if let Some((_, v)) = self.entries.get_mut(id.0) {
            *v = v.saturating_add(n);
        }
    }

    /// Every metric by full name (`<module key>.<name>`).
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), *v))
    }

    /// One metric by full name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<u64> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }
}

/// Explicitly not hashed: counters are observation, not state.
impl StateHash for Metrics {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for Metrics {
    const NAME: &'static str = "server.module_metrics";

    /// Ops counters, not simulation state.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

/// Outcomes produced by systems rather than commands (lead ruling, M4):
/// a system that changes Inventories or Mailboxes, or anything else that
/// produces ledger rows or must be durable, records it here in the same
/// tick. The cell logs every one before the tick ends, hands it to the
/// persistence writer with the command outcomes, and on replay compares
/// it with the recorded one.
///
/// A system checks [`SystemOutcomes::has_room`] before it changes anything:
/// a change whose outcome cannot be recorded waits for the next tick.
/// Systems in the `Persist` phase must not emit (the tick's log is closed).
#[derive(Debug)]
pub struct SystemOutcomes {
    items: mantis_core::mem::BoundedVec<ModuleOutcome>,
}

impl SystemOutcomes {
    /// Outcomes systems may record per tick.
    pub const CAPACITY: usize = 256;

    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: mantis_core::mem::BoundedVec::with_capacity(Self::CAPACITY),
        }
    }

    /// True while another outcome fits this tick.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.items.len() < Self::CAPACITY
    }

    /// Records one outcome.
    ///
    /// # Errors
    /// [`SystemError::Invariant`](mantis_core::schedule::SystemError::Invariant) when the tick's sink is full: the system
    /// checked [`SystemOutcomes::has_room`] first, so this is a bug.
    pub fn emit(&mut self, outcome: ModuleOutcome) -> Result<(), mantis_core::schedule::SystemError> {
        self.items
            .push(outcome)
            .map_err(|_| mantis_core::schedule::SystemError::Invariant("system outcome sink full"))
    }

    /// This tick's outcomes, in emission order.
    #[must_use]
    pub fn pending(&self) -> &[ModuleOutcome] {
        &self.items
    }

    /// Forgets this tick's outcomes (after the cell logged them).
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

impl Default for SystemOutcomes {
    fn default() -> Self {
        Self::new()
    }
}

/// Not hashed: drained into the log before the tick's state hash.
impl StateHash for SystemOutcomes {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for SystemOutcomes {
    const NAME: &'static str = "server.system_outcomes";

    /// Output: logged and cleared every tick.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

/// True while a system may still record an outcome this tick.
#[must_use]
pub fn outcome_room(world: &World) -> bool {
    world
        .resource::<SystemOutcomes>()
        .is_some_and(SystemOutcomes::has_room)
}

/// Records a system outcome (see [`SystemOutcomes`]).
///
/// # Errors
/// [`SystemError::Invariant`](mantis_core::schedule::SystemError::Invariant)
/// when the sink is missing or full.
pub fn emit_outcome(
    world: &mut World,
    outcome: ModuleOutcome,
) -> Result<(), mantis_core::schedule::SystemError> {
    world
        .resource_mut::<SystemOutcomes>()
        .ok_or(mantis_core::schedule::SystemError::Invariant(
            "system outcome sink",
        ))?
        .emit(outcome)
}

/// Module messages to clients, sent at the end of the tick with the
/// corrections. Output, not state: drained every tick and not hashed.
#[derive(Debug)]
pub struct Outbox {
    items: mantis_core::mem::BoundedVec<(SessionId, ExtensionKind, Payload)>,
    dropped: u64,
}

impl Outbox {
    /// Room for `capacity` messages per tick.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: mantis_core::mem::BoundedVec::with_capacity(capacity),
            dropped: 0,
        }
    }

    /// Queues `payload` for `session`'s client. False (and counted) when the
    /// payload is too long or the tick's outbox is full.
    pub fn send(&mut self, session: SessionId, kind: ExtensionKind, payload: &[u8]) -> bool {
        let ok = Payload::from_slice(payload).is_some_and(|p| self.items.push((session, kind, p)).is_ok());
        if !ok {
            self.dropped += 1;
        }
        ok
    }

    /// Messages refused for size or capacity.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// This tick's messages.
    #[must_use]
    pub fn pending(&self) -> &[(SessionId, ExtensionKind, Payload)] {
        &self.items
    }

    /// Forgets this tick's messages (after sending).
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Explicitly not hashed: output, like corrections.
impl StateHash for Outbox {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for Outbox {
    const NAME: &'static str = "server.module_outbox";

    /// Output: sent and cleared every tick.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

/// Queues a module message to `session`'s client (see [`Outbox`]).
pub fn send_to_client(world: &mut World, session: SessionId, kind: ExtensionKind, payload: &[u8]) -> bool {
    world
        .resource_mut::<Outbox>()
        .is_some_and(|o| o.send(session, kind, payload))
}

/// Decodes an extension payload as contract message `M`.
///
/// # Errors
/// [`ExtensionRefusal::Invalid`] for a malformed payload.
pub fn decode<M: mantis_core::wire::Message>(payload: &[u8]) -> Result<M, ExtensionRefusal> {
    mantis_core::wire::decode_message::<M>(payload).map_err(|_| ExtensionRefusal::Invalid)
}

/// Sends contract message `M` to `session`'s client, under `M`'s id as the
/// extension kind.
pub fn tell<M: mantis_core::wire::Message>(world: &mut World, session: SessionId, msg: &M) -> bool {
    let mut bytes = Vec::new();
    mantis_core::wire::encode_into(msg, &mut bytes);
    send_to_client(world, session, ExtensionKind(M::ID.0), &bytes)
}

/// Sends contract message `M` to `character`'s client, if it is in this cell.
pub fn tell_character<M: mantis_core::wire::Message>(world: &mut World, character: u64, msg: &M) -> bool {
    session_of(world, character).is_some_and(|s| tell(world, s, msg))
}

/// The character of the session making a request: the module's caller.
///
/// # Errors
/// [`ExtensionRefusal::NotAllowed`] when the session has no character in
/// this cell.
pub fn caller(world: &World, session: SessionId) -> Result<u64, ExtensionRefusal> {
    character_of(world, session).ok_or(ExtensionRefusal::NotAllowed)
}

/// The session's persistent character identity, if it is in this cell.
#[must_use]
pub fn character_of(world: &World, session: SessionId) -> Option<u64> {
    world
        .resource::<crate::session::Sessions>()
        .and_then(|s| s.map.get(&session))
        .map(|s| s.character)
}

/// The session in this cell playing `character`.
#[must_use]
pub fn session_of(world: &World, character: u64) -> Option<SessionId> {
    world
        .resource::<crate::session::Sessions>()
        .and_then(|s| s.map.values().find(|x| x.character == character))
        .map(|s| s.id)
}

/// The session whose avatar replicates as `entity` (what clients name).
#[must_use]
pub fn session_of_entity(world: &World, entity: mantis_core::ecs::EntityId) -> Option<SessionId> {
    world
        .resource::<crate::session::Sessions>()
        .and_then(|s| s.map.values().find(|x| x.repl.0 == entity))
        .map(|s| s.id)
}

/// A session's avatar position, if it has one here.
#[must_use]
pub fn position_of(world: &World, session: SessionId) -> Option<mantis_core::math::Vec3> {
    let avatar = world
        .resource::<crate::session::Sessions>()
        .and_then(|s| s.map.get(&session))
        .and_then(|s| s.avatar)?;
    world.get::<crate::components::Body>(avatar).map(|b| b.0.position)
}

/// Every session in this cell, in id order.
pub fn sessions(world: &World) -> impl Iterator<Item = SessionId> + '_ {
    world
        .resource::<crate::session::Sessions>()
        .into_iter()
        .flat_map(|s| s.map.keys().copied())
}

/// Content tables available to modules, by name. Covered by the content
/// hash, so not hashed again.
#[derive(Debug, Default)]
pub struct Tables {
    tables: BTreeMap<String, Arc<[u8]>>,
}

impl Tables {
    /// Adds a table.
    pub fn insert(&mut self, name: &str, bytes: Arc<[u8]>) {
        self.tables.insert(name.to_owned(), bytes);
    }

    /// A table's bytes.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.tables.get(name).map(|t| &t[..])
    }
}

/// Explicitly not hashed: tables are content, covered by the content hash.
impl StateHash for Tables {
    fn state_hash(&self, _h: &mut StableHasher) {}
}

impl Resource for Tables {
    const NAME: &'static str = "server.module_tables";

    /// Content, installed with the modules.
    fn save(&self, _e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        mantis_core::ecs::Saved::Rebuilt
    }
}

#[derive(Clone, Copy)]
struct Handler {
    module: ModuleId,
    require: Require,
    run: HandlerFn,
}

#[derive(Clone, Copy)]
struct Command {
    module: ModuleId,
    run: CommandFn,
}

/// What modules registered in one cell.
#[derive(Default)]
pub struct Registry {
    /// Graph action handlers, by action id.
    actions: Vec<Option<(ModuleId, ActionFn)>>,
    handlers: BTreeMap<u16, Handler>,
    commands: BTreeMap<u16, Command>,
    services: BTreeMap<u16, (ModuleId, ServiceFn)>,
    advance: Vec<fn(&mut World)>,
}

/// How an extension kind is handled (the host routes by this).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// An intent, dispatched to a handler.
    Intent,
    /// An economy command, logged and executed on delivery.
    Command,
}

impl Registry {
    /// Applies one graph action through the module that registered it.
    /// Allocation-free.
    ///
    /// # Errors
    /// The handler's [`ActionError`]; also when no module registered the
    /// action or its module is disabled (the instance stops: fail closed).
    pub fn apply_action(&self, world: &mut World, call: &ActionCall) -> Result<(), ActionError> {
        let Some(Some((module, run))) = self.actions.get(usize::from(call.action.0)) else {
            return Err(ActionError("no module handles this action"));
        };
        let enabled = world
            .resource::<ModuleStates>()
            .is_some_and(|s| s.is_enabled(*module));
        if !enabled {
            return Err(ActionError("the action's module is disabled"));
        }
        run(world, call)
    }

    /// How `kind` is handled, if any module registered it.
    #[must_use]
    pub fn route(&self, kind: ExtensionKind) -> Option<Route> {
        if self.handlers.contains_key(&kind.0) {
            Some(Route::Intent)
        } else if self.commands.contains_key(&kind.0) {
            Some(Route::Command)
        } else {
            None
        }
    }

    /// Every registered kind and its route.
    #[must_use]
    pub fn routes(&self) -> BTreeMap<u16, Route> {
        self.handlers
            .keys()
            .map(|k| (*k, Route::Intent))
            .chain(self.commands.keys().map(|k| (*k, Route::Command)))
            .collect()
    }

    /// Dispatches an extension intent. Allocation-free.
    ///
    /// # Errors
    /// The refusal to answer the client with.
    pub fn handle(
        &self,
        world: &mut World,
        ctx: &TickContext,
        session: SessionId,
        kind: ExtensionKind,
        payload: &[u8],
    ) -> Result<(), ExtensionRefusal> {
        let h = self.handlers.get(&kind.0).ok_or(ExtensionRefusal::Invalid)?;
        let on = world
            .resource::<ModuleStates>()
            .is_some_and(|m| m.is_enabled(h.module));
        if !on {
            return Err(ExtensionRefusal::FeatureDisabled);
        }
        let joined = world
            .resource::<crate::session::Sessions>()
            .and_then(|s| s.map.get(&session))
            .map(|s| s.avatar.is_some());
        match (h.require, joined) {
            (_, None) | (Require::Avatar, Some(false)) => return Err(ExtensionRefusal::NotAllowed),
            _ => {}
        }
        (h.run)(world, ctx, session, payload)
    }

    /// Dispatches an inbound service update to the module that registered
    /// its topic. A disabled module ignores it.
    ///
    /// # Errors
    /// No module handles the topic, or the handler refused the update.
    pub fn service(
        &self,
        world: &mut World,
        ctx: &TickContext,
        topic: u16,
        payload: &[u8],
    ) -> Result<(), &'static str> {
        let (module, run) = self
            .services
            .get(&topic)
            .ok_or("no module handles this service topic")?;
        let on = world
            .resource::<ModuleStates>()
            .is_some_and(|m| m.is_enabled(*module));
        if on { run(world, ctx, payload) } else { Ok(()) }
    }

    /// Executes a command on delivery.
    pub fn execute(&self, world: &mut World, ctx: &TickContext, command: &ModuleCommand) -> ModuleOutcome {
        let Some(c) = self.commands.get(&command.kind.0) else {
            return ModuleOutcome::refused(command, ExtensionRefusal::Invalid);
        };
        let on = world
            .resource::<ModuleStates>()
            .is_some_and(|m| m.is_enabled(c.module));
        if !on {
            return ModuleOutcome::refused(command, ExtensionRefusal::FeatureDisabled);
        }
        (c.run)(world, ctx, command)
    }

    /// Tick boundary for every registered event queue. Allocation-free.
    pub fn advance_events(&self, world: &mut World) {
        for f in &self.advance {
            f(world);
        }
    }
}

/// Applies one graph action: the module's rule behind an `Action` node.
/// Deterministic, allocation-free, and touching only simulation state.
pub type ActionFn = fn(&mut World, &ActionCall) -> Result<(), ActionError>;

/// What one module registers through. Every registration is tagged with
/// the module, so disabling it reaches all of them.
pub struct Registrar<'a> {
    module: ModuleId,
    key: &'static str,
    catalog: &'a GraphCatalog,
    declared: &'a [String],
    optional: &'a [(String, mantis_core::module::OptionalProvider)],
    registry: &'a mut Registry,
    world: &'a mut World,
    schedule: &'a mut Schedule,
    metrics: Vec<(String, u64)>,
    flags: BTreeMap<String, bool>,
    tunables: Vec<(String, f32)>,
    tick_rate: u32,
    seed: mantis_core::rng::Seed,
}

impl Registrar<'_> {
    /// What the package provides for `contract`, one of this module's
    /// manifest `optional` contracts: resolved when the package started.
    /// `Absent` also for a contract the manifest does not name.
    #[must_use]
    pub fn optional(&self, contract: &str) -> mantis_core::module::OptionalProvider {
        self.optional
            .iter()
            .find(|(c, _)| c == contract)
            .map_or(mantis_core::module::OptionalProvider::Absent, |(_, p)| p.clone())
    }

    /// The value of one of this module's flags (short name, as declared in
    /// its manifest) after package and Ops overrides. Undeclared flags read
    /// as false.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.get(name).copied().unwrap_or(false)
    }

    /// Declares a live tunable, `<module key>.<name>`, with its start value.
    /// Ops may change it while the cell runs (signed, applied at a tick
    /// boundary, logged); the module reads it through [`tunable`].
    pub fn tunable(&mut self, name: &str, start: f32) -> String {
        let full = format!("{}.{name}", self.key);
        self.tunables.push((full.clone(), start));
        full
    }

    /// The cell's random seed (for the module's own deterministic streams).
    #[must_use]
    pub fn seed(&self) -> mantis_core::rng::Seed {
        self.seed
    }

    /// The cell's tick rate in Hz (to turn durations into ticks).
    #[must_use]
    pub fn tick_rate(&self) -> u32 {
        self.tick_rate
    }

    /// The module's key.
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// Registers the handler of graph action `name`, which the module's
    /// manifest must declare under `graph_actions`. Graphs reach it by the
    /// id the cell's catalog gave the name. The handler runs inside graph
    /// evaluation, before the effects phase's systems; it must not touch
    /// the graph runtime.
    ///
    /// # Errors
    /// [`RegistryError::UndeclaredAction`], [`RegistryError::ActionNotInCatalog`],
    /// or [`RegistryError::DuplicateAction`].
    pub fn graph_action(&mut self, name: &str, run: ActionFn) -> Result<(), RegistryError> {
        if !self.declared.iter().any(|d| d == name) {
            return Err(RegistryError::UndeclaredAction(name.to_owned()));
        }
        let id = self
            .catalog
            .action_id(name)
            .ok_or_else(|| RegistryError::ActionNotInCatalog(name.to_owned()))?;
        let index = usize::from(id.0);
        if self.registry.actions.len() <= index {
            self.registry.actions.resize(index + 1, None);
        }
        let slot = self
            .registry
            .actions
            .get_mut(index)
            .ok_or_else(|| RegistryError::ActionNotInCatalog(name.to_owned()))?;
        if slot.is_some() {
            return Err(RegistryError::DuplicateAction(name.to_owned()));
        }
        *slot = Some((self.module, run));
        Ok(())
    }

    /// Registers a handler for extension intents of `kind`.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateKind`].
    pub fn handler(
        &mut self,
        kind: ExtensionKind,
        require: Require,
        run: HandlerFn,
    ) -> Result<(), RegistryError> {
        if self.registry.route(kind).is_some() {
            return Err(RegistryError::DuplicateKind(kind.0));
        }
        self.registry.handlers.insert(
            kind.0,
            Handler {
                module: self.module,
                require,
                run,
            },
        );
        Ok(())
    }

    /// Registers the handler of inbound service updates on `topic`
    /// ([`crate::service`]): they arrive as logged intents.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateKind`] when another module took the topic.
    pub fn service(&mut self, topic: u16, run: ServiceFn) -> Result<(), RegistryError> {
        if self.registry.services.contains_key(&topic) {
            return Err(RegistryError::DuplicateKind(topic));
        }
        self.registry.services.insert(topic, (self.module, run));
        Ok(())
    }

    /// Registers an economy command executor for `kind`. Client extensions
    /// of this kind are routed as commands: logged, then executed on
    /// delivery, with the outcome logged after.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateKind`].
    pub fn command(&mut self, kind: ExtensionKind, run: CommandFn) -> Result<(), RegistryError> {
        if self.registry.route(kind).is_some() {
            return Err(RegistryError::DuplicateKind(kind.0));
        }
        self.registry.commands.insert(
            kind.0,
            Command {
                module: self.module,
                run,
            },
        );
        Ok(())
    }

    /// Adds a system. Its name must be `<module key>.<name>`.
    ///
    /// # Errors
    /// [`RegistryError::SystemName`] or the schedule's refusal.
    pub fn system(&mut self, desc: SystemDesc, system: impl System + 'static) -> Result<(), RegistryError> {
        let named = desc
            .name
            .strip_prefix(self.key)
            .is_some_and(|rest| rest.len() > 1 && rest.starts_with('.'));
        if !named {
            return Err(RegistryError::SystemName(desc.name));
        }
        self.schedule.add(desc, system)?;
        Ok(())
    }

    /// Starts an access declaration for a system against this cell's world.
    pub fn access(&mut self) -> AccessBuilder<'_> {
        Access::builder(self.world)
    }

    /// Registers a component type.
    ///
    /// # Errors
    /// The world's refusal.
    pub fn component<T: Component>(&mut self) -> Result<(), RegistryError> {
        self.world.register::<T>()?;
        Ok(())
    }

    /// Inserts a resource.
    ///
    /// # Errors
    /// The world's refusal.
    pub fn resource<R: Resource>(&mut self, value: R) -> Result<(), RegistryError> {
        self.world.insert_resource(value)?;
        Ok(())
    }

    /// Declares an event type this module sends, with room for `capacity`
    /// events per tick. Idempotent: a receiver may declare it too.
    ///
    /// # Errors
    /// The world's refusal.
    pub fn event<T: Event>(&mut self, capacity: usize) -> Result<(), RegistryError> {
        if self.world.resource::<Events<T>>().is_none() {
            self.world.insert_resource(Events::<T>::with_capacity(capacity))?;
            self.registry.advance.push(advance_events::<T>);
        }
        Ok(())
    }

    /// Answers query `Q`.
    ///
    /// # Errors
    /// [`RegistryError::DuplicateQuery`].
    pub fn query<Q: Query>(&mut self, answer: QueryFn<Q>) -> Result<(), RegistryError> {
        if self.world.resource::<Queries>().is_none() {
            self.world.insert_resource(Queries::default())?;
        }
        let queries = self
            .world
            .resource_mut::<Queries>()
            .ok_or(RegistryError::Module("queries resource"))?;
        queries.provide::<Q>(self.key, answer)?;
        Ok(())
    }

    /// The bytes of content table `name`.
    ///
    /// # Errors
    /// [`RegistryError::MissingTable`]: the host refuses to start.
    pub fn table(&self, name: &str) -> Result<Arc<[u8]>, RegistryError> {
        self.world
            .resource::<Tables>()
            .and_then(|t| t.tables.get(name).cloned())
            .ok_or_else(|| RegistryError::MissingTable(name.to_owned()))
    }

    /// Registers an Ops counter `<module key>.<name>`.
    pub fn metric(&mut self, name: &str) -> MetricId {
        let full = format!("{}.{name}", self.key);
        let base = self.world.resource::<Metrics>().map_or(0, |m| m.entries.len());
        self.metrics.push((full, 0));
        MetricId(base + self.metrics.len() - 1)
    }
}

/// A package's resolved modules and their linked server halves.
pub struct ModuleSet {
    graph: ModuleGraph,
    modules: Vec<Arc<dyn ServerModule>>,
    tables: Vec<(String, Arc<[u8]>)>,
}

impl ModuleSet {
    /// Pairs the resolved graph with the linked implementations (from the
    /// package's generated `modules.rs`). Linked modules outside the graph
    /// (overridden ones, for example) are left out.
    ///
    /// # Errors
    /// [`RegistryError::NotLinked`] when the graph names a module nothing
    /// implements.
    pub fn new(graph: ModuleGraph, linked: &[Arc<dyn ServerModule>]) -> Result<Self, RegistryError> {
        let mut modules = Vec::with_capacity(graph.modules.len());
        for m in &graph.modules {
            let found = linked
                .iter()
                .find(|l| l.key() == m.key)
                .ok_or_else(|| RegistryError::NotLinked(m.key.clone()))?;
            modules.push(Arc::clone(found));
        }
        Ok(Self {
            graph,
            modules,
            tables: Vec::new(),
        })
    }

    /// An empty set (a package with no modules).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            graph: ModuleGraph::default(),
            modules: Vec::new(),
            tables: Vec::new(),
        }
    }

    /// Supplies a content table.
    #[must_use]
    pub fn with_table(mut self, name: &str, bytes: Arc<[u8]>) -> Self {
        self.tables.push((name.to_owned(), bytes));
        self
    }

    /// The resolved graph.
    #[must_use]
    pub fn graph(&self) -> &ModuleGraph {
        &self.graph
    }

    /// Installs every module into a cell's world and schedule, in graph
    /// order, then applies the graph's enabled flags.
    ///
    /// # Errors
    /// The first [`RegistryError`].
    pub fn install(
        &self,
        world: &mut World,
        schedule: &mut Schedule,
        tick_rate: u32,
        seed: mantis_core::rng::Seed,
        catalog: &GraphCatalog,
    ) -> Result<Registry, RegistryError> {
        let mut registry = Registry::default();
        let mut tables = Tables::default();
        for (name, bytes) in &self.tables {
            tables.insert(name, Arc::clone(bytes));
        }
        world.insert_resource(tables)?;
        world.insert_resource(Metrics::default())?;
        world.insert_resource(Outbox::with_capacity(1024))?;
        world.insert_resource(SystemOutcomes::new())?;
        world.insert_resource(crate::service::ServiceOutbox::new())?;
        if world.resource::<Queries>().is_none() {
            world.insert_resource(Queries::default())?;
        }
        let mut states = ModuleStates::default();
        for (i, (module, resolved)) in self.modules.iter().zip(&self.graph.modules).enumerate() {
            let id = ModuleId(u16::try_from(i).map_err(|_| RegistryError::Module("too many modules"))?);
            let mut r = Registrar {
                module: id,
                key: module.key(),
                catalog,
                declared: &resolved.graph_actions,
                optional: &resolved.optional,
                registry: &mut registry,
                world,
                schedule,
                metrics: Vec::new(),
                flags: resolved.flags.clone(),
                tunables: Vec::new(),
                tick_rate,
                seed,
            };
            module.register(&mut r)?;
            for declared in &resolved.graph_actions {
                let registered = catalog
                    .action_id(declared)
                    .and_then(|a| r.registry.actions.get(usize::from(a.0)))
                    .is_some_and(|slot| slot.is_some_and(|(m, _)| m == id));
                if !registered {
                    return Err(RegistryError::UnregisteredAction(declared.clone()));
                }
            }
            let metrics = std::mem::take(&mut r.metrics);
            states.tunables.extend(std::mem::take(&mut r.tunables));
            for (name, on) in &resolved.flags {
                states.flags.insert(format!("{}.{name}", resolved.key), *on);
            }
            if let Some(m) = world.resource_mut::<Metrics>() {
                m.entries.extend(metrics);
            }
            states.keys.push(resolved.key.clone());
            states.enabled.push(resolved.enabled);
        }
        world.insert_resource(states)?;
        for m in &self.graph.modules {
            set_enabled(world, schedule, &m.key, m.enabled);
        }
        Ok(registry)
    }
}

/// The live value of a module flag, by full name (`<module key>.<flag>`).
/// Unknown flags read as false.
#[must_use]
pub fn flag(world: &World, full: &str) -> bool {
    world
        .resource::<ModuleStates>()
        .and_then(|s| s.flags.get(full).copied())
        .unwrap_or(false)
}

/// The live value of a tunable, by full name.
#[must_use]
pub fn tunable(world: &World, full: &str) -> Option<f32> {
    world
        .resource::<ModuleStates>()
        .and_then(|s| s.tunables.get(full).copied())
}

/// What a live change did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiveApplied {
    /// A module was switched on or off (clients are told).
    Module,
    /// A module flag changed.
    Flag,
    /// A tunable changed.
    Tunable,
}

/// The kind of a live change: a flag (a module key or `<key>.<flag>`).
pub const LIVE_FLAG: u8 = 0;
/// The kind of a live change: a tunable.
pub const LIVE_TUNABLE: u8 = 1;

/// Applies one live change (already verified by the host and logged as an
/// intent). A flag naming a module key switches the module; any other
/// flag or tunable must already exist.
///
/// # Errors
/// Why it was refused: the name is unknown or the value is not finite.
pub fn set_live(
    world: &mut World,
    schedule: &mut Schedule,
    name: &str,
    kind: u8,
    value: f32,
) -> Result<LiveApplied, &'static str> {
    if !value.is_finite() {
        return Err("live value not finite");
    }
    let on = value != 0.0;
    let states = world.resource_mut::<ModuleStates>().ok_or("no modules")?;
    match kind {
        LIVE_FLAG if states.id(name).is_some() => {
            set_enabled(world, schedule, name, on);
            Ok(LiveApplied::Module)
        }
        LIVE_FLAG => {
            let slot = states.flags.get_mut(name).ok_or("unknown flag")?;
            *slot = on;
            Ok(LiveApplied::Flag)
        }
        LIVE_TUNABLE => {
            let slot = states.tunables.get_mut(name).ok_or("unknown tunable")?;
            *slot = value;
            Ok(LiveApplied::Tunable)
        }
        _ => Err("unknown live kind"),
    }
}

/// Enables or disables a module everywhere: its handlers and commands (via
/// [`ModuleStates`]), its systems, and its queries.
pub fn set_enabled(world: &mut World, schedule: &mut Schedule, key: &str, enabled: bool) -> bool {
    let Some(states) = world.resource_mut::<ModuleStates>() else {
        return false;
    };
    let Some(id) = states.id(key) else {
        return false;
    };
    if let Some(slot) = states.enabled.get_mut(usize::from(id.0)) {
        *slot = enabled;
    }
    schedule.set_enabled_under(key, enabled);
    if let Some(q) = world.resource_mut::<Queries>() {
        q.set_enabled(key, enabled);
    }
    true
}
