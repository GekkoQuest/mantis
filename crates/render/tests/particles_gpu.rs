//! GPU particles end to end. API validation runs on the no-op backend everywhere; the
//! simulation-versus-reference and pixel checks run on a real headless adapter (skipped
//! and counted when none exists).

#![expect(clippy::cast_possible_truncation)] // Pixel math.

use glam::{Mat4, Quat, Vec3};
use mantis_formats::particle_effect::{
    BlendMode, Burst, ColorKey, EmitterDef, EmitterShape, ParticleEffect, SimulationSpace, SizeKey,
};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{
    hardware_or_skip, noop, read_buffer, read_texture_4bpp, validate_wgsl, validation_errors,
};
use mantis_render::math::Camera;
use mantis_render::particles::{
    DRAW_WGSL, EmitterTransform, GpuParticle, ParticleConfig, ParticleError, ParticleSystem, SIM_WGSL,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SIZE: u32 = 64;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// The draw tests render into an 8-bit target so `read_texture_4bpp` applies; the system
/// takes the color format as a parameter, so this exercises the same pipelines.
const LDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DT: f32 = 1.0 / 60.0;
/// Pool slots of the reference comparison.
const POOL: u32 = 2048;

fn sparks() -> EmitterDef {
    EmitterDef {
        capacity: 300,
        duration: 1.0,
        looping: true,
        rate: 120.0,
        bursts: vec![Burst { time: 0.0, count: 40 }],
        lifetime_min: 0.3,
        lifetime_max: 0.9,
        shape: EmitterShape::Cone {
            angle: 0.5,
            radius: 0.2,
        },
        speed_min: 2.0,
        speed_max: 5.0,
        acceleration: [0.0, -9.81, 0.0],
        drag: 0.4,
        color: vec![
            ColorKey {
                t: 0.0,
                value: [6.0, 3.0, 1.0, 1.0],
            },
            ColorKey {
                t: 1.0,
                value: [1.0, 0.1, 0.0, 0.0],
            },
        ],
        size: vec![
            SizeKey {
                t: 0.0,
                value: [0.06],
            },
            SizeKey { t: 1.0, value: [0.0] },
        ],
        blend: BlendMode::Additive,
        space: SimulationSpace::World,
    }
}

fn smoke() -> EmitterDef {
    EmitterDef {
        capacity: 200,
        duration: 2.0,
        looping: true,
        rate: 40.0,
        bursts: Vec::new(),
        lifetime_min: 1.0,
        lifetime_max: 2.0,
        shape: EmitterShape::Sphere { radius: 0.4 },
        speed_min: 0.1,
        speed_max: 0.6,
        acceleration: [0.0, 0.5, 0.2],
        drag: 1.0,
        color: vec![
            ColorKey {
                t: 0.0,
                value: [0.4, 0.4, 0.4, 0.0],
            },
            ColorKey {
                t: 0.3,
                value: [0.4, 0.4, 0.4, 0.5],
            },
            ColorKey {
                t: 1.0,
                value: [0.2, 0.2, 0.2, 0.0],
            },
        ],
        size: vec![SizeKey { t: 0.0, value: [0.2] }, SizeKey { t: 1.0, value: [1.2] }],
        blend: BlendMode::Alpha,
        space: SimulationSpace::Local,
    }
}

fn glow(capacity: u32, local: bool) -> EmitterDef {
    EmitterDef {
        capacity,
        duration: 1.0,
        looping: false,
        rate: 0.0,
        bursts: vec![Burst {
            time: 0.0,
            count: capacity,
        }],
        lifetime_min: 10.0,
        lifetime_max: 10.0,
        shape: EmitterShape::Point,
        speed_min: 0.0,
        speed_max: 0.0,
        acceleration: [0.0; 3],
        drag: 0.0,
        color: vec![ColorKey {
            t: 0.0,
            value: [1.0, 1.0, 1.0, 1.0],
        }],
        size: vec![SizeKey { t: 0.0, value: [1.0] }],
        blend: BlendMode::Additive,
        space: if local {
            SimulationSpace::Local
        } else {
            SimulationSpace::World
        },
    }
}

/// Sparks (additive, world), smoke (alpha, local), and a local additive glow.
fn mixed() -> ParticleEffect {
    ParticleEffect {
        emitters: vec![sparks(), smoke(), glow(8, true)],
    }
}

fn camera() -> Camera {
    Camera {
        position: Vec3::ZERO,
        yaw: 0.0,
        pitch: 0.0,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    }
}

/// View-projection plus the camera's world right and up axes (rows of the view matrix).
fn camera_inputs() -> (Mat4, Vec3, Vec3) {
    let c = camera();
    let view = c.view();
    (
        c.view_projection(),
        view.row(0).truncate(),
        view.row(1).truncate(),
    )
}

struct Targets {
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
}

fn targets(ctx: &GpuContext, format: wgpu::TextureFormat) -> Targets {
    let make = |label: &str, format: wgpu::TextureFormat, usage: wgpu::TextureUsages| {
        ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let color = make(
        "color",
        format,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = make("depth", DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT);
    Targets {
        color_view: color.create_view(&wgpu::TextureViewDescriptor::default()),
        depth_view: depth.create_view(&wgpu::TextureViewDescriptor::default()),
        color,
    }
}

/// One frame as the renderer runs it: upload, the simulation compute pass, a scene pass
/// that clears color to black and depth to `scene_depth` (standing in for the opaque
/// passes), then the particle pass with color loaded and depth read-only.
fn frame(ctx: &GpuContext, system: &ParticleSystem, t: &Targets, scene_depth: f32) {
    system.upload(&ctx.queue);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("particles_sim"),
            timestamp_writes: None,
        });
        system.record_simulate(&mut pass);
    }
    {
        let _scene = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &t.color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &t.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(scene_depth),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
    }
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("particles"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &t.color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &t.depth_view,
                depth_ops: None,
                stencil_ops: None,
            }),
            ..Default::default()
        });
        system.record_draw(&mut pass);
    }
    let _ = ctx.queue.submit(Some(encoder.finish()));
}

