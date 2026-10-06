//! Hot reload: a polling file watcher and the report of a reload.
//!
//! [`SourceWatcher`] polls one file. It compares the modification time and
//! length first (one `metadata` call) and only reads the file when they
//! changed; it then compares a 64-bit FNV-1a hash of the contents, so a
//! touch without an edit does not trigger a reload. `Ui::reload` applies new
//! sources: on a parse error the old tree stays live; on success node state
//! is carried over by id.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What a successful reload changed, by element id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// Ids present only in the new layout.
    pub added: Vec<String>,
    /// Ids present only in the old layout.
    pub removed: Vec<String>,
    /// Ids present in both (their state was preserved).
    pub kept: Vec<String>,
}

/// 64-bit FNV-1a: a cheap, non-cryptographic content hash.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Polls a file for changes.
#[derive(Clone, Debug)]
pub struct SourceWatcher {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
    hash: Option<u64>,
}

impl SourceWatcher {
    /// Watches `path`. The first successful [`SourceWatcher::poll`] returns
    /// the current contents.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            stamp: None,
            hash: None,
        }
    }

    /// The watched path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the new contents when the file changed since the last poll.
    ///
    /// # Errors
    /// I/O errors from reading the file's metadata or contents (a missing
    /// file, for example). The watcher's state is unchanged on error.
    pub fn poll(&mut self) -> io::Result<Option<String>> {
        let meta = std::fs::metadata(&self.path)?;
        let stamp = (meta.modified()?, meta.len());
        if self.stamp == Some(stamp) {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.path)?;
        self.stamp = Some(stamp);
        let h = fnv1a(text.as_bytes());
        if self.hash == Some(h) {
            return Ok(None);
        }
        self.hash = Some(h);
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_values() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
