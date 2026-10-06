//! std.guild on the toy cluster (M9): two characters found and join a guild
//! through the social role, one moves to the other cell and is told its
//! guild there, guild chat crosses cells, a rank change reaches both, the
//! social role restarts and reads every guild back from the persistence
//! writer, guild chat still works after it, and every cell replays from its
//! own log.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use std::time::{Duration, Instant};

use mantis_core::log::SessionId;
use mantis_core::social::guild_rank;
use mantis_core::wire::{Decoder, Encoder, Wire, WireString};
use mantis_server::bots::Profile;
use mantis_server::modules::ExtensionKind;
use mantis_server::simnet::LinkConfig;
use mantis_services::cluster::{CellLink, CellLinkConfig, ClusterConfig, LocalCluster};
use mantis_services::host::Role;
use toy_server::cluster::{after_tick, relocate};
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

/// std.guild's registered kinds (packages/std/modules/guild/contract).
const CREATE: u16 = 1080;
const INVITE: u16 = 1081;
const ACCEPT: u16 = 1082;
const SET_RANK: u16 = 1085;
const JOINED: u16 = 1087;
const CHANGED: u16 = 1089;
const INVITED: u16 = 1092;
/// std.chat's kinds and guild channel.
const SAY: u16 = 1010;
const LINE: u16 = 1011;
const GUILD_CHANNEL: u8 = 3;

fn seen(sim: &Sim, bot: usize, kind: u16) -> Vec<Vec<u8>> {
    sim.bots[bot]
        .bot
        .stats
        .extension_messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(kind))
        .map(|(_, b)| b.clone())
        .collect()
}

/// A guild line: (from, text).
fn guild_lines(sim: &Sim, bot: usize) -> Vec<(u64, String)> {
    seen(sim, bot, LINE)
        .iter()
        .filter_map(|b| {
            let mut d = Decoder::new(b);
            let channel = d.u8().ok()?;
            let from = d.u64().ok()?;
            let _to = d.u64().ok()?;
            let text = WireString::<200>::decode(&mut d).ok()?;
            (channel == GUILD_CHANNEL).then(|| (from, text.as_str().to_owned()))
        })
        .collect()
}

fn say_guild(sim: &mut Sim, bot: usize, text: &str) {
    let mut b = Vec::new();
    let mut e = Encoder::new(&mut b);
    e.u8(GUILD_CHANNEL);
    e.u64(0);
    WireString::<200>::new(text).unwrap().encode(&mut e);
    sim.bots[bot].bot.feature(ExtensionKind(SAY), &b);
}

