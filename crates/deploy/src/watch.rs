//! Noticing that operator-provided files changed: certificates and keys
//! rotated in place, without a restart.
//!
//! A set of files is compared by (length, modification time) against the
//! set last **accepted**. A changed set is offered once; the caller loads
//! and checks it and accepts it only when it is good. A set that fails (a
//! certificate written before its key, say) is offered again at the next
//! look, so a rotation completes when the second file lands, and the old
//! material keeps serving meanwhile.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

type Stamp = Option<(u64, SystemTime)>;

fn stamp(p: &Path) -> Stamp {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Watches a set of files.
#[derive(Debug)]
pub struct FileWatch {
    paths: Vec<PathBuf>,
    accepted: Vec<Stamp>,
    failed: Option<Vec<Stamp>>,
    every: Duration,
    last_look: Option<Instant>,
}

impl FileWatch {
    /// Watches `paths`, their current state accepted, looking at most once
    /// per `every`.
    #[must_use]
    pub fn new(paths: Vec<PathBuf>, every: Duration) -> Self {
        let accepted = paths.iter().map(|p| stamp(p)).collect();
        Self {
            paths,
            accepted,
            failed: None,
            every,
            last_look: None,
        }
    }

    /// The files watched.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Whether the files differ from the set last accepted, and differ from
    /// the set last refused (so a bad set is reported once). Looks at the
    /// files at most once per `every`.
    pub fn changed(&mut self) -> bool {
        let now = Instant::now();
        if self.last_look.is_some_and(|t| now.duration_since(t) < self.every) {
            return false;
        }
        self.last_look = Some(now);
        let current: Vec<Stamp> = self.paths.iter().map(|p| stamp(p)).collect();
        current != self.accepted && self.failed.as_ref() != Some(&current)
    }

    /// Accepts the files as they are now.
    pub fn accept(&mut self) {
        self.accepted = self.paths.iter().map(|p| stamp(p)).collect();
        self.failed = None;
    }

    /// Records that the files as they are now were refused: not offered
    /// again until one of them changes.
    pub fn refuse(&mut self) {
        self.failed = Some(self.paths.iter().map(|p| stamp(p)).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_set_is_offered_until_accepted_and_a_refused_one_once() {
        let d = std::env::temp_dir().join(format!("mantis-watch-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b) = (d.join("a.pem"), d.join("b.pem"));
        std::fs::write(&a, "1").unwrap();
        std::fs::write(&b, "1").unwrap();
        let mut w = FileWatch::new(vec![a.clone(), b.clone()], Duration::ZERO);
        assert!(!w.changed());
        std::fs::write(&a, "22").unwrap();
        assert!(w.changed());
        w.refuse();
        assert!(!w.changed(), "a refused set is not offered again");
        std::fs::write(&b, "22").unwrap();
        assert!(w.changed(), "until a file changes");
        w.accept();
        assert!(!w.changed());
        let mut slow = FileWatch::new(vec![a.clone()], Duration::from_secs(3600));
        std::fs::write(&a, "333").unwrap();
        assert!(slow.changed(), "the first look happens at once");
        std::fs::write(&a, "4444").unwrap();
        assert!(!slow.changed(), "then at most once per period");
        let _ = std::fs::remove_dir_all(&d);
    }
}
