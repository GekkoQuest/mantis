//! Render submission allocates nothing on the engine side (CLAUDE.md rule 4). The gate:
//! scene updates and `Renderer::prepare` (batching, light clustering, cascade fitting,
//! every per-frame uniform) perform zero heap operations after warm-up. The metric:
//! `Renderer::encode` (uploads and graph execution through wgpu) is counted and reported,
//! not gated, so a wgpu upgrade that changes its allocation behavior shows up as a moved
//! number (`MANTIS-METRIC` line).

#![allow(clippy::cast_precision_loss)] // Test scene layout.

use glam::{Mat4, Vec3};
use mantis_formats::material::reference_materials;
use mantis_formats::sh::ShL1;
use mantis_render::gpu_test::noop;
use mantis_render::lighting::clusters::PointLight;
use mantis_render::math::Camera;
use mantis_render::mesh::cube;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc, count_allocs};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn frame_preparation_allocates_nothing_after_warm_up() -> TestResult {
    let ctx = noop()?;
    let config = RendererConfig {
        max_instances: 1024,
        max_batches: 64,
        max_vertices: 4096,
        max_indices: 8192,
        shadow_resolution: 256,
        ..RendererConfig::new(128, 72, wgpu::TextureFormat::Rgba8Unorm)
    };
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let (v, i) = cube();
    let mesh = renderer.add_mesh(&ctx.queue, &v, &i)?;
    let mut handles = Vec::new();
    for (k, m) in reference_materials()?.into_iter().enumerate() {
        let id = renderer.add_material(&ctx.device, &ctx.queue, m, [None; 4])?;
        for n in 0..100 {
            let at = Vec3::new((n % 10) as f32 * 2.0, k as f32 * 2.0, -((n / 10) as f32) * 2.0);
            handles.push(renderer.scene_mut().spawn(mesh, id, Mat4::from_translation(at))?);
        }
    }
    let lights: Vec<PointLight> = (0..64)
        .map(|n| PointLight {
            position: Vec3::new(n as f32, 1.0, -(n as f32)),
            range: 6.0,
            color: Vec3::ONE,
            intensity: 1.0,
        })
        .collect();
    let output = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("output"),
        size: wgpu::Extent3d {
            width: 128,
            height: 72,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = output.create_view(&wgpu::TextureViewDescriptor::default());
    let mut frame = FrameInputs {
        camera: Camera {
            position: Vec3::new(10.0, 5.0, 10.0),
            yaw: 0.3,
            pitch: -0.2,
            fov_y: 1.0,
            aspect: 16.0 / 9.0,
            near: 0.1,
        },
        time: 0.0,
        sun_direction: Vec3::new(0.3, -1.0, 0.2),
        sun_color: Vec3::ONE,
        shadows: true,
        sky: ShL1::constant([0.2, 0.3, 0.5]),
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 1.0,
        post: PostSettings::default(),
    };
    let mut wgpu_ops = Vec::new();
    for n in 0..20u32 {
        frame.time = n as f32 / 60.0;
        frame.camera.yaw += 0.01;
        let measured = n >= 3;
        let moved = Mat4::from_translation(Vec3::new(n as f32 * 0.1, 0.0, 0.0));
        let mut update = || -> Result<(), Box<dyn std::error::Error>> {
            for h in handles.iter().take(50) {
                renderer.scene_mut().set_transform(*h, moved)?;
            }
            renderer.set_lights(&lights);
            let stats = renderer.prepare(&frame);
            assert_eq!((stats.instances, stats.lights, stats.cascades), (400, 64, 4));
            Ok(())
        };
        if measured {
            assert_no_alloc("scene update and render frame preparation", &mut update)?;
        } else {
            update()?;
        }
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let (result, stats) =
            count_allocs(|| renderer.encode(&ctx.device, &ctx.queue, &mut encoder, (&output, &view)));
        result?;
        if measured {
            wgpu_ops.push(stats.total_ops());
        }
        let _ = ctx.queue.submit(Some(encoder.finish()));
    }
    let max = wgpu_ops.iter().copied().max().unwrap_or(0);
    let min = wgpu_ops.iter().copied().min().unwrap_or(0);
    eprintln!(
        "MANTIS-METRIC render_encode_heap_ops_per_frame min={min} max={max} (wgpu internals; metric, not a gate)"
    );
    Ok(())
}

struct NullLoader(u64);

impl mantis_render::streaming::SectorLoader for NullLoader {
    fn request(
        &mut self,
        _hash: mantis_core::content::ContentHash,
        _priority: f32,
    ) -> mantis_render::streaming::RequestId {
        self.0 += 1;
        mantis_render::streaming::RequestId(self.0)
    }
    fn reprioritize(&mut self, _id: mantis_render::streaming::RequestId, _priority: f32) {}
    fn cancel(&mut self, _id: mantis_render::streaming::RequestId) {}
    fn unload(&mut self, _hash: mantis_core::content::ContentHash) {}
}

#[test]
fn streaming_and_crowd_selection_allocate_nothing() {
    use mantis_render::crowd::{CrowdAgent, CrowdConfig, CrowdSelector, CrowdTier};
    use mantis_render::streaming::{SectorEntry, SectorStreamer, StreamingConfig};
    let sectors: Vec<SectorEntry> = (0..400)
        .map(|i| SectorEntry {
            coord: (i % 20, i / 20),
            lods: [mantis_core::content::ContentHash::ZERO; 4],
            lod_count: 4,
            center: Vec3::new((i % 20) as f32 * 100.0, 0.0, (i / 20) as f32 * 100.0),
            priority_bias: 0.0,
            lod_distance_scale: 1.0,
        })
        .collect();
    let mut streamer = SectorStreamer::new(StreamingConfig::default(), sectors);
    let mut loader = NullLoader(0);
    let mut selector = CrowdSelector::new(CrowdConfig::default(), 300);
    let agents: Vec<CrowdAgent> = (0..300)
        .map(|i| CrowdAgent {
            position: Vec3::new((i % 30) as f32 * 3.0, 0.0, -((i / 30) as f32) * 8.0),
            radius: 1.0,
            priority: 1.0,
        })
        .collect();
    let mut tiers = vec![CrowdTier::Culled; 300];
    let camera = Camera {
        position: Vec3::new(40.0, 2.0, 10.0),
        yaw: 0.0,
        pitch: 0.0,
        fov_y: 1.2,
        aspect: 1.6,
        near: 0.1,
    };
    let frustum = mantis_render::math::Frustum::from_view_projection(&camera.view_projection());
    let mut step = |n: u32| {
        let p = Vec3::new(n as f32 * 7.0, 0.0, n as f32 * 3.0);
        let _ = streamer.update(p, Vec3::new(10.0, 0.0, 4.0), Vec3::NEG_Z, &mut loader);
        selector.select(camera.position, &frustum, &agents, &mut tiers)
    };
    let _ = step(0);
    for n in 1..100 {
        let stats = assert_no_alloc("sector streaming update and crowd tier selection", || step(n));
        assert_eq!(stats.near + stats.mid + stats.far + stats.culled, 300);
    }
}
