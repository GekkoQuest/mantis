//! Type-erased structure-of-arrays columns, with no unsafe code.
//!
//! A column is a `Vec<T>` of component values plus a parallel `Vec<Tick>` of
//! change ticks, behind the object-safe [`Column`] trait. Typed access is a
//! checked `Any` downcast, one per archetype per query, never per entity.

use core::any::Any;

use super::component::Component;
use crate::hash::StableHasher;
use crate::time::Tick;
use crate::wire::{DecodeError, Decoder, Encoder};

/// Object-safe interface over a [`TypedColumn`]. Internal to the ECS.
pub trait Column: Any + Send + Sync {
    /// Number of rows.
    fn len(&self) -> usize;
    /// Reserves room for `additional` more rows.
    fn reserve(&mut self, additional: usize);
    /// Rows the column can hold without reallocating.
    fn capacity(&self) -> usize;
    /// Swap-removes `row`, dropping the value. False if out of range.
    fn swap_remove_drop(&mut self, row: usize) -> bool;
    /// Swap-removes `row` and pushes the value, with its change tick, onto
    /// `dst`. False (and nothing moved) if `row` is out of range or `dst` has
    /// a different type.
    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool;
    /// Truncates to `len` rows (drops the rest).
    fn truncate(&mut self, len: usize);
    /// Feeds the value at `row` to `h`. False if out of range.
    fn hash_row(&self, row: usize, h: &mut StableHasher) -> bool;
    /// The change tick of `row`.
    fn changed_at(&self, row: usize) -> Option<Tick>;
    /// Writes row `row` (its value, then its change tick) into a snapshot.
    /// False when the row is missing or the component has no snapshot support.
    fn save_row(&self, row: usize, e: &mut Encoder<'_>) -> bool;
    /// Appends one row read from a snapshot.
    fn load_row(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError>;
    /// Writes the value at `row` as inspector text. False if out of range.
    fn inspect_row(&self, row: usize, out: &mut dyn core::fmt::Write) -> bool;
}

/// Storage of one component type within one archetype.
pub struct TypedColumn<T> {
    pub(crate) data: Vec<T>,
    pub(crate) changed: Vec<Tick>,
}

impl<T> Default for TypedColumn<T> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            changed: Vec::new(),
        }
    }
}

impl<T: Component> TypedColumn<T> {
    pub(crate) fn push(&mut self, value: T, tick: Tick) {
        self.data.push(value);
        self.changed.push(tick);
    }

    pub(crate) fn swap_remove(&mut self, row: usize) -> Option<(T, Tick)> {
        if row >= self.data.len() || row >= self.changed.len() {
            return None;
        }
        Some((self.data.swap_remove(row), self.changed.swap_remove(row)))
    }
}

impl<T: Component> Column for TypedColumn<T> {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn reserve(&mut self, additional: usize) {
        self.data.reserve(additional);
        self.changed.reserve(additional);
    }

    fn capacity(&self) -> usize {
        self.data.capacity().min(self.changed.capacity())
    }

    fn swap_remove_drop(&mut self, row: usize) -> bool {
        self.swap_remove(row).is_some()
    }

    fn swap_remove_into(&mut self, row: usize, dst: &mut dyn Column) -> bool {
        let dst: &mut dyn Any = dst;
        let Some(dst) = dst.downcast_mut::<Self>() else {
            return false;
        };
        match self.swap_remove(row) {
            Some((value, tick)) => {
                dst.push(value, tick);
                true
            }
            None => false,
        }
    }

    fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
        self.changed.truncate(len);
    }

    fn hash_row(&self, row: usize, h: &mut StableHasher) -> bool {
        match self.data.get(row) {
            Some(v) => {
                v.state_hash(h);
                true
            }
            None => false,
        }
    }

    fn changed_at(&self, row: usize) -> Option<Tick> {
        self.changed.get(row).copied()
    }

    fn save_row(&self, row: usize, e: &mut Encoder<'_>) -> bool {
        match (self.data.get(row), self.changed.get(row)) {
            (Some(v), Some(t)) => {
                if !v.save(e) {
                    return false;
                }
                e.u64(t.0);
                true
            }
            _ => false,
        }
    }

    fn inspect_row(&self, row: usize, out: &mut dyn core::fmt::Write) -> bool {
        self.data.get(row).is_some_and(|v| v.inspect(out).is_ok())
    }

    fn load_row(&mut self, d: &mut Decoder<'_>) -> Result<(), DecodeError> {
        let v = T::load(d)?;
        let t = Tick(d.u64()?);
        self.push(v, t);
        Ok(())
    }
}

/// Typed shared view of a column.
pub(crate) fn typed<T: Component>(col: &dyn Column) -> Option<&TypedColumn<T>> {
    let any: &dyn Any = col;
    any.downcast_ref::<TypedColumn<T>>()
}

/// Typed exclusive view of a column.
pub(crate) fn typed_mut<T: Component>(col: &mut dyn Column) -> Option<&mut TypedColumn<T>> {
    let any: &mut dyn Any = col;
    any.downcast_mut::<TypedColumn<T>>()
}
