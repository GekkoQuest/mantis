//! Guilds in the social role (std.guild): the authority applies a relayed
//! operation, makes its rows durable through the persistence writer, and
//! only then projects its updates; a restarted social role reads every
//! guild back from the writer; a resent batch is written once.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mantis_core::social::{GUILD_OP, GUILD_UPDATE, GuildChange, GuildOp, GuildUpdate, guild_rank};
use mantis_core::wire::{BoundedArray, decode_exact, encode_into};
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
    seq: u64,
}

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
            seq: 0,
        };
        c.handle
            .block_on(c.rpc.call::<methods::Presence>(
                &m::Present {
                    cell: m::CellNo(1),
                    characters: BoundedArray::from_slice(&[m::CharacterId(1), m::CharacterId(2)]).unwrap(),
                },
                T,
            ))
            .unwrap();
        c.handle
            .block_on(
                c.rpc
                    .call::<methods::Restored>(&m::RestoredCell { cell: m::CellNo(1) }, T),
            )
            .unwrap();
        c
    }

    fn relay(&mut self, op: &GuildOp) -> m::RelayAck {
        self.seq += 1;
        let mut payload = Vec::new();
        encode_into(op, &mut payload);
        let req = m::Relay {
            cell: m::CellNo(1),
            seq: self.seq,
            restore: false,
            topic: GUILD_OP,
            payload: BoundedArray::from_slice(&payload).unwrap(),
        };
        self.handle
            .block_on(self.rpc.call::<methods::RelayOp>(&req, T))
            .unwrap()
    }

    fn updates(&self) -> Vec<GuildUpdate> {
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
                if p.topic == GUILD_UPDATE {
                    let bytes: Vec<u8> = p.payload.iter().copied().collect();
                    out.push(decode_exact::<GuildUpdate>(&bytes).unwrap());
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
    cell.seq = 100;
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
