//! One process, one role: what every node does whatever its role.
//!
//! [`Node::start`] reads the configuration, verifies the registry with the
//! deploy key (a tampered or older registry stops the process before it
//! binds anything but its health endpoint), checks its role's row of the
//! caller matrix, reads the cluster key, installs the drain signals, and
//! serves `/live`, `/ready` (503 until the role serves) and `/metrics`.
//! [`Node::wait_for_dependencies`] then blocks until every role it depends
//! on is ready.

use std::net::SocketAddr;
use std::time::Duration;

use mantis_services::host::Role;
use mantis_services::host::rpc::RpcClient;

use crate::config::NodeConfig;
use crate::drain::Drain;
use crate::health::{HealthServer, Phase, Status};
use crate::keys;
use crate::matrix;
use crate::ready;
use crate::registry::{Instance, Registry};

/// A started node: verified registry, keys, health, drain.
pub struct Node {
    /// Its configuration.
    pub config: NodeConfig,
    /// The verified registry.
    pub registry: Registry,
    /// This process's entry in it.
    pub me: Instance,
    /// The cluster key.
    pub key: Vec<u8>,
    /// Its mutual-TLS material, checked at start.
    pub tls: std::sync::Arc<mantis_services::tls::TlsIdentity>,
    /// Its status, served on the health endpoint.
    pub status: Status,
    /// The drain request.
    pub drain: Drain,
    runtime: Option<tokio::runtime::Runtime>,
    health: Option<HealthServer>,
}

/// Prints one line of the node's own log.
pub fn say(role: Role, instance: &str, line: &str) {
    println!("mantisd {} {instance}: {line}", matrix::name(role));
}

/// Reads and verifies the registry `config` names.
///
/// # Errors
/// The deploy key or the registry cannot be read, the signature does not
/// verify, the serial is older than allowed, or the body is invalid.
pub fn verified_registry(config: &NodeConfig) -> Result<Registry, String> {
    let deploy = keys::read_public_key(&config.deploy_key)?;
    let text = std::fs::read_to_string(&config.registry)
        .map_err(|e| format!("{}: {e}", config.registry.display()))?;
    let registry =
        Registry::verify(&text, &deploy).map_err(|e| format!("{}: {e}", config.registry.display()))?;
    if registry.serial < config.registry_min_serial {
        return Err(format!(
            "{}: serial {} is older than registry_min_serial {}: refused (rollback)",
            config.registry.display(),
            registry.serial,
            config.registry_min_serial
        ));
    }
    Ok(registry)
}

impl Node {
    /// Starts the node for `config`; `stdin_eof` makes the end of standard
    /// input a drain request.
    ///
    /// # Errors
    /// Why the node must not run.
    pub fn start(config: NodeConfig, stdin_eof: bool) -> Result<Self, String> {
        let registry = verified_registry(&config)?;
        let me = registry
            .instance(&config.instance)
            .cloned()
            .ok_or_else(|| format!("the registry has no instance {:?}", config.instance))?;
        if me.role != config.role {
            return Err(format!(
                "the registry lists {} as a {}, the configuration as a {}",
                me.name,
                matrix::name(me.role),
                matrix::name(config.role)
            ));
        }
        for (what, listen, listed) in [
            ("listen_rpc", config.listen_rpc, me.rpc),
            ("listen_health", config.listen_health, me.health),
        ] {
            if listen.port() != listed.port() {
                return Err(format!(
                    "{what} {listen} is not on the port the registry lists for {} ({listed})",
                    me.name
                ));
            }
        }
        matrix::check(config.role)?;
        let key = keys::read_cluster_key(&config.cluster_key)?;
        let tls = crate::tls::load(
            &registry,
            config.role,
            &me.name,
            &config.tls_cert,
            &config.tls_key,
        )?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let drain = Drain::install(runtime.handle(), stdin_eof);
        let status = Status::new(matrix::name(config.role), &me.name);
        let health = runtime.block_on(HealthServer::bind(config.listen_health, status.clone()))?;
        say(
            config.role,
            &me.name,
            &format!(
                "registry {} serial {} verified; certificate {} verified; rpc {} (mutual TLS), health {}",
                registry.cluster,
                registry.serial,
                crate::pki::identity(&registry.cluster, config.role, &me.name),
                config.listen_rpc,
                health.addr()
            ),
        );
        print!("{}", matrix::graph(config.role));
        Ok(Self {
            config,
            registry,
            me,
            key,
            tls,
            status,
            drain,
            runtime: Some(runtime),
            health: Some(health),
        })
    }

