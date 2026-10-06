//! The importer plugin API.
//!
//! An [`Importer`] turns source files into cooked payloads. Built-in importers handle the
//! engine's source formats; a package registers its own for formats only it uses (a
//! legacy archive, a custom table). The cook runs importers in **phase** order; within a
//! phase, sources in path order. An importer may look up what an earlier phase produced
//! from another source ([`ImportContext::resolve`]): a material names its textures, a
//! sector its meshes and materials, by source path, and receives their content hashes.
//!
//! Importers are pure functions of their source bytes, the outputs they resolve, and
//! their version: no clocks, no randomness, no file system access outside the tree.

use std::collections::BTreeMap;

use mantis_core::content::ContentHash;
use mantis_formats::bundle::{AssetKind, Domain};

use crate::tree::ContentTree;

/// One source file.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    /// Path relative to the content root, `/`-separated.
    pub path: &'a str,
    /// Bytes.
    pub bytes: &'a [u8],
}

impl Source<'_> {
    /// The bytes as UTF-8 text.
    ///
    /// # Errors
    /// [`CookError`] when they are not UTF-8.
    pub fn text(&self) -> Result<&str, CookError> {
        core::str::from_utf8(self.bytes)
            .map_err(|e| CookError::at(self.path, 0, &format!("not UTF-8 text ({e})")))
    }

    /// The file extension (after the last `.` of the file name), if any.
    pub fn extension(&self) -> Option<&str> {
        let name = self.path.rsplit('/').next().unwrap_or(self.path);
        name.rsplit_once('.').map(|(_, ext)| ext)
    }

    /// The path without its extension.
    pub fn stem_path(&self) -> &str {
        match self.extension() {
            Some(ext) => self
                .path
                .get(..self.path.len() - ext.len() - 1)
                .unwrap_or(self.path),
            None => self.path,
        }
    }
}

/// A cook error, located in a source file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CookError {
    /// The source path (or a file system path for I/O errors).
    pub file: String,
    /// 1-based line, or 0 when the error is about the whole file.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl CookError {
    /// An error at `file:line`.
    pub fn at(file: &str, line: usize, message: &str) -> Self {
        Self {
            file: file.to_owned(),
            line,
            message: message.to_owned(),
        }
    }

    /// An I/O error on `path`.
    pub fn io(path: &std::path::Path, e: &std::io::Error) -> Self {
        Self {
            file: path.to_string_lossy().into_owned(),
            line: 0,
            message: e.to_string(),
        }
    }
}

impl core::fmt::Display for CookError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line > 0 {
            write!(f, "{}:{}: {}", self.file, self.line, self.message)
        } else {
            write!(f, "{}: {}", self.file, self.message)
        }
    }
}

impl std::error::Error for CookError {}

/// One cooked payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cooked {
    /// Logical name in the bundle (relative path, `/`-separated, unique per cook).
    pub name: String,
    /// What it is.
    pub kind: AssetKind,
    /// Which bundle it belongs to.
    pub domain: Domain,
    /// The payload.
    pub bytes: Vec<u8>,
}

/// What an earlier phase produced from one source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Produced {
    /// Output name.
    pub name: String,
    /// What it is.
    pub kind: AssetKind,
    /// Its content hash.
    pub hash: ContentHash,
}

/// What an importer may look at while importing.
pub struct ImportContext<'a> {
    pub(crate) tree: &'a ContentTree,
    pub(crate) assets: &'a BTreeMap<String, crate::pipeline::CookedAsset>,
    pub(crate) produced: &'a BTreeMap<String, Vec<Produced>>,
    pub(crate) phase: u32,
    pub(crate) importer: &'static str,
}

