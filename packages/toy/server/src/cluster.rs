//! The zone's link to the service roles: after every tick, each cell's
//! module outcomes (commands and systems) go to the persistence writer,
//! module messages for service roles go out (cross-cell lines to social,
//! never logged), lines social delivered come in as logged intents, the
//! characters each cell hosts are reported to social, verified live changes from Ops are queued into every cell (applied at
//! the next tick boundary, logged), and cell summaries are published for
//! the read-only inspector.

use mantis_adapter_contract::core_types::WireString;
use mantis_core::log::SessionId;
use mantis_core::social::{FRIEND_OP, GUILD_OP, PARTY_OP};
use mantis_server::cell::TickReport;
use mantis_server::host::{Admission, Host, Verdict};
use mantis_server::intent::CellIntent;
use mantis_server::modules::Payload;
use mantis_server::service::{SOCIAL_DELIVER, SOCIAL_PUBLISH, SocialLine};
use mantis_server::session::Sessions;
use mantis_server::zone::Zone;
use mantis_services::cluster::{CellLink, CellOutcome, OpsOrder, TokenVerifier};
use mantis_services::generated::services as m;
use mantis_services::inspect;

use crate::world;

/// Summaries are published this often, in ticks.
pub const SUMMARY_EVERY: u64 = 30;

/// What one round after a tick moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moved {
    /// Outcomes handed to the writer.
    pub outcomes: usize,
    /// Live changes queued into cells.
    pub live: usize,
    /// Lines published to social.
    pub published: usize,
    /// Social lines queued into cells.
    pub delivered: usize,
    /// Characters relocated into instance cells.
    pub placed: usize,
    /// Party and friends operations relayed to social.
    pub relayed: usize,
    /// Party and friends updates queued into cells.
    pub projected: usize,
    /// Instance cells released to the realm.
    pub released: usize,
}

/// Runs after every zone tick: outcomes out, live changes in, summaries.
pub fn after_tick(zone: &mut Zone, link: &CellLink, reports: &[TickReport]) -> Moved {
    let mut moved = Moved::default();
    for (i, report) in reports.iter().enumerate() {
        let Some(cell) = zone.cell_mut(i) else {
            continue;
        };
        let id = cell.id().0;
        let mut batch = Vec::new();
        cell.drain_outcomes(|tick, o| {
            let bytes = o.payload.as_slice();
            let mut payload = [0u8; 512];
            let len = bytes.len().min(payload.len());
            for (to, from) in payload.iter_mut().zip(bytes) {
                *to = *from;
            }
            batch.push(CellOutcome {
                // The tick it was made at: one a recovery replay made is
                // already durable under that tick's batch numbers.
                tick: tick.0,
                kind: o.kind.0,
                session: o.session.map_or(0, |s| s.0),
                ok: o.result.is_ok(),
                payload,
                len,
            });
        });
        moved.outcomes += batch.len();
        link.push(id, batch);
        // Where each character is: the realm hears where characters leave
        // the world and arrive, to place a returning character where it
        // left.
        link.track(id, &whereabouts(cell.world()));
        // Messages for service roles: outputs, never logged.
        cell.service_messages(|topic, payload| match topic {
            SOCIAL_PUBLISH => {
                if let Ok(l) = SocialLine::parse(payload.as_slice()) {
                    link.publish(l.channel, l.from, l.to, l.text.as_str());
                    moved.published += 1;
                }
            }
            PARTY_OP | FRIEND_OP | GUILD_OP => {
                link.relay(id, topic, payload.as_slice());
                moved.relayed += 1;
            }
            _ => {}
        });
        if report.tick.0.is_multiple_of(SUMMARY_EVERY) {
            let world = cell.world();
            let hosted: Vec<u64> = world
                .resource::<Sessions>()
                .map(|s| s.map.values().map(|x| x.character).collect())
                .unwrap_or_default();
            link.presence(id, &hosted);
            link.poll_matches(&hosted);
            publish_inspection(cell, link);
            link.summary(m::CellSummary {
                cell: m::CellNo(id),
                tick: report.tick.0,
                state_hash: report.state_hash,
                sessions: world
                    .resource::<Sessions>()
                    .map_or(0, |s| u32::try_from(s.map.len()).unwrap_or(u32::MAX)),
                entities: world.components.len(),
                cheats: world.resource::<Sessions>().map_or(0, |s| {
                    s.map.values().fold(0u32, |n, x| n.saturating_add(x.cheats))
                }),
            });
        }
    }
    // Instance cells empty for the package's grace go back to the realm.
    for i in zone.take_released() {
        if let Some(cell) = zone.cells().get(i) {
            link.release_instance(cell.id().0);
            moved.released += 1;
        }
    }
    inbound(zone, link, &mut moved);
    answer_inspector(zone, link);
    moved
}

