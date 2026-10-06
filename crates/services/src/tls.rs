//! Mutual TLS for internal RPC.
//!
//! Every node holds a [`TlsIdentity`]: the cluster CAs it trusts, its
//! certificate chain, and its key. A leaf names exactly one identity, a URI
//! alternative name `mantis://<cluster>/<role>/<instance>`, and an IP
//! alternative name for each address its RPC listens on. Both ends of a
//! connection present a certificate chained to a trusted CA:
//!
//! - the client verifies the server's chain, its IP, and that its identity
//!   names the client's own cluster and the role it meant to call;
//! - the server verifies the client's chain and that its identity names the
//!   server's cluster; the hello must then name the certificate's role, so
//!   a certificate issued to one role cannot call as another, and the
//!   caller matrix applies to the certificate's role.
//!
//! The cluster-key HMAC in the hello stays as a second factor. Expired,
//! not-yet-valid and foreign certificates fail the handshake. Issuance,
//! distribution and renewal are the deployment's (`mantisd certs`); [`dev`]
//! issues in memory for tests and local development.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::host::Role;

/// The URI scheme of an identity.
pub const SCHEME: &str = "mantis://";

/// A node's TLS material, all DER.
#[derive(Clone)]
pub struct TlsIdentity {
    /// The CAs it trusts: a peer chains to any of them (two during a CA
    /// rotation).
    pub cas: Vec<Vec<u8>>,
    /// Its certificate chain, leaf first.
    pub chain: Vec<Vec<u8>>,
    /// Its private key, PKCS#8.
    pub key: Vec<u8>,
}

impl std::fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("cas", &self.cas.len())
            .field("chain", &self.chain.len())
            .field("key", &"(secret)")
            .finish()
    }
}

/// Who a certificate says its holder is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PeerIdentity {
    /// The cluster.
    pub cluster: String,
    /// The role.
    pub role: Role,
    /// The instance.
    pub instance: String,
}

impl PeerIdentity {
    /// The identity URI.
    #[must_use]
    pub fn uri(&self) -> String {
        format!("{SCHEME}{}/{}/{}", self.cluster, self.role.name(), self.instance)
    }
}

/// Why TLS material or an identity was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IdentityError {
    /// A PEM file did not parse.
    Pem(String),
    /// The certificate did not parse.
    Certificate(String),
    /// No `mantis://` identity.
    NoIdentity,
    /// More than one `mantis://` identity.
    SeveralIdentities,
    /// A `mantis://` identity that is not `mantis://<cluster>/<role>/<instance>`.
    Malformed(String),
    /// The material does not make a TLS configuration (no chain, a bad key).
    Config(String),
    /// The identity names another role than the one it is used for.
    WrongRole {
        /// The role required.
        expected: Role,
        /// The role the certificate names.
        found: Role,
    },
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pem(e) => write!(f, "PEM: {e}"),
            Self::Certificate(e) => write!(f, "certificate: {e}"),
            Self::NoIdentity => write!(f, "no {SCHEME} identity"),
            Self::SeveralIdentities => write!(f, "more than one {SCHEME} identity"),
            Self::Malformed(uri) => write!(f, "{uri:?} is not {SCHEME}<cluster>/<role>/<instance>"),
            Self::Config(e) => write!(f, "TLS configuration: {e}"),
            Self::WrongRole { expected, found } => {
                write!(
                    f,
                    "the certificate is for {}, not {}",
                    found.name(),
                    expected.name()
                )
            }
        }
    }
}

impl std::error::Error for IdentityError {}

