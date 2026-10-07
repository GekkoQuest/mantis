//! The real toy client behind the gateway (server-engine's `Sim::enable_gateway`), at
//! 100 ms RTT and 2 % loss, moving all the while, with bots in view:
//!
//! 1. **Cell host swap mid-play** (`Sim::swap_cell_host`): the client follows the
//!    hand-off on its connection (no new handshake). The new host's ticks start over and
//!    it names the avatar afresh; the client adopts it without a reset.
//! 2. **A dropped connection**: the client reconnects through the gateway with its resume
//!    ticket (`toy_client::gateway::Redial`) and plays on, with no reset.
//! 3. **A stale ticket**: a connection that presents the ticket the resume used is
//!    refused `StaleEpoch`; the client treats it as a reconnect and comes back with its
//!    newest ticket, never a dead client.
//!
//! Throughout: the Predictive correction p99 row (< 0.10 m), no undecodable frame, and
//! the local avatar reset only once (at spawn). Prints `budget:` and `MANTIS-METRIC` lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use mantis_adapter_contract::{
    ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent, TransportKind,
};
use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::net::SessionState;
use mantis_client::reconnect::ReconnectStatus;
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_core::time::Tick;
use mantis_server::bots::Profile;
use mantis_server::simnet::{LinkConfig, SimClient, SimNet};
use toy_client::{HeadlessSink, ToyClient};
use toy_server::sim::Sim;
use toy_server::tunables::Tunables;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SEED: u64 = 4243;
const LINK: LinkConfig = LinkConfig::RTT100_LOSS2;
/// The tick of the swap, the drop, and the stale-ticket connection.
const SWAP_AT: u64 = 600;
const DROP_AT: u64 = 1100;
const STALE_AT: u64 = 1500;
const END: u64 = 2100;

/// A simulated connection that can be lost. QUIC reports a lost connection to its own
/// side; the simulated network tells only the server, so this says it to the client.
struct Droppable {
    inner: SimClient,
    lost: Arc<AtomicBool>,
    reported: bool,
}

impl Transport for Droppable {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        if !self.lost.load(Ordering::Acquire) {
            self.inner.poll(sink);
        } else if !self.reported {
            self.reported = true;
            self.inner.disconnect(ConnectionId(0));
            sink(TransportEvent::Disconnected {
                conn: ConnectionId(0),
                reason: DisconnectReason::TimedOut,
            });
        }
    }
    fn send(
        &mut self,
        conn: ConnectionId,
        channel: mantis_adapter_contract::Channel,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        if self.lost.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        self.inner.send(conn, channel, bytes)
    }
    fn disconnect(&mut self, conn: ConnectionId) {
        self.lost.store(true, Ordering::Release);
        self.inner.disconnect(conn);
    }
    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }
    fn max_unreliable_payload(&self) -> usize {
        self.inner.max_unreliable_payload()
    }
}

/// Connects to the gateway; `current` holds the newest connection's loss switch.
fn dial(net: &SimNet, current: &Mutex<Arc<AtomicBool>>) -> Droppable {
    let lost = Arc::new(AtomicBool::new(false));
    *current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&lost);
    Droppable {
        inner: net.connect(LINK),
        lost,
        reported: false,
    }
}

/// Every cell (index) that serves a session of the first host.
fn occupied_cells(sim: &Sim) -> Vec<usize> {
    let mut cells: Vec<usize> = sim
        .host
        .sessions()
        .into_iter()
        .filter_map(|s| sim.zone.route(s))
        .collect();
    cells.sort_unstable();
    cells.dedup();
    cells
}

type Client = ToyClient<Droppable, HeadlessSink>;

fn key(c: &Client, k: KeyCode, pressed: bool) -> TestResult {
    c.events
        .as_ref()
        .ok_or("no event channel")?
        .send(PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(k),
            pressed,
        }))?;
    Ok(())
}

/// Moves in a loop (the correction row's pattern).
fn steer(c: &Client, k: u64) -> TestResult {
    match k % 120 {
        0 => key(c, KeyCode::W, true),
        10 => key(c, KeyCode::A, true),
        30 => key(c, KeyCode::A, false),
        40 => key(c, KeyCode::W, false),
        60 => key(c, KeyCode::S, true),
        70 => key(c, KeyCode::D, true),
        90 => key(c, KeyCode::D, false),
        100 => key(c, KeyCode::S, false),
        _ => Ok(()),
    }
}

