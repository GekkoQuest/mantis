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

use mantis_core::social::{PARTY_OP, PARTY_UPDATE, PartyOp, PartyUpdate};
use mantis_core::wire::{BoundedArray, WireString, decode_exact};
use ring::rand::{SecureRandom, SystemRandom};

use crate::account::AccountService;
use crate::generated::services as m;
use crate::host::clock::ServiceClock;
use crate::host::lock;
use crate::host::rpc::Endpoint;
use crate::host::rpc::{Router, RpcClient, RpcError, RpcServer};
use crate::host::{RPC_TIMEOUT, Role};
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
use crate::tls::{IdentityError, TlsHandle, TlsIdentity};

/// The system of record a cluster writes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreChoice {
    /// In memory (development; lost at exit).
    Memory,
    /// PostgreSQL, by libpq connection string, in the `mantis` schema.
    Postgres(String),
    /// PostgreSQL in the named schema (tests keep theirs apart).
    PostgresSchema(String, String),
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
    /// The cluster key every role proves (`None`: a fresh random key).
    pub key: Option<Vec<u8>>,
    /// Ops's live-data signing key, PKCS#8 (`None`: a fresh key).
    pub live_pkcs8: Option<Vec<u8>>,
    /// Each role's TLS identity: every role's RPC server serves mutual TLS
    /// with its own, and every internal client calls with its caller's.
    /// `None`: plaintext. A role missing from the map is a start error.
    pub tls: Option<BTreeMap<Role, Arc<TlsIdentity>>>,
    /// The clock every role runs on (the wall clock by default; a test's
    /// manual clock makes token lifetimes, epochs and audit times its own).
    pub clock: ServiceClock,
}

/// `role`'s identity from `tls` (`None` when plaintext).
fn identity_for(
    tls: Option<&BTreeMap<Role, Arc<TlsIdentity>>>,
    role: Role,
) -> Result<Option<Arc<TlsIdentity>>, String> {
    match tls {
        None => Ok(None),
        Some(map) => map
            .get(&role)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("no TLS identity for the {} role", role.name())),
    }
}

/// A client calling `addr` (served by `server`) as `caller`, with the
/// caller's identity from `tls`.
fn client_for(
    tls: Option<&BTreeMap<Role, Arc<TlsIdentity>>>,
    addr: SocketAddr,
    caller: Role,
    server: Role,
    key: &[u8],
) -> Result<RpcClient, String> {
    RpcClient::with_tls(addr, caller, key.to_vec(), identity_for(tls, caller)?, server)
        .map_err(|e: IdentityError| format!("{} calling {}: {e}", caller.name(), server.name()))
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
            key: None,
            live_pkcs8: None,
            tls: None,
            clock: ServiceClock::default(),
        }
    }
}

/// The system of record `choice` names, and its name for the graph.
fn open_store(
    runtime: &tokio::runtime::Runtime,
    choice: &StoreChoice,
) -> Result<(Box<dyn LedgerStore>, String), String> {
    Ok(match choice {
        StoreChoice::Memory => (Box::new(MemoryStore::new()), "memory".to_owned()),
        StoreChoice::Postgres(conn) | StoreChoice::PostgresSchema(conn, _) => {
            let schema = match choice {
                StoreChoice::PostgresSchema(_, schema) => schema.as_str(),
                _ => "mantis",
            };
            let pg = runtime
                .block_on(PgStore::connect(conn, schema))
                .map_err(|e| e.0)?;
            (Box::new(pg), format!("postgres (schema {schema})"))
        }
    })
}

