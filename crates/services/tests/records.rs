//! Durable account and realm state: registration, login times, password
//! changes, bans, character creation and deletion, and the placement cell
//! hosts report are written through the persistence writer before the role
//! answers, and survive a restart of the account and realm roles. Session,
//! entry and transfer tokens are not durable and are refused after it.
//! Every test runs on the in-memory store, and on PostgreSQL when
//! `MANTIS_TEST_POSTGRES` names one (each skip printed and counted).

#![expect(clippy::unwrap_used, clippy::too_many_lines)]

use std::time::Duration;

use mantis_core::wire::WireString;
use mantis_services::cluster::{ClusterConfig, LocalCluster, StoreChoice};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{Method, RpcClient, RpcError};
use mantis_services::methods;
use mantis_services::persist::pg::{PgStore, postgres_or_skip};

const T: Duration = Duration::from_secs(5);

fn s<const N: usize>(text: &str) -> WireString<N> {
    WireString::new(text).unwrap()
}

/// Clients of the cluster's roles, as each caller.
struct Callers<'a> {
    cluster: &'a LocalCluster,
}

impl Callers<'_> {
    fn call<M: Method>(&self, server: Role, caller: Role, req: &M::Request) -> Result<M::Response, RpcError> {
        let addr = self.cluster.addr(server).unwrap();
        let client = RpcClient::new(addr, caller, self.cluster.key.clone());
        self.cluster
            .handle()
            .block_on(async move { client.call::<M>(req, T).await })
    }

    fn account<M: Method>(&self, caller: Role, req: &M::Request) -> Result<M::Response, RpcError> {
        self.call::<M>(Role::Account, caller, req)
    }

    fn realm<M: Method>(&self, caller: Role, req: &M::Request) -> Result<M::Response, RpcError> {
        self.call::<M>(Role::Realm, caller, req)
    }

    fn login(&self, name: &str, password: &str) -> Result<m::Session, RpcError> {
        self.account::<methods::LoginAccount>(
            Role::Gateway,
            &m::Login {
                name: s(name),
                password: s(password),
            },
        )
    }

    fn characters(&self, account: u64) -> Vec<u64> {
        self.realm::<methods::ListAccountCharacters>(
            Role::Gateway,
            &m::ListCharacters {
                account: m::AccountId(account),
            },
        )
        .unwrap()
        .ids
        .iter()
        .map(|c| c.0)
        .collect()
    }

    /// Registers the world cells (x < 0 and x >= 0) with the realm, as a
    /// cell host does after it sees a new run.
    fn register_cells(&self) {
        for (cell, lo, hi) in [(1, -1000.0, 0.0), (2, 0.0, 1000.0)] {
            self.realm::<methods::RegisterCellHost>(
                Role::Cell,
                &m::RegisterCell {
                    cell: m::CellNo(cell),
                    address: s(&format!("127.0.0.1:{}", 7400 + cell)),
                    lo,
                    hi,
                    instance: false,
                    world: 0,
                },
            )
            .unwrap();
        }
    }
}

fn restart(cluster: &mut LocalCluster, role: Role) {
    let addr = cluster.stop_role(role).unwrap();
    match role {
        Role::Account => cluster.start_account(addr).unwrap(),
        Role::Realm => cluster.start_realm(addr).unwrap(),
        _ => unreachable!("only account and realm restart here"),
    }
}

