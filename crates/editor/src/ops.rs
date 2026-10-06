//! The server half of the live inspector: read-only GETs against the Ops dashboard's
//! inspector routes (plan 16: production servers are reachable only through Ops, audited,
//! read-only). The editor never speaks the game protocol or the cluster RPC to a server.
//!
//! The dashboard is HTTPS with a bearer token; the editor pins the dashboard's
//! certificate (`toy-server cluster --ops-cert-out FILE`), so it talks to exactly that
//! dashboard. This client issues `GET` only; it has no way to send an Ops command.
//!
//! | route | view |
//! |---|---|
//! | `GET /inspect/{cell}` | [`CellSummary`] |
//! | `GET /inspect/{cell}/systems` | [`SystemsView`] |
//! | `GET /inspect/{cell}/components` | component names |
//! | `GET /inspect/{cell}/entities?component=&offset=&limit=` | [`EntitiesView`] |

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};

use crate::json::{Json, parse};

/// Most entities one page may ask for (the dashboard caps it too).
pub const MAX_PAGE: u32 = 100;

/// Largest response accepted.
const MAX_RESPONSE: usize = 4 << 20;

/// Errors talking to the dashboard.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpsError {
    /// The connection, TLS, or I/O failed.
    Transport(String),
    /// The dashboard answered with a non-200 status (401 for a bad token).
    Status(u16, String),
    /// The body is not the expected JSON.
    Body(String),
}

impl core::fmt::Display for OpsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Status(code, body) => write!(f, "status {code}: {body}"),
            Self::Body(e) => write!(f, "response: {e}"),
        }
    }
}

impl std::error::Error for OpsError {}

/// A cell's summary.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CellSummary {
    /// Cell number.
    pub cell: i64,
    /// Current tick.
    pub tick: i64,
    /// Latest state hash.
    pub state_hash: u64,
    /// Attached sessions.
    pub sessions: i64,
    /// Live entities.
    pub entities: i64,
}

/// One system's timing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SystemTiming {
    /// System name.
    pub name: String,
    /// Schedule phase.
    pub phase: String,
    /// Last run, microseconds.
    pub micros_last: i64,
    /// p99, microseconds.
    pub micros_p99: i64,
    /// Runs recorded.
    pub runs: i64,
}

/// A cell's system timings and pipeline health.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SystemsView {
    /// Cell number.
    pub cell: i64,
    /// Tick of the reading.
    pub tick: i64,
    /// Every system, in schedule order.
    pub systems: Vec<SystemTiming>,
    /// Intents waiting in the inbox.
    pub inbox_depth: i64,
    /// Per-client encode p99, microseconds.
    pub encode_micros_p99: i64,
    /// Log writer lag, ticks.
    pub log_lag_ticks: i64,
}

/// One entity and its components as text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EntityView {
    /// `index:generation`.
    pub id: String,
    /// `(component name, value text)`.
    pub components: Vec<(String, String)>,
}

/// One page of entities carrying a component.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EntitiesView {
    /// Cell number.
    pub cell: i64,
    /// Tick of the reading.
    pub tick: i64,
    /// The component filtered on.
    pub component: String,
    /// Entities carrying it in all.
    pub total: i64,
    /// This page.
    pub entities: Vec<EntityView>,
}

fn field<'a>(v: &'a Json, key: &str) -> Result<&'a Json, OpsError> {
    v.get(key).ok_or_else(|| OpsError::Body(format!("no `{key}`")))
}

fn int(v: &Json, key: &str) -> Result<i64, OpsError> {
    field(v, key)?
        .int()
        .ok_or_else(|| OpsError::Body(format!("`{key}` is not an integer")))
}

fn text(v: &Json, key: &str) -> Result<String, OpsError> {
    field(v, key)?
        .str()
        .map(str::to_owned)
        .ok_or_else(|| OpsError::Body(format!("`{key}` is not a string")))
}

/// Percent-encodes a query value.
fn query(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Decodes a `Transfer-Encoding: chunked` body.
fn unchunk(body: &[u8]) -> Result<Vec<u8>, OpsError> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| OpsError::Body("a chunk without its size line".to_owned()))?;
        let size_text = String::from_utf8_lossy(rest.get(..line_end).unwrap_or(&[])).into_owned();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| OpsError::Body(format!("a bad chunk size `{size_text}`")))?;
        rest = rest.get(line_end + 2..).unwrap_or(&[]);
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(
            rest.get(..size)
                .ok_or_else(|| OpsError::Body("a short chunk".to_owned()))?,
        );
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

/// A read-only client of the Ops dashboard's inspector routes.
#[derive(Clone)]
pub struct OpsInspector {
    addr: SocketAddr,
    token: String,
    tls: Arc<rustls::ClientConfig>,
    timeout: Duration,
}

