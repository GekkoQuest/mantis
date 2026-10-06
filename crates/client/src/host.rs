//! Wiring: builds a world session (simulation + render loop + the channels between them)
//! and the platform event channel.
//!
//! [`build_world_session`] returns the loop bodies unspawned, so tests drive them tick by
//! tick and frame by frame against a manual clock; [`WorldSessionBuild::spawn`] runs them
//! on their threads. Either way, the handles are owned by the `World` scope, so exiting
//! the scope stops and joins both threads.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use crate::camera::{CameraRig, LookConfig};
use crate::core_api::{MotionStep, Tick, TickRate};
use crate::input::accumulator::InputAccumulator;
use crate::input::intent::MoveIntentMap;
use crate::input::router::InputRouter;
use crate::render_world::{PresentationConfig, render_world_channel};
use crate::sim::{ClientSim, ClientSimConfig, ClientSimParts, IntentSink};
use crate::snapshot::{MarkerReceiver, SnapshotSender, marker_channel, snapshot_channel};
use crate::threads::render_thread::{
    FrameSink, LookBindings, PlatformEvent, RenderLoop, RenderLoopParts, RenderThread,
};
use crate::threads::sim_thread::{SimDriver, SimThread};
use crate::time::{FixedStepper, HostClock};

/// Capacity of the platform-to-render event channel. Preallocated: sending and receiving
/// never allocate. The platform thread blocks only if the render thread is this many
/// events behind.
pub const PLATFORM_EVENT_CAPACITY: usize = 1024;

/// Creates the platform event channel.
pub fn platform_event_channel() -> (SyncSender<PlatformEvent>, Receiver<PlatformEvent>) {
    sync_channel(PLATFORM_EVENT_CAPACITY)
}

/// World session tuning.
#[derive(Clone, Copy, Debug)]
pub struct WorldSessionConfig {
    /// Client tick rate (equal to the server's for Predictive movement).
    pub tick_rate: TickRate,
    /// Remote presentation timing.
    pub presentation: PresentationConfig,
    /// Simulation tuning.
    pub sim: ClientSimConfig,
    /// Preallocated snapshot frames in flight.
    pub snapshot_frames: usize,
    /// Most ticks run in one burst before the timeline slips.
    pub max_catchup: u32,
    /// Longest the sim thread sleeps between clock checks.
    pub max_sleep: Duration,
    /// Look tuning.
    pub look: LookConfig,
    /// Analog look bindings.
    pub look_bindings: LookBindings,
}

impl WorldSessionConfig {
    /// Defaults for `rate`, with the interpolation delay kept consistent between the sim
    /// and the render world.
    pub fn new(rate: TickRate) -> Self {
        let presentation = PresentationConfig::default();
        Self {
            tick_rate: rate,
            presentation,
            sim: ClientSimConfig {
                server_rate: rate,
                correction_window: Duration::from_millis(150),
                snap_distance: 4.0,
                remote_capacity: 512,
                remote_timeout: Duration::from_secs(2),
                max_remote_extrapolation: presentation.max_remote_extrapolation,
                delay: crate::jitter::DelayConfig::default(),
                input_buffer: 256,
                timeline_rise_shift: 6,
                timeline_adaptive: true,
            },
            snapshot_frames: 8,
            max_catchup: 8,
            max_sleep: Duration::from_millis(2),
            look: LookConfig::default(),
            look_bindings: LookBindings::default(),
        }
    }
}

/// Game-specific parts of a world session.
pub struct WorldSessionParts<M: MotionStep, O: IntentSink> {
    /// The motion model.
    pub motion: M,
    /// The ground model.
    pub ground: Arc<M::Ground>,
    /// Avatar state before the first snapshot.
    pub initial_state: M::State,
    /// Input routing with the package's actions and contexts.
    pub router: InputRouter,
    /// Action-to-intent mapping.
    pub intents: MoveIntentMap,
    /// Intent outbox.
    pub outbox: O,
}

