//! The whole toy server in-process, driven tick by tick against headless
//! bots over a simulated network: the harness of the budget tests and the
//! soak run. Everything is seeded, so a run replays identically.
//!
//! With [`Sim::enable_gateway`], clients may reach the host through a
//! [`Gateway`] instead ([`Sim::gateway_net`], [`Sim::add_bot_via_gateway`]),
//! and [`Sim::swap_cell_host`] hands every session of a cell to a second
//! host whose ticks start over: the gateway's `Transferred`, its epoch
//! stamps, and resume tickets, as a cluster moving a cell's sessions to
//! another host shows them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mantis_adapter_contract::core_types::{BoundedArray, Vec3, WireString};
use mantis_adapter_contract::{HandOff, TransportKind};
use mantis_core::log::{LogWriter, SessionId};
use mantis_net::gateway::{Gateway, GatewayConfig, Route, Routes};
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_server::cell::{BoxedSink, CellError, TickReport};
use mantis_server::host::{Admission, AdmissionLimits, Host, Verdict};
use mantis_server::intent::CellLogSchema;
use mantis_server::jobs::WorkerSet;
use mantis_server::simnet::{LinkConfig, SimDialer, SimNet};
use mantis_server::zone::Zone;

use crate::tunables::Tunables;
use crate::wire::LegacyWire;
use crate::world;

/// Which adapter a bot speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// The native adapter (Predictive, QUIC-shaped link).
    Native,
    /// The legacy adapter (Validated, TCP-shaped link).
    Legacy,
}

/// One bot and what it is.
pub struct SimBot {
    /// Its adapter.
    pub side: Side,
    /// Its behaviour.
    pub profile: Profile,
    /// The bot.
    pub bot: Bot,
    /// Its connection on its side's network ([`Sim::set_link`]).
    pub conn: mantis_adapter_contract::ConnectionId,
    /// It reaches the host through the gateway.
    pub via_gateway: bool,
}

/// The address the Sim's host answers at behind the gateway.
pub const HOST_A: &str = "sim-host-a";
/// The address of the host [`Sim::swap_cell_host`] moves sessions to.
pub const HOST_B: &str = "sim-host-b";

/// The Sim's gateway ([`Sim::enable_gateway`]).
pub struct SimGateway {
    /// The clients' network: connect here to reach the gateway.
    pub net: SimNet,
    /// The gateway.
    pub gateway: Gateway,
}

/// The second cell host, started by the first [`Sim::swap_cell_host`]: the
/// same world, built afresh (its ticks start over).
pub struct SecondHost {
    /// Its legacy listener's network (unused: hand-offs are native).
    pub legacy_net: SimNet,
    /// The host.
    pub host: Host,
    /// Its zone.
    pub zone: Zone,
}

/// Entry tokens route to the Sim's host.
struct SimRoutes {
    cell: u64,
    ready: Vec<(u64, Route)>,
}

impl Routes for SimRoutes {
    fn begin(&mut self, ticket: u64, _token: &[u8]) {
        self.ready.push((
            ticket,
            Route::Host {
                cell: self.cell,
                address: HOST_A.to_owned(),
            },
        ));
    }

    fn ready(&mut self, out: &mut Vec<(u64, Route)>) {
        out.append(&mut self.ready);
    }
}

/// The second host's admission: hand-off tokens the Sim minted, each for a
/// character at a position, single use.
#[derive(Clone, Default)]
struct Desk(Arc<Mutex<DeskState>>);

#[derive(Default)]
struct DeskState {
    grants: BTreeMap<[u8; 32], (u64, Vec3)>,
    ready: Vec<(u64, Verdict)>,
    next: u64,
}

impl Desk {
    fn state(&self) -> std::sync::MutexGuard<'_, DeskState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mint(&self, character: u64, at: Vec3) -> [u8; 32] {
        let mut d = self.state();
        d.next += 1;
        let mut token = [0x5d; 32];
        token[..8].copy_from_slice(&d.next.to_le_bytes());
        d.grants.insert(token, (character, at));
        token
    }
}

impl Admission for Desk {
    fn begin(&mut self, ticket: u64, token: &[u8]) {
        let mut d = self.state();
        let grant = <[u8; 32]>::try_from(token).ok().and_then(|t| d.grants.remove(&t));
        let verdict = match grant {
            Some((character, at)) => Verdict::Admit {
                character,
                spawn: Some(at),
            },
            None => Verdict::Refuse,
        };
        d.ready.push((ticket, verdict));
    }

    fn ready(&mut self, out: &mut Vec<(u64, Verdict)>) {
        out.append(&mut self.state().ready);
    }
}

