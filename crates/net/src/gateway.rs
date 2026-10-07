//! The gateway (M13c): the one game address clients connect to. It
//! terminates their TLS (a [`crate::quic::QuicServer`] in production),
//! routes each session to the cell host serving its character, and relays
//! frames both ways without decoding them, so a session survives the
//! character moving to another host, and the client's connection dropping.
//!
//! - **Routing.** A session's `Hello` names an entry token. [`Routes`]
//!   (the realm, in a cluster) checks it without consuming it and names the
//!   cell and its host's game address; the gateway connects there through
//!   its [`Dialer`] and forwards the `Hello` unchanged: the host redeems the
//!   token and admits the session as it would a direct client. A token that
//!   does not route is refused at the gateway and never reaches a host.
//! - **Relay.** Client frames go to the host serving the session, host
//!   frames to the client, on the channel they came on. Only the leading
//!   bytes of a frame are read (its kind and message id); nothing is
//!   allocated per frame. Two frames are filtered: a client's `Linked`
//!   (the gateway's own word to a host) is dropped, and a `SnapshotAck` is
//!   forwarded only for a snapshot this session's host sent (so after a
//!   hand-off no acknowledgement of the host before reaches the new one).
//! - **Hand-off.** A host sends `HandOff { cell, address, token }`: the
//!   gateway connects to that host and presents the token with the
//!   session's `Hello`. Until the new host welcomes the session the old one
//!   keeps serving it; then the gateway closes the old connection (the
//!   session ends there), sends the client `Transferred { cell, epoch, tick
//!   }` before any frame of the new host, and stamps every later snapshot
//!   with the session's epoch (native snapshot flag bit 3), so the client
//!   drops a snapshot of the old host that arrives late. A refused or
//!   failed hand-off tells the old host `Linked { up: true }`, and the
//!   session plays on there.
//! - **Resume.** After `Welcome` and after every `Transferred` the client
//!   gets a `ResumeTicket`. When its connection drops, the gateway keeps
//!   the session's host connection for [`GatewayConfig::resume_ms`] and
//!   tells the host `Linked { up: false }` (the avatar comes to rest). A
//!   client that reconnects with `Hello { token: ticket }` gets the session
//!   back: the host is told `Linked { up: true }` (the next input starts
//!   the stream), and the client gets `Welcome` and a new ticket. A ticket
//!   is single use; one replaced by a newer ticket is refused with
//!   `StaleEpoch`. A ticket presented while the old connection still looks
//!   alive replaces that connection.
//! - **Refusals.** `BadToken` (the token does not route, or the ticket is
//!   unknown), `Standby` (no host serves the cell now, or it cannot be
//!   reached: retry shortly), `StaleEpoch`, `Full`; a host's own refusal is
//!   relayed.
//!
//! The gateway speaks the native protocol. Legacy (plaintext TCP) clients
//! keep connecting to a cell host's legacy listener: their protocol has no
//! TLS to terminate and no form for a hand-off or a resume.
//!
//! [`Gateway::poll`] is synchronous and non-blocking: dials and route
//! checks complete in the background and arrive on later polls.

use std::collections::BTreeMap;

use mantis_adapter_contract::core_types::{BoundedArray, Message, MessageId, Tick};
use mantis_adapter_contract::native::{
    FRAME_MESSAGE, FRAME_SNAPSHOT, encode_inbound, encode_outbound_frame, stamp_epoch,
};
use mantis_adapter_contract::{
    Channel, ConnectionId, HandOff, Hello, Inbound, Linked, Outbound, Refuse, RefuseReason, Relinked,
    ResumeTicket, SnapshotAck, Transferred, Transport, TransportEvent, Welcome, decode_outbound,
    parse_inbound,
};

/// Connections to cell hosts, made on demand.
pub trait Dialer: Transport {
    /// Starts connecting to `address` (a cell host's game address). Its
    /// `Connected`, or its `Disconnected` when it cannot be made, arrives
    /// on a later poll. `None`: not an address this dialer reaches.
    fn dial(&mut self, address: &str) -> Option<ConnectionId>;
}

