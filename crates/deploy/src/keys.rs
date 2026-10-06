//! Key and secret files.
//!
//! Every secret a node needs is a file, never an environment variable or
//! a command-line argument (both leak into process listings and container
//! metadata):
//!
//! | File | Content | Who reads it |
//! |---|---|---|
//! | cluster key | 32 random bytes, hex | every node: the RPC hello proves it |
//! | deploy key (private) | Ed25519 PKCS#8, DER | the operator signing a registry |
//! | deploy key (public) | 32 bytes, hex | every node: it verifies the registry |
//! | live-data key | Ed25519 PKCS#8, DER | Ops: it signs live changes |
//! | operator token | 48 hex characters | Ops: the dashboard's bearer token |
//! | cluster CA (certificate) | PEM | the operator: copied into the registry, which every node trusts |
//! | cluster CA (private key) | PKCS#8 PEM | the operator only: it issues node certificates (`mantisd certs`); never mounted into a node |
//! | node certificate and key | PEM | that node: its mutual-TLS identity ([`crate::pki`]) |
//!
//! Cell hosts never read the live-data key's file: the signed registry
//! carries its public half.

use std::fmt::Write as _;
use std::path::Path;

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};

/// Lower-case hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The bytes of a hex string (either case, even length, nothing else).
///
/// # Errors
/// Not hex.
pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let digits = text.as_bytes();
    let (pairs, odd) = digits.as_chunks::<2>();
    if !odd.is_empty() {
        return Err("hex has an even number of digits".to_owned());
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("not a hex digit: {:?}", char::from(c))),
    };
    pairs
        .iter()
        .map(|[hi, lo]| Ok((nibble(*hi)? << 4) | nibble(*lo)?))
        .collect()
}

/// `n` bytes from the operating system's secure random source.
///
/// # Errors
/// No randomness.
pub fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "the operating system gave no randomness".to_owned())?;
    Ok(b)
}

fn read_text(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

/// Reads a hex file holding exactly `len` bytes (one line).
///
/// # Errors
/// The file is missing, not hex, or the wrong length.
pub fn read_hex(path: &Path, len: usize) -> Result<Vec<u8>, String> {
    let text = read_text(path)?;
    let bytes = unhex(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() == len {
        Ok(bytes)
    } else {
        Err(format!(
            "{}: expected {len} bytes of hex, found {}",
            path.display(),
            bytes.len()
        ))
    }
}

/// Bytes in a cluster key.
pub const CLUSTER_KEY_BYTES: usize = 32;

/// Bytes in an Ed25519 public key.
pub const PUBLIC_KEY_BYTES: usize = 32;

/// Reads the cluster key (32 bytes, hex).
///
/// # Errors
/// [`read_hex`].
pub fn read_cluster_key(path: &Path) -> Result<Vec<u8>, String> {
    read_hex(path, CLUSTER_KEY_BYTES)
}

/// Reads an Ed25519 public key (32 bytes, hex).
///
/// # Errors
/// [`read_hex`].
pub fn read_public_key(path: &Path) -> Result<[u8; PUBLIC_KEY_BYTES], String> {
    let bytes = read_hex(path, PUBLIC_KEY_BYTES)?;
    bytes
        .try_into()
        .map_err(|_| format!("{}: not a public key", path.display()))
}

/// Reads a PKCS#8 (DER) Ed25519 key document, checking it parses.
///
/// # Errors
/// The file is missing or not an Ed25519 PKCS#8 document.
pub fn read_pkcs8(path: &Path) -> Result<Vec<u8>, String> {
    let der = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ed25519KeyPair::from_pkcs8(&der).map_err(|_| format!("{}: not an Ed25519 PKCS#8 key", path.display()))?;
    Ok(der)
}

/// Reads an Ed25519 key pair from a PKCS#8 (DER) file.
///
/// # Errors
/// [`read_pkcs8`].
pub fn read_key_pair(path: &Path) -> Result<Ed25519KeyPair, String> {
    let der = read_pkcs8(path)?;
    Ed25519KeyPair::from_pkcs8(&der).map_err(|_| format!("{}: not an Ed25519 PKCS#8 key", path.display()))
}

/// Reads an operator token: at least 16 characters, letters and digits.
///
/// # Errors
/// The file is missing or the token is malformed.
pub fn read_operator_token(path: &Path) -> Result<String, String> {
    let token = read_text(path)?;
    if token.len() >= 16 && token.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(token)
    } else {
        Err(format!(
            "{}: an operator token is at least 16 letters and digits",
            path.display()
        ))
    }
}

/// Writes a secret file, readable by its owner only where the platform
/// lets a program say so.
///
/// # Errors
/// The write failed.
pub fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// A new Ed25519 key: its PKCS#8 document and its public key.
///
/// # Errors
/// No randomness.
pub fn new_key_pair() -> Result<(Vec<u8>, [u8; PUBLIC_KEY_BYTES]), String> {
    let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "cannot generate an Ed25519 key".to_owned())?;
    let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| "ring refused its own key".to_owned())?;
    let public: [u8; PUBLIC_KEY_BYTES] = pair
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| "an Ed25519 public key is 32 bytes".to_owned())?;
    Ok((doc.as_ref().to_vec(), public))
}

