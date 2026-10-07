//! QUIC transport (decision 0002): one reliable bidirectional stream per
//! connection for [`Channel::Reliable`], and RFC 9221 datagrams under the
//! Mantis sequence header for [`Channel::Unreliable`].
//!
//! Stream framing: the client opens the stream with the 4-byte preamble
//! `MTS1`, then frames are `len: u32 LE` followed by `len` bytes
//! (`len <= MAX_FRAME`). Datagram framing: `seq: u32 LE` followed by the
//! payload; the receiver drops any datagram whose sequence is not newer than
//! the last accepted one.
//!
//! The listener's certificate reloads without a restart
//! ([`QuicServer::reloader`], [`CertWatcher`]): connections already open
//! keep the session they negotiated, and every new connection gets the new
//! chain.

use std::net::SocketAddr;
use std::path::PathBuf;
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

/// The listener's certificate chain and key: a self-signed one generated
/// for development and tests ([`ServerCertificate::localhost`]), one a
/// development CA issued ([`DevCa::issue`]), or a chain issued by the
/// deployment ([`ServerCertificate::from_pem`], [`ServerCertificate::from_der`]).
#[derive(Clone)]
pub struct ServerCertificate {
    /// The certificate (clients pin it, or its issuer).
    pub cert_der: Vec<u8>,
    /// Intermediates after it, if any.
    chain: Vec<Vec<u8>>,
    key_der: Vec<u8>,
}

/// The earlier name of [`ServerCertificate`].
pub type DevCertificate = ServerCertificate;

impl ServerCertificate {
    /// From DER: the chain (leaf first) and its PKCS#8 key.
    ///
    /// # Errors
    /// [`NetError::Tls`] for an empty chain.
    pub fn from_der(chain: Vec<Vec<u8>>, key_pkcs8: Vec<u8>) -> Result<Self, NetError> {
        let mut certs = chain.into_iter();
        let cert_der = certs
            .next()
            .ok_or_else(|| NetError::Tls("an empty certificate chain".to_owned()))?;
        Ok(Self {
            cert_der,
            chain: certs.collect(),
            key_der: key_pkcs8,
        })
    }

    /// The PKCS#8 key (DER), for writing the pair out.
    #[must_use]
    pub fn key_pkcs8(&self) -> &[u8] {
        &self.key_der
    }

    /// The chain and key as PEM text (the files a listener reloads from).
    #[must_use]
    pub fn to_pem(&self) -> (String, String) {
        let chain: String = std::iter::once(&self.cert_der)
            .chain(&self.chain)
            .map(|c| pem("CERTIFICATE", c))
            .collect();
        (chain, pem("PRIVATE KEY", &self.key_der))
    }

    /// Generates a certificate for `localhost`.
    ///
    /// # Errors
    /// [`NetError::Tls`] if generation fails.
    pub fn localhost() -> Result<Self, NetError> {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .map_err(|e| NetError::Tls(e.to_string()))?;
        Ok(Self {
            cert_der: ck.cert.der().to_vec(),
            chain: Vec::new(),
            key_der: ck.signing_key.serialize_der(),
        })
    }

    /// From PEM: the chain (leaf first) and its PKCS#8 key.
    ///
    /// # Errors
    /// [`NetError::Tls`] for a file that holds no certificate or no key.
    pub fn from_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<Self, NetError> {
        use rustls::pki_types::pem::PemObject;
        let mut certs = CertificateDer::pem_slice_iter(chain_pem)
            .map(|c| c.map(|c| c.to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| NetError::Tls(format!("the certificate file: {e}")))?
            .into_iter();
        let cert_der = certs
            .next()
            .ok_or_else(|| NetError::Tls("the certificate file holds no certificate".to_owned()))?;
        let key = PrivatePkcs8KeyDer::from_pem_slice(key_pem)
            .map_err(|e| NetError::Tls(format!("the key file: {e}")))?;
        Ok(Self {
            cert_der,
            chain: certs.collect(),
            key_der: key.secret_pkcs8_der().to_vec(),
        })
    }

    /// A QUIC server configuration presenting this chain (refused when the
    /// key does not match the certificate).
    fn server_config(&self) -> Result<quinn::ServerConfig, NetError> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        let chain = std::iter::once(&self.cert_der)
            .chain(&self.chain)
            .map(|c| CertificateDer::from(c.clone()))
            .collect();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
            .map_err(|e| NetError::Tls(e.to_string()))?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
        config.transport_config(Arc::new(transport_config()));
        Ok(config)
    }
}

