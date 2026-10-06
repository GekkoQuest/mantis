//! The world view draws the render world: a world session's render loop feeding a
//! `WorldView` on the no-op GPU backend (no window), with a proxy per entity, a
//! third-person camera, and the UI pass, all passing `wgpu` validation.

use std::sync::{Arc, Mutex, PoisonError};

use mantis_client::core_api::{
    Angle16, CoreMotion, EntityId, FlatGround, InputSeq, Motion, MotionParams, MotionState, MoveInput, Tick,
    TickRate, Vec3,
};
use mantis_client::host::{
    WorldSessionConfig, WorldSessionParts, build_world_session, platform_event_channel,
};
use mantis_client::input::action::ActionTable;
use mantis_client::input::binding::ContextTable;
use mantis_client::input::intent::MoveIntentMap;
use mantis_client::input::router::InputRouter;
use mantis_client::sim::IntentSink;
use mantis_client::snapshot::RemoteState;
use mantis_client::threads::render_thread::{FrameContext, FrameSink};
use mantis_client::time::{HostClock, HostInstant, ManualClock, tick_start_nanos};
use mantis_client::world_view::{WorldView, WorldViewConfig};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{noop, validation_errors};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Discard;

impl IntentSink for Discard {
    fn send_move(&mut self, _input: &MoveInput) {}
}

#[derive(Default)]
struct Seen {
    frames: u32,
    entities: usize,
    failures: u32,
}

struct OffscreenSink {
    ctx: GpuContext,
    view: WorldView,
    target: (wgpu::Texture, wgpu::TextureView),
    seen: Arc<Mutex<Seen>>,
}

impl FrameSink for OffscreenSink {
    fn resize(&mut self, _width: u32, _height: u32) {}

    fn submit(&mut self, frame: &FrameContext<'_>) {
        let _ = self.view.update(frame);
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let ok = self
            .view
            .encode(
                &self.ctx.device,
                &self.ctx.queue,
                &mut encoder,
                (&self.target.0, &self.target.1),
            )
            .is_ok();
        let _ = self.ctx.queue.submit(Some(encoder.finish()));
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.frames += 1;
        seen.entities = self.view.entity_count();
        seen.failures += u32::from(!ok);
    }
}

#[test]
fn the_world_view_draws_every_entity_and_passes_validation() -> TestResult {
    let ctx = noop()?;
    let device = ctx.device.clone();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut result: TestResult = Ok(());
    let errors = validation_errors(&device, || {
        result = (|| {
            let mut config = WorldViewConfig::new(128, 96, wgpu::TextureFormat::Rgba8Unorm);
            config.renderer.max_instances = 64;
            config.renderer.max_batches = 8;
            config.renderer.shadow_resolution = 128;
            config.renderer.max_vertices = 4096;
            config.renderer.max_indices = 8192;
            let view = WorldView::new(&ctx.device, &ctx.queue, false, config)?;
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("output"),
                size: wgpu::Extent3d {
                    width: 128,
                    height: 96,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let target_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let rate = TickRate::new(30).ok_or("rate")?;
            let clock = Arc::new(ManualClock::new());
            let (_events, rx) = platform_event_channel();
            let session_config = WorldSessionConfig::new(rate);
            let mut session = build_world_session(
                &session_config,
                WorldSessionParts {
                    motion: CoreMotion::<FlatGround>::new(Motion::new(MotionParams::DEFAULT)?),
                    ground: Arc::new(FlatGround(0.0)),
                    initial_state: MotionState::at_rest(Vec3::ZERO, Angle16(0)),
                    router: InputRouter::new(ActionTable::new(), ContextTable::new()),
                    intents: MoveIntentMap::new(),
                    outbox: Discard,
                },
                Arc::clone(&clock) as Arc<dyn HostClock>,
                rx,
                OffscreenSink {
                    ctx: GpuContext::from_parts(
                        ctx.instance.clone(),
                        ctx.adapter.clone(),
                        ctx.device.clone(),
                        ctx.queue.clone(),
                    ),
                    view,
                    target: (texture, target_view),
                    seen: Arc::clone(&seen),
                },
            );
            for k in 1..=40u64 {
                clock.set(HostInstant::from_nanos(u64::try_from(tick_start_nanos(
                    rate,
                    Tick(k),
                ))?));
                if let Some(mut frame) = session.snapshots.acquire() {
                    frame.server_tick = Tick(k);
                    frame.received_at = clock.now();
                    frame.ack = Some(InputSeq(0));
                    frame.local = Some((EntityId::new(1, 0), MotionState::at_rest(Vec3::ZERO, Angle16(0))));
                    let _ = frame.push_remote(RemoteState {
                        id: EntityId::new(2, 0),
                        tick: Tick(k),
                        position: Vec3::new(2.0, 0.0, 5.0),
                        velocity: Vec3::ZERO,
                        yaw: Angle16(0),
                    });
                    let _ = session.snapshots.send(frame);
                }
                let _ = session.sim.run_due(clock.now());
                let _ = session.render.frame();
            }
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!((seen.frames, seen.failures), (40, 0));
    assert_eq!(seen.entities, 2, "the local avatar and the remote entity");
    Ok(())
}
