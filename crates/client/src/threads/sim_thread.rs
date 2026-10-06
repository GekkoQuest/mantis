//! The simulation thread: a fixed-tick loop that runs a [`TickHandler`] and publishes one
//! render world per tick.
//!
//! [`SimDriver`] is the loop body and runs without a thread, so tests drive it tick by
//! tick against a manual clock. [`SimThread`] runs a driver on its own thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::core_api::Tick;
use crate::render_world::{RenderWorld, RenderWorldPublisher};
use crate::time::{FixedStepper, HostClock, HostInstant, StepBatch};

/// Game logic run once per tick on the simulation thread.
pub trait TickHandler: Send + 'static {
    /// Runs `tick`, which is due at host instant `tick_time`, and fills `world` (already
    /// cleared and stamped for this tick) with what the render thread may see.
    fn tick(&mut self, tick: Tick, tick_time: HostInstant, world: &mut RenderWorld);
}

/// Sees the handler after every tick (a dev-build inspector copies what it shows).
pub type TickObserver<H> = Box<dyn FnMut(&H, Tick) + Send>;

/// The simulation loop body.
pub struct SimDriver<H: TickHandler> {
    stepper: FixedStepper,
    handler: H,
    publisher: RenderWorldPublisher,
    observer: Option<TickObserver<H>>,
}

impl<H: TickHandler> core::fmt::Debug for SimDriver<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimDriver")
            .field("stepper", &self.stepper)
            .field("observed", &self.observer.is_some())
            .finish_non_exhaustive()
    }
}

impl<H: TickHandler> SimDriver<H> {
    /// A driver over `handler`, publishing through `publisher`.
    pub fn new(stepper: FixedStepper, handler: H, publisher: RenderWorldPublisher) -> Self {
        Self {
            stepper,
            handler,
            publisher,
            observer: None,
        }
    }

    /// Calls `observer` with the handler after every tick (or stops calling one).
    pub fn set_observer(&mut self, observer: Option<TickObserver<H>>) {
        self.observer = observer;
    }

    /// Runs every tick due at `now`, publishing after each.
    pub fn run_due(&mut self, now: HostInstant) -> StepBatch {
        let batch = self.stepper.poll(now);
        for tick in batch.ticks() {
            let t = self.stepper.tick_time(tick);
            let world = self.publisher.back_mut();
            world.begin(tick, t);
            self.handler.tick(tick, t, world);
            let _ = self.publisher.publish();
            if let Some(observe) = self.observer.as_mut() {
                observe(&self.handler, tick);
            }
        }
        batch
    }

    /// Time until the next tick is due.
    pub fn until_next(&self, now: HostInstant) -> Duration {
        self.stepper.until_next(now)
    }

    /// The handler.
    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// The stepper.
    pub fn stepper(&self) -> &FixedStepper {
        &self.stepper
    }

    /// Total publishes.
    pub fn published(&self) -> u64 {
        self.publisher.published()
    }
}

/// Errors from joining a client thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadPanicked(pub &'static str);

impl core::fmt::Display for ThreadPanicked {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} thread panicked", self.0)
    }
}

impl std::error::Error for ThreadPanicked {}

/// A running simulation thread. Stopping it (explicitly or by drop) joins it.
#[derive(Debug)]
pub struct SimThread<H: TickHandler> {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<SimDriver<H>>>,
}

impl<H: TickHandler> SimThread<H> {
    /// Spawns the loop. It sleeps until the next tick is due, at most `max_sleep` at a
    /// time so a stop request or a clock change is noticed promptly.
    ///
    /// # Errors
    /// The OS error if the thread cannot be spawned.
    pub fn spawn(
        mut driver: SimDriver<H>,
        clock: Arc<dyn HostClock>,
        max_sleep: Duration,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::Builder::new().name("mantis-sim".into()).spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                let now = clock.now();
                let _ = driver.run_due(now);
                let wait = driver.until_next(clock.now()).min(max_sleep);
                if !wait.is_zero() {
                    thread::park_timeout(wait);
                }
            }
            driver
        })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Wakes the thread early (after a manual clock advance, for example).
    pub fn wake(&self) {
        if let Some(h) = &self.handle {
            h.thread().unpark();
        }
    }

    /// Stops and joins the thread, returning the driver.
    ///
    /// # Errors
    /// [`ThreadPanicked`] if the loop panicked.
    pub fn stop(mut self) -> Result<SimDriver<H>, ThreadPanicked> {
        self.stop_and_join().ok_or(ThreadPanicked("simulation"))
    }

    fn stop_and_join(&mut self) -> Option<SimDriver<H>> {
        self.stop.store(true, Ordering::Release);
        let h = self.handle.take()?;
        h.thread().unpark();
        h.join().ok()
    }
}

impl<H: TickHandler> Drop for SimThread<H> {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}
