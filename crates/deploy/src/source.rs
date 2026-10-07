//! Where a node gets its signed registry, and how it follows new ones.
//!
//! `[node] registry` names one source:
//!
//! | Value | Source |
//! |---|---|
//! | `registry.toml` or `file:registry.toml` | one file |
//! | `dir:registries` | a directory a deploy tool writes registries into; the highest serial among the files that verify wins |
//! | `https://deploy.example.internal:8443/registry.toml` | an HTTPS URL; the server's certificate must chain to `[node] registry_ca` (pinned, never the system roots) |
//!
//! Every registry is verified with the deploy key before anything in it is
//! used, wherever it came from: the transport is not trusted, the
//! signature is. A node re-reads its source every `registry_refresh_s` and
//! applies a registry only when it verifies, names the same cluster, keeps
//! this node's own entry, and carries a **higher** serial; anything else is
//! refused, counted, and the running registry stays.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::keys::PUBLIC_KEY_BYTES;
use crate::registry::Registry;
use crate::target::Target;

/// The largest registry fetched.
pub const MAX_REGISTRY: usize = 1 << 20;

/// How long one fetch may take.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(10);

/// A registry source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// One file.
    File(PathBuf),
    /// A directory of registry files.
    Dir(PathBuf),
    /// An HTTPS URL with a pinned CA.
    Https {
        /// The server.
        server: Target,
        /// The path (with its leading `/`).
        path: String,
        /// The CA file the server's certificate must chain to.
        ca: PathBuf,
    },
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file {}", p.display()),
            Self::Dir(p) => write!(f, "directory {}", p.display()),
            Self::Https { server, path, .. } => write!(f, "https://{server}{path}"),
        }
    }
}

