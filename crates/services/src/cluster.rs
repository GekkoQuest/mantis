//! Running the roles: every service role in one process for local
//! development ([`LocalCluster`]), and the link a cell host keeps to them
//! ([`CellLink`]).
//!
//! The cell host's side is synchronous and never blocks its tick: outcomes
//! are queued to a background task that pushes them to the persistence
//! writer in order, with per-cell batch sequence numbers (a resent batch
//! is a no-op there, so retries are safe); live changes are polled in the
//! background, verified with the live-data key, and handed over as
//! [`Verified`] changes the host queues into its cells; cell summaries are
//! published for the read-only inspector Ops calls.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mantis_core::social::{
    FRIEND_OP, FRIEND_UPDATE, FriendOp, FriendUpdate, PARTY_OP, PARTY_UPDATE, PartyOp, PartyUpdate,
};
use mantis_core::wire::{BoundedArray, WireString, decode_exact};
use ring::rand::{SecureRandom, SystemRandom};

use crate::account::AccountService;
use crate::generated::services as m;
use crate::host::rpc::{Router, RpcClient, RpcError, RpcServer};
use crate::host::{RPC_TIMEOUT, Role};
use crate::host::{lock, now_ms};
use crate::inspect::{EntityQuery, InspectorState};
use crate::matchmaking::MatchmakingService;
use crate::methods;
use crate::ops::dashboard::{Dashboard, DashboardConfig, dev_tls};
use crate::ops::live::{LiveFeed, LiveSigner, Verified};
use crate::ops::{Command, OpsService};
use crate::persist::memory::MemoryStore;
use crate::persist::pg::PgStore;
use crate::persist::{LedgerStore, PersistService};
use crate::realm::RealmService;
use crate::social::SocialService;

/// The system of record a cluster writes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreChoice {
    /// In memory (development; lost at exit).
    Memory,
    /// PostgreSQL, by libpq connection string, in the `mantis` schema.
    Postgres(String),
}

/// How to start a local cluster.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// The address every role binds (loopback by default; port 0 each).
    pub bind: IpAddr,
    /// The system of record.
    pub store: StoreChoice,
    /// The Ops dashboard.
    pub dashboard: DashboardConfig,
    /// Characters per match.
    pub group: usize,
}

impl ClusterConfig {
    /// Loopback, in-memory store, a loopback dashboard on an ephemeral port.
    #[must_use]
    pub fn local() -> Self {
        let mut dashboard = DashboardConfig::loopback();
        dashboard.listen = SocketAddr::from(([127, 0, 0, 1], 0));
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            store: StoreChoice::Memory,
            dashboard,
            group: 2,
        }
    }
}

/// Every service role in one process.
pub struct LocalCluster {
    runtime: tokio::runtime::Runtime,
    servers: Vec<(Role, RpcServer)>,
    dashboard: Option<Dashboard>,
    /// The dashboard's development certificate (DER), for clients to trust.
    pub dashboard_cert: Vec<u8>,
    /// The cluster key every role proves at the RPC hello.
    pub key: Vec<u8>,
    /// The account role.
    pub account: AccountService,
    /// The realm role.
    pub realm: RealmService,
    /// The social role.
    pub social: SocialService,
    /// The persistence writer.
    pub persist: PersistService,
    /// Ops.
    pub ops: OpsService,
    store: String,
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "no randomness".to_owned())?;
    Ok(b)
}

