//! The `town-300` reference scene (plan 17), end to end: `packages/toy/content` cooked
//! into a temporary signed store, the toy world streamed, 300 animated avatars split 50
//! near, 100 mid, 150 far by the crowd selector, twelve particle braziers, and the town
//! UI, drawn headless at 1920x1080 ([`toy_client::town::TownScene`]).
//!
//! **Budget rows** (scenario `crates/testkit/scenarios/town-300.toml`):
//! - client CPU frame time, p99, under 6 ms: one render-thread frame (streaming,
//!   characters with animation on the worker pool, UI, renderer preparation, recording
//!   the render graph, and the queue submission), on the monotonic clock;
//! - client GPU frame time, p99, under 8 ms: timestamp queries bracketing the frame's
//!   commands ([`mantis_render::gpu_timer::FrameTimer`]). An adapter without timestamp
//!   queries is a counted skip (`MANTIS-GPU-SKIP`) for this row only.
//!
//! Each row takes [`MEASURED`] frames after [`WARM_UP`] frames, in a release build:
//! `cargo test --release -p toy-e2e --test town_300 -- --nocapture`. Debug builds skip
//! the timing test (unoptimized code is not the budgeted build); the structural test runs
//! in every build on any backend.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::{Path, PathBuf};

use mantis_client::world_stream::Gpu;
use mantis_cook::package::{Signing, cook};
use mantis_render::crowd::CrowdStats;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{SKIP_MARKER, hardware_or_skip, noop, record_skip};
use mantis_render::gpu_timer::FrameTimer;
use toy_client::town::{AVATARS, BRAZIERS, TownConfig, TownFrame, TownScene, fixture_fonts};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

/// Reference output size.
const WIDTH: u32 = 1920;
/// Reference output size.
const HEIGHT: u32 = 1080;
/// Frames before measuring (pipelines, history, atlas, and caches settle).
const WARM_UP: usize = 60;
/// Frames measured per row.
const MEASURED: usize = 600;
/// Plan 17 targets.
const CPU_TARGET_MS: f64 = 6.0;
/// Plan 17 targets.
const GPU_TARGET_MS: f64 = 8.0;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("mantis-toy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cooked(dir: &Path) -> Result<PathBuf, Error> {
    let content = Path::new(env!("CARGO_MANIFEST_DIR")).join("../content");
    let out = dir.join("cooked");
    cook(&content, &out, 1, &Signing::Development).map_err(|errors| {
        errors
            .iter()
            .map(|e| format!("{}:{}: {}", e.file, e.line, e.message))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    Ok(out)
}

/// Animation and streaming workers: the reference uses up to four threads.
fn workers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).clamp(1, 4))
}

fn gpu(ctx: &GpuContext) -> Gpu<'_> {
    Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    }
}

fn target(ctx: &GpuContext, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("town output"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    (t, v)
}

fn scene(ctx: &GpuContext, store: &Path, width: u32, height: u32) -> Result<TownScene, Error> {
    let config = TownConfig::new(width, height, wgpu::TextureFormat::Rgba8Unorm, workers());
    let mut scene = TownScene::new(&gpu(ctx), store, None, fixture_fonts()?, config)?;
    let settled = scene.settle(&gpu(ctx), 4000)?;
    assert_eq!(settled.resident, 4, "the four toy sectors stream in");
    Ok(scene)
}

/// One frame: record, submit. With a timer, the frame's commands are bracketed by
/// timestamps.
fn frame(
    scene: &mut TownScene,
    ctx: &GpuContext,
    out: &(wgpu::Texture, wgpu::TextureView),
    timer: Option<&FrameTimer>,
) -> Result<TownFrame, Error> {
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("town") });
    if let Some(t) = timer {
        t.begin(&mut encoder);
    }
    let f = scene.record(&gpu(ctx), &mut encoder, (&out.0, &out.1))?;
    if let Some(t) = timer {
        t.end(&mut encoder);
    }
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(f)
}

fn assert_town(f: &TownFrame) {
    let CrowdStats {
        near,
        mid,
        far,
        culled,
        ..
    } = f.characters.crowd;
    assert_eq!((near, mid, far, culled), (50, 100, 150, 0), "tiers: {f:?}");
    assert_eq!(f.characters.refused, 0, "no renderer refusals: {f:?}");
    assert_eq!(f.emitters as usize, BRAZIERS * 2, "two emitters per brazier");
    assert!(f.ui_quads > 20, "the town UI draws: {f:?}");
    assert_eq!(f.world.resident, 4);
    assert!(f.instances as usize >= AVATARS, "{f:?}");
}

