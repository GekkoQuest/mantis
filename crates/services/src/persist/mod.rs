//! The persistence writer: drains cell log outcomes into the system of
//! record (decision 0005), owns the ledger and the Ops audit trail.
//!
//! The writer is idempotent per cell: each push carries the cell's batch
//! sequence number, a batch at or below the stored watermark is
//! acknowledged without being written again, and a batch is written in
//! one transaction with its outcomes, its ledger rows, and the new
//! watermark. Ledger rows land in the month partition of the outcome's
//! time; partitions are created by migration and ahead of need.

pub mod memory;
pub mod migrate;
pub mod pg;

use std::sync::{Arc, Mutex};

use mantis_core::ledger::ledger_of;
use mantis_core::social::{FriendChange, GuildChange};
use mantis_core::wire::{BoundedArray, WireString};

use crate::generated::services as m;
use crate::host::rpc::{Router, RpcError};
use crate::methods;

/// One outcome as stored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoredOutcome {
    /// Cell tick.
    pub tick: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// Command kind.
    pub kind: u16,
    /// Session (0 none).
    pub session: u64,
    /// Succeeded.
    pub ok: bool,
    /// Payload.
    pub payload: Vec<u8>,
}

/// One durable account (the account role's row; session tokens are never
/// stored).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccountRecord {
    /// The account.
    pub id: u64,
    /// The name as registered (unique case-insensitively, [`name_key`]).
    pub name: String,
    /// The password salt.
    pub salt: [u8; 16],
    /// The derived password key.
    pub hash: [u8; 32],
    /// The derivation's iterations.
    pub iterations: u32,
    /// Unix milliseconds it was created.
    pub created_ms: u64,
    /// Unix milliseconds of its last login (0 never).
    pub last_login_ms: u64,
    /// Unix milliseconds a ban ends (0 not banned).
    pub banned_until_ms: u64,
    /// Why it was banned.
    pub ban_reason: String,
}

/// One durable character (the realm's summary: its world state stays in
/// its cell's snapshots and outcomes).
#[derive(Clone, PartialEq, Debug)]
pub struct CharacterRecord {
    /// The character.
    pub id: u64,
    /// Its account.
    pub account: u64,
    /// Its name (unique among living characters, case-insensitively).
    pub name: String,
    /// Its class or kind.
    pub kind: u32,
    /// Unix milliseconds it was created.
    pub created_ms: u64,
    /// Deleted: kept so its id is never reused.
    pub deleted: bool,
    /// The cell it was last in (0 never entered).
    pub cell: u64,
    /// That cell's world.
    pub world: u32,
    /// Where it was last.
    pub position: [f32; 3],
    /// Its level.
    pub level: u32,
}

/// The key a name is unique under: lower-cased.
#[must_use]
pub fn name_key(name: &str) -> String {
    name.to_lowercase()
}

/// An account record as the wire carries it.
#[must_use]
pub fn account_row(a: &AccountRecord) -> m::AccountRow {
    m::AccountRow {
        id: a.id,
        name: WireString::new(&a.name).unwrap_or_default(),
        salt: BoundedArray::from_slice(&a.salt).unwrap_or_default(),
        hash: BoundedArray::from_slice(&a.hash).unwrap_or_default(),
        iterations: a.iterations,
        created_ms: a.created_ms,
        last_login_ms: a.last_login_ms,
        banned_until_ms: a.banned_until_ms,
        ban_reason: WireString::new(&a.ban_reason).unwrap_or_default(),
    }
}

/// An account record back from the wire (`None` for a malformed salt or
/// key).
#[must_use]
pub fn account_record(r: &m::AccountRow) -> Option<AccountRecord> {
    let salt: Vec<u8> = r.salt.iter().copied().collect();
    let hash: Vec<u8> = r.hash.iter().copied().collect();
    Some(AccountRecord {
        id: r.id,
        name: r.name.as_str().to_owned(),
        salt: salt.try_into().ok()?,
        hash: hash.try_into().ok()?,
        iterations: r.iterations,
        created_ms: r.created_ms,
        last_login_ms: r.last_login_ms,
        banned_until_ms: r.banned_until_ms,
        ban_reason: r.ban_reason.as_str().to_owned(),
    })
}

