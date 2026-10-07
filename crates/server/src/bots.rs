//! Headless bots (plan 7.7): they speak the intent model through any adapter
//! and drive load tests, acceptance tests, soak runs, and envelope tests.
//!
//! A bot is generic over a [`BotWire`], the client half of an adapter's
//! protocol: [`NativeWire`] here for the engine's native protocol, and each
//! package supplies one for its legacy adapter. Behaviour comes from a
//! [`Profile`]: honest bots must never be rejected; cheating bots must be
//! caught within budget.
//!
//! Predictive bots predict with `Motion::step`, keep unacknowledged inputs,
//! and on each snapshot rewind to the server state and replay them, exactly
//! as the native client does; the distance between what they had predicted
//! and the reconciled state is the correction magnitude (a budget row).
//! Validated bots integrate locally and report positions as claims.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use mantis_adapter_contract::core_types::{
    AimAngles, Angle16, BoundedArray, ContentHash, EntityId, InputSeq, MotionModifiers, MotionState,
    MoveButtons, MoveInput, Tick, Vec3,
};
use mantis_adapter_contract::native::{BaselineStore, ServerFrame, decode_server_frame, encode_inbound};
use mantis_adapter_contract::{
    Channel, ConnectionId, Extension, ExtensionKind, ExtensionRefusal, Hello, Inbound, LocalAvatar, Move,
    MoveClaim, MovementMode, Outbound, PROTOCOL_VERSION, RefuseReason, SnapshotAck, SnapshotFrame, Transport,
    TransportEvent,
};
use mantis_core::kinematics::{GroundQuery, Motion, MotionParams};
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::time::TickRate;

/// Something a bot learned from the server.
#[derive(Clone, Copy, PartialEq, Debug)]
#[expect(clippy::large_enum_variant)] // delivered one at a time to the bot
pub enum BotEvent {
    /// The session was accepted.
    Welcome {
        /// The avatar.
        avatar: Option<EntityId>,
    },
    /// The session was refused.
    Refused(RefuseReason),
    /// A hard correction (Validated).
    Corrected {
        /// Where the server put the avatar.
        position: Vec3,
    },
    /// A package feature message (copied out of the frame).
    ExtensionMessage {
        /// The extension.
        kind: ExtensionKind,
        /// Payload length.
        len: u16,
        /// Payload bytes (the first `len`).
        bytes: [u8; 512],
    },
    /// The client modules this session may run, and their tier.
    Permitted(mantis_adapter_contract::PermittedModules),
    /// A module was switched on or off.
    FeatureState {
        /// The module key.
        key: mantis_adapter_contract::core_types::WireString<64>,
        /// Enabled.
        enabled: bool,
    },
    /// A package intent extension was refused.
    ExtensionRefused {
        /// The extension.
        kind: ExtensionKind,
        /// The refused send's request id.
        request: u32,
        /// Why.
        reason: ExtensionRefusal,
    },
    /// A snapshot.
    Snapshot {
        /// Its tick.
        tick: Tick,
        /// The applied-input acknowledgement.
        ack: Option<InputSeq>,
        /// The avatar's authoritative state.
        local: Option<LocalAvatar>,
        /// Remote samples it carried.
        remotes: usize,
    },
}

/// The client half of an adapter's protocol.
pub trait BotWire: Send {
    /// The adapter's movement mode.
    fn mode(&self) -> MovementMode;
    /// Encodes a contract message in the protocol; returns the channel to use,
    /// or `None` when the protocol has no form for it.
    fn encode(&self, msg: &Inbound, out: &mut Vec<u8>) -> Option<Channel>;
    /// Decodes one received frame.
    fn decode(&mut self, bytes: &[u8], events: &mut dyn FnMut(BotEvent));
    /// True when the client currently knows `id` as a remote entity (it
    /// entered view and has not left it).
    fn sees(&self, id: EntityId) -> bool;
}

/// The native protocol's client half. The mode is normally Predictive; a
/// Validated native wire lets envelope tests run without a legacy adapter.
pub struct NativeWire {
    mode: MovementMode,
    ring: Vec<SnapshotFrame>,
    scratch: SnapshotFrame,
    known: BTreeSet<EntityId>,
}

impl NativeWire {
    /// A native wire in `mode`.
    #[must_use]
    pub fn new(mode: MovementMode) -> Self {
        Self {
            mode,
            ring: (0..16)
                .map(|_| SnapshotFrame::with_capacity(64, 256, 64, 64))
                .collect(),
            scratch: SnapshotFrame::with_capacity(64, 256, 64, 64),
            known: BTreeSet::new(),
        }
    }
}

