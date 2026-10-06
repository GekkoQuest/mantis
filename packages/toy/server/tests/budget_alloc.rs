//! Budget rows on `cell-500-100` that hold in any build profile (plan 17):
//!
//! - engine allocations per tick, `Movement` through `Outbound`, including
//!   the per-client encode jobs: 0. The cell tick runs under
//!   `assert_no_alloc`, and every job, on a worker or inline, runs inside the
//!   allocation harness through the worker set's job wrapper;
//! - total snapshot bytes per client per second: under 20 KB.

#![expect(clippy::cast_precision_loss)]

use std::sync::atomic::{AtomicU64, Ordering};

use mantis_adapter_contract::{Channel, ConnectionId};
use mantis_server::cell::OutboundSink;
use mantis_server::jobs::WorkerSet;
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};
use toy_server::scenario::Crowd;
use toy_server::tunables::Tunables;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

static JOB_ALLOCS: AtomicU64 = AtomicU64::new(0);
static JOBS: AtomicU64 = AtomicU64::new(0);

/// Runs one per-client encode job inside the allocation harness.
fn counting_wrapper(job: &mut dyn FnMut()) {
    let ((), stats) = count_allocs(job);
    JOB_ALLOCS.fetch_add(stats.total_ops(), Ordering::Relaxed);
    JOBS.fetch_add(1, Ordering::Relaxed);
}

/// Counts bytes without allocating.
#[derive(Default)]
struct Bytes {
    total: u64,
    frames: u64,
}

impl OutboundSink for Bytes {
    fn send(&mut self, _adapter: usize, _conn: ConnectionId, _channel: Channel, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        self.frames += 1;
    }
}

#[test]
fn cell_500_100_allocates_nothing_per_tick_including_encode_jobs() {
    let t = Tunables::defaults().unwrap();
    let mut crowd = Crowd::cell_500_100(&t, 1).unwrap();
    let workers = WorkerSet::new(3, 256, Some(counting_wrapper));
    let mut sink = Bytes::default();
    // Warm-up: joins, first snapshots, every pool and baseline ring filled.
    for _ in 0..60 {
        crowd.tick(&mut sink, Some(&workers)).unwrap();
    }
    JOB_ALLOCS.store(0, Ordering::Relaxed);
    JOBS.store(0, Ordering::Relaxed);
    let mut offloaded = 0;
    let mut inline = 0;
    for _ in 0..90 {
        crowd.push_inputs();
        let r = assert_no_alloc("cell-500-100 tick", || crowd.cell.tick(&mut sink, Some(&workers))).unwrap();
        crowd.ack_all(r.tick);
        offloaded += r.jobs.offloaded;
        inline += r.jobs.inline;
    }
    let jobs = JOBS.load(Ordering::Relaxed);
    eprintln!(
        "budget: cell-500-100 allocations: 0 on the cell thread; {} in {jobs} encode jobs ({offloaded} on workers, {inline} inline)",
        JOB_ALLOCS.load(Ordering::Relaxed)
    );
    assert_eq!(
        jobs,
        90 * crowd.clients(),
        "every client's job ran inside the harness"
    );
    assert!(offloaded > 0 && inline > 0, "both job paths were exercised");
    assert_eq!(JOB_ALLOCS.load(Ordering::Relaxed), 0, "an encode job allocated");
}

/// Snapshot bytes per client per second and per frame.
fn snapshot_bytes(t: &Tunables, ticks: u64) -> Result<(f64, f64), mantis_server::cell::CellError> {
    let mut crowd = Crowd::cell_500_100(t, 2)?;
    let mut sink = Bytes::default();
    for _ in 0..60 {
        crowd.tick(&mut sink, None)?;
    }
    let mut sink = Bytes::default();
    for _ in 0..ticks {
        crowd.tick(&mut sink, None)?;
    }
    let seconds = ticks as f64 / f64::from(t.tick_rate.hz());
    let per_client = sink.total as f64 / crowd.clients() as f64 / seconds;
    let per_frame = sink.total as f64 / sink.frames as f64;
    Ok((per_client, per_frame))
}

/// Per-remote snapshot baselines are off in the shipped tunables (the
/// encode row's headroom); their bandwidth is tracked here, not gated.
#[test]
fn cell_500_100_snapshot_bytes_with_own_bases_is_reported() {
    let mut t = Tunables::defaults().unwrap();
    let (off, _) = snapshot_bytes(&t, 300).unwrap();
    t.interest.snapshot_own_bases = true;
    let (on, per_frame) = snapshot_bytes(&t, 300).unwrap();
    println!(
        "MANTIS-METRIC cell_500_100_own_bases snapshot_bytes_per_client_per_s={on:.0} per_frame={per_frame:.0} without={off:.0}"
    );
    assert!(on < off, "own bases send fewer bytes: {on:.0} vs {off:.0}");
}

#[test]
fn cell_500_100_snapshot_bytes_per_client_per_second() {
    let t = Tunables::defaults().unwrap();
    assert!(
        !t.interest.snapshot_own_bases,
        "budget rows measure the shipped default"
    );
    let mut crowd = Crowd::cell_500_100(&t, 2).unwrap();
    let mut sink = Bytes::default();
    for _ in 0..60 {
        crowd.tick(&mut sink, None).unwrap();
    }
    let mut sink = Bytes::default();
    let ticks = 300u64;
    for _ in 0..ticks {
        crowd.tick(&mut sink, None).unwrap();
    }
    let seconds = ticks as f64 / f64::from(t.tick_rate.hz());
    let per_client = sink.total as f64 / crowd.clients() as f64 / seconds;
    let per_frame = sink.total as f64 / sink.frames as f64;
    eprintln!(
        "budget: cell-500-100 snapshot bytes = {per_client:.0} B per client per second, {per_frame:.0} B per frame (target < 20480)"
    );
    assert_eq!(
        sink.frames,
        ticks * crowd.clients(),
        "every client got every snapshot"
    );
    assert!(per_client < 20_480.0);
}

#[test]
fn the_inspector_timing_allocates_nothing_per_tick() {
    let t = Tunables::defaults().unwrap();
    let mut crowd = Crowd::cell_500_100(&t, 2).unwrap();
    crowd
        .cell
        .set_stopwatch(std::sync::Arc::new(toy_server::cluster::OsStopwatch::new()));
    let mut sink = Bytes::default();
    for _ in 0..60 {
        crowd.tick(&mut sink, None).unwrap();
    }
    for _ in 0..60 {
        crowd.push_inputs();
        let r = assert_no_alloc("cell-500-100 tick, timed", || crowd.cell.tick(&mut sink, None)).unwrap();
        crowd.ack_all(r.tick);
    }
    let timings = crowd.cell.timings();
    assert!(timings.systems.iter().all(|(_, _, t)| t.runs() >= 60));
    assert!(timings.encode.runs() >= 60 * crowd.clients());
}