/// The public key of a PKCS#8 Ed25519 document.
///
/// # Errors
/// Not an Ed25519 PKCS#8 document.
pub fn public_of(pkcs8: &[u8]) -> Result<[u8; PUBLIC_KEY_BYTES], String> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| "not an Ed25519 PKCS#8 key".to_owned())?;
    pair.public_key()
        .as_ref()
        .try_into()
        .map_err(|_| "an Ed25519 public key is 32 bytes".to_owned())
}

/// The files [`new_keys`] writes, relative to its directory.
pub mod files {
    /// The cluster key.
    pub const CLUSTER: &str = "cluster.key";
    /// The deploy key (private), which signs registries.
    pub const DEPLOY: &str = "deploy.pk8";
    /// The deploy key (public), which every node trusts.
    pub const DEPLOY_PUBLIC: &str = "deploy.pub";
    /// The live-data key Ops signs live changes with.
    pub const LIVE: &str = "live.pk8";
    /// The live-data public key (copied into the registry).
    pub const LIVE_PUBLIC: &str = "live.pub";
    /// The Ops dashboard's operator token.
    pub const OPERATOR_TOKEN: &str = "operator.token";
    /// The cluster CA certificate.
    pub const CA: &str = "ca.crt";
    /// The cluster CA private key (the operator's only).
    pub const CA_KEY: &str = "ca.key";
    /// Every file, in the order they are written.
    pub const ALL: [&str; 8] = [
        CLUSTER,
        DEPLOY,
        DEPLOY_PUBLIC,
        LIVE,
        LIVE_PUBLIC,
        OPERATOR_TOKEN,
        CA,
        CA_KEY,
    ];
    /// A node instance's certificate.
    #[must_use]
    pub fn cert(instance: &str) -> String {
        format!("{instance}.crt")
    }
    /// A node instance's private key.
    #[must_use]
    pub fn key(instance: &str) -> String {
        format!("{instance}.key")
    }
}

/// Writes a fresh set of keys into `dir` (see [`files`]), with a cluster
/// CA for `cluster` valid for `ca_days`; refuses to overwrite any.
///
/// # Errors
/// A file exists, or a write failed.
pub fn new_keys(dir: &Path, cluster: &str, ca_days: u32) -> Result<(), String> {
    let exists = |f: &&str| dir.join(f).exists();
    let (ca, rest): (Vec<&str>, Vec<&str>) = files::ALL
        .iter()
        .partition(|f| **f == files::CA || **f == files::CA_KEY);
    // A set written before the cluster CA existed gains one; nothing is
    // ever overwritten.
    if rest.iter().all(exists) && !ca.iter().any(exists) {
        return write_ca(dir, cluster, ca_days);
    }
    if let Some(existing) = files::ALL.iter().map(|f| dir.join(f)).find(|p| p.exists()) {
        return Err(format!(
            "{} exists; keys are never overwritten",
            existing.display()
        ));
    }
    write_secret(
        &dir.join(files::CLUSTER),
        hex(&random(CLUSTER_KEY_BYTES)?).as_bytes(),
    )?;
    let (deploy, deploy_public) = new_key_pair()?;
    write_secret(&dir.join(files::DEPLOY), &deploy)?;
    write_secret(&dir.join(files::DEPLOY_PUBLIC), hex(&deploy_public).as_bytes())?;
    let (live, live_public) = new_key_pair()?;
    write_secret(&dir.join(files::LIVE), &live)?;
    write_secret(&dir.join(files::LIVE_PUBLIC), hex(&live_public).as_bytes())?;
    write_secret(&dir.join(files::OPERATOR_TOKEN), hex(&random(24)?).as_bytes())?;
    write_ca(dir, cluster, ca_days)
}

fn write_ca(dir: &Path, cluster: &str, ca_days: u32) -> Result<(), String> {
    let ca = crate::pki::new_ca(
        cluster,
        crate::pki::Validity::starting_now(std::time::SystemTime::now(), ca_days),
    )?;
    write_secret(&dir.join(files::CA), ca.cert_pem.as_bytes())?;
    write_secret(&dir.join(files::CA_KEY), ca.key_pem.as_bytes())?;
    Ok(())
}

/// Reads the CA files in `dir`.
///
/// # Errors
/// A file is missing.
pub fn read_ca(dir: &Path) -> Result<crate::pki::CaFiles, String> {
    let read =
        |f: &str| std::fs::read_to_string(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()));
    Ok(crate::pki::CaFiles {
        cert_pem: read(files::CA)?,
        key_pem: read(files::CA_KEY)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_junk() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(unhex("0001ABff").unwrap(), b);
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
        assert!(unhex(" 00").is_err());
    }

    #[test]
    fn a_key_pair_round_trips_through_pkcs8() {
        let (doc, public) = new_key_pair().unwrap();
        assert_eq!(public_of(&doc).unwrap(), public);
        assert!(public_of(&doc[1..]).is_err());
    }
}
