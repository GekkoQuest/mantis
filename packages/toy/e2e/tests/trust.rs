//! The game connection's trust, end to end over loopback QUIC: the toy server's game
//! listener presents a leaf issued by a CA for the name `game.test` and the address
//! `127.0.0.1`, and the real toy client verifies it against a CA bundle.
//!
//! - The client plays; the server rotates its certificate mid-session (a new leaf from the
//!   same CA): the open session never drops and frames keep decoding, and a client that
//!   connects after the rotation verifies the new chain against the same bundle.
//! - A wrong name, an untrusted chain (another CA), an expired certificate, and a pinned
//!   development leaf that is not the server's are refused, each with a notice naming the
//!   reason.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::net::SessionState;
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::time::HostClock;
use mantis_net::NetRuntime;
use mantis_net::quic::{DevCa, QuicClient, QuicServer, ServerCertificate, ServerTrust};
use mantis_net::tcp::TcpServer;
use toy_client::trust::{TrustArgs, TrustFlags, notice};
use toy_client::{HeadlessSink, ToyClient};
use toy_server::tunables::Tunables;
use toy_server::world;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Client = ToyClient<QuicClient, HeadlessSink>;

fn bundle_trust(ca: &DevCa, name: &str) -> Result<ServerTrust, Box<dyn std::error::Error>> {
    Ok(ServerTrust::from_pem_bundle(ca.cert_pem().as_bytes(), name)?)
}

fn welcomed(c: &Client) -> bool {
    matches!(c.net.state(), SessionState::Welcomed { avatar: Some(_), .. })
}

/// The refusal notice for connecting to `addr` with `trust` (an error if it connected).
fn refusal(runtime: &NetRuntime, addr: SocketAddr, trust: &ServerTrust) -> Result<String, String> {
    match QuicClient::connect_trusted(runtime, addr, trust) {
        Ok(_) => Err("connected".to_owned()),
        Err(e) => Ok(notice(&e, trust)),
    }
}

#[test]
fn a_ca_bundle_verifies_the_game_listener_across_a_certificate_rotation() -> TestResult {
    let t = Tunables::defaults()?;
    let runtime = NetRuntime::new(2)?;
    let ca = DevCa::new()?;
    let leaf = ca.issue(&["game.test", "127.0.0.1"])?;
    let quic = QuicServer::bind(&runtime, "127.0.0.1:0".parse()?, &leaf)?;
    let reloader = quic.reloader();
    let tcp = TcpServer::bind(&runtime, "127.0.0.1:0".parse()?)?;
    let addr = quic.local_addr();
    let mut host = world::host(&t, Box::new(quic), Box::new(tcp));
    let mut zone = world::zone(&t, 1, |_| None).map_err(|e| format!("{e:?}"))?;

    // The bundle as an operator hands it out, and the name from the address (the
    // client's default when --server-name is not given).
    let dir = std::env::temp_dir().join(format!("mantis-trust-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let bundle = dir.join("ca.crt");
    std::fs::write(&bundle, ca.cert_pem())?;
    let trust = TrustArgs::from_flags(
        TrustFlags {
            ca_bundle: bundle.to_str(),
            ..TrustFlags::default()
        },
        "unused.der",
    )?
    .load(toy_client::gateway::Gateway::parse(&addr.to_string())?.host())?;
    let clock: Arc<dyn HostClock> = Arc::new(mantis_client::platform::MonotonicClock::new());
    let mut client = ToyClient::connect_trusted(&runtime, addr, &trust, Arc::clone(&clock), HeadlessSink)?;
    client.start(b"trusted");

    let mut rotated_at = None;
    let mut late: Option<Client> = None;
    let mut pressed = false;
    for _ in 0..6000 {
        host.poll(&mut zone);
        zone.step(&mut host, None).map_err(|e| format!("{e:?}"))?;
        client.step(1);
        if let Some(c) = late.as_mut() {
            c.step(1);
        }
        if !pressed && welcomed(&client) {
            client
                .events
                .as_ref()
                .ok_or("events")?
                .send(PlatformEvent::Input(RawInput::Button {
                    source: ButtonSource::Key(KeyCode::W),
                    pressed: true,
                }))?;
            pressed = true;
        }
        let snapshots = client.net.stats().snapshots;
        if rotated_at.is_none() && snapshots >= 60 {
            // Rotate: a new leaf from the same CA. The open session keeps its connection;
            // new handshakes get the new chain.
            let next: ServerCertificate = ca.issue(&["game.test", "127.0.0.1"])?;
            reloader.reload(&next)?;
            rotated_at = Some(snapshots);
            let mut c = ToyClient::connect_trusted(&runtime, addr, &trust, Arc::clone(&clock), HeadlessSink)?;
            c.start(b"after-rotation");
            late = Some(c);
        }
        let done = rotated_at.is_some_and(|r| snapshots >= r + 60)
            && late
                .as_ref()
                .is_some_and(|c| welcomed(c) && c.net.stats().snapshots >= 30);
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = std::fs::remove_dir_all(&dir);
    let rotated_at = rotated_at.ok_or("never rotated")?;
    let net = client.net.stats();
    assert!(
        welcomed(&client),
        "the session survived the rotation: {:?}",
        client.net.state()
    );
    assert!(net.snapshots >= rotated_at + 60, "frames kept coming: {net:?}");
    assert_eq!(net.undecodable, 0, "{net:?}");
    assert_eq!(net.reconnects, 0, "no reconnect was needed: {net:?}");
    let late = late.ok_or("no second client")?;
    assert!(
        welcomed(&late),
        "a client after the rotation verified the new chain"
    );
    assert_eq!(late.net.stats().undecodable, 0);
    println!(
        "trust: rotated at snapshot {rotated_at}; the session went on to {} snapshots; a client after the rotation took {}",
        net.snapshots,
        late.net.stats().snapshots
    );
    Ok(())
}

#[test]
fn untrusted_servers_are_refused_with_the_reason() -> TestResult {
    let runtime = NetRuntime::new(2)?;
    let ca = DevCa::new()?;
    let leaf = ca.issue(&["game.test", "127.0.0.1"])?;
    let server = QuicServer::bind(&runtime, "127.0.0.1:0".parse()?, &leaf)?;
    let addr = server.local_addr();

    // The right CA and name: trusted.
    assert!(QuicClient::connect_trusted(&runtime, addr, &bundle_trust(&ca, "game.test")?).is_ok());
    // A name the certificate does not carry.
    let why = refusal(&runtime, addr, &bundle_trust(&ca, "other.test")?)?;
    assert!(why.contains("not valid for `other.test`"), "{why}");
    // Another CA: an untrusted chain.
    let stranger = DevCa::new()?;
    let why = refusal(&runtime, addr, &bundle_trust(&stranger, "game.test")?)?;
    assert!(why.contains("not signed by a CA in --ca-bundle"), "{why}");
    // A pinned development leaf that is not the server's.
    let dev = ServerCertificate::localhost()?;
    let pinned = ServerTrust::Pinned(dev.cert_der.clone());
    let why = refusal(&runtime, addr, &pinned)?;
    assert!(why.contains("not the pinned development certificate"), "{why}");
    drop(server);

    // An expired certificate.
    let old = ca.issue_with_validity(&["game.test", "127.0.0.1"], ((2020, 1, 1), (2021, 1, 1)))?;
    let server = QuicServer::bind(&runtime, "127.0.0.1:0".parse()?, &old)?;
    let why = refusal(&runtime, server.local_addr(), &bundle_trust(&ca, "game.test")?)?;
    assert!(why.contains("has expired"), "{why}");
    Ok(())
}
