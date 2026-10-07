//! The native protocol session (plan 9, decision 0011): the network side of a world
//! session, over any [`Transport`] (QUIC in production, the deterministic simulated
//! network in tests).
//!
//! [`NativeSession::step`] runs on the network thread:
//! 1. polls the transport; every server frame is decoded with the engine's native codec
//!    against a ring of recently applied snapshots (delta baselines). A snapshot newer
//!    than the last applied one is kept as a baseline, acknowledged with `SnapshotAck`
//!    (only applied snapshots are acknowledged, so the server only ever deltas against a
//!    frame this client holds), converted into the simulation's
//!    [`crate::snapshot::SnapshotFrame`], and handed to the simulation thread;
//! 2. sends the move intents the simulation queued ([`MoveOutbox`]), each together with a
//!    repeat of the previous one so a single lost datagram never costs the server a step.
//!
//! Session messages (`Welcome`, `Refuse`, `SetPosition`) update [`SessionState`].
//! Everything after construction is allocation-free.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use mantis_adapter_contract::native::{ServerFrame, decode_server_frame, encode_inbound};
use mantis_adapter_contract::{
    Channel, ConnectionId, Extension, ExtensionKind, ExtensionMessage, ExtensionRefusal, FeatureState, Hello,
    Inbound, Move, Outbound, PROTOCOL_VERSION, PermittedModules, RefuseReason, RemoteSample, SnapshotAck,
    SnapshotFrame as WireFrame, Transport, TransportEvent,
};
use mantis_core::content::ContentHash;
use mantis_core::ecs::EntityId;
use mantis_core::kinematics::{MotionState, MoveInput};
use mantis_core::time::Tick;
use mantis_core::wire::BoundedArray;

use crate::sim::IntentSink;
use crate::snapshot::{RemoteState, SnapshotFrame, SnapshotSender};
use crate::time::HostClock;

/// The server as a client sees it.
const SERVER: ConnectionId = ConnectionId(0);

/// Sim-thread end of the move queue: an [`IntentSink`] that never blocks.
#[derive(Debug)]
pub struct MoveOutbox {
    tx: SyncSender<MoveInput>,
    dropped: Arc<AtomicU64>,
}

/// Network-thread end of the move queue.
#[derive(Debug)]
pub struct MoveInbox {
    rx: Receiver<MoveInput>,
    dropped: Arc<AtomicU64>,
}

/// A bounded move queue holding `capacity` inputs.
pub fn move_channel(capacity: usize) -> (MoveOutbox, MoveInbox) {
    let (tx, rx) = sync_channel(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        MoveOutbox {
            tx,
            dropped: Arc::clone(&dropped),
        },
        MoveInbox { rx, dropped },
    )
}

