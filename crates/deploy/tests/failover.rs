//! Failover roles as processes: account, realm and social each run an
//! active and a standby instance (`mantisd` processes holding a lease
//! through the writer). The active instance of each is killed in turn
//! while a cell host runs; the standby takes over within one lease
//! lifetime, and callers (a gateway client, the cell host's link) reach it
//! through the same multi-instance address without being told. Accounts,
//! passwords, characters and the cell host's outcomes survive; the killed
//! instance comes back as a standby.

#![expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::social::{PARTY_OP, PartyOp};
use mantis_core::wire::{WireString, encode_into};
use mantis_deploy::cell::CellNode;
use mantis_deploy::config::NodeConfig;
use mantis_deploy::node::Node;
use mantis_deploy::pki;
use mantis_services::cluster::{CellLink, CellOutcome};
use mantis_services::generated::services as m;
use mantis_services::host::rpc::{Endpoint, RpcClient, RpcError};
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use mantis_services::tls::TlsIdentity;
use support::{CellSpec, Cluster, LEASE_TTL_MS, TestStore, service, standby, wait_for};

const FAILOVER: [Role; 3] = [Role::Account, Role::Realm, Role::Social];

fn wire<const N: usize>(s: &str) -> WireString<N> {
    WireString::new(s).unwrap()
}

/// A client of every instance of `role`, as `caller` (a certificate the
/// test issues from the cluster CA), through one multi-instance endpoint.
fn client(cluster: &Cluster, caller: Role, instance: &str, role: Role) -> RpcClient {
    let ca = mantis_deploy::keys::read_ca(&cluster.dir.join("keys")).unwrap();
    let ok = pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    let leaf = pki::issue(&ca, support::CLUSTER, caller, instance, &[], ok).unwrap();
    let id = TlsIdentity::from_pem(
        ca.cert_pem.as_bytes(),
        leaf.cert_pem.as_bytes(),
        leaf.key_pem.as_bytes(),
    )
    .unwrap();
    let targets: Vec<String> = cluster
        .registry
        .of(role)
        .iter()
        .map(|i| i.rpc.to_string())
        .collect();
    RpcClient::with_endpoint(
        Endpoint::new(&targets.join(",")).unwrap(),
        caller,
        cluster.key.clone(),
        Some(Arc::new(id).into()),
        role,
    )
    .unwrap()
}