#[test]
fn the_town_scene_holds_its_reference_shape() -> TestResult {
    let dir = TempDir::new("town-shape")?;
    let store = cooked(&dir.0)?;
    let ctx = noop()?;
    let out = target(&ctx, 320, 180);
    let mut town = scene(&ctx, &store, 320, 180)?;
    for _ in 0..30 {
        let f = frame(&mut town, &ctx, &out, None)?;
        assert_town(&f);
    }
    Ok(())
}

/// The value at quantile `q` (nearest rank) of `samples`.
fn quantile(samples: &mut [f64], q: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
    samples.get(rank - 1).copied().unwrap_or(f64::NAN)
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing budgets run in release builds")]
fn town_300_frame_times_hold_the_client_budgets() -> TestResult {
    let Some(ctx) = hardware_or_skip("town_300_frame_times_hold_the_client_budgets") else {
        return Ok(());
    };
    let adapter = ctx.describe();
    let dir = TempDir::new("town-timing")?;
    let store = cooked(&dir.0)?;
    let out = target(&ctx, WIDTH, HEIGHT);
    let mut town = scene(&ctx, &store, WIDTH, HEIGHT)?;
    let timer = FrameTimer::new(&ctx.device, &ctx.queue);
    if timer.is_none() {
        record_skip(
            "town_300_frame_times_hold_the_client_budgets (gpu row)",
            &format!("{adapter} has no timestamp queries"),
        );
    }
    for _ in 0..WARM_UP {
        let _ = frame(&mut town, &ctx, &out, timer.as_ref())?;
        if let Some(t) = &timer {
            let _ = t.read(&ctx.device)?;
        }
    }
    let mut cpu = Vec::with_capacity(MEASURED);
    let mut gpu_ms = Vec::with_capacity(MEASURED);
    for _ in 0..MEASURED {
        let start = std::time::Instant::now();
        let f = frame(&mut town, &ctx, &out, timer.as_ref())?;
        cpu.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_town(&f);
        // Waiting for the GPU here keeps frames from overlapping, so each timestamp pair
        // measures one frame alone; the CPU time above was taken before the wait.
        match &timer {
            Some(t) => {
                if let Some(ms) = t.read(&ctx.device)? {
                    gpu_ms.push(ms);
                }
            }
            None => {
                let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
            }
        }
    }
    let cpu_p50 = quantile(&mut cpu, 0.5);
    let cpu_p99 = quantile(&mut cpu, 0.99);
    println!(
        "budget: client CPU frame time p99 (town-300) = {cpu_p99:.3} ms over {MEASURED} frames, p50 {cpu_p50:.3} ms (target < {CPU_TARGET_MS}) on {adapter}"
    );
    println!(
        "MANTIS-METRIC client_cpu_frame_p99_ms value={cpu_p99:.3} p50={cpu_p50:.3} frames={MEASURED} avatars={AVATARS} near=50 mid=100 far=150 workers={} target={CPU_TARGET_MS}",
        workers()
    );
    if timer.is_some() {
        assert!(
            gpu_ms.len() * 10 >= MEASURED * 9,
            "most frames have usable timestamps ({} of {MEASURED})",
            gpu_ms.len()
        );
        let samples = gpu_ms.len();
        let gpu_p50 = quantile(&mut gpu_ms, 0.5);
        let gpu_p99 = quantile(&mut gpu_ms, 0.99);
        println!(
            "budget: client GPU frame time p99 (town-300) = {gpu_p99:.3} ms over {samples} frames, p50 {gpu_p50:.3} ms (target < {GPU_TARGET_MS}) at {WIDTH}x{HEIGHT} on {adapter}"
        );
        println!(
            "MANTIS-METRIC client_gpu_frame_p99_ms value={gpu_p99:.3} p50={gpu_p50:.3} frames={samples} width={WIDTH} height={HEIGHT} target={GPU_TARGET_MS}"
        );
        assert!(gpu_p99 < GPU_TARGET_MS, "GPU frame p99 {gpu_p99:.3} ms");
    } else {
        println!("{SKIP_MARKER}: client GPU frame time row skipped (no timestamp queries)");
    }
    assert!(cpu_p99 < CPU_TARGET_MS, "CPU frame p99 {cpu_p99:.3} ms");
    Ok(())
}