/// A character record as the wire carries it.
#[must_use]
pub fn character_row(c: &CharacterRecord) -> m::CharacterRow {
    m::CharacterRow {
        id: c.id,
        account: c.account,
        name: WireString::new(&c.name).unwrap_or_default(),
        kind: c.kind,
        created_ms: c.created_ms,
        deleted: c.deleted,
        cell: c.cell,
        world: c.world,
        x: c.position[0],
        y: c.position[1],
        z: c.position[2],
        level: c.level,
    }
}

/// A character record back from the wire.
#[must_use]
pub fn character_record(r: &m::CharacterRow) -> CharacterRecord {
    CharacterRecord {
        id: r.id,
        account: r.account,
        name: r.name.as_str().to_owned(),
        kind: r.kind,
        created_ms: r.created_ms,
        deleted: r.deleted,
        cell: r.cell,
        world: r.world,
        position: [r.x, r.y, r.z],
        level: r.level,
    }
}

/// Account and character rows per page of a read.
pub const RECORD_PAGE: usize = 32;

/// One ledger row as stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoredLedger {
    /// Month partition, YYYYMM.
    pub month: u32,
    /// Cell.
    pub cell: u64,
    /// Tick.
    pub tick: u64,
    /// Character.
    pub character: u64,
    /// Item (0 gold).
    pub item: u32,
    /// Change.
    pub delta: i64,
    /// Unix milliseconds.
    pub at_ms: u64,
}

/// One Ops audit row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuditRow {
    /// Id (assigned by the store).
    pub id: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// Who.
    pub actor: String,
    /// The command.
    pub command: String,
    /// Its arguments.
    pub args: String,
    /// `begun`, `done`, or `failed`.
    pub status: String,
    /// State before (set on completion).
    pub before: Option<String>,
    /// State after.
    pub after: Option<String>,
    /// How to undo it, or why there is no undo.
    pub undo: Option<String>,
}

/// A store failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