/// Seeded resume tickets.
fn ticket_source(seed: u64) -> mantis_net::gateway::TicketSource {
    let mut n = seed;
    Box::new(move |t: &mut [u8; 32]| {
        for chunk in t.chunks_mut(8) {
            // splitmix64
            n = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = n;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            for (slot, b) in chunk.iter_mut().zip(z.to_le_bytes()) {
                *slot = b;
            }
        }
    })
}

/// The server and its bots.
pub struct Sim {
    /// The native listener's network.
    pub native_net: SimNet,
    /// The legacy listener's network.
    pub legacy_net: SimNet,
    /// The host.
    pub host: Host,
    /// The zone.
    pub zone: Zone,
    /// The bots, in the order added.
    pub bots: Vec<SimBot>,
    /// Per-client job workers (none: jobs run inline).
    pub workers: Option<WorkerSet>,
    tunables: Tunables,
    seed: u64,
    ticks: u64,
    ms: u64,
    /// Simulated wall-clock ticks (they pass during a stall; server ticks
    /// do not).
    wall: u64,
    /// Server ticks still to skip ([`Sim::stall_server`]).
    stalled: u64,
    /// Ticks skipped by the stall in progress, told to the cells on resume.
    slipped: u64,
    /// The gateway, once enabled.
    pub gateway: Option<SimGateway>,
    /// The second host, once a cell's sessions moved to it.
    pub second: Option<SecondHost>,
    /// The second host's native network (reachable from the start).
    second_net: SimNet,
    desk: Desk,
    instances: usize,
}