fn config(max_particles: u32) -> ParticleConfig {
    ParticleConfig {
        max_particles,
        max_effects: 4,
        max_emitters: 16,
    }
}

#[test]
fn shaders_are_valid_wgsl() -> TestResult {
    validate_wgsl(SIM_WGSL)?;
    validate_wgsl(DRAW_WGSL)?;
    Ok(())
}

#[test]
fn particle_frames_pass_api_validation() -> TestResult {
    let ctx = noop()?;
    let t = targets(&ctx, HDR);
    let (vp, right, up) = camera_inputs();
    let mut outcome: Result<(), ParticleError> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        outcome = (|| {
            let mut system = ParticleSystem::new(&ctx.device, config(4096), HDR, DEPTH);
            let id = system.register(&ctx.queue, &mixed())?;
            let a = system.spawn(id, EmitterTransform::at(Vec3::new(0.0, 0.0, 5.0)), 1)?;
            let b = system.spawn(
                id,
                EmitterTransform {
                    position: Vec3::new(1.0, 0.0, 6.0),
                    rotation: Quat::from_rotation_z(0.7),
                    scale: 1.0,
                },
                2,
            )?;
            for n in 0..6 {
                let stats = system.prepare(DT, vp, right, up);
                assert_eq!(stats.live_emitters, 6);
                frame(&ctx, &system, &t, 0.0);
                if n == 2 {
                    system.set_transform(a, EmitterTransform::at(Vec3::new(0.5, 0.2, -5.0)))?;
                    system.stop(b)?;
                }
            }
            system.kill(a)?;
            let _ = system.prepare(DT, vp, right, up);
            frame(&ctx, &system, &t, 0.0);
            Ok(())
        })();
    });
    outcome?;
    assert_eq!(errors, None);
    Ok(())
}

fn read_pool(
    ctx: &GpuContext,
    system: &ParticleSystem,
    slots: usize,
) -> Result<Vec<GpuParticle>, Box<dyn std::error::Error>> {
    let size = (slots * core::mem::size_of::<GpuParticle>()) as u64;
    let bytes = read_buffer(ctx, system.particle_buffer(), size)?;
    let pool: &[GpuParticle] = bytemuck::try_cast_slice(bytes.get(..size as usize).ok_or("short pool")?)
        .map_err(|e| format!("{e:?}"))?;
    Ok(pool.to_vec())
}

