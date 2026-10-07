//! `mantisd local`: every service role in one process, through the
//! services' `LocalCluster`, for development.
//!
//! It also writes what a process-per-role cell host needs to join it: the
//! cluster key and a registry signed with the deploy key, listing every
//! role at its ephemeral address with a health endpoint of its own, plus
//! the cell host the configuration names. So a package's
//! `<binary> node cell-host --config FILE` can run against a local cluster
//! exactly as against separate processes.
//!
//! ```toml
//! [local]
//! bind = "127.0.0.1"                    # every role's RPC, on ephemeral ports (loopback only)
//! dashboard = "127.0.0.1:7480"          # the Ops dashboard
//! operator_token_out = "state/operator.token"   # a fresh token, written at start
//! dashboard_cert_out = "state/ops-cert.der"
//! store = "memory"                      # or "postgres" with postgres_file
//! deploy_key = "keys/deploy.pk8"        # signs the registry written below
//! registry_out = "state/registry.toml"
//! cluster_key_out = "state/cluster.key"
//! cell_host_rpc = "127.0.0.1:7520"      # optional: a cell host to list in the registry
//! cell_host_health = "127.0.0.1:7620"
//! cells = [1, 2, 3]
//! cell_host_tls_out = "state"           # where the cell host's certificate and key are written
//! ```
//!
//! Every role serves mutual TLS. Every start makes a new cluster CA in memory (its private key never
//! leaves the process), issues every role's certificate and the listed
//! cell host's (`cell-host-local.crt` and `.key` in `cell_host_tls_out`),
//! and puts the CA in the registry it signs.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use mantis_core::module::toml;
use mantis_services::cluster::{ClusterConfig, LocalCluster, StoreChoice};
use mantis_services::host::Role;
use mantis_services::tls::TlsIdentity;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::drain::Drain;
use crate::fields::{FieldError, Fields};
use crate::health::{HealthServer, Phase, Status};
use crate::keys;
use crate::matrix;
use crate::pki;
use crate::registry::{Instance, Registry, sign};

/// The local cluster's name (in the registry and every certificate).
pub const CLUSTER: &str = "local";

/// The cell host's instance name in the local registry.
pub const CELL_HOST: &str = "cell-host-local";

/// `mantisd local`'s configuration.
#[derive(Clone, Debug)]
pub struct LocalConfig {
    /// Where every role binds (loopback).
    pub bind: IpAddr,
    /// The dashboard's listener.
    pub dashboard: SocketAddr,
    /// Where the operator token is written.
    pub operator_token_out: PathBuf,
    /// Where the dashboard certificate is written.
    pub dashboard_cert_out: Option<PathBuf>,
    /// The PostgreSQL connection string file, or `None` for memory.
    pub postgres_file: Option<PathBuf>,
    /// The deploy key that signs the written registry.
    pub deploy_key: PathBuf,
    /// Where the registry is written.
    pub registry_out: PathBuf,
    /// Where the cluster key is written.
    pub cluster_key_out: PathBuf,
    /// A cell host to list: rpc, health, cells, and where its certificate
    /// and key are written.
    pub cell_host: Option<(SocketAddr, SocketAddr, Vec<u64>, PathBuf)>,
}

impl LocalConfig {
    /// Reads `path`.
    ///
    /// # Errors
    /// Unreadable or refused.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text, path).map_err(String::from)
    }

    /// Parses `text` read from `path`.
    ///
    /// # Errors
    /// [`FieldError`].
    pub fn parse(text: &str, path: &Path) -> Result<Self, FieldError> {
        let file = path.display().to_string();
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let doc = toml::parse(text).map_err(|e| FieldError(format!("{file}: {e}")))?;
        if let Some(t) = doc
            .tables
            .iter()
            .find(|t| t.name != "local" && !(t.name.is_empty() && t.entries.is_empty()))
        {
            return Err(FieldError(format!(
                "{file}: unknown table [{}] (only [local])",
                t.name
            )));
        }
        let t = doc
            .table("local")
            .ok_or_else(|| FieldError(format!("{file}: missing [local]")))?;
        let mut f = Fields::new(&file, t);
        let bind_text = f.str("bind")?;
        let bind: IpAddr = bind_text.parse().map_err(|_| f.error("bind", "an IP address"))?;
        if !bind.is_loopback() {
            return Err(f.error("bind", "local mode binds loopback only"));
        }
        let dashboard = f.addr("dashboard")?;
        let operator_token_out = f.path("operator_token_out", base)?;
        let dashboard_cert_out = f.opt_path("dashboard_cert_out", base)?;
        let postgres_file = match f.str("store")? {
            "memory" => None,
            "postgres" => Some(f.path("postgres_file", base)?),
            _ => return Err(f.error("store", "\"memory\" or \"postgres\"")),
        };
        let deploy_key = f.path("deploy_key", base)?;
        let registry_out = f.path("registry_out", base)?;
        let cluster_key_out = f.path("cluster_key_out", base)?;
        let cell_host = if t.get("cells").is_some() {
            let rpc = f.addr("cell_host_rpc")?;
            let health = f.addr("cell_host_health")?;
            let cells = f.uints("cells")?;
            let tls_out = f.path("cell_host_tls_out", base)?;
            Some((rpc, health, cells, tls_out))
        } else {
            None
        };
        f.finish()?;
        Ok(Self {
            bind,
            dashboard,
            operator_token_out,
            dashboard_cert_out,
            postgres_file,
            deploy_key,
            registry_out,
            cluster_key_out,
            cell_host,
        })
    }
}

