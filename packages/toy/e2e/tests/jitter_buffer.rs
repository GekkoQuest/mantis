//! The jitter buffer end to end: the real native client against the whole toy server
//! over the seeded simulated network, with remote characters in view.
//!
//! **RTT step** (lead, M13 (1)). The client's link steps from 200 ms to 300 ms RTT mid-run. The same run
//! is made with the old fixed presentation (a 100 ms interpolation delay and a timeline
//! that only rises slowly) and with the adaptive one (the package's `[tunables.client]`
//! floor and ceiling, and the timeline's adaptive rise). Remote characters must never be
//! held (shown frozen past their newest sample) for more than a few frames, and the local
//! avatar's correction p99 must stay under the 10 cm row.
//!
//! **Server stall** (lead, M13 (2)). The server stops ticking for 15 ticks (`Sim::stall_server`)
//! at 100 ms RTT, then resumes 15 ticks behind wall time. Remote characters must hold
//! (extrapolate briefly, then freeze) rather than snap: no remote moves further between
//! two frames than it can run, plus slack (on resume it glides on from where it was held:
//! [`mantis_client::sim`] bridges the gap with the held position). The local avatar's worst correction on resume
//! is bounded, and no frame is undecodable.
//!
//! Prints `budget:` and `MANTIS-METRIC` lines for both runs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::jitter::DelayConfig;
use mantis_client::net::SessionState;
use mantis_client::render_world::PoseSource;
use mantis_client::threads::render_thread::{FrameContext, FrameSink, PlatformEvent};
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_core::time::Tick;
use mantis_server::bots::Profile;
use mantis_server::simnet::LinkConfig;
use toy_client::{ClientOptions, ToyClient};
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Counts how remote characters are shown, frame by frame.
#[derive(Default)]
struct PoseCounter {
    /// Per remote: whether it was interpolated since it last entered view (steady state),
    /// its current run of held frames, and the last frame it was shown in.
    remotes: BTreeMap<u64, (bool, u32, u64)>,
    frames: u64,
    interpolated: u64,
    extrapolated: u64,
    held: u64,
    longest_hold: u32,
    /// Per remote: last steady position, and the frame it was shown at.
    last: BTreeMap<u64, (mantis_client::core_api::Vec3, u64)>,
    /// The largest move of a steady remote between two consecutive frames, in metres.
    largest_step: f32,
}

impl FrameSink for PoseCounter {
    fn resize(&mut self, _width: u32, _height: u32) {}

    fn submit(&mut self, frame: &FrameContext<'_>) {
        self.frames += 1;
        for pose in frame.poses.as_slice() {
            let frame_no = self.frames;
            let entry = self
                .remotes
                .entry(pose.id.to_bits())
                .or_insert((false, 0, frame_no));
            if entry.2 + 1 < frame_no {
                // It left view and came back: it enters again.
                *entry = (false, 0, frame_no);
            }
            entry.2 = frame_no;
            let steady = entry.0;
            if pose.source != PoseSource::LocalPredicted {
                let id = pose.id.to_bits();
                if steady
                    && let Some((p, f)) = self.last.get(&id)
                    && *f + 1 == frame_no
                {
                    self.largest_step = self.largest_step.max((pose.position - *p).length());
                }
                self.last.insert(id, (pose.position, frame_no));
            }
            match pose.source {
                PoseSource::RemoteInterpolated => {
                    self.interpolated += 1;
                    entry.0 = true;
                    entry.1 = 0;
                }
                PoseSource::RemoteExtrapolated => {
                    self.extrapolated += 1;
                    entry.1 = 0;
                }
                // Held before its first interpolated frame is a character entering view
                // (render time has not reached its first sample yet), not a stall.
                PoseSource::RemoteHeld if entry.0 => {
                    self.held += 1;
                    entry.1 += 1;
                    self.longest_hold = self.longest_hold.max(entry.1);
                }
                PoseSource::RemoteHeld | PoseSource::LocalPredicted => {}
            }
        }
    }
}

/// What one run measured.
#[derive(Debug)]
struct Run {
    held: u64,
    longest_hold: u32,
    extrapolated: u64,
    interpolated: u64,
    correction_p99: f32,
    max_delay_ms: u128,
    fast_rises: u64,
    undecodable: u64,
    /// Ticks after the event until the timeline estimate stayed within 5 ms of where it
    /// settled.
    absorb_ticks: u64,
    /// The largest move of a steady remote between two consecutive frames, in metres.
    largest_step: f32,
    /// The largest local correction, in metres.
    max_correction: f32,
}

/// What happens at [`EVENT_AT`].
#[derive(Clone, Copy, Debug)]
enum Event {
    /// The client's link changes.
    Link(LinkConfig),
    /// The server stops ticking for this many ticks.
    Stall(u64),
}

