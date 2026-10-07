//! The persistence writer and the migration runner, proven against the
//! in-memory store in every run and against Postgres when
//! `MANTIS_TEST_POSTGRES` is set (a counted, printed skip otherwise).
#![expect(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::Duration;

use mantis_core::ledger::{GOLD, Ledger, LedgerRow};
use mantis_core::wire::{BoundedArray, encode_into};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{RpcClient, RpcError, RpcServer};
use mantis_services::methods;
use mantis_services::persist::memory::MemoryStore;
use mantis_services::persist::migrate::{MIGRATIONS, Migration, checksum};
use mantis_services::persist::pg::{PgStore, postgres_or_skip};
use mantis_services::persist::{LedgerStore, PersistService, month_of, next_month};

/// 2023-11-14 (UTC).
const NOW: u64 = 1_700_000_000_000;
/// 2024-03-09 (UTC): a month with no partition yet.
const LATER: u64 = 1_710_000_000_000;

fn ledger_payload(rows: &[(u64, u32, i64)], tail: &[u8]) -> BoundedArray<u8, 512> {
    let mut l = Ledger::default();
    for &(character, item, delta) in rows {
        l.push(LedgerRow {
            character,
            item,
            delta,
        })
        .unwrap();
    }
    let mut bytes = Vec::new();
    encode_into(&l, &mut bytes);
    bytes.extend_from_slice(tail);
    BoundedArray::from_slice(&bytes).unwrap()
}

fn row(tick: u64, at_ms: u64, ok: bool, payload: &BoundedArray<u8, 512>) -> m::OutcomeRow {
    m::OutcomeRow {
        tick,
        at_ms,
        kind: 1050,
        session: 7,
        ok,
        payload: *payload,
    }
}

fn push(cell: u64, seq: u64, rows: &[m::OutcomeRow]) -> m::PushOutcomes {
    m::PushOutcomes {
        cell: m::CellNo(cell),
        seq,
        rows: BoundedArray::from_slice(rows).unwrap(),
    }
}

#[test]
fn months_are_civil_utc() {
    assert_eq!(month_of(0), 197_001);
    assert_eq!(month_of(NOW), 202_311);
    assert_eq!(month_of(951_782_400_000), 200_002, "a leap day");
    assert_eq!(month_of(1_704_067_199_999), 202_312);
    assert_eq!(month_of(1_704_067_200_000), 202_401);
    assert_eq!(next_month(202_312), 202_401);
    assert_eq!(next_month(202_311), 202_312);
}

const V1: &str = "CREATE TABLE a (x INTEGER);";
const V1_EDITED: &str = "CREATE TABLE a (x BIGINT);";
const V2: &str = "CREATE TABLE b (y INTEGER);";

#[test]
fn applying_the_set_twice_is_a_no_op() {
    let mut store = MemoryStore::new();
    let every: Vec<u32> = MIGRATIONS.iter().map(|m| m.version).collect();
    assert_eq!(
        every,
        (1..=u32::try_from(MIGRATIONS.len()).unwrap()).collect::<Vec<_>>(),
        "numbered 1, 2, 3..."
    );
    assert_eq!(store.migrate().unwrap(), every);
    assert_eq!(store.migrate().unwrap(), Vec::<u32>::new());
}

#[test]
fn a_modified_applied_migration_is_refused() {
    let first = [Migration {
        version: 1,
        name: "0001_a.sql",
        sql: V1,
    }];
    let edited = [
        Migration {
            version: 1,
            name: "0001_a.sql",
            sql: V1_EDITED,
        },
        Migration {
            version: 2,
            name: "0002_b.sql",
            sql: V2,
        },
    ];
    let mut store = MemoryStore::new();
    assert_eq!(store.migrate_with(&first).unwrap(), vec![1]);
    let err = store.migrate_with(&edited).unwrap_err();
    assert!(err.0.contains("migration 1"), "{err}");
    // Nothing after the refusal was applied: the unedited set lands 2 only.
    let fixed = [first[0], edited[1]];
    assert_eq!(store.migrate_with(&fixed).unwrap(), vec![2]);
    assert_ne!(checksum(V1), checksum(V1_EDITED));
}

#[test]
fn a_store_from_a_newer_build_is_refused() {
    let both = [
        Migration {
            version: 1,
            name: "0001_a.sql",
            sql: V1,
        },
        Migration {
            version: 2,
            name: "0002_b.sql",
            sql: V2,
        },
    ];
    let mut store = MemoryStore::new();
    store.migrate_with(&both).unwrap();
    let err = store.migrate_with(&both[..1]).unwrap_err();
    assert!(err.0.contains("does not know"), "{err}");
}

