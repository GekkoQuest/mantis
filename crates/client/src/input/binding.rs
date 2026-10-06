//! Bindings and contexts: which device inputs drive which actions, per context.
//!
//! A context is a named action set ("gameplay", "menu", "text entry"). Contexts are
//! stacked at runtime; the topmost context that binds a device input consumes it. An
//! exclusive context consumes everything, bound or not, so lower contexts see nothing
//! while it is on top. Bindings are rebindable at runtime and validated: a binding must
//! match its action's kind, and one device input drives at most one action per context.

use super::InputError;
use super::action::{ActionId, ActionKind, ActionTable};
use super::device::{AxisSource, ButtonSource};

/// Maximum contexts in one table.
pub const MAX_CONTEXTS: usize = 32;

/// One binding of a device input to an action.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Binding {
    /// A button drives a button action.
    Button(ButtonSource),
    /// A button contributes a fixed value to an axis action while held (for example,
    /// a key contributing +1 to "move y").
    ButtonAxis {
        /// The button.
        source: ButtonSource,
        /// Contribution while held, in [-1, 1].
        value: f32,
    },
    /// An analog axis drives an axis action.
    Axis {
        /// The axis.
        source: AxisSource,
        /// Multiplier applied after the dead zone, in [-1, 1] (negative inverts).
        scale: f32,
        /// Inputs with magnitude below this read as zero, in [0, 1).
        deadzone: f32,
    },
}

/// The device input a binding listens to, used for conflict detection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SourceKey {
    /// A button.
    Button(ButtonSource),
    /// An axis.
    Axis(AxisSource),
}

impl Binding {
    /// The device input this binding listens to.
    pub fn source(&self) -> SourceKey {
        match *self {
            Binding::Button(s) | Binding::ButtonAxis { source: s, .. } => SourceKey::Button(s),
            Binding::Axis { source, .. } => SourceKey::Axis(source),
        }
    }

    /// The action kind this binding can drive.
    pub fn action_kind(&self) -> ActionKind {
        match self {
            Binding::Button(_) => ActionKind::Button,
            Binding::ButtonAxis { .. } | Binding::Axis { .. } => ActionKind::Axis,
        }
    }

    fn validate(&self) -> Result<(), InputError> {
        let ok = match *self {
            Binding::Button(_) => true,
            Binding::ButtonAxis { value, .. } => value.is_finite() && (-1.0..=1.0).contains(&value),
            Binding::Axis { scale, deadzone, .. } => {
                scale.is_finite()
                    && (-1.0..=1.0).contains(&scale)
                    && deadzone.is_finite()
                    && (0.0..1.0).contains(&deadzone)
            }
        };
        if ok {
            Ok(())
        } else {
            Err(InputError::InvalidBinding)
        }
    }
}

/// Index of a context in its [`ContextTable`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ContextId(u8);

impl ContextId {
    /// Dense index in `0..MAX_CONTEXTS`.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One context: a named action set with its bindings.
#[derive(Clone, Debug)]
pub struct InputContext {
    name: String,
    exclusive: bool,
    bindings: Vec<(ActionId, Binding)>,
}

impl InputContext {
    /// Name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this context consumes every input while on top.
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    /// All bindings in this context.
    pub fn bindings(&self) -> &[(ActionId, Binding)] {
        &self.bindings
    }

