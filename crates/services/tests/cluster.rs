//! Every role in one process, and a cell host's link to them: the service
//! graph, outcomes pushed to the writer in order, signed live changes
//! reaching the host, and the read-only inspector.

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::wire::encode_into;
use mantis_services::cluster::{CellLink, CellLinkConfig, CellOutcome, ClusterConfig, LocalCluster};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::ops::Command;
use mantis_services::tls::dev::DevCa;
use mantis_services::tls::{TlsHandle, TlsIdentity};

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
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: vec![(1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0))],
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap();
    assert!(cluster.realm().cells().contains_key(&1), "the cell registered");

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

/// A cell host's link to `cluster`, with `tls` as its identity.
fn link_to(cluster: &LocalCluster, tls: Option<std::sync::Arc<TlsIdentity>>) -> Result<CellLink, String> {
    CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: vec![(1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0))],
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: tls.map(TlsHandle::from),
        },
    )
}

#[test]
fn every_role_and_a_cell_host_link_run_over_mutual_tls() {
    let ca = DevCa::new("local").unwrap();
    let ids = ca.every_role(&["127.0.0.1".parse().unwrap()]).unwrap();
    let mut config = ClusterConfig::local();
    config.tls = Some(ids.clone());
    let cluster = LocalCluster::start(&config).unwrap();

    // A host without a certificate cannot even register.
    assert!(link_to(&cluster, None).is_err());
    // Nor can one holding another role's certificate.
    assert!(link_to(&cluster, Some(ids[&Role::Gateway].clone())).is_err());

    let link = link_to(&cluster, Some(ids[&Role::Cell].clone())).unwrap();
    assert!(cluster.realm().cells().contains_key(&1), "the cell registered");
    link.push(1, vec![trade(5, 40, -30), trade(5, 41, 30)]);
    wait_for("a durable batch", || {
        link.stats
            .durable_batches
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    });
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
    // Ops reaches the inspector over TLS, expecting a cell host.
    link.summary(m::CellSummary {
        cell: m::CellNo(1),
        tick: 77,
        state_hash: 0xfeed,
        sessions: 2,
        entities: 9,
        cheats: 0,
    });
    cluster.try_add_cell(1, link.inspector().unwrap()).unwrap();
    let seen = cluster.execute("alice", &Command::Inspect { cell: 1 }).unwrap();
    assert!(seen.after.contains("tick=77"), "{}", seen.after);

    // A role missing from the identities is a start error.
    let mut partial = ids;
    partial.remove(&Role::Social);
    let mut config = ClusterConfig::local();
    config.tls = Some(partial);
    let refused = LocalCluster::start(&config).err().unwrap_or_default();
    assert!(refused.contains("social"), "{refused}");
    drop(link);
}

#[test]
fn a_draining_host_flushes_every_queued_outcome_and_relay_before_it_exits() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: vec![(1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0))],
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap();
    assert_eq!(link.pending(), 0);
    assert_eq!(link.flush(Duration::ZERO), Ok(()));

    // The writer is down: what is queued stays pending, and a flush that
    // runs out of time says how much.
    let writer = cluster.stop_role(Role::Persist).unwrap();
    link.push(1, vec![trade(5, 40, -30)]);
    link.push(1, vec![trade(6, 40, -5)]);
    link.relay(1, mantis_core::social::PARTY_OP, &{
        let mut b = Vec::new();
        encode_into(&mantis_core::social::PartyOp::Leave { me: 40 }, &mut b);
        b
    });
    assert_eq!(
        link.flush(Duration::from_millis(150)),
        Err(2),
        "the relay lands; the outcomes wait"
    );

    // Back up: within the push retry cap the flush completes, and every
    // outcome is durable.
    cluster.start_persist(writer).unwrap();
    let start = Instant::now();
    assert_eq!(link.flush(Duration::from_secs(5)), Ok(()));
    assert!(
        start.elapsed() < mantis_services::cluster::PUSH_RETRY_MAX * 4,
        "{:?}",
        start.elapsed()
    );
    let rows = cluster.persist.with_store(|s| s.ledger_of(40)).unwrap();
    assert_eq!(rows.iter().map(|r| r.delta).collect::<Vec<_>>(), vec![-30, -5]);
}

