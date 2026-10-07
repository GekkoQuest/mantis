//! A node's own mutual-TLS material, checked before it serves anything.
//!
//! The CAs come from the signed registry only, so a node trusts exactly
//! the CAs its registry names. Its certificate and key come from files
//! (`[node] tls_cert`, `tls_key`). At start the node checks its own
//! certificate the way every peer will: it chains to a registry CA, it is
//! valid now (not expired, not yet valid), it allows client and server
//! use, and its identity is exactly `mantis://<cluster>/<role>/<instance>`
//! for this node. A node whose certificate fails any of these exits with
//! the reason instead of failing every handshake later.
//!
//! **Live, without a restart** ([`maintain`]):
//!
//! - **Renewal.** When the certificate and key files change, the new pair
//!   is checked as at start and swapped into the node's [`TlsHandle`]. A
//!   pair that fails is refused once (logged, `tls_refused`), and the
//!   running identity stays.
//! - **Revocation.** When a newer registry changes the CA list, the node's
//!   trust becomes exactly the new list. Peers from a removed CA are then
//!   refused at the handshake, within one registry refresh. If this node's
//!   own certificate is from a removed CA, the new list is applied anyway
//!   (the node is revoked; its peers refuse it) and the node reports
//!   itself not ready until its certificate is renewed.
//! - **What a swap does to connections.** The internal RPC closes every
//!   connection accepted under the old identity, and clients drop theirs
//!   before their next call, so every peer handshakes again against the new
//!   material. That is right for RPC, where every caller reconnects by
//!   design. It is never done to game sessions: the game listener's own
//!   rotation ([`crate::cell::CellNode::game_tls_changed`]) keeps open
//!   sessions and gives only new handshakes the new chain.

use std::path::Path;
use std::sync::Arc;

use mantis_services::host::Role;
use mantis_services::tls::{TlsHandle, TlsIdentity, identity_of};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::WebPkiClientVerifier;

use crate::pki;
use crate::registry::Registry;

/// Reads and checks the node's certificate and key against `registry`, as
/// role `role` and instance `instance`.
///
/// # Errors
/// A file is missing or malformed, the certificate does not chain to a
/// registry CA, is not valid now, or names another identity.
pub fn load(
    registry: &Registry,
    role: Role,
    instance: &str,
    cert: &Path,
    key: &Path,
) -> Result<Arc<TlsIdentity>, String> {
    let read = |p: &Path| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let chain = pki::pem_certs(&read(cert)?).map_err(|e| format!("{}: {e}", cert.display()))?;
    let key_der = pki::pem_key(&read(key)?).map_err(|e| format!("{}: {e}", key.display()))?;
    let leaf = chain
        .first()
        .ok_or_else(|| format!("{}: holds no certificate", cert.display()))?;
    verify(&registry.cas, &chain).map_err(|e| format!("{}: {e}", cert.display()))?;
    let want = pki::identity(&registry.cluster, role, instance);
    let found = identity_of(leaf).map_err(|e| format!("{}: {e}", cert.display()))?;
    if found.uri() != want {
        return Err(format!(
            "{}: names {:?}; this node is {want:?}",
            cert.display(),
            found.uri()
        ));
    }
    let identity = TlsIdentity {
        cas: registry.cas.clone(),
        chain,
        key: key_der,
    };
    // The configurations the RPC layer will build must build.
    identity
        .server_config()
        .and_then(|_| identity.client_config())
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    Ok(Arc::new(identity))
}

/// Checks `chain` (leaf first) against `cas` now, as a peer will: chained
/// to one of them, valid now, usable by a client.
///
/// # Errors
/// Why a peer would refuse it.
pub fn verify(cas: &[Vec<u8>], chain: &[Vec<u8>]) -> Result<(), String> {
    let mut roots = rustls::RootCertStore::empty();
    for ca in cas {
        roots
            .add(CertificateDer::from(ca.clone()))
            .map_err(|e| format!("a registry CA: {e}"))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|e| e.to_string())?;
    let (leaf, rest) = chain.split_first().ok_or("no certificate")?;
    let intermediates: Vec<CertificateDer<'_>> =
        rest.iter().map(|c| CertificateDer::from(c.as_slice())).collect();
    verifier
        .verify_client_cert(
            &CertificateDer::from(leaf.as_slice()),
            &intermediates,
            UnixTime::now(),
        )
        .map(|_| ())
        .map_err(|e| format!("refused as a peer would refuse it: {e}"))
}

/// How often [`maintain`] looks at the certificate files.
pub const LOOK: std::time::Duration = std::time::Duration::from_secs(1);

