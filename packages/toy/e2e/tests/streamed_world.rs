//! The toy's cooked world, end to end: `packages/toy/content` cooked into a temporary
//! signed store with a per-run development key, every bundle verified before anything is
//! read, and the four toy sectors streamed around a walking camera into a headless
//! renderer through the client's streaming pool.
//!
//! **Budget row** (plan 17, sector stream-in render-thread hand-off under 2 ms per
//! frame): the hand-off time of every frame that handed a sector to the renderer is
//! measured on the monotonic clock, and the row is the worst of them. The world's
//! materials are compiled at load time, before streaming starts
//! (`WorldStreamer::preload_materials`), and that load cost is reported beside the row.
//! In a debug build the row is printed but not asserted (unoptimized code is not the
//! budgeted build); in release it must hold. Structure (residency, unloads, instances, world-light
//! pages and bricks) is asserted on every backend; pixels only on a real adapter (on the
//! no-op backend the pixel path is a counted skip).

#![expect(clippy::cast_precision_loss, clippy::too_many_lines)] // Test camera paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use glam::Vec3;
use mantis_client::platform::MonotonicClock;
use mantis_client::time::HostClock;
use mantis_client::world_stream::{Gpu, HANDOFF_BUDGET, WorldStreamer};
use mantis_cook::package::{Signing, cook};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{SKIP_MARKER, hardware_or_skip, noop, read_texture_4bpp};
use mantis_render::math::Camera;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const SIZE: u32 = 128;
const CLEAR: [f64; 4] = [0.0, 0.0, 0.0, 1.0];

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

fn content() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../content")
}