#[test]
#[expect(clippy::too_many_lines)] // One session: play, swap, drop, stale ticket, play on.
fn the_client_follows_a_cell_host_swap_and_resumes_after_a_drop_without_a_correction() -> TestResult {
    let t = Tunables::defaults()?;
    let rate = t.tick_rate;
    let mut sim = Sim::new(t, SEED, |_| None).map_err(|e| format!("{e:?}"))?;
    sim.enable_gateway();
    for _ in 0..4 {
        let _ = sim.add_bot_via_gateway(Profile::Honest, LINK)?;
    }
    let net = sim.gateway_net().ok_or("no gateway")?.clone();
    let current = Arc::new(Mutex::new(Arc::new(AtomicBool::new(false))));
    let clock = Arc::new(ManualClock::new());
    let mut client = ToyClient::new(
        dial(&net, &current),
        Arc::clone(&clock) as Arc<dyn HostClock>,
        HeadlessSink,
    )?;
    {
        let (net, current) = (net.clone(), Arc::clone(&current));
        client.enable_reconnect(b"swap", Box::new(move || Ok(dial(&net, &current))));
    }
    client.start(b"swap");

    let mut moving = false;
    let mut handed_off = 0;
    let mut swapped = false;
    let mut swap_placed = None;
    let mut used_ticket: Option<Vec<u8>> = None;
    let mut drop_live = None;
    let mut stale_seen = false;
    let mut stale_live = None;
    for k in 1..=END {
        let _ = sim.step_server().map_err(|e| format!("{e:?}"))?;
        clock.set(HostInstant::from_nanos(u64::try_from(tick_start_nanos(
            rate,
            Tick(k),
        ))?));
        if !moving && matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. }) {
            moving = true;
        }
        if moving {
            steer(&client, k)?;
        }
        match k {
            SWAP_AT => {
                assert!(moving, "playing before the swap");
                for cell in occupied_cells(&sim) {
                    handed_off += sim.swap_cell_host(cell)?;
                }
                swapped = true;
            }
            DROP_AT => {
                used_ticket = client.net.resume_ticket().map(|t| t.token().to_vec());
                current
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .store(true, Ordering::Release);
            }
            STALE_AT => {
                // A connection presenting the ticket the resume already used (another copy of
                // the client, or a ticket replaced by a hand-off): the old connection closes,
                // the gateway refuses the ticket as stale, and the client comes back with its
                // newest ticket.
                let stale = used_ticket.clone().ok_or("no ticket before the drop")?;
                client.net.transport_mut().disconnect(ConnectionId(0));
                let fresh = dial(&net, &current);
                client.net.reconnect(fresh, &stale);
            }
            _ => {}
        }
        client.step(2);
        sim.step_bots();
        let stats = client.sim.handler().stats();
        if swapped && swap_placed.is_none() && stats.rebases > 0 {
            swap_placed = Some(k - SWAP_AT);
        }
        let live = client.redial.as_ref().map(toy_client::gateway::Redial::current);
        if k > DROP_AT
            && drop_live.is_none()
            && k < STALE_AT
            && live == Some(ReconnectStatus::Live)
            && client.net.stats().reconnects > 0
        {
            drop_live = Some(k - DROP_AT);
        }
        if k > STALE_AT {
            stale_seen |= client.net.state()
                == SessionState::Refused(mantis_adapter_contract::RefuseReason::StaleEpoch)
                || matches!(live, Some(ReconnectStatus::Reconnecting { .. }));
            if stale_live.is_none()
                && stale_seen
                && live == Some(ReconnectStatus::Live)
                && matches!(client.net.state(), SessionState::Welcomed { .. })
            {
                stale_live = Some(k - STALE_AT);
            }
        }
        assert!(
            !matches!(live, Some(ReconnectStatus::Failed(_))),
            "tick {k}: the client gave up: {live:?}"
        );
    }

    let sim_stats = client.sim.handler().stats();
    let net_stats = client.net.stats();
    let prediction = client.sim.handler().predictor().stats();
    let p99 = client
        .sim
        .handler()
        .corrections()
        .quantile(0.99)
        .ok_or("no reconciliations")?;
    let gw = sim.gateway.as_ref().ok_or("no gateway")?.gateway.stats;
    let swap_placed = swap_placed.ok_or("no frame of the new host was applied")?;
    let drop_live = drop_live.ok_or("not live again after the drop")?;
    let stale_live = stale_live.ok_or("not live again after the stale ticket")?;
    println!(
        "budget: gateway swap: correction p99 = {p99} m at 100 ms RTT, 2% loss (target < 0.10); placed on the new host {swap_placed} ticks after the swap; live {drop_live} ticks after a drop and {stale_live} ticks after a stale-ticket refusal; {} snapshots, {} undecodable, {} old-host snapshots dropped by epoch",
        sim_stats.snapshots, net_stats.undecodable, net_stats.stale_epoch
    );
    println!(
        "MANTIS-METRIC gateway_swap correction_p99_m={p99} placed_ticks={swap_placed} drop_live_ticks={drop_live} stale_live_ticks={stale_live} snapshots={} undecodable={} stale_epoch={} resets={} corrected={} avatars_adopted={}",
        sim_stats.snapshots,
        net_stats.undecodable,
        net_stats.stale_epoch,
        sim_stats.resets,
        prediction.corrected,
        sim_stats.avatars_adopted
    );

    // The swap: a hand-off on the same connection, and no reset (the avatar keeps its
    // replication id; a renamed one would be adopted, `avatars_adopted`).
    assert!(
        handed_off >= 1,
        "the client's cell was swapped ({handed_off} sessions)"
    );
    assert!(gw.handed_off >= 1, "{gw:?}");
    assert!(
        net_stats.rebases >= 1,
        "Transferred on the connection: {net_stats:?}"
    );
    // The drop and the stale ticket: two reconnects, each resumed.
    assert!(
        stale_seen,
        "the stale ticket was refused and surfaced as a reconnect"
    );
    assert_eq!(
        client
            .redial
            .as_ref()
            .map(toy_client::gateway::Redial::reconnects),
        Some(2)
    );
    assert_eq!(sim_stats.reconnects, 2, "{sim_stats:?}");
    assert_eq!(prediction.resumed, 2, "{prediction:?}");
    assert!(gw.resumed >= 2, "{gw:?}");
    // The rows.
    assert_eq!(sim_stats.resets, 1, "reset only at spawn: {sim_stats:?}");
    assert_eq!(net_stats.undecodable, 0, "every frame decoded: {net_stats:?}");
    assert!(p99 < 0.10, "correction p99 {p99} m");
    assert!(sim_stats.snapshots > 1500, "{sim_stats:?}");
    Ok(())
}