/// Everything the writer promises, against any store.
fn writer_contract(store: Box<dyn LedgerStore>) {
    let writer = PersistService::new(store, NOW).unwrap();
    let months = writer.with_store(|s| s.months()).unwrap();
    assert!(
        months.contains(&202_311) && months.contains(&202_312),
        "{months:?}"
    );

    // A trade: ledger rows land with the outcome; other payload follows the ledger.
    let trade = ledger_payload(&[(1, GOLD, -50), (2, GOLD, 50), (1, 9, 1)], &[0xAA, 0xBB]);
    let refused = ledger_payload(&[(1, GOLD, -1_000)], &[]);
    let chat = BoundedArray::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
    let batch = push(
        5,
        1,
        &[
            row(10, NOW, true, &trade),
            row(11, NOW, false, &refused),
            row(12, NOW, true, &chat),
        ],
    );
    assert_eq!(writer.push(&batch).unwrap().seq, 1);

    let one = writer.with_store(|s| s.ledger_of(1)).unwrap();
    assert_eq!(one.len(), 2, "a refused outcome has no ledger rows: {one:?}");
    assert_eq!(
        one.iter()
            .filter(|l| l.item == GOLD)
            .map(|l| l.delta)
            .sum::<i64>(),
        -50
    );
    assert!(
        one.iter()
            .all(|l| l.month == 202_311 && l.cell == 5 && l.tick == 10)
    );
    assert_eq!(writer.with_store(|s| s.ledger_of(2)).unwrap().len(), 1);
    let outcomes = writer.with_store(|s| s.outcomes_of(5)).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].payload.last(), Some(&0xBB));
    assert!(!outcomes[1].ok);

    // A resent batch is acknowledged and not written twice.
    assert_eq!(writer.push(&batch).unwrap().seq, 1);
    assert_eq!(writer.with_store(|s| s.ledger_of(1)).unwrap().len(), 2);
    assert_eq!(writer.with_store(|s| s.outcomes_of(5)).unwrap().len(), 3);

    // A month with no partition is created ahead of the write.
    let spring = push(
        5,
        2,
        &[row(20, LATER, true, &ledger_payload(&[(2, GOLD, -5)], &[]))],
    );
    assert_eq!(writer.push(&spring).unwrap().seq, 2);
    assert!(writer.with_store(|s| s.months()).unwrap().contains(&202_403));
    let two = writer.with_store(|s| s.ledger_of(2)).unwrap();
    assert_eq!(two.iter().map(|l| l.delta).sum::<i64>(), 45);
    assert_eq!(two.last().map(|l| l.month), Some(202_403));

    // Cells are independent: cell 6 starts at its own batch 1.
    let other = push(6, 1, &[row(1, NOW, true, &ledger_payload(&[(3, 4, 2)], &[]))]);
    assert_eq!(writer.push(&other).unwrap().seq, 1);
    assert_eq!(writer.with_store(|s| s.last_batch(6)).unwrap(), Some(1));
    assert_eq!(writer.with_store(|s| s.last_batch(5)).unwrap(), Some(2));

    // Audit rows: begun before, completed after.
    let id = writer
        .with_store(|s| s.audit_begin("ops-a", "ban", "account=3", NOW))
        .unwrap();
    let rows = writer.with_store(|s| s.audit_rows()).unwrap();
    assert_eq!(rows.last().map(|r| r.status.as_str()), Some("begun"));
    writer
        .with_store(|s| s.audit_complete(id, "done", "unbanned", "banned", "ban account=3 until=0"))
        .unwrap();
    let rows = writer.with_store(|s| s.audit_rows()).unwrap();
    let done = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(done.status, "done");
    assert_eq!(done.undo.as_deref(), Some("ban account=3 until=0"));

    social_rows_contract(&writer);
}