impl IntentSink for MoveOutbox {
    fn send_move(&mut self, input: &MoveInput) {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(*input) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl MoveInbox {
    /// Inputs dropped because the network thread fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Where the session is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionState {
    /// `Hello` sent, no answer yet.
    Connecting,
    /// Accepted; the avatar, once the server has spawned it.
    Welcomed {
        /// The session's avatar.
        avatar: Option<EntityId>,
        /// The session's persistent character identity.
        character: u64,
    },
    /// Refused at the handshake.
    Refused(RefuseReason),
    /// The transport reported the connection closed.
    Closed,
}

/// Session tuning.
#[derive(Clone, Copy, Debug)]
pub struct NetConfig {
    /// The client's gameplay content hash (must equal the server's).
    pub content: ContentHash,
    /// Snapshots kept as delta baselines (the server deltas against acknowledged ones).
    pub baselines: usize,
    /// Capacities of one decoded frame: entered, remotes, removed, markers.
    pub frame_capacity: [usize; 4],
    /// Send every move twice (current and previous), as the native bots do.
    pub repeat_moves: bool,
}

impl NetConfig {
    /// Defaults for `content`: 64 baselines, frames of 64 entered, 256 remotes, 64
    /// removed, 64 markers, repeated moves. 64 baselines keep every applied snapshot of
    /// the last 64 server ticks (one snapshot per tick), the window the encoder may
    /// reference a remote's own delta baseline in (per-remote baselines, M13).
    pub fn new(content: ContentHash) -> Self {
        Self {
            content,
            baselines: 64,
            frame_capacity: [64, 256, 64, 64],
            repeat_moves: true,
        }
    }
}

/// Counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NetStats {
    /// Snapshots decoded and applied.
    pub snapshots: u64,
    /// Snapshots older than the newest applied (dropped).
    pub stale: u64,
    /// Frames that failed to decode (corrupt, unknown, or missing baseline).
    pub undecodable: u64,
    /// Snapshots the simulation had no free frame for.
    pub sim_full: u64,
    /// Items dropped because a frame was full.
    pub overflowed: u64,
    /// Moves sent (repeats included).
    pub moves_sent: u64,
    /// Sends the transport refused.
    pub send_errors: u64,
    /// Authoritative position resets (`SetPosition`).
    pub position_resets: u64,
    /// Permitted-module lists received.
    pub permitted_updates: u64,
    /// Package intent extensions the server refused (`ExtensionRefused`).
    pub extensions_refused: u64,
    /// Module-to-client messages received (`ExtensionMessage`).
    pub extension_messages: u64,
    /// Module-to-client messages dropped because the queue was full.
    pub extension_messages_dropped: u64,
    /// Module client-to-server messages sent.
    pub extensions_sent: u64,
    /// Cell hand-offs ([`NativeSession::rebase`]).
    pub rebases: u64,
    /// Reconnects ([`NativeSession::reconnect`]).
    pub reconnects: u64,
    /// Server messages this build does not know, ignored.
    pub unknown_messages: u64,
    /// Moves not sent because the session was not accepted yet (before `Welcome`, or
    /// while reconnecting).
    pub moves_unsent: u64,
}

/// One native-protocol session.
pub struct NativeSession<T: Transport> {
    transport: T,
    clock: Arc<dyn HostClock>,
    config: NetConfig,
    ring: Ring,
    /// Bumped at every [`NativeSession::rebase`] (hand-off) and reconnect.
    epoch: u32,
    /// Bumped at every [`NativeSession::reconnect`].
    connection: u32,
    /// The first move sent on this connection (stamped on frames: prediction resumes from
    /// it after a reconnect).
    first_move: Option<mantis_core::kinematics::InputSeq>,
    /// The client mods announced in `Hello`, kept for reconnects.
    hello_modules: Vec<mantis_adapter_contract::ModuleEntry>,
    scratch: WireFrame,
    last_applied: Option<Tick>,
    pending_acks: Vec<Tick>,
    refusals: Vec<(ExtensionKind, u32, ExtensionRefusal)>,
    /// The `request` of the last extension sent (never 0 once one was sent).
    last_request: u32,
    extensions: Vec<ExtensionMessage>,
    entity_changes: Vec<crate::modules::EntityChange>,
    features: Vec<FeatureState>,
    /// The latest permitted-module list and tier the cell sent (`None` until one arrives).
    permitted: Option<PermittedModules>,
    /// Whether `permitted` changed since [`NativeSession::take_permitted`] last ran.
    permitted_changed: bool,
    snapshots: SnapshotSender<MotionState>,
    moves: MoveInbox,
    previous: Option<MoveInput>,
    out: Vec<u8>,
    state: SessionState,
    stats: NetStats,
}

impl<T: Transport> core::fmt::Debug for NativeSession<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeSession")
            .field("state", &self.state)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> NativeSession<T> {
    /// A session over `transport` feeding `snapshots` and sending from `moves`.
    pub fn new(
        transport: T,
        clock: Arc<dyn HostClock>,
        config: NetConfig,
        snapshots: SnapshotSender<MotionState>,
        moves: MoveInbox,
    ) -> Self {
        let [entered, remotes, removed, markers] = config.frame_capacity;
        let frame = || WireFrame::with_capacity(entered, remotes, removed, markers);
        Self {
            ring: Ring {
                frames: (0..config.baselines.max(1)).map(|_| frame()).collect(),
                live: vec![false; config.baselines.max(1)],
            },
            epoch: 0,
            connection: 0,
            first_move: None,
            hello_modules: Vec::new(),
            scratch: frame(),
            last_applied: None,
            pending_acks: Vec::with_capacity(64),
            refusals: Vec::with_capacity(16),
            last_request: 0,
            extensions: Vec::with_capacity(64),
            entity_changes: Vec::with_capacity(256),
            features: Vec::with_capacity(64),
            permitted: None,
            permitted_changed: false,
            snapshots,
            moves,
            previous: None,
            out: Vec::with_capacity(2048),
            state: SessionState::Connecting,
            stats: NetStats::default(),
            transport,
            clock,
            config,
        }
    }

    /// The modules and tier the cell permits client mods, as last sent (on joining a
    /// cell, after a transfer, and on every tier change).
    pub fn permitted(&self) -> Option<&PermittedModules> {
        self.permitted.as_ref()
    }

    /// The permitted list if it changed since the last call (client mod loading applies
    /// it).
    pub fn take_permitted(&mut self) -> Option<&PermittedModules> {
        if core::mem::take(&mut self.permitted_changed) {
            self.permitted.as_ref()
        } else {
            None
        }
    }

    /// Where the session is.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Counters.
    pub fn stats(&self) -> NetStats {
        self.stats
    }

    /// Hands every refused extension since the last call to `f` (the UI shows a disabled
    /// or refused feature as such, never as a dead control). Allocation-free.
    pub fn drain_refusals(&mut self, mut f: impl FnMut(ExtensionKind, ExtensionRefusal)) {
        for (kind, _request, reason) in self.refusals.drain(..) {
            f(kind, reason);
        }
    }

    /// Hands every module-to-client message since the last call to `f`, in arrival order,
    /// for routing by kind to the module client that owns it. Allocation-free.
    pub fn drain_extensions(&mut self, mut f: impl FnMut(&ExtensionMessage)) {
        for m in self.extensions.drain(..) {
            f(&m);
        }
    }

    /// Hands every entity that entered or left interest since the last call to `f` (the
    /// change sets module view models observe). Allocation-free.
    pub fn drain_entity_changes(&mut self, mut f: impl FnMut(&crate::modules::EntityChange)) {
        for c in self.entity_changes.drain(..) {
            f(&c);
        }
    }

    /// Hands every module feature state the server announced since the last call to `f`
    /// (sent when the session joins a cell and when Ops switches a module).
    pub fn drain_feature_states(&mut self, mut f: impl FnMut(&str, bool)) {
        for s in self.features.drain(..) {
            f(s.module.as_str(), s.enabled);
        }
    }

    /// Like [`NativeSession::drain_refusals`], with the `request` of the refused send
    /// ([`NativeSession::last_request`] at the time it was sent; 0 when untracked).
    pub fn drain_tracked_refusals(&mut self, mut f: impl FnMut(ExtensionKind, u32, ExtensionRefusal)) {
        for (kind, request, reason) in self.refusals.drain(..) {
            f(kind, request, reason);
        }
    }

    /// The `request` the last [`NativeSession::send_extension`] carried: every send gets
    /// the next non-zero number, which the server echoes in any refusal of that send.
    pub fn last_request(&self) -> u32 {
        self.last_request
    }

    /// Sends a module's client-to-server message as an `Extension` intent (reliable).
    /// Returns false for a payload over the contract's limit.
    pub fn send_extension(&mut self, kind: ExtensionKind, payload: &[u8]) -> bool {
        let Some(payload) = BoundedArray::from_slice(payload) else {
            return false;
        };
        self.last_request = self.last_request.wrapping_add(1).max(1);
        let request = self.last_request;
        self.send(
            &Inbound::Extension(Extension {
                kind,
                request,
                payload,
            }),
            Channel::Reliable,
        );
        self.stats.extensions_sent += 1;
        true
    }

    /// The transport (for tests and diagnostics).
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    fn send(&mut self, msg: &Inbound, channel: Channel) {
        self.out.clear();
        encode_inbound(msg, &mut self.out);
        if self.transport.send(SERVER, channel, &self.out).is_err() {
            self.stats.send_errors += 1;
        }
    }

    /// Opens the session with `Hello` carrying `token` and no client mods.
    pub fn start(&mut self, token: &[u8]) {
        self.start_with_modules(token, &[]);
    }

    /// A cell hand-off: the connection stays up, and the next frames come from another
    /// host that counts its own ticks and shares no baselines with the last one. Clears the
    /// baseline ring, the stale filter, and pending acknowledgements, and bumps the epoch
    /// the simulation sees with the next frame (it restarts its own stale filter and its
    /// server timeline).
    pub fn rebase(&mut self) {
        self.ring.clear();
        self.last_applied = None;
        self.pending_acks.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.stats.rebases += 1;
    }

    /// Reconnects over a new `transport` (to the gateway again) and opens the session
    /// with `token` (a resume ticket, or a fresh entry token) and the same client mods as
    /// the first `Hello`. Everything per connection starts over: the baselines, the stale
    /// filter, the move repeat, the session state; the simulation sees a new epoch and a
    /// new connection with the next frame and resets prediction to the authoritative
    /// state, as at spawn.
    pub fn reconnect(&mut self, transport: T, token: &[u8]) {
        self.transport = transport;
        self.rebase();
        self.connection = self.connection.wrapping_add(1);
        self.first_move = None;
        self.previous = None;
        self.state = SessionState::Connecting;
        self.stats.reconnects += 1;
        let modules = core::mem::take(&mut self.hello_modules);
        self.start_with_modules(token, &modules);
    }

    /// Opens the session with `Hello` carrying `token` and the client mods this client
    /// runs, with their content hashes ([`crate::mods::ModHost::hello_modules`]). The
    /// server refuses a key the package does not permit; the hashes are for
    /// compatibility and support, never trust. At most 32 are sent (the contract's bound).
    pub fn start_with_modules(&mut self, token: &[u8], modules: &[mantis_adapter_contract::ModuleEntry]) {
        modules.clone_into(&mut self.hello_modules);
        let mut list = BoundedArray::new();
        for m in modules {
            if list.push(*m).is_err() {
                break;
            }
        }
        let hello = Inbound::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: 0,
            content: self.config.content,
            modules: list,
            token: BoundedArray::from_slice(token).unwrap_or_default(),
        });
        self.send(&hello, Channel::Reliable);
    }

    /// Handles everything received, acknowledges applied snapshots, then sends queued
    /// moves.
    pub fn step(&mut self) {
        self.receive();
        let acks = core::mem::take(&mut self.pending_acks);
        for tick in &acks {
            self.send(
                &Inbound::SnapshotAck(SnapshotAck { tick: *tick }),
                Channel::Unreliable,
            );
        }
        self.pending_acks = acks;
        self.pending_acks.clear();
        self.send_moves();
    }

    /// Sends every queued move (each followed by a repeat of the previous one when
    /// configured). Call after the simulation tick so new moves leave without a tick's
    /// delay.
    pub fn send_moves(&mut self) {
        while let Ok(input) = self.moves.rx.try_recv() {
            // Until the session is accepted, a move reaches no session: the server would drop
            // it. Dropping it here keeps the first move the server applies on a connection
            // the first one recorded for it.
            if !matches!(self.state, SessionState::Welcomed { .. }) {
                self.stats.moves_unsent += 1;
                continue;
            }
            if self.first_move.is_none() {
                self.first_move = Some(input.seq);
            }
            self.send(&Inbound::Move(Move { input }), Channel::Unreliable);
            self.stats.moves_sent += 1;
            if self.config.repeat_moves
                && let Some(previous) = self.previous
            {
                self.send(&Inbound::Move(Move { input: previous }), Channel::Unreliable);
                self.stats.moves_sent += 1;
            }
            self.previous = Some(input);
        }
    }

    fn receive(&mut self) {
        let Self {
            transport,
            clock,
            ring,
            scratch,
            last_applied,
            pending_acks,
            refusals,
            extensions,
            entity_changes,
            features,
            permitted,
            permitted_changed,
            snapshots,
            state,
            stats,
            epoch,
            connection,
            first_move,
            ..
        } = self;
        let (epoch, connection, first_move) = (*epoch, *connection, *first_move);
        transport.poll(&mut |event| match event {
            TransportEvent::Frame { bytes, .. } => {
                scratch.clear();
                match decode_server_frame(bytes, &*ring, scratch) {
                    Ok(ServerFrame::Snapshot) => {
                        let tick = scratch.header.server_tick;
                        if last_applied.is_some_and(|t| tick <= t) {
                            stats.stale += 1;
                            return;
                        }
                        *last_applied = Some(tick);
                        stats.snapshots += 1;
                        stats.overflowed += u64::from(scratch.overflowed);
                        // Keep it as a baseline, overwriting the oldest.
                        ring.keep(scratch);
                        push_entity_changes(entity_changes, scratch);
                        if pending_acks.len() < pending_acks.capacity() {
                            pending_acks.push(tick);
                        }
                        match snapshots.acquire() {
                            Some(mut frame) => {
                                frame.epoch = epoch;
                                frame.connection = connection;
                                frame.resume_from = first_move;
                                stats.overflowed += u64::from(fill(&mut frame, scratch, clock.now()));
                                let _ = snapshots.send(frame);
                            }
                            None => stats.sim_full += 1,
                        }
                    }
                    Ok(ServerFrame::Message(Outbound::Welcome(w))) => {
                        *state = SessionState::Welcomed {
                            avatar: w.avatar,
                            character: w.character,
                        };
                    }
                    Ok(ServerFrame::Message(Outbound::Refuse(r))) => *state = SessionState::Refused(r.reason),
                    Ok(ServerFrame::Message(Outbound::SetPosition(_))) => stats.position_resets += 1,
                    Ok(ServerFrame::Message(Outbound::FeatureState(f))) => {
                        if features.len() < features.capacity() {
                            features.push(f);
                        }
                    }
                    Ok(ServerFrame::Message(Outbound::PermittedModules(p))) => {
                        stats.permitted_updates += 1;
                        *permitted = Some(p);
                        *permitted_changed = true;
                    }
                    Ok(ServerFrame::Message(Outbound::ExtensionMessage(m))) => {
                        stats.extension_messages += 1;
                        if extensions.len() < extensions.capacity() {
                            extensions.push(m);
                        } else {
                            stats.extension_messages_dropped += 1;
                        }
                    }
                    Ok(ServerFrame::Message(Outbound::ExtensionRefused(r))) => {
                        stats.extensions_refused += 1;
                        if refusals.len() < refusals.capacity() {
                            refusals.push((r.kind, r.request, r.reason));
                        }
                    }
                    // A message this build does not know (the contract enums are
                    // non-exhaustive): ignored and counted.
                    Ok(_) => stats.unknown_messages += 1,
                    Err(_) => stats.undecodable += 1,
                }
            }
            TransportEvent::Disconnected { .. } => *state = SessionState::Closed,
            TransportEvent::Connected(_) => {}
        });
    }
}