/// Cooks the toy content into `dir/cooked` with a key generated for this run.
fn cooked(dir: &Path) -> Result<PathBuf, Error> {
    let out = dir.join("cooked");
    let c = cook(&content(), &out, 1, &Signing::Development).map_err(|errors| {
        errors
            .iter()
            .map(|e| format!("{}:{}: {}", e.file, e.line, e.message))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    assert!(c.assets > 20, "{} assets", c.assets);
    Ok(out)
}

fn renderer(ctx: &GpuContext) -> Result<Renderer, Error> {
    let mut config = RendererConfig::new(SIZE, SIZE, wgpu::TextureFormat::Rgba8Unorm);
    config.max_instances = 256;
    config.max_vertices = 1 << 16;
    config.max_indices = 1 << 17;
    config.shadow_resolution = 512;
    Ok(Renderer::new(
        &ctx.device,
        &ctx.queue,
        ctx.capabilities.bindless_textures,
        config,
    )?)
}

fn camera_at(p: Vec3) -> Camera {
    Camera {
        position: p + Vec3::new(0.0, 14.0, -18.0),
        yaw: 0.0,
        pitch: -0.6,
        fov_y: 1.1,
        aspect: 1.0,
        near: 0.1,
    }
}

fn draw(ctx: &GpuContext, r: &mut Renderer, camera: Camera, time: f32) -> Result<wgpu::Texture, Error> {
    let _ = r.prepare(&FrameInputs {
        camera,
        time,
        sun_direction: Vec3::new(0.35, -1.0, 0.25),
        sun_color: Vec3::splat(2.0),
        shadows: true,
        sky: mantis_formats_sky(),
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: CLEAR,
        ssao_strength: 0.5,
        post: PostSettings::OFF,
    });
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("toy.frame"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
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
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&t, &v))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(t)
}

/// No sky term: everything visible comes from the streamed world's probes, lightmaps,
/// and the sun.
fn mantis_formats_sky() -> mantis_render::lighting::sh::ShL1 {
    mantis_render::lighting::sh::ShL1::ZERO
}

#[derive(Default)]
struct Walk {
    handoffs: Vec<Duration>,
    sectors_handed: u32,
    max_resident: u32,
    unloaded: u32,
}

/// Walks the camera from `from` to `to` in `steps`, streaming and drawing every frame,
/// then lets streaming settle at `to`.
fn walk(
    ctx: &GpuContext,
    r: &mut Renderer,
    w: &mut WorldStreamer,
    clock: &dyn HostClock,
    (from, to, steps): (Vec3, Vec3, u32),
    out: &mut Walk,
) -> Result<(), Error> {
    let gpu = Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    };
    let step = (to - from) / steps as f32;
    for i in 0..steps + 400 {
        let at = from + step * i.min(steps) as f32;
        let camera = camera_at(at);
        let stats = w.update(
            r,
            &gpu,
            clock,
            HANDOFF_BUDGET,
            (camera.position, step * 30.0, camera.forward()),
        );
        if stats.handed_off > 0 {
            out.handoffs.push(stats.handoff);
            out.sectors_handed += stats.handed_off;
        }
        out.unloaded += stats.unloaded;
        out.max_resident = out.max_resident.max(stats.resident);
        let errors = w.take_errors();
        assert!(errors.is_empty(), "{errors:?}");
        let _ = draw(ctx, r, camera, i as f32 / 30.0)?;
        if i >= steps && stats.stream.in_flight == 0 && !stats.backlog && stats.stream.requested == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err("streaming did not settle".into())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if entry.file_name() != "cooked" {
                copy_tree(&entry.path(), &target)?;
            }
        } else {
            let _ = std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[test]
fn visual_iteration_never_changes_the_handshake_hash() -> TestResult {
    // Decision 0020: re-baking light and repainting a texture change the presentation
    // bundle only.
    let dir = TempDir::new("world-visual")?;
    let original = dir.0.join("a");
    let edited = dir.0.join("b");
    copy_tree(&content(), &original.join("content"))?;
    copy_tree(&content(), &edited.join("content"))?;
    let sector = edited.join("content/sectors/0_0.sector.toml");
    let text = std::fs::read_to_string(&sector)?;
    let rebaked = text.replace("sun_color = [2.0, 1.95, 1.85]", "sun_color = [1.5, 1.4, 1.2]");
    assert_ne!(rebaked, text, "the noon keyframe is in the toy sector");
    std::fs::write(&sector, rebaked)?;
    let texture = edited.join("content/textures/planks.ppm");
    let mut bytes = std::fs::read(&texture)?;
    if let Some(b) = bytes.last_mut() {
        *b ^= 0x3f;
    }
    std::fs::write(&texture, bytes)?;
    let cook_at = |root: &Path| {
        cook(
            &root.join("content"),
            &root.join("cooked"),
            1,
            &Signing::Development,
        )
        .map_err(|e| format!("{e:?}"))
    };
    let (a, b) = (cook_at(&original)?, cook_at(&edited)?);
    let [gameplay_a, server_a, presentation_a] = a.hashes;
    let [gameplay_b, server_b, presentation_b] = b.hashes;
    assert_eq!(gameplay_a, gameplay_b, "the handshake hash ignores visuals");
    assert_eq!(server_a, server_b);
    assert_ne!(presentation_a, presentation_b, "the presentation bundle changed");
    Ok(())
}

#[test]
fn the_toy_world_cooks_signed_and_refuses_a_wrong_key() -> TestResult {
    let dir = TempDir::new("world-keys")?;
    let store = cooked(&dir.0)?;
    let opened = toy_client::world::open(&store, None, 1)?;
    assert_eq!(opened.sectors, 4, "four toy sectors");
    let key = std::fs::read(store.join(toy_client::world::DEV_PUBLIC_KEY))?;
    let mut wrong = <[u8; 32]>::try_from(key.as_slice())?;
    wrong[7] ^= 0x40;
    let refused = toy_client::world::open(&store, Some(wrong), 1);
    assert!(
        refused.as_ref().is_err_and(|e| e.contains("signature")),
        "{:?}",
        refused.map(|o| o.sectors)
    );
    // A second cook signs with a new key: the content hash (the bundle manifest) is the
    // same, the signature is not.
    let again = TempDir::new("world-keys-again")?;
    let second = cooked(&again.0)?;
    let reopened = toy_client::world::open(&second, None, 1)?;
    assert_eq!(
        reopened.content_hash, opened.content_hash,
        "cooking is deterministic"
    );
    assert_ne!(
        std::fs::read(second.join(toy_client::world::DEV_PUBLIC_KEY))?,
        key,
        "a development key is generated per run"
    );
    Ok(())
}

#[test]
fn the_toy_client_renders_the_cooked_world_streamed() -> TestResult {
    let dir = TempDir::new("world-stream")?;
    let store = cooked(&dir.0)?;
    let hardware = hardware_or_skip("the_toy_client_renders_the_cooked_world_streamed (pixels)");
    let pixels = hardware.is_some();
    let ctx = match hardware {
        Some(ctx) => ctx,
        None => noop()?,
    };
    let mut r = renderer(&ctx)?;
    let opened = toy_client::world::open(&store, None, 2)?;
    let mut w = opened.streamer;
    let gpu = Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    };
    let load_start = std::time::Instant::now();
    let compiled = w.preload_materials(&mut r, &gpu)?;
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(compiled, 2, "the ground and crate materials");
    let clock: Arc<dyn HostClock> = Arc::new(MonotonicClock::new());
    let mut stats = Walk::default();
    // Approach from far west (nothing in range), walk to the middle (all four load),
    // then on east until the western sectors fall behind and unload.
    let west = Vec3::new(-110.0, 0.0, 0.0);
    let middle = Vec3::new(0.0, 0.0, 0.0);
    let east = Vec3::new(60.0, 0.0, 0.0);
    walk(
        &ctx,
        &mut r,
        &mut w,
        clock.as_ref(),
        (west, middle, 120),
        &mut stats,
    )?;
    assert_eq!(w.resident(), vec![(-1, -1), (-1, 0), (0, -1), (0, 0)]);
    let instances = r.scene_mut().instance_count();
    assert_eq!(instances, 4 + 10, "a ground per sector plus the ten crates");
    assert_eq!(r.world_light().resident_pages(), 4, "one lightmap per sector");
    assert_eq!(
        r.world_light().resident_sectors(),
        4,
        "one probe brick per sector"
    );
    if pixels {
        let frame = draw(&ctx, &mut r, camera_at(middle), 0.0)?;
        let px = read_texture_4bpp(&ctx, &frame)?;
        // The lower half of the view is streamed ground (the sky above the world's edge
        // stays the clear color).
        let half = (SIZE * SIZE * 2) as usize;
        let lower = px.get(half..).ok_or("frame")?;
        let lit = lower
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|[r, g, b, _]| *r > 8 || *g > 8 || *b > 8)
            .count();
        assert_eq!(lit, half / 4, "the streamed ground fills the lower half");
        let mut colors: Vec<[u8; 3]> = px
            .as_chunks::<4>()
            .0
            .iter()
            .map(|[r, g, b, _]| [*r, *g, *b])
            .collect();
        colors.sort_unstable();
        colors.dedup();
        assert!(
            colors.len() > 16,
            "textured, lit, shadowed: {} colors",
            colors.len()
        );
    } else {
        println!("{SKIP_MARKER}: streamed world pixels (no-op backend; structure asserted)");
    }
    walk(
        &ctx,
        &mut r,
        &mut w,
        clock.as_ref(),
        (middle, east, 60),
        &mut stats,
    )?;
    assert_eq!(
        w.resident(),
        vec![(0, -1), (0, 0)],
        "the western sectors unloaded"
    );
    assert_eq!(r.scene_mut().instance_count(), 2 + 6);
    assert_eq!(r.world_light().resident_pages(), 2);
    assert_eq!(r.world_light().resident_sectors(), 2);
    assert!(stats.unloaded >= 2);
    assert_eq!(stats.max_resident, 4);
    let worst = stats.handoffs.iter().max().copied().unwrap_or_default();
    let worst_ms = worst.as_secs_f64() * 1000.0;
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "budget: Sector stream-in render-thread hand-off (toy world, {build}) = {worst_ms:.3} ms worst per frame over {} hand-offs, {} sectors (materials compiled at load: {load_ms:.1} ms; target < 2.0)",
        stats.handoffs.len(),
        stats.sectors_handed
    );
    println!(
        "MANTIS-METRIC client_sector_handoff_ms value={worst_ms:.3} load_ms={load_ms:.1} handoffs={} sectors={} build={build} pixels={pixels} target=2.0",
        stats.handoffs.len(),
        stats.sectors_handed
    );
    assert!(stats.sectors_handed >= 4);
    if !cfg!(debug_assertions) {
        assert!(
            worst < HANDOFF_BUDGET,
            "hand-off {worst_ms:.3} ms over the 2 ms row"
        );
    }
    Ok(())
}

