//! A deterministic in-process network for bots, tests, and budget scenarios.
//!
//! [`SimNet`] connects any number of [`SimClient`]s to one [`SimServer`]; all
//! three implement `Transport`, so a host cannot tell them from sockets.
//! Time is simulated (milliseconds advanced by the driver with
//! [`SimNet::advance`]) and randomness comes from a seeded stream, so a
//! scenario replays identically.
//!
//! Link model per direction:
//! - **reliable**: every frame arrives, in order. A lost transmission costs one
//!   retransmission timeout, and later frames wait behind it (head-of-line
//!   blocking, as on TCP or a QUIC stream);
//! - **unreliable**: a lost frame is gone; delivery is FIFO (jitter delays
//!   but does not reorder), and the receiver would drop any out-of-order
//!   frame anyway (the Mantis sequence header semantics).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::lock;
use mantis_adapter_contract::{
    Channel, ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent, TransportKind,
};
use mantis_core::rng::{Rng, Salt, Seed};
use mantis_core::time::Tick;

/// One link's conditions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LinkConfig {
    /// One-way latency in ms (half the RTT).
    pub one_way_ms: u32,
    /// Extra uniform jitter in ms, `0..=jitter_ms`.
    pub jitter_ms: u32,
    /// Loss probability per transmission, per mille.
    pub loss_permille: u32,
    /// Retransmission delay of a lost reliable frame, in ms.
    pub rto_ms: u32,
}

impl LinkConfig {
    /// A perfect link.
    pub const PERFECT: Self = Self {
        one_way_ms: 0,
        jitter_ms: 0,
        loss_permille: 0,
        rto_ms: 0,
    };

    /// The budget-table condition: 100 ms RTT and 2% loss.
    pub const RTT100_LOSS2: Self = Self {
        one_way_ms: 50,
        jitter_ms: 10,
        loss_permille: 20,
        rto_ms: 200,
    };
}

