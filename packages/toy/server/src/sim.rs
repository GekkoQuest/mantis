//! The whole toy server in-process, driven tick by tick against headless
//! bots over a simulated network: the harness of the budget tests and the
//! soak run. Everything is seeded, so a run replays identically.

use mantis_adapter_contract::TransportKind;
use mantis_core::log::LogWriter;
use mantis_server::bots::{Bot, BotConfig, NativeWire, Profile};
use mantis_server::cell::{BoxedSink, CellError, TickReport};
use mantis_server::host::Host;
use mantis_server::intent::CellLogSchema;
use mantis_server::jobs::WorkerSet;
use mantis_server::simnet::{LinkConfig, SimNet};
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
        })
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
        let (wire, transport): (
            Box<dyn mantis_server::bots::BotWire>,
            Box<dyn mantis_adapter_contract::Transport>,
        ) = match side {
            Side::Native => (
                Box::new(NativeWire::default()),
                Box::new(self.native_net.connect(link)),
            ),
            Side::Legacy => (
                Box::new(LegacyWire::new(world::LEGACY_BUILD)),
                Box::new(self.legacy_net.connect(link)),
            ),
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
        self.bots.push(SimBot { side, profile, bot });
        Ok(self.bots.len() - 1)
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
        self.ticks += 1;
        let target = self.ticks * 1000 / u64::from(self.tunables.tick_rate.hz());
        let dt = target.saturating_sub(self.ms);
        self.ms = target;
        self.native_net.advance(dt);
        self.legacy_net.advance(dt);
        self.host.poll(&mut self.zone);
        self.zone.step(&mut self.host, self.workers.as_ref())
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
