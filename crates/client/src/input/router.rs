//! The input router: runs on the render thread, turns device events into per-frame
//! action state through the active context stack.
//!
//! The router never allocates after construction: held inputs, the context stack, and
//! action state are fixed-capacity.

use super::InputError;
use super::action::{ActionBits, ActionId, ActionKind, ActionTable, MAX_ACTIONS};
use super::binding::{Binding, ContextId, ContextTable, MAX_CONTEXTS, SourceKey};
use super::device::{AxisSource, ButtonSource, RawInput};

/// Maximum simultaneously held buttons tracked. Further presses are ignored (fail closed).
pub const MAX_HELD: usize = 32;

/// Action state for one rendered frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ActionFrame {
    /// Button actions held at the end of the frame.
    pub held: ActionBits,
    /// Button actions that went from released to held during the frame.
    pub pressed: ActionBits,
    /// Button actions that went from held to released during the frame.
    pub released: ActionBits,
    /// Axis action values, in [-1, 1], indexed by [`ActionId::index`].
    pub axes: [f32; MAX_ACTIONS],
    /// Raw mouse motion this frame, in counts.
    pub look_dx: f32,
    /// Raw mouse motion this frame, in counts.
    pub look_dy: f32,
}

impl Default for ActionFrame {
    fn default() -> Self {
        Self {
            held: ActionBits::EMPTY,
            pressed: ActionBits::EMPTY,
            released: ActionBits::EMPTY,
            axes: [0.0; MAX_ACTIONS],
            look_dx: 0.0,
            look_dy: 0.0,
        }
    }
}

impl ActionFrame {
    /// Value of axis action `id` (0 for unknown ids).
    pub fn axis(&self, id: ActionId) -> f32 {
        self.axes.get(id.index()).copied().unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, Debug)]
struct Held {
    source: ButtonSource,
    action: ActionId,
    /// `Some` for a button contributing to an axis action.
    axis_value: Option<f32>,
}

/// Routes device events through the context stack.
#[derive(Clone, Debug)]
pub struct InputRouter {
    actions: ActionTable,
    contexts: ContextTable,
    stack: Vec<ContextId>,
    held: Vec<Held>,
    hold_count: [u8; MAX_ACTIONS],
    pressed: ActionBits,
    released: ActionBits,
    analog: [f32; AxisSource::COUNT],
    look_dx: f32,
    look_dy: f32,
    ignored_presses: u32,
}

impl InputRouter {
    /// A router over `actions` and `contexts` with an empty stack.
    pub fn new(actions: ActionTable, contexts: ContextTable) -> Self {
        Self {
            actions,
            contexts,
            stack: Vec::with_capacity(MAX_CONTEXTS),
            held: Vec::with_capacity(MAX_HELD),
            hold_count: [0; MAX_ACTIONS],
            pressed: ActionBits::EMPTY,
            released: ActionBits::EMPTY,
            analog: [0.0; AxisSource::COUNT],
            look_dx: 0.0,
            look_dy: 0.0,
            ignored_presses: 0,
        }
    }

    /// The action table.
    pub fn actions(&self) -> &ActionTable {
        &self.actions
    }

    /// The context table.
    pub fn contexts(&self) -> &ContextTable {
        &self.contexts
    }

    /// Mutable context table, for rebinding. Releases every held input, so a rebind
    /// never leaves an action stuck.
    pub fn contexts_mut(&mut self) -> &mut ContextTable {
        self.release_all();
        &mut self.contexts
    }

    /// The active stack, bottom first.
    pub fn stack(&self) -> &[ContextId] {
        &self.stack
    }

    /// Pushes a context on top. Held inputs are released so nothing stays stuck across
    /// the change; the user re-presses under the new context.
    ///
    /// # Errors
    /// [`InputError::UnknownContext`], or [`InputError::TooManyContexts`] when the stack is full.
    pub fn push_context(&mut self, ctx: ContextId) -> Result<(), InputError> {
        if self.contexts.get(ctx).is_none() {
            return Err(InputError::UnknownContext);
        }
        if self.stack.len() >= MAX_CONTEXTS {
            return Err(InputError::TooManyContexts);
        }
        self.release_all();
        self.stack.push(ctx);
        Ok(())
    }

    /// Pops the top context, releasing held inputs.
    pub fn pop_context(&mut self) -> Option<ContextId> {
        let c = self.stack.pop();
        if c.is_some() {
            self.release_all();
        }
        c
    }

    /// Presses ignored because [`MAX_HELD`] buttons were already held.
    pub fn ignored_presses(&self) -> u32 {
        self.ignored_presses
    }

