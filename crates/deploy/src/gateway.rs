//! The gateway role (`mantisd gateway`): the game's one front door.
//!
//! Clients dial the gateway's game listener (QUIC, the address the
//! registry lists as the instance's `game`); the gateway checks each
//! session's entry token with the realm (`RouteEntry`, as the `gateway`
//! role over mutual TLS with this node's live identity), connects to the
//! cell host serving it, and relays frames both ways
//! ([`mantis_net::gateway`]). Clients never learn a cell host's address,
//! so a cell host can be replaced, moved or swapped behind the gateway
//! while clients keep dialling the same address.
//!
//! - **Client certificate.** The listener presents the chain of
//!   `[gateway] cert` and `key`, checked like the cell host's game
//!   certificate ([`crate::game_tls::load`], for the host of the registry's
//!   `game` address when `ca` is set). The files are watched: a rotated
//!   pair that checks out is handed to the listener, **open sessions keep
//!   their connection and only new handshakes get the new chain**; a pair
//!   that fails is refused once and the running chain stays
//!   (`gateway_tls_rotations`, `gateway_tls_refused`).
//! - **Cell hosts.** The gateway trusts the cell hosts' game listeners
//!   through the registry's cluster CAs, for the one name
//!   `[gateway] hosts_name` every host's game certificate carries (issue
//!   them with `mantisd certs --server NAME --hosts <its address>,<hosts
//!   name>`). The CAs are taken when the gateway starts: a registry that
//!   changes them is reported (`gateway_hosts_ca_stale` 1) and takes effect
//!   at the gateway's next start; a CA rotation lists both CAs for a while,
//!   so restart the gateways within that overlap.
//! - **Readiness and drain.** `/ready` once the realm is ready and the
//!   listener is bound. A drain turns `/ready` 503 first (so a load
//!   balancer stops sending new clients), then waits up to the drain grace
//!   for the sessions in play to end (a session whose client is away waits
//!   only for a resume), then closes the listener: the remaining clients
//!   see their connection close.
//! - **Metrics.** `gateway_sessions`, `gateway_sessions_away`, and the
//!   gateway's counters (`gateway_joined`, `gateway_refused`,
//!   `gateway_handed_off`, `gateway_resumed`, ...), refreshed every second.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use mantis_net::NetRuntime;
use mantis_net::gateway::{Gateway, GatewayStats, GatewayThread};
use mantis_net::quic::{CertReloader, QuicDialer, QuicServer, ServerCertificate, ServerTrust};
use mantis_services::cluster::{EntryRoutes, secure_tickets};
use mantis_services::host::Role;

use crate::config::GatewayConfig;
use crate::health::Status;
use crate::node::{Node, say};
use crate::target::Target;
use crate::watch::FileWatch;

/// How often the gateway is polled.
const POLL: Duration = Duration::from_millis(1);
/// How often the certificate files and the counters are looked at.
const LOOK: Duration = Duration::from_secs(1);

/// The client-facing chain as a listener certificate.
fn client_certificate(settings: &GatewayConfig, game: &Target) -> Result<ServerCertificate, String> {
    let tls = crate::game_tls::load(&settings.tls, game)?;
    ServerCertificate::from_der(tls.chain, tls.key).map_err(|e| e.to_string())
}

/// Hands rotated client-facing certificates to the listener.
struct Rotation {
    settings: GatewayConfig,
    game: Target,
    watch: FileWatch,
    reloader: CertReloader,
    status: Status,
    instance: String,
}

impl Rotation {
    fn poll(&mut self) {
        if !self.watch.changed() {
            return;
        }
        let rotated = client_certificate(&self.settings, &self.game)
            .and_then(|c| self.reloader.reload(&c).map_err(|e| e.to_string()));
        match rotated {
            Ok(()) => {
                self.watch.accept();
                self.status.metrics.add("gateway_tls_rotations", 1);
                say(
                    Role::Gateway,
                    &self.instance,
                    "client certificate rotated: new handshakes get it, open sessions keep theirs",
                );
            }
            Err(e) => {
                self.watch.refuse();
                self.status.metrics.add("gateway_tls_refused", 1);
                say(
                    Role::Gateway,
                    &self.instance,
                    &format!("client certificate rotation refused, the running one stays: {e}"),
                );
            }
        }
    }
}

/// What the gateway's thread last saw: its counters, its sessions, and
/// those whose client is away.
#[derive(Clone, Copy, Default)]
struct Seen {
    stats: GatewayStats,
    sessions: usize,
    away: usize,
}

impl Seen {
    /// Sessions whose client is connected.
    const fn playing(&self) -> usize {
        self.sessions.saturating_sub(self.away)
    }
}

