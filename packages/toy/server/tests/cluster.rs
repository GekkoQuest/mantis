//! The zone linked to every service role in one process, as
//! `toy-server cluster` runs it: module outcomes reach the persistence
//! writer, a signed Ops flag switches a module in every cell at a tick
//! boundary, and the inspector reads the cells.

#![expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]

use std::time::{Duration, Instant};

use mantis_core::log::SessionId;
use mantis_server::modules::{ExtensionKind, ModuleCommand, ModuleStates, Payload};
use mantis_services::cluster::{CellLink, CellLinkConfig, ClusterConfig, LocalCluster};
use mantis_services::host::Role;
use mantis_services::ops::Command;
use toy_server::cluster::after_tick;
use toy_server::sim::Sim;
use toy_server::tunables::Tunables;
use toy_server::world;

fn chat_enabled(sim: &Sim, cell: usize) -> bool {
    let states = sim.zone.cells()[cell].world().resource::<ModuleStates>().unwrap();
    states.is_enabled(states.id("std.chat").unwrap())
}

#[test]
fn the_zone_pushes_outcomes_and_applies_signed_live_changes() {
    let mut cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
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
            poll: Duration::from_millis(10),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap();
    for cell in [1, 2] {
        cluster.add_cell(cell, link.inspector().unwrap());
    }
    let t = Tunables::defaults().unwrap();
    let mut sim = Sim::new(t, 5, |_| None).unwrap();

    // A command (a kind no module registered: refused, but still an
    // outcome the writer records).
    let cmd = ModuleCommand {
        kind: ExtensionKind(60_000),
        session: Some(SessionId(9)),
        request: 0,
        payload: Payload::EMPTY,
    };
    assert!(sim.zone.cells()[0].commands().push(cmd));
    assert!(chat_enabled(&sim, 0) && chat_enabled(&sim, 1));
    cluster
        .execute(
            "alice",
            &Command::Flag {
                name: "std.chat".to_owned(),
                on: false,
            },
        )
        .unwrap();

    let start = Instant::now();
    let mut applied = 0;
    while chat_enabled(&sim, 0) || chat_enabled(&sim, 1) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the live change never applied"
        );
        let reports = sim.step().unwrap();
        applied += after_tick(&mut sim.zone, &link, &reports).live;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(applied, 2, "queued once into each cell");
    for _ in 0..30 {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
    }

    let start = Instant::now();
    loop {
        let stored = cluster.persist.with_store(|s| s.outcomes_of(1)).unwrap();
        if let Some(o) = stored.first() {
            assert_eq!((o.kind, o.session, o.ok), (60_000, 9, false));
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "no outcome reached the writer"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let seen = cluster.execute("alice", &Command::Inspect { cell: 2 }).unwrap();
    assert!(seen.after.starts_with("cell=2 tick="), "{}", seen.after);

    // Ops restarts: every cell keeps the flag throughout, and receives the
    // current value again from the new run.
    let addr = cluster.stop_role(Role::Ops).unwrap();
    for _ in 0..10 {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
    }
    cluster.start_ops(addr).unwrap();
    let start = Instant::now();
    let mut again = 0;
    while again < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the current value never came again"
        );
        let reports = sim.step().unwrap();
        again += after_tick(&mut sim.zone, &link, &reports).live;
        assert!(!chat_enabled(&sim, 0) && !chat_enabled(&sim, 1), "the flag held");
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..5 {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
    }
    assert_eq!(again, 2, "queued once more into each cell");
    assert!(!chat_enabled(&sim, 0) && !chat_enabled(&sim, 1));
}

fn link(cluster: &LocalCluster) -> CellLink {
    CellLink::start(
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
            poll: Duration::from_millis(10),
            instances: Vec::new(),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
        },
    )
    .unwrap()
}

