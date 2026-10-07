//! Shared harness for the client's integration tests: a headless world session driven tick
//! by tick and frame by frame against a manual clock, talking to a stand-in server that
//! runs the same motion model with configurable latency.
//!
//! Tests return `Result` and use `?`; the workspace lints deny `unwrap` everywhere.

use std::collections::VecDeque;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mantis_client::core_api::{
    Angle16, CoreMotion, EntityId, FlatGround, InputSeq, Motion, MotionModifiers, MotionParams, MotionState,
    MoveButtons, MoveInput, Tick, TickRate, Vec3,
};
use mantis_client::host::{
    WorldSessionBuild, WorldSessionConfig, WorldSessionParts, build_world_session, platform_event_channel,
};
use mantis_client::input::action::{ActionKind, ActionTable};
use mantis_client::input::binding::{Binding, ContextTable};
use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::input::intent::MoveIntentMap;
use mantis_client::input::router::InputRouter;
use mantis_client::render_world::{Pose, PoseSource};
use mantis_client::sim::IntentSink;
use mantis_client::snapshot::RemoteState;
use mantis_client::threads::render_thread::{FrameContext, FrameSink, PlatformEvent};
use mantis_client::time::{HostClock, HostInstant, ManualClock};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

pub const HZ: u32 = 30;
pub const AVATAR: EntityId = EntityId::new(1, 1);
pub const NPC: EntityId = EntityId::new(2, 1);
pub const NPC_VELOCITY: Vec3 = Vec3::new(3.0, 0.0, 0.0);

/// The client's motion model: core kinematics over flat ground.
pub type ClientMotion = CoreMotion<FlatGround>;

/// Core's motion model with default parameters (the same function the server runs).
pub fn core_motion() -> Result<Motion, Box<dyn std::error::Error>> {
    Ok(Motion::new(MotionParams::DEFAULT)?)
}

pub fn rate() -> Result<TickRate, Box<dyn std::error::Error>> {
    TickRate::new(HZ).ok_or_else(|| "zero rate".into())
}

pub fn tick_duration() -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(HZ))
}

/// Outbox writing into a shared, preallocated queue (no allocation while under capacity).
#[derive(Clone)]
pub struct SharedOutbox(pub Arc<Mutex<VecDeque<MoveInput>>>);

impl SharedOutbox {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(VecDeque::with_capacity(capacity))))
    }

    pub fn drain_into(&self, out: &mut VecDeque<MoveInput>) {
        let mut q = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(m) = q.pop_front() {
            out.push_back(m);
        }
    }
}

impl IntentSink for SharedOutbox {
    fn send_move(&mut self, input: &MoveInput) {
        let mut q = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if q.len() < q.capacity() {
            q.push_back(*input);
        }
    }
}

/// Sink recording the local and NPC poses of every frame (preallocated).
pub struct PoseSink {
    pub frames: Arc<Mutex<Vec<FramePoses>>>,
}

#[derive(Clone, Copy, Debug)]
pub struct FramePoses {
    pub now_nanos: u64,
    pub local: Option<Pose>,
    pub npc: Option<Pose>,
    pub world_tick: Tick,
}

impl FrameSink for PoseSink {
    fn resize(&mut self, _width: u32, _height: u32) {}

    fn submit(&mut self, f: &FrameContext<'_>) {
        let poses = f.poses.as_slice();
        let local = poses
            .iter()
            .copied()
            .find(|p| p.source == PoseSource::LocalPredicted);
        let npc = poses.iter().copied().find(|p| p.id == NPC);
        let mut frames = self.frames.lock().unwrap_or_else(PoisonError::into_inner);
        if frames.len() < frames.capacity() {
            frames.push(FramePoses {
                now_nanos: f.now.as_nanos(),
                local,
                npc,
                world_tick: f.world.tick(),
            });
        }
    }
}