/// Reloads a [`QuicServer`]'s certificate: connections already open keep
/// theirs, every new connection gets the new chain. Cloneable, usable from
/// any thread.
#[derive(Clone)]
pub struct CertReloader {
    endpoint: quinn::Endpoint,
}

impl core::fmt::Debug for CertReloader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CertReloader").finish_non_exhaustive()
    }
}

impl CertReloader {
    /// Presents `cert` to new connections from now on.
    ///
    /// # Errors
    /// [`NetError::Tls`] when the chain and key do not make a configuration
    /// (a key that does not match its certificate): the old one stays.
    pub fn reload(&self, cert: &DevCertificate) -> Result<(), NetError> {
        self.endpoint.set_server_config(Some(cert.server_config()?));
        Ok(())
    }
}

/// Watches a chain and key file pair and reloads the listener when either
/// changes: the trigger a deployment uses (write the key, then the chain;
/// a pair caught mid-rotation does not load and is tried again at the next
/// check). Check it every tick; it reads the files at most once per
/// `every`.
pub struct CertWatcher {
    chain: PathBuf,
    key: PathBuf,
    reloader: CertReloader,
    every: std::time::Duration,
    next: std::time::Instant,
    /// The bytes last loaded.
    loaded: Option<(Vec<u8>, Vec<u8>)>,
    /// Reloads that took effect.
    pub reloads: u64,
}

impl core::fmt::Debug for CertWatcher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CertWatcher")
            .field("chain", &self.chain)
            .field("key", &self.key)
            .field("reloads", &self.reloads)
            .finish_non_exhaustive()
    }
}

impl CertWatcher {
    /// Watches `chain` and `key` (PEM), as loaded now, for `reloader`.
    #[must_use]
    pub fn new(chain: PathBuf, key: PathBuf, reloader: CertReloader, every: std::time::Duration) -> Self {
        let loaded = std::fs::read(&chain).ok().zip(std::fs::read(&key).ok());
        Self {
            chain,
            key,
            reloader,
            every,
            next: std::time::Instant::now() + every,
            loaded,
            reloads: 0,
        }
    }

    /// Reloads if the files changed since the last load and the check is
    /// due. `None`: nothing to do; `Some(Ok)`: the new chain is presented;
    /// `Some(Err)`: the files changed but do not load (the old chain stays;
    /// tried again at the next check).
    pub fn poll(&mut self) -> Option<Result<(), NetError>> {
        let now = std::time::Instant::now();
        if now < self.next {
            return None;
        }
        self.next = now + self.every;
        let (Ok(chain), Ok(key)) = (std::fs::read(&self.chain), std::fs::read(&self.key)) else {
            return Some(Err(NetError::Tls(
                "the certificate or key file is unreadable".to_owned(),
            )));
        };
        if self
            .loaded
            .as_ref()
            .is_some_and(|(c, k)| *c == chain && *k == key)
        {
            return None;
        }
        let result = DevCertificate::from_pem(&chain, &key).and_then(|cert| self.reloader.reload(&cert));
        if result.is_ok() {
            self.loaded = Some((chain, key));
            self.reloads += 1;
        }
        Some(result)
    }
}

impl core::fmt::Debug for ServerCertificate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ServerCertificate").finish_non_exhaustive()
    }
}

/// A development certificate authority issuing listener certificates, for
/// tests and local development only (a deployment issues its own).
pub struct DevCa {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    der: Vec<u8>,
    pem: String,
}

impl core::fmt::Debug for DevCa {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DevCa").finish_non_exhaustive()
    }
}

/// A certificate's validity bounds, `(year, month, day)` each.
pub type Validity = ((i32, u8, u8), (i32, u8, u8));

