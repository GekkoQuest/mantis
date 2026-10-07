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
use mantis_services::host::rpc::{Endpoint, RpcClient};
use mantis_services::tls::TlsHandle;

use crate::config::NodeConfig;
use crate::drain::Drain;
use crate::health::{HealthServer, Phase, Status};
use crate::keys;
use crate::matrix;
use crate::ready;
use crate::registry::{Instance, Registry};
use crate::target::Target;

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
    /// Its mutual-TLS identity, checked at start and swapped live when its
    /// certificate files rotate or the registry's CA list changes
    /// ([`crate::tls`]).
    pub tls: TlsHandle,
    /// One live address per registry instance this node may dial, updated
    /// in place when a newer registry moves it.
    pub endpoints: Endpoints,
    /// Its status, served on the health endpoint.
    pub status: Status,
    /// The drain request.
    pub drain: Drain,
    /// The running registry, replaced when the source offers a newer one
    /// (`registry` above is the one the node started with).
    pub live: crate::source::Live,
    runtime: Option<tokio::runtime::Runtime>,
    health: Option<HealthServer>,
}

/// The live address of every registry instance a node may dial, by
/// instance name. An [`Endpoint`] is shared with every client made from
/// it, so moving it moves them all.
#[derive(Clone, Default)]
pub struct Endpoints(std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Endpoint>>>);

impl Endpoints {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Endpoint>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The endpoint of a cell host `instance`.
    #[must_use]
    pub fn get(&self, instance: &str) -> Option<Endpoint> {
        self.lock().get(instance).cloned()
    }

    /// The endpoint of service `role`: every instance of it, a call going
    /// to the active one.
    #[must_use]
    pub fn role(&self, role: Role) -> Option<Endpoint> {
        self.lock().get(&Self::role_key(role)).cloned()
    }

    fn role_key(role: Role) -> String {
        format!("role {}", matrix::name(role))
    }

    /// Makes the endpoints those of `registry`'s instances of `roles`: one
    /// per service role listing every instance of it (active and
    /// standbys), one per cell host. Existing ones are moved in place, new
    /// ones added, gone ones removed. Returns how many moved.
    ///
    /// # Errors
    /// A target the services refuse (never one the registry accepted).
    pub fn update(&self, registry: &Registry, roles: &[Role]) -> Result<usize, String> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for role in roles {
            let instances = registry.of(*role);
            if *role == Role::Cell {
                for i in instances {
                    wanted.push((i.name.clone(), i.rpc.to_string()));
                }
            } else if !instances.is_empty() {
                let targets: Vec<String> = instances.iter().map(|i| i.rpc.to_string()).collect();
                wanted.push((Self::role_key(*role), targets.join(",")));
            }
        }
        let mut map = self.lock();
        let mut moved = 0;
        for (key, target) in &wanted {
            match map.get(key) {
                Some(e) => {
                    if e.set(target)? {
                        moved += 1;
                    }
                }
                None => {
                    map.insert(key.clone(), Endpoint::new(target)?);
                }
            }
        }
        map.retain(|key, _| wanted.iter().any(|(k, _)| k == key));
        Ok(moved)
    }
}

/// Moves the node's endpoints whenever a newer registry is applied.
async fn follow_endpoints(
    endpoints: Endpoints,
    mut registries: tokio::sync::watch::Receiver<std::sync::Arc<Registry>>,
    roles: Vec<Role>,
    status: Status,
    who: (Role, String),
) {
    while registries.changed().await.is_ok() {
        let next = std::sync::Arc::clone(&registries.borrow_and_update());
        match endpoints.update(&next, &roles) {
            Ok(0) => {}
            Ok(n) => {
                status
                    .metrics
                    .add("endpoints_moved", i64::try_from(n).unwrap_or(i64::MAX));
                say(
                    who.0,
                    &who.1,
                    &format!("registry serial {}: {n} address(es) moved", next.serial),
                );
            }
            Err(e) => say(who.0, &who.1, &format!("registry serial {}: {e}", next.serial)),
        }
    }
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
    let registry = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(config.registry.load_at_start(&deploy, config.ready_timeout))?;
    if registry.serial < config.registry_min_serial {
        return Err(format!(
            "{}: serial {} is older than registry_min_serial {}: refused (rollback)",
            config.registry, registry.serial, config.registry_min_serial
        ));
    }
    Ok(registry)
}