impl Default for NativeWire {
    fn default() -> Self {
        Self::new(MovementMode::Predictive)
    }
}

impl BotWire for NativeWire {
    fn mode(&self) -> MovementMode {
        self.mode
    }

    fn encode(&self, msg: &Inbound, out: &mut Vec<u8>) -> Option<Channel> {
        encode_inbound(msg, out);
        Some(match msg {
            Inbound::Move(_) | Inbound::SnapshotAck(_) => Channel::Unreliable,
            _ => Channel::Reliable,
        })
    }

    fn decode(&mut self, bytes: &[u8], events: &mut dyn FnMut(BotEvent)) {
        self.scratch.clear();
        let frame = decode_server_frame(bytes, &self.ring[..], &mut self.scratch);
        match frame {
            Ok(ServerFrame::Snapshot) => {
                let h = self.scratch.header;
                for id in self.scratch.removed.iter() {
                    self.known.remove(id);
                }
                for (id, _) in self.scratch.entered.iter() {
                    self.known.insert(*id);
                }
                // Keep it as a future baseline (overwrite the oldest).
                if let Some(oldest) = self.ring.iter_mut().min_by_key(|f| f.header.server_tick) {
                    oldest.copy_from(&self.scratch);
                }
                events(BotEvent::Snapshot {
                    tick: h.server_tick,
                    ack: h.ack,
                    local: h.local,
                    remotes: self.scratch.remotes.len(),
                });
            }
            Ok(ServerFrame::Message(Outbound::Welcome(w))) => events(BotEvent::Welcome { avatar: w.avatar }),
            Ok(ServerFrame::Message(Outbound::Refuse(r))) => events(BotEvent::Refused(r.reason)),
            Ok(ServerFrame::Message(Outbound::ExtensionMessage(m))) => {
                let mut bytes = [0u8; 512];
                let mut len = 0u16;
                for (slot, b) in bytes.iter_mut().zip(m.payload.iter()) {
                    *slot = *b;
                    len += 1;
                }
                events(BotEvent::ExtensionMessage {
                    kind: m.kind,
                    len,
                    bytes,
                });
            }
            Ok(ServerFrame::Message(Outbound::PermittedModules(p))) => events(BotEvent::Permitted(p)),
            Ok(ServerFrame::Message(Outbound::FeatureState(f))) => events(BotEvent::FeatureState {
                key: f.module,
                enabled: f.enabled,
            }),
            Ok(ServerFrame::Message(Outbound::ExtensionRefused(r))) => events(BotEvent::ExtensionRefused {
                kind: r.kind,
                request: r.request,
                reason: r.reason,
            }),
            Ok(ServerFrame::Message(Outbound::SetPosition(p))) => {
                events(BotEvent::Corrected { position: p.position });
            }
            // Undecodable, or a message a newer contract adds: ignored.
            Ok(_) | Err(_) => {}
        }
    }

    fn sees(&self, id: EntityId) -> bool {
        self.known.contains(&id)
    }
}

impl BaselineStore for NativeWire {
    fn baseline(&self, tick: Tick) -> Option<&SnapshotFrame> {
        self.ring[..].baseline(tick)
    }
}

/// How a bot behaves.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Profile {
    /// Plays by the rules, wandering at random.
    Honest,
    /// Plays by the rules, standing still (an observer).
    Idle,
    /// Plays by the rules, running straight ahead at this heading.
    Heading(Angle16),
    /// Moves `factor` times faster than allowed (Validated).
    SpeedHack(f32),
    /// Jumps `distance` units every `every` ticks (Validated).
    Teleport {
        /// Interval in ticks.
        every: u32,
        /// Distance per jump.
        distance: f32,
    },
    /// At this heading, runs for `hold` ticks out of every `every` and
    /// stands for the rest, holding back the inputs of every run and then
    /// sending them all at once: the adversary of input realignment
    /// (Predictive). It must gain no distance, and the steps it withheld
    /// are not what the cell ran, so its predictions are corrected.
    Withholder {
        /// The heading.
        yaw: Angle16,
        /// Ticks held back.
        hold: u32,
        /// Out of this many.
        every: u32,
    },
    /// Otherwise standing still, jumps `distance` along +x on its first
    /// claim and again on the first claim after every correction: the
    /// adversary of the correction resync rule (Validated).
    Relapse {
        /// Distance per jump.
        distance: f32,
    },
}

/// Bot settings.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BotConfig {
    /// Behaviour.
    pub profile: Profile,
    /// Movement tuning (the package's).
    pub motion: MotionParams,
    /// Client tick rate.
    pub rate: TickRate,
    /// Random seed for the walk.
    pub seed: u64,
    /// Content hash to announce.
    pub content: ContentHash,
    /// Offset of the bot's clock from server time, in ms.
    pub clock_offset_ms: u32,
}