/// The account and realm roles, writing their rows through the writer at
/// `persist` and read back from it.
fn records(
    runtime: &tokio::runtime::Runtime,
    tls: Option<&BTreeMap<Role, Arc<TlsIdentity>>>,
    persist: SocketAddr,
    key: &[u8],
    clock: &ServiceClock,
) -> Result<(AccountService, RealmService), String> {
    let writer = |role: Role| client_for(tls, persist, role, Role::Persist, key);
    let account = AccountService::with_writer(writer(Role::Account)?).clocked(clock.clone());
    runtime.block_on(account.load_durable())?;
    let realm = RealmService::with_writer(writer(Role::Realm)?).clocked(clock.clone());
    runtime.block_on(realm.load_durable())?;
    Ok((account, realm))
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
    /// Ops's live-data signing key (PKCS#8), for restarting Ops.
    live_pkcs8: Vec<u8>,
    store: String,
    tls: Option<BTreeMap<Role, Arc<TlsIdentity>>>,
    clock: ServiceClock,
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
        let key = match &config.key {
            Some(k) if !k.is_empty() => k.clone(),
            Some(_) => return Err("the cluster key is empty".to_owned()),
            None => random(32)?,
        };
        let (store, store_name) = open_store(&runtime, &config.store)?;
        let clock = config.clock.clone();
        let now = clock.now_ms();
        // PgStore blocks in place on the runtime: build the writer there.
        let persist = runtime
            .block_on(async { tokio::task::spawn(async move { PersistService::new(store, now) }).await })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.0)?;
        let tls = config.tls.as_ref();
        let bind = |role: Role, router: Router| -> Result<RpcServer, String> {
            let identity = identity_for(tls, role)?;
            runtime
                .block_on(RpcServer::bind_tls(
                    SocketAddr::new(config.bind, 0),
                    key.clone(),
                    router,
                    identity.map(TlsHandle::from),
                ))
                .map_err(|e| format!("{}: {e}", role.name()))
        };
        let persist_server = bind(Role::Persist, persist.router())?;
        let (account, realm) = records(&runtime, tls, persist_server.addr(), &key, &clock)?;
        let account_server = bind(Role::Account, account.router())?;
        let realm_server = bind(Role::Realm, realm.router())?;
        // Social writes guild and friend rows through the writer and reads
        // them back.
        let social = SocialService::with_writer(client_for(
            tls,
            persist_server.addr(),
            Role::Social,
            Role::Persist,
            &key,
        )?)
        .clocked(clock.clone());
        runtime.block_on(social.load_durable())?;
        let social_server = bind(Role::Social, social.router())?;
        let (account_addr, realm_addr) = (account_server.addr(), realm_server.addr());
        let mut servers = vec![
            (Role::Account, account_server),
            (Role::Realm, realm_server),
            (Role::Social, social_server),
            (Role::Persist, persist_server),
        ];
        let handle = runtime.handle().clone();
        let to_realm = Arc::new(client_for(tls, realm_addr, Role::Matchmaking, Role::Realm, &key)?);
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
        servers.push((Role::Matchmaking, bind(Role::Matchmaking, matchmaking.router())?));
        let (signer, live_pkcs8) = match &config.live_pkcs8 {
            Some(pkcs8) => (LiveSigner::from_pkcs8(pkcs8)?, pkcs8.clone()),
            None => LiveSigner::generate(&SystemRandom::new())?,
        };
        let ops = OpsService::new(
            persist.clone(),
            Arc::new(client_for(tls, account_addr, Role::Ops, Role::Account, &key)?),
            signer,
        )
        .clocked(clock.clone());
        runtime.block_on(ops.load_live())?;
        servers.push((Role::Ops, bind(Role::Ops, ops.router())?));
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
            live_pkcs8,
            store: store_name,
            tls: config.tls.clone(),
            clock,
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

    /// The clock every role runs on (cell links in tests share it).
    #[must_use]
    pub fn clock(&self) -> ServiceClock {
        self.clock.clone()
    }

    /// The runtime the roles run on (cell links share it).
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Lets the Ops inspector read the cell served at `inspector` (a cell
    /// host). The Ops identity was checked at start, so this cannot fail
    /// there; [`LocalCluster::try_add_cell`] reports why it would.
    pub fn add_cell(&self, cell: u64, inspector: SocketAddr) {
        if let Err(e) = self.try_add_cell(cell, inspector) {
            eprintln!("ops cannot inspect cell {cell}: {e}");
        }
    }

    /// [`LocalCluster::add_cell`], with its error.
    ///
    /// # Errors
    /// The Ops identity does not make a client.
    pub fn try_add_cell(&self, cell: u64, inspector: SocketAddr) -> Result<(), String> {
        let client = client_for(self.tls.as_ref(), inspector, Role::Ops, Role::Cell, &self.key)?;
        self.ops.add_cell(cell, Arc::new(client));
        Ok(())
    }

    fn bind_role(&self, role: Role, addr: SocketAddr, router: Router) -> Result<RpcServer, String> {
        let identity = identity_for(self.tls.as_ref(), role)?;
        self.runtime
            .block_on(RpcServer::bind_tls(
                addr,
                self.key.clone(),
                router,
                identity.map(TlsHandle::from),
            ))
            .map_err(|e| format!("{}: {e}", role.name()))
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
        self.stop_role(Role::Social)
    }

    /// Starts a fresh Ops role at `addr` over the same writer and signing
    /// key, as an Ops process restarting would: a new run that publishes
    /// every durable live value again. (The dashboard keeps serving the
    /// Ops it started with.)
    ///
    /// # Errors
    /// The writer or the bind failed.
    pub fn start_ops(&mut self, addr: SocketAddr) -> Result<(), String> {
        let account = self.addr(Role::Account).ok_or("no account role")?;
        let ops = OpsService::new(
            self.persist.clone(),
            Arc::new(client_for(
                self.tls.as_ref(),
                account,
                Role::Ops,
                Role::Account,
                &self.key,
            )?),
            LiveSigner::from_pkcs8(&self.live_pkcs8)?,
        )
        .clocked(self.clock.clone());
        self.runtime.block_on(ops.load_live())?;
        let server = self.bind_role(Role::Ops, addr, ops.router())?;
        self.ops = ops;
        self.servers.push((Role::Ops, server));
        Ok(())
    }

    /// Starts a fresh realm role at `addr`: a new run, its directory empty
    /// (cell hosts register again when they see the new epoch), its
    /// characters read back from the writer.
    ///
    /// # Errors
    /// The writer or the bind failed.
    pub fn start_realm(&mut self, addr: SocketAddr) -> Result<(), String> {
        let persist = self.addr(Role::Persist).ok_or("no persistence writer")?;
        let writer = client_for(self.tls.as_ref(), persist, Role::Realm, Role::Persist, &self.key)?;
        self.realm = RealmService::with_writer(writer).clocked(self.clock.clone());
        self.runtime.block_on(self.realm.load_durable())?;
        let server = self.bind_role(Role::Realm, addr, self.realm.router())?;
        self.servers.push((Role::Realm, server));
        Ok(())
    }

    /// Starts a fresh account role at `addr`: its accounts read back from
    /// the writer, no session token of the old run valid.
    ///
    /// # Errors
    /// The writer or the bind failed.
    pub fn start_account(&mut self, addr: SocketAddr) -> Result<(), String> {
        let persist = self.addr(Role::Persist).ok_or("no persistence writer")?;
        let writer = client_for(
            self.tls.as_ref(),
            persist,
            Role::Account,
            Role::Persist,
            &self.key,
        )?;
        self.account = AccountService::with_writer(writer).clocked(self.clock.clone());
        self.runtime.block_on(self.account.load_durable())?;
        let server = self.bind_role(Role::Account, addr, self.account.router())?;
        self.servers.push((Role::Account, server));
        Ok(())
    }

    /// Starts the persistence writer's RPC server again at `addr`, over the
    /// same store (a writer process restarting on its database).
    ///
    /// # Errors
    /// The bind failed.
    pub fn start_persist(&mut self, addr: SocketAddr) -> Result<(), String> {
        let server = self.bind_role(Role::Persist, addr, self.persist.router())?;
        self.servers.push((Role::Persist, server));
        Ok(())
    }

    /// Stops `role`'s RPC server, as a crash of its process would (its
    /// callers see it unreachable). Returns where it listened.
    pub fn stop_role(&mut self, role: Role) -> Option<SocketAddr> {
        let at = self.servers.iter().position(|(r, _)| *r == role)?;
        let (_, server) = self.servers.remove(at);
        let addr = server.addr();
        let _guard = self.runtime.enter();
        drop(server);
        Some(addr)
    }

    /// Starts a fresh social role at `addr`: a new epoch, parties empty
    /// (cells restore them), guilds and friends read back from the writer.
    ///
    /// # Errors
    /// The bind failed.
    pub fn start_social(&mut self, addr: SocketAddr) -> Result<(), String> {
        let persist = self.addr(Role::Persist).ok_or("no persistence writer")?;
        self.social = SocialService::with_writer(client_for(
            self.tls.as_ref(),
            persist,
            Role::Social,
            Role::Persist,
            &self.key,
        )?)
        .clocked(self.clock.clone());
        self.runtime.block_on(self.social.load_durable())?;
        let server = self.bind_role(Role::Social, addr, self.social.router())?;
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
    tx: std::sync::mpsc::Sender<(u64, Option<Admitted>)>,
    rx: std::sync::mpsc::Receiver<(u64, Option<Admitted>)>,
}

/// A redeemed entry token: who plays, and where a returning character
/// enters (where it left the world).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Admitted {
    /// The character.
    pub character: u64,
    /// Where it enters; `None` where the host spawns new sessions.
    pub spawn: Option<[f32; 3]>,
}

