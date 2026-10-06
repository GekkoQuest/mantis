//! Actions: the named, typed things input means ("move forward", "jump", "look x").
//!
//! Actions are data: a package defines its action table, and bindings map device inputs
//! onto them per context. Gameplay reads actions, never devices.

use super::InputError;

/// Maximum actions in one table. Fixed so per-frame state is a fixed-size value.
pub const MAX_ACTIONS: usize = 128;

/// Index of an action in its [`ActionTable`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ActionId(u8);

impl ActionId {
    /// Dense index in `0..MAX_ACTIONS`.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Whether an action is a button or an axis.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ActionKind {
    /// Held or not, with press and release edges.
    Button,
    /// A value in [-1, 1].
    Axis,
}

/// One action definition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ActionDef {
    /// Stable name, as used by binding files.
    pub name: String,
    /// Kind.
    pub kind: ActionKind,
}

/// The action table for a session.
#[derive(Clone, Debug, Default)]
pub struct ActionTable {
    defs: Vec<ActionDef>,
}

impl ActionTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines an action.
    ///
    /// # Errors
    /// [`InputError::DuplicateAction`] if the name exists, [`InputError::TooManyActions`]
    /// past [`MAX_ACTIONS`], [`InputError::InvalidName`] for an empty name.
    pub fn define(&mut self, name: &str, kind: ActionKind) -> Result<ActionId, InputError> {
        if name.is_empty() {
            return Err(InputError::InvalidName);
        }
        if self.find(name).is_some() {
            return Err(InputError::DuplicateAction);
        }
        let idx = u8::try_from(self.defs.len()).map_err(|_| InputError::TooManyActions)?;
        if usize::from(idx) >= MAX_ACTIONS {
            return Err(InputError::TooManyActions);
        }
        self.defs.push(ActionDef {
            name: name.to_owned(),
            kind,
        });
        Ok(ActionId(idx))
    }

    /// Looks an action up by name.
    pub fn find(&self, name: &str) -> Option<ActionId> {
        self.defs
            .iter()
            .position(|d| d.name == name)
            .and_then(|i| u8::try_from(i).ok())
            .map(ActionId)
    }

    /// The id at dense index `index`, if defined.
    pub fn id_at(&self, index: u8) -> Option<ActionId> {
        (usize::from(index) < self.defs.len()).then_some(ActionId(index))
    }

    /// The definition of `id`.
    pub fn get(&self, id: ActionId) -> Option<&ActionDef> {
        self.defs.get(id.index())
    }

    /// The kind of `id`.
    pub fn kind(&self, id: ActionId) -> Option<ActionKind> {
        self.get(id).map(|d| d.kind)
    }

    /// Number of actions.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// A set of actions as a fixed-size bit set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ActionBits(u128);

impl ActionBits {
    /// The empty set.
    pub const EMPTY: ActionBits = ActionBits(0);

    /// True when `id` is in the set.
    pub const fn contains(self, id: ActionId) -> bool {
        self.0 & (1u128 << id.0) != 0
    }

    /// Adds `id`.
    pub fn insert(&mut self, id: ActionId) {
        self.0 |= 1u128 << id.0;
    }

    /// Removes `id`.
    pub fn remove(&mut self, id: ActionId) {
        self.0 &= !(1u128 << id.0);
    }

    /// Union.
    #[must_use]
    pub const fn union(self, other: ActionBits) -> ActionBits {
        ActionBits(self.0 | other.0)
    }

    /// True when empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}
