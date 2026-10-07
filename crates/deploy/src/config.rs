//! A node's configuration: one TOML file per process, read strictly.
//!
//! ```toml
//! [node]
//! role = "social"                   # account, realm, social, matchmaking, persist, ops, cell-host
//! instance = "social-1"             # its name in the registry
//! registry = "registry.toml"        # the signed registry: a file, "dir:DIR", or "https://host[:port]/path"
//! registry_ca = "keys/deploy-ca.pem" # https only: the CA its server's certificate must chain to
//! registry_refresh_s = 10           # how often the source is read again (0: never; read once at start)
//! deploy_key = "keys/deploy.pub"    # the public key the registry must verify with
//! registry_min_serial = 1           # optional: refuse an older registry (rollback)
//! cluster_key = "keys/cluster.key"  # the RPC cluster key (a file, never an environment variable)
//! tls_cert = "keys/social-1.crt"    # this instance's certificate (mantisd certs), PEM
//! tls_key = "keys/social-1.key"     # its private key, PEM (a secret)
//! listen_rpc = "0.0.0.0:7503"       # where this process binds its RPC
//! listen_health = "0.0.0.0:7603"    # where it binds its health endpoint
//! ready_timeout_s = 120             # how long readiness waits for dependencies
//! drain_grace_ms = 10000            # how long a drain may take
//! lease_ttl_ms = 3000               # account, realm, social, matchmaking, ops: how long the role's
//!                                   # lease lasts unless renewed (a standby takes over within it)
//!
//! [persist]                         # only for role = "persist"
//! store = "postgres"                # or "memory" (development: lost at exit)
//! postgres_file = "secrets/pg.conn" # the libpq connection string, in a file
//! postgres_schema = "mantis"
//!
//! [ops]                             # only for role = "ops"
//! live_key = "keys/live.pk8"        # signs live changes; its public half is in the registry
//! operator_token = "keys/operator.token"
//! operator_name = "operator"
//! dashboard = "127.0.0.1:7480"      # the HTTPS dashboard: its own listener, never a game port
//! dashboard_allow_remote = false    # true only behind a reviewed, operator-only network
//! dashboard_allowed_peers = ["10.79.0.1/32"]  # optional: only these peers reach TLS (CIDR)
//! dashboard_cert_out = "state/ops-cert.der"
//! game_ports = [7400, 7401]         # the dashboard refuses to share one
//!
//! [matchmaking]                     # only for role = "matchmaking"
//! group = 2                         # characters per match
//!
//! [cell_host]                       # only for role = "cell-host"
//! advertise = "127.0.0.1:7400"      # the game address the realm hands to clients
//! state = "state/cells"             # snapshots and logs survive here across restarts
//! poll_ms = 100                     # how often the link polls the service roles
//! snapshot_every_ticks = 150
//! game_cert = "tls/game.crt"        # optional: the game listener's chain (PEM, leaf first);
//! game_key = "tls/game.key"         #   both or neither; without them the package's development issuer
//! game_ca = "tls/clients-trust.pem" # optional: chains must verify against it for the advertised host
//!
//! [package]                         # only for role = "cell-host": the package's own keys,
//!                                   # read as strictly by the package (PackageSettings)
//! ```
//!
//! Every key is documented above. An unknown table or key, a missing
//! required key, a value of the wrong type or out of range, or a table for
//! another role is refused with its line. Units are in key suffixes
//! (`_ms`, `_s`, `_ticks`). Relative paths are relative to the
//! configuration file's directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mantis_core::module::toml::{self, Table, Value};
use mantis_services::host::Role;

use crate::fields::{FieldError, Fields};
use crate::matrix;

/// The persistence writer's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Store {
    /// In memory: development only, lost when the process exits.
    Memory,
    /// PostgreSQL.
    Postgres {
        /// The file holding the libpq connection string (it holds a
        /// password: a file, never an environment variable).
        conn_file: PathBuf,
        /// The schema.
        schema: String,
    },
}