impl ImportContext<'_> {
    /// The phase running now.
    pub fn phase(&self) -> u32 {
        self.phase
    }

    /// Another source file's bytes.
    pub fn source(&self, path: &str) -> Option<Source<'_>> {
        self.tree.get(path)
    }

    /// The outputs an earlier phase produced from source `path`.
    pub fn produced(&self, path: &str) -> &[Produced] {
        self.produced.get(path).map_or(&[], Vec::as_slice)
    }

    /// Every output of `kind` that earlier phases produced, as `(name, bytes)` in name
    /// order (a presentation graph checks its bindings against every cooked gameplay
    /// graph).
    pub fn cooked_of(&self, kind: AssetKind) -> impl Iterator<Item = (&str, &[u8])> {
        self.assets
            .iter()
            .filter(move |(_, a)| a.kind == kind)
            .map(|(name, a)| (name.as_str(), a.bytes.as_slice()))
    }

    /// The content hash of the single output of `kind` that an earlier phase produced
    /// from source `path`. `from` and `line` locate the reference for the error.
    ///
    /// # Errors
    /// [`CookError`] when `path` produced no such output (missing, not cooked yet because
    /// its importer runs in this or a later phase, or ambiguous).
    pub fn resolve(
        &self,
        path: &str,
        kind: AssetKind,
        from: &str,
        line: usize,
    ) -> Result<ContentHash, CookError> {
        self.resolve_bytes(path, kind, from, line).map(|(hash, _)| hash)
    }

    /// Like [`ImportContext::resolve`], with the cooked bytes (a VAT bake reads the
    /// skeleton, clip, and mesh it names; a baker reads sector geometry).
    ///
    /// # Errors
    /// As [`ImportContext::resolve`].
    pub fn resolve_bytes(
        &self,
        path: &str,
        kind: AssetKind,
        from: &str,
        line: usize,
    ) -> Result<(ContentHash, &[u8]), CookError> {
        let mut found = self.produced(path).iter().filter(|p| p.kind == kind);
        match (found.next(), found.next()) {
            (Some(p), None) => self
                .assets
                .get(&p.name)
                .map(|a| (p.hash, a.bytes.as_slice()))
                .ok_or_else(|| {
                    CookError::at(from, line, &format!("`{path}` output `{}` is missing", p.name))
                }),
            (Some(_), Some(_)) => Err(CookError::at(
                from,
                line,
                &format!("`{path}` produced more than one {kind:?}"),
            )),
            (None, _) => Err(CookError::at(
                from,
                line,
                &format!(
                    "`{path}` is not a cooked {kind:?} (missing, or its importer does not run before `{}`)",
                    self.importer
                ),
            )),
        }
    }
}

/// An importer plugin.
pub trait Importer: Send + Sync {
    /// A stable name (`mesh.obj`, `texture.netpbm`, a package's `game.archive`).
    fn name(&self) -> &'static str;

    /// The output version. Bump it whenever the same source would cook to different
    /// bytes, so the content hash changes visibly.
    fn version(&self) -> u32;

    /// When it runs: lower phases first. Built-ins use 0 (leaf assets: tables, meshes,
    /// textures, skeletons, UI, gradings, mixer graphs), 5 (particle effects), 10 (assets
    /// naming leaves: clips, materials, presentation graphs, sound banks), 15 (animation
    /// graphs and VAT bakes, which name clips), 20 (sector bakes: probe volumes and
    /// lightmaps), 30 (sectors, which pick up their bakes).
    fn phase(&self) -> u32;

    /// Whether this importer handles the source at `path`.
    fn accepts(&self, path: &str) -> bool;

    /// Whether the file at `path` is an input this importer reads through
    /// [`ImportContext::source`] while cooking another source (a texture's settings
    /// sidecar, a sound's wave file). Inputs are not cooked on their own by any importer
    /// (even one that accepts their extension), and the cook
    /// does not report them as unhandled.
    fn inputs(&self, path: &str) -> bool {
        let _ = path;
        false
    }

    /// Cooks one source.
    ///
    /// # Errors
    /// [`CookError`] located in the source.
    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError>;
}
