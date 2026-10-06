//! Timing rows of the budget table on `cell-500-100` (plan 17). Timings are
//! meaningful only in an optimized build, so these run with
//! `cargo test --release -p toy-server --test budget_timing` and are ignored
//! in debug builds. The hardware tier is recorded in
//! `crates/testkit/scenarios/cell-500-100.toml`.
//!
//! - cell tick, p99: under 4 ms;
//! - per-client Outbound encode, p99: under 20 µs (each encode job is timed
//!   by the worker set's job wrapper, on workers and inline alike), taken
//!   over the **least-loaded window**: the lowest p99 of three consecutive
//!   900-tick windows in one run, so a background compile on the host does
//!   not fail the row while a real regression (which raises every window)
//!   still does. The tick p99 is over all three windows.
//!
//! The tests take a lock and run one at a time, so none is timed against
//! another's workers.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use mantis_adapter_contract::{Channel, ConnectionId};
use mantis_server::cell::OutboundSink;
use mantis_server::jobs::WorkerSet;
use toy_server::scenario::Crowd;
use toy_server::tunables::Tunables;

/// Job durations in 100 ns buckets up to 1 ms; the last bucket is overflow.
const BUCKETS: usize = 10_001;
static JOB_NS: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];

fn timing_wrapper(job: &mut dyn FnMut()) {
    let start = Instant::now();
    job();
    let ns = start.elapsed().as_nanos();
    let bucket = ((ns / 100) as usize).min(BUCKETS - 1);
    JOB_NS[bucket].fetch_add(1, Ordering::Relaxed);
}

fn histogram_p99_ns() -> (u64, u64) {
    let counts: Vec<u64> = JOB_NS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let total: u64 = counts.iter().sum();
    let want = total - total / 100;
    let mut seen = 0;
    for (i, c) in counts.iter().enumerate() {
        seen += c;
        if seen >= want {
            return ((i as u64 + 1) * 100, total);
        }
    }
    (u64::MAX, total)
}

#[derive(Default)]
struct Bytes(u64);

impl OutboundSink for Bytes {
    fn send(&mut self, _a: usize, _c: ConnectionId, _ch: Channel, bytes: &[u8]) {
        self.0 += bytes.len() as u64;
    }
}

/// Measurement windows of the encode row; its p99 is the least-loaded one.
const WINDOWS: usize = 3;

/// The timing tests run one at a time: each measures `cell-500-100` with
/// its own workers, and running them side by side would time each one
/// against the others.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn p99(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * 0.99) as usize]
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing budgets run in release builds")]
fn cell_500_100_tick_and_encode_p99() {
    let _serial = serial();
    let t = Tunables::defaults().unwrap();
    let mut crowd = Crowd::cell_500_100(&t, 7).unwrap();
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).clamp(1, 4));
    let workers = WorkerSet::new(threads, 256, Some(timing_wrapper));
    let mut sink = Bytes::default();
    for _ in 0..120 {
        crowd.tick(&mut sink, Some(&workers)).unwrap();
    }
    for c in &JOB_NS {
        c.store(0, Ordering::Relaxed);
    }
    let mut ticks = Vec::with_capacity(900 * WINDOWS);
    let mut windows = Vec::with_capacity(WINDOWS);
    for _ in 0..WINDOWS {
        for c in &JOB_NS {
            c.store(0, Ordering::Relaxed);
        }
        for _ in 0..900 {
            crowd.push_inputs();
            let start = Instant::now();
            let r = crowd.cell.tick(&mut sink, Some(&workers)).unwrap();
            ticks.push(start.elapsed().as_secs_f64() * 1000.0);
            crowd.ack_all(r.tick);
        }
        let (p99_ns, jobs) = histogram_p99_ns();
        assert_eq!(jobs, 900 * crowd.clients());
        windows.push(p99_ns);
    }
    let tick_p99 = p99(ticks.clone());
    let tick_mean = ticks.iter().sum::<f64>() / ticks.len() as f64;
    let encode_p99_ns = windows.iter().copied().min().unwrap_or(u64::MAX);
    let all: Vec<String> = windows
        .iter()
        .map(|ns| format!("{:.1}", *ns as f64 / 1000.0))
        .collect();
    eprintln!(
        "budget: cell-500-100 on {threads} worker threads: tick p99 {tick_p99:.3} ms (mean {tick_mean:.3} ms; target < 4 ms); per-client encode p99 <= {:.1} us, least-loaded of windows [{}] us, 900 x {} jobs each (target < 20 us)",
        encode_p99_ns as f64 / 1000.0,
        all.join(", "),
        crowd.clients()
    );
    assert!(tick_p99 < 4.0);
    assert!(encode_p99_ns < 20_000);
}

