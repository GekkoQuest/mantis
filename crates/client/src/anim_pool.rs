//! The persistent animation worker pool (plan 8.4): near-tier animation graphs advance on
//! `workers` threads (the calling thread is one of them), started once and reused every
//! frame without allocating.
//!
//! Animation instances live in shards, one per worker, each behind its own mutex; a
//! character's [`Slot`] names its shard and index. Every frame the caller marks which
//! slots are near ([`AnimPool::set_near`]), [`AnimPool::run`] wakes the workers with the
//! frame's time step, advances shard 0 itself, and waits until every shard is done. Shards
//! are filled evenly, so the work splits evenly. Locks and condition variables do not
//! allocate; the shards are sized for the pool's capacity up front.
//!
//! Each worker runs its share inside a wrapper ([`AnimPool::with_wrapper`]; identity by
//! default) so tests can count the worker threads' heap operations, which a per-thread
//! allocation counter would not see from the calling thread.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use mantis_anim::AnimInstance;
use mantis_core::ecs::EntityId;

/// Where a character's animation lives.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Slot {
    shard: usize,
    index: usize,
}

#[derive(Debug)]
struct Entry {
    entity: EntityId,
    anim: AnimInstance,
    near: bool,
}

#[derive(Debug, Default)]
struct Shard {
    entries: Vec<Entry>,
}

#[derive(Debug, Default)]
struct Control {
    generation: u64,
    dt: f32,
    pending: usize,
    stop: bool,
}

/// Runs a worker's share of a frame (see [`AnimPool::with_wrapper`]).
pub type WorkerWrapper = fn(&mut dyn FnMut());

fn identity(f: &mut dyn FnMut()) {
    f();
}

#[derive(Debug)]
struct Shared {
    wrap: WorkerWrapper,
    shards: Vec<Mutex<Shard>>,
    control: Mutex<Control>,
    kick: Condvar,
    done: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn advance(shard: &Mutex<Shard>, dt: f32) {
    for e in &mut lock(shard).entries {
        if e.near {
            let _ = e.anim.update(dt);
        }
    }
}

fn worker(shared: &Shared, index: usize) {
    let mut seen = 0u64;
    loop {
        let dt = {
            let mut c = lock(&shared.control);
            while c.generation == seen && !c.stop {
                c = shared.kick.wait(c).unwrap_or_else(PoisonError::into_inner);
            }
            if c.stop {
                return;
            }
            seen = c.generation;
            c.dt
        };
        if let Some(shard) = shared.shards.get(index) {
            (shared.wrap)(&mut || advance(shard, dt));
        }
        let mut c = lock(&shared.control);
        c.pending = c.pending.saturating_sub(1);
        if c.pending == 0 {
            shared.done.notify_all();
        }
    }
}

/// The pool.
#[derive(Debug)]
pub struct AnimPool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl AnimPool {
    /// A pool of `workers` (at least 1; the calling thread is the first) holding up to
    /// `capacity` instances in total.
    ///
    /// # Errors
    /// The OS error when a thread cannot start; threads already started are stopped.
    pub fn new(workers: usize, capacity: usize) -> std::io::Result<Self> {
        Self::with_wrapper(workers, capacity, identity)
    }

    /// As [`AnimPool::new`], with every worker thread's share of a frame run inside
    /// `wrap` (tests count the workers' heap operations with it).
    ///
    /// # Errors
    /// As [`AnimPool::new`].
    pub fn with_wrapper(workers: usize, capacity: usize, wrap: WorkerWrapper) -> std::io::Result<Self> {
        let workers = workers.max(1);
        let per_shard = capacity.div_ceil(workers).max(1);
        let shared = Arc::new(Shared {
            wrap,
            shards: (0..workers)
                .map(|_| {
                    Mutex::new(Shard {
                        entries: Vec::with_capacity(per_shard + 1),
                    })
                })
                .collect(),
            control: Mutex::new(Control::default()),
            kick: Condvar::new(),
            done: Condvar::new(),
        });
        let mut pool = Self {
            shared,
            threads: Vec::with_capacity(workers - 1),
        };
        for index in 1..workers {
            let shared = Arc::clone(&pool.shared);
            let h = std::thread::Builder::new()
                .name(format!("mantis-anim-{index}"))
                .spawn(move || worker(&shared, index))?;
            pool.threads.push(h);
        }
        Ok(pool)
    }

    /// Workers, the calling thread included.
    pub fn workers(&self) -> usize {
        self.shared.shards.len()
    }

    /// Adds an instance to the least-filled shard.
    pub fn add(&mut self, entity: EntityId, anim: AnimInstance) -> Slot {
        let shard = (0..self.shared.shards.len())
            .min_by_key(|s| {
                self.shared
                    .shards
                    .get(*s)
                    .map_or(usize::MAX, |m| lock(m).entries.len())
            })
            .unwrap_or(0);
        let index = self.shared.shards.get(shard).map_or(0, |m| {
            let mut s = lock(m);
            s.entries.push(Entry {
                entity,
                anim,
                near: false,
            });
            s.entries.len() - 1
        });
        Slot { shard, index }
    }

    /// Removes the instance at `slot`. The shard's last instance moves into the hole;
    /// returns its entity, whose slot is now `slot`.
    pub fn remove(&mut self, slot: Slot) -> Option<EntityId> {
        let m = self.shared.shards.get(slot.shard)?;
        let mut s = lock(m);
        if slot.index >= s.entries.len() {
            return None;
        }
        let _ = s.entries.swap_remove(slot.index);
        s.entries.get(slot.index).map(|e| e.entity)
    }

    /// Runs `f` on the instance at `slot`.
    pub fn with<R>(&self, slot: Slot, f: impl FnOnce(&mut AnimInstance) -> R) -> Option<R> {
        let m = self.shared.shards.get(slot.shard)?;
        let mut s = lock(m);
        s.entries.get_mut(slot.index).map(|e| f(&mut e.anim))
    }

    /// Marks whether the instance at `slot` advances in the next [`AnimPool::run`].
    pub fn set_near(&mut self, slot: Slot, near: bool) {
        if let Some(m) = self.shared.shards.get(slot.shard)
            && let Some(e) = lock(m).entries.get_mut(slot.index)
        {
            e.near = near;
        }
    }

    /// Advances every near instance by `dt` on all workers and returns when all are done.
    pub fn run(&mut self, dt: f32) {
        let helpers = self.threads.len();
        if helpers > 0 {
            let mut c = lock(&self.shared.control);
            c.generation = c.generation.wrapping_add(1);
            c.dt = dt;
            c.pending = helpers;
            drop(c);
            self.shared.kick.notify_all();
        }
        if let Some(shard) = self.shared.shards.first() {
            advance(shard, dt);
        }
        if helpers > 0 {
            let mut c = lock(&self.shared.control);
            while c.pending > 0 {
                c = self.shared.done.wait(c).unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
}

impl Drop for AnimPool {
    fn drop(&mut self) {
        lock(&self.shared.control).stop = true;
        self.shared.kick.notify_all();
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}
