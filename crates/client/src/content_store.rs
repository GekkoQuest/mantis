//! Read side of the cooked content store (the cook's `store` writes it; plan 11).
//!
//! Layout under the store root: `objects/<first two hex digits>/<64 hex digits>` for
//! payloads keyed by their BLAKE3 hash, and `bundles/<domain>.bundle` for the signed
//! manifests. Every read is verified: a payload whose bytes do not hash to its name, or a
//! bundle whose signature does not verify against the trusted public key, is refused.
//!
//! The handshake content hash is [`Bundle::hash`] of the gameplay bundle.

use std::path::{Path, PathBuf};

use mantis_core::content::ContentHash;
use mantis_formats::FormatError;
use mantis_formats::bundle::{Bundle, Domain, SignedBundle};

/// Why content could not be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StoreError {
    /// A file could not be read.
    Io {
        /// The path.
        path: String,
        /// The OS error.
        error: String,
    },
    /// A bundle does not parse.
    Bundle(FormatError),
    /// A bundle's signature does not verify against the trusted key.
    Signature,
    /// The bundle file holds another domain.
    Domain,
    /// A payload's bytes do not match its hash.
    Corrupt(ContentHash),
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StoreError::Io { path, error } => write!(f, "{path}: {error}"),
            StoreError::Bundle(e) => write!(f, "bundle: {e}"),
            StoreError::Signature => f.write_str("bundle signature does not verify"),
            StoreError::Domain => f.write_str("bundle is for another domain"),
            StoreError::Corrupt(h) => write!(f, "object {h} does not match its hash"),
        }
    }
}

impl std::error::Error for StoreError {}

fn domain_name(domain: Domain) -> &'static str {
    match domain {
        Domain::Gameplay => "gameplay",
        Domain::Server => "server",
        Domain::Presentation => "presentation",
    }
}

/// A cooked content store on disk.
#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
}

impl ContentStore {
    /// The store at `root`.
    pub fn open(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read(path: &Path) -> Result<Vec<u8>, StoreError> {
        std::fs::read(path).map_err(|e| StoreError::Io {
            path: path.to_string_lossy().into_owned(),
            error: e.to_string(),
        })
    }

    /// The verified bundle of `domain`.
    ///
    /// # Errors
    /// [`StoreError::Io`], [`StoreError::Bundle`], [`StoreError::Signature`], or
    /// [`StoreError::Domain`].
    pub fn bundle(&self, domain: Domain, public_key: &[u8]) -> Result<Bundle, StoreError> {
        let path = self
            .root
            .join("bundles")
            .join(format!("{}.bundle", domain_name(domain)));
        let bytes = Self::read(&path)?;
        let signed = SignedBundle::parse(&bytes).map_err(StoreError::Bundle)?;
        let bundle = signed
            .verify(public_key)
            .map_err(|_| StoreError::Signature)?
            .clone();
        if bundle.domain != domain {
            return Err(StoreError::Domain);
        }
        Ok(bundle)
    }

    /// The payload with `hash`, verified.
    ///
    /// # Errors
    /// [`StoreError::Io`] or [`StoreError::Corrupt`].
    pub fn get(&self, hash: &ContentHash) -> Result<Vec<u8>, StoreError> {
        let hex = hash.to_string();
        let shard = hex.get(..2).unwrap_or("00");
        let bytes = Self::read(&self.root.join("objects").join(shard).join(&hex))?;
        if ContentHash::of(&bytes) != *hash {
            return Err(StoreError::Corrupt(*hash));
        }
        Ok(bytes)
    }
}
