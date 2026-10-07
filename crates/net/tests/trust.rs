//! Who a client trusts, and certificate rotation on a running listener:
//! a pinned certificate or a CA bundle with a server name, refusals named by
//! cause (unknown issuer, wrong name, expired, not yet valid), and a listener
//! whose chain is replaced mid-session (by call or by its files) keeping its
//! open connections while new ones get the new chain.

#![expect(clippy::unwrap_used, clippy::panic)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mantis_adapter_contract::{Channel, ConnectionId, Transport, TransportEvent};
use mantis_net::quic::{
    CertWatcher, DevCa, QuicClient, QuicServer, SERVER, ServerCertificate, ServerTrust, TrustFailure,
};
use mantis_net::{NetError, NetRuntime};

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn roots(ca: &DevCa, name: &str) -> ServerTrust {
    ServerTrust::Roots {
        roots: vec![ca.cert_der().to_vec()],
        server_name: name.to_owned(),
    }
}

/// Polls `t` until `count` frames arrived, collecting connections seen; a
/// drop of `session` fails the test (short-lived probe connections come and
/// go).
fn frames(
    t: &mut dyn Transport,
    count: usize,
    session: Option<ConnectionId>,
) -> (Vec<Vec<u8>>, Vec<ConnectionId>) {
    let (mut got, mut conns) = (Vec::new(), Vec::new());
    let start = Instant::now();
    while got.len() < count {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out with {got:?}"
        );
        t.poll(&mut |e| match e {
            TransportEvent::Frame { bytes, .. } => got.push(bytes.to_vec()),
            TransportEvent::Connected(c) => conns.push(c),
            TransportEvent::Disconnected { conn, reason } => {
                assert!(Some(conn) != session, "the open session dropped: {reason:?}");
            }
        });
        std::thread::sleep(Duration::from_millis(2));
    }
    (got, conns)
}

fn untrusted(r: Result<QuicClient, NetError>) -> TrustFailure {
    match r {
        Err(NetError::Untrusted(why)) => why,
        Err(e) => panic!("not a trust refusal: {e}"),
        Ok(_) => panic!("trusted"),
    }
}

#[test]
fn clients_trust_a_ca_for_a_name_and_say_why_they_refuse() {
    let runtime = NetRuntime::new(2).unwrap();
    let ca = DevCa::new().unwrap();
    let leaf = ca.issue(&["game.test", "127.0.0.1"]).unwrap();
    let server = QuicServer::bind(&runtime, loopback(), &leaf).unwrap();
    let addr = server.local_addr();
    // Trusted: the CA, the name the leaf carries.
    assert!(QuicClient::connect_trusted(&runtime, addr, &roots(&ca, "game.test")).is_ok());
    // From a PEM bundle.
    let bundle = ServerTrust::from_pem_bundle(ca.cert_pem().as_bytes(), "game.test").unwrap();
    assert!(QuicClient::connect_trusted(&runtime, addr, &bundle).is_ok());
    // A name the leaf does not carry.
    assert_eq!(
        untrusted(QuicClient::connect_trusted(
            &runtime,
            addr,
            &roots(&ca, "other.test")
        )),
        TrustFailure::WrongName {
            expected: "other.test".to_owned()
        }
    );
    // Another CA.
    let stranger = DevCa::new().unwrap();
    assert_eq!(
        untrusted(QuicClient::connect_trusted(
            &runtime,
            addr,
            &roots(&stranger, "game.test")
        )),
        TrustFailure::UnknownIssuer
    );
    // Pinning a certificate the server does not present.
    assert_eq!(
        untrusted(QuicClient::connect(&runtime, addr, stranger.cert_der())),
        TrustFailure::UnknownIssuer
    );
    // Expired and not-yet-valid leaves.
    for (validity, why) in [
        (((2000, 1, 1), (2001, 1, 1)), TrustFailure::Expired),
        (((2100, 1, 1), (2101, 1, 1)), TrustFailure::NotYetValid),
    ] {
        let bad = ca.issue_with_validity(&["game.test"], validity).unwrap();
        let server = QuicServer::bind(&runtime, loopback(), &bad).unwrap();
        assert_eq!(
            untrusted(QuicClient::connect_trusted(
                &runtime,
                server.local_addr(),
                &roots(&ca, "game.test")
            )),
            why
        );
    }
    assert!(ServerTrust::from_pem_bundle(b"", "game.test").is_err());
}

