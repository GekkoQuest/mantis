//! The replay viewer: steps a fresh client simulation through an MCRC recording
//! (`mantis_client::recording`) tick by tick and checks it against what was recorded.
//!
//! For each tick record, in order:
//! 1. every recorded snapshot frame is delivered through the real snapshot channel;
//! 2. the recorded input is merged into the real input accumulator as one frame (held,
//!    pressed, released, axes, look), so the tick takes exactly the recorded actions and
//!    look; only the informational frame counter can differ, and nothing reads it;
//! 3. the simulation runs the tick at the recorded tick and host instant;
//! 4. the intent it sent is compared with the recorded one
//!    ([`ReplayError::IntentDivergence`]), then its state hash with the recorded hash
//!    ([`ReplayError::Divergence`]). As in the server replay (`mantis_core::replay`),
//!    divergence is an error.
//!
//! The caller supplies the simulation through a factory that turns the viewer's wiring
//! (accumulator, inbox, intent capture) into [`ClientSimParts`] with the motion model,
//! ground, timestep, tuning, and intent map the recording was made with. Seeking backward
//! rebuilds the simulation from the factory and replays from the start.
//!
//! The viewer stays usable after an error: the diverging tick has been applied, so
//! [`ReplayViewer::inspect`] shows the replayed state at that tick.

use core::fmt;
use std::sync::Arc;

use mantis_client::core_api::{AvatarKinematics, EntityId, MotionStep, MoveInput, Tick};
use mantis_client::input::accumulator::InputAccumulator;
use mantis_client::input::router::ActionFrame;
use mantis_client::recording::{RecordState, Recording};
use mantis_client::render_world::{Correction, PresentationConfig, RenderWorld};
use mantis_client::sim::{ClientSim, ClientSimParts, IntentSink};
use mantis_client::snapshot::{SnapshotInbox, SnapshotSender, snapshot_channel};
use mantis_client::threads::sim_thread::TickHandler;
use mantis_formats::FormatError;

/// The intent sink a replayed simulation sends to: keeps the last intent and a count.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplayCapture {
    sent: u64,
    last: Option<MoveInput>,
}

impl ReplayCapture {
    /// Intents sent so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// The last intent sent.
    pub fn last(&self) -> Option<MoveInput> {
        self.last
    }
}

impl IntentSink for ReplayCapture {
    fn send_move(&mut self, input: &MoveInput) {
        self.sent = self.sent.saturating_add(1);
        self.last = Some(*input);
    }
}

/// What the viewer hands the factory: put these into the [`ClientSimParts`] it returns.
#[derive(Debug)]
pub struct ReplayWiring<S> {
    /// The input accumulator the viewer merges recorded input into.
    pub accumulator: Arc<InputAccumulator>,
    /// The inbox the viewer delivers recorded snapshot frames to.
    pub inbox: SnapshotInbox<S>,
    /// The sink that captures sent intents.
    pub outbox: ReplayCapture,
}

/// Replay failed at a tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayError {
    /// The state hash after `tick` differs from the recording.
    Divergence {
        /// The first diverging tick.
        tick: Tick,
        /// The recorded hash.
        expected: u64,
        /// The replayed hash.
        actual: u64,
    },
    /// The intent sent at `tick` differs from the recording.
    IntentDivergence {
        /// The tick.
        tick: Tick,
        /// The recorded intent.
        expected: Option<MoveInput>,
        /// The replayed intent.
        actual: Option<MoveInput>,
    },
    /// The snapshot channel refused a recorded frame at `tick` (the factory did not
    /// wire the viewer's inbox).
    Inbox {
        /// The tick.
        tick: Tick,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Divergence {
                tick,
                expected,
                actual,
            } => write!(
                f,
                "client replay diverged at {tick}: recorded {expected:#018x}, replayed {actual:#018x}"
            ),
            Self::IntentDivergence {
                tick,
                expected,
                actual,
            } => write!(
                f,
                "client replay sent a different intent at {tick}: recorded {expected:?}, replayed {actual:?}"
            ),
            Self::Inbox { tick } => write!(f, "client replay could not deliver the frames of {tick}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// One verified tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StepReport {
    /// The tick.
    pub tick: Tick,
    /// Snapshot frames delivered before it.
    pub frames: usize,
    /// The intent sent (equal to the recorded one).
    pub sent: Option<MoveInput>,
    /// The state hash after it (equal to the recorded one).
    pub state_hash: u64,
}

/// The replayed simulation's state, as plain data for the editor UI.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ReplayInspector<S> {
    /// The tick last replayed, if any.
    pub tick: Option<Tick>,
    /// Records replayed.
    pub replayed: usize,
    /// Records in the recording.
    pub total: usize,
    /// The local avatar entity, once known.
    pub local: Option<EntityId>,
    /// The predicted local avatar state.
    pub local_state: S,
    /// Remote entities tracked.
    pub remote_count: usize,
    /// Newest server tick applied.
    pub last_server_tick: Option<Tick>,
    /// The visual correction being decayed.
    pub correction: Correction,
    /// The state hash now.
    pub state_hash: u64,
}

