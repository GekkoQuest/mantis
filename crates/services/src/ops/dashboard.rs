//! The Ops dashboard: HTTPS on its own listener, loopback by default, never
//! a game port.
//!
//! The listener completes a TLS handshake before any byte reaches HTTP
//! parsing, and HTTP parsing before any handler runs, so game-protocol
//! traffic (which is neither) never reaches an Ops handler; `handled`
//! counts handler entries so tests can prove it. Every request but
//! `/health` needs an operator token (`authorization: Bearer <token>`),
//! whose operator name becomes the audit row's actor.
//!
//! Routes: `GET /health`, `GET /audit`, `GET /live`, `GET /inspect/{cell}`,
//! `GET /ledger/{character}` (audited, read-only),
//! `POST /ops/{ban|maintenance|flag|tunable|kick|drain}` with a body of
//! `key=value` pairs separated by `&` or newlines.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::{Command, OpsError, OpsService};

/// Where the dashboard listens and who may use it.
#[derive(Clone, Debug)]
pub struct DashboardConfig {
    /// The listener (default `127.0.0.1:7480`).
    pub listen: SocketAddr,
    /// Allow a non-loopback listener (an explicit, reviewed choice).
    pub allow_remote: bool,
    /// The game ports of this host: the dashboard refuses to share one.
    pub game_ports: Vec<u16>,
    /// Operator tokens to operator names.
    pub operators: BTreeMap<String, String>,
}

impl DashboardConfig {
    /// Loopback on the default port, no operators yet.
    #[must_use]
    pub fn loopback() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 7480)),
            allow_remote: false,
            game_ports: Vec::new(),
            operators: BTreeMap::new(),
        }
    }

    /// Refuses a listener that is remote without permission, or that is a
    /// game port.
    ///
    /// # Errors
    /// Why the dashboard must not start.
    pub fn check(&self) -> Result<(), String> {
        if !self.listen.ip().is_loopback() && !self.allow_remote {
            return Err(format!(
                "the Ops dashboard listens on loopback unless allow_remote is set; {} is not loopback",
                self.listen
            ));
        }
        if self.listen.port() != 0 && self.game_ports.contains(&self.listen.port()) {
            return Err(format!(
                "the Ops dashboard never shares a game port ({})",
                self.listen.port()
            ));
        }
        if self.operators.keys().any(|t| t.len() < 16) {
            return Err("operator tokens have at least 16 bytes".to_owned());
        }
        Ok(())
    }
}

/// A self-signed development certificate for `localhost`: the server
/// configuration and the certificate (DER) clients trust.
///
/// # Errors
/// Generation failed.
pub fn dev_tls() -> Result<(Arc<rustls::ServerConfig>, Vec<u8>), String> {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).map_err(|e| e.to_string())?;
    let cert = ck.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config =
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert.clone())], key)
            .map_err(|e| e.to_string())?;
    Ok((Arc::new(config), cert))
}

/// TLS connections, handshaken off the accept path.
struct TlsIncoming {
    rx: mpsc::Receiver<(TlsStream<tokio::net::TcpStream>, SocketAddr)>,
    addr: SocketAddr,
}