/// What a bot measured.
#[derive(Clone, Debug, Default)]
pub struct BotStats {
    /// Snapshots received.
    pub snapshots: u64,
    /// Hard corrections received (Validated).
    pub corrections: u64,
    /// Claims sent (Validated).
    pub claims: u64,
    /// Inputs sent (Predictive).
    pub inputs: u64,
    /// Reconciliation error magnitudes, one per snapshot (Predictive).
    pub reconcile: Vec<f32>,
    /// Client ticks from the first claim beyond honest movement (faster
    /// than the run speed, or a jump) to the first correction received.
    pub detection_ticks: Option<u64>,
    /// Jumps made (Teleport and Relapse).
    pub jumps: u64,
    /// Refused at the handshake.
    pub refused: Option<RefuseReason>,
    /// Extension refusals received, newest last.
    pub extension_refusals: Vec<(ExtensionKind, ExtensionRefusal)>,
    /// The request id each refusal echoed, in the same order.
    pub refused_requests: Vec<u32>,
    /// Feature messages received, newest last.
    pub extension_messages: Vec<(ExtensionKind, Vec<u8>)>,
    /// The latest known state of each module the server announced.
    pub features: std::collections::BTreeMap<String, bool>,
    /// Every permitted-modules list received, in order: (tier, keys).
    pub permitted: Vec<(mantis_adapter_contract::ModTier, Vec<String>)>,
    /// Remote samples seen in the last snapshot.
    pub last_remotes: usize,
    /// For a watched entity: whether the client knew it, per snapshot.
    pub watched: Vec<bool>,
}

/// A headless client.
pub struct Bot {
    wire: Box<dyn BotWire>,
    transport: Box<dyn Transport>,
    motion: Motion,
    ground: Arc<dyn GroundQuery + Send + Sync>,
    cfg: BotConfig,
    rng: Rng,
    state: MotionState,
    history: VecDeque<MoveInput>,
    seq: InputSeq,
    tick: u64,
    welcomed: bool,
    synced: bool,
    buttons: MoveButtons,
    yaw: Angle16,
    out: Vec<u8>,
    first_cheat_tick: Option<u64>,
    watch: Option<EntityId>,
    avatar: Option<EntityId>,
    relapse_due: bool,
    token: Vec<u8>,
    /// Inputs a [`Profile::Withholder`] is holding back.
    held: Vec<MoveInput>,
    next_request: u32,
    /// Measurements.
    pub stats: BotStats,
}