/// Every service role's mutual-TLS identity from the in-memory CA
/// (instance `<role>-local`, the name the registry lists).
fn role_identities(
    ca: &pki::CaFiles,
    bind: IpAddr,
    validity: pki::Validity,
) -> Result<BTreeMap<Role, Arc<TlsIdentity>>, String> {
    let mut identities = BTreeMap::new();
    for role in matrix::DEPLOYED.into_iter().filter(|r| *r != Role::Cell) {
        let instance = format!("{}-local", matrix::name(role));
        let leaf = pki::issue(ca, CLUSTER, role, &instance, &[bind], validity)?;
        let id = TlsIdentity::from_pem(
            ca.cert_pem.as_bytes(),
            leaf.cert_pem.as_bytes(),
            leaf.key_pem.as_bytes(),
        )
        .map_err(|e| format!("{instance}: {e}"))?;
        identities.insert(role, Arc::new(id));
    }
    Ok(identities)
}

/// Runs every service role in this process until a drain.
///
/// # Errors
/// Why the cluster could not start.
pub fn run(config: &LocalConfig, stdin_eof: bool) -> Result<(), String> {
    let deploy = keys::read_key_pair(&config.deploy_key)?;
    let validity = pki::Validity::starting_now(std::time::SystemTime::now(), pki::LEAF_DAYS);
    let ca = pki::new_ca(CLUSTER, validity)?;
    let mut cc = ClusterConfig::local();
    cc.bind = config.bind;
    cc.dashboard.listen = config.dashboard;
    let token = mantis_services::cluster::operator_token()?;
    keys::write_secret(&config.operator_token_out, token.as_bytes())?;
    cc.dashboard.operators.insert(token, "operator".to_owned());
    if let Some(file) = &config.postgres_file {
        let conn = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        cc.store = StoreChoice::Postgres(conn.trim().to_owned());
    }
    cc.tls = Some(role_identities(&ca, config.bind, validity)?);
    let cluster = LocalCluster::start(&cc)?;
    if let Some(path) = &config.dashboard_cert_out {
        keys::write_secret(path, &cluster.dashboard_cert)?;
    }
    keys::write_secret(&config.cluster_key_out, keys::hex(&cluster.key).as_bytes())?;
    let handle = cluster.handle();
    let drain = Drain::install(&handle, stdin_eof);
    // One health endpoint per role, so the registry lists each at its own
    // address as process-per-role does.
    let mut health = Vec::new();
    let mut instances = Vec::new();
    for role in matrix::DEPLOYED {
        let Some(rpc) = cluster.addr(role) else {
            continue;
        };
        let name = format!("{}-local", matrix::name(role));
        let status = Status::new(matrix::name(role), &name);
        status.set(Phase::Ready, "");
        let server = handle.block_on(HealthServer::bind(
            SocketAddr::new(config.bind, 0),
            status.clone(),
        ))?;
        instances.push(Instance {
            name,
            role,
            rpc: rpc.into(),
            health: server.addr().into(),
            cells: Vec::new(),
            lease_owner: None,
        });
        health.push((status, server));
    }
    if let Some((rpc, health_addr, cells, _)) = &config.cell_host {
        instances.push(Instance {
            name: CELL_HOST.to_owned(),
            role: Role::Cell,
            rpc: (*rpc).into(),
            health: (*health_addr).into(),
            cells: cells.clone(),
            lease_owner: None,
        });
        for cell in cells {
            cluster.try_add_cell(*cell, *rpc)?;
        }
    }
    let live_key: [u8; keys::PUBLIC_KEY_BYTES] = cluster
        .ops()
        .public_key()
        .try_into()
        .map_err(|_| "the live-data public key is 32 bytes")?;
    // A start time in seconds: every restart publishes a newer registry.
    let serial = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_secs());
    let ca_der = pki::pem_certs(ca.cert_pem.as_bytes())?
        .into_iter()
        .next()
        .ok_or("the local CA")?;
    let registry = Registry {
        serial,
        cluster: CLUSTER.to_owned(),
        live_key,
        cas: vec![ca_der],
        instances,
    };
    if let Some((rpc, _, _, out)) = &config.cell_host {
        let leaf = pki::issue(&ca, CLUSTER, Role::Cell, CELL_HOST, &[rpc.ip()], validity)?;
        keys::write_secret(&out.join(keys::files::cert(CELL_HOST)), leaf.cert_pem.as_bytes())?;
        keys::write_secret(&out.join(keys::files::key(CELL_HOST)), leaf.key_pem.as_bytes())?;
    }
    let signed = sign(&registry.render(), &deploy);
    keys::write_secret(&config.registry_out, signed.as_bytes())?;
    print!("{}", cluster.graph());
    print!("{}", registry.summary());
    println!(
        "mantisd local: registry written to {}, cluster key to {}, operator token to {}",
        config.registry_out.display(),
        config.cluster_key_out.display(),
        config.operator_token_out.display()
    );
    let why = handle.block_on(drain.wait());
    println!("mantisd local: draining ({why})");
    stop(&handle, health, cluster, why);
    Ok(())
}

/// Marks every role draining, closes the health endpoints, stops the roles.
fn stop(
    handle: &tokio::runtime::Handle,
    health: Vec<(Status, HealthServer)>,
    cluster: LocalCluster,
    why: &str,
) {
    for (status, _) in &health {
        status.set(Phase::Draining, why);
    }
    {
        let _guard = handle.enter();
        drop(health);
    }
    drop(cluster);
    println!("mantisd local: stopped");
}