/// Where an entry token sends a session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Route {
    /// To `cell`, served by the host at `address`.
    Host {
        /// The cell.
        cell: u64,
        /// Its host's game address.
        address: String,
    },
    /// Refused, and why.
    Refuse(RefuseReason),
}

/// Checks entry tokens off the poll (the realm, in a cluster). `begin` must
/// not block; answers arrive at a later poll.
pub trait Routes: Send {
    /// Starts checking `token` for the session `ticket`.
    fn begin(&mut self, ticket: u64, token: &[u8]);
    /// Answers that arrived since the last call.
    fn ready(&mut self, out: &mut Vec<(u64, Route)>);
}

/// Fills a fresh resume ticket (the OS's secure random source in
/// production; seeded in a simulation).
pub type TicketSource = Box<dyn FnMut(&mut [u8; 32]) + Send>;

/// Gateway settings.
#[derive(Clone, Copy, Debug)]
pub struct GatewayConfig {
    /// How long a session waits for a dropped client, in milliseconds (the
    /// resume ticket's validity).
    pub resume_ms: u64,
    /// Sessions at once; more are refused with `Full`.
    pub capacity: usize,
    /// How long a route check or a host connection may take before the
    /// session is refused with `Standby`, in milliseconds.
    pub route_timeout_ms: u64,
}

impl GatewayConfig {
    /// A minute to resume, 4096 sessions, five seconds to route.
    pub const DEFAULT: Self = Self {
        resume_ms: 60_000,
        capacity: 4096,
        route_timeout_ms: 5_000,
    };
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Gateway counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GatewayStats {
    /// Sessions a host welcomed.
    pub joined: u64,
    /// Frames relayed from clients to hosts.
    pub up: u64,
    /// Frames relayed from hosts to clients.
    pub down: u64,
    /// Snapshots stamped with a hand-off epoch.
    pub stamped: u64,
    /// Hand-offs completed.
    pub handed_off: u64,
    /// Hand-offs that failed (the session stayed on its host).
    pub hand_offs_failed: u64,
    /// Sessions resumed with a ticket.
    pub resumed: u64,
    /// Sessions whose client never came back in time.
    pub expired: u64,
    /// Sessions refused at the gateway.
    pub refused: u64,
    /// Frames dropped: a client's `Linked`, an acknowledgement of a
    /// snapshot this session's host did not send, a frame before its phase.
    pub dropped: u64,
    /// Sends the transports refused (backpressure, closed connections).
    pub send_errors: u64,
}

/// Ticks of snapshots relayed per session, kept to filter
/// acknowledgements (the client's delta window).
const SENT_TICKS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Waiting for the client's `Hello`.
    Hello,
    /// The token is being checked.
    Routing,
    /// Connecting to the host.
    Dialing,
    /// `Hello` sent; waiting for `Welcome`.
    Joining,
    /// Relaying.
    Live,
}

/// A hand-off in progress: the new host's connection.
struct Next {
    conn: ConnectionId,
    cell: u64,
    hello: Vec<u8>,
    connected: bool,
}

/// After a resume, which of the host's snapshots go to the new connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hold {
    /// All of them.
    None,
    /// None until the host says from which tick they are new (`Relinked`):
    /// a snapshot built before the host learnt of the resume deltas against
    /// frames the new connection never had.
    Awaiting,
    /// Those of this tick and later.
    From(u64),
}

struct Session {
    phase: Phase,
    hold: Hold,
    /// The client's connection (`None` while it is away).
    client: Option<ConnectionId>,
    /// The host's connection.
    host: Option<ConnectionId>,
    next: Option<Next>,
    /// The client's `Hello` frame, sent to every host.
    hello: Vec<u8>,
    cell: u64,
    epoch: u32,
    ticket: Option<[u8; 32]>,
    /// Clock milliseconds the phase or the wait for the client ends.
    deadline: u64,
    welcome: Option<Welcome>,
    sent: [u64; SENT_TICKS],
    sent_at: usize,
    last_tick: u64,
}

