//! A process-per-role cluster on loopback, for tests: keys, a signed
//! registry, one configuration file per instance, and child processes
//! (`mantisd <role>`, or the package's binary for cell hosts) the test
//! starts, kills, restarts and drains.

#![allow(dead_code)]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mantis_deploy::health::probe_blocking;
use mantis_deploy::keys;
use mantis_deploy::matrix;
use mantis_deploy::pki;
use mantis_deploy::registry::{Instance, Registry, sign};
use mantis_services::host::Role;

/// The ports the harness hands out: below the operating systems' ephemeral
/// ranges (Windows 49152-65535, Linux 32768-60999). A port from the
/// ephemeral range is unsafe between being chosen and being bound by a
/// child: any process may take it as an outgoing source port, and on
/// Windows the NAT service reserves 100-port blocks out of that range
/// whenever a container publishes a port, after which a bind there fails
/// with "access denied" (measured: a cell host exiting before ready).
const PORTS: std::ops::Range<u16> = 20_000..32_000;

/// Ports this process has handed out already (several clusters run in
/// parallel test threads).
static TAKEN: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// `n` loopback ports free for TCP **and** UDP (a QUIC game listener binds
/// UDP on the same number), from [`PORTS`], never handed out twice by this
/// process. The scan starts at an offset from the process id, so test
/// binaries running at the same time start apart; each port is checked by
/// binding it.
pub fn free_ports(n: usize) -> Vec<u16> {
    let span = PORTS.end - PORTS.start;
    let offset = u16::try_from(std::process::id() % u32::from(span)).unwrap();
    let mut taken = TAKEN.lock().unwrap();
    let mut out = Vec::with_capacity(n);
    for k in 0..span {
        if out.len() == n {
            break;
        }
        let port = PORTS.start + (offset + k) % span;
        if taken.contains(&port) {
            continue;
        }
        let free = TcpListener::bind(("127.0.0.1", port)).is_ok()
            && std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok();
        if free {
            taken.push(port);
            out.push(port);
        }
    }
    assert_eq!(out.len(), n, "only {} free ports in {PORTS:?}", out.len());
    out
}

