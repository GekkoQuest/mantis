//! Per-remote snapshot baselines (`[tunables.interest] snapshot_own_bases`) with the
//! engine's default (off) and enabled:
//!
//! - the cell-500-100 snapshot bytes per client per second (the bandwidth row);
//! - the real native client against the toy server at 100 ms RTT and 2 % loss: the
//!   Predictive correction p99 row, and no undecodable frame in either configuration
//!   (frames then mix frame-level and own-base deltas; the client's 64-frame ring covers
//!   every base the server may name).
//!
//! Prints `budget:` and `MANTIS-METRIC` lines for both configurations.

use std::sync::Arc;

use mantis_adapter_contract::{Channel, ConnectionId};
use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::net::SessionState;
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_core::time::Tick;
use mantis_server::bots::Profile;
use mantis_server::cell::OutboundSink;
use mantis_server::simnet::LinkConfig;
use toy_client::{HeadlessSink, ToyClient};
use toy_server::scenario::Crowd;
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Bytes {
    total: u64,
    frames: u64,
}

impl OutboundSink for Bytes {
    fn send(&mut self, _adapter: usize, _conn: ConnectionId, _channel: Channel, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        self.frames += 1;
    }
}

fn tunables(own_bases: bool) -> Result<Tunables, Box<dyn std::error::Error>> {
    let mut t = Tunables::defaults()?;
    t.interest.snapshot_own_bases = own_bases;
    Ok(t)
}

/// cell-500-100 snapshot bytes per client per second, after 60 warm-up ticks.
#[expect(clippy::cast_precision_loss)] // Byte counts and seconds are far below 2^52.
fn bandwidth(own_bases: bool) -> Result<f64, Box<dyn std::error::Error>> {
    let t = tunables(own_bases)?;
    let mut crowd = Crowd::cell_500_100(&t, 2).map_err(|e| format!("{e:?}"))?;
    let mut sink = Bytes::default();
    for _ in 0..60 {
        crowd.tick(&mut sink, None).map_err(|e| format!("{e:?}"))?;
    }
    let mut sink = Bytes::default();
    let ticks = 300u64;
    for _ in 0..ticks {
        crowd.tick(&mut sink, None).map_err(|e| format!("{e:?}"))?;
    }
    let seconds = ticks as f64 / f64::from(t.tick_rate.hz());
    Ok(sink.total as f64 / crowd.clients() as f64 / seconds)
}

/// The correction row's run (as `native_client`, with `bots` bots), with own bases set:
/// (p99 in metres, snapshots applied, undecodable frames). With more bots in view than
/// the interest budget (32), remotes rotate in and out of snapshots, so frames mix
/// frame-level and own-base deltas.
fn correction(own_bases: bool, bots: usize) -> Result<(f32, u64, u64), Box<dyn std::error::Error>> {
    let t = tunables(own_bases)?;
    let rate = t.tick_rate;
    let mut sim = Sim::new(t, 17, |_| None).map_err(|e| format!("{e:?}"))?;
    for _ in 0..bots {
        let _ = sim.add_bot(Side::Native, Profile::Honest, LinkConfig::RTT100_LOSS2)?;
    }
    let clock = Arc::new(ManualClock::new());
    let transport = sim.native_net.connect(LinkConfig::RTT100_LOSS2);
    let mut client = ToyClient::new(transport, Arc::clone(&clock) as Arc<dyn HostClock>, HeadlessSink)?;
    client.start(b"bases");
    let key = |c: &ToyClient<_, HeadlessSink>, k: KeyCode, pressed: bool| -> TestResult {
        c.events
            .as_ref()
            .ok_or("no event channel")?
            .send(PlatformEvent::Input(RawInput::Button {
                source: ButtonSource::Key(k),
                pressed,
            }))?;
        Ok(())
    };
    let mut moving = false;
    for k in 1..=1800u64 {
        let _ = sim.step_server().map_err(|e| format!("{e:?}"))?;
        clock.set(HostInstant::from_nanos(u64::try_from(tick_start_nanos(
            rate,
            Tick(k),
        ))?));
        if !moving && matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. }) {
            moving = true;
        }
        if moving {
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
        client.step(2);
        sim.step_bots();
    }
    let p99 = client
        .sim
        .handler()
        .corrections()
        .quantile(0.99)
        .ok_or("no reconciliations")?;
    let net = client.net.stats();
    Ok((p99, net.snapshots, net.undecodable))
}

#[test]
fn own_bases_off_and_on_bandwidth_and_correction_rows() -> TestResult {
    for (label, on) in [("off", false), ("on", true)] {
        let bytes = bandwidth(on)?;
        let (p99, snapshots, undecodable) = correction(on, 8)?;
        let (crowd_p99, crowd_snapshots, crowd_undecodable) = correction(on, 40)?;
        println!(
            "budget: own bases {label}: cell-500-100 snapshot bytes = {bytes:.0} B per client per second (target < 20480); correction p99 = {p99} m at 100 ms RTT, 2% loss (target < 0.10); {snapshots} snapshots, {undecodable} undecodable"
        );
        println!(
            "MANTIS-METRIC snapshot_own_bases_{label} bytes_per_client_s={bytes:.0} correction_p99_m={p99} snapshots={snapshots} undecodable={undecodable}"
        );
        assert!(bytes < 20_480.0, "{label}: {bytes} B/s");
        assert!(p99 < 0.10, "{label}: correction p99 {p99} m");
        assert!(snapshots > 1000, "{label}: {snapshots} snapshots");
        assert_eq!(undecodable, 0, "{label}: every frame decoded");
        println!(
            "budget: own bases {label}, 40 bots (the budget rotates remotes): correction p99 = {crowd_p99} m; {crowd_snapshots} snapshots, {crowd_undecodable} undecodable"
        );
        println!(
            "MANTIS-METRIC snapshot_own_bases_{label}_40_bots correction_p99_m={crowd_p99} snapshots={crowd_snapshots} undecodable={crowd_undecodable}"
        );
        assert!(crowd_p99 < 0.10, "{label}, 40 bots: correction p99 {crowd_p99} m");
        assert!(crowd_snapshots > 1000, "{label}, 40 bots");
        assert_eq!(crowd_undecodable, 0, "{label}, 40 bots: every frame decoded");
    }
    Ok(())
}