impl Source {
    /// Parses a `[node] registry` value; relative paths are relative to
    /// `base`. `ca` is `[node] registry_ca`, required for HTTPS and refused
    /// otherwise.
    ///
    /// # Errors
    /// Malformed, or a CA where none applies (or none where one must).
    pub fn parse(spec: &str, base: &Path, ca: Option<PathBuf>) -> Result<Self, String> {
        if let Some(rest) = spec.strip_prefix("https://") {
            let (authority, path) = rest
                .split_once('/')
                .map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
            let authority = if authority.contains(':') {
                authority.to_owned()
            } else {
                format!("{authority}:443")
            };
            let server = Target::parse(&authority)?;
            if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("{spec:?}: the path has whitespace"));
            }
            let ca =
                ca.ok_or("an https registry needs registry_ca: the CA its server's certificate chains to")?;
            return Ok(Self::Https { server, path, ca });
        }
        if spec.starts_with("http://") {
            return Err("the registry is fetched over https only".to_owned());
        }
        if ca.is_some() {
            return Err("registry_ca applies to an https registry only".to_owned());
        }
        let (dir, path) = match spec.strip_prefix("dir:") {
            Some(p) => (true, p),
            None => (false, spec.strip_prefix("file:").unwrap_or(spec)),
        };
        if path.is_empty() {
            return Err("an empty registry path".to_owned());
        }
        let path = base.join(path);
        Ok(if dir { Self::Dir(path) } else { Self::File(path) })
    }

    /// Reads the source and returns the registry it offers, verified with
    /// `deploy_key` (a directory: the highest serial that verifies).
    ///
    /// # Errors
    /// Nothing readable verifies.
    pub async fn load(&self, deploy_key: &[u8; PUBLIC_KEY_BYTES]) -> Result<Registry, String> {
        match self {
            Self::File(p) => {
                let text = read_text(p)?;
                Registry::verify(&text, deploy_key).map_err(|e| format!("{}: {e}", p.display()))
            }
            Self::Dir(dir) => {
                // Registries are small files: read in place (std::fs).
                let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                let mut best: Option<Registry> = None;
                let mut refused = Vec::new();
                for e in entries {
                    let path = e.map_err(|e| format!("{}: {e}", dir.display()))?.path();
                    if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                        continue;
                    }
                    match read_text(&path).and_then(|t| {
                        Registry::verify(&t, deploy_key).map_err(|e| format!("{}: {e}", path.display()))
                    }) {
                        Ok(r) if best.as_ref().is_none_or(|b| r.serial > b.serial) => best = Some(r),
                        Ok(_) => {}
                        Err(e) => refused.push(e),
                    }
                }
                best.ok_or_else(|| {
                    format!(
                        "{}: no registry verifies{}",
                        dir.display(),
                        if refused.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", refused.join("; "))
                        }
                    )
                })
            }
            Self::Https { server, path, ca } => {
                let text = fetch(server, path, ca).await?;
                Registry::verify(&text, deploy_key).map_err(|e| format!("https://{server}{path}: {e}"))
            }
        }
    }

    /// [`Source::load`] at start: an HTTPS server that does not answer yet
    /// (it starts beside the node) is asked again until `patience` runs
    /// out; a registry that answers but does not verify is refused at once.
    ///
    /// # Errors
    /// [`Source::load`], or no answer within `patience`.
    pub async fn load_at_start(
        &self,
        deploy_key: &[u8; PUBLIC_KEY_BYTES],
        patience: Duration,
    ) -> Result<Registry, String> {
        let Self::Https { server, path, ca } = self else {
            return self.load(deploy_key).await;
        };
        let start = tokio::time::Instant::now();
        let text = loop {
            match fetch(server, path, ca).await {
                Ok(text) => break text,
                Err(e) if start.elapsed() >= patience => {
                    return Err(format!("{e} (asked for {} s)", patience.as_secs()));
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        Registry::verify(&text, deploy_key).map_err(|e| format!("https://{server}{path}: {e}"))
    }
}

fn read_text(p: &Path) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if bytes.len() > MAX_REGISTRY {
        return Err(format!("{}: larger than {MAX_REGISTRY} bytes", p.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", p.display()))
}

/// `GET path` from `server` over TLS whose certificate chains to the CA in
/// `ca_file` (PEM), as HTTP/1.1 with `connection: close`; the body of a
/// 200 answer. Chunked transfer coding is refused (a registry is served as
/// a file).
async fn fetch(server: &Target, path: &str, ca_file: &Path) -> Result<String, String> {
    let ask = async {
        let pem = std::fs::read(ca_file).map_err(|e| format!("{}: {e}", ca_file.display()))?;
        let mut roots = rustls::RootCertStore::empty();
        for der in crate::pki::pem_certs(&pem)? {
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .map_err(|e| format!("{}: {e}", ca_file.display()))?;
        }
        let config =
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
        let name = match server.ip() {
            Some(ip) => rustls::pki_types::ServerName::IpAddress(ip.into()),
            None => rustls::pki_types::ServerName::try_from(server.host().to_owned())
                .map_err(|e| e.to_string())?,
        };
        let mut last = format!("{server}: no address");
        let mut tcp = None;
        for addr in server.resolve().await? {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        let tcp = tcp.ok_or(last)?;
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("https://{server}{path}: {e}"))?;
        let req = format!("GET {path} HTTP/1.1\r\nhost: {server}\r\nconnection: close\r\n\r\n");
        tls.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = tls.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
            if out.len() > MAX_REGISTRY + 16 * 1024 {
                return Err(format!(
                    "https://{server}{path}: larger than {MAX_REGISTRY} bytes"
                ));
            }
        }
        let text = String::from_utf8(out).map_err(|_| "not UTF-8".to_owned())?;
        let (head, body) = text.split_once("\r\n\r\n").ok_or("not an HTTP answer")?;
        let status = head.split(' ').nth(1).unwrap_or("");
        if status != "200" {
            return Err(format!("https://{server}{path}: answered {status}"));
        }
        if head
            .lines()
            .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:"))
        {
            return Err(format!(
                "https://{server}{path}: serve the registry as a plain file (no transfer coding)"
            ));
        }
        if let Some(len) = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        }) && len != body.len()
        {
            return Err(format!("https://{server}{path}: {} of {len} bytes", body.len()));
        }
        Ok(body.to_owned())
    };
    tokio::time::timeout(FETCH_DEADLINE, ask).await.map_err(|_| {
        format!(
            "https://{server}{path}: no answer within {} s",
            FETCH_DEADLINE.as_secs()
        )
    })?
}

/// Why a newer registry was not applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not newer: its serial is at or below the running one.
    NotNewer,
    /// It names another cluster.
    OtherCluster(String),
    /// This node's own entry is gone, or names another role.
    NotListed,
}