impl<S: AvatarKinematics> fmt::Display for ReplayInspector<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tick {
            Some(t) => writeln!(f, "tick {} ({}/{})", t.0, self.replayed, self.total)?,
            None => writeln!(f, "not started (0/{})", self.total)?,
        }
        match self.local {
            Some(id) => writeln!(f, "local {}:{}", id.index(), id.generation())?,
            None => writeln!(f, "local none")?,
        }
        let p = self.local_state.position();
        let v = self.local_state.velocity();
        writeln!(f, "position {} {} {}", p.x, p.y, p.z)?;
        writeln!(f, "velocity {} {} {}", v.x, v.y, v.z)?;
        writeln!(f, "yaw {}", self.local_state.yaw().0)?;
        writeln!(f, "remotes {}", self.remote_count)?;
        match self.last_server_tick {
            Some(t) => writeln!(f, "server tick {}", t.0)?,
            None => writeln!(f, "server tick none")?,
        }
        let c = self.correction.offset;
        writeln!(f, "correction {} {} {}", c.x, c.y, c.z)?;
        write!(f, "state hash {:#018x}", self.state_hash)
    }
}

struct Session<M: MotionStep> {
    sim: ClientSim<M, ReplayCapture>,
    snapshots: SnapshotSender<M::State>,
    accumulator: Arc<InputAccumulator>,
    world: RenderWorld,
}

/// Steps a fresh client simulation through a recording, verifying every tick.
pub struct ReplayViewer<M: MotionStep, F> {
    recording: Recording<M::State>,
    factory: F,
    session: Session<M>,
    cursor: usize,
    channel_frames: usize,
    channel_width: usize,
}

impl<M: MotionStep, F> fmt::Debug for ReplayViewer<M, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplayViewer")
            .field("cursor", &self.cursor)
            .field("records", &self.recording.ticks.len())
            .finish_non_exhaustive()
    }
}