/// `[persist]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistConfig {
    /// The store.
    pub store: Store,
}

/// `[ops]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpsConfig {
    /// The live-data signing key (PKCS#8).
    pub live_key: PathBuf,
    /// The operator token file.
    pub operator_token: PathBuf,
    /// The operator's name in the audit trail.
    pub operator_name: String,
    /// The dashboard's listener.
    pub dashboard: SocketAddr,
    /// A non-loopback dashboard listener is allowed.
    pub dashboard_allow_remote: bool,
    /// The peers allowed to reach the dashboard's TLS, as CIDR prefixes
    /// (empty: any).
    pub dashboard_allowed_peers: Vec<(std::net::IpAddr, u8)>,
    /// Where the dashboard's development certificate is written.
    pub dashboard_cert_out: Option<PathBuf>,
    /// Game ports the dashboard must not share.
    pub game_ports: Vec<u16>,
}

/// `[matchmaking]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchmakingConfig {
    /// Characters per match.
    pub group: usize,
}

/// `[cell_host]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellHostConfig {
    /// The game address registered with the realm.
    pub advertise: String,
    /// Snapshots and logs.
    pub state: PathBuf,
    /// How often the link polls the service roles.
    pub poll: Duration,
    /// Ticks between snapshots.
    pub snapshot_every_ticks: u64,
    /// The game listener's certificate chain and key (PEM), when the
    /// operator provides them; `None`: the package's development issuer.
    pub game_tls: Option<GameTlsFiles>,
}

/// The game listener's certificate files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameTlsFiles {
    /// The chain, leaf first (PEM).
    pub cert: PathBuf,
    /// Its private key (PKCS#8 PEM).
    pub key: PathBuf,
    /// Optional: the CA bundle (PEM) clients trust; when set, every chain
    /// (at start and on each rotation) must verify against it for the
    /// advertised host before it is used.
    pub ca: Option<PathBuf>,
}

/// A node's configuration.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// The file it was read from.
    pub file: PathBuf,
    /// Its role.
    pub role: Role,
    /// Its instance name in the registry.
    pub instance: String,
    /// Where the signed registry comes from.
    pub registry: crate::source::Source,
    /// How often the registry source is read again (`ZERO`: never).
    pub registry_refresh: Duration,
    /// The deploy public key file.
    pub deploy_key: PathBuf,
    /// The oldest registry serial accepted.
    pub registry_min_serial: u64,
    /// The cluster key file.
    pub cluster_key: PathBuf,
    /// This instance's TLS certificate (PEM).
    pub tls_cert: PathBuf,
    /// Its private key (PEM).
    pub tls_key: PathBuf,
    /// Where the RPC binds.
    pub listen_rpc: SocketAddr,
    /// Where the health endpoint binds.
    pub listen_health: SocketAddr,
    /// How long readiness waits for dependencies.
    pub ready_timeout: Duration,
    /// How long a drain may take.
    pub drain_grace: Duration,
    /// A failover role's lease lifetime.
    pub lease_ttl: Duration,
    /// `[persist]`.
    pub persist: Option<PersistConfig>,
    /// `[ops]`.
    pub ops: Option<OpsConfig>,
    /// `[matchmaking]`.
    pub matchmaking: Option<MatchmakingConfig>,
    /// `[cell_host]`.
    pub cell_host: Option<CellHostConfig>,
    /// `[package]`, for the package's cell host to read.
    pub package: Table,
}

const ROLE_TABLES: [(&str, Role); 4] = [
    ("persist", Role::Persist),
    ("ops", Role::Ops),
    ("matchmaking", Role::Matchmaking),
    ("cell_host", Role::Cell),
];

