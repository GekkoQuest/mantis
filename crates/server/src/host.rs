//! The network host (plan 7.3, 9): listeners (an adapter plus its transport),
//! session handshakes, the server's hand-written validators, and routing of
//! every inbound message to the cell that currently serves its session.
//!
//! Dispatch order for every frame: the adapter translates bytes into
//! contract messages, the validators check each one (adapters are never
//! trusted), the session's phase decides whether it is allowed at all, and
//! only then is it handed to a cell's inbox.

use std::collections::BTreeMap;
use std::sync::Arc;

use mantis_adapter_contract::core_types::{Angle16, Tick, ValidationError, Vec3};
use mantis_adapter_contract::{
    AppearanceId, Cast, Channel, Choose, ConnectionId, Extension, ExtensionRefusal, ExtensionRefused,
    Goodbye, Hello, Inbound, Interact, Linked, Move, MoveClaim, Outbound, Refuse, SnapshotAck, Transport,
    TransportEvent, TransportKind, Validators, Welcome, WireAdapter,
};
use mantis_core::log::SessionId;
use mantis_net::handshake::{ServerPolicy, negotiate};

use crate::cell::OutboundSink;
use crate::components::ReplicationIds;
use crate::intent::CellIntent;
use crate::lease::CharacterId;
use crate::modules::{ModuleCommand, Payload, Route};
use crate::zone::Zone;

/// The server's validators: semantic checks on every decoded message,
/// whatever adapter produced it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServerValidators;

/// Positions beyond this are refused at the boundary.
pub const WORLD_LIMIT: f32 = 1.0e6;

impl Validators for ServerValidators {
    fn validate_hello(&self, msg: &Hello) -> Result<(), ValidationError> {
        if msg.token.is_empty() {
            Err(ValidationError("empty session token"))
        } else {
            Ok(())
        }
    }
    fn validate_move(&self, _msg: &Move) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_move_claim(&self, msg: &MoveClaim) -> Result<(), ValidationError> {
        let p = msg.position;
        if p.x.abs() > WORLD_LIMIT || p.y.abs() > WORLD_LIMIT || p.z.abs() > WORLD_LIMIT {
            Err(ValidationError("position outside the world"))
        } else {
            Ok(())
        }
    }
    fn validate_cast(&self, _msg: &Cast) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_interact(&self, _msg: &Interact) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_choose(&self, msg: &Choose) -> Result<(), ValidationError> {
        if msg.option >= 64 {
            Err(ValidationError("choice option out of range"))
        } else {
            Ok(())
        }
    }
    /// Shape only: whether a module handles the kind is decided by the
    /// cell's registry, which answers unknown kinds with `Invalid`.
    fn validate_extension(&self, _msg: &Extension) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_snapshot_ack(&self, _msg: &SnapshotAck) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_goodbye(&self, _msg: &Goodbye) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_linked(&self, _msg: &Linked) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// One way in: an adapter and the transport its clients speak.
pub struct Listener {
    /// The adapter.
    pub adapter: Arc<dyn WireAdapter>,
    /// The transport.
    pub transport: Box<dyn Transport>,
}

/// Host settings.
pub struct HostConfig {
    /// Handshake policy.
    pub policy: ServerPolicy,
    /// Appearance given to new avatars.
    pub look: AppearanceId,
    /// Tick rate announced in `Welcome`.
    pub tick_rate: u16,
    /// Sessions accepted before refusing with `Full`.
    pub capacity: usize,
    /// Token check (the account service in production).
    pub token_ok: fn(&[u8]) -> bool,
    /// Where a new session's avatar spawns.
    pub spawn: fn(SessionId) -> Vec3,
}

/// Verifies session tokens off the tick: the account or realm role in a
/// cluster (lead ruling, M7). `begin` must not block; verdicts are
/// collected at the next host poll, which runs on a tick boundary.
pub trait Admission: Send {
    /// Starts verifying `token` for the handshake `ticket`.
    fn begin(&mut self, ticket: u64, token: &[u8]);
    /// Verdicts that arrived since the last call.
    fn ready(&mut self, out: &mut Vec<(u64, Verdict)>);
}

/// The answer to one admission.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Verdict {
    /// The token is good: the session plays as `character`.
    Admit {
        /// The character the token was issued for.
        character: u64,
        /// Where it enters: a returning character where it left the
        /// world; `None`, where the host spawns new sessions.
        spawn: Option<Vec3>,
    },
    /// The token is bad, used, or expired.
    Refuse,
}

