//! Typed internal RPC between service roles (plan 10).
//!
//! - **Typed.** A [`Method`] names its request and response types (any
//!   `Wire` type) and the roles allowed to call it: the **caller matrix**,
//!   enforced by the server before the handler runs.
//! - **Framed.** One TCP connection per client; frames are
//!   `[u32 len][u8 kind][u64 call][u16 method][payload]`. A connection
//!   opens with a hello naming the caller's role and proving the cluster
//!   secret (an HMAC of the role with the shared key; production adds mTLS
//!   in front, an Ops concern).
//! - **Timeouts** on every call; **reconnect** on the next call after a
//!   connection drops, with a bounded backoff; calls in flight when a
//!   connection drops fail with [`RpcError::Disconnected`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::lock;

use mantis_core::wire::{Decoder, Encoder, Wire};
use ring::hmac;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::Role;

/// Largest frame.
pub const MAX_FRAME: usize = 1 << 20;

const HELLO: u8 = 0;
const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const FAILURE: u8 = 3;

/// One RPC method.
pub trait Method: 'static {
    /// Stable id, unique per server.
    const ID: u16;
    /// Name, for logs.
    const NAME: &'static str;
    /// Roles allowed to call it.
    const CALLERS: &'static [Role];
    /// The request.
    type Request: Wire + Send + 'static;
    /// The response.
    type Response: Wire + Send + 'static;
}

/// Why a call failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RpcError {
    /// No answer within the timeout.
    Timeout,
    /// The connection could not be made or was lost.
    Disconnected,
    /// The caller's role may not call this method.
    Forbidden,
    /// The server has no such method.
    NoSuchMethod,
    /// The payload did not decode.
    Malformed,
    /// The handler refused, with its reason.
    Refused(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("timed out"),
            Self::Disconnected => f.write_str("disconnected"),
            Self::Forbidden => f.write_str("caller not allowed"),
            Self::NoSuchMethod => f.write_str("no such method"),
            Self::Malformed => f.write_str("malformed payload"),
            Self::Refused(why) => write!(f, "refused: {why}"),
        }
    }
}

impl std::error::Error for RpcError {}

fn encode_error(e: &RpcError, out: &mut Vec<u8>) {
    let mut enc = Encoder::new(out);
    match e {
        RpcError::Timeout => enc.u8(0),
        RpcError::Disconnected => enc.u8(1),
        RpcError::Forbidden => enc.u8(2),
        RpcError::NoSuchMethod => enc.u8(3),
        RpcError::Malformed => enc.u8(4),
        RpcError::Refused(why) => {
            enc.u8(5);
            let b = why.as_bytes();
            enc.u16(u16::try_from(b.len().min(1024)).unwrap_or(0));
            enc.bytes(b.get(..b.len().min(1024)).unwrap_or(&[]));
        }
    }
}

fn decode_error(bytes: &[u8]) -> RpcError {
    let mut d = Decoder::new(bytes);
    match d.u8() {
        Ok(0) => RpcError::Timeout,
        Ok(1) => RpcError::Disconnected,
        Ok(2) => RpcError::Forbidden,
        Ok(3) => RpcError::NoSuchMethod,
        Ok(5) => d
            .u16()
            .and_then(|n| d.take(usize::from(n)))
            .map_or(RpcError::Malformed, |b| {
                RpcError::Refused(String::from_utf8_lossy(b).into_owned())
            }),
        _ => RpcError::Malformed,
    }
}

fn frame(kind: u8, call: u64, method: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 15);
    let len = u32::try_from(payload.len() + 11).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&call.to_le_bytes());
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

struct Frame {
    kind: u8,
    call: u64,
    method: u16,
    payload: Vec<u8>,
}

async fn read_frame(s: &mut (impl AsyncReadExt + Unpin)) -> Option<Frame> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.ok()?;
    let len = usize::try_from(u32::from_le_bytes(len)).ok()?;
    if !(11..=MAX_FRAME).contains(&len) {
        return None;
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.ok()?;
    let mut d = Decoder::new(&body);
    let kind = d.u8().ok()?;
    let call = d.u64().ok()?;
    let method = d.u16().ok()?;
    let payload = d.take(d.remaining()).ok()?.to_vec();
    Some(Frame {
        kind,
        call,
        method,
        payload,
    })
}

/// Proof that a caller holds the cluster key: HMAC-SHA256 of its role.
fn proof(key: &[u8], role: Role) -> Vec<u8> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&k, &[role as u8]).as_ref().to_vec()
}

type Reply = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, RpcError>> + Send>>;