/// The characters a cell holds and where their avatars are.
fn whereabouts(world: &mantis_core::ecs::World) -> Vec<(u64, [f32; 3])> {
    world.resource::<Sessions>().map_or_else(Vec::new, |s| {
        s.map
            .values()
            .filter_map(|x| {
                let at = world
                    .components
                    .get::<mantis_server::components::Body>(x.avatar?)?
                    .0
                    .position;
                Some((x.character, [at.x, at.y, at.z]))
            })
            .collect()
    })
}

/// Publishes a cell's run times and component names for the Ops inspector.
fn publish_inspection(cell: &mantis_server::cell::Cell, link: &CellLink) {
    let t = cell.timings();
    let times = inspect::system_times(
        cell.id().0,
        t.tick.0,
        t.systems
            .iter()
            .map(|(name, phase, timing)| (*name, *phase, *timing)),
        t.inbox_depth,
        t.encode,
    );
    let names = cell.component_names().into_iter().map(str::to_owned).collect();
    link.publish_inspection(times, names);
}

/// Answers the entity pages the Ops inspector asked for, between ticks.
fn answer_inspector(zone: &Zone, link: &CellLink) {
    for q in link.entity_queries() {
        let Some(cell) = zone.cells().iter().find(|c| c.id().0 == q.cell) else {
            continue;
        };
        let page = cell
            .inspect_entities(&q.component, q.offset, q.limit)
            .map(|p| inspect::entity_page(q.cell, cell.tick_now().0, &q.component, &p));
        q.answer(page);
    }
}

/// The OS monotonic clock, for the inspector's run times (diagnostics; the
/// simulation never reads it).
#[derive(Debug)]
pub struct OsStopwatch(std::time::Instant);