impl Sim {
    /// A server with no bots yet; `log(i)` supplies cell `i`'s log.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn new(
        tunables: Tunables,
        seed: u64,
        log: impl FnMut(usize) -> Option<LogWriter<CellLogSchema, BoxedSink>>,
    ) -> Result<Self, CellError> {
        Self::with_instances(tunables, seed, 0, log)
    }

    /// [`Sim::new`] with `instances` instance cells after the world cells.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn with_instances(
        tunables: Tunables,
        seed: u64,
        instances: usize,
        log: impl FnMut(usize) -> Option<LogWriter<CellLogSchema, BoxedSink>>,
    ) -> Result<Self, CellError> {
        let native_net = SimNet::new(seed ^ 0x6e61_7469_7665);
        let legacy_net = SimNet::new(seed ^ 0x6c65_6761_6379);
        let host = world::host(
            &tunables,
            Box::new(native_net.server(TransportKind::Quic)),
            Box::new(legacy_net.server(TransportKind::Tcp)),
        );
        Ok(Self {
            zone: world::zone_with_instances(&tunables, seed, instances, log)?,
            native_net,
            legacy_net,
            host,
            bots: Vec::new(),
            workers: None,
            tunables,
            seed,
            ticks: 0,
            ms: 0,
            wall: 0,
            stalled: 0,
            slipped: 0,
            gateway: None,
            second: None,
            second_net: SimNet::new(seed ^ 0x7365_636f_6e64),
            desk: Desk::default(),
            instances,
        })
    }

    /// Starts the gateway: clients connecting to [`Sim::gateway_net`] are
    /// routed to this host (any entry token; the host checks it as for a
    /// direct client), and hand-offs reach the second host. The gateway
    /// reaches hosts over perfect links (a data centre).
    pub fn enable_gateway(&mut self) {
        if self.gateway.is_some() {
            return;
        }
        let net = SimNet::new(self.seed ^ 0x0067_6174_6577_6179);
        let mut dialer = SimDialer::new();
        dialer.add_host(HOST_A, &self.native_net, LinkConfig::PERFECT);
        dialer.add_host(HOST_B, &self.second_net, LinkConfig::PERFECT);
        let routes = SimRoutes {
            cell: self.zone.cells().first().map_or(0, |c| c.id().0),
            ready: Vec::new(),
        };
        let gateway = Gateway::new(
            Box::new(net.server(TransportKind::Quic)),
            Box::new(dialer),
            Box::new(routes),
            ticket_source(self.seed ^ 0x7469_636b_6574),
            GatewayConfig::DEFAULT,
        );
        self.gateway = Some(SimGateway { net, gateway });
    }

    /// The network clients connect to the gateway on ([`Sim::enable_gateway`]).
    #[must_use]
    pub fn gateway_net(&self) -> Option<&SimNet> {
        self.gateway.as_ref().map(|g| &g.net)
    }

    /// Simulated time in milliseconds (the gateway's clock).
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.ms
    }

    /// Hands every session of cell `cell` (an index) to the second host,
    /// started on first use with the same world built afresh, so its ticks
    /// start over: each session's avatar continues there where it stands
    /// (a hand-off token the second host redeems), and the gateway switches
    /// its client over (`Transferred`, then snapshots stamped with the new
    /// epoch). Sessions connected directly ignore the hand-off; their
    /// avatars come to rest. Returns how many sessions were handed off.
    ///
    /// # Errors
    /// No gateway, no such cell, or the second host's zone failed to build.
    pub fn swap_cell_host(&mut self, cell: usize) -> Result<usize, String> {
        if self.gateway.is_none() {
            return Err("swap_cell_host needs the gateway (Sim::enable_gateway)".to_owned());
        }
        if self.second.is_none() {
            let legacy_net = SimNet::new(self.seed ^ 0x0073_6563_6f6e_646c);
            let mut host = world::host(
                &self.tunables,
                Box::new(self.second_net.server(TransportKind::Quic)),
                Box::new(legacy_net.server(TransportKind::Tcp)),
            );
            host.set_admission(Box::new(self.desk.clone()), AdmissionLimits::DEFAULT);
            let zone =
                world::zone_with_instances(&self.tunables, self.seed ^ 0x5eed, self.instances, |_| None)
                    .map_err(|e| format!("the second host's zone: {e:?}"))?;
            self.second = Some(SecondHost {
                legacy_net,
                host,
                zone,
            });
        }
        let id = self
            .zone
            .cells()
            .get(cell)
            .map(|c| c.id().0)
            .ok_or_else(|| format!("no cell {cell}"))?;
        let sessions: Vec<SessionId> = self
            .host
            .sessions()
            .into_iter()
            .filter(|s| self.zone.route(*s) == Some(cell))
            .collect();
        let mut moved = 0;
        for s in sessions {
            let Some(c) = self.zone.cells().get(cell) else {
                continue;
            };
            let Some((character, Some(avatar))) = c.session(s).map(|x| (x.character, x.avatar)) else {
                continue;
            };
            let Some(at) = c
                .world()
                .get::<mantis_server::components::Body>(avatar)
                .map(|b| b.0.position)
            else {
                continue;
            };
            let token = self.desk.mint(character, at);
            let to = HandOff {
                cell: id,
                address: WireString::new(HOST_B).unwrap_or_default(),
                token: BoundedArray::from_slice(&token).unwrap_or_default(),
            };
            if self.host.hand_off(s, &to, &mut self.zone) {
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Connects a bot; it logs in on its next step. Returns its index.
    ///
    /// # Errors
    /// The name of an invalid motion parameter.
    pub fn add_bot(&mut self, side: Side, profile: Profile, link: LinkConfig) -> Result<usize, &'static str> {
        self.add_bot_with_token(side, profile, link, None)
    }

    /// [`Sim::add_bot`], presenting `token` at the handshake (an entry
    /// token from the realm, when the host verifies tokens).
    ///
    /// # Errors
    /// The name of an invalid motion parameter.
    pub fn add_bot_with_token(
        &mut self,
        side: Side,
        profile: Profile,
        link: LinkConfig,
        token: Option<&[u8]>,
    ) -> Result<usize, &'static str> {
        let n = self.bots.len() as u64;
        let client = match side {
            Side::Native => self.native_net.connect(link),
            Side::Legacy => self.legacy_net.connect(link),
        };
        let conn = client.id();
        let (wire, transport): (
            Box<dyn mantis_server::bots::BotWire>,
            Box<dyn mantis_adapter_contract::Transport>,
        ) = match side {
            Side::Native => (Box::new(NativeWire::default()), Box::new(client)),
            Side::Legacy => (Box::new(LegacyWire::new(world::LEGACY_BUILD)), Box::new(client)),
        };
        let mut bot = Bot::new(
            wire,
            transport,
            world::ground(),
            bot_config(&self.tunables, self.seed, n, profile),
            mantis_core::math::Vec3::ZERO,
        )?;
        if let Some(token) = token {
            bot = bot.with_token(token);
        }
        bot.start();
        self.bots.push(SimBot {
            side,
            profile,
            bot,
            conn,
            via_gateway: false,
        });
        Ok(self.bots.len() - 1)
    }

    /// Connects a native bot through the gateway ([`Sim::enable_gateway`]).
    /// Returns its index.
    ///
    /// # Errors
    /// No gateway, or the name of an invalid motion parameter.
    pub fn add_bot_via_gateway(&mut self, profile: Profile, link: LinkConfig) -> Result<usize, &'static str> {
        let client = self
            .gateway
            .as_ref()
            .ok_or("no gateway (Sim::enable_gateway)")?
            .net
            .connect(link);
        let n = self.bots.len() as u64;
        let conn = client.id();
        let mut bot = Bot::new(
            Box::new(NativeWire::default()),
            Box::new(client),
            world::ground(),
            bot_config(&self.tunables, self.seed, n, profile),
            mantis_core::math::Vec3::ZERO,
        )?;
        bot.start();
        self.bots.push(SimBot {
            side: Side::Native,
            profile,
            bot,
            conn,
            via_gateway: true,
        });
        Ok(self.bots.len() - 1)
    }

    /// Drops bot `index`'s connection to the gateway and reconnects it over
    /// a new one with `link`, resuming with its latest ticket. False for a
    /// bot not behind the gateway, or without a ticket.
    pub fn resume_bot(&mut self, index: usize, link: LinkConfig) -> bool {
        let Some(net) = self.gateway.as_ref().map(|g| g.net.clone()) else {
            return false;
        };
        let Some(b) = self.bots.get_mut(index).filter(|b| b.via_gateway) else {
            return false;
        };
        b.bot.disconnect();
        let client = net.connect(link);
        let conn = client.id();
        if !b.bot.resume(Box::new(client)) {
            return false;
        }
        b.conn = conn;
        true
    }

    /// Changes bot `index`'s link mid-run, both ways, for frames sent from
    /// now on (a latency step, a loss burst).
    pub fn set_link(&self, index: usize, cfg: LinkConfig) -> bool {
        self.bots.get(index).is_some_and(|b| {
            if b.via_gateway {
                return self.gateway.as_ref().is_some_and(|g| g.net.set_link(b.conn, cfg));
            }
            match b.side {
                Side::Native => self.native_net.set_link(b.conn, cfg),
                Side::Legacy => self.legacy_net.set_link(b.conn, cfg),
            }
        })
    }

    /// Server ticks so far.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// One tick: simulated time advances a tick's worth, the host polls,
    /// the zone steps, and every bot takes one client tick.
    ///
    /// # Errors
    /// [`CellError`].
    pub fn step(&mut self) -> Result<Vec<TickReport>, CellError> {
        let reports = self.step_server()?;
        self.step_bots();
        Ok(reports)
    }

    /// The server half of [`Sim::step`] (for tests that measure it alone).
    ///
    /// # Errors
    /// [`CellError`].
    pub fn step_server(&mut self) -> Result<Vec<TickReport>, CellError> {
        self.wall += 1;
        let target = self.wall * 1000 / u64::from(self.tunables.tick_rate.hz());
        let dt = target.saturating_sub(self.ms);
        self.ms = target;
        self.native_net.advance(dt);
        self.legacy_net.advance(dt);
        self.second_net.advance(dt);
        if let Some(g) = &self.gateway {
            g.net.advance(dt);
        }
        if let Some(g) = &mut self.gateway {
            // Before the host polls: what clients sent reaches it this tick.
            g.gateway.poll(self.ms);
        }
        if self.stalled > 0 {
            // A stalled host: time and the network go on; nothing is read,
            // nothing ticks. Its frames wait in the transports.
            self.stalled -= 1;
            self.slipped += 1;
            return Ok(Vec::new());
        }
        if self.slipped > 0 {
            let slipped = std::mem::take(&mut self.slipped);
            self.host.clock_slipped(slipped);
            self.zone
                .clock_slipped(u32::try_from(slipped).unwrap_or(u32::MAX));
        }
        self.ticks += 1;
        self.host.poll(&mut self.zone);
        let reports = self.zone.step(&mut self.host, self.workers.as_ref())?;
        if let Some(b) = &mut self.second {
            b.host.poll(&mut b.zone);
            b.zone.step(&mut b.host, self.workers.as_ref())?;
        }
        Ok(reports)
    }

    /// The test hook for a stalled cell host (a GC-like pause, a slow
    /// disk): the next `ticks` server steps pass wall-clock time and the
    /// network but neither poll nor tick. On resume the host ticks once per
    /// step again, as `serve` does after an overrun: it re-anchors and never
    /// bursts to catch up (docs/SERVER.md section 2), so the cell's tick
    /// count stays `ticks` behind wall time.
    pub fn stall_server(&mut self, ticks: u64) {
        self.stalled += ticks;
    }

    /// True while a stall is in progress.
    #[must_use]
    pub fn stalling(&self) -> bool {
        self.stalled > 0
    }

    /// The bot half of [`Sim::step`].
    pub fn step_bots(&mut self) {
        for b in &mut self.bots {
            b.bot.step();
        }
    }
}

/// The settings of bot `n` of a run seeded `seed`: the package's motion and
/// tick rate, its own walk seed, and its own clock offset (every client
/// clock is somewhere else).
#[must_use]
pub fn bot_config(t: &Tunables, seed: u64, n: u64, profile: Profile) -> BotConfig {
    BotConfig {
        profile,
        motion: t.motion,
        rate: t.tick_rate,
        seed: seed.wrapping_mul(1_000_003).wrapping_add(n),
        content: t.content,
        clock_offset_ms: u32::try_from((n * 7919) % 100_000).unwrap_or(0),
    }
}