/// What [`maintain`] keeps current.
pub struct Maintain {
    /// The node's live identity.
    pub tls: TlsHandle,
    /// The node's live registry (its CA list).
    pub live: crate::source::Live,
    /// The node's role.
    pub role: Role,
    /// The node's instance name.
    pub instance: String,
    /// The certificate file.
    pub cert: std::path::PathBuf,
    /// The key file.
    pub key: std::path::PathBuf,
    /// Where it reports (log, `/metrics`, `/ready`).
    pub status: crate::health::Status,
}

/// Keeps the node's TLS identity current: renewed certificate files, and
/// the registry's CA list (revocation). Runs for the life of the node.
pub async fn maintain(m: Maintain) {
    let mut files = crate::watch::FileWatch::new(vec![m.cert.clone(), m.key.clone()], LOOK);
    let mut registries = m.live.subscribe();
    let mut trusted: Vec<Vec<u8>> = m.live.current().cas.clone();
    // Set when this node marked itself not ready for an untrusted own
    // certificate; cleared (ready again) by a renewal.
    let mut untrusted = false;
    let say = |line: &str| crate::node::say(m.role, &m.instance, line);
    let mut tick = tokio::time::interval(LOOK);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            changed = registries.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        let registry = m.live.current();
        if files.changed() {
            match load(&registry, m.role, &m.instance, &m.cert, &m.key) {
                Ok(id) => match m.tls.set(id) {
                    Ok(()) => {
                        files.accept();
                        trusted.clone_from(&registry.cas);
                        m.status.metrics.add("tls_renewals", 1);
                        if std::mem::take(&mut untrusted) {
                            m.status.set(crate::health::Phase::Ready, "");
                        }
                        say("certificate renewed: every RPC connection handshakes again with it");
                        continue;
                    }
                    Err(e) => {
                        files.refuse();
                        m.status.metrics.add("tls_refused", 1);
                        say(&format!(
                            "certificate renewal refused, the running one stays: {e}"
                        ));
                    }
                },
                Err(e) => {
                    files.refuse();
                    m.status.metrics.add("tls_refused", 1);
                    say(&format!(
                        "certificate renewal refused, the running one stays: {e}"
                    ));
                }
            }
        }
        if registry.cas != trusted {
            // Revocation (or a rotation's overlap): trust exactly the new list.
            let current = m.tls.current();
            let next = TlsIdentity {
                cas: registry.cas.clone(),
                chain: current.chain.clone(),
                key: current.key.clone(),
            };
            match m.tls.set(Arc::new(next)) {
                Ok(()) => {
                    trusted.clone_from(&registry.cas);
                    m.status.metrics.add("tls_ca_changes", 1);
                    say(&format!(
                        "registry serial {}: trusting {} CA(s); every RPC connection handshakes again",
                        registry.serial,
                        registry.cas.len()
                    ));
                    if let Err(e) = verify(&registry.cas, &current.chain) {
                        m.status.metrics.add("tls_own_certificate_untrusted", 1);
                        untrusted = true;
                        m.status.set(
                            crate::health::Phase::Starting,
                            format!(
                                "this node's certificate is not from a CA the registry lists: renew it ({e})"
                            ),
                        );
                        say(
                            "this node's own certificate is from a CA the registry no longer lists: peers refuse it until it is renewed",
                        );
                    }
                }
                Err(e) => say(&format!(
                    "registry serial {}: the new CA list does not apply: {e}",
                    registry.serial
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::{Validity, issue, new_ca};
    use std::time::SystemTime;

    #[test]
    fn a_node_refuses_an_expired_foreign_or_misnamed_certificate() {
        let now = SystemTime::now();
        let ok = Validity::starting_now(now, 7);
        let ca = new_ca("dev", ok).unwrap();
        let cas = pki::pem_certs(ca.cert_pem.as_bytes()).unwrap();
        let leaf = issue(&ca, "dev", Role::Social, "social-1", &[], ok).unwrap();
        let chain = pki::pem_certs(leaf.cert_pem.as_bytes()).unwrap();
        verify(&cas, &chain).unwrap();
        assert_eq!(
            identity_of(&chain[0]).unwrap().uri(),
            "mantis://dev/social/social-1"
        );

        let today = pki::day_of(now);
        let expired = Validity {
            from_day: today - 10,
            until_day: today - 1,
        };
        let old = issue(&ca, "dev", Role::Social, "social-1", &[], expired).unwrap();
        let e = verify(&cas, &pki::pem_certs(old.cert_pem.as_bytes()).unwrap()).unwrap_err();
        assert!(e.contains("xpired"), "{e}");

        let other = new_ca("dev", ok).unwrap();
        let foreign = issue(&other, "dev", Role::Social, "social-1", &[], ok).unwrap();
        let e = verify(&cas, &pki::pem_certs(foreign.cert_pem.as_bytes()).unwrap()).unwrap_err();
        assert!(e.contains("refused"), "{e}");
    }
}
