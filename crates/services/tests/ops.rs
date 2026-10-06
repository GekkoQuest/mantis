//! Ops: audit rows written before dispatch and completed with before,
//! after, and undo; signed live changes and the cell-side feed; the HTTPS
//! dashboard on its own loopback listener; and proof that game-protocol
//! traffic never reaches an Ops handler.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mantis_adapter_contract::native::encode_inbound;
use mantis_adapter_contract::{Hello, Inbound};
use mantis_core::content::ContentHash;
use mantis_core::wire::{BoundedArray, WireString};
use mantis_services::account::AccountService;
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{RpcClient, RpcServer};
use mantis_services::methods;
use mantis_services::ops::dashboard::{Dashboard, DashboardConfig, dev_tls};
use mantis_services::ops::live::{self, LiveFeed, LiveRefused, LiveSigner};
use mantis_services::ops::{Command, OpsError, OpsService};
use mantis_services::persist::PersistService;
use mantis_services::persist::memory::MemoryStore;
use ring::rand::SystemRandom;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const KEY: &[u8] = b"cluster-key-for-tests";
const TOKEN: &str = "operator-token-0123456789";
const NOW: u64 = 1_700_000_000_000;

struct Cluster {
    account: AccountService,
    _account_server: RpcServer,
    ops: OpsService,
    persist: PersistService,
    account_addr: SocketAddr,
}

async fn cluster(store: MemoryStore) -> Cluster {
    let account = AccountService::new();
    let server = RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), account.router())
        .await
        .unwrap();
    let account_addr = server.addr();
    let audit = PersistService::new(Box::new(store), NOW).unwrap();
    let (signer, _) = LiveSigner::generate(&SystemRandom::new()).unwrap();
    let client = Arc::new(RpcClient::new(account_addr, Role::Ops, KEY.to_vec()));
    let ops = OpsService::new(audit.clone(), client, signer);
    Cluster {
        account,
        _account_server: server,
        ops,
        persist: audit,
        account_addr,
    }
}

async fn register(addr: SocketAddr, name: &str) -> u64 {
    let gateway = RpcClient::new(addr, Role::Gateway, KEY.to_vec());
    let req = m::Register {
        name: WireString::new(name).unwrap(),
        password: WireString::new("correct horse").unwrap(),
    };
    gateway
        .call::<methods::RegisterAccount>(&req, Duration::from_secs(5))
        .await
        .unwrap()
        .account
        .0
}