impl Session {
    fn new(client: ConnectionId, deadline: u64) -> Self {
        Self {
            phase: Phase::Hello,
            hold: Hold::None,
            client: Some(client),
            host: None,
            next: None,
            hello: Vec::new(),
            cell: 0,
            epoch: 0,
            ticket: None,
            deadline,
            welcome: None,
            sent: [u64::MAX; SENT_TICKS],
            sent_at: 0,
            last_tick: 0,
        }
    }

    fn sent_tick(&mut self, tick: u64) {
        if let Some(slot) = self.sent.get_mut(self.sent_at) {
            *slot = tick;
        }
        self.sent_at = (self.sent_at + 1) % SENT_TICKS;
        self.last_tick = self.last_tick.max(tick);
    }

    fn forget_sent(&mut self) {
        self.sent = [u64::MAX; SENT_TICKS];
        self.sent_at = 0;
    }
}

/// The message id of a native message frame, without decoding it.
fn message_id(frame: &[u8]) -> Option<MessageId> {
    match frame {
        [FRAME_MESSAGE, lo, hi, ..] => Some(MessageId(u16::from_le_bytes([*lo, *hi]))),
        _ => None,
    }
}

/// The payload of a native message frame.
fn message_payload(frame: &[u8]) -> &[u8] {
    frame.get(3..).unwrap_or(&[])
}

/// The server tick of a native snapshot frame.
fn snapshot_tick(frame: &[u8]) -> Option<u64> {
    match frame {
        [FRAME_SNAPSHOT, rest @ ..] => rest.first_chunk::<8>().map(|b| u64::from_le_bytes(*b)),
        _ => None,
    }
}

/// A connection event kept past its poll: connected, gone, or a frame
/// needing more than a relay (a `Hello`, a host's session message).
type Event = (ConnectionId, Option<Vec<u8>>, bool);

/// The gateway. See the module documentation.
pub struct Gateway {
    clients: Box<dyn Transport>,
    hosts: Box<dyn Dialer>,
    routes: Box<dyn Routes>,
    tickets: TicketSource,
    cfg: GatewayConfig,
    sessions: BTreeMap<u64, Session>,
    by_client: BTreeMap<ConnectionId, u64>,
    by_host: BTreeMap<ConnectionId, u64>,
    by_ticket: BTreeMap<[u8; 32], u64>,
    retired: std::collections::BTreeSet<[u8; 32]>,
    next_session: u64,
    now: u64,
    /// Frames to send to clients and hosts after a poll's events (a
    /// callback holds the transport it came from).
    scratch: Vec<u8>,
    stamped: Vec<u8>,
    answers: Vec<(u64, Route)>,
    /// Counters.
    pub stats: GatewayStats,
}

/// Tickets retired by a newer one, kept to answer `StaleEpoch`.
const RETIRED_KEPT: usize = 65_536;

impl Gateway {
    /// A gateway accepting clients on `clients`, reaching hosts through
    /// `hosts`, routing tokens with `routes`, and drawing resume tickets
    /// from `tickets`.
    #[must_use]
    pub fn new(
        clients: Box<dyn Transport>,
        hosts: Box<dyn Dialer>,
        routes: Box<dyn Routes>,
        tickets: TicketSource,
        cfg: GatewayConfig,
    ) -> Self {
        Self {
            clients,
            hosts,
            routes,
            tickets,
            cfg,
            sessions: BTreeMap::new(),
            by_client: BTreeMap::new(),
            by_host: BTreeMap::new(),
            by_ticket: BTreeMap::new(),
            retired: std::collections::BTreeSet::new(),
            next_session: 1,
            now: 0,
            scratch: Vec::with_capacity(1024),
            stamped: Vec::with_capacity(2048),
            answers: Vec::with_capacity(64),
            stats: GatewayStats::default(),
        }
    }