/// Guild and friend rows: batches apply once, in order, and read back as
/// the book holds them.
fn social_rows_contract(writer: &PersistService) {
    use mantis_core::social::{FriendChange, GuildChange, guild_rank};
    use mantis_services::persist::{friend_row, guild_row};
    let guilds = |seq, rows: &[GuildChange]| m::StoreGuildRows {
        epoch: 0,
        seq,
        rows: BoundedArray::from_slice(&rows.iter().map(guild_row).collect::<Vec<_>>()).unwrap(),
    };
    let member = |guild, character, rank, since| GuildChange::Member {
        guild,
        character,
        rank,
        since,
    };
    let found = [
        GuildChange::Guild {
            id: 1,
            name: "Lamplighters".to_owned(),
        },
        member(1, 7, guild_rank::LEADER, 1),
    ];
    assert_eq!(writer.write_guilds(&guilds(1, &found)).unwrap().seq, 1);
    let more = [
        member(1, 8, guild_rank::MEMBER, 2),
        member(1, 7, guild_rank::OFFICER, 1),
    ];
    assert_eq!(writer.write_guilds(&guilds(2, &more)).unwrap().seq, 2);
    // A resent batch is acknowledged and not applied again.
    let gone = [GuildChange::NoMember {
        guild: 1,
        character: 8,
    }];
    assert_eq!(writer.write_guilds(&guilds(2, &gone)).unwrap().seq, 2);
    let rows = writer.with_store(|s| s.guild_rows()).unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.contains(&member(1, 7, guild_rank::OFFICER, 1)));
    let disband = [GuildChange::NoGuild { id: 1 }];
    writer.write_guilds(&guilds(3, &disband)).unwrap();
    assert!(
        writer.with_store(|s| s.guild_rows()).unwrap().is_empty(),
        "members go with the guild"
    );

    let friends = |seq, rows: &[FriendChange]| m::StoreFriendRows {
        epoch: 0,
        seq,
        rows: BoundedArray::from_slice(&rows.iter().map(friend_row).collect::<Vec<_>>()).unwrap(),
    };
    let first = [
        FriendChange::Asked { asker: 9, asked: 4 },
        FriendChange::Friends { a: 4, b: 5 },
    ];
    writer.write_friends(&friends(1, &first)).unwrap();
    let second = [
        FriendChange::NoAsk { asker: 9, asked: 4 },
        FriendChange::Friends { a: 4, b: 9 },
        FriendChange::Asked { asker: 5, asked: 6 },
    ];
    writer.write_friends(&friends(2, &second)).unwrap();
    writer.write_friends(&friends(2, &first)).unwrap();
    assert_eq!(
        writer.with_store(|s| s.friend_rows()).unwrap(),
        vec![
            FriendChange::Friends { a: 4, b: 5 },
            FriendChange::Friends { a: 4, b: 9 },
            FriendChange::Asked { asker: 5, asked: 6 },
        ]
    );
    assert_eq!(writer.with_store(|s| s.friend_seq()).unwrap(), 2);
    // A friendship not stored lower id first is refused at the boundary.
    let backwards = m::StoreFriendRows {
        epoch: 0,
        seq: 3,
        rows: BoundedArray::from_slice(&[m::FriendRow { kind: 1, a: 9, b: 4 }]).unwrap(),
    };
    assert!(writer.write_friends(&backwards).is_err());

    // Live values: the latest of each, in the order last set.
    writer.with_store(|s| s.set_live("std.chat", 0, 0.0)).unwrap();
    writer
        .with_store(|s| s.set_live("std.vendor.markup", 1, 1.5))
        .unwrap();
    writer.with_store(|s| s.set_live("std.chat", 0, 1.0)).unwrap();
    assert_eq!(
        writer.with_store(|s| s.live_values()).unwrap(),
        vec![
            ("std.vendor.markup".to_owned(), 1, 1.5),
            ("std.chat".to_owned(), 0, 1.0)
        ]
    );
}

#[test]
fn the_writer_against_memory() {
    writer_contract(Box::new(MemoryStore::new()));
}

