//! Per-session rate limits (plan 12) on the toy host, under the simulated
//! network: honest clients at their full input rate are never limited; a
//! flood is cut off at the host within the tick it arrives, never reaching
//! the cell, and counts on the session's cheat counter; past the package's
//! threshold the session is ended.

use mantis_core::log::SessionId;
use mantis_server::bots::Profile;
use mantis_server::modules::ExtensionKind;
use mantis_server::session::Sessions;
use mantis_server::simnet::LinkConfig;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;

/// Messages one flooding client sends in one tick.
const FLOOD: u32 = 300;

fn cheats(sim: &Sim, session: u64) -> Option<u32> {
    sim.zone.cells().iter().find_map(|c| {
        c.world()
            .resource::<Sessions>()
            .and_then(|s| s.map.get(&SessionId(session)).map(|x| x.cheats))
    })
}

#[test]
fn honest_clients_at_their_full_input_rate_are_never_limited() {
    let t = Tunables::defaults().unwrap();
    let mut sim = Sim::new(t, 5, |_| None).unwrap();
    for k in 0..16 {
        let side = if k % 2 == 0 { Side::Native } else { Side::Legacy };
        sim.add_bot(side, Profile::Honest, LinkConfig::RTT100_LOSS2)
            .unwrap();
    }
    for _ in 0..600 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.stats.joined, 16);
    assert_eq!(sim.host.stats.rate_limited, 0, "{:?}", sim.host.stats);
    for s in 1..=16 {
        assert_eq!(sim.host.violations(SessionId(s)), Some(0));
    }
    println!(
        "budget: honest clients rate-limited = {} over 600 ticks at 100 ms RTT, 2% loss (target 0)",
        sim.host.stats.rate_limited
    );
}

#[test]
fn a_flood_is_cut_at_the_host_within_the_tick_and_counted_against_the_session() {
    let t = Tunables::defaults().unwrap();
    let burst = t.limits.extensions.burst;
    let mut sim = Sim::new(t, 6, |_| None).unwrap();
    sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)
        .unwrap();
    for _ in 0..20 {
        sim.step().unwrap();
    }
    assert_eq!(cheats(&sim, 1), Some(0));
    // Session 1 sends 300 messages of one extension kind in one tick.
    for _ in 0..FLOOD {
        sim.bots[0].bot.feature(ExtensionKind(0x7FFF), &[0]);
    }
    let before = sim.zone.cells().iter().map(|c| c.tick_now().0).max().unwrap();
    let reports = sim.step().unwrap();
    // The host refused everything past the burst in the tick it arrived.
    assert_eq!(sim.host.stats.rate_limited, u64::from(FLOOD - burst));
    assert_eq!(sim.host.violations(SessionId(1)), Some(FLOOD - burst));
    // The cell ticked as usual, and learns the count as a logged intent on
    // its next tick.
    assert!(reports.iter().all(|r| r.tick.0 == before + 1));
    sim.step().unwrap();
    assert_eq!(cheats(&sim, 1), Some(FLOOD - burst));
    assert_eq!(cheats(&sim, 2), Some(0), "the honest neighbour is untouched");
    // The bucket refills: a second later the client may speak again.
    for _ in 0..30 {
        sim.step().unwrap();
    }
    let limited = sim.host.stats.rate_limited;
    sim.bots[0].bot.feature(ExtensionKind(0x7FFF), &[0]);
    sim.step().unwrap();
    assert_eq!(sim.host.stats.rate_limited, limited);
}

#[test]
fn a_session_past_the_threshold_is_ended() {
    let t = Tunables::defaults().unwrap();
    let kick_after = t.limits.kick_after;
    let mut sim = Sim::new(t, 7, |_| None).unwrap();
    sim.add_bot(Side::Native, Profile::Idle, LinkConfig::PERFECT)
        .unwrap();
    for _ in 0..20 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.sessions_in_world(), 1);
    for _ in 0..(kick_after + 100) {
        sim.bots[0].bot.feature(ExtensionKind(0x7FFF), &[0]);
    }
    for _ in 0..3 {
        sim.step().unwrap();
    }
    assert_eq!(sim.host.stats.limit_kicks, 1);
    assert_eq!(sim.host.sessions_in_world(), 0);
    assert_eq!(cheats(&sim, 1), None, "the avatar left its cell");
}
