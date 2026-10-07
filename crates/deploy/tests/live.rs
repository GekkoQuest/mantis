//! A running process-per-role cluster changed without restarting it:
//!
//! - **A moved address.** The registry's serial rises with the writer at a
//!   new port; the writer is restarted there (its own listener moves only
//!   with a restart), and every other node and cell host follows the new
//!   address from the registry, without a restart, with no outcome lost.
//! - **Revocation.** A CA removed from the registry (serial bump) is no
//!   longer trusted: a client holding a leaf from it is refused within one
//!   registry refresh; leaves from the CAs still listed keep working.
//! - **Renewal.** A node's certificate files replaced in place are taken
//!   up within its file look, with no restart and callers reconnecting; an
//!   expired replacement is refused and the running identity stays.

#![expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_precision_loss)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::wire::encode_into;
use mantis_deploy::cell::CellNode;
use mantis_deploy::config::NodeConfig;
use mantis_deploy::keys;
use mantis_deploy::node::Node;
use mantis_deploy::pki;
use mantis_services::cluster::{CellLink, CellOutcome};
use mantis_services::generated::services as m;
use mantis_services::host::rpc::{RpcClient, RpcError};
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use mantis_services::tls::TlsIdentity;
use support::{CellSpec, Cluster, TestStore, service, wait_for};

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

fn credit(cell: u64, tick: u64) -> CellOutcome {
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character: cell * 1000 + tick % 8,
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
        // Opaque to the writer: no module is involved.
        kind: 0xfff0,
        session: 0,
        ok: true,
        payload,
        len: bytes.len(),
    }
}

/// A cell host in this process, through the cell-host library API.
struct Host {
    node: CellNode,
    link: Option<CellLink>,
    cells: Vec<u64>,
    log: BTreeMap<u64, Vec<CellOutcome>>,
    tick: u64,
}

impl Host {
    fn start(cluster: &Cluster, name: &str) -> Self {
        let node = Node::start(NodeConfig::load(&cluster.config(name)).unwrap(), false).unwrap();
        node.wait_for_dependencies().unwrap();
        let node = CellNode::new(node).unwrap();
        let cells = node.cells().to_vec();
        let world: Vec<(u64, (f32, f32))> = cells
            .iter()
            .map(|c| (*c, ((*c as f32 - 2.0) * 100.0, (*c as f32 - 1.0) * 100.0)))
            .collect();
        let link = node.link(&world, &[]).unwrap();
        node.ready();
        Self {
            node,
            link: Some(link),
            cells,
            log: BTreeMap::new(),
            tick: 0,
        }
    }

    fn link(&self) -> &CellLink {
        self.link.as_ref().unwrap()
    }

    fn step(&mut self) {
        self.tick += 1;
        for cell in &self.cells {
            let o = credit(*cell, self.tick);
            self.log.entry(*cell).or_default().push(o);
            self.link().push(*cell, vec![o]);
        }
        self.node.observe(self.tick, self.link());
    }

    fn durable(&self) -> u64 {
        self.link().stats.durable_batches.load(Ordering::Relaxed)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(link) = self.link.take() {
            let _guard = self.node.handle().enter();
            drop(link);
        }
    }
}

