//! Recovery and chaos under the simulated network (decision 0007, plan
//! 7.1): a cell host crashing mid-soak, and a clean shutdown followed by a
//! deploy, each printing a `budget:` line with its recovery time in ticks;
//! and a social role restart during party changes (timing-sensitive, see
//! its comment).

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use std::time::{Duration, Instant};

use mantis_core::log::{BuildId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::time::Tick;
use mantis_core::wire::Encoder;
use mantis_server::bots::Profile;
use mantis_server::cell::{BoxedSink, MemoryLog};
use mantis_server::intent::{CellIntent, CellLogSchema};
use mantis_server::modules::{ExtensionKind, ModuleCommand, Payload};
use mantis_server::simnet::LinkConfig;
use mantis_services::cluster::{CellLink, CellLinkConfig, ClusterConfig, LocalCluster, batch_seq};
use mantis_services::host::Role;
use toy_server::cluster::after_tick;
use toy_server::recovery::{logged_outcomes, recover_cell};
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

/// std.containers' service-only grant (packages/std/modules/containers).
const GRANT: u16 = 1043;
const GOLD_PER_GRANT: u64 = 5;
/// Snapshots are taken this often, in ticks: the recovery budget.
const SNAPSHOT_EVERY: u64 = 150;
const SEED: u64 = 77;
const BUILD: BuildId = BuildId([4; 32]);

fn grant(character: u64) -> ModuleCommand {
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes);
    e.u64(character);
    e.u32(7);
    e.u32(1);
    e.u64(GOLD_PER_GRANT);
    ModuleCommand {
        kind: ExtensionKind(GRANT),
        session: None,
        request: 0,
        payload: Payload::from_slice(&bytes).unwrap(),
    }
}

fn header(t: &Tunables, i: usize, start: Tick, build: BuildId) -> LogHeader {
    let cfg = world::cell_config(t, i, SEED);
    LogHeader {
        build,
        content: t.content,
        cell: cfg.id,
        seed: cfg.seed,
        start_tick: start,
    }
}

fn writer(log: &MemoryLog, h: &LogHeader) -> LogWriter<CellLogSchema, BoxedSink> {
    let sink: BoxedSink = Box::new(log.clone());
    LogWriter::create(sink, h, 1 << 22).unwrap()
}

fn link(cluster: &LocalCluster) -> CellLink {
    CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: cluster.addr(Role::Persist).unwrap(),
            ops: cluster.addr(Role::Ops).unwrap(),
            social: cluster.addr(Role::Social).unwrap(),
            matchmaking: cluster.addr(Role::Matchmaking).unwrap(),
            realm: cluster.addr(Role::Realm).unwrap(),
            live_key: cluster.ops.public_key(),
            cells: world::regions()
                .into_iter()
                .enumerate()
                .map(|(i, r)| (i as u64 + 1, "127.0.0.1:7400".to_owned(), r))
                .collect(),
            instances: Vec::new(),
            poll: Duration::from_millis(10),
            inspector: "127.0.0.1:0".parse().unwrap(),
        },
    )
    .unwrap()
}

/// Grants to every character in the world, through the cell hosting it.
fn grant_all(sim: &Sim, characters: u64) -> u64 {
    let mut n = 0;
    for c in 1..=characters {
        if let Some(cell) = sim.zone.route(SessionId(c)).and_then(|i| sim.zone.cells().get(i))
            && cell.commands().push(grant(c))
        {
            n += 1;
        }
    }
    n
}