impl NodeConfig {
    /// Reads the configuration at `path`.
    ///
    /// # Errors
    /// The file cannot be read, or is refused.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text, path).map_err(String::from)
    }

    /// Parses configuration `text` read from `path`.
    ///
    /// # Errors
    /// [`FieldError`].
    pub fn parse(text: &str, path: &Path) -> Result<Self, FieldError> {
        let file = path.display().to_string();
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let doc = toml::parse(text).map_err(|e| FieldError(format!("{file}: {e}")))?;
        for t in &doc.tables {
            let known =
                t.name == "node" || t.name == "package" || ROLE_TABLES.iter().any(|(n, _)| *n == t.name);
            if t.name.is_empty() {
                if let Some(e) = t.entries.first() {
                    return Err(FieldError(format!(
                        "{file} line {}: `{}` is outside any table",
                        e.line, e.key
                    )));
                }
            } else if !known {
                return Err(FieldError(format!("{file}: unknown table [{}]", t.name)));
            }
        }
        let node = doc
            .table("node")
            .ok_or_else(|| FieldError(format!("{file}: missing [node]")))?;
        let n = node_table(&file, base, node)?;
        let role = n.role;
        for (name, owner) in ROLE_TABLES {
            if doc.table(name).is_some() && owner != role {
                return Err(FieldError(format!(
                    "{file}: [{name}] configures a {} node; this node is a {}",
                    matrix::name(owner),
                    matrix::name(role)
                )));
            }
        }
        if doc.table("package").is_some() && role != Role::Cell {
            return Err(FieldError(format!(
                "{file}: [package] configures a cell-host node; this node is a {}",
                matrix::name(role)
            )));
        }
        let required = |name: &str| {
            doc.table(name)
                .ok_or_else(|| FieldError(format!("{file}: a {} node needs [{name}]", matrix::name(role))))
        };
        let persist = match role {
            Role::Persist => Some(persist_config(&file, base, required("persist")?)?),
            _ => None,
        };
        let ops = match role {
            Role::Ops => Some(ops_config(&file, base, required("ops")?)?),
            _ => None,
        };
        let matchmaking = match role {
            Role::Matchmaking => {
                let mut f = Fields::new(&file, required("matchmaking")?);
                let group = usize::try_from(f.uint("group", 1..=64)?).unwrap_or(1);
                f.finish()?;
                Some(MatchmakingConfig { group })
            }
            _ => None,
        };
        let cell_host = match role {
            Role::Cell => Some(cell_config(&file, base, required("cell_host")?)?),
            _ => None,
        };
        let package = doc.table("package").cloned().unwrap_or(Table {
            name: "package".to_owned(),
            entries: Vec::new(),
        });
        Ok(Self {
            file: path.to_path_buf(),
            role,
            instance: n.instance,
            registry: n.registry,
            registry_refresh: n.registry_refresh,
            deploy_key: n.deploy_key,
            registry_min_serial: n.registry_min_serial,
            cluster_key: n.cluster_key,
            tls_cert: n.tls_cert,
            tls_key: n.tls_key,
            listen_rpc: n.listen_rpc,
            listen_health: n.listen_health,
            ready_timeout: n.ready_timeout,
            drain_grace: n.drain_grace,
            lease_ttl: n.lease_ttl,
            persist,
            ops,
            matchmaking,
            cell_host,
            package,
        })
    }

    /// Reads the package's keys from `[package]` (a cell host's package).
    #[must_use]
    pub fn package_settings(&self) -> PackageSettings {
        PackageSettings {
            file: self.file.display().to_string(),
            base: self
                .file
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            table: self.package.clone(),
        }
    }
}

struct NodeTable {
    role: Role,
    instance: String,
    registry: crate::source::Source,
    registry_refresh: Duration,
    deploy_key: PathBuf,
    registry_min_serial: u64,
    cluster_key: PathBuf,
    tls_cert: PathBuf,
    tls_key: PathBuf,
    listen_rpc: SocketAddr,
    listen_health: SocketAddr,
    ready_timeout: Duration,
    drain_grace: Duration,
    lease_ttl: Duration,
}

