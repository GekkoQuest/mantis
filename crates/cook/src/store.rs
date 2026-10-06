//! The content-addressed store: every payload saved once under its BLAKE3 hash, plus the
//! signed bundles that list them.
//!
//! Layout under the store root: `objects/<first two hex digits>/<64 hex digits>` for
//! payloads and `bundles/<domain>.bundle` for signed manifests. Writes go to a temporary
//! name first and are renamed into place, so a crash never leaves a truncated object
//! under a valid name. Reads verify the hash.

use std::path::{Path, PathBuf};

use mantis_core::content::ContentHash;
use mantis_formats::bundle::Domain;

use crate::importer::CookError;
use crate::pipeline::CookOutput;
use crate::sign::SigningKey;

/// A store rooted at a directory.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

fn domain_name(d: Domain) -> &'static str {
    match d {
        Domain::Gameplay => "gameplay",
        Domain::Server => "server",
        Domain::Presentation => "presentation",
    }
}

impl Store {
    /// A store at `root` (created on first write).
    pub fn new(root: &Path) -> Store {
        Store {
            root: root.to_path_buf(),
        }
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.to_string();
        let shard = hex.get(..2).unwrap_or("00");
        self.root.join("objects").join(shard).join(&hex)
    }

    fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CookError> {
        let dir = path
            .parent()
            .ok_or_else(|| CookError::at(&path.to_string_lossy(), 0, "no parent folder"))?;
        std::fs::create_dir_all(dir).map_err(|e| CookError::io(dir, &e))?;
        let tmp = path.with_extension("partial");
        std::fs::write(&tmp, bytes).map_err(|e| CookError::io(&tmp, &e))?;
        std::fs::rename(&tmp, path).map_err(|e| CookError::io(path, &e))
    }

    /// Saves a payload (no-op when already present) and returns its hash.
    ///
    /// # Errors
    /// [`CookError`] for I/O failures.
    pub fn put(&self, bytes: &[u8]) -> Result<ContentHash, CookError> {
        let hash = ContentHash::of(bytes);
        let path = self.object_path(&hash);
        if !path.exists() {
            Self::write_atomic(&path, bytes)?;
        }
        Ok(hash)
    }

    /// Loads a payload, verifying its hash.
    ///
    /// # Errors
    /// [`CookError`] when it is missing, unreadable, or corrupt.
    pub fn get(&self, hash: &ContentHash) -> Result<Vec<u8>, CookError> {
        let path = self.object_path(hash);
        let bytes = std::fs::read(&path).map_err(|e| CookError::io(&path, &e))?;
        if ContentHash::of(&bytes) != *hash {
            return Err(CookError::at(
                &path.to_string_lossy(),
                0,
                "object does not match its hash",
            ));
        }
        Ok(bytes)
    }

    /// The path of the signed bundle of `domain`.
    pub fn bundle_path(&self, domain: Domain) -> PathBuf {
        self.root
            .join("bundles")
            .join(format!("{}.bundle", domain_name(domain)))
    }

    /// Saves every payload of `output` and one signed bundle per domain. Returns the
    /// bundle hashes (gameplay, server, presentation).
    ///
    /// # Errors
    /// [`CookError`] for I/O failures.
    pub fn publish(
        &self,
        output: &CookOutput,
        content_version: u32,
        key: &SigningKey,
    ) -> Result<[ContentHash; 3], CookError> {
        for asset in output.assets.values() {
            let _ = self.put(&asset.bytes)?;
        }
        let mut hashes = [ContentHash::ZERO; 3];
        for (slot, domain) in hashes
            .iter_mut()
            .zip([Domain::Gameplay, Domain::Server, Domain::Presentation])
        {
            let bundle = output.bundle(domain, content_version);
            *slot = bundle.hash();
            Self::write_atomic(&self.bundle_path(domain), &key.sign(&bundle))?;
        }
        Ok(hashes)
    }

    /// Loads and verifies the bundle of `domain` against `public_key`, then checks that
    /// every payload it lists is present and intact.
    ///
    /// # Errors
    /// [`CookError`] for a missing, unsigned, tampered, or incomplete bundle.
    pub fn load_bundle(
        &self,
        domain: Domain,
        public_key: &[u8],
    ) -> Result<mantis_formats::bundle::Bundle, CookError> {
        let path = self.bundle_path(domain);
        let bytes = std::fs::read(&path).map_err(|e| CookError::io(&path, &e))?;
        let file = path.to_string_lossy().into_owned();
        let signed = mantis_formats::bundle::SignedBundle::parse(&bytes)
            .map_err(|e| CookError::at(&file, 0, &format!("bundle: {e}")))?;
        let bundle = signed
            .verify(public_key)
            .map_err(|_| CookError::at(&file, 0, "signature does not verify"))?
            .clone();
        if bundle.domain != domain {
            return Err(CookError::at(&file, 0, "bundle is for another domain"));
        }
        for e in &bundle.entries {
            let _ = self.get(&e.hash)?;
        }
        Ok(bundle)
    }
}