/// A random operator token (hex).
///
/// # Errors
/// No randomness.
pub fn operator_token() -> Result<String, String> {
    Ok(random(24)?.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

impl LocalCluster {
    /// Starts every role: account, realm, social, matchmaking, the
    /// persistence writer (migrating its store), and Ops with its
    /// dashboard.
    ///
    /// # Errors
    /// Why the cluster cannot start.
    pub fn start(config: &ClusterConfig) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let key = random(32)?;
        let (store, store_name): (Box<dyn LedgerStore>, String) = match &config.store {
            StoreChoice::Memory => (Box::new(MemoryStore::new()), "memory".to_owned()),
            StoreChoice::Postgres(conn) => {
                let pg = runtime
                    .block_on(PgStore::connect(conn, "mantis"))
                    .map_err(|e| e.0)?;
                (Box::new(pg), "postgres (schema mantis)".to_owned())
            }
        };
        // PgStore blocks in place on the runtime: build the writer there.
        let persist = runtime
            .block_on(async { tokio::task::spawn(async move { PersistService::new(store, now_ms()) }).await })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.0)?;
        let account = AccountService::new();
        let realm = RealmService::new();
        let bind = |router: Router| {
            let key = key.clone();
            runtime.block_on(RpcServer::bind(SocketAddr::new(config.bind, 0), key, router))
        };
        let account_server = bind(account.router()).map_err(|e| e.to_string())?;
        let realm_server = bind(realm.router()).map_err(|e| e.to_string())?;
        let persist_server = bind(persist.router()).map_err(|e| e.to_string())?;
        // Social writes guild rows through the writer and reads them back.
        let social =
            SocialService::with_writer(RpcClient::new(persist_server.addr(), Role::Social, key.clone()));
        runtime.block_on(social.load_guilds())?;
        let social_server = bind(social.router()).map_err(|e| e.to_string())?;
        let (account_addr, realm_addr) = (account_server.addr(), realm_server.addr());
        let mut servers = vec![
            (Role::Account, account_server),
            (Role::Realm, realm_server),
            (Role::Social, social_server),
            (Role::Persist, persist_server),
        ];
        let handle = runtime.handle().clone();
        let to_realm = Arc::new(RpcClient::new(realm_addr, Role::Matchmaking, key.clone()));
        let matchmaking = MatchmakingService::new(
            config.group,
            Arc::new(move |queue| {
                let req = m::CreateInstance {
                    template: u32::from(queue),
                };
                let (realm, handle) = (Arc::clone(&to_realm), handle.clone());
                tokio::task::block_in_place(|| {
                    handle
                        .block_on(async move { realm.call::<methods::NewInstance>(&req, RPC_TIMEOUT).await })
                })
                .map(|i| (i.cell.0, i.address.as_str().to_owned()))
            }),
        );
        servers.push((
            Role::Matchmaking,
            bind(matchmaking.router()).map_err(|e| e.to_string())?,
        ));
        let (signer, _pkcs8) = LiveSigner::generate(&SystemRandom::new())?;
        let ops = OpsService::new(
            persist.clone(),
            Arc::new(RpcClient::new(account_addr, Role::Ops, key.clone())),
            signer,
        );
        servers.push((Role::Ops, bind(ops.router()).map_err(|e| e.to_string())?));
        let (tls, dashboard_cert) = dev_tls()?;
        let dashboard = runtime.block_on(Dashboard::start(&config.dashboard, ops.clone(), tls))?;
        Ok(Self {
            runtime,
            servers,
            dashboard: Some(dashboard),
            dashboard_cert,
            key,
            account,
            realm,
            social,
            persist,
            ops,
            store: store_name,
        })
    }

    /// Where `role` listens for RPC.
    #[must_use]
    pub fn addr(&self, role: Role) -> Option<SocketAddr> {
        self.servers
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, s)| s.addr())
    }

    /// The dashboard's address.
    #[must_use]
    pub fn dashboard_addr(&self) -> Option<SocketAddr> {
        self.dashboard.as_ref().map(Dashboard::addr)
    }

    /// The runtime the roles run on (cell links share it).
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Lets the Ops inspector read the cell served at `inspector`.
    pub fn add_cell(&self, cell: u64, inspector: SocketAddr) {
        self.ops.add_cell(
            cell,
            Arc::new(RpcClient::new(inspector, Role::Ops, self.key.clone())),
        );
    }

    /// Runs one Ops command (the dashboard's path, for tools and tests).
    ///
    /// # Errors
    /// The command's failure, as text.
    pub fn execute(&self, actor: &str, cmd: &Command) -> Result<crate::ops::Executed, String> {
        let ops = self.ops.clone();
        let (actor, cmd) = (actor.to_owned(), cmd.clone());
        self.runtime
            .block_on(async move { tokio::spawn(async move { ops.execute(&actor, &cmd).await }).await })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// Stops the social role (its in-memory state is lost), as a crash
    /// would. Returns where it listened.
    pub fn stop_social(&mut self) -> Option<SocketAddr> {
        let at = self.servers.iter().position(|(r, _)| *r == Role::Social)?;
        let (_, server) = self.servers.remove(at);
        let addr = server.addr();
        let _guard = self.runtime.enter();
        drop(server);
        Some(addr)
    }

    /// Starts a fresh social role at `addr`: a new epoch, parties and
    /// friends empty (cells restore them), guilds read back from the writer.
    ///
    /// # Errors
    /// The bind failed.
    pub fn start_social(&mut self, addr: SocketAddr) -> Result<(), String> {
        let persist = self.addr(Role::Persist).ok_or("no persistence writer")?;
        self.social = SocialService::with_writer(RpcClient::new(persist, Role::Social, self.key.clone()));
        self.runtime.block_on(self.social.load_guilds())?;
        let server = self
            .runtime
            .block_on(RpcServer::bind(addr, self.key.clone(), self.social.router()))
            .map_err(|e| e.to_string())?;
        self.servers.push((Role::Social, server));
        Ok(())
    }

    /// The resolved service graph: every role, where it listens, and the
    /// caller matrix of every method.
    #[must_use]
    pub fn graph(&self) -> String {
        let mut out = String::from("service graph (all roles in this process):\n");
        for (role, server) in &self.servers {
            let _ = writeln!(out, "  {:<12} rpc {}", role.name(), server.addr());
        }
        if let Some(d) = &self.dashboard {
            let _ = writeln!(
                out,
                "  {:<12} https {} (dashboard, its own listener)",
                "ops",
                d.addr()
            );
        }
        let _ = writeln!(out, "  {:<12} {}", "store", self.store);
        out.push_str("methods (id name: callers):\n");
        for (name, id, callers) in methods::matrix() {
            let who: Vec<&str> = callers.iter().map(|r| r.name()).collect();
            let _ = writeln!(out, "  {id:>3} {name}: {}", who.join(", "));
        }
        out
    }
}

impl Drop for LocalCluster {
    fn drop(&mut self) {
        // Servers and the dashboard stop their tasks; drop them while the
        // runtime still runs.
        let _guard = self.runtime.enter();
        self.dashboard = None;
        self.servers.clear();
    }
}

// ---- admission --------------------------------------------------------------

/// Verifies game-handshake tokens with the realm in the background: the
/// cell host starts a verification and collects the answers later, never
/// waiting on the network in its tick (lead ruling, M7). A timeout is the
/// host's to enforce.
pub struct TokenVerifier {
    handle: tokio::runtime::Handle,
    realm: Arc<RpcClient>,
    cells: Vec<m::CellNo>,
    tx: std::sync::mpsc::Sender<(u64, Option<u64>)>,
    rx: std::sync::mpsc::Receiver<(u64, Option<u64>)>,
}

