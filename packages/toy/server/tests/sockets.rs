//! The toy server over real loopback sockets: a native bot over QUIC and a
//! legacy bot over TCP log in, play, and receive snapshots.

#![expect(clippy::unwrap_used)]

use std::time::Duration;

use mantis_net::NetRuntime;
use mantis_net::quic::{DevCertificate, QuicClient, QuicServer};
use mantis_net::tcp::{TcpClient, TcpServer};
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use toy_server::tunables::Tunables;
use toy_server::wire::LegacyWire;
use toy_server::world;

fn bot(
    wire: Box<dyn mantis_server::bots::BotWire>,
    transport: Box<dyn mantis_adapter_contract::Transport>,
    t: &Tunables,
    seed: u64,
) -> Bot {
    let mut b = Bot::new(
        wire,
        transport,
        world::ground(),
        BotConfig {
            profile: Profile::Honest,
            motion: t.motion,
            rate: t.tick_rate,
            seed,
            content: t.content,
            clock_offset_ms: 1000,
        },
        mantis_core::math::Vec3::ZERO,
    )
    .unwrap();
    b.start();
    b
}

#[test]
fn native_and_legacy_bots_play_over_loopback() {
    let t = Tunables::defaults().unwrap();
    let runtime = NetRuntime::new(2).unwrap();
    let cert = DevCertificate::localhost().unwrap();
    let quic = QuicServer::bind(&runtime, "127.0.0.1:0".parse().unwrap(), &cert).unwrap();
    let tcp = TcpServer::bind(&runtime, "127.0.0.1:0".parse().unwrap()).unwrap();
    let (quic_addr, tcp_addr) = (quic.local_addr(), tcp.local_addr());
    let mut host = world::host(&t, Box::new(quic), Box::new(tcp));
    let mut zone = world::zone(&t, 1, |_| None).unwrap();
    let mut native = bot(
        Box::new(NativeWire::default()),
        Box::new(QuicClient::connect(&runtime, quic_addr, &cert.cert_der).unwrap()),
        &t,
        1,
    );
    let mut legacy = bot(
        Box::new(LegacyWire::new(world::LEGACY_BUILD)),
        Box::new(TcpClient::connect(&runtime, tcp_addr).unwrap()),
        &t,
        2,
    );
    let mut ticks = 0;
    while ticks < 3000 && (native.stats.snapshots < 60 || legacy.stats.snapshots < 60) {
        host.poll(&mut zone);
        zone.step(&mut host, None).unwrap();
        native.step();
        legacy.step();
        std::thread::sleep(Duration::from_millis(2));
        ticks += 1;
    }
    assert_eq!(host.stats.joined, 2);
    assert!(native.synced() && legacy.synced(), "both bots entered the world");
    assert!(
        native.stats.snapshots >= 60 && legacy.stats.snapshots >= 60,
        "snapshots flow"
    );
    assert_eq!(
        legacy.stats.corrections, 0,
        "the honest legacy bot was never corrected"
    );
    assert!(
        native.stats.last_remotes > 0 && legacy.stats.last_remotes > 0,
        "they see each other"
    );
}

/// The game listener's certificate rotates mid-play: every bot already in
/// the world stays (no session drops, snapshots keep flowing), a bot
/// connecting afterwards is handed the new certificate, and one pinning the
/// old certificate is refused as untrusted.
#[test]
fn the_game_certificate_rotates_mid_play_without_dropping_a_session() {
    let t = Tunables::defaults().unwrap();
    let runtime = NetRuntime::new(2).unwrap();
    let old = mantis_net::quic::ServerCertificate::localhost().unwrap();
    let quic = QuicServer::bind(&runtime, "127.0.0.1:0".parse().unwrap(), &old).unwrap();
    let tcp = TcpServer::bind(&runtime, "127.0.0.1:0".parse().unwrap()).unwrap();
    let (quic_addr, tcp_addr) = (quic.local_addr(), tcp.local_addr());
    let reloader = quic.reloader();
    let mut host = world::host(&t, Box::new(quic), Box::new(tcp));
    let mut zone = world::zone(&t, 1, |_| None).unwrap();
    let trust = |cert: &mantis_net::quic::ServerCertificate| {
        mantis_net::quic::ServerTrust::Pinned(cert.cert_der.clone())
    };
    let native = |seed: u64, trust: &mantis_net::quic::ServerTrust| {
        bot(
            Box::new(NativeWire::default()),
            Box::new(mantis_net::quic::QuicClient::connect_trusted(&runtime, quic_addr, trust).unwrap()),
            &t,
            seed,
        )
    };
    let mut bots = vec![
        native(1, &trust(&old)),
        native(2, &trust(&old)),
        bot(
            Box::new(LegacyWire::new(world::LEGACY_BUILD)),
            Box::new(TcpClient::connect(&runtime, tcp_addr).unwrap()),
            &t,
            3,
        ),
    ];
    let mut run = |bots: &mut Vec<Bot>, until: &dyn Fn(&[Bot]) -> bool| {
        let mut ticks = 0;
        while !until(bots) {
            assert!(ticks < 3000, "timed out");
            host.poll(&mut zone);
            zone.step(&mut host, None).unwrap();
            for b in bots.iter_mut() {
                b.step();
            }
            std::thread::sleep(Duration::from_millis(2));
            ticks += 1;
        }
    };
    run(&mut bots, &|b| {
        b.iter().all(|b| b.synced() && b.stats.snapshots >= 30)
    });
    let before: Vec<u64> = bots.iter().map(|b| b.stats.snapshots).collect();

    // Rotate (pinned development certificates; CA-issued chains rotate the
    // same way, crates/net/tests/trust.rs).
    let new = mantis_net::quic::ServerCertificate::localhost().unwrap();
    reloader.reload(&new).unwrap();
    // A bot pinning the old certificate is refused, by cause.
    assert!(matches!(
        mantis_net::quic::QuicClient::connect_trusted(&runtime, quic_addr, &trust(&old)),
        Err(mantis_net::NetError::Untrusted(
            mantis_net::quic::TrustFailure::UnknownIssuer
        ))
    ));
    // New bots pinning the new certificate connect and play.
    bots.push(native(4, &trust(&new)));
    bots.push(native(5, &trust(&new)));
    run(&mut bots, &|b| {
        b.iter().all(Bot::synced) && b.iter().zip(&before).all(|(b, n)| b.stats.snapshots >= n + 60)
    });
    assert_eq!(host.sessions_in_world(), 5, "nobody dropped, both new bots in");
    assert_eq!(host.stats.joined, 5);
}