#[test]
fn a_failed_write_leaves_nothing_and_the_retry_lands() {
    let mut store = MemoryStore::new();
    store.fail_next_write = true;
    let writer = PersistService::new(Box::new(store), NOW).unwrap();
    let batch = push(1, 1, &[row(1, NOW, true, &ledger_payload(&[(1, GOLD, 10)], &[]))]);
    assert!(matches!(writer.push(&batch), Err(RpcError::Refused(_))));
    assert!(writer.with_store(|s| s.ledger_of(1)).unwrap().is_empty());
    assert_eq!(writer.with_store(|s| s.last_batch(1)).unwrap(), None);
    assert_eq!(writer.push(&batch).unwrap().seq, 1);
    assert_eq!(writer.with_store(|s| s.ledger_of(1)).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_writer_over_rpc_honours_the_caller_matrix() {
    let writer = PersistService::new(Box::new(MemoryStore::new()), NOW).unwrap();
    let key = b"cluster-key".to_vec();
    let server = RpcServer::bind("127.0.0.1:0".parse().unwrap(), key.clone(), writer.router())
        .await
        .unwrap();
    let wait = Duration::from_secs(5);
    let cell = RpcClient::new(server.addr(), Role::Cell, key.clone());
    let batch = push(9, 1, &[row(1, NOW, true, &ledger_payload(&[(4, GOLD, 7)], &[]))]);
    assert_eq!(cell.call::<methods::Push>(&batch, wait).await.unwrap().seq, 1);

    let ops = RpcClient::new(server.addr(), Role::Ops, key.clone());
    let query = m::LedgerOf {
        character: m::CharacterId(4),
    };
    let rows = ops.call::<methods::Ledger>(&query, wait).await.unwrap();
    assert_eq!(rows.rows.iter().map(|r| r.delta).collect::<Vec<_>>(), vec![7]);

    // Only cells push; only Ops reads ledgers.
    let gateway = RpcClient::new(server.addr(), Role::Gateway, key);
    assert_eq!(
        gateway.call::<methods::Push>(&batch, wait).await.unwrap_err(),
        RpcError::Forbidden
    );
    assert_eq!(
        cell.call::<methods::Ledger>(&query, wait).await.unwrap_err(),
        RpcError::Forbidden
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_writer_against_postgres() {
    let Some(config) = postgres_or_skip("the_writer_against_postgres") else {
        return;
    };
    let schema = format!("mantis_test_{}", std::process::id());
    let store = PgStore::connect(&config, &schema).await.unwrap();
    let probe = PgStore::connect(&config, &schema).await.unwrap();
    let result = tokio::task::spawn(async move {
        writer_contract(Box::new(store));
    })
    .await;
    probe.drop_schema(&schema).await.unwrap();
    result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_migrations_are_idempotent_and_guarded() {
    let Some(config) = postgres_or_skip("postgres_migrations_are_idempotent_and_guarded") else {
        return;
    };
    let schema = format!("mantis_mig_{}", std::process::id());
    let store = PgStore::connect(&config, &schema).await.unwrap();
    let probe = PgStore::connect(&config, &schema).await.unwrap();
    let outcome = tokio::task::spawn(async move {
        let mut store = store;
        let first = store.migrate();
        let second = store.migrate();
        (first, second)
    })
    .await
    .unwrap();
    probe.drop_schema(&schema).await.unwrap();
    assert_eq!(
        outcome.0.unwrap(),
        MIGRATIONS.iter().map(|m| m.version).collect::<Vec<_>>()
    );
    assert_eq!(outcome.1.unwrap(), Vec::<u32>::new());
}

/// Concurrent pushes to a PostgreSQL writer on a two-worker runtime never
/// deadlock: waiting for the store never pins a worker the database
/// connection needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pushes_to_a_postgres_writer_do_not_deadlock() {
    let Some(config) = postgres_or_skip("concurrent_pushes_to_a_postgres_writer_do_not_deadlock") else {
        return;
    };
    let schema = format!("mantis_conc_{}", std::process::id());
    let store = PgStore::connect(&config, &schema).await.unwrap();
    let probe = PgStore::connect(&config, &schema).await.unwrap();
    let writer = tokio::task::spawn(async move { PersistService::new(Box::new(store), NOW) })
        .await
        .unwrap()
        .unwrap();
    let key = b"k".to_vec();
    let server = RpcServer::bind("127.0.0.1:0".parse().unwrap(), key.clone(), writer.router())
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for cell in 1..=4u64 {
        let client = RpcClient::new(server.addr(), Role::Cell, key.clone());
        tasks.push(tokio::spawn(async move {
            for seq in 1..=25u64 {
                let rows = [row(seq, NOW, true, &ledger_payload(&[(cell, GOLD, 1)], &[]))];
                client
                    .call::<methods::Push>(&push(cell, seq, &rows), Duration::from_secs(5))
                    .await
                    .unwrap();
            }
        }));
    }
    let all = async {
        for t in tasks {
            t.await.unwrap();
        }
    };
    let done = tokio::time::timeout(Duration::from_secs(30), all).await;
    drop(server);
    probe.drop_schema(&schema).await.unwrap();
    assert!(done.is_ok(), "concurrent pushes deadlocked");
}
