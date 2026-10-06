//! Every role in one process, and a cell host's link to them: the service
//! graph, outcomes pushed to the writer in order, signed live changes
//! reaching the host, and the read-only inspector.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::wire::encode_into;
use mantis_services::cluster::{CellLink, CellLinkConfig, CellOutcome, ClusterConfig, LocalCluster};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::ops::Command;

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn trade(tick: u64, character: u64, delta: i64) -> CellOutcome {
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character,
        item: GOLD,
        delta,
    })
    .unwrap();
    let mut bytes = Vec::new();
    encode_into(&l, &mut bytes);
    let mut payload = [0u8; 512];
    payload[..bytes.len()].copy_from_slice(&bytes);
    CellOutcome {
        tick,
        kind: 1050,
        session: 3,
        ok: true,
        payload,
        len: bytes.len(),
    }
}

#[test]
fn all_roles_run_in_one_process_and_a_cell_host_links_to_them() {
    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let graph = cluster.graph();
    for role in ["account", "realm", "social", "persist", "matchmaking", "ops"] {
        assert!(graph.contains(&format!("  {role}")), "{graph}");
    }
    assert!(graph.contains("https") && graph.contains("Push: cell"), "{graph}");
    assert!(cluster.dashboard_addr().unwrap().ip().is_loopback());

    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: cluster.addr(Role::Persist).unwrap(),
            ops: cluster.addr(Role::Ops).unwrap(),
            social: cluster.addr(Role::Social).unwrap(),
            matchmaking: cluster.addr(Role::Matchmaking).unwrap(),
            realm: cluster.addr(Role::Realm).unwrap(),
            live_key: cluster.ops.public_key(),
            cells: vec![(1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0))],
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
        },
    )
    .unwrap();
    assert!(cluster.realm.cells().contains_key(&1), "the cell registered");

    // Outcomes reach the writer, in order, with their ledger rows.
    link.push(1, vec![trade(5, 40, -30), trade(5, 41, 30)]);
    link.push(1, vec![trade(6, 40, -5)]);
    wait_for("durable batches", || {
        link.stats
            .durable_batches
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
    });
    let rows = cluster.persist.with_store(|s| s.ledger_of(40)).unwrap();
    assert_eq!(rows.iter().map(|r| r.delta).collect::<Vec<_>>(), vec![-30, -5]);
    // Batches are numbered by tick, so a recovered host numbers them alike.
    assert_eq!(
        cluster.persist.with_store(|s| s.last_batch(1)).unwrap(),
        Some(mantis_services::cluster::batch_seq(6, 0))
    );

    // An Ops flag reaches the host, verified.
    cluster
        .execute(
            "alice",
            &Command::Flag {
                name: "std.chat.whispers".to_owned(),
                on: false,
            },
        )
        .unwrap();
    let mut got = Vec::new();
    wait_for("the live change", || {
        got.extend(link.live_changes());
        !got.is_empty()
    });
    assert_eq!((got[0].name.as_str(), got[0].value), ("std.chat.whispers", 0.0));

    // The inspector answers Ops with what the host published.
    link.summary(m::CellSummary {
        cell: m::CellNo(1),
        tick: 77,
        state_hash: 0xfeed,
        sessions: 2,
        entities: 9,
        cheats: 0,
    });
    cluster.add_cell(1, link.inspector().unwrap());
    let seen = cluster.execute("alice", &Command::Inspect { cell: 1 }).unwrap();
    assert!(seen.after.contains("tick=77") && seen.after.contains("state_hash=000000000000feed"));
    assert_eq!(seen.undo, "none: read-only");
    drop(link);
}
