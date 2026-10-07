//! Role failover: two instances of every failover role (account, realm,
//! social, matchmaking, Ops), one active per role chosen by a lease in the
//! persistence writer, on a manual clock.
//!
//! - A killed active is replaced by its standby within a bounded number of
//!   clock ticks (a `budget:` line per role), and the new term serves the
//!   durable state of the old one.
//! - An active that stops renewing but keeps serving (a hung process)
//!   cannot write once its lease is taken over: the writer refuses its
//!   stale epoch, and it answers as a standby. Nothing is written twice.
//! - An old active that returns is a standby.
//! - The lease row's rules, in memory and on Postgres.
#![expect(clippy::unwrap_used)]

use std::time::Duration;

use mantis_core::wire::WireString;
use mantis_services::cluster::{ClusterConfig, LocalCluster};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::clock::{ManualClock, ServiceClock};
use mantis_services::host::rpc::{Endpoint, Method, RpcClient, RpcError};
use mantis_services::methods;
use mantis_services::ops::Command;
use mantis_services::persist::LedgerStore;
use mantis_services::persist::memory::MemoryStore;
use mantis_services::persist::pg::{PgStore, postgres_or_skip};

const START_MS: u64 = 1_700_000_000_000;
const TTL: Duration = Duration::from_millis(3000);
/// One step of the test's clock.
const TICK: Duration = Duration::from_millis(100);
const T: Duration = Duration::from_secs(5);
const FAILOVER: [Role; 5] = [
    Role::Account,
    Role::Realm,
    Role::Social,
    Role::Matchmaking,
    Role::Ops,
];

fn replicated() -> (LocalCluster, ManualClock) {
    let clock = ManualClock::new(START_MS);
    let mut config = ClusterConfig::local();
    config.clock = ServiceClock::manual(&clock);
    config.instances = 2;
    config.lease_ttl = TTL;
    (LocalCluster::start(&config).unwrap(), clock)
}

/// A client of every instance of `server`, calling as `caller`.
fn client(cluster: &LocalCluster, caller: Role, server: Role) -> RpcClient {
    let endpoint = cluster.endpoint(server).unwrap();
    RpcClient::with_endpoint(endpoint, caller, cluster.key.clone(), None, server).unwrap()
}

fn call<M: Method>(
    cluster: &LocalCluster,
    client: &RpcClient,
    req: &M::Request,
) -> Result<M::Response, RpcError> {
    cluster.handle().block_on(client.call::<M>(req, T))
}

fn s<const N: usize>(text: &str) -> WireString<N> {
    WireString::new(text).unwrap()
}

/// One clock tick, every lease task settled.
fn tick(cluster: &LocalCluster, clock: &ManualClock) {
    clock.advance(TICK);
    cluster.settle_leases().unwrap();
}

fn every_role_has_one_active(cluster: &LocalCluster) {
    for role in FAILOVER {
        assert!(
            cluster.active(role).is_some(),
            "{} has no active instance",
            role.name()
        );
    }
}

/// An account with a character in a guild, through the roles' endpoints:
/// (account, character).
fn some_state(cluster: &LocalCluster) -> (u64, u64) {
    let gateway = client(cluster, Role::Gateway, Role::Account);
    let account = call::<methods::RegisterAccount>(
        cluster,
        &gateway,
        &m::Register {
            name: s("amy"),
            password: s("amy-password"),
        },
    )
    .unwrap()
    .account
    .0;
    let realm = client(cluster, Role::Gateway, Role::Realm);
    let character = call::<methods::NewCharacter>(
        cluster,
        &realm,
        &m::CreateCharacter {
            account: m::AccountId(account),
            name: s("Amaryllis"),
            kind: 1,
        },
    )
    .unwrap()
    .character
    .0;
    let social = client(cluster, Role::Ops, Role::Social);
    call::<methods::NewGuild>(
        cluster,
        &social,
        &m::CreateGuild {
            leader: m::CharacterId(character),
            name: s("Gardeners"),
        },
    )
    .unwrap();
    (account, character)
}

