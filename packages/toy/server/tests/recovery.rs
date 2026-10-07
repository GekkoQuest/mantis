//! Recovery and chaos under the simulated network (decision 0007, plan
//! 7.1): a cell host crashing mid-soak, and a clean shutdown followed by a
//! deploy, each printing a `budget:` line with its recovery time in ticks;
//! and a social role restart during party changes (timing-sensitive, see
//! its comment).

#![expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

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
    CellLink::start(&cluster.handle(), &link_config(cluster)).unwrap()
}

fn link_config(cluster: &LocalCluster) -> CellLinkConfig {
    {
        CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops.public_key(),
            cells: world::regions()
                .into_iter()
                .enumerate()
                .map(|(i, r)| (i as u64 + 1, "127.0.0.1:7400".to_owned(), r))
                .collect(),
            instances: Vec::new(),
            poll: Duration::from_millis(10),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        }
    }
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
    if toy_server::world::skip_unless_linked(
        "std.containers",
        "a crashed cell host recovers from snapshot and log losing no acknowledged outcome",
    ) {
        return;
    }
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
    if toy_server::world::skip_unless_linked(
        "std.containers",
        "clean shutdown snapshots and a new build recovers from the snapshot alone",
    ) {
        return;
    }
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

/// A crashed role's address, dropping every connection. A closed port would
/// do the same, but a refused loopback connect takes two seconds on
/// Windows, and every step waits for every call the link makes.
struct Outage {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Outage {
    fn at(addr: std::net::SocketAddr) -> Self {
        // The stopped server's listener closes as its accept task ends.
        let mut bound = std::net::TcpListener::bind(addr);
        for _ in 0..200 {
            if bound.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            bound = std::net::TcpListener::bind(addr);
        }
        let listener = bound.unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => drop(conn),
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Outage {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Ticks after the restart until the invitation lands: the budget.
const RESTART_ANSWER_LIMIT: u64 = 16;
/// Ticks after that until all three rosters agree: the budget.
const ROSTER_CONVERGED_LIMIT: u64 = 12;

/// The cells and their clients run on the simulated network; the roles and
/// the cell host's link run on one manual clock that advances one tick per
/// step, and every step waits for the link to settle (everything the step
/// queued and every poll and retry the advance made due has run). So the
/// tick counts are the same on every machine: a budget, not a metric. RPC
/// still crosses loopback; only its timeouts are wall time.
///
/// With `tls`, every role and the link run mutual TLS: the link's clients
/// handshake again with the restarted role.
fn social_restart_ticks(tls: bool) -> (u64, u64) {
    use mantis_core::wire::Wire;
    use mantis_services::host::clock::{ManualClock, ServiceClock};
    let clock = ManualClock::new(1_700_000_000_000);
    let mut config = ClusterConfig::local();
    config.clock = ServiceClock::manual(&clock);
    let ids = tls.then(|| {
        mantis_services::tls::dev::DevCa::new("recovery")
            .unwrap()
            .every_role(&["127.0.0.1".parse().unwrap()])
            .unwrap()
    });
    config.tls.clone_from(&ids);
    let mut cluster = LocalCluster::start(&config).unwrap();
    let t = Tunables::defaults().unwrap();
    let mut link_config = link_config(&cluster);
    link_config.tls = ids.map(|ids| mantis_services::tls::TlsHandle::from(ids[&Role::Cell].clone()));
    let link = CellLink::start_on(&cluster.handle(), &link_config, cluster.clock()).unwrap();
    let mut sim = Sim::new(t, SEED, |_| None).unwrap();
    // Three characters in the first cell, on lossy links.
    for _ in 0..3 {
        sim.add_bot(Side::Native, Profile::Idle, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    let tick = Duration::from_millis(u64::from(1000 / t.tick_rate.hz()));
    let step = |sim: &mut Sim| {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
        clock.advance(tick);
        let settled = link.settle(Duration::from_secs(30));
        assert!(settled.is_ok(), "tick {}: {settled:?}", sim.ticks());
    };
    let until = |sim: &mut Sim, what: &str, limit: u64, done: &dyn Fn(&Sim) -> bool| -> u64 {
        let mut ticks = 0;
        while !done(sim) {
            assert!(ticks < limit * 4, "{what}: not within {} ticks", limit * 4);
            step(sim);
            ticks += 1;
        }
        ticks
    };
    until(&mut sim, "everyone in", 60, &|s| {
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
    until(&mut sim, "the first invitation", 60, &|s| invited(s, 1));
    accept(&mut sim, 1, 1);
    until(&mut sim, "the first roster", 60, &|s| {
        (0..2).all(|b| last_roster(s, b).is_some_and(|r| r.2 == vec![1, 2]))
    });
    let party = last_roster(&sim, 0).unwrap().0;

    // The social role crashes. While it is down, the leader invites a third
    // member: the operation is retried until a role answers.
    let addr = cluster.stop_social().unwrap();
    let outage = Outage::at(addr);
    invite(&mut sim, 0, 2);
    for _ in 0..30 {
        step(&mut sim);
    }
    assert!(!invited(&sim, 2), "nothing answers while the role is down");
    drop(outage);
    cluster.start_social(addr).unwrap();
    assert_eq!(cluster.social.party_of(1), None, "the new run starts empty");

    // The new run gets this host's projection before the invitation, so the
    // invitation lands on the existing party; the third member accepts.
    let restart_ticks = until(
        &mut sim,
        "the invitation after the restart",
        RESTART_ANSWER_LIMIT,
        &|s| invited(s, 2),
    );
    accept(&mut sim, 2, 1);
    let converge_ticks = until(&mut sim, "the roster converged", ROSTER_CONVERGED_LIMIT, &|s| {
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
    (restart_ticks, converge_ticks)
}

#[test]
fn a_social_restart_during_party_changes_converges_with_no_member_lost() {
    if toy_server::world::skip_unless_linked("std.party", "the social restart during party changes") {
        return;
    }
    let first = social_restart_ticks(false);
    assert_eq!(
        social_restart_ticks(false),
        first,
        "a second run counts the same ticks"
    );
    // Over mutual TLS: the link's clients handshake with the restarted role
    // (and fail fast against the outage); the same ticks.
    assert_eq!(
        social_restart_ticks(true),
        first,
        "the same ticks over mutual TLS"
    );
    let (restart_ticks, converge_ticks) = first;
    println!(
        "budget: social restart: first answer {restart_ticks} ticks after the restart (limit {RESTART_ANSWER_LIMIT}), roster converged {converge_ticks} ticks later (limit {ROSTER_CONVERGED_LIMIT}); deterministic: the same on a second run and over mutual TLS"
    );
    assert!(restart_ticks <= RESTART_ANSWER_LIMIT);
    assert!(converge_ticks <= ROSTER_CONVERGED_LIMIT);
}

/// A crash and recovery leaves the writer's ledger exactly as the logs say:
/// the outcomes a recovery replay makes again keep their original ticks and
/// batch numbers, so the writer knows them as already durable (principle 7:
/// every durable change exactly once).
fn ledger_rows_survive_a_crash_recover_cycle_unchanged(store: mantis_services::cluster::StoreChoice) {
    const BOTS: u64 = 8;
    const CRASH: u64 = 330;
    let mut config = ClusterConfig::local();
    config.store = store;
    let cluster = LocalCluster::start(&config).unwrap();
    let t = Tunables::defaults().unwrap();
    let logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let mut sim = Sim::new(t, SEED, |i| {
        Some(writer(&logs[i], &header(&t, i, Tick(1), BUILD)))
    })
    .unwrap();
    let first = link(&cluster);
    for _ in 0..BOTS {
        sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
            .unwrap();
    }
    let mut snapshots: Vec<Vec<u8>> = vec![Vec::new(); 2];
    for tick in 1..=CRASH {
        if tick % 25 == 0 {
            grant_all(&sim, BOTS);
        }
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &first, &reports);
        if tick % SNAPSHOT_EVERY == 0 {
            for (i, cell) in sim.zone.cells().iter().enumerate() {
                snapshots[i] = cell.snapshot(BUILD, t.content).unwrap();
            }
        }
    }
    assert_eq!(first.flush(Duration::from_secs(10)), Ok(()));
    let rows = |c: &LocalCluster| -> Vec<(u64, u64, i64)> {
        let mut all: Vec<(u64, u64, i64)> = (1..=BOTS)
            .flat_map(|ch| {
                c.persist
                    .with_store(|s| s.ledger_of(ch))
                    .unwrap()
                    .into_iter()
                    .map(move |r| (ch, r.tick, r.delta))
            })
            .collect();
        all.sort_unstable();
        all
    };
    let before = rows(&cluster);
    assert!(!before.is_empty(), "grants made ledger rows");

    // The host crashes and recovers: replay remakes ticks 301..=330 and
    // their outcomes; the link pushes the log backlog again; the recovered
    // cells tick on and drain what the replay queued.
    drop(sim);
    drop(first);
    let link = link(&cluster);
    let mut cells = Vec::new();
    for i in 0..2 {
        let (cell, rec) =
            recover_cell(&t, i, SEED, &snapshots[i], &logs[i].bytes(), BUILD, |_| None).unwrap();
        assert_eq!((rec.snapshot_tick.0, rec.tick.0), (300, CRASH));
        let outcomes = logged_outcomes(&logs[i].bytes(), BUILD, t.content).unwrap();
        link.push(i as u64 + 1, outcomes);
        cells.push(cell);
    }
    let mut zone = mantis_server::zone::Zone::new(cells, world::regions()).unwrap();
    let mut sink = mantis_server::cell::NullSink;
    for _ in 0..5 {
        let reports = zone.step(&mut sink, None).unwrap();
        after_tick(&mut zone, &link, &reports);
    }
    assert_eq!(link.flush(Duration::from_secs(10)), Ok(()));
    let after = rows(&cluster);
    assert_eq!(after.len(), before.len(), "no row doubled, none lost");
    assert_eq!(after, before);
    assert!(
        after.iter().all(|(_, tick, _)| *tick <= CRASH),
        "nothing stamped with a new tick"
    );
}

#[test]
fn ledger_rows_survive_a_crash_recover_cycle_unchanged_in_memory() {
    if toy_server::world::skip_unless_linked(
        "std.containers",
        "ledger rows survive a crash recover cycle unchanged in memory",
    ) {
        return;
    }
    ledger_rows_survive_a_crash_recover_cycle_unchanged(mantis_services::cluster::StoreChoice::Memory);
}

#[test]
fn ledger_rows_survive_a_crash_recover_cycle_unchanged_on_postgres() {
    if toy_server::world::skip_unless_linked(
        "std.containers",
        "ledger rows survive a crash recover cycle unchanged on postgres",
    ) {
        return;
    }
    let Some(conn) = mantis_services::persist::pg::postgres_or_skip(
        "ledger_rows_survive_a_crash_recover_cycle_unchanged_on_postgres",
    ) else {
        return;
    };
    let schema = format!("mantis_recovery_{}", std::process::id());
    ledger_rows_survive_a_crash_recover_cycle_unchanged(
        mantis_services::cluster::StoreChoice::PostgresSchema(conn.clone(), schema.clone()),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let probe = mantis_services::persist::pg::PgStore::connect(&conn, &schema)
            .await
            .unwrap();
        probe.drop_schema(&schema).await.unwrap();
    });
}

/// A crash can tear the end of a cell's log at any byte of its last tick.
/// Through the deployed node's recovery path, every such cut recovers to
/// the last complete tick with exactly its state, discards the rest (and
/// says how much), and offers the writer only outcomes of complete ticks.
#[test]
fn a_log_torn_at_any_byte_of_its_last_tick_recovers_to_the_last_complete_tick() {
    use mantis_core::log::LogEntry;
    use toy_server::node::start_cell;
    use toy_server::recovery::write_snapshots;
    const TICKS: u64 = 170;
    if toy_server::world::skip_unless_linked(
        "std.containers",
        "a log torn at any byte of its last tick recovers to the last complete tick",
    ) {
        return;
    }
    let t = Tunables::defaults().unwrap();
    let dir = std::env::temp_dir().join(format!("mantis-torn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A fresh node: the world cells and one instance cell, its own logs.
    let cells: Vec<_> = (0..3).map(|i| start_cell(&t, i, SEED, &dir).unwrap().0).collect();
    let mut zone = world::zone_of(&t, cells, 1).unwrap();
    write_snapshots(&dir, &zone, &t).unwrap();
    let mut sink = mantis_server::cell::NullSink;
    let mut hashes = Vec::new();
    for tick in 1..=TICKS {
        if tick % 10 == 0 {
            // An economy command each time: outcomes in the log.
            assert!(zone.cells()[0].commands().push(grant(9)));
        }
        let reports = zone.step(&mut sink, None).unwrap();
        hashes.push(reports[0].state_hash);
        if tick == 150 {
            write_snapshots(&dir, &zone, &t).unwrap();
        }
    }
    drop(zone);
    let log_path = dir.join("cell-1-1.log");
    let log = std::fs::read(&log_path).unwrap();
    // Where tick 169 ends: everything after it is the last tick's records.
    let mut reader = LogReader::<CellLogSchema>::open(&log, toy_server::build_id(), t.content).unwrap();
    let mut end_169 = 0;
    while let Ok(Some(e)) = reader.next_entry() {
        if matches!(e, LogEntry::TickEnd { tick, .. } if tick.0 == TICKS - 1) {
            end_169 = reader.position();
        }
    }
    assert!(end_169 > 0 && end_169 < log.len());
    let snapshot = std::fs::read(dir.join("cell-1.snapshot")).unwrap();
    let mut cuts = 0;
    for cut in end_169..log.len() {
        let case = dir.join(format!("cut-{cut}"));
        std::fs::create_dir_all(&case).unwrap();
        std::fs::write(case.join("cell-1.snapshot"), &snapshot).unwrap();
        std::fs::write(case.join("cell-1-1.log"), &log[..cut]).unwrap();
        let (mut cell, outcomes, rec) = start_cell(&t, 0, SEED, &case).unwrap();
        let rec = rec.unwrap();
        assert_eq!(
            (rec.snapshot_tick.0, rec.tick.0),
            (150, TICKS - 1),
            "cut at {cut}"
        );
        assert_eq!(rec.discarded_bytes, cut - end_169, "cut at {cut}");
        assert_eq!(
            cell.world().state_hash(),
            hashes[usize::try_from(TICKS - 2).unwrap()],
            "cut at {cut}: exactly the state after the last complete tick"
        );
        assert!(outcomes.iter().all(|o| o.tick < TICKS), "cut at {cut}");
        // Nothing of the torn tick runs: the next tick executes no command
        // the torn records held.
        cell.drain_outcomes(|_, _| {});
        cell.tick(&mut mantis_server::cell::NullSink, None).unwrap();
        let mut ran = 0;
        cell.drain_outcomes(|_, _| ran += 1);
        assert_eq!(ran, 0, "cut at {cut}: a torn tick's command ran");
        let _ = std::fs::remove_dir_all(&case);
        cuts += 1;
    }
    println!(
        "torn log: {cuts} cuts across the last tick's {} bytes all recovered to tick {}",
        log.len() - end_169,
        TICKS - 1
    );
    let _ = std::fs::remove_dir_all(&dir);
}