/// Converts a decoded wire snapshot into the simulation's frame. Returns the items that
/// did not fit.
fn fill(
    frame: &mut SnapshotFrame<MotionState>,
    wire: &WireFrame,
    received_at: crate::time::HostInstant,
) -> u32 {
    let h = &wire.header;
    frame.server_tick = h.server_tick;
    frame.received_at = received_at;
    frame.ack = h.ack;
    frame.local = h.local.map(|l| (l.id, l.state));
    frame.local_mods = h.local_mods;
    let mut overflow = 0u32;
    for r in wire.remotes.iter() {
        let RemoteSample {
            id,
            tick,
            position,
            velocity,
            yaw,
        } = *r;
        overflow += u32::from(!frame.push_remote(RemoteState {
            id,
            tick,
            position,
            velocity,
            yaw,
        }));
    }
    for id in wire.removed.iter() {
        overflow += u32::from(!frame.push_removed(*id));
    }
    for (id, appearance) in wire.entered.iter() {
        overflow += u32::from(!frame.push_entered(*id, appearance.0));
    }
    for m in wire.markers.iter() {
        overflow += u32::from(!frame.push_marker(*m));
    }
    overflow
}

/// The delta baselines: the most recently applied snapshots of this epoch. A slot holds a
/// frame only once one was kept there, so an empty slot (tick 0) never serves as a base.
struct Ring {
    frames: Vec<WireFrame>,
    live: Vec<bool>,
}

