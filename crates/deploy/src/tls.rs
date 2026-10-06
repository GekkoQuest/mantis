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

use std::path::Path;
use std::sync::Arc;

use mantis_services::host::Role;
use mantis_services::tls::{TlsIdentity, identity_of};
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
