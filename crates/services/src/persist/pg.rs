//! The PostgreSQL store (decision 0005). Calls run on the service runtime:
//! the store is used from synchronous RPC handlers, so each call blocks the
//! current worker thread in place while it waits for Postgres.

use tokio_postgres::{Client, NoTls};

use super::migrate::{MIGRATIONS, Migration, MigrationTarget, migrate};
use mantis_core::social::{FriendChange, GuildChange};

use super::{AuditRow, LedgerStore, StoreError, StoredLedger, StoredOutcome};

/// A store over one Postgres connection, in one schema.
pub struct PgStore {
    client: Client,
    runtime: tokio::runtime::Handle,
}

fn pg(e: &tokio_postgres::Error) -> StoreError {
    StoreError(format!("postgres: {e}"))
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn i32_of(v: u32) -> i32 {
    v.cast_signed()
}

fn u32_of(v: i32) -> u32 {
    v.cast_unsigned()
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl PgStore {
    /// Connects with `config` (a libpq connection string) and works in
    /// `schema` (created if missing), so tests can isolate themselves.
    ///
    /// # Errors
    /// [`StoreError`].
    pub async fn connect(config: &str, schema: &str) -> Result<Self, StoreError> {
        let (client, connection) = tokio_postgres::connect(config, NoTls).await.map_err(|e| pg(&e))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let quoted = format!("\"{}\"", schema.replace('"', ""));
        client
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {quoted}; SET search_path TO {quoted};"
            ))
            .await
            .map_err(|e| pg(&e))?;
        Ok(Self {
            client,
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Drops `schema` and everything in it (test cleanup).
    ///
    /// # Errors
    /// [`StoreError`].
    pub async fn drop_schema(&self, schema: &str) -> Result<(), StoreError> {
        let quoted = format!("\"{}\"", schema.replace('"', ""));
        self.client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {quoted} CASCADE"))
            .await
            .map_err(|e| pg(&e))
    }

    fn block<T>(&self, f: impl std::future::Future<Output = T>) -> T {
        tokio::task::block_in_place(|| self.runtime.block_on(f))
    }
}

impl MigrationTarget for PgStore {
    fn applied(&mut self) -> Result<Vec<(u32, String)>, String> {
        self.block(async {
            self.client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL)",
                )
                .await
                .map_err(|e| e.to_string())?;
            let rows = self
                .client
                .query("SELECT version, checksum FROM schema_migrations ORDER BY version", &[])
                .await
                .map_err(|e| e.to_string())?;
            Ok(rows
                .iter()
                .map(|r| (u32::try_from(r.get::<_, i32>(0)).unwrap_or(0), r.get::<_, String>(1)))
                .collect())
        })
    }

    fn apply(&mut self, m: &Migration, checksum: &str) -> Result<(), String> {
        let client = &mut self.client;
        let rt = self.runtime.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let tx = client.transaction().await.map_err(|e| e.to_string())?;
                tx.batch_execute(m.sql).await.map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
                    &[&i32::try_from(m.version).unwrap_or(i32::MAX), &m.name, &checksum],
                )
                .await
                .map_err(|e| e.to_string())?;
                tx.commit().await.map_err(|e| e.to_string())
            })
        })
    }
}

impl LedgerStore for PgStore {
    fn migrate(&mut self) -> Result<Vec<u32>, StoreError> {
        migrate(self, MIGRATIONS).map_err(|e| StoreError(e.to_string()))
    }

    fn ensure_month(&mut self, month: u32) -> Result<(), StoreError> {
        let m = i32::try_from(month).unwrap_or(0);
        self.block(self.client.execute("SELECT ensure_ledger_partition($1)", &[&m]))
            .map(|_| ())
            .map_err(|e| pg(&e))
    }