impl OsStopwatch {
    /// Starts now.
    #[must_use]
    pub fn new() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Default for OsStopwatch {
    fn default() -> Self {
        Self::new()
    }
}

impl mantis_core::schedule::Stopwatch for OsStopwatch {
    fn now_nanos(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Times every cell of `zone` with one OS stopwatch (the Ops inspector).
pub fn time_cells(zone: &mut Zone) {
    let stopwatch: std::sync::Arc<dyn mantis_core::schedule::Stopwatch> =
        std::sync::Arc::new(OsStopwatch::new());
    for i in 0..zone.cells().len() {
        if let Some(cell) = zone.cell_mut(i) {
            cell.set_stopwatch(std::sync::Arc::clone(&stopwatch));
        }
    }
}

/// What the services sent back, into the cells as logged intents.
fn inbound(zone: &mut Zone, link: &CellLink, moved: &mut Moved) {
    // Placements in an instance cell of this zone: the character is
    // relocated there (a logged intent in the cell hosting it; the zone
    // then transfers the avatar).
    for p in link.placements() {
        let world_cells = world::regions().len();
        let Some(k) = zone
            .cells()
            .iter()
            .position(|c| c.id().0 == p.cell)
            .and_then(|i| i.checked_sub(world_cells))
        else {
            continue;
        };
        // The instance's client module tier first: the cell applies it at
        // its next tick, before the transferred avatar arrives.
        if let Some(cell) = zone.cell_mut(world_cells + k) {
            cell.inbox().push(
                SessionId(0),
                CellIntent::SetModTier {
                    tier: world::queue_tier(p.queue),
                },
            );
        }
        let at = world::instance_spawn(k, p.character);
        if relocate(zone, p.character, at) {
            moved.placed += 1;
        }
    }
    // Party and friends updates enter their cell as logged intents.
    for p in link.projections() {
        let Some(payload) = Payload::from_slice(&p.payload) else {
            continue;
        };
        let index = zone.cells().iter().position(|c| c.id().0 == p.cell);
        if let Some(cell) = index.and_then(|i| zone.cell_mut(i)) {
            let intent = CellIntent::ServiceUpdate {
                topic: p.topic,
                payload,
            };
            if cell.inbox().push(SessionId(0), intent) {
                moved.projected += 1;
            }
        }
    }
    // Lines social delivered here enter their cell as logged intents.
    for d in link.social_deliveries() {
        let Some(text) = WireString::new(&d.text) else {
            continue;
        };
        let social_line = SocialLine {
            channel: d.channel,
            from: d.from,
            to: d.to,
            text,
        };
        let index = zone.cells().iter().position(|c| c.id().0 == d.cell);
        if let Some(cell) = index.and_then(|i| zone.cell_mut(i)) {
            let intent = CellIntent::ServiceUpdate {
                topic: SOCIAL_DELIVER,
                payload: social_line.payload(),
            };
            if cell.inbox().push(SessionId(0), intent) {
                moved.delivered += 1;
            }
        }
    }
    for change in link.live_changes() {
        let Some(name) = WireString::new(&change.name) else {
            continue;
        };
        for i in 0..zone.cells().len() {
            if let Some(cell) = zone.cell_mut(i) {
                let intent = CellIntent::SetLive {
                    name,
                    kind: change.kind,
                    value: change.value,
                };
                if cell.inbox().push(SessionId(0), intent) {
                    moved.live += 1;
                }
            }
        }
    }
}

/// Applies Ops orders (kick, maintenance drain) to the host, once per tick
/// after the zone stepped.
#[derive(Debug, Default)]
pub struct OpsApplier {
    ticks: u64,
    drain_at: Option<u64>,
}

impl OpsApplier {
    /// Ticks so far.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Applies orders received since the last tick, and ends the remaining
    /// sessions when a drain's grace period is over. Returns sessions ended.
    pub fn apply(&mut self, host: &mut Host, zone: &mut Zone, link: &CellLink, tick_rate: u32) -> usize {
        self.ticks += 1;
        let mut ended = 0;
        for order in link.ops_orders() {
            match order {
                OpsOrder::Kick { character } => {
                    if let Some(session) = session_of(zone, character)
                        && host.kick(session, zone)
                    {
                        ended += 1;
                    }
                }
                OpsOrder::Drain { on, grace_ms } => {
                    host.set_maintenance(on);
                    // The realm reclaims this host's instance cells while it
                    // drains, and is offered them again after.
                    link.withdraw_instances(on);
                    self.drain_at =
                        on.then(|| self.ticks + u64::from(grace_ms) * u64::from(tick_rate) / 1000);
                }
            }
        }
        if self.drain_at.is_some_and(|at| self.ticks >= at) {
            self.drain_at = None;
            for session in host.sessions() {
                if host.kick(session, zone) {
                    ended += 1;
                }
            }
        }
        ended
    }
}

fn session_of(zone: &Zone, character: u64) -> Option<SessionId> {
    zone.cells().iter().find_map(|c| {
        c.world()
            .resource::<Sessions>()
            .and_then(|s| s.map.values().find(|x| x.character == character).map(|x| x.id))
    })
}

/// Moves `character` to `at` through the cell hosting it (a logged
/// intent); the zone transfers the avatar when `at` is another cell's.
/// False when no cell hosts the character.
pub fn relocate(zone: &mut Zone, character: u64, at: mantis_core::math::Vec3) -> bool {
    let index = zone.cells().iter().position(|c| {
        c.world()
            .resource::<Sessions>()
            .is_some_and(|s| s.map.values().any(|x| x.character == character))
    });
    index.and_then(|i| zone.cell_mut(i)).is_some_and(|c| {
        c.inbox().push(
            SessionId(0),
            CellIntent::Relocate {
                character,
                position: at,
            },
        )
    })
}

/// Admission through the realm: tokens are redeemed in the background and
/// verdicts collected at the next host poll.
pub struct RealmAdmission(pub TokenVerifier);

impl Admission for RealmAdmission {
    fn begin(&mut self, ticket: u64, token: &[u8]) {
        self.0.begin(ticket, token);
    }

    fn ready(&mut self, out: &mut Vec<(u64, Verdict)>) {
        out.extend(self.0.ready().into_iter().map(|(ticket, admitted)| {
            (
                ticket,
                admitted.map_or(Verdict::Refuse, |a| Verdict::Admit {
                    character: a.character,
                    spawn: a.spawn.map(|[x, y, z]| mantis_core::math::Vec3::new(x, y, z)),
                }),
            )
        }));
    }
}

/// What [`start_local`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalOptions {
    /// The zone's seed.
    pub seed: u64,
    /// Honest simulated clients that join (so the cells do real work).
    pub bots: usize,
}

/// A headless toy cluster in this process: every service role, the Ops
/// dashboard on loopback, and the zone ticking on its own thread at the
/// tunables' rate with the inspector's stopwatch on. Dropping it stops the
/// zone, then the cluster.
pub struct LocalWorld {
    /// The Ops dashboard (HTTPS, loopback, ephemeral port).
    pub ops_addr: std::net::SocketAddr,
    /// The dashboard's development certificate (DER), to pin.
    pub ops_cert_der: Vec<u8>,
    /// An operator token the dashboard accepts (Bearer).
    pub token: String,
    /// The live cells, world cells first.
    pub cells: Vec<u64>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    zone: Option<std::thread::JoinHandle<Result<(), String>>>,
    cluster: Option<mantis_services::cluster::LocalCluster>,
}

impl LocalWorld {
    /// Stops the zone thread and reports how it ended.
    ///
    /// # Errors
    /// Why the zone stopped early.
    pub fn stop(mut self) -> Result<(), String> {
        self.halt()
    }

