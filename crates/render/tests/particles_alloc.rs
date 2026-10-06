//! Particle frame preparation allocates nothing after warm-up (CLAUDE.md rule 4): clocks,
//! spawn windows, uniforms, draw lists, the alpha sort, and instances finishing and
//! releasing their pool ranges all run on storage reserved at construction.

use glam::{Quat, Vec3};
use mantis_formats::particle_effect::{
    BlendMode, Burst, ColorKey, EmitterDef, EmitterShape, ParticleEffect, SimulationSpace, SizeKey,
};
use mantis_render::gpu_test::noop;
use mantis_render::math::Camera;
use mantis_render::particles::{EmitterTransform, ParticleConfig, ParticleSystem};
use mantis_testkit::alloc::{CountingAllocator, assert_no_alloc};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn emitter(blend: BlendMode, space: SimulationSpace, looping: bool) -> EmitterDef {
    EmitterDef {
        capacity: 64,
        duration: 0.5,
        looping,
        rate: 90.0,
        bursts: vec![Burst { time: 0.1, count: 8 }],
        lifetime_min: 0.2,
        lifetime_max: 0.4,
        shape: EmitterShape::Sphere { radius: 0.5 },
        speed_min: 0.5,
        speed_max: 1.5,
        acceleration: [0.0, -2.0, 0.0],
        drag: 0.3,
        color: vec![
            ColorKey {
                t: 0.0,
                value: [1.0, 0.8, 0.5, 1.0],
            },
            ColorKey {
                t: 1.0,
                value: [0.5, 0.5, 0.5, 0.0],
            },
        ],
        size: vec![SizeKey { t: 0.0, value: [0.3] }],
        blend,
        space,
    }
}

#[test]
fn prepare_allocates_nothing_after_warm_up() -> TestResult {
    let ctx = noop()?;
    let mut system = ParticleSystem::new(
        &ctx.device,
        ParticleConfig {
            max_particles: 8192,
            max_effects: 4,
            max_emitters: 64,
        },
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureFormat::Depth32Float,
    );
    let looping = system.register(
        &ctx.queue,
        &ParticleEffect {
            emitters: vec![
                emitter(BlendMode::Additive, SimulationSpace::World, true),
                emitter(BlendMode::Alpha, SimulationSpace::Local, true),
            ],
        },
    )?;
    let one_shot = system.register(
        &ctx.queue,
        &ParticleEffect {
            emitters: vec![emitter(BlendMode::Alpha, SimulationSpace::World, false)],
        },
    )?;
    let mut handles = Vec::new();
    for i in 0..12u8 {
        let t = EmitterTransform {
            position: Vec3::new(f32::from(i), 0.0, 2.0 + f32::from(i % 5)),
            rotation: Quat::from_rotation_y(f32::from(i) * 0.3),
            scale: 1.0,
        };
        handles.push(system.spawn(looping, t, u32::from(i))?);
        let _ = system.spawn(one_shot, t, 100 + u32::from(i))?;
    }
    let view_proj = Camera {
        position: Vec3::new(4.0, 1.0, 3.0),
        yaw: 0.2,
        pitch: -0.1,
        fov_y: 1.0,
        aspect: 1.5,
        near: 0.1,
    }
    .view_projection();
    let (right, up) = (Vec3::X, Vec3::Y);
    // Warm-up.
    for _ in 0..3 {
        let _ = system.prepare(1.0 / 60.0, view_proj, right, up);
    }
    for h in handles.iter().step_by(3) {
        system.stop(*h)?;
    }
    let mut freed = 0;
    let mut spawned = 0;
    assert_no_alloc("particle prepare", || {
        for _ in 0..120 {
            let stats = system.prepare(1.0 / 60.0, view_proj, right, up);
            freed += stats.freed;
            spawned += stats.spawned;
        }
    });
    assert!(spawned > 1000, "{spawned} spawned");
    // The one-shot instances and the stopped looping ones finished inside the gate.
    assert_eq!(freed, 12 + 4 * 2, "{freed} freed");
    assert_eq!(system.live_emitters(), 8 * 2);
    Ok(())
}
