//! The six service roles, one per process, built from the services'
//! public constructors with addresses from the verified registry.
//!
//! A role starts once its dependencies are ready, serves until a drain is
//! requested, then stops: its `/ready` turns 503 first (so orchestrators
//! stop routing to it), then its RPC listener stops accepting, requests
//! already being handled are answered within the drain grace, and every
//! connection closes, so callers see the loss and reconnect to whatever
//! serves the address next. The persistence writer holds nothing in flight between calls: a
//! batch is durable or it is retried by the cell host.

use std::sync::Arc;

use mantis_services::account::AccountService;
use mantis_services::generated::services as m;
use mantis_services::host::rpc::{Router, RpcClient, RpcServer};
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

use crate::config::Store;
use crate::keys;
use crate::matrix;
use crate::node::Node;

/// What a running role keeps alive.
struct Running {
    servers: Vec<RpcServer>,
    dashboard: Option<Dashboard>,
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
        Some(Arc::clone(&node.tls)),
    ))
    .map_err(|e| format!("rpc {}: {e}", node.config.listen_rpc))
}

fn client(node: &Node, to: Role) -> Result<RpcClient, String> {
    node.client(node.rpc_of(to)?, to)
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

/// Accounts are rows the writer keeps: read back before serving, so a
/// restarted account role knows every account, session and ban.
fn account(node: &Node) -> Result<Router, String> {
    let account = AccountService::with_writer(client(node, Role::Persist)?);
    let rows = node.block_on(account.load_durable())?;
    node.say(&format!("read back {rows} account rows from the writer"));
    Ok(account.router())
}

/// Characters are rows the writer keeps: read back before serving. The cell
/// directory is not: cell hosts register again when the realm's epoch
/// changes.
fn realm(node: &Node) -> Result<Router, String> {
    let realm = RealmService::with_writer(client(node, Role::Persist)?);
    let rows = node.block_on(realm.load_durable())?;
    node.say(&format!("read back {rows} character rows from the writer"));
    Ok(realm.router())
}

fn social(node: &Node) -> Result<Router, String> {
    let social = SocialService::with_writer(client(node, Role::Persist)?);
    let (guilds, friends) = node.block_on(social.load_durable())?;
    node.say(&format!(
        "read back {guilds} guild rows and {friends} friend rows from the writer"
    ));
    Ok(social.router())
}

fn matchmaking(node: &Node) -> Result<Router, String> {
    let group = node
        .config
        .matchmaking
        .ok_or("a matchmaking node needs [matchmaking]")?
        .group;
    let realm = Arc::new(client(node, Role::Realm)?);
    let handle = node.handle();
    let service = MatchmakingService::new(
        group,
        Arc::new(move |queue| {
            let req = m::CreateInstance {
                template: u32::from(queue),
            };
            let (realm, handle) = (Arc::clone(&realm), handle.clone());
            tokio::task::block_in_place(|| {
                handle.block_on(async move { realm.call::<methods::NewInstance>(&req, RPC_TIMEOUT).await })
            })
            .map(|i| (i.cell.0, i.address.as_str().to_owned()))
        }),
    );
    Ok(service.router())
}

fn ops(node: &Node) -> Result<(Router, Dashboard), String> {
    let config = node.config.ops.as_ref().ok_or("an ops node needs [ops]")?;
    let signer = LiveSigner::from_pkcs8(&keys::read_pkcs8(&config.live_key)?)
        .map_err(|e| format!("{}: {e}", config.live_key.display()))?;
    if signer.public_key() != node.registry.live_key {
        return Err(format!(
            "{}: its public key is not the registry's live_key, so every cell would refuse \
             every live change",
            config.live_key.display()
        ));
    }
    let ops = OpsService::remote(
        Arc::new(client(node, Role::Persist)?),
        Arc::new(client(node, Role::Account)?),
        signer,
    );
    // Every durable live value is published again as the new run's
    // changes, so cells hold the current flags and tunables after an Ops
    // restart.
    let values = node.block_on(ops.load_live())?;
    node.say(&format!(
        "{values} durable live values published again (run {})",
        ops.live_epoch()
    ));
    for host in node.registry.of(Role::Cell) {
        let inspector = Arc::new(node.client(host.rpc, Role::Cell)?);
        for cell in &host.cells {
            ops.add_cell(*cell, Arc::clone(&inspector));
        }
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
    let (tls, cert) = dev_tls()?;
    if let Some(path) = &config.dashboard_cert_out {
        keys::write_secret(path, &cert)?;
    }
    let router = ops.router();
    let started = node.block_on(Dashboard::start(&dashboard, ops, tls))?;
    node.say(&format!(
        "dashboard https {} (its own listener; operator token from {})",
        started.addr(),
        config.operator_token.display()
    ));
    Ok((router, started))
}

fn start(node: &Node) -> Result<Running, String> {
    let mut dashboard = None;
    let router = match node.config.role {
        Role::Account => account(node)?,
        Role::Realm => realm(node)?,
        Role::Persist => persist(node)?,
        Role::Social => social(node)?,
        Role::Matchmaking => matchmaking(node)?,
        Role::Ops => {
            let (router, d) = ops(node)?;
            dashboard = Some(d);
            router
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
        dashboard,
    })
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
    let Running { servers, dashboard } = running;
    node.block_on(async move {
        drop(dashboard);
        for server in servers {
            server.shutdown(grace).await;
        }
    });
    node.say("stopped: requests in flight answered, listeners and connections closed");
    Ok(())
}
