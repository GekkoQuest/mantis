//! Allocation harness: the enforcement mechanism for the zero-allocation rule.
//!
//! [`CountingAllocator`] wraps the system allocator and counts heap operations
//! **per thread**, and only while a counting scope is open on that thread. Tests
//! running in parallel therefore never see each other's allocations, and code
//! outside a scope is never counted.
//!
//! Each test binary that uses the harness installs the allocator itself:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;
//! ```
//!
//! Every counting entry point verifies, with a probe allocation, that the
//! counting allocator really is the global allocator. A test binary that
//! forgets to install it fails loudly instead of passing with a count of zero.
//!
//! What counts: every `alloc`, `alloc_zeroed`, `realloc`, and `dealloc` on the
//! current thread while a scope is open, except inside [`exempt`]. A
//! deallocation is a heap operation like any other, and the hot path performs
//! none. Work that runs on another thread (a per-client encode job, for
//! example) is counted by wrapping it on that thread.
//!
//! This is the single module of `mantis-testkit` permitted to contain unsafe
//! code (decision 0015). The unsafe code delegates to [`std::alloc::System`]
//! and does nothing else.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

/// A global allocator that delegates to [`System`] and counts heap operations
/// per thread inside open counting scopes.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountingAllocator;

/// Heap operations observed inside one counting scope on one thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Calls to `alloc` and `alloc_zeroed`.
    pub allocs: u64,
    /// Calls to `dealloc`.
    pub deallocs: u64,
    /// Calls to `realloc`.
    pub reallocs: u64,
    /// Bytes requested by `alloc`, `alloc_zeroed`, and `realloc` (new size).
    pub bytes: u64,
    /// Heap operations of any kind performed inside an [`exempt`] scope. These
    /// are reported for separate budgeting and never fail [`assert_no_alloc`].
    pub exempt_ops: u64,
}

impl AllocStats {
    /// Total counted (non-exempt) heap operations.
    #[must_use]
    pub const fn total_ops(&self) -> u64 {
        self.allocs
            .wrapping_add(self.deallocs)
            .wrapping_add(self.reallocs)
    }

    /// True when no counted heap operation happened.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.allocs == 0 && self.deallocs == 0 && self.reallocs == 0
    }

    const fn delta_since(self, start: Self) -> Self {
        Self {
            allocs: self.allocs.wrapping_sub(start.allocs),
            deallocs: self.deallocs.wrapping_sub(start.deallocs),
            reallocs: self.reallocs.wrapping_sub(start.reallocs),
            bytes: self.bytes.wrapping_sub(start.bytes),
            exempt_ops: self.exempt_ops.wrapping_sub(start.exempt_ops),
        }
    }
}

impl fmt::Display for AllocStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "allocs={} deallocs={} reallocs={} bytes={} exempt_ops={}",
            self.allocs, self.deallocs, self.reallocs, self.bytes, self.exempt_ops
        )
    }
}

/// The counting allocator is not the global allocator of this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessNotInstalled;

impl fmt::Display for HarnessNotInstalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "mantis_testkit::alloc::CountingAllocator is not installed as the #[global_allocator] \
             of this test binary; allocation counts would be meaningless",
        )
    }
}

impl std::error::Error for HarnessNotInstalled {}

/// Per-thread counters. Every field is a `Cell` of a `Copy` type with a const
/// initializer, so accessing the thread-local never allocates and registers no
/// destructor. That is what makes it sound to touch from inside the allocator.
struct ThreadCounters {
    depth: Cell<u32>,
    exempt_depth: Cell<u32>,
    allocs: Cell<u64>,
    deallocs: Cell<u64>,
    reallocs: Cell<u64>,
    bytes: Cell<u64>,
    exempt_ops: Cell<u64>,
}

impl ThreadCounters {
    const fn new() -> Self {
        Self {
            depth: Cell::new(0),
            exempt_depth: Cell::new(0),
            allocs: Cell::new(0),
            deallocs: Cell::new(0),
            reallocs: Cell::new(0),
            bytes: Cell::new(0),
            exempt_ops: Cell::new(0),
        }
    }

    fn snapshot(&self) -> AllocStats {
        AllocStats {
            allocs: self.allocs.get(),
            deallocs: self.deallocs.get(),
            reallocs: self.reallocs.get(),
            bytes: self.bytes.get(),
            exempt_ops: self.exempt_ops.get(),
        }
    }
}

thread_local! {
    static COUNTERS: ThreadCounters = const { ThreadCounters::new() };
}

#[derive(Clone, Copy)]
enum Op {
    Alloc(usize),
    Dealloc,
    Realloc(usize),
}

fn bump(cell: &Cell<u64>, by: u64) {
    cell.set(cell.get().wrapping_add(by));
}

