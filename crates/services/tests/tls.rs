//! Mutual TLS for internal RPC: the identity format, plaintext refused,
//! certificates checked at the handshake (CA, validity, cluster, the
//! server's role), the hello bound to the certificate's role, the caller
//! matrix applied to that role, and the cluster-key HMAC kept as a second
//! factor.

#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mantis_core::wire::WireString;
use mantis_services::account::AccountService;
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{RpcClient, RpcError, RpcServer};
use mantis_services::methods;
use mantis_services::tls::dev::{ALWAYS, DevCa, Validity};
use mantis_services::tls::{IdentityError, PeerIdentity, TlsIdentity, identity_of, parse_uri};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEY: &[u8] = b"cluster-key-for-tls-tests";
const WAIT: Duration = Duration::from_secs(5);
const CLUSTER: &str = "tls-test";

fn local() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

fn uri(role: Role, instance: &str) -> String {
    format!("mantis://{CLUSTER}/{}/{instance}", role.name())
}

fn register() -> m::Register {
    m::Register {
        name: WireString::new("someone").unwrap(),
        password: WireString::new("long enough").unwrap(),
    }
}

/// An account server over mutual TLS with `ca`'s account identity.
async fn account_server(ca: &DevCa) -> RpcServer {
    RpcServer::bind_tls(
        "127.0.0.1:0".parse().unwrap(),
        KEY.to_vec(),
        AccountService::new().router(),
        Some(ca.identity(Role::Account, "account-1", &[local()]).unwrap()),
    )
    .await
    .unwrap()
}

async fn register_with(client: &RpcClient) -> Result<m::Registered, RpcError> {
    client.call::<methods::RegisterAccount>(&register(), WAIT).await
}

/// Waits until `server` has refused `n` connections.
async fn refused(server: &RpcServer, n: u64) {
    for _ in 0..200 {
        if server.refused() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(server.refused(), n, "connections refused");
}

/// A gateway client with exactly the leaf `leaf`.
fn gateway(addr: SocketAddr, leaf: Arc<TlsIdentity>) -> RpcClient {
    RpcClient::with_tls(addr, Role::Gateway, KEY.to_vec(), Some(leaf), Role::Account).unwrap()
}

// ---- the identity format ---------------------------------------------------

#[test]
fn identities_parse_exactly_and_refuse_everything_else() {
    assert_eq!(
        parse_uri("mantis://compose-local/social/social-1"),
        Ok(PeerIdentity {
            cluster: "compose-local".to_owned(),
            role: Role::Social,
            instance: "social-1".to_owned(),
        })
    );
    for role in Role::ALL {
        let id = parse_uri(&format!("mantis://c/{}/i", role.name())).unwrap();
        assert_eq!(id.role, role);
        assert_eq!(id.uri(), format!("mantis://c/{}/i", role.name()));
    }
    let long = "a".repeat(33);
    for bad in [
        "mantis://c/cell",
        "mantis://c/cell/i/extra",
        "mantis://c/cell/i/",
        "mantis:///cell/i",
        "mantis://c/cell/",
        "mantis://C/cell/i",
        "mantis://c/Cell/i",
        "mantis://c/cell-host/i",
        "mantis://c/wizard/i",
        "mantis://c/cell/i?x=1",
        "mantis://c/cell/i#f",
        "mantis://c_1/cell/i",
        "mantis://c/cell/i.1",
        "spiffe://c/cell/i",
        &format!("mantis://{long}/cell/i"),
        &format!("mantis://c/cell/{long}"),
    ] {
        assert!(
            matches!(parse_uri(bad), Err(IdentityError::Malformed(_))),
            "{bad} refused"
        );
    }
}

#[test]
fn a_certificate_names_exactly_one_identity() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let leaf = |uris: &[&str]| ca.leaf(uris, &[local()], ALWAYS).unwrap().chain[0].clone();
    let one = uri(Role::Cell, "cell-host-1");
    assert_eq!(identity_of(&leaf(&[&one])).map(|i| i.uri()), Ok(one.clone()));
    // Other schemes are ignored.
    assert_eq!(
        identity_of(&leaf(&["spiffe://elsewhere/workload", &one])).map(|i| i.role),
        Ok(Role::Cell)
    );
    assert_eq!(identity_of(&leaf(&[])), Err(IdentityError::NoIdentity));
    assert_eq!(
        identity_of(&leaf(&["spiffe://elsewhere/workload"])),
        Err(IdentityError::NoIdentity)
    );
    let other = uri(Role::Ops, "ops-1");
    assert_eq!(
        identity_of(&leaf(&[&one, &other])),
        Err(IdentityError::SeveralIdentities)
    );
    // A malformed identity is refused, not skipped; the scheme's case does
    // not hide one.
    assert!(matches!(
        identity_of(&leaf(&["mantis://tls-test/wizard/w-1"])),
        Err(IdentityError::Malformed(_))
    ));
    assert!(matches!(
        identity_of(&leaf(&["MANTIS://tls-test/cell/c-1"])),
        Err(IdentityError::Malformed(_))
    ));
    assert!(matches!(
        identity_of(b"not a certificate"),
        Err(IdentityError::Certificate(_))
    ));
}

