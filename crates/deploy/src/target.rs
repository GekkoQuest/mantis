//! Where an instance listens: `host:port`, where the host is an IP literal
//! or a DNS name. A name is resolved each time a connection is made, never
//! once at start, so an instance that moves behind its name is followed.

use std::net::{IpAddr, SocketAddr};

/// A `host:port` target.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Target {
    host: String,
    port: u16,
}

/// Whether `s` is a DNS host name: labels of 1 to 63 letters, digits and
/// `-` (not at either end), dot-separated, at most 253 characters.
fn host_name(s: &str) -> bool {
    (1..=253).contains(&s.len())
        && s.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

impl Target {
    /// Parses `host:port` (`[v6]:port` for an IPv6 literal).
    ///
    /// # Errors
    /// Not a target.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Ok(a) = text.parse::<SocketAddr>() {
            return Ok(Self::from(a));
        }
        let (host, port) = text
            .rsplit_once(':')
            .ok_or_else(|| format!("{text:?} is not host:port"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| format!("{text:?}: the port is 0 to 65535"))?;
        if host.parse::<IpAddr>().is_err() && !host_name(host) {
            return Err(format!(
                "{text:?}: the host is neither an IP address nor a DNS name"
            ));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// The host (an IP literal, without brackets, or a name).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The IP, when the host is a literal.
    #[must_use]
    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    /// The socket address, when the host is a literal.
    #[must_use]
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.ip().map(|ip| SocketAddr::new(ip, self.port))
    }

    /// Resolves the target now (names through the system resolver).
    ///
    /// # Errors
    /// The name does not resolve.
    pub async fn resolve(&self) -> Result<Vec<SocketAddr>, String> {
        if let Some(a) = self.socket_addr() {
            return Ok(vec![a]);
        }
        let found: Vec<SocketAddr> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|e| format!("{self}: {e}"))?
            .collect();
        if found.is_empty() {
            Err(format!("{self}: resolves to no address"))
        } else {
            Ok(found)
        }
    }
}

impl From<SocketAddr> for Target {
    fn from(a: SocketAddr) -> Self {
        Self {
            host: a.ip().to_string(),
            port: a.port(),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip() {
            Some(IpAddr::V6(v6)) => write!(f, "[{v6}]:{}", self.port),
            _ => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_names_and_literals() {
        let t = Target::parse("Persist.Services.Internal:7505").unwrap();
        assert_eq!(
            (t.host(), t.port(), t.ip()),
            ("persist.services.internal", 7505, None)
        );
        assert_eq!(t.to_string(), "persist.services.internal:7505");
        let t = Target::parse("10.0.0.3:7505").unwrap();
        assert_eq!(t.socket_addr(), Some("10.0.0.3:7505".parse().unwrap()));
        assert_eq!(Target::parse("[::1]:80").unwrap().to_string(), "[::1]:80");
        for bad in [
            "persist",
            "persist:",
            "persist:99999",
            "-bad:1",
            "a_b:1",
            ":1",
            "a..b:1",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_name_resolves_at_call_time() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let found = rt
            .block_on(Target::parse("localhost:9").unwrap().resolve())
            .unwrap();
        assert!(
            found.iter().all(|a| a.ip().is_loopback() && a.port() == 9),
            "{found:?}"
        );
    }
}