    /// Sessions open (playing, joining, or waiting for their client).
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Sessions waiting for their client to come back.
    #[must_use]
    pub fn sessions_away(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.phase == Phase::Live && s.client.is_none())
            .count()
    }

    /// The cell and hand-off epoch of the session of client connection
    /// `conn` (for tests and Ops).
    #[must_use]
    pub fn session_of(&self, conn: ConnectionId) -> Option<(u64, u32)> {
        let s = self.sessions.get(self.by_client.get(&conn)?)?;
        Some((s.cell, s.epoch))
    }

    /// Relays everything received, applies route answers, connects and
    /// hands off sessions, and ends those past their deadlines. `now_ms`
    /// is the gateway's clock (milliseconds, any origin).
    pub fn poll(&mut self, now_ms: u64) {
        self.now = now_ms;
        self.poll_hosts();
        self.poll_clients();
        self.poll_routes();
        self.expire();
    }

    // ---- clients --------------------------------------------------------

    fn poll_clients(&mut self) {
        let mut events: Vec<Event> = Vec::new();
        let Self {
            clients,
            hosts,
            sessions,
            by_client,
            stats,
            ..
        } = self;
        clients.poll(&mut |e| match e {
            TransportEvent::Connected(c) => events.push((c, None, true)),
            TransportEvent::Disconnected { conn, .. } => events.push((conn, None, false)),
            TransportEvent::Frame { conn, channel, bytes } => {
                let Some(s) = by_client.get(&conn).and_then(|id| sessions.get_mut(id)) else {
                    // A client that connected in this poll: its Hello, after
                    // its Connected.
                    events.push((conn, Some(bytes.to_vec()), true));
                    return;
                };
                if s.phase != Phase::Live {
                    // Before Welcome only the Hello matters: handled below.
                    if s.phase == Phase::Hello {
                        events.push((conn, Some(bytes.to_vec()), true));
                    } else {
                        stats.dropped += 1;
                    }
                    return;
                }
                // Live: relay, filtering the gateway's own message and
                // acknowledgements of snapshots this host did not send.
                match message_id(bytes) {
                    Some(id) if id == Linked::ID || id == Hello::ID => {
                        stats.dropped += 1;
                        return;
                    }
                    Some(id) if id == SnapshotAck::ID => {
                        let tick = message_payload(bytes)
                            .first_chunk::<8>()
                            .map(|b| u64::from_le_bytes(*b));
                        if !tick.is_some_and(|t| s.sent.contains(&t)) {
                            stats.dropped += 1;
                            return;
                        }
                    }
                    _ => {}
                }
                let Some(host) = s.host else {
                    stats.dropped += 1;
                    return;
                };
                if hosts.send(host, channel, bytes).is_ok() {
                    stats.up += 1;
                } else {
                    stats.send_errors += 1;
                }
            }
        });
        for (conn, frame, alive) in events {
            match (frame, alive) {
                (None, true) => self.client_connected(conn),
                (None, false) => self.client_gone(conn),
                (Some(bytes), _) => self.client_hello(conn, &bytes),
            }
        }
    }

    fn client_connected(&mut self, conn: ConnectionId) {
        if self.sessions.len() >= self.cfg.capacity {
            self.refuse_client(conn, RefuseReason::Full);
            return;
        }
        let id = self.next_session;
        self.next_session += 1;
        let deadline = self.now.saturating_add(self.cfg.route_timeout_ms);
        self.sessions.insert(id, Session::new(conn, deadline));
        self.by_client.insert(conn, id);
    }

    fn client_hello(&mut self, conn: ConnectionId, frame: &[u8]) {
        let Some(&id) = self.by_client.get(&conn) else {
            return;
        };
        let hello = match message_id(frame) {
            Some(mid) if mid == Hello::ID => match parse_inbound(mid, message_payload(frame)) {
                Ok(Inbound::Hello(h)) if !h.token.is_empty() => h,
                _ => {
                    self.end_session(id, Some(RefuseReason::BadToken));
                    return;
                }
            },
            _ => {
                self.stats.dropped += 1;
                return;
            }
        };
        let token: Vec<u8> = hello.token.iter().copied().collect();
        if let Ok(ticket) = <[u8; 32]>::try_from(token.as_slice()) {
            if let Some(&parked) = self.by_ticket.get(&ticket) {
                self.resume(id, parked, ticket, conn);
                return;
            }
            if self.retired.contains(&ticket) {
                self.end_session(id, Some(RefuseReason::StaleEpoch));
                return;
            }
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            s.hello = frame.to_vec();
            s.phase = Phase::Routing;
            s.deadline = self.now.saturating_add(self.cfg.route_timeout_ms);
        }
        self.routes.begin(id, &token);
    }

    /// The client of the provisional session `id` resumes session `parked`.
    fn resume(&mut self, id: u64, parked: u64, ticket: [u8; 32], conn: ConnectionId) {
        // The provisional session goes; the client joins the parked one.
        self.sessions.remove(&id);
        self.by_ticket.remove(&ticket);
        self.retire(ticket);
        let Some(s) = self.sessions.get_mut(&parked) else {
            self.by_client.remove(&conn);
            self.refuse_client(conn, RefuseReason::BadToken);
            return;
        };
        if s.phase != Phase::Live {
            self.by_client.remove(&conn);
            self.refuse_client(conn, RefuseReason::BadToken);
            return;
        }
        // A connection the client left without the gateway noticing yet is
        // replaced.
        let old = s.client.replace(conn);
        s.ticket = None;
        s.forget_sent();
        s.hold = Hold::Awaiting;
        let host = s.host;
        let welcome = s.welcome.map(|w| Welcome {
            tick: Tick(s.last_tick),
            ..w
        });
        if let Some(old) = old {
            self.by_client.remove(&old);
            self.clients.disconnect(old);
        }
        self.by_client.insert(conn, parked);
        if let Some(host) = host {
            self.send_host(host, &Inbound::Linked(Linked { up: true }));
        }
        if let Some(w) = welcome {
            self.send_client(conn, &Outbound::Welcome(w));
        }
        self.issue_ticket(parked);
        self.stats.resumed += 1;
    }

    fn client_gone(&mut self, conn: ConnectionId) {
        let Some(id) = self.by_client.remove(&conn) else {
            return;
        };
        let live = self.sessions.get(&id).is_some_and(|s| s.phase == Phase::Live);
        if !live {
            self.end_session(id, None);
            return;
        }
        let deadline = self.now.saturating_add(self.cfg.resume_ms);
        let host = self.sessions.get_mut(&id).and_then(|s| {
            s.client = None;
            s.deadline = deadline;
            s.host
        });
        if let Some(host) = host {
            self.send_host(host, &Inbound::Linked(Linked { up: false }));
        }
    }

    // ---- routes ---------------------------------------------------------

    fn poll_routes(&mut self) {
        let mut answers = std::mem::take(&mut self.answers);
        answers.clear();
        self.routes.ready(&mut answers);
        for (id, route) in answers.drain(..) {
            if !self.sessions.get(&id).is_some_and(|s| s.phase == Phase::Routing) {
                continue;
            }
            match route {
                Route::Refuse(reason) => self.end_session(id, Some(reason)),
                Route::Host { cell, address } => match self.hosts.dial(&address) {
                    None => self.end_session(id, Some(RefuseReason::Standby)),
                    Some(host) => {
                        self.by_host.insert(host, id);
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.host = Some(host);
                            s.cell = cell;
                            s.phase = Phase::Dialing;
                            s.deadline = self.now.saturating_add(self.cfg.route_timeout_ms);
                        }
                    }
                },
            }
        }
        self.answers = answers;
    }

    // ---- hosts ----------------------------------------------------------

    fn poll_hosts(&mut self) {
        let mut events: Vec<Event> = Vec::new();
        let Self {
            clients,
            hosts,
            sessions,
            by_host,
            stats,
            stamped,
            ..
        } = self;
        hosts.poll(&mut |e| match e {
            TransportEvent::Connected(c) => events.push((c, None, true)),
            TransportEvent::Disconnected { conn, .. } => events.push((conn, None, false)),
            TransportEvent::Frame { conn, channel, bytes } => {
                let Some(s) = by_host.get(&conn).and_then(|id| sessions.get_mut(id)) else {
                    stats.dropped += 1;
                    return;
                };
                let current = s.host == Some(conn) && s.phase == Phase::Live;
                if !current {
                    // Joining, or the new host of a hand-off: its messages
                    // are read below; its snapshots wait for its Welcome.
                    if message_id(bytes).is_some() {
                        events.push((conn, Some(bytes.to_vec()), true));
                    } else {
                        stats.dropped += 1;
                    }
                    return;
                }
                if let Some(id) = message_id(bytes)
                    && (id == HandOff::ID || id == Welcome::ID || id == Refuse::ID || id == Relinked::ID)
                {
                    events.push((conn, Some(bytes.to_vec()), true));
                    return;
                }
                let Some(client) = s.client else {
                    // The client is away: nothing to relay.
                    return;
                };
                let out: &[u8] = match snapshot_tick(bytes) {
                    Some(tick) => {
                        match s.hold {
                            Hold::Awaiting => {
                                stats.dropped += 1;
                                return;
                            }
                            Hold::From(from) if tick < from => {
                                stats.dropped += 1;
                                return;
                            }
                            Hold::From(_) => s.hold = Hold::None,
                            Hold::None => {}
                        }
                        s.sent_tick(tick);
                        if s.epoch > 0 && stamp_epoch(bytes, s.epoch, stamped) {
                            stats.stamped += 1;
                            stamped
                        } else {
                            bytes
                        }
                    }
                    None => bytes,
                };
                if clients.send(client, channel, out).is_ok() {
                    stats.down += 1;
                } else {
                    stats.send_errors += 1;
                }
            }
        });
        for (conn, frame, alive) in events {
            match (frame, alive) {
                (None, true) => self.host_connected(conn),
                (None, false) => self.host_gone(conn),
                (Some(bytes), _) => self.host_message(conn, &bytes),
            }
        }
    }

    fn host_connected(&mut self, conn: ConnectionId) {
        let Some(&id) = self.by_host.get(&conn) else {
            return;
        };
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        let hello = if s.host == Some(conn) && s.phase == Phase::Dialing {
            s.phase = Phase::Joining;
            s.hello.clone()
        } else if let Some(n) = s.next.as_mut().filter(|n| n.conn == conn) {
            n.connected = true;
            n.hello.clone()
        } else {
            return;
        };
        if self.hosts.send(conn, Channel::Reliable, &hello).is_err() {
            self.stats.send_errors += 1;
        }
    }

    fn host_gone(&mut self, conn: ConnectionId) {
        let Some(id) = self.by_host.remove(&conn) else {
            return;
        };
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        if s.next.as_ref().is_some_and(|n| n.conn == conn) {
            s.next = None;
            let host = s.host;
            self.stats.hand_offs_failed += 1;
            if let Some(host) = host {
                self.send_host(host, &Inbound::Linked(Linked { up: true }));
            }
            return;
        }
        if s.host != Some(conn) {
            return;
        }
        s.host = None;
        // Dialing: the host cannot be reached now.
        let reason = (s.phase == Phase::Dialing).then_some(RefuseReason::Standby);
        self.end_session(id, reason);
    }

    fn host_message(&mut self, conn: ConnectionId, frame: &[u8]) {
        let Some(&id) = self.by_host.get(&conn) else {
            return;
        };
        let Some(mid) = message_id(frame) else {
            return;
        };
        let Ok(msg) = decode_outbound(mid, message_payload(frame)) else {
            self.stats.dropped += 1;
            return;
        };
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        let is_next = s.next.as_ref().is_some_and(|n| n.conn == conn);
        match (is_next, s.phase, msg) {
            (true, _, Outbound::Welcome(w)) => self.complete_hand_off(id, w),
            (true, _, Outbound::Refuse(_)) => {
                s.next = None;
                let host = s.host;
                self.by_host.remove(&conn);
                self.hosts.disconnect(conn);
                self.stats.hand_offs_failed += 1;
                if let Some(host) = host {
                    self.send_host(host, &Inbound::Linked(Linked { up: true }));
                }
            }
            (false, Phase::Joining, Outbound::Welcome(w)) => {
                s.phase = Phase::Live;
                s.welcome = Some(w);
                s.last_tick = w.tick.0;
                let client = s.client;
                self.stats.joined += 1;
                if let Some(c) = client {
                    self.send_client(c, &Outbound::Welcome(w));
                }
                self.issue_ticket(id);
            }
            (false, _, Outbound::Refuse(r)) => {
                if let Some(c) = s.client {
                    self.send_client(c, &Outbound::Refuse(r));
                }
                self.end_session(id, None);
            }
            (false, Phase::Live, Outbound::HandOff(h)) => self.begin_hand_off(id, &h),
            (false, Phase::Live, Outbound::Relinked(r)) => {
                if s.hold == Hold::Awaiting {
                    s.hold = Hold::From(r.tick.0);
                }
            }
            (false, Phase::Live, other) => {
                if let Some(c) = s.client {
                    self.send_client(c, &other);
                }
            }
            // The new host's other messages before its Welcome, and messages
            // before their phase: none matter.
            _ => self.stats.dropped += 1,
        }
    }

    fn begin_hand_off(&mut self, id: u64, h: &HandOff) {
        let Some(s) = self.sessions.get(&id) else {
            return;
        };
        if s.next.is_some() {
            self.stats.hand_offs_failed += 1;
            return;
        }
        // The session's Hello, with the hand-off's token.
        let hello = match message_id(&s.hello).map(|mid| parse_inbound(mid, message_payload(&s.hello))) {
            Some(Ok(Inbound::Hello(mut hello))) => {
                let token: Vec<u8> = h.token.iter().copied().collect();
                hello.token = BoundedArray::from_slice(&token).unwrap_or_default();
                let mut out = Vec::new();
                encode_inbound(&Inbound::Hello(hello), &mut out);
                out
            }
            _ => Vec::new(),
        };
        let host = s.host;
        let dialed = if hello.is_empty() {
            None
        } else {
            self.hosts.dial(h.address.as_str())
        };
        let Some(conn) = dialed else {
            self.stats.hand_offs_failed += 1;
            if let Some(host) = host {
                self.send_host(host, &Inbound::Linked(Linked { up: true }));
            }
            return;
        };
        self.by_host.insert(conn, id);
        if let Some(s) = self.sessions.get_mut(&id) {
            s.next = Some(Next {
                conn,
                cell: h.cell,
                hello,
                connected: false,
            });
        }
    }

    fn complete_hand_off(&mut self, id: u64, w: Welcome) {
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        let Some(next) = s.next.take() else {
            return;
        };
        let old = s.host.replace(next.conn);
        s.cell = next.cell;
        s.epoch = s.epoch.wrapping_add(1).max(1);
        s.welcome = Some(Welcome {
            avatar: w.avatar,
            ..s.welcome.unwrap_or(w)
        });
        s.last_tick = w.tick.0;
        s.forget_sent();
        let (client, cell, epoch) = (s.client, s.cell, s.epoch);
        if let Some(old) = old {
            self.by_host.remove(&old);
            self.hosts.disconnect(old);
        }
        self.stats.handed_off += 1;
        // A client away keeps its ticket: it resumes with it, and is
        // welcomed by the new host's session.
        if let Some(c) = client {
            self.send_client(
                c,
                &Outbound::Transferred(Transferred {
                    cell,
                    epoch,
                    tick: w.tick,
                }),
            );
            self.issue_ticket(id);
        }
    }

    // ---- shared ---------------------------------------------------------

    fn issue_ticket(&mut self, id: u64) {
        let mut ticket = [0u8; 32];
        (self.tickets)(&mut ticket);
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        let old = s.ticket.replace(ticket);
        let client = s.client;
        if let Some(old) = old {
            self.by_ticket.remove(&old);
            self.retire(old);
        }
        self.by_ticket.insert(ticket, id);
        if let Some(c) = client {
            self.send_client(
                c,
                &Outbound::ResumeTicket(ResumeTicket {
                    token: BoundedArray::from_slice(&ticket).unwrap_or_default(),
                    expires_ms: self.cfg.resume_ms,
                }),
            );
        }
    }

    fn retire(&mut self, ticket: [u8; 32]) {
        if self.retired.len() >= RETIRED_KEPT {
            self.retired.pop_first();
        }
        self.retired.insert(ticket);
    }

    /// Ends session `id`: its client is refused with `reason` (when given)
    /// and disconnected, its host connections closed, its ticket retired.
    fn end_session(&mut self, id: u64, reason: Option<RefuseReason>) {
        let Some(s) = self.sessions.remove(&id) else {
            return;
        };
        if let Some(c) = s.client {
            self.by_client.remove(&c);
            match reason {
                Some(r) => self.refuse_client(c, r),
                None => self.clients.disconnect(c),
            }
        }
        for conn in s.host.into_iter().chain(s.next.map(|n| n.conn)) {
            self.by_host.remove(&conn);
            self.hosts.disconnect(conn);
        }
        if let Some(t) = s.ticket {
            self.by_ticket.remove(&t);
            self.retire(t);
        }
    }

    fn refuse_client(&mut self, conn: ConnectionId, reason: RefuseReason) {
        self.stats.refused += 1;
        self.send_client(conn, &Outbound::Refuse(Refuse { reason }));
        self.clients.disconnect(conn);
    }

    fn send_client(&mut self, conn: ConnectionId, msg: &Outbound) {
        self.scratch.clear();
        encode_outbound_frame(msg, &mut self.scratch);
        if self.clients.send(conn, Channel::Reliable, &self.scratch).is_err() {
            self.stats.send_errors += 1;
        }
    }

    fn send_host(&mut self, conn: ConnectionId, msg: &Inbound) {
        self.scratch.clear();
        encode_inbound(msg, &mut self.scratch);
        if self.hosts.send(conn, Channel::Reliable, &self.scratch).is_err() {
            self.stats.send_errors += 1;
        }
    }

    /// Ends sessions past their deadline: a route or connection that took
    /// too long (`Standby`), a client that did not come back in time.
    fn expire(&mut self) {
        let now = self.now;
        let late: Vec<(u64, Phase, bool)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.deadline <= now && (s.phase != Phase::Live || s.client.is_none()))
            .map(|(id, s)| (*id, s.phase, s.client.is_none()))
            .collect();
        for (id, phase, away) in late {
            if phase == Phase::Live && away {
                self.stats.expired += 1;
                self.end_session(id, None);
            } else if phase != Phase::Live {
                self.end_session(id, Some(RefuseReason::Standby));
            }
        }
    }
}

