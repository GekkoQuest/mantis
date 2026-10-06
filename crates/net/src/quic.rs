//! QUIC transport (decision 0002): one reliable bidirectional stream per
//! connection for [`Channel::Reliable`], and RFC 9221 datagrams under the
//! Mantis sequence header for [`Channel::Unreliable`].
//!
//! Stream framing: the client opens the stream with the 4-byte preamble
//! `MTS1`, then frames are `len: u32 LE` followed by `len` bytes
//! (`len <= MAX_FRAME`). Datagram framing: `seq: u32 LE` followed by the
//! payload; the receiver drops any datagram whose sequence is not newer than
//! the last accepted one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mantis_adapter_contract::{
    Channel, ConnectionId, DisconnectReason, Transport, TransportError, TransportEvent, TransportKind,
};
use quinn::{Connection, RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::shared::{MAX_FRAME, NetEvent, Poller, SendQueue, SeqFilter, Shared, lock, split_seq};
use crate::{NetError, NetRuntime};

const PREAMBLE: [u8; 4] = *b"MTS1";
const ALPN: &[u8] = b"mantis/1";

/// A conservative unreliable payload limit: 1200-byte QUIC datagrams minus
/// QUIC overhead and the 4-byte sequence header.
pub const MAX_UNRELIABLE: usize = 1100;

/// A self-signed certificate for development and tests (never production).
#[derive(Clone)]
pub struct DevCertificate {
    /// The certificate (clients pin it).
    pub cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl DevCertificate {
    /// Generates a certificate for `localhost`.
    ///
    /// # Errors
    /// [`NetError::Tls`] if generation fails.
    pub fn localhost() -> Result<Self, NetError> {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .map_err(|e| NetError::Tls(e.to_string()))?;
        Ok(Self {
            cert_der: ck.cert.der().to_vec(),
            key_der: ck.signing_key.serialize_der(),
        })
    }
}

impl core::fmt::Debug for DevCertificate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DevCertificate").finish_non_exhaustive()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(std::time::Duration::from_secs(2)));
    let _ = quinn::IdleTimeout::try_from(std::time::Duration::from_secs(10))
        .map(|timeout| t.max_idle_timeout(Some(timeout)));
    t.datagram_receive_buffer_size(Some(1 << 20));
    t
}

async fn writer_task(
    conn: Connection,
    mut send: SendStream,
    queue: Arc<SendQueue>,
    shared: Arc<Shared>,
    id: ConnectionId,
) {
    let mut seq: u32 = 0;
    let mut header = [0u8; 4];
    let mut datagram = Vec::with_capacity(MAX_UNRELIABLE + 4);
    loop {
        queue.notify.notified().await;
        if queue.closing.load(Ordering::Acquire) {
            conn.close(0u32.into(), b"closed");
            shared.disconnected(id, DisconnectReason::Local);
            return;
        }
        loop {
            let Some(out) = lock(&queue.frames).pop_front() else {
                break;
            };
            let ok = match out.channel {
                Channel::Reliable => {
                    let len = u32::try_from(out.bytes.len()).unwrap_or(u32::MAX);
                    header.copy_from_slice(&len.to_le_bytes());
                    send.write_all(&header).await.is_ok() && send.write_all(&out.bytes).await.is_ok()
                }
                Channel::Unreliable => {
                    seq = seq.wrapping_add(1);
                    datagram.clear();
                    datagram.extend_from_slice(&seq.to_le_bytes());
                    datagram.extend_from_slice(&out.bytes);
                    // A datagram that cannot be sent right now is dropped:
                    // that is what unreliable means.
                    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&datagram));
                    true
                }
            };
            shared.pool.give_back(out.bytes);
            if !ok {
                shared.disconnected(id, DisconnectReason::Io);
                return;
            }
        }
    }
}