/// Calls until one answers (a takeover in progress answers `Standby` or
/// not at all), for at most `limit`.
fn until_answered<T>(limit: Duration, mut call: impl FnMut() -> Result<T, RpcError>) -> (T, Duration) {
    let start = Instant::now();
    loop {
        match call() {
            Ok(v) => return (v, start.elapsed()),
            Err(e) => assert!(start.elapsed() < limit, "no answer within {limit:?}: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn credit(cell: u64, tick: u64) -> CellOutcome {
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character: cell * 1000 + tick % 4,
        item: GOLD,
        delta: 1,
    })
    .unwrap();
    let mut bytes = Vec::new();
    encode_into(&l, &mut bytes);
    let mut payload = [0u8; 512];
    payload[..bytes.len()].copy_from_slice(&bytes);
    CellOutcome {
        tick,
        // Opaque to the writer: no module is involved.
        kind: 0xfff0,
        session: 0,
        ok: true,
        payload,
        len: bytes.len(),
    }
}

fn active_of(cluster: &Cluster, role: Role) -> String {
    let (a, b) = (service(role), standby(role));
    let active = |n: &str| cluster.metrics(n).get("lease_active").copied() == Some(1);
    wait_for(&format!("an active {role:?}"), Duration::from_secs(20), || {
        active(&a) || active(&b)
    });
    if active(&a) { a } else { b }
}

#[test]
fn a_killed_active_instance_is_replaced_by_its_standby_and_nothing_is_lost() {
    let mut cluster = Cluster::with_standbys(
        "failover",
        &TestStore::Memory,
        &[CellSpec {
            name: "cells-a",
            cells: vec![1, 2],
            package: String::new(),
            game_tls: false,
        }],
        &FAILOVER,
    );
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
    for role in FAILOVER {
        cluster.start(&standby(role));
    }
    for role in [
        Role::Persist,
        Role::Account,
        Role::Realm,
        Role::Social,
        Role::Matchmaking,
        Role::Ops,
    ] {
        cluster.wait_ready(&service(role), Duration::from_secs(60));
    }
    for role in FAILOVER {
        cluster.wait_ready(&standby(role), Duration::from_secs(60));
        let (a, b) = (active_of(&cluster, role), standby(role));
        let other = if a == b { service(role) } else { b };
        wait_for(
            &format!("{other} ready as a standby"),
            Duration::from_secs(10),
            || {
                cluster
                    .ready(&other)
                    .is_ok_and(|(c, b)| c == 200 && b.contains("(standby)"))
            },
        );
        wait_for(
            &format!("{a} ready as the active instance"),
            Duration::from_secs(10),
            || {
                cluster
                    .ready(&a)
                    .is_ok_and(|(c, b)| c == 200 && b.contains("(active)"))
            },
        );
    }

    // A cell host, through the cell-host library API.
    let node = Node::start(NodeConfig::load(&cluster.config("cells-a")).unwrap(), false).unwrap();
    node.wait_for_dependencies().unwrap();
    let node = CellNode::new(node).unwrap();
    let link: CellLink = node.link(&[(1, (-100.0, 0.0)), (2, (0.0, 100.0))], &[]).unwrap();
    node.ready();
    let mut tick = 0u64;
    let mut step = |link: &CellLink| {
        tick += 1;
        for cell in [1, 2] {
            link.push(cell, vec![credit(cell, tick)]);
        }
    };

    // Before: an account, a character, a party operation relayed.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let account = client(&cluster, Role::Gateway, "gateway-test", Role::Account);
    let realm = client(&cluster, Role::Gateway, "gateway-test", Role::Realm);
    let login = || {
        rt.block_on(account.call::<methods::LoginAccount>(
            &m::Login {
                name: wire("alpha"),
                password: wire("correct-horse-1"),
            },
            RPC_TIMEOUT,
        ))
    };
    rt.block_on(account.call::<methods::RegisterAccount>(
        &m::Register {
            name: wire("alpha"),
            password: wire("correct-horse-1"),
        },
        RPC_TIMEOUT,
    ))
    .unwrap();
    let alpha = login().unwrap().account;
    let character = rt
        .block_on(realm.call::<methods::NewCharacter>(
            &m::CreateCharacter {
                account: alpha,
                name: wire("walker"),
                kind: 1,
            },
            RPC_TIMEOUT,
        ))
        .unwrap()
        .character;
    let characters = || {
        rt.block_on(
            realm.call::<methods::ListAccountCharacters>(&m::ListCharacters { account: alpha }, RPC_TIMEOUT),
        )
    };
    let relay = |link: &CellLink, from: u64, to: u64| {
        let mut b = Vec::new();
        encode_into(&PartyOp::Invite { from, to }, &mut b);
        link.relay(1, PARTY_OP, &b);
    };
    link.presence(1, &[1001, 1002, 1003]);
    relay(&link, 1001, 1002);
    for _ in 0..30 {
        step(&link);
        std::thread::sleep(Duration::from_millis(33));
    }
    let limit = Duration::from_millis(LEASE_TTL_MS + LEASE_TTL_MS / 3 + 1000);

    // Each role's active instance is killed in turn.
    for role in FAILOVER {
        let active = active_of(&cluster, role);
        let taking_over = if active == service(role) {
            standby(role)
        } else {
            service(role)
        };
        let killed_at = Instant::now();
        cluster.kill(&active);
        let took = wait_for(
            &format!("{taking_over} taking over"),
            Duration::from_secs(20),
            || {
                step(&link);
                cluster.metrics(&taking_over).get("lease_active").copied() == Some(1)
            },
        );
        let answered = match role {
            Role::Account => until_answered(Duration::from_secs(20), login).1,
            Role::Realm => {
                let (list, t) = until_answered(Duration::from_secs(20), characters);
                assert_eq!(list.ids.iter().copied().collect::<Vec<_>>(), vec![character]);
                t
            }
            _ => {
                let before = link.stats.relayed.load(std::sync::atomic::Ordering::Relaxed);
                relay(&link, 1001, 1003);
                wait_for(
                    "a relay through the new social instance",
                    Duration::from_secs(20),
                    || {
                        step(&link);
                        std::thread::sleep(Duration::from_millis(20));
                        link.stats.relayed.load(std::sync::atomic::Ordering::Relaxed) > before
                    },
                )
            }
        };
        let total = killed_at.elapsed();
        println!(
            "budget: {} failover: {taking_over} active {} ms after the active instance was killed, first answer {} ms later (limit {} ms: one lease lifetime, a third more for the standby's ask, 1000 ms for its load)",
            mantis_deploy::matrix::name(role),
            took.as_millis(),
            answered.as_millis(),
            limit.as_millis()
        );
        assert!(took <= limit, "{role:?} takeover took {took:?}, limit {limit:?}");
        assert!(
            total <= limit * 2,
            "{role:?}: {total:?} from the kill to an answer"
        );
        // The killed instance comes back, as a standby.
        cluster.start(&active);
        cluster.wait_ready(&active, Duration::from_secs(30));
        wait_for(
            "the restarted instance a standby",
            Duration::from_secs(10),
            || cluster.ready(&active).is_ok_and(|(_, b)| b.contains("(standby)")),
        );
    }
    // Accounts and passwords, characters: kept through every takeover.
    assert_eq!(until_answered(Duration::from_secs(10), login).0.account, alpha);
    let (list, _) = until_answered(Duration::from_secs(10), characters);
    assert_eq!(list.ids.iter().copied().collect::<Vec<_>>(), vec![character]);
    // The cell host's outcomes all became durable.
    wait_for("the link flushed", Duration::from_secs(20), || {
        step(&link);
        std::thread::sleep(Duration::from_millis(20));
        link.pending() == 0
    });
    assert_eq!(
        link.stats
            .projected_duplicates
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    node.finish(link).unwrap();
    for role in FAILOVER {
        assert_eq!(
            cluster.drain(&standby(role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
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
