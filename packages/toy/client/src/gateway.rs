//! The gateway address and reconnecting through it.
//!
//! The client knows one address, the gateway's (`--server HOST:PORT`, a name or an IP).
//! The name is resolved again for every connection, so a reconnect reaches whatever the
//! name points at now. Hand-offs between cell hosts happen behind the gateway on the same
//! connection ([`mantis_client::net`]); only a dropped connection dials again, through
//! [`Redial`], which drives a [`Reconnector`] and shows its status ([`SharedStatus`]).

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use mantis_adapter_contract::Transport;
use mantis_client::net::NativeSession;
use mantis_client::reconnect::{ReconnectPolicy, ReconnectStatus, ReconnectStep, Reconnector, SharedStatus};
use mantis_client::time::HostInstant;

/// A gateway address: a host (name or IP) and a port.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Gateway {
    host: String,
    port: u16,
}

impl Gateway {
    /// Parses `HOST:PORT` (`[V6]:PORT` for an IPv6 literal).
    ///
    /// # Errors
    /// The address in words.
    pub fn parse(server: &str) -> Result<Self, String> {
        if let Ok(addr) = server.parse::<SocketAddr>() {
            return Ok(Self {
                host: addr.ip().to_string(),
                port: addr.port(),
            });
        }
        let bad = || format!("--server: not HOST:PORT: {server}");
        let (host, port) = server.rsplit_once(':').ok_or_else(bad)?;
        let port = port.parse().map_err(|_| bad())?;
        if host.is_empty() || host.contains(':') {
            return Err(bad());
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// The host as given (the default server name for certificate checks).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Resolves the host now (every call asks again).
    ///
    /// # Errors
    /// A name that does not resolve, in words.
    pub fn resolve(&self) -> Result<SocketAddr, String> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}:{}: {e}", self.host, self.port))?
            .next()
            .ok_or_else(|| format!("{}:{}: no address", self.host, self.port))
    }
}

impl core::fmt::Display for Gateway {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Opens a new connection to the gateway (resolving it again).
pub type Dial<T> = Box<dyn FnMut() -> Result<T, String> + Send>;

/// Reconnects a session through the gateway: drives a [`Reconnector`] and dials when it
/// says so. A dial that fails counts as a failed attempt (the session stays closed).
pub struct Redial<T> {
    reconnector: Reconnector,
    dial: Dial<T>,
    status: Arc<SharedStatus>,
    last_error: Option<String>,
    dials: u64,
}

impl<T> core::fmt::Debug for Redial<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Redial")
            .field("reconnector", &self.reconnector)
            .field("last_error", &self.last_error)
            .field("dials", &self.dials)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> Redial<T> {
    /// Reconnects with the default backoff, falling back to `launcher_token` when no
    /// resume ticket is valid, dialing with `dial`.
    pub fn new(launcher_token: &[u8], dial: Dial<T>) -> Self {
        Self::with_policy(ReconnectPolicy::default(), launcher_token, dial)
    }

    /// [`Redial::new`] with `policy`.
    pub fn with_policy(policy: ReconnectPolicy, launcher_token: &[u8], dial: Dial<T>) -> Self {
        Self {
            reconnector: Reconnector::new(policy, launcher_token),
            dial,
            status: Arc::new(SharedStatus::new()),
            last_error: None,
            dials: 0,
        }
    }

    /// One step after the session's own: dials again when the reconnector says so, and
    /// publishes the status.
    pub fn step(&mut self, now: HostInstant, net: &mut NativeSession<T>) {
        if let ReconnectStep::Connect { token } = self.reconnector.step(now, net) {
            self.dials += 1;
            match (self.dial)() {
                Ok(transport) => net.reconnect(transport, &token),
                Err(e) => self.last_error = Some(e),
            }
        }
        self.status.set(self.reconnector.status());
    }

    /// The status, shared with whoever shows it.
    pub fn status(&self) -> &Arc<SharedStatus> {
        &self.status
    }

    /// Where the connection is.
    pub fn current(&self) -> ReconnectStatus {
        self.reconnector.status()
    }

    /// Successful reconnects so far.
    pub fn reconnects(&self) -> u64 {
        self.reconnector.reconnects()
    }

    /// Connections dialed so far.
    pub fn dials(&self) -> u64 {
        self.dials
    }

    /// Why the last dial failed, if one did.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::Gateway;

    #[test]
    fn gateway_addresses_parse_names_ips_and_v6_literals() {
        let g = Gateway::parse("gateway.test:7777");
        assert_eq!(g.as_ref().map(Gateway::host), Ok("gateway.test"));
        assert_eq!(g.as_ref().map(Gateway::port), Ok(7777));
        let ip = Gateway::parse("127.0.0.1:7000");
        assert_eq!(ip.as_ref().map(Gateway::host), Ok("127.0.0.1"));
        let v6 = Gateway::parse("[::1]:7000");
        assert_eq!(v6.as_ref().map(Gateway::host), Ok("::1"));
        assert_eq!(v6.map(|g| g.to_string()), Ok("[::1]:7000".to_owned()));
        for bad in ["gateway.test", ":7000", "gateway.test:port", "::1:7000"] {
            assert!(Gateway::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_ip_gateway_resolves_to_itself_every_time() {
        let g = Gateway::parse("127.0.0.1:7000");
        let addr = g.and_then(|g| g.resolve());
        assert_eq!(addr, "127.0.0.1:7000".parse().map_err(|_| String::new()));
    }
}