/// A cluster or instance name: 1 to 32 of `a-z`, `0-9` and `-`.
fn valid_part(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses `mantis://<cluster>/<role>/<instance>`.
///
/// # Errors
/// [`IdentityError::Malformed`].
pub fn parse_uri(uri: &str) -> Result<PeerIdentity, IdentityError> {
    let bad = || IdentityError::Malformed(uri.to_owned());
    let rest = uri.strip_prefix(SCHEME).ok_or_else(bad)?;
    let mut parts = rest.split('/');
    let (Some(cluster), Some(role), Some(instance), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let role = Role::ALL.into_iter().find(|r| r.name() == role).ok_or_else(bad)?;
    if !valid_part(cluster) || !valid_part(instance) {
        return Err(bad());
    }
    Ok(PeerIdentity {
        cluster: cluster.to_owned(),
        role,
        instance: instance.to_owned(),
    })
}

/// The identity a certificate names: its one `mantis://` URI alternative
/// name. URIs of other schemes are ignored; the scheme is matched without
/// regard to case, so `MANTIS://...` counts (and is then refused as
/// malformed) rather than slipping past as another scheme.
///
/// # Errors
/// The certificate does not parse, names no identity or several, or a
/// malformed one.
pub fn identity_of(cert_der: &[u8]) -> Result<PeerIdentity, IdentityError> {
    let (rest, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| IdentityError::Certificate(e.to_string()))?;
    if !rest.is_empty() {
        return Err(IdentityError::Certificate("trailing bytes".to_owned()));
    }
    let san = cert
        .subject_alternative_name()
        .map_err(|e| IdentityError::Certificate(e.to_string()))?;
    let mut found = None;
    for name in san.iter().flat_map(|s| s.value.general_names.iter()) {
        if let x509_parser::extensions::GeneralName::URI(uri) = name
            && uri
                .get(..SCHEME.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(SCHEME))
        {
            if found.is_some() {
                return Err(IdentityError::SeveralIdentities);
            }
            found = Some(parse_uri(uri)?);
        }
    }
    found.ok_or(IdentityError::NoIdentity)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl TlsIdentity {
    /// From PEM files: the trusted CAs (one or more certificates), the
    /// chain (leaf first), and the PKCS#8 key.
    ///
    /// # Errors
    /// [`IdentityError::Pem`] for a file that does not parse or holds
    /// nothing.
    pub fn from_pem(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, IdentityError> {
        let certs = |pem: &[u8], what: &str| -> Result<Vec<Vec<u8>>, IdentityError> {
            let all = CertificateDer::pem_slice_iter(pem)
                .map(|c| c.map(|c| c.to_vec()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| IdentityError::Pem(format!("{what}: {e}")))?;
            if all.is_empty() {
                return Err(IdentityError::Pem(format!("{what}: no certificate")));
            }
            Ok(all)
        };
        let key = PrivatePkcs8KeyDer::from_pem_slice(key_pem)
            .map_err(|e| IdentityError::Pem(format!("the key: {e}")))?;
        Ok(Self {
            cas: certs(ca_pem, "the CA file")?,
            chain: certs(cert_pem, "the certificate file")?,
            key: key.secret_pkcs8_der().to_vec(),
        })
    }

    /// The identity its own leaf names.
    ///
    /// # Errors
    /// No leaf, or [`identity_of`]'s errors.
    pub fn identity(&self) -> Result<PeerIdentity, IdentityError> {
        identity_of(
            self.chain
                .first()
                .ok_or_else(|| IdentityError::Config("no certificate".to_owned()))?,
        )
    }

    fn roots(&self) -> Result<rustls::RootCertStore, IdentityError> {
        let mut roots = rustls::RootCertStore::empty();
        for ca in &self.cas {
            roots
                .add(CertificateDer::from(ca.clone()))
                .map_err(|e| IdentityError::Config(format!("a CA: {e}")))?;
        }
        if roots.is_empty() {
            return Err(IdentityError::Config("no CA".to_owned()));
        }
        Ok(roots)
    }

    fn chain_der(&self) -> Vec<CertificateDer<'static>> {
        self.chain
            .iter()
            .map(|c| CertificateDer::from(c.clone()))
            .collect()
    }

    fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    /// A server configuration requiring a client certificate chained to one
    /// of the CAs. TLS 1.3 only.
    ///
    /// # Errors
    /// [`IdentityError::Config`].
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, IdentityError> {
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(self.roots()?), provider())
                .build()
                .map_err(|e| IdentityError::Config(e.to_string()))?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| IdentityError::Config(e.to_string()))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.chain_der(), self.key_der())
            .map_err(|e| IdentityError::Config(e.to_string()))?;
        Ok(Arc::new(config))
    }

    /// A client configuration presenting this identity and trusting the
    /// CAs. TLS 1.3 only.
    ///
    /// # Errors
    /// [`IdentityError::Config`].
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, IdentityError> {
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| IdentityError::Config(e.to_string()))?
            .with_root_certificates(self.roots()?)
            .with_client_auth_cert(self.chain_der(), self.key_der())
            .map_err(|e| IdentityError::Config(e.to_string()))?;
        Ok(Arc::new(config))
    }
}

