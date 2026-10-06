//! Source content discovery.
//!
//! A [`ContentTree`] is every regular file under a content root, keyed by its path
//! relative to the root with `/` separators, in byte order (so cooking is the same on
//! every platform). Hidden files and folders (names starting with `.`) and any folder
//! named `cooked` are skipped: cooked output never feeds back into a cook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::importer::{CookError, Source};

/// Every source file of one content root.
#[derive(Clone, Debug, Default)]
pub struct ContentTree {
    files: BTreeMap<String, Vec<u8>>,
}

impl ContentTree {
    /// An empty tree (sources added with [`ContentTree::insert`], for tests and tools).
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads every file under `root`.
    ///
    /// # Errors
    /// [`CookError`] for an unreadable file or a non-UTF-8 path.
    pub fn read(root: &Path) -> Result<ContentTree, CookError> {
        let mut tree = ContentTree::new();
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| CookError::io(&dir, &e))?;
            for entry in entries {
                let entry = entry.map_err(|e| CookError::io(&dir, &e))?;
                let path = entry.path();
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| CookError::at(&path.to_string_lossy(), 0, "path is not UTF-8"))?;
                if name.starts_with('.') {
                    continue;
                }
                let kind = entry.file_type().map_err(|e| CookError::io(&path, &e))?;
                if kind.is_dir() {
                    if name != "cooked" {
                        stack.push(path);
                    }
                } else if kind.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .map_err(|_| CookError::at(&path.to_string_lossy(), 0, "outside the content root"))?
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    let bytes = std::fs::read(&path).map_err(|e| CookError::io(&path, &e))?;
                    tree.files.insert(rel, bytes);
                }
            }
        }
        Ok(tree)
    }

    /// Adds or replaces a source file.
    pub fn insert(&mut self, path: &str, bytes: impl Into<Vec<u8>>) {
        self.files.insert(path.to_owned(), bytes.into());
    }

    /// Every source, in path order.
    pub fn sources(&self) -> impl Iterator<Item = Source<'_>> {
        self.files.iter().map(|(path, bytes)| Source { path, bytes })
    }

    /// One source by path.
    pub fn get(&self, path: &str) -> Option<Source<'_>> {
        self.files
            .get_key_value(path)
            .map(|(path, bytes)| Source { path, bytes })
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}
