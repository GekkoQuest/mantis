//! Headless end-to-end tests of a world session: input on the render thread becomes move
//! intents on the sim thread, the predictor runs ahead of a lagging stand-in server, the
//! render world shows the local avatar extrapolated and the remote entity interpolated,
//! and the `World` scope owns the threads.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mantis_client::core_api::MotionModifiers;
use mantis_client::input::device::KeyCode;
use mantis_client::predict::metrics::BUCKET_WIDTH;
use mantis_client::render_world::PoseSource;
use mantis_client::scope::{ScopeKind, ScopeStack};
use support::{NPC_VELOCITY, Rig, TestResult, ground, tick_duration};

#[test]
fn agreeing_server_never_corrects_and_motion_is_continuous() -> TestResult {
    let mut rig = Rig::new(3, 3, ground())?;
    for k in 0..300u64 {
        if k == 10 {
            rig.key(KeyCode::W, true)?;
        }
        if k == 200 {
            rig.key(KeyCode::W, false)?;
        }
        rig.run_tick(4);
    }
    let sim = rig.session.sim.handler();
    let stats = sim.predictor().stats();
    assert!(sim.stats().moves_sent > 250, "{:?}", sim.stats());
    assert_eq!(stats.corrected, 0, "{stats:?}");
    assert!(stats.matched > 250, "{stats:?}");
    assert_eq!(
        sim.corrections().quantile(0.99),
        Some(BUCKET_WIDTH),
        "p99 correction is in the zero bucket"
    );
    assert_eq!(sim.stats().resets, 1, "one spawn");

    let frames = rig.frames();
    assert_eq!(frames.len(), 1200);
    // One render world per tick, four frames each: the world tick advances exactly once
    // every four frames, never backward.
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.world_tick.0, u64::try_from(i / 4)?, "frame {i}");
    }
    let locals: Vec<_> = frames.iter().filter_map(|f| f.local).collect();
    let last = locals.last().ok_or("no local poses")?;
    assert!(last.position.z > 10.0, "moved forward: {:?}", last.position);
    // At top speed, extrapolation to render time advances the drawn avatar by exactly
    // speed times the frame interval on every frame, across tick boundaries too: the
    // latest prediction is shown immediately, never a tick late.
    let frame_dt = tick_duration().as_secs_f32() / 4.0;
    let run_speed = support::core_motion()?.max_horizontal_speed(&MotionModifiers::default());
    let cruise: Vec<_> = frames
        .iter()
        .skip(4 * 60)
        .take(4 * 130)
        .filter_map(|f| f.local)
        .collect();
    assert_eq!(cruise.len(), 4 * 130);
    for w in cruise.windows(2) {
        if let [a, b] = w {
            let step = (b.position - a.position).length();
            assert!(
                (step - run_speed * frame_dt).abs() < 1e-4,
                "cruise frame step {step}"
            );
        }
    }

    // The remote entity is interpolated, and lands exactly on the server's trajectory
    // shifted by the timeline offset (two ticks) plus the interpolation delay (three).
    let late = frames
        .iter()
        .skip(800)
        .filter_map(|f| f.npc.map(|p| (f.now_nanos, p)));
    let mut checked = 0;
    for (now, pose) in late {
        assert_eq!(pose.source, PoseSource::RemoteInterpolated);
        #[allow(clippy::cast_precision_loss)]
        let server_ticks = now as f64 / 1e9 * 30.0 - 5.0;
        #[allow(clippy::cast_possible_truncation)]
        let expect_x = (-10.0 + f64::from(NPC_VELOCITY.x) * server_ticks / 30.0) as f32;
        assert!(
            (pose.position.x - expect_x).abs() < 1e-3,
            "npc x {} expected {expect_x}",
            pose.position.x
        );
        checked += 1;
    }
    assert!(checked > 300);
    Ok(())
}

/// Steady-state motion of the drawn local avatar along +Z after the server slows it.
struct Drawn {
    /// Frames where the drawn avatar moved backward.
    backward: usize,
    /// Largest change of per-frame step between consecutive frames (jitter).
    jitter: f32,
}

/// Runs the divergent-server scenario and measures the drawn avatar from tick 100 on.
fn divergent_run(smoothing: Duration) -> Result<(Rig, Drawn), Box<dyn std::error::Error>> {
    let mut rig = Rig::new_with(3, 3, ground(), |c| c.sim.correction_window = smoothing)?;
    for k in 0..240u64 {
        if k == 10 {
            rig.key(KeyCode::W, true)?;
        }
        if k == 60 {
            // The server applies a slow the client did not predict.
            rig.server.mods = MotionModifiers {
                speed_scale: 0.5,
                ..MotionModifiers::default()
            };
        }
        rig.run_tick(4);
    }
    let z: Vec<f32> = rig
        .frames()
        .iter()
        .skip(4 * 100)
        .filter_map(|f| f.local.map(|p| p.position.z))
        .collect();
    let steps: Vec<f32> = z
        .windows(2)
        .filter_map(|w| if let [a, b] = w { Some(b - a) } else { None })
        .collect();
    let backward = steps.iter().filter(|s| **s < -1e-6).count();
    let jitter = steps
        .windows(2)
        .filter_map(|w| if let [a, b] = w { Some((b - a).abs()) } else { None })
        .fold(0.0, f32::max);
    Ok((rig, Drawn { backward, jitter }))
}

