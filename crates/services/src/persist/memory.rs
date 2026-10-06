//! The in-memory store: the writer's reference implementation, used in
//! every test run.

use std::collections::{BTreeMap, BTreeSet};

use super::migrate::{MIGRATIONS, Migration, MigrationTarget, migrate};
use mantis_core::social::{FriendBook, FriendChange, GuildBook, GuildChange};

use super::{AuditRow, LedgerStore, StoreError, StoredLedger, StoredOutcome};

/// Everything in memory.
#[derive(Debug, Default)]
pub struct MemoryStore {
    migrations: Vec<(u32, String)>,
    months: BTreeSet<u32>,
    batches: BTreeMap<u64, u64>,
    outcomes: BTreeMap<u64, Vec<StoredOutcome>>,
    ledger: Vec<StoredLedger>,
    audit: Vec<AuditRow>,
    guilds: GuildBook,
    guild_seq: u64,
    friends: FriendBook,
    friend_seq: u64,
    /// Name -> (order, kind, value).
    live: BTreeMap<String, (u64, u8, f32)>,
    live_seq: u64,
    /// Fail the next write (tests of atomicity).
    pub fail_next_write: bool,
    /// Fail every audit write (tests that a command without its audit row
    /// never runs).
    pub fail_audit: bool,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies `set` (tests of the migration runner use their own sets).
    ///
    /// # Errors
    /// The runner's refusal, as a [`StoreError`].
    pub fn migrate_with(&mut self, set: &[Migration]) -> Result<Vec<u32>, StoreError> {
        migrate(self, set).map_err(|e| StoreError(e.to_string()))
    }
}

impl MigrationTarget for MemoryStore {
    fn applied(&mut self) -> Result<Vec<(u32, String)>, String> {
        Ok(self.migrations.clone())
    }

    fn apply(&mut self, m: &Migration, checksum: &str) -> Result<(), String> {
        self.migrations.push((m.version, checksum.to_owned()));
        Ok(())
    }
}

impl LedgerStore for MemoryStore {
    fn migrate(&mut self) -> Result<Vec<u32>, StoreError> {
        self.migrate_with(MIGRATIONS)
    }

    fn ensure_month(&mut self, month: u32) -> Result<(), StoreError> {
        self.months.insert(month);
        Ok(())
    }

    fn months(&mut self) -> Result<Vec<u32>, StoreError> {
        Ok(self.months.iter().copied().collect())
    }

    fn last_batch(&mut self, cell: u64) -> Result<Option<u64>, StoreError> {
        Ok(self.batches.get(&cell).copied())
    }

    fn write_batch(
        &mut self,
        cell: u64,
        batch: u64,
        outcomes: &[StoredOutcome],
        ledger: &[StoredLedger],
    ) -> Result<(), StoreError> {
        if std::mem::take(&mut self.fail_next_write) {
            return Err(StoreError("write failed (injected)".to_owned()));
        }
        if let Some(l) = ledger.iter().find(|l| !self.months.contains(&l.month)) {
            return Err(StoreError(format!("no partition for month {}", l.month)));
        }
        self.outcomes.entry(cell).or_default().extend_from_slice(outcomes);
        self.ledger.extend_from_slice(ledger);
        self.batches.insert(cell, batch);
        Ok(())
    }

    fn ledger_of(&mut self, character: u64) -> Result<Vec<StoredLedger>, StoreError> {
        Ok(self
            .ledger
            .iter()
            .filter(|l| l.character == character)
            .copied()
            .collect())
    }

    fn outcomes_of(&mut self, cell: u64) -> Result<Vec<StoredOutcome>, StoreError> {
        Ok(self.outcomes.get(&cell).cloned().unwrap_or_default())
    }

    fn audit_begin(&mut self, actor: &str, command: &str, args: &str, at_ms: u64) -> Result<u64, StoreError> {
        if self.fail_audit {
            return Err(StoreError("audit write failed (injected)".to_owned()));
        }
        let id = self.audit.len() as u64 + 1;
        self.audit.push(AuditRow {
            id,
            at_ms,
            actor: actor.to_owned(),
            command: command.to_owned(),
            args: args.to_owned(),
            status: "begun".to_owned(),
            before: None,
            after: None,
            undo: None,
        });
        Ok(id)
    }

    fn audit_complete(
        &mut self,
        id: u64,
        status: &str,
        before: &str,
        after: &str,
        undo: &str,
    ) -> Result<(), StoreError> {
        let row = self
            .audit
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| StoreError(format!("no audit row {id}")))?;
        status.clone_into(&mut row.status);
        row.before = Some(before.to_owned());
        row.after = Some(after.to_owned());
        row.undo = Some(undo.to_owned());
        Ok(())
    }

    fn audit_rows(&mut self) -> Result<Vec<AuditRow>, StoreError> {
        Ok(self.audit.clone())
    }

    fn guild_seq(&mut self) -> Result<u64, StoreError> {
        Ok(self.guild_seq)
    }

    fn write_guilds(&mut self, seq: u64, changes: &[GuildChange]) -> Result<(), StoreError> {
        if std::mem::take(&mut self.fail_next_write) {
            return Err(StoreError("write failed (injected)".to_owned()));
        }
        // The same rows Postgres keeps: rebuild from them plus this batch.
        let mut rows = self.guilds.rows();
        rows.extend_from_slice(changes);
        self.guilds = GuildBook::from_rows(&rows);
        self.guild_seq = seq;
        Ok(())
    }

    fn guild_rows(&mut self) -> Result<Vec<GuildChange>, StoreError> {
        Ok(self.guilds.rows())
    }

    fn friend_seq(&mut self) -> Result<u64, StoreError> {
        Ok(self.friend_seq)
    }

    fn write_friends(&mut self, seq: u64, changes: &[FriendChange]) -> Result<(), StoreError> {
        if std::mem::take(&mut self.fail_next_write) {
            return Err(StoreError("write failed (injected)".to_owned()));
        }
        // The same rows Postgres keeps: rebuild from them plus this batch.
        let mut rows = self.friends.rows();
        rows.extend_from_slice(changes);
        self.friends = FriendBook::from_rows(&rows);
        self.friend_seq = seq;
        Ok(())
    }

    fn friend_rows(&mut self) -> Result<Vec<FriendChange>, StoreError> {
        Ok(self.friends.rows())
    }

    fn set_live(&mut self, name: &str, kind: u8, value: f32) -> Result<(), StoreError> {
        if std::mem::take(&mut self.fail_next_write) {
            return Err(StoreError("write failed (injected)".to_owned()));
        }
        self.live_seq += 1;
        self.live.insert(name.to_owned(), (self.live_seq, kind, value));
        Ok(())
    }

    fn live_values(&mut self) -> Result<Vec<(String, u8, f32)>, StoreError> {
        let mut v: Vec<(u64, String, u8, f32)> = self
            .live
            .iter()
            .map(|(n, (seq, kind, value))| (*seq, n.clone(), *kind, *value))
            .collect();
        v.sort_by_key(|x| x.0);
        Ok(v.into_iter().map(|(_, n, k, val)| (n, k, val)).collect())
    }
}
