//! The render thread: once per frame it drains platform events through the input router,
//! applies mouse and stick look to the camera, hands action state to the simulation,
//! acquires the newest render world, samples every pose at the frame instant, and submits
//! the frame to a [`FrameSink`] (the renderer in production, a recorder in tests).
//!
//! [`RenderLoop`] is the loop body and runs without a thread or a window.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::sim_thread::ThreadPanicked;
use crate::camera::CameraRig;
use crate::input::accumulator::InputAccumulator;
use crate::input::action::ActionId;
use crate::input::device::RawInput;
use crate::input::router::{ActionFrame, InputRouter};
use crate::render_world::{FramePoses, RenderWorld, RenderWorldReader};
use crate::time::{HostClock, HostInstant};

/// Events from the platform layer to the render thread. Only text and IME events carry
/// heap data.
#[derive(Clone, PartialEq, Debug)]
pub enum PlatformEvent {
    /// A device event.
    Input(RawInput),
    /// The drawable area changed size, in physical pixels.
    Resized {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// The user asked to close the window.
    CloseRequested,
    /// The cursor moved, in physical pixels from the drawable's top-left (UI pointing).
    CursorMoved {
        /// Horizontal position.
        x: f32,
        /// Vertical position.
        y: f32,
    },
    /// Committed character input from the keyboard layout (UI text fields).
    Text(String),
    /// IME composition in progress; empty text ends it (handled on the render thread,
    /// plan 8.6).
    ImePreedit {
        /// The composition.
        text: String,
        /// Caret or selection within it, byte offsets.
        cursor: Option<(usize, usize)>,
    },
    /// IME composition committed.
    ImeCommit(String),
}

/// Everything a sink may read for one frame.
#[derive(Debug)]
pub struct FrameContext<'a> {
    /// Frame number, from 1.
    pub frame: u64,
    /// Frame instant on the host timeline.
    pub now: HostInstant,
    /// Seconds since the previous frame (0 for the first).
    pub frame_dt: f32,
    /// The render world in use.
    pub world: &'a RenderWorld,
    /// Every entity's pose at `now`.
    pub poses: &'a FramePoses,
    /// Camera orientation.
    pub camera: &'a CameraRig,
    /// This frame's action state.
    pub actions: &'a ActionFrame,
}

/// Consumer of frames: the renderer.
pub trait FrameSink: Send + 'static {
    /// The drawable was resized.
    fn resize(&mut self, width: u32, height: u32);
    /// Renders and presents one frame.
    fn submit(&mut self, frame: &FrameContext<'_>);
    /// Sees every input and text event before game actions do (the UI, plan 8.6).
    /// Returning true consumes it: the input router never sees it. Focus loss always
    /// reaches the router too.
    fn ui_event(&mut self, _event: &PlatformEvent) -> bool {
        false
    }
}

/// Which axis actions drive analog look.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LookBindings {
    /// Horizontal look axis.
    pub x: Option<ActionId>,
    /// Vertical look axis.
    pub y: Option<ActionId>,
}

/// Per-frame outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameReport {
    /// Frame number.
    pub frame: u64,
    /// Tick of the render world rendered.
    pub world_tick: crate::core_api::Tick,
    /// Platform events consumed.
    pub events: u32,
    /// True once the platform asked to close.
    pub close_requested: bool,
}

/// The render loop body.
pub struct RenderLoop<S: FrameSink> {
    clock: Arc<dyn HostClock>,
    reader: RenderWorldReader,
    router: InputRouter,
    camera: CameraRig,
    look: LookBindings,
    accumulator: Arc<InputAccumulator>,
    events: Receiver<PlatformEvent>,
    poses: FramePoses,
    sink: S,
    frame: u64,
    last: Option<HostInstant>,
    close_requested: bool,
}

impl<S: FrameSink> core::fmt::Debug for RenderLoop<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RenderLoop")
            .field("frame", &self.frame)
            .field("close_requested", &self.close_requested)
            .finish_non_exhaustive()
    }
}

/// Construction parameters for [`RenderLoop`].
pub struct RenderLoopParts<S: FrameSink> {
    /// Shared host clock.
    pub clock: Arc<dyn HostClock>,
    /// Render world consumer end.
    pub reader: RenderWorldReader,
    /// Input router with its contexts.
    pub router: InputRouter,
    /// Camera.
    pub camera: CameraRig,
    /// Analog look bindings.
    pub look: LookBindings,
    /// Render-to-sim input hand-off.
    pub accumulator: Arc<InputAccumulator>,
    /// Platform events.
    pub events: Receiver<PlatformEvent>,
    /// Pose capacity per frame.
    pub pose_capacity: usize,
    /// The frame sink.
    pub sink: S,
}

