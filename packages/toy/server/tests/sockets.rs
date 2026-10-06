//! The toy server over real loopback sockets: a native bot over QUIC and a
//! legacy bot over TCP log in, play, and receive snapshots.

#![allow(clippy::unwrap_used)]

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