impl axum::serve::Listener for TlsIncoming {
    type Io = TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

/// How long a client has to finish the TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(5);

struct Dash {
    ops: OpsService,
    operators: BTreeMap<String, String>,
    handled: AtomicU64,
    refused_handshakes: Arc<AtomicU64>,
}

/// A running dashboard.
pub struct Dashboard {
    addr: SocketAddr,
    dash: Arc<Dash>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Dashboard {
    /// Checks `config`, binds its listener, and serves.
    ///
    /// # Errors
    /// The configuration is refused, or the bind failed.
    pub async fn start(
        config: &DashboardConfig,
        ops: OpsService,
        tls: Arc<rustls::ServerConfig>,
    ) -> Result<Self, String> {
        config.check()?;
        let listener = TcpListener::bind(config.listen)
            .await
            .map_err(|e| format!("{}: {e}", config.listen))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let refused = Arc::new(AtomicU64::new(0));
        let dash = Arc::new(Dash {
            ops,
            operators: config.operators.clone(),
            handled: AtomicU64::new(0),
            refused_handshakes: Arc::clone(&refused),
        });
        let (tx, rx) = mpsc::channel(64);
        let acceptor = TlsAcceptor::from(tls);
        let accept = tokio::spawn(async move {
            while let Ok((tcp, peer)) = listener.accept().await {
                let (acceptor, tx, refused) = (acceptor.clone(), tx.clone(), Arc::clone(&refused));
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => {
                            let _ = tx.send((stream, peer)).await;
                        }
                        _ => {
                            refused.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let app = routes(Arc::clone(&dash));
        let serve = tokio::spawn(async move {
            let _ = axum::serve(TlsIncoming { rx, addr }, app).await;
        });
        Ok(Self {
            addr,
            dash,
            tasks: vec![accept, serve],
        })
    }

    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Requests that reached a handler (any route, authorised or not).
    #[must_use]
    pub fn handled(&self) -> u64 {
        self.dash.handled.load(Ordering::Relaxed)
    }

    /// Connections dropped before HTTP: failed or timed-out handshakes.
    #[must_use]
    pub fn refused_handshakes(&self) -> u64 {
        self.dash.refused_handshakes.load(Ordering::Relaxed)
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn routes(dash: Arc<Dash>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/audit", get(audit))
        .route("/live", get(live))
        .route("/inspect/{cell}", get(inspect))
        .route("/inspect/{cell}/systems", get(inspect_systems))
        .route("/inspect/{cell}/components", get(inspect_components))
        .route("/inspect/{cell}/entities", get(inspect_entities))
        .route("/ledger/{character}", get(ledger))
        .route("/ops/{command}", post(command))
        .with_state(dash)
}

fn json(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn error(status: StatusCode, why: &str) -> Response {
    json(status, format!("{{\"error\":{}}}", quote(why)))
}

/// Constant-time comparison of two tokens.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The operator behind the request's token.
fn operator(dash: &Dash, headers: &HeaderMap) -> Option<String> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let mut found = None;
    for (t, name) in &dash.operators {
        if same(t.as_bytes(), token.as_bytes()) {
            found = Some(name.clone());
        }
    }
    found
}

/// Counts the handler entry and returns the request's operator.
fn enter(dash: &Dash, headers: &HeaderMap) -> Option<String> {
    dash.handled.fetch_add(1, Ordering::Relaxed);
    operator(dash, headers)
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "an operator token is required")
}

async fn health(State(dash): State<Arc<Dash>>) -> Response {
    dash.handled.fetch_add(1, Ordering::Relaxed);
    json(StatusCode::OK, "{\"ok\":true}".to_owned())
}

async fn audit(State(dash): State<Arc<Dash>>, headers: HeaderMap) -> Response {
    if enter(&dash, &headers).is_none() {
        return unauthorized();
    }
    let ops = dash.ops.clone();
    let rows = match tokio::task::spawn_blocking(move || ops.audit_rows()).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return error(StatusCode::SERVICE_UNAVAILABLE, &e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let opt = |v: &Option<String>| v.as_deref().map_or_else(|| "null".to_owned(), quote);
    let items: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{{\"id\":{},\"at_ms\":{},\"actor\":{},\"command\":{},\"args\":{},\"status\":{},\"before\":{},\"after\":{},\"undo\":{}}}",
                r.id,
                r.at_ms,
                quote(&r.actor),
                quote(&r.command),
                quote(&r.args),
                quote(&r.status),
                opt(&r.before),
                opt(&r.after),
                opt(&r.undo)
            )
        })
        .collect();
    json(StatusCode::OK, format!("[{}]", items.join(",")))
}

async fn live(State(dash): State<Arc<Dash>>, headers: HeaderMap) -> Response {
    if enter(&dash, &headers).is_none() {
        return unauthorized();
    }
    let changes = dash.ops.live_since(0);
    let items: Vec<String> = changes
        .changes
        .iter()
        .map(|c| {
            format!(
                "{{\"seq\":{},\"name\":{},\"kind\":{},\"value\":{}}}",
                c.seq,
                quote(c.name.as_str()),
                c.kind,
                c.value
            )
        })
        .collect();
    json(StatusCode::OK, format!("[{}]", items.join(",")))
}

async fn run(dash: &Dash, actor: &str, cmd: &Command) -> Response {
    match dash.ops.execute(actor, cmd).await {
        Ok(done) => json(
            StatusCode::OK,
            format!(
                "{{\"audit\":{},\"before\":{},\"after\":{},\"undo\":{}}}",
                done.audit,
                quote(&done.before),
                quote(&done.after),
                quote(&done.undo)
            ),
        ),
        Err(e @ OpsError::Invalid(_)) => error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e @ OpsError::Audit(_)) => error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
        Err(e @ OpsError::Failed { .. }) => error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn inspect(State(dash): State<Arc<Dash>>, headers: HeaderMap, Path(cell): Path<u64>) -> Response {
    let Some(actor) = enter(&dash, &headers) else {
        return unauthorized();
    };
    run(&dash, &actor, &Command::Inspect { cell }).await
}

/// A cell's system run times (plan 13): read-only telemetry.
async fn inspect_systems(
    State(dash): State<Arc<Dash>>,
    headers: HeaderMap,
    Path(cell): Path<u64>,
) -> Response {
    if enter(&dash, &headers).is_none() {
        return unauthorized();
    }
    let t = match dash.ops.system_times(cell).await {
        Ok(t) => t,
        Err(e) => return error(StatusCode::BAD_GATEWAY, &e),
    };
    let systems: Vec<String> = t
        .systems
        .iter()
        .map(|s| {
            format!(
                "{{\"name\":{},\"phase\":{},\"micros_last\":{},\"micros_p99\":{},\"runs\":{}}}",
                quote(s.name.as_str()),
                quote(crate::inspect::phase_name(s.phase)),
                s.micros_last,
                s.micros_p99,
                s.runs
            )
        })
        .collect();
    json(
        StatusCode::OK,
        format!(
            "{{\"cell\":{},\"tick\":{},\"systems\":[{}],\"inbox_depth\":{},\"encode_micros_p99\":{},\"log_lag_ticks\":{}}}",
            t.cell.0,
            t.tick,
            systems.join(","),
            t.inbox_depth,
            t.encode_micros_p99,
            t.log_lag_ticks
        ),
    )
}

/// A cell's component names: read-only.
async fn inspect_components(
    State(dash): State<Arc<Dash>>,
    headers: HeaderMap,
    Path(cell): Path<u64>,
) -> Response {
    if enter(&dash, &headers).is_none() {
        return unauthorized();
    }
    match dash.ops.component_names(cell).await {
        Ok(n) => {
            let names: Vec<String> = n.names.iter().map(|s| quote(s.as_str())).collect();
            json(
                StatusCode::OK,
                format!("{{\"components\":[{}]}}", names.join(",")),
            )
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, &e),
    }
}

/// `%XX` and `+` decoded; `None` for a malformed escape or non-UTF-8.
fn unescape(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                let hex = std::str::from_utf8(&hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
            }
            b => out.push(b),
        }
    }
    String::from_utf8(out).ok()
}

/// The entities query: `component` (required), `offset` (default 0),
/// `limit` (default 50, 1 to 100).
fn entities_query(raw: Option<&str>) -> Result<(String, u32, u16), &'static str> {
    let (mut component, mut offset, mut limit) = (None, 0u32, 50u16);
    for pair in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = unescape(v).ok_or("a malformed query value")?;
        match k {
            "component" => component = Some(v),
            "offset" => offset = v.parse().map_err(|_| "offset is a number")?,
            "limit" => limit = v.parse().map_err(|_| "limit is 1 to 100")?,
            _ => return Err("unknown query parameter"),
        }
    }
    let component = component.filter(|c| !c.is_empty()).ok_or("name a component")?;
    if !(1..=crate::methods::MAX_INSPECT_LIMIT).contains(&limit) {
        return Err("limit is 1 to 100");
    }
    Ok((component, offset, limit))
}

/// A page of a cell's entities with a component: audited, read-only.
async fn inspect_entities(
    State(dash): State<Arc<Dash>>,
    headers: HeaderMap,
    Path(cell): Path<u64>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let Some(actor) = enter(&dash, &headers) else {
        return unauthorized();
    };
    let (component, offset, limit) = match entities_query(raw.as_deref()) {
        Ok(q) => q,
        Err(why) => return error(StatusCode::BAD_REQUEST, why),
    };
    let page = match dash
        .ops
        .entity_page(&actor, cell, &component, offset, limit)
        .await
    {
        Ok(p) => p,
        Err(e @ OpsError::Invalid(_)) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e @ OpsError::Audit(_)) => return error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
        Err(e @ OpsError::Failed { .. }) => return error(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let entities: Vec<String> = crate::inspect::page_rows(page.text.as_str())
        .iter()
        .map(|(id, components)| {
            let components: Vec<String> = components
                .iter()
                .map(|(name, value)| format!("{{\"name\":{},\"value\":{}}}", quote(name), quote(value)))
                .collect();
            format!(
                "{{\"id\":{},\"components\":[{}]}}",
                quote(id),
                components.join(",")
            )
        })
        .collect();
    json(
        StatusCode::OK,
        format!(
            "{{\"cell\":{},\"tick\":{},\"component\":{},\"total\":{},\"entities\":[{}]}}",
            page.cell.0,
            page.tick,
            quote(page.component.as_str()),
            page.total,
            entities.join(",")
        ),
    )
}

/// A character's ledger rows: audited (read-only), then listed.
async fn ledger(State(dash): State<Arc<Dash>>, headers: HeaderMap, Path(character): Path<u64>) -> Response {
    let Some(actor) = enter(&dash, &headers) else {
        return unauthorized();
    };
    if let Err(e) = dash
        .ops
        .execute(&actor, &Command::LedgerTrace { character })
        .await
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string());
    }
    let ops = dash.ops.clone();
    let rows = match tokio::task::spawn_blocking(move || ops.ledger_trace(character)).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return error(StatusCode::SERVICE_UNAVAILABLE, &e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let items: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{{\"month\":{},\"cell\":{},\"tick\":{},\"item\":{},\"delta\":{},\"at_ms\":{}}}",
                r.month, r.cell, r.tick, r.item, r.delta, r.at_ms
            )
        })
        .collect();
    json(StatusCode::OK, format!("[{}]", items.join(",")))
}

/// `key=value` pairs separated by `&` or newlines.
fn form(body: &str) -> BTreeMap<&str, &str> {
    body.split(['&', '\n'])
        .filter_map(|p| p.trim().split_once('='))
        .collect()
}

fn parse(command: &str, body: &str) -> Result<Command, String> {
    let f = form(body);
    let get = |k: &str| f.get(k).copied().ok_or_else(|| format!("{k} is required"));
    let bool_of = |v: &str| match v {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(format!("not a switch: {v}")),
    };
    Ok(match command {
        "ban" => Command::Ban {
            account: get("account")?.parse().map_err(|_| "account: not a number")?,
            until_ms: get("until_ms")?.parse().map_err(|_| "until_ms: not a number")?,
            reason: f.get("reason").copied().unwrap_or("").to_owned(),
        },
        "maintenance" => Command::Maintenance {
            on: bool_of(get("on")?)?,
        },
        "flag" => Command::Flag {
            name: get("name")?.to_owned(),
            on: bool_of(get("on")?)?,
        },
        "kick" => Command::Kick {
            character: get("character")?.parse().map_err(|_| "character: not a number")?,
        },
        "drain" => Command::Drain {
            on: bool_of(get("on")?)?,
            grace_seconds: f
                .get("grace_seconds")
                .copied()
                .unwrap_or("0")
                .parse()
                .map_err(|_| "grace_seconds: not a number")?,
        },
        "tunable" => Command::Tunable {
            name: get("name")?.to_owned(),
            value: get("value")?.parse().map_err(|_| "value: not a number")?,
        },
        other => return Err(format!("no command {other}")),
    })
}

async fn command(
    State(dash): State<Arc<Dash>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: String,
) -> Response {
    let Some(actor) = enter(&dash, &headers) else {
        return unauthorized();
    };
    match parse(&name, &body) {
        Ok(cmd) => run(&dash, &actor, &cmd).await,
        Err(why) => error(StatusCode::BAD_REQUEST, &why),
    }
}
