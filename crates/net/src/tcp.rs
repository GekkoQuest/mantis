//! TCP transport for legacy-shaped adapters (decision 0002): one stream per
//! connection carries both channels.
//!
//! Framing: the client sends the preamble `MTT1`, then each frame is
//! `len: u32 LE` (covering the channel byte and payload), `channel: u8`
//! (0 reliable, 1 unreliable), then the payload. Unreliable frames arrive in
//! order on TCP; they are tagged so the owner can treat them alike on every
//! transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mantis_adapter_contract::{
    Channel, ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent, TransportKind,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::shared::{MAX_FRAME, NetEvent, Poller, SendQueue, Shared, lock};
use crate::{NetError, NetRuntime};

const PREAMBLE: [u8; 4] = *b"MTT1";

/// Unreliable frames on TCP are bounded like QUIC datagrams so adapters
/// behave the same on either transport.
pub const MAX_UNRELIABLE: usize = 1100;

async fn writer(mut w: OwnedWriteHalf, queue: Arc<SendQueue>, shared: Arc<Shared>, id: ConnectionId) {
    loop {
        queue.notify.notified().await;
        if queue.closing.load(Ordering::Acquire) {
            let _ = w.shutdown().await;
            shared.disconnected(id, DisconnectReason::Local);
            return;
        }
        loop {
            let Some(out) = lock(&queue.frames).pop_front() else {
                break;
            };
            let len = u32::try_from(out.bytes.len() + 1).unwrap_or(u32::MAX);
            let mut header = [0u8; 5];
            header[..4].copy_from_slice(&len.to_le_bytes());
            header[4] = u8::from(out.channel == Channel::Unreliable);
            let ok = w.write_all(&header).await.is_ok() && w.write_all(&out.bytes).await.is_ok();
            shared.pool.give_back(out.bytes);
            if !ok {
                shared.disconnected(id, DisconnectReason::Io);
                return;
            }
        }
    }
}

async fn reader(mut r: OwnedReadHalf, shared: Arc<Shared>, id: ConnectionId, check_preamble: bool) {
    let reason = async {
        if check_preamble {
            let mut pre = [0u8; 4];
            if r.read_exact(&mut pre).await.is_err() || pre != PREAMBLE {
                return DisconnectReason::ProtocolViolation;
            }
        }
        let mut head = [0u8; 5];
        loop {
            if r.read_exact(&mut head).await.is_err() {
                return DisconnectReason::Closed;
            }
            let [l0, l1, l2, l3, kind] = head;
            let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
            let channel = match kind {
                0 => Channel::Reliable,
                1 => Channel::Unreliable,
                _ => return DisconnectReason::ProtocolViolation,
            };
            if len == 0 || len > MAX_FRAME + 1 {
                return DisconnectReason::ProtocolViolation;
            }
            let mut buf = shared.pool.copy_of(&[]);
            buf.resize(len - 1, 0);
            if r.read_exact(&mut buf).await.is_err() {
                return DisconnectReason::Closed;
            }
            shared.push(NetEvent::Frame(id, channel, buf));
        }
    }
    .await;
    shared.disconnected(id, reason);
}

/// The server side: accepts TCP connections.
pub struct TcpServer {
    shared: Arc<Shared>,
    poller: Poller,
    local_addr: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl TcpServer {
    /// Binds `addr` and starts accepting.
    ///
    /// # Errors
    /// [`NetError::Io`] if the socket cannot be bound.
    pub fn bind(runtime: &NetRuntime, addr: SocketAddr) -> Result<Self, NetError> {
        let listener = runtime
            .handle()
            .block_on(tokio::net::TcpListener::bind(addr))
            .map_err(|e| NetError::Io(e.kind()))?;
        let local_addr = listener.local_addr().map_err(|e| NetError::Io(e.kind()))?;
        let shared = Shared::new();
        let sh = Arc::clone(&shared);
        let next = Arc::new(AtomicU64::new(1));
        let accept = runtime.handle().spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                let id = ConnectionId(next.fetch_add(1, Ordering::Relaxed));
                let (r, w) = stream.into_split();
                let queue = SendQueue::new();
                sh.connected(id, Arc::clone(&queue));
                tokio::spawn(writer(w, queue, Arc::clone(&sh), id));
                tokio::spawn(reader(r, Arc::clone(&sh), id, true));
            }
        });
        Ok(Self {
            shared,
            poller: Poller::new(),
            local_addr,
            accept,
        })
    }

    /// The bound address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for TcpServer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.accept.abort();
        let conns: Vec<ConnectionId> = lock(&self.shared.conns).keys().copied().collect();
        for c in conns {
            self.shared.close(c);
        }
    }
}

impl Transport for TcpServer {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        self.poller.poll(&self.shared, sink);
    }

    fn send(&mut self, conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        self.shared.send(conn, channel, bytes, MAX_UNRELIABLE)
    }

    fn disconnect(&mut self, conn: ConnectionId) {
        self.shared.close(conn);
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn max_unreliable_payload(&self) -> usize {
        MAX_UNRELIABLE
    }
}

/// The client side: one connection, as [`crate::quic::SERVER`].
pub struct TcpClient {
    shared: Arc<Shared>,
    poller: Poller,
}

impl TcpClient {
    /// Connects to `addr` and blocks until connected.
    ///
    /// # Errors
    /// [`NetError::Connect`] if the connection fails.
    pub fn connect(runtime: &NetRuntime, addr: SocketAddr) -> Result<Self, NetError> {
        let shared = Shared::new();
        let sh = Arc::clone(&shared);
        runtime.handle().block_on(async move {
            let stream = tokio::net::TcpStream::connect(addr)
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let _ = stream.set_nodelay(true);
            let (r, mut w) = stream.into_split();
            w.write_all(&PREAMBLE)
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let queue = SendQueue::new();
            sh.connected(crate::quic::SERVER, Arc::clone(&queue));
            tokio::spawn(writer(w, queue, Arc::clone(&sh), crate::quic::SERVER));
            tokio::spawn(reader(r, sh, crate::quic::SERVER, false));
            Ok::<_, NetError>(())
        })?;
        Ok(Self {
            shared,
            poller: Poller::new(),
        })
    }
}

impl Drop for TcpClient {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.close(crate::quic::SERVER);
    }
}

impl Transport for TcpClient {
    fn poll(&mut self, sink: &mut dyn FnMut(TransportEvent<'_>)) {
        self.poller.poll(&self.shared, sink);
    }

    fn send(&mut self, conn: ConnectionId, channel: Channel, bytes: &[u8]) -> Result<(), TransportError> {
        self.shared.send(conn, channel, bytes, MAX_UNRELIABLE)
    }

    fn disconnect(&mut self, conn: ConnectionId) {
        self.shared.close(conn);
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn max_unreliable_payload(&self) -> usize {
        MAX_UNRELIABLE
    }
}