impl Bot {
    /// A bot speaking `wire` over `transport`, standing at `spawn`.
    ///
    /// # Errors
    /// The name of an invalid motion parameter.
    pub fn new(
        wire: Box<dyn BotWire>,
        transport: Box<dyn Transport>,
        ground: Arc<dyn GroundQuery + Send + Sync>,
        cfg: BotConfig,
        spawn: Vec3,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            motion: Motion::new(cfg.motion)?,
            rng: Rng::for_cell(Seed(cfg.seed), Tick(0), Salt::named("server.bot.walk")),
            state: MotionState::at_rest(spawn, Angle16(0)),
            history: VecDeque::with_capacity(256),
            seq: InputSeq(0),
            tick: 0,
            welcomed: false,
            synced: false,
            buttons: MoveButtons::NONE,
            yaw: Angle16(0),
            out: Vec::with_capacity(256),
            first_cheat_tick: None,
            watch: None,
            avatar: None,
            relapse_due: true,
            token: b"bot".to_vec(),
            held: Vec::new(),
            next_request: 0,
            stats: BotStats::default(),
            wire,
            transport,
            ground,
            cfg,
        })
    }

    /// Presents `token` at the handshake instead of the placeholder (an
    /// entry token from the realm, in a cluster).
    #[must_use]
    pub fn with_token(mut self, token: &[u8]) -> Self {
        self.token = token.to_vec();
        self
    }

    /// Closes the connection, as a client quitting does: the host ends
    /// the session and its character leaves the world.
    pub fn disconnect(&mut self) {
        self.transport
            .disconnect(mantis_adapter_contract::ConnectionId(0));
    }

    /// True once the session was accepted.
    #[must_use]
    pub fn welcomed(&self) -> bool {
        self.welcomed
    }

    /// Records, per snapshot, whether the client knows `id`
    /// ([`BotStats::watched`]).
    pub fn watch(&mut self, id: EntityId) {
        self.watch = Some(id);
    }

    /// The avatar the server announced.
    #[must_use]
    pub fn avatar(&self) -> Option<EntityId> {
        self.avatar
    }

    /// The bot's session tick count.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.tick
    }

    /// True once the bot has seen its avatar in a snapshot (it moves only
    /// from then on).
    #[must_use]
    pub fn synced(&self) -> bool {
        self.synced
    }

    /// The bot's own view of its avatar.
    #[must_use]
    pub fn state(&self) -> &MotionState {
        &self.state
    }

    fn send(&mut self, msg: &Inbound) {
        self.out.clear();
        if let Some(ch) = self.wire.encode(msg, &mut self.out) {
            let _ = self.transport.send(ConnectionId(0), ch, &self.out);
        }
    }

    /// Sends a package feature request (a module's extension intent).
    /// Returns its request id (echoed in a refusal), or 0 when the payload
    /// is too long to send.
    pub fn feature(&mut self, kind: ExtensionKind, payload: &[u8]) -> u32 {
        let Some(payload) = BoundedArray::from_slice(payload) else {
            return 0;
        };
        self.next_request = self.next_request.wrapping_add(1).max(1);
        let request = self.next_request;
        self.send(&Inbound::Extension(Extension {
            kind,
            request,
            payload,
        }));
        request
    }

    /// Opens the session.
    pub fn start(&mut self) {
        let hello = Inbound::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: 0,
            content: self.cfg.content,
            modules: BoundedArray::new(),
            token: BoundedArray::from_slice(&self.token).unwrap_or_default(),
        });
        self.send(&hello);
    }

    fn on_event(&mut self, e: &BotEvent) {
        match *e {
            BotEvent::Welcome { avatar } => {
                self.welcomed = true;
                self.avatar = avatar;
            }
            BotEvent::Refused(r) => self.stats.refused = Some(r),
            BotEvent::Permitted(p) => {
                let keys = p.modules.iter().map(|m| m.as_str().to_owned()).collect();
                self.stats.permitted.push((p.tier, keys));
            }
            BotEvent::FeatureState { key, enabled } => {
                self.stats.features.insert(key.as_str().to_owned(), enabled);
            }
            BotEvent::ExtensionMessage { kind, len, bytes } => {
                let body = bytes.get(..usize::from(len)).unwrap_or(&[]).to_vec();
                self.stats.extension_messages.push((kind, body));
            }
            BotEvent::ExtensionRefused {
                kind,
                request,
                reason,
            } => {
                self.stats.extension_refusals.push((kind, reason));
                self.stats.refused_requests.push(request);
            }
            BotEvent::Corrected { position } => {
                self.stats.corrections += 1;
                if let (Some(first), None) = (self.first_cheat_tick, self.stats.detection_ticks) {
                    self.stats.detection_ticks = Some(self.tick.saturating_sub(first));
                }
                self.state.position = position;
                self.state.velocity = Vec3::ZERO;
                self.relapse_due = true;
            }
            BotEvent::Snapshot {
                tick,
                ack,
                local,
                remotes,
            } => {
                self.stats.snapshots += 1;
                self.stats.last_remotes = remotes;
                if let Some(w) = self.watch {
                    let seen = self.wire.sees(w);
                    self.stats.watched.push(seen);
                }
                let predictive = self.wire.mode() == MovementMode::Predictive;
                if predictive {
                    self.send(&Inbound::SnapshotAck(SnapshotAck { tick }));
                }
                match local {
                    // The first sight of the avatar places the bot.
                    Some(l) if !self.synced => {
                        self.synced = true;
                        self.state = l.state;
                        self.history.clear();
                    }
                    Some(l) if predictive => {
                        if let Some(ack) = ack {
                            self.reconcile(ack, l.state);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Rewinds to the server's state and replays unacknowledged inputs.
    fn reconcile(&mut self, ack: InputSeq, server: MotionState) {
        while self.history.front().is_some_and(|i| !i.seq.is_newer_than(ack)) {
            self.history.pop_front();
        }
        let dt = self.cfg.rate.dt_seconds();
        let mut s = server;
        for input in &self.history {
            s = self
                .motion
                .step(self.ground.as_ref(), &s, input, &MotionModifiers::NONE, dt);
        }
        let error = (s.position - self.state.position).length();
        self.stats.reconcile.push(error);
        self.state = s;
    }

    fn choose_movement(&mut self) {
        match self.cfg.profile {
            Profile::Idle | Profile::Relapse { .. } => {
                self.buttons = MoveButtons::NONE;
                return;
            }
            Profile::Heading(yaw) => {
                self.buttons = MoveButtons::FORWARD;
                self.yaw = yaw;
                return;
            }
            Profile::Withholder { yaw, hold, every } => {
                let running = self.tick % u64::from(every.max(1)) < u64::from(hold);
                self.buttons = if running {
                    MoveButtons::FORWARD
                } else {
                    MoveButtons::NONE
                };
                self.yaw = yaw;
                return;
            }
            _ => {}
        }
        if self.tick.is_multiple_of(45) {
            let r = self.rng.below(8);
            self.buttons = match r {
                0 => MoveButtons::NONE,
                1 => MoveButtons::BACKWARD,
                2 => MoveButtons::FORWARD.with(MoveButtons::STRAFE_LEFT),
                _ => MoveButtons::FORWARD,
            };
            self.yaw = Angle16(u16::try_from(self.rng.below(65_536)).unwrap_or(0));
        }
    }

    /// Sends this tick's input and repeats the previous one, so a single
    /// lost datagram never costs a step; a [`Profile::Withholder`] holds
    /// its inputs back instead, then sends them all.
    fn send_input(&mut self, input: MoveInput, previous: Option<MoveInput>) {
        if let Profile::Withholder { hold, every, .. } = self.cfg.profile
            && self.tick % u64::from(every.max(1)) < u64::from(hold)
        {
            self.held.push(input);
            return;
        }
        for held in std::mem::take(&mut self.held) {
            self.send(&Inbound::Move(Move { input: held }));
        }
        self.send(&Inbound::Move(Move { input }));
        if let Some(p) = previous {
            self.send(&Inbound::Move(Move { input: p }));
        }
    }

    /// One client tick: handle everything received, then act.
    pub fn step(&mut self) {
        let mut events = Vec::new();
        let wire = &mut self.wire;
        self.transport.poll(&mut |e| {
            if let TransportEvent::Frame { bytes, .. } = e {
                wire.decode(bytes, &mut |ev| events.push(ev));
            }
        });
        for e in events {
            self.on_event(&e);
        }
        self.tick += 1;
        if !self.welcomed || !self.synced {
            return;
        }
        self.choose_movement();
        let dt = self.cfg.rate.dt_seconds();
        if self.wire.mode() == MovementMode::Predictive {
            self.seq = self.seq.next();
            let input = MoveInput {
                seq: self.seq,
                tick: Tick(self.tick),
                buttons: self.buttons,
                yaw: self.yaw,
                aim: AimAngles::default(),
            };
            self.state = self.motion.step(
                self.ground.as_ref(),
                &self.state,
                &input,
                &MotionModifiers::NONE,
                dt,
            );
            let previous = self.history.back().copied();
            self.history.push_back(input);
            if self.history.len() > 240 {
                self.history.pop_front();
            }
            self.send_input(input, previous);
            self.stats.inputs += 1;
        } else {
            // Validated, and any mode a newer contract adds: the client
            // claims positions (the stricter path).

            let speed = match self.cfg.profile {
                Profile::SpeedHack(f) => f,
                _ => 1.0,
            };
            let mods = MotionModifiers {
                speed_scale: speed,
                ..MotionModifiers::NONE
            };
            let input = MoveInput {
                buttons: self.buttons.without(MoveButtons::JUMP),
                yaw: self.yaw,
                ..MoveInput::default()
            };
            self.state = self
                .motion
                .step(self.ground.as_ref(), &self.state, &input, &mods, dt);
            let mut cheating = self.state.velocity.horizontal().length() > self.cfg.motion.run_speed;
            let jump = match self.cfg.profile {
                Profile::Teleport { every, distance }
                    if every > 0 && self.tick.is_multiple_of(u64::from(every)) =>
                {
                    Some(distance)
                }
                Profile::Relapse { distance } if self.relapse_due => {
                    self.relapse_due = false;
                    Some(distance)
                }
                _ => None,
            };
            if let Some(distance) = jump {
                self.stats.jumps += 1;
                self.state.position.x += distance;
                if let Some(h) = self
                    .ground
                    .height_at(self.state.position.x, self.state.position.z)
                {
                    self.state.position.y = h;
                }
                cheating = true;
            }
            if cheating && self.first_cheat_tick.is_none() {
                self.first_cheat_tick = Some(self.tick);
            }
            let client_ms = u32::try_from(self.tick * 1000 / u64::from(self.cfg.rate.hz()))
                .unwrap_or(u32::MAX)
                .wrapping_add(self.cfg.clock_offset_ms);
            self.send(&Inbound::MoveClaim(MoveClaim {
                position: self.state.position,
                client_time_ms: client_ms,
            }));
            self.stats.claims += 1;
        }
    }
}