/// Whether `next` may replace `current` on node `instance`.
///
/// # Errors
/// [`Refused`].
pub fn admissible(current: &Registry, next: &Registry, instance: &str) -> Result<(), Refused> {
    if next.serial <= current.serial {
        return Err(Refused::NotNewer);
    }
    if next.cluster != current.cluster {
        return Err(Refused::OtherCluster(next.cluster.clone()));
    }
    let (Some(me), Some(was)) = (next.instance(instance), current.instance(instance)) else {
        return Err(Refused::NotListed);
    };
    if me.role != was.role {
        return Err(Refused::NotListed);
    }
    Ok(())
}

/// The running registry of a node, replaced when its source offers an
/// admissible newer one.
#[derive(Clone)]
pub struct Live {
    rx: watch::Receiver<Arc<Registry>>,
}

impl Live {
    /// The running registry.
    #[must_use]
    pub fn current(&self) -> Arc<Registry> {
        Arc::clone(&self.rx.borrow())
    }

    /// A receiver that sees every registry applied from now on.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<Registry>> {
        self.rx.clone()
    }
}

/// Told of each applied registry, and of each refusal or failure.
pub type Report = Arc<dyn Fn(Result<&Registry, String>) + Send + Sync>;

/// What a node needs to follow its source.
pub struct Follow {
    /// The source.
    pub source: Source,
    /// The deploy public key.
    pub deploy_key: [u8; PUBLIC_KEY_BYTES],
    /// This node's instance name.
    pub instance: String,
    /// How often the source is read again (`ZERO`: never).
    pub every: Duration,
    /// Called with each applied registry, and with each refusal or failure
    /// (for the node's log and metrics).
    pub report: Report,
}

/// Starts following `follow.source` from `first` on `handle`.
#[must_use]
pub fn follow(handle: &tokio::runtime::Handle, first: Registry, follow: Follow) -> Live {
    let (tx, rx) = watch::channel(Arc::new(first));
    if !follow.every.is_zero() {
        handle.spawn(async move {
            loop {
                tokio::time::sleep(follow.every).await;
                let current = Arc::clone(&tx.borrow());
                match follow.source.load(&follow.deploy_key).await {
                    Ok(next) => match admissible(&current, &next, &follow.instance) {
                        Ok(()) => {
                            (follow.report)(Ok(&next));
                            if tx.send(Arc::new(next)).is_err() {
                                return;
                            }
                        }
                        Err(Refused::NotNewer) => {}
                        Err(why) => (follow.report)(Err(format!(
                            "registry serial {} from {} refused: {why:?}",
                            next.serial, follow.source
                        ))),
                    },
                    Err(e) => (follow.report)(Err(e)),
                }
            }
        });
    }
    Live { rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_parse() {
        let base = Path::new("cfg");
        assert_eq!(
            Source::parse("r.toml", base, None).unwrap(),
            Source::File(base.join("r.toml"))
        );
        assert_eq!(
            Source::parse("file:r.toml", base, None).unwrap(),
            Source::File(base.join("r.toml"))
        );
        assert_eq!(
            Source::parse("dir:regs", base, None).unwrap(),
            Source::Dir(base.join("regs"))
        );
        let h = Source::parse(
            "https://deploy.internal/r/registry.toml",
            base,
            Some("ca.pem".into()),
        )
        .unwrap();
        assert_eq!(h.to_string(), "https://deploy.internal:443/r/registry.toml");
        assert!(Source::parse("https://deploy.internal/r", base, None).is_err());
        assert!(Source::parse("http://deploy.internal/r", base, Some("ca.pem".into())).is_err());
        assert!(Source::parse("r.toml", base, Some("ca.pem".into())).is_err());
        assert!(Source::parse("dir:", base, None).is_err());
    }
}
