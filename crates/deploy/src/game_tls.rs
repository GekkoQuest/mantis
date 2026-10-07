//! The game listener's certificate, from files the operator provides
//! (`[cell_host] game_cert`, `game_key`, optional `game_ca`).
//!
//! Two deployments, one mechanism:
//!
//! - **Public:** a chain from a public CA for the name players' clients
//!   dial; clients trust their usual roots.
//! - **Private:** a chain from the cluster CA (`mantisd certs --server`);
//!   clients are given that CA as their trust bundle.
//!
//! Each chain is checked before it is handed to the listener: the PEM
//! parses, the key matches the leaf, and, when `game_ca` is set, the chain
//! verifies against it for the advertised host now (a rotation to an
//! expired or wrong certificate is refused and the running chain stays).
//!
//! **A rotation never drops a game session.** Unlike the internal RPC,
//! where every connection is closed on a certificate swap and every peer
//! reconnects by design, the game listener keeps open sessions on their
//! current connection and gives only new handshakes the new chain; this
//! module only produces the new chain, and the package hands it to the
//! listener's reloader (`CellNode::game_tls_changed`).

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::ServerCertVerifier as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};

use crate::config::GameTlsFiles;
use crate::pki;
use crate::target::Target;

/// A checked game certificate chain and key, DER.
#[derive(Clone, PartialEq, Eq)]
pub struct GameTls {
    /// The chain, leaf first.
    pub chain: Vec<Vec<u8>>,
    /// The PKCS#8 private key.
    pub key: Vec<u8>,
}

impl std::fmt::Debug for GameTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameTls")
            .field("chain", &self.chain.len())
            .field("key", &"(secret)")
            .finish()
    }
}

/// Reads and checks the game certificate files, for clients dialling
/// `advertise`.
///
/// # Errors
/// A file is missing or malformed, the key does not match the leaf, or
/// (with a CA bundle) the chain does not verify for the advertised host
/// now.
pub fn load(files: &GameTlsFiles, advertise: &Target) -> Result<GameTls, String> {
    let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let chain = pki::pem_certs(&read(&files.cert)?).map_err(|e| format!("{}: {e}", files.cert.display()))?;
    if chain.is_empty() {
        return Err(format!("{}: holds no certificate", files.cert.display()));
    }
    let key = pki::pem_key(&read(&files.key)?).map_err(|e| format!("{}: {e}", files.key.display()))?;
    // rustls refuses a key that does not match the leaf.
    let certs: Vec<CertificateDer<'static>> = chain.iter().cloned().map(CertificateDer::from).collect();
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(
            certs.clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.clone())),
        )
        .map_err(|e| format!("{} and {}: {e}", files.cert.display(), files.key.display()))?;
    if let Some(ca) = &files.ca {
        let mut roots = rustls::RootCertStore::empty();
        for der in pki::pem_certs(&read(ca)?)? {
            roots
                .add(CertificateDer::from(der))
                .map_err(|e| format!("{}: {e}", ca.display()))?;
        }
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .map_err(|e| e.to_string())?;
        let name = match advertise.ip() {
            Some(ip) => ServerName::IpAddress(ip.into()),
            None => ServerName::try_from(advertise.host().to_owned()).map_err(|e| e.to_string())?,
        };
        let (leaf, rest) = certs.split_first().ok_or("no certificate")?;
        verifier
            .verify_server_cert(leaf, rest, &name, &[], UnixTime::now())
            .map_err(|e| {
                format!(
                    "{}: does not verify against {} for {}: {e}",
                    files.cert.display(),
                    ca.display(),
                    advertise.host()
                )
            })?;
    }
    Ok(GameTls { chain, key })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::{Validity, issue_server, new_ca};

    #[test]
    fn a_chain_must_match_its_key_and_verify_against_the_bundle() {
        let d = std::env::temp_dir().join(format!("mantis-game-tls-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let now = std::time::SystemTime::now();
        let ok = Validity::starting_now(now, 7);
        let ca = new_ca("dev", ok).unwrap();
        let leaf = issue_server(&ca, "game", &["127.0.0.1".to_owned()], ok).unwrap();
        let other = issue_server(&ca, "game", &["127.0.0.1".to_owned()], ok).unwrap();
        std::fs::write(d.join("ca.pem"), &ca.cert_pem).unwrap();
        std::fs::write(d.join("game.crt"), &leaf.cert_pem).unwrap();
        std::fs::write(d.join("game.key"), &leaf.key_pem).unwrap();
        std::fs::write(d.join("other.key"), &other.key_pem).unwrap();
        let files = |key: &str, ca: bool| GameTlsFiles {
            cert: d.join("game.crt"),
            key: d.join(key),
            ca: ca.then(|| d.join("ca.pem")),
        };
        let here = Target::parse("127.0.0.1:7400").unwrap();
        assert_eq!(load(&files("game.key", true), &here).unwrap().chain.len(), 1);
        assert!(
            load(&files("other.key", false), &here).is_err(),
            "a key of another leaf"
        );
        let elsewhere = Target::parse("10.9.9.9:7400").unwrap();
        let e = load(&files("game.key", true), &elsewhere).unwrap_err();
        assert!(e.contains("does not verify"), "{e}");
        let today = pki::day_of(now);
        let old = issue_server(
            &ca,
            "game",
            &["127.0.0.1".to_owned()],
            Validity {
                from_day: today - 9,
                until_day: today - 1,
            },
        )
        .unwrap();
        std::fs::write(d.join("old.crt"), &old.cert_pem).unwrap();
        std::fs::write(d.join("old.key"), &old.key_pem).unwrap();
        let expired = GameTlsFiles {
            cert: d.join("old.crt"),
            key: d.join("old.key"),
            ca: Some(d.join("ca.pem")),
        };
        assert!(load(&expired, &here).is_err(), "expired");
        let _ = std::fs::remove_dir_all(&d);
    }
}