fn accounts_and_characters_survive_restarts(store: StoreChoice) {
    let mut config = ClusterConfig::local();
    config.store = store;
    let mut cluster = LocalCluster::start(&config).unwrap();
    let c = Callers { cluster: &cluster };

    // Register, log in, change the password, ban another account.
    let alice = c
        .account::<methods::RegisterAccount>(
            Role::Gateway,
            &m::Register {
                name: s("Alice"),
                password: s("first password"),
            },
        )
        .unwrap()
        .account
        .0;
    // Names are unique case-insensitively.
    assert!(matches!(
        c.account::<methods::RegisterAccount>(
            Role::Gateway,
            &m::Register {
                name: s("ALICE"),
                password: s("another one"),
            },
        ),
        Err(RpcError::Refused(_))
    ));
    let mallory = c
        .account::<methods::RegisterAccount>(
            Role::Ops,
            &m::Register {
                name: s("mallory"),
                password: s("mallory password"),
            },
        )
        .unwrap()
        .account
        .0;
    let before = c.login("alice", "first password").unwrap();
    c.account::<methods::ChangeAccountPassword>(
        Role::Gateway,
        &m::ChangePassword {
            name: s("alice"),
            old: s("first password"),
            new: s("second password"),
        },
    )
    .unwrap();
    assert!(c.login("alice", "first password").is_err());
    c.account::<methods::BanAccount>(
        Role::Ops,
        &m::Ban {
            account: m::AccountId(mallory),
            until_ms: u64::MAX / 2,
            reason: s("testing bans"),
        },
    )
    .unwrap();
    let last_login = cluster.account.account(alice).unwrap().last_login_ms;
    assert!(last_login > 0, "the login time is durable");

    // Characters: two created, one deleted, one placed by a cell host.
    c.register_cells();
    let create = |name: &str| {
        c.realm::<methods::NewCharacter>(
            Role::Gateway,
            &m::CreateCharacter {
                account: m::AccountId(alice),
                name: s(name),
                kind: 3,
            },
        )
    };
    let hero = create("Hero").unwrap().character.0;
    let spare = create("Spare").unwrap().character.0;
    assert!(
        matches!(create("hero"), Err(RpcError::Refused(_))),
        "names unique case-insensitively"
    );
    c.realm::<methods::RemoveCharacter>(
        Role::Gateway,
        &m::DeleteCharacter {
            account: m::AccountId(alice),
            character: m::CharacterId(spare),
        },
    )
    .unwrap();
    c.realm::<methods::PlaceCharacter>(
        Role::Cell,
        &m::CharacterPlaced {
            character: m::CharacterId(hero),
            cell: m::CellNo(2),
            world: 1,
            x: 12.5,
            y: 0.0,
            z: -3.0,
            level: 4,
        },
    )
    .unwrap();
    let entry = c
        .realm::<methods::Select>(
            Role::Gateway,
            &m::SelectCharacter {
                account: m::AccountId(alice),
                character: m::CharacterId(hero),
            },
        )
        .unwrap();

    // Both roles restart.
    restart(&mut cluster, Role::Account);
    restart(&mut cluster, Role::Realm);
    let c = Callers { cluster: &cluster };

    // Accounts: the new password, the login time, the ban, unique names.
    assert!(c.login("alice", "first password").is_err());
    let after = c.login("Alice", "second password").unwrap();
    assert_eq!(after.account, before.account);
    assert!(cluster.account.account(alice).unwrap().last_login_ms >= last_login);
    assert!(matches!(c.login("mallory", "mallory password"), Err(RpcError::Refused(r)) if r == "banned"));
    let banned = cluster.account.account(mallory).unwrap();
    assert_eq!(banned.ban_reason, "testing bans");
    assert!(matches!(
        c.account::<methods::RegisterAccount>(
            Role::Gateway,
            &m::Register {
                name: s("alice"),
                password: s("a third one"),
            },
        ),
        Err(RpcError::Refused(_))
    ));
    // A session token of the old run is refused.
    assert!(
        c.account::<methods::VerifySession>(Role::Cell, &m::VerifyToken { token: before.token })
            .is_err()
    );
    // A new account numbers past every old one.
    let carol = c
        .account::<methods::RegisterAccount>(
            Role::Gateway,
            &m::Register {
                name: s("carol"),
                password: s("carol password"),
            },
        )
        .unwrap()
        .account
        .0;
    assert!(carol > mallory);

    // Characters: the living one, with its kind, level and placement; the
    // deleted one gone, its id never reused, its name free again.
    assert_eq!(c.characters(alice), vec![hero]);
    let row = cluster.realm.character(hero).unwrap();
    assert_eq!((row.name.as_str(), row.kind, row.level), ("Hero", 3, 4));
    assert_eq!((row.cell, row.world, row.position), (2, 1, [12.5, 0.0, -3.0]));
    assert!(cluster.realm.character(spare).unwrap().deleted);
    let again = create_on(&c, alice, "Spare").unwrap().character.0;
    assert!(again > spare, "a deleted character's id is never reused");
    // The entry token of the old run is refused; selecting again places
    // the returning character in the cell it left, where it left.
    assert!(
        c.realm::<methods::RedeemToken>(
            Role::Cell,
            &m::Redeem {
                token: entry.token,
                cell: entry.cell,
            },
        )
        .is_err()
    );
    c.register_cells();
    let entry = c
        .realm::<methods::Select>(
            Role::Gateway,
            &m::SelectCharacter {
                account: m::AccountId(alice),
                character: m::CharacterId(hero),
            },
        )
        .unwrap();
    assert_eq!(entry.cell.0, 2, "where it left");
    let redeemed = c
        .realm::<methods::RedeemToken>(
            Role::Cell,
            &m::Redeem {
                token: entry.token,
                cell: entry.cell,
            },
        )
        .unwrap();
    assert!(redeemed.placed);
    assert_eq!([redeemed.x, redeemed.y, redeemed.z], [12.5, 0.0, -3.0]);
    // A new character enters where new characters do.
    let entry = c
        .realm::<methods::Select>(
            Role::Gateway,
            &m::SelectCharacter {
                account: m::AccountId(alice),
                character: m::CharacterId(again),
            },
        )
        .unwrap();
    let redeemed = c
        .realm::<methods::RedeemToken>(
            Role::Cell,
            &m::Redeem {
                token: entry.token,
                cell: entry.cell,
            },
        )
        .unwrap();
    assert!(!redeemed.placed);
}

