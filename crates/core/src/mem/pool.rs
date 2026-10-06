//! Object pools with generational handles, for messages, snapshots, events,
//! and encode buffers. Objects are reset and reused, never dropped, so their
//! internal buffers keep their capacity across uses.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU32, Ordering};

static NEXT_POOL_ID: AtomicU32 = AtomicU32::new(1);

/// Returns an object to its pristine state for reuse, without releasing its
/// storage (for example `Vec::clear`).
pub trait Reset {
    /// Clears contents; keeps capacity.
    fn reset(&mut self);
}

impl<T> Reset for Vec<T> {
    fn reset(&mut self) {
        self.clear();
    }
}

impl<T> Reset for super::BoundedVec<T> {
    fn reset(&mut self) {
        self.clear();
    }
}

/// Handle to a checked-out pool object. Released handles fail lookups.
pub struct PoolHandle<T> {
    pool: u32,
    index: u32,
    generation: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for PoolHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for PoolHandle<T> {}
impl<T> PartialEq for PoolHandle<T> {
    fn eq(&self, o: &Self) -> bool {
        (self.pool, self.index, self.generation) == (o.pool, o.index, o.generation)
    }
}
impl<T> Eq for PoolHandle<T> {}
impl<T> core::fmt::Debug for PoolHandle<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PoolHandle({}:{}v{})", self.pool, self.index, self.generation)
    }
}

struct Entry<T> {
    value: T,
    generation: u32,
    in_use: bool,
}

/// A fixed-size pool of pre-built objects. [`Pool::acquire`] fails closed
/// when every object is checked out; it never builds a new one.
pub struct Pool<T> {
    id: u32,
    entries: Vec<Entry<T>>,
    free: Vec<u32>,
}

impl<T: Reset> Pool<T> {
    /// Builds `size` objects with `make` up front (`size <= u32::MAX`). This
    /// is the only allocating call.
    pub fn new(size: usize, mut make: impl FnMut() -> T) -> Self {
        let size = size.min(u32::MAX as usize);
        let entries: Vec<Entry<T>> = (0..size)
            .map(|_| Entry {
                value: make(),
                generation: 0,
                in_use: false,
            })
            .collect();
        // Hand out low indices first: deterministic and cache friendly.
        let free: Vec<u32> = (0..size).rev().filter_map(|i| u32::try_from(i).ok()).collect();
        Self {
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            entries,
            free,
        }
    }

    /// Checks out an object (already reset), or `None` when the pool is
    /// exhausted.
    pub fn acquire(&mut self) -> Option<PoolHandle<T>> {
        let index = self.free.pop()?;
        let entry = self.entries.get_mut(index as usize)?;
        entry.in_use = true;
        Some(PoolHandle {
            pool: self.id,
            index,
            generation: entry.generation,
            _marker: PhantomData,
        })
    }

    fn entry(&self, h: PoolHandle<T>) -> Option<&Entry<T>> {
        let e = self.entries.get(h.index as usize)?;
        (h.pool == self.id && e.in_use && e.generation == h.generation).then_some(e)
    }

    /// Shared access to a checked-out object.
    #[must_use]
    pub fn get(&self, h: PoolHandle<T>) -> Option<&T> {
        self.entry(h).map(|e| &e.value)
    }

    /// Exclusive access to a checked-out object.
    pub fn get_mut(&mut self, h: PoolHandle<T>) -> Option<&mut T> {
        self.entry(h)?;
        self.entries.get_mut(h.index as usize).map(|e| &mut e.value)
    }

    /// Resets and returns an object to the pool. False for a stale or foreign
    /// handle (nothing happens then).
    pub fn release(&mut self, h: PoolHandle<T>) -> bool {
        if self.entry(h).is_none() {
            return false;
        }
        let Some(e) = self.entries.get_mut(h.index as usize) else {
            return false;
        };
        e.value.reset();
        e.in_use = false;
        e.generation = e.generation.wrapping_add(1);
        self.free.push(h.index);
        true
    }

    /// Objects currently checked out.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.entries.len() - self.free.len()
    }

    /// Total objects.
    #[must_use]
    pub fn size(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_reuse_and_stale_handles() {
        let mut p: Pool<Vec<u8>> = Pool::new(2, || Vec::with_capacity(64));
        let a = p.acquire().unwrap();
        let b = p.acquire().unwrap();
        assert_eq!(p.acquire(), None, "fails closed");
        assert_eq!(p.in_use(), 2);
        p.get_mut(a).unwrap().extend_from_slice(b"hello");
        assert_eq!(p.get(a).map(Vec::as_slice), Some(&b"hello"[..]));
        assert!(p.release(a));
        assert!(!p.release(a), "double release refused");
        assert_eq!(p.get(a), None, "stale handle");
        let c = p.acquire().unwrap();
        assert_ne!(a, c);
        let buf = p.get(c).unwrap();
        assert!(buf.is_empty(), "reset on release");
        assert!(buf.capacity() >= 64, "capacity kept");
        let mut other: Pool<Vec<u8>> = Pool::new(1, Vec::new);
        assert_eq!(other.get(b), None, "foreign handle");
        assert!(!other.release(b));
        assert_eq!(p.size(), 2);
    }
}