#[test]
fn gpu_simulation_matches_cpu_reference() -> TestResult {
    let Some(ctx) = hardware_or_skip("gpu_simulation_matches_cpu_reference") else {
        return Ok(());
    };
    let t = targets(&ctx, HDR);
    let (vp, right, up) = camera_inputs();
    let mut system = ParticleSystem::new(&ctx.device, config(POOL), HDR, DEPTH);
    let id = system.register(&ctx.queue, &mixed())?;
    let a = system.spawn(
        id,
        EmitterTransform {
            position: Vec3::new(0.5, -0.5, 4.0),
            rotation: Quat::from_euler(glam::EulerRot::YXZ, 0.4, 0.3, -0.2),
            scale: 1.5,
        },
        11,
    )?;
    let b = system.spawn(id, EmitterTransform::at(Vec3::new(-1.0, 0.0, -6.0)), 12)?;
    let mut reference = vec![GpuParticle::default(); POOL as usize];
    for n in 0..45 {
        if n == 15 {
            system.set_transform(
                a,
                EmitterTransform {
                    position: Vec3::new(0.0, 0.5, 4.5),
                    rotation: Quat::from_rotation_x(-0.3),
                    scale: 0.5,
                },
            )?;
        }
        if n == 25 {
            system.stop(b)?;
        }
        let _ = system.prepare(DT, vp, right, up);
        system.simulate_reference_frame(&mut reference);
        frame(&ctx, &system, &t, 0.0);
    }
    let gpu = read_pool(&ctx, &system, POOL as usize)?;
    let (mut alive, mut compared) = (0usize, 0usize);
    for (slot, (g, c)) in gpu.iter().zip(&reference).enumerate() {
        if g.is_alive() != c.is_alive() {
            // Only a lifetime rounded differently by the device may disagree.
            assert!(
                (c.age - c.lifetime).abs() < 1e-4,
                "slot {slot}: gpu {g:?} cpu {c:?}"
            );
            continue;
        }
        if !c.is_alive() {
            continue;
        }
        alive += 1;
        for (gp, cp) in g.position.iter().zip(&c.position) {
            assert!(
                (gp - cp).abs() <= 1e-3 * cp.abs().max(1.0),
                "slot {slot} position: gpu {g:?} cpu {c:?}"
            );
        }
        for (gv, cv) in g.velocity.iter().zip(&c.velocity) {
            assert!(
                (gv - cv).abs() <= 1e-3 * cv.abs().max(1.0),
                "slot {slot} velocity: gpu {g:?} cpu {c:?}"
            );
        }
        assert!((g.age - c.age).abs() < 1e-5, "slot {slot} age");
        assert!(
            (g.lifetime - c.lifetime).abs() <= 1e-5 * c.lifetime.max(1.0),
            "slot {slot} lifetime"
        );
        compared += 1;
    }
    assert!(
        alive > 100,
        "a meaningful population: {alive} alive, {compared} compared"
    );
    Ok(())
}

/// Renders one frame of a single glow particle 3 m in front of the camera.
fn render_glow(ctx: &GpuContext, scene_depth: f32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let t = targets(ctx, LDR);
    let (vp, right, up) = camera_inputs();
    let mut system = ParticleSystem::new(&ctx.device, config(64), LDR, DEPTH);
    let id = system.register(
        &ctx.queue,
        &ParticleEffect {
            emitters: vec![glow(1, false)],
        },
    )?;
    let _ = system.spawn(id, EmitterTransform::at(Vec3::new(0.0, 0.0, 3.0)), 5)?;
    let stats = system.prepare(DT, vp, right, up);
    assert_eq!(stats.spawned, 1);
    frame(ctx, &system, &t, scene_depth);
    Ok(read_texture_4bpp(ctx, &t.color)?)
}

fn pixel(data: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    let mut out = [0u8; 4];
    if let Some(p) = data.get(i..i + 4) {
        out.copy_from_slice(p);
    }
    out
}

#[test]
fn additive_particles_light_pixels_and_respect_depth() -> TestResult {
    let Some(ctx) = hardware_or_skip("additive_particles_light_pixels_and_respect_depth") else {
        return Ok(());
    };
    // Scene depth cleared to the far plane (reverse Z: 0): the particle is in front.
    let lit = render_glow(&ctx, 0.0)?;
    let center = pixel(&lit, SIZE / 2, SIZE / 2);
    assert!(
        center[0] > 200 && center[1] > 200 && center[2] > 200,
        "center {center:?}"
    );
    // The sprite is soft: dimmer toward its edge, black outside it. A 1 m sprite at 3 m
    // with a 1 rad vertical field of view spans about 20 of 64 pixels.
    let edge = pixel(&lit, SIZE / 2 + 7, SIZE / 2);
    assert!(edge[0] > 0 && edge[0] < center[0], "edge {edge:?}");
    for (x, y) in [
        (2, 2),
        (SIZE - 3, 2),
        (2, SIZE - 3),
        (SIZE - 3, SIZE - 3),
        (SIZE / 2, 4),
    ] {
        let [r, g, b, _] = pixel(&lit, x, y);
        assert_eq!([r, g, b], [0, 0, 0], "far pixel {x},{y}");
    }
    // Scene depth cleared to the near plane (1): everything is closer than the particle.
    let occluded = render_glow(&ctx, 1.0)?;
    assert!(
        occluded
            .as_chunks::<4>()
            .0
            .iter()
            .all(|[r, g, b, _]| [*r, *g, *b] == [0, 0, 0]),
        "occluded particles must not draw"
    );
    Ok(())
}

