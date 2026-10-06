//! Integration tests for the allocation harness, with the counting allocator
//! installed as this binary's global allocator.

use mantis_testkit::alloc::{
    AllocStats, CountingAllocator, assert_no_alloc, count_allocs, exempt, is_installed, try_count_allocs,
};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[test]
fn harness_reports_installed() {
    assert!(is_installed());
    assert!(try_count_allocs(|| ()).is_ok());
}

#[test]
fn empty_scope_counts_nothing() {
    let ((), stats) = count_allocs(|| ());
    assert_eq!(stats, AllocStats::default());
}

#[test]
fn box_alloc_and_free_are_counted_exactly() {
    let ((), stats) = count_allocs(|| {
        let b = std::hint::black_box(Box::new([0u8; 48]));
        drop(b);
    });
    assert_eq!(stats.allocs, 1);
    assert_eq!(stats.deallocs, 1);
    assert_eq!(stats.reallocs, 0);
    assert_eq!(stats.bytes, 48);
}

#[test]
fn vec_growth_counts_realloc() {
    let mut v: Vec<u64> = Vec::with_capacity(1);
    v.push(1);
    let ((), stats) = count_allocs(|| {
        v.push(2); // exceeds capacity 1: realloc
    });
    assert_eq!(stats.reallocs, 1);
    assert_eq!(stats.allocs, 0);
}

#[test]
fn free_of_outside_allocation_is_counted() {
    let b = Box::new(7u32);
    let ((), stats) = count_allocs(|| drop(std::hint::black_box(b)));
    assert_eq!(stats.deallocs, 1);
    assert!(!stats.is_zero());
}

#[test]
fn reuse_within_capacity_is_allocation_free() {
    let mut buf: Vec<u32> = Vec::with_capacity(256);
    let sum = assert_no_alloc("reuse_within_capacity", || {
        for round in 0..4u32 {
            buf.clear();
            for i in 0..256u32 {
                buf.push(i ^ round);
            }
        }
        buf.iter().copied().map(u64::from).sum::<u64>()
    });
    assert_eq!(sum, (0..256u64).map(|i| i ^ 3).sum::<u64>());
}

#[test]
fn nested_scopes_accumulate_outward() {
    let (inner, outer) = count_allocs(|| {
        let a = std::hint::black_box(Box::new(1u8));
        let ((), inner) = count_allocs(|| {
            let b = std::hint::black_box(Box::new(2u8));
            drop(b);
        });
        drop(a);
        inner
    });
    assert_eq!(inner.allocs, 1);
    assert_eq!(inner.deallocs, 1);
    assert_eq!(outer.allocs, 2);
    assert_eq!(outer.deallocs, 2);
}

#[test]
fn exempt_scope_is_tallied_separately() {
    let ((), stats) = count_allocs(|| {
        exempt(|| drop(std::hint::black_box(vec![1u8, 2, 3])));
    });
    assert!(stats.is_zero());
    assert_eq!(stats.exempt_ops, 2);
    assert_no_alloc("exempt_inside_assert", || {
        exempt(|| drop(std::hint::black_box(Box::new(1u16))));
    });
}

#[test]
fn allocation_outside_scope_is_not_counted() {
    let keep = Box::new(1u64);
    let ((), stats) = count_allocs(|| ());
    assert!(stats.is_zero());
    drop(keep);
}

#[test]
#[should_panic(expected = "hot-path allocation in `deliberate`")]
fn assert_no_alloc_fails_on_allocation() {
    assert_no_alloc("deliberate", || drop(std::hint::black_box(Box::new(5u64))));
}

#[test]
fn panic_inside_scope_closes_it() {
    let r = std::panic::catch_unwind(|| {
        count_allocs(|| {
            std::panic::resume_unwind(Box::new("boom"));
        })
    });
    assert!(r.is_err());
    // The guard closed the scope during unwinding, so a fresh scope starts
    // from zero and nothing is left counting.
    let ((), stats) = count_allocs(|| ());
    assert!(stats.is_zero());
}

#[test]
fn counts_are_per_thread() {
    let ((), stats) = count_allocs(|| {
        let handle = std::thread::spawn(|| {
            // The spawned thread measures its own work; the parent scope
            // never sees this vector.
            let ((), theirs) = count_allocs(|| drop(std::hint::black_box(vec![0u8; 64])));
            theirs
        });
        let theirs = handle.join();
        assert!(matches!(theirs, Ok(s) if s.allocs == 1 && s.deallocs == 1 && s.bytes == 64));
    });
    // Spawning a thread allocates on the parent thread, which is expected.
    // What matters is that the 64-byte buffer was attributed to the child.
    assert!(stats.allocs >= 1);
}
