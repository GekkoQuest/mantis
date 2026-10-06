//! The typed internal RPC (caller matrix, validation, timeouts, reconnect,
//! cluster key) and the account, realm, social, and matchmaking roles,
//! each served over it.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mantis_core::wire::{BoundedArray, WireString};
use mantis_services::account::AccountService;
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::{Method, Router, RpcClient, RpcError, RpcServer};
use mantis_services::matchmaking::MatchmakingService;
use mantis_services::methods;
use mantis_services::realm::RealmService;
use mantis_services::social::{GUILD, SocialService, WHISPER, WORLD};

const KEY: &[u8] = b"cluster-key-for-tests";
const WAIT: Duration = Duration::from_secs(5);

async fn serve(router: Router) -> RpcServer {
    RpcServer::bind("127.0.0.1:0".parse().unwrap(), KEY.to_vec(), router)
        .await
        .unwrap()
}

fn client(addr: SocketAddr, role: Role) -> RpcClient {
    RpcClient::new(addr, role, KEY.to_vec())
}

async fn call<M: Method>(c: &RpcClient, req: &M::Request) -> Result<M::Response, RpcError> {
    c.call::<M>(req, WAIT).await
}

fn s<const N: usize>(text: &str) -> WireString<N> {
    WireString::new(text).unwrap()
}

// ---- the RPC layer ----------------------------------------------------------