/// Records one heap operation. Never allocates and never panics. During thread
/// teardown the thread-local may already be gone; nothing is recorded then, and
/// no scope can be open at that point.
fn record(op: Op) {
    let _ = COUNTERS.try_with(|c| {
        if c.depth.get() == 0 {
            return;
        }
        if c.exempt_depth.get() > 0 {
            bump(&c.exempt_ops, 1);
            return;
        }
        match op {
            Op::Alloc(size) => {
                bump(&c.allocs, 1);
                bump(&c.bytes, size as u64);
            }
            Op::Dealloc => bump(&c.deallocs, 1),
            Op::Realloc(size) => {
                bump(&c.reallocs, 1);
                bump(&c.bytes, size as u64);
            }
        }
    });
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract. The bookkeeping in `record` touches only
// a const-initialised thread-local of `Cell`s; it does not allocate, does not
// unwind, and does not touch the memory being managed.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(Op::Alloc(layout.size()));
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(Op::Alloc(layout.size()));
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract for `layout`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(Op::Dealloc);
        // SAFETY: `ptr` was allocated by this allocator (that is, by `System`)
        // with `layout`, as the caller guarantees.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(Op::Realloc(new_size));
        // SAFETY: `ptr` was allocated by `System` with `layout`, and `new_size`
        // satisfies `GlobalAlloc::realloc`'s contract, as the caller guarantees.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Closes a counting scope on drop, so a panicking closure cannot leave the
/// thread counting forever.
struct DepthGuard;

impl DepthGuard {
    fn open() -> Self {
        COUNTERS.with(|c| c.depth.set(c.depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        let _ = COUNTERS.try_with(|c| c.depth.set(c.depth.get().saturating_sub(1)));
    }
}

/// Closes an exempt scope on drop.
struct ExemptGuard;

impl ExemptGuard {
    fn open() -> Self {
        COUNTERS.with(|c| c.exempt_depth.set(c.exempt_depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for ExemptGuard {
    fn drop(&mut self) {
        let _ = COUNTERS.try_with(|c| c.exempt_depth.set(c.exempt_depth.get().saturating_sub(1)));
    }
}

const PROBE_UNKNOWN: u8 = 0;
const PROBE_INSTALLED: u8 = 1;
const PROBE_MISSING: u8 = 2;

/// The global allocator of a process never changes, so the probe result is
/// cached after the first check.
static PROBE: AtomicU8 = AtomicU8::new(PROBE_UNKNOWN);

fn measure_raw<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    let start = COUNTERS.with(ThreadCounters::snapshot);
    let result = {
        let _guard = DepthGuard::open();
        f()
    };
    let end = COUNTERS.with(ThreadCounters::snapshot);
    (result, end.delta_since(start))
}

/// Reports whether [`CountingAllocator`] is this binary's global allocator.
///
/// The first call performs one probe allocation inside a counting scope; the
/// answer is cached for the life of the process.
#[must_use]
pub fn is_installed() -> bool {
    match PROBE.load(Ordering::Acquire) {
        PROBE_INSTALLED => true,
        PROBE_MISSING => false,
        _ => {
            let ((), stats) = measure_raw(|| {
                let probe = std::hint::black_box(Box::new(0x6d61_6e74_6973_u64));
                drop(std::hint::black_box(probe));
            });
            let installed = stats.allocs >= 1 && stats.deallocs >= 1;
            PROBE.store(
                if installed { PROBE_INSTALLED } else { PROBE_MISSING },
                Ordering::Release,
            );
            installed
        }
    }
}

/// Runs `f` and returns its result with the heap operations it performed on
/// the current thread.
///
/// # Errors
/// [`HarnessNotInstalled`] if [`CountingAllocator`] is not the global
/// allocator. `f` is not run in that case.
pub fn try_count_allocs<R>(f: impl FnOnce() -> R) -> Result<(R, AllocStats), HarnessNotInstalled> {
    if !is_installed() {
        return Err(HarnessNotInstalled);
    }
    Ok(measure_raw(f))
}

/// Runs `f` and returns its result with the heap operations it performed on
/// the current thread.
///
/// Scopes nest: an outer scope's stats include everything an inner scope saw.
///
/// # Panics
/// If [`CountingAllocator`] is not installed. This is a test-harness entry
/// point, and failing the test is the intended behavior.
#[allow(clippy::panic)]
pub fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
    match try_count_allocs(f) {
        Ok(out) => out,
        Err(e) => panic!("{e}"),
    }
}

/// Runs `f` and fails the calling test if `f` performed any counted heap
/// operation (allocation, reallocation, or deallocation) on the current thread.
///
/// # Panics
/// If `f` touched the heap outside an [`exempt`] scope, or if the harness is
/// not installed. The message names `label` and the observed counts.
pub fn assert_no_alloc<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let (result, stats) = count_allocs(f);
    assert!(stats.is_zero(), "hot-path allocation in `{label}`: {stats}");
    result
}

/// Runs `f` with counting suspended on the current thread.
///
/// Heap operations inside are tallied in [`AllocStats::exempt_ops`] instead of
/// the counted fields. This exists for the script VM's pooled allocator, which
/// decision 0001 exempts from the engine harness and budgets separately. Engine
/// code must not use it to hide its own allocations.
pub fn exempt<R>(f: impl FnOnce() -> R) -> R {
    let _guard = ExemptGuard::open();
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_delta_and_display() {
        let a = AllocStats {
            allocs: 5,
            deallocs: 4,
            reallocs: 3,
            bytes: 100,
            exempt_ops: 2,
        };
        let b = AllocStats {
            allocs: 7,
            deallocs: 4,
            reallocs: 4,
            bytes: 164,
            exempt_ops: 2,
        };
        let d = b.delta_since(a);
        assert_eq!(
            d,
            AllocStats {
                allocs: 2,
                deallocs: 0,
                reallocs: 1,
                bytes: 64,
                exempt_ops: 0
            }
        );
        assert_eq!(d.total_ops(), 3);
        assert!(!d.is_zero());
        assert!(AllocStats::default().is_zero());
        assert_eq!(
            d.to_string(),
            "allocs=2 deallocs=0 reallocs=1 bytes=64 exempt_ops=0"
        );
    }

    /// This unit-test binary does not install the counting allocator, so the
    /// probe must say so and the fallible entry point must refuse to run.
    #[test]
    fn uninstalled_harness_is_detected() {
        assert!(!is_installed());
        assert_eq!(try_count_allocs(|| 1).err(), Some(HarnessNotInstalled));
    }
}