#[test]
fn pool_refuses_reuses_and_frees() -> TestResult {
    let ctx = noop()?;
    let (vp, right, up) = camera_inputs();
    let mut system = ParticleSystem::new(
        &ctx.device,
        ParticleConfig {
            max_particles: 1000,
            max_effects: 2,
            max_emitters: 4,
        },
        HDR,
        DEPTH,
    );
    let mut big = smoke();
    big.capacity = 400;
    big.lifetime_min = 0.2;
    big.lifetime_max = 0.5;
    let id = system.register(
        &ctx.queue,
        &ParticleEffect {
            emitters: vec![big.clone()],
        },
    )?;
    let at = EmitterTransform::at(Vec3::ZERO);
    let a = system.spawn(id, at, 1)?;
    let b = system.spawn(id, at, 2)?;
    assert_eq!(system.emitter_range(a, 0), Some(0..400));
    assert_eq!(system.emitter_range(b, 0), Some(400..800));
    assert_eq!(system.spawn(id, at, 3), Err(ParticleError::PoolFull));
    assert_eq!(system.live_emitters(), 2, "a refused spawn keeps nothing");

    system.kill(a)?;
    assert_eq!(system.set_transform(a, at), Err(ParticleError::StaleHandle));
    assert_eq!(system.kill(a), Err(ParticleError::StaleHandle));
    let c = system.spawn(id, at, 3)?;
    assert_eq!(
        system.emitter_range(c, 0),
        Some(0..400),
        "the freed range is reused"
    );
    assert_ne!(a, c, "a reused slot gets a new generation");

    // Stop: particles finish, then the instance frees itself.
    let _ = system.prepare(0.1, vp, right, up);
    system.stop(b)?;
    let mut frames = 0;
    while system.is_live(b) {
        let stats = system.prepare(0.1, vp, right, up);
        frames += 1;
        assert!(frames < 50, "stopped instance never freed");
        if !system.is_live(b) {
            assert_eq!(stats.freed, 1);
        }
    }
    // The youngest particle was spawned on the frame before stop; it is older than the
    // longest lifetime (0.5 s) after five more 0.1 s frames.
    assert_eq!(frames, 5);
    assert_eq!(system.live_emitters(), 1);
    assert_eq!(system.allocator().free_slots(), 600);
    assert_eq!(system.stop(b), Err(ParticleError::StaleHandle));

    // Instance slots, effect slots, and effect size are bounded too.
    let small = ParticleEffect {
        emitters: vec![glow(1, false); 4],
    };
    let small_id = system.register(&ctx.queue, &small)?;
    assert_eq!(system.spawn(small_id, at, 4), Err(ParticleError::TooManyEmitters));
    assert_eq!(
        system.register(&ctx.queue, &small),
        Err(ParticleError::TooManyEffects)
    );
    let mut huge = big;
    huge.capacity = 1001;
    let mut fresh = ParticleSystem::new(&ctx.device, config(1000), HDR, DEPTH);
    assert_eq!(
        fresh.register(&ctx.queue, &ParticleEffect { emitters: vec![huge] }),
        Err(ParticleError::EffectTooLarge)
    );
    let empty = ParticleEffect { emitters: Vec::new() };
    assert!(matches!(
        fresh.register(&ctx.queue, &empty),
        Err(ParticleError::InvalidEffect(_))
    ));
    assert_eq!(
        fresh.spawn(mantis_render::particles::EffectId(3), at, 0),
        Err(ParticleError::UnknownEffect)
    );
    let id = fresh.register(&ctx.queue, &mixed())?;
    let bad = EmitterTransform::at(Vec3::new(f32::NAN, 0.0, 0.0));
    assert_eq!(fresh.spawn(id, bad, 0), Err(ParticleError::InvalidTransform));
    Ok(())
}