#[test]
fn divergent_server_corrects_and_render_world_smooths() -> TestResult {
    let (rig, smoothed) = divergent_run(Duration::from_millis(150))?;
    let sim = rig.session.sim.handler();
    let stats = sim.predictor().stats();
    assert!(stats.corrected > 100, "{stats:?}");
    assert!(
        stats.replayed >= stats.corrected * 5,
        "each correction replays the inputs in flight: {stats:?}"
    );
    let p99 = sim.corrections().quantile(0.99).ok_or("no samples")?;
    assert!(p99 > BUCKET_WIDTH && p99 < 1.0, "p99 {p99}");

    // The simulation snaps to the corrected state every tick. Without smoothing, the drawn
    // avatar visibly steps backward at each correction; with it, it never moves backward
    // and its frame-to-frame jitter is much lower.
    let (_, raw) = divergent_run(Duration::ZERO)?;
    assert!(
        raw.backward > 50,
        "unsmoothed run steps backward at corrections: {}",
        raw.backward
    );
    assert_eq!(smoothed.backward, 0, "smoothed run never steps backward");
    assert!(
        smoothed.jitter < raw.jitter * 0.5,
        "jitter smoothed {} raw {}",
        smoothed.jitter,
        raw.jitter
    );
    Ok(())
}

#[test]
fn authoritative_modifiers_end_corrections_once_inputs_in_flight_drain() -> TestResult {
    let mut rig = Rig::new(3, 3, ground())?;
    rig.server.send_mods = true;
    let mut corrected_at_150 = 0;
    for k in 0..240u64 {
        if k == 10 {
            rig.key(KeyCode::W, true)?;
        }
        if k == 60 {
            rig.server.mods = MotionModifiers {
                speed_scale: 0.5,
                ..MotionModifiers::default()
            };
        }
        if k == 150 {
            corrected_at_150 = rig.session.sim.handler().predictor().stats().corrected;
        }
        rig.run_tick(2);
    }
    let stats = rig.session.sim.handler().predictor().stats();
    // Only the inputs predicted before the modifier change reached the client mispredict;
    // after that the client integrates with the server's modifiers and matches bit for bit.
    assert!(stats.corrected > 0 && stats.corrected <= 8, "{stats:?}");
    assert_eq!(
        stats.corrected, corrected_at_150,
        "no corrections in steady state"
    );
    Ok(())
}

#[test]
fn world_scope_owns_session_threads_and_leaks_nothing() -> TestResult {
    let ground_model = ground();
    let rig = Rig::new(1, 1, Arc::clone(&ground_model))?;
    assert_eq!(Arc::strong_count(&ground_model), 2);

    let mut scopes = ScopeStack::new();
    scopes.enter(ScopeKind::App)?;
    scopes.enter(ScopeKind::Login)?;
    scopes.enter(ScopeKind::Character)?;
    scopes.enter(ScopeKind::World)?;
    let Rig {
        config,
        clock,
        session,
        frames,
        outbox,
        events,
        server,
        ..
    } = rig;
    drop(server);
    let running = session.spawn(&config, Some(Duration::from_millis(1)))?;
    scopes.insert(running)?;

    // Let both threads run against the manual clock.
    for _ in 0..20 {
        clock.advance(tick_duration());
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..10_000 {
        if frames.lock().map_or(0, |f| f.len()) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        frames.lock().is_ok_and(|f| !f.is_empty()),
        "render thread produced frames"
    );

    // Exiting the World scope stops and joins both threads and frees everything they owned.
    scopes.exit(ScopeKind::World)?;
    assert_eq!(Arc::strong_count(&ground_model), 1, "ground model released");
    assert_eq!(Arc::strong_count(&frames), 1, "frame sink released");
    assert_eq!(Arc::strong_count(&outbox.0), 1, "intent outbox released");
    assert!(
        events
            .send(mantis_client::threads::render_thread::PlatformEvent::CloseRequested)
            .is_err(),
        "render thread gone"
    );
    assert_eq!(scopes.current(), Some(ScopeKind::Character));
    Ok(())
}

#[test]
fn timeline_markers_reach_the_render_side_on_the_host_timeline() -> TestResult {
    let mut rig = Rig::new(3, 3, ground())?;
    rig.marker_every = Some(10);
    for _ in 0..120 {
        rig.run_tick(1);
    }
    let mut seen = Vec::new();
    let _ = rig.session.markers.drain(|m| seen.push((m.marker.at, m.at)));
    assert!(seen.len() >= 10, "{} markers", seen.len());
    // Ticks arrive in order and map to increasing host instants one marker period apart.
    for pair in seen.windows(2) {
        let [(tick_a, at_a), (tick_b, at_b)] = pair else {
            continue;
        };
        assert_eq!(tick_b.0 - tick_a.0, 10);
        let gap = at_b.saturating_since(*at_a);
        let period = tick_duration() * 10;
        assert!(gap.abs_diff(period) < tick_duration(), "{gap:?} vs {period:?}");
    }
    Ok(())
}
