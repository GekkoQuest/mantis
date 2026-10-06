//! Runtime state of one mixer-graph bus.

use mantis_formats::mixer_graph::Bus;

use crate::dsp::{EffectState, Ramp};

/// A bus: its scratch buffer, gain ramp, and effect chain.
#[derive(Clone, Debug)]
pub(crate) struct BusState {
    pub(crate) id: u32,
    /// Index of the parent bus; `None` for the master.
    pub(crate) parent: Option<usize>,
    pub(crate) gain: Ramp,
    effects: Vec<EffectState>,
    /// One block of interleaved stereo frames, allocated once.
    pub(crate) buffer: Vec<[f32; 2]>,
}

impl BusState {
    pub(crate) fn new(bus: &Bus, parent: Option<usize>, rate: u32, block_frames: usize) -> BusState {
        BusState {
            id: bus.id,
            parent,
            gain: Ramp::new(bus.gain),
            effects: bus.effects.iter().map(|e| EffectState::new(*e, rate)).collect(),
            buffer: vec![[0.0; 2]; block_frames],
        }
    }

    /// Applies the gain ramp, then the effects in order.
    pub(crate) fn process(&mut self, frames: &mut [[f32; 2]]) {
        for frame in frames.iter_mut() {
            let g = self.gain.next();
            frame[0] *= g;
            frame[1] *= g;
        }
        for e in &mut self.effects {
            e.process(frames);
        }
    }
}
