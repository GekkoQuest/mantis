//! The toy server end to end: headless bots log in through both adapters
//! over a simulated network and play. Budget rows proven here (plan 17):
//!
//! - correction magnitude, Predictive, p99 under 10 cm at 100 ms RTT, 2% loss;
//! - Validated false-rejection rate, honest bots, under 0.1% of claims at
//!   100 ms RTT, 2% loss;
//! - Validated detection of a 20% speed violation within 2 ticks;
//! - `border-party`: a transfer completes within 2 ticks and no observer
//!   loses the entity for more than 1 tick;
//! - replay: every cell replays from its own log with no divergence.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use mantis_adapter_contract::core_types::*;
use mantis_core::log::{BuildId, CellId, LogHeader, LogReader, LogWriter, SessionId};
use mantis_core::replay::replay;
use mantis_core::rng::Seed;
use mantis_server::bots::Profile;
use mantis_server::cell::{BoxedSink, MemoryLog};
use mantis_server::components::{Body, ReplicationId};
use mantis_server::intent::CellLogSchema;
use mantis_server::simnet::LinkConfig;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;
use toy_server::world;

fn p99(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * 0.99) as usize]
}

fn sim(seed: u64) -> Sim {
    Sim::new(Tunables::defaults().unwrap(), seed, |_| None).unwrap()
}