#[test]
fn a_guild_spans_cells_survives_a_social_restart_and_every_cell_replays() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let t = Tunables::defaults().unwrap();
    let build = mantis_core::log::BuildId([9; 32]);
    let logs: Vec<mantis_server::cell::MemoryLog> = (0..2)
        .map(|_| mantis_server::cell::MemoryLog::default())
        .collect();
    let mut sim = Sim::new(t, 41, |i| {
        let cfg = world::cell_config(&t, i, 41);
        let header = mantis_core::log::LogHeader {
            build,
            content: t.content,
            cell: cfg.id,
            seed: cfg.seed,
            start_tick: mantis_core::time::Tick(1),
        };
        let sink: mantis_server::cell::BoxedSink = Box::new(logs[i].clone());
        Some(mantis_core::log::LogWriter::create(sink, &header, 1 << 22).unwrap())
    })
    .unwrap();
    let link = CellLink::start(
        &cluster.handle(),
        &CellLinkConfig {
            key: cluster.key.clone(),
            persist: cluster.addr(Role::Persist).unwrap(),
            ops: cluster.addr(Role::Ops).unwrap(),
            social: cluster.addr(Role::Social).unwrap(),
            matchmaking: cluster.addr(Role::Matchmaking).unwrap(),
            realm: cluster.addr(Role::Realm).unwrap(),
            live_key: cluster.ops.public_key(),
            cells: world::regions()
                .into_iter()
                .enumerate()
                .map(|(i, r)| (i as u64 + 1, "127.0.0.1:7400".to_owned(), r))
                .collect(),
            instances: Vec::new(),
            poll: Duration::from_millis(5),
            inspector: "127.0.0.1:0".parse().unwrap(),
        },
    )
    .unwrap();
    let step = |sim: &mut Sim| {
        let reports = sim.step().unwrap();
        std::thread::sleep(Duration::from_millis(1));
        after_tick(&mut sim.zone, &link, &reports)
    };
    let until = |sim: &mut Sim, what: &str, done: &dyn Fn(&Sim) -> bool| {
        let start = Instant::now();
        while !done(sim) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out: {what}");
            step(sim);
        }
    };
    // Characters 1 and 2 in the first world cell.
    for _ in 0..2 {
        sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
            .unwrap();
    }
    until(&mut sim, "both in the world", &|s| {
        s.bots.iter().all(|b| b.bot.avatar().is_some()) && s.ticks() >= 31
    });

    // 1 founds a guild; the name is checked in the cell and by social.
    let mut create = Vec::new();
    WireString::<24>::new("Lamplighters")
        .unwrap()
        .encode(&mut Encoder::new(&mut create));
    sim.bots[0].bot.feature(ExtensionKind(CREATE), &create);
    until(&mut sim, "founded", &|s| !seen(s, 0, JOINED).is_empty());
    let joined = seen(&sim, 0, JOINED);
    let mut d = Decoder::new(&joined[0]);
    let guild = d.u32().unwrap();
    assert_eq!(WireString::<24>::decode(&mut d).unwrap().as_str(), "Lamplighters");
    assert_eq!(d.u8().unwrap(), guild_rank::LEADER);
    assert_eq!(cluster.social.guild_of(1), Some(guild));

    // 1 invites 2, who accepts; 1 is told of its new member.
    let target = sim.bots[1].bot.avatar().unwrap();
    let mut invite = Vec::new();
    target.encode(&mut Encoder::new(&mut invite));
    sim.bots[0].bot.feature(ExtensionKind(INVITE), &invite);
    until(&mut sim, "the invitation", &|s| !seen(s, 1, INVITED).is_empty());
    let mut accept = Vec::new();
    Encoder::new(&mut accept).u32(guild);
    sim.bots[1].bot.feature(ExtensionKind(ACCEPT), &accept);
    until(&mut sim, "joined", &|s| {
        !seen(s, 1, JOINED).is_empty() && !seen(s, 0, CHANGED).is_empty()
    });
    assert_eq!(cluster.social.guild_rank(2), Some(guild_rank::MEMBER));

    // 2 moves to the other cell and is told its guild there.
    let joins_before = seen(&sim, 1, JOINED).len();
    assert!(relocate(
        &mut sim.zone,
        2,
        mantis_core::math::Vec3::new(30.0, 0.0, 0.0)
    ));
    until(&mut sim, "2 in cell 2", &|s| {
        s.zone.route(SessionId(2)) == Some(1)
    });
    until(&mut sim, "the guild on arrival", &|s| {
        seen(s, 1, JOINED).len() > joins_before
    });

    // Guild chat crosses cells through the social role.
    say_guild(&mut sim, 0, "meet at the gate");
    until(&mut sim, "the guild line in cell 2", &|s| {
        guild_lines(s, 1).contains(&(1, "meet at the gate".to_owned()))
    });
    assert!(
        guild_lines(&sim, 0).contains(&(1, "meet at the gate".to_owned())),
        "the speaker sees its own line"
    );

    // A rank change reaches the member in the other cell.
    let mut rank = Vec::new();
    let mut e = Encoder::new(&mut rank);
    e.u64(2);
    e.u8(guild_rank::OFFICER);
    sim.bots[0].bot.feature(ExtensionKind(SET_RANK), &rank);
    until(&mut sim, "the officer", &|s| {
        seen(s, 1, CHANGED).iter().any(|b| {
            let mut d = Decoder::new(b);
            (d.u32().ok(), d.u64().ok(), d.u8().ok()) == (Some(guild), Some(2), Some(guild_rank::OFFICER))
        })
    });

    // The social role restarts: guilds come back from the writer.
    let addr = cluster.stop_social().unwrap();
    for _ in 0..10 {
        step(&mut sim);
    }
    cluster.start_social(addr).unwrap();
    assert_eq!(cluster.social.guild_of(2), Some(guild));
    assert_eq!(cluster.social.guild_rank(2), Some(guild_rank::OFFICER));
    // Guild chat works again once presence is reported.
    for _ in 0..40 {
        step(&mut sim);
    }
    say_guild(&mut sim, 0, "still here");
    until(&mut sim, "a guild line after the restart", &|s| {
        guild_lines(s, 1).contains(&(1, "still here".to_owned()))
    });
    for _ in 0..30 {
        step(&mut sim);
    }

    // Every cell replays from its own log: guild state in a cell is a
    // projection fed by logged updates.
    drop(sim);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = mantis_core::log::LogReader::<mantis_server::intent::CellLogSchema>::open(
            &bytes, build, t.content,
        )
        .unwrap();
        let mut cell = world::cell(&t, i, 41, world::adapters(t.content), None).unwrap();
        let report = mantis_core::replay::replay(&mut cell, &mut reader).unwrap();
        assert!(report.ticks > 100, "cell {i}");
    }
}