async fn reader_task(mut recv: RecvStream, shared: Arc<Shared>, id: ConnectionId, check_preamble: bool) {
    let reason = async {
        if check_preamble {
            let mut pre = [0u8; 4];
            if recv.read_exact(&mut pre).await.is_err() || pre != PREAMBLE {
                return DisconnectReason::ProtocolViolation;
            }
        }
        let mut len_buf = [0u8; 4];
        loop {
            if recv.read_exact(&mut len_buf).await.is_err() {
                return DisconnectReason::Closed;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > MAX_FRAME {
                return DisconnectReason::ProtocolViolation;
            }
            let mut buf = shared.pool.copy_of(&[]);
            buf.resize(len, 0);
            if recv.read_exact(&mut buf).await.is_err() {
                return DisconnectReason::Closed;
            }
            shared.push(NetEvent::Frame(id, Channel::Reliable, buf));
        }
    }
    .await;
    shared.disconnected(id, reason);
}

async fn datagram_task(conn: Connection, shared: Arc<Shared>, id: ConnectionId) {
    let mut filter = SeqFilter::default();
    while let Ok(d) = conn.read_datagram().await {
        let Some((seq, payload)) = split_seq(&d) else {
            continue;
        };
        if filter.accept(seq) {
            let buf = shared.pool.copy_of(payload);
            shared.push(NetEvent::Frame(id, Channel::Unreliable, buf));
        }
    }
}

/// The server side: accepts connections on a UDP port.
pub struct QuicServer {
    shared: Arc<Shared>,
    poller: Poller,
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
}

impl QuicServer {
    /// Binds `addr` and starts accepting with `cert`.
    ///
    /// # Errors
    /// [`NetError`] for TLS or socket failures.
    pub fn bind(runtime: &NetRuntime, addr: SocketAddr, cert: &DevCertificate) -> Result<Self, NetError> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone()));
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert.cert_der.clone())], key)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
        config.transport_config(Arc::new(transport_config()));
        let endpoint = {
            let _guard = runtime.handle().enter();
            quinn::Endpoint::server(config, addr).map_err(|e| NetError::Io(e.kind()))?
        };
        let local_addr = endpoint.local_addr().map_err(|e| NetError::Io(e.kind()))?;
        let shared = Shared::new();
        let next = Arc::new(AtomicU64::new(1));
        let ep = endpoint.clone();
        let sh = Arc::clone(&shared);
        runtime.handle().spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let sh = Arc::clone(&sh);
                let id = ConnectionId(next.fetch_add(1, Ordering::Relaxed));
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let Ok((send, recv)) = conn.accept_bi().await else {
                        return;
                    };
                    let queue = SendQueue::new();
                    sh.connected(id, Arc::clone(&queue));
                    tokio::spawn(datagram_task(conn.clone(), Arc::clone(&sh), id));
                    tokio::spawn(writer_task(conn.clone(), send, queue, Arc::clone(&sh), id));
                    reader_task(recv, sh, id, true).await;
                });
            }
        });
        Ok(Self {
            shared,
            poller: Poller::new(),
            endpoint,
            local_addr,
        })
    }

    /// The bound address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for QuicServer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

impl Transport for QuicServer {
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
        TransportKind::Quic
    }

    fn max_unreliable_payload(&self) -> usize {
        MAX_UNRELIABLE
    }
}

/// The client side: one connection to a server, as [`ConnectionId`] 0.
pub struct QuicClient {
    shared: Arc<Shared>,
    poller: Poller,
    endpoint: quinn::Endpoint,
}

/// The connection id under which a client sees its server.
pub const SERVER: ConnectionId = ConnectionId(0);

impl QuicClient {
    /// Connects to `addr`, trusting exactly `server_cert_der`, and blocks until
    /// the connection and its stream are open.
    ///
    /// # Errors
    /// [`NetError`] for TLS, socket, or connection failures.
    pub fn connect(runtime: &NetRuntime, addr: SocketAddr, server_cert_der: &[u8]) -> Result<Self, NetError> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(server_cert_der.to_vec()))
            .map_err(|e| NetError::Tls(e.to_string()))?;
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic));
        config.transport_config(Arc::new(transport_config()));
        let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }
            .parse()
            .map_err(|_| NetError::Connect("bind address".to_owned()))?;
        let shared = Shared::new();
        let sh = Arc::clone(&shared);
        let endpoint = runtime.handle().block_on(async move {
            let mut endpoint = quinn::Endpoint::client(bind).map_err(|e| NetError::Io(e.kind()))?;
            endpoint.set_default_client_config(config);
            let conn = endpoint
                .connect(addr, "localhost")
                .map_err(|e| NetError::Connect(e.to_string()))?
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let (mut send, recv) = conn
                .open_bi()
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            send.write_all(&PREAMBLE)
                .await
                .map_err(|e| NetError::Connect(e.to_string()))?;
            let queue = SendQueue::new();
            sh.connected(SERVER, Arc::clone(&queue));
            tokio::spawn(datagram_task(conn.clone(), Arc::clone(&sh), SERVER));
            tokio::spawn(writer_task(conn.clone(), send, queue, Arc::clone(&sh), SERVER));
            tokio::spawn(reader_task(recv, sh, SERVER, false));
            Ok::<_, NetError>(endpoint)
        })?;
        Ok(Self {
            shared,
            poller: Poller::new(),
            endpoint,
        })
    }
}

impl Drop for QuicClient {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

impl Transport for QuicClient {
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
        TransportKind::Quic
    }

    fn max_unreliable_payload(&self) -> usize {
        MAX_UNRELIABLE
    }
}