impl TokenVerifier {
    /// Verifies tokens naming any of `cells` with the realm at `realm`.
    #[must_use]
    pub fn new(handle: &tokio::runtime::Handle, realm: SocketAddr, key: Vec<u8>, cells: &[u64]) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            handle: handle.clone(),
            realm: Arc::new(RpcClient::new(realm, Role::Cell, key)),
            cells: cells.iter().map(|c| m::CellNo(*c)).collect(),
            tx,
            rx,
        }
    }

    /// Starts verifying `token` for `ticket`; returns at once.
    pub fn begin(&self, ticket: u64, token: &[u8]) {
        let (realm, tx) = (Arc::clone(&self.realm), self.tx.clone());
        let req = m::RedeemOnHost {
            token: BoundedArray::from_slice(token).unwrap_or_default(),
            cells: BoundedArray::from_slice(&self.cells).unwrap_or_default(),
        };
        self.handle.spawn(async move {
            let character = realm
                .call::<methods::RedeemForHost>(&req, RPC_TIMEOUT)
                .await
                .ok()
                .map(|r| r.character.0);
            let _ = tx.send((ticket, character));
        });
    }

    /// Answers since the last call: the character, or `None` (refuse).
    pub fn ready(&self) -> Vec<(u64, Option<u64>)> {
        self.rx.try_iter().collect()
    }
}

// ---- the cell host's link ---------------------------------------------------

/// One outcome from a cell, as the host hands it over.
#[derive(Clone, Copy, Debug)]
pub struct CellOutcome {
    /// The cell tick.
    pub tick: u64,
    /// The command kind.
    pub kind: u16,
    /// The session (0 none).
    pub session: u64,
    /// Succeeded.
    pub ok: bool,
    /// The payload.
    pub payload: [u8; 512],
    /// Its length.
    pub len: usize,
}

/// Where a cell host's link connects.
#[derive(Clone, Debug)]
pub struct CellLinkConfig {
    /// The cluster key.
    pub key: Vec<u8>,
    /// The persistence writer.
    pub persist: SocketAddr,
    /// Ops (live changes).
    pub ops: SocketAddr,
    /// Social (cross-cell lines and presence).
    pub social: SocketAddr,
    /// Matchmaking (placements of hosted characters).
    pub matchmaking: SocketAddr,
    /// The realm (cell registration).
    pub realm: SocketAddr,
    /// The live-data public key (from Ops).
    pub live_key: Vec<u8>,
    /// Cells on this host: id, game address, x range.
    pub cells: Vec<(u64, String, (f32, f32))>,
    /// Instance cells on this host: id, game address. The realm hands them
    /// to matchmaking one group at a time.
    pub instances: Vec<(u64, String)>,
    /// How often live changes are polled.
    pub poll: Duration,
    /// Where the inspector listens (port 0 picks one).
    pub inspector: SocketAddr,
}

/// What the link has done.
#[derive(Debug, Default)]
pub struct LinkStats {
    /// Outcome batches the writer acknowledged.
    pub durable_batches: AtomicU64,
    /// Push attempts that failed and were retried.
    pub push_retries: AtomicU64,
    /// Live changes verified and handed to the host.
    pub live_applied: AtomicU64,
    /// Live polls refused (bad signature or gap): the feed stops there.
    pub live_refused: AtomicU64,
    /// Lines published to social.
    pub published: AtomicU64,
    /// Lines social refused (target offline, not a member).
    pub publish_refused: AtomicU64,
    /// Lines social delivered to this host's cells.
    pub delivered: AtomicU64,
    /// Operations relayed to social (acknowledged).
    pub relayed: AtomicU64,
    /// Relay attempts that failed and were retried.
    pub relay_retries: AtomicU64,
    /// Projected updates received.
    pub projected: AtomicU64,
    /// Projected updates received twice (must stay 0).
    pub projected_duplicates: AtomicU64,
    /// Gaps in a cell's projected sequence within one epoch (must stay 0).
    pub projected_gaps: AtomicU64,
    /// Social epochs seen beyond the first (restarts of the role).
    pub social_restarts: AtomicU64,
    /// Cell projections restored into the social role.
    pub restores: AtomicU64,
}

/// An update social projected onto one of this host's cells.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProjectedUpdate {
    /// The cell.
    pub cell: u64,
    /// The topic.
    pub topic: u16,
    /// The update, as the core encodes it.
    pub payload: Vec<u8>,
}

fn instance_cell(cell: u64, address: &str) -> m::RegisterCell {
    m::RegisterCell {
        cell: m::CellNo(cell),
        address: WireString::new(address).unwrap_or_default(),
        lo: 0.0,
        hi: 0.0,
        instance: true,
    }
}

/// An Ops order for the cell host, applied at its next tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpsOrder {
    /// End this character's session.
    Kick {
        /// The character.
        character: u64,
    },
    /// Refuse new sessions and end the rest after `grace_ms`, or lift it.
    Drain {
        /// Drain (true) or lift (false).
        on: bool,
        /// Milliseconds before remaining sessions are ended.
        grace_ms: u32,
    },
}

/// Matchmaking placed a hosted character in an instance cell.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placed {
    /// The character.
    pub character: u64,
    /// The instance cell.
    pub cell: u64,
    /// Its game address.
    pub address: String,
    /// The queue that formed the match.
    pub queue: u16,
}