impl TokenVerifier {
    /// Verifies tokens naming any of `cells` with the realm at `realm`.
    #[must_use]
    pub fn new(handle: &tokio::runtime::Handle, realm: SocketAddr, key: Vec<u8>, cells: &[u64]) -> Self {
        Self::with_client(handle, RpcClient::new(realm, Role::Cell, key), cells)
    }

    /// [`TokenVerifier::new`] over mutual TLS with the cell host's identity
    /// `tls` (`None`: plaintext).
    ///
    /// # Errors
    /// The identity is not a cell host's, or does not make a configuration.
    pub fn with_tls(
        handle: &tokio::runtime::Handle,
        realm: SocketAddr,
        key: Vec<u8>,
        tls: Option<Arc<TlsIdentity>>,
        cells: &[u64],
    ) -> Result<Self, IdentityError> {
        Self::with_endpoint(
            handle,
            Endpoint::fixed(realm),
            key,
            tls.map(TlsHandle::from),
            cells,
        )
    }

    /// [`TokenVerifier::new`] with the realm at `realm` (followed when it
    /// moves) and the cell host's identity `tls` (followed when it changes).
    ///
    /// # Errors
    /// The identity is not a cell host's, or does not make a configuration.
    pub fn with_endpoint(
        handle: &tokio::runtime::Handle,
        realm: Endpoint,
        key: Vec<u8>,
        tls: Option<TlsHandle>,
        cells: &[u64],
    ) -> Result<Self, IdentityError> {
        let client = RpcClient::with_endpoint(realm, Role::Cell, key, tls, Role::Realm)?;
        Ok(Self::with_client(handle, client, cells))
    }

    fn with_client(handle: &tokio::runtime::Handle, realm: RpcClient, cells: &[u64]) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            handle: handle.clone(),
            realm: Arc::new(realm),
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
                .map(|r| Admitted {
                    character: r.character.0,
                    spawn: r.placed.then_some([r.x, r.y, r.z]),
                });
            let _ = tx.send((ticket, character));
        });
    }

    /// Answers since the last call: who is admitted, or `None` (refuse).
    pub fn ready(&self) -> Vec<(u64, Option<Admitted>)> {
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
    pub persist: Endpoint,
    /// Ops (live changes).
    pub ops: Endpoint,
    /// Social (cross-cell lines and presence).
    pub social: Endpoint,
    /// Matchmaking (placements of hosted characters).
    pub matchmaking: Endpoint,
    /// The realm (cell registration).
    pub realm: Endpoint,
    /// The world this host's cells belong to (registered with the realm and
    /// reported with every placement).
    pub world: u32,
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
    /// The cell host's own TLS identity (role `cell`); `None`: plaintext.
    pub tls: Option<TlsHandle>,
}

impl CellLinkConfig {
    /// Serves the inspector router at `inspector`, as this cell host.
    fn serve_inspector(&self, handle: &tokio::runtime::Handle, router: Router) -> Result<RpcServer, String> {
        handle
            .block_on(RpcServer::bind_tls(
                self.inspector,
                self.key.clone(),
                router,
                self.tls.clone(),
            ))
            .map_err(|e| e.to_string())
    }

