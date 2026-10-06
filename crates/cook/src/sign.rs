//! Bundle signing (Ed25519).
//!
//! Development bundles are signed with a key generated per run ([`SigningKey::generate`]);
//! production keys never live in the repository: Ops provides a PKCS#8 key file whose path
//! the cook is given ([`SigningKey::from_pkcs8_file`]). Hosts hold only the public key and
//! verify with [`mantis_formats::bundle::SignedBundle::verify`].

use std::path::Path;

use mantis_formats::bundle::{Bundle, SIGNATURE_LEN};
use ring::signature::KeyPair;

use crate::importer::CookError;

/// A private signing key.
pub struct SigningKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl core::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SigningKey")
            .field("public", &self.public_key())
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    /// A fresh random key (development and tests).
    ///
    /// # Errors
    /// [`CookError`] when the system random source fails.
    pub fn generate() -> Result<SigningKey, CookError> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| CookError::at("<signing key>", 0, "could not generate a key"))?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// A key from PKCS#8 bytes.
    ///
    /// # Errors
    /// [`CookError`] for bytes that are not an Ed25519 PKCS#8 key.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<SigningKey, CookError> {
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(bytes)
            .map_err(|_| CookError::at("<signing key>", 0, "not an Ed25519 PKCS#8 key"))?;
        Ok(SigningKey { pair })
    }

    /// A key from a PKCS#8 file (the production path Ops provides).
    ///
    /// # Errors
    /// [`CookError`] for an unreadable file or an invalid key.
    pub fn from_pkcs8_file(path: &Path) -> Result<SigningKey, CookError> {
        let bytes = std::fs::read(path).map_err(|e| CookError::io(path, &e))?;
        Self::from_pkcs8(&bytes)
    }

    /// The public key hosts verify with (32 bytes).
    pub fn public_key(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.pair.public_key().as_ref());
        out
    }

    /// The signed bundle bytes.
    pub fn sign(&self, bundle: &Bundle) -> Vec<u8> {
        let signed = bundle.signed_bytes();
        let sig = self.pair.sign(&signed);
        let mut signature = [0u8; SIGNATURE_LEN];
        signature.copy_from_slice(sig.as_ref());
        bundle.encode(&signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_core::content::ContentHash;
    use mantis_formats::FormatError;
    use mantis_formats::bundle::{AssetKind, Domain, Entry, SignedBundle};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn bundle() -> Bundle {
        Bundle {
            domain: Domain::Presentation,
            content_version: 3,
            entries: vec![Entry {
                name: "meshes/crate.mesh".to_owned(),
                kind: AssetKind::Mesh,
                size: 4,
                hash: ContentHash::of(b"mesh"),
            }],
        }
    }

    #[test]
    fn a_signed_bundle_verifies_only_with_its_key_and_only_unaltered() -> TestResult {
        let key = SigningKey::generate()?;
        let other = SigningKey::generate()?;
        let bytes = key.sign(&bundle());
        let parsed = SignedBundle::parse(&bytes)?;
        assert_eq!(parsed.verify(&key.public_key())?, &bundle());
        assert_eq!(
            parsed.verify(&other.public_key()).err(),
            Some(FormatError::Validity),
            "another key"
        );
        // A single flipped byte anywhere is refused (structure or signature).
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= 0x01;
            }
            let accepted = SignedBundle::parse(&b).is_ok_and(|s| s.verify(&key.public_key()).is_ok());
            assert!(!accepted, "byte {i} flipped and still accepted");
        }
        Ok(())
    }

    #[test]
    fn keys_round_trip_through_pkcs8() -> TestResult {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "keygen")?;
        let a = SigningKey::from_pkcs8(pkcs8.as_ref())?;
        let b = SigningKey::from_pkcs8(pkcs8.as_ref())?;
        assert_eq!(a.public_key(), b.public_key());
        assert!(SigningKey::from_pkcs8(&[1, 2, 3]).is_err());
        Ok(())
    }
}