impl DevCa {
    /// A new CA.
    ///
    /// # Errors
    /// [`NetError::Tls`] when generation fails.
    pub fn new() -> Result<Self, NetError> {
        let tls = |e: rcgen::Error| NetError::Tls(e.to_string());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(tls)?;
        let mut p = rcgen::CertificateParams::default();
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "mantis development game CA");
        p.distinguished_name = dn;
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        p.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let cert = p.self_signed(&key).map_err(tls)?;
        Ok(Self {
            der: cert.der().to_vec(),
            pem: cert.pem(),
            issuer: rcgen::Issuer::new(p, key),
        })
    }

    /// The CA certificate (DER): what clients trust.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.der
    }

    /// The CA certificate (PEM).
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.pem
    }

    /// A listener certificate for `names` (DNS names, or IP literals),
    /// valid for centuries either side of now.
    ///
    /// # Errors
    /// [`NetError::Tls`] when issuance fails.
    pub fn issue(&self, names: &[&str]) -> Result<ServerCertificate, NetError> {
        self.issue_with_validity(names, ((1975, 1, 1), (4096, 1, 1)))
    }

    /// [`DevCa::issue`] with explicit validity (expired and not-yet-valid
    /// leaves for tests).
    ///
    /// # Errors
    /// [`NetError::Tls`] when issuance fails.
    pub fn issue_with_validity(
        &self,
        names: &[&str],
        validity: Validity,
    ) -> Result<ServerCertificate, NetError> {
        let tls = |e: rcgen::Error| NetError::Tls(e.to_string());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(tls)?;
        let mut p = rcgen::CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
            .map_err(tls)?;
        p.is_ca = rcgen::IsCa::ExplicitNoCa;
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let ((y0, m0, d0), (y1, m1, d1)) = validity;
        p.not_before = rcgen::date_time_ymd(y0, m0, d0);
        p.not_after = rcgen::date_time_ymd(y1, m1, d1);
        let cert = p.signed_by(&key, &self.issuer).map_err(tls)?;
        Ok(ServerCertificate {
            cert_der: cert.der().to_vec(),
            chain: Vec::new(),
            key_der: key.serialize_der(),
        })
    }
}

/// How a client decides to trust the game server.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ServerTrust {
    /// Development: exactly this certificate (DER), named `localhost`.
    Pinned(Vec<u8>),
    /// Production: a chain to one of `roots` (DER), valid for
    /// `server_name` (sent as SNI and checked against the certificate).
    Roots {
        /// The trusted CAs.
        roots: Vec<Vec<u8>>,
        /// The name the server's certificate must carry.
        server_name: String,
    },
    /// The public web PKI roots plus `extra` (DER, an operator's own CAs),
    /// for `server_name`. Needs the `public-roots` feature; without it a
    /// connect is refused with [`NetError::Tls`].
    Public {
        /// CAs trusted besides the public roots.
        extra: Vec<Vec<u8>>,
        /// The name the server's certificate must carry.
        server_name: String,
    },
}

impl ServerTrust {
    /// Every certificate of a PEM bundle (an operator's CA, or the
    /// cluster's), for `server_name`.
    ///
    /// # Errors
    /// [`NetError::Tls`] for a bundle that holds no certificate.
    pub fn from_pem_bundle(pem: &[u8], server_name: &str) -> Result<Self, NetError> {
        use rustls::pki_types::pem::PemObject;
        let roots = CertificateDer::pem_slice_iter(pem)
            .map(|c| c.map(|c| c.to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| NetError::Tls(format!("the CA bundle: {e}")))?;
        if roots.is_empty() {
            return Err(NetError::Tls("the CA bundle holds no certificate".to_owned()));
        }
        Ok(Self::Roots {
            roots,
            server_name: server_name.to_owned(),
        })
    }

    /// [`ServerTrust::Public`] with `extra_pem`'s certificates (possibly
    /// none) besides the public roots.
    ///
    /// # Errors
    /// [`NetError::Tls`] for a bundle that does not parse.
    pub fn public_with_pem(extra_pem: &[u8], server_name: &str) -> Result<Self, NetError> {
        use rustls::pki_types::pem::PemObject;
        let extra = CertificateDer::pem_slice_iter(extra_pem)
            .map(|c| c.map(|c| c.to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| NetError::Tls(format!("the CA bundle: {e}")))?;
        Ok(Self::Public {
            extra,
            server_name: server_name.to_owned(),
        })
    }

    /// The root store this trust verifies against.
    fn root_store(&self) -> Result<rustls::RootCertStore, NetError> {
        let mut roots = rustls::RootCertStore::empty();
        let own: &[Vec<u8>] = match self {
            Self::Pinned(der) => std::slice::from_ref(der),
            Self::Roots { roots, .. } => roots,
            Self::Public { extra, .. } => public_roots(&mut roots, extra)?,
        };
        for der in own {
            roots
                .add(CertificateDer::from(der.clone()))
                .map_err(|e| NetError::Tls(e.to_string()))?;
        }
        Ok(roots)
    }

    fn server_name(&self) -> &str {
        match self {
            Self::Pinned(_) => "localhost",
            Self::Roots { server_name, .. } | Self::Public { server_name, .. } => server_name,
        }
    }
}

/// Adds the public web PKI roots to `roots`; `extra` is trusted besides.
#[cfg(feature = "public-roots")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature with and without the feature"
)]
fn public_roots<'a>(
    roots: &mut rustls::RootCertStore,
    extra: &'a [Vec<u8>],
) -> Result<&'a [Vec<u8>], NetError> {
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(extra)
}

