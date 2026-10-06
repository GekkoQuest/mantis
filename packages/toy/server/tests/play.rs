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

#![expect(
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

const LOGGED_BUILD: BuildId = BuildId([5; 32]);

/// A sim whose cells log, with their logs.
fn logged_sim(seed: u64) -> (Sim, Vec<MemoryLog>) {
    let t = Tunables::defaults().unwrap();
    let logs: Vec<MemoryLog> = (0..2).map(|_| MemoryLog::default()).collect();
    let s = Sim::new(t, seed, |i| {
        let cfg = world::cell_config(&t, i, seed);
        let header = LogHeader {
            build: LOGGED_BUILD,
            content: t.content,
            cell: cfg.id,
            seed: cfg.seed,
            start_tick: Tick(1),
        };
        let sink: BoxedSink = Box::new(logs[i].clone());
        Some(LogWriter::create(sink, &header, 1 << 22).unwrap())
    })
    .unwrap();
    (s, logs)
}

/// Every cell of `s` replays from its log to the same state.
fn assert_replays(s: Sim, logs: &[MemoryLog], seed: u64) {
    let t = Tunables::defaults().unwrap();
    let finals: Vec<u64> = s.zone.cells().iter().map(|c| c.world().state_hash()).collect();
    drop(s);
    for (i, log) in logs.iter().enumerate() {
        let bytes = log.bytes();
        let mut reader = LogReader::<CellLogSchema>::open(&bytes, LOGGED_BUILD, t.content).unwrap();
        let mut fresh = world::cell(&t, i, seed, world::adapters(t.content), None).unwrap();
        replay(&mut fresh, &mut reader).unwrap();
        assert_eq!(fresh.world().state_hash(), finals[i], "cell {i} replays exactly");
    }
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

/// A sustained latency step: 200 ms RTT, then 300 ms from tick 900. The
/// server realigns its input consumption, so Predictive corrections stay
/// within the row after the step.
#[test]
fn predictive_corrections_stay_bounded_across_a_latency_step() {
    let (mut s, logs) = logged_sim(9);
    let rtt200 = LinkConfig {
        one_way_ms: 100,
        jitter_ms: 0,
        loss_permille: 0,
        rto_ms: 200,
    };
    for _ in 0..8 {
        s.add_bot(Side::Native, Profile::Honest, rtt200).unwrap();
    }
    for _ in 0..900 {
        s.step().unwrap();
    }
    let before: Vec<usize> = s.bots.iter().map(|b| b.bot.stats.reconcile.len()).collect();
    for i in 0..s.bots.len() {
        assert!(s.set_link(
            i,
            LinkConfig {
                one_way_ms: 150,
                ..rtt200
            }
        ));
    }
    for _ in 0..900 {
        s.step().unwrap();
    }
    let after: Vec<f32> = s
        .bots
        .iter()
        .zip(&before)
        .flat_map(|(b, n)| b.bot.stats.reconcile.iter().skip(*n).copied())
        .collect();
    let p = p99(after);
    let (pauses, late): (u64, u64) = (1..=8u64)
        .filter_map(|id| {
            let sid = SessionId(id);
            let cell = s.zone.route(sid)?;
            s.zone.cells()[cell]
                .session(sid)
                .map(|x| (u64::from(x.pauses), u64::from(x.late)))
        })
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    eprintln!(
        "budget: Predictive correction p99 after a 200 to 300 ms RTT step = {p} m (target < 0.10); {pauses} realigning pauses, {late} late inputs"
    );
    assert!(p < 0.10, "{p}");
    assert!(pauses > 0, "it realigned");
    // Pauses derive from logged intents: the logs replay exactly.
    assert_replays(s, &logs, 9);
}

/// A client that holds its inputs back and then sends them all gains no
/// distance from realignment, is corrected, and cannot slow the cell: its
/// pauses are bounded, one per `pause_every` ticks at most.
#[test]
fn withholding_inputs_gains_nothing_and_is_corrected() {
    let (mut s, logs) = logged_sim(10);
    let link = LinkConfig {
        one_way_ms: 50,
        jitter_ms: 0,
        loss_permille: 0,
        rto_ms: 200,
    };
    let east = Angle16(16_384);
    let honest = s.add_bot(Side::Native, Profile::Heading(east), link).unwrap();
    let cheat = s
        .add_bot(
            Side::Native,
            Profile::Withholder {
                yaw: east,
                hold: 10,
                every: 20,
            },
            link,
        )
        .unwrap();
    let ticks_before = s.ticks();
    let position = |s: &Sim, id: u64| {
        let sid = SessionId(id);
        let cell = s.zone.route(sid).unwrap();
        let c = &s.zone.cells()[cell];
        let avatar = c.session(sid).unwrap().avatar.unwrap();
        c.world().components.get::<Body>(avatar).unwrap().0.position
    };
    while !(s.bots[honest].bot.synced() && s.bots[cheat].bot.synced()) {
        s.step().unwrap();
    }
    let start = (position(&s, 1), position(&s, 2));
    let reconcile_start = s.bots[cheat].bot.stats.reconcile.len();
    for _ in 0..600 {
        s.step().unwrap();
    }
    let honest_went = (position(&s, 1) - start.0).horizontal().length();
    let cheat_went = (position(&s, 2) - start.1).horizontal().length();
    let (pauses, late, synthesized) = {
        let sid = SessionId(2);
        let cell = s.zone.route(sid).unwrap();
        let x = s.zone.cells()[cell].session(sid).unwrap();
        (x.pauses, x.late, x.synthesized)
    };
    let pause_every = mantis_server::movement::InputConfig::DEFAULT.pause_every;
    eprintln!(
        "withholder: {cheat_went} m vs honest {honest_went} m; {pauses} pauses, {late} late, {synthesized} synthesized"
    );
    assert!(late > 0 && synthesized > 0, "it did arrive late");
    assert!(cheat_went <= honest_went + 0.01, "no distance gained");
    assert!(
        u64::from(pauses) <= 600 / u64::from(pause_every) + 1,
        "bounded pauses"
    );
    let corrections = s.bots[cheat].bot.stats.reconcile[reconcile_start..]
        .iter()
        .filter(|e| **e > 0.01)
        .count();
    assert!(corrections > 0, "its predictions are corrected");
    assert!(s.ticks() - ticks_before >= 600, "the cell ticked on");
    assert_replays(s, &logs, 10);
}

/// The input window covers the worst supported round trip at 60 Hz:
/// Predictive clients at the package's `max_rtt` keep every input (none
/// dropped as too far ahead, so none synthesized once in step; idle ones,
/// which never cross into the other cell, show it), and honest walkers'
/// corrections stay within the row.
#[test]
fn the_input_window_covers_the_worst_supported_round_trip_at_60_hz() {
    let mut t = Tunables::defaults().unwrap();
    t.tick_rate = mantis_core::time::TickRate::new(60).unwrap();
    // A client at 60 Hz sends twice the inputs and acknowledgements.
    for b in [&mut t.limits.inputs, &mut t.limits.acks] {
        b.per_second *= 2;
        b.burst *= 2;
    }
    t.limits.bytes_per_second *= 2;
    let inputs = t.inputs();
    assert_eq!(
        (inputs.window, inputs.max_lead),
        (24 + 12 + 2, 12),
        "400 ms is 24 ticks, 200 ms of lead is 12"
    );
    let mut s = Sim::new(t, 11, |_| None).unwrap();
    let worst = LinkConfig {
        one_way_ms: t.max_rtt_ms / 2,
        jitter_ms: 20,
        loss_permille: 0,
        rto_ms: 400,
    };
    for k in 0..6 {
        let profile = if k % 2 == 0 {
            Profile::Honest
        } else {
            Profile::Idle
        };
        s.add_bot(Side::Native, profile, worst).unwrap();
    }
    for _ in 0..600 {
        s.step().unwrap();
    }
    let idle = [2u64, 4, 6];
    let synthesized_then: Vec<u32> = idle.iter().map(|id| synthesized(&s, *id)).collect();
    let marks: Vec<usize> = s.bots.iter().map(|b| b.bot.stats.reconcile.len()).collect();
    for _ in 0..1200 {
        s.step().unwrap();
    }
    for (k, id) in idle.iter().copied().enumerate() {
        assert_eq!(
            synthesized(&s, id),
            synthesized_then[k],
            "session {id} lost an input"
        );
    }
    let after: Vec<f32> = s
        .bots
        .iter()
        .zip(&marks)
        .flat_map(|(b, n)| b.bot.stats.reconcile.iter().skip(*n).copied())
        .collect();
    let p = p99(after);
    eprintln!(
        "budget: Predictive correction p99 at 60 Hz and the worst supported RTT ({} ms) = {p} m (target < 0.10)",
        t.max_rtt_ms
    );
    assert!(p < 0.10, "{p}");
}

fn synthesized(s: &Sim, id: u64) -> u32 {
    let sid = SessionId(id);
    let cell = s.zone.route(sid).unwrap();
    s.zone.cells()[cell].session(sid).unwrap().synthesized
}

/// A stalled cell host (15 ticks mid-play, bots at 100 ms RTT with jitter)
/// resumes one tick per step, never bursting: the inputs queued during the
/// stall are consumed in order, Predictive corrections after the resume stay
/// within the row, no honest Validated claim is rejected, and the logs replay.
#[test]
fn a_stalled_host_resumes_without_bursting_and_loses_nothing() {
    stalled_host(15);
}

/// A stall longer than the input window: the cell skips the seqs it can no
/// longer consume in time (unapplied, corrected once) and plays on with
/// its normal lead; Validated clients are still judged fairly.
#[test]
fn a_stall_past_the_input_window_skips_ahead_once() {
    stalled_host(40);
}

fn stalled_host(stall: u64) {
    // 100 ms RTT with jitter but no loss: an input lost on the way would be
    // synthesized whatever the stall did.
    let rtt100 = LinkConfig {
        loss_permille: 0,
        ..LinkConfig::RTT100_LOSS2
    };
    let (mut s, logs) = logged_sim(12);
    for k in 0..12 {
        let (side, profile) = match k % 4 {
            0 => (Side::Native, Profile::Honest),
            1 => (Side::Native, Profile::Idle),
            2 => (Side::Legacy, Profile::Honest),
            _ => (Side::Legacy, Profile::Idle),
        };
        s.add_bot(side, profile, rtt100).unwrap();
    }
    for _ in 0..300 {
        s.step().unwrap();
    }
    // The idle native bots never change cells: their sessions' counters
    // show every input of the stall consumed, in order, with none lost.
    let idle_native = [1usize, 5, 9];
    let state = |s: &Sim, bot: usize| {
        let avatar = s.bots[bot].bot.avatar().unwrap();
        let x = s
            .zone
            .cells()
            .iter()
            .find_map(|c| {
                c.world()
                    .resource::<mantis_server::session::Sessions>()?
                    .map
                    .values()
                    .find(|x| x.repl.0 == avatar)
                    .cloned()
            })
            .unwrap();
        (x.last_seq.unwrap().0, x.synthesized, x.pauses, x.skipped)
    };
    let before: Vec<(u32, u32, u32, u32)> = idle_native.iter().map(|id| state(&s, *id)).collect();
    let marks: Vec<usize> = s.bots.iter().map(|b| b.bot.stats.reconcile.len()).collect();
    let corrections_before: u64 = s.bots.iter().map(|b| b.bot.stats.corrections).sum();
    let cheats_of = |s: &Sim| -> u64 {
        (1..=12u64)
            .filter_map(|id| {
                let sid = SessionId(id);
                let cell = s.zone.route(sid)?;
                s.zone.cells()[cell].session(sid).map(|x| u64::from(x.cheats))
            })
            .sum()
    };
    let cheats_before = cheats_of(&s);
    let ticks_before = s.ticks();
    s.stall_server(stall);
    for _ in 0..stall {
        assert!(s.step().unwrap().is_empty(), "nothing ticks while stalled");
    }
    assert_eq!(s.ticks(), ticks_before, "the stall ticked nothing");
    for _ in 0..300 {
        let reports = s.step().unwrap();
        assert_eq!(reports.len(), 2, "one tick per step: never a burst");
    }
    assert_eq!(s.ticks(), ticks_before + 300, "re-anchored, not caught up");
    for (k, id) in idle_native.iter().enumerate() {
        let (seq, synthesized, pauses, skipped) = state(&s, *id);
        assert_eq!(synthesized, before[k].1, "bot {id} lost an input");
        assert_eq!(
            u64::from(seq.wrapping_sub(before[k].0)),
            300 - u64::from(pauses - before[k].2) + u64::from(skipped - before[k].3),
            "bot {id} consumed one input per tick, in order (skipping only past the window)"
        );
    }
    let after: Vec<f32> = s
        .bots
        .iter()
        .zip(&marks)
        .filter(|(b, _)| b.side == Side::Native)
        .flat_map(|(b, n)| b.bot.stats.reconcile.iter().skip(*n).copied())
        .collect();
    let p = p99(after);
    eprintln!("budget: Predictive correction p99 after a {stall}-tick host stall = {p} m (target < 0.10)");
    assert!(p < 0.10, "{p}");
    let corrections: u64 = s.bots.iter().map(|b| b.bot.stats.corrections).sum::<u64>() - corrections_before;
    let cheats = cheats_of(&s) - cheats_before;
    eprintln!("stall: {corrections} Validated corrections, {cheats} counted");
    assert_eq!(
        (corrections, cheats),
        (0, 0),
        "no honest claim rejected because of the stall"
    );
    assert_replays(s, &logs, 12);
}
