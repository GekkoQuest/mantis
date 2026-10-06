//! Zero engine allocation on the client hot paths (CLAUDE.md rule 4, plan 17): after
//! warm-up, a simulation tick (snapshot intake, reconciliation, prediction, render-world
//! publish) and a render frame (event drain, input routing, camera, render-world acquire,
//! pose sampling, submission to the sink) perform no heap operation at all.

mod support;

use mantis_client::core_api::{EntityId, Tick, Vec3};
use mantis_client::input::device::KeyCode;
use mantis_client::render_world::{
    FramePoses, PresentationConfig, RemoteEntity, RemoteSample, render_world_channel,
};
use mantis_client::time::{HostClock, HostInstant};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};
use support::{Rig, TestResult, ground};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

#[test]
fn render_world_publish_and_consume_allocate_nothing_after_warm_up() -> TestResult {
    let (mut publisher, mut reader) = render_world_channel(64, PresentationConfig::default());
    let mut poses = FramePoses::with_capacity(65);
    let sample = RemoteSample {
        time: HostInstant::from_nanos(1),
        position: Vec3::new(1.0, 0.0, 0.0),
        ..RemoteSample::default()
    };
    let remote = RemoteEntity::new(EntityId::new(3, 0), &[sample]).ok_or("window")?;
    // Warm-up: rotate every buffer once.
    for t in 0..3 {
        publisher.back_mut().begin(Tick(t), HostInstant::from_nanos(t));
        let _ = publisher.publish();
        let _ = reader.acquire();
    }
    let ticks: u32 = assert_no_alloc("render world publish/acquire/sample", || {
        let mut n = 0;
        for t in 3..200u64 {
            let w = publisher.back_mut();
            w.begin(Tick(t), HostInstant::from_nanos(t));
            for _ in 0..64 {
                let _ = w.push_remote(remote);
            }
            let _ = w.push_remote(remote); // overflow is counted, not grown
            let _ = publisher.publish();
            reader
                .acquire()
                .sample_into(HostInstant::from_nanos(t), &mut poses);
            n += 1;
        }
        n
    });
    assert_eq!(ticks, 197);
    assert_eq!(poses.as_slice().len(), 64);
    Ok(())
}

#[test]
fn sim_tick_and_render_frame_allocate_nothing_after_warm_up() -> TestResult {
    let mut rig = Rig::new(3, 3, ground())?;
    // Warm-up: spawn, start moving, fill every buffer and remote track at least once.
    rig.key(KeyCode::W, true)?;
    for _ in 0..60 {
        rig.run_tick(2);
    }
    // Measured: the sim tick and the render frames, with snapshots arriving, inputs being
    // predicted and reconciled, and platform events flowing. The stand-in server and the
    // test's network plumbing run outside the measured closures.
    for k in 0..60u64 {
        if k % 10 == 0 {
            rig.key(KeyCode::W, k % 20 == 0)?;
        }
        rig.pin_clock();
        rig.deliver_snapshots();
        let now = rig.clock.now();
        let batch = assert_no_alloc("client sim tick", || rig.session.sim.run_due(now));
        assert_eq!(batch.count, 1);
        rig.outbox.drain_into(&mut rig.sent);
        while let Some(m) = rig.sent.pop_front() {
            rig.server
                .uplink
                .push_back((rig.client_tick + rig.server.up_ticks, m));
        }
        rig.server.step(rig.client_tick);
        let report = assert_no_alloc("render frame", || rig.session.render.frame());
        assert!(!report.close_requested);
        let (_, stats) = count_allocs(|| rig.session.render.frame());
        assert!(stats.is_zero(), "second frame of the tick: {stats}");
        rig.client_tick += 1;
    }
    let sim = rig.session.sim.handler();
    assert!(
        sim.stats().snapshots > 100 && sim.remote_count() == 1,
        "{:?}",
        sim.stats()
    );
    // The measured frames rendered real content: the avatar and the remote entity, at
    // advancing instants, against the configured tick rate.
    assert_eq!(rig.config.tick_rate.hz(), support::HZ);
    let frames = rig.frames();
    let last = frames.last().ok_or("no frames")?;
    assert!(last.local.is_some() && last.npc.is_some());
    assert!(
        frames
            .windows(2)
            .all(|w| matches!(w, [a, b] if a.now_nanos <= b.now_nanos))
    );
    assert_eq!(last.world_tick.0, rig.client_tick - 1);
    Ok(())
}