fn node_table(file: &str, base: &Path, node: &Table) -> Result<NodeTable, FieldError> {
    let mut f = Fields::new(file, node);
    let role_name = f.str("role")?;
    let role = matrix::parse(role_name).ok_or_else(|| {
        f.error(
            "role",
            "one of account, realm, social, matchmaking, persist, ops, cell-host",
        )
    })?;
    let instance = f.str("instance")?.to_owned();
    let registry_ca = f.opt_path("registry_ca", base)?;
    let spec = f.str("registry")?;
    let registry =
        crate::source::Source::parse(spec, base, registry_ca).map_err(|e| f.error("registry", &e))?;
    let registry_refresh = f.duration_or("registry_refresh_s", 0..=86_400, 10)?;
    let deploy_key = f.path("deploy_key", base)?;
    let registry_min_serial = u64::try_from(f.int_or("registry_min_serial", 1..=i64::MAX, 1)?).unwrap_or(1);
    let cluster_key = f.path("cluster_key", base)?;
    let tls_cert = f.path("tls_cert", base)?;
    let tls_key = f.path("tls_key", base)?;
    let listen_rpc = f.addr("listen_rpc")?;
    let listen_health = f.addr("listen_health")?;
    if listen_rpc == listen_health {
        return Err(f.error("listen_health", "the health endpoint has its own listener"));
    }
    let ready_timeout = f.duration_or("ready_timeout_s", 1..=3600, 120)?;
    let drain_grace = f.duration_or("drain_grace_ms", 0..=600_000, 10_000)?;
    let lease_ttl = f.duration_or("lease_ttl_ms", 300..=600_000, 3000)?;
    f.finish()?;
    Ok(NodeTable {
        role,
        instance,
        registry,
        registry_refresh,
        deploy_key,
        registry_min_serial,
        cluster_key,
        tls_cert,
        tls_key,
        listen_rpc,
        listen_health,
        ready_timeout,
        drain_grace,
        lease_ttl,
    })
}

fn persist_config(file: &str, base: &Path, t: &Table) -> Result<PersistConfig, FieldError> {
    let mut f = Fields::new(file, t);
    let store = match f.str("store")? {
        "memory" => Store::Memory,
        "postgres" => Store::Postgres {
            conn_file: f.path("postgres_file", base)?,
            schema: f.opt_str("postgres_schema")?.unwrap_or("mantis").to_owned(),
        },
        _ => return Err(f.error("store", "\"memory\" or \"postgres\"")),
    };
    if let Store::Postgres { schema, .. } = &store
        && !(1..=48).contains(&schema.len())
    {
        return Err(f.error("postgres_schema", "1 to 48 characters"));
    }
    if let Store::Postgres { schema, .. } = &store
        && !schema
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(f.error("postgres_schema", "a-z, 0-9 and _ only"));
    }
    f.finish()?;
    Ok(PersistConfig { store })
}