/// Without the `public-roots` feature, public trust is refused.
#[cfg(not(feature = "public-roots"))]
fn public_roots<'a>(
    _roots: &mut rustls::RootCertStore,
    _extra: &'a [Vec<u8>],
) -> Result<&'a [Vec<u8>], NetError> {
    Err(NetError::Tls("built without the public-roots feature".to_owned()))
}

/// Why a server's certificate was not trusted.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TrustFailure {
    /// It does not chain to a trusted CA (or is not the pinned one).
    UnknownIssuer,
    /// It is not valid for the name the client expected.
    WrongName {
        /// The name expected.
        expected: String,
    },
    /// It has expired.
    Expired,
    /// It is not valid yet.
    NotYetValid,
    /// Anything else, as rustls named it.
    Other(String),
}

impl core::fmt::Display for TrustFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownIssuer => f.write_str("its certificate is not issued by a trusted authority"),
            Self::WrongName { expected } => write!(f, "its certificate is not valid for {expected}"),
            Self::Expired => f.write_str("its certificate has expired"),
            Self::NotYetValid => f.write_str("its certificate is not valid yet"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

/// The standard verifier, recording why it refused.
#[derive(Debug)]
struct Classifying {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    expected: String,
    failure: Arc<std::sync::Mutex<Option<TrustFailure>>>,
}

impl rustls::client::danger::ServerCertVerifier for Classifying {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let result =
            self.inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);
        if let Err(rustls::Error::InvalidCertificate(e)) = &result {
            use rustls::CertificateError as C;
            let why = match e {
                C::UnknownIssuer | C::BadSignature => TrustFailure::UnknownIssuer,
                C::NotValidForName | C::NotValidForNameContext { .. } => TrustFailure::WrongName {
                    expected: self.expected.clone(),
                },
                C::Expired | C::ExpiredContext { .. } => TrustFailure::Expired,
                C::NotValidYet | C::NotValidYetContext { .. } => TrustFailure::NotYetValid,
                other => TrustFailure::Other(format!("{other:?}")),
            };
            *lock(&self.failure) = Some(why);
        }
        result
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// One PEM block.
fn pem(label: &str, der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = Vec::with_capacity(der.len().div_ceil(3) * 4);
    for chunk in der.chunks(3) {
        let n = chunk
            .iter()
            .zip([16u32, 8, 0])
            .fold(0u32, |n, (b, shift)| n | (u32::from(*b) << shift));
        for i in 0..4usize {
            let sextet = (n >> (18 - 6 * i)) & 63;
            let c = ALPHABET.get(sextet as usize).copied().unwrap_or(b'=');
            b64.push(if i <= chunk.len() { c } else { b'=' });
        }
    }
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
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

/// How long a closing connection waits for its peer to read the frames
/// queued before the close.
const CLOSE_LINGER: std::time::Duration = std::time::Duration::from_millis(500);

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
            // What was queued before the close still goes (a refusal, then
            // the disconnect): the reliable frames are written and the
            // stream finished, and the peer has a moment to read them.
            loop {
                let Some(out) = lock(&queue.frames).pop_front() else {
                    break;
                };
                if out.channel == Channel::Reliable {
                    let len = u32::try_from(out.bytes.len()).unwrap_or(u32::MAX);
                    header.copy_from_slice(&len.to_le_bytes());
                    if send.write_all(&header).await.is_err() || send.write_all(&out.bytes).await.is_err() {
                        break;
                    }
                }
                shared.pool.give_back(out.bytes);
            }
            if send.finish().is_ok() {
                let _ = tokio::time::timeout(CLOSE_LINGER, send.stopped()).await;
            }
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

/// A client configuration trusting servers as `trust` says, and where its
/// verifier records why it refused one.
type TrustSlot = Arc<std::sync::Mutex<Option<TrustFailure>>>;

fn client_config(trust: &ServerTrust) -> Result<(quinn::ClientConfig, TrustSlot), NetError> {
    let roots = trust.root_store()?;
    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .map_err(|e| NetError::Tls(e.to_string()))?;
    let failure: TrustSlot = Arc::default();
    let verifier = Arc::new(Classifying {
        inner,
        expected: trust.server_name().to_owned(),
        failure: Arc::clone(&failure),
    });
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| NetError::Tls(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config()));
    Ok((config, failure))
}

/// Many client connections from one endpoint, each opened on demand without
/// blocking ([`crate::gateway::Dialer`]): a gateway's connections to cell
/// hosts. A connection that cannot be made is reported `Disconnected`.
pub struct QuicDialer {
    shared: Arc<Shared>,
    poller: Poller,
    endpoint: quinn::Endpoint,
    handle: tokio::runtime::Handle,
    server_name: String,
    next: u64,
}

impl QuicDialer {
    /// A dialer trusting hosts as `trust` says (a cluster CA naming every
    /// host by one name, or a pinned development certificate).
    ///
    /// # Errors
    /// [`NetError`] for TLS or socket failures.
    pub fn new(runtime: &NetRuntime, trust: &ServerTrust) -> Result<Self, NetError> {
        let (config, _) = client_config(trust)?;
        let endpoint = {
            let _guard = runtime.handle().enter();
            let bind: SocketAddr = "0.0.0.0:0"
                .parse()
                .map_err(|_| NetError::Connect("bind address".to_owned()))?;
            let mut endpoint = quinn::Endpoint::client(bind).map_err(|e| NetError::Io(e.kind()))?;
            endpoint.set_default_client_config(config);
            endpoint
        };
        Ok(Self {
            shared: Shared::new(),
            poller: Poller::new(),
            endpoint,
            handle: runtime.handle().clone(),
            server_name: trust.server_name().to_owned(),
            next: 1,
        })
    }
}

impl crate::gateway::Dialer for QuicDialer {
    fn dial(&mut self, address: &str) -> Option<ConnectionId> {
        let addr: SocketAddr = address.parse().ok()?;
        let id = ConnectionId(self.next);
        self.next += 1;
        let (endpoint, name, sh) = (
            self.endpoint.clone(),
            self.server_name.clone(),
            Arc::clone(&self.shared),
        );
        self.handle.spawn(async move {
            let opened = async {
                let conn = endpoint.connect(addr, &name).ok()?.await.ok()?;
                let (mut send, recv) = conn.open_bi().await.ok()?;
                send.write_all(&PREAMBLE).await.ok()?;
                Some((conn, send, recv))
            }
            .await;
            let Some((conn, send, recv)) = opened else {
                sh.push(NetEvent::Disconnected(id, DisconnectReason::Io));
                return;
            };
            let queue = SendQueue::new();
            sh.connected(id, Arc::clone(&queue));
            tokio::spawn(datagram_task(conn.clone(), Arc::clone(&sh), id));
            tokio::spawn(writer_task(conn, send, queue, Arc::clone(&sh), id));
            reader_task(recv, sh, id, false).await;
        });
        Some(id)
    }
}

impl Drop for QuicDialer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

impl Transport for QuicDialer {
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
        let config = cert.server_config()?;
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

    /// A handle that reloads this listener's certificate (keep it before
    /// handing the server to a host).
    #[must_use]
    pub fn reloader(&self) -> CertReloader {
        CertReloader {
            endpoint: self.endpoint.clone(),
        }
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
        Self::connect_trusted(runtime, addr, &ServerTrust::Pinned(server_cert_der.to_vec()))
    }

    /// Connects to `addr`, trusting the server as `trust` says, and blocks
    /// until the connection and its stream are open.
    ///
    /// # Errors
    /// [`NetError::Untrusted`] naming why the server was not trusted, or
    /// [`NetError`] for TLS, socket, or connection failures.
    pub fn connect_trusted(
        runtime: &NetRuntime,
        addr: SocketAddr,
        trust: &ServerTrust,
    ) -> Result<Self, NetError> {
        let (config, failure) = client_config(trust)?;
        let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }
            .parse()
            .map_err(|_| NetError::Connect("bind address".to_owned()))?;
        let shared = Shared::new();
        let sh = Arc::clone(&shared);
        let name = trust.server_name().to_owned();
        let untrusted = || lock(&failure).take().map(NetError::Untrusted);
        let endpoint = runtime.handle().block_on(async move {
            let mut endpoint = quinn::Endpoint::client(bind).map_err(|e| NetError::Io(e.kind()))?;
            endpoint.set_default_client_config(config);
            let conn = endpoint
                .connect(addr, &name)
                .map_err(|e| NetError::Connect(e.to_string()))?
                .await
                .map_err(|e| untrusted().unwrap_or_else(|| NetError::Connect(e.to_string())))?;
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