struct InFlight {
    at: u64,
    channel: Channel,
    reliable: bool,
    seq: u64,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Direction {
    flight: VecDeque<InFlight>,
    last_reliable_at: u64,
    last_unreliable_at: u64,
    last_unreliable_seq: u64,
    next_seq: u64,
    delivered: VecDeque<(Channel, Vec<u8>)>,
}

struct Link {
    cfg: LinkConfig,
    up: Direction,
    down: Direction,
    open: bool,
    announced: bool,
}

struct State {
    now: u64,
    rng: Rng,
    links: BTreeMap<u64, Link>,
    next_conn: u64,
    server_events: VecDeque<(u64, bool)>,
    max_unreliable: usize,
    stream_only: bool,
}

impl State {
    fn send(&mut self, conn: u64, up: bool, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        if channel == Channel::Unreliable && bytes.len() > self.max_unreliable {
            return Err(TransportError::TooLarge {
                len: bytes.len(),
                max: self.max_unreliable,
            });
        }
        let now = self.now;
        let rng = &mut self.rng;
        let link = self
            .links
            .get_mut(&conn)
            .filter(|l| l.open)
            .ok_or(TransportError::UnknownConnection(ConnectionId(conn)))?;
        let cfg = link.cfg;
        let dir = if up { &mut link.up } else { &mut link.down };
        let mut at = now + u64::from(cfg.one_way_ms) + u64::from(rng.below(cfg.jitter_ms + 1));
        let lost = rng.chance(cfg.loss_permille, 1000);
        let reliable = self.stream_only || channel == Channel::Reliable;
        match if reliable {
            Channel::Reliable
        } else {
            Channel::Unreliable
        } {
            Channel::Reliable => {
                if lost {
                    at += u64::from(cfg.rto_ms);
                }
                at = at.max(dir.last_reliable_at);
                dir.last_reliable_at = at;
            }
            Channel::Unreliable => {
                if lost {
                    return Ok(());
                }
                // Network queues are FIFO: jitter delays, it does not reorder.
                at = at.max(dir.last_unreliable_at);
                dir.last_unreliable_at = at;
            }
        }
        dir.next_seq += 1;
        dir.flight.push_back(InFlight {
            at,
            channel,
            reliable,
            seq: dir.next_seq,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn deliver(&mut self) {
        let now = self.now;
        for link in self.links.values_mut() {
            for dir in [&mut link.up, &mut link.down] {
                // Deliver in arrival-time order; unreliable frames that arrive
                // after a newer one are dropped.
                let mut due: Vec<InFlight> = Vec::new();
                let mut keep = VecDeque::new();
                while let Some(f) = dir.flight.pop_front() {
                    if f.at <= now {
                        due.push(f);
                    } else {
                        keep.push_back(f);
                    }
                }
                dir.flight = keep;
                due.sort_by_key(|f| (f.at, f.seq));
                for f in due {
                    if !f.reliable {
                        if f.seq <= dir.last_unreliable_seq {
                            continue;
                        }
                        dir.last_unreliable_seq = f.seq;
                    }
                    dir.delivered.push_back((f.channel, f.bytes));
                }
            }
        }
    }
}

/// The simulated network.
#[derive(Clone)]
pub struct SimNet {
    state: Arc<Mutex<State>>,
}

impl SimNet {
    /// A network whose randomness derives from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                now: 0,
                rng: Rng::for_cell(Seed(seed), Tick(0), Salt::named("server.simnet")),
                links: BTreeMap::new(),
                next_conn: 1,
                server_events: VecDeque::new(),
                max_unreliable: 1100,
                stream_only: false,
            })),
        }
    }

    /// The server end. A TCP-kind network carries every frame on its one
    /// stream: both channels are reliable and ordered, as on a socket.
    #[must_use]
    pub fn server(&self, kind: TransportKind) -> SimServer {
        lock(&self.state).stream_only = kind == TransportKind::Tcp;
        SimServer {
            state: Arc::clone(&self.state),
            kind,
        }
    }

    /// Opens a client connection with `cfg` conditions.
    #[must_use]
    pub fn connect(&self, cfg: LinkConfig) -> SimClient {
        let mut s = lock(&self.state);
        let id = s.next_conn;
        s.next_conn += 1;
        s.links.insert(
            id,
            Link {
                cfg,
                up: Direction::default(),
                down: Direction::default(),
                open: true,
                announced: false,
            },
        );
        s.server_events.push_back((id, true));
        SimClient {
            state: Arc::clone(&self.state),
            conn: id,
        }
    }

    /// Changes connection `conn`'s conditions mid-run: frames sent from now
    /// on, both ways, use `cfg` (frames in flight keep their arrival time;
    /// reliable frames still arrive in order). False for an unknown
    /// connection.
    pub fn set_link(&self, conn: ConnectionId, cfg: LinkConfig) -> bool {
        lock(&self.state).links.get_mut(&conn.0).is_some_and(|l| {
            l.cfg = cfg;
            true
        })
    }

    /// Advances simulated time by `ms` and delivers everything due.
    pub fn advance(&self, ms: u64) {
        let mut s = lock(&self.state);
        s.now += ms;
        s.deliver();
    }

    /// Simulated time in ms.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        lock(&self.state).now
    }
}

/// The server end of a [`SimNet`].
pub struct SimServer {
    state: Arc<Mutex<State>>,
    kind: TransportKind,
}

impl Transport for SimServer {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        let mut s = lock(&self.state);
        while let Some((conn, connected)) = s.server_events.pop_front() {
            if connected {
                if let Some(l) = s.links.get_mut(&conn) {
                    l.announced = true;
                }
                sink(TransportEvent::Connected(ConnectionId(conn)));
            } else {
                sink(TransportEvent::Disconnected {
                    conn: ConnectionId(conn),
                    reason: DisconnectReason::Closed,
                });
            }
        }
        for (id, link) in &mut s.links {
            while let Some((channel, bytes)) = link.up.delivered.pop_front() {
                sink(TransportEvent::Frame {
                    conn: ConnectionId(*id),
                    channel,
                    bytes: &bytes,
                });
            }
        }
    }

    fn send(&mut self, conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        lock(&self.state).send(conn.0, false, channel, bytes)
    }

    fn disconnect(&mut self, conn: ConnectionId) {
        if let Some(l) = lock(&self.state).links.get_mut(&conn.0) {
            l.open = false;
        }
    }

    fn kind(&self) -> TransportKind {
        self.kind
    }

    fn max_unreliable_payload(&self) -> usize {
        lock(&self.state).max_unreliable
    }
}