/// The new term of `role` serves what the old one made durable.
fn serves_the_old_state(cluster: &LocalCluster, role: Role, account: u64, character: u64) {
    match role {
        Role::Account => {
            let gateway = client(cluster, Role::Gateway, Role::Account);
            let session = call::<methods::LoginAccount>(
                cluster,
                &gateway,
                &m::Login {
                    name: s("amy"),
                    password: s("amy-password"),
                },
            )
            .unwrap();
            assert_eq!(session.account.0, account);
        }
        Role::Realm => {
            let gateway = client(cluster, Role::Gateway, Role::Realm);
            let listed = call::<methods::ListAccountCharacters>(
                cluster,
                &gateway,
                &m::ListCharacters {
                    account: m::AccountId(account),
                },
            )
            .unwrap();
            assert!(listed.ids.iter().any(|c| c.0 == character));
        }
        Role::Social => assert!(cluster.social().guild_of(character).is_some()),
        Role::Ops => {
            // The new term writes live values under its own epoch.
            cluster
                .execute(
                    "alice",
                    &Command::Flag {
                        name: "std.chat".to_owned(),
                        on: false,
                    },
                )
                .unwrap();
        }
        _ => {}
    }
}

#[test]
fn a_killed_active_is_replaced_within_budget_and_serves_its_durable_state() {
    let (mut cluster, clock) = replicated();
    every_role_has_one_active(&cluster);
    print!("{}", cluster.graph());
    let (account, character) = some_state(&cluster);
    let ttl_ticks = TTL.as_millis() / TICK.as_millis();
    // The lease lapses at most one lifetime after the kill, and the standby
    // asks every third of one.
    let limit = ttl_ticks + ttl_ticks / 3 + 1;
    for role in FAILOVER {
        let (killed, epoch) = cluster.active(role).unwrap();
        cluster.stop_instance(role, killed).unwrap();
        let mut ticks = 0u128;
        let (now, next) = loop {
            tick(&cluster, &clock);
            ticks += 1;
            if let Some(active) = cluster.active(role) {
                break active;
            }
            assert!(ticks <= limit * 4, "{} never took over", role.name());
        };
        println!(
            "budget: the {} standby took over {ticks} ticks of {} ms after the active was killed (limit {limit} ticks: one lease lifetime and one renewal period)",
            role.name(),
            TICK.as_millis()
        );
        assert_ne!(now, killed);
        assert_eq!(next, epoch + 1, "a takeover is the next epoch");
        assert!(ticks <= limit);
        serves_the_old_state(&cluster, role, account, character);
        // The killed instance comes back as a standby.
        cluster.start_instance(role, killed).unwrap();
        tick(&cluster, &clock);
        assert_eq!(cluster.active(role), Some((now, next)));
    }
}

#[test]
fn a_hung_active_cannot_write_after_its_lease_is_taken() {
    let (cluster, clock) = replicated();
    let (hung, epoch) = cluster.active(Role::Account).unwrap();
    let gateway = client(&cluster, Role::Gateway, Role::Account);
    // Straight to the hung instance, as a caller that still prefers it.
    let at_hung = RpcClient::with_endpoint(
        Endpoint::fixed(*cluster.instances(Role::Account).get(hung).unwrap()),
        Role::Gateway,
        cluster.key.clone(),
        None,
        Role::Account,
    )
    .unwrap();
    let register = |name: &str| m::Register {
        name: s(name),
        password: s("password"),
    };
    call::<methods::RegisterAccount>(&cluster, &gateway, &register("before")).unwrap();

    // It stops renewing but keeps serving; the standby takes over.
    assert!(cluster.stall_instance(Role::Account, hung));
    let mut ticks = 0;
    while cluster.active(Role::Account).is_none_or(|(i, _)| i == hung) {
        tick(&cluster, &clock);
        ticks += 1;
        assert!(ticks < 200, "the standby never took over");
    }
    let (now, next) = cluster.active(Role::Account).unwrap();
    assert_eq!((now != hung, next), (true, epoch + 1));

    // Two instances believe they are active. The hung one's write is
    // refused as stale: it is a standby from then on, and wrote nothing.
    assert_eq!(
        call::<methods::RegisterAccount>(&cluster, &at_hung, &register("zombie")),
        Err(RpcError::Standby)
    );
    assert_eq!(
        call::<methods::RegisterAccount>(&cluster, &at_hung, &register("again")),
        Err(RpcError::Standby)
    );
    // Callers listing both instances reach the active one.
    call::<methods::RegisterAccount>(&cluster, &gateway, &register("after")).unwrap();
    let rows = cluster.persist.with_store(|s| s.account_rows()).unwrap();
    let names: Vec<&str> = rows.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"before") && names.contains(&"after"), "{names:?}");
    let mut ids: Vec<u64> = rows.iter().map(|a| a.id).collect();
    ids.dedup();
    assert_eq!(ids.len(), 2, "no id written twice");
}

