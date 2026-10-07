//! The proving package through a process-per-role cluster: every service
//! role a `mantisd` process, the cell host the package's own binary
//! (`toy-server node cell-host`), and headless bots on both adapters
//! (native over QUIC, legacy over TCP) playing through it. The cell host is
//! killed mid-play and restarted: it recovers every cell from its snapshot
//! and log on disk, reconnects to every role, and new bots play again; no
//! acknowledged outcome is lost and none is doubled (the recovery
//! invariants of `packages/toy/server/tests/recovery.rs`, across
//! processes).
//!
//! The toy-server executable is not a dependency of this crate (engine
//! crates never depend on packages): it is run from the target directory,
//! next to `mantisd`. The gates build every target first; run alone, build
//! it with `cargo build -p toy-server` (same profile and target directory).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::indexing_slicing
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mantis_core::content::ContentHash;
use mantis_core::ledger::ledger_of;
use mantis_core::log::{BuildId, LogEntry, LogError, LogReader};
use mantis_server::intent::CellLogSchema;
use mantis_services::generated::services as m;
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use support::{CellSpec, Cluster, TestStore, service, wait_for};

/// The module whose service-only grant is the cell host's test load. Plan
/// 13: any module may be deleted and the tests stay green, so the checks
/// that need the grant run only when the package links it.
const CONTAINERS: &str = "std.containers";
/// Ticks between snapshots: the recovery budget.
const SNAPSHOT_EVERY: u64 = 150;
const HOST: &str = "cells-toy";

/// One ledger row: (character, cell, tick, item, change).
type Row = (u64, u64, u64, u32, i64);

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The toy-server executable beside `mantisd`, or a failure naming the
/// build command.
fn toy_server() -> PathBuf {
    let mantisd = Path::new(env!("CARGO_BIN_EXE_mantisd"));
    let name = format!("toy-server{}", std::env::consts::EXE_SUFFIX);
    let path = mantisd.with_file_name(&name);
    assert!(
        path.is_file(),
        "{} is missing: this test runs the toy package's cell host. Build it first with \
         `cargo build -p toy-server` (the same profile and CARGO_TARGET_DIR as this test), \
         as the gates' build step does.",
        path.display()
    );
    path
}

fn cooked() -> PathBuf {
    let dir = workspace().join("packages").join("toy").join("cooked");
    assert!(
        dir.join("keys").join("dev.pub").is_file(),
        "{} is not cooked: cargo run -p mantis-cook -- packages/toy",
        dir.display()
    );
    dir
}

/// Headless bots on one adapter, as `scripts/bots.ps1` runs them. They log
/// in first (the cell host redeems every entry token), on both adapters.
fn bots(cluster: &Cluster, legacy: bool, count: u32, seconds: u32, seed: u32) -> Child {
    let (quic, tcp) = cluster.game[HOST];
    let mut cmd = Command::new(toy_server());
    cmd.arg("bots")
        .args(["--count", &count.to_string(), "--seconds", &seconds.to_string()])
        .args(["--seed", &seed.to_string()])
        .arg("--cooked")
        .arg(cooked())
        .args(cluster.bot_login_args());
    if legacy {
        cmd.args(["--tcp", &tcp.to_string()]);
    } else {
        // The listener presents a chain from the cluster CA; bots trust the
        // CA bundle, never one pinned leaf, so they follow a rotation.
        cmd.args(["--quic", &quic.to_string(), "--ca"])
            .arg(cluster.dir.join("keys").join(mantis_deploy::keys::files::CA));
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

/// Waits for a bots process and returns what it printed.
fn bots_output(child: Child) -> String {
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "bots failed:\n{text}");
    text
}

/// The largest "N in world" a bots process reported.
fn most_in_world(text: &str) -> u64 {
    text.lines()
        .filter_map(|l| l.split(": ").nth(1)?.split(" in world").next()?.parse().ok())
        .max()
        .unwrap_or(0)
}

/// `MANTIS-RECOVERY` lines of the cell host's `run`th start: cell ->
/// (snapshot tick, replayed, tick). Other fields (`discarded`, the bytes of
/// a torn final record) are read and printed by the caller's log.
fn recovery(output: &str, run: u32) -> BTreeMap<u64, (u64, u64, u64)> {
    let marker = format!("---- start {run} ----");
    let section = output.split(&marker).nth(1).unwrap_or("");
    let section = section.split("---- start").next().unwrap_or("");
    section
        .lines()
        .filter_map(|l| l.strip_prefix("MANTIS-RECOVERY "))
        .map(|l| {
            let v: BTreeMap<&str, u64> = l
                .split(' ')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k, v.parse().unwrap()))
                .collect();
            (v["cell"], (v["snapshot_tick"], v["replayed"], v["tick"]))
        })
        .collect()
}

