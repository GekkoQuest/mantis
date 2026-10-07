//! The cluster's certificates: one CA per cluster, one short-lived leaf
//! per node instance, for mutual TLS between every role.
//!
//! - **The CA**: ECDSA P-256 with SHA-256, self-signed, `CA:true` with
//!   path length 0, key usage keyCertSign and cRLSign; 90 days by default.
//!   Its private key stays with the operator: no node ever reads it. The
//!   signed registry carries the CA certificate, so a node trusts exactly
//!   the CA its signed registry names.
//! - **A leaf**: ECDSA P-256, key usage digitalSignature, extended key
//!   usage serverAuth and clientAuth (one certificate serves a node's RPC
//!   listener and makes its outgoing calls); 7 days by default.
//! - **The identity** is exactly one URI subject alternative name,
//!   `mantis://<cluster>/<role>/<instance>`, where `role` is the services'
//!   `Role::name()` (`cell` for a cell host). The RPC layer takes a
//!   caller's role from it, so a certificate for one role cannot call as
//!   another. The leaf also carries an IP subject alternative name for the
//!   address the registry lists for the instance's RPC, which callers dial.
//!   The subject common name is for humans only.
//!
//! Renewal reissues the leaves from the CA ([`issue`]) and restarts the
//! nodes one at a time; see `docs/DEPLOY.md`.

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mantis_services::host::Role;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use crate::keys;

/// Default CA lifetime.
pub const CA_DAYS: u32 = 90;

/// Default leaf lifetime: short, so a leaked leaf is useful briefly.
pub const LEAF_DAYS: u32 = 7;

/// The URI scheme of identities.
pub const SCHEME: &str = "mantis://";

/// A node's identity URI.
#[must_use]
pub fn identity(cluster: &str, role: Role, instance: &str) -> String {
    format!("{SCHEME}{cluster}/{}/{instance}", role.name())
}

/// A validity window, whole days in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    /// The first day it is valid (from midnight UTC).
    pub from_day: i64,
    /// The day it stops being valid (at midnight UTC).
    pub until_day: i64,
}

impl Validity {
    /// From yesterday (clock skew between machines) for `days` days from
    /// today.
    #[must_use]
    pub fn starting_now(now: SystemTime, days: u32) -> Self {
        let today = day_of(now);
        Self {
            from_day: today - 1,
            until_day: today + i64::from(days),
        }
    }
}

/// Days since 1970-01-01 (UTC) of `t`.
#[must_use]
pub fn day_of(t: SystemTime) -> i64 {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    secs.div_euclid(86_400)
}

/// The civil date (year, month, day) of a day number since 1970-01-01
/// (proleptic Gregorian; Howard Hinnant's algorithm).
#[must_use]
pub fn civil(day: i64) -> (i32, u8, u8) {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        i32::try_from(y).unwrap_or(i32::MAX),
        u8::try_from(m).unwrap_or(1),
        u8::try_from(d).unwrap_or(1),
    )
}

fn set_validity(p: &mut CertificateParams, validity: Validity) {
    let (y, m, d) = civil(validity.from_day);
    p.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = civil(validity.until_day);
    p.not_after = rcgen::date_time_ymd(y, m, d);
}

fn serial() -> Result<SerialNumber, String> {
    let mut b = keys::random(16)?;
    // A positive integer.
    if let Some(first) = b.first_mut() {
        *first &= 0x7f;
    }
    Ok(SerialNumber::from_slice(&b))
}

fn ca_params(cluster: &str, validity: Validity) -> Result<CertificateParams, String> {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("mantis cluster CA {cluster}"));
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity(&mut p, validity);
    p.serial_number = Some(serial()?);
    Ok(p)
}

/// A CA: its certificate and private key, both PEM.
#[derive(Clone, Debug)]
pub struct CaFiles {
    /// The certificate (PEM).
    pub cert_pem: String,
    /// The private key (PKCS#8 PEM). The operator's: never mounted into a
    /// node.
    pub key_pem: String,
}