fn at(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A registry target on loopback.
fn target(port: u16) -> mantis_deploy::target::Target {
    at(port).into()
}

/// The loopback address of a target the harness wrote (always an IP).
pub fn addr(t: &mantis_deploy::target::Target) -> SocketAddr {
    t.socket_addr().unwrap()
}

/// The test clusters' name (in the registry and every certificate).
pub const CLUSTER: &str = "test";

/// The persistence writer's store for a test cluster.
#[derive(Clone, Debug)]
pub enum TestStore {
    /// In memory.
    Memory,
    /// PostgreSQL: the connection string and a schema of the test's own.
    Postgres(String, String),
}

/// A cell host instance to list.
#[derive(Clone, Debug)]
pub struct CellSpec {
    /// Instance name.
    pub name: &'static str,
    /// Cells.
    pub cells: Vec<u64>,
    /// `[package]` text for the package's cell host.
    pub package: String,
    /// The game listener presents a chain from the cluster CA (files
    /// `keys/game-<name>.crt` and `.key`, trust `keys/ca.crt`) instead of
    /// the package's development certificate.
    pub game_tls: bool,
}

/// One running child.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The threads copying its stdout and stderr: joined once it exits, so
    /// a failure message carries everything it printed.
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl Proc {
    /// Waits for the output copies to finish (the child has exited).
    fn finish_output(&mut self) {
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
    }
}

/// A test cluster.
pub struct Cluster {
    /// Its directory (keys, registry, configurations, state).
    pub dir: PathBuf,
    /// The registry it signed.
    pub registry: Registry,
    /// The cluster key.
    pub key: Vec<u8>,
    /// The operator token.
    pub token: String,
    /// The Ops dashboard's address.
    pub dashboard: SocketAddr,
    /// Each cell host's game address (QUIC) and a second game port (TCP).
    pub game: BTreeMap<String, (SocketAddr, SocketAddr)>,
    configs: BTreeMap<String, PathBuf>,
    procs: BTreeMap<String, Proc>,
    output: BTreeMap<String, Arc<Mutex<String>>>,
    starts: BTreeMap<String, u32>,
}

/// The service instance name of `role`.
pub fn service(role: Role) -> String {
    format!("{}-1", matrix::name(role))
}

/// The second instance of a failover role.
pub fn standby(role: Role) -> String {
    format!("{}-2", matrix::name(role))
}

/// A failover role's lease lifetime in the tests.
pub const LEASE_TTL_MS: u64 = 1200;

impl Cluster {
    /// Writes keys, a signed registry with one instance of each service
    /// role and the given cell hosts, and every configuration file.
    pub fn new(label: &str, store: &TestStore, cell_hosts: &[CellSpec]) -> Self {
        Self::with_standbys(label, store, cell_hosts, &[])
    }

    /// [`Cluster::new`], with a second instance (`<role>-2`, a standby
    /// while the first holds the lease) of each of `standbys`.
    pub fn with_standbys(label: &str, store: &TestStore, cell_hosts: &[CellSpec], standbys: &[Role]) -> Self {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("deploy-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_dir = dir.join("keys");
        keys::new_keys(&key_dir, CLUSTER, 30).unwrap();
        let key = keys::read_cluster_key(&key_dir.join(keys::files::CLUSTER)).unwrap();
        let token = std::fs::read_to_string(key_dir.join(keys::files::OPERATOR_TOKEN)).unwrap();
        let live_key = keys::read_public_key(&key_dir.join(keys::files::LIVE_PUBLIC)).unwrap();
        let services = [
            Role::Account,
            Role::Realm,
            Role::Persist,
            Role::Social,
            Role::Matchmaking,
            Role::Ops,
        ];
        let mut ports =
            free_ports((services.len() + standbys.len()) * 2 + cell_hosts.len() * 4 + 1).into_iter();
        let mut next = || ports.next().unwrap();
        let mut instances = Vec::new();
        for role in services {
            instances.push(Instance {
                name: service(role),
                role,
                rpc: target(next()),
                health: target(next()),
                cells: Vec::new(),
                lease_owner: None,
            });
        }
        for role in standbys {
            instances.push(Instance {
                name: standby(*role),
                role: *role,
                rpc: target(next()),
                health: target(next()),
                cells: Vec::new(),
                lease_owner: None,
            });
        }
        let mut game = BTreeMap::new();
        for c in cell_hosts {
            instances.push(Instance {
                name: c.name.to_owned(),
                role: Role::Cell,
                rpc: target(next()),
                health: target(next()),
                cells: c.cells.clone(),
                lease_owner: None,
            });
            game.insert(c.name.to_owned(), (at(next()), at(next())));
        }
        let dashboard = at(next());
        let ca = keys::read_ca(&key_dir).unwrap();
        let registry = Registry {
            serial: 1,
            cluster: CLUSTER.to_owned(),
            live_key,
            cas: pki::pem_certs(ca.cert_pem.as_bytes()).unwrap(),
            instances,
        };
        let deploy = keys::read_key_pair(&key_dir.join(keys::files::DEPLOY)).unwrap();
        std::fs::write(dir.join("registry.toml"), sign(&registry.render(), &deploy)).unwrap();
        // Every instance's certificate, as `mantisd certs` issues them.
        let validity = pki::Validity::starting_now(std::time::SystemTime::now(), pki::LEAF_DAYS);
        pki::issue_registry(&ca, &registry, &key_dir, validity, None).unwrap();
        let mut configs = BTreeMap::new();
        for i in &registry.instances {
            let mut text = format!(
                "[node]\nrole = \"{}\"\ninstance = \"{}\"\nregistry = \"registry.toml\"\n\
                 deploy_key = \"keys/{}\"\ncluster_key = \"keys/{}\"\ntls_cert = \"keys/{}\"\n\
                 tls_key = \"keys/{}\"\nlisten_rpc = \"{}\"\n\
                 listen_health = \"{}\"\nready_timeout_s = 60\ndrain_grace_ms = 10000\n\
                 registry_refresh_s = 1\nlease_ttl_ms = {LEASE_TTL_MS}\n",
                matrix::name(i.role),
                i.name,
                keys::files::DEPLOY_PUBLIC,
                keys::files::CLUSTER,
                keys::files::cert(&i.name),
                keys::files::key(&i.name),
                i.rpc,
                i.health
            );
            match i.role {
                Role::Persist => match store {
                    TestStore::Memory => text.push_str("\n[persist]\nstore = \"memory\"\n"),
                    TestStore::Postgres(conn, schema) => {
                        std::fs::write(dir.join("pg.conn"), conn).unwrap();
                        let _ = write!(
                            text,
                            "\n[persist]\nstore = \"postgres\"\npostgres_file = \"pg.conn\"\n\
                             postgres_schema = \"{schema}\"\n"
                        );
                    }
                },
                Role::Ops => {
                    let ports: Vec<String> = game
                        .values()
                        .flat_map(|(q, t): &(SocketAddr, SocketAddr)| {
                            [q.port().to_string(), t.port().to_string()]
                        })
                        .collect();
                    let _ = write!(
                        text,
                        "\n[ops]\nlive_key = \"keys/{}\"\noperator_token = \"keys/{}\"\n\
                         dashboard = \"{dashboard}\"\ndashboard_cert_out = \"ops-cert.der\"\n\
                         dashboard_allowed_peers = [\"127.0.0.1/32\"]\n\
                         game_ports = [{}]\n",
                        keys::files::LIVE,
                        keys::files::OPERATOR_TOKEN,
                        ports.join(", ")
                    );
                }
                Role::Matchmaking => text.push_str("\n[matchmaking]\ngroup = 2\n"),
                Role::Cell => {
                    let spec = cell_hosts.iter().find(|c| c.name == i.name).unwrap();
                    let (quic, _) = game[&i.name];
                    let _ = write!(
                        text,
                        "\n[cell_host]\nadvertise = \"{quic}\"\nstate = \"state/{}\"\npoll_ms = 20\n\
                         snapshot_every_ticks = 150\n",
                        i.name
                    );
                    if spec.game_tls {
                        let ca = keys::read_ca(&key_dir).unwrap();
                        let leaf = pki::issue_server(
                            &ca,
                            "game",
                            &[quic.ip().to_string()],
                            pki::Validity::starting_now(std::time::SystemTime::now(), 7),
                        )
                        .unwrap();
                        std::fs::write(key_dir.join(format!("game-{}.crt", i.name)), &leaf.cert_pem).unwrap();
                        std::fs::write(key_dir.join(format!("game-{}.key", i.name)), &leaf.key_pem).unwrap();
                        let _ = write!(
                            text,
                            "game_cert = \"keys/game-{0}.crt\"\ngame_key = \"keys/game-{0}.key\"\n\
                             game_ca = \"keys/{1}\"\n",
                            i.name,
                            keys::files::CA
                        );
                    }
                    if !spec.package.is_empty() {
                        let _ = write!(text, "\n[package]\n{}", spec.package);
                    }
                }
                Role::Account | Role::Realm | Role::Social | Role::Gateway => {}
            }
            let path = dir.join(format!("{}.toml", i.name));
            std::fs::write(&path, text).unwrap();
            configs.insert(i.name.clone(), path);
        }
        Self {
            dir,
            registry,
            key,
            token,
            dashboard,
            game,
            configs,
            procs: BTreeMap::new(),
            output: BTreeMap::new(),
            starts: BTreeMap::new(),
        }
    }

    /// Publishes `registry` (signed with the deploy key) where every node
    /// reads it, and rewrites the listen ports of any instance it moved.
    /// Running nodes apply it within their refresh (1 s); a moved node's
    /// own listener moves only when it is restarted.
    pub fn publish(&mut self, registry: Registry) {
        let deploy = keys::read_key_pair(&self.dir.join("keys").join(keys::files::DEPLOY)).unwrap();
        let path = self.dir.join("registry.toml");
        let partial = self.dir.join("registry.toml.partial");
        std::fs::write(&partial, sign(&registry.render(), &deploy)).unwrap();
        std::fs::rename(&partial, &path).unwrap();
        for next in &registry.instances {
            let Some(was) = self.registry.instance(&next.name) else {
                continue;
            };
            if (&was.rpc, &was.health) == (&next.rpc, &next.health) {
                continue;
            }
            let config = self.config(&next.name);
            let text = std::fs::read_to_string(&config).unwrap();
            let text = text
                .replace(
                    &format!("listen_rpc = \"{}\"", was.rpc),
                    &format!("listen_rpc = \"{}\"", next.rpc),
                )
                .replace(
                    &format!("listen_health = \"{}\"", was.health),
                    &format!("listen_health = \"{}\"", next.health),
                );
            std::fs::write(&config, text).unwrap();
        }
        self.registry = registry;
    }

    /// The configuration file of `name`.
    pub fn config(&self, name: &str) -> PathBuf {
        self.configs[name].clone()
    }

    /// The TLS identity `mantisd certs` issued to `instance`.
    pub fn tls_of(&self, instance: &str) -> std::sync::Arc<mantis_services::tls::TlsIdentity> {
        let k = self.dir.join("keys");
        let read = |f: String| std::fs::read(k.join(f)).unwrap();
        std::sync::Arc::new(
            mantis_services::tls::TlsIdentity::from_pem(
                &read(keys::files::CA.to_owned()),
                &read(keys::files::cert(instance)),
                &read(keys::files::key(instance)),
            )
            .unwrap(),
        )
    }

    /// A client calling instance `to` as instance `from`, with `from`'s
    /// certificate, over mutual TLS.
    pub fn client(&self, from: &str, to: &str) -> mantis_services::host::rpc::RpcClient {
        let (from_i, to_i) = (self.instance(from), self.instance(to));
        mantis_services::host::rpc::RpcClient::with_tls(
            addr(&to_i.rpc),
            from_i.role,
            self.key.clone(),
            Some(self.tls_of(from)),
            to_i.role,
        )
        .unwrap()
    }

    /// The registry entry of `name`.
    pub fn instance(&self, name: &str) -> &Instance {
        self.registry.instance(name).unwrap()
    }

    /// Starts `name` with `program` (`mantisd`, or a package binary with
    /// its `node` subcommand) and its configuration; stdin closes to drain.
    pub fn start_with(&mut self, name: &str, program: &Path, prefix: &[&str]) {
        assert!(!self.procs.contains_key(name), "{name} is already running");
        let role = matrix::name(self.instance(name).role);
        let mut child = Command::new(program)
            .args(prefix)
            .arg(role)
            .arg("--config")
            .arg(self.config(name))
            .arg("--drain-on-stdin-eof")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{}: {e}", program.display()));
        let out = Arc::clone(self.output.entry(name.to_owned()).or_default());
        let n = self.starts.entry(name.to_owned()).or_default();
        *n += 1;
        let run = *n;
        let _ = writeln!(out.lock().unwrap(), "---- start {run} ----");
        let mut readers = Vec::new();
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let out = Arc::clone(&out);
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    let _ = writeln!(out.lock().unwrap(), "{line}");
                }
            }));
        }
        let stdin = child.stdin.take();
        self.procs.insert(
            name.to_owned(),
            Proc {
                child,
                stdin,
                readers,
            },
        );
    }

    /// Starts a service instance with `mantisd`.
    pub fn start(&mut self, name: &str) {
        self.start_with(name, Path::new(env!("CARGO_BIN_EXE_mantisd")), &[]);
    }

    /// Everything `name` printed, every run.
    pub fn output(&self, name: &str) -> String {
        self.output
            .get(name)
            .map(|o| o.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// `name`'s `/ready` answer.
    pub fn ready(&self, name: &str) -> Result<(u16, String), String> {
        probe_blocking(&self.instance(name).health, "/ready", Duration::from_millis(200))
    }

    /// `name`'s `/metrics` as `name -> value`.
    pub fn metrics(&self, name: &str) -> BTreeMap<String, i64> {
        let (_, body) = probe_blocking(
            &self.instance(name).health,
            "/metrics",
            Duration::from_millis(500),
        )
        .unwrap_or_default();
        body.lines()
            .filter_map(|l| l.split_once(' '))
            .filter_map(|(k, v)| Some((k.to_owned(), v.parse().ok()?)))
            .collect()
    }

    /// Waits until `name` is ready; returns how long it took.
    #[track_caller]
    pub fn wait_ready(&mut self, name: &str, timeout: Duration) -> Duration {
        let start = Instant::now();
        loop {
            if let Ok((200, _)) = self.ready(name) {
                return start.elapsed();
            }
            if let Some(p) = self.procs.get_mut(name)
                && let Ok(Some(status)) = p.child.try_wait()
            {
                panic!(
                    "{name} exited ({status}) before it was ready:\n{}",
                    self.output(name)
                );
            }
            assert!(
                start.elapsed() < timeout,
                "{name} not ready within {timeout:?}: {:?}\n{}",
                self.ready(name),
                self.output(name)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills `name` at once, as a crash would.
    pub fn kill(&mut self, name: &str) {
        let mut p = self.procs.remove(name).unwrap();
        let _ = p.child.kill();
        let _ = p.child.wait();
        p.finish_output();
    }

    /// Asks `name` to drain (closes its stdin) and waits for it to exit.
    #[track_caller]
    pub fn drain(&mut self, name: &str, timeout: Duration) -> ExitStatus {
        let mut p = self.procs.remove(name).unwrap();
        drop(p.stdin.take());
        let start = Instant::now();
        loop {
            if let Some(status) = p.child.try_wait().unwrap() {
                p.finish_output();
                return status;
            }
            if start.elapsed() > timeout {
                let _ = p.child.kill();
                panic!("{name} did not drain within {timeout:?}:\n{}", self.output(name));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for `name` to exit by itself.
    #[track_caller]
    pub fn wait_exit(&mut self, name: &str, timeout: Duration) -> ExitStatus {
        let mut p = self.procs.remove(name).unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = p.child.try_wait().unwrap() {
                p.finish_output();
                return status;
            }
            if start.elapsed() > timeout {
                let _ = p.child.kill();
                panic!("{name} did not exit within {timeout:?}:\n{}", self.output(name));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// True while `name` runs.
    pub fn running(&mut self, name: &str) -> bool {
        self.procs
            .get_mut(name)
            .is_some_and(|p| matches!(p.child.try_wait(), Ok(None)))
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for (_, mut p) in std::mem::take(&mut self.procs) {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Polls `ok` until it holds, at most `timeout`.
pub fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < timeout,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    start.elapsed()
}

/// One HTTPS request to the Ops dashboard; the status and the body.
pub fn https(
    addr: SocketAddr,
    cert: &[u8],
    method: &str,
    path: &str,
    token: &str,
    body: &str,
) -> (u16, String) {
    use std::io::{Read as _, Write as _};
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert.to_vec()))
        .unwrap();
    let config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let mut tcp = std::net::TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
         authorization: Bearer {token}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let code = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    (code, body)
}