#[test]
fn live_changes_are_signed_ordered_and_verified() {
    let rng = SystemRandom::new();
    let (signer, pkcs8) = LiveSigner::generate(&rng).unwrap();
    let again = LiveSigner::from_pkcs8(&pkcs8).unwrap();
    assert_eq!(signer.public_key(), again.public_key());
    let a = signer.sign(1, "std.chat.whispers", live::FLAG, 0.0).unwrap();
    let b = signer.sign(2, "std.vendor.markup", live::TUNABLE, 1.5).unwrap();
    assert!(live::verify(&signer.public_key(), &a));
    assert!(signer.sign(3, "x", live::TUNABLE, f32::INFINITY).is_err());

    // Any field changed after signing fails verification.
    let mut forged = b;
    forged.value = 99.0;
    assert!(!live::verify(&signer.public_key(), &forged));
    let (other, _) = LiveSigner::generate(&rng).unwrap();
    assert!(!live::verify(&other.public_key(), &a));

    let batch = |c: &[m::LiveChange]| m::LiveChanges {
        changes: BoundedArray::from_slice(c).unwrap(),
    };
    let mut feed = LiveFeed::new(signer.public_key());
    let mut out = Vec::new();
    feed.accept(&batch(&[a, b]), &mut out).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!((out[1].name.as_str(), out[1].value), ("std.vendor.markup", 1.5));
    assert_eq!(feed.since(), 2);
    // Already applied: skipped, not applied twice.
    out.clear();
    feed.accept(&batch(&[a, b]), &mut out).unwrap();
    assert!(out.is_empty());
    // A gap or a forgery stops the feed; what came before it still applies.
    let c = signer.sign(3, "std.party.invites", live::FLAG, 1.0).unwrap();
    let e = signer.sign(5, "std.party.invites", live::FLAG, 0.0).unwrap();
    let mut bad = signer.sign(4, "std.party.invites", live::FLAG, 0.0).unwrap();
    bad.kind = live::TUNABLE;
    assert_eq!(
        feed.accept(&batch(&[c, e]), &mut out),
        Err(LiveRefused::Gap { expected: 4, got: 5 })
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        feed.accept(&batch(&[bad]), &mut out),
        Err(LiveRefused::BadSignature { seq: 4 })
    );
    assert_eq!(feed.since(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_command_is_audited_before_dispatch_with_before_after_and_undo() {
    let c = cluster(MemoryStore::new()).await;
    let account = register(c.account_addr, "player_one").await;

    let ban = Command::Ban {
        account,
        until_ms: u64::MAX / 2,
        reason: "spam".to_owned(),
    };
    let done = c.ops.execute("alice", &ban).await.unwrap();
    assert_eq!(c.account.banned_until(account), Some(u64::MAX / 2));
    assert!(done.before.contains("banned_until_ms=0"), "{}", done.before);
    assert_eq!(done.undo, format!("ban account={account} until_ms=0"));

    // The undo, run as a command, restores the state before.
    let undo = Command::Ban {
        account,
        until_ms: 0,
        reason: "undo".to_owned(),
    };
    c.ops.execute("alice", &undo).await.unwrap();
    assert_eq!(c.account.banned_until(account), Some(0));

    let m = c
        .ops
        .execute("bob", &Command::Maintenance { on: true })
        .await
        .unwrap();
    assert!(c.account.maintenance());
    assert_eq!(
        (m.before.as_str(), m.undo.as_str()),
        ("maintenance=false", "maintenance on=false")
    );

    // Live changes: the first has no live value before it, and says why
    // there is no undo; the second has one.
    let f1 = c
        .ops
        .execute(
            "bob",
            &Command::Flag {
                name: "std.chat.whispers".to_owned(),
                on: false,
            },
        )
        .await
        .unwrap();
    assert!(f1.undo.starts_with("none: "), "{}", f1.undo);
    let f2 = c
        .ops
        .execute(
            "bob",
            &Command::Flag {
                name: "std.chat.whispers".to_owned(),
                on: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(f2.undo, "flag name=std.chat.whispers value=false");
    c.ops
        .execute(
            "bob",
            &Command::Tunable {
                name: "std.vendor.markup".to_owned(),
                value: 1.25,
            },
        )
        .await
        .unwrap();
    let feed = c.ops.live_since(0);
    assert_eq!(
        feed.changes.iter().map(|c| c.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        feed.changes
            .iter()
            .all(|ch| live::verify(&c.ops.public_key(), ch))
    );
    assert_eq!(c.ops.live_since(2).changes.iter().count(), 1);

    // A malformed command is refused before any audit row.
    let before_rows = c.ops.audit_rows().unwrap().len();
    let bad = Command::Tunable {
        name: "std.vendor.markup".to_owned(),
        value: f32::NAN,
    };
    assert!(matches!(
        c.ops.execute("bob", &bad).await,
        Err(OpsError::Invalid(_))
    ));
    assert_eq!(c.ops.audit_rows().unwrap().len(), before_rows);

    let rows = c.ops.audit_rows().unwrap();
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().all(|r| r.status == "done"));
    assert!(rows.iter().all(|r| r.before.is_some()
        && r.after.is_some()
        && r.undo.as_deref().is_some_and(|u| !u.is_empty())));
    assert_eq!(rows[0].actor, "alice");
    assert_eq!(rows[0].command, "ban");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_audit_row_means_no_dispatch_and_a_failed_dispatch_is_recorded() {
    let mut store = MemoryStore::new();
    store.fail_audit = true;
    let c = cluster(store).await;
    let r = c.ops.execute("alice", &Command::Maintenance { on: true }).await;
    assert!(matches!(r, Err(OpsError::Audit(_))), "{r:?}");
    assert!(!c.account.maintenance(), "the command ran without its audit row");

    // A dispatch that fails (no such cell to inspect) leaves a completed
    // row saying so.
    let c = cluster(MemoryStore::new()).await;
    let r = c.ops.execute("alice", &Command::Inspect { cell: 9 }).await;
    let Err(OpsError::Failed { audit, .. }) = r else {
        panic!("{r:?}");
    };
    let rows = c.ops.audit_rows().unwrap();
    let row = rows.iter().find(|r| r.id == audit).unwrap();
    assert_eq!(row.status, "failed");
    assert!(row.undo.as_deref().unwrap().starts_with("none: nothing changed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cells_poll_live_changes_over_rpc_and_only_cells_may() {
    let c = cluster(MemoryStore::new()).await;
    c.ops
        .execute(
            "alice",
            &Command::Flag {
                name: "std.party".to_owned(),
                on: false,
            },
        )
        .await
        .unwrap();
    let server = RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), c.ops.router())
        .await
        .unwrap();
    let wait = Duration::from_secs(5);
    let cell = RpcClient::new(server.addr(), Role::Cell, KEY.to_vec());
    let got = cell
        .call::<methods::Live>(
            &m::PollLive {
                cell: m::CellNo(1),
                since: 0,
            },
            wait,
        )
        .await
        .unwrap();
    let mut feed = LiveFeed::new(c.ops.public_key());
    let mut out = Vec::new();
    feed.accept(&got, &mut out).unwrap();
    assert_eq!((out[0].name.as_str(), out[0].value), ("std.party", 0.0));
    let gateway = RpcClient::new(server.addr(), Role::Gateway, KEY.to_vec());
    assert!(
        gateway
            .call::<methods::Live>(
                &m::PollLive {
                    cell: m::CellNo(1),
                    since: 0
                },
                wait
            )
            .await
            .is_err()
    );
}

// ---- the dashboard ----------------------------------------------------------

fn config() -> DashboardConfig {
    let mut cfg = DashboardConfig::loopback();
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.game_ports = vec![7400, 7401];
    cfg.operators = BTreeMap::from([(TOKEN.to_owned(), "alice".to_owned())]);
    cfg
}

async fn tls(addr: SocketAddr, cert: &[u8]) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert.to_vec())).unwrap();
    let config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr).await.unwrap();
    connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap()
}

/// One HTTPS request; returns the status code and body.
async fn https(
    addr: SocketAddr,
    cert: &[u8],
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = tls(addr, cert).await;
    let auth = token
        .map(|t| format!("authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n{auth}content-length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_owned());
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dashboard_serves_https_on_loopback_with_operator_tokens() {
    let c = cluster(MemoryStore::new()).await;
    let (server_tls, cert) = dev_tls().unwrap();
    let dash = Dashboard::start(&config(), c.ops.clone(), server_tls)
        .await
        .unwrap();
    let addr = dash.addr();
    assert!(addr.ip().is_loopback());

    assert_eq!(https(addr, &cert, "GET", "/health", None, "").await.0, 200);
    let (status, _) = https(addr, &cert, "POST", "/ops/maintenance", None, "on=true").await;
    assert_eq!(status, 401);
    let (status, _) = https(
        addr,
        &cert,
        "POST",
        "/ops/maintenance",
        Some("wrong-token-0123456789"),
        "on=true",
    )
    .await;
    assert_eq!(status, 401);
    assert!(!c.account.maintenance());

    let (status, body) = https(addr, &cert, "POST", "/ops/maintenance", Some(TOKEN), "on=true").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"undo\":\"maintenance on=false\""), "{body}");
    assert!(c.account.maintenance());
    let (status, body) = https(
        addr,
        &cert,
        "POST",
        "/ops/flag",
        Some(TOKEN),
        "name=std.chat&on=off",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = https(addr, &cert, "POST", "/ops/launch", Some(TOKEN), "").await;
    assert_eq!(status, 400, "{body}");

    let (status, body) = https(addr, &cert, "GET", "/audit", Some(TOKEN), "").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("\"actor\":\"alice\"") && body.contains("\"command\":\"flag\""),
        "{body}"
    );
    let (status, body) = https(addr, &cert, "GET", "/live", Some(TOKEN), "").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"name\":\"std.chat\""), "{body}");
}

#[test]
fn the_dashboard_refuses_remote_listeners_and_game_ports() {
    let mut cfg = config();
    cfg.listen = "0.0.0.0:7480".parse().unwrap();
    assert!(cfg.check().is_err());
    cfg.allow_remote = true;
    assert!(cfg.check().is_ok());
    cfg.listen = "127.0.0.1:7400".parse().unwrap();
    assert!(cfg.check().unwrap_err().contains("game port"));
    cfg.listen = "127.0.0.1:7480".parse().unwrap();
    cfg.operators.insert("short".to_owned(), "eve".to_owned());
    assert!(cfg.check().is_err());
}

fn game_frames() -> Vec<Vec<u8>> {
    let hello = Inbound::Hello(Hello {
        protocol: 1,
        capabilities: 0,
        content: ContentHash::of(b"package"),
        modules: BoundedArray::default(),
        token: BoundedArray::from_slice(TOKEN.as_bytes()).unwrap(),
    });
    let mut a = Vec::new();
    encode_inbound(&hello, &mut a);
    // The same bytes behind a length prefix, as stream transports frame them.
    let mut b = u32::try_from(a.len()).unwrap().to_le_bytes().to_vec();
    b.extend_from_slice(&a);
    vec![a, b]
}

/// The architecture guarantee, end to end: the dashboard is HTTPS on its
/// own listener, so game-protocol bytes, raw or inside TLS, never reach an
/// Ops handler, and the Ops RPC listener refuses them at the hello.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_game_protocol_message_reaches_an_ops_handler() {
    let c = cluster(MemoryStore::new()).await;
    let (server_tls, cert) = dev_tls().unwrap();
    let dash = Dashboard::start(&config(), c.ops.clone(), server_tls)
        .await
        .unwrap();
    let rows_before = c.ops.audit_rows().unwrap().len();

    for frame in game_frames() {
        // Raw, as a game client would send to its game port.
        let mut tcp = TcpStream::connect(dash.addr()).await.unwrap();
        tcp.write_all(&frame).await.unwrap();
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut sink)).await;
        // Inside TLS: still not HTTP.
        let mut s = tls(dash.addr(), &cert).await;
        s.write_all(&frame).await.unwrap();
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut sink)).await;
        let reply = String::from_utf8_lossy(&sink);
        assert!(reply.is_empty() || reply.starts_with("HTTP/1.1 400"), "{reply}");
    }
    let mut waited = 0;
    while dash.refused_handshakes() < 2 && waited < 100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    assert_eq!(dash.handled(), 0, "a game frame reached an Ops handler");
    assert!(dash.refused_handshakes() >= 2);
    assert!(!c.account.maintenance());
    assert_eq!(c.ops.audit_rows().unwrap().len(), rows_before);

    // The Ops RPC listener (cells poll live changes there) wants a hello
    // proving the cluster key before any frame is dispatched.
    let rpc = RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), c.ops.router())
        .await
        .unwrap();
    for frame in game_frames() {
        let mut tcp = TcpStream::connect(rpc.addr()).await.unwrap();
        tcp.write_all(&frame).await.unwrap();
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut sink)).await;
        assert!(sink.is_empty(), "the RPC listener answered a game frame");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dashboard_traces_a_ledger_read_only_and_audited() {
    use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
    let c = cluster(MemoryStore::new()).await;
    // A sale recorded by the writer.
    let mut l = Ledger::default();
    l.push(LedgerRow {
        character: 7,
        item: GOLD,
        delta: 25,
    })
    .unwrap();
    let mut bytes = Vec::new();
    mantis_core::wire::encode_into(&l, &mut bytes);
    let row = m::OutcomeRow {
        tick: 3,
        at_ms: NOW,
        kind: 1066,
        session: 1,
        ok: true,
        payload: BoundedArray::from_slice(&bytes).unwrap(),
    };
    c.persist
        .push(&m::PushOutcomes {
            cell: m::CellNo(1),
            seq: 1,
            rows: BoundedArray::from_slice(&[row]).unwrap(),
        })
        .unwrap();
    let audit = c.ops.audit_rows().unwrap().len();
    let (server_tls, cert) = dev_tls().unwrap();
    let dash = Dashboard::start(&config(), c.ops.clone(), server_tls)
        .await
        .unwrap();
    let (status, body) = https(dash.addr(), &cert, "GET", "/ledger/7", Some(TOKEN), "").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"delta\":25") && body.contains("\"cell\":1"),
        "{body}"
    );
    let rows = c.ops.audit_rows().unwrap();
    assert_eq!(rows.len(), audit + 1);
    assert_eq!(rows.last().map(|r| r.command.as_str()), Some("ledger"));
    assert_eq!(
        rows.last().and_then(|r| r.undo.as_deref()),
        Some("none: read-only")
    );
    let (status, _) = https(
        dash.addr(),
        &cert,
        "POST",
        "/ops/kick",
        Some(TOKEN),
        "character=5",
    )
    .await;
    assert_eq!(
        status, 502,
        "no host has the character: the failed kick is audited"
    );
}
