//! Process-per-role clusters on loopback: every service role a `mantisd`
//! child process, cell hosts in this process through the cell-host role's
//! library API (the package binary's path, without a package).
//!
//! - A tampered, foreign-signed, or rolled-back registry is refused before
//!   anything but the health endpoint binds.
//! - Readiness blocks until dependencies are up.
//! - Roles killed and restarted mid-run: every cell host reconnects, and
//!   the recovery invariants hold (no acknowledged outcome lost, zero
//!   duplicates or gaps), with `budget:` lines for reconnect time in ticks.
//! - Ops in its own process audits through the writer and reaches cells.
//! - Every role drains cleanly.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::social::{PARTY_OP, PartyOp};
use mantis_core::wire::encode_into;
use mantis_deploy::cell::CellNode;
use mantis_deploy::config::NodeConfig;
use mantis_deploy::node::Node;
use mantis_services::cluster::{CellLink, CellOutcome, PUSH_RETRY_MAX, RELAY_RETRY_MAX};
use mantis_services::generated::services as m;
use mantis_services::host::rpc::{RpcClient, RpcError};
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use support::{CellSpec, Cluster, TestStore, service, wait_for};

/// The cell hosts' tick rate in these tests.
const HZ: u64 = 30;
const TICK: Duration = Duration::from_millis(1000 / HZ);
/// Characters per cell credited in turn, one ledger row per tick.
const CHARACTERS: u64 = 16;

/// A reconnect is one retry away once the role is back: the longest retry
/// wait (`PUSH_RETRY_MAX`, `RELAY_RETRY_MAX`, the link's poll), plus the
/// RPC client's own connect backoff, at the tick rate, doubled for a
/// loaded machine.
fn reconnect_limit_ticks() -> u64 {
    let longest = PUSH_RETRY_MAX.max(RELAY_RETRY_MAX).max(Duration::from_millis(20));
    let connect_backoff = Duration::from_millis(10 + 20 + 40 + 80);
    let ms = u64::try_from((longest + connect_backoff).as_millis()).unwrap();
    2 * ms.div_ceil(1000 / HZ)
}

/// The kind of the synthetic outcomes these tests push. The writer treats a
/// kind as opaque (it stores the outcome and takes ledger rows from any
/// successful one), so these tests depend on no module; the value is
/// outside every module's extension range.
const SYNTHETIC_KIND: u16 = 0xfff0;

fn grant(cell: u64, tick: u64) -> CellOutcome {
    let character = cell * 1000 + tick % CHARACTERS;
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character,
        item: GOLD,
        delta: i64::try_from(tick).unwrap(),
    })
    .unwrap();
    let mut bytes = Vec::new();
    encode_into(&l, &mut bytes);
    let mut payload = [0u8; 512];
    payload[..bytes.len()].copy_from_slice(&bytes);
    CellOutcome {
        tick,
        kind: SYNTHETIC_KIND,
        session: 0,
        ok: true,
        payload,
        len: bytes.len(),
    }
}

/// A cell host run in this process through `mantis_deploy::cell`, with a
/// log of every outcome of every completed tick (what a real cell host's
/// log holds).
struct Host {
    name: &'static str,
    cells: Vec<u64>,
    node: Option<CellNode>,
    link: Option<CellLink>,
    log: BTreeMap<u64, Vec<CellOutcome>>,
    tick: u64,
}

impl Host {
    fn start(cluster: &Cluster, name: &'static str) -> Self {
        let mut h = Self {
            name,
            cells: cluster.instance(name).cells.clone(),
            node: None,
            link: None,
            log: BTreeMap::new(),
            tick: 0,
        };
        h.boot(cluster);
        h
    }

    /// Starts the node and its link; after a crash, pushes every logged
    /// outcome again (the writer drops what is already durable).
    fn boot(&mut self, cluster: &Cluster) {
        let config = NodeConfig::load(&cluster.config(self.name)).unwrap();
        let node = Node::start(config, false).unwrap();
        node.wait_for_dependencies().unwrap();
        let node = CellNode::new(node).unwrap();
        let world: Vec<(u64, (f32, f32))> = self
            .cells
            .iter()
            // Cell c owns x in (100 (c - 2), 100 (c - 1)]: cell 2 holds x = 0,
            // where the realm places a new character.
            .map(|c| (*c, ((*c as f32 - 2.0) * 100.0, (*c as f32 - 1.0) * 100.0)))
            .collect();
        let link = node.link(&world, &[]).unwrap();
        for (cell, outcomes) in &self.log {
            link.push(*cell, outcomes.clone());
        }
        node.ready();
        self.node = Some(node);
        self.link = Some(link);
    }

    fn link(&self) -> &CellLink {
        self.link.as_ref().unwrap()
    }

    fn step(&mut self) {
        self.tick += 1;
        for cell in &self.cells {
            let o = grant(*cell, self.tick);
            self.log.entry(*cell).or_default().push(o);
            self.link().push(*cell, vec![o]);
        }
        let node = self.node.as_ref().unwrap();
        node.observe(self.tick, self.link());
    }

    /// A crash: the node and the link vanish with whatever was queued.
    fn crash(&mut self) {
        let node = self.node.take().unwrap();
        let link = self.link.take().unwrap();
        let handle = node.handle();
        {
            let _guard = handle.enter();
            drop(link);
        }
        drop(node);
    }