    /// A client calling `addr`, served by `server`, as this cell host.
    fn client(&self, endpoint: &Endpoint, server: Role) -> Result<RpcClient, String> {
        RpcClient::with_endpoint(
            endpoint.clone(),
            Role::Cell,
            self.key.clone(),
            self.tls.clone(),
            server,
        )
        .map_err(|e| e.to_string())
    }
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
    /// Realm runs seen beyond the first: each re-registered this host's
    /// cells.
    pub realm_restarts: AtomicU64,
    /// Placements (a character left the world or arrived after a transfer)
    /// the realm acknowledged.
    pub placements: AtomicU64,
    /// Outcome messages and relays queued and not yet acknowledged
    /// ([`CellLink::pending`]).
    pub pending: AtomicU64,
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

fn instance_cell(cell: u64, address: &str, world: u32) -> m::RegisterCell {
    m::RegisterCell {
        cell: m::CellNo(cell),
        address: WireString::new(address).unwrap_or_default(),
        lo: 0.0,
        hi: 0.0,
        instance: true,
        world,
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

/// A link's background tasks, in the order a stepped link gives them their
/// turns when several are ready at once: what the step queued is sent
/// first (presence before relays), then the polls read the results.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Task {
    Push,
    SocialSend,
    Relays,
    Matches,
    Projections,
    SocialPoll,
    LivePoll,
    Realm,
    Whereabouts,
}

/// Link tasks.
const TASKS: usize = 9;

/// What a link's tasks are doing: how many are running (not parked on the
/// clock, a queue, or the gate), and per task, items queued for it and
/// whether it is parked on its queue.
#[derive(Debug, Default)]
struct Activity {
    running: AtomicU64,
    tasks: [TaskState; TASKS],
}

/// One task's queue: items waiting, and whether it is parked on it.
#[derive(Debug, Default)]
struct TaskState {
    queued: AtomicU64,
    receiving: std::sync::atomic::AtomicBool,
}

impl Activity {
    /// Nothing runs, and no task parked on its queue has an item waiting.
    fn quiet(&self) -> bool {
        self.running.load(Ordering::SeqCst) == 0
            && self
                .tasks
                .iter()
                .all(|t| !t.receiving.load(Ordering::SeqCst) || t.queued.load(Ordering::SeqCst) == 0)
    }

    fn of(&self, task: Task) -> Option<&TaskState> {
        self.tasks.get(task as usize)
    }

    fn running(&self) -> u64 {
        self.running.load(Ordering::SeqCst)
    }
}

/// A stepped link's turns: every task that wakes (an item taken, a clock
/// wait over) waits here for [`CellLink::settle`] to let it run, one task
/// at a time, in (due time, task, arrival) order. So the order of every
/// call the link makes is the same on every machine.
#[derive(Debug, Default)]
struct Gate {
    waiting: Mutex<BTreeMap<(u64, Task, u64), tokio::sync::oneshot::Sender<()>>>,
    next: AtomicU64,
}

/// The clock a link's tasks wait on, their activity, and (on a manual
/// clock) the gate that orders their turns.
#[derive(Clone, Debug)]
struct Pace {
    clock: ServiceClock,
    activity: Arc<Activity>,
    gate: Option<Arc<Gate>>,
    task: Task,
}

impl Pace {
    /// The same pace, for `task`.
    fn of(&self, task: Task) -> Self {
        let mut p = self.clone();
        p.task = task;
        p
    }

    /// Registers for a turn at or after clock time `due` (then `took`, under
    /// the gate's lock, so a settle sees the registration and the change
    /// together), stops running, and parks until `settle` grants the turn.
    async fn turn(&self, gate: &Gate, due: u64, took: impl FnOnce()) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut waiting = lock(&gate.waiting);
            let seq = gate.next.fetch_add(1, Ordering::Relaxed);
            waiting.insert((due, self.task, seq), tx);
            took();
        }
        // The granter counted this task running before granting.
        let _ = rx.await;
    }

