//! The native client against the toy server, end to end.
//!
//! 1. **Budget row** (plan 17, Predictive correction magnitude): the whole toy server
//!    in-process with honest bots, and the real native client (input routing, the
//!    simulation thread's predictor, the native protocol session) over the seeded
//!    simulated network at 100 ms RTT with 2% loss. Deterministic: the same seed replays
//!    the same numbers. The client's correction p99 must stay under 10 cm.
//! 2. **Loopback QUIC**: the same client over real sockets logs in and plays.

use std::sync::Arc;
use std::time::Duration;

use mantis_client::input::device::{ButtonSource, KeyCode, RawInput};
use mantis_client::net::SessionState;
use mantis_client::threads::render_thread::PlatformEvent;
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_core::time::Tick;
use mantis_server::bots::Profile;
use mantis_server::simnet::LinkConfig;
use toy_client::{HeadlessSink, ToyClient};
use toy_server::sim::{Side, Sim};
use toy_server::tunables::Tunables;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn key<T: mantis_adapter_contract::Transport>(
    c: &ToyClient<T, HeadlessSink>,
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

#[test]
fn predictive_correction_p99_over_100ms_rtt_and_2_percent_loss() -> TestResult {
    let tunables = Tunables::defaults()?;
    let rate = tunables.tick_rate;
    let mut sim = Sim::new(tunables, 17, |_| None).map_err(|e| format!("{e:?}"))?;
    for _ in 0..8 {
        let _ = sim.add_bot(Side::Native, Profile::Honest, LinkConfig::RTT100_LOSS2)?;
    }
    let clock = Arc::new(ManualClock::new());
    let transport = sim.native_net.connect(LinkConfig::RTT100_LOSS2);
    let mut client = ToyClient::new(transport, Arc::clone(&clock) as Arc<dyn HostClock>, HeadlessSink)?;
    client.start(b"e2e");
    let ticks = 1800u64;
    let mut moving = false;
    let mut most_remotes = 0usize;
    for k in 1..=ticks {
        let _ = sim.step_server().map_err(|e| format!("{e:?}"))?;
        clock.set(HostInstant::from_nanos(u64::try_from(tick_start_nanos(
            rate,
            Tick(k),
        ))?));
        let welcomed = matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. });
        // Play once in the world: run forward the whole time, strafing in alternating
        // directions and jumping now and then, so prediction is exercised at speed,
        // through direction changes, and in the air.
        if welcomed && !moving {
            key(&client, KeyCode::W, true)?;
            moving = true;
        }
        if moving {
            match k % 90 {
                0 => key(&client, KeyCode::A, true)?,
                30 => key(&client, KeyCode::A, false)?,
                45 => key(&client, KeyCode::D, true)?,
                75 => key(&client, KeyCode::D, false)?,
                _ => {}
            }
            if k % 120 == 60 {
                key(&client, KeyCode::Space, true)?;
            }
            if k % 120 == 62 {
                key(&client, KeyCode::Space, false)?;
            }
        }
        client.step(2);
        most_remotes = most_remotes.max(client.sim.handler().remote_count());
        sim.step_bots();
    }
    let net = client.net.stats();
    let sim_side = client.sim.handler();
    let corrections = sim_side.corrections();
    let p99 = corrections.quantile(0.99).ok_or("no reconciliations recorded")?;
    let samples = corrections.total();
    let predictor = sim_side.predictor().stats();
    println!(
        "budget: Predictive correction p99 (native client) = {p99} m over {samples} snapshots (target < 0.10)"
    );
    println!(
        "MANTIS-METRIC client_correction_p99_m value={p99} samples={samples} rtt_ms=100 loss_permille=20 target=0.10"
    );
    println!(
        "net: {} snapshots, {} stale, {} undecodable, {} moves sent; predictor matched {} corrected {}",
        net.snapshots, net.stale, net.undecodable, net.moves_sent, predictor.matched, predictor.corrected
    );
    assert!(samples > 1000, "{samples} reconciliations");
    assert!(net.snapshots > 1000, "{net:?}");
    assert_eq!(net.undecodable, 0, "every delta had its baseline: {net:?}");
    // The client runs out of interest range over the run; the bots were in view early on.
    assert!(most_remotes >= 8, "{most_remotes} remotes at most");
    assert!(
        p99 < 0.10,
        "Predictive correction p99 {p99} m (plan 17: under 10 cm)"
    );
    Ok(())
}

#[test]
fn the_native_client_plays_over_loopback_quic() -> TestResult {
    use mantis_net::NetRuntime;
    use mantis_net::quic::{DevCertificate, QuicServer};
    use mantis_net::tcp::TcpServer;
    use toy_server::world;

    let t = Tunables::defaults()?;
    let runtime = NetRuntime::new(2)?;
    let cert = DevCertificate::localhost()?;
    let quic = QuicServer::bind(&runtime, "127.0.0.1:0".parse()?, &cert)?;
    let tcp = TcpServer::bind(&runtime, "127.0.0.1:0".parse()?)?;
    let addr = quic.local_addr();
    let mut host = world::host(&t, Box::new(quic), Box::new(tcp));
    let mut zone = world::zone(&t, 1, |_| None).map_err(|e| format!("{e:?}"))?;
    let clock: Arc<dyn HostClock> = Arc::new(mantis_client::platform::MonotonicClock::new());
    let mut client = ToyClient::connect_quic(&runtime, addr, &cert.cert_der, clock, HeadlessSink)?;
    client.start(b"loopback");
    let mut pressed = false;
    for _ in 0..3000 {
        host.poll(&mut zone);
        zone.step(&mut host, None).map_err(|e| format!("{e:?}"))?;
        client.step(1);
        if !pressed && matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. }) {
            key(&client, KeyCode::W, true)?;
            pressed = true;
        }
        if client.net.stats().snapshots >= 90 && client.sim.handler().stats().moves_sent > 30 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let net = client.net.stats();
    assert!(
        matches!(client.net.state(), SessionState::Welcomed { avatar: Some(_), .. }),
        "{:?}",
        client.net.state()
    );
    assert!(net.snapshots >= 90, "{net:?}");
    assert!(client.sim.handler().stats().moves_sent > 30, "the client played");
    assert_eq!(net.undecodable, 0, "{net:?}");
    Ok(())
}