fn run_for(host: &mut Host, ticks: u64) {
    for _ in 0..ticks {
        host.step();
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// The writer's ledger rows of every character `host` credited, read as
/// Ops; and what the host's log says they must be.
/// Ledger rows by character: (tick, change).
type Rows = BTreeMap<u64, Vec<(u64, i64)>>;

fn rows(cluster: &Cluster, host: &Host) -> (Rows, Rows) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let persist = cluster.client(&service(Role::Ops), &service(Role::Persist));
    let mut logged = Rows::new();
    for (cell, outcomes) in &host.log {
        for o in outcomes {
            logged
                .entry(cell * 1000 + o.tick % 8)
                .or_default()
                .push((o.tick, i64::try_from(o.tick).unwrap()));
        }
    }
    let mut durable = BTreeMap::new();
    for character in logged.keys() {
        let got = again(|| {
            rt.block_on(persist.call::<methods::Ledger>(
                &m::LedgerOf {
                    character: m::CharacterId(*character),
                },
                RPC_TIMEOUT,
            ))
        })
        .unwrap();
        let mut v: Vec<(u64, i64)> = got.rows.iter().map(|r| (r.tick, r.delta)).collect();
        v.sort_unstable();
        durable.insert(*character, v);
    }
    for v in logged.values_mut() {
        v.sort_unstable();
    }
    (logged, durable)
}

/// One call, once more if its connection was lost (a client learns of a
/// server's restart or a swapped identity on its next call).
fn again<T>(mut call: impl FnMut() -> Result<T, RpcError>) -> Result<T, RpcError> {
    match call() {
        Err(RpcError::Disconnected) => call(),
        other => other,
    }
}

fn metric(cluster: &Cluster, name: &str, key: &str) -> i64 {
    cluster.metrics(name).get(key).copied().unwrap_or(0)
}

#[test]
fn a_registry_with_a_higher_serial_moves_every_node_to_the_writers_new_address() {
    let mut cluster = Cluster::new(
        "move",
        &TestStore::Memory,
        &[CellSpec {
            name: "cells-a",
            cells: vec![1, 2],
            package: String::new(),
            game_tls: false,
        }],
    );
    start_services(&mut cluster);
    let mut host = Host::start(&cluster, "cells-a");
    run_for(&mut host, 30);
    let persist = service(Role::Persist);
    assert_eq!(metric(&cluster, &service(Role::Social), "registry_serial"), 1);

    // The writer moves: a registry with a higher serial lists it on new
    // ports. Its own listener moves with a restart; nobody else restarts.
    let ports = support::free_ports(2);
    let mut next = cluster.registry.clone();
    next.serial = 2;
    for i in next.instances.iter_mut().filter(|i| i.name == persist) {
        i.rpc = std::net::SocketAddr::from(([127, 0, 0, 1], ports[0])).into();
        i.health = std::net::SocketAddr::from(([127, 0, 0, 1], ports[1])).into();
    }
    let started = Instant::now();
    let moved_at = host.tick;
    cluster.kill(&persist);
    cluster.publish(next);
    cluster.start(&persist);
    cluster.wait_ready(&persist, Duration::from_secs(30));
    assert!(cluster.output(&persist).contains("registry test serial 2"));

    // Every other node applies serial 2 and moves its writer address.
    for role in [Role::Account, Role::Realm, Role::Social, Role::Ops] {
        let name = service(role);
        wait_for(&format!("{name} on serial 2"), Duration::from_secs(10), || {
            metric(&cluster, &name, "registry_serial") == 2 && metric(&cluster, &name, "endpoints_moved") >= 1
        });
        assert!(
            cluster.output(&name).contains("address(es) moved"),
            "{}",
            cluster.output(&name)
        );
    }
    // The cell host follows too: its outcomes are durable at the new address.
    let before = host.durable();
    let mut ticks = 0;
    while host.durable() <= before || host.link().pending() > 0 {
        assert!(ticks < 600, "the cell host never reached the moved writer");
        host.step();
        ticks += 1;
        std::thread::sleep(Duration::from_millis(33));
    }
    println!(
        "MANTIS-METRIC deploy_registry_move_followed_ms={} cell_host_ticks={ticks}",
        started.elapsed().as_millis()
    );
    assert_eq!(metric(&cluster, "cells-a", "registry_serial"), 2);
    // Social, Account, Realm and Ops read and write through the moved writer.
    let ops_social = cluster.client(&service(Role::Ops), &service(Role::Account));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    again(|| {
        rt.block_on(ops_social.call::<methods::Maintenance>(&m::SetMaintenance { on: false }, RPC_TIMEOUT))
    })
    .unwrap();
    // Every outcome from the move on is durable once, at the new address.
    // (The store is in memory, so the restarted writer starts empty: what
    // it held before the move is gone by design; PostgreSQL keeps it.)
    run_for(&mut host, 10);
    wait_for("the link flushed", Duration::from_secs(20), || {
        host.link().pending() == 0
    });
    let (mut logged, durable) = rows(&cluster, &host);
    for v in logged.values_mut() {
        v.retain(|(tick, _)| *tick > moved_at);
    }
    assert_eq!(logged, durable);
    drop(host);
    for role in SERVICES.iter().rev() {
        assert_eq!(
            cluster.drain(&service(*role), Duration::from_secs(30)).code(),
            Some(0)
        );
    }
}

/// A TLS identity for `role`/`instance` from `ca`, trusting `trusted`.
fn identity(ca: &pki::CaFiles, trusted: &[&pki::CaFiles], role: Role, instance: &str) -> Arc<TlsIdentity> {
    let ok = pki::Validity::starting_now(std::time::SystemTime::now(), 7);
    let leaf = pki::issue(ca, support::CLUSTER, role, instance, &[], ok).unwrap();
    let bundle: String = trusted.iter().map(|c| c.cert_pem.clone()).collect();
    Arc::new(
        TlsIdentity::from_pem(
            bundle.as_bytes(),
            leaf.cert_pem.as_bytes(),
            leaf.key_pem.as_bytes(),
        )
        .unwrap(),
    )
}

#[test]
fn a_ca_removed_from_the_registry_is_refused_within_one_refresh() {
    let mut cluster = Cluster::new("revoke", &TestStore::Memory, &[]);
    let keys_dir = cluster.dir.join("keys");
    let first = keys::read_ca(&keys_dir).unwrap();
    let second = pki::new_ca(
        support::CLUSTER,
        pki::Validity::starting_now(std::time::SystemTime::now(), 30),
    )
    .unwrap();
    // Both CAs trusted (as during a rotation's overlap).
    let mut both = cluster.registry.clone();
    both.serial = 2;
    both.cas
        .push(pki::pem_certs(second.cert_pem.as_bytes()).unwrap().remove(0));
    cluster.publish(both);
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
    let client = |id: Arc<TlsIdentity>| {
        RpcClient::with_tls(at, Role::Ops, cluster.key.clone(), Some(id), Role::Persist).unwrap()
    };
    let call = |c: &RpcClient| rt.block_on(c.call::<methods::Ledger>(&ledger, RPC_TIMEOUT));
    let from_first = client(identity(&first, &[&first, &second], Role::Ops, "ops-1"));
    let from_second = client(identity(&second, &[&first, &second], Role::Ops, "ops-1"));
    assert!(call(&from_first).is_ok());
    assert!(call(&from_second).is_ok(), "the second CA is trusted");

    // The second CA is removed, and the serial raised.
    let mut revoked = cluster.registry.clone();
    revoked.serial = 3;
    revoked.cas.truncate(1);
    let at_publish = Instant::now();
    cluster.publish(revoked);
    let refused_after = wait_for("the removed CA refused", Duration::from_secs(5), || {
        // A held connection is closed by the swap; a new one is refused at
        // the handshake.
        call(&from_second).is_err() && call(&from_second).is_err()
    });
    println!(
        "budget: a leaf from a removed CA refused {} ms after the registry was published (limit one refresh, 1000 ms, plus 1000 ms for the node's look)",
        at_publish.elapsed().as_millis()
    );
    assert!(refused_after < Duration::from_secs(2), "{refused_after:?}");
    assert_eq!(metric(&cluster, &persist, "registry_serial"), 3);
    assert!(metric(&cluster, &persist, "tls_ca_changes") >= 1);
    // The remaining CA's leaves keep working (after reconnecting).
    assert!(again(|| call(&from_first)).is_ok());
    assert!(
        cluster.ready(&persist).is_ok_and(|(c, _)| c == 200),
        "persist's own leaf is from the kept CA"
    );
    assert_eq!(cluster.drain(&persist, Duration::from_secs(30)).code(), Some(0));
}

#[test]
fn renewed_certificate_files_are_taken_up_without_a_restart() {
    let mut cluster = Cluster::new("renew", &TestStore::Memory, &[]);
    let persist = service(Role::Persist);
    cluster.start(&persist);
    cluster.wait_ready(&persist, Duration::from_secs(30));
    let keys_dir = cluster.dir.join("keys");
    let ca = keys::read_ca(&keys_dir).unwrap();
    let ops = cluster.client(&service(Role::Ops), &persist);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ledger = m::LedgerOf {
        character: m::CharacterId(1),
    };
    assert!(
        rt.block_on(ops.call::<methods::Ledger>(&ledger, RPC_TIMEOUT))
            .is_ok()
    );
    let crt = keys_dir.join(keys::files::cert(&persist));
    let key = keys_dir.join(keys::files::key(&persist));
    let ip = [support::addr(&cluster.instance(&persist).rpc).ip()];

    // An expired replacement: refused, the running identity stays.
    let today = pki::day_of(std::time::SystemTime::now());
    let old = pki::issue(
        &ca,
        support::CLUSTER,
        Role::Persist,
        &persist,
        &ip,
        pki::Validity {
            from_day: today - 9,
            until_day: today - 1,
        },
    )
    .unwrap();
    std::fs::write(&key, &old.key_pem).unwrap();
    std::fs::write(&crt, &old.cert_pem).unwrap();
    wait_for("the expired certificate refused", Duration::from_secs(10), || {
        metric(&cluster, &persist, "tls_refused") >= 1
    });
    assert!(again(|| rt.block_on(ops.call::<methods::Ledger>(&ledger, RPC_TIMEOUT))).is_ok());

    // A good renewal: taken up in place.
    let fresh = pki::issue(
        &ca,
        support::CLUSTER,
        Role::Persist,
        &persist,
        &ip,
        pki::Validity::starting_now(std::time::SystemTime::now(), 7),
    )
    .unwrap();
    let started = Instant::now();
    std::fs::write(&key, &fresh.key_pem).unwrap();
    std::fs::write(&crt, &fresh.cert_pem).unwrap();
    wait_for("the renewal taken up", Duration::from_secs(10), || {
        metric(&cluster, &persist, "tls_renewals") >= 1
    });
    println!(
        "MANTIS-METRIC deploy_certificate_renewal_taken_up_ms={}",
        started.elapsed().as_millis()
    );
    assert!(cluster.running(&persist), "no restart");
    assert_eq!(
        cluster.output(&persist).matches("---- start").count(),
        1,
        "started once"
    );
    assert!(again(|| rt.block_on(ops.call::<methods::Ledger>(&ledger, RPC_TIMEOUT))).is_ok());
    assert!(cluster.output(&persist).contains("certificate renewed"));
    assert_eq!(cluster.drain(&persist, Duration::from_secs(30)).code(), Some(0));
}