    /// The binding for `source`, if any.
    pub fn binding_for(&self, source: SourceKey) -> Option<(ActionId, Binding)> {
        self.bindings.iter().copied().find(|(_, b)| b.source() == source)
    }
}

/// All contexts of a session.
#[derive(Clone, Debug, Default)]
pub struct ContextTable {
    contexts: Vec<InputContext>,
}

impl ContextTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines a context.
    ///
    /// # Errors
    /// [`InputError::DuplicateContext`], [`InputError::TooManyContexts`], or
    /// [`InputError::InvalidName`].
    pub fn define(&mut self, name: &str, exclusive: bool) -> Result<ContextId, InputError> {
        if name.is_empty() {
            return Err(InputError::InvalidName);
        }
        if self.find(name).is_some() {
            return Err(InputError::DuplicateContext);
        }
        let idx = u8::try_from(self.contexts.len()).map_err(|_| InputError::TooManyContexts)?;
        if usize::from(idx) >= MAX_CONTEXTS {
            return Err(InputError::TooManyContexts);
        }
        self.contexts.push(InputContext {
            name: name.to_owned(),
            exclusive,
            bindings: Vec::new(),
        });
        Ok(ContextId(idx))
    }

    /// Looks a context up by name.
    pub fn find(&self, name: &str) -> Option<ContextId> {
        self.contexts
            .iter()
            .position(|c| c.name == name)
            .and_then(|i| u8::try_from(i).ok())
            .map(ContextId)
    }

    /// The context `id`.
    pub fn get(&self, id: ContextId) -> Option<&InputContext> {
        self.contexts.get(id.index())
    }

    /// Adds a binding.
    ///
    /// # Errors
    /// [`InputError::UnknownContext`], [`InputError::UnknownAction`],
    /// [`InputError::KindMismatch`], [`InputError::InvalidBinding`], or
    /// [`InputError::Conflict`] if the device input is already bound in this context.
    pub fn bind(
        &mut self,
        actions: &ActionTable,
        ctx: ContextId,
        action: ActionId,
        binding: Binding,
    ) -> Result<(), InputError> {
        let kind = actions.kind(action).ok_or(InputError::UnknownAction)?;
        if kind != binding.action_kind() {
            return Err(InputError::KindMismatch);
        }
        binding.validate()?;
        let c = self
            .contexts
            .get_mut(ctx.index())
            .ok_or(InputError::UnknownContext)?;
        if let Some((existing, _)) = c.binding_for(binding.source()) {
            return Err(InputError::Conflict { existing });
        }
        c.bindings.push((action, binding));
        Ok(())
    }

    /// Removes the binding of `source` in `ctx`, returning the action it drove.
    ///
    /// # Errors
    /// [`InputError::UnknownContext`] or [`InputError::NotBound`].
    pub fn unbind(&mut self, ctx: ContextId, source: SourceKey) -> Result<ActionId, InputError> {
        let c = self
            .contexts
            .get_mut(ctx.index())
            .ok_or(InputError::UnknownContext)?;
        let pos = c
            .bindings
            .iter()
            .position(|(_, b)| b.source() == source)
            .ok_or(InputError::NotBound)?;
        Ok(c.bindings.remove(pos).0)
    }

    /// Replaces the binding of `old` for `action` in `ctx` with `new`. Atomic: on error
    /// the old binding is kept.
    ///
    /// # Errors
    /// [`InputError::NotBound`] if `action` is not bound to `old` in `ctx`, or any error of
    /// [`ContextTable::bind`] for `new` (including a conflict with another action).
    pub fn rebind(
        &mut self,
        actions: &ActionTable,
        ctx: ContextId,
        action: ActionId,
        old: SourceKey,
        new: Binding,
    ) -> Result<(), InputError> {
        let c = self.contexts.get(ctx.index()).ok_or(InputError::UnknownContext)?;
        let pos = c
            .bindings
            .iter()
            .position(|(a, b)| *a == action && b.source() == old)
            .ok_or(InputError::NotBound)?;
        let saved = c.bindings.get(pos).copied().ok_or(InputError::NotBound)?;
        if let Some(cm) = self.contexts.get_mut(ctx.index()) {
            cm.bindings.remove(pos);
        }
        match self.bind(actions, ctx, action, new) {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Some(cm) = self.contexts.get_mut(ctx.index()) {
                    cm.bindings.insert(pos.min(cm.bindings.len()), saved);
                }
                Err(e)
            }
        }
    }
}