/// The stand-in server: integrates received move inputs with the same motion model,
/// answers with one snapshot per tick.
pub struct StandInServer {
    pub motion: Motion,
    pub ground: FlatGround,
    pub state: MotionState,
    pub mods: MotionModifiers,
    pub ack: Option<InputSeq>,
    pub tick: Tick,
    pub npc_position: Vec3,
    /// Inputs in flight to the server: (arrival client tick, input).
    pub uplink: VecDeque<(u64, MoveInput)>,
    /// Snapshots in flight to the client: (arrival client tick, server tick, ack, state, npc).
    pub downlink: VecDeque<(u64, Tick, Option<InputSeq>, MotionState, Vec3, MotionModifiers)>,
    /// Whether snapshots carry the server's modifiers (false models a divergence the
    /// client cannot know about, such as an unmodeled collision).
    pub send_mods: bool,
    pub up_ticks: u64,
    pub down_ticks: u64,
}

impl StandInServer {
    pub fn new(up_ticks: u64, down_ticks: u64) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            motion: core_motion()?,
            ground: FlatGround(0.0),
            state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
            mods: MotionModifiers::default(),
            ack: None,
            tick: Tick::ZERO,
            npc_position: Vec3::new(-10.0, 0.0, 5.0),
            uplink: VecDeque::with_capacity(256),
            downlink: VecDeque::with_capacity(256),
            send_mods: false,
            up_ticks,
            down_ticks,
        })
    }

    /// One server tick at client tick `now_tick`: apply inputs that have arrived, then
    /// emit a snapshot that will arrive `down_ticks` later.
    pub fn step(&mut self, now_tick: u64) {
        while let Some((arrive, input)) = self.uplink.front().copied() {
            if arrive > now_tick {
                break;
            }
            let _ = self.uplink.pop_front();
            self.state = self
                .motion
                .step(&self.ground, &self.state, &input, &self.mods, 1.0 / 30.0);
            self.ack = Some(input.seq);
        }
        self.npc_position += NPC_VELOCITY * (1.0 / 30.0);
        self.tick = self.tick.next();
        self.downlink.push_back((
            now_tick + self.down_ticks,
            self.tick,
            self.ack,
            self.state,
            self.npc_position,
            if self.send_mods {
                self.mods
            } else {
                MotionModifiers::default()
            },
        ));
    }
}

/// A headless session plus the stand-in server and its plumbing.
pub struct Rig {
    pub config: WorldSessionConfig,
    pub clock: Arc<ManualClock>,
    pub session: WorldSessionBuild<ClientMotion, SharedOutbox, PoseSink>,
    pub events: SyncSender<PlatformEvent>,
    pub outbox: SharedOutbox,
    pub server: StandInServer,
    pub frames: Arc<Mutex<Vec<FramePoses>>>,
    pub client_tick: u64,
    pub sent: VecDeque<MoveInput>,
    /// Attach a timeline marker to every snapshot whose server tick is a multiple of this.
    pub marker_every: Option<u64>,
    /// The session epoch and connection stamped on delivered frames (the network
    /// session bumps them at a hand-off and at a reconnect).
    pub epoch: u32,
    pub connection: u32,
    /// The first input sent on the current connection, stamped on frames (as the
    /// network session does); `track_first` records the next one sent.
    pub resume_from: Option<mantis_client::core_api::InputSeq>,
    pub track_first: bool,
}

pub fn ground() -> Arc<FlatGround> {
    Arc::new(FlatGround(0.0))
}

impl Rig {
    pub fn new(
        up_ticks: u64,
        down_ticks: u64,
        ground: Arc<FlatGround>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with(up_ticks, down_ticks, ground, |_| {})
    }

