//! Cooking a whole package: `packages/<p>/content/` into the content-addressed store at
//! `packages/<p>/cooked/` (gitignored), with one signed bundle per domain.
//!
//! Store layout (see [`crate::store`]): `objects/<2 hex>/<64 hex>` payloads,
//! `bundles/{gameplay,server,presentation}.bundle` signed manifests, and, for a
//! development cook, [`DEV_PUBLIC_KEY`] holding the 32-byte Ed25519 public key of the key
//! generated for that run. A production cook signs with the PKCS#8 key Ops provides and
//! writes no key file; hosts are configured with the production public key instead.
//!
//! The handshake content hash is the hash of the gameplay bundle manifest
//! ([`mantis_formats::bundle::Bundle::hash`]), the first of [`Cooked::hashes`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mantis_core::content::ContentHash;

use crate::importer::CookError;
use crate::importers;
use crate::pipeline::Cook;
use crate::sign::SigningKey;
use crate::store::Store;
use crate::tree::ContentTree;

/// The development public key file, relative to the store root.
pub const DEV_PUBLIC_KEY: &str = "keys/dev.pub";

/// The graph actions the package's resolved modules declare (`graph_actions` in each
/// module manifest), when `content` is a package's `content/` directory inside a
/// workspace (`packages/<p>/content` next to `packages/<p>/package.toml`); `None`
/// otherwise, and the cook then checks graph structure only (cells still refuse unknown
/// action names at load).
///
/// # Errors
/// [`CookError`] for an unreadable or invalid manifest, or a package whose module graph
/// does not resolve (the refusal a host would give).
pub fn declared_actions(content: &Path) -> Result<Option<BTreeSet<String>>, CookError> {
    use mantis_core::module::{Discovered, parse_manifest, parse_package, resolve};
    let Some(package_dir) = content.parent() else {
        return Ok(None);
    };
    let package_toml = package_dir.join("package.toml");
    let Some(packages) = package_dir.parent() else {
        return Ok(None);
    };
    if !package_toml.is_file() || packages.file_name().is_none_or(|n| n != "packages") {
        return Ok(None);
    }
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| CookError::io(p, &e));
    let package = package_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modules_of = parse_package(&read(&package_toml)?)
        .map_err(|e| CookError::at(&package_toml.display().to_string(), 0, &e.to_string()))?;
    let mut found = Vec::new();
    for origin in sorted_dirs(packages) {
        let name = origin
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name != package && !modules_of.uses.contains(&name) {
            continue;
        }
        for module in sorted_dirs(&origin.join("modules")) {
            let path = module.join("manifest.toml");
            if path.is_file() {
                let manifest = parse_manifest(&read(&path)?)
                    .map_err(|e| CookError::at(&path.display().to_string(), 0, &e.to_string()))?;
                found.push(Discovered {
                    origin: name.clone(),
                    manifest,
                });
            }
        }
    }
    let graph = resolve(&modules_of, &found, &BTreeMap::new())
        .map_err(|e| CookError::at(&package_toml.display().to_string(), 0, &e.to_string()))?;
    Ok(Some(
        found
            .iter()
            .filter(|d| graph.modules.iter().any(|r| r.key == d.manifest.key))
            .flat_map(|d| d.manifest.graph_actions.iter().cloned())
            .collect(),
    ))
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Where a package's sources and cooked store live.
pub fn layout(package: &Path) -> (PathBuf, PathBuf) {
    (package.join("content"), package.join("cooked"))
}

/// How bundles are signed.
#[derive(Debug)]
pub enum Signing {
    /// A key generated for this run; its public key is written to [`DEV_PUBLIC_KEY`].
    Development,
    /// The production key at this path (provided by Ops, never in the repository).
    Production(PathBuf),
}

/// A finished package cook.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cooked {
    /// Bundle hashes: gameplay (the handshake hash), server, presentation.
    pub hashes: [ContentHash; 3],
    /// The public key the bundles verify with.
    pub public_key: [u8; 32],
    /// Cooked assets.
    pub assets: usize,
}

/// Cooks `content` into the store at `out` with the built-in importers.
///
/// # Errors
/// Every [`CookError`] the cook found (each located at its source file and line), or the
/// I/O or signing failure.
pub fn cook(
    content: &Path,
    out: &Path,
    content_version: u32,
    signing: &Signing,
) -> Result<Cooked, Vec<CookError>> {
    let tree = ContentTree::read(content).map_err(|e| vec![e])?;
    let mut all = importers::builtin();
    if let Some(actions) = declared_actions(content).map_err(|e| vec![e])? {
        all.retain(|i| i.name() != "graph.toml");
        all.push(std::sync::Arc::new(
            importers::content::gameplay::Graphs::with_actions(actions),
        ));
    }
    let output = Cook::new(all).map_err(|e| vec![e])?.run(&tree)?;
    let key = match signing {
        Signing::Development => SigningKey::generate(),
        Signing::Production(path) => SigningKey::from_pkcs8_file(path),
    }
    .map_err(|e| vec![e])?;
    let store = Store::new(out);
    let hashes = store
        .publish(&output, content_version, &key)
        .map_err(|e| vec![e])?;
    let public_key = key.public_key();
    let key_path = out.join(DEV_PUBLIC_KEY);
    match signing {
        Signing::Development => {
            if let Some(dir) = key_path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| vec![CookError::io(dir, &e)])?;
            }
            std::fs::write(&key_path, public_key).map_err(|e| vec![CookError::io(&key_path, &e)])?;
        }
        Signing::Production(_) => {
            // A stale development key must never sit next to production bundles.
            if key_path.exists() {
                std::fs::remove_file(&key_path).map_err(|e| vec![CookError::io(&key_path, &e)])?;
            }
        }
    }
    Ok(Cooked {
        hashes,
        public_key,
        assets: output.assets.len(),
    })
}

/// Reads the development public key of a store.
///
/// # Errors
/// [`CookError`] when the file is missing or not 32 bytes.
pub fn dev_public_key(store: &Path) -> Result<[u8; 32], CookError> {
    let path = store.join(DEV_PUBLIC_KEY);
    let bytes = std::fs::read(&path).map_err(|e| CookError::io(&path, &e))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| CookError::at(&path.to_string_lossy(), 0, "a public key is 32 bytes"))
}
