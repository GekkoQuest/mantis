//! The hand-off of input from the render thread to the simulation thread.
//!
//! The render thread merges every frame's action state and look sample into an
//! [`InputAccumulator`]; the simulation thread takes it once per tick. Edges are
//! combined (bitwise OR) across frames, so a press and release that both happen between
//! two ticks still reach the simulation: a tap is never lost because the frame rate is
//! higher than the tick rate. Held state, axes, and look are latest-wins.

use std::sync::{Mutex, PoisonError};

use super::action::{ActionBits, ActionId, MAX_ACTIONS};
use super::router::ActionFrame;
use crate::camera::LookSample;

/// Input for one simulation tick.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TickInput {
    /// Button actions held at the latest frame.
    pub held: ActionBits,
    /// Button actions pressed during any frame since the last tick.
    pub pressed: ActionBits,
    /// Button actions released during any frame since the last tick.
    pub released: ActionBits,
    /// Axis values at the latest frame.
    pub axes: [f32; MAX_ACTIONS],
    /// Quantized look at the latest frame.
    pub look: LookSample,
    /// Frames merged since the last tick (0 when the render thread produced none).
    pub frames: u32,
}

impl Default for TickInput {
    fn default() -> Self {
        Self {
            held: ActionBits::EMPTY,
            pressed: ActionBits::EMPTY,
            released: ActionBits::EMPTY,
            axes: [0.0; MAX_ACTIONS],
            look: LookSample::default(),
            frames: 0,
        }
    }
}

impl TickInput {
    /// True when `id` is held now or was pressed at any point since the last tick.
    pub fn active(&self, id: ActionId) -> bool {
        self.held.contains(id) || self.pressed.contains(id)
    }

    /// Value of axis action `id`.
    pub fn axis(&self, id: ActionId) -> f32 {
        self.axes.get(id.index()).copied().unwrap_or(0.0)
    }
}

/// Shared render-to-sim input slot. Never allocates.
#[derive(Debug, Default)]
pub struct InputAccumulator {
    inner: Mutex<TickInput>,
}

impl InputAccumulator {
    /// An empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render thread: merges one frame.
    pub fn merge_frame(&self, frame: &ActionFrame, look: LookSample) {
        let mut t = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        t.held = frame.held;
        t.pressed = t.pressed.union(frame.pressed);
        t.released = t.released.union(frame.released);
        t.axes = frame.axes;
        t.look = look;
        t.frames = t.frames.saturating_add(1);
    }

    /// Sim thread: takes the input for one tick and clears the edges.
    pub fn take(&self) -> TickInput {
        let mut t = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let out = *t;
        t.pressed = ActionBits::EMPTY;
        t.released = ActionBits::EMPTY;
        t.frames = 0;
        out
    }
}