impl Ring {
    /// Keeps `frame`, overwriting an empty slot or else the oldest frame.
    fn keep(&mut self, frame: &WireFrame) {
        let slot = self.live.iter().position(|l| !*l).or_else(|| {
            self.frames
                .iter()
                .enumerate()
                .min_by_key(|(_, f)| f.header.server_tick)
                .map(|(i, _)| i)
        });
        if let Some(i) = slot
            && let (Some(f), Some(l)) = (self.frames.get_mut(i), self.live.get_mut(i))
        {
            f.copy_from(frame);
            *l = true;
        }
    }

    /// Forgets every frame (a hand-off or a reconnect: no base carries over).
    fn clear(&mut self) {
        for (f, l) in self.frames.iter_mut().zip(self.live.iter_mut()) {
            f.clear();
            *l = false;
        }
    }
}

impl mantis_adapter_contract::native::BaselineStore for Ring {
    fn baseline(&self, tick: Tick) -> Option<&WireFrame> {
        self.frames
            .iter()
            .zip(&self.live)
            .find(|(f, l)| **l && f.header.server_tick == tick)
            .map(|(f, _)| f)
    }
}

/// Queues the entities a snapshot entered and removed, for the module view models (as
/// many as fit; the queue never grows).
fn push_entity_changes(out: &mut Vec<crate::modules::EntityChange>, wire: &WireFrame) {
    for (entity, appearance) in wire.entered.iter() {
        if out.len() < out.capacity() {
            out.push(crate::modules::EntityChange::Entered {
                entity: *entity,
                appearance: appearance.0,
            });
        }
    }
    for entity in wire.removed.iter() {
        if out.len() < out.capacity() {
            out.push(crate::modules::EntityChange::Removed(*entity));
        }
    }
}