    /// A drain: flush, then stop.
    fn drain(&mut self) {
        let node = self.node.take().unwrap();
        node.finish(self.link.take().unwrap()).unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.node.is_some() && self.link.is_some() {
            self.crash();
        }
    }
}

fn stat(
    link: &CellLink,
    f: impl Fn(&mantis_services::cluster::LinkStats) -> &std::sync::atomic::AtomicU64,
) -> u64 {
    f(&link.stats).load(Ordering::Relaxed)
}

/// Ticks every host until `done`, at most `limit` ticks; returns the
/// ticks, or `None` at the limit.
fn try_tick_until(hosts: &mut [Host], limit: u64, done: &mut dyn FnMut(&[Host]) -> bool) -> Option<u64> {
    let mut ticks = 0;
    let mut next = Instant::now();
    while !done(hosts) {
        if ticks >= limit {
            return None;
        }
        for h in hosts.iter_mut() {
            h.step();
        }
        ticks += 1;
        next += TICK;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    Some(ticks)
}

/// [`try_tick_until`], failing the test at the limit.
fn tick_until(hosts: &mut [Host], limit: u64, what: &str, done: &mut dyn FnMut(&[Host]) -> bool) -> u64 {
    try_tick_until(hosts, limit, done).unwrap_or_else(|| panic!("{what}: not within {limit} ticks"))
}

/// Ticks every host while a restarted role comes up. Returns the ticks
/// from the process start to the role's first `/ready`, and per host the
/// ticks from that readiness to the host being `back` (0 when it
/// reconnected in the same tick, or while the role was finishing start-up).
fn reconnect_ticks(
    cluster: &mut Cluster,
    role: &str,
    hosts: &mut [Host],
    back: &dyn Fn(&Host) -> bool,
) -> (u64, Vec<u64>) {
    let mut t = 0u64;
    let mut ready_at: Option<u64> = None;
    let mut at: Vec<Option<u64>> = vec![None; hosts.len()];
    let mut exited = false;
    let done = try_tick_until(hosts, 60 * HZ, &mut |hs| {
        t += 1;
        if !cluster.running(role) {
            exited = true;
            return true;
        }
        if ready_at.is_none() && matches!(cluster.ready(role), Ok((200, _))) {
            ready_at = Some(t);
        }
        for (i, h) in hs.iter().enumerate() {
            if at[i].is_none() && back(h) {
                at[i] = Some(t);
            }
        }
        ready_at.is_some() && at.iter().all(Option::is_some)
    });
    assert!(
        done.is_some() && !exited,
        "hosts not back after {role} restarted (ready at tick {ready_at:?}, back at {at:?});          {role} answers {:?}:
{}",
        cluster.ready(role),
        cluster.output(role)
    );
    let ready = ready_at.unwrap_or(0);
    (
        ready,
        at.iter().map(|a| a.unwrap_or(0).saturating_sub(ready)).collect(),
    )
}

fn run_for(hosts: &mut [Host], ticks: u64) {
    let mut n = 0;
    tick_until(hosts, ticks + 1, "running", &mut |_| {
        n += 1;
        n > ticks
    });
}

/// Every ledger row the writer holds for the hosts' characters, by
/// character, read over RPC as Ops.
fn durable_rows(cluster: &Cluster, hosts: &[Host]) -> BTreeMap<u64, Vec<(u64, u64, i64)>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // As Ops would read it: Ops's own certificate, over mutual TLS.
    let persist = cluster.client(&service(Role::Ops), &service(Role::Persist));
    let mut out = BTreeMap::new();
    for h in hosts {
        for cell in &h.cells {
            for k in 0..CHARACTERS {
                let character = cell * 1000 + k;
                let req = m::LedgerOf {
                    character: m::CharacterId(character),
                };
                let rows = rt
                    .block_on(persist.call::<methods::Ledger>(&req, RPC_TIMEOUT))
                    .unwrap();
                assert!(
                    rows.rows.iter().count() < 64,
                    "the ledger page is full: the check would be blind"
                );
                let mut got: Vec<(u64, u64, i64)> =
                    rows.rows.iter().map(|r| (r.cell.0, r.tick, r.delta)).collect();
                got.sort_unstable();
                out.insert(character, got);
            }
        }
    }
    out
}

/// What the writer must hold: every logged outcome, once.
fn logged_rows(hosts: &[Host]) -> BTreeMap<u64, Vec<(u64, u64, i64)>> {
    let mut out: BTreeMap<u64, Vec<(u64, u64, i64)>> = BTreeMap::new();
    for h in hosts {
        for (cell, outcomes) in &h.log {
            for k in 0..CHARACTERS {
                out.entry(cell * 1000 + k).or_default();
            }
            for o in outcomes {
                out.entry(cell * 1000 + o.tick % CHARACTERS).or_default().push((
                    *cell,
                    o.tick,
                    i64::try_from(o.tick).unwrap(),
                ));
            }
        }
    }
    for rows in out.values_mut() {
        rows.sort_unstable();
    }
    out
}