/// Follows the node's registry source: applies each admissible newer
/// registry, reports it in the log and `/metrics` (`registry_serial`,
/// `registry_refused`, `registry_restart_required`).
fn follow_registry(
    config: &NodeConfig,
    instance: &str,
    registry: &Registry,
    status: &Status,
    handle: &tokio::runtime::Handle,
) -> Result<crate::source::Live, String> {
    let runtime_handle = handle;
    let (status, role, name) = (status.clone(), config.role, instance.to_owned());
    let (rpc_port, health_port) = (config.listen_rpc.port(), config.listen_health.port());
    Ok(crate::source::follow(
        runtime_handle,
        registry.clone(),
        crate::source::Follow {
            source: config.registry.clone(),
            deploy_key: keys::read_public_key(&config.deploy_key)?,
            instance: instance.to_owned(),
            every: config.registry_refresh,
            report: std::sync::Arc::new(move |r| match r {
                Ok(next) => {
                    status
                        .metrics
                        .set("registry_serial", i64::try_from(next.serial).unwrap_or(i64::MAX));
                    say(role, &name, &format!("registry serial {} applied", next.serial));
                    if let Some(me) = next.instance(&name)
                        && (me.rpc.port() != rpc_port || me.health.port() != health_port)
                    {
                        status.metrics.add("registry_restart_required", 1);
                        say(
                            role,
                            &name,
                            &format!(
                                "the registry moves this node to rpc {} health {}: its own listeners                                          move only with a restart",
                                me.rpc, me.health
                            ),
                        );
                    }
                }
                Err(why) => {
                    status.metrics.add("registry_refused", 1);
                    say(role, &name, &why);
                }
            }),
        },
    ))
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
            ("listen_rpc", config.listen_rpc, &me.rpc),
            ("listen_health", config.listen_health, &me.health),
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
        let tls = TlsHandle::new(crate::tls::load(
            &registry,
            config.role,
            &me.name,
            &config.tls_cert,
            &config.tls_key,
        )?);
        let endpoints = Endpoints::default();
        endpoints.update(&registry, &matrix::dials(config.role))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let drain = Drain::install(runtime.handle(), stdin_eof);
        let status = Status::new(matrix::name(config.role), &me.name);
        status.metrics.set(
            "registry_serial",
            i64::try_from(registry.serial).unwrap_or(i64::MAX),
        );
        let live = follow_registry(&config, &me.name, &registry, &status, runtime.handle())?;
        runtime.spawn(crate::tls::maintain(crate::tls::Maintain {
            tls: tls.clone(),
            live: live.clone(),
            role: config.role,
            instance: me.name.clone(),
            cert: config.tls_cert.clone(),
            key: config.tls_key.clone(),
            status: status.clone(),
        }));
        runtime.spawn(follow_endpoints(
            endpoints.clone(),
            live.subscribe(),
            matrix::dials(config.role),
            status.clone(),
            (config.role, me.name.clone()),
        ));
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
            endpoints,
            live,
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
    pub fn rpc_of(&self, role: Role) -> Result<Target, String> {
        if !matrix::dials(self.config.role).contains(&role) {
            return Err(format!(
                "a {} node does not call {} (caller matrix)",
                matrix::name(self.config.role),
                matrix::name(role)
            ));
        }
        Ok(self.registry.one(role)?.rpc.clone())
    }

    /// Resolves `target` now, to its first address.
    ///
    /// # Errors
    /// The name does not resolve.
    pub fn resolve(&self, target: &Target) -> Result<SocketAddr, String> {
        let found = self.block_on(target.resolve())?;
        found
            .first()
            .copied()
            .ok_or_else(|| format!("{target}: no address"))
    }

    /// The live address of the single instance of `role` (a role this
    /// node dials): it follows newer registries.
    ///
    /// # Errors
    /// `role` is not one this node dials, or the registry has none.
    pub fn endpoint_of(&self, role: Role) -> Result<Endpoint, String> {
        self.rpc_of(role)?;
        self.endpoints
            .role(role)
            .ok_or_else(|| format!("no address for {}", matrix::name(role)))
    }

    /// A client of the single instance of `role` over mutual TLS with this
    /// node's live identity, at its live address: it verifies the server's
    /// certificate names `role` in this cluster, and presents this node's.
    ///
    /// # Errors
    /// `role` is not one this node dials, or the identity does not make a
    /// client configuration.
    pub fn client_of(&self, role: Role) -> Result<RpcClient, String> {
        self.client_at(self.endpoint_of(role)?, role)
    }

    /// A client of `instance` (a cell host, for Ops) at its live address.
    ///
    /// # Errors
    /// [`Node::client_of`].
    pub fn client_to(&self, instance: &Instance) -> Result<RpcClient, String> {
        let endpoint = self
            .endpoints
            .get(&instance.name)
            .ok_or_else(|| format!("{} is not an instance this node calls", instance.name))?;
        self.client_at(endpoint, instance.role)
    }

    fn client_at(&self, endpoint: Endpoint, server: Role) -> Result<RpcClient, String> {
        let at = endpoint.target();
        RpcClient::with_endpoint(
            endpoint,
            self.config.role,
            self.key.clone(),
            Some(self.tls.clone()),
            server,
        )
        .map_err(|e| format!("a client of {} at {at}: {e}", matrix::name(server)))
    }

    /// Blocks until every dependency is ready (or the readiness timeout,
    /// or a drain). Returns how long it waited.
    ///
    /// # Errors
    /// A dependency is missing from the registry or never became ready.
    pub fn wait_for_dependencies(&self) -> Result<Duration, String> {
        let deps = matrix::dependencies(self.config.role)
            .iter()
            .map(|r| {
                let all = self.registry.of(*r);
                if all.is_empty() {
                    Err(format!("the registry has no {} instance", matrix::name(*r)))
                } else {
                    Ok(all)
                }
            })
            .collect::<Result<Vec<Vec<&Instance>>, String>>()?;
        if deps.is_empty() {
            return Ok(Duration::ZERO);
        }
        let waited = self.block_on(ready::wait_for(
            &deps,
            &self.status,
            self.config.ready_timeout,
            &self.drain,
        ))?;
        let names: Vec<&str> = deps
            .iter()
            .filter_map(|d| d.first().map(|i| matrix::name(i.role)))
            .collect();
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
