//! The six service roles, one per process, built from the services'
//! public constructors with addresses from the verified registry.
//!
//! **Failover roles** (account, realm, social, matchmaking, Ops) run as one
//! active instance and any number of standbys, each process a
//! [`mantis_services::failover::Seat`]: a lease held through the
//! persistence writer under this instance's lease owner (stable across its
//! restarts), renewed every third of `[node] lease_ttl_ms`. A standby
//! answers its callers `Standby` (they move on to the active instance) and
//! takes over within one lease lifetime of the active one's last renewal;
//! a takeover builds a fresh service and loads its durable state, as a
//! restart does. Its `/ready` reads `(active)` or `(standby)`: both are
//! ready. The persistence writer is one instance.
//!
//! A role starts once its dependencies are ready, serves until a drain is
//! requested, then stops: its `/ready` turns 503 first (so orchestrators
//! stop routing to it), then its RPC listener stops accepting, requests
//! already being handled are answered within the drain grace, and every
//! connection closes, so callers see the loss and reconnect to whatever
//! serves the address next; a failover role's lease is given up, so a
//! standby takes over. The persistence writer holds nothing in flight
//! between calls: a batch is durable or it is retried by the cell host.

use std::sync::Arc;

use mantis_services::account::AccountService;
use mantis_services::failover::{LeaseConfig, Seat};
use mantis_services::generated::services as m;
use mantis_services::host::Fence;
use mantis_services::host::clock::ServiceClock;
use mantis_services::host::rpc::{Endpoint, Router, RpcClient, RpcServer};
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::matchmaking::MatchmakingService;
use mantis_services::methods;
use mantis_services::ops::OpsService;
use mantis_services::ops::dashboard::{Dashboard, DashboardConfig, dev_tls};
use mantis_services::ops::live::LiveSigner;
use mantis_services::persist::memory::MemoryStore;
use mantis_services::persist::pg::PgStore;
use mantis_services::persist::{LedgerStore, PersistService};
use mantis_services::realm::RealmService;
use mantis_services::social::SocialService;
use mantis_services::tls::TlsHandle;
use std::sync::Mutex;

use crate::config::Store;
use crate::keys;
use crate::matrix;
use crate::node::Node;

/// What a running role keeps alive: its server, and a failover role's
/// seat (dropping it ends the term) and the Ops dashboard of the term.
struct Running {
    servers: Vec<RpcServer>,
    seat: Option<Box<dyn std::any::Any + Send>>,
    dashboard: DashboardSlot,
}

/// The Ops dashboard of the active term (none on a standby).
type DashboardSlot = Arc<Mutex<Option<Dashboard>>>;

/// What a role's build makes a client with: this node's identity and key,
/// and a live endpoint (owned, so a build can run after start).
#[derive(Clone)]
struct Dial {
    caller: Role,
    key: Vec<u8>,
    tls: TlsHandle,
}

impl Dial {
    fn of(node: &Node) -> Self {
        Self {
            caller: node.config.role,
            key: node.key.clone(),
            tls: node.tls.clone(),
        }
    }

    fn to(&self, endpoint: &Endpoint, server: Role) -> Result<RpcClient, String> {
        RpcClient::with_endpoint(
            endpoint.clone(),
            self.caller,
            self.key.clone(),
            Some(self.tls.clone()),
            server,
        )
        .map_err(|e| format!("a client of {}: {e}", matrix::name(server)))
    }
}

/// The router a failover role serves and what keeps its term alive.
struct Seated {
    router: Router,
    keep: Box<dyn std::any::Any + Send>,
}

