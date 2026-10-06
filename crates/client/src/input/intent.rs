//! Turning actions into intents on the simulation thread.
//!
//! The mapping from actions to movement buttons is data. The resulting move intent
//! carries the quantized look the render thread sampled, so the predictor and the server
//! integrate the same yaw.

use super::InputError;
use super::accumulator::TickInput;
use super::action::{ActionId, ActionKind, ActionTable};
use crate::core_api::{AimAngles, InputSeq, MoveButtons, MoveInput, Tick};

/// An axis action mapped onto a pair of movement buttons by threshold.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AxisButtons {
    /// The axis action.
    pub action: ActionId,
    /// Magnitude at or above which the button is set, in (0, 1].
    pub threshold: f32,
    /// Set when the axis is at or above `threshold`.
    pub positive: MoveButtons,
    /// Set when the axis is at or below `-threshold`.
    pub negative: MoveButtons,
}

/// Data-defined mapping from actions to move intent buttons.
#[derive(Clone, Debug, Default)]
pub struct MoveIntentMap {
    buttons: Vec<(ActionId, MoveButtons)>,
    axes: Vec<AxisButtons>,
}

impl MoveIntentMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps a button action to movement buttons.
    ///
    /// # Errors
    /// [`InputError::UnknownAction`] or [`InputError::KindMismatch`].
    pub fn map_button(
        &mut self,
        actions: &ActionTable,
        action: ActionId,
        buttons: MoveButtons,
    ) -> Result<(), InputError> {
        match actions.kind(action) {
            Some(ActionKind::Button) => {
                self.buttons.push((action, buttons));
                Ok(())
            }
            Some(ActionKind::Axis) => Err(InputError::KindMismatch),
            None => Err(InputError::UnknownAction),
        }
    }

    /// Maps an axis action to movement buttons by threshold.
    ///
    /// # Errors
    /// [`InputError::UnknownAction`], [`InputError::KindMismatch`], or
    /// [`InputError::InvalidBinding`] for a threshold outside (0, 1].
    pub fn map_axis(&mut self, actions: &ActionTable, mapping: AxisButtons) -> Result<(), InputError> {
        match actions.kind(mapping.action) {
            Some(ActionKind::Axis) => {}
            Some(ActionKind::Button) => return Err(InputError::KindMismatch),
            None => return Err(InputError::UnknownAction),
        }
        if !(mapping.threshold > 0.0 && mapping.threshold <= 1.0) {
            return Err(InputError::InvalidBinding);
        }
        self.axes.push(mapping);
        Ok(())
    }

    /// Movement buttons for a tick's input.
    pub fn buttons(&self, input: &TickInput) -> MoveButtons {
        let mut b = MoveButtons::NONE;
        for (action, bits) in &self.buttons {
            if input.active(*action) {
                b = b.with(*bits);
            }
        }
        for m in &self.axes {
            let v = input.axis(m.action);
            if v >= m.threshold {
                b = b.with(m.positive);
            } else if v <= -m.threshold {
                b = b.with(m.negative);
            }
        }
        b
    }

    /// Builds the move intent for `tick` with sequence number `seq`.
    pub fn build(&self, input: &TickInput, seq: InputSeq, tick: Tick) -> MoveInput {
        MoveInput {
            seq,
            tick,
            buttons: self.buttons(input),
            yaw: input.look.yaw,
            aim: AimAngles {
                yaw: input.look.yaw,
                pitch: input.look.pitch,
            },
        }
    }
}
