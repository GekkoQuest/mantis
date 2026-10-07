//! The in-house migration runner (approved for M6): numbered SQL files
//! embedded in the binary, applied in order, one transaction per file,
//! recorded with a checksum in `schema_migrations`. A file already applied
//! whose text changed refuses start: history is never rewritten.

use ring::digest;

/// One migration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Migration {
    /// Its number (applied in ascending order).
    pub version: u32,
    /// Its file name.
    pub name: &'static str,
    /// Its SQL.
    pub sql: &'static str,
}

/// Every migration, in order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_records.sql",
        sql: include_str!("../../migrations/0001_records.sql"),
    },
    Migration {
        version: 2,
        name: "0002_ledger_partitions.sql",
        sql: include_str!("../../migrations/0002_ledger_partitions.sql"),
    },
    Migration {
        version: 3,
        name: "0003_guilds.sql",
        sql: include_str!("../../migrations/0003_guilds.sql"),
    },
    Migration {
        version: 4,
        name: "0004_friends.sql",
        sql: include_str!("../../migrations/0004_friends.sql"),
    },
    Migration {
        version: 5,
        name: "0005_live.sql",
        sql: include_str!("../../migrations/0005_live.sql"),
    },
    Migration {
        version: 6,
        name: "0006_accounts.sql",
        sql: include_str!("../../migrations/0006_accounts.sql"),
    },
    Migration {
        version: 7,
        name: "0007_characters.sql",
        sql: include_str!("../../migrations/0007_characters.sql"),
    },
];

/// The SHA-256 of a migration's text, hex.
#[must_use]
pub fn checksum(sql: &str) -> String {
    let d = digest::digest(&digest::SHA256, sql.as_bytes());
    d.as_ref().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Where migrations are applied.
pub trait MigrationTarget {
    /// The migrations already applied: `(version, checksum)`.
    ///
    /// # Errors
    /// The store's error, as text.
    fn applied(&mut self) -> Result<Vec<(u32, String)>, String>;

    /// Applies one migration and records it, in one transaction.
    ///
    /// # Errors
    /// The store's error, as text; nothing is recorded.
    fn apply(&mut self, m: &Migration, checksum: &str) -> Result<(), String>;
}

/// Why migrating refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MigrateError {
    /// An applied migration's text changed.
    Modified {
        /// Its version.
        version: u32,
    },
    /// The store has a migration this binary does not know (a newer build
    /// ran against it).
    Unknown {
        /// Its version.
        version: u32,
    },
    /// The store failed.
    Store(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Modified { version } => write!(f, "migration {version} was applied with different text"),
            Self::Unknown { version } => write!(
                f,
                "the store has migration {version}, which this build does not know"
            ),
            Self::Store(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for MigrateError {}

/// Applies every pending migration of `set` to `target`, in order. Returns
/// the versions applied (empty when up to date).
///
/// # Errors
/// [`MigrateError`]: nothing further is applied.
pub fn migrate(target: &mut dyn MigrationTarget, set: &[Migration]) -> Result<Vec<u32>, MigrateError> {
    let applied = target.applied().map_err(MigrateError::Store)?;
    for (version, sum) in &applied {
        match set.iter().find(|m| m.version == *version) {
            None => return Err(MigrateError::Unknown { version: *version }),
            Some(m) if checksum(m.sql) != *sum => return Err(MigrateError::Modified { version: *version }),
            Some(_) => {}
        }
    }
    let mut done = Vec::new();
    let mut pending: Vec<&Migration> = set
        .iter()
        .filter(|m| !applied.iter().any(|(v, _)| *v == m.version))
        .collect();
    pending.sort_by_key(|m| m.version);
    for m in pending {
        target.apply(m, &checksum(m.sql)).map_err(MigrateError::Store)?;
        done.push(m.version);
    }
    Ok(done)
}
