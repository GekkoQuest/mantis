//! The client's threads (plan 8.1): simulation, render, streaming pool, audio.
//!
//! Each loop body is a plain struct that runs headless; each thread wrapper owns a join
//! handle and stops and joins on drop, so a scope that owns a thread can never leak it.

pub mod audio;
pub mod render_thread;
pub mod sim_thread;
pub mod streaming;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use super::audio::{AudioBackend, AudioSendError, AudioThread, NullOutput};
    use super::render_thread::{
        FrameContext, FrameSink, LookBindings, PlatformEvent, RenderLoop, RenderLoopParts, RenderThread,
    };
    use super::sim_thread::{SimDriver, SimThread, TickHandler};
    use super::streaming::{Priority, StreamJob, StreamingPool};
    use crate::camera::{CameraRig, LookConfig};
    use crate::core_api::Tick;
    use crate::input::accumulator::InputAccumulator;
    use crate::input::action::ActionTable;
    use crate::input::binding::ContextTable;
    use crate::input::device::RawInput;
    use crate::input::router::InputRouter;
    use crate::render_world::{PresentationConfig, RenderWorld, render_world_channel};
    use crate::testing::{TestResult, rate};
    use crate::time::{FixedStepper, HostClock, HostInstant, ManualClock};

    /// Waits on real time for a condition, bounded so a broken test fails instead of hangs.
    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..10_000 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[derive(Default)]
    struct CountingHandler {
        ticks: Vec<(Tick, HostInstant)>,
    }

    impl TickHandler for CountingHandler {
        fn tick(&mut self, tick: Tick, tick_time: HostInstant, _world: &mut RenderWorld) {
            self.ticks.push((tick, tick_time));
        }
    }

    #[test]
    fn sim_driver_runs_due_ticks_and_publishes_each() -> TestResult {
        let (publisher, mut reader) = render_world_channel(0, PresentationConfig::default());
        let stepper = FixedStepper::new(rate(30)?, Tick(0), HostInstant::ZERO, 4);
        let mut d = SimDriver::new(stepper, CountingHandler::default(), publisher);
        let _ = d.run_due(HostInstant::ZERO);
        let _ = d.run_due(HostInstant::from_nanos(100_000_000));
        let ticks: Vec<Tick> = d.handler().ticks.iter().map(|(t, _)| *t).collect();
        assert_eq!(ticks, vec![Tick(0), Tick(1), Tick(2), Tick(3)]);
        assert_eq!(d.published(), 4);
        let w = reader.acquire();
        assert_eq!(
            (w.tick(), w.published_at()),
            (Tick(3), HostInstant::from_nanos(100_000_000))
        );
        Ok(())
    }

    #[test]
    fn sim_thread_follows_the_injected_clock() -> TestResult {
        let clock = Arc::new(ManualClock::new());
        let (publisher, mut reader) = render_world_channel(0, PresentationConfig::default());
        let stepper = FixedStepper::new(rate(30)?, Tick(0), HostInstant::ZERO, 100);
        let driver = SimDriver::new(stepper, CountingHandler::default(), publisher);
        let sim = SimThread::spawn(driver, clock.clone(), Duration::from_millis(2))?;
        assert!(wait_until(
            || reader.acquire().tick() == Tick(0) && reader.acquired_publish() == 1
        ));
        // One simulated second: ticks 1..=30 become due, however long the thread slept.
        clock.advance(Duration::from_secs(1));
        sim.wake();
        assert!(wait_until(|| reader.acquire().tick() == Tick(30)));
        let driver = sim.stop()?;
        assert_eq!(driver.handler().ticks.len(), 31);
        Ok(())
    }

    #[derive(Default)]
    struct RecordingSink {
        frames: Vec<(u64, Tick, usize)>,
        resized: Option<(u32, u32)>,
    }

    impl FrameSink for RecordingSink {
        fn resize(&mut self, width: u32, height: u32) {
            self.resized = Some((width, height));
        }
        fn submit(&mut self, f: &FrameContext<'_>) {
            self.frames
                .push((f.frame, f.world.tick(), f.poses.as_slice().len()));
        }
    }

    fn render_loop(
        clock: Arc<ManualClock>,
    ) -> (
        RenderLoop<RecordingSink>,
        crate::render_world::RenderWorldPublisher,
        std::sync::mpsc::Sender<PlatformEvent>,
        Arc<InputAccumulator>,
    ) {
        let (publisher, reader) = render_world_channel(4, PresentationConfig::default());
        let (tx, rx) = channel();
        let acc = Arc::new(InputAccumulator::new());
        let rl = RenderLoop::new(RenderLoopParts {
            clock,
            reader,
            router: InputRouter::new(ActionTable::new(), ContextTable::new()),
            camera: CameraRig::new(LookConfig {
                mouse_turns_per_count: 0.001,
                ..LookConfig::default()
            }),
            look: LookBindings::default(),
            accumulator: Arc::clone(&acc),
            events: rx,
            pose_capacity: 8,
            sink: RecordingSink::default(),
        });
        (rl, publisher, tx, acc)
    }

    #[test]
    fn render_loop_frames_are_decoupled_from_ticks() -> TestResult {
        let clock = Arc::new(ManualClock::new());
        let (mut rl, mut publisher, tx, acc) = render_loop(clock.clone());
        // Five frames per tick at a high refresh rate: each renders the latest world.
        for tick in 1..=3u64 {
            publisher.back_mut().begin(Tick(tick), clock.now());
            let _ = publisher.publish();
            for _ in 0..5 {
                clock.advance(Duration::from_micros(6_944));
                let r = rl.frame();
                assert_eq!(r.world_tick, Tick(tick));
            }
        }
        assert_eq!(rl.sink().frames.len(), 15);
        // Mouse look is applied on the frame it arrives and handed to the sim quantized.
        tx.send(PlatformEvent::Input(RawInput::MouseMotion { dx: 250.0, dy: 0.0 }))?;
        tx.send(PlatformEvent::Resized {
            width: 640,
            height: 360,
        })?;
        let r = rl.frame();
        assert_eq!(r.events, 2);
        assert!((rl.camera().yaw_turns() - 0.25).abs() < 1e-6);
        assert_eq!(acc.take().look.yaw.0, 16_384);
        assert_eq!(rl.sink().resized, Some((640, 360)));
        // Closing the platform channel requests close.
        drop(tx);
        assert!(rl.frame().close_requested);
        Ok(())
    }

    #[test]
    fn render_thread_stops_on_close_request() -> TestResult {
        let clock = Arc::new(ManualClock::new());
        let (rl, _publisher, tx, _acc) = render_loop(clock);
        let rt = RenderThread::spawn(rl, Some(Duration::from_millis(1)))?;
        tx.send(PlatformEvent::CloseRequested)?;
        assert!(wait_until(|| rt.is_finished()));
        let rl = rt.stop()?;
        assert!(!rl.sink().frames.is_empty());
        Ok(())
    }

    struct Job {
        value: u32,
        started: Arc<AtomicU32>,
        gate: Arc<std::sync::Barrier>,
    }

    impl StreamJob for Job {
        type Output = u32;
        fn run(self) -> u32 {
            self.started.fetch_add(1, Ordering::SeqCst);
            if self.value == 0 {
                let _ = self.gate.wait();
            }
            assert!(self.value != 13, "simulated decode failure");
            self.value
        }
    }

    #[test]
    fn streaming_runs_by_priority_and_reports_failures() -> TestResult {
        let started = Arc::new(AtomicU32::new(0));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let mut pool = StreamingPool::new(1)?;
        // Occupy the single worker so the queue order is decided while all are queued.
        let _blocker = pool.submit(
            Job {
                value: 0,
                started: started.clone(),
                gate: gate.clone(),
            },
            Priority(0),
        );
        assert!(wait_until(|| started.load(Ordering::SeqCst) == 1));
        let low = pool.submit(
            Job {
                value: 1,
                started: started.clone(),
                gate: gate.clone(),
            },
            Priority(1),
        );
        let _high = pool.submit(
            Job {
                value: 2,
                started: started.clone(),
                gate: gate.clone(),
            },
            Priority(5),
        );
        let cancelled = pool.submit(
            Job {
                value: 3,
                started: started.clone(),
                gate: gate.clone(),
            },
            Priority(9),
        );
        let _fail = pool.submit(
            Job {
                value: 13,
                started: started.clone(),
                gate: gate.clone(),
            },
            Priority(4),
        );
        assert!(pool.reprioritize(low, Priority(7)));
        assert!(pool.cancel(cancelled));
        assert!(!pool.cancel(cancelled));
        let _ = gate.wait();
        assert!(wait_until(|| pool.finished() == 4));
        let clock = ManualClock::new();
        let mut order = Vec::new();
        let mut failed = 0;
        let r = pool.drain_budgeted(
            &clock,
            Duration::from_millis(2),
            |_| Duration::ZERO,
            |_, v| order.push(v),
            |_| failed += 1,
        );
        assert_eq!(
            order,
            vec![0, 1, 2],
            "blocker, then reprioritized low (7), then high (5)"
        );
        assert_eq!((failed, r.handed_off, r.failed, r.remaining), (1, 3, 1, false));
        Ok(())
    }

    struct Tiny(u32);

    impl StreamJob for Tiny {
        type Output = u32;
        fn run(self) -> u32 {
            self.0
        }
    }

    #[test]
    fn streaming_handoff_respects_the_frame_budget() -> TestResult {
        let mut pool = StreamingPool::new(2)?;
        for i in 0..10 {
            let _ = pool.submit(Tiny(i), Priority(0));
        }
        assert!(wait_until(|| pool.finished() == 10));
        let clock = ManualClock::new();
        // Each hand-off costs 0.75 ms of host time; the budget is 2 ms.
        let cost = Duration::from_micros(750);
        let mut frames = Vec::new();
        loop {
            let mut n = 0;
            let r = pool.drain_budgeted(
                &clock,
                Duration::from_millis(2),
                |_| cost,
                |_, _| {
                    clock.advance(cost);
                    n += 1;
                },
                |_| {},
            );
            assert!(r.elapsed <= Duration::from_millis(2), "{:?}", r.elapsed);
            frames.push(n);
            if !r.remaining {
                break;
            }
        }
        // Two fit (1.5 ms); a third is predicted to overrun (2.25 ms) and waits.
        assert_eq!(frames, vec![2, 2, 2, 2, 2]);
        // An item larger than the whole budget still goes alone, so progress never stalls.
        let _ = pool.submit(Tiny(99), Priority(0));
        assert!(wait_until(|| pool.finished() == 1));
        let mut got = 0;
        let r = pool.drain_budgeted(
            &clock,
            Duration::from_millis(2),
            |_| Duration::from_millis(5),
            |_, v| got = v,
            |_| {},
        );
        assert_eq!((got, r.handed_off), (99, 1));
        Ok(())
    }

    struct SumBackend {
        events: u32,
        blocks: u32,
    }

    impl AudioBackend for SumBackend {
        type Event = u32;
        fn handle(&mut self, e: u32) {
            self.events += e;
        }
        fn render(&mut self, out: &mut [f32]) {
            self.blocks += 1;
            out.fill(0.25);
        }
    }

    #[test]
    fn audio_queue_is_bounded_and_never_blocks() -> TestResult {
        let out = NullOutput {
            block_len: 64,
            block_duration: Duration::from_millis(50),
        };
        let (audio, tx) = AudioThread::spawn(SumBackend { events: 0, blocks: 0 }, out, 4)?;
        let mut full = 0;
        for _ in 0..1000 {
            match tx.send(1) {
                Ok(()) => {}
                Err(AudioSendError::Full) => full += 1,
                Err(AudioSendError::Stopped) => return Err("audio stopped early".into()),
            }
        }
        assert_eq!(u64::try_from(full)?, tx.dropped());
        let (backend, _) = audio.stop()?;
        assert_eq!(
            u64::from(backend.events) + tx.dropped(),
            1000,
            "every event is applied or counted"
        );
        assert_eq!(tx.send(1), Err(AudioSendError::Stopped));
        Ok(())
    }
}
