//! A vector with a fixed capacity that never reallocates.

use core::fmt;
use core::ops::{Deref, DerefMut};

/// The container is full; the rejected value is handed back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CapacityError<T>(pub T);

impl<T> fmt::Debug for CapacityError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapacityError(..)")
    }
}

impl<T> fmt::Display for CapacityError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("container is at its fixed capacity")
    }
}

impl<T> std::error::Error for CapacityError<T> {}

/// A `Vec` whose capacity is fixed at construction. `push` past capacity
/// fails with [`CapacityError`] instead of allocating. Dereferences to a slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BoundedVec<T> {
    items: Vec<T>,
    cap: usize,
}

impl<T> BoundedVec<T> {
    /// An empty vector that holds at most `capacity` items. This is the only
    /// allocating call.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
            cap: capacity,
        }
    }

    /// Appends `value`, or returns it if the vector is full.
    ///
    /// # Errors
    /// [`CapacityError`] carrying `value` when full.
    pub fn push(&mut self, value: T) -> Result<(), CapacityError<T>> {
        if self.items.len() >= self.cap {
            return Err(CapacityError(value));
        }
        self.items.push(value);
        Ok(())
    }

    /// Removes and returns the last item.
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Removes every item, keeping the storage.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Shortens to `len` items.
    pub fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    /// Removes the item at `index` by swapping in the last one.
    pub fn swap_remove(&mut self, index: usize) -> Option<T> {
        if index < self.items.len() {
            Some(self.items.swap_remove(index))
        } else {
            None
        }
    }

    /// Removes the item at `index`, shifting later items down (order kept).
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index < self.items.len() {
            Some(self.items.remove(index))
        } else {
            None
        }
    }

    /// Keeps only the items for which `keep` returns true, preserving order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }

    /// The fixed capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Free slots left.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.cap - self.items.len()
    }

    /// True when no more items fit.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.items.len() >= self.cap
    }
}

impl<T: Copy> BoundedVec<T> {
    /// Appends all of `values`, or none of them if they do not all fit.
    ///
    /// # Errors
    /// [`CapacityError`] carrying the number of items that did not fit.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<(), CapacityError<usize>> {
        if values.len() > self.remaining() {
            return Err(CapacityError(values.len() - self.remaining()));
        }
        self.items.extend_from_slice(values);
        Ok(())
    }
}

impl<T> Deref for BoundedVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<T> DerefMut for BoundedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fails_closed_when_full() {
        let mut v = BoundedVec::with_capacity(2);
        assert_eq!(v.push(1), Ok(()));
        assert_eq!(v.push(2), Ok(()));
        assert!(v.is_full());
        assert_eq!(v.push(3), Err(CapacityError(3)));
        assert_eq!(&*v, &[1, 2]);
        assert_eq!(v.capacity(), 2);
        assert_eq!(v.pop(), Some(2));
        assert_eq!(v.remaining(), 1);
        assert_eq!(v.extend_from_slice(&[7, 8]), Err(CapacityError(1)));
        assert_eq!(&*v, &[1], "all or nothing");
        assert_eq!(v.extend_from_slice(&[7]), Ok(()));
        assert_eq!(v.swap_remove(0), Some(1));
        assert_eq!(v.swap_remove(5), None);
        v[0] = 9;
        assert_eq!(&*v, &[9]);
        v.retain(|x| *x != 9);
        assert!(v.is_empty());
        v.clear();
        assert_eq!(v.capacity(), 2);
    }
}