impl<S: FrameSink> RenderLoop<S> {
    /// Builds a loop.
    pub fn new(parts: RenderLoopParts<S>) -> Self {
        Self {
            clock: parts.clock,
            reader: parts.reader,
            router: parts.router,
            camera: parts.camera,
            look: parts.look,
            accumulator: parts.accumulator,
            events: parts.events,
            poses: FramePoses::with_capacity(parts.pose_capacity),
            sink: parts.sink,
            frame: 0,
            last: None,
            close_requested: false,
        }
    }

    /// Runs one frame.
    #[expect(clippy::cast_possible_truncation)] // Frame deltas fit f32.
    pub fn frame(&mut self) -> FrameReport {
        let now = self.clock.now();
        let frame_dt = self
            .last
            .map_or(0.0, |l| now.saturating_since(l).as_secs_f64() as f32);
        self.last = Some(now);
        self.frame = self.frame.saturating_add(1);

        let mut events = 0u32;
        loop {
            match self.events.try_recv() {
                Ok(ev) => {
                    events = events.saturating_add(1);
                    match ev {
                        PlatformEvent::Input(raw) => {
                            let consumed = self.sink.ui_event(&ev);
                            if !consumed || raw == RawInput::FocusLost {
                                self.router.handle(raw);
                            }
                        }
                        PlatformEvent::Resized { width, height } => self.sink.resize(width, height),
                        PlatformEvent::CloseRequested => self.close_requested = true,
                        PlatformEvent::CursorMoved { .. }
                        | PlatformEvent::Text(_)
                        | PlatformEvent::ImePreedit { .. }
                        | PlatformEvent::ImeCommit(_) => {
                            let _ = self.sink.ui_event(&ev);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.close_requested = true;
                    break;
                }
            }
        }

        let actions = self.router.end_frame();
        self.camera.apply_mouse(actions.look_dx, actions.look_dy);
        let sx = self.look.x.map_or(0.0, |a| actions.axis(a));
        let sy = self.look.y.map_or(0.0, |a| actions.axis(a));
        self.camera.apply_stick(sx, sy, frame_dt);
        self.accumulator.merge_frame(&actions, self.camera.sample());

        let world = self.reader.acquire();
        world.sample_into(now, &mut self.poses);
        let ctx = FrameContext {
            frame: self.frame,
            now,
            frame_dt,
            world,
            poses: &self.poses,
            camera: &self.camera,
            actions: &actions,
        };
        self.sink.submit(&ctx);
        FrameReport {
            frame: self.frame,
            world_tick: world.tick(),
            events,
            close_requested: self.close_requested,
        }
    }

    /// The camera.
    pub fn camera(&self) -> &CameraRig {
        &self.camera
    }

    /// The sink.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The sink, mutably (to attach a UI before the loop starts).
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
}

/// A running render thread. Stopping it (explicitly or by drop) joins it.
#[derive(Debug)]
pub struct RenderThread<S: FrameSink> {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<RenderLoop<S>>>,
}

impl<S: FrameSink> RenderThread<S> {
    /// Spawns the loop. With `frame_interval` set, frames are paced to it by sleeping;
    /// without, pacing is the sink's (vsync in `submit`). The loop also ends when the
    /// platform asks to close.
    ///
    /// # Errors
    /// The OS error if the thread cannot be spawned.
    pub fn spawn(mut render: RenderLoop<S>, frame_interval: Option<Duration>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("mantis-render".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Acquire) {
                    let start = render.clock.now();
                    let report = render.frame();
                    if report.close_requested {
                        break;
                    }
                    if let Some(interval) = frame_interval {
                        let spent = render.clock.now().saturating_since(start);
                        if let Some(rest) = interval.checked_sub(spent) {
                            thread::park_timeout(rest);
                        }
                    }
                }
                render
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// True once the loop has ended on its own (close requested).
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops and joins the thread, returning the loop.
    ///
    /// # Errors
    /// [`ThreadPanicked`] if the loop panicked.
    pub fn stop(mut self) -> Result<RenderLoop<S>, ThreadPanicked> {
        self.stop_and_join().ok_or(ThreadPanicked("render"))
    }

    fn stop_and_join(&mut self) -> Option<RenderLoop<S>> {
        self.stop.store(true, Ordering::Release);
        let h = self.handle.take()?;
        h.thread().unpark();
        h.join().ok()
    }
}

impl<S: FrameSink> Drop for RenderThread<S> {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}