type NowFn = dyn Fn(Role, &[u8]) -> Result<Vec<u8>, RpcError> + Send + Sync;
type LaterFn = dyn Fn(Role, &[u8]) -> Reply + Send + Sync;

enum Handler {
    /// Answers at once.
    Now(Box<NowFn>),
    /// Answers later (waits on another thread without blocking the runtime).
    Later(Box<LaterFn>),
}

struct Entry {
    callers: &'static [Role],
    handler: Handler,
}

/// Checks a request payload by method id before its handler runs.
pub type Validate = fn(u16, &[u8]) -> Result<(), RpcError>;

/// The method table of one server.
#[derive(Default)]
pub struct Router {
    methods: BTreeMap<u16, Entry>,
    validate: Option<Validate>,
}

impl Router {
    /// No methods.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// No methods; every request passes `validate` first.
    #[must_use]
    pub fn validated(validate: Validate) -> Self {
        Self {
            methods: BTreeMap::new(),
            validate: Some(validate),
        }
    }

    /// Serves `M` with `f`, which gets the caller's role.
    ///
    /// A duplicate id replaces the earlier handler (a programming error
    /// caught by each role's tests).
    pub fn serve<M: Method>(
        &mut self,
        f: impl Fn(Role, M::Request) -> Result<M::Response, RpcError> + Send + Sync + 'static,
    ) -> &mut Self {
        let handler = Handler::Now(Box::new(move |role, payload| {
            let req =
                mantis_core::wire::decode_exact::<M::Request>(payload).map_err(|_| RpcError::Malformed)?;
            let resp = f(role, req)?;
            let mut out = Vec::new();
            mantis_core::wire::encode_into(&resp, &mut out);
            Ok(out)
        }));
        self.methods.insert(
            M::ID,
            Entry {
                callers: M::CALLERS,
                handler,
            },
        );
        self
    }

    /// Serves `M` with `f`, whose answer comes later (a future): for a
    /// handler that waits on another thread, such as a cell host's tick
    /// loop, without blocking the runtime. Same caller matrix and checks.
    pub fn serve_later<M, F, Fut>(&mut self, f: F) -> &mut Self
    where
        M: Method,
        F: Fn(Role, M::Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<M::Response, RpcError>> + Send + 'static,
    {
        let f = Arc::new(f);
        let handler = Handler::Later(Box::new(move |role, payload| {
            let req = mantis_core::wire::decode_exact::<M::Request>(payload);
            let f = Arc::clone(&f);
            Box::pin(async move {
                let req = req.map_err(|_| RpcError::Malformed)?;
                let resp = f(role, req).await?;
                let mut out = Vec::new();
                mantis_core::wire::encode_into(&resp, &mut out);
                Ok(out)
            })
        }));
        self.methods.insert(
            M::ID,
            Entry {
                callers: M::CALLERS,
                handler,
            },
        );
        self
    }

    /// Dispatches one call (the caller matrix first).
    async fn dispatch(&self, role: Role, method: u16, payload: &[u8]) -> Result<Vec<u8>, RpcError> {
        let e = self.methods.get(&method).ok_or(RpcError::NoSuchMethod)?;
        if !e.callers.contains(&role) {
            return Err(RpcError::Forbidden);
        }
        if let Some(v) = self.validate {
            v(method, payload)?;
        }
        match &e.handler {
            Handler::Now(h) => h(role, payload),
            Handler::Later(h) => h(role, payload).await,
        }
    }
}

/// A running RPC server.
pub struct RpcServer {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    connections: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl RpcServer {
    /// Serves `router` on `addr` (port 0 picks one), accepting callers that
    /// prove `key`.
    ///
    /// # Errors
    /// The bind error.
    pub async fn bind(addr: SocketAddr, key: Vec<u8>, router: Router) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let router = Arc::new(router);
        let key = Arc::new(key);
        let connections: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let live = Arc::clone(&connections);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (router, key) = (Arc::clone(&router), Arc::clone(&key));
                let conn = tokio::spawn(async move {
                    let _ = serve_connection(stream, &router, &key).await;
                });
                let mut all = lock(&live);
                all.retain(|h| !h.is_finished());
                all.push(conn.abort_handle());
            }
        });
        Ok(Self {
            addr,
            task,
            connections,
        })
    }

    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Stopping a server closes its connections too: callers see the loss and
/// reconnect to whatever serves the address next.
impl Drop for RpcServer {
    fn drop(&mut self) {
        self.task.abort();
        for c in lock(&self.connections).drain(..) {
            c.abort();
        }
    }
}