#[test]
fn streaming_the_toy_world_repeatedly_holds_residency_flat() -> TestResult {
    let dir = TempDir::new("world-laps")?;
    let store = cooked(&dir.0)?;
    let ctx = noop()?;
    let mut r = renderer(&ctx)?;
    let opened = toy_client::world::open(&store, None, 2)?;
    let mut w = opened.streamer;
    // Evict as soon as nothing uses an asset: the strictest setting.
    w.set_cache_budget(0);
    let clock: Arc<dyn HostClock> = Arc::new(MonotonicClock::new());
    let west = Vec3::new(-200.0, 0.0, 0.0);
    let east = Vec3::new(200.0, 0.0, 0.0);
    let mut stats = Walk::default();
    let mut peaks = Vec::new();
    let mut aways = Vec::new();
    let space = r.mesh_space();
    for lap in 0..4 {
        let (from, to) = if lap % 2 == 0 { (west, east) } else { (east, west) };
        let mut peak = mantis_client::world_stream::Residency::default();
        // Walk across in steps, sampling residency as sectors come and go.
        let steps = 40u32;
        let step = (to - from) / steps as f32;
        for i in 0..=steps {
            walk(
                &ctx,
                &mut r,
                &mut w,
                clock.as_ref(),
                (from + step * i as f32, from + step * i as f32, 1),
                &mut stats,
            )?;
            let now = w.residency();
            peak.sectors = peak.sectors.max(now.sectors);
            peak.meshes = peak.meshes.max(now.meshes);
            peak.materials = peak.materials.max(now.materials);
            peak.textures = peak.textures.max(now.textures);
        }
        let away = w.residency();
        aways.push((away.sectors, away.meshes, away.materials, away.textures));
        peaks.push((peak.sectors, peak.meshes, peak.materials, peak.textures));
        assert_eq!(r.world_light().resident_pages(), 0, "lap {lap}: pages released");
        assert_eq!(
            r.world_light().resident_sectors(),
            0,
            "lap {lap}: bricks released"
        );
        assert_eq!(r.mesh_space(), space, "lap {lap}: every mesh range returned");
    }
    assert!(
        peaks.windows(2).all(|p| p.first() == p.get(1)),
        "flat peaks: {peaks:?}"
    );
    assert!(
        aways.windows(2).all(|p| p.first() == p.get(1)),
        "flat lows: {aways:?}"
    );
    let (sectors, meshes, materials, textures) = peaks.first().copied().unwrap_or_default();
    assert_eq!(sectors, 4, "the whole toy world was resident at the peak");
    assert_eq!(
        aways.first().copied().unwrap_or_default(),
        (0, 0, 0, 0),
        "nothing left far away"
    );
    let evicted = w.residency().evicted;
    println!(
        "MANTIS-METRIC client_peak_residency sectors={sectors} meshes={meshes} materials={materials} textures={textures} laps=4 evicted={evicted} budget=0"
    );
    Ok(())
}