fn key(
    c: &ToyClient<mantis_server::simnet::SimClient, PoseCounter>,
    k: KeyCode,
    pressed: bool,
) -> TestResult {
    c.events
        .as_ref()
        .ok_or("no event channel")?
        .send(PlatformEvent::Input(RawInput::Button {
            source: ButtonSource::Key(k),
            pressed,
        }))?;
    Ok(())
}

const TICKS: u64 = 1800;
const EVENT_AT: u64 = 900;
const RTT200: LinkConfig = LinkConfig {
    one_way_ms: 100,
    jitter_ms: 10,
    loss_permille: 0,
    rto_ms: 200,
};
const RTT300: LinkConfig = LinkConfig {
    one_way_ms: 150,
    ..RTT200
};

fn run(options: ClientOptions) -> Result<Run, Box<dyn std::error::Error>> {
    run_scenario(options, RTT200, Event::Link(RTT300))
}

fn run_scenario(
    options: ClientOptions,
    link: LinkConfig,
    event: Event,
) -> Result<Run, Box<dyn std::error::Error>> {
    let tunables = Tunables::defaults()?;
    let rate = tunables.tick_rate;
    let mut sim = Sim::new(tunables, 29, |_| None).map_err(|e| format!("{e:?}"))?;
    for _ in 0..8 {
        let _ = sim.add_bot(Side::Native, Profile::Honest, LinkConfig::PERFECT)?;
    }
    let clock = Arc::new(ManualClock::new());
    let transport = sim.native_net.connect(link);
    let mut client = ToyClient::new_with_options(
        transport,
        Arc::clone(&clock) as Arc<dyn HostClock>,
        PoseCounter::default(),
        options,
    )?;
    client.start(b"jitter");
    let half_tick = Duration::from_nanos(u64::try_from(tick_start_nanos(rate, Tick(1)))? / 2);
    let mut moving = false;
    let mut offsets = Vec::with_capacity(usize::try_from(TICKS)?);
    for k in 1..=TICKS {
        if k == EVENT_AT {
            match event {
                Event::Link(after) => client.net.transport_mut().set_link(after),
                Event::Stall(ticks) => sim.stall_server(ticks),
            }
        }
        let _ = sim.step_server().map_err(|e| format!("{e:?}"))?;
        let start = HostInstant::from_nanos(u64::try_from(tick_start_nanos(rate, Tick(k)))?);
        clock.set(start);
        if !moving && matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. }) {
            moving = true;
        }
        if moving {
            // Run back and forth with sidesteps, so the avatar keeps changing direction
            // but stays near the bots: forward 40 ticks, back 40, with a sidestep each way.
            match k % 120 {
                0 => key(&client, KeyCode::W, true)?,
                10 => key(&client, KeyCode::A, true)?,
                30 => key(&client, KeyCode::A, false)?,
                40 => key(&client, KeyCode::W, false)?,
                60 => key(&client, KeyCode::S, true)?,
                70 => key(&client, KeyCode::D, true)?,
                90 => key(&client, KeyCode::D, false)?,
                100 => key(&client, KeyCode::S, false)?,
                _ => {}
            }
        }
        // Two frames per tick (60 frames per second): one at the tick, one half way.
        client.step(1);
        clock.set(start.saturating_add(half_tick));
        let _ = client.render.frame();
        sim.step_bots();
        offsets.push(client.sim.handler().timeline().offset_nanos().unwrap_or(0));
    }
    let settled = offsets.last().copied().unwrap_or(0);
    let step = usize::try_from(EVENT_AT)?;
    let last_off = offsets
        .iter()
        .rposition(|o| (o - settled).abs() > 5_000_000)
        .map_or(0, |i| i + 1);
    let absorb_ticks = u64::try_from(last_off.saturating_sub(step))?;
    let sim_side = client.sim.handler();
    let counts = client.render.sink();
    Ok(Run {
        held: counts.held,
        longest_hold: counts.longest_hold,
        extrapolated: counts.extrapolated,
        interpolated: counts.interpolated,
        correction_p99: sim_side.corrections().quantile(0.99).ok_or("no corrections")?,
        max_delay_ms: sim_side.jitter().max_delay.as_millis(),
        fast_rises: sim_side.timeline().fast_rises(),
        undecodable: client.net.stats().undecodable,
        absorb_ticks,
        largest_step: counts.largest_step,
        max_correction: sim_side.corrections().max(),
    })
}

fn report(scenario: &str, label: &str, r: &Run) {
    println!(
        "budget: {scenario} ({label}): held {} frames (longest run {}), extrapolated {}, interpolated {}; correction p99 {} m, max {} m; largest remote step {} m; max delay {} ms; fast timeline rises {}; absorbed in {} ticks",
        r.held,
        r.longest_hold,
        r.extrapolated,
        r.interpolated,
        r.correction_p99,
        r.max_correction,
        r.largest_step,
        r.max_delay_ms,
        r.fast_rises,
        r.absorb_ticks
    );
    println!(
        "MANTIS-METRIC jitter_{}_{label} held_frames={} longest_hold={} extrapolated_frames={} correction_p99_m={} max_correction_m={} largest_remote_step_m={} max_delay_ms={} absorb_ticks={}",
        scenario.split_whitespace().next().unwrap_or("run").to_lowercase(),
        r.held,
        r.longest_hold,
        r.extrapolated,
        r.correction_p99,
        r.max_correction,
        r.largest_step,
        r.max_delay_ms,
        r.absorb_ticks
    );
}