async fn serve_connection(mut stream: TcpStream, router: &Router, key: &[u8]) -> Option<()> {
    stream.set_nodelay(true).ok()?;
    let hello = read_frame(&mut stream).await?;
    let role = Role::from_u8(*hello.payload.first()?)?;
    if hello.kind != HELLO || hello.payload.get(1..) != Some(proof(key, role).as_slice()) {
        return None;
    }
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    while let Some(f) = read_frame(&mut rd).await {
        if f.kind != REQUEST {
            break;
        }
        let out = match router.dispatch(role, f.method, &f.payload).await {
            Ok(payload) => frame(RESPONSE, f.call, f.method, &payload),
            Err(e) => {
                let mut payload = Vec::new();
                encode_error(&e, &mut payload);
                frame(FAILURE, f.call, f.method, &payload)
            }
        };
        if tx.send(out).await.is_err() {
            break;
        }
    }
    drop(tx);
    let _ = writer.await;
    Some(())
}

type Pending = Arc<Mutex<BTreeMap<u64, oneshot::Sender<Result<Vec<u8>, RpcError>>>>>;

struct Connection {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

/// A client of one server, reconnecting as needed.
pub struct RpcClient {
    addr: SocketAddr,
    role: Role,
    key: Vec<u8>,
    next_call: std::sync::atomic::AtomicU64,
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl RpcClient {
    /// A client calling `addr` as `role`; it connects on the first call.
    #[must_use]
    pub fn new(addr: SocketAddr, role: Role, key: Vec<u8>) -> Self {
        Self {
            addr,
            role,
            key,
            next_call: std::sync::atomic::AtomicU64::new(1),
            conn: tokio::sync::Mutex::new(None),
        }
    }

    /// The server it calls.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    async fn connect(&self) -> Result<Connection, RpcError> {
        let mut delay = Duration::from_millis(10);
        for _ in 0..4 {
            if let Ok(mut stream) = TcpStream::connect(self.addr).await {
                let _ = stream.set_nodelay(true);
                let mut hello = vec![self.role as u8];
                hello.extend(proof(&self.key, self.role));
                stream
                    .write_all(&frame(HELLO, 0, 0, &hello))
                    .await
                    .map_err(|_| RpcError::Disconnected)?;
                return Ok(Self::start(stream));
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        Err(RpcError::Disconnected)
    }

    fn start(stream: TcpStream) -> Connection {
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let pending: Pending = Arc::default();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let (p, a) = (Arc::clone(&pending), Arc::clone(&alive));
        tokio::spawn(async move {
            while let Some(f) = read_frame(&mut rd).await {
                let result = match f.kind {
                    RESPONSE => Ok(f.payload),
                    FAILURE => Err(decode_error(&f.payload)),
                    _ => continue,
                };
                if let Some(waiter) = lock(&p).remove(&f.call) {
                    let _ = waiter.send(result);
                }
            }
            a.store(false, std::sync::atomic::Ordering::Release);
            for (_, waiter) in std::mem::take(&mut *lock(&p)) {
                let _ = waiter.send(Err(RpcError::Disconnected));
            }
        });
        Connection { tx, pending, alive }
    }

    /// Calls `M` with `req`, waiting at most `timeout`.
    ///
    /// # Errors
    /// [`RpcError`].
    pub async fn call<M: Method>(
        &self,
        req: &M::Request,
        timeout: Duration,
    ) -> Result<M::Response, RpcError> {
        let mut payload = Vec::new();
        mantis_core::wire::encode_into(req, &mut payload);
        let call = self.next_call.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        {
            let mut conn = self.conn.lock().await;
            let stale = conn
                .as_ref()
                .is_none_or(|c| !c.alive.load(std::sync::atomic::Ordering::Acquire));
            if stale {
                *conn = Some(self.connect().await?);
            }
            let c = conn.as_ref().ok_or(RpcError::Disconnected)?;
            lock(&c.pending).insert(call, done);
            c.tx.send(frame(REQUEST, call, M::ID, &payload))
                .await
                .map_err(|_| RpcError::Disconnected)?;
        }
        let bytes = match tokio::time::timeout(timeout, wait).await {
            Err(_) => {
                if let Some(c) = self.conn.lock().await.as_ref() {
                    lock(&c.pending).remove(&call);
                }
                return Err(RpcError::Timeout);
            }
            Ok(Err(_)) => return Err(RpcError::Disconnected),
            Ok(Ok(result)) => result?,
        };
        mantis_core::wire::decode_exact::<M::Response>(&bytes).map_err(|_| RpcError::Malformed)
    }
}