/// Bounds on sessions waiting for their verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AdmissionLimits {
    /// Sessions verifying at once; more are refused with `Full`.
    pub max_verifying: usize,
    /// Host polls (ticks) to wait for a verdict; then the session is
    /// refused with `BadToken`. Never admitted on timeout.
    pub timeout_ticks: u64,
}

impl AdmissionLimits {
    /// 256 at once, 5 seconds at 30 Hz.
    pub const DEFAULT: Self = Self {
        max_verifying: 256,
        timeout_ticks: 150,
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Handshaking,
    /// Waiting for the admission verdict: holds no cell resources.
    Verifying {
        deadline: u64,
        protocol: u16,
        capabilities: u32,
        mode: mantis_adapter_contract::MovementMode,
    },
    InWorld,
}

struct HostSession {
    id: SessionId,
    phase: Phase,
    limiter: crate::limits::SessionLimiter,
    /// The client modules the handshake declared (for Ops; never trusted).
    modules: Vec<mantis_adapter_contract::ModuleEntry>,
}

/// Host counters (readable by Ops).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HostStats {
    /// Sessions that entered the world.
    pub joined: u64,
    /// Messages an adapter refused to encode (a value its protocol cannot
    /// carry); nothing was sent for them.
    pub encode_refused: u64,
    /// Joins refused because the next replicated id is outside the range
    /// some listening adapter can carry ([`Host::entity_ids`]).
    pub ids_exhausted: u64,
    /// Client modules declared by accepted handshakes.
    pub client_modules: u64,
    /// Handshakes refused.
    pub refused_handshakes: u64,
    /// Frames the adapter could not translate.
    pub adapter_errors: u64,
    /// Messages refused by a validator.
    pub invalid: u64,
    /// Messages not allowed in the session's phase.
    pub out_of_phase: u64,
    /// Messages dropped because the cell's inbox was full.
    pub inbox_full: u64,
    /// Sessions refused because their verdict did not arrive in time.
    pub admission_timeouts: u64,
    /// Sessions that started verifying.
    pub verifying_started: u64,
    /// Sessions ended by Ops (kick or drain).
    pub kicked: u64,
    /// Messages and frames refused by the rate limits.
    pub rate_limited: u64,
    /// Sessions ended for exceeding the rate limits.
    pub limit_kicks: u64,
}

/// The network host.
pub struct Host {
    cfg: HostConfig,
    listeners: Vec<Listener>,
    sessions: BTreeMap<(usize, ConnectionId), HostSession>,
    next_session: u64,
    repl_ids: ReplicationIds,
    pending: Vec<(usize, ConnectionId, Option<Vec<u8>>, bool)>,
    scratch: Vec<u8>,
    admission: Option<(Box<dyn Admission>, AdmissionLimits)>,
    limits: crate::limits::RateLimits,
    verdicts: Vec<(u64, Verdict)>,
    polls: u64,
    /// Counters.
    pub stats: HostStats,
    /// The poll after a clock slip, and the ticks slipped.
    slip: (u64, u64),
}

impl Host {
    /// The replicated ids every listening adapter can carry: the
    /// intersection of their declared ranges.
    #[must_use]
    pub fn entity_ids(&self) -> mantis_adapter_contract::EntityIdRange {
        self.listeners
            .iter()
            .fold(mantis_adapter_contract::EntityIdRange::ALL, |r, l| {
                r.intersect(l.adapter.entity_ids())
            })
    }