/// The script runtime's budget row (decision 0001): a server script that
/// loops forever is stopped by its instruction budget, and `cell-500-100`
/// still meets its tick budget.
#[test]
#[cfg_attr(debug_assertions, ignore = "timing budgets run in release builds")]
fn cell_500_100_with_an_infinite_loop_script_meets_the_tick_budget() {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use mantis_core::ecs::EntityId;
    use mantis_script::Limits;
    use mantis_server::scripting::{ScriptModule, ScriptSource, Scripts};

    let _serial = serial();

    let t = Tunables::defaults().unwrap();
    let scripts = ScriptModule::new(
        "toy.scripts",
        vec![
            ScriptSource {
                name: "spin".into(),
                source: "function on_tick() while true do end end".into(),
                owner: EntityId::new(1, 0),
            },
            ScriptSource {
                name: "watch".into(),
                source: "function on_tick(t) local x = host.position(mantis.owner()) if x then host.set('x', math.floor(x)) end end"
                    .into(),
                owner: EntityId::new(1, 0),
            },
        ],
        Limits::DEFAULT,
    );
    let set = toy_server::world::module_set_with(
        &BTreeMap::new(),
        &["[module]\nkey = \"toy.scripts\"\nversion = \"0.1.0\"\n"],
        vec![Arc::new(scripts)],
    )
    .unwrap();
    let mut crowd = Crowd::with_modules(&t, 500, 100, 9, &set).unwrap();
    let mut sink = Bytes::default();
    for _ in 0..60 {
        crowd.tick(&mut sink, None).unwrap();
    }
    let mut ticks = Vec::with_capacity(300);
    for _ in 0..300 {
        crowd.push_inputs();
        let start = Instant::now();
        let r = crowd.cell.tick(&mut sink, None).unwrap();
        ticks.push(start.elapsed().as_secs_f64() * 1000.0);
        crowd.ack_all(r.tick);
    }
    let stats = crowd.cell.world().resource::<Scripts>().unwrap().stats;
    let tick_p99 = p99(ticks);
    eprintln!(
        "budget: cell-500-100 with an infinite-loop script: tick p99 {tick_p99:.3} ms (target < 4 ms); budget stops {}",
        stats.budget_exhausted
    );
    assert!(stats.budget_exhausted >= 300, "the loop was stopped every tick");
    assert!(tick_p99 < 4.0);
}

/// The Ops inspector's cost (plan 13): the same `cell-500-100` timed and
/// untimed, ticked alternately so drift hits both alike. Timing every
/// system, graph evaluation and per-client encode must not move the tick
/// row; publishing the inspector's report (every 30 ticks, after the
/// tick) is measured on its own.
#[test]
#[cfg_attr(debug_assertions, ignore = "timing budgets run in release builds")]
fn the_inspector_does_not_move_the_cell_500_100_tick_row() {
    use std::sync::Arc;

    use mantis_core::schedule::Stopwatch;
    use toy_server::cluster::OsStopwatch;

    let _serial = serial();

    let t = Tunables::defaults().unwrap();
    let mut off = Crowd::cell_500_100(&t, 7).unwrap();
    let mut on = Crowd::cell_500_100(&t, 7).unwrap();
    let stopwatch: Arc<dyn Stopwatch> = Arc::new(OsStopwatch::new());
    on.cell.set_stopwatch(stopwatch);
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).clamp(1, 4));
    let workers = WorkerSet::new(threads, 256, None);
    let mut sink = Bytes::default();
    for _ in 0..120 {
        off.tick(&mut sink, Some(&workers)).unwrap();
        on.tick(&mut sink, Some(&workers)).unwrap();
    }
    let (mut t_off, mut t_on, mut publish) = (Vec::with_capacity(900), Vec::with_capacity(900), Vec::new());
    for i in 0..900 {
        for (crowd, out) in [(&mut off, &mut t_off), (&mut on, &mut t_on)] {
            crowd.push_inputs();
            let start = Instant::now();
            let r = crowd.cell.tick(&mut sink, Some(&workers)).unwrap();
            out.push(start.elapsed().as_secs_f64() * 1000.0);
            crowd.ack_all(r.tick);
        }
        if i % 30 == 0 {
            let start = Instant::now();
            let timings = on.cell.timings();
            let report = mantis_services::inspect::system_times(
                1,
                timings.tick.0,
                timings.systems.iter().map(|(n, p, t)| (*n, *p, *t)),
                timings.inbox_depth,
                timings.encode,
            );
            let names = on.cell.component_names();
            publish.push(start.elapsed().as_secs_f64() * 1000.0);
            assert!(report.systems.len() > 1 && !names.is_empty());
            assert!(
                report.systems.iter().all(|s| s.runs > 0),
                "every system ran and was timed"
            );
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let (p_off, p_on) = (p99(t_off.clone()), p99(t_on.clone()));
    let (m_off, m_on) = (mean(&t_off), mean(&t_on));
    let publish_max = publish.iter().copied().fold(0.0, f64::max);
    eprintln!(
        "budget: inspector cost on cell-500-100: tick p99 {p_on:.3} ms timed vs {p_off:.3} ms untimed; mean {m_on:.3} vs {m_off:.3} ms (+{:.1}%); report publish max {publish_max:.3} ms every 30 ticks (target: tick row unchanged, < 4 ms)",
        (m_on / m_off - 1.0) * 100.0
    );
    assert!(p_on < 4.0, "the tick row holds with the inspector on");
    assert!(m_on < m_off * 1.05 + 0.02, "timing costs under 5% of a tick");
}