    /// Feeds one device event.
    pub fn handle(&mut self, ev: RawInput) {
        match ev {
            RawInput::Button {
                source,
                pressed: true,
            } => self.press(source),
            RawInput::Button {
                source,
                pressed: false,
            } => self.release(source),
            RawInput::Axis { source, value } => {
                let v = if value.is_finite() {
                    value.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                if let Some(slot) = self.analog.get_mut(source.index()) {
                    *slot = v;
                }
            }
            RawInput::MouseMotion { dx, dy } => {
                if dx.is_finite() && dy.is_finite() {
                    self.look_dx += dx;
                    self.look_dy += dy;
                }
            }
            RawInput::FocusLost => {
                self.release_all();
                self.analog = [0.0; AxisSource::COUNT];
            }
        }
    }

    /// Resolves which binding, if any, consumes `key` through the stack.
    fn resolve(&self, key: SourceKey) -> Option<(ActionId, Binding)> {
        for ctx in self.stack.iter().rev() {
            let Some(c) = self.contexts.get(*ctx) else {
                continue;
            };
            if let Some(hit) = c.binding_for(key) {
                return Some(hit);
            }
            if c.exclusive() {
                return None;
            }
        }
        None
    }

    fn press(&mut self, source: ButtonSource) {
        if self.held.iter().any(|h| h.source == source) {
            return; // OS key repeat
        }
        let Some((action, binding)) = self.resolve(SourceKey::Button(source)) else {
            return;
        };
        if self.held.len() >= MAX_HELD {
            self.ignored_presses = self.ignored_presses.saturating_add(1);
            return;
        }
        let axis_value = match binding {
            Binding::ButtonAxis { value, .. } => Some(value),
            Binding::Button(_) | Binding::Axis { .. } => None,
        };
        self.held.push(Held {
            source,
            action,
            axis_value,
        });
        if axis_value.is_none()
            && let Some(n) = self.hold_count.get_mut(action.index())
        {
            if *n == 0 {
                self.pressed.insert(action);
            }
            *n = n.saturating_add(1);
        }
    }

    fn release(&mut self, source: ButtonSource) {
        let Some(pos) = self.held.iter().position(|h| h.source == source) else {
            return;
        };
        let h = self.held.swap_remove(pos);
        self.release_held(h);
    }

    fn release_held(&mut self, h: Held) {
        if h.axis_value.is_none()
            && let Some(n) = self.hold_count.get_mut(h.action.index())
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.released.insert(h.action);
            }
        }
    }

    /// Releases every held button, emitting release edges.
    pub fn release_all(&mut self) {
        while let Some(h) = self.held.pop() {
            self.release_held(h);
        }
    }

    /// Ends the frame: computes axis values, returns the frame's action state, and clears
    /// per-frame edges and look motion.
    pub fn end_frame(&mut self) -> ActionFrame {
        let mut axes = [0.0f32; MAX_ACTIONS];
        for h in &self.held {
            if let (Some(v), Some(slot)) = (h.axis_value, axes.get_mut(h.action.index())) {
                *slot += v;
            }
        }
        // Analog bindings: each axis source is consumed by the topmost context binding it.
        for source_index in 0..AxisSource::COUNT {
            let Some(source) = axis_source_from_index(source_index) else {
                continue;
            };
            let raw = self.analog.get(source_index).copied().unwrap_or(0.0);
            if raw == 0.0 {
                continue;
            }
            if let Some((action, Binding::Axis { scale, deadzone, .. })) =
                self.resolve(SourceKey::Axis(source))
                && let Some(slot) = axes.get_mut(action.index())
            {
                *slot += apply_deadzone(raw, deadzone) * scale;
            }
        }
        let mut held = ActionBits::EMPTY;
        for (i, n) in self.hold_count.iter().enumerate() {
            if *n > 0
                && let Ok(i) = u8::try_from(i)
                && let Some(id) = self.action_id(i)
            {
                held.insert(id);
            }
        }
        for a in &mut axes {
            *a = a.clamp(-1.0, 1.0);
        }
        let frame = ActionFrame {
            held,
            pressed: self.pressed,
            released: self.released,
            axes,
            look_dx: self.look_dx,
            look_dy: self.look_dy,
        };
        self.pressed = ActionBits::EMPTY;
        self.released = ActionBits::EMPTY;
        self.look_dx = 0.0;
        self.look_dy = 0.0;
        frame
    }

    fn action_id(&self, index: u8) -> Option<ActionId> {
        let id = self.actions.id_at(index)?;
        (self.actions.kind(id) == Some(ActionKind::Button)).then_some(id)
    }
}

fn axis_source_from_index(i: usize) -> Option<AxisSource> {
    use super::device::GamepadAxis as G;
    let a = match i {
        0 => G::LeftStickX,
        1 => G::LeftStickY,
        2 => G::RightStickX,
        3 => G::RightStickY,
        4 => G::LeftTrigger,
        5 => G::RightTrigger,
        _ => return None,
    };
    Some(AxisSource::Gamepad(a))
}

/// Radial-free per-axis dead zone with rescale, so output spans the full range.
fn apply_deadzone(v: f32, deadzone: f32) -> f32 {
    let m = v.abs();
    if m <= deadzone {
        0.0
    } else {
        ((m - deadzone) / (1.0 - deadzone)).min(1.0) * v.signum()
    }
}