/// A client end of a [`SimNet`]; its server is `ConnectionId(0)`.
pub struct SimClient {
    state: Arc<Mutex<State>>,
    conn: u64,
}

impl SimClient {
    /// This client's connection id as the server sees it.
    #[must_use]
    pub fn id(&self) -> ConnectionId {
        ConnectionId(self.conn)
    }

    /// Changes this connection's conditions mid-run ([`SimNet::set_link`]).
    pub fn set_link(&self, cfg: LinkConfig) {
        if let Some(l) = lock(&self.state).links.get_mut(&self.conn) {
            l.cfg = cfg;
        }
    }
}

impl Transport for SimClient {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        let mut s = lock(&self.state);
        if let Some(link) = s.links.get_mut(&self.conn) {
            while let Some((channel, bytes)) = link.down.delivered.pop_front() {
                sink(TransportEvent::Frame {
                    conn: ConnectionId(0),
                    channel,
                    bytes: &bytes,
                });
            }
        }
    }

    fn send(&mut self, _conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        lock(&self.state).send(self.conn, true, channel, bytes)
    }

    fn disconnect(&mut self, _conn: ConnectionId) {
        let mut s = lock(&self.state);
        if let Some(l) = s.links.get_mut(&self.conn) {
            l.open = false;
        }
        s.server_events.push_back((self.conn, false));
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn max_unreliable_payload(&self) -> usize {
        lock(&self.state).max_unreliable
    }
}

/// A gateway's connections to simulated cell hosts
/// ([`mantis_net::gateway::Dialer`]): each address names a host's
/// [`SimNet`], reached over a link with the given conditions. A host
/// closing a connection is not seen here (the simulated network does not
/// tell clients); the gateway closing one is seen by the host.
pub struct SimDialer {
    hosts: BTreeMap<String, (SimNet, LinkConfig)>,
    conns: BTreeMap<u64, SimClient>,
    connected: VecDeque<u64>,
    next: u64,
}

impl SimDialer {
    /// No hosts yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hosts: BTreeMap::new(),
            conns: BTreeMap::new(),
            connected: VecDeque::new(),
            next: 1,
        }
    }

    /// Makes `address` reach the host listening on `net`, over links with
    /// `cfg` conditions.
    pub fn add_host(&mut self, address: &str, net: &SimNet, cfg: LinkConfig) {
        self.hosts.insert(address.to_owned(), (net.clone(), cfg));
    }
}

impl Default for SimDialer {
    fn default() -> Self {
        Self::new()
    }
}

impl mantis_net::gateway::Dialer for SimDialer {
    fn dial(&mut self, address: &str) -> Option<ConnectionId> {
        let (net, cfg) = self.hosts.get(address)?;
        let client = net.connect(*cfg);
        let id = self.next;
        self.next += 1;
        self.conns.insert(id, client);
        self.connected.push_back(id);
        Some(ConnectionId(id))
    }
}