fn ops_config(file: &str, base: &Path, t: &Table) -> Result<OpsConfig, FieldError> {
    let mut f = Fields::new(file, t);
    let live_key = f.path("live_key", base)?;
    let operator_token = f.path("operator_token", base)?;
    let operator_name = f.opt_str("operator_name")?.unwrap_or("operator").to_owned();
    if operator_name.is_empty() || operator_name.len() > 32 {
        return Err(f.error("operator_name", "1 to 32 characters"));
    }
    let dashboard = f.addr("dashboard")?;
    let dashboard_allow_remote = f.bool_or("dashboard_allow_remote", false)?;
    let dashboard_allowed_peers = match t.get("dashboard_allowed_peers") {
        None => Vec::new(),
        Some(_) => f
            .strs("dashboard_allowed_peers")?
            .iter()
            .map(|c| {
                cidr(c).ok_or_else(|| f.error("dashboard_allowed_peers", &format!("{c:?} is not ip/prefix")))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let dashboard_cert_out = f.opt_path("dashboard_cert_out", base)?;
    let ports = match t.get("game_ports") {
        None => Vec::new(),
        Some(_) => f.uints("game_ports")?,
    };
    let game_ports = ports
        .into_iter()
        .map(|p| u16::try_from(p).map_err(|_| f.error("game_ports", "ports are 1 to 65535")))
        .collect::<Result<Vec<u16>, _>>()?;
    f.finish()?;
    Ok(OpsConfig {
        live_key,
        operator_token,
        operator_name,
        dashboard,
        dashboard_allow_remote,
        dashboard_allowed_peers,
        dashboard_cert_out,
        game_ports,
    })
}

/// `ip/prefix`, the prefix within the address family's width.
fn cidr(text: &str) -> Option<(std::net::IpAddr, u8)> {
    let (ip, prefix) = text.split_once('/')?;
    let ip: std::net::IpAddr = ip.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((ip, prefix))
}

fn cell_config(file: &str, base: &Path, t: &Table) -> Result<CellHostConfig, FieldError> {
    let mut f = Fields::new(file, t);
    let advertise = f.str("advertise")?.to_owned();
    if crate::target::Target::parse(&advertise).is_err() {
        return Err(f.error("advertise", "the game address clients dial, host:port"));
    }
    let state = f.path("state", base)?;
    let poll = f.duration_or("poll_ms", 5..=10_000, 100)?;
    let snapshot_every_ticks = f.uint("snapshot_every_ticks", 1..=1_000_000)?;
    let game_tls = match (f.opt_path("game_cert", base)?, f.opt_path("game_key", base)?) {
        (Some(cert), Some(key)) => Some(GameTlsFiles {
            cert,
            key,
            ca: f.opt_path("game_ca", base)?,
        }),
        (None, None) => {
            if t.get("game_ca").is_some() {
                return Err(f.error("game_ca", "only with game_cert and game_key"));
            }
            None
        }
        _ => return Err(f.error("game_cert", "game_cert and game_key come together")),
    };
    f.finish()?;
    Ok(CellHostConfig {
        advertise,
        state,
        poll,
        snapshot_every_ticks,
        game_tls,
    })
}

/// The `[package]` table of a cell host's configuration, read as strictly
/// as the rest: the package reads each key it knows inside
/// [`PackageSettings::read`], which then refuses every key it did not read
/// ([`Fields::finish`]).
pub struct PackageSettings {
    file: String,
    base: PathBuf,
    table: Table,
}

impl PackageSettings {
    /// Reads the keys with `read`, then refuses any key it did not read.
    ///
    /// # Errors
    /// What `read` refused, or an unknown key.
    pub fn read<T>(
        &self,
        read: impl FnOnce(&mut Fields<'_>, &Path) -> Result<T, FieldError>,
    ) -> Result<T, String> {
        let mut f = Fields::new(&self.file, &self.table);
        let out = read(&mut f, &self.base)?;
        f.finish()?;
        Ok(out)
    }

    /// The raw value of `key` (diagnostics).
    #[must_use]
    pub fn raw(&self, key: &str) -> Option<&Value> {
        self.table.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "[node]\nrole = \"social\"\ninstance = \"social-1\"\nregistry = \"r.toml\"\n\
        deploy_key = \"k/deploy.pub\"\ncluster_key = \"k/cluster.key\"\n\
        tls_cert = \"k/social-1.crt\"\ntls_key = \"k/social-1.key\"\n\
        listen_rpc = \"127.0.0.1:7503\"\nlisten_health = \"127.0.0.1:7603\"\n";

    fn parse(text: &str) -> Result<NodeConfig, FieldError> {
        NodeConfig::parse(text, Path::new("cfg/node.toml"))
    }

    #[test]
    fn a_minimal_node_reads_with_defaults_and_relative_paths() {
        let c = parse(NODE).unwrap();
        assert_eq!(c.role, Role::Social);
        assert_eq!(
            c.registry,
            crate::source::Source::File(Path::new("cfg").join("r.toml"))
        );
        assert_eq!(c.registry_refresh, Duration::from_secs(10));
        assert_eq!(c.ready_timeout, Duration::from_secs(120));
        assert_eq!(c.drain_grace, Duration::from_millis(10_000));
        assert!(c.persist.is_none() && c.cell_host.is_none());
    }

    #[test]
    fn strictness() {
        let e = |extra: &str| parse(&format!("{NODE}{extra}")).unwrap_err().0;
        assert!(e("colour = \"red\"\n").contains("has no key `colour`"));
        assert!(e("ready_timeout_s = 0\n").contains("outside 1..=3600"));
        assert!(e("ready_timeout_s = \"60\"\n").contains("expected an integer"));
        assert!(e("[persist]\nstore = \"memory\"\n").contains("configures a persist node"));
        assert!(e("[gateway]\n").contains("unknown table [gateway]"));
        assert!(e("[package]\nx = 1\n").contains("configures a cell-host node"));
        let missing = parse(&NODE.replace("instance = \"social-1\"\n", ""))
            .unwrap_err()
            .0;
        assert!(missing.contains("`instance`: missing"), "{missing}");
        let top = parse(&format!("x = 1\n{NODE}")).unwrap_err().0;
        assert!(top.contains("outside any table"), "{top}");
        let same = parse(&NODE.replace("7603", "7503")).unwrap_err().0;
        assert!(same.contains("its own listener"), "{same}");
        let role = parse(&NODE.replace("\"social\"", "\"gateway\"")).unwrap_err().0;
        assert!(role.contains("one of account"), "{role}");
    }

    #[test]
    fn dashboard_peers_are_cidr_prefixes() {
        assert_eq!(cidr("10.79.0.1/32"), Some(("10.79.0.1".parse().unwrap(), 32)));
        assert_eq!(cidr("::1/128"), Some(("::1".parse().unwrap(), 128)));
        assert_eq!(cidr("10.0.0.0/33"), None);
        assert_eq!(cidr("10.0.0.0"), None);
        assert_eq!(cidr("host/8"), None);
    }

    #[test]
    fn role_tables_are_required_and_typed() {
        let persist = NODE.replace("\"social\"", "\"persist\"");
        assert!(parse(&persist).unwrap_err().0.contains("needs [persist]"));
        let pg = parse(&format!(
            "{persist}[persist]\nstore = \"postgres\"\npostgres_file = \"pg.conn\"\n"
        ))
        .unwrap();
        assert_eq!(
            pg.persist.unwrap().store,
            Store::Postgres {
                conn_file: Path::new("cfg").join("pg.conn"),
                schema: "mantis".to_owned()
            }
        );
        let bad = parse(&format!(
            "{persist}[persist]\nstore = \"memory\"\npostgres_file = \"x\"\n"
        ));
        assert!(bad.unwrap_err().0.contains("has no key `postgres_file`"));

        let cell = NODE.replace("\"social\"", "\"cell-host\"");
        let c = parse(&format!(
            "{cell}[cell_host]\nadvertise = \"127.0.0.1:7400\"\nstate = \"s\"\nsnapshot_every_ticks = 150\n\
             [package]\nseed = 4\n"
        ))
        .unwrap();
        let host = c.cell_host.clone().unwrap();
        assert_eq!(
            (host.poll, host.snapshot_every_ticks),
            (Duration::from_millis(100), 150)
        );
        let seed = c.package_settings().read(|f, _| f.uint("seed", 0..=9)).unwrap();
        assert_eq!(seed, 4);
        let unread = c.package_settings().read(|_, _| Ok(()));
        assert!(unread.unwrap_err().contains("has no key `seed`"));
    }
}