#[test]
fn sessions_are_admitted_with_realm_tokens_as_their_character() {
    use mantis_server::bots::Profile;
    use mantis_server::host::AdmissionLimits;
    use mantis_server::simnet::LinkConfig;
    use mantis_services::cluster::TokenVerifier;
    use mantis_services::generated::services as m;
    use mantis_services::host::rpc::RpcClient;
    use mantis_services::methods;
    use toy_server::cluster::RealmAdmission;
    use toy_server::sim::Side;

    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let _link = link(&cluster);
    let realm = cluster.addr(Role::Realm).unwrap();
    let gateway = RpcClient::new(realm, Role::Gateway, cluster.key.clone());
    let handle = cluster.handle();
    let wait = Duration::from_secs(5);
    let character = handle
        .block_on(gateway.call::<methods::NewCharacter>(
            &m::CreateCharacter {
                account: m::AccountId(1),
                name: mantis_core::wire::WireString::new("hero").unwrap(),
            },
            wait,
        ))
        .unwrap()
        .character;
    let placed = handle
        .block_on(gateway.call::<methods::Select>(
            &m::SelectCharacter {
                account: m::AccountId(1),
                character,
            },
            wait,
        ))
        .unwrap();
    let token: Vec<u8> = placed.token.iter().copied().collect();

    let t = Tunables::defaults().unwrap();
    let mut sim = Sim::new(t, 8, |_| None).unwrap();
    let verifier = TokenVerifier::new(&handle, realm, cluster.key.clone(), &[1, 2]);
    sim.host
        .set_admission(Box::new(RealmAdmission(verifier)), AdmissionLimits::DEFAULT);
    sim.add_bot_with_token(Side::Native, Profile::Idle, LinkConfig::PERFECT, Some(&token))
        .unwrap();
    // The same token again: single use, refused.
    sim.add_bot_with_token(Side::Legacy, Profile::Idle, LinkConfig::PERFECT, Some(&token))
        .unwrap();
    sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    let start = Instant::now();
    while sim
        .bots
        .iter()
        .filter(|b| b.bot.welcomed() || b.bot.stats.refused.is_some())
        .count()
        < 3
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "verdicts never arrived"
        );
        sim.step().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(sim.host.stats.joined, 1);
    let characters: Vec<u64> = sim
        .zone
        .cells()
        .iter()
        .flat_map(|c| {
            c.world()
                .resource::<mantis_server::session::Sessions>()
                .map(|s| s.map.values().map(|x| x.character).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(characters, vec![character.0]);
}

/// std.chat's registered kinds (packages/std/modules/chat/contract).
const SAY: u16 = 1010;
const LINE: u16 = 1011;

fn whisper(target: u64, text: &str) -> Vec<u8> {
    use mantis_core::wire::{Encoder, Wire, WireString};
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out);
    e.u8(2);
    e.u64(target);
    WireString::<200>::new(text).unwrap().encode(&mut e);
    out
}

/// A received line: (channel, from, to, text).
fn line(bytes: &[u8]) -> (u8, u64, u64, String) {
    use mantis_core::wire::{Decoder, Wire, WireString};
    let mut d = Decoder::new(bytes);
    let channel = d.u8().unwrap();
    let from = d.u64().unwrap();
    let to = d.u64().unwrap();
    let text = WireString::<200>::decode(&mut d).unwrap();
    (channel, from, to, text.as_str().to_owned())
}

#[test]
fn a_whisper_crosses_cells_through_social_and_both_cells_replay_without_it() {
    use mantis_server::bots::Profile;
    use mantis_server::simnet::LinkConfig;
    use toy_server::sim::Side;

    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let link = link(&cluster);
    let t = Tunables::defaults().unwrap();
    let logs: Vec<mantis_server::cell::MemoryLog> = (0..2)
        .map(|_| mantis_server::cell::MemoryLog::default())
        .collect();
    let header = |i: usize| mantis_core::log::LogHeader {
        build: mantis_core::log::BuildId([3; 32]),
        content: t.content,
        cell: world::cell_config(&t, i, 21).id,
        seed: world::cell_config(&t, i, 21).seed,
        start_tick: mantis_core::time::Tick(1),
    };
    let mut sim = Sim::new(t, 21, |i| {
        let sink: mantis_server::cell::BoxedSink = Box::new(logs[i].clone());
        Some(mantis_core::log::LogWriter::create(sink, &header(i), 1 << 20).unwrap())
    })
    .unwrap();
    // Sessions 1 (x = -17, the first cell) and 12 (x = 3, the second).
    for _ in 0..12 {
        sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
            .unwrap();
    }
    let step = |sim: &mut Sim| {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports)
    };
    let start = Instant::now();
    while !sim.bots.iter().all(|b| b.bot.welcomed()) || sim.ticks() < 40 {
        assert!(start.elapsed() < Duration::from_secs(10));
        step(&mut sim);
    }
    // Presence reaches social on the summary ticks.
    for _ in 0..40 {
        step(&mut sim);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (a, b) = (0, 11);
    sim.bots[a]
        .bot
        .feature(ExtensionKind(SAY), &whisper(12, "across the border"));
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the whisper never arrived"
        );
        step(&mut sim);
        std::thread::sleep(Duration::from_millis(2));
        let got: Vec<_> = sim.bots[b]
            .bot
            .stats
            .extension_messages
            .iter()
            .filter(|(k, _)| *k == ExtensionKind(LINE))
            .map(|(_, bytes)| line(bytes))
            .collect();
        if let Some(l) = got.first() {
            assert_eq!(l, &(2, 1, 12, "across the border".to_owned()));
            break;
        }
    }
    // The speaker saw their own line once, at once.
    let echoes = sim.bots[a]
        .bot
        .stats
        .extension_messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(LINE))
        .count();
    assert_eq!(echoes, 1);
    for _ in 0..3 {
        step(&mut sim);
    }

    // Both cells replay from their logs alone: the line that crossed is a
    // logged intent in the receiving cell.
    drop(sim);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = mantis_core::log::LogReader::<mantis_server::intent::CellLogSchema>::open(
            &bytes,
            mantis_core::log::BuildId([3; 32]),
            t.content,
        )
        .unwrap();
        let mut cell = world::cell(&t, i, 21, world::adapters(t.content), None).unwrap();
        let report = mantis_core::replay::replay(&mut cell, &mut reader).unwrap();
        assert!(report.ticks > 0);
    }
}