    /// Waits `d` of the clock's time, parked.
    async fn sleep(&self, d: Duration) {
        let running = &self.activity.running;
        if let Some(gate) = &self.gate {
            let due = self
                .clock
                .now_ms()
                .saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            self.turn(gate, due, || {
                running.fetch_sub(1, Ordering::SeqCst);
            })
            .await;
        } else {
            running.fetch_sub(1, Ordering::SeqCst);
            self.clock.sleep(d).await;
            running.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The next item queued for this task, parked while there is none.
    async fn recv<T>(&self, rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> Option<T> {
        let a = &self.activity;
        let fallback = TaskState::default();
        let state = a.of(self.task).unwrap_or(&fallback);
        let (receiving, queued) = (&state.receiving, &state.queued);
        receiving.store(true, Ordering::SeqCst);
        a.running.fetch_sub(1, Ordering::SeqCst);
        let item = rx.recv().await;
        let took = || {
            receiving.store(false, Ordering::SeqCst);
            queued.fetch_sub(1, Ordering::SeqCst);
        };
        match (&self.gate, item.is_some()) {
            (Some(gate), true) => self.turn(gate, 0, took).await,
            (None, true) => {
                took();
                a.running.fetch_add(1, Ordering::SeqCst);
            }
            (_, false) => {
                receiving.store(false, Ordering::SeqCst);
                a.running.fetch_add(1, Ordering::SeqCst);
            }
        }
        item
    }

    /// Spawns a link task, running until it first parks.
    fn spawn(
        &self,
        handle: &tokio::runtime::Handle,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.activity.running.fetch_add(1, Ordering::SeqCst);
        handle.spawn(task)
    }

    /// Queues an item for `task` on `tx`.
    fn send<T>(&self, task: Task, tx: &tokio::sync::mpsc::UnboundedSender<T>, item: T) -> bool {
        let state = self.activity.of(task);
        if let Some(s) = state {
            s.queued.fetch_add(1, Ordering::SeqCst);
        }
        let sent = tx.send(item).is_ok();
        if !sent && let Some(s) = state {
            s.queued.fetch_sub(1, Ordering::SeqCst);
        }
        sent
    }
}

/// A link's placement plumbing: (match jobs, placements, the match task,
/// the realm task).
type Placement = (
    tokio::sync::mpsc::UnboundedSender<MatchOut>,
    std::sync::mpsc::Receiver<Placed>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
);

/// The matchmaking and realm-watch tasks of a link.
fn spawn_placement(
    handle: &tokio::runtime::Handle,
    config: &CellLinkConfig,
    stats: &Arc<LinkStats>,
    pace: &Pace,
) -> Result<Placement, String> {
    let (match_tx, match_rx) = tokio::sync::mpsc::unbounded_channel();
    let (placed_tx, placed_rx) = std::sync::mpsc::channel();
    let busy: Arc<Mutex<std::collections::BTreeSet<u64>>> = Arc::default();
    let matches = pace.spawn(
        handle,
        match_task(
            config.client(&config.matchmaking, Role::Matchmaking)?,
            config.client(&config.realm, Role::Realm)?,
            match_rx,
            (placed_tx, config.world),
            Arc::clone(&busy),
            pace.of(Task::Matches),
        ),
    );
    let realm_epoch = handle.block_on(
        config
            .client(&config.realm, Role::Realm)?
            .call::<methods::RealmRun>(&m::PollRealm {}, RPC_TIMEOUT),
    );
    let realm_watch = pace.spawn(
        handle,
        realm_task(
            config.client(&config.realm, Role::Realm)?,
            config.clone(),
            realm_epoch.map_or(0, |e| e.epoch),
            busy,
            Arc::clone(stats),
            pace.of(Task::Realm),
        ),
    );
    Ok((match_tx, placed_rx, matches, realm_watch))
}

/// Watches the realm's run: a new epoch (a restarted realm, its directory
/// empty) registers this host's world cells again, and its instance cells
/// that are free (a busy one registers itself when it is released).
async fn realm_task(
    realm: RpcClient,
    config: CellLinkConfig,
    mut epoch: u64,
    busy: Arc<Mutex<std::collections::BTreeSet<u64>>>,
    stats: Arc<LinkStats>,
    pace: Pace,
) {
    loop {
        pace.sleep(config.poll).await;
        let Ok(now) = realm
            .call::<methods::RealmRun>(&m::PollRealm {}, RPC_TIMEOUT)
            .await
        else {
            continue;
        };
        if now.epoch == epoch {
            continue;
        }
        let mut done = true;
        for (cell, address, (lo, hi)) in &config.cells {
            let req = m::RegisterCell {
                cell: m::CellNo(*cell),
                address: WireString::new(address).unwrap_or_default(),
                lo: *lo,
                hi: *hi,
                instance: false,
                world: config.world,
            };
            done &= realm
                .call::<methods::RegisterCellHost>(&req, RPC_TIMEOUT)
                .await
                .is_ok();
        }
        let free: Vec<(u64, String)> = {
            let busy = lock(&busy);
            config
                .instances
                .iter()
                .filter(|(c, _)| !busy.contains(c))
                .cloned()
                .collect()
        };
        for (cell, address) in &free {
            done &= realm
                .call::<methods::RegisterCellHost>(&instance_cell(*cell, address, config.world), RPC_TIMEOUT)
                .await
                .is_ok();
        }
        // Only a run fully registered into is remembered; otherwise the next
        // poll tries again.
        if done {
            if epoch != 0 {
                stats.realm_restarts.fetch_add(1, Ordering::Relaxed);
            }
            epoch = now.epoch;
        }
    }
}

/// Registers the host's cells and instance cells with the realm.
fn register(handle: &tokio::runtime::Handle, config: &CellLinkConfig) -> Result<(), String> {
    let realm = config.client(&config.realm, Role::Realm)?;
    for (cell, address, (lo, hi)) in &config.cells {
        let req = m::RegisterCell {
            cell: m::CellNo(*cell),
            address: WireString::new(address).unwrap_or_default(),
            lo: *lo,
            hi: *hi,
            instance: false,
            world: config.world,
        };
        handle
            .block_on(realm.call::<methods::RegisterCellHost>(&req, RPC_TIMEOUT))
            .map_err(|e| format!("registering cell {cell}: {e}"))?;
    }
    for (cell, address) in &config.instances {
        handle
            .block_on(
                realm.call::<methods::RegisterCellHost>(
                    &instance_cell(*cell, address, config.world),
                    RPC_TIMEOUT,
                ),
            )
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

/// A link's social plumbing: what the host queues and receives, and the
/// tasks that move it.
struct SocialParts {
    social_out: tokio::sync::mpsc::UnboundedSender<SocialOut>,
    social_in: std::sync::mpsc::Receiver<SocialDelivery>,
    relay_out: tokio::sync::mpsc::UnboundedSender<(u64, u16, Vec<u8>)>,
    projected: std::sync::mpsc::Receiver<ProjectedUpdate>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Characters per cell on this host.
type Hosted = Arc<Mutex<BTreeMap<u64, Vec<u64>>>>;

/// The social send, poll, relay and projection tasks of a link.
fn spawn_social(
    handle: &tokio::runtime::Handle,
    config: &CellLinkConfig,
    stats: &Arc<LinkStats>,
    hosted: &Hosted,
    pace: &Pace,
) -> Result<SocialParts, String> {
    let (social_tx, social_rx) = tokio::sync::mpsc::unbounded_channel();
    let (deliver_tx, deliver_rx) = std::sync::mpsc::channel();
    let social_send = pace.spawn(
        handle,
        social_send_task(
            config.client(&config.social, Role::Social)?,
            social_rx,
            Arc::clone(stats),
            pace.of(Task::SocialSend),
        ),
    );
    let social_poll = pace.spawn(
        handle,
        social_poll_task(
            config.client(&config.social, Role::Social)?,
            config.cells.iter().map(|c| c.0).collect(),
            config.poll,
            deliver_tx,
            Arc::clone(stats),
            pace.of(Task::SocialPoll),
        ),
    );
    let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
    let (projected_tx, projected_rx) = std::sync::mpsc::channel();
    let mirror: Arc<Mutex<Mirror>> = Arc::default();
    let relays = pace.spawn(
        handle,
        relay_task(
            config.client(&config.social, Role::Social)?,
            relay_rx,
            (Arc::clone(hosted), Arc::clone(&mirror)),
            Arc::clone(stats),
            u64::from_le_bytes(random(8)?.try_into().unwrap_or([1; 8])).max(1),
            pace.of(Task::Relays),
        ),
    );
    let projections = pace.spawn(
        handle,
        projection_task(
            config.client(&config.social, Role::Social)?,
            config
                .cells
                .iter()
                .map(|c| c.0)
                .chain(config.instances.iter().map(|c| c.0))
                .collect(),
            (config.poll, projected_tx),
            mirror,
            Arc::clone(stats),
            pace.of(Task::Projections),
        ),
    );
    Ok(SocialParts {
        social_out: social_tx,
        social_in: deliver_rx,
        relay_out: relay_tx,
        projected: projected_rx,
        tasks: vec![social_send, social_poll, relays, projections],
    })
}

/// Where this host last saw each character, per cell, to tell the realm
/// where characters leave the world and arrive.
#[derive(Debug, Default)]
struct Whereabouts {
    here: BTreeMap<u64, BTreeMap<u64, [f32; 3]>>,
}

/// The task telling the realm where characters leave the world or arrive,
/// in order, each retried until the realm answers.
fn spawn_whereabouts(
    handle: &tokio::runtime::Handle,
    config: &CellLinkConfig,
    stats: &Arc<LinkStats>,
    pace: &Pace,
) -> Result<
    (
        tokio::sync::mpsc::UnboundedSender<m::CharacterPlaced>,
        tokio::task::JoinHandle<()>,
    ),
    String,
> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = pace.spawn(
        handle,
        whereabouts_task(
            config.client(&config.realm, Role::Realm)?,
            rx,
            Arc::clone(stats),
            pace.of(Task::Whereabouts),
        ),
    );
    Ok((tx, task))
}

async fn whereabouts_task(
    realm: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<m::CharacterPlaced>,
    stats: Arc<LinkStats>,
    pace: Pace,
) {
    while let Some(req) = pace.recv(&mut rx).await {
        let mut delay = Duration::from_millis(20);
        loop {
            match realm.call::<methods::PlaceCharacter>(&req, RPC_TIMEOUT).await {
                Ok(_) => {
                    stats.placements.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                // Refused for good (malformed): nothing to retry.
                Err(
                    RpcError::Refused(_) | RpcError::Malformed | RpcError::Forbidden | RpcError::NoSuchMethod,
                ) => {
                    break;
                }
                Err(_) => {
                    pace.sleep(delay).await;
                    delay = (delay * 2).min(RELAY_RETRY_MAX);
                }
            }
        }
        stats.pending.fetch_sub(1, Ordering::SeqCst);
    }
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
    pace: Pace,
    whereabouts: Mutex<Whereabouts>,
    whereabouts_out: tokio::sync::mpsc::UnboundedSender<m::CharacterPlaced>,
    world: u32,
    /// Counters.
    pub stats: Arc<LinkStats>,
}

impl CellLink {
    /// Registers the host's cells with the realm, starts the inspector and
    /// the background push and poll tasks, on `handle`, on the wall clock.
    ///
    /// # Errors
    /// Registration or the inspector bind failed.
    pub fn start(handle: &tokio::runtime::Handle, config: &CellLinkConfig) -> Result<Self, String> {
        Self::start_on(handle, config, ServiceClock::default())
    }

    /// [`CellLink::start`] on `clock`: polls and retries wait on it (a
    /// test's manual clock makes them happen at the same clock times on
    /// every machine; see [`CellLink::settle`]).
    ///
    /// # Errors
    /// Registration or the inspector bind failed.
    pub fn start_on(
        handle: &tokio::runtime::Handle,
        config: &CellLinkConfig,
        clock: ServiceClock,
    ) -> Result<Self, String> {
        let pace = Pace {
            gate: clock.stepped().then(Arc::default),
            clock,
            activity: Arc::default(),
            task: Task::Push,
        };
        register(handle, config)?;
        let summaries: Arc<Mutex<BTreeMap<u64, m::CellSummary>>> = Arc::default();
        let hosted: Arc<Mutex<BTreeMap<u64, Vec<u64>>>> = Arc::default();
        let (order_tx, order_rx) = std::sync::mpsc::channel();
        let (query_tx, query_rx) = std::sync::mpsc::channel();
        let inspection = Arc::new(InspectorState::new(query_tx));
        let inspect = host_router(&summaries, &hosted, order_tx, &inspection);
        let inspector = config.serve_inspector(handle, inspect)?;
        let stats = Arc::new(LinkStats::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let push = pace.spawn(
            handle,
            push_task(
                config.client(&config.persist, Role::Persist)?,
                rx,
                Arc::clone(&stats),
                Arc::clone(&inspection),
                pace.of(Task::Push),
            ),
        );
        let (live_tx, live_rx) = std::sync::mpsc::channel();
        let first_cell = config.cells.first().map_or(0, |c| c.0);
        let poll = pace.spawn(
            handle,
            poll_task(
                config.client(&config.ops, Role::Ops)?,
                LiveFeed::new(config.live_key.clone()),
                (first_cell, config.poll),
                live_tx,
                Arc::clone(&stats),
                pace.of(Task::LivePoll),
            ),
        );
        let social = spawn_social(handle, config, &stats, &hosted, &pace)?;
        let (match_tx, placed_rx, matches, realm_watch) = spawn_placement(handle, config, &stats, &pace)?;
        let (whereabouts_out, whereabouts) = spawn_whereabouts(handle, config, &stats, &pace)?;
        Ok(Self {
            handle: handle.clone(),
            match_out: match_tx,
            instances: config.instances.iter().cloned().collect(),
            relay_out: social.relay_out,
            projected: social.projected,
            placed: placed_rx,
            outcomes: tx,
            live: live_rx,
            social_out: social.social_out,
            social_in: social.social_in,
            summaries,
            hosted,
            orders: order_rx,
            inspection,
            queries: query_rx,
            inspector: Some(inspector),
            tasks: [push, poll, matches, realm_watch, whereabouts]
                .into_iter()
                .chain(social.tasks)
                .collect(),
            pace,
            whereabouts: Mutex::default(),
            whereabouts_out,
            world: config.world,
            stats,
        })
    }

    /// Waits until the link is quiet: every task parked, nothing queued
    /// untaken. On a manual clock the link is stepped: each task that woke
    /// (an item to send, a poll or retry due by the clock) gets its turn
    /// here, one at a time, in a fixed order, and runs until it parks
    /// again. A test that steps its cells, advances the clock, and settles
    /// sees the link make the same calls in the same order at the same step
    /// on every machine.
    ///
    /// # Errors
    /// Still busy when `timeout` (wall time) passes.
    pub fn settle(&self, timeout: Duration) -> Result<(), String> {
        let until = std::time::Instant::now() + timeout;
        let activity = &self.pace.activity;
        loop {
            match &self.pace.gate {
                None if activity.quiet() => return Ok(()),
                None => {}
                Some(gate) => {
                    let mut waiting = lock(&gate.waiting);
                    if activity.quiet() {
                        let now = self.pace.clock.now_ms();
                        let next = waiting.keys().next().copied().filter(|(due, _, _)| *due <= now);
                        let Some(key) = next else {
                            return Ok(());
                        };
                        if let Some(turn) = waiting.remove(&key) {
                            activity.running.fetch_add(1, Ordering::SeqCst);
                            if turn.send(()).is_err() {
                                // The task is gone (the link is closing).
                                activity.running.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        continue;
                    }
                }
            }
            if std::time::Instant::now() >= until {
                let waiting: Vec<Task> = self
                    .pace
                    .gate
                    .as_ref()
                    .map(|g| lock(&g.waiting).keys().map(|k| k.1).collect())
                    .unwrap_or_default();
                return Err(format!(
                    "the link is still busy: {} tasks running ({} outcome messages and relays unacknowledged); waiting for a turn: {waiting:?}",
                    activity.running(),
                    self.pending(),
                ));
            }
            std::thread::sleep(Duration::from_micros(100));
        }
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
            self.stats.pending.fetch_add(1, Ordering::SeqCst);
            if !self.pace.send(Task::Push, &self.outcomes, (cell, outcomes)) {
                self.stats.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Tells the link which characters `cell` (of this host's world) holds now and
    /// where (each tick): the realm hears, in order, where each character
    /// left the world (gone from every cell of this host) or arrived (new
    /// in a cell, after a transfer or an entry), so a restarted realm
    /// places a returning character where it left.
    pub fn track(&self, cell: u64, here: &[(u64, [f32; 3])]) {
        let world = self.world;
        let mut w = lock(&self.whereabouts);
        let before = w.here.remove(&cell).unwrap_or_default();
        let now: BTreeMap<u64, [f32; 3]> = here.iter().copied().collect();
        let send = |character: u64, p: [f32; 3]| {
            self.stats.pending.fetch_add(1, Ordering::SeqCst);
            let req = m::CharacterPlaced {
                character: m::CharacterId(character),
                cell: m::CellNo(cell),
                world,
                x: p[0],
                y: p[1],
                z: p[2],
                level: 0,
            };
            if !self.pace.send(Task::Whereabouts, &self.whereabouts_out, req) {
                self.stats.pending.fetch_sub(1, Ordering::SeqCst);
            }
        };
        // Departures first: one gone from every cell here left the world.
        for (character, p) in &before {
            if !now.contains_key(character) && !w.here.values().any(|c| c.contains_key(character)) {
                send(*character, *p);
            }
        }
        for (character, p) in &now {
            if !before.contains_key(character) {
                send(*character, *p);
            }
        }
        w.here.insert(cell, now);
    }

    /// Outcome messages and relays queued and not yet acknowledged by the
    /// writer or the social role.
    #[must_use]
    pub fn pending(&self) -> u64 {
        self.stats.pending.load(Ordering::SeqCst)
    }

    /// Waits until everything queued is acknowledged ([`CellLink::pending`]
    /// is 0). A drain snapshots, then flushes, then exits: after a deploy,
    /// recovery is from the snapshot alone (decision 0007), so an outcome
    /// still queued at exit would never reach the writer.
    ///
    /// # Errors
    /// What is still pending when `timeout` passes.
    pub fn flush(&self, timeout: Duration) -> Result<(), u64> {
        let until = std::time::Instant::now() + timeout;
        loop {
            let left = self.pending();
            if left == 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= until {
                return Err(left);
            }
            std::thread::sleep(Duration::from_millis(2));
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
        let _ = self.pace.send(
            Task::SocialSend,
            &self.social_out,
            SocialOut::Publish(Box::new(m::Publish {
                channel,
                from: m::CharacterId(from),
                to,
                text: WireString::new(text).unwrap_or_default(),
            })),
        );
    }

    /// Tells social which characters `cell` hosts now.
    pub fn presence(&self, cell: u64, characters: &[u64]) {
        lock(&self.hosted).insert(cell, characters.to_vec());
        let ids: Vec<m::CharacterId> = characters.iter().take(256).map(|c| m::CharacterId(*c)).collect();
        let _ = self.pace.send(
            Task::SocialSend,
            &self.social_out,
            SocialOut::Present(Box::new(m::Present {
                cell: m::CellNo(cell),
                characters: BoundedArray::from_slice(&ids).unwrap_or_default(),
            })),
        );
    }

    /// Relays a module's operation for social (in order per cell, retried
    /// until acknowledged, applied once).
    pub fn relay(&self, cell: u64, topic: u16, payload: &[u8]) {
        self.stats.pending.fetch_add(1, Ordering::SeqCst);
        if !self
            .pace
            .send(Task::Relays, &self.relay_out, (cell, topic, payload.to_vec()))
        {
            self.stats.pending.fetch_sub(1, Ordering::SeqCst);
        }
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
            let _ = self.pace.send(
                Task::Matches,
                &self.match_out,
                MatchOut::Poll(characters.to_vec()),
            );
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
            let _ = self.pace.send(
                Task::Matches,
                &self.match_out,
                MatchOut::Release(cell, address.clone()),
            );
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
            let _ = self.pace.send(Task::Matches, &self.match_out, job);
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
    pace: Pace,
) {
    while let Some((cell, outcomes)) = pace.recv(&mut rx).await {
        let mut by_tick: BTreeMap<u64, Vec<CellOutcome>> = BTreeMap::new();
        for o in outcomes {
            by_tick.entry(o.tick).or_default().push(o);
        }
        for (tick, outcomes) in by_tick {
            Box::pin(push_tick(&writer, cell, tick, &outcomes, &stats, &pace)).await;
            inspection.durable(cell, tick);
        }
        stats.pending.fetch_sub(1, Ordering::SeqCst);
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

async fn push_tick(
    writer: &RpcClient,
    cell: u64,
    tick: u64,
    outcomes: &[CellOutcome],
    stats: &LinkStats,
    pace: &Pace,
) {
    {
        for (chunk_index, chunk) in (0u64..).zip(outcomes.chunks(32)) {
            let rows: Vec<m::OutcomeRow> = chunk
                .iter()
                .map(|o| m::OutcomeRow {
                    tick: o.tick,
                    at_ms: pace.clock.now_ms(),
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
                pace.sleep(delay).await;
                delay = (delay * 2).min(PUSH_RETRY_MAX);
            }
            stats.durable_batches.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn match_task(
    matchmaking: RpcClient,
    realm: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<MatchOut>,
    (out, world): (std::sync::mpsc::Sender<Placed>, u32),
    busy: Arc<Mutex<std::collections::BTreeSet<u64>>>,
    pace: Pace,
) {
    while let Some(job) = pace.recv(&mut rx).await {
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
                lock(&busy).remove(&cell);
                let _ = realm
                    .call::<methods::RegisterCellHost>(&instance_cell(cell, &address, world), RPC_TIMEOUT)
                    .await;
            }
            MatchOut::Withdraw(cell) => {
                lock(&busy).insert(cell);
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

/// The host's copy of what social projected onto its cells: the parties of
/// characters there (friends and guilds are durable in the writer). After a
/// restart of the social role, each cell's copy is restored into it before
/// any other operation.
#[derive(Debug, Default)]
struct Mirror {
    parties: BTreeMap<u64, BTreeMap<u32, (u64, Vec<u64>)>>,
}

impl Mirror {
    fn observe(&mut self, cell: u64, topic: u16, payload: &[u8]) {
        if topic != PARTY_UPDATE {
            return;
        }
        match decode_exact::<PartyUpdate>(payload) {
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
        out
    }
}

/// The longest wait between push attempts while the writer is down: a
/// restarted writer is reached within this of coming back.
pub const PUSH_RETRY_MAX: Duration = Duration::from_millis(250);

/// The longest wait between relay attempts while the social role is down.
pub const RELAY_RETRY_MAX: Duration = Duration::from_millis(200);

/// Calls until the role answers (or refuses for good).
async fn relay_once(
    social: &RpcClient,
    req: &m::Relay,
    stats: &LinkStats,
    pace: &Pace,
) -> Option<m::RelayAck> {
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
                pace.sleep(delay).await;
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
    pace: &Pace,
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
            run: 0,
            seq: 0,
            restore: true,
            topic,
            payload: BoundedArray::from_slice(&payload).unwrap_or_default(),
        };
        let _ = Box::pin(relay_once(social, &req, stats, pace)).await;
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
        pace.sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
    stats.restores.fetch_add(1, Ordering::Relaxed);
}

async fn relay_task(
    social: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(u64, u16, Vec<u8>)>,
    (hosted, mirror): (Hosted, Arc<Mutex<Mirror>>),
    stats: Arc<LinkStats>,
    run: u64,
    pace: Pace,
) {
    let mut seqs: BTreeMap<u64, u64> = BTreeMap::new();
    while let Some((cell, topic, payload)) = pace.recv(&mut rx).await {
        let seq = seqs.entry(cell).or_insert(0);
        *seq += 1;
        let req = m::Relay {
            cell: m::CellNo(cell),
            run,
            seq: *seq,
            restore: false,
            topic,
            payload: BoundedArray::from_slice(&payload).unwrap_or_default(),
        };
        // A role that restarted asks for the cell's projection first.
        for _ in 0..3 {
            match Box::pin(relay_once(&social, &req, &stats, &pace)).await {
                Some(ack) if ack.needs_restore => {
                    Box::pin(restore_cell(&social, cell, &hosted, &mirror, &stats, &pace)).await;
                }
                _ => break,
            }
        }
        stats.relayed.fetch_add(1, Ordering::Relaxed);
        stats.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn projection_task(
    social: RpcClient,
    cells: Vec<u64>,
    (every, out): (Duration, std::sync::mpsc::Sender<ProjectedUpdate>),
    mirror: Arc<Mutex<Mirror>>,
    stats: Arc<LinkStats>,
    pace: Pace,
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
        pace.sleep(every).await;
    }
}

async fn social_send_task(
    social: RpcClient,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SocialOut>,
    stats: Arc<LinkStats>,
    pace: Pace,
) {
    while let Some(out) = pace.recv(&mut rx).await {
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
    pace: Pace,
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
        pace.sleep(every).await;
    }
}

async fn poll_task(
    ops: RpcClient,
    mut feed: LiveFeed,
    (cell, every): (u64, Duration),
    out: std::sync::mpsc::Sender<Verified>,
    stats: Arc<LinkStats>,
    pace: Pace,
) {
    let mut verified = Vec::new();
    loop {
        let req = m::PollLive {
            cell: m::CellNo(cell),
            since: feed.since(),
            epoch: feed.epoch(),
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
        pace.sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `methods::server_of` names the role whose router really serves each
    /// method, every method in the caller matrix has a server, and no
    /// router serves a method the matrix does not list.
    #[test]
    fn every_method_has_exactly_the_server_its_router_says() {
        let persist = PersistService::new(Box::new(MemoryStore::new()), 0).unwrap();
        let (signer, _) = LiveSigner::generate(&SystemRandom::new()).unwrap();
        let account = Arc::new(RpcClient::new(
            "127.0.0.1:1".parse().unwrap(),
            Role::Ops,
            Vec::new(),
        ));
        let (orders, _) = std::sync::mpsc::channel();
        let (queries, _) = std::sync::mpsc::channel();
        let routers = [
            (Role::Account, AccountService::new().router()),
            (Role::Realm, RealmService::new().router()),
            (Role::Social, SocialService::new().router()),
            (Role::Persist, persist.router()),
            (
                Role::Matchmaking,
                MatchmakingService::new(2, Arc::new(|_| Err(RpcError::Disconnected))).router(),
            ),
            (
                Role::Ops,
                OpsService::new(persist.clone(), account, signer).router(),
            ),
            (
                Role::Cell,
                host_router(
                    &Arc::default(),
                    &Arc::default(),
                    orders,
                    &Arc::new(InspectorState::new(queries)),
                ),
            ),
        ];
        for (role, router) in &routers {
            for id in router.ids() {
                assert_eq!(methods::server_of(id), Some(*role), "method {id}");
            }
        }
        for (name, id, _) in methods::matrix() {
            let server = methods::server_of(id).unwrap_or_else(|| panic!("{name} has no server"));
            let router = routers.iter().find(|(r, _)| *r == server).map(|(_, r)| r);
            assert!(
                router.is_some_and(|r| r.serves(id)),
                "{name} is not served by {server:?}"
            );
        }
        let listed: Vec<u16> = methods::matrix().iter().map(|(_, id, _)| *id).collect();
        for (_, router) in &routers {
            assert!(router.ids().iter().all(|id| listed.contains(id)));
        }
    }
}