#[test]
fn a_restarted_realm_gets_every_cell_registered_again() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: vec![
                (1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0)),
                (2, "127.0.0.1:7400".to_owned(), (0.0, 1000.0)),
            ],
            poll: Duration::from_millis(20),
            instances: vec![(10, "127.0.0.1:7400".to_owned())],
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap();
    let before = cluster.realm().epoch();
    assert_eq!(cluster.realm().cells().len(), 3);

    let addr = cluster.stop_role(Role::Realm).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    cluster.start_realm(addr).unwrap();
    assert_ne!(cluster.realm().epoch(), before, "a new run");
    assert!(cluster.realm().cells().is_empty());
    // The link counts the restart once every cell is registered into it.
    wait_for("cells registered again", || {
        cluster.realm().cells().len() == 3
            && link
                .stats
                .realm_restarts
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
    });
    let cells = cluster.realm().cells();
    assert!(cells.get(&10).is_some_and(|c| c.instance && !c.busy));
    assert_eq!(
        link.stats
            .realm_restarts
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn after_an_ops_restart_every_cell_holds_and_re_receives_the_current_flags_and_tunables() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Persist).unwrap()),
            ops: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Ops).unwrap()),
            social: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Social).unwrap()),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(
                cluster.addr(Role::Matchmaking).unwrap(),
            ),
            realm: mantis_services::host::rpc::Endpoint::fixed(cluster.addr(Role::Realm).unwrap()),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: vec![(1, "127.0.0.1:7400".to_owned(), (-1000.0, 0.0))],
            poll: Duration::from_millis(20),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap();
    let flag = |name: &str, on| Command::Flag {
        name: name.to_owned(),
        on,
    };
    let tunable = |name: &str, value| Command::Tunable {
        name: name.to_owned(),
        value,
    };
    cluster
        .execute("alice", &flag("std.chat.whispers", false))
        .unwrap();
    cluster
        .execute("alice", &tunable("std.vendor.markup", 1.5))
        .unwrap();
    // Set twice: the durable value is the latest.
    cluster
        .execute("alice", &tunable("std.vendor.markup", 1.25))
        .unwrap();
    let held = |got: &[mantis_services::ops::live::Verified]| -> Vec<(String, f32)> {
        let mut now = std::collections::BTreeMap::new();
        for v in got {
            now.insert(v.name.clone(), v.value);
        }
        now.into_iter().collect()
    };
    let mut got = Vec::new();
    wait_for("three changes", || {
        got.extend(link.live_changes());
        got.len() == 3
    });
    let current = vec![
        ("std.chat.whispers".to_owned(), 0.0),
        ("std.vendor.markup".to_owned(), 1.25),
    ];
    assert_eq!(held(&got), current);

    // Ops restarts (a new process on the same writer and key): it publishes
    // the current values again, and the cell host follows the new run.
    let addr = cluster.stop_role(Role::Ops).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    let before = cluster.ops().live_epoch();
    cluster.start_ops(addr).unwrap();
    assert!(cluster.ops().live_epoch() > before, "a newer run");
    let mut again = Vec::new();
    wait_for("the current values again", || {
        again.extend(link.live_changes());
        again.len() == 2
    });
    assert_eq!(held(&again), current, "exactly the current values, once each");

    // A change in the new run reaches the host too: no gap stops the feed.
    cluster.execute("bob", &flag("std.party.invites", false)).unwrap();
    wait_for("a change after the restart", || {
        again.extend(link.live_changes());
        again.len() == 3
    });
    assert_eq!(
        link.stats.live_refused.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn a_cluster_runs_with_the_key_files_it_is_given() {
    let (signer, pkcs8) =
        mantis_services::ops::live::LiveSigner::generate(&ring::rand::SystemRandom::new()).unwrap();
    let mut config = ClusterConfig::local();
    config.key = Some(b"the cluster key from a file".to_vec());
    config.live_pkcs8 = Some(pkcs8);
    let cluster = LocalCluster::start(&config).unwrap();
    assert_eq!(cluster.key, b"the cluster key from a file");
    assert_eq!(cluster.ops().public_key(), signer.public_key());
    config.key = Some(Vec::new());
    assert!(LocalCluster::start(&config).is_err(), "an empty key is refused");
}