/// A new CA for `cluster`.
///
/// # Errors
/// Generation failed.
pub fn new_ca(cluster: &str, validity: Validity) -> Result<CaFiles, String> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let cert = ca_params(cluster, validity)?
        .self_signed(&key)
        .map_err(|e| e.to_string())?;
    Ok(CaFiles {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// A leaf: its certificate and private key, both PEM.
#[derive(Clone, Debug)]
pub struct LeafFiles {
    /// The certificate (PEM).
    pub cert_pem: String,
    /// The private key (PKCS#8 PEM).
    pub key_pem: String,
}

/// Issues the leaf of `instance` (role `role`, cluster `cluster`) with an
/// IP alternative name for each of `ips`, signed by `ca`.
///
/// # Errors
/// [`issue_for`].
pub fn issue(
    ca: &CaFiles,
    cluster: &str,
    role: Role,
    instance: &str,
    ips: &[IpAddr],
    validity: Validity,
) -> Result<LeafFiles, String> {
    let hosts: Vec<String> = ips.iter().map(IpAddr::to_string).collect();
    issue_for(ca, cluster, role, instance, &hosts, validity)
}

/// Issues the leaf of `instance` with an alternative name for each of
/// `hosts`: an IP literal becomes an IP name, anything else a DNS name
/// (what callers dial, so what they verify).
///
/// # Errors
/// The CA files do not parse, a name is not a valid identity part, or a
/// host is neither an IP literal nor a DNS name.
pub fn issue_for(
    ca: &CaFiles,
    cluster: &str,
    role: Role,
    instance: &str,
    hosts: &[String],
    validity: Validity,
) -> Result<LeafFiles, String> {
    for (what, part) in [("cluster", cluster), ("instance", instance)] {
        let ok = (1..=32).contains(&part.len())
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(format!("{what} {part:?}: 1 to 32 of a-z, 0-9 and -"));
        }
    }
    let uri = identity(cluster, role, instance)
        .try_into()
        .map_err(|_| "the identity is not ASCII".to_owned())?;
    leaf(
        ca,
        &format!("{}.{instance}", role.name()),
        Some(SanType::URI(uri)),
        hosts,
        &[
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ],
        validity,
    )
}

/// Issues a plain TLS server certificate for `hosts` (no node identity):
/// for the registry server (`mantisd registry serve`) and for a game
/// listener signed by the cluster CA.
///
/// # Errors
/// The CA files do not parse, there is no host, or a host is neither an IP
/// literal nor a DNS name.
pub fn issue_server(
    ca: &CaFiles,
    name: &str,
    hosts: &[String],
    validity: Validity,
) -> Result<LeafFiles, String> {
    if hosts.is_empty() {
        return Err("a server certificate names at least one host".to_owned());
    }
    leaf(
        ca,
        name,
        None,
        hosts,
        &[ExtendedKeyUsagePurpose::ServerAuth],
        validity,
    )
}

fn leaf(
    ca: &CaFiles,
    common_name: &str,
    identity: Option<SanType>,
    hosts: &[String],
    usages: &[ExtendedKeyUsagePurpose],
    validity: Validity,
) -> Result<LeafFiles, String> {
    let ca_key = KeyPair::from_pem(&ca.key_pem).map_err(|e| format!("the CA key: {e}"))?;
    let ca_der = pem_certs(ca.cert_pem.as_bytes())?
        .into_iter()
        .next()
        .ok_or("the CA certificate file holds no certificate")?;
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(ca_der), ca_key)
        .map_err(|e| format!("the CA certificate: {e}"))?;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    p.distinguished_name = dn;
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = usages.to_vec();
    p.subject_alt_names = identity
        .into_iter()
        .chain(
            hosts
                .iter()
                .map(|h| match h.parse::<IpAddr>() {
                    Ok(ip) => Ok(SanType::IpAddress(ip)),
                    Err(_) => h
                        .clone()
                        .try_into()
                        .map(SanType::DnsName)
                        .map_err(|_| format!("{h:?} is not a host name")),
                })
                .collect::<Result<Vec<_>, String>>()?,
        )
        .collect();
    set_validity(&mut p, validity);
    p.serial_number = Some(serial()?);
    p.use_authority_key_identifier_extension = true;
    let cert = p.signed_by(&key, &issuer).map_err(|e| e.to_string())?;
    Ok(LeafFiles {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Issues the leaf of every instance in `registry` (or only `only`) into
/// `out` as `<instance>.crt` and `<instance>.key`, each with an IP
/// alternative name for the RPC address the registry lists. Returns the
/// instances issued.
///
/// # Errors
/// The CA is not one the registry carries (nodes would refuse every
/// certificate it signs), `only` names no instance, or a write failed.
pub fn issue_registry(
    ca: &CaFiles,
    registry: &crate::registry::Registry,
    out: &std::path::Path,
    validity: Validity,
    only: Option<&str>,
) -> Result<Vec<String>, String> {
    let ca_der = pem_certs(ca.cert_pem.as_bytes())?
        .into_iter()
        .next()
        .ok_or("the CA certificate file holds no certificate")?;
    if !registry.cas.contains(&ca_der) {
        return Err(
            "this CA is not in the registry's `ca` list: nodes would refuse what it signs".to_owned(),
        );
    }
    let mut issued = Vec::new();
    for i in &registry.instances {
        if only.is_some_and(|o| o != i.name) {
            continue;
        }
        let leaf = issue_for(
            ca,
            &registry.cluster,
            i.role,
            &i.name,
            &[i.rpc.host().to_owned()],
            validity,
        )?;
        keys::write_secret(&out.join(keys::files::cert(&i.name)), leaf.cert_pem.as_bytes())?;
        keys::write_secret(&out.join(keys::files::key(&i.name)), leaf.key_pem.as_bytes())?;
        issued.push(i.name.clone());
    }
    if let Some(o) = only
        && issued.is_empty()
    {
        return Err(format!("the registry has no instance {o:?}"));
    }
    Ok(issued)
}

/// Every certificate (DER) in PEM text.
///
/// # Errors
/// Malformed PEM.
pub fn pem_certs(pem: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    CertificateDer::pem_slice_iter(pem)
        .map(|c| c.map(|c| c.as_ref().to_vec()).map_err(|e| format!("PEM: {e:?}")))
        .collect()
}

/// The PKCS#8 private key (DER) in PEM text.
///
/// # Errors
/// Malformed PEM or no PKCS#8 key.
pub fn pem_key(pem: &[u8]) -> Result<Vec<u8>, String> {
    rustls::pki_types::PrivatePkcs8KeyDer::from_pem_slice(pem)
        .map(|k| k.secret_pkcs8_der().to_vec())
        .map_err(|e| format!("PEM private key: {e:?}"))
}

/// Seconds from `now` until `day` starts (negative when past).
#[must_use]
pub fn seconds_until(now: SystemTime, day: i64) -> i64 {
    let at = day.saturating_mul(86_400);
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    at - now
}

/// A day count as a duration.
#[must_use]
pub fn days(n: u32) -> Duration {
    Duration::from_secs(u64::from(n) * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_732), (2026, 10, 6));
        assert_eq!(day_of(UNIX_EPOCH + Duration::from_secs(86_399)), 0);
    }

    #[test]
    fn a_ca_issues_leaves_with_one_identity() {
        let v = Validity::starting_now(SystemTime::now(), 7);
        let ca = new_ca("dev", v).unwrap();
        let leaf = issue(
            &ca,
            "dev",
            Role::Social,
            "social-1",
            &["10.0.0.3".parse().unwrap()],
            v,
        )
        .unwrap();
        let der = pem_certs(leaf.cert_pem.as_bytes()).unwrap();
        assert_eq!(der.len(), 1);
        let uri = identity("dev", Role::Social, "social-1");
        assert_eq!(uri, "mantis://dev/social/social-1");
        assert!(der[0].windows(uri.len()).any(|w| w == uri.as_bytes()));
        assert!(!pem_key(leaf.key_pem.as_bytes()).unwrap().is_empty());
        assert!(issue(&ca, "Dev", Role::Social, "social-1", &[], v).is_err());
        assert!(issue(&ca, "dev", Role::Social, "social/1", &[], v).is_err());
    }
}
