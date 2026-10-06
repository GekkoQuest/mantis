//! Loopback tests of both transports: connection events, reliable ordering,
//! unreliable sequencing, framing violations, limits, and the zero-allocation
//! caller side.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mantis_adapter_contract::{
    Channel, ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent,
};
use mantis_net::NetRuntime;
use mantis_net::quic::{DevCertificate, QuicClient, QuicServer, SERVER};
use mantis_net::tcp::{TcpClient, TcpServer};
use mantis_testkit::alloc::{CountingAllocator, count_allocs};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Connected(ConnectionId),
    Frame(ConnectionId, Channel, Vec<u8>),
    Disconnected(ConnectionId, DisconnectReason),
}

fn drain(t: &mut dyn Transport, out: &mut Vec<Ev>) {
    t.poll(&mut |e| {
        out.push(match e {
            TransportEvent::Connected(c) => Ev::Connected(c),
            TransportEvent::Frame { conn, channel, bytes } => Ev::Frame(conn, channel, bytes.to_vec()),
            TransportEvent::Disconnected { conn, reason } => Ev::Disconnected(conn, reason),
        });
    });
}

/// Polls `t` until `done(events)` or the timeout.
fn wait(t: &mut dyn Transport, events: &mut Vec<Ev>, done: impl Fn(&[Ev]) -> bool) {
    let start = Instant::now();
    while !done(events) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out; events so far: {events:?}"
        );
        drain(t, events);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn frames(events: &[Ev], channel: Channel) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            Ev::Frame(_, c, b) if *c == channel => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn exercise(server: &mut dyn Transport, client: &mut dyn Transport) {
    let mut sev = Vec::new();
    wait(server, &mut sev, |e| {
        e.iter().any(|e| matches!(e, Ev::Connected(_)))
    });
    let Ev::Connected(conn) = sev[0] else {
        panic!("{sev:?}")
    };

    // Reliable, ordered, both directions.
    for i in 0..200u32 {
        client.send(SERVER, Channel::Reliable, &i.to_le_bytes()).unwrap();
    }
    wait(server, &mut sev, |e| frames(e, Channel::Reliable).len() == 200);
    let got: Vec<u32> = frames(&sev, Channel::Reliable)
        .iter()
        .map(|b| u32::from_le_bytes(b[..4].try_into().unwrap()))
        .collect();
    assert_eq!(got, (0..200).collect::<Vec<_>>(), "reliable is ordered");
    server.send(conn, Channel::Reliable, b"welcome").unwrap();
    let mut cev = Vec::new();
    wait(client, &mut cev, |e| {
        frames(e, Channel::Reliable).contains(&b"welcome".to_vec())
    });

    // Unreliable: delivered on loopback, never out of order.
    for i in 0..50u32 {
        client
            .send(SERVER, Channel::Unreliable, &i.to_le_bytes())
            .unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    wait(server, &mut sev, |e| {
        !frames(e, Channel::Unreliable).is_empty()
            && frames(e, Channel::Unreliable).last() == Some(&49u32.to_le_bytes().to_vec())
    });
    let un: Vec<u32> = frames(&sev, Channel::Unreliable)
        .iter()
        .map(|b| u32::from_le_bytes(b[..4].try_into().unwrap()))
        .collect();
    assert!(un.windows(2).all(|w| w[0] < w[1]), "sequenced: {un:?}");

    // Limits and unknown connections fail closed.
    let big = vec![0u8; client.max_unreliable_payload() + 1];
    assert!(matches!(
        client.send(SERVER, Channel::Unreliable, &big),
        Err(TransportError::TooLarge { .. })
    ));
    assert_eq!(
        server.send(ConnectionId(999), Channel::Reliable, b"x"),
        Err(TransportError::UnknownConnection(ConnectionId(999)))
    );

    // Steady state: the caller side allocates nothing.
    let payload = [7u8; 64];
    for _ in 0..50 {
        client.send(SERVER, Channel::Reliable, &payload).unwrap();
    }
    wait(server, &mut sev, |e| frames(e, Channel::Reliable).len() >= 250);
    std::thread::sleep(Duration::from_millis(50));
    let mut seen = 0usize;
    let ((), stats) = count_allocs(|| {
        for _ in 0..20 {
            let _ = client.send(SERVER, Channel::Reliable, &payload);
        }
    });
    assert!(stats.is_zero(), "send allocated: {stats}");
    std::thread::sleep(Duration::from_millis(100));
    let ((), stats) = count_allocs(|| {
        server.poll(&mut |e| {
            if matches!(e, TransportEvent::Frame { .. }) {
                seen += 1;
            }
        });
    });
    assert!(seen > 0);
    assert!(stats.is_zero(), "poll allocated: {stats}");

    // Server-side disconnect reaches both ends.
    server.disconnect(conn);
    wait(server, &mut sev, |e| {
        e.iter()
            .any(|e| matches!(e, Ev::Disconnected(c, _) if *c == conn))
    });
    wait(client, &mut cev, |e| {
        e.iter().any(|e| matches!(e, Ev::Disconnected(..)))
    });
}

#[test]
fn quic_loopback() {
    let rt = NetRuntime::new(2).unwrap();
    let cert = DevCertificate::localhost().unwrap();
    let mut server = QuicServer::bind(&rt, loopback(), &cert).unwrap();
    let mut client = QuicClient::connect(&rt, server.local_addr(), &cert.cert_der).unwrap();
    exercise(&mut server, &mut client);
}

#[test]
fn quic_refuses_an_untrusted_certificate() {
    let rt = NetRuntime::new(1).unwrap();
    let cert = DevCertificate::localhost().unwrap();
    let other = DevCertificate::localhost().unwrap();
    let server = QuicServer::bind(&rt, loopback(), &cert).unwrap();
    assert!(QuicClient::connect(&rt, server.local_addr(), &other.cert_der).is_err());
}

#[test]
fn tcp_loopback() {
    let rt = NetRuntime::new(2).unwrap();
    let mut server = TcpServer::bind(&rt, loopback()).unwrap();
    let mut client = TcpClient::connect(&rt, server.local_addr()).unwrap();
    exercise(&mut server, &mut client);
}

#[test]
fn tcp_refuses_a_bad_preamble() {
    use std::io::Write;
    let rt = NetRuntime::new(1).unwrap();
    let mut server = TcpServer::bind(&rt, loopback()).unwrap();
    let mut raw = std::net::TcpStream::connect(server.local_addr()).unwrap();
    raw.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut ev = Vec::new();
    wait(&mut server, &mut ev, |e| {
        e.iter().any(|e| matches!(e, Ev::Disconnected(..)))
    });
    assert!(
        ev.iter()
            .any(|e| matches!(e, Ev::Disconnected(_, DisconnectReason::ProtocolViolation)))
    );
    assert!(
        frames(&ev, Channel::Reliable).is_empty(),
        "no frame from a bad stream"
    );
}