impl core::fmt::Debug for OpsInspector {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpsInspector")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl OpsInspector {
    /// A client of the dashboard at `addr`, trusting exactly the certificate `cert_der`
    /// and sending the operator `token`.
    ///
    /// # Errors
    /// [`OpsError::Transport`] for a certificate rustls refuses.
    pub fn new(addr: SocketAddr, cert_der: &[u8], token: &str) -> Result<Self, OpsError> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(cert_der.to_vec()))
            .map_err(|e| OpsError::Transport(e.to_string()))?;
        let tls =
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| OpsError::Transport(e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        Ok(Self {
            addr,
            token: token.to_owned(),
            tls: Arc::new(tls),
            timeout: Duration::from_secs(5),
        })
    }

    /// One `GET`; the body of a 200 response.
    fn get(&self, path: &str) -> Result<Json, OpsError> {
        let transport = |e: &dyn core::fmt::Display| OpsError::Transport(e.to_string());
        let name = ServerName::try_from("localhost").map_err(|e| transport(&e))?;
        let conn = rustls::ClientConnection::new(Arc::clone(&self.tls), name).map_err(|e| transport(&e))?;
        let tcp = TcpStream::connect_timeout(&self.addr, self.timeout).map_err(|e| transport(&e))?;
        tcp.set_read_timeout(Some(self.timeout))
            .map_err(|e| transport(&e))?;
        tcp.set_write_timeout(Some(self.timeout))
            .map_err(|e| transport(&e))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let request = format!(
            "GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\nauthorization: Bearer {}\r\n\r\n",
            self.token
        );
        stream.write_all(request.as_bytes()).map_err(|e| transport(&e))?;
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                    if raw.len() > MAX_RESPONSE {
                        return Err(OpsError::Body("the response is too large".to_owned()));
                    }
                }
                // A peer closing without a TLS close_notify still delivered its body.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(transport(&e)),
            }
        }
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| OpsError::Body("no header end".to_owned()))?;
        let head = String::from_utf8_lossy(raw.get(..split).unwrap_or(&[])).into_owned();
        let mut body = raw.get(split + 4..).unwrap_or(&[]).to_vec();
        let status: u16 = head
            .get(9..12)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| OpsError::Body("no status line".to_owned()))?;
        let chunked = head.lines().any(|l| {
            l.to_ascii_lowercase().starts_with("transfer-encoding:")
                && l.to_ascii_lowercase().contains("chunked")
        });
        if chunked {
            body = unchunk(&body)?;
        }
        let text = String::from_utf8(body).map_err(|e| OpsError::Body(e.to_string()))?;
        if status != 200 {
            return Err(OpsError::Status(status, text));
        }
        parse(&text).map_err(OpsError::Body)
    }

    /// `GET /inspect/{cell}`: the audited Ops `inspect` command, whose result text is
    /// `cell=N tick=T state_hash=HEX sessions=N entities=N ...`.
    ///
    /// # Errors
    /// [`OpsError`].
    pub fn cell(&self, cell: u64) -> Result<CellSummary, OpsError> {
        let v = self.get(&format!("/inspect/{cell}"))?;
        let after = text(&v, "after")?;
        let field = |key: &str| {
            after
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
                .ok_or_else(|| OpsError::Body(format!("no `{key}` in `{after}`")))
        };
        let number = |key: &str| -> Result<i64, OpsError> {
            field(key)?
                .parse()
                .map_err(|_| OpsError::Body(format!("`{key}` is not a number in `{after}`")))
        };
        Ok(CellSummary {
            cell: number("cell")?,
            tick: number("tick")?,
            state_hash: u64::from_str_radix(field("state_hash")?, 16)
                .map_err(|_| OpsError::Body(format!("`state_hash` is not hex in `{after}`")))?,
            sessions: number("sessions")?,
            entities: number("entities")?,
        })
    }

    /// `GET /inspect/{cell}/systems`.
    ///
    /// # Errors
    /// [`OpsError`].
    pub fn systems(&self, cell: u64) -> Result<SystemsView, OpsError> {
        let v = self.get(&format!("/inspect/{cell}/systems"))?;
        let systems = field(&v, "systems")?
            .items()
            .iter()
            .map(|s| {
                Ok(SystemTiming {
                    name: text(s, "name")?,
                    phase: text(s, "phase")?,
                    micros_last: int(s, "micros_last")?,
                    micros_p99: int(s, "micros_p99")?,
                    runs: int(s, "runs")?,
                })
            })
            .collect::<Result<Vec<_>, OpsError>>()?;
        Ok(SystemsView {
            cell: int(&v, "cell")?,
            tick: int(&v, "tick")?,
            systems,
            inbox_depth: int(&v, "inbox_depth")?,
            encode_micros_p99: int(&v, "encode_micros_p99")?,
            log_lag_ticks: int(&v, "log_lag_ticks")?,
        })
    }

    /// `GET /inspect/{cell}/components`.
    ///
    /// # Errors
    /// [`OpsError`].
    pub fn components(&self, cell: u64) -> Result<Vec<String>, OpsError> {
        let v = self.get(&format!("/inspect/{cell}/components"))?;
        field(&v, "components")?
            .items()
            .iter()
            .map(|c| {
                c.str()
                    .map(str::to_owned)
                    .ok_or_else(|| OpsError::Body("a component name is not a string".to_owned()))
            })
            .collect()
    }

    /// `GET /inspect/{cell}/entities?component=...&offset=...&limit=...` (`limit` capped
    /// at [`MAX_PAGE`]). A page may hold fewer entities than asked when values are long:
    /// page with `offset += entities.len()`; `total` is always the full count. Each page
    /// read is an audited Ops action.
    ///
    /// # Errors
    /// [`OpsError`].
    pub fn entities(
        &self,
        cell: u64,
        component: &str,
        offset: u32,
        limit: u32,
    ) -> Result<EntitiesView, OpsError> {
        let v = self.get(&format!(
            "/inspect/{cell}/entities?component={}&offset={offset}&limit={}",
            query(component),
            limit.min(MAX_PAGE)
        ))?;
        let entities = field(&v, "entities")?
            .items()
            .iter()
            .map(|e| {
                let components = field(e, "components")?
                    .items()
                    .iter()
                    .map(|c| Ok((text(c, "name")?, text(c, "value")?)))
                    .collect::<Result<Vec<_>, OpsError>>()?;
                Ok(EntityView {
                    id: text(e, "id")?,
                    components,
                })
            })
            .collect::<Result<Vec<_>, OpsError>>()?;
        Ok(EntitiesView {
            cell: int(&v, "cell")?,
            tick: int(&v, "tick")?,
            component: text(&v, "component")?,
            total: int(&v, "total")?,
            entities,
        })
    }
}