/// The identity of a connection's peer: its leaf, already verified by the
/// handshake.
pub(crate) fn peer_identity(certs: Option<&[CertificateDer<'_>]>) -> Result<PeerIdentity, IdentityError> {
    identity_of(certs.and_then(<[_]>::first).ok_or(IdentityError::NoIdentity)?)
}

pub mod dev {
    //! An in-memory CA issuing node identities, for tests and local
    //! development only. Production certificates come from the
    //! deployment's issuer.

    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::sync::Arc;

    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
        Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
    };

    use super::{SCHEME, TlsIdentity};
    use crate::host::Role;

    /// A certificate's validity, as `(year, month, day)` bounds.
    pub type Validity = ((i32, u8, u8), (i32, u8, u8));

    /// Valid for centuries either side of now.
    pub const ALWAYS: Validity = ((1975, 1, 1), (4096, 1, 1));

    /// A CA held in memory.
    pub struct DevCa {
        cluster: String,
        issuer: Issuer<'static, KeyPair>,
        der: Vec<u8>,
    }

    impl std::fmt::Debug for DevCa {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DevCa")
                .field("cluster", &self.cluster)
                .finish_non_exhaustive()
        }
    }

    impl DevCa {
        /// A new CA for `cluster`.
        ///
        /// # Errors
        /// Generation failed.
        pub fn new(cluster: &str) -> Result<Self, String> {
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
            let mut p = CertificateParams::default();
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, format!("mantis dev CA {cluster}"));
            p.distinguished_name = dn;
            p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let cert = p.self_signed(&key).map_err(|e| e.to_string())?;
            Ok(Self {
                cluster: cluster.to_owned(),
                der: cert.der().to_vec(),
                issuer: Issuer::new(p, key),
            })
        }

        /// The CA certificate (DER).
        #[must_use]
        pub fn der(&self) -> &[u8] {
            &self.der
        }

        /// The identity of `instance` of `role`, valid for `ips`.
        ///
        /// # Errors
        /// Issuance failed.
        pub fn identity(
            &self,
            role: Role,
            instance: &str,
            ips: &[IpAddr],
        ) -> Result<Arc<TlsIdentity>, String> {
            let uri = format!("{SCHEME}{}/{}/{instance}", self.cluster, role.name());
            self.leaf(&[uri.as_str()], ips, ALWAYS)
        }

        /// A leaf with exactly the URI alternative names `uris` (any, even
        /// malformed), `ips`, and `validity`, trusting this CA.
        ///
        /// # Errors
        /// Issuance failed.
        pub fn leaf(
            &self,
            uris: &[&str],
            ips: &[IpAddr],
            validity: Validity,
        ) -> Result<Arc<TlsIdentity>, String> {
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
            let mut p = CertificateParams::default();
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, "mantis dev node");
            p.distinguished_name = dn;
            p.is_ca = IsCa::ExplicitNoCa;
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            let mut names = Vec::new();
            for uri in uris {
                names.push(SanType::URI(
                    (*uri).try_into().map_err(|_| format!("{uri:?} is not ASCII"))?,
                ));
            }
            names.extend(ips.iter().map(|ip| SanType::IpAddress(*ip)));
            p.subject_alt_names = names;
            let ((y0, m0, d0), (y1, m1, d1)) = validity;
            p.not_before = rcgen::date_time_ymd(y0, m0, d0);
            p.not_after = rcgen::date_time_ymd(y1, m1, d1);
            p.use_authority_key_identifier_extension = true;
            let cert = p.signed_by(&key, &self.issuer).map_err(|e| e.to_string())?;
            Ok(Arc::new(TlsIdentity {
                cas: vec![self.der.clone()],
                chain: vec![cert.der().to_vec()],
                key: key.serialize_der(),
            }))
        }

        /// One identity per role (`<role>-dev`), all valid for `ips`.
        ///
        /// # Errors
        /// Issuance failed.
        pub fn every_role(&self, ips: &[IpAddr]) -> Result<BTreeMap<Role, Arc<TlsIdentity>>, String> {
            Role::ALL
                .into_iter()
                .map(|r| Ok((r, self.identity(r, &format!("{}-dev", r.name()), ips)?)))
                .collect()
        }
    }
}