#[test]
fn every_method_has_callers_and_a_unique_id() {
    let rows = methods::matrix();
    let mut ids: Vec<u16> = rows.iter().map(|r| r.1).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), rows.len());
    assert!(rows.iter().all(|r| !r.2.is_empty()), "a method nobody may call");
    // Game-facing roles never reach Ops-only methods.
    for (name, _, callers) in &rows {
        if [
            "BanAccount",
            "Maintenance",
            "Ledger",
            "InspectCell",
            "Kick",
            "Drain",
        ]
        .contains(name)
        {
            assert_eq!(*callers, &[Role::Ops][..], "{name}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rpc_layer_enforces_callers_validation_and_the_cluster_key() {
    let account = AccountService::new();
    let server = serve(account.router()).await;
    let gateway = client(server.addr(), Role::Gateway);
    let cell = client(server.addr(), Role::Cell);
    let short = m::Register {
        name: s("someone"),
        password: s("short"),
    };
    assert!(matches!(
        call::<methods::RegisterAccount>(&gateway, &short).await,
        Err(RpcError::Refused(_))
    ));
    let ok = m::Register {
        name: s("someone"),
        password: s("long enough"),
    };
    assert_eq!(
        call::<methods::RegisterAccount>(&cell, &ok).await,
        Err(RpcError::Forbidden)
    );
    assert!(call::<methods::RegisterAccount>(&gateway, &ok).await.is_ok());
    // A method the role does not serve.
    assert_eq!(
        call::<methods::Poll>(
            &cell,
            &m::PollSocial {
                cell: m::CellNo(1),
                since: 0
            }
        )
        .await,
        Err(RpcError::NoSuchMethod)
    );
    // A caller without the cluster key is cut off at the hello.
    let stranger = RpcClient::new(server.addr(), Role::Gateway, b"another key".to_vec());
    assert_eq!(
        call::<methods::RegisterAccount>(&stranger, &ok).await,
        Err(RpcError::Disconnected)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_time_out_and_clients_reconnect() {
    let mut slow = Router::new();
    slow.serve::<methods::Maintenance>(|_, req| {
        if req.on {
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(m::MaintenanceWas { was: false })
    });
    let server = serve(slow).await;
    let addr = server.addr();
    let ops = client(addr, Role::Ops);
    assert_eq!(
        ops.call::<methods::Maintenance>(&m::SetMaintenance { on: true }, Duration::from_millis(50))
            .await,
        Err(RpcError::Timeout)
    );
    // The connection survives a timeout.
    assert!(
        call::<methods::Maintenance>(&ops, &m::SetMaintenance { on: false })
            .await
            .is_ok()
    );

    // The server goes away: calls fail, then succeed once it is back.
    drop(server);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        call::<methods::Maintenance>(&ops, &m::SetMaintenance { on: false }).await,
        Err(RpcError::Disconnected)
    );
    let mut back = Router::new();
    back.serve::<methods::Maintenance>(|_, _| Ok(m::MaintenanceWas { was: true }));
    let _again = RpcServer::bind(addr, KEY.to_vec(), back).await.unwrap();
    let r = call::<methods::Maintenance>(&ops, &m::SetMaintenance { on: false })
        .await
        .unwrap();
    assert!(r.was, "answered by the new server");
}

// ---- account ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts_log_in_with_single_use_tokens_and_honour_bans_and_maintenance() {
    let account = AccountService::new();
    let server = serve(account.router()).await;
    let (gateway, cell, ops) = (
        client(server.addr(), Role::Gateway),
        client(server.addr(), Role::Cell),
        client(server.addr(), Role::Ops),
    );
    let reg = m::Register {
        name: s("player_one"),
        password: s("correct horse"),
    };
    let id = call::<methods::RegisterAccount>(&gateway, &reg)
        .await
        .unwrap()
        .account;
    assert!(
        call::<methods::RegisterAccount>(&gateway, &reg).await.is_err(),
        "name taken"
    );

    let login = m::Login {
        name: s("player_one"),
        password: s("correct horse"),
    };
    let wrong = m::Login {
        name: s("player_one"),
        password: s("wrong horse!"),
    };
    assert!(call::<methods::LoginAccount>(&gateway, &wrong).await.is_err());
    let session = call::<methods::LoginAccount>(&gateway, &login).await.unwrap();
    assert_eq!(session.account, id);
    let verify = m::VerifyToken { token: session.token };
    assert_eq!(
        call::<methods::VerifySession>(&cell, &verify)
            .await
            .unwrap()
            .account,
        id
    );
    assert!(
        call::<methods::VerifySession>(&cell, &verify).await.is_err(),
        "a token verifies once"
    );

    let ban = m::Ban {
        account: id,
        until_ms: u64::MAX / 2,
        reason: s("spam"),
    };
    assert_eq!(
        call::<methods::BanAccount>(&ops, &ban)
            .await
            .unwrap()
            .previous_until_ms,
        0
    );
    assert_eq!(
        call::<methods::LoginAccount>(&gateway, &login).await,
        Err(RpcError::Refused("banned".to_owned()))
    );
    let lift = m::Ban {
        account: id,
        until_ms: 0,
        reason: s("appeal"),
    };
    call::<methods::BanAccount>(&ops, &lift).await.unwrap();
    assert!(call::<methods::LoginAccount>(&gateway, &login).await.is_ok());

    let on = m::SetMaintenance { on: true };
    assert!(!call::<methods::Maintenance>(&ops, &on).await.unwrap().was);
    assert_eq!(
        call::<methods::LoginAccount>(&gateway, &login).await,
        Err(RpcError::Refused("maintenance".to_owned()))
    );
    assert_ne!(account.fingerprint(), String::new());
}

// ---- realm ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_realm_places_characters_and_issues_single_use_tokens() {
    let realm = RealmService::new();
    let server = serve(realm.router()).await;
    let (gateway, cell) = (
        client(server.addr(), Role::Gateway),
        client(server.addr(), Role::Cell),
    );
    let world = |n: u64, lo: f32, hi: f32| m::RegisterCell {
        cell: m::CellNo(n),
        address: s(&format!("127.0.0.1:{}", 7400 + n)),
        lo,
        hi,
        instance: false,
    };
    call::<methods::RegisterCellHost>(&cell, &world(1, -1000.0, 0.0))
        .await
        .unwrap();
    call::<methods::RegisterCellHost>(&cell, &world(2, 0.0, 1000.0))
        .await
        .unwrap();
    assert!(
        call::<methods::RegisterCellHost>(&cell, &world(3, 5.0, 5.0))
            .await
            .is_err(),
        "an empty range"
    );
    let instance = m::RegisterCell {
        cell: m::CellNo(10),
        address: s("127.0.0.1:7500"),
        lo: 0.0,
        hi: 0.0,
        instance: true,
    };
    call::<methods::RegisterCellHost>(&cell, &instance).await.unwrap();
    assert_eq!(realm.cells().len(), 3);

    let account = m::AccountId(7);
    let created = call::<methods::NewCharacter>(
        &gateway,
        &m::CreateCharacter {
            account,
            name: s("hero"),
        },
    )
    .await
    .unwrap()
    .character;
    let list = call::<methods::ListAccountCharacters>(&gateway, &m::ListCharacters { account })
        .await
        .unwrap();
    assert_eq!(list.ids.iter().copied().collect::<Vec<_>>(), vec![created]);
    assert!(
        call::<methods::Select>(
            &gateway,
            &m::SelectCharacter {
                account: m::AccountId(8),
                character: created
            }
        )
        .await
        .is_err(),
        "not your character"
    );
    let placed = call::<methods::Select>(
        &gateway,
        &m::SelectCharacter {
            account,
            character: created,
        },
    )
    .await
    .unwrap();
    assert_eq!(placed.cell, m::CellNo(2), "x = 0 is in the second cell");

    let wrong_cell = m::Redeem {
        token: placed.token,
        cell: m::CellNo(1),
    };
    assert!(call::<methods::RedeemToken>(&cell, &wrong_cell).await.is_err());
    let redeem = m::Redeem {
        token: placed.token,
        cell: placed.cell,
    };
    assert_eq!(
        call::<methods::RedeemToken>(&cell, &redeem)
            .await
            .unwrap()
            .character,
        created
    );
    assert!(
        call::<methods::RedeemToken>(&cell, &redeem).await.is_err(),
        "once"
    );

    // A transfer token for the neighbour carries the lease epoch.
    let t = call::<methods::Transfer>(
        &cell,
        &m::IssueTransfer {
            character: created,
            from: m::CellNo(2),
            to: m::CellNo(1),
            epoch: 4,
        },
    )
    .await
    .unwrap();
    let r = call::<methods::RedeemToken>(
        &cell,
        &m::Redeem {
            token: t.token,
            cell: m::CellNo(1),
        },
    )
    .await
    .unwrap();
    assert_eq!(r.epoch, 4);

    // Instances: one free cell, then none.
    let one = call::<methods::NewInstance>(&cell, &m::CreateInstance { template: 1 })
        .await
        .unwrap();
    assert_eq!(one.cell, m::CellNo(10));
    assert!(
        call::<methods::NewInstance>(&cell, &m::CreateInstance { template: 1 })
            .await
            .is_err()
    );
    assert_eq!(
        call::<methods::NewInstance>(&gateway, &m::CreateInstance { template: 1 }).await,
        Err(RpcError::Forbidden)
    );
}

// ---- social -----------------------------------------------------------------

async fn present(cell: &RpcClient, n: u64, characters: &[u64]) {
    let ids: Vec<m::CharacterId> = characters.iter().map(|c| m::CharacterId(*c)).collect();
    call::<methods::Presence>(
        cell,
        &m::Present {
            cell: m::CellNo(n),
            characters: BoundedArray::from_slice(&ids).unwrap(),
        },
    )
    .await
    .unwrap();
}

async fn poll(cell: &RpcClient, n: u64, since: u64) -> Vec<m::SocialUpdate> {
    call::<methods::Poll>(
        cell,
        &m::PollSocial {
            cell: m::CellNo(n),
            since,
        },
    )
    .await
    .unwrap()
    .updates
    .iter()
    .copied()
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn social_projects_cross_cell_chat_and_guilds_onto_the_cells_hosting_them() {
    let social = SocialService::new();
    let server = serve(social.router()).await;
    let cell = client(server.addr(), Role::Cell);
    present(&cell, 1, &[11, 12]).await;
    present(&cell, 2, &[21]).await;

    // A whisper crosses cells: the target's cell and the sender's both see it.
    let whisper = m::Publish {
        channel: WHISPER,
        from: m::CharacterId(11),
        to: 21,
        text: s("hello there"),
    };
    call::<methods::PublishLine>(&cell, &whisper).await.unwrap();
    let on2 = poll(&cell, 2, 0).await;
    assert_eq!(on2.len(), 1);
    assert_eq!(
        (on2[0].to, on2[0].from, on2[0].text.as_str()),
        (m::CharacterId(21), m::CharacterId(11), "hello there")
    );
    assert_eq!(poll(&cell, 1, 0).await.len(), 1);
    let offline = m::Publish { to: 99, ..whisper };
    assert!(call::<methods::PublishLine>(&cell, &offline).await.is_err());

    // Guild lines reach members only, wherever they are.
    let g = call::<methods::NewGuild>(
        &cell,
        &m::CreateGuild {
            leader: m::CharacterId(12),
            name: s("lamplighters"),
        },
    )
    .await
    .unwrap()
    .guild;
    call::<methods::EnterGuild>(
        &cell,
        &m::JoinGuild {
            character: m::CharacterId(21),
            guild: g,
        },
    )
    .await
    .unwrap();
    let since2 = poll(&cell, 2, 0).await.last().unwrap().seq;
    let line = m::Publish {
        channel: GUILD,
        from: m::CharacterId(12),
        to: g,
        text: s("meet at the gate"),
    };
    call::<methods::PublishLine>(&cell, &line).await.unwrap();
    let new2 = poll(&cell, 2, since2).await;
    assert_eq!(new2.len(), 1);
    assert_eq!(new2[0].channel, GUILD);
    let outsider = m::Publish {
        from: m::CharacterId(11),
        ..line
    };
    assert!(
        call::<methods::PublishLine>(&cell, &outsider).await.is_err(),
        "not a member"
    );

    // World lines reach everyone online; presence updates move characters.
    present(&cell, 1, &[11]).await;
    let since1 = poll(&cell, 1, 0).await.last().map_or(0, |u| u.seq);
    let world = m::Publish {
        channel: WORLD,
        from: m::CharacterId(21),
        to: 0,
        text: s("server restart soon"),
    };
    call::<methods::PublishLine>(&cell, &world).await.unwrap();
    let to1: Vec<u64> = poll(&cell, 1, since1).await.iter().map(|u| u.to.0).collect();
    assert_eq!(to1, vec![11], "12 left cell 1");
}

// ---- matchmaking ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matchmaking_places_full_groups_and_keeps_places_when_no_instance_is_free() {
    let free = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = Arc::clone(&free);
    let mm = MatchmakingService::new(
        2,
        Arc::new(move |_queue| {
            if f.load(std::sync::atomic::Ordering::Relaxed) {
                Ok((10, "127.0.0.1:7500".to_owned()))
            } else {
                Err(RpcError::Refused("no free instance cell".to_owned()))
            }
        }),
    );
    let server = serve(mm.router()).await;
    let gateway = client(server.addr(), Role::Gateway);
    let enqueue = |c: u64| m::Enqueue {
        character: m::CharacterId(c),
        queue: 1,
    };
    let poll = |c: u64| m::PollMatch {
        character: m::CharacterId(c),
    };
    call::<methods::Queue>(&gateway, &enqueue(1)).await.unwrap();
    assert!(
        call::<methods::Queue>(&gateway, &enqueue(1)).await.is_err(),
        "already queued"
    );
    // The group is full but no instance is free: the call fails, nobody
    // loses their place.
    assert!(call::<methods::Queue>(&gateway, &enqueue(2)).await.is_err());
    assert_eq!(
        call::<methods::MatchFor>(&gateway, &poll(1)).await.unwrap().cell,
        m::CellNo(0)
    );
    free.store(true, std::sync::atomic::Ordering::Relaxed);
    call::<methods::Queue>(&gateway, &enqueue(3)).await.unwrap();
    for c in [1, 2] {
        let got = call::<methods::MatchFor>(&gateway, &poll(c)).await.unwrap();
        assert_eq!(
            (got.cell, got.address.as_str()),
            (m::CellNo(10), "127.0.0.1:7500")
        );
    }
    assert_eq!(
        call::<methods::MatchFor>(&gateway, &poll(3)).await.unwrap().cell,
        m::CellNo(0),
        "3 waits for a second player"
    );
}