#[test]
fn a_rotated_chain_keeps_open_sessions_and_reaches_new_connections() {
    let runtime = NetRuntime::new(2).unwrap();
    let old = ServerCertificate::localhost().unwrap();
    let mut server = QuicServer::bind(&runtime, loopback(), &old).unwrap();
    let addr = server.local_addr();
    let reloader = server.reloader();
    let mut before = QuicClient::connect(&runtime, addr, &old.cert_der).unwrap();
    before.send(SERVER, Channel::Reliable, b"before").unwrap();
    let (_, conns) = frames(&mut server, 1, None);

    // A key that does not match its certificate is refused; the old chain
    // stays.
    let other = ServerCertificate::localhost().unwrap();
    let mismatched =
        ServerCertificate::from_der(vec![other.cert_der.clone()], old.key_pkcs8().to_vec()).unwrap();
    assert!(reloader.reload(&mismatched).is_err());
    assert!(QuicClient::connect(&runtime, addr, &old.cert_der).is_ok());

    // Rotate.
    let new = ServerCertificate::localhost().unwrap();
    reloader.reload(&new).unwrap();
    // The open session carries on, both ways.
    before.send(SERVER, Channel::Reliable, b"after").unwrap();
    let (got, _) = frames(&mut server, 1, conns.first().copied());
    assert_eq!(got, vec![b"after".to_vec()]);
    server.send(conns[0], Channel::Reliable, b"still here").unwrap();
    let (got, _) = frames(&mut before, 1, Some(SERVER));
    assert_eq!(got, vec![b"still here".to_vec()]);
    // New connections see the new certificate.
    assert!(QuicClient::connect(&runtime, addr, &new.cert_der).is_ok());
    assert_eq!(
        untrusted(QuicClient::connect(&runtime, addr, &old.cert_der)),
        TrustFailure::UnknownIssuer
    );
}

#[test]
fn a_watcher_reloads_from_changed_files_and_keeps_the_old_chain_on_a_bad_pair() {
    let runtime = NetRuntime::new(2).unwrap();
    let dir = std::env::temp_dir().join(format!("mantis-cert-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (chain, key) = (dir.join("game.crt"), dir.join("game.key"));
    let write = |cert: &ServerCertificate| {
        let (c, k) = cert.to_pem();
        std::fs::write(&key, k).unwrap();
        std::fs::write(&chain, c).unwrap();
    };
    let first = ServerCertificate::localhost().unwrap();
    write(&first);
    let loaded =
        ServerCertificate::from_pem(&std::fs::read(&chain).unwrap(), &std::fs::read(&key).unwrap()).unwrap();
    assert_eq!(loaded.cert_der, first.cert_der);
    let server = QuicServer::bind(&runtime, loopback(), &loaded).unwrap();
    let addr = server.local_addr();
    let mut watch = CertWatcher::new(chain.clone(), key.clone(), server.reloader(), Duration::ZERO);
    assert!(watch.poll().is_none(), "unchanged files: nothing to do");

    // New files: reloaded.
    let second = ServerCertificate::localhost().unwrap();
    write(&second);
    assert!(matches!(watch.poll(), Some(Ok(()))));
    assert_eq!(watch.reloads, 1);
    assert!(QuicClient::connect(&runtime, addr, &second.cert_der).is_ok());

    // A pair caught mid-rotation (a new chain, the old key) is refused and
    // the chain in service stays, then loads once the key arrives.
    let third = ServerCertificate::localhost().unwrap();
    let (third_chain, third_key) = third.to_pem();
    std::fs::write(&chain, third_chain).unwrap();
    assert!(matches!(watch.poll(), Some(Err(_))));
    assert!(QuicClient::connect(&runtime, addr, &second.cert_der).is_ok());
    std::fs::write(&key, third_key).unwrap();
    assert!(matches!(watch.poll(), Some(Ok(()))));
    assert!(QuicClient::connect(&runtime, addr, &third.cert_der).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Public trust: the web PKI roots plus an operator's own CAs. A leaf from
/// a private CA is refused unless that CA is among the extras; without the
/// `public-roots` feature, public trust refuses to connect at all.
#[test]
fn public_trust_needs_the_feature_and_trusts_only_public_or_extra_roots() {
    let runtime = NetRuntime::new(2).unwrap();
    let ca = DevCa::new().unwrap();
    let server = QuicServer::bind(&runtime, loopback(), &ca.issue(&["game.test"]).unwrap()).unwrap();
    let addr = server.local_addr();
    let public = ServerTrust::public_with_pem(b"", "game.test").unwrap();
    let with_ca = ServerTrust::public_with_pem(ca.cert_pem().as_bytes(), "game.test").unwrap();
    if cfg!(feature = "public-roots") {
        assert_eq!(
            untrusted(QuicClient::connect_trusted(&runtime, addr, &public)),
            TrustFailure::UnknownIssuer
        );
        assert!(QuicClient::connect_trusted(&runtime, addr, &with_ca).is_ok());
    } else {
        for trust in [public, with_ca] {
            assert!(matches!(
                QuicClient::connect_trusted(&runtime, addr, &trust),
                Err(NetError::Tls(e)) if e.contains("public-roots")
            ));
        }
    }
}
