//! Durable social state (std.guild, std.friends): the authority applies a
//! relayed operation, makes its rows durable through the persistence
//! writer, and only then projects its updates; a restarted social role
//! reads every guild, friendship, and open request back from the writer,
//! including those of characters who are all offline; a resent batch is
//! written once.

#![expect(clippy::unwrap_used)]

use std::time::Duration;

use mantis_core::social::{
    FRIEND_OP, FRIEND_UPDATE, FriendChange, FriendOp, FriendUpdate, GUILD_OP, GUILD_UPDATE, GuildChange,
    GuildOp, GuildUpdate, guild_rank,
};
use mantis_core::wire::{BoundedArray, Wire, decode_exact, encode_into};
use mantis_services::cluster::{ClusterConfig, LocalCluster};
use mantis_services::generated::services as m;
use mantis_services::host::Role;
use mantis_services::host::rpc::RpcClient;
use mantis_services::methods;
use mantis_services::persist::guild_row;

const T: Duration = Duration::from_secs(5);

struct Cell {
    rpc: RpcClient,
    handle: tokio::runtime::Handle,
    /// The relaying link's run: a new `Cell` is a restarted host.
    run: u64,
    seq: u64,
}

static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Cell {
    fn new(cluster: &LocalCluster) -> Self {
        let rpc = RpcClient::new(
            cluster.addr(Role::Social).unwrap(),
            Role::Cell,
            cluster.key.clone(),
        );
        let c = Self {
            rpc,
            handle: cluster.handle(),
            run: RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            seq: 0,
        };
        c.present(&[1, 2]);
        c.handle
            .block_on(
                c.rpc
                    .call::<methods::Restored>(&m::RestoredCell { cell: m::CellNo(1) }, T),
            )
            .unwrap();
        c
    }

    /// Reports the characters in cell 1.
    fn present(&self, characters: &[u64]) {
        let ids: Vec<m::CharacterId> = characters.iter().map(|c| m::CharacterId(*c)).collect();
        self.handle
            .block_on(self.rpc.call::<methods::Presence>(
                &m::Present {
                    cell: m::CellNo(1),
                    characters: BoundedArray::from_slice(&ids).unwrap(),
                },
                T,
            ))
            .unwrap();
    }

    fn relay(&mut self, op: &GuildOp) -> m::RelayAck {
        self.relay_on(GUILD_OP, op)
    }

    fn relay_on(&mut self, topic: u16, op: &impl Wire) -> m::RelayAck {
        self.seq += 1;
        let mut payload = Vec::new();
        encode_into(op, &mut payload);
        let req = m::Relay {
            cell: m::CellNo(1),
            run: self.run,
            seq: self.seq,
            restore: false,
            topic,
            payload: BoundedArray::from_slice(&payload).unwrap(),
        };
        self.handle
            .block_on(self.rpc.call::<methods::RelayOp>(&req, T))
            .unwrap()
    }

    fn updates(&self) -> Vec<GuildUpdate> {
        self.updates_on(GUILD_UPDATE)
    }

    fn updates_on<U: Wire>(&self, topic: u16) -> Vec<U> {
        let mut out = Vec::new();
        let mut since = 0;
        loop {
            let got = self
                .handle
                .block_on(self.rpc.call::<methods::Projection>(
                    &m::PollProjections {
                        cell: m::CellNo(1),
                        since,
                    },
                    T,
                ))
                .unwrap();
            if got.items.is_empty() {
                return out;
            }
            for p in got.items.iter() {
                since = p.seq;
                if p.topic == topic {
                    let bytes: Vec<u8> = p.payload.iter().copied().collect();
                    out.push(decode_exact::<U>(&bytes).unwrap());
                }
            }
        }
    }
}