/// The system of record: [`memory::MemoryStore`] in every run,
/// [`pg::PgStore`] in production and when `MANTIS_TEST_POSTGRES` is set.
pub trait LedgerStore: Send {
    /// Applies pending migrations; returns the versions applied.
    ///
    /// # Errors
    /// [`StoreError`], including a refused (modified) migration.
    fn migrate(&mut self) -> Result<Vec<u32>, StoreError>;
    /// Creates the partition for `month` (YYYYMM) if missing.
    ///
    /// # Errors
    /// [`StoreError`].
    fn ensure_month(&mut self, month: u32) -> Result<(), StoreError>;
    /// The month partitions that exist.
    ///
    /// # Errors
    /// [`StoreError`].
    fn months(&mut self) -> Result<Vec<u32>, StoreError>;
    /// The cell's last durable batch.
    ///
    /// # Errors
    /// [`StoreError`].
    fn last_batch(&mut self, cell: u64) -> Result<Option<u64>, StoreError>;
    /// Writes one batch atomically: outcomes, ledger rows, watermark.
    ///
    /// # Errors
    /// [`StoreError`]; nothing is written.
    fn write_batch(
        &mut self,
        cell: u64,
        batch: u64,
        outcomes: &[StoredOutcome],
        ledger: &[StoredLedger],
    ) -> Result<(), StoreError>;
    /// A character's ledger rows, oldest first.
    ///
    /// # Errors
    /// [`StoreError`].
    fn ledger_of(&mut self, character: u64) -> Result<Vec<StoredLedger>, StoreError>;
    /// Stored outcomes of a cell, in log order.
    ///
    /// # Errors
    /// [`StoreError`].
    fn outcomes_of(&mut self, cell: u64) -> Result<Vec<StoredOutcome>, StoreError>;
    /// Writes an audit row with status `begun`; returns its id.
    ///
    /// # Errors
    /// [`StoreError`].
    fn audit_begin(&mut self, actor: &str, command: &str, args: &str, at_ms: u64) -> Result<u64, StoreError>;
    /// Completes an audit row.
    ///
    /// # Errors
    /// [`StoreError`].
    fn audit_complete(
        &mut self,
        id: u64,
        status: &str,
        before: &str,
        after: &str,
        undo: &str,
    ) -> Result<(), StoreError>;
    /// Every audit row, oldest first.
    ///
    /// # Errors
    /// [`StoreError`].
    fn audit_rows(&mut self) -> Result<Vec<AuditRow>, StoreError>;
    /// The last applied batch of guild changes (0 before the first).
    ///
    /// # Errors
    /// [`StoreError`].
    fn guild_seq(&mut self) -> Result<u64, StoreError>;
    /// Applies one batch of guild changes atomically, with its sequence
    /// number as the new watermark. The caller skips a batch at or below
    /// the watermark.
    ///
    /// # Errors
    /// [`StoreError`]; nothing is written.
    fn write_guilds(&mut self, seq: u64, changes: &[GuildChange]) -> Result<(), StoreError>;
    /// Every guild row: each guild, then its members in join order, guilds
    /// in id order.
    ///
    /// # Errors
    /// [`StoreError`].
    fn guild_rows(&mut self) -> Result<Vec<GuildChange>, StoreError>;
    /// The last applied batch of friend changes (0 before the first).
    ///
    /// # Errors
    /// [`StoreError`].
    fn friend_seq(&mut self) -> Result<u64, StoreError>;
    /// Applies one batch of friend changes atomically, with its sequence
    /// number as the new watermark. The caller skips a batch at or below
    /// the watermark.
    ///
    /// # Errors
    /// [`StoreError`]; nothing is written.
    fn write_friends(&mut self, seq: u64, changes: &[FriendChange]) -> Result<(), StoreError>;
    /// Every friend row: each friendship once (lower id first), then each
    /// open request.
    ///
    /// # Errors
    /// [`StoreError`].
    fn friend_rows(&mut self) -> Result<Vec<FriendChange>, StoreError>;
    /// The last applied batch of account rows (0 before the first).
    ///
    /// # Errors
    /// [`StoreError`].
    fn account_seq(&mut self) -> Result<u64, StoreError>;
    /// Writes one batch of whole account rows atomically (each replaces the
    /// row of its id), with its sequence number as the new watermark. The
    /// caller skips a batch at or below the watermark.
    ///
    /// # Errors
    /// [`StoreError`], also for a name another account holds
    /// case-insensitively; nothing is written.
    fn write_accounts(&mut self, seq: u64, rows: &[AccountRecord]) -> Result<(), StoreError>;
    /// Every account row, in id order.
    ///
    /// # Errors
    /// [`StoreError`].
    fn account_rows(&mut self) -> Result<Vec<AccountRecord>, StoreError>;
    /// The last applied batch of character rows (0 before the first).
    ///
    /// # Errors
    /// [`StoreError`].
    fn character_seq(&mut self) -> Result<u64, StoreError>;
    /// Writes one batch of whole character rows atomically (each replaces
    /// the row of its id), with its sequence number as the new watermark.
    /// The caller skips a batch at or below the watermark.
    ///
    /// # Errors
    /// [`StoreError`], also for a name another living character holds
    /// case-insensitively; nothing is written.
    fn write_characters(&mut self, seq: u64, rows: &[CharacterRecord]) -> Result<(), StoreError>;
    /// Every character row, deleted ones included, in id order.
    ///
    /// # Errors
    /// [`StoreError`].
    fn character_rows(&mut self) -> Result<Vec<CharacterRecord>, StoreError>;
    /// Sets live value `name` (a flag or tunable), replacing any earlier
    /// value of it.
    ///
    /// # Errors
    /// [`StoreError`]; nothing is written.
    fn set_live(&mut self, name: &str, kind: u8, value: f32) -> Result<(), StoreError>;
    /// Every live value: (name, kind, value), in the order last set.
    ///
    /// # Errors
    /// [`StoreError`].
    fn live_values(&mut self) -> Result<Vec<(String, u8, f32)>, StoreError>;
}

/// The month (YYYYMM, UTC) of a Unix time in milliseconds.
#[must_use]
pub fn month_of(at_ms: u64) -> u32 {
    // Civil-from-days (H. Hinnant), integer only.
    let days = i64::try_from(at_ms / 86_400_000).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    u32::try_from(year * 100 + month).unwrap_or(0)
}

/// The month after `month` (YYYYMM).
#[must_use]
pub fn next_month(month: u32) -> u32 {
    if month % 100 == 12 {
        (month / 100 + 1) * 100 + 1
    } else {
        month + 1
    }
}

/// Guild rows per page of [`PersistService::load_guilds`].
pub const GUILD_PAGE: usize = 256;