#[test]
fn an_old_active_returning_is_a_standby_and_takes_over_again_later() {
    let (mut cluster, clock) = replicated();
    let (account, character) = some_state(&cluster);
    let (first, epoch) = cluster.active(Role::Realm).unwrap();
    cluster.stop_instance(Role::Realm, first).unwrap();
    while cluster.active(Role::Realm).is_none() {
        tick(&cluster, &clock);
    }
    let (second, _) = cluster.active(Role::Realm).unwrap();
    cluster.start_instance(Role::Realm, first).unwrap();
    for _ in 0..(TTL.as_millis() / TICK.as_millis()) {
        tick(&cluster, &clock);
        assert_eq!(cluster.active(Role::Realm).map(|a| a.0), Some(second));
    }
    // The second goes: the first takes over again, with the next epoch,
    // and still serves every character.
    cluster.stop_instance(Role::Realm, second).unwrap();
    while cluster.active(Role::Realm).is_none() {
        tick(&cluster, &clock);
    }
    assert_eq!(cluster.active(Role::Realm), Some((first, epoch + 2)));
    serves_the_old_state(&cluster, Role::Realm, account, character);
}

/// The lease row's rules, on any store.
fn lease_contract(store: &mut dyn LedgerStore) {
    let role = Role::Account as u8;
    assert_eq!(store.lease(role).unwrap(), None);
    let a = store.acquire_lease(role, "a", 1000, 300).unwrap();
    assert_eq!((a.owner.as_str(), a.epoch, a.expires_ms), ("a", 1, 1300));
    // Another owner, before it lapses: unchanged.
    let b = store.acquire_lease(role, "b", 1200, 300).unwrap();
    assert_eq!((b.owner.as_str(), b.epoch, b.expires_ms), ("a", 1, 1300));
    // The holder renews and keeps its epoch.
    let a = store.acquire_lease(role, "a", 1250, 300).unwrap();
    assert_eq!((a.owner.as_str(), a.epoch, a.expires_ms), ("a", 1, 1550));
    // Lapsed: taken with the next epoch.
    let b = store.acquire_lease(role, "b", 1550, 300).unwrap();
    assert_eq!((b.owner.as_str(), b.epoch, b.expires_ms), ("b", 2, 1850));
    // The old holder is refused now.
    let a = store.acquire_lease(role, "a", 1600, 300).unwrap();
    assert_eq!((a.owner.as_str(), a.epoch), ("b", 2));
    // Roles are apart.
    let other = store.acquire_lease(Role::Realm as u8, "a", 1600, 300).unwrap();
    assert_eq!((other.owner.as_str(), other.epoch), ("a", 1));
    assert_eq!(store.lease(role).unwrap().map(|l| l.epoch), Some(2));
}

#[test]
fn leases_in_memory() {
    lease_contract(&mut MemoryStore::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_on_postgres() {
    let Some(config) = postgres_or_skip("leases_on_postgres") else {
        return;
    };
    let schema = format!("mantis_lease_{}", std::process::id());
    let store = PgStore::connect(&config, &schema).await.unwrap();
    let probe = PgStore::connect(&config, &schema).await.unwrap();
    let result = tokio::task::spawn(async move {
        let mut store = store;
        store.migrate().unwrap();
        lease_contract(&mut store);
    })
    .await;
    probe.drop_schema(&schema).await.unwrap();
    result.unwrap();
}