#[test]
fn a_crashed_cell_host_recovers_from_snapshot_and_log_losing_no_acknowledged_outcome() {
    const BOTS: u64 = 16;
    const CRASH: u64 = 420;
    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let t = Tunables::defaults().unwrap();
    let logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let mut sim = Sim::new(t, SEED, |i| {
        Some(writer(&logs[i], &header(&t, i, Tick(1), BUILD)))
    })
    .unwrap();
    let first_link = link(&cluster);
    for k in 0..BOTS {
        let side = if k % 2 == 0 { Side::Native } else { Side::Legacy };
        sim.add_bot(side, Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    let mut snapshots: Vec<(u64, Vec<u8>)> = vec![(0, Vec::new()); 2];
    let mut hashes = vec![Vec::new(); 2];
    for tick in 1..=CRASH {
        if tick % 25 == 0 {
            grant_all(&sim, BOTS);
        }
        let reports = sim.step().unwrap();
        for (i, r) in reports.iter().enumerate() {
            assert_eq!(r.tick.0, tick);
            hashes[i].push(r.state_hash);
        }
        after_tick(&mut sim.zone, &first_link, &reports);
        if tick % SNAPSHOT_EVERY == 0 {
            for (i, cell) in sim.zone.cells().iter().enumerate() {
                snapshots[i] = (tick, cell.snapshot(BUILD, t.content).unwrap());
            }
        }
    }
    assert!(sim.host.stats.joined >= BOTS - 2, "{:?}", sim.host.stats);

    // The host crashes: the zone, its clients, and the link's unsent queue
    // are gone. The logs and snapshots survive.
    drop(sim);
    drop(first_link);

    let link = link(&cluster);
    let new_logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let mut cells = Vec::new();
    for i in 0..2 {
        let start = Instant::now();
        let (cell, rec) = recover_cell(&t, i, SEED, &snapshots[i].1, &logs[i].bytes(), BUILD, |next| {
            Some(writer(&new_logs[i], &header(&t, i, next, BUILD)))
        })
        .unwrap();
        assert_eq!(rec.snapshot_tick.0, snapshots[i].0);
        assert_eq!(rec.tick.0, CRASH, "every acknowledged tick was flushed");
        assert_eq!(
            cell.world().state_hash(),
            hashes[i][usize::try_from(CRASH - 1).unwrap()]
        );
        assert!(rec.replayed <= SNAPSHOT_EVERY);
        println!(
            "budget: cell {} recovery replayed {} ticks after the snapshot of tick {} (limit {SNAPSHOT_EVERY} ticks), {} ms",
            i + 1,
            rec.replayed,
            rec.snapshot_tick.0,
            start.elapsed().as_millis()
        );
        cells.push(cell);
    }

    // Every outcome the logs hold reaches the writer exactly once.
    let mut expected_gold = 0i64;
    for (i, log) in logs.iter().enumerate() {
        let cell_id = i as u64 + 1;
        let outcomes = logged_outcomes(&log.bytes(), BUILD, t.content).unwrap();
        expected_gold += i64::try_from(outcomes.iter().filter(|o| o.ok && o.kind == GRANT).count()).unwrap()
            * i64::try_from(GOLD_PER_GRANT).unwrap();
        let last = outcomes.last().map(|o| o.tick);
        link.push(cell_id, outcomes.clone());
        let start = Instant::now();
        loop {
            let stored = cluster.persist.with_store(|s| s.outcomes_of(cell_id)).unwrap();
            if stored.len() == outcomes.len() {
                break;
            }
            assert!(stored.len() <= outcomes.len(), "an outcome was written twice");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the writer never caught up"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            cluster.persist.with_store(|s| s.last_batch(cell_id)).unwrap(),
            last.map(|tick| batch_seq(tick, 0)),
            "the watermark is the last logged tick's batch"
        );
    }
    assert!(expected_gold > 0);
    let durable_gold: i64 = (1..=BOTS)
        .map(|c| {
            cluster
                .persist
                .with_store(|s| s.ledger_of(c))
                .unwrap()
                .iter()
                .filter(|r| r.item == 0)
                .map(|r| r.delta)
                .sum::<i64>()
        })
        .sum();
    assert_eq!(
        durable_gold, expected_gold,
        "no acknowledged grant was lost or doubled"
    );

    // The recovered cells run on: sessions without clients are ended.
    let mut zone = mantis_server::zone::Zone::new(cells, world::regions()).unwrap();
    for i in 0..zone.cells().len() {
        let cell = zone.cell_mut(i).unwrap();
        for s in cell.detached_sessions() {
            assert!(cell.inbox().push(s, CellIntent::Leave));
        }
    }
    let mut sink = mantis_server::cell::NullSink;
    for k in 1..=10 {
        let reports = zone.step(&mut sink, None).unwrap();
        assert!(reports.iter().all(|r| r.tick.0 == CRASH + k));
    }
    for (cell, log) in zone.cells().iter().zip(&new_logs) {
        assert!(cell.detached_sessions().is_empty());
        // The new log continues where the recovery left off.
        let bytes = log.bytes();
        let reader = LogReader::<CellLogSchema>::open(&bytes, BUILD, t.content).unwrap();
        assert_eq!(reader.header().start_tick.0, CRASH + 1);
    }
}

#[test]
fn clean_shutdown_snapshots_and_a_new_build_recovers_from_the_snapshot_alone() {
    const BOTS: u64 = 8;
    const TICKS: u64 = 200;
    let t = Tunables::defaults().unwrap();
    let logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let mut sim = Sim::new(t, SEED, |i| {
        Some(writer(&logs[i], &header(&t, i, Tick(1), BUILD)))
    })
    .unwrap();
    for _ in 0..BOTS {
        sim.add_bot(Side::Native, Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    let mut last = [0u64; 2];
    for tick in 1..=TICKS {
        if tick % 20 == 0 {
            grant_all(&sim, BOTS);
        }
        let reports = sim.step().unwrap();
        for (i, r) in reports.iter().enumerate() {
            last[i] = r.state_hash;
        }
    }
    // Clean shutdown: every cell ends in a snapshot.
    let snapshots: Vec<Vec<u8>> = sim
        .zone
        .cells()
        .iter()
        .map(|c| c.snapshot(BUILD, t.content).unwrap())
        .collect();
    drop(sim);

    // A deploy: another build. The old segments are refused; the snapshot
    // alone brings every cell back at the shutdown tick.
    let deployed = BuildId([9; 32]);
    for i in 0..2 {
        assert!(matches!(
            LogReader::<CellLogSchema>::open(&logs[i].bytes(), deployed, t.content),
            Err(mantis_core::log::LogError::BuildMismatch { .. })
        ));
        let start = Instant::now();
        let new_log = MemoryLog::default();
        let (cell, rec) = recover_cell(&t, i, SEED, &snapshots[i], &logs[i].bytes(), deployed, |next| {
            Some(writer(&new_log, &header(&t, i, next, deployed)))
        })
        .unwrap();
        assert!(rec.log_refused);
        assert_eq!((rec.tick.0, rec.replayed), (TICKS, 0));
        assert_eq!(cell.world().state_hash(), last[i]);
        println!(
            "budget: cell {} recovery after a deploy replayed {} ticks (snapshot only, limit 0 ticks), {} ms",
            i + 1,
            rec.replayed,
            start.elapsed().as_millis()
        );
    }
}

/// std.party's registered kinds (packages/std/modules/party/contract).
const INVITE: u16 = 1000;
const ACCEPT: u16 = 1001;
const INVITED: u16 = 1004;
const ROSTER: u16 = 1005;

fn last_roster(sim: &Sim, bot: usize) -> Option<(u32, u64, Vec<u64>)> {
    use mantis_core::wire::{BoundedArray, Decoder, Wire};
    sim.bots[bot]
        .bot
        .stats
        .extension_messages
        .iter()
        .rev()
        .find(|(k, _)| *k == ExtensionKind(ROSTER))
        .map(|(_, b)| {
            let mut d = Decoder::new(b);
            let party = d.u32().unwrap();
            let leader = d.u64().unwrap();
            let members = BoundedArray::<u64, 5>::decode(&mut d).unwrap();
            (party, leader, members.iter().copied().collect())
        })
}

fn invited(sim: &Sim, bot: usize) -> bool {
    sim.bots[bot]
        .bot
        .stats
        .extension_messages
        .iter()
        .any(|(k, _)| *k == ExtensionKind(INVITED))
}

/// Timing-sensitive: the social role runs over real loopback RPC on the
/// wall clock (the cells and their clients run on the simulated network).
/// The test asserts only the invariants (no member lost, no duplicated or
/// missing projected update, one restart seen) and waits for convergence
/// with a generous wall-clock timeout; the tick counts are reported as a
/// `MANTIS-METRIC` line.
#[test]
fn a_social_restart_during_party_changes_converges_with_no_member_lost() {
    use mantis_core::wire::Wire;
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let t = Tunables::defaults().unwrap();
    let link = link(&cluster);
    let mut sim = Sim::new(t, SEED, |_| None).unwrap();
    // Three characters in the first cell, on lossy links.
    for _ in 0..3 {
        sim.add_bot(Side::Native, Profile::Idle, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    let step = |sim: &mut Sim| {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
        std::thread::sleep(Duration::from_millis(1));
    };
    let until = |sim: &mut Sim, what: &str, done: &dyn Fn(&Sim) -> bool| -> u64 {
        let start = Instant::now();
        let mut ticks = 0;
        while !done(sim) {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out: {what}");
            step(sim);
            ticks += 1;
        }
        ticks
    };
    until(&mut sim, "everyone in", &|s| {
        s.bots.iter().all(|b| b.bot.avatar().is_some()) && s.ticks() >= 31
    });
    let invite = |sim: &mut Sim, from: usize, to: usize| {
        let target = sim.bots[to].bot.avatar().unwrap();
        let mut bytes = Vec::new();
        target.encode(&mut Encoder::new(&mut bytes));
        sim.bots[from].bot.feature(ExtensionKind(INVITE), &bytes);
    };
    let accept = |sim: &mut Sim, who: usize, from_character: u64| {
        let mut bytes = Vec::new();
        Encoder::new(&mut bytes).u64(from_character);
        sim.bots[who].bot.feature(ExtensionKind(ACCEPT), &bytes);
    };
    // A party of two forms.
    invite(&mut sim, 0, 1);
    until(&mut sim, "the first invitation", &|s| invited(s, 1));
    accept(&mut sim, 1, 1);
    until(&mut sim, "the first roster", &|s| {
        (0..2).all(|b| last_roster(s, b).is_some_and(|r| r.2 == vec![1, 2]))
    });
    let party = last_roster(&sim, 0).unwrap().0;

    // The social role crashes. While it is down, the leader invites a third
    // member: the operation is retried until a role answers.
    let addr = cluster.stop_social().unwrap();
    invite(&mut sim, 0, 2);
    for _ in 0..30 {
        step(&mut sim);
    }
    assert!(!invited(&sim, 2), "nothing answers while the role is down");
    cluster.start_social(addr).unwrap();
    assert_eq!(cluster.social.party_of(1), None, "the new run starts empty");

    // The new run gets this host's projection before the invitation, so the
    // invitation lands on the existing party; the third member accepts.
    let restart_ticks = until(&mut sim, "the invitation after the restart", &|s| invited(s, 2));
    accept(&mut sim, 2, 1);
    let converge_ticks = until(&mut sim, "the roster converged", &|s| {
        (0..3).all(|b| last_roster(s, b).is_some_and(|r| r == (party, 1, vec![1, 2, 3])))
    });
    for c in 1..=3 {
        assert_eq!(cluster.social.party_of(c), Some(party), "member {c} lost");
    }
    let stats = &link.stats;
    let get = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(get(&stats.projected_duplicates), 0);
    assert_eq!(get(&stats.projected_gaps), 0);
    assert!(get(&stats.restores) >= 1);
    assert_eq!(get(&stats.social_restarts), 1);
    // The cells are deterministic; the roles answer over loopback RPC on
    // the wall clock, so these counts vary between machines: a metric, not
    // a budget.
    println!(
        "MANTIS-METRIC social_restart_ticks_to_first_answer={restart_ticks} roster_converged_ticks_later={converge_ticks}"
    );
}
