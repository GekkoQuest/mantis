//! The per-tick arena: bump allocation into pre-sized storage, reset at tick
//! end, with epoch-checked handles.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};

use super::bounded::CapacityError;

static NEXT_ARENA_ID: AtomicU32 = AtomicU32::new(1);

/// Handle to one value in a [`TickArena`]. Valid until the arena is reset;
/// after that every lookup with it returns `None`.
pub struct ArenaRef<T> {
    arena: u32,
    epoch: u32,
    index: u32,
    _marker: PhantomData<fn() -> T>,
}

/// Handle to a contiguous run of values in a [`TickArena`].
pub struct ArenaSlice<T> {
    arena: u32,
    epoch: u32,
    start: u32,
    len: u32,
    _marker: PhantomData<fn() -> T>,
}

// Manual impls: the handles are Copy whatever T is.
impl<T> Clone for ArenaRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for ArenaRef<T> {}
impl<T> PartialEq for ArenaRef<T> {
    fn eq(&self, o: &Self) -> bool {
        (self.arena, self.epoch, self.index) == (o.arena, o.epoch, o.index)
    }
}
impl<T> Eq for ArenaRef<T> {}
impl<T> core::fmt::Debug for ArenaRef<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ArenaRef({}:{}:{})", self.arena, self.epoch, self.index)
    }
}
impl<T> Clone for ArenaSlice<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for ArenaSlice<T> {}
impl<T> PartialEq for ArenaSlice<T> {
    fn eq(&self, o: &Self) -> bool {
        (self.arena, self.epoch, self.start, self.len) == (o.arena, o.epoch, o.start, o.len)
    }
}
impl<T> Eq for ArenaSlice<T> {}
impl<T> core::fmt::Debug for ArenaSlice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ArenaSlice({}:{}:{}+{})",
            self.arena, self.epoch, self.start, self.len
        )
    }
}

impl<T> ArenaSlice<T> {
    /// Number of values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A typed bump arena for one tick's scratch data (events, target lists,
/// encode scratch). Capacity is fixed at construction; allocation past it
/// fails and returns the value. [`TickArena::reset`] drops everything and
/// invalidates every outstanding handle by bumping the epoch.
///
/// For bytes, use `TickArena<u8>` with [`TickArena::alloc_slice_copy`] or
/// [`TickArena::alloc_slice_fill`].
#[derive(Debug)]
pub struct TickArena<T> {
    id: u32,
    epoch: u32,
    items: Vec<T>,
    cap: usize,
}

impl<T> TickArena<T> {
    /// An arena holding at most `capacity` values (`capacity <= u32::MAX`).
    /// This is the only allocating call.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.min(u32::MAX as usize);
        Self {
            id: NEXT_ARENA_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            items: Vec::with_capacity(cap),
            cap,
        }
    }

