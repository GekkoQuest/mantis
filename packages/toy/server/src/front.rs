//! The toy cluster's gateway (`toy-server cluster --gateway ADDR`): the
//! one game address clients connect to ([`mantis_net::gateway`]). It
//! terminates their TLS with its own certificate, routes entry tokens
//! through the realm, and relays each session to the cell host serving it.

use std::net::SocketAddr;
use std::time::Duration;

use mantis_net::NetRuntime;
use mantis_net::gateway::{Gateway, GatewayConfig, GatewayThread};
use mantis_net::quic::{CertWatcher, QuicDialer, QuicServer, ServerCertificate, ServerTrust};
use mantis_services::cluster::EntryRoutes;
use mantis_services::host::rpc::Endpoint;

/// How often the gateway polls.
pub const POLL: Duration = Duration::from_millis(1);

/// What the gateway needs.
pub struct FrontSettings {
    /// Where clients connect.
    pub listen: SocketAddr,
    /// The certificate clients see.
    pub cert: ServerCertificate,
    /// How the gateway trusts cell hosts' game certificates.
    pub hosts: ServerTrust,
    /// The realm, every instance.
    pub realm: Endpoint,
    /// The cluster key.
    pub key: Vec<u8>,
}

/// A running gateway: its address, and its thread (stopped when dropped).
pub struct Front {
    /// Where it listens.
    pub addr: SocketAddr,
    /// The polling thread.
    pub thread: GatewayThread,
}

/// Starts the gateway on `runtime` (its sockets) and `handle` (realm
/// calls). With `watcher`, its certificate reloads when the files change.
///
/// # Errors
/// The listener, the dialer, or the realm client could not be made.
pub fn start(
    runtime: &NetRuntime,
    handle: &tokio::runtime::Handle,
    s: FrontSettings,
    files: Option<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<Front, String> {
    let clients =
        QuicServer::bind(runtime, s.listen, &s.cert).map_err(|e| format!("gateway listener: {e}"))?;
    let addr = clients.local_addr();
    let mut watcher = files.map(|(c, k)| CertWatcher::new(c, k, clients.reloader(), Duration::from_secs(1)));
    let hosts = QuicDialer::new(runtime, &s.hosts).map_err(|e| format!("gateway dialer: {e}"))?;
    let routes = EntryRoutes::new(handle, s.realm, s.key, None).map_err(|e| e.to_string())?;
    let gateway = Gateway::new(
        Box::new(clients),
        Box::new(hosts),
        Box::new(routes),
        mantis_services::cluster::secure_tickets(),
        GatewayConfig::DEFAULT,
    );
    let thread = GatewayThread::spawn(gateway, POLL, move |_| {
        if let Some(Err(e)) = watcher.as_mut().and_then(CertWatcher::poll) {
            eprintln!(
                "toy-server: the gateway certificate files changed but do not load ({e}); the old chain stays"
            );
        }
    });
    Ok(Front { addr, thread })
}
