//! The health endpoint every node serves on its own listener.
//!
//! Plain HTTP/1.1, `GET` only, one request per connection, a bounded
//! request and a short deadline:
//!
//! | Path | 200 when | Body |
//! |---|---|---|
//! | `/live` | the process runs | `live <role> <instance>` |
//! | `/ready` | it serves (dependencies were up, nothing is draining) | `ready <role> <instance>`; otherwise 503 with `starting ...` or `draining ...` and why |
//! | `/metrics` | always | `name value` lines |
//!
//! It is for orchestrators and for other nodes' readiness checks. It
//! carries no secrets and changes nothing, so it needs no token; it is
//! never the Ops dashboard and never a game port.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mantis_services::host::Metrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::target::Target;

/// Where a node is in its life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Waiting for dependencies or starting its role.
    Starting,
    /// Serving.
    Ready,
    /// Draining: no longer ready, finishing work before exit.
    Draining,
}

/// A node's status, shared by the node and its health endpoint.
#[derive(Clone)]
pub struct Status {
    role: &'static str,
    instance: Arc<str>,
    state: Arc<Mutex<(Phase, String)>>,
    /// The node's counters, served on `/metrics`.
    pub metrics: Metrics,
}

impl Status {
    /// Starting, for `role` (its configuration name) and `instance`.
    #[must_use]
    pub fn new(role: &'static str, instance: &str) -> Self {
        Self {
            role,
            instance: Arc::from(instance),
            state: Arc::new(Mutex::new((Phase::Starting, "starting".to_owned()))),
            metrics: Metrics::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Phase, String)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Moves to `phase`, saying why.
    pub fn set(&self, phase: Phase, note: impl Into<String>) {
        *self.lock() = (phase, note.into());
    }

    /// Changes the note only while the phase is `phase` (a ready node's
    /// "active" or "standby", never overwriting a drain).
    pub fn note_if(&self, phase: Phase, note: impl Into<String>) {
        let mut s = self.lock();
        if s.0 == phase {
            s.1 = note.into();
        }
    }

    /// The phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.lock().0
    }

    /// The role's name.
    #[must_use]
    pub fn role(&self) -> &'static str {
        self.role
    }

    /// The instance name.
    #[must_use]
    pub fn instance(&self) -> &str {
        &self.instance
    }

    fn answer(&self, path: &str) -> (u16, String) {
        let (phase, note) = self.lock().clone();
        let who = format!("{} {}", self.role, self.instance);
        match path {
            "/live" => (200, format!("live {who}\n")),
            "/ready" => match phase {
                Phase::Ready if note.is_empty() => (200, format!("ready {who}\n")),
                // A failover role's instance: "ready <role> <instance> (active)"
                // or "(standby)". Both are ready: a standby serves its role
                // the moment the active instance's lease lapses.
                Phase::Ready => (200, format!("ready {who} ({note})\n")),
                Phase::Starting => (503, format!("starting {who}: {note}\n")),
                Phase::Draining => (503, format!("draining {who}: {note}\n")),
            },
            "/metrics" => {
                let mut out = String::new();
                let _ = writeln!(out, "node_ready {}", i64::from(phase == Phase::Ready));
                out.push_str(&self.metrics.render());
                (200, out)
            }
            _ => (404, "not found\n".to_owned()),
        }
    }
}

/// The largest request head read.
const MAX_REQUEST: usize = 2048;

/// How long a client has to send its request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// A running health endpoint.
pub struct HealthServer {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    /// Binds `addr` and serves `status`.
    ///
    /// # Errors
    /// The bind failed.
    pub async fn bind(addr: SocketAddr, status: Status) -> Result<Self, String> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("health endpoint {addr}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let status = status.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(REQUEST_DEADLINE, serve(stream, &status)).await;
                });
            }
        });
        Ok(Self { addr, task })
    }

    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for HealthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut stream: TcpStream, status: &Status) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
        if buf.len() > MAX_REQUEST {
            return respond(&mut stream, 431, "request too large\n").await;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut words = head.lines().next().unwrap_or("").split(' ');
    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    if method != "GET" {
        return respond(&mut stream, 405, "GET only\n").await;
    }
    let (code, body) = status.answer(path);
    respond(&mut stream, code, &body).await
}

async fn respond(stream: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// Asks `target` for `path`: the status code and the body. A host name is
/// resolved now, and each address it resolves to is tried in turn.
///
/// # Errors
/// No connection, no answer within `deadline`, or not HTTP.
pub async fn probe(target: &Target, path: &str, deadline: Duration) -> Result<(u16, String), String> {
    let ask = async {
        let mut last = format!("{target}: no address");
        let mut stream = None;
        for addr in target.resolve().await? {
            match TcpStream::connect(addr).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        let mut s = stream.ok_or(last)?;
        let req = format!("GET {path} HTTP/1.1\r\nhost: {target}\r\nconnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out).into_owned();
        let code = text
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| "not an HTTP answer".to_owned())?;
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
        Ok((code, body))
    };
    tokio::time::timeout(deadline, ask)
        .await
        .map_err(|_| format!("no answer within {} ms", deadline.as_millis()))?
}

/// Blocking [`probe`] for tools and tests outside a runtime.
///
/// # Errors
/// [`probe`], or no runtime could be built.
pub fn probe_blocking(target: &Target, path: &str, deadline: Duration) -> Result<(u16, String), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(probe(target, path, deadline))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_answers_by_phase() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let status = Status::new("social", "social-1");
            let server = HealthServer::bind("127.0.0.1:0".parse().unwrap(), status.clone())
                .await
                .unwrap();
            let at = Target::from(server.addr());
            let get = |path: &'static str| probe(&at, path, Duration::from_secs(2));
            assert_eq!(
                get("/live").await.unwrap(),
                (200, "live social social-1\n".to_owned())
            );
            status.set(Phase::Starting, "waiting for persist");
            assert_eq!(
                get("/ready").await.unwrap(),
                (503, "starting social social-1: waiting for persist\n".to_owned())
            );
            status.set(Phase::Ready, "");
            status.metrics.set("rpc_calls", 4);
            assert_eq!(get("/ready").await.unwrap().0, 200);
            assert_eq!(get("/metrics").await.unwrap().1, "node_ready 1\nrpc_calls 4\n");
            assert_eq!(get("/nothing").await.unwrap().0, 404);
            status.set(Phase::Draining, "drain requested");
            assert_eq!(get("/ready").await.unwrap().0, 503);
        });
    }
}