fn create_on(c: &Callers<'_>, account: u64, name: &str) -> Result<m::Created, RpcError> {
    c.realm::<methods::NewCharacter>(
        Role::Gateway,
        &m::CreateCharacter {
            account: m::AccountId(account),
            name: s(name),
            kind: 3,
        },
    )
}

#[test]
fn accounts_and_characters_survive_restarts_in_memory() {
    accounts_and_characters_survive_restarts(StoreChoice::Memory);
}

#[test]
fn accounts_and_characters_survive_restarts_on_postgres() {
    let Some(conn) = postgres_or_skip("accounts_and_characters_survive_restarts_on_postgres") else {
        return;
    };
    let schema = format!("mantis_records_{}", std::process::id());
    accounts_and_characters_survive_restarts(StoreChoice::PostgresSchema(conn.clone(), schema.clone()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let probe = PgStore::connect(&conn, &schema).await.unwrap();
        probe.drop_schema(&schema).await.unwrap();
    });
}

#[test]
fn an_account_or_realm_role_without_its_store_does_not_start() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let account = mantis_services::account::AccountService::new();
    assert!(rt.block_on(account.load_durable()).is_err());
    let realm = mantis_services::realm::RealmService::new();
    assert!(rt.block_on(realm.load_durable()).is_err());
}

/// Two hosts serving the same regions as two worlds register without
/// colliding; a new character enters the lowest-numbered world, a returning
/// one the cell it left in whichever world.
#[test]
fn worlds_register_side_by_side_and_new_characters_enter_the_first() {
    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let c = Callers { cluster: &cluster };
    for (cell, lo, hi, world) in [
        (11, -1000.0, 0.0, 2),
        (12, 0.0, 1000.0, 2),
        (1, -1000.0, 0.0, 1),
        (2, 0.0, 1000.0, 1),
    ] {
        c.realm::<methods::RegisterCellHost>(
            Role::Cell,
            &m::RegisterCell {
                cell: m::CellNo(cell),
                address: s(&format!("127.0.0.1:{}", 7400 + cell)),
                lo,
                hi,
                instance: false,
                world,
            },
        )
        .unwrap();
    }
    assert_eq!(cluster.realm.cells().len(), 4);
    let hero = create_on(&c, 5, "Wanderer").unwrap().character.0;
    let select = || {
        c.realm::<methods::Select>(
            Role::Gateway,
            &m::SelectCharacter {
                account: m::AccountId(5),
                character: m::CharacterId(hero),
            },
        )
        .unwrap()
        .cell
        .0
    };
    assert_eq!(select(), 2, "the cell owning x = 0 in world 1");
    c.realm::<methods::PlaceCharacter>(
        Role::Cell,
        &m::CharacterPlaced {
            character: m::CharacterId(hero),
            cell: m::CellNo(12),
            world: 2,
            x: 5.0,
            y: 0.0,
            z: 0.0,
            level: 0,
        },
    )
    .unwrap();
    assert_eq!(select(), 12, "back in world 2, where it left");
}