impl core::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gateway")
            .field("sessions", &self.sessions.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// A gateway polled on its own thread on the wall clock, every `period`:
/// what a gateway process runs. Stopped (and joined) when dropped.
pub struct GatewayThread {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stats: std::sync::Arc<std::sync::Mutex<GatewayStats>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GatewayThread {
    /// Runs `gateway` on a new thread. `each` runs after every poll (a
    /// certificate watcher, for one).
    #[must_use]
    pub fn spawn(
        mut gateway: Gateway,
        period: std::time::Duration,
        mut each: impl FnMut(&mut Gateway) + Send + 'static,
    ) -> Self {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stats = std::sync::Arc::new(std::sync::Mutex::new(GatewayStats::default()));
        let (halt, shared) = (std::sync::Arc::clone(&stop), std::sync::Arc::clone(&stats));
        let started = std::time::Instant::now();
        let thread = std::thread::spawn(move || {
            while !halt.load(Ordering::Acquire) {
                let now = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                gateway.poll(now);
                each(&mut gateway);
                *crate::shared::lock(&shared) = gateway.stats;
                std::thread::sleep(period);
            }
        });
        Self {
            stop,
            stats,
            thread: Some(thread),
        }
    }

    /// The gateway's counters as of its last poll.
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        *crate::shared::lock(&self.stats)
    }
}

impl Drop for GatewayThread {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