    /// Current epoch (number of resets so far, wrapping).
    #[must_use]
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Values allocated this epoch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing is allocated this epoch.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The fixed capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Drops every value and invalidates every handle. Keeps the storage.
    pub fn reset(&mut self) {
        self.items.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    #[expect(clippy::cast_possible_truncation)] // len <= cap <= u32::MAX
    fn next_index(&self) -> u32 {
        self.items.len() as u32
    }

    /// Stores `value` for the rest of the epoch.
    ///
    /// # Errors
    /// [`CapacityError`] carrying `value` when the arena is full.
    pub fn alloc(&mut self, value: T) -> Result<ArenaRef<T>, CapacityError<T>> {
        if self.items.len() >= self.cap {
            return Err(CapacityError(value));
        }
        let index = self.next_index();
        self.items.push(value);
        Ok(ArenaRef {
            arena: self.id,
            epoch: self.epoch,
            index,
            _marker: PhantomData,
        })
    }

    fn live<U>(&self, arena: u32, epoch: u32) -> Option<PhantomData<U>> {
        (arena == self.id && epoch == self.epoch).then_some(PhantomData)
    }

    /// The value behind `r`, or `None` if `r` is from another arena or an
    /// earlier epoch.
    #[must_use]
    pub fn get(&self, r: ArenaRef<T>) -> Option<&T> {
        self.live::<T>(r.arena, r.epoch)?;
        self.items.get(r.index as usize)
    }

    /// Exclusive access to the value behind `r`.
    pub fn get_mut(&mut self, r: ArenaRef<T>) -> Option<&mut T> {
        self.live::<T>(r.arena, r.epoch)?;
        self.items.get_mut(r.index as usize)
    }

    /// The values behind `s`.
    #[must_use]
    pub fn slice(&self, s: ArenaSlice<T>) -> Option<&[T]> {
        self.live::<T>(s.arena, s.epoch)?;
        let start = s.start as usize;
        self.items.get(start..start + s.len as usize)
    }

    /// Exclusive access to the values behind `s`.
    pub fn slice_mut(&mut self, s: ArenaSlice<T>) -> Option<&mut [T]> {
        self.live::<T>(s.arena, s.epoch)?;
        let start = s.start as usize;
        self.items.get_mut(start..start + s.len as usize)
    }

    #[expect(clippy::cast_possible_truncation)] // n <= remaining <= u32::MAX
    fn slice_handle(&self, start: u32, n: usize) -> ArenaSlice<T> {
        ArenaSlice {
            arena: self.id,
            epoch: self.epoch,
            start,
            len: n as u32,
            _marker: PhantomData,
        }
    }
}

impl<T: Clone> TickArena<T> {
    /// Allocates `n` copies of `value` contiguously.
    ///
    /// # Errors
    /// [`CapacityError`] carrying `n` when they do not all fit (nothing is
    /// allocated then).
    pub fn alloc_slice_fill(&mut self, n: usize, value: T) -> Result<ArenaSlice<T>, CapacityError<usize>> {
        if n > self.cap - self.items.len() {
            return Err(CapacityError(n));
        }
        let start = self.next_index();
        self.items.extend(core::iter::repeat_n(value, n));
        Ok(self.slice_handle(start, n))
    }
}

impl<T: Copy> TickArena<T> {
    /// Copies `values` into the arena contiguously.
    ///
    /// # Errors
    /// [`CapacityError`] carrying the requested length when it does not fit.
    pub fn alloc_slice_copy(&mut self, values: &[T]) -> Result<ArenaSlice<T>, CapacityError<usize>> {
        if values.len() > self.cap - self.items.len() {
            return Err(CapacityError(values.len()));
        }
        let start = self.next_index();
        self.items.extend_from_slice(values);
        Ok(self.slice_handle(start, values.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_get_and_reset_invalidates() {
        let mut a = TickArena::with_capacity(4);
        let r1 = a.alloc(10u32).unwrap();
        let s = a.alloc_slice_copy(&[1, 2]).unwrap();
        assert_eq!(a.get(r1), Some(&10));
        assert_eq!(a.slice(s), Some(&[1u32, 2][..]));
        assert_eq!(s.len(), 2);
        *a.get_mut(r1).unwrap() = 11;
        a.slice_mut(s).unwrap()[0] = 5;
        assert_eq!(a.get(r1), Some(&11));
        assert_eq!(a.slice(s), Some(&[5u32, 2][..]));
        assert_eq!(a.alloc(3).unwrap().index, 3);
        assert_eq!(a.alloc(4), Err(CapacityError(4)), "fails closed");
        assert_eq!(a.alloc_slice_copy(&[1]), Err(CapacityError(1)));
        a.reset();
        assert_eq!(a.epoch(), 1);
        assert!(a.is_empty());
        assert_eq!(a.get(r1), None, "stale handle");
        assert_eq!(a.slice(s), None);
        let r2 = a.alloc(7).unwrap();
        assert_eq!(a.get(r2), Some(&7));
        assert_eq!(a.capacity(), 4);
        let f = a.alloc_slice_fill(3, 0u32).unwrap();
        assert_eq!(a.slice(f), Some(&[0u32, 0, 0][..]));
        assert_eq!(a.alloc_slice_fill(1, 9), Err(CapacityError(1)));
    }

    #[test]
    fn handles_from_another_arena_are_refused() {
        let mut a = TickArena::with_capacity(2);
        let mut b = TickArena::with_capacity(2);
        let ra = a.alloc(1u8).unwrap();
        let _rb = b.alloc(2u8).unwrap();
        assert_eq!(b.get(ra), None);
        assert_eq!(a.get(ra), Some(&1));
    }
}