/// std.party's registered kinds (packages/std/modules/party/contract).
const INVITE: u16 = 1000;
const ACCEPT: u16 = 1001;
const INVITED: u16 = 1004;
const ROSTER: u16 = 1005;

/// A roster message: (party, leader, members).
fn roster(bytes: &[u8]) -> (u32, u64, Vec<u64>) {
    use mantis_core::wire::{BoundedArray, Decoder, Wire};
    let mut d = Decoder::new(bytes);
    let party = d.u32().unwrap();
    let leader = d.u64().unwrap();
    let members = BoundedArray::<u64, 5>::decode(&mut d).unwrap();
    (party, leader, members.iter().copied().collect())
}

fn rosters_seen(sim: &Sim, bot: usize) -> Vec<(u32, u64, Vec<u64>)> {
    sim.bots[bot]
        .bot
        .stats
        .extension_messages
        .iter()
        .filter(|(k, _)| *k == ExtensionKind(ROSTER))
        .map(|(_, b)| roster(b))
        .collect()
}

#[test]
fn a_party_survives_an_instance_round_trip_and_the_instance_releases_itself() {
    use mantis_core::wire::{Encoder, Wire};
    use mantis_server::bots::Profile;
    use mantis_server::simnet::LinkConfig;
    use mantis_services::generated::services as m;
    use mantis_services::host::rpc::RpcClient;
    use mantis_services::methods;
    use toy_server::cluster::relocate;
    use toy_server::sim::Side;

    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let mut t = Tunables::defaults().unwrap();
    // Release one second after the instance empties.
    t.instance_release_grace = 1;
    let build = mantis_core::log::BuildId([4; 32]);
    let logs: Vec<mantis_server::cell::MemoryLog> = (0..3)
        .map(|_| mantis_server::cell::MemoryLog::default())
        .collect();
    let mut sim = Sim::with_instances(t, 31, 1, |i| {
        let cfg = world::cell_config(&t, i, 31);
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
    let instance_cell = sim.zone.cells()[2].id().0;
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
            instances: vec![(instance_cell, "127.0.0.1:7400".to_owned())],
            poll: Duration::from_millis(5),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
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
    // Two characters in the first world cell (sessions 1 and 2).
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    until(&mut sim, "both in the world", &|s| {
        s.bots
            .iter()
            .all(|b| b.bot.welcomed() && b.bot.avatar().is_some())
            && s.ticks() >= 31
    });

    // They form a party through the social role.
    let target = sim.bots[1].bot.avatar().unwrap();
    let mut invite = Vec::new();
    target.encode(&mut Encoder::new(&mut invite));
    sim.bots[0].bot.feature(ExtensionKind(INVITE), &invite);
    until(&mut sim, "the invitation", &|s| {
        s.bots[1]
            .bot
            .stats
            .extension_messages
            .iter()
            .any(|(k, _)| *k == ExtensionKind(INVITED))
    });
    let mut accept = Vec::new();
    Encoder::new(&mut accept).u64(1);
    sim.bots[1].bot.feature(ExtensionKind(ACCEPT), &accept);
    until(&mut sim, "the roster", &|s| {
        (0..2).all(|b| !rosters_seen(s, b).is_empty())
    });
    let (party, leader, members) = rosters_seen(&sim, 0).last().cloned().unwrap();
    assert_eq!((leader, members), (1, vec![1, 2]));
    assert_eq!(cluster.social.party_of(2), Some(party));
    let seen_before: Vec<usize> = (0..2).map(|b| rosters_seen(&sim, b).len()).collect();

    // The party queues; matchmaking places both in the instance cell.
    let gateway = RpcClient::new(
        cluster.addr(Role::Matchmaking).unwrap(),
        Role::Gateway,
        cluster.key.clone(),
    );
    for character in [1, 2] {
        cluster
            .handle()
            .block_on(gateway.call::<methods::Queue>(
                &m::Enqueue {
                    character: m::CharacterId(character),
                    queue: 1,
                },
                Duration::from_secs(5),
            ))
            .unwrap();
    }
    until(&mut sim, "both in the instance", &|s| {
        [1, 2].iter().all(|x| s.zone.route(SessionId(*x)) == Some(2))
    });
    assert!(cluster.realm.cells()[&instance_cell].busy);
    // The party is intact inside: each member is sent its roster on arrival.
    until(&mut sim, "rosters in the instance", &|s| {
        (0..2).all(|b| rosters_seen(s, b).len() > seen_before[b])
    });
    for b in 0..2 {
        assert_eq!(
            rosters_seen(&sim, b).last().cloned().unwrap(),
            (party, 1, vec![1, 2])
        );
    }
    let before: Vec<u64> = sim.bots.iter().map(|b| b.bot.stats.snapshots).collect();
    for _ in 0..60 {
        step(&mut sim);
    }
    for (b, n) in sim.bots.iter().zip(before) {
        assert!(
            b.bot.stats.snapshots > n + 30,
            "snapshots kept coming in the instance"
        );
        assert!(
            b.bot.state().position.x >= world::INSTANCE_X,
            "{:?}",
            b.bot.state().position
        );
    }

    // They come back to the world, still a party.
    let seen_inside: Vec<usize> = (0..2).map(|b| rosters_seen(&sim, b).len()).collect();
    for character in [1, 2] {
        assert!(relocate(
            &mut sim.zone,
            character,
            mantis_core::math::Vec3::new(-30.0, 0.0, 0.0)
        ));
    }
    until(&mut sim, "both back", &|s| {
        [1, 2].iter().all(|x| s.zone.route(SessionId(*x)) == Some(0))
    });
    until(&mut sim, "rosters after return", &|s| {
        (0..2).all(|b| rosters_seen(s, b).len() > seen_inside[b])
    });
    assert_eq!(
        rosters_seen(&sim, 1).last().cloned().unwrap(),
        (party, 1, vec![1, 2])
    );
    assert_eq!(cluster.social.party_of(1), Some(party));

    // Nobody releases the instance by hand: it releases itself after the
    // package's grace.
    until(&mut sim, "the instance released", &|_| {
        !cluster.realm.cells()[&instance_cell].busy
    });
    assert_eq!(sim.host.stats.joined, 2);

    // Every cell, the instance included, replays from its own log.
    drop(sim);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = mantis_core::log::LogReader::<mantis_server::intent::CellLogSchema>::open(
            &bytes, build, t.content,
        )
        .unwrap();
        let mut cell = world::cell(&t, i, 31, world::adapters(t.content), None).unwrap();
        let report = mantis_core::replay::replay(&mut cell, &mut reader).unwrap();
        assert!(report.ticks > 100, "cell {i}");
    }
}

fn last_permitted(sim: &Sim, bot: usize) -> Option<(mantis_adapter_contract::ModTier, Vec<String>)> {
    sim.bots[bot].bot.stats.permitted.last().cloned()
}

#[test]
fn a_competitive_instance_permits_presentation_modules_only_and_replays() {
    use mantis_adapter_contract::ModTier;
    use mantis_server::bots::Profile;
    use mantis_server::cell::ModPolicy;
    use mantis_server::simnet::LinkConfig;
    use mantis_services::generated::services as m;
    use mantis_services::host::rpc::RpcClient;
    use mantis_services::methods;
    use toy_server::cluster::relocate;
    use toy_server::sim::Side;

    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let t = Tunables::defaults().unwrap();
    let build = mantis_core::log::BuildId([5; 32]);
    let logs: Vec<mantis_server::cell::MemoryLog> = (0..3)
        .map(|_| mantis_server::cell::MemoryLog::default())
        .collect();
    let mut sim = Sim::with_instances(t, 32, 1, |i| {
        let cfg = world::cell_config(&t, i, 32);
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
    let instance_cell = sim.zone.cells()[2].id().0;
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
            instances: vec![(instance_cell, "127.0.0.1:7400".to_owned())],
            poll: Duration::from_millis(5),
            inspector: "127.0.0.1:0".parse().unwrap(),
            tls: None,
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
    let mods = world::client_mods();
    assert_eq!(mods, ["toy.hud", "toy.helper"]);
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    // On joining the open world, each session is sent the package's
    // client modules at the automation tier.
    until(&mut sim, "both in the world", &|s| {
        (0..2).all(|b| last_permitted(s, b).is_some())
    });
    for b in 0..2 {
        assert_eq!(
            last_permitted(&sim, b).unwrap(),
            (ModTier::Automation, mods.clone())
        );
    }

    // Both queue for the competitive queue; the instance is set to the
    // presentation tier before they arrive, and they are told on arrival.
    let gateway = RpcClient::new(
        cluster.addr(Role::Matchmaking).unwrap(),
        Role::Gateway,
        cluster.key.clone(),
    );
    for character in [1, 2] {
        cluster
            .handle()
            .block_on(gateway.call::<methods::Queue>(
                &m::Enqueue {
                    character: m::CharacterId(character),
                    queue: world::COMPETITIVE_QUEUE,
                },
                Duration::from_secs(5),
            ))
            .unwrap();
    }
    until(&mut sim, "both in the instance", &|s| {
        [1, 2].iter().all(|x| s.zone.route(SessionId(*x)) == Some(2))
    });
    until(&mut sim, "the presentation tier", &|s| {
        (0..2).all(|b| last_permitted(s, b).is_some_and(|p| p.0 == ModTier::Presentation))
    });
    for b in 0..2 {
        let seen = &sim.bots[b].bot.stats.permitted;
        assert_eq!(
            seen.last().cloned().unwrap(),
            (ModTier::Presentation, mods.clone())
        );
        // Never told automation inside the instance: the tier was set
        // before the transfer completed.
        let inside = seen.iter().rposition(|p| p.0 == ModTier::Automation).unwrap();
        assert_eq!(inside, seen.len() - 2, "{seen:?}");
    }
    assert_eq!(
        sim.zone.cells()[2].world().resource::<ModPolicy>().unwrap().tier,
        ModTier::Presentation
    );
    assert_eq!(
        sim.zone.cells()[0].world().resource::<ModPolicy>().unwrap().tier,
        ModTier::Automation,
        "the open world is untouched"
    );

    // Back in the world, automation is permitted again.
    for character in [1, 2] {
        assert!(relocate(
            &mut sim.zone,
            character,
            mantis_core::math::Vec3::new(-30.0, 0.0, 0.0)
        ));
    }
    until(&mut sim, "automation again", &|s| {
        (0..2).all(|b| {
            s.zone.route(SessionId(b as u64 + 1)) == Some(0)
                && last_permitted(s, b).is_some_and(|p| p.0 == ModTier::Automation)
        })
    });
    for _ in 0..10 {
        step(&mut sim);
    }

    // The tier is a logged intent: every cell replays to the same state,
    // the instance included.
    let hashes: Vec<_> = sim.zone.cells().iter().map(|c| c.world().state_hash()).collect();
    drop(sim);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = mantis_core::log::LogReader::<mantis_server::intent::CellLogSchema>::open(
            &bytes, build, t.content,
        )
        .unwrap();
        let mut cell = world::cell(&t, i, 32, world::adapters(t.content), None).unwrap();
        let report = mantis_core::replay::replay(&mut cell, &mut reader).unwrap();
        assert!(report.ticks > 30, "cell {i}");
        assert_eq!(cell.world().state_hash(), hashes[i], "cell {i}");
        if i == 2 {
            assert_eq!(
                cell.world().resource::<ModPolicy>().unwrap().tier,
                ModTier::Presentation
            );
        }
    }
}

#[test]
fn ops_kicks_drains_and_traces_ledgers_through_the_cell_host() {
    use mantis_adapter_contract::RefuseReason;
    use mantis_server::bots::Profile;
    use mantis_server::simnet::LinkConfig;
    use toy_server::cluster::OpsApplier;
    use toy_server::sim::Side;

    let cluster = LocalCluster::start(&ClusterConfig::local()).unwrap();
    let link = link(&cluster);
    for cell in [1, 2] {
        cluster.add_cell(cell, link.inspector().unwrap());
    }
    let t = Tunables::defaults().unwrap();
    let mut sim = Sim::new(t, 41, |_| None).unwrap();
    let mut ops = OpsApplier::default();
    let mut step = |sim: &mut Sim| {
        let reports = sim.step().unwrap();
        after_tick(&mut sim.zone, &link, &reports);
        ops.apply(&mut sim.host, &mut sim.zone, &link, 30);
        std::thread::sleep(Duration::from_millis(1));
    };
    for _ in 0..3 {
        sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
            .unwrap();
    }
    // Presence is reported on summary ticks; wait for it.
    while sim.ticks() < 70 {
        step(&mut sim);
    }
    assert_eq!(sim.host.sessions_in_world(), 3);

    // Kick character 2: its session ends at the next tick.
    let k = cluster.execute("alice", &Command::Kick { character: 2 }).unwrap();
    assert!(k.undo.starts_with("none: "));
    for _ in 0..5 {
        step(&mut sim);
    }
    assert_eq!(sim.host.sessions_in_world(), 2);
    assert!(
        cluster
            .execute("alice", &Command::Kick { character: 99 })
            .is_err()
    );

    // Drain with a 1 s grace: new sessions are refused at once, the rest
    // end after the grace.
    let d = cluster
        .execute(
            "alice",
            &Command::Drain {
                on: true,
                grace_seconds: 1,
            },
        )
        .unwrap();
    assert!(d.undo.starts_with("drain on=false"));
    assert!(cluster.account.maintenance());
    for _ in 0..3 {
        step(&mut sim);
    }
    sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    for _ in 0..5 {
        step(&mut sim);
    }
    assert_eq!(sim.bots[3].bot.stats.refused, Some(RefuseReason::Maintenance));
    assert_eq!(sim.host.sessions_in_world(), 2, "the grace is not over");
    for _ in 0..40 {
        step(&mut sim);
    }
    assert_eq!(sim.host.sessions_in_world(), 0);
    cluster
        .execute(
            "alice",
            &Command::Drain {
                on: false,
                grace_seconds: 0,
            },
        )
        .unwrap();
    for _ in 0..3 {
        step(&mut sim);
    }
    assert!(!sim.host.in_maintenance());
    assert!(!cluster.account.maintenance());

    // The ledger trace is read-only and audited.
    let trace = cluster
        .execute("alice", &Command::LedgerTrace { character: 1 })
        .unwrap();
    assert_eq!(trace.undo, "none: read-only");
    let rows = cluster.handle().block_on(cluster.ops.audit_rows()).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.command.as_str()).collect();
    assert_eq!(names, ["kick", "kick", "drain", "drain", "ledger"]);
    assert_eq!(rows[1].status, "failed");
}