/// Starts this instance's seat for its role, building the role's service
/// with `build` on each takeover, and reports its term on `/ready` and
/// `/metrics` (`lease_active`, `lease_epoch`, `lease_takeovers`).
fn seat<S, B, F>(
    node: &Node,
    build: B,
    on_standby: Option<DashboardSlot>,
) -> Result<(Arc<Seat<S>>, Seated), String>
where
    S: Clone + Send + Sync + 'static,
    B: Fn(Fence) -> F + Send + Sync + 'static,
    F: std::future::Future<Output = Result<(S, Router), String>> + Send + 'static,
{
    let persist = Arc::new(node.client_of(Role::Persist)?);
    let lease = LeaseConfig {
        role: node.config.role,
        owner: node.me.owner().to_owned(),
        ttl: node.config.lease_ttl,
    };
    let seat = {
        let _guard = node.handle().enter();
        Arc::new(Seat::start(persist, lease, ServiceClock::default(), build))
    };
    node.say(&format!(
        "failover: lease owner {:?}, lifetime {} ms; standby until it holds the lease",
        node.me.owner(),
        node.config.lease_ttl.as_millis()
    ));
    let watched = Arc::clone(&seat);
    let (status, role, name) = (node.status.clone(), node.config.role, node.me.name.clone());
    node.handle().spawn(async move {
        let mut was: Option<u64> = None;
        loop {
            let now = watched.epoch();
            if now != was {
                if let Some(epoch) = now {
                    status.metrics.add("lease_takeovers", 1);
                    crate::node::say(role, &name, &format!("active: lease term {epoch}"));
                } else {
                    crate::node::say(role, &name, "standby");
                    if let Some(slot) = &on_standby {
                        // A standby serves no dashboard: the active one does.
                        drop(lock(slot).take());
                    }
                }
                was = now;
            }
            status.metrics.set("lease_active", i64::from(now.is_some()));
            status.metrics.set(
                "lease_epoch",
                now.map_or(0, |e| i64::try_from(e).unwrap_or(i64::MAX)),
            );
            status.note_if(
                crate::health::Phase::Ready,
                if now.is_some() { "active" } else { "standby" },
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    let router = seat.router();
    let keep: Box<dyn std::any::Any + Send> = Box::new(Arc::clone(&seat));
    Ok((seat, Seated { router, keep }))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Binds the role's RPC over mutual TLS with the node's identity:
/// plaintext is refused, and a caller's role is its certificate's.
fn serve(node: &Node, router: Router) -> Result<RpcServer, String> {
    node.block_on(RpcServer::bind_tls(
        node.config.listen_rpc,
        node.key.clone(),
        router,
        Some(node.tls.clone()),
    ))
    .map_err(|e| format!("rpc {}: {e}", node.config.listen_rpc))
}

fn persist(node: &Node) -> Result<Router, String> {
    let config = node
        .config
        .persist
        .as_ref()
        .ok_or("a persist node needs [persist]")?;
    let (store, name): (Box<dyn LedgerStore>, String) = match &config.store {
        Store::Memory => (Box::new(MemoryStore::new()), "memory (lost at exit)".to_owned()),
        Store::Postgres { conn_file, schema } => {
            let conn =
                std::fs::read_to_string(conn_file).map_err(|e| format!("{}: {e}", conn_file.display()))?;
            let pg = node
                .block_on(PgStore::connect(conn.trim(), schema))
                .map_err(|e| format!("postgres: {}", e.0))?;
            (Box::new(pg), format!("postgres (schema {schema})"))
        }
    };
    // The PostgreSQL store blocks in place on the runtime: build the writer
    // on a runtime thread.
    let writer = node
        .block_on(async { tokio::spawn(async move { PersistService::new(store, now_ms()) }).await })
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("the writer cannot start: {}", e.0))?;
    node.say(&format!("store: {name}, migrated"));
    Ok(writer.router())
}

/// Accounts are rows the writer keeps: each term reads them back before
/// it serves, so a new active instance knows every account and ban.
fn account(node: &Node) -> Result<Seated, String> {
    let (dial, writer) = (Dial::of(node), node.endpoint_of(Role::Persist)?);
    let (role, name) = (node.config.role, node.me.name.clone());
    let (_, seated) = seat(
        node,
        move |fence| {
            let (dial, writer, name) = (dial.clone(), writer.clone(), name.clone());
            async move {
                let s = AccountService::with_writer(dial.to(&writer, Role::Persist)?).fenced(fence);
                let rows = s.load_durable().await?;
                crate::node::say(
                    role,
                    &name,
                    &format!("read back {rows} account rows from the writer"),
                );
                let router = s.router();
                Ok((s, router))
            }
        },
        None,
    )?;
    Ok(seated)
}

/// Characters are rows the writer keeps: each term reads them back. The
/// cell directory is not: cell hosts register again when the realm's
/// epoch changes, which a takeover does.
fn realm(node: &Node) -> Result<Seated, String> {
    let (dial, writer) = (Dial::of(node), node.endpoint_of(Role::Persist)?);
    let (role, name) = (node.config.role, node.me.name.clone());
    let (_, seated) = seat(
        node,
        move |fence| {
            let (dial, writer, name) = (dial.clone(), writer.clone(), name.clone());
            async move {
                let s = RealmService::with_writer(dial.to(&writer, Role::Persist)?).fenced(fence);
                let rows = s.load_durable().await?;
                crate::node::say(
                    role,
                    &name,
                    &format!("read back {rows} character rows from the writer"),
                );
                let router = s.router();
                Ok((s, router))
            }
        },
        None,
    )?;
    Ok(seated)
}

/// Guilds and friends are rows the writer keeps; parties are rebuilt from
/// the cells' projections, as after a restart.
fn social(node: &Node) -> Result<Seated, String> {
    let (dial, writer) = (Dial::of(node), node.endpoint_of(Role::Persist)?);
    let (role, name) = (node.config.role, node.me.name.clone());
    let (_, seated) = seat(
        node,
        move |fence| {
            let (dial, writer, name) = (dial.clone(), writer.clone(), name.clone());
            async move {
                let s = SocialService::with_writer(dial.to(&writer, Role::Persist)?).fenced(fence);
                let (guilds, friends) = s.load_durable().await?;
                crate::node::say(
                    role,
                    &name,
                    &format!("read back {guilds} guild rows and {friends} friend rows from the writer"),
                );
                let router = s.router();
                Ok((s, router))
            }
        },
        None,
    )?;
    Ok(seated)
}

/// Matchmaking keeps no durable state: a takeover starts with empty
/// queues, and players queue again.
fn matchmaking(node: &Node) -> Result<Seated, String> {
    let group = node
        .config
        .matchmaking
        .ok_or("a matchmaking node needs [matchmaking]")?
        .group;
    let (dial, realm) = (Dial::of(node), node.endpoint_of(Role::Realm)?);
    let (_, seated) = seat(
        node,
        move |_fence| {
            let (dial, realm) = (dial.clone(), realm.clone());
            async move {
                let realm = Arc::new(dial.to(&realm, Role::Realm)?);
                let handle = tokio::runtime::Handle::current();
                let service = MatchmakingService::new(
                    group,
                    Arc::new(move |queue| {
                        let req = m::CreateInstance {
                            template: u32::from(queue),
                        };
                        let (realm, handle) = (Arc::clone(&realm), handle.clone());
                        tokio::task::block_in_place(|| {
                            handle.block_on(async move {
                                realm.call::<methods::NewInstance>(&req, RPC_TIMEOUT).await
                            })
                        })
                        .map(|i| (i.cell.0, i.address.as_str().to_owned()))
                    }),
                );
                let router = service.router();
                Ok((service, router))
            }
        },
        None,
    )?;
    Ok(seated)
}

/// Which cells each cell host serves, by instance.
type Hosted = std::collections::BTreeMap<String, Vec<u64>>;

fn hosted(registry: &crate::registry::Registry) -> Hosted {
    registry
        .of(Role::Cell)
        .into_iter()
        .map(|h| (h.name.clone(), h.cells.clone()))
        .collect()
}

/// What an Ops term needs to reach the cell hosts.
#[derive(Clone)]
struct CellClients {
    endpoints: crate::node::Endpoints,
    dial: Dial,
    hosted: Arc<Mutex<Hosted>>,
}

impl CellClients {
    /// Gives `ops` a client of every cell of every cell host now listed,
    /// and takes away the cells of hosts no longer listed (`before`).
    fn apply(&self, ops: &OpsService, before: &Hosted) {
        let now = lock(&self.hosted).clone();
        for (host, cells) in &now {
            if before.get(host) == Some(cells) {
                continue;
            }
            let Some(endpoint) = self.endpoints.get(host) else {
                continue;
            };
            if let Ok(client) = self.dial.to(&endpoint, Role::Cell) {
                let client = Arc::new(client);
                for cell in cells {
                    ops.add_cell(*cell, Arc::clone(&client));
                }
            }
        }
        for cell in before.values().flatten() {
            if !now.values().flatten().any(|c| c == cell) {
                ops.remove_cell(*cell);
            }
        }
    }
}

/// Keeps the cell hosts Ops knows in step with newer registries: the
/// active term's service gains new hosts and loses gone ones (a moved one
/// is followed by its endpoint).
async fn follow_cells(
    seat: Arc<Seat<OpsService>>,
    mut registries: tokio::sync::watch::Receiver<Arc<crate::registry::Registry>>,
    cells: CellClients,
) {
    while registries.changed().await.is_ok() {
        let next = Arc::clone(&registries.borrow_and_update());
        let before = std::mem::replace(&mut *lock(&cells.hosted), hosted(&next));
        if let Some(ops) = seat.service() {
            cells.apply(&ops, &before);
        }
    }
}

/// What every Ops term needs: the live-data key, its clients, its
/// dashboard's settings.
#[derive(Clone)]
struct OpsPlan {
    dial: Dial,
    persist: Endpoint,
    account: Endpoint,
    pkcs8: Vec<u8>,
    dashboard: DashboardConfig,
    cert_out: Option<std::path::PathBuf>,
    cells: CellClients,
    slot: DashboardSlot,
    who: (Role, String),
}

/// One Ops term: a fresh service, its durable live values published again,
/// its cell clients, and its dashboard (the previous term's, if any, is
/// stopped first: one dashboard per process, on the active term only).
async fn ops_term(plan: OpsPlan, fence: Fence) -> Result<(OpsService, Router), String> {
    let signer = LiveSigner::from_pkcs8(&plan.pkcs8)?;
    let ops = OpsService::remote(
        Arc::new(plan.dial.to(&plan.persist, Role::Persist)?),
        Arc::new(plan.dial.to(&plan.account, Role::Account)?),
        signer,
    )
    .fenced(fence);
    let values = ops.load_live().await?;
    crate::node::say(
        plan.who.0,
        &plan.who.1,
        &format!(
            "{values} durable live values published again (run {})",
            ops.live_epoch()
        ),
    );
    plan.cells.apply(&ops, &Hosted::new());
    drop(lock(&plan.slot).take());
    let (tls, cert) = dev_tls()?;
    if let Some(path) = &plan.cert_out {
        keys::write_secret(path, &cert)?;
    }
    let dashboard = Dashboard::start(&plan.dashboard, ops.clone(), tls).await?;
    crate::node::say(
        plan.who.0,
        &plan.who.1,
        &format!("dashboard https {} (its own listener)", dashboard.addr()),
    );
    *lock(&plan.slot) = Some(dashboard);
    let router = ops.router();
    Ok((ops, router))
}

fn ops(node: &Node) -> Result<(Seated, DashboardSlot), String> {
    let config = node.config.ops.as_ref().ok_or("an ops node needs [ops]")?;
    let pkcs8 = keys::read_pkcs8(&config.live_key)?;
    let signer = LiveSigner::from_pkcs8(&pkcs8).map_err(|e| format!("{}: {e}", config.live_key.display()))?;
    if signer.public_key() != node.registry.live_key {
        return Err(format!(
            "{}: its public key is not the registry's live_key, so every cell would refuse \
             every live change",
            config.live_key.display()
        ));
    }
    let token = keys::read_operator_token(&config.operator_token)?;
    let mut dashboard = DashboardConfig::loopback();
    dashboard.listen = config.dashboard;
    dashboard.allow_remote = config.dashboard_allow_remote;
    dashboard
        .allowed_peers
        .clone_from(&config.dashboard_allowed_peers);
    dashboard.game_ports.clone_from(&config.game_ports);
    dashboard.operators.insert(token, config.operator_name.clone());
    dashboard.check()?;
    let slot: DashboardSlot = Arc::default();
    let cells = CellClients {
        endpoints: node.endpoints.clone(),
        dial: Dial::of(node),
        hosted: Arc::new(Mutex::new(hosted(&node.registry))),
    };
    let plan = OpsPlan {
        dial: Dial::of(node),
        persist: node.endpoint_of(Role::Persist)?,
        account: node.endpoint_of(Role::Account)?,
        pkcs8,
        dashboard,
        cert_out: config.dashboard_cert_out.clone(),
        cells: cells.clone(),
        slot: Arc::clone(&slot),
        who: (node.config.role, node.me.name.clone()),
    };
    let (seat, seated) = seat(
        node,
        move |fence| ops_term(plan.clone(), fence),
        Some(Arc::clone(&slot)),
    )?;
    node.handle()
        .spawn(follow_cells(seat, node.live.subscribe(), cells));
    node.say(&format!(
        "dashboard on {} while this instance is active (operator token from {})",
        config.dashboard,
        config.operator_token.display()
    ));
    Ok((seated, slot))
}

fn start(node: &Node) -> Result<Running, String> {
    let mut dashboard: DashboardSlot = Arc::default();
    let (router, seat) = match node.config.role {
        Role::Persist => (persist(node)?, None),
        Role::Account => split(account(node)?),
        Role::Realm => split(realm(node)?),
        Role::Social => split(social(node)?),
        Role::Matchmaking => split(matchmaking(node)?),
        Role::Ops => {
            let (seated, slot) = ops(node)?;
            dashboard = slot;
            split(seated)
        }
        Role::Cell | Role::Gateway => {
            return Err(format!(
                "{} is not a service role mantisd runs",
                matrix::name(node.config.role)
            ));
        }
    };
    let server = serve(node, router)?;
    node.say(&format!("rpc {}", server.addr()));
    Ok(Running {
        servers: vec![server],
        seat,
        dashboard,
    })
}

fn split(s: Seated) -> (Router, Option<Box<dyn std::any::Any + Send>>) {
    (s.router, Some(s.keep))
}

/// Runs a service role on `node` until a drain: waits for its
/// dependencies, serves, then stops.
///
/// # Errors
/// Why the role could not start.
pub fn run(node: &Node) -> Result<(), String> {
    node.wait_for_dependencies()?;
    let running = start(node)?;
    node.ready();
    let why = node.block_on(node.drain.wait());
    node.draining(why);
    let grace = node.config.drain_grace;
    let Running {
        servers,
        seat,
        dashboard,
    } = running;
    node.block_on(async move {
        drop(lock(&dashboard).take());
        for server in servers {
            server.shutdown(grace).await;
        }
        // The term ends with the process: a standby takes the lease over.
        drop(seat);
    });
    node.say("stopped: requests in flight answered, listeners and connections closed");
    Ok(())
}