#[test]
fn honest_bots_at_100ms_rtt_and_2pct_loss() {
    let mut s = sim(1);
    for _ in 0..24 {
        s.add_bot(Side::Native, Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
        s.add_bot(Side::Legacy, Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    let mut transfers = 0;
    for _ in 0..900 {
        s.step().unwrap();
        transfers += s.zone.transfers.iter().filter(|t| t.issued).count();
    }
    assert_eq!(s.host.stats.joined, 48);
    assert_eq!(s.host.stats.refused_handshakes, 0);
    assert_eq!(s.host.stats.adapter_errors, 0);
    assert_eq!(s.host.stats.invalid, 0);
    assert!(transfers > 0, "play crossed the border");

    let mut reconcile = Vec::new();
    let (mut claims, mut corrections) = (0u64, 0u64);
    for b in &s.bots {
        assert!(b.bot.synced(), "every bot entered the world");
        assert!(b.bot.stats.snapshots > 700, "snapshots flow");
        match b.side {
            Side::Native => reconcile.extend_from_slice(&b.bot.stats.reconcile),
            Side::Legacy => {
                claims += b.bot.stats.claims;
                corrections += b.bot.stats.corrections;
            }
        }
    }
    let p = p99(reconcile);
    eprintln!("budget: Predictive correction p99 = {p} m (target < 0.10)");
    assert!(p < 0.10);
    let cheats: u64 = (1..=48u64)
        .filter_map(|id| {
            let sid = SessionId(id);
            let cell = s.zone.route(sid)?;
            s.zone.cells()[cell].session(sid).map(|x| u64::from(x.cheats))
        })
        .sum();
    let rate = cheats.max(corrections) as f64 / claims as f64;
    eprintln!(
        "budget: Validated false rejections = {cheats} counted, {corrections} corrections, {claims} claims, rate {rate} (target < 0.001)"
    );
    assert!(claims > 24 * 800);
    assert!(rate < 0.001);
}

#[test]
fn a_20pct_speed_violation_is_detected_within_2_ticks() {
    let mut s = sim(2);
    let honest = s
        .add_bot(Side::Legacy, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    let speed = s
        .add_bot(Side::Legacy, Profile::SpeedHack(1.2), LinkConfig::PERFECT)
        .unwrap();
    let teleport = s
        .add_bot(
            Side::Legacy,
            Profile::Teleport {
                every: 90,
                distance: 6.0,
            },
            LinkConfig::PERFECT,
        )
        .unwrap();
    for _ in 0..300 {
        s.step().unwrap();
    }
    assert_eq!(s.bots[honest].bot.stats.corrections, 0);
    // Client ticks from the first claim beyond honest speed to receiving the
    // correction, on a zero-latency link: the server caught it on the tick
    // the claim arrived.
    let d = s.bots[speed]
        .bot
        .stats
        .detection_ticks
        .expect("speed hack detected");
    eprintln!("budget: 20% speed violation corrected {d} ticks after the claim (target <= 2)");
    assert!(d <= 2);
    assert!(
        s.bots[teleport].bot.stats.corrections >= 3,
        "every jump is corrected"
    );
    assert!(s.bots[teleport].bot.stats.detection_ticks.unwrap() <= 2);
}

#[test]
fn border_party_observers_never_lose_a_crossing_entity() {
    let mut s = sim(3);
    // Spawn points straddle the border (x = 0). Session ids follow
    // connection order: walkers are sessions 1 and 2, observers after.
    let native_walker = s
        .add_bot(
            Side::Native,
            Profile::Heading(Angle16(16_384)),
            LinkConfig::RTT100_LOSS2,
        )
        .unwrap();
    let legacy_walker = s
        .add_bot(
            Side::Legacy,
            Profile::Heading(Angle16(16_384)),
            LinkConfig::RTT100_LOSS2,
        )
        .unwrap();
    let mut observers = Vec::new();
    for side in [Side::Native, Side::Legacy, Side::Native, Side::Legacy] {
        observers.push(s.add_bot(side, Profile::Idle, LinkConfig::RTT100_LOSS2).unwrap());
    }
    // Let everyone log in, then watch the walkers.
    for _ in 0..20 {
        s.step().unwrap();
    }
    let walkers: Vec<EntityId> = [native_walker, legacy_walker]
        .iter()
        .map(|i| s.bots[*i].bot.avatar().unwrap())
        .collect();
    for (k, o) in observers.iter().enumerate() {
        s.bots[*o].bot.watch(walkers[k % 2]);
    }
    let mut issued = Vec::new();
    for step in 0..240u64 {
        s.step().unwrap();
        for t in s.zone.transfers.iter().filter(|t| t.issued) {
            issued.push((step, *t));
        }
        // Each transfer completes within 2 ticks: the destination owns the
        // entity and the source has released it.
        for (at, t) in &issued {
            if step == at + 2 {
                assert!(
                    s.zone.cells()[t.to].local(t.repl).is_some(),
                    "destination owns it"
                );
                assert!(
                    s.zone.cells()[t.from].local(t.repl).is_none(),
                    "source released it"
                );
            }
        }
    }
    let crossed: Vec<ReplicationId> = issued.iter().map(|(_, t)| t.repl).collect();
    for w in &walkers {
        assert!(crossed.contains(&ReplicationId(*w)), "walker {w:?} crossed");
    }
    for o in &observers {
        let seen = &s.bots[*o].bot.stats.watched;
        let first = seen.iter().position(|x| *x).expect("observer saw the walker");
        let mut gap = 0;
        let mut worst = 0;
        for x in &seen[first..] {
            gap = if *x { 0 } else { gap + 1 };
            worst = worst.max(gap);
        }
        // Once the walker runs out of interest range it leaves view for
        // good; count only gaps that end.
        let tail = seen.iter().rev().take_while(|x| !**x).count();
        let lost_mid_way = seen[first..seen.len() - tail].iter().filter(|x| !**x).count();
        eprintln!("budget: border-party observer {o}: {lost_mid_way} ticks without the walker (target <= 1)");
        assert!(
            lost_mid_way <= 1,
            "observer {o} lost the walker (worst gap {worst})"
        );
    }
}

#[test]
fn every_cell_replays_from_its_own_log() {
    let t = Tunables::defaults().unwrap();
    let build = BuildId([3; 32]);
    let logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let seed = 9;
    let mut s = Sim::new(t, seed, |i| {
        let cfg = world::cell_config(&t, i, seed);
        let header = LogHeader {
            build,
            content: t.content,
            cell: cfg.id,
            seed: cfg.seed,
            start_tick: Tick(1),
        };
        let sink: BoxedSink = Box::new(logs[i].clone());
        Some(LogWriter::create(sink, &header, 1 << 16).unwrap())
    })
    .unwrap();
    for k in 0..12 {
        let side = if k % 2 == 0 { Side::Native } else { Side::Legacy };
        let profile = if k < 4 {
            Profile::Heading(Angle16(16_384))
        } else {
            Profile::Honest
        };
        s.add_bot(side, profile, LinkConfig::RTT100_LOSS2).unwrap();
    }
    let mut transfers = 0;
    for _ in 0..450 {
        s.step().unwrap();
        transfers += s.zone.transfers.iter().filter(|t| t.issued).count();
    }
    assert!(transfers > 0, "the log covers transfers");
    let finals: Vec<u64> = s.zone.cells().iter().map(|c| c.world().state_hash()).collect();
    drop(s);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = LogReader::<CellLogSchema>::open(&bytes, build, t.content).unwrap();
        assert_eq!(reader.header().cell, CellId(i as u64 + 1));
        assert_eq!(reader.header().seed, Seed(seed ^ (i as u64 + 1)));
        let mut fresh = world::cell(&t, i, seed, world::adapters(t.content), None).unwrap();
        let report = replay(&mut fresh, &mut reader).unwrap();
        assert_eq!(report.ticks, 450);
        assert_eq!(
            fresh.world().state_hash(),
            finals[i],
            "cell {i} replays to the same state"
        );
    }
}

/// The server-side position of a bot's avatar.
fn server_position(s: &Sim, bot: usize) -> Option<Vec3> {
    let avatar = s.bots[bot].bot.avatar()?;
    s.zone.cells().iter().find_map(|c| {
        let e = c.local(ReplicationId(avatar))?;
        c.world().get::<Body>(e).map(|b| b.0.position)
    })
}

#[test]
fn a_relapsing_teleporter_is_corrected_every_time_and_never_gains_ground() {
    let t = Tunables::defaults().unwrap();
    let dt = 1.0 / t.tick_rate.hz() as f32;
    let honest_step = t.motion.run_speed * (1.0 + t.envelope.speed_tolerance) * dt;
    for (link, distance) in [
        (LinkConfig::PERFECT, 10.0f32),
        (LinkConfig::RTT100_LOSS2, 10.0),
        // A jump smaller than a few ticks of running: indistinguishable from
        // running, so it must cost the time running would.
        (LinkConfig::PERFECT, 0.8),
    ] {
        let mut s = sim(4);
        let honest = s.add_bot(Side::Legacy, Profile::Honest, link).unwrap();
        let cheat = s
            .add_bot(Side::Legacy, Profile::Relapse { distance }, link)
            .unwrap();
        let mut start: Option<(u64, Vec3)> = None;
        let mut farthest = 0.0f32;
        for step in 0..600u64 {
            s.step().unwrap();
            let Some(p) = server_position(&s, cheat) else {
                continue;
            };
            let (t0, p0) = *start.get_or_insert((step, p));
            let gained = (p - p0).horizontal().length();
            farthest = farthest.max(gained);
            // Never ahead of an honest runner at full speed since spawn.
            let bound = honest_step * (step - t0) as f32 + t.envelope.distance_slack;
            assert!(
                gained <= bound,
                "{link:?} {distance} m: gained {gained} m by step {step}, bound {bound}"
            );
        }
        let b = &s.bots[cheat].bot.stats;
        let sid = SessionId(2);
        let cheats = s.zone.cells()[s.zone.route(sid).unwrap()]
            .session(sid)
            .unwrap()
            .cheats;
        eprintln!(
            "relapse {distance} m over {link:?}: {} jumps, {} corrections received, {cheats} counted, farthest {farthest} m",
            b.jumps, b.corrections
        );
        assert_eq!(
            s.bots[honest].bot.stats.corrections, 0,
            "the honest neighbour is untouched"
        );
        if distance > 5.0 {
            // Every jump is corrected (the last may still be in flight) and
            // the avatar never leaves its spawn point on the server.
            assert!(b.jumps >= 10, "the adversary kept relapsing");
            assert!(b.corrections + 1 >= b.jumps, "every jump corrected");
            assert!(u64::from(cheats) + 1 >= b.jumps, "every jump counted");
            assert!(farthest < 0.01, "no ground gained: {farthest} m");
        }
    }
}