/// A guild change as the wire carries it.
#[must_use]
pub fn guild_row(c: &GuildChange) -> m::GuildRow {
    let row = |kind, guild, character, rank, since, name: &str| m::GuildRow {
        kind,
        guild,
        character,
        rank,
        since,
        name: mantis_core::wire::WireString::new(name).unwrap_or_default(),
    };
    match c {
        GuildChange::Guild { id, name } => row(1, *id, 0, 0, 0, name),
        GuildChange::NoGuild { id } => row(2, *id, 0, 0, 0, ""),
        GuildChange::Member {
            guild,
            character,
            rank,
            since,
        } => row(3, *guild, *character, *rank, *since, ""),
        GuildChange::NoMember { guild, character } => row(4, *guild, *character, 0, 0, ""),
    }
}

/// A guild change back from the wire (`None` for an unknown kind or rank).
#[must_use]
pub fn guild_change(r: &m::GuildRow) -> Option<GuildChange> {
    Some(match r.kind {
        1 => GuildChange::Guild {
            id: r.guild,
            name: r.name.as_str().to_owned(),
        },
        2 => GuildChange::NoGuild { id: r.guild },
        3 if r.rank <= mantis_core::social::guild_rank::MEMBER => GuildChange::Member {
            guild: r.guild,
            character: r.character,
            rank: r.rank,
            since: r.since,
        },
        4 => GuildChange::NoMember {
            guild: r.guild,
            character: r.character,
        },
        _ => return None,
    })
}

/// `text` cut to at most `N` bytes at a character boundary (audit text
/// longer than its wire field keeps its head).
#[must_use]
pub fn clip<const N: usize>(text: &str) -> WireString<N> {
    let mut end = text.len().min(N);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    WireString::new(text.get(..end).unwrap_or("")).unwrap_or_default()
}

/// An audit row as the wire carries it.
#[must_use]
pub fn audit_entry(r: &AuditRow) -> m::AuditEntry {
    let text = |v: &Option<String>| clip(v.as_deref().unwrap_or(""));
    m::AuditEntry {
        id: r.id,
        at_ms: r.at_ms,
        actor: clip(&r.actor),
        command: clip(&r.command),
        args: clip(&r.args),
        status: clip(&r.status),
        before: text(&r.before),
        after: text(&r.after),
        undo: text(&r.undo),
    }
}

/// An audit row back from the wire: before, after, and undo are absent
/// while the row is `begun`.
#[must_use]
pub fn audit_row(e: &m::AuditEntry) -> AuditRow {
    let open = e.status.as_str() == "begun";
    let text = |w: &str| (!open).then(|| w.to_owned());
    AuditRow {
        id: e.id,
        at_ms: e.at_ms,
        actor: e.actor.as_str().to_owned(),
        command: e.command.as_str().to_owned(),
        args: e.args.as_str().to_owned(),
        status: e.status.as_str().to_owned(),
        before: text(e.before.as_str()),
        after: text(e.after.as_str()),
        undo: text(e.undo.as_str()),
    }
}

/// Friend rows per page of [`PersistService::load_friends`].
pub const FRIEND_PAGE: usize = 256;

/// A friend change as the wire carries it.
#[must_use]
pub fn friend_row(c: &FriendChange) -> m::FriendRow {
    let (kind, a, b) = match *c {
        FriendChange::Friends { a, b } => (1, a, b),
        FriendChange::NoFriends { a, b } => (2, a, b),
        FriendChange::Asked { asker, asked } => (3, asker, asked),
        FriendChange::NoAsk { asker, asked } => (4, asker, asked),
    };
    m::FriendRow { kind, a, b }
}

/// A friend change back from the wire (`None` for an unknown kind, or a
/// friendship not stored lower id first).
#[must_use]
pub fn friend_change(r: &m::FriendRow) -> Option<FriendChange> {
    let (a, b) = (r.a, r.b);
    Some(match r.kind {
        1 if a < b => FriendChange::Friends { a, b },
        2 if a < b => FriendChange::NoFriends { a, b },
        3 => FriendChange::Asked { asker: a, asked: b },
        4 => FriendChange::NoAsk { asker: a, asked: b },
        _ => return None,
    })
}

/// The persistence writer role.
#[derive(Clone)]
pub struct PersistService {
    store: Arc<Mutex<Box<dyn LedgerStore>>>,
}

fn store_err(e: &StoreError) -> RpcError {
    RpcError::Refused(e.0.clone())
}