fn seen_now(seen: &Mutex<Seen>) -> Seen {
    *seen.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Copies the gateway's counters into `/metrics`.
fn report(status: &Status, seen: Seen) {
    let Seen {
        stats,
        sessions,
        away,
    } = seen;
    let m = &status.metrics;
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    m.set("gateway_sessions", i64::try_from(sessions).unwrap_or(i64::MAX));
    m.set("gateway_sessions_away", i64::try_from(away).unwrap_or(i64::MAX));
    m.set("gateway_joined", n(stats.joined));
    m.set("gateway_refused", n(stats.refused));
    m.set("gateway_handed_off", n(stats.handed_off));
    m.set("gateway_hand_offs_failed", n(stats.hand_offs_failed));
    m.set("gateway_resumed", n(stats.resumed));
    m.set("gateway_expired", n(stats.expired));
    m.set("gateway_up", n(stats.up));
    m.set("gateway_down", n(stats.down));
    m.set("gateway_stamped", n(stats.stamped));
    m.set("gateway_dropped", n(stats.dropped));
    m.set("gateway_send_errors", n(stats.send_errors));
}

/// Reports, while the gateway runs, a registry whose CAs differ from the
/// ones the cell-host dialer trusts.
async fn watch_cas(node_live: crate::source::Live, trusted: Vec<Vec<u8>>, status: Status, instance: String) {
    let mut registries = node_live.subscribe();
    let mut stale = false;
    while registries.changed().await.is_ok() {
        let now = registries.borrow_and_update().cas != trusted;
        if now != stale {
            stale = now;
            status.metrics.set("gateway_hosts_ca_stale", i64::from(now));
            if now {
                say(
                    Role::Gateway,
                    &instance,
                    "the registry's cluster CAs changed: the gateway trusts cell hosts by the CAs \
                     it started with until it is restarted",
                );
            }
        }
    }
}

/// A started gateway: the relay's thread (stopped when dropped), what it
/// last saw, and the network runtime its transports run on (dropped after
/// the thread).
struct Started {
    thread: GatewayThread,
    seen: Arc<Mutex<Seen>>,
    net: NetRuntime,
}

/// Binds the client listener and starts the relay: clients through the
/// realm's routes to the cell hosts, the client certificate watched.
fn start(node: &Node, settings: &GatewayConfig) -> Result<Started, String> {
    let game = node.me.rpc.clone();
    let cert = client_certificate(settings, &game)?;
    node.say(&format!(
        "client certificate from {}",
        settings.tls.cert.display()
    ));
    let net = NetRuntime::new(2).map_err(|e| format!("{e:?}"))?;
    let clients = QuicServer::bind(&net, node.config.listen_rpc, &cert)
        .map_err(|e| format!("game {}: {e:?}", node.config.listen_rpc))?;
    let addr = clients.local_addr();
    let reloader = clients.reloader();
    let hosts = QuicDialer::new(
        &net,
        &ServerTrust::Roots {
            roots: node.registry.cas.clone(),
            server_name: settings.hosts_name.clone(),
        },
    )
    .map_err(|e| format!("the cell-host dialer: {e:?}"))?;
    let routes = EntryRoutes::new(
        &node.handle(),
        node.endpoint_of(Role::Realm)?,
        node.key.clone(),
        Some(node.tls.clone()),
    )
    .map_err(|e| format!("the realm routes: {e}"))?;
    let gateway = Gateway::new(
        Box::new(clients),
        Box::new(hosts),
        Box::new(routes),
        secure_tickets(),
        mantis_net::gateway::GatewayConfig {
            resume_ms: u64::try_from(settings.resume.as_millis()).unwrap_or(u64::MAX),
            capacity: settings.capacity,
            route_timeout_ms: u64::try_from(settings.route_timeout.as_millis()).unwrap_or(u64::MAX),
        },
    );
    let mut paths = vec![settings.tls.cert.clone(), settings.tls.key.clone()];
    paths.extend(settings.tls.ca.clone());
    let mut rotation = Rotation {
        settings: settings.clone(),
        game,
        watch: FileWatch::new(paths, LOOK),
        reloader,
        status: node.status.clone(),
        instance: node.me.name.clone(),
    };
    let seen = Arc::new(Mutex::new(Seen::default()));
    let thread = {
        let seen = Arc::clone(&seen);
        GatewayThread::spawn(gateway, POLL, move |g| {
            *seen.lock().unwrap_or_else(PoisonError::into_inner) = Seen {
                stats: g.stats,
                sessions: g.sessions(),
                away: g.sessions_away(),
            };
            rotation.poll();
        })
    };
    node.say(&format!(
        "game {addr} (QUIC): clients through the realm's routes to cell hosts named {}",
        settings.hosts_name
    ));
    Ok(Started { thread, seen, net })
}

/// Runs the gateway on `node` until a drain: waits for the realm, serves
/// clients, then stops.
///
/// # Errors
/// Why the gateway could not start.
pub fn run(node: &Node) -> Result<(), String> {
    let settings = node
        .config
        .gateway
        .clone()
        .ok_or("a gateway node needs [gateway]")?;
    node.wait_for_dependencies()?;
    let Started { thread, seen, net } = start(node, &settings)?;
    let reporter = {
        let (seen, status) = (Arc::clone(&seen), node.status.clone());
        node.handle().spawn(async move {
            loop {
                report(&status, seen_now(&seen));
                tokio::time::sleep(LOOK).await;
            }
        })
    };
    let cas = node.handle().spawn(watch_cas(
        node.live.clone(),
        node.registry.cas.clone(),
        node.status.clone(),
        node.me.name.clone(),
    ));
    node.ready();
    let why = node.block_on(node.drain.wait());
    node.draining(why);
    let deadline = Instant::now() + node.config.drain_grace;
    // Sessions whose client is away wait only for a resume: nothing to give
    // them time for.
    while seen_now(&seen).playing() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    reporter.abort();
    cas.abort();
    // The thread stops (and is joined) before the network runtime it polls.
    drop(thread);
    let last = seen_now(&seen);
    report(&node.status, last);
    drop(net);
    node.say(&format!(
        "stopped: the game listener closed ({} session(s) in play and {} awaiting a resume were closed)",
        last.playing(),
        last.away
    ));
    Ok(())
}