enum MatchOut {
    Poll(Vec<u64>),
    Release(u64, String),
    Withdraw(u64),
}

/// A line social delivered to a character in one of this host's cells.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SocialDelivery {
    /// The cell hosting the character.
    pub cell: u64,
    /// The channel (social numbering).
    pub channel: u8,
    /// The speaker.
    pub from: u64,
    /// The character it is for.
    pub to: u64,
    /// The text.
    pub text: String,
}

enum SocialOut {
    Publish(Box<m::Publish>),
    Present(Box<m::Present>),
}

/// Registers the host's cells and instance cells with the realm.
fn register(handle: &tokio::runtime::Handle, config: &CellLinkConfig) -> Result<(), String> {
    let realm = RpcClient::new(config.realm, Role::Cell, config.key.clone());
    for (cell, address, (lo, hi)) in &config.cells {
        let req = m::RegisterCell {
            cell: m::CellNo(*cell),
            address: WireString::new(address).unwrap_or_default(),
            lo: *lo,
            hi: *hi,
            instance: false,
        };
        handle
            .block_on(realm.call::<methods::RegisterCellHost>(&req, RPC_TIMEOUT))
            .map_err(|e| format!("registering cell {cell}: {e}"))?;
    }
    for (cell, address) in &config.instances {
        handle
            .block_on(realm.call::<methods::RegisterCellHost>(&instance_cell(*cell, address), RPC_TIMEOUT))
            .map_err(|e| format!("registering instance cell {cell}: {e}"))?;
    }
    Ok(())
}

/// What Ops calls on a cell host: the read-only inspector, kick, drain.
fn host_router(
    summaries: &Arc<Mutex<BTreeMap<u64, m::CellSummary>>>,
    hosted: &Arc<Mutex<BTreeMap<u64, Vec<u64>>>>,
    order_tx: std::sync::mpsc::Sender<OpsOrder>,
    inspection: &Arc<InspectorState>,
) -> Router {
    let mut inspect = Router::validated(methods::validate);
    inspection.serve(&mut inspect);
    let read = Arc::clone(summaries);
    inspect.serve::<methods::InspectCell>(move |_, req| {
        lock(&read)
            .get(&req.cell.0)
            .copied()
            .ok_or_else(|| RpcError::Refused(format!("no cell {} on this host", req.cell.0)))
    });
    let (seen, orders) = (Arc::clone(hosted), Mutex::new(order_tx.clone()));
    inspect.serve::<methods::Kick>(move |_, req| {
        let found = lock(&seen).values().any(|c| c.contains(&req.character.0));
        if found {
            let _ = lock(&orders).send(OpsOrder::Kick {
                character: req.character.0,
            });
        }
        Ok(m::Kicked { found })
    });
    let orders = Mutex::new(order_tx);
    inspect.serve::<methods::Drain>(move |_, req| {
        let _ = lock(&orders).send(OpsOrder::Drain {
            on: req.on,
            grace_ms: req.grace_ms,
        });
        Ok(m::Empty {})
    });
    inspect
}

/// A cell host's link to the service roles.
pub struct CellLink {
    handle: tokio::runtime::Handle,
    outcomes: tokio::sync::mpsc::UnboundedSender<(u64, Vec<CellOutcome>)>,
    live: std::sync::mpsc::Receiver<Verified>,
    social_out: tokio::sync::mpsc::UnboundedSender<SocialOut>,
    social_in: std::sync::mpsc::Receiver<SocialDelivery>,
    relay_out: tokio::sync::mpsc::UnboundedSender<(u64, u16, Vec<u8>)>,
    projected: std::sync::mpsc::Receiver<ProjectedUpdate>,
    match_out: tokio::sync::mpsc::UnboundedSender<MatchOut>,
    instances: BTreeMap<u64, String>,
    placed: std::sync::mpsc::Receiver<Placed>,
    summaries: Arc<Mutex<BTreeMap<u64, m::CellSummary>>>,
    hosted: Arc<Mutex<BTreeMap<u64, Vec<u64>>>>,
    orders: std::sync::mpsc::Receiver<OpsOrder>,
    inspection: Arc<InspectorState>,
    queries: std::sync::mpsc::Receiver<EntityQuery>,
    inspector: Option<RpcServer>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Counters.
    pub stats: Arc<LinkStats>,
}