/// Whether the toy-server executable under test links module `key`: its
/// own resolved module graph, as `serve` prints it at start (it is run for
/// 0 ticks on ephemeral loopback ports). This crate cannot call the
/// package's `world::linked` (engine crates never depend on packages), so
/// it asks the binary the test runs. Fails closed: no graph, no answer.
fn linked(key: &str) -> bool {
    let out = Command::new(toy_server())
        .args([
            "serve",
            "--quic",
            "127.0.0.1:0",
            "--tcp",
            "127.0.0.1:0",
            "--ticks",
            "0",
            "--cooked",
        ])
        .arg(cooked())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && text.contains("modules of package"),
        "toy-server did not print its module graph:
{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .any(|l| l.trim_start().starts_with(&format!("{key} ")) && l.contains(" enabled"))
}

/// Every ledger row of every successful outcome every log of the state
/// directory holds, of every complete tick, as (character, cell, tick,
/// item, change), whatever module made it.
fn logged_rows(state: &Path) -> BTreeSet<Row> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(state).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        // The build and content the log was written with, from its own
        // header (opening with anything else names them).
        let (build, content) =
            match LogReader::<CellLogSchema>::open(&bytes, BuildId([0; 32]), ContentHash::of(b"")) {
                Err(LogError::BuildMismatch { found, .. }) => {
                    match LogReader::<CellLogSchema>::open(&bytes, found, ContentHash::of(b"")) {
                        Err(LogError::ContentMismatch { found: content, .. }) => (found, content),
                        other => panic!("{}: {:?}", path.display(), other.err()),
                    }
                }
                other => panic!("{}: {:?}", path.display(), other.err()),
            };
        let mut reader = LogReader::<CellLogSchema>::open(&bytes, build, content).unwrap();
        let cell = reader.header().cell.0;
        let first = reader.header().start_tick.0;
        let mut last = 0;
        let mut pending = Vec::new();
        loop {
            match reader.next_entry() {
                Ok(Some(LogEntry::Outcome { tick, outcome })) => {
                    if outcome.result.is_ok()
                        && let Some(l) = ledger_of(outcome.payload.as_slice())
                    {
                        for r in l.rows() {
                            pending.push((r.character, cell, tick.0, r.item, r.delta));
                        }
                    }
                }
                Ok(Some(LogEntry::TickEnd { tick, .. })) => {
                    last = tick.0;
                    out.extend(pending.drain(..));
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(LogError::TruncatedTail { .. }) => break,
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
        println!(
            "log {}: cell {cell}, ticks {first}..={last}, {} ledger rows not in a complete tick",
            path.display(),
            pending.len()
        );
    }
    out
}

/// The writer's ledger rows of `characters`, as (character, cell, tick,
/// item, change), read over RPC as Ops; and how many rows there were.
fn durable_rows(cluster: &Cluster, characters: &BTreeSet<u64>) -> (BTreeSet<Row>, usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // As Ops would read it: Ops's own certificate, over mutual TLS.
    let persist = cluster.client(&service(Role::Ops), &service(Role::Persist));
    let mut set = BTreeSet::new();
    let mut rows = 0;
    for c in characters {
        let req = m::LedgerOf {
            character: m::CharacterId(*c),
        };
        let got = rt
            .block_on(persist.call::<methods::Ledger>(&req, RPC_TIMEOUT))
            .unwrap();
        let n = got.rows.iter().count();
        assert!(
            n < 64,
            "character {c}: the ledger page is full, the check would be blind"
        );
        for r in got.rows.iter() {
            rows += 1;
            set.insert((*c, r.cell.0, r.tick, r.item, r.delta));
        }
    }
    (set, rows)
}

#[test]
fn bots_play_through_a_process_per_role_cluster_and_a_killed_cell_host_comes_back_losing_nothing() {
    let cooked = cooked();
    let toy = toy_server();
    let containers = linked(CONTAINERS);
    if !containers {
        println!(
            "skipped: {CONTAINERS} is not linked; the grant test load and its volume check are skipped \
             (1 module-dependent check skipped); every other check runs"
        );
    }
    let package = format!(
        "quic = \"{{quic}}\"\ntcp = \"{{tcp}}\"\ncert_out = \"toy-cert.der\"\ncooked = \"{}\"\n\
         seed = 7\ngrant_every_ticks = {}\n",
        cooked.display().to_string().replace('\\', "/"),
        if containers { 30 } else { 0 }
    );
    let mut cluster = Cluster::new(
        "toy",
        &TestStore::Memory,
        &[CellSpec {
            name: HOST,
            cells: vec![1, 2, 3],
            package,
            game_tls: true,
        }],
    );
    // The game listeners the registry does not carry: fill them in.
    let (quic, tcp) = cluster.game[HOST];
    let config = cluster.config(HOST);
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("{quic}", &quic.to_string())
        .replace("{tcp}", &tcp.to_string());
    std::fs::write(&config, text).unwrap();

    for role in [
        Role::Persist,
        Role::Account,
        Role::Realm,
        Role::Social,
        Role::Matchmaking,
        Role::Ops,
    ] {
        cluster.start(&service(role));
    }
    cluster.start_with(HOST, &toy, &["node"]);
    let up = cluster.wait_ready(HOST, Duration::from_secs(120));
    println!("MANTIS-METRIC deploy_toy_cell_host_ready_ms={}", up.as_millis());
    let first = recovery(&cluster.output(HOST), 1);
    assert_eq!(
        first.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3],
        "{first:?}"
    );
    assert!(
        first.values().all(|r| *r == (0, 0, 0)),
        "a fresh start: {first:?}"
    );

    // Native and legacy bots play; the cell host grants every 30 ticks.
    // While they play, the game listener's certificate is rotated in place
    // (new files from the cluster CA): open sessions keep their connection,
    // only new handshakes get the new chain.
    let native = bots(&cluster, false, 4, 8, 1);
    let legacy = bots(&cluster, true, 4, 8, 2);
    std::thread::sleep(Duration::from_secs(3));
    let keys_dir = cluster.dir.join("keys");
    let ca = mantis_deploy::keys::read_ca(&keys_dir).unwrap();
    let renewed = mantis_deploy::pki::issue_server(
        &ca,
        "game",
        &[quic.ip().to_string()],
        mantis_deploy::pki::Validity::starting_now(std::time::SystemTime::now(), 7),
    )
    .unwrap();
    std::fs::write(keys_dir.join(format!("game-{HOST}.key")), &renewed.key_pem).unwrap();
    std::fs::write(keys_dir.join(format!("game-{HOST}.crt")), &renewed.cert_pem).unwrap();
    wait_for("the game certificate rotated", Duration::from_secs(10), || {
        cluster
            .metrics(HOST)
            .get("game_tls_rotations")
            .copied()
            .unwrap_or(0)
            == 1
    });
    let native = bots_output(native);
    let legacy = bots_output(legacy);
    let final_line = native
        .lines()
        .rev()
        .find(|l| l.contains(" in world"))
        .unwrap_or("");
    assert!(
        most_in_world(final_line) == 4 && final_line.contains(" 0 refused"),
        "native bots across the game certificate rotation lost a session:\n{native}"
    );
    println!("native bots across a game certificate rotation: {final_line}");
    assert!(most_in_world(&native) >= 3, "native bots:\n{native}");
    assert!(most_in_world(&legacy) >= 3, "legacy bots:\n{legacy}");
    let before = cluster.metrics(HOST);
    assert!(
        before.get("cell_tick").copied().unwrap_or(0) > i64::try_from(SNAPSHOT_EVERY).unwrap(),
        "{before:?}"
    );

    // More bots join, and the cell host is killed while they play.
    let native = bots(&cluster, false, 4, 6, 3);
    let legacy = bots(&cluster, true, 4, 6, 4);
    std::thread::sleep(Duration::from_secs(3));
    let crashed_at = cluster.metrics(HOST).get("cell_tick").copied().unwrap_or(0);
    cluster.kill(HOST);
    let _ = bots_output(native);
    let _ = bots_output(legacy);

    // It comes back from its snapshots and logs, and every cell
    // reconnects to every role.
    let start = Instant::now();
    cluster.start_with(HOST, &toy, &["node"]);
    cluster.wait_ready(HOST, Duration::from_secs(120));
    let ready_ms = start.elapsed().as_millis();
    let second = recovery(&cluster.output(HOST), 2);
    assert_eq!(
        second.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3],
        "{second:?}"
    );
    for (cell, (snapshot_tick, replayed, tick)) in &second {
        println!(
            "budget: toy cell {cell} recovered after a cell-host kill: {replayed} ticks replayed after the snapshot of tick {snapshot_tick} (limit {SNAPSHOT_EVERY} ticks), at tick {tick}"
        );
        assert!(*replayed <= SNAPSHOT_EVERY, "cell {cell}");
        assert!(*snapshot_tick > 0, "cell {cell} came back from a snapshot");
    }
    let resumed = second[&1].2;
    assert!(
        resumed + 2 >= u64::try_from(crashed_at).unwrap(),
        "cell 1 resumed at tick {resumed}, the host was at {crashed_at} when killed"
    );
    let durable_before = cluster
        .metrics(HOST)
        .get("link_durable_batches")
        .copied()
        .unwrap_or(0);
    let tick_ready = cluster.metrics(HOST).get("cell_tick").copied().unwrap_or(0);
    // Nothing left unacknowledged; and, when the test load runs (so there
    // are outcomes to push), new batches durable through the new link.
    // Without the load nothing produces outcomes, so only the first holds.
    wait_for(
        "the restarted host's link durable",
        Duration::from_secs(60),
        || {
            let now = cluster.metrics(HOST);
            now.get("link_pending") == Some(&0)
                && (!containers || now.get("link_durable_batches").copied().unwrap_or(0) > durable_before)
        },
    );
    let tick_durable = cluster.metrics(HOST).get("cell_tick").copied().unwrap_or(0);
    println!(
        "MANTIS-METRIC deploy_toy_cell_host_restart process_start_to_ready_ms={ready_ms} ready_to_durable_ticks={}",
        tick_durable - tick_ready
    );

    // New bots play through the restarted host, on both adapters.
    // While they play, the account and realm roles are killed and
    // restarted: sessions already in the world do not depend on either.
    let native = bots(&cluster, false, 4, 8, 5);
    let legacy = bots(&cluster, true, 4, 8, 6);
    std::thread::sleep(Duration::from_secs(3));
    let tick_before = cluster.metrics(HOST).get("cell_tick").copied().unwrap_or(0);
    for role in [Role::Account, Role::Realm] {
        cluster.kill(&service(role));
    }
    for role in [Role::Account, Role::Realm] {
        cluster.start(&service(role));
    }
    for role in [Role::Account, Role::Realm] {
        cluster.wait_ready(&service(role), Duration::from_secs(30));
    }
    let native = bots_output(native);
    let legacy = bots_output(legacy);
    for (side, out) in [("native", &native), ("legacy", &legacy)] {
        let last = out.lines().rev().find(|l| l.contains(" in world")).unwrap_or("");
        assert!(
            most_in_world(last) >= 3 && last.contains(" 0 refused"),
            "{side} bots through an account and realm restart:\n{out}"
        );
        println!("{side} bots through an account and realm restart: {last}");
    }
    assert!(
        cluster.metrics(HOST).get("cell_tick").copied().unwrap_or(0) > tick_before,
        "the cells kept ticking"
    );

    // A clean drain: a final snapshot, the link flushed, exit 0.
    assert_eq!(cluster.drain(HOST, Duration::from_secs(60)).code(), Some(0));
    assert!(cluster.output(HOST).contains("drained: every outcome durable"));

    // Every ledger row every log holds is in the writer exactly once,
    // whichever modules made them.
    let logged = logged_rows(&cluster.dir.join("state").join(HOST));
    if containers {
        assert!(logged.len() > 20, "the test load granted: {}", logged.len());
    }
    let characters: BTreeSet<u64> = logged.iter().map(|g| g.0).collect();
    let (durable, rows) = durable_rows(&cluster, &characters);
    let lost = logged.difference(&durable).count();
    let unexpected = durable.difference(&logged).count();
    let duplicates = rows - durable.len();
    println!(
        "MANTIS-METRIC deploy_toy_ledger_rows_logged={} characters={} lost={lost} unexpected={unexpected} duplicates={duplicates}",
        logged.len(),
        characters.len()
    );
    if (lost, unexpected, duplicates) != (0, 0, 0) {
        println!("lost: {:?}", logged.difference(&durable).collect::<Vec<_>>());
        println!(
            "unexpected: {:?}",
            durable.difference(&logged).collect::<Vec<_>>()
        );
    }
    assert_eq!((lost, unexpected, duplicates), (0, 0, 0));

    for role in [
        Role::Ops,
        Role::Matchmaking,
        Role::Social,
        Role::Realm,
        Role::Account,
        Role::Persist,
    ] {
        assert_eq!(
            cluster.drain(&service(role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
}