    /// The host's clock slipped `ticks` behind wall time (a stall, or an
    /// overrun it re-anchored after): the time passed still counts for the
    /// rate limits and admission timeouts, so what clients sent meanwhile is
    /// judged at their real rate, not as one burst.
    pub fn clock_slipped(&mut self, ticks: u64) {
        self.polls = self.polls.saturating_add(ticks);
        // The next poll reads the backlog: its buckets hold the slip too.
        self.slip = (self.polls + 1, ticks);
    }

    /// Starts replicated id allocation at index `next` (tests of the id
    /// range; the default starts at 0).
    pub fn start_ids_at(&mut self, next: u32) {
        self.repl_ids = ReplicationIds::starting_at(next);
    }

    /// A host over `listeners`. A listener's index is its adapter index in
    /// every cell, so cells must be built with the adapters in this order.
    #[must_use]
    pub fn new(cfg: HostConfig, listeners: Vec<Listener>) -> Self {
        Self {
            cfg,
            listeners,
            sessions: BTreeMap::new(),
            next_session: 1,
            repl_ids: ReplicationIds::default(),
            pending: Vec::new(),
            scratch: Vec::with_capacity(256),
            admission: None,
            limits: crate::limits::RateLimits::DEFAULT,
            verdicts: Vec::new(),
            polls: 0,
            stats: HostStats::default(),
            slip: (0, 0),
        }
    }

    /// Verifies tokens through `admission` instead of `token_ok` alone (which
    /// still runs first, as a shape check): a session that passed the
    /// handshake waits, holding no cell resources, until its verdict
    /// arrives or `limits.timeout_ticks` polls pass.
    #[must_use]
    pub fn with_admission(mut self, admission: Box<dyn Admission>, limits: AdmissionLimits) -> Self {
        self.admission = Some((admission, limits));
        self
    }

    /// Sets the per-session rate limits (the package's `[tunables.limits]`).
    pub fn set_limits(&mut self, limits: crate::limits::RateLimits) {
        self.limits = limits;
    }

    /// A session's refused messages so far (Ops), by session id.
    #[must_use]
    pub fn violations(&self, session: SessionId) -> Option<u32> {
        self.sessions
            .values()
            .find(|s| s.id == session)
            .map(|s| s.limiter.violations)
    }

    /// The client modules `session` declared in its handshake, with their
    /// hashes (informational: a client can lie; the permitted list is what
    /// the server enforces).
    #[must_use]
    pub fn client_modules(&self, session: SessionId) -> Option<&[mantis_adapter_contract::ModuleEntry]> {
        self.sessions
            .values()
            .find(|s| s.id == session)
            .map(|s| s.modules.as_slice())
    }

    /// [`Host::with_admission`] on a built host.
    pub fn set_admission(&mut self, admission: Box<dyn Admission>, limits: AdmissionLimits) {
        self.admission = Some((admission, limits));
    }

    /// Refuses new sessions with `Maintenance` (a drain), or admits again.
    pub fn set_maintenance(&mut self, on: bool) {
        self.cfg.policy.maintenance = on;
    }

    /// True while new sessions are refused for maintenance.
    #[must_use]
    pub fn in_maintenance(&self) -> bool {
        self.cfg.policy.maintenance
    }