/// PEM, as the deployment's files hold it.
fn pem(label: &str, der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::new();
    for chunk in der.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                b64.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                b64.push('=');
            }
        }
    }
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|l| std::str::from_utf8(l).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

#[test]
fn identities_load_from_pem_files_with_several_cas() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let next = DevCa::new(CLUSTER).unwrap();
    let id = ca.identity(Role::Persist, "persist-1", &[local()]).unwrap();
    let cas = format!(
        "{}{}",
        pem("CERTIFICATE", ca.der()),
        pem("CERTIFICATE", next.der())
    );
    let loaded = TlsIdentity::from_pem(
        cas.as_bytes(),
        pem("CERTIFICATE", &id.chain[0]).as_bytes(),
        pem("PRIVATE KEY", &id.key).as_bytes(),
    )
    .unwrap();
    assert_eq!(loaded.cas, vec![ca.der().to_vec(), next.der().to_vec()]);
    assert_eq!(loaded.chain, id.chain);
    assert_eq!(loaded.key, id.key);
    assert_eq!(loaded.identity().map(|i| i.role), Ok(Role::Persist));
    assert!(loaded.server_config().is_ok() && loaded.client_config().is_ok());
    assert!(matches!(
        TlsIdentity::from_pem(b"", b"", b""),
        Err(IdentityError::Pem(_))
    ));
    // A client refuses an identity issued to another role than it calls as.
    assert_eq!(
        RpcClient::with_tls(
            "127.0.0.1:1".parse().unwrap(),
            Role::Cell,
            KEY.to_vec(),
            Some(Arc::new(loaded)),
            Role::Realm
        )
        .err(),
        Some(IdentityError::WrongRole {
            expected: Role::Cell,
            found: Role::Persist
        })
    );
}