/// Waits until every host's link has nothing pending, then checks the
/// writer holds exactly what the logs hold.
fn assert_no_outcome_lost_or_doubled(cluster: &Cluster, hosts: &mut [Host]) {
    let start = Instant::now();
    while !hosts.iter().all(|h| h.link().pending() == 0) {
        if start.elapsed() > Duration::from_secs(30) {
            let persist = service(Role::Persist);
            let pending: Vec<(&str, u64, u64)> = hosts
                .iter()
                .map(|h| (h.name, h.link().pending(), stat(h.link(), |s| &s.push_retries)))
                .collect();
            panic!(
                "links not flushed within 30 s: (host, pending, push retries) {pending:?};                  persist answers {:?}:
{}",
                cluster.ready(&persist),
                cluster.output(&persist)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let durable = durable_rows(cluster, hosts);
    let logged = logged_rows(hosts);
    let rows: usize = logged.values().map(Vec::len).sum();
    let mut duplicates = 0usize;
    let mut gaps = 0usize;
    for (character, want) in &logged {
        let got = &durable[character];
        let mut seen = std::collections::BTreeSet::new();
        duplicates += got.iter().filter(|r| !seen.insert(**r)).count();
        gaps += want.iter().filter(|r| !got.contains(r)).count();
        assert_eq!(
            got, want,
            "character {character}: the writer and the log disagree"
        );
    }
    println!("MANTIS-METRIC deploy_outcomes_checked={rows} duplicates={duplicates} gaps={gaps}");
    assert_eq!((duplicates, gaps), (0, 0));
}

const SERVICES: [Role; 6] = [
    Role::Persist,
    Role::Account,
    Role::Realm,
    Role::Social,
    Role::Matchmaking,
    Role::Ops,
];

fn start_services(cluster: &mut Cluster) {
    for role in SERVICES {
        cluster.start(&service(role));
    }
    for role in SERVICES {
        cluster.wait_ready(&service(role), Duration::from_secs(60));
    }
}

fn hosts_spec() -> Vec<CellSpec> {
    vec![
        CellSpec {
            name: "cells-a",
            cells: vec![1, 2],
            package: String::new(),
            game_tls: false,
        },
        CellSpec {
            name: "cells-b",
            cells: vec![3, 4],
            package: String::new(),
            game_tls: false,
        },
    ]
}

#[test]
fn a_tampered_registry_or_a_bad_certificate_is_refused_before_anything_binds() {
    let mut cluster = Cluster::new("tamper", &TestStore::Memory, &[]);
    let name = service(Role::Account);
    let path = cluster.dir.join("registry.toml");
    let good = std::fs::read_to_string(&path).unwrap();
    let account = cluster.instance(&name).clone();

    // One changed port in another instance's entry.
    let persist = cluster.instance(&service(Role::Persist)).rpc.clone();
    let persist = support::addr(&persist);
    let moved = std::net::SocketAddr::new(persist.ip(), persist.port().wrapping_add(1));
    std::fs::write(&path, good.replace(&persist.to_string(), &moved.to_string())).unwrap();
    cluster.start(&name);
    let status = cluster.wait_exit(&name, Duration::from_secs(30));
    assert_eq!(status.code(), Some(1));
    let out = cluster.output(&name);
    assert!(out.contains("does not verify with the deploy key"), "{out}");
    assert!(
        std::net::TcpStream::connect(support::addr(&account.health)).is_err()
            && std::net::TcpStream::connect(support::addr(&account.rpc)).is_err(),
        "nothing binds for a refused registry"
    );

    // Signed by another key.
    let (other, _) = mantis_deploy::keys::new_key_pair().unwrap();
    let other = ring::signature::Ed25519KeyPair::from_pkcs8(&other).unwrap();
    let body = cluster.registry.render();
    std::fs::write(&path, mantis_deploy::registry::sign(&body, &other)).unwrap();
    cluster.start(&name);
    assert_eq!(cluster.wait_exit(&name, Duration::from_secs(30)).code(), Some(1));
    assert!(cluster.output(&name).matches("does not verify").count() >= 2);

    // Unsigned.
    std::fs::write(&path, &body).unwrap();
    cluster.start(&name);
    assert_eq!(cluster.wait_exit(&name, Duration::from_secs(30)).code(), Some(1));
    assert!(cluster.output(&name).contains("not signed"));

    // An older serial than the node accepts (a rollback).
    std::fs::write(&path, &good).unwrap();
    let config = cluster.config(&name);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace("[node]\n", "[node]\nregistry_min_serial = 2\n"),
    )
    .unwrap();
    cluster.start(&name);
    assert_eq!(cluster.wait_exit(&name, Duration::from_secs(30)).code(), Some(1));
    assert!(cluster.output(&name).contains("rollback"));
    std::fs::write(&config, &text).unwrap();

    // Its own certificate expired, from another CA, or naming another
    // role: refused at start, before anything binds.
    let keys_dir = cluster.dir.join("keys");
    let ca = mantis_deploy::keys::read_ca(&keys_dir).unwrap();
    let today = mantis_deploy::pki::day_of(std::time::SystemTime::now());
    let crt = keys_dir.join(mantis_deploy::keys::files::cert(&name));
    let key = keys_dir.join(mantis_deploy::keys::files::key(&name));
    let (good_crt, good_key) = (std::fs::read(&crt).unwrap(), std::fs::read(&key).unwrap());
    let ip = [support::addr(&account.rpc).ip()];
    let other_ca = mantis_deploy::pki::new_ca(
        support::CLUSTER,
        mantis_deploy::pki::Validity::starting_now(std::time::SystemTime::now(), 7),
    )
    .unwrap();
    let ok = mantis_deploy::pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    for (leaf, why) in [
        (
            mantis_deploy::pki::issue(
                &ca,
                support::CLUSTER,
                Role::Account,
                &name,
                &ip,
                mantis_deploy::pki::Validity {
                    from_day: today - 9,
                    until_day: today - 1,
                },
            )
            .unwrap(),
            "xpired",
        ),
        (
            mantis_deploy::pki::issue(&other_ca, support::CLUSTER, Role::Account, &name, &ip, ok).unwrap(),
            "refused as a peer would refuse it",
        ),
        (
            mantis_deploy::pki::issue(&ca, support::CLUSTER, Role::Persist, &name, &ip, ok).unwrap(),
            "this node is",
        ),
    ] {
        std::fs::write(&crt, &leaf.cert_pem).unwrap();
        std::fs::write(&key, &leaf.key_pem).unwrap();
        cluster.start(&name);
        assert_eq!(cluster.wait_exit(&name, Duration::from_secs(30)).code(), Some(1));
        let out = cluster.output(&name);
        assert!(out.contains(why), "{why}:\n{out}");
    }
    std::fs::write(&crt, good_crt).unwrap();
    std::fs::write(&key, good_key).unwrap();

    // The good registry runs (once the writer it reads its rows from is
    // up), and drains cleanly.
    std::fs::write(&config, text).unwrap();
    let persist = service(Role::Persist);
    cluster.start(&persist);
    cluster.start(&name);
    cluster.wait_ready(&name, Duration::from_secs(30));
    assert_eq!(cluster.drain(&name, Duration::from_secs(30)).code(), Some(0));
    assert_eq!(cluster.drain(&persist, Duration::from_secs(30)).code(), Some(0));
}

#[test]
fn readiness_blocks_until_dependencies_are_up() {
    let mut cluster = Cluster::new("ready", &TestStore::Memory, &[]);
    let (social, persist) = (service(Role::Social), service(Role::Persist));
    cluster.start(&social);
    // It lives, says what it waits for, and serves nothing.
    wait_for("social's health endpoint", Duration::from_secs(30), || {
        mantis_deploy::health::probe_blocking(
            &cluster.instance(&social).health,
            "/live",
            Duration::from_millis(200),
        )
        .is_ok()
    });
    let held = Instant::now();
    while held.elapsed() < Duration::from_millis(1500) {
        let (code, body) = cluster.ready(&social).unwrap();
        assert_eq!(code, 503, "{body}");
        assert!(body.contains("waiting for persist (persist-1 at "), "{body}");
        // No RPC before the dependencies are ready: the node has not bound
        // its listener (it logs ": rpc <addr>" when it does). Asked of the
        // node itself, not of the port: another test process may hold the
        // same number.
        let out = cluster.output(&social);
        assert!(
            !out.contains(": rpc "),
            "no RPC before the dependencies are ready:
{out}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let started = Instant::now();
    cluster.start(&persist);
    cluster.wait_ready(&social, Duration::from_secs(30));
    let after = started.elapsed();
    println!(
        "MANTIS-METRIC deploy_social_ready_after_persist_start_ms={}",
        after.as_millis()
    );
    assert!(cluster.output(&social).contains("dependencies ready"));

    // Matchmaking waits for the realm; it is refused nothing, only held.
    let (mm, realm) = (service(Role::Matchmaking), service(Role::Realm));
    cluster.start(&mm);
    wait_for(
        "matchmaking says what it waits for",
        Duration::from_secs(30),
        || {
            cluster
                .ready(&mm)
                .is_ok_and(|(c, b)| c == 503 && b.contains("waiting for realm"))
        },
    );
    cluster.start(&realm);
    cluster.wait_ready(&mm, Duration::from_secs(30));
    for name in [&mm, &realm, &social, &persist] {
        assert_eq!(
            cluster.drain(name, Duration::from_secs(30)).code(),
            Some(0),
            "{name}"
        );
    }
}

#[test]
fn every_cell_host_reconnects_when_roles_restart_mid_run_and_no_outcome_is_lost() {
    let mut cluster = Cluster::new("restart", &TestStore::Memory, &hosts_spec());
    start_services(&mut cluster);
    let mut hosts = vec![Host::start(&cluster, "cells-a"), Host::start(&cluster, "cells-b")];
    let limit = reconnect_limit_ticks();
    run_for(&mut hosts, 60);

    // Party operations relayed through social, with presence.
    for h in &hosts {
        for cell in &h.cells {
            let here: Vec<u64> = (0..4).map(|k| cell * 1000 + k).collect();
            h.link().presence(*cell, &here);
        }
    }
    let invite = |h: &Host, cell: u64, from: u64, to: u64| {
        let mut b = Vec::new();
        encode_into(
            &PartyOp::Invite {
                from: cell * 1000 + from,
                to: cell * 1000 + to,
            },
            &mut b,
        );
        h.link().relay(cell, PARTY_OP, &b);
    };
    for h in &hosts {
        for cell in &h.cells {
            invite(h, *cell, 0, 1);
        }
    }
    run_for(&mut hosts, 30);

    // Social is killed mid-run. While it is down, each cell relays an
    // operation (retried until a role answers).
    let social = service(Role::Social);
    cluster.kill(&social);
    for h in &hosts {
        for cell in &h.cells {
            invite(h, *cell, 0, 2);
        }
    }
    run_for(&mut hosts, 20);
    let relayed: Vec<u64> = hosts.iter().map(|h| stat(h.link(), |s| &s.relayed)).collect();
    cluster.start(&social);
    let (up, back) = reconnect_ticks(&mut cluster, &social, &mut hosts, &|h| {
        stat(h.link(), |s| &s.social_restarts) == 1
    });
    for (h, ticks) in hosts.iter().zip(&back) {
        println!(
            "budget: {} reconnected to a restarted social role {ticks} ticks after it was ready (limit {limit} ticks; process start to ready {up} ticks)",
            h.name
        );
        assert!(
            *ticks <= limit,
            "{}: social reconnect took {ticks} ticks, limit {limit}",
            h.name
        );
    }
    let ticks = tick_until(
        &mut hosts,
        limit * 4,
        "the relays held while social was down",
        &mut |hs| {
            hs.iter()
                .zip(&relayed)
                .all(|(h, before)| stat(h.link(), |s| &s.relayed) >= before + 2)
        },
    );
    println!("MANTIS-METRIC deploy_relays_after_social_restart_ticks={ticks}");
    for h in &hosts {
        assert_eq!(stat(h.link(), |s| &s.projected_duplicates), 0, "{}", h.name);
        assert_eq!(stat(h.link(), |s| &s.projected_gaps), 0, "{}", h.name);
    }

    // Ops in its own process: audited through the writer, live changes
    // signed with the registry's key reach every cell host, the inspector
    // reaches a host by the registry's address.
    let cert = std::fs::read(cluster.dir.join("ops-cert.der")).unwrap();
    let (code, body) = support::https(
        cluster.dashboard,
        &cert,
        "POST",
        "/ops/flag",
        &cluster.token,
        "name=std.chat.whispers&on=false",
    );
    assert_eq!(code, 200, "{body}");
    let mut live: Vec<u64> = vec![0; hosts.len()];
    tick_until(&mut hosts, 300, "the live change reaches every host", &mut |hs| {
        for (i, h) in hs.iter().enumerate() {
            live[i] += h.link().live_changes().len() as u64;
        }
        live.iter().all(|n| *n == 1)
    });
    hosts[0].link().summary(m::CellSummary {
        cell: m::CellNo(1),
        tick: hosts[0].tick,
        state_hash: 0xbeef,
        sessions: 0,
        entities: 0,
        cheats: 0,
    });
    let (code, body) = support::https(cluster.dashboard, &cert, "GET", "/inspect/1", &cluster.token, "");
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("beef"), "{body}");
    let (code, audit) = support::https(cluster.dashboard, &cert, "GET", "/audit", &cluster.token, "");
    assert_eq!(code, 200, "{audit}");
    assert!(
        audit.contains("\"command\":\"flag\"") && audit.contains("\"command\":\"inspect\""),
        "{audit}"
    );

    // Ops is killed and restarted: it publishes the durable flag again as
    // its new run, and every host takes it once more, refusing nothing.
    let ops = service(Role::Ops);
    cluster.kill(&ops);
    run_for(&mut hosts, 10);
    cluster.start(&ops);
    let mut again: Vec<u64> = vec![0; hosts.len()];
    let (up, back) = reconnect_ticks(&mut cluster, &ops, &mut hosts, &|_| true);
    let _ = back;
    let ticks = tick_until(
        &mut hosts,
        60 * HZ,
        "the flag again after the Ops restart",
        &mut |hs| {
            for (i, h) in hs.iter().enumerate() {
                again[i] += h.link().live_changes().len() as u64;
            }
            again.iter().all(|n| *n >= 1)
        },
    );
    run_for(&mut hosts, 10);
    for (i, h) in hosts.iter().enumerate() {
        again[i] += h.link().live_changes().len() as u64;
        assert_eq!(again[i], 1, "{}: the current flag, exactly once", h.name);
        assert_eq!(stat(h.link(), |s| &s.live_refused), 0, "{}", h.name);
    }
    println!(
        "MANTIS-METRIC deploy_ops_restart_ticks process_start_to_ready={up} flag_again_after_ready={ticks}"
    );

    // The realm is killed and restarted with an empty directory: every
    // host registers its cells again.
    let realm = service(Role::Realm);
    cluster.kill(&realm);
    run_for(&mut hosts, 10);
    cluster.start(&realm);
    let (up, back) = reconnect_ticks(&mut cluster, &realm, &mut hosts, &|h| {
        stat(h.link(), |s| &s.realm_restarts) == 1
    });
    for (h, ticks) in hosts.iter().zip(&back) {
        println!(
            "budget: {} registered its cells with a restarted realm {ticks} ticks after it was ready (limit {limit} ticks; process start to ready {up} ticks)",
            h.name
        );
        assert!(
            *ticks <= limit,
            "{}: realm re-registration took {ticks} ticks, limit {limit}",
            h.name
        );
    }

    // A cell host crashes with outcomes queued, and comes back: it pushes
    // its log again; the writer applies each batch once.
    hosts[0].crash();
    run_for(&mut hosts[1..], 15);
    let start = Instant::now();
    hosts[0].boot(&cluster);
    let back = hosts[0].tick;
    let ticks = tick_until(
        &mut hosts,
        limit * 4,
        "the restarted host is durable again",
        &mut |hs| {
            hs[0].tick > back && hs[0].link().pending() == 0 && stat(hs[0].link(), |s| &s.durable_batches) > 0
        },
    );
    println!(
        "budget: cells-a durable again {ticks} ticks after its restart (limit {limit} ticks), {} ms including start-up",
        start.elapsed().as_millis()
    );
    assert!(ticks <= limit);
    run_for(&mut hosts, 30);
    assert_no_outcome_lost_or_doubled(&cluster, &mut hosts);

    // Every role drains: cell hosts flush, then services stop.
    for h in &mut hosts {
        h.drain();
    }
    for role in SERVICES.iter().rev() {
        let name = service(*role);
        assert_eq!(
            cluster.drain(&name, Duration::from_secs(30)).code(),
            Some(0),
            "{name}"
        );
        assert!(cluster.output(&name).contains("stopped"), "{name}");
    }
}

/// The persistence writer restarted on its database: needs PostgreSQL
/// (`MANTIS_TEST_POSTGRES`); skipped otherwise, as the services' own
/// PostgreSQL tests are.
///
/// Timing-sensitive (lead ruling): the writer is a child process whose
/// start-up, and the PostgreSQL round trips, run on the wall clock under
/// whatever load the machine has. The test asserts only the invariants
/// (every host reconnected and durable again, no outcome lost or
/// duplicated), waits with generous wall-clock timeouts, and reports the
/// tick counts as `MANTIS-METRIC` lines, not budgets.
#[test]
fn every_cell_host_reconnects_to_a_restarted_writer_and_no_outcome_is_lost() {
    let Some(conn) = mantis_services::persist::pg::postgres_or_skip(
        "every_cell_host_reconnects_to_a_restarted_writer_and_no_outcome_is_lost",
    ) else {
        return;
    };
    let schema = format!("deploy_{}", std::process::id());
    let mut cluster = Cluster::new(
        "writer",
        &TestStore::Postgres(conn.clone(), schema.clone()),
        &hosts_spec(),
    );
    start_services(&mut cluster);
    let mut hosts = vec![Host::start(&cluster, "cells-a"), Host::start(&cluster, "cells-b")];
    run_for(&mut hosts, 60);
    let persist = service(Role::Persist);
    cluster.kill(&persist);
    run_for(&mut hosts, 30);
    assert!(
        hosts.iter().all(|h| h.link().pending() > 0),
        "outcomes queue while the writer is down"
    );
    let before: Vec<u64> = hosts
        .iter()
        .map(|h| stat(h.link(), |s| &s.durable_batches))
        .collect();
    cluster.start(&persist);
    let (up, back) = reconnect_ticks(&mut cluster, &persist, &mut hosts, &|h| {
        let i = usize::from(h.name == "cells-b");
        stat(h.link(), |s| &s.durable_batches) > before[i]
    });
    for (h, ticks) in hosts.iter().zip(&back) {
        println!(
            "MANTIS-METRIC deploy_writer_reconnect_ticks host={} after_ready={ticks} process_start_to_ready={up}",
            h.name
        );
    }
    run_for(&mut hosts, 30);
    assert_no_outcome_lost_or_doubled(&cluster, &mut hosts);
    for h in &mut hosts {
        h.drain();
    }
    for role in SERVICES.iter().rev() {
        assert_eq!(
            cluster.drain(&service(*role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let pg = mantis_services::persist::pg::PgStore::connect(&conn, &schema)
            .await
            .unwrap();
        pg.drop_schema(&schema).await.unwrap();
    });
}

#[test]
fn mantisd_refuses_to_host_cells_and_names_the_package_command() {
    let mut cluster = Cluster::new("refuse", &TestStore::Memory, &hosts_spec()[..1]);
    cluster.start("cells-a");
    assert_eq!(
        cluster.wait_exit("cells-a", Duration::from_secs(30)).code(),
        Some(1)
    );
    let out = cluster.output("cells-a");
    assert!(out.contains("mantisd does not host cells"), "{out}");
    assert!(out.contains("node cell-host --config"), "{out}");
}

#[test]
fn rpc_between_processes_is_mutual_tls_with_the_caller_matrix_on_the_certificates_role() {
    use mantis_deploy::pki;
    use mantis_services::host::rpc::{RpcClient, RpcError};
    let mut cluster = Cluster::new("mtls", &TestStore::Memory, &hosts_spec()[..1]);
    let persist = service(Role::Persist);
    cluster.start(&persist);
    cluster.wait_ready(&persist, Duration::from_secs(30));
    let at = support::addr(&cluster.instance(&persist).rpc);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ledger = m::LedgerOf {
        character: m::CharacterId(1),
    };
    let call = |c: &RpcClient| rt.block_on(c.call::<methods::Ledger>(&ledger, RPC_TIMEOUT));

    // Ops's own certificate: answered.
    let ops = cluster.client(&service(Role::Ops), &persist);
    assert!(call(&ops).is_ok());

    // Plaintext, even with the right cluster key: refused.
    let plain = RpcClient::new(at, Role::Ops, cluster.key.clone());
    assert_eq!(call(&plain).unwrap_err(), RpcError::Disconnected);

    // A cell host's certificate calling an Ops-only method: the matrix
    // applies to the certificate's role.
    let cell_host = cluster.client("cells-a", &persist);
    assert_eq!(call(&cell_host).unwrap_err(), RpcError::Forbidden);

    // Ops's identity, but expired, or from another CA: refused at the
    // handshake.
    let ca = mantis_deploy::keys::read_ca(&cluster.dir.join("keys")).unwrap();
    let today = pki::day_of(std::time::SystemTime::now());
    let expired = pki::Validity {
        from_day: today - 9,
        until_day: today - 1,
    };
    let ok = pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    let other = pki::new_ca(support::CLUSTER, ok).unwrap();
    let ip = [support::addr(&cluster.instance(&service(Role::Ops)).rpc).ip()];
    for (signer, validity, what) in [(&ca, expired, "expired"), (&other, ok, "foreign CA")] {
        let leaf = pki::issue(
            signer,
            support::CLUSTER,
            Role::Ops,
            &service(Role::Ops),
            &ip,
            validity,
        )
        .unwrap();
        let id = mantis_services::tls::TlsIdentity::from_pem(
            ca.cert_pem.as_bytes(),
            leaf.cert_pem.as_bytes(),
            leaf.key_pem.as_bytes(),
        )
        .unwrap();
        let client = RpcClient::with_tls(
            at,
            Role::Ops,
            cluster.key.clone(),
            Some(std::sync::Arc::new(id)),
            Role::Persist,
        )
        .unwrap();
        assert_eq!(call(&client).unwrap_err(), RpcError::Disconnected, "{what}");
    }
    // The node still serves its rightful callers.
    assert!(call(&ops).is_ok());

    // The Ops dashboard admits only its allowed peers: from any other
    // address the connection is dropped before a TLS byte is sent.
    let ops_name = service(Role::Ops);
    let config = cluster.config(&ops_name);
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("[\"127.0.0.1/32\"]", "[\"127.0.0.2/32\"]")).unwrap();
    let account = service(Role::Account);
    cluster.start(&account);
    cluster.start(&ops_name);
    cluster.wait_ready(&ops_name, Duration::from_secs(30));
    let mut tcp = std::net::TcpStream::connect(cluster.dashboard).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // The start of a TLS ClientHello record.
    let _ = std::io::Write::write_all(&mut tcp, &[0x16, 0x03, 0x01, 0x00, 0x05, 0x01, 0, 0, 1, 0]);
    let mut got = [0u8; 64];
    // Closed at once (end of stream or a reset), not left waiting: a TLS
    // server would have answered this malformed hello with an alert.
    match std::io::Read::read(&mut tcp, &mut got) {
        Ok(n) => assert_eq!(n, 0, "a refused peer gets no TLS byte"),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            "the connection was not dropped: {e}"
        ),
    }
    for name in [&ops_name, &account, &persist] {
        assert_eq!(
            cluster.drain(name, Duration::from_secs(30)).code(),
            Some(0),
            "{name}"
        );
    }
}

/// A client calling instance `to` as a role with no instance in the
/// registry (the game front door, `gateway`, has no deployable process
/// yet): a certificate the test issues from the cluster CA.
fn client_as(cluster: &Cluster, role: Role, instance: &str, to: &str) -> RpcClient {
    use mantis_deploy::pki;
    let ca = mantis_deploy::keys::read_ca(&cluster.dir.join("keys")).unwrap();
    let ok = pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    let leaf = pki::issue(&ca, support::CLUSTER, role, instance, &[], ok).unwrap();
    let id = mantis_services::tls::TlsIdentity::from_pem(
        ca.cert_pem.as_bytes(),
        leaf.cert_pem.as_bytes(),
        leaf.key_pem.as_bytes(),
    )
    .unwrap();
    let server = cluster.instance(to);
    RpcClient::with_tls(
        support::addr(&server.rpc),
        role,
        cluster.key.clone(),
        Some(std::sync::Arc::new(id)),
        server.role,
    )
    .unwrap()
}

/// One call, made once more if it found its connection lost: a client
/// whose server restarted learns it on its next call, and reconnects on
/// the one after (the RPC client's documented contract). Any other answer
/// is final.
fn again<T>(
    mut call: impl FnMut() -> Result<T, mantis_services::host::rpc::RpcError>,
) -> Result<T, mantis_services::host::rpc::RpcError> {
    match call() {
        Err(mantis_services::host::rpc::RpcError::Disconnected) => call(),
        other => other,
    }
}

fn wire<const N: usize>(s: &str) -> mantis_core::wire::WireString<N> {
    mantis_core::wire::WireString::new(s).unwrap()
}

/// Account and realm killed and restarted: what they keep through the
/// writer comes back (accounts, passwords, bans, characters and where each
/// left the world); the single-use tokens they held in memory (session,
/// entry, transfer) are refused, so clients log in or select again; the
/// cell directory comes back when every cell host sees the realm's new
/// epoch and registers again. (In-world sessions through such a restart:
/// `toy::bots_play_...`.)
#[test]
fn account_and_realm_restarts_keep_accounts_and_characters_and_refuse_old_tokens() {
    let mut cluster = Cluster::new("accounts", &TestStore::Memory, &hosts_spec()[..1]);
    start_services(&mut cluster);
    let host = Host::start(&cluster, "cells-a");
    let (account_name, realm_name) = (service(Role::Account), service(Role::Realm));
    let account = client_as(&cluster, Role::Gateway, "gateway-test", &account_name);
    let realm = client_as(&cluster, Role::Gateway, "gateway-test", &realm_name);
    let ops_account = cluster.client(&service(Role::Ops), &account_name);
    let cell_account = cluster.client("cells-a", &account_name);
    let cell_realm = cluster.client("cells-a", &realm_name);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let login = |name: &str, password: &str| {
        again(|| {
            rt.block_on(account.call::<methods::LoginAccount>(
                &m::Login {
                    name: wire(name),
                    password: wire(password),
                },
                RPC_TIMEOUT,
            ))
        })
    };
    let verify = |token: &m::Session| {
        again(|| {
            rt.block_on(
                cell_account
                    .call::<methods::VerifySession>(&m::VerifyToken { token: token.token }, RPC_TIMEOUT),
            )
        })
    };
    let redeem = |token: &m::Placement, cell: u64| {
        again(|| {
            rt.block_on(cell_realm.call::<methods::RedeemToken>(
                &m::Redeem {
                    token: token.token,
                    cell: m::CellNo(cell),
                },
                RPC_TIMEOUT,
            ))
        })
    };

    // Before: two accounts, one banned; a character, placed by its cell
    // host where it leaves the world; tokens of every kind outstanding.
    for name in ["alpha", "beta"] {
        rt.block_on(account.call::<methods::RegisterAccount>(
            &m::Register {
                name: wire(name),
                password: wire("correct-horse-1"),
            },
            RPC_TIMEOUT,
        ))
        .unwrap();
    }
    let alpha = login("alpha", "correct-horse-1").unwrap();
    let beta = login("beta", "correct-horse-1").unwrap();
    rt.block_on(ops_account.call::<methods::BanAccount>(
        &m::Ban {
            account: beta.account,
            until_ms: u64::MAX / 2,
            reason: wire("test"),
        },
        RPC_TIMEOUT,
    ))
    .unwrap();
    assert!(
        login("beta", "correct-horse-1").is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "banned"
    );
    let character = rt
        .block_on(realm.call::<methods::NewCharacter>(
            &m::CreateCharacter {
                account: alpha.account,
                name: wire("walker"),
                kind: 1,
            },
            RPC_TIMEOUT,
        ))
        .unwrap()
        .character;
    let select = || {
        again(|| {
            rt.block_on(realm.call::<methods::Select>(
                &m::SelectCharacter {
                    account: alpha.account,
                    character,
                },
                RPC_TIMEOUT,
            ))
        })
    };
    let first = select().unwrap();
    assert_eq!(first.cell.0, 2, "a new character enters the cell owning x = 0");
    // It enters, crosses into cell 1, and leaves the world at (-42, 0, 7).
    host.link().track(2, &[(character.0, [5.0, 0.0, 1.0])]);
    host.link().track(2, &[]);
    host.link().track(1, &[(character.0, [-42.0, 0.0, 7.0])]);
    host.link().track(1, &[]);
    wait_for("the placements acknowledged", Duration::from_secs(30), || {
        host.link().pending() == 0
    });
    let session = login("alpha", "correct-horse-1").unwrap();
    let entry = select().unwrap();
    assert_eq!(entry.cell.0, 1, "a returning character enters the cell it left");
    let transfer = rt
        .block_on(cell_realm.call::<methods::Transfer>(
            &m::IssueTransfer {
                character,
                from: m::CellNo(2),
                to: m::CellNo(1),
                epoch: 1,
            },
            RPC_TIMEOUT,
        ))
        .unwrap();
    let transfer = m::Placement {
        cell: m::CellNo(1),
        address: wire(""),
        token: transfer.token,
    };

    // Both roles killed, then restarted on the same writer.
    cluster.kill(&account_name);
    cluster.kill(&realm_name);
    cluster.start(&account_name);
    cluster.start(&realm_name);
    cluster.wait_ready(&account_name, Duration::from_secs(30));
    cluster.wait_ready(&realm_name, Duration::from_secs(30));
    for name in [&account_name, &realm_name] {
        let out = cluster.output(name);
        let restarted = out.split("---- start 2 ----").nth(1).unwrap_or("");
        assert!(
            restarted.contains("read back"),
            "{name} read its rows back:\n{out}"
        );
    }

    // Kept: accounts and passwords, bans.
    let back = login("alpha", "correct-horse-1");
    assert!(back.is_ok(), "the account and its password: {back:?}");
    assert!(
        login("alpha", "wrong-horse-22").is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "the password is still checked"
    );
    assert!(
        login("beta", "correct-horse-1").is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "the ban"
    );
    // Refused: tokens of the old run (clients log in or select again).
    assert!(
        verify(&session).is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "a session token of the old run"
    );
    assert!(
        redeem(&entry, 1).is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "an entry token of the old run"
    );
    assert!(
        redeem(&transfer, 1).is_err_and(|e| matches!(e, RpcError::Refused(_))),
        "a transfer token of the old run"
    );
    let fresh = login("alpha", "correct-horse-1").unwrap();
    assert_eq!(
        verify(&fresh).unwrap().account,
        alpha.account,
        "a new session works"
    );
    // Kept: the character.
    let listed = rt
        .block_on(realm.call::<methods::ListAccountCharacters>(
            &m::ListCharacters {
                account: alpha.account,
            },
            RPC_TIMEOUT,
        ))
        .unwrap();
    assert_eq!(listed.ids.iter().copied().collect::<Vec<_>>(), vec![character]);
    // The directory comes back with the cell host's registration on the
    // new epoch; the returning character is then placed where it left.
    let waited = wait_for("the cell host registered again", Duration::from_secs(30), || {
        stat(host.link(), |s| &s.realm_restarts) >= 1
    });
    let mut placement = None;
    wait_for("a selection after the restart", Duration::from_secs(30), || {
        placement = select().ok();
        placement.is_some()
    });
    let placement = placement.unwrap();
    assert_eq!(placement.cell.0, 1, "the cell it left");
    let entered = redeem(&placement, 1).unwrap();
    assert!(entered.placed);
    assert_eq!(
        (entered.character, entered.x, entered.y, entered.z),
        (character, -42.0, 0.0, 7.0)
    );
    println!(
        "MANTIS-METRIC deploy_realm_reregistered_ms={}",
        waited.as_millis()
    );
    drop(host);
    for role in SERVICES.iter().rev() {
        assert_eq!(
            cluster.drain(&service(*role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
}