    /// Every session in the world.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionId> {
        self.sessions
            .values()
            .filter(|s| s.phase == Phase::InWorld)
            .map(|s| s.id)
            .collect()
    }

    /// Ends `session` (Ops kick or drain): its connection is closed and its
    /// avatar leaves its cell on the next tick, as if it had said goodbye.
    /// False when no such session exists.
    pub fn kick(&mut self, session: SessionId, zone: &mut Zone) -> bool {
        let Some(&(li, conn)) = self
            .sessions
            .iter()
            .find(|(_, s)| s.id == session)
            .map(|(k, _)| k)
        else {
            return false;
        };
        self.leave(li, conn, zone);
        if let Some(l) = self.listeners.get_mut(li) {
            l.transport.disconnect(conn);
        }
        self.stats.kicked += 1;
        true
    }

    /// Sessions waiting for their admission verdict.
    #[must_use]
    pub fn sessions_verifying(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| matches!(s.phase, Phase::Verifying { .. }))
            .count()
    }

    /// The adapters, in listener order (pass them to every cell).
    #[must_use]
    pub fn adapters(&self) -> Vec<Arc<dyn WireAdapter>> {
        self.listeners.iter().map(|l| Arc::clone(&l.adapter)).collect()
    }

    /// Sessions currently in the world.
    #[must_use]
    pub fn sessions_in_world(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.phase == Phase::InWorld)
            .count()
    }

    /// Polls every listener and routes everything received, then applies
    /// admission verdicts and timeouts. Call once per tick, before the zone
    /// steps: admissions take effect on that tick boundary.
    pub fn poll(&mut self, zone: &mut Zone) {
        self.polls += 1;
        self.pending.clear();
        for (li, l) in self.listeners.iter_mut().enumerate() {
            let pending = &mut self.pending;
            l.transport.poll(&mut |e| match e {
                TransportEvent::Connected(c) => pending.push((li, c, None, true)),
                TransportEvent::Frame { conn, bytes, .. } => {
                    pending.push((li, conn, Some(bytes.to_vec()), true));
                }
                TransportEvent::Disconnected { conn, .. } => pending.push((li, conn, None, false)),
            });
        }
        let pending = std::mem::take(&mut self.pending);
        for (li, conn, frame, alive) in &pending {
            match (frame, alive) {
                (None, true) => {
                    let id = SessionId(self.next_session);
                    self.next_session += 1;
                    self.sessions.insert(
                        (*li, *conn),
                        HostSession {
                            id,
                            phase: Phase::Handshaking,
                            limiter: crate::limits::SessionLimiter::default(),
                            modules: Vec::new(),
                        },
                    );
                }
                (None, false) => self.leave(*li, *conn, zone),
                (Some(bytes), _) => self.frame(*li, *conn, bytes, zone),
            }
        }
        self.pending = pending;
        self.settle(zone);
        self.report_violations(zone);
    }

    /// Tells each cell how many of its sessions' messages were refused
    /// this poll (a logged intent onto the session's cheat counter), and
    /// ends sessions past the package's threshold.
    fn report_violations(&mut self, zone: &mut Zone) {
        let mut over = Vec::new();
        for (&(li, conn), s) in &mut self.sessions {
            if s.limiter.unreported == 0 {
                continue;
            }
            let count = std::mem::take(&mut s.limiter.unreported);
            if s.phase == Phase::InWorld
                && let Some(c) = zone.route(s.id).and_then(|r| zone.cell_mut(r))
            {
                let _ = c.inbox().push(s.id, CellIntent::Throttled { count });
            }
            if s.limiter.violations >= self.limits.kick_after {
                over.push((li, conn, s.id));
            }
        }
        for (li, conn, id) in over {
            self.stats.limit_kicks += 1;
            if self
                .sessions
                .get(&(li, conn))
                .is_some_and(|s| s.phase == Phase::InWorld)
            {
                self.kick(id, zone);
            } else {
                self.refuse(li, conn, mantis_adapter_contract::RefuseReason::BadToken);
            }
        }
    }

    /// Applies verdicts that arrived and refuses sessions past their
    /// deadline (fail closed).
    fn settle(&mut self, zone: &mut Zone) {
        let Some((admission, _)) = self.admission.as_mut() else {
            return;
        };
        let mut verdicts = std::mem::take(&mut self.verdicts);
        verdicts.clear();
        admission.ready(&mut verdicts);
        for (ticket, verdict) in &verdicts {
            let Some((&(li, conn), s)) = self.sessions.iter().find(|(_, s)| s.id.0 == *ticket) else {
                continue;
            };
            let Phase::Verifying {
                protocol,
                capabilities,
                mode,
                ..
            } = s.phase
            else {
                continue;
            };
            let id = s.id;
            match verdict {
                Verdict::Admit { character, spawn } => {
                    if self.sessions_in_world() >= self.cfg.capacity {
                        self.refuse(li, conn, mantis_adapter_contract::RefuseReason::Full);
                    } else {
                        let entry = (*character, *spawn);
                        self.admit(li, conn, id, entry, (protocol, capabilities), mode, zone);
                    }
                }
                Verdict::Refuse => self.refuse(li, conn, mantis_adapter_contract::RefuseReason::BadToken),
            }
        }
        self.verdicts = verdicts;
        let late: Vec<(usize, ConnectionId)> = self
            .sessions
            .iter()
            .filter(|(_, s)| matches!(s.phase, Phase::Verifying { deadline, .. } if deadline <= self.polls))
            .map(|(k, _)| *k)
            .collect();
        for (li, conn) in late {
            self.stats.admission_timeouts += 1;
            self.refuse(li, conn, mantis_adapter_contract::RefuseReason::BadToken);
        }
    }

    fn refuse(&mut self, li: usize, conn: ConnectionId, reason: mantis_adapter_contract::RefuseReason) {
        self.stats.refused_handshakes += 1;
        self.reply(li, conn, &Outbound::Refuse(Refuse { reason }));
        if let Some(l) = self.listeners.get_mut(li) {
            l.transport.disconnect(conn);
        }
        self.sessions.remove(&(li, conn));
    }

    fn frame(&mut self, li: usize, conn: ConnectionId, bytes: &[u8], zone: &mut Zone) {
        let Some(adapter) = self.listeners.get(li).map(|l| Arc::clone(&l.adapter)) else {
            return;
        };
        let (poll, rate, limits) = (self.polls, u32::from(self.cfg.tick_rate), self.limits);
        let Some(s) = self.sessions.get_mut(&(li, conn)) else {
            return;
        };
        if !s.limiter.bytes(bytes.len(), poll, rate, &limits) {
            self.stats.rate_limited += 1;
            return;
        }
        let mut msgs = Vec::new();
        if adapter.decode(bytes, &mut |m| msgs.push(m)).is_err() {
            self.stats.adapter_errors += 1;
            return;
        }
        for m in msgs {
            if m.validate(&ServerValidators).is_err() {
                self.stats.invalid += 1;
                continue;
            }
            self.message(li, conn, &m, adapter.as_ref(), zone);
        }
    }

    fn reply(&mut self, li: usize, conn: ConnectionId, msg: &Outbound) {
        let Some(l) = self.listeners.get_mut(li) else {
            return;
        };
        self.scratch.clear();
        match l.adapter.encode_outbound(msg, &mut self.scratch) {
            Ok(()) => {
                let _ = l.transport.send(conn, Channel::Reliable, &self.scratch);
            }
            Err(mantis_adapter_contract::AdapterError::Unsupported(_)) => {}
            // A value the protocol cannot carry: refused whole, counted.
            Err(_) => self.stats.encode_refused += 1,
        }
    }

    fn message(
        &mut self,
        li: usize,
        conn: ConnectionId,
        m: &Inbound,
        adapter: &dyn WireAdapter,
        zone: &mut Zone,
    ) {
        let (poll, rate, limits) = (self.polls, u32::from(self.cfg.tick_rate), self.limits);
        let slipped = if self.slip.0 == poll { self.slip.1 } else { 0 };
        let Some(s) = self.sessions.get_mut(&(li, conn)) else {
            return;
        };
        if let Some(kind) = crate::limits::Kind::of(m)
            && !s.limiter.admit(kind, poll, rate, &limits, slipped)
        {
            self.stats.rate_limited += 1;
            return;
        }
        let (id, phase) = (s.id, s.phase);
        match (phase, *m) {
            (Phase::Handshaking, Inbound::Hello(h)) => self.hello(li, conn, id, &h, adapter, zone),
            (Phase::InWorld, Inbound::Goodbye(_)) => self.leave(li, conn, zone),
            (Phase::InWorld, Inbound::SnapshotAck(a)) => {
                if let Some(c) = zone.route(id).and_then(|r| zone.cell_mut(r)) {
                    c.inbox().ack(id, a.tick);
                }
            }
            (
                Phase::InWorld,
                m @ (Inbound::Move(_)
                | Inbound::MoveClaim(_)
                | Inbound::Cast(_)
                | Inbound::Interact(_)
                | Inbound::Choose(_)),
            ) => {
                let intent = match m {
                    Inbound::Move(mv) => CellIntent::Move(mv.input),
                    Inbound::MoveClaim(c) => CellIntent::MoveClaim {
                        position: c.position,
                        client_time_ms: c.client_time_ms,
                    },
                    Inbound::Cast(c) => CellIntent::Cast(c),
                    Inbound::Interact(i) => CellIntent::Interact(i),
                    Inbound::Choose(c) => CellIntent::Choose(c),
                    _ => return,
                };
                match zone.route(id).and_then(|r| zone.cell_mut(r)) {
                    Some(c) => {
                        if !c.inbox().push(id, intent) {
                            self.stats.inbox_full += 1;
                        }
                    }
                    None => self.stats.out_of_phase += 1,
                }
            }
            (Phase::InWorld, Inbound::Extension(x)) => self.extension(li, conn, id, &x, zone),
            _ => self.stats.out_of_phase += 1,
        }
    }

    /// Routes an extension to the module that registered its kind: an
    /// intent to the cell inbox, an economy command to the command inbox.
    fn extension(&mut self, li: usize, conn: ConnectionId, id: SessionId, x: &Extension, zone: &mut Zone) {
        let Some(cell) = zone.route(id).and_then(|r| zone.cell_mut(r)) else {
            self.stats.out_of_phase += 1;
            return;
        };
        let payload = Payload::from_bytes(x.payload.iter().copied());
        let queued = match (cell.extension_route(x.kind), payload) {
            (Some(Route::Intent), Some(payload)) => cell.inbox().push(
                id,
                CellIntent::Extension {
                    kind: x.kind,
                    request: x.request,
                    payload,
                },
            ),
            (Some(Route::Command), Some(payload)) => cell.commands().push(ModuleCommand {
                kind: x.kind,
                session: Some(id),
                request: x.request,
                payload,
            }),
            _ => {
                self.stats.invalid += 1;
                let refusal = ExtensionRefused {
                    kind: x.kind,
                    request: x.request,
                    reason: ExtensionRefusal::Invalid,
                };
                self.reply(li, conn, &Outbound::ExtensionRefused(refusal));
                return;
            }
        };
        if !queued {
            self.stats.inbox_full += 1;
        }
    }

    fn hello(
        &mut self,
        li: usize,
        conn: ConnectionId,
        id: SessionId,
        h: &Hello,
        adapter: &dyn WireAdapter,
        zone: &mut Zone,
    ) {
        let full = self.sessions_in_world() >= self.cfg.capacity;
        let accepted = match negotiate(h, &self.cfg.policy, self.cfg.token_ok, full) {
            Ok(a) => a,
            Err(reason) => {
                self.refuse(li, conn, reason);
                return;
            }
        };
        let accepted = (accepted.protocol, accepted.capabilities);
        if let Some(s) = self.sessions.get_mut(&(li, conn)) {
            s.modules = h.modules.iter().copied().collect();
            self.stats.client_modules += s.modules.len() as u64;
        }
        let verifying = self.sessions_verifying();
        let polls = self.polls;
        if let Some((admission, limits)) = self.admission.as_mut() {
            if verifying >= limits.max_verifying {
                self.refuse(li, conn, mantis_adapter_contract::RefuseReason::Full);
                return;
            }
            let token: Vec<u8> = h.token.iter().copied().collect();
            admission.begin(id.0, &token);
            let deadline = polls + limits.timeout_ticks.max(1);
            self.stats.verifying_started += 1;
            if let Some(s) = self.sessions.get_mut(&(li, conn)) {
                s.phase = Phase::Verifying {
                    deadline,
                    protocol: accepted.0,
                    capabilities: accepted.1,
                    mode: adapter.movement_mode(),
                };
            }
            return;
        }
        self.admit(
            li,
            conn,
            id,
            (id.0, None),
            accepted,
            adapter.movement_mode(),
            zone,
        );
    }

    /// Enters an admitted session into the world as `character`, at
    /// `spawn` when given (a returning character), else where the host
    /// spawns new sessions.
    #[expect(clippy::too_many_arguments, reason = "one call site per admission path")]
    fn admit(
        &mut self,
        li: usize,
        conn: ConnectionId,
        id: SessionId,
        (character, spawn): (u64, Option<Vec3>),
        accepted: (u16, u32),
        mode: mantis_adapter_contract::MovementMode,
        zone: &mut Zone,
    ) {
        // Inside every listening adapter's range: no client is ever handed
        // an id its protocol cannot carry (fail closed, counted, and told).
        let Some(repl) = self.repl_ids.allocate_within(self.entity_ids()) else {
            self.stats.ids_exhausted += 1;
            self.refuse(li, conn, mantis_adapter_contract::RefuseReason::Full);
            return;
        };
        let spawn = spawn.unwrap_or_else(|| (self.cfg.spawn)(id));
        let Some(cell_index) = zone.cell_for(spawn.x) else {
            self.stats.refused_handshakes += 1;
            return;
        };
        let Ok(epoch) = zone.register(id, CharacterId(character), cell_index) else {
            self.stats.refused_handshakes += 1;
            return;
        };
        let implicit_ack = self
            .listeners
            .get(li)
            .is_some_and(|l| l.transport.kind() == TransportKind::Tcp);
        let tick = zone
            .cells()
            .get(cell_index)
            .map_or(Tick::ZERO, crate::cell::Cell::tick_now);
        if let Some(c) = zone.cell_mut(cell_index) {
            c.inbox().push(
                id,
                CellIntent::Join {
                    repl,
                    spawn,
                    yaw: Angle16(0),
                    look: self.cfg.look,
                    mode,
                    epoch,
                    character,
                },
            );
            if c.attach_client(id, conn, li, implicit_ack).is_err() {
                self.stats.refused_handshakes += 1;
                return;
            }
        }
        if let Some(s) = self.sessions.get_mut(&(li, conn)) {
            s.phase = Phase::InWorld;
        }
        self.stats.joined += 1;
        self.reply(
            li,
            conn,
            &Outbound::Welcome(Welcome {
                protocol: accepted.0,
                capabilities: accepted.1,
                session: id.0,
                tick,
                tick_rate: self.cfg.tick_rate,
                mode,
                avatar: Some(repl.0),
                character,
            }),
        );
    }

    fn leave(&mut self, li: usize, conn: ConnectionId, zone: &mut Zone) {
        let Some(s) = self.sessions.remove(&(li, conn)) else {
            return;
        };
        if s.phase == Phase::InWorld
            && let Some(r) = zone.route(s.id)
            && let Some(c) = zone.cell_mut(r)
        {
            c.inbox().push(s.id, CellIntent::Leave);
            c.detach_client(s.id);
        }
    }
}

impl OutboundSink for Host {
    fn send(&mut self, adapter: usize, conn: ConnectionId, channel: Channel, bytes: &[u8]) {
        if let Some(l) = self.listeners.get_mut(adapter) {
            let _ = l.transport.send(conn, channel, bytes);
        }
    }
}