    fn halt(&mut self) -> Result<(), String> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let ended = match self.zone.take() {
            Some(t) => t.join().map_err(|_| "the zone thread panicked".to_owned())?,
            None => Ok(()),
        };
        self.cluster = None;
        ended
    }
}

impl Drop for LocalWorld {
    fn drop(&mut self) {
        let _ = self.halt();
    }
}

/// Starts a [`LocalWorld`].
///
/// # Errors
/// Why a role, the dashboard, or the zone could not start.
pub fn start_local(opts: &LocalOptions) -> Result<LocalWorld, String> {
    use mantis_services::cluster::{CellLinkConfig, ClusterConfig, LocalCluster, operator_token};
    use mantis_services::host::Role;

    let token = operator_token()?;
    let mut config = ClusterConfig::local();
    config
        .dashboard
        .operators
        .insert(token.clone(), "local".to_owned());
    let cluster = LocalCluster::start(&config)?;
    let ops_addr = cluster.dashboard_addr().ok_or("no dashboard")?;
    let addr = |r: Role| cluster.addr(r).ok_or_else(|| format!("no {r:?} role"));
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: mantis_services::host::rpc::Endpoint::fixed(addr(Role::Persist)?),
            ops: mantis_services::host::rpc::Endpoint::fixed(addr(Role::Ops)?),
            social: mantis_services::host::rpc::Endpoint::fixed(addr(Role::Social)?),
            matchmaking: mantis_services::host::rpc::Endpoint::fixed(addr(Role::Matchmaking)?),
            realm: mantis_services::host::rpc::Endpoint::fixed(addr(Role::Realm)?),
            world: 0,
            live_key: cluster.ops().public_key(),
            cells: world::regions()
                .into_iter()
                .zip(1u64..)
                .map(|(r, id)| (id, "127.0.0.1:7400".to_owned(), r))
                .collect(),
            poll: std::time::Duration::from_millis(20),
            instances: Vec::new(),
            inspector: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            tls: None,
        },
    )?;
    let inspector = link.inspector().ok_or("no inspector")?;
    let cells: Vec<u64> = (1..=world::regions().len() as u64).collect();
    for c in &cells {
        cluster.add_cell(*c, inspector);
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let halt = std::sync::Arc::clone(&stop);
    let (seed, bots) = (opts.seed, opts.bots);
    let zone = std::thread::spawn(move || run_local(&link, seed, bots, &halt));
    Ok(LocalWorld {
        ops_addr,
        ops_cert_der: cluster.dashboard_cert.clone(),
        token,
        cells,
        stop,
        zone: Some(zone),
        cluster: Some(cluster),
    })
}

fn run_local(
    link: &CellLink,
    seed: u64,
    bots: usize,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    use mantis_server::bots::Profile;
    use mantis_server::simnet::LinkConfig;

    let t = crate::tunables::Tunables::defaults().map_err(|e| e.to_string())?;
    let mut sim = crate::sim::Sim::new(t, seed, |_| None).map_err(|e| format!("{e:?}"))?;
    time_cells(&mut sim.zone);
    for k in 0..bots {
        let side = if k % 2 == 0 {
            crate::sim::Side::Native
        } else {
            crate::sim::Side::Legacy
        };
        sim.add_bot(side, Profile::Honest, LinkConfig::PERFECT)?;
    }
    let period = std::time::Duration::from_secs(1) / t.tick_rate.hz();
    let mut next = std::time::Instant::now();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let reports = sim.step().map_err(|e| format!("{e:?}"))?;
        after_tick(&mut sim.zone, link, &reports);
        next += period;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}

/// Paces a serving loop: sleeps until the next tick is due. After an
/// overrun (a slow tick, a stall) it re-anchors to now and never bursts to
/// catch up (docs/SERVER.md section 2), and tells the host and every cell
/// how many ticks of wall time passed without them.
pub fn pace(
    next: &mut std::time::Instant,
    period: std::time::Duration,
    host: &mut mantis_server::host::Host,
    zone: &mut Zone,
) {
    *next += period;
    let now = std::time::Instant::now();
    if *next > now {
        std::thread::sleep(*next - now);
        return;
    }
    let slipped = (now - *next).as_nanos() / period.as_nanos().max(1);
    if slipped > 0 {
        host.clock_slipped(u64::try_from(slipped).unwrap_or(u64::MAX));
        zone.clock_slipped(u32::try_from(slipped).unwrap_or(u32::MAX));
    }
    *next = now;
}