#[test]
fn guild_changes_are_durable_before_they_are_told_and_survive_a_restart() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let mut cell = Cell::new(&cluster);
    assert!(
        cell.relay(&GuildOp::Create {
            me: 1,
            name: "Lamplighters".to_owned(),
        })
        .applied
    );
    cell.relay(&GuildOp::Invite { me: 1, to: 2 });
    cell.relay(&GuildOp::Accept { me: 2, guild: 1 });
    cell.relay(&GuildOp::SetRank {
        me: 1,
        who: 2,
        rank: guild_rank::OFFICER,
    });
    assert_eq!(cluster.social.guild_of(2), Some(1));

    // Every update the cell was told is backed by a durable row.
    let told = cell.updates();
    assert!(told.contains(&GuildUpdate::Joined {
        to: 2,
        guild: 1,
        name: "Lamplighters".to_owned(),
        rank: guild_rank::MEMBER
    }));
    assert!(told.contains(&GuildUpdate::Member {
        to: 2,
        guild: 1,
        member: 2,
        rank: guild_rank::OFFICER
    }));
    let rows = cluster.persist.with_store(|s| s.guild_rows()).unwrap();
    assert_eq!(
        rows,
        vec![
            GuildChange::Guild {
                id: 1,
                name: "Lamplighters".to_owned()
            },
            GuildChange::Member {
                guild: 1,
                character: 1,
                rank: guild_rank::LEADER,
                since: 1
            },
            GuildChange::Member {
                guild: 1,
                character: 2,
                rank: guild_rank::OFFICER,
                since: 2
            },
        ]
    );
    let seq = cluster.persist.with_store(|s| s.guild_seq()).unwrap();
    assert_eq!(seq, 3, "three operations changed rows; the invitation did not");

    // A resent batch is written once.
    let again = m::StoreGuildRows {
        epoch: 0,
        seq,
        rows: BoundedArray::from_slice(&[guild_row(&GuildChange::NoGuild { id: 1 })]).unwrap(),
    };
    let durable = cluster.persist.write_guilds(&again).unwrap();
    assert_eq!(durable.seq, seq);
    assert_eq!(cluster.persist.with_store(|s| s.guild_rows()).unwrap().len(), 3);

    // The social role restarts: parties and friends start empty, guilds
    // are read back from the writer, and new batches number past the old.
    let addr = cluster.stop_social().unwrap();
    cluster.start_social(addr).unwrap();
    assert_eq!(cluster.social.guild_of(2), Some(1));
    assert_eq!(cluster.social.guild_rank(1), Some(guild_rank::LEADER));
    assert_eq!(cluster.social.guild_rank(2), Some(guild_rank::OFFICER));
    let mut cell = Cell::new(&cluster);
    cell.relay(&GuildOp::Leave { me: 1 });
    assert_eq!(
        cluster.social.guild_rank(2),
        Some(guild_rank::LEADER),
        "the officer leads"
    );
    assert_eq!(cluster.persist.with_store(|s| s.guild_seq()).unwrap(), 4);
    // Arriving (presence) told character 2 its guild again in the new run.
    assert!(
        cell.updates()
            .iter()
            .any(|u| matches!(u, GuildUpdate::Joined { to: 2, guild: 1, .. }))
    );
}

#[test]
fn friendships_and_requests_of_offline_characters_survive_a_social_restart() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let mut cell = Cell::new(&cluster);
    // 1 and 2 become friends; 1 asks 3, who has never been online.
    cell.relay_on(FRIEND_OP, &FriendOp::Request { me: 1, other: 2 });
    cell.relay_on(FRIEND_OP, &FriendOp::Request { me: 2, other: 1 });
    cell.relay_on(FRIEND_OP, &FriendOp::Request { me: 1, other: 3 });
    assert!(cluster.social.are_friends(1, 2));
    let told: Vec<FriendUpdate> = cell.updates_on(FRIEND_UPDATE);
    assert!(told.contains(&FriendUpdate::List {
        to: 2,
        friends: vec![1]
    }));
    // Every change told was durable first: the friendship, the request to
    // 3, and the closed request between 1 and 2.
    assert_eq!(
        cluster.persist.with_store(|s| s.friend_rows()).unwrap(),
        vec![
            FriendChange::Friends { a: 1, b: 2 },
            FriendChange::Asked { asker: 1, asked: 3 },
        ]
    );
    assert_eq!(cluster.persist.with_store(|s| s.friend_seq()).unwrap(), 3);

    // Everyone logs off; the social role restarts and nobody is online to
    // offer anything back.
    cell.present(&[]);
    let addr = cluster.stop_social().unwrap();
    cluster.start_social(addr).unwrap();
    assert!(cluster.social.are_friends(2, 1), "offline friends survive");

    // 3 logs in for the first time and accepts the request made while it
    // had never been online; 1 arriving is told its whole list.
    let mut cell = Cell::new(&cluster);
    cell.present(&[1, 3]);
    cell.relay_on(
        FRIEND_OP,
        &FriendOp::Respond {
            me: 3,
            other: 1,
            accept: true,
        },
    );
    assert!(cluster.social.are_friends(1, 3));
    let told: Vec<FriendUpdate> = cell.updates_on(FRIEND_UPDATE);
    assert!(told.contains(&FriendUpdate::List {
        to: 1,
        friends: vec![2, 3]
    }));
    assert_eq!(
        cluster.persist.with_store(|s| s.friend_seq()).unwrap(),
        4,
        "batches number past the old"
    );
}

#[test]
fn a_restarted_cell_host_relays_from_seq_1_again() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let mut first = Cell::new(&cluster);
    for _ in 0..5 {
        first.relay_on(FRIEND_OP, &FriendOp::Show { me: 1 });
    }
    // A resent operation of the same run is applied once.
    first.seq -= 1;
    assert!(!first.relay_on(FRIEND_OP, &FriendOp::Show { me: 1 }).applied);
    // The host restarts: its relays count from 1 under a new run, and apply.
    let mut restarted = Cell::new(&cluster);
    assert!(
        restarted
            .relay_on(FRIEND_OP, &FriendOp::Request { me: 1, other: 2 })
            .applied
    );
    let _ = cluster.stop_social();
}