/// Longest run of held frames a remote may show after the RTT step (two frames: one
/// tick at 60 frames per second).
const MAX_HOLD_FRAMES: u32 = 2;

#[test]
fn an_rtt_step_never_holds_remotes_and_the_timeline_absorbs_it_sooner() -> TestResult {
    let fixed = run(ClientOptions {
        delay: Some(DelayConfig::fixed(Duration::from_millis(100))),
        timeline_adaptive: Some(false),
    })?;
    report("RTT 200 -> 300 ms", "fixed", &fixed);
    let adaptive = run(ClientOptions::default())?;
    report("RTT 200 -> 300 ms", "adaptive", &adaptive);
    assert!(adaptive.interpolated > 5_000, "{adaptive:?}");
    assert_eq!(adaptive.undecodable, 0, "{adaptive:?}");
    assert!(
        adaptive.longest_hold <= MAX_HOLD_FRAMES,
        "remotes held {} frames in a row: {adaptive:?}",
        adaptive.longest_hold
    );
    assert!(
        adaptive.extrapolated <= fixed.extrapolated,
        "{adaptive:?} vs {fixed:?}"
    );
    // One sustained shift, detected once, absorbed well before the slow rise gets there.
    assert_eq!(adaptive.fast_rises, 1, "{adaptive:?}");
    assert!(
        adaptive.absorb_ticks * 2 < fixed.absorb_ticks,
        "absorbed in {} ticks, fixed in {}",
        adaptive.absorb_ticks,
        fixed.absorb_ticks
    );
    Ok(())
}

/// The local half of the row: the avatar's correction p99 under 10 cm across the step.
/// Before the server re-aligned its input stream after a latency increase
/// (`mantis_server::movement`, M13) this measured 0.27 m: every input that reached the
/// server after its seq was consumed was replaced by a repeat.
#[test]
fn the_local_correction_p99_holds_across_an_rtt_step() -> TestResult {
    let adaptive = run(ClientOptions::default())?;
    report("RTT 200 -> 300 ms", "adaptive", &adaptive);
    assert!(
        adaptive.correction_p99 < 0.10,
        "correction p99 {} m (row: under 10 cm)",
        adaptive.correction_p99
    );
    Ok(())
}

/// Slack over a remote's own running speed for one frame's move (half a tick at 30 Hz):
/// interpolation and the delay's slew may add a little, a snap adds metres.
const SNAP_SLACK_M: f32 = 0.25;
/// The largest local correction allowed on resuming from the stall.
const RESUME_CORRECTION_M: f32 = 0.5;

#[test]
fn a_server_stall_holds_remotes_instead_of_snapping() -> TestResult {
    let rtt100 = LinkConfig {
        one_way_ms: 50,
        ..RTT200
    };
    let fixed = run_scenario(
        ClientOptions {
            delay: Some(DelayConfig::fixed(Duration::from_millis(100))),
            timeline_adaptive: Some(false),
        },
        rtt100,
        Event::Stall(15),
    )?;
    report("stall 15 ticks", "fixed", &fixed);
    let adaptive = run_scenario(ClientOptions::default(), rtt100, Event::Stall(15))?;
    report("stall 15 ticks", "adaptive", &adaptive);
    let run_speed = Tunables::defaults()?.motion.run_speed;
    let frame_step = run_speed / 60.0 + SNAP_SLACK_M;
    assert_eq!(adaptive.undecodable, 0, "{adaptive:?}");
    assert!(adaptive.held > 0, "remotes held through the stall: {adaptive:?}");
    // They hold only while the stall lasts (15 ticks, 30 frames), not until a slow
    // timeline catches up.
    assert!(adaptive.longest_hold <= 30, "{adaptive:?}");
    assert!(
        adaptive.longest_hold < fixed.longest_hold,
        "{adaptive:?} vs {fixed:?}"
    );
    assert_eq!(
        adaptive.absorb_ticks.min(30),
        adaptive.absorb_ticks,
        "re-anchored on resume: {adaptive:?}"
    );
    assert!(
        adaptive.largest_step <= frame_step,
        "a remote moved {} m in one frame (limit {frame_step} m): {adaptive:?}",
        adaptive.largest_step
    );
    assert!(
        adaptive.max_correction <= RESUME_CORRECTION_M,
        "resume correction {} m: {adaptive:?}",
        adaptive.max_correction
    );
    assert!(adaptive.correction_p99 < 0.10, "{adaptive:?}");
    Ok(())
}
