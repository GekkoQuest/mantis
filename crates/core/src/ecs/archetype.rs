//! Archetype tables: one dense structure-of-arrays table per component set.

use std::collections::BTreeMap;

use super::column::Column;
use super::component::{ComponentId, ComponentSet};
use super::entity::EntityId;

/// One archetype: every entity with exactly the component set `set`.
///
/// Invariant: `columns.len() == ids.len()`, `ids` is strictly ascending, and
/// every column has exactly `entities.len()` rows.
pub struct Archetype {
    pub(crate) set: ComponentSet,
    pub(crate) ids: Vec<ComponentId>,
    pub(crate) columns: Vec<Box<dyn Column>>,
    pub(crate) entities: Vec<EntityId>,
    pub(crate) add_edges: BTreeMap<ComponentId, u32>,
    pub(crate) remove_edges: BTreeMap<ComponentId, u32>,
}

impl Archetype {
    pub(crate) fn new(set: ComponentSet, ids: Vec<ComponentId>, columns: Vec<Box<dyn Column>>) -> Self {
        Self {
            set,
            ids,
            columns,
            entities: Vec::new(),
            add_edges: BTreeMap::new(),
            remove_edges: BTreeMap::new(),
        }
    }

    /// Index of the column holding `id`.
    pub(crate) fn column_index(&self, id: ComponentId) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }

    pub(crate) fn column(&self, id: ComponentId) -> Option<&dyn Column> {
        let idx = self.column_index(id)?;
        self.columns.get(idx).map(AsRef::as_ref)
    }

    pub(crate) fn column_mut(&mut self, id: ComponentId) -> Option<&mut dyn Column> {
        let idx = self.column_index(id)?;
        self.columns.get_mut(idx).map(AsMut::as_mut)
    }

    /// Restores the length invariant after a failed partial push.
    pub(crate) fn truncate_columns(&mut self) {
        let len = self.entities.len();
        for c in &mut self.columns {
            c.truncate(len);
        }
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.entities.reserve(additional);
        for c in &mut self.columns {
            c.reserve(additional);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }
}