impl<M, F> ReplayViewer<M, F>
where
    M: MotionStep,
    M::State: RecordState,
    F: FnMut(ReplayWiring<M::State>) -> ClientSimParts<M, ReplayCapture>,
{
    /// A viewer over `recording`, building its simulation with `factory`.
    pub fn new(recording: Recording<M::State>, mut factory: F) -> Self {
        // The channel holds every frame of the busiest tick, each list at its widest.
        let channel_frames = recording
            .ticks
            .iter()
            .map(|t| t.frames.len())
            .max()
            .unwrap_or(0)
            .max(1);
        let channel_width = recording
            .ticks
            .iter()
            .flat_map(|t| &t.frames)
            .map(mantis_client::recording::RecordedFrame::widest_list)
            .max()
            .unwrap_or(0)
            .max(1);
        let session = Self::build(&mut factory, channel_frames, channel_width);
        Self {
            recording,
            factory,
            session,
            cursor: 0,
            channel_frames,
            channel_width,
        }
    }

    /// Parses `bytes` and builds a viewer over them.
    ///
    /// # Errors
    /// The recording's first malformed field.
    pub fn open(bytes: &[u8], factory: F) -> Result<Self, FormatError> {
        Ok(Self::new(Recording::parse(bytes)?, factory))
    }

    fn build(factory: &mut F, frames: usize, width: usize) -> Session<M> {
        let (snapshots, inbox) = snapshot_channel(frames, width);
        let accumulator = Arc::new(InputAccumulator::new());
        let parts = factory(ReplayWiring {
            accumulator: Arc::clone(&accumulator),
            inbox,
            outbox: ReplayCapture::default(),
        });
        let world = RenderWorld::with_capacity(parts.config.remote_capacity, PresentationConfig::default());
        Session {
            sim: ClientSim::new(parts),
            snapshots,
            accumulator,
            world,
        }
    }

    /// The recording.
    pub fn recording(&self) -> &Recording<M::State> {
        &self.recording
    }

    /// The replayed simulation.
    pub fn sim(&self) -> &ClientSim<M, ReplayCapture> {
        &self.session.sim
    }

    /// The tick last replayed, or `None` before the first step.
    pub fn tick(&self) -> Option<Tick> {
        self.cursor
            .checked_sub(1)
            .and_then(|i| self.recording.ticks.get(i))
            .map(|t| t.tick)
    }

    /// True when every record has been replayed.
    pub fn at_end(&self) -> bool {
        self.cursor >= self.recording.ticks.len()
    }

    /// Replays the next record, or returns `None` at the end.
    ///
    /// # Errors
    /// [`ReplayError`] when the tick does not reproduce the recording; the tick has still
    /// been applied.
    pub fn step(&mut self) -> Result<Option<StepReport>, ReplayError> {
        let Some(rec) = self.recording.ticks.get(self.cursor) else {
            return Ok(None);
        };
        let s = &mut self.session;
        let mut delivered = true;
        for frame in &rec.frames {
            let Some(mut f) = s.snapshots.acquire() else {
                delivered = false;
                break;
            };
            delivered &= frame.fill(&mut f);
            delivered &= s.snapshots.send(f);
        }
        let input = &rec.input;
        let frame = ActionFrame {
            held: input.held,
            pressed: input.pressed,
            released: input.released,
            axes: input.axes,
            look_dx: 0.0,
            look_dy: 0.0,
        };
        s.accumulator.merge_frame(&frame, input.look);
        let before = s.sim.outbox().sent();
        s.world.begin(rec.tick, rec.time);
        s.sim.tick(rec.tick, rec.time, &mut s.world);
        self.cursor += 1;

        if !delivered {
            return Err(ReplayError::Inbox { tick: rec.tick });
        }
        let out = s.sim.outbox();
        let sent = if out.sent() == before { None } else { out.last() };
        if sent != rec.sent {
            return Err(ReplayError::IntentDivergence {
                tick: rec.tick,
                expected: rec.sent,
                actual: sent,
            });
        }
        let actual = s.sim.state_hash();
        if actual != rec.state_hash {
            return Err(ReplayError::Divergence {
                tick: rec.tick,
                expected: rec.state_hash,
                actual,
            });
        }
        Ok(Some(StepReport {
            tick: rec.tick,
            frames: rec.frames.len(),
            sent,
            state_hash: actual,
        }))
    }

    /// Rebuilds the simulation and rewinds to before the first record.
    pub fn restart(&mut self) {
        self.session = Self::build(&mut self.factory, self.channel_frames, self.channel_width);
        self.cursor = 0;
    }

    /// Positions the viewer after the last record at or before `tick` (before the first
    /// record when there is none), replaying and verifying every record on the way.
    /// Seeking backward restarts from the beginning.
    ///
    /// # Errors
    /// The first [`ReplayError`] on the way; the viewer stops at that tick.
    pub fn seek(&mut self, tick: Tick) -> Result<(), ReplayError> {
        if self.tick().is_some_and(|t| t > tick) {
            self.restart();
        }
        while self
            .recording
            .ticks
            .get(self.cursor)
            .is_some_and(|r| r.tick <= tick)
        {
            self.step()?;
        }
        Ok(())
    }

    /// Replays every remaining record. Returns the number replayed.
    ///
    /// # Errors
    /// The first [`ReplayError`].
    pub fn run_to_end(&mut self) -> Result<usize, ReplayError> {
        let mut n = 0;
        while self.step()?.is_some() {
            n += 1;
        }
        Ok(n)
    }

    /// The replayed state now.
    pub fn inspect(&self) -> ReplayInspector<M::State> {
        let sim = &self.session.sim;
        ReplayInspector {
            tick: self.tick(),
            replayed: self.cursor,
            total: self.recording.ticks.len(),
            local: sim.local(),
            local_state: *sim.predictor().state(),
            remote_count: sim.remote_count(),
            last_server_tick: sim.last_server_tick(),
            correction: sim.correction(),
            state_hash: sim.state_hash(),
        }
    }
}