impl CellLink {
    /// Registers the host's cells with the realm, starts the inspector and
    /// the background push and poll tasks, on `handle`.
    ///
    /// # Errors
    /// Registration or the inspector bind failed.
    pub fn start(handle: &tokio::runtime::Handle, config: &CellLinkConfig) -> Result<Self, String> {
        register(handle, config)?;
        let summaries: Arc<Mutex<BTreeMap<u64, m::CellSummary>>> = Arc::default();
        let hosted: Arc<Mutex<BTreeMap<u64, Vec<u64>>>> = Arc::default();
        let (order_tx, order_rx) = std::sync::mpsc::channel();
        let (query_tx, query_rx) = std::sync::mpsc::channel();
        let inspection = Arc::new(InspectorState::new(query_tx));
        let inspect = host_router(&summaries, &hosted, order_tx, &inspection);
        let inspector = handle
            .block_on(RpcServer::bind(config.inspector, config.key.clone(), inspect))
            .map_err(|e| e.to_string())?;
        let stats = Arc::new(LinkStats::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let push = handle.spawn(push_task(
            RpcClient::new(config.persist, Role::Cell, config.key.clone()),
            rx,
            Arc::clone(&stats),
            Arc::clone(&inspection),
        ));
        let (live_tx, live_rx) = std::sync::mpsc::channel();
        let first_cell = config.cells.first().map_or(0, |c| c.0);
        let poll = handle.spawn(poll_task(
            RpcClient::new(config.ops, Role::Cell, config.key.clone()),
            LiveFeed::new(config.live_key.clone()),
            first_cell,
            config.poll,
            live_tx,
            Arc::clone(&stats),
        ));
        let (social_tx, social_rx) = tokio::sync::mpsc::unbounded_channel();
        let (deliver_tx, deliver_rx) = std::sync::mpsc::channel();
        let social_send = handle.spawn(social_send_task(
            RpcClient::new(config.social, Role::Cell, config.key.clone()),
            social_rx,
            Arc::clone(&stats),
        ));
        let social_poll = handle.spawn(social_poll_task(
            RpcClient::new(config.social, Role::Cell, config.key.clone()),
            config.cells.iter().map(|c| c.0).collect(),
            config.poll,
            deliver_tx,
            Arc::clone(&stats),
        ));
        let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
        let (projected_tx, projected_rx) = std::sync::mpsc::channel();
        let mirror: Arc<Mutex<Mirror>> = Arc::default();
        let relays = handle.spawn(relay_task(
            RpcClient::new(config.social, Role::Cell, config.key.clone()),
            relay_rx,
            Arc::clone(&hosted),
            Arc::clone(&mirror),
            Arc::clone(&stats),
        ));
        let projections = handle.spawn(projection_task(
            RpcClient::new(config.social, Role::Cell, config.key.clone()),
            config
                .cells
                .iter()
                .map(|c| c.0)
                .chain(config.instances.iter().map(|c| c.0))
                .collect(),
            config.poll,
            projected_tx,
            mirror,
            Arc::clone(&stats),
        ));
        let (match_tx, match_rx) = tokio::sync::mpsc::unbounded_channel();
        let (placed_tx, placed_rx) = std::sync::mpsc::channel();
        let matches = handle.spawn(match_task(
            RpcClient::new(config.matchmaking, Role::Cell, config.key.clone()),
            RpcClient::new(config.realm, Role::Cell, config.key.clone()),
            match_rx,
            placed_tx,
        ));
        Ok(Self {
            handle: handle.clone(),
            match_out: match_tx,
            instances: config.instances.iter().cloned().collect(),
            relay_out: relay_tx,
            projected: projected_rx,
            placed: placed_rx,
            outcomes: tx,
            live: live_rx,
            social_out: social_tx,
            social_in: deliver_rx,
            summaries,
            hosted,
            orders: order_rx,
            inspection,
            queries: query_rx,
            inspector: Some(inspector),
            tasks: vec![push, poll, social_send, social_poll, matches, relays, projections],
            stats,
        })
    }

    /// Where the inspector listens (register it with Ops).
    #[must_use]
    pub fn inspector(&self) -> Option<SocketAddr> {
        self.inspector.as_ref().map(RpcServer::addr)
    }

    /// Queues one tick's outcomes of `cell` for the writer.
    pub fn push(&self, cell: u64, outcomes: Vec<CellOutcome>) {
        if !outcomes.is_empty() {
            for o in &outcomes {
                self.inspection.pushed(cell, o.tick);
            }
            let _ = self.outcomes.send((cell, outcomes));
        }
    }

    /// Publishes `cell`'s run times and component names for the Ops
    /// inspector ([`crate::inspect::system_times`]).
    pub fn publish_inspection(&self, times: m::SystemTimes, names: Vec<String>) {
        self.inspection.publish(times, names);
    }

    /// Entity pages the Ops inspector asked for since the last call; answer
    /// each between ticks ([`EntityQuery::answer`]).
    pub fn entity_queries(&self) -> Vec<EntityQuery> {
        self.queries.try_iter().collect()
    }

    /// Ticks the oldest outcome of `cell` not yet durable is behind `now`.
    #[must_use]
    pub fn log_lag(&self, cell: u64, now: u64) -> u32 {
        self.inspection.log_lag(cell, now)
    }

    /// Publishes a line on a social channel (in order, in the background).
    pub fn publish(&self, channel: u8, from: u64, to: u64, text: &str) {
        let _ = self.social_out.send(SocialOut::Publish(Box::new(m::Publish {
            channel,
            from: m::CharacterId(from),
            to,
            text: WireString::new(text).unwrap_or_default(),
        })));
    }

    /// Tells social which characters `cell` hosts now.
    pub fn presence(&self, cell: u64, characters: &[u64]) {
        lock(&self.hosted).insert(cell, characters.to_vec());
        let ids: Vec<m::CharacterId> = characters.iter().take(256).map(|c| m::CharacterId(*c)).collect();
        let _ = self.social_out.send(SocialOut::Present(Box::new(m::Present {
            cell: m::CellNo(cell),
            characters: BoundedArray::from_slice(&ids).unwrap_or_default(),
        })));
    }

    /// Relays a module's operation for social (in order per cell, retried
    /// until acknowledged, applied once).
    pub fn relay(&self, cell: u64, topic: u16, payload: &[u8]) {
        let _ = self.relay_out.send((cell, topic, payload.to_vec()));
    }

    /// Updates social projected onto this host's cells since the last call,
    /// in order per cell.
    pub fn projections(&self) -> Vec<ProjectedUpdate> {
        self.projected.try_iter().collect()
    }

    /// Asks matchmaking (in the background) whether any of `characters`
    /// was placed; answers arrive through [`CellLink::placements`].
    pub fn poll_matches(&self, characters: &[u64]) {
        if !characters.is_empty() {
            let _ = self.match_out.send(MatchOut::Poll(characters.to_vec()));
        }
    }

    /// Placements found since the last call.
    pub fn placements(&self) -> Vec<Placed> {
        self.placed.try_iter().collect()
    }

    /// Hands an emptied instance cell back to the realm (it registered
    /// with the address given in [`CellLinkConfig::instances`]).
    pub fn release_instance(&self, cell: u64) {
        if let Some(address) = self.instances.get(&cell) {
            let _ = self.match_out.send(MatchOut::Release(cell, address.clone()));
        }
    }

    /// Takes this host's instance cells out of the realm's directory (a
    /// drain), or offers them again.
    pub fn withdraw_instances(&self, withdraw: bool) {
        for (cell, address) in &self.instances {
            let job = if withdraw {
                MatchOut::Withdraw(*cell)
            } else {
                MatchOut::Release(*cell, address.clone())
            };
            let _ = self.match_out.send(job);
        }
    }

    /// Ops orders received since the last call, in order.
    pub fn ops_orders(&self) -> Vec<OpsOrder> {
        self.orders.try_iter().collect()
    }

    /// Lines social delivered since the last call, in order.
    pub fn social_deliveries(&self) -> Vec<SocialDelivery> {
        self.social_in.try_iter().collect()
    }

    /// Live changes verified since the last call, in order.
    pub fn live_changes(&self) -> Vec<Verified> {
        self.live.try_iter().collect()
    }

    /// Publishes `cell`'s summary for the inspector.
    pub fn summary(&self, summary: m::CellSummary) {
        lock(&self.summaries).insert(summary.cell.0, summary);
    }
}

impl Drop for CellLink {
    fn drop(&mut self) {
        let _guard = self.handle.enter();
        for t in &self.tasks {
            t.abort();
        }
        self.inspector = None;
    }
}

async fn push_task(
    writer: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(u64, Vec<CellOutcome>)>,
    stats: Arc<LinkStats>,
    inspection: Arc<InspectorState>,
) {
    while let Some((cell, outcomes)) = rx.recv().await {
        let mut by_tick: BTreeMap<u64, Vec<CellOutcome>> = BTreeMap::new();
        for o in outcomes {
            by_tick.entry(o.tick).or_default().push(o);
        }
        for (tick, outcomes) in by_tick {
            Box::pin(push_tick(&writer, cell, tick, &outcomes, &stats)).await;
            inspection.durable(cell, tick);
        }
    }
}

/// Outcome batches per tick at most (chunks of 32 rows).
pub const BATCHES_PER_TICK: u64 = 256;

/// The batch sequence number of chunk `chunk` of `tick`'s outcomes: derived
/// from the log, so a host that recovered and pushes again from its log
/// numbers every batch the same way, and the writer applies each once.
#[must_use]
pub fn batch_seq(tick: u64, chunk: u64) -> u64 {
    tick.saturating_mul(BATCHES_PER_TICK)
        .saturating_add(chunk.min(BATCHES_PER_TICK - 1))
}

async fn push_tick(writer: &RpcClient, cell: u64, tick: u64, outcomes: &[CellOutcome], stats: &LinkStats) {
    {
        for (chunk_index, chunk) in (0u64..).zip(outcomes.chunks(32)) {
            let rows: Vec<m::OutcomeRow> = chunk
                .iter()
                .map(|o| m::OutcomeRow {
                    tick: o.tick,
                    at_ms: now_ms(),
                    kind: o.kind,
                    session: o.session,
                    ok: o.ok,
                    payload: BoundedArray::from_slice(o.payload.get(..o.len).unwrap_or(&[]))
                        .unwrap_or_default(),
                })
                .collect();
            let req = m::PushOutcomes {
                cell: m::CellNo(cell),
                seq: batch_seq(tick, chunk_index),
                rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            };
            // Retry the same batch until it is durable: the writer treats a
            // resent sequence number as already written.
            let mut delay = Duration::from_millis(50);
            while writer.call::<methods::Push>(&req, RPC_TIMEOUT).await.is_err() {
                stats.push_retries.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
            stats.durable_batches.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn match_task(
    matchmaking: RpcClient,
    realm: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<MatchOut>,
    out: std::sync::mpsc::Sender<Placed>,
) {
    while let Some(job) = rx.recv().await {
        match job {
            MatchOut::Poll(characters) => {
                for character in characters {
                    let req = m::PollMatch {
                        character: m::CharacterId(character),
                    };
                    if let Ok(found) = matchmaking.call::<methods::MatchFor>(&req, RPC_TIMEOUT).await
                        && found.cell.0 != 0
                    {
                        let placed = Placed {
                            character,
                            cell: found.cell.0,
                            address: found.address.as_str().to_owned(),
                            queue: found.queue,
                        };
                        if out.send(placed).is_err() {
                            return;
                        }
                    }
                }
            }
            MatchOut::Release(cell, address) => {
                let _ = realm
                    .call::<methods::RegisterCellHost>(&instance_cell(cell, &address), RPC_TIMEOUT)
                    .await;
            }
            MatchOut::Withdraw(cell) => {
                let _ = realm
                    .call::<methods::Withdraw>(
                        &m::WithdrawCell {
                            cell: m::CellNo(cell),
                        },
                        RPC_TIMEOUT,
                    )
                    .await;
            }
        }
    }
}

/// The host's copy of what social projected onto its cells: the parties
/// and friend lists of characters there. After a restart of the social
/// role, each cell's copy is restored into it before any other operation.
#[derive(Debug, Default)]
struct Mirror {
    parties: BTreeMap<u64, BTreeMap<u32, (u64, Vec<u64>)>>,
    friends: BTreeMap<u64, BTreeMap<u64, Vec<u64>>>,
}

impl Mirror {
    fn observe(&mut self, cell: u64, topic: u16, payload: &[u8]) {
        match topic {
            PARTY_UPDATE => match decode_exact::<PartyUpdate>(payload) {
                Ok(PartyUpdate::Roster {
                    party,
                    leader,
                    members,
                    ..
                }) => {
                    self.parties
                        .entry(cell)
                        .or_default()
                        .insert(party, (leader, members));
                }
                Ok(PartyUpdate::Left { to, party }) => {
                    let cell_parties = self.parties.entry(cell).or_default();
                    if let Some((_, members)) = cell_parties.get_mut(&party) {
                        members.retain(|m| *m != to);
                        if members.len() < 2 {
                            cell_parties.remove(&party);
                        }
                    }
                }
                _ => {}
            },
            FRIEND_UPDATE => {
                if let Ok(FriendUpdate::List { to, friends }) = decode_exact::<FriendUpdate>(payload) {
                    self.friends.entry(cell).or_default().insert(to, friends);
                }
            }
            _ => {}
        }
    }

    /// The restore operations for `cell`: (topic, payload).
    fn restore(&self, cell: u64) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        for (party, (leader, members)) in self.parties.get(&cell).into_iter().flatten() {
            let op = PartyOp::Restore {
                party: *party,
                leader: *leader,
                members: members.clone(),
            };
            let mut b = Vec::new();
            mantis_core::wire::encode_into(&op, &mut b);
            out.push((PARTY_OP, b));
        }
        for (me, friends) in self.friends.get(&cell).into_iter().flatten() {
            let op = FriendOp::Restore {
                me: *me,
                friends: friends.clone(),
            };
            let mut b = Vec::new();
            mantis_core::wire::encode_into(&op, &mut b);
            out.push((FRIEND_OP, b));
        }
        out
    }
}

/// The longest wait between relay attempts while the social role is down.
pub const RELAY_RETRY_MAX: Duration = Duration::from_millis(200);

/// Calls until the role answers (or refuses for good).
async fn relay_once(social: &RpcClient, req: &m::Relay, stats: &LinkStats) -> Option<m::RelayAck> {
    let mut delay = Duration::from_millis(20);
    loop {
        match social.call::<methods::RelayOp>(req, RPC_TIMEOUT).await {
            Ok(ack) => return Some(ack),
            // Refused for good (a malformed operation): no retry.
            Err(
                RpcError::Refused(_) | RpcError::Malformed | RpcError::Forbidden | RpcError::NoSuchMethod,
            ) => {
                return None;
            }
            Err(_) => {
                stats.relay_retries.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RELAY_RETRY_MAX);
            }
        }
    }
}

async fn restore_cell(
    social: &RpcClient,
    cell: u64,
    hosted: &Mutex<BTreeMap<u64, Vec<u64>>>,
    mirror: &Mutex<Mirror>,
    stats: &LinkStats,
) {
    // Who is here first, so what the restore leads to reaches them.
    let here: Vec<m::CharacterId> = lock(hosted)
        .get(&cell)
        .map(|c| c.iter().take(256).map(|x| m::CharacterId(*x)).collect())
        .unwrap_or_default();
    let present = m::Present {
        cell: m::CellNo(cell),
        characters: BoundedArray::from_slice(&here).unwrap_or_default(),
    };
    let _ = social.call::<methods::Presence>(&present, RPC_TIMEOUT).await;
    let ops = lock(mirror).restore(cell);
    for (topic, payload) in ops {
        let req = m::Relay {
            cell: m::CellNo(cell),
            seq: 0,
            restore: true,
            topic,
            payload: BoundedArray::from_slice(&payload).unwrap_or_default(),
        };
        let _ = Box::pin(relay_once(social, &req, stats)).await;
    }
    let mut delay = Duration::from_millis(20);
    while social
        .call::<methods::Restored>(
            &m::RestoredCell {
                cell: m::CellNo(cell),
            },
            RPC_TIMEOUT,
        )
        .await
        .is_err()
    {
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
    stats.restores.fetch_add(1, Ordering::Relaxed);
}

async fn relay_task(
    social: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(u64, u16, Vec<u8>)>,
    hosted: Arc<Mutex<BTreeMap<u64, Vec<u64>>>>,
    mirror: Arc<Mutex<Mirror>>,
    stats: Arc<LinkStats>,
) {
    let mut seqs: BTreeMap<u64, u64> = BTreeMap::new();
    while let Some((cell, topic, payload)) = rx.recv().await {
        let seq = seqs.entry(cell).or_insert(0);
        *seq += 1;
        let req = m::Relay {
            cell: m::CellNo(cell),
            seq: *seq,
            restore: false,
            topic,
            payload: BoundedArray::from_slice(&payload).unwrap_or_default(),
        };
        // A role that restarted asks for the cell's projection first.
        for _ in 0..3 {
            match Box::pin(relay_once(&social, &req, &stats)).await {
                Some(ack) if ack.needs_restore => {
                    Box::pin(restore_cell(&social, cell, &hosted, &mirror, &stats)).await;
                }
                _ => break,
            }
        }
        stats.relayed.fetch_add(1, Ordering::Relaxed);
    }
}

async fn projection_task(
    social: RpcClient,
    cells: Vec<u64>,
    every: Duration,
    out: std::sync::mpsc::Sender<ProjectedUpdate>,
    mirror: Arc<Mutex<Mirror>>,
    stats: Arc<LinkStats>,
) {
    let mut epoch = 0u64;
    let mut since: BTreeMap<u64, u64> = cells.iter().map(|c| (*c, 0)).collect();
    loop {
        for cell in &cells {
            let seen = since.get(cell).copied().unwrap_or(0);
            let req = m::PollProjections {
                cell: m::CellNo(*cell),
                since: seen,
            };
            let Ok(got) = social.call::<methods::Projection>(&req, RPC_TIMEOUT).await else {
                continue;
            };
            if got.epoch != epoch {
                if epoch != 0 {
                    stats.social_restarts.fetch_add(1, Ordering::Relaxed);
                }
                // A new run of the role: every cell starts again from 0.
                epoch = got.epoch;
                for s in since.values_mut() {
                    *s = 0;
                }
                if seen != 0 {
                    break;
                }
            }
            let mut last = since.get(cell).copied().unwrap_or(0);
            for p in got.items.iter() {
                if p.seq <= last {
                    stats.projected_duplicates.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if p.seq != last + 1 {
                    stats.projected_gaps.fetch_add(1, Ordering::Relaxed);
                }
                last = p.seq;
                stats.projected.fetch_add(1, Ordering::Relaxed);
                let bytes: Vec<u8> = p.payload.iter().copied().collect();
                lock(&mirror).observe(*cell, p.topic, &bytes);
                let u = ProjectedUpdate {
                    cell: *cell,
                    topic: p.topic,
                    payload: p.payload.iter().copied().collect(),
                };
                if out.send(u).is_err() {
                    return;
                }
            }
            since.insert(*cell, last);
        }
        tokio::time::sleep(every).await;
    }
}

async fn social_send_task(
    social: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SocialOut>,
    stats: Arc<LinkStats>,
) {
    while let Some(out) = rx.recv().await {
        match out {
            SocialOut::Publish(p) => match social.call::<methods::PublishLine>(&p, RPC_TIMEOUT).await {
                Ok(_) => stats.published.fetch_add(1, Ordering::Relaxed),
                Err(_) => stats.publish_refused.fetch_add(1, Ordering::Relaxed),
            },
            SocialOut::Present(p) => {
                let _ = social.call::<methods::Presence>(&p, RPC_TIMEOUT).await;
                0
            }
        };
    }
}

async fn social_poll_task(
    social: RpcClient,
    cells: Vec<u64>,
    every: Duration,
    out: std::sync::mpsc::Sender<SocialDelivery>,
    stats: Arc<LinkStats>,
) {
    // Per cell: the social run (epoch) and the last sequence seen in it. A
    // restarted social role numbers from 1 again, so a new epoch starts
    // the cell's position over.
    let mut since: BTreeMap<u64, (u64, u64)> = cells.iter().map(|c| (*c, (0, 0))).collect();
    loop {
        for (cell, (epoch, seen)) in &mut since {
            let req = m::PollSocial {
                cell: m::CellNo(*cell),
                since: *seen,
            };
            let Ok(mut updates) = social.call::<methods::Poll>(&req, RPC_TIMEOUT).await else {
                continue;
            };
            if updates.epoch != *epoch {
                *epoch = updates.epoch;
                *seen = 0;
                let req = m::PollSocial {
                    cell: m::CellNo(*cell),
                    since: 0,
                };
                let Ok(fresh) = social.call::<methods::Poll>(&req, RPC_TIMEOUT).await else {
                    continue;
                };
                updates = fresh;
            }
            for u in updates.updates.iter() {
                *seen = (*seen).max(u.seq);
                // Kind 1: a line (other kinds are not lines).
                if u.kind != 1 {
                    continue;
                }
                stats.delivered.fetch_add(1, Ordering::Relaxed);
                let d = SocialDelivery {
                    cell: *cell,
                    channel: u.channel,
                    from: u.from.0,
                    to: u.to.0,
                    text: u.text.as_str().to_owned(),
                };
                if out.send(d).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(every).await;
    }
}

async fn poll_task(
    ops: RpcClient,
    mut feed: LiveFeed,
    cell: u64,
    every: Duration,
    out: std::sync::mpsc::Sender<Verified>,
    stats: Arc<LinkStats>,
) {
    let mut verified = Vec::new();
    loop {
        let req = m::PollLive {
            cell: m::CellNo(cell),
            since: feed.since(),
        };
        if let Ok(changes) = ops.call::<methods::Live>(&req, RPC_TIMEOUT).await {
            let result = feed.accept(&changes, &mut verified);
            for v in verified.drain(..) {
                stats.live_applied.fetch_add(1, Ordering::Relaxed);
                if out.send(v).is_err() {
                    return;
                }
            }
            if result.is_err() {
                stats.live_refused.fetch_add(1, Ordering::Relaxed);
            }
        }
        tokio::time::sleep(every).await;
    }
}