impl PersistService {
    /// A writer over `store`: migrates it and creates this month and the
    /// next ahead of need.
    ///
    /// # Errors
    /// [`StoreError`]: the writer must not start.
    pub fn new(mut store: Box<dyn LedgerStore>, now_ms: u64) -> Result<Self, StoreError> {
        store.migrate()?;
        let month = month_of(now_ms);
        store.ensure_month(month)?;
        store.ensure_month(next_month(month))?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
        })
    }

    /// Runs `f` with the store (Ops and tests).
    ///
    /// On a multi-thread runtime the wait for the store and the work under
    /// it run in `block_in_place`: a handler waiting for another's query
    /// never pins a worker, so the database connection's task always has a
    /// worker to run on (otherwise concurrent pushes to a PostgreSQL store
    /// deadlock once every worker waits on the lock).
    pub fn with_store<R>(&self, f: impl FnOnce(&mut dyn LedgerStore) -> R) -> R {
        let run = || {
            let mut s = crate::host::lock(&self.store);
            f(s.as_mut())
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(run)
            }
            _ => run(),
        }
    }

    /// Makes one batch of guild changes durable (once per sequence number).
    ///
    /// # Errors
    /// [`RpcError::Refused`] for a malformed row or a store failure (social
    /// retries the same batch).
    pub fn write_guilds(&self, req: &m::StoreGuildRows) -> Result<m::Durable, RpcError> {
        let changes = req
            .rows
            .iter()
            .map(guild_change)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| RpcError::Refused("malformed guild row".to_owned()))?;
        self.with_store(|store| {
            let last = store.guild_seq().map_err(|e| store_err(&e))?;
            if req.seq <= last {
                return Ok(m::Durable { seq: last });
            }
            store.write_guilds(req.seq, &changes).map_err(|e| store_err(&e))?;
            Ok(m::Durable { seq: req.seq })
        })
    }

    /// Makes one batch of friend changes durable (once per sequence number).
    ///
    /// # Errors
    /// [`RpcError::Refused`] for a malformed row or a store failure (social
    /// retries the same batch).
    pub fn write_friends(&self, req: &m::StoreFriendRows) -> Result<m::Durable, RpcError> {
        let changes = req
            .rows
            .iter()
            .map(friend_change)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| RpcError::Refused("malformed friend row".to_owned()))?;
        self.with_store(|store| {
            let last = store.friend_seq().map_err(|e| store_err(&e))?;
            if req.seq <= last {
                return Ok(m::Durable { seq: last });
            }
            store
                .write_friends(req.seq, &changes)
                .map_err(|e| store_err(&e))?;
            Ok(m::Durable { seq: req.seq })
        })
    }

    /// Makes one batch of account rows durable (once per sequence number).
    ///
    /// # Errors
    /// [`RpcError::Refused`] for a malformed row or a store failure (the
    /// account role retries the same batch).
    pub fn write_accounts(&self, req: &m::StoreAccountRows) -> Result<m::Durable, RpcError> {
        let rows = req
            .rows
            .iter()
            .map(account_record)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| RpcError::Refused("malformed account row".to_owned()))?;
        self.with_store(|store| {
            let last = store.account_seq().map_err(|e| store_err(&e))?;
            if req.seq <= last {
                return Ok(m::Durable { seq: last });
            }
            store.write_accounts(req.seq, &rows).map_err(|e| store_err(&e))?;
            Ok(m::Durable { seq: req.seq })
        })
    }

    /// One page of the account rows, with the stored watermark.
    ///
    /// # Errors
    /// [`RpcError::Refused`] when the store fails.
    pub fn load_accounts(&self, page: u32) -> Result<m::AccountRows, RpcError> {
        let (seq, rows) = self
            .with_store(|store| Ok::<_, StoreError>((store.account_seq()?, store.account_rows()?)))
            .map_err(|e| store_err(&e))?;
        let start = usize::try_from(page)
            .unwrap_or(usize::MAX)
            .saturating_mul(RECORD_PAGE);
        let rows: Vec<m::AccountRow> = rows
            .iter()
            .skip(start)
            .take(RECORD_PAGE)
            .map(account_row)
            .collect();
        let more = rows.len() == RECORD_PAGE;
        Ok(m::AccountRows {
            seq,
            rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            more,
        })
    }

    /// Makes one batch of character rows durable (once per sequence number).
    ///
    /// # Errors
    /// [`RpcError::Refused`] for a store failure (the realm retries the
    /// same batch).
    pub fn write_characters(&self, req: &m::StoreCharacterRows) -> Result<m::Durable, RpcError> {
        let rows: Vec<CharacterRecord> = req.rows.iter().map(character_record).collect();
        self.with_store(|store| {
            let last = store.character_seq().map_err(|e| store_err(&e))?;
            if req.seq <= last {
                return Ok(m::Durable { seq: last });
            }
            store
                .write_characters(req.seq, &rows)
                .map_err(|e| store_err(&e))?;
            Ok(m::Durable { seq: req.seq })
        })
    }

    /// One page of the character rows, with the stored watermark.
    ///
    /// # Errors
    /// [`RpcError::Refused`] when the store fails.
    pub fn load_characters(&self, page: u32) -> Result<m::CharacterRows, RpcError> {
        let (seq, rows) = self
            .with_store(|store| Ok::<_, StoreError>((store.character_seq()?, store.character_rows()?)))
            .map_err(|e| store_err(&e))?;
        let start = usize::try_from(page)
            .unwrap_or(usize::MAX)
            .saturating_mul(RECORD_PAGE);
        let rows: Vec<m::CharacterRow> = rows
            .iter()
            .skip(start)
            .take(RECORD_PAGE)
            .map(character_row)
            .collect();
        let more = rows.len() == RECORD_PAGE;
        Ok(m::CharacterRows {
            seq,
            rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            more,
        })
    }

    /// One page of the friend rows, with the stored watermark.
    ///
    /// # Errors
    /// [`RpcError::Refused`] when the store fails.
    pub fn load_friends(&self, page: u32) -> Result<m::FriendRows, RpcError> {
        let (seq, rows) = self
            .with_store(|store| Ok::<_, StoreError>((store.friend_seq()?, store.friend_rows()?)))
            .map_err(|e| store_err(&e))?;
        let start = usize::try_from(page)
            .unwrap_or(usize::MAX)
            .saturating_mul(FRIEND_PAGE);
        let rows: Vec<m::FriendRow> = rows
            .iter()
            .skip(start)
            .take(FRIEND_PAGE)
            .map(friend_row)
            .collect();
        let more = rows.len() == FRIEND_PAGE;
        Ok(m::FriendRows {
            seq,
            rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            more,
        })
    }

    /// One page of the guild rows, with the stored watermark.
    ///
    /// # Errors
    /// [`RpcError::Refused`] when the store fails.
    pub fn load_guilds(&self, page: u32) -> Result<m::GuildRows, RpcError> {
        let (seq, rows) = self
            .with_store(|store| Ok::<_, StoreError>((store.guild_seq()?, store.guild_rows()?)))
            .map_err(|e| store_err(&e))?;
        let start = usize::try_from(page)
            .unwrap_or(usize::MAX)
            .saturating_mul(GUILD_PAGE);
        let rows: Vec<m::GuildRow> = rows.iter().skip(start).take(GUILD_PAGE).map(guild_row).collect();
        let more = rows.len() == GUILD_PAGE;
        Ok(m::GuildRows {
            seq,
            rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            more,
        })
    }

    /// Handles one pushed batch.
    ///
    /// # Errors
    /// [`RpcError::Refused`] when the store fails (the cell retries).
    pub fn push(&self, req: &m::PushOutcomes) -> Result<m::Durable, RpcError> {
        let cell = req.cell.0;
        self.with_store(|store| {
            let last = store.last_batch(cell).map_err(|e| store_err(&e))?;
            if last.is_some_and(|l| req.seq <= l) {
                return Ok(m::Durable {
                    seq: last.unwrap_or(req.seq),
                });
            }
            let mut outcomes = Vec::new();
            let mut ledger = Vec::new();
            for row in req.rows.iter() {
                let payload: Vec<u8> = row.payload.iter().copied().collect();
                if row.ok
                    && let Some(l) = ledger_of(&payload)
                {
                    let month = month_of(row.at_ms);
                    for r in l.rows() {
                        ledger.push(StoredLedger {
                            month,
                            cell,
                            tick: row.tick,
                            character: r.character,
                            item: r.item,
                            delta: r.delta,
                            at_ms: row.at_ms,
                        });
                    }
                }
                outcomes.push(StoredOutcome {
                    tick: row.tick,
                    at_ms: row.at_ms,
                    kind: row.kind,
                    session: row.session,
                    ok: row.ok,
                    payload,
                });
            }
            let mut months: Vec<u32> = ledger.iter().map(|l| l.month).collect();
            months.sort_unstable();
            months.dedup();
            for month in months {
                store.ensure_month(month).map_err(|e| store_err(&e))?;
            }
            store
                .write_batch(cell, req.seq, &outcomes, &ledger)
                .map_err(|e| store_err(&e))?;
            Ok(m::Durable { seq: req.seq })
        })
    }

    /// The guild, friend, account and character rows' methods.
    fn serve_records(&self, r: &mut Router) {
        let me = self.clone();
        r.serve::<methods::WriteGuilds>(move |_, req| me.write_guilds(&req));
        let me = self.clone();
        r.serve::<methods::LoadGuilds>(move |_, req| me.load_guilds(req.page));
        let me = self.clone();
        r.serve::<methods::WriteFriends>(move |_, req| me.write_friends(&req));
        let me = self.clone();
        r.serve::<methods::LoadFriends>(move |_, req| me.load_friends(req.page));
        let me = self.clone();
        r.serve::<methods::WriteAccounts>(move |_, req| me.write_accounts(&req));
        let me = self.clone();
        r.serve::<methods::LoadAccounts>(move |_, req| me.load_accounts(req.page));
        let me = self.clone();
        r.serve::<methods::WriteCharacters>(move |_, req| me.write_characters(&req));
        let me = self.clone();
        r.serve::<methods::LoadCharacters>(move |_, req| me.load_characters(req.page));
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve::<methods::Push>(move |_, req| me.push(&req));
        self.serve_records(&mut r);
        let me = self.clone();
        r.serve::<methods::StoreLiveValue>(move |_, req| {
            me.with_store(|s| s.set_live(req.name.as_str(), req.kind, req.value))
                .map_err(|e| store_err(&e))?;
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::ReadLiveValues>(move |_, _| {
            let values = me.with_store(|s| s.live_values()).map_err(|e| store_err(&e))?;
            let values: Vec<m::LiveValue> = values
                .iter()
                .map(|(name, kind, value)| m::LiveValue {
                    name: clip(name),
                    kind: *kind,
                    value: *value,
                })
                .collect();
            Ok(m::LiveValues {
                values: BoundedArray::from_slice(&values).unwrap_or_default(),
            })
        });
        let me = self.clone();
        r.serve::<methods::AuditOpen>(move |_, req| {
            let id = me
                .with_store(|s| {
                    s.audit_begin(
                        req.actor.as_str(),
                        req.command.as_str(),
                        req.args.as_str(),
                        req.at_ms,
                    )
                })
                .map_err(|e| store_err(&e))?;
            Ok(m::AuditId { id })
        });
        let me = self.clone();
        r.serve::<methods::AuditClose>(move |_, req| {
            me.with_store(|s| {
                s.audit_complete(
                    req.id,
                    req.status.as_str(),
                    req.before.as_str(),
                    req.after.as_str(),
                    req.undo.as_str(),
                )
            })
            .map_err(|e| store_err(&e))?;
            Ok(m::Empty {})
        });
        let me = self.clone();
        r.serve::<methods::AuditTrail>(move |_, req| {
            let rows = me.with_store(|s| s.audit_rows()).map_err(|e| store_err(&e))?;
            let mut page: Vec<m::AuditEntry> = rows
                .iter()
                .filter(|r| r.id > req.since)
                .take(usize::from(req.limit) + 1)
                .map(audit_entry)
                .collect();
            let more = page.len() > usize::from(req.limit);
            page.truncate(usize::from(req.limit));
            Ok(m::AuditRows {
                rows: BoundedArray::from_slice(&page).unwrap_or_default(),
                more,
            })
        });
        let me = self.clone();
        r.serve::<methods::Ledger>(move |_, req| {
            let rows = me
                .with_store(|s| s.ledger_of(req.character.0))
                .map_err(|e| store_err(&e))?;
            let rows: Vec<m::LedgerEntry> = rows
                .iter()
                .rev()
                .take(64)
                .rev()
                .map(|l| m::LedgerEntry {
                    month: l.month,
                    cell: m::CellNo(l.cell),
                    tick: l.tick,
                    item: l.item,
                    delta: l.delta,
                    at_ms: l.at_ms,
                })
                .collect();
            Ok(m::LedgerRows {
                rows: BoundedArray::from_slice(&rows).unwrap_or_default(),
            })
        });
        r
    }
}
