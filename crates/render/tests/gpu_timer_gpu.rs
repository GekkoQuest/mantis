//! The GPU frame timer: bracketing a frame's commands with timestamps passes API
//! validation on every backend, and on a real adapter with timestamp queries it reads a
//! positive, plausible time. Without timestamp queries there is no timer, and the
//! hardware half is a counted skip.

use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, record_skip, validation_errors};
use mantis_render::gpu_timer::FrameTimer;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Clears a target `passes` times between the two timestamps and returns the reading.
fn timed_clears(ctx: &GpuContext, timer: &FrameTimer, passes: u32) -> Result<Option<f64>, String> {
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("timed"),
        size: wgpu::Extent3d {
            width: 512,
            height: 512,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("timed") });
    timer.begin(&mut encoder);
    for _ in 0..passes {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        drop(pass);
    }
    timer.end(&mut encoder);
    let _ = ctx.queue.submit(Some(encoder.finish()));
    timer.read(&ctx.device)
}

#[test]
fn the_frame_timer_validates_on_every_backend() -> TestResult {
    let ctx = noop()?;
    let Some(timer) = FrameTimer::new(&ctx.device, &ctx.queue) else {
        // A device without timestamp queries has no timer; nothing else to check here.
        assert!(!ctx.capabilities.timestamp_queries);
        return Ok(());
    };
    let mut outcome = Ok(None);
    let errors = validation_errors(&ctx.device, || outcome = timed_clears(&ctx, &timer, 2));
    assert_eq!(errors, None);
    let _ = outcome?;
    Ok(())
}

#[test]
fn the_frame_timer_reads_a_plausible_time_on_hardware() -> TestResult {
    let Some(ctx) = hardware_or_skip("the_frame_timer_reads_a_plausible_time_on_hardware") else {
        return Ok(());
    };
    let Some(timer) = FrameTimer::new(&ctx.device, &ctx.queue) else {
        record_skip(
            "the_frame_timer_reads_a_plausible_time_on_hardware",
            &format!("{} has no timestamp queries", ctx.describe()),
        );
        return Ok(());
    };
    assert!(ctx.capabilities.timestamp_queries);
    // The timer is reused frame after frame.
    for _ in 0..3 {
        let ms = timed_clears(&ctx, &timer, 16)?.ok_or("unusable timestamps")?;
        assert!(ms > 0.0 && ms < 1000.0, "{ms} ms");
    }
    Ok(())
}