    fn months(&mut self) -> Result<Vec<u32>, StoreError> {
        let rows = self
            .block(self.client.query(
                "SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
                 JOIN pg_class p ON p.oid = i.inhparent JOIN pg_namespace n ON n.oid = p.relnamespace \
                 WHERE p.relname = 'ledger' AND n.nspname = current_schema() ORDER BY c.relname",
                &[],
            ))
            .map_err(|e| pg(&e))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                r.get::<_, String>(0)
                    .strip_prefix("ledger_")
                    .and_then(|m| m.parse().ok())
            })
            .collect())
    }

    fn last_batch(&mut self, cell: u64) -> Result<Option<u64>, StoreError> {
        let rows = self
            .block(
                self.client
                    .query("SELECT last_batch FROM batches WHERE cell = $1", &[&i64_of(cell)]),
            )
            .map_err(|e| pg(&e))?;
        Ok(rows.first().map(|r| u64_of(r.get::<_, i64>(0))))
    }

    fn write_batch(
        &mut self,
        cell: u64,
        batch: u64,
        outcomes: &[StoredOutcome],
        ledger: &[StoredLedger],
    ) -> Result<(), StoreError> {
        let client = &mut self.client;
        let rt = self.runtime.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let tx = client.transaction().await?;
                for (i, o) in outcomes.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO outcomes (cell, batch, idx, tick, at_ms, kind, session, ok, payload) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                        &[
                            &i64_of(cell),
                            &i64_of(batch),
                            &i32::try_from(i).unwrap_or(i32::MAX),
                            &i64_of(o.tick),
                            &i64_of(o.at_ms),
                            &i32::from(o.kind),
                            &i64_of(o.session),
                            &o.ok,
                            &o.payload,
                        ],
                    )
                    .await?;
                }
                for l in ledger {
                    tx.execute(
                        "INSERT INTO ledger (month, cell, tick, character, item, delta, at_ms) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                        &[
                            &i32::try_from(l.month).unwrap_or(0),
                            &i64_of(l.cell),
                            &i64_of(l.tick),
                            &i64_of(l.character),
                            &i64::from(l.item),
                            &l.delta,
                            &i64_of(l.at_ms),
                        ],
                    )
                    .await?;
                }
                tx.execute(
                    "INSERT INTO batches (cell, last_batch) VALUES ($1, $2) \
                     ON CONFLICT (cell) DO UPDATE SET last_batch = EXCLUDED.last_batch",
                    &[&i64_of(cell), &i64_of(batch)],
                )
                .await?;
                tx.commit().await
            })
        })
        .map_err(|e| pg(&e))
    }

    fn ledger_of(&mut self, character: u64) -> Result<Vec<StoredLedger>, StoreError> {
        let rows = self
            .block(self.client.query(
                "SELECT month, cell, tick, character, item, delta, at_ms FROM ledger \
                 WHERE character = $1 ORDER BY at_ms, cell, tick",
                &[&i64_of(character)],
            ))
            .map_err(|e| pg(&e))?;
        Ok(rows
            .iter()
            .map(|r| StoredLedger {
                month: u32::try_from(r.get::<_, i32>(0)).unwrap_or(0),
                cell: u64_of(r.get(1)),
                tick: u64_of(r.get(2)),
                character: u64_of(r.get(3)),
                item: u32::try_from(r.get::<_, i64>(4)).unwrap_or(0),
                delta: r.get(5),
                at_ms: u64_of(r.get(6)),
            })
            .collect())
    }

    fn outcomes_of(&mut self, cell: u64) -> Result<Vec<StoredOutcome>, StoreError> {
        let rows = self
            .block(self.client.query(
                "SELECT tick, at_ms, kind, session, ok, payload FROM outcomes WHERE cell = $1 ORDER BY batch, idx",
                &[&i64_of(cell)],
            ))
            .map_err(|e| pg(&e))?;
        Ok(rows
            .iter()
            .map(|r| StoredOutcome {
                tick: u64_of(r.get(0)),
                at_ms: u64_of(r.get(1)),
                kind: u16::try_from(r.get::<_, i32>(2)).unwrap_or(0),
                session: u64_of(r.get(3)),
                ok: r.get(4),
                payload: r.get(5),
            })
            .collect())
    }

    fn audit_begin(&mut self, actor: &str, command: &str, args: &str, at_ms: u64) -> Result<u64, StoreError> {
        let row = self
            .block(self.client.query_one(
                "INSERT INTO ops_audit (at_ms, actor, command, args, status) VALUES ($1, $2, $3, $4, 'begun') RETURNING id",
                &[&i64_of(at_ms), &actor, &command, &args],
            ))
            .map_err(|e| pg(&e))?;
        Ok(u64_of(row.get(0)))
    }

    fn audit_complete(
        &mut self,
        id: u64,
        status: &str,
        before: &str,
        after: &str,
        undo: &str,
    ) -> Result<(), StoreError> {
        self.block(self.client.execute(
            "UPDATE ops_audit SET status = $2, before = $3, after = $4, undo = $5 WHERE id = $1",
            &[&i64_of(id), &status, &before, &after, &undo],
        ))
        .map(|_| ())
        .map_err(|e| pg(&e))
    }

    fn audit_rows(&mut self) -> Result<Vec<AuditRow>, StoreError> {
        let rows = self
            .block(self.client.query(
                "SELECT id, at_ms, actor, command, args, status, before, after, undo FROM ops_audit ORDER BY id",
                &[],
            ))
            .map_err(|e| pg(&e))?;
        Ok(rows
            .iter()
            .map(|r| AuditRow {
                id: u64_of(r.get(0)),
                at_ms: u64_of(r.get(1)),
                actor: r.get(2),
                command: r.get(3),
                args: r.get(4),
                status: r.get(5),
                before: r.get(6),
                after: r.get(7),
                undo: r.get(8),
            })
            .collect())
    }

    fn guild_seq(&mut self) -> Result<u64, StoreError> {
        let rows = self
            .block(
                self.client
                    .query("SELECT seq FROM guild_watermark WHERE id = 1", &[]),
            )
            .map_err(|e| pg(&e))?;
        Ok(rows.first().map_or(0, |r| u64_of(r.get::<_, i64>(0))))
    }

    fn write_guilds(&mut self, seq: u64, changes: &[GuildChange]) -> Result<(), StoreError> {
        let client = &mut self.client;
        let rt = self.runtime.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let tx = client.transaction().await?;
                for c in changes {
                    match c {
                        GuildChange::Guild { id, name } => {
                            tx.execute(
                                "INSERT INTO guilds (id, name) VALUES ($1, $2) \
                                 ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                                &[&i32_of(*id), name],
                            )
                            .await?;
                        }
                        GuildChange::NoGuild { id } => {
                            tx.execute("DELETE FROM guilds WHERE id = $1", &[&i32_of(*id)])
                                .await?;
                        }
                        GuildChange::Member {
                            guild,
                            character,
                            rank,
                            since,
                        } => {
                            tx.execute(
                                "INSERT INTO guild_members (character, guild, rank, since) VALUES ($1, $2, $3, $4) \
                                 ON CONFLICT (character) DO UPDATE SET guild = EXCLUDED.guild, \
                                 rank = EXCLUDED.rank, since = EXCLUDED.since",
                                &[&i64_of(*character), &i32_of(*guild), &i16::from(*rank), &i64_of(*since)],
                            )
                            .await?;
                        }
                        GuildChange::NoMember { guild, character } => {
                            tx.execute(
                                "DELETE FROM guild_members WHERE character = $1 AND guild = $2",
                                &[&i64_of(*character), &i32_of(*guild)],
                            )
                            .await?;
                        }
                    }
                }
                tx.execute(
                    "INSERT INTO guild_watermark (id, seq) VALUES (1, $1) \
                     ON CONFLICT (id) DO UPDATE SET seq = EXCLUDED.seq",
                    &[&i64_of(seq)],
                )
                .await?;
                tx.commit().await
            })
        })
        .map_err(|e| pg(&e))
    }

    fn guild_rows(&mut self) -> Result<Vec<GuildChange>, StoreError> {
        let guilds = self
            .block(self.client.query("SELECT id, name FROM guilds ORDER BY id", &[]))
            .map_err(|e| pg(&e))?;
        let members = self
            .block(self.client.query(
                "SELECT guild, character, rank, since FROM guild_members ORDER BY guild, since, character",
                &[],
            ))
            .map_err(|e| pg(&e))?;
        let mut out = Vec::with_capacity(guilds.len() + members.len());
        let mut members = members.iter().peekable();
        for g in &guilds {
            let id = u32_of(g.get::<_, i32>(0));
            out.push(GuildChange::Guild {
                id,
                name: g.get::<_, String>(1),
            });
            while let Some(m) = members.next_if(|m| u32_of(m.get::<_, i32>(0)) == id) {
                out.push(GuildChange::Member {
                    guild: id,
                    character: u64_of(m.get::<_, i64>(1)),
                    rank: u8::try_from(m.get::<_, i16>(2)).unwrap_or(u8::MAX),
                    since: u64_of(m.get::<_, i64>(3)),
                });
            }
        }
        Ok(out)
    }

    fn friend_seq(&mut self) -> Result<u64, StoreError> {
        let rows = self
            .block(
                self.client
                    .query("SELECT seq FROM friend_watermark WHERE id = 1", &[]),
            )
            .map_err(|e| pg(&e))?;
        Ok(rows.first().map_or(0, |r| u64_of(r.get::<_, i64>(0))))
    }

    fn write_friends(&mut self, seq: u64, changes: &[FriendChange]) -> Result<(), StoreError> {
        let client = &mut self.client;
        let rt = self.runtime.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let tx = client.transaction().await?;
                for c in changes {
                    let (sql, x, y) = match *c {
                        FriendChange::Friends { a, b } => (
                            "INSERT INTO friendships (a, b) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                            a,
                            b,
                        ),
                        FriendChange::NoFriends { a, b } => ("DELETE FROM friendships WHERE a = $1 AND b = $2", a, b),
                        FriendChange::Asked { asker, asked } => (
                            "INSERT INTO friend_requests (asker, asked) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                            asker,
                            asked,
                        ),
                        FriendChange::NoAsk { asker, asked } => (
                            "DELETE FROM friend_requests WHERE asker = $1 AND asked = $2",
                            asker,
                            asked,
                        ),
                    };
                    tx.execute(sql, &[&i64_of(x), &i64_of(y)]).await?;
                }
                tx.execute(
                    "INSERT INTO friend_watermark (id, seq) VALUES (1, $1) \
                     ON CONFLICT (id) DO UPDATE SET seq = EXCLUDED.seq",
                    &[&i64_of(seq)],
                )
                .await?;
                tx.commit().await
            })
        })
        .map_err(|e| pg(&e))
    }

    fn friend_rows(&mut self) -> Result<Vec<FriendChange>, StoreError> {
        let friends = self
            .block(
                self.client
                    .query("SELECT a, b FROM friendships ORDER BY a, b", &[]),
            )
            .map_err(|e| pg(&e))?;
        let asks = self
            .block(self.client.query(
                "SELECT asker, asked FROM friend_requests ORDER BY asked, asker",
                &[],
            ))
            .map_err(|e| pg(&e))?;
        let pair = |r: &tokio_postgres::Row| (u64_of(r.get::<_, i64>(0)), u64_of(r.get::<_, i64>(1)));
        let mut out: Vec<FriendChange> = friends
            .iter()
            .map(|r| {
                let (a, b) = pair(r);
                FriendChange::Friends { a, b }
            })
            .collect();
        out.extend(asks.iter().map(|r| {
            let (from, to) = pair(r);
            FriendChange::Asked {
                asker: from,
                asked: to,
            }
        }));
        Ok(out)
    }

    fn set_live(&mut self, name: &str, kind: u8, value: f32) -> Result<(), StoreError> {
        self.block(self.client.execute(
            "INSERT INTO live_values (name, kind, value, seq) \
             VALUES ($1, $2, $3, (SELECT COALESCE(MAX(seq), 0) + 1 FROM live_values)) \
             ON CONFLICT (name) DO UPDATE SET kind = EXCLUDED.kind, value = EXCLUDED.value, seq = EXCLUDED.seq",
            &[&name, &i16::from(kind), &value],
        ))
        .map_err(|e| pg(&e))?;
        Ok(())
    }

    fn live_values(&mut self) -> Result<Vec<(String, u8, f32)>, StoreError> {
        let rows = self
            .block(
                self.client
                    .query("SELECT name, kind, value FROM live_values ORDER BY seq", &[]),
            )
            .map_err(|e| pg(&e))?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    u8::try_from(r.get::<_, i16>(1)).unwrap_or(u8::MAX),
                    r.get::<_, f32>(2),
                )
            })
            .collect())
    }
}

static SKIPPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The greppable prefix of every Postgres skip line.
pub const SKIP_MARKER: &str = "MANTIS-PG-SKIP";

/// The connection string in `MANTIS_TEST_POSTGRES`, or `None` after
/// recording and printing a skip: the Postgres path is never skipped
/// silently. Every skip prints one line,
/// `MANTIS-PG-SKIP test=<name> reason=<why>`, so a run summary can count
/// them with `cargo test 2>&1 | grep -c MANTIS-PG-SKIP`. The same tests
/// always run against [`super::memory::MemoryStore`].
#[must_use]
pub fn postgres_or_skip(test: &str) -> Option<String> {
    match std::env::var("MANTIS_TEST_POSTGRES") {
        Ok(config) if !config.trim().is_empty() => Some(config),
        _ => {
            SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Written to the process stderr directly, past the test
            // harness capture, so every run shows it.
            let line = format!(
                "{SKIP_MARKER} test={test} reason=MANTIS_TEST_POSTGRES is not set
"
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            None
        }
    }
}

/// Postgres skips recorded in this process.
#[must_use]
pub fn skipped() -> u32 {
    SKIPPED.load(std::sync::atomic::Ordering::Relaxed)
}
