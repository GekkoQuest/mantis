use std::sync::{Arc, Mutex};

use super::*;
use crate::ecs::{Component, Write};
use crate::time::TickRate;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Counter(u32);
crate::impl_state_hash!(Counter { 0 });
impl Component for Counter {
    const NAME: &'static str = "test.counter";
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Other(u32);
crate::impl_state_hash!(Other { 0 });
impl Component for Other {
    const NAME: &'static str = "test.other";
}

fn ctx(tick: u64) -> TickContext {
    TickContext {
        tick: Tick(tick),
        rate: TickRate::HZ_30,
        seed: Seed(1),
    }
}

fn desc(name: &'static str, phase: Phase, priority: i32, access: Access) -> SystemDesc {
    SystemDesc {
        name,
        phase,
        priority,
        access,
    }
}

type Log = Arc<Mutex<Vec<&'static str>>>;

fn logger(log: &Log, name: &'static str) -> impl System + 'static {
    let log = Arc::clone(log);
    move |_: &mut World, _: &TickContext| -> Result<(), SystemError> {
        log.lock()
            .map_err(|_| SystemError::Invariant("poisoned"))?
            .push(name);
        Ok(())
    }
}

#[test]
fn phases_are_ordered() {
    assert_eq!(Phase::ALL.len(), 10);
    for (i, p) in Phase::ALL.iter().enumerate() {
        assert_eq!(p.index(), i);
    }
    assert!(Phase::Inbound < Phase::Persist);
    assert!(Phase::Interest.is_job_phase() && Phase::Outbound.is_job_phase());
    assert!(!Phase::Scripts.is_job_phase());
    let hot: Vec<_> = Phase::ALL
        .iter()
        .filter(|p| p.is_hot())
        .map(|p| p.name())
        .collect();
    assert_eq!(
        hot,
        [
            "Movement", "Combat", "Effects", "AI", "Scripts", "Interest", "Outbound"
        ]
    );
}

#[test]
fn resolved_order_ignores_registration_order() {
    let specs = [
        ("b.second", Phase::Movement, 10),
        ("a.first", Phase::Movement, 10),
        ("z.early", Phase::Movement, -5),
        ("t.timer", Phase::Timers, 0),
        ("p.persist", Phase::Persist, 0),
    ];
    let mut runs = Vec::new();
    for rotation in 0..specs.len() {
        let log: Log = Arc::default();
        let mut s = Schedule::new();
        let mut rotated = specs;
        rotated.rotate_left(rotation);
        for (name, phase, prio) in rotated {
            s.add(desc(name, phase, prio, Access::NONE), logger(&log, name))
                .unwrap();
        }
        let mut world = World::new();
        s.run_tick(&mut world, &ctx(1)).unwrap();
        runs.push(log.lock().unwrap().clone());
    }
    for r in &runs {
        assert_eq!(r, &["t.timer", "z.early", "a.first", "b.second", "p.persist"]);
    }
}

#[test]
fn registration_is_validated() {
    let mut world = World::new();
    world.register::<Counter>().unwrap();
    let writes = Access::builder(&mut world).write::<Counter>().build().unwrap();
    let mut s = Schedule::new();
    let noop = |_: &mut World, _: &TickContext| Ok(());
    assert_eq!(
        s.add(desc("", Phase::Ai, 0, Access::NONE), noop),
        Err(ScheduleError::EmptyName)
    );
    s.add(desc("x", Phase::Ai, 0, Access::NONE), noop).unwrap();
    assert_eq!(
        s.add(desc("x", Phase::Combat, 0, Access::NONE), noop),
        Err(ScheduleError::DuplicateName("x"))
    );
    for phase in [Phase::Interest, Phase::Outbound] {
        assert_eq!(
            s.add(desc("w", phase, 0, writes), noop),
            Err(ScheduleError::WriteInJobPhase { system: "w", phase })
        );
    }
    let reads = Access::builder(&mut world).read::<Counter>().build().unwrap();
    s.add(desc("r", Phase::Outbound, 0, reads), noop).unwrap();
}

#[test]
fn conflict_lint() {
    let mut world = World::new();
    world.register::<Counter>().unwrap();
    world.register::<Other>().unwrap();
    let w_counter = Access::builder(&mut world).write::<Counter>().build().unwrap();
    let r_counter = Access::builder(&mut world).read::<Counter>().build().unwrap();
    let r_other = Access::builder(&mut world).read::<Other>().build().unwrap();
    let noop = |_: &mut World, _: &TickContext| Ok(());
    let mut s = Schedule::new();
    s.add(desc("m.write", Phase::Movement, 0, w_counter), noop)
        .unwrap();
    s.add(desc("m.read", Phase::Movement, 5, r_counter), noop)
        .unwrap();
    s.add(desc("m.unrelated", Phase::Movement, 5, r_other), noop)
        .unwrap();
    s.add(desc("c.read", Phase::Combat, 0, r_counter), noop).unwrap();
    assert_eq!(s.lints().len(), 1, "{:?}", s.lints());
    assert!(matches!(
        s.lints()[0],
        Lint::Conflict {
            phase: Phase::Movement,
            first: "m.write",
            second: "m.read",
            ..
        }
    ));
    s.add(desc("m.tie", Phase::Movement, 0, r_counter), noop).unwrap();
    assert!(s.lints().iter().any(|l| matches!(
        l,
        Lint::AmbiguousOrder {
            phase: Phase::Movement,
            first: "m.tie",
            second: "m.write"
        }
    )));
}

#[test]
fn failure_stops_the_phase_and_names_the_system() {
    let log: Log = Arc::default();
    let mut s = Schedule::new();
    s.add(desc("a", Phase::Combat, 0, Access::NONE), logger(&log, "a"))
        .unwrap();
    s.add(
        desc("b", Phase::Combat, 1, Access::NONE),
        |_: &mut World, _: &TickContext| Err(SystemError::Invariant("boom")),
    )
    .unwrap();
    s.add(desc("c", Phase::Combat, 2, Access::NONE), logger(&log, "c"))
        .unwrap();
    s.add(desc("d", Phase::Persist, 0, Access::NONE), logger(&log, "d"))
        .unwrap();
    let mut world = World::new();
    let err = s.run_tick(&mut world, &ctx(3)).unwrap_err();
    assert_eq!(
        err,
        RunError {
            system: "b",
            phase: Phase::Combat,
            error: SystemError::Invariant("boom")
        }
    );
    assert_eq!(*log.lock().unwrap(), vec!["a"], "c and later phases did not run");
}

#[test]
fn run_tick_stamps_change_tick_and_systems_mutate() {
    let mut world = World::new();
    world.register::<Counter>().unwrap();
    let e = world.spawn((Counter(0),)).unwrap();
    let mut q = world.query::<(Write<Counter>,)>().unwrap();
    let access = Access::builder(&mut world).query(&q).build().unwrap();
    let mut s = Schedule::new();
    s.add(
        desc("timers.count", Phase::Timers, 0, access),
        move |w: &mut World, _: &TickContext| -> Result<(), SystemError> {
            q.for_each(&mut w.components, |_, (mut c,)| c.0 += 1)?;
            Ok(())
        },
    )
    .unwrap();
    for t in 1..=3 {
        s.run_tick(&mut world, &ctx(t)).unwrap();
    }
    assert_eq!(world.get::<Counter>(e), Some(&Counter(3)));
    assert_eq!(world.components.changed_tick::<Counter>(e), Some(Tick(3)));
    assert_eq!(
        s.phase_order(Phase::Timers).map(|d| d.name).collect::<Vec<_>>(),
        ["timers.count"]
    );
}

#[test]
fn a_disabled_group_keeps_its_place_and_is_skipped() {
    let log: Log = Arc::default();
    let mut s = Schedule::new();
    for name in [
        "std.party.tick",
        "std.partyx.tick",
        "std.party.cleanup",
        "core.move",
    ] {
        s.add(desc(name, Phase::Movement, 0, Access::NONE), logger(&log, name))
            .unwrap();
    }
    assert_eq!(
        s.set_enabled_under("std.party", false),
        2,
        "prefix must end at a dot"
    );
    assert!(!s.is_enabled("std.party.tick") && s.is_enabled("std.partyx.tick"));
    let mut world = World::new();
    s.run_tick(&mut world, &ctx(1)).unwrap();
    assert_eq!(*log.lock().unwrap(), ["core.move", "std.partyx.tick"]);
    assert_eq!(s.set_enabled_under("std.party", true), 2);
    log.lock().unwrap().clear();
    s.run_tick(&mut world, &ctx(2)).unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        [
            "core.move",
            "std.party.cleanup",
            "std.party.tick",
            "std.partyx.tick"
        ]
    );
}

/// A stopwatch that advances 1000 ns per read.
struct Steps(std::sync::atomic::AtomicU64);

impl Stopwatch for Steps {
    fn now_nanos(&self) -> u64 {
        self.0.fetch_add(1000, std::sync::atomic::Ordering::Relaxed)
    }
}

#[test]
fn systems_are_timed_only_with_a_stopwatch() {
    let log: Log = Arc::default();
    let mut world = World::new();
    let mut s = Schedule::new();
    s.add(
        desc("a.one", Phase::Timers, 0, Access::default()),
        logger(&log, "one"),
    )
    .unwrap();
    s.run_tick(&mut world, &ctx(1)).unwrap();
    let (name, phase, t) = s.timings().next().unwrap();
    assert_eq!((name, phase, t.runs()), ("a.one", Phase::Timers, 0));
    s.set_stopwatch(Arc::new(Steps(std::sync::atomic::AtomicU64::new(0))));
    for tick in 2..5 {
        s.run_tick(&mut world, &ctx(tick)).unwrap();
    }
    let (_, _, t) = s.timings().next().unwrap();
    assert_eq!(t.runs(), 3);
    assert_eq!(t.last_nanos(), 1000);
    assert_eq!(t.percentile_nanos(99), 1000);
}

#[test]
fn timing_percentiles_use_the_recent_window() {
    let mut t = Timing::default();
    assert_eq!(t.percentile_nanos(99), 0);
    for n in 1..=100u64 {
        t.record(n);
    }
    assert_eq!(t.percentile_nanos(50), 50);
    assert_eq!(t.percentile_nanos(99), 99);
    assert_eq!(t.percentile_nanos(100), 100);
    // The window forgets: TIMING_WINDOW later runs of 7 ns are all it holds.
    for _ in 0..TIMING_WINDOW {
        t.record(7);
    }
    assert_eq!(t.percentile_nanos(99), 7);
    assert_eq!(t.runs(), 100 + TIMING_WINDOW as u64);
    t.record(u64::MAX);
    assert_eq!(t.last_nanos(), u32::MAX, "saturates");
}
