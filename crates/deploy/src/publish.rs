//! `mantisd registry serve`: publishes signed registries over HTTPS for
//! nodes on other machines (`[node] registry = "https://..."`).
//!
//! It serves `GET /<name>.toml` from one directory, re-read on every
//! request, so a deploy tool publishes by writing a new signed file there.
//! It serves nothing else and changes nothing. Its certificate is an
//! ordinary TLS server certificate for the names nodes dial (`mantisd certs
//! --server`); nodes pin the CA that signed it (`[node] registry_ca`). The
//! registries are signed, so this server is a transport only: a node
//! verifies each registry with the deploy key whatever served it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::pki;
use crate::source::MAX_REGISTRY;

/// A running registry server.
pub struct RegistryServer {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl RegistryServer {
    /// Serves `dir` on `listen` with the certificate chain and key in
    /// `cert` and `key` (PEM).
    ///
    /// # Errors
    /// The files do not make a TLS configuration, or the bind failed.
    pub async fn start(listen: SocketAddr, dir: PathBuf, cert: &Path, key: &Path) -> Result<Self, String> {
        let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
        let chain: Vec<rustls::pki_types::CertificateDer<'static>> = pki::pem_certs(&read(cert)?)?
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            pki::pem_key(&read(key)?)?,
        ));
        let config =
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_no_client_auth()
                .with_single_cert(chain, key_der)
                .map_err(|e| format!("{}: {e}", cert.display()))?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| format!("{listen}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let dir = Arc::new(dir);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, dir) = (acceptor.clone(), Arc::clone(&dir));
                tokio::spawn(async move {
                    let serve = async {
                        let mut tls = acceptor.accept(tcp).await.ok()?;
                        let (code, body) = answer(&mut tls, &dir).await;
                        let head = format!(
                            "HTTP/1.1 {code}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\n\
                             connection: close\r\n\r\n",
                            body.len()
                        );
                        tls.write_all(head.as_bytes()).await.ok()?;
                        tls.write_all(&body).await.ok()?;
                        tls.shutdown().await.ok()
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(10), serve).await;
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

impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer(tls: &mut (impl AsyncReadExt + Unpin), dir: &Path) -> (&'static str, Vec<u8>) {
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match tls.read(&mut chunk).await {
            Ok(n) if n > 0 && head.len() < 4096 => head.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
            _ => return ("400 Bad Request", b"bad request\n".to_vec()),
        }
    }
    let line = String::from_utf8_lossy(&head);
    let mut words = line.lines().next().unwrap_or("").split(' ');
    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    if method != "GET" {
        return ("405 Method Not Allowed", b"GET only\n".to_vec());
    }
    // One file name, `.toml`, nothing that leaves the directory.
    let name = path.strip_prefix('/').unwrap_or("");
    let ok = std::path::Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !name.contains("..");
    if !ok {
        return ("404 Not Found", b"not found\n".to_vec());
    }
    match std::fs::read(dir.join(name)) {
        Ok(bytes) if bytes.len() <= MAX_REGISTRY => ("200 OK", bytes),
        Ok(_) => ("500 Internal Server Error", b"too large\n".to_vec()),
        Err(_) => ("404 Not Found", b"not found\n".to_vec()),
    }
}
