//! GPU frustum culling against the CPU reference: the compute pass must produce exactly
//! the visible set the CPU computes, per batch, for every instance not within a float
//! margin of a frustum plane.

#![expect(clippy::cast_precision_loss)] // Test data generation.

use glam::{Mat4, Vec3};
use mantis_render::culling::{CULL_WGSL, GpuCuller, SceneBuffers};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_buffer, validate_wgsl, validation_errors};
use mantis_render::math::{Camera, Frustum, Sphere};
use mantis_render::scene::{DrawIndexedIndirect, FramePrep, MaterialId, MeshRange, Scene, cull_reference};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const INSTANCES: usize = 2000;
const BATCHES: usize = 6;

/// A deterministic scatter of instances over four meshes and three materials.
fn build_scene() -> Result<(Scene, FramePrep, Frustum), Box<dyn std::error::Error>> {
    let mut scene = Scene::new(INSTANCES, BATCHES, false);
    let meshes: Vec<_> = (0..2)
        .map(|i| {
            scene.add_mesh(MeshRange {
                index_count: 36,
                first_index: 36 * i,
                base_vertex: 0,
                vertex_count: 24,
                bounds: Sphere {
                    center: Vec3::ZERO,
                    radius: 0.5 + i as f32,
                },
            })
        })
        .collect();
    // xorshift: a fixed sequence, no external randomness.
    let mut state = 0x2545_f491_u32;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as f32 / u32::MAX as f32
    };
    for i in 0..INSTANCES {
        let p = Vec3::new(
            next() * 200.0 - 100.0,
            next() * 20.0 - 10.0,
            next() * 200.0 - 100.0,
        );
        let mesh = *meshes.get(i % 2).ok_or("mesh")?;
        let material = MaterialId(u32::try_from(i % 3)?);
        let _ = scene.spawn(mesh, material, Mat4::from_translation(p))?;
    }
    let mut prep = FramePrep::with_capacity(INSTANCES, BATCHES);
    let _ = scene.prepare(&mut prep);
    let camera = Camera {
        position: Vec3::new(0.0, 2.0, 0.0),
        yaw: 0.6,
        pitch: -0.1,
        fov_y: 1.0,
        aspect: 1.5,
        near: 0.1,
    };
    let frustum = Frustum::from_view_projection(&camera.view_projection());
    Ok((scene, prep, frustum))
}

fn run_cull(ctx: &GpuContext, prep: &FramePrep, frustum: &Frustum) -> SceneBuffers {
    let buffers = SceneBuffers::new(
        &ctx.device,
        INSTANCES as u64,
        BATCHES as u64,
        wgpu::BufferUsages::COPY_SRC,
    );
    let culler = GpuCuller::new(&ctx.device);
    let bind = culler.bind_group(&ctx.device, &buffers);
    let count = buffers.upload(&ctx.queue, prep, frustum);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cull") });
    culler.record(&mut encoder, &bind, count);
    let _ = ctx.queue.submit(Some(encoder.finish()));
    buffers
}

#[test]
fn cull_shader_is_valid_wgsl() -> TestResult {
    validate_wgsl(CULL_WGSL)?;
    Ok(())
}

#[test]
fn cull_pipeline_passes_api_validation() -> TestResult {
    let ctx = noop()?;
    let (_, prep, frustum) = build_scene()?;
    let errors = validation_errors(&ctx.device, || {
        let _ = run_cull(&ctx, &prep, &frustum);
    });
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn gpu_cull_matches_cpu_reference() -> TestResult {
    let Some(ctx) = hardware_or_skip("gpu_cull_matches_cpu_reference") else {
        return Ok(());
    };
    let (_, prep, frustum) = build_scene()?;
    let buffers = run_cull(&ctx, &prep, &frustum);
    let args_bytes = read_buffer(
        &ctx,
        &buffers.args,
        (BATCHES * core::mem::size_of::<DrawIndexedIndirect>()) as u64,
    )?;
    let visible_bytes = read_buffer(&ctx, &buffers.visible, (INSTANCES * 4) as u64)?;
    let args: &[DrawIndexedIndirect] =
        bytemuck::try_cast_slice(args_bytes.get(..BATCHES * 20).ok_or("args")?)
            .map_err(|e| format!("{e:?}"))?;
    let visible: &[u32] = bytemuck::try_cast_slice(&visible_bytes).map_err(|e| format!("{e:?}"))?;
    let reference = cull_reference(&prep, &frustum);

    let mut compared = 0usize;
    for (b, (arg, batch)) in args.iter().zip(&prep.batches).enumerate() {
        let start = batch.base as usize;
        let gpu: std::collections::BTreeSet<u32> = visible
            .get(start..start + arg.instance_count as usize)
            .ok_or("visible slice")?
            .iter()
            .copied()
            .collect();
        let cpu: std::collections::BTreeSet<u32> =
            reference.get(b).ok_or("reference")?.iter().copied().collect();
        // Instances near a plane may round either way on the GPU; everything else must agree.
        let ambiguous: std::collections::BTreeSet<u32> = prep
            .instances
            .iter()
            .enumerate()
            .filter(|(_, inst)| inst.batch as usize == b)
            .filter(|(_, inst)| frustum.classify_sphere(&inst.world_sphere(), 1e-3).is_none())
            .filter_map(|(i, _)| u32::try_from(i).ok())
            .collect();
        let gpu_firm: Vec<_> = gpu.difference(&ambiguous).collect();
        let cpu_firm: Vec<_> = cpu.difference(&ambiguous).collect();
        assert_eq!(gpu_firm, cpu_firm, "batch {b}");
        assert_eq!(
            gpu.len(),
            arg.instance_count as usize,
            "batch {b}: no duplicate slots"
        );
        assert_eq!((arg.index_count, arg.first_instance), (36, 0));
        compared += cpu_firm.len();
    }
    let total: usize = reference.iter().map(Vec::len).sum();
    assert!(
        compared > 100 && compared < INSTANCES,
        "a meaningful subset is visible: {compared} of {total}"
    );
    Ok(())
}