// ---- the handshake and the hello -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_tls_calls_work_and_plaintext_is_refused() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let server = account_server(&ca).await;
    let ok = gateway(
        server.addr(),
        ca.identity(Role::Gateway, "gateway-1", &[local()]).unwrap(),
    );
    assert!(register_with(&ok).await.is_ok());
    assert_eq!(server.refused(), 0);

    // A plaintext client, right key and all, gets no answer: the server
    // closes the socket after refusing, so the client sees the loss at once
    // (Disconnected); the property owned here is "no answer", so a platform
    // whose close is slower than the call's timeout may see Timeout instead,
    // and the refusal is counted either way.
    let plain = RpcClient::new(server.addr(), Role::Gateway, KEY.to_vec());
    let answer = plain
        .call::<methods::RegisterAccount>(&register(), Duration::from_secs(2))
        .await;
    assert!(
        matches!(answer, Err(RpcError::Disconnected | RpcError::Timeout)),
        "{answer:?}"
    );
    refused(&server, 1).await;

    // Raw bytes that are not a TLS handshake are dropped too.
    let mut raw = tokio::net::TcpStream::connect(server.addr()).await.unwrap();
    raw.write_all(b"\x10\x00\x00\x00hello there, server")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(WAIT, raw.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(
        n == 0 || buf.first() == Some(&0x15),
        "closed, or a TLS alert: no RPC answer"
    );
    refused(&server, 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_for_one_role_cannot_call_as_another() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let server = account_server(&ca).await;
    // The caller matrix applies to the certificate's role: a cell host's
    // certificate may not register accounts.
    let cell = RpcClient::with_tls(
        server.addr(),
        Role::Cell,
        KEY.to_vec(),
        Some(ca.identity(Role::Cell, "cell-host-1", &[local()]).unwrap()),
        Role::Account,
    )
    .unwrap();
    assert_eq!(register_with(&cell).await, Err(RpcError::Forbidden));

    // A cell host's certificate with a hello claiming the gateway role (the
    // cluster key and all) is cut off at the hello.
    let cell_id = ca.identity(Role::Cell, "cell-host-1", &[local()]).unwrap();
    let connector = tokio_rustls::TlsConnector::from(cell_id.client_config().unwrap());
    let tcp = tokio::net::TcpStream::connect(server.addr()).await.unwrap();
    let name = rustls::pki_types::ServerName::IpAddress(local().into());
    let mut tls = connector.connect(name, tcp).await.unwrap();
    let mut hello = vec![Role::Gateway as u8];
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, KEY);
    hello.extend_from_slice(ring::hmac::sign(&k, &[Role::Gateway as u8]).as_ref());
    let mut frame = u32::try_from(hello.len() + 11).unwrap().to_le_bytes().to_vec();
    frame.push(0); // hello
    frame.extend_from_slice(&0u64.to_le_bytes());
    frame.extend_from_slice(&0u16.to_le_bytes());
    frame.extend_from_slice(&hello);
    tls.write_all(&frame).await.unwrap();
    tls.flush().await.unwrap();
    refused(&server, 1).await;
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(WAIT, tls.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "the connection is closed without an answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_expired_and_not_yet_valid_certificates_are_refused() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let server = account_server(&ca).await;
    let gw = uri(Role::Gateway, "gateway-1");
    let leaf = |v: Validity| ca.leaf(&[&gw], &[local()], v).unwrap();
    let cases: [(&str, Arc<TlsIdentity>); 7] = [
        ("expired", leaf(((2000, 1, 1), (2001, 1, 1)))),
        ("not yet valid", leaf(((2100, 1, 1), (2101, 1, 1)))),
        (
            "from a foreign CA",
            DevCa::new(CLUSTER)
                .unwrap()
                .identity(Role::Gateway, "gateway-1", &[local()])
                .unwrap(),
        ),
        (
            "of another cluster",
            ca.leaf(&["mantis://elsewhere/gateway/gateway-1"], &[local()], ALWAYS)
                .unwrap(),
        ),
        (
            "with two identities",
            ca.leaf(&[&gw, &uri(Role::Ops, "ops-1")], &[local()], ALWAYS)
                .unwrap(),
        ),
        ("without an identity", ca.leaf(&[], &[local()], ALWAYS).unwrap()),
        (
            "with a malformed identity",
            ca.leaf(&["mantis://tls-test/wizard/w-1"], &[local()], ALWAYS)
                .unwrap(),
        ),
    ];
    let mut n = 0;
    for (what, id) in cases {
        // Built raw: `with_tls` would refuse the identity-less ones itself.
        let connector = tokio_rustls::TlsConnector::from(id.client_config().unwrap());
        let tcp = tokio::net::TcpStream::connect(server.addr()).await.unwrap();
        let name = rustls::pki_types::ServerName::IpAddress(local().into());
        if let Ok(mut tls) = connector.connect(name, tcp).await {
            let mut buf = [0u8; 16];
            let read = tokio::time::timeout(WAIT, tls.read(&mut buf)).await.unwrap();
            assert!(
                matches!(read, Ok(0) | Err(_)),
                "a client certificate {what} is refused"
            );
        }
        n += 1;
        refused(&server, n).await;
    }
    // The ordinary client sees a refusal as a lost connection.
    let expired = gateway(server.addr(), leaf(((2000, 1, 1), (2001, 1, 1))));
    assert_eq!(register_with(&expired).await, Err(RpcError::Disconnected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_check_the_server_chain_address_cluster_and_role() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let id = ca.identity(Role::Gateway, "gateway-1", &[local()]).unwrap();
    let serve = |server_id: Arc<TlsIdentity>| async move {
        RpcServer::bind_tls(
            "127.0.0.1:0".parse().unwrap(),
            KEY.to_vec(),
            AccountService::new().router(),
            Some(server_id),
        )
        .await
        .unwrap()
    };
    let account = |ips: &[IpAddr], v: Validity| ca.leaf(&[&uri(Role::Account, "account-1")], ips, v).unwrap();
    let cases: [(&str, Arc<TlsIdentity>); 5] = [
        ("expired", account(&[local()], ((2000, 1, 1), (2001, 1, 1)))),
        (
            "for another address",
            account(&[IpAddr::from([10, 0, 0, 9])], ALWAYS),
        ),
        (
            "from a foreign CA",
            DevCa::new(CLUSTER)
                .unwrap()
                .identity(Role::Account, "account-1", &[local()])
                .unwrap(),
        ),
        (
            "of another cluster",
            ca.leaf(&["mantis://elsewhere/account/account-1"], &[local()], ALWAYS)
                .unwrap(),
        ),
        (
            "of another role",
            ca.identity(Role::Persist, "persist-1", &[local()]).unwrap(),
        ),
    ];
    for (what, server_id) in cases {
        let server = serve(server_id).await;
        let client = gateway(server.addr(), Arc::clone(&id));
        assert_eq!(
            register_with(&client).await,
            Err(RpcError::Disconnected),
            "a server certificate {what} is refused"
        );
    }
    // The right one works.
    let server = serve(account(&[local()], ALWAYS)).await;
    assert!(register_with(&gateway(server.addr(), id)).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cluster_key_stays_a_second_factor() {
    let ca = DevCa::new(CLUSTER).unwrap();
    let server = account_server(&ca).await;
    let wrong_key = RpcClient::with_tls(
        server.addr(),
        Role::Gateway,
        b"another key".to_vec(),
        Some(ca.identity(Role::Gateway, "gateway-1", &[local()]).unwrap()),
        Role::Account,
    )
    .unwrap();
    assert_eq!(register_with(&wrong_key).await, Err(RpcError::Disconnected));
    refused(&server, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_stalled_mid_handshake_costs_one_call_its_timeout() {
    let ca = DevCa::new(CLUSTER).unwrap();
    // A peer that accepts and never answers: a role restarting, its socket
    // open, its TLS not yet up.
    let stall = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = stall.local_addr().unwrap();
    let held = std::thread::spawn(move || stall.accept().map(|(conn, _)| conn));
    let client = gateway(addr, ca.identity(Role::Gateway, "gateway-1", &[local()]).unwrap());
    let start = std::time::Instant::now();
    assert_eq!(
        client
            .call::<methods::RegisterAccount>(&register(), Duration::from_millis(300))
            .await,
        Err(RpcError::Disconnected)
    );
    assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    // The role comes up on the same address: the next call handshakes and
    // is answered.
    drop(held.join().unwrap());
    let mut server = None;
    for _ in 0..100 {
        if let Ok(s) = RpcServer::bind_tls(
            addr,
            KEY.to_vec(),
            AccountService::new().router(),
            Some(ca.identity(Role::Account, "account-1", &[local()]).unwrap()),
        )
        .await
        {
            server = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server.is_some(), "the address is free again");
    assert!(register_with(&client).await.is_ok());
}