    /// The runtime the node runs on.
    #[must_use]
    pub fn handle(&self) -> tokio::runtime::Handle {
        match &self.runtime {
            Some(r) => r.handle().clone(),
            None => tokio::runtime::Handle::current(),
        }
    }

    /// Runs `f` to completion on the node's runtime.
    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.handle().block_on(f)
    }

    /// Prints a line of this node's log.
    pub fn say(&self, line: &str) {
        say(self.config.role, &self.me.name, line);
    }

    /// Where the single instance of `role` listens for RPC. Only roles this
    /// node's role may dial are given out.
    ///
    /// # Errors
    /// `role` is not one this node dials, or the registry has none.
    pub fn rpc_of(&self, role: Role) -> Result<SocketAddr, String> {
        if !matrix::dials(self.config.role).contains(&role) {
            return Err(format!(
                "a {} node does not call {} (caller matrix)",
                matrix::name(self.config.role),
                matrix::name(role)
            ));
        }
        Ok(self.registry.one(role)?.rpc)
    }

    /// A client of the `server` role at `addr`, over mutual TLS with this
    /// node's identity: it verifies the server's certificate names
    /// `server` in this cluster, and presents this node's.
    ///
    /// # Errors
    /// The identity does not make a client configuration.
    pub fn client(&self, addr: SocketAddr, server: Role) -> Result<RpcClient, String> {
        RpcClient::with_tls(
            addr,
            self.config.role,
            self.key.clone(),
            Some(std::sync::Arc::clone(&self.tls)),
            server,
        )
        .map_err(|e| format!("a client of {} at {addr}: {e}", matrix::name(server)))
    }

    /// Blocks until every dependency is ready (or the readiness timeout,
    /// or a drain). Returns how long it waited.
    ///
    /// # Errors
    /// A dependency is missing from the registry or never became ready.
    pub fn wait_for_dependencies(&self) -> Result<Duration, String> {
        let deps = matrix::dependencies(self.config.role)
            .iter()
            .map(|r| self.registry.one(*r))
            .collect::<Result<Vec<&Instance>, String>>()?;
        if deps.is_empty() {
            return Ok(Duration::ZERO);
        }
        let waited = self.block_on(ready::wait_for(
            &deps,
            &self.status,
            self.config.ready_timeout,
            &self.drain,
        ))?;
        let names: Vec<&str> = deps.iter().map(|d| matrix::name(d.role)).collect();
        self.say(&format!(
            "dependencies ready after {} ms: {}",
            waited.as_millis(),
            names.join(", ")
        ));
        Ok(waited)
    }

    /// Marks the node ready.
    pub fn ready(&self) {
        self.status.set(Phase::Ready, "");
        self.say("ready");
    }

    /// Marks the node draining (no longer ready).
    pub fn draining(&self, why: &str) {
        self.status.set(Phase::Draining, why.to_owned());
        self.say(&format!("draining ({why})"));
    }

    /// Where the health endpoint listens.
    #[must_use]
    pub fn health_addr(&self) -> Option<SocketAddr> {
        self.health.as_ref().map(HealthServer::addr)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            {
                let _guard = rt.enter();
                self.health = None;
            }
            rt.shutdown_timeout(Duration::from_millis(500));
        }
    }
}