impl Transport for SimDialer {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        while let Some(id) = self.connected.pop_front() {
            sink(TransportEvent::Connected(ConnectionId(id)));
        }
        for (id, client) in &mut self.conns {
            client.poll(&mut |e| {
                if let TransportEvent::Frame { channel, bytes, .. } = e {
                    sink(TransportEvent::Frame {
                        conn: ConnectionId(*id),
                        channel,
                        bytes,
                    });
                }
            });
        }
    }

    fn send(&mut self, conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        self.conns
            .get_mut(&conn.0)
            .ok_or(TransportError::UnknownConnection(conn))?
            .send(ConnectionId(0), channel, bytes)
    }

    fn disconnect(&mut self, conn: ConnectionId) {
        if let Some(mut client) = self.conns.remove(&conn.0) {
            client.disconnect(ConnectionId(0));
        }
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn max_unreliable_payload(&self) -> usize {
        self.hosts
            .values()
            .next()
            .map_or(1100, |(net, _)| lock(&net.state).max_unreliable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(t: &mut dyn Transport) -> Vec<(Channel, Vec<u8>)> {
        let mut v = Vec::new();
        t.poll(&mut |e| {
            if let TransportEvent::Frame { channel, bytes, .. } = e {
                v.push((channel, bytes.to_vec()));
            }
        });
        v
    }

    #[test]
    fn reliable_is_complete_and_ordered_unreliable_is_lossy_and_sequenced() {
        let net = SimNet::new(1);
        let mut server = net.server(TransportKind::Quic);
        let mut client = net.connect(LinkConfig {
            one_way_ms: 50,
            jitter_ms: 30,
            loss_permille: 100,
            rto_ms: 200,
        });
        let mut connected = 0;
        server.poll(&mut |e| {
            if matches!(e, TransportEvent::Connected(_)) {
                connected += 1;
            }
        });
        assert_eq!(connected, 1);
        for i in 0..500u32 {
            client
                .send(ConnectionId(0), Channel::Reliable, &i.to_le_bytes())
                .unwrap();
            client
                .send(ConnectionId(0), Channel::Unreliable, &i.to_le_bytes())
                .unwrap();
            net.advance(10);
        }
        net.advance(10_000);
        let got = drain(&mut server);
        let rel: Vec<u32> = got
            .iter()
            .filter(|(c, _)| *c == Channel::Reliable)
            .map(|(_, b)| u32::from_le_bytes(b.as_slice().try_into().unwrap()))
            .collect();
        assert_eq!(rel, (0..500).collect::<Vec<_>>(), "reliable: all, in order");
        let un: Vec<u32> = got
            .iter()
            .filter(|(c, _)| *c == Channel::Unreliable)
            .map(|(_, b)| u32::from_le_bytes(b.as_slice().try_into().unwrap()))
            .collect();
        assert!(un.len() < 480 && un.len() > 420, "about 10% lost: {}", un.len());
        assert!(un.windows(2).all(|w| w[0] < w[1]), "never out of order");
    }

    #[test]
    fn a_tcp_network_carries_both_channels_on_its_stream() {
        let net = SimNet::new(3);
        let mut server = net.server(TransportKind::Tcp);
        let mut client = net.connect(LinkConfig::RTT100_LOSS2);
        for i in 0..300u32 {
            let ch = if i % 2 == 0 {
                Channel::Reliable
            } else {
                Channel::Unreliable
            };
            client.send(ConnectionId(0), ch, &i.to_le_bytes()).unwrap();
            net.advance(10);
        }
        net.advance(10_000);
        let got = drain(&mut server);
        let all: Vec<u32> = got
            .iter()
            .map(|(_, b)| u32::from_le_bytes(b.as_slice().try_into().unwrap()))
            .collect();
        assert_eq!(all, (0..300).collect::<Vec<_>>(), "everything, in order");
        assert_eq!(got[1].0, Channel::Unreliable, "the channel label is kept");
    }

    #[test]
    fn latency_is_applied() {
        let net = SimNet::new(2);
        let mut server = net.server(TransportKind::Quic);
        let mut client = net.connect(LinkConfig {
            one_way_ms: 50,
            ..LinkConfig::PERFECT
        });
        client.send(ConnectionId(0), Channel::Reliable, b"x").unwrap();
        net.advance(49);
        assert!(drain(&mut server).is_empty());
        net.advance(1);
        assert_eq!(drain(&mut server).len(), 1);
    }
}