/// A built, not yet running, world session.
pub struct WorldSessionBuild<M: MotionStep, O: IntentSink, S: FrameSink> {
    /// Simulation loop body.
    pub sim: SimDriver<ClientSim<M, O>>,
    /// Render loop body.
    pub render: RenderLoop<S>,
    /// Network end of the snapshot channel.
    pub snapshots: SnapshotSender<M::State>,
    /// Timeline markers for the render thread's presentation.
    pub markers: MarkerReceiver,
    /// The shared clock.
    pub clock: Arc<dyn HostClock>,
}

/// A running world session; dropping it stops and joins both threads.
pub struct WorldSession<M: MotionStep, O: IntentSink, S: FrameSink> {
    /// Network end of the snapshot channel.
    pub snapshots: SnapshotSender<M::State>,
    /// Timeline markers for the render thread's presentation.
    pub markers: MarkerReceiver,
    /// Render thread (dropped first: it reads what the sim publishes).
    pub render: RenderThread<S>,
    /// Simulation thread.
    pub sim: SimThread<ClientSim<M, O>>,
}

impl<M: MotionStep, O: IntentSink, S: FrameSink> core::fmt::Debug for WorldSession<M, O, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorldSession").finish_non_exhaustive()
    }
}

/// Builds a world session whose first tick is due now.
pub fn build_world_session<M: MotionStep, O: IntentSink, S: FrameSink>(
    config: &WorldSessionConfig,
    parts: WorldSessionParts<M, O>,
    clock: Arc<dyn HostClock>,
    events: Receiver<PlatformEvent>,
    sink: S,
) -> WorldSessionBuild<M, O, S> {
    let mut presentation = config.presentation;
    presentation.interpolation_delay = config.sim.delay.floor;
    presentation.max_remote_extrapolation = config.sim.max_remote_extrapolation;
    let (publisher, reader) = render_world_channel(config.sim.remote_capacity, presentation);
    let (snapshots, inbox) = snapshot_channel(config.snapshot_frames, config.sim.remote_capacity);
    let (marker_tx, markers) = marker_channel(config.sim.remote_capacity.saturating_mul(4).max(64));
    let accumulator = Arc::new(InputAccumulator::new());
    let sim = ClientSim::new(ClientSimParts {
        config: config.sim,
        motion: parts.motion,
        ground: parts.ground,
        dt: config.tick_rate.dt_seconds(),
        initial_state: parts.initial_state,
        accumulator: Arc::clone(&accumulator),
        intents: parts.intents,
        inbox,
        outbox: parts.outbox,
        markers: Some(marker_tx),
    });
    let stepper = FixedStepper::new(config.tick_rate, Tick::ZERO, clock.now(), config.max_catchup);
    let render_loop = RenderLoop::new(RenderLoopParts {
        clock: Arc::clone(&clock),
        reader,
        router: parts.router,
        camera: CameraRig::new(config.look),
        look: config.look_bindings,
        accumulator,
        events,
        pose_capacity: config.sim.remote_capacity.saturating_add(1),
        sink,
    });
    WorldSessionBuild {
        sim: SimDriver::new(stepper, sim, publisher),
        render: render_loop,
        snapshots,
        markers,
        clock,
    }
}

impl<M: MotionStep, O: IntentSink, S: FrameSink> WorldSessionBuild<M, O, S> {
    /// Spawns the simulation and render threads.
    ///
    /// # Errors
    /// The OS error if a thread cannot be spawned; anything already spawned is joined.
    pub fn spawn(
        self,
        config: &WorldSessionConfig,
        frame_interval: Option<Duration>,
    ) -> std::io::Result<WorldSession<M, O, S>> {
        let sim = SimThread::spawn(self.sim, Arc::clone(&self.clock), config.max_sleep)?;
        let render = RenderThread::spawn(self.render, frame_interval)?;
        Ok(WorldSession {
            snapshots: self.snapshots,
            markers: self.markers,
            render,
            sim,
        })
    }
}

impl<S: FrameSink> crate::platform::RunningSession for RenderThread<S> {
    fn is_finished(&self) -> bool {
        RenderThread::is_finished(self)
    }
}

impl<M: MotionStep, O: IntentSink, S: FrameSink> crate::platform::RunningSession for WorldSession<M, O, S> {
    fn is_finished(&self) -> bool {
        self.render.is_finished()
    }
}