    pub fn new_with(
        up_ticks: u64,
        down_ticks: u64,
        ground: Arc<FlatGround>,
        tune: impl FnOnce(&mut WorldSessionConfig),
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let clock = Arc::new(ManualClock::new());
        let mut config = WorldSessionConfig::new(rate()?);
        tune(&mut config);
        let mut actions = ActionTable::new();
        let forward = actions.define("move_forward", ActionKind::Button)?;
        let mut contexts = ContextTable::new();
        let gameplay = contexts.define("gameplay", false)?;
        contexts.bind(
            &actions,
            gameplay,
            forward,
            Binding::Button(ButtonSource::Key(KeyCode::W)),
        )?;
        let mut intents = MoveIntentMap::new();
        intents.map_button(&actions, forward, MoveButtons::FORWARD)?;
        let mut router = InputRouter::new(actions, contexts);
        router.push_context(gameplay)?;
        let outbox = SharedOutbox::new(1024);
        let frames = Arc::new(Mutex::new(Vec::with_capacity(4096)));
        let (events, rx) = platform_event_channel();
        let session = build_world_session(
            &config,
            WorldSessionParts {
                motion: CoreMotion::new(core_motion()?),
                ground,
                initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
                router,
                intents,
                outbox: outbox.clone(),
            },
            clock.clone(),
            rx,
            PoseSink {
                frames: Arc::clone(&frames),
            },
        );
        Ok(Self {
            config,
            clock,
            session,
            events,
            outbox,
            server: StandInServer::new(up_ticks, down_ticks)?,
            frames,
            client_tick: 0,
            sent: VecDeque::with_capacity(1024),
            marker_every: None,
            epoch: 0,
            connection: 0,
            resume_from: None,
            track_first: false,
        })
    }

    pub fn key(&self, k: KeyCode, pressed: bool) -> TestResult {
        self.events.send(PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(k),
            pressed,
        }))?;
        Ok(())
    }

    /// Delivers every snapshot due at the current client tick.
    pub fn deliver_snapshots(&mut self) {
        while let Some(&(arrive, server_tick, ack, state, npc, mods)) = self.server.downlink.front() {
            if arrive > self.client_tick {
                break;
            }
            let _ = self.server.downlink.pop_front();
            let Some(mut frame) = self.session.snapshots.acquire() else {
                continue;
            };
            frame.server_tick = server_tick;
            frame.received_at = self.clock.now();
            frame.epoch = self.epoch;
            frame.connection = self.connection;
            frame.resume_from = self.resume_from;
            frame.ack = ack;
            frame.local = Some((AVATAR, state));
            frame.local_mods = mods;
            let _ = frame.push_remote(RemoteState {
                id: NPC,
                tick: server_tick,
                position: npc,
                velocity: NPC_VELOCITY,
                yaw: Angle16(0),
            });
            if self.marker_every.is_some_and(|n| server_tick.0.is_multiple_of(n)) {
                let _ = frame.push_marker(mantis_core::graph::TimelineMarker {
                    id: mantis_core::graph::MarkerId {
                        graph: mantis_core::graph::GraphId(1),
                        node: mantis_core::graph::NodeKey(1),
                    },
                    kind: mantis_core::graph::MarkerKind::CastStart,
                    at: server_tick,
                    offset: 0,
                    source: NPC,
                    target: None,
                    instance: mantis_core::graph::GraphInstanceId(server_tick.0),
                });
            }
            let _ = self.session.snapshots.send(frame);
        }
    }

    /// Pins the clock to the exact instant the stepper schedules the current tick.
    pub fn pin_clock(&self) {
        let start = u128::from(self.client_tick) * 1_000_000_000 / u128::from(HZ);
        self.clock
            .set(HostInstant::from_nanos(u64::try_from(start).unwrap_or(u64::MAX)));
    }

    /// One client tick with `frames_per_tick` evenly spaced render frames, the first at
    /// the tick instant.
    pub fn run_tick(&mut self, frames_per_tick: u32) {
        self.pin_clock();
        self.deliver_snapshots();
        let _ = self.session.sim.run_due(self.clock.now());
        self.outbox.drain_into(&mut self.sent);
        while let Some(m) = self.sent.pop_front() {
            if self.track_first {
                self.track_first = false;
                self.resume_from = Some(m.seq);
            }
            self.server
                .uplink
                .push_back((self.client_tick + self.server.up_ticks, m));
        }
        self.server.step(self.client_tick);
        let frame_step = tick_duration() / frames_per_tick.max(1);
        for i in 0..frames_per_tick {
            if i > 0 {
                self.clock.advance(frame_step);
            }
            let _ = self.session.render.frame();
        }
        self.client_tick += 1;
    }

    pub fn frames(&self) -> Vec<FramePoses> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
